//! Shell state: variables, functions, aliases, options, working directory.
//!
//! The state is `Clone + Send`: subshells, pipeline stages and background
//! jobs each run on a copy (on their own thread), which is what gives them
//! subshell semantics. The working directory is part of the state and never
//! the process-wide one, so concurrent stages can't step on each other.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::ast::Command;
use crate::jobs::Jobs;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Array(Vec<String>),
}

impl Value {
    /// Scalar view: arrays are joined with spaces.
    pub fn to_str(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Array(a) => a.join(" "),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Var {
    pub value: Value,
    pub exported: bool,
    pub readonly: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub errexit: bool,
    pub xtrace: bool,
    pub nounset: bool,
    pub noglob: bool,
    pub pipefail: bool,
    /// Error when a glob matches nothing (zsh default). Off: keep the word.
    pub nonomatch: bool,
    /// Patterns match leading dots.
    pub globdots: bool,
    pub interactive: bool,
}

pub const OPTION_NAMES: &[&str] =
    &["errexit", "xtrace", "nounset", "noglob", "pipefail", "nomatch", "globdots", "interactive"];

impl Options {
    pub fn get(&self, name: &str) -> Option<bool> {
        Some(match name {
            "errexit" => self.errexit,
            "xtrace" => self.xtrace,
            "nounset" => self.nounset,
            "noglob" => self.noglob,
            "pipefail" => self.pipefail,
            "nomatch" => !self.nonomatch,
            "globdots" | "dotglob" => self.globdots,
            "interactive" => self.interactive,
            _ => return None,
        })
    }

    pub fn set(&mut self, name: &str, on: bool) -> bool {
        let name = name.to_ascii_lowercase().replace('_', "");
        let (name, on) = match name.strip_prefix("no") {
            Some(rest) if Options::default().get(rest).is_some() && rest != "match" => (rest.to_string(), !on),
            _ => (name, on),
        };
        match name.as_str() {
            "errexit" => self.errexit = on,
            "xtrace" => self.xtrace = on,
            "nounset" => self.nounset = on,
            "noglob" => self.noglob = on,
            "glob" => self.noglob = !on,
            "pipefail" => self.pipefail = on,
            "nomatch" => self.nonomatch = !on,
            "globdots" | "dotglob" => self.globdots = on,
            _ => return false,
        }
        true
    }
}

#[derive(Clone)]
pub struct Shell {
    pub vars: HashMap<String, Var>,
    pub funcs: HashMap<String, Arc<Command>>,
    pub aliases: HashMap<String, String>,
    pub cwd: PathBuf,
    pub dirstack: Vec<PathBuf>,
    pub arg0: String,
    pub args: Vec<String>,
    pub status: i32,
    pub opts: Options,
    /// One frame per active function call: variables to restore on return.
    pub locals: Vec<Vec<(String, Option<Var>)>>,
    pub loop_depth: usize,
    /// >0 while running a condition (`if`, `while`, `&&` lhs): no errexit.
    pub cond_depth: usize,
    /// Running as a background job: externals get their own process group.
    pub background: bool,
    pub jobs: Arc<Mutex<Jobs>>,
    pub history: Arc<Mutex<Vec<String>>>,
    pub start: Instant,
    pub pid: u32,
    pub last_bg: Option<usize>,
    /// Exit handler from `trap '...' EXIT`.
    pub exit_trap: Option<String>,
    /// Status of the last command substitution in the current command.
    pub sub_status: Option<i32>,
    /// `getopts` position inside the current argument.
    pub optpos: usize,
}

impl Shell {
    pub fn new() -> Shell {
        let mut vars = HashMap::new();
        for (k, v) in std::env::vars_os() {
            let (Ok(mut k), Ok(v)) = (k.into_string(), v.into_string()) else { continue };
            if cfg!(windows) && k.eq_ignore_ascii_case("path") {
                k = "PATH".into();
            }
            vars.insert(k, Var { value: Value::Str(v), exported: true, readonly: false });
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut sh = Shell {
            vars,
            funcs: HashMap::new(),
            aliases: HashMap::new(),
            cwd: cwd.clone(),
            dirstack: Vec::new(),
            arg0: "vsh".into(),
            args: Vec::new(),
            status: 0,
            opts: Options::default(),
            locals: Vec::new(),
            loop_depth: 0,
            cond_depth: 0,
            background: false,
            jobs: Arc::new(Mutex::new(Jobs::default())),
            history: Arc::new(Mutex::new(Vec::new())),
            start: Instant::now(),
            pid: std::process::id(),
            last_bg: None,
            exit_trap: None,
            sub_status: None,
            optpos: 0,
        };
        if sh.get("HOME").is_none()
            && let Some(home) = sh.get("USERPROFILE")
        {
            sh.set_var("HOME", Value::Str(display_path(Path::new(&home))));
        }
        let level = sh.get("SHLVL").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0) + 1;
        sh.set_var("SHLVL", Value::Str(level.to_string()));
        sh.export("SHLVL");
        sh.set_cwd(cwd);
        if sh.get("IFS").is_none() {
            sh.set_var("IFS", Value::Str(" \t\n".into()));
        }
        if let Ok(exe) = std::env::current_exe() {
            sh.set_var("SHELL", Value::Str(display_path(&exe)));
        }
        sh
    }

    /// Scalar value of a variable (arrays joined), including dynamic ones.
    pub fn get(&self, name: &str) -> Option<String> {
        match name {
            "RANDOM" => Some(random().to_string()),
            "SECONDS" => Some(self.start.elapsed().as_secs().to_string()),
            _ => self.vars.get(name).map(|v| v.value.to_str()),
        }
    }

    pub fn set_var(&mut self, name: &str, value: Value) -> bool {
        if let Some(v) = self.vars.get_mut(name) {
            if v.readonly {
                eprintln!("vsh: read-only variable: {name}");
                return false;
            }
            v.value = value;
        } else {
            self.vars.insert(name.to_string(), Var { value, exported: false, readonly: false });
        }
        true
    }

    pub fn set_str(&mut self, name: &str, value: impl Into<String>) -> bool {
        self.set_var(name, Value::Str(value.into()))
    }

    pub fn export(&mut self, name: &str) {
        match self.vars.get_mut(name) {
            Some(v) => v.exported = true,
            None => {
                self.vars.insert(name.into(), Var { value: Value::Str(String::new()), exported: true, readonly: false });
            }
        }
    }

    pub fn unset(&mut self, name: &str) -> bool {
        if self.vars.get(name).is_some_and(|v| v.readonly) {
            eprintln!("vsh: read-only variable: {name}");
            return false;
        }
        self.vars.remove(name);
        true
    }

    /// Make `name` local to the current function (saving the outer value).
    pub fn make_local(&mut self, name: &str) {
        let old = self.vars.get(name).cloned();
        if let Some(frame) = self.locals.last_mut()
            && !frame.iter().any(|(n, _)| n == name)
        {
            frame.push((name.to_string(), old));
        }
    }

    /// Exported variables as (name, value), sorted.
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> =
            self.vars.iter().filter(|(_, v)| v.exported).map(|(k, v)| (k.clone(), v.value.to_str())).collect();
        env.sort();
        env
    }

    pub fn set_cwd(&mut self, dir: PathBuf) {
        let shown = display_path(&dir);
        self.cwd = dir;
        self.set_str("PWD", shown);
        self.export("PWD");
    }

    pub fn home(&self) -> Option<PathBuf> {
        self.get("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
    }

    /// `~/...` form of a path, for prompts.
    pub fn tilde_path(&self, p: &Path) -> String {
        let shown = display_path(p);
        if let Some(home) = self.home() {
            let home = display_path(&home);
            if shown == home {
                return "~".into();
            }
            if let Some(rest) = shown.strip_prefix(&format!("{}/", home.trim_end_matches('/'))) {
                return format!("~/{rest}");
            }
        }
        shown
    }

    pub fn ifs(&self) -> String {
        self.get("IFS").unwrap_or_else(|| " \t\n".into())
    }

    /// `$-`
    pub fn flags(&self) -> String {
        let mut f = String::new();
        if self.opts.errexit {
            f.push('e');
        }
        if self.opts.noglob {
            f.push('f');
        }
        if self.opts.interactive {
            f.push('i');
        }
        if self.opts.nounset {
            f.push('u');
        }
        if self.opts.xtrace {
            f.push('x');
        }
        f
    }
}

/// A path as the shell shows it: forward slashes on Windows, without the
/// `\\?\` verbatim prefix.
pub fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) {
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

/// Lexically normalize `base/rel` (resolving `.` and `..` without touching
/// the filesystem), like zsh's logical `cd`.
pub fn join_logical(base: &Path, rel: &str) -> PathBuf {
    use std::path::Component;
    let mut rel = rel.to_string();
    // Windows `cd D:` means the root of D: here.
    if cfg!(windows) && rel.len() == 2 && rel.as_bytes()[1] == b':' {
        rel.push('/');
    }
    let joined = base.join(&rel);
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
                // Never pop past the root.
                if out.as_os_str().is_empty() && joined.has_root() {
                    out.push(Component::RootDir);
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn random() -> u32 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut s = STATE.load(Ordering::Relaxed);
    if s == 0 {
        s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1)
            | 1;
    }
    s ^= s << 13;
    s ^= s >> 7;
    s ^= s << 17;
    STATE.store(s, Ordering::Relaxed);
    (s >> 33) as u32 % 32768
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_join() {
        let base = if cfg!(windows) { PathBuf::from("C:/a/b") } else { PathBuf::from("/a/b") };
        let r = join_logical(&base, "../c/./d");
        assert_eq!(display_path(&r), if cfg!(windows) { "C:/a/c/d" } else { "/a/c/d" });
        let up = join_logical(&base, "../../../..");
        assert!(up.has_root());
    }

    #[test]
    fn options() {
        let mut o = Options::default();
        assert!(o.set("no_glob", true));
        assert!(o.noglob);
        assert!(o.set("NOMATCH", false));
        assert!(o.nonomatch);
        assert!(!o.set("bogus", true));
    }
}
