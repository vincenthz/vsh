//! Executing the syntax tree.
//!
//! Command lookup order is: function, shell builtin, builtin utility, and
//! only then `$PATH`. A name containing a `/` always runs that file, and the
//! `command` builtin skips straight to `$PATH`.

use std::fs::OpenOptions;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::ast::*;
use crate::builtins;
use crate::glob;
use crate::io::{self as vio, Input, IoSet, Output};
use crate::parser::{Parser, parse_heredoc_body};
use crate::shell::{Shell, Value};
use crate::utils;

/// Non-local control flow.
#[derive(Debug)]
pub enum Flow {
    Break(usize),
    Continue(usize),
    Return(i32),
    Exit(i32),
}

pub type Exec = Result<i32, Flow>;

/// Which kinds of commands a lookup may find.
#[derive(Clone, Copy)]
pub struct Lookup {
    pub functions: bool,
    pub builtins: bool,
    pub utils: bool,
}

impl Lookup {
    pub const ALL: Lookup = Lookup { functions: true, builtins: true, utils: true };
    pub const EXTERNAL: Lookup = Lookup { functions: false, builtins: false, utils: false };
}

/// What a command name resolves to.
pub enum Resolved {
    Function(Arc<Command>),
    Builtin(builtins::BuiltinFn),
    Util(utils::UtilFn),
    External(PathBuf),
    NotFound,
}

pub fn flow_status(r: Exec) -> i32 {
    match r {
        Ok(s) | Err(Flow::Return(s)) | Err(Flow::Exit(s)) => s,
        Err(Flow::Break(_)) | Err(Flow::Continue(_)) => 0,
    }
}

impl Shell {
    /// Parse and run source text one complete command at a time.
    pub fn run_source(&mut self, src: &str, io: &IoSet) -> Exec {
        let mut p = Parser::new(src);
        loop {
            match p.next_command(&self.aliases) {
                Ok(None) => break,
                Ok(Some(list)) => {
                    self.status = self.exec_list(&list, io)?;
                    if vio::interrupted() && !self.opts.interactive {
                        return Ok(130);
                    }
                }
                Err(e) => {
                    eprintln!("vsh: {e}");
                    self.status = 1;
                    return Ok(1);
                }
            }
        }
        Ok(self.status)
    }

    pub fn exec_list(&mut self, list: &List, io: &IoSet) -> Exec {
        let mut status = 0;
        for item in list {
            if item.background {
                self.spawn_job(&item.and_or, io);
                status = 0;
            } else {
                status = self.exec_and_or(&item.and_or, io)?;
            }
            self.status = status;
            if vio::interrupted() {
                return Ok(130);
            }
        }
        Ok(status)
    }

    pub fn exec_and_or(&mut self, ao: &AndOr, io: &IoSet) -> Exec {
        let has_rest = !ao.rest.is_empty();
        let mut status = self.exec_pipeline_in(&ao.first, io, has_rest)?;
        let mut last_ran = !has_rest;
        for (i, (conn, p)) in ao.rest.iter().enumerate() {
            let run = match conn {
                Connector::And => status == 0,
                Connector::Or => status != 0,
            };
            if run {
                let is_last = i + 1 == ao.rest.len();
                status = self.exec_pipeline_in(p, io, !is_last)?;
                last_ran = is_last;
            }
            self.status = status;
        }
        let negated = if last_ran { ao.rest.last().map(|(_, p)| p).unwrap_or(&ao.first).negate } else { true };
        if status != 0 && last_ran && !negated && self.opts.errexit && self.cond_depth == 0 {
            return Err(Flow::Exit(status));
        }
        Ok(status)
    }

    fn exec_pipeline_in(&mut self, p: &Pipeline, io: &IoSet, cond: bool) -> Exec {
        if cond {
            self.cond_depth += 1;
        }
        let r = self.exec_pipeline(p, io);
        if cond {
            self.cond_depth -= 1;
        }
        r
    }

    pub fn exec_pipeline(&mut self, p: &Pipeline, io: &IoSet) -> Exec {
        if p.negate {
            self.cond_depth += 1;
        }
        let r = self.exec_pipeline_inner(p, io);
        if p.negate {
            self.cond_depth -= 1;
            return r.map(|s| (s == 0) as i32);
        }
        r
    }

    fn exec_pipeline_inner(&mut self, p: &Pipeline, io: &IoSet) -> Exec {
        if p.cmds.len() == 1 {
            return self.exec_command(&p.cmds[0], io);
        }
        // Every stage but the last runs on its own thread with a copy of the
        // shell; the last one runs here, so `... | read x` sets x (zsh).
        let mut handles = Vec::new();
        let mut stdin = io.stdin.clone();
        let n = p.cmds.len();
        for (i, cmd) in p.cmds[..n - 1].iter().enumerate() {
            let (r, w) = match vio::pipe() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("vsh: pipe: {e}");
                    return Ok(1);
                }
            };
            let stderr = if p.stderr_too[i] { w.clone() } else { io.stderr.clone() };
            let stage_io = IoSet { stdin: std::mem::replace(&mut stdin, r), stdout: w, stderr };
            let mut sub = self.clone();
            sub.cond_depth = 0;
            let cmd = cmd.clone();
            let h = std::thread::spawn(move || {
                let r = sub.exec_command(&cmd, &stage_io);
                drop(stage_io);
                flow_status(r)
            });
            handles.push(h);
        }
        let last_io = IoSet { stdin, stdout: io.stdout.clone(), stderr: io.stderr.clone() };
        let last = self.exec_command(&p.cmds[n - 1], &last_io);
        drop(last_io);
        let statuses: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap_or(1)).collect();
        let last = last?;
        if self.opts.pipefail {
            return Ok(statuses.iter().rev().copied().find(|&s| s != 0).filter(|_| last == 0).unwrap_or(last));
        }
        Ok(last)
    }

    pub fn exec_command(&mut self, cmd: &Command, io: &IoSet) -> Exec {
        match cmd {
            Command::Simple(s) => self.exec_simple(s, io),
            Command::Compound(c, redirs) => {
                let io = match self.apply_redirs(redirs, io) {
                    Ok(io) => io,
                    Err(e) => {
                        eprintln!("vsh: {e}");
                        return Ok(1);
                    }
                };
                self.exec_compound(c, &io)
            }
            Command::FuncDef(name, body) => {
                self.funcs.insert(name.clone(), body.clone());
                Ok(0)
            }
        }
    }

    fn exec_compound(&mut self, c: &Compound, io: &IoSet) -> Exec {
        match c {
            Compound::Brace(list) => self.exec_list(list, io),
            Compound::Subshell(list) => {
                let mut sub = self.clone();
                Ok(flow_status(sub.exec_list(list, io)))
            }
            Compound::If { arms, otherwise } => {
                for (cond, body) in arms {
                    if self.run_cond(cond, io)? == 0 {
                        return self.exec_list(body, io);
                    }
                }
                match otherwise {
                    Some(body) => self.exec_list(body, io),
                    None => Ok(0),
                }
            }
            Compound::While { cond, body, until } => {
                let mut status = 0;
                self.loop_depth += 1;
                let r = loop {
                    if vio::interrupted() {
                        break Ok(130);
                    }
                    let c = match self.run_cond(cond, io) {
                        Ok(c) => c,
                        Err(f) => break Err(f),
                    };
                    if (c == 0) == *until {
                        break Ok(status);
                    }
                    match self.loop_body(body, io) {
                        LoopStep::Next(s) => status = s,
                        LoopStep::Stop(r) => break r,
                    }
                };
                self.loop_depth -= 1;
                r
            }
            Compound::For { var, items, body } => {
                let items = match items {
                    Some(words) => match self.expand_args(words) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("vsh: {e}");
                            return Ok(1);
                        }
                    },
                    None => self.args.clone(),
                };
                self.loop_depth += 1;
                let mut r = Ok(0);
                for item in items {
                    if vio::interrupted() {
                        r = Ok(130);
                        break;
                    }
                    self.set_str(var, item);
                    match self.loop_body(body, io) {
                        LoopStep::Next(s) => r = Ok(s),
                        LoopStep::Stop(x) => {
                            r = x;
                            break;
                        }
                    }
                }
                self.loop_depth -= 1;
                r
            }
            Compound::ForArith { init, cond, step, body } => {
                if let Err(e) = self.arith_text(init) {
                    eprintln!("vsh: {e}");
                    return Ok(1);
                }
                let mut status = 0;
                self.loop_depth += 1;
                let r = loop {
                    if vio::interrupted() {
                        break Ok(130);
                    }
                    match self.arith_text(cond) {
                        Ok(0) if !cond.trim().is_empty() => break Ok(status),
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("vsh: {e}");
                            break Ok(1);
                        }
                    }
                    match self.loop_body(body, io) {
                        LoopStep::Next(s) => status = s,
                        LoopStep::Stop(r) => break r,
                    }
                    if let Err(e) = self.arith_text(step) {
                        eprintln!("vsh: {e}");
                        break Ok(1);
                    }
                };
                self.loop_depth -= 1;
                r
            }
            Compound::Repeat { count, body } => {
                let n = match self.expand_str(count).map_err(|e| e.0).and_then(|s| self.arith_text(&s)) {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("vsh: {e}");
                        return Ok(1);
                    }
                };
                self.loop_depth += 1;
                let mut r = Ok(0);
                for _ in 0..n.max(0) {
                    if vio::interrupted() {
                        r = Ok(130);
                        break;
                    }
                    match self.loop_body(body, io) {
                        LoopStep::Next(s) => r = Ok(s),
                        LoopStep::Stop(x) => {
                            r = x;
                            break;
                        }
                    }
                }
                self.loop_depth -= 1;
                r
            }
            Compound::Case { word, arms } => {
                let w = match self.expand_str(word) {
                    Ok(w) => w,
                    Err(e) => {
                        eprintln!("vsh: {e}");
                        return Ok(1);
                    }
                };
                let mut i = 0;
                while i < arms.len() {
                    let mut hit = false;
                    for pat in &arms[i].patterns {
                        match self.expand_pattern(pat) {
                            Ok(p) if glob::matches(&p, &w) => {
                                hit = true;
                                break;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("vsh: {e}");
                                return Ok(1);
                            }
                        }
                    }
                    if hit {
                        let mut status = self.exec_list(&arms[i].body, io)?;
                        while arms[i].fallthrough && i + 1 < arms.len() {
                            i += 1;
                            status = self.exec_list(&arms[i].body, io)?;
                        }
                        return Ok(status);
                    }
                    i += 1;
                }
                Ok(0)
            }
            Compound::Cond(toks) => Ok(crate::cond::eval(self, toks)),
            Compound::Arith(expr) => match self.arith_text(expr) {
                Ok(v) => Ok((v == 0) as i32),
                Err(e) => {
                    eprintln!("vsh: {e}");
                    Ok(2)
                }
            },
        }
    }

    /// Expand `$...` in raw arithmetic text, then evaluate it.
    pub fn arith_text(&mut self, text: &str) -> Result<i64, String> {
        let w = parse_heredoc_body(text).map_err(|e| e.msg)?;
        let s = self.expand_str(&w).map_err(|e| e.0)?;
        self.arith(&s).map_err(|e| e.0)
    }

    fn run_cond(&mut self, cond: &List, io: &IoSet) -> Exec {
        self.cond_depth += 1;
        let r = self.exec_list(cond, io);
        self.cond_depth -= 1;
        r
    }

    fn loop_body(&mut self, body: &List, io: &IoSet) -> LoopStep {
        match self.exec_list(body, io) {
            Ok(s) => LoopStep::Next(s),
            Err(Flow::Break(n)) => LoopStep::Stop(if n > 1 { Err(Flow::Break(n - 1)) } else { Ok(0) }),
            Err(Flow::Continue(n)) if n > 1 => LoopStep::Stop(Err(Flow::Continue(n - 1))),
            Err(Flow::Continue(_)) => LoopStep::Next(0),
            Err(f) => LoopStep::Stop(Err(f)),
        }
    }

    fn exec_simple(&mut self, cmd: &Simple, io: &IoSet) -> Exec {
        self.sub_status = None;
        let argv = match self.expand_args(&cmd.words) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("vsh: {e}");
                return Ok(1);
            }
        };
        let mut assigns = Vec::new();
        for a in &cmd.assigns {
            let v = match &a.value {
                AssignValue::Scalar(w) => self.expand_assign(w).map(Value::Str),
                AssignValue::Array(ws) => self.expand_args(ws).map(Value::Array),
            };
            match v {
                Ok(v) => assigns.push((a.name.clone(), v, a.append)),
                Err(e) => {
                    eprintln!("vsh: {e}");
                    return Ok(1);
                }
            }
        }
        if argv.is_empty() {
            for (name, v, append) in assigns {
                let v = if append { self.appended(&name, v) } else { v };
                if !self.set_var(&name, v) {
                    return Ok(1);
                }
            }
            if !cmd.redirs.is_empty()
                && let Err(e) = self.apply_redirs(&cmd.redirs, io)
            {
                eprintln!("vsh: {e}");
                return Ok(1);
            }
            return Ok(self.sub_status.unwrap_or(0));
        }
        let io = match self.apply_redirs(&cmd.redirs, io) {
            Ok(io) => io,
            Err(e) => {
                eprintln!("vsh: {e}");
                return Ok(1);
            }
        };
        if self.opts.xtrace {
            let mut line = String::from("+");
            for (n, v, _) in &assigns {
                line.push_str(&format!(" {n}={}", crate::fmt::quote(&v.to_str())));
            }
            for a in &argv {
                line.push(' ');
                line.push_str(&crate::fmt::quote(a));
            }
            let mut err = io.stderr.writer();
            let _ = writeln!(err, "{line}");
            let _ = err.flush();
        }
        let assigns: Vec<(String, String)> = assigns
            .into_iter()
            .map(|(n, v, append)| {
                let v = if append { self.appended(&n, v) } else { v };
                (n, v.to_str())
            })
            .collect();
        self.run_command(&argv, &assigns, &io, Lookup::ALL)
    }

    fn appended(&self, name: &str, v: Value) -> Value {
        match (self.vars.get(name).map(|v| &v.value), v) {
            (Some(Value::Array(a)), Value::Array(b)) => Value::Array(a.iter().cloned().chain(b).collect()),
            (Some(Value::Array(a)), Value::Str(s)) => Value::Array(a.iter().cloned().chain([s]).collect()),
            (Some(Value::Str(a)), Value::Str(s)) => Value::Str(format!("{a}{s}")),
            (Some(Value::Str(a)), Value::Array(b)) => Value::Array([a.clone()].into_iter().chain(b).collect()),
            (None, v) => v,
        }
    }

    pub fn resolve(&self, name: &str, lookup: Lookup, path_override: Option<&str>) -> Resolved {
        if crate::path::has_dir(name) {
            return match crate::path::find(name, None, self.get("PATHEXT").as_deref(), &self.cwd) {
                Some(p) => Resolved::External(p),
                None => Resolved::NotFound,
            };
        }
        if lookup.functions
            && let Some(f) = self.funcs.get(name)
        {
            return Resolved::Function(f.clone());
        }
        if lookup.builtins
            && let Some(b) = builtins::lookup(name)
        {
            return Resolved::Builtin(b);
        }
        if lookup.utils
            && let Some(u) = utils::lookup(name)
        {
            return Resolved::Util(u);
        }
        let path = path_override.map(String::from).or_else(|| self.get("PATH"));
        match crate::path::find(name, path.as_deref(), self.get("PATHEXT").as_deref(), &self.cwd) {
            Some(p) => Resolved::External(p),
            None => Resolved::NotFound,
        }
    }

    /// Run an expanded command line. `assigns` are `NAME=value` prefixes that
    /// apply to this command only.
    pub fn run_command(&mut self, argv: &[String], assigns: &[(String, String)], io: &IoSet, lookup: Lookup) -> Exec {
        let name = argv[0].as_str();
        let path_override = assigns.iter().rev().find(|(n, _)| n == "PATH").map(|(_, v)| v.as_str());
        match self.resolve(name, lookup, path_override) {
            Resolved::Function(body) => self.with_temp_vars(assigns, |sh| sh.call_function(name, body, argv, io)),
            Resolved::Builtin(b) => self.with_temp_vars(assigns, |sh| b(sh, argv, io)),
            Resolved::Util(u) => Ok(self.run_util(u, argv, assigns, io)),
            Resolved::External(path) => Ok(self.run_external(&path, argv, assigns, io)),
            Resolved::NotFound => {
                let mut err = io.stderr.writer();
                let _ = writeln!(err, "vsh: command not found: {name}");
                let _ = err.flush();
                Ok(127)
            }
        }
    }

    fn with_temp_vars(&mut self, assigns: &[(String, String)], f: impl FnOnce(&mut Shell) -> Exec) -> Exec {
        if assigns.is_empty() {
            return f(self);
        }
        let saved: Vec<(String, Option<crate::shell::Var>)> =
            assigns.iter().map(|(n, _)| (n.clone(), self.vars.get(n).cloned())).collect();
        for (n, v) in assigns {
            self.set_str(n, v.clone());
            self.export(n);
        }
        let r = f(self);
        for (n, old) in saved.into_iter().rev() {
            match old {
                Some(v) => {
                    self.vars.insert(n, v);
                }
                None => {
                    self.vars.remove(&n);
                }
            }
        }
        r
    }

    fn merged_env(&self, assigns: &[(String, String)]) -> Vec<(String, String)> {
        let mut env = self.env();
        for (n, v) in assigns {
            env.retain(|(k, _)| !utils::env_key_eq(k, n));
            env.push((n.clone(), v.clone()));
        }
        env.sort();
        env
    }

    pub fn run_util(&mut self, f: utils::UtilFn, argv: &[String], assigns: &[(String, String)], io: &IoSet) -> i32 {
        let env = self.merged_env(assigns);
        let mut stdin = BufReader::new(io.stdin.reader());
        let mut stdout = io.stdout.writer();
        let mut stderr = io.stderr.writer();
        let status = {
            let mut ctx = utils::Ctx {
                name: &argv[0],
                stdin: &mut stdin,
                stdout: &mut *stdout,
                stderr: &mut *stderr,
                stdout_tty: io.stdout.is_terminal(),
                cwd: &self.cwd,
                env: &env,
            };
            utils::run(f, &mut ctx, &argv[1..])
        };
        let _ = stderr.flush();
        status
    }

    pub fn call_function(&mut self, name: &str, body: Arc<Command>, argv: &[String], io: &IoSet) -> Exec {
        if self.locals.len() > 500 {
            eprintln!("vsh: maximum nested function level reached");
            return Ok(1);
        }
        let saved_args = std::mem::replace(&mut self.args, argv[1..].to_vec());
        let saved_arg0 = std::mem::replace(&mut self.arg0, name.to_string());
        self.locals.push(Vec::new());
        let r = self.exec_command(&body, io);
        if let Some(frame) = self.locals.pop() {
            for (n, old) in frame.into_iter().rev() {
                match old {
                    Some(v) => {
                        self.vars.insert(n, v);
                    }
                    None => {
                        self.vars.remove(&n);
                    }
                }
            }
        }
        self.args = saved_args;
        self.arg0 = saved_arg0;
        match r {
            Err(Flow::Return(s)) => Ok(s),
            r => r,
        }
    }

    pub fn run_external(&mut self, path: &Path, argv: &[String], assigns: &[(String, String)], io: &IoSet) -> i32 {
        let env = self.merged_env(assigns);
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
        match self.spawn_external(path, argv, &env, io) {
            Ok(status) => status,
            Err(e) => {
                #[cfg(unix)]
                if e.raw_os_error() == Some(libc::ENOEXEC) {
                    // No shebang: run it as a vsh script.
                    let mut sub = self.clone();
                    return flow_status(sub.source_file(path, Some(argv[1..].to_vec()), io));
                }
                let mut err = io.stderr.writer();
                let (msg, code) = match e.kind() {
                    io::ErrorKind::PermissionDenied => ("permission denied", 126),
                    io::ErrorKind::NotFound => ("no such file or directory", 127),
                    _ => ("", 126),
                };
                if msg.is_empty() {
                    let _ = writeln!(err, "vsh: {}: {}", argv[0], crate::fmt::io_error(&e));
                } else {
                    let _ = writeln!(err, "vsh: {msg}: {}", argv[0]);
                }
                let _ = err.flush();
                code
            }
        }
    }

    fn spawn_external(&self, path: &Path, argv: &[String], env: &[(String, String)], io: &IoSet) -> io::Result<i32> {
        let mut cmd = if cfg!(windows) && !is_native_executable(path) {
            // Shell scripts on Windows run through this shell.
            let mut c = Process::new(std::env::current_exe()?);
            c.arg(path);
            c
        } else {
            Process::new(path)
        };
        cmd.args(&argv[1..]);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.arg0(&argv[0]);
            if self.background {
                cmd.process_group(0);
            }
        }
        cmd.env_clear();
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
        cmd.current_dir(&self.cwd);
        let (stdin, feed) = io.stdin.stdio()?;
        cmd.stdin(stdin);
        cmd.stdout(io.stdout.stdio()?);
        cmd.stderr(io.stderr.stdio()?);
        let mut child = cmd.spawn()?;
        // Drop our copies of the pipe ends so readers see EOF.
        drop(cmd);
        let feeder = match (feed, child.stdin.take()) {
            (Some(bytes), Some(mut sink)) => Some(std::thread::spawn(move || {
                let _ = sink.write_all(&bytes);
            })),
            _ => None,
        };
        let status = child.wait()?;
        if let Some(f) = feeder {
            let _ = f.join();
        }
        Ok(exit_code(status, &argv[0]))
    }

    /// Command substitution: run in a subshell, collect stdout.
    pub fn capture(&mut self, list: &List) -> String {
        let (r, w) = match io::pipe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("vsh: pipe: {e}");
                return String::new();
            }
        };
        let reader = std::thread::spawn(move || {
            let mut r = r;
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            buf
        });
        let mut sub = self.clone();
        sub.cond_depth = 0;
        let io = IoSet { stdin: Input::Inherit, stdout: Output::Pipe(Arc::new(w)), stderr: Output::Stderr };
        let status = flow_status(sub.exec_list(list, &io));
        drop(io);
        let out = reader.join().unwrap_or_default();
        self.sub_status = Some(status);
        self.status = status;
        String::from_utf8_lossy(&out).into_owned()
    }

    fn spawn_job(&mut self, ao: &AndOr, io: &IoSet) {
        let mut sub = self.clone();
        sub.background = true;
        sub.opts.interactive = false;
        let mut io = io.clone();
        if matches!(io.stdin, Input::Inherit) {
            io.stdin = Input::Null;
        }
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let ao2 = ao.clone();
        let handle = std::thread::spawn(move || {
            let s = flow_status(sub.exec_and_or(&ao2, &io));
            drop(io);
            flag.store(true, Ordering::Release);
            s
        });
        let text = crate::fmt::and_or(ao);
        let id = self.jobs.lock().unwrap().add(text, done, handle);
        if self.opts.interactive {
            eprintln!("[{id}]");
        }
        self.last_bg = Some(id);
    }

    pub fn apply_redirs(&mut self, redirs: &[Redir], io: &IoSet) -> Result<IoSet, String> {
        let mut io = io.clone();
        for r in redirs {
            let default_fd = match r.op {
                RedirOp::In | RedirOp::ReadWrite | RedirOp::DupIn | RedirOp::HereDoc | RedirOp::HereString => 0,
                _ => 1,
            };
            let fd = r.fd.unwrap_or(default_fd);
            if fd > 2 {
                return Err(format!("file descriptor {fd} is not supported"));
            }
            let target = self.expand_str(&r.target).map_err(|e| e.0)?;
            match r.op {
                RedirOp::HereDoc => set_input(&mut io, fd, Input::bytes(target.into_bytes()))?,
                RedirOp::HereString => set_input(&mut io, fd, Input::bytes(format!("{target}\n").into_bytes()))?,
                RedirOp::DupOut | RedirOp::DupIn => {
                    if target == "-" {
                        match fd {
                            0 => io.stdin = Input::Null,
                            1 => io.stdout = Output::Null,
                            _ => io.stderr = Output::Null,
                        }
                        continue;
                    }
                    let src: u32 = target.parse().map_err(|_| format!("bad file descriptor: {target}"))?;
                    match (fd, src) {
                        (1, 2) => io.stdout = io.stderr.clone(),
                        (2, 1) => io.stderr = io.stdout.clone(),
                        (a, b) if a == b => {}
                        (0, _) | (_, 0) => {}
                        _ => return Err(format!("file descriptor {src} is not supported")),
                    }
                }
                op => {
                    let path = utils::resolve(&self.cwd, &target);
                    let mut o = OpenOptions::new();
                    match op {
                        RedirOp::In => o.read(true),
                        RedirOp::ReadWrite => o.read(true).write(true).create(true),
                        RedirOp::Out | RedirOp::OutErr => o.write(true).create(true).truncate(true),
                        _ => o.append(true).create(true),
                    };
                    let f = o.open(&path).map_err(|e| format!("{}: {target}", crate::fmt::io_error(&e)))?;
                    let f = Arc::new(f);
                    match op {
                        RedirOp::In | RedirOp::ReadWrite => set_input(&mut io, fd, Input::File(f))?,
                        RedirOp::OutErr | RedirOp::AppendErr => {
                            io.stdout = Output::File(f.clone());
                            io.stderr = Output::File(f);
                        }
                        _ => match fd {
                            1 => io.stdout = Output::File(f),
                            2 => io.stderr = Output::File(f),
                            _ => io.stdin = Input::File(f),
                        },
                    }
                }
            }
        }
        Ok(io)
    }

    /// Run a script file in this shell (`source`), optionally with new
    /// positional parameters.
    pub fn source_file(&mut self, path: &Path, args: Option<Vec<String>>, io: &IoSet) -> Exec {
        let src = match std::fs::read(path) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => {
                eprintln!("vsh: {}: {}", crate::fmt::io_error(&e), crate::shell::display_path(path));
                return Ok(127);
            }
        };
        let saved = args.map(|a| std::mem::replace(&mut self.args, a));
        let r = self.run_source(&src, io);
        if let Some(a) = saved {
            self.args = a;
        }
        match r {
            Err(Flow::Return(s)) => Ok(s),
            r => r,
        }
    }
}

enum LoopStep {
    Next(i32),
    Stop(Exec),
}

fn set_input(io: &mut IoSet, fd: u32, input: Input) -> Result<(), String> {
    if fd != 0 {
        return Err(format!("file descriptor {fd} is not readable here"));
    }
    io.stdin = input;
    Ok(())
}

fn is_native_executable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["exe", "com", "bat", "cmd"].iter().any(|x| x.eq_ignore_ascii_case(e)))
}

#[cfg(unix)]
fn exit_code(status: ExitStatus, name: &str) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        return code;
    }
    let sig = status.signal().unwrap_or(0);
    let msg = match sig {
        libc::SIGSEGV => Some("segmentation fault"),
        libc::SIGBUS => Some("bus error"),
        libc::SIGABRT => Some("abort"),
        libc::SIGFPE => Some("floating point exception"),
        libc::SIGILL => Some("illegal hardware instruction"),
        libc::SIGKILL => Some("killed"),
        libc::SIGTERM => Some("terminated"),
        _ => None,
    };
    if let Some(m) = msg {
        let core = if status.core_dumped() { " (core dumped)" } else { "" };
        eprintln!("vsh: {m}{core}  {name}");
    }
    128 + sig
}

#[cfg(not(unix))]
fn exit_code(status: ExitStatus, _name: &str) -> i32 {
    status.code().unwrap_or(1)
}
