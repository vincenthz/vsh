//! Interactive loop: line editing, history, completion, prompt.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::history::DefaultHistory;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{CompletionType, Config, Context, Editor, Helper};

use crate::exec::Flow;
use crate::io::IoSet;
use crate::parser::Parser;
use crate::shell::{Shell, display_path};

#[derive(Default)]
struct CompState {
    cwd: PathBuf,
    home: Option<PathBuf>,
    commands: Vec<String>,
    vars: Vec<String>,
    path: String,
}

struct VshHelper {
    state: Arc<Mutex<CompState>>,
    hinter: HistoryHinter,
}

impl Helper for VshHelper {}

impl Validator for VshHelper {
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        let mut p = Parser::new(ctx.input());
        loop {
            match p.next_command(&Default::default()) {
                Ok(None) => return Ok(ValidationResult::Valid(None)),
                Ok(Some(_)) => {}
                Err(e) if e.incomplete => return Ok(ValidationResult::Incomplete),
                // Let the shell report real syntax errors.
                Err(_) => return Ok(ValidationResult::Valid(None)),
            }
        }
    }
}

impl Hinter for VshHelper {
    type Hint = String;
    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<String> {
        self.hinter.hint(line, pos, ctx)
    }
}

impl Highlighter for VshHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("\x1b[90m{hint}\x1b[0m"))
    }
}

const WORD_BREAKS: &[char] = &[' ', '\t', '\n', ';', '|', '&', '(', ')', '<', '>', '"', '\'', '='];

impl Completer for VshHelper {
    type Candidate = Pair;

    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> rustyline::Result<(usize, Vec<Pair>)> {
        let st = self.state.lock().unwrap();
        let before = &line[..pos];
        // Find the word start, honoring backslash escapes.
        let mut start = 0;
        let chars: Vec<(usize, char)> = before.char_indices().collect();
        for (k, &(i, c)) in chars.iter().enumerate() {
            let escaped = k > 0 && chars[k - 1].1 == '\\';
            if WORD_BREAKS.contains(&c) && !escaped {
                start = i + c.len_utf8();
            }
        }
        let word = &before[start..];
        let head = before[..start].trim_end();
        let command_pos = head.is_empty()
            || head.ends_with(['|', ';', '&', '(', '{'])
            || ["then", "do", "else", "sudo", "command", "builtin", "exec", "time", "env", "xargs"]
                .iter()
                .any(|k| head.ends_with(k) && head[..head.len() - k.len()].ends_with([' ', '\t', ';']) || head == *k);

        if let Some(var) = word.strip_prefix('$') {
            let (prefix, var) = match var.strip_prefix('{') {
                Some(v) => ("${", v),
                None => ("$", var),
            };
            let pairs = st
                .vars
                .iter()
                .filter(|v| v.starts_with(var))
                .map(|v| Pair { display: v.clone(), replacement: format!("{prefix}{v}{}", if prefix == "${" { "}" } else { "" }) })
                .collect();
            return Ok((start, pairs));
        }
        if command_pos && !word.contains('/') && !word.is_empty() {
            let pairs: Vec<Pair> = st
                .commands
                .iter()
                .filter(|c| c.starts_with(word))
                .map(|c| Pair { display: c.clone(), replacement: format!("{} ", escape(c)) })
                .collect();
            if !pairs.is_empty() {
                return Ok((start, pairs));
            }
        }
        Ok((start, complete_path(&unescape(word), &st.cwd, st.home.as_deref())))
    }
}

fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if " \t'\"\\$`&|;<>()[]*?{}!#~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn complete_path(word: &str, cwd: &Path, home: Option<&Path>) -> Vec<Pair> {
    let (dir_part, prefix) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let dir = if let Some(rest) = dir_part.strip_prefix("~/").or(if dir_part == "~/" { Some("") } else { None }) {
        match home {
            Some(h) => h.join(rest),
            None => return Vec::new(),
        }
    } else if dir_part.is_empty() {
        cwd.to_path_buf()
    } else {
        crate::utils::resolve(cwd, dir_part)
    };
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let fold = cfg!(windows);
    let mut out: Vec<Pair> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let ok = if fold { name.to_lowercase().starts_with(&prefix.to_lowercase()) } else { name.starts_with(prefix) };
            if !ok || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }
            let is_dir = e.path().is_dir();
            let display = if is_dir { format!("{name}/") } else { name.clone() };
            let replacement = format!("{}{}{}", escape(dir_part), escape(&name), if is_dir { "/" } else { " " });
            Some(Pair { display, replacement })
        })
        .collect();
    out.sort_by(|a, b| a.display.cmp(&b.display));
    out
}

/// A prompt with both forms rustyline needs.
struct Prompt {
    raw: String,
    styled: String,
}

impl rustyline::Prompt for Prompt {
    fn raw(&self) -> &str {
        &self.raw
    }
    fn styled(&self) -> &str {
        &self.styled
    }
}

/// Expand zsh prompt escapes. Returns (plain, styled).
fn render_prompt(sh: &Shell, fmt: &str) -> (String, String) {
    let mut raw = String::new();
    let mut styled = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    let push = |raw: &mut String, styled: &mut String, s: &str| {
        raw.push_str(s);
        styled.push_str(s);
    };
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c != '%' || i >= chars.len() {
            push(&mut raw, &mut styled, &c.to_string());
            continue;
        }
        // Optional numeric argument: %1~
        let mut num = String::new();
        while i < chars.len() && chars[i].is_ascii_digit() {
            num.push(chars[i]);
            i += 1;
        }
        let Some(&e) = chars.get(i) else { break };
        i += 1;
        let user = sh.get("USER").or_else(|| sh.get("USERNAME")).unwrap_or_default();
        let host = sh.get("HOST").or_else(|| sh.get("COMPUTERNAME")).unwrap_or_else(hostname);
        match e {
            '%' => push(&mut raw, &mut styled, "%"),
            'n' => push(&mut raw, &mut styled, &user),
            'm' => push(&mut raw, &mut styled, host.split('.').next().unwrap_or("")),
            'M' => push(&mut raw, &mut styled, &host),
            '~' | 'd' | '/' | 'c' | '.' => {
                let full = if e == '~' || e == 'c' || e == '.' { sh.tilde_path(&sh.cwd) } else { display_path(&sh.cwd) };
                let n: usize = if e == 'c' || e == '.' { num.parse().unwrap_or(1) } else { num.parse().unwrap_or(0) };
                let shown = if n > 0 {
                    let parts: Vec<&str> = full.split('/').filter(|p| !p.is_empty()).collect();
                    if parts.len() > n { parts[parts.len() - n..].join("/") } else { full.clone() }
                } else {
                    full
                };
                push(&mut raw, &mut styled, &shown);
            }
            '#' => push(&mut raw, &mut styled, if is_root() { "#" } else { "%" }),
            '?' => push(&mut raw, &mut styled, &sh.status.to_string()),
            'j' => push(&mut raw, &mut styled, &sh.jobs.lock().unwrap().list.len().to_string()),
            'T' | 't' | '*' | 'D' => {
                let now = jiff::Zoned::now();
                let s = match e {
                    'T' => now.strftime("%H:%M").to_string(),
                    '*' => now.strftime("%H:%M:%S").to_string(),
                    'D' => now.strftime("%y-%m-%d").to_string(),
                    _ => now.strftime("%l:%M%p").to_string(),
                };
                push(&mut raw, &mut styled, &s);
            }
            'B' => styled.push_str("\x1b[1m"),
            'b' => styled.push_str("\x1b[22m"),
            'U' => styled.push_str("\x1b[4m"),
            'u' => styled.push_str("\x1b[24m"),
            'f' => styled.push_str("\x1b[39m"),
            'k' => styled.push_str("\x1b[49m"),
            'F' | 'K' => {
                let mut color = num.clone();
                if chars.get(i) == Some(&'{') {
                    let end = chars[i..].iter().position(|&c| c == '}').map(|p| i + p);
                    if let Some(end) = end {
                        color = chars[i + 1..end].iter().collect();
                        i = end + 1;
                    }
                }
                styled.push_str(&color_code(&color, e == 'K'));
            }
            '(' => {
                // %(?.true.false) and %(N?.true.false)
                let rest: String = chars[i..].iter().collect();
                if let Some((cond, t, f, used)) = parse_ternary(&rest) {
                    i += used;
                    let n: i32 = cond.trim_end_matches('?').parse().unwrap_or(0);
                    let pick = if cond.ends_with('?') { sh.status == n } else { false };
                    let (r, s) = render_prompt(sh, if pick { &t } else { &f });
                    raw.push_str(&r);
                    styled.push_str(&s);
                }
            }
            _ => {
                push(&mut raw, &mut styled, "%");
                push(&mut raw, &mut styled, &e.to_string());
            }
        }
    }
    styled.push_str("\x1b[0m");
    (raw, styled)
}

fn parse_ternary(s: &str) -> Option<(String, String, String, usize)> {
    let sep_pos = s.find(|c: char| !c.is_ascii_digit() && c != '?')?;
    let cond = s[..sep_pos].to_string();
    let sep = s[sep_pos..].chars().next()?;
    let body = &s[sep_pos + sep.len_utf8()..];
    let mid = body.find(sep)?;
    let t = &body[..mid];
    let rest = &body[mid + sep.len_utf8()..];
    let end = rest.find(')')?;
    let f = &rest[..end];
    let used = sep_pos + sep.len_utf8() + mid + sep.len_utf8() + end + 1;
    Some((cond, t.to_string(), f.to_string(), used))
}

fn color_code(name: &str, bg: bool) -> String {
    let base = if bg { 40 } else { 30 };
    let idx = match name {
        "black" => Some(0),
        "red" => Some(1),
        "green" => Some(2),
        "yellow" => Some(3),
        "blue" => Some(4),
        "magenta" => Some(5),
        "cyan" => Some(6),
        "white" => Some(7),
        n => n.parse::<u8>().ok().filter(|n| *n < 8).map(|n| n as i32),
    };
    match (idx, name.parse::<u8>()) {
        (Some(i), _) => format!("\x1b[{}m", base + i),
        (None, Ok(n)) => format!("\x1b[{};5;{}m", if bg { 48 } else { 38 }, n),
        _ => String::new(),
    }
}

#[cfg(unix)]
fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn is_root() -> bool {
    false
}

#[cfg(unix)]
fn hostname() -> String {
    let mut buf = [0u8; 256];
    let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if r != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(not(unix))]
fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

const DEFAULT_PROMPT: &str = "%F{green}%n@%m%f %F{blue}%~%f %(?..%F{red}%?%f )%# ";

fn history_path(sh: &Shell) -> Option<PathBuf> {
    match sh.get("HISTFILE") {
        Some(f) if !f.is_empty() => Some(crate::utils::resolve(&sh.cwd, &f)),
        _ => sh.home().map(|h| h.join(".vsh_history")),
    }
}

fn refresh(sh: &Shell, st: &mut CompState) {
    st.cwd = sh.cwd.clone();
    st.home = sh.home();
    let path = sh.get("PATH").unwrap_or_default();
    if path != st.path || st.commands.is_empty() {
        st.path = path;
        st.commands = crate::path::all_commands(Some(&st.path), &sh.cwd);
        st.commands.extend(crate::builtins::names().map(String::from));
        st.commands.extend(crate::utils::names().map(String::from));
        st.commands.sort();
        st.commands.dedup();
    }
    let mut cmds: Vec<String> = st.commands.clone();
    cmds.extend(sh.funcs.keys().cloned());
    cmds.extend(sh.aliases.keys().cloned());
    cmds.sort();
    cmds.dedup();
    st.commands = cmds;
    let mut vars: Vec<String> = sh.vars.keys().cloned().collect();
    vars.sort();
    st.vars = vars;
}

/// Run the interactive loop; returns the exit status.
pub fn run(sh: &mut Shell) -> i32 {
    let config = Config::builder()
        .history_ignore_space(true)
        .history_ignore_dups(true)
        .unwrap_or_else(|_| Config::builder())
        .completion_type(CompletionType::List)
        .max_history_size(10_000)
        .unwrap_or_else(|_| Config::builder())
        .build();
    let mut rl: Editor<VshHelper, DefaultHistory> = match Editor::with_config(config) {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("vsh: cannot start line editor: {e}");
            return 1;
        }
    };
    let state = Arc::new(Mutex::new(CompState::default()));
    rl.set_helper(Some(VshHelper { state: state.clone(), hinter: HistoryHinter::new() }));
    let hist = history_path(sh);
    if let Some(h) = &hist {
        let _ = rl.load_history(h);
    }
    {
        let mut shared = sh.history.lock().unwrap();
        shared.extend(rl.history().iter().cloned());
    }
    let io = IoSet::default();
    let status = loop {
        for line in sh.jobs.lock().unwrap().take_finished() {
            eprintln!("{line}");
        }
        crate::io::clear_interrupt();
        let _ = std::env::set_current_dir(&sh.cwd);
        refresh(sh, &mut state.lock().unwrap());
        let fmt = sh.get("PROMPT").or_else(|| sh.get("PS1")).unwrap_or_else(|| DEFAULT_PROMPT.into());
        let (raw, styled) = render_prompt(sh, &fmt);
        match rl.readline(&Prompt { raw, styled }) {
            Ok(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                if !line.starts_with(' ') {
                    let _ = rl.add_history_entry(line.as_str());
                    sh.history.lock().unwrap().push(line.clone());
                }
                crate::io::clear_interrupt();
                match sh.run_source(&line, &io) {
                    Ok(s) => sh.status = s,
                    Err(Flow::Exit(s)) => break s,
                    Err(Flow::Return(s)) => sh.status = s,
                    Err(_) => {}
                }
                if crate::io::interrupted() {
                    eprintln!();
                }
            }
            Err(ReadlineError::Interrupted) => {
                sh.status = 130;
            }
            Err(ReadlineError::Eof) => break sh.status,
            Err(e) => {
                eprintln!("vsh: {e}");
                break 1;
            }
        }
        if let Some(h) = &hist {
            let _ = rl.append_history(h);
        }
    };
    if let Some(h) = &hist {
        let _ = rl.save_history(h);
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_escapes() {
        let mut sh = Shell::new();
        sh.set_str("USER", "me");
        sh.status = 3;
        let (raw, _) = render_prompt(&sh, "%n %(?.ok.%?) %%");
        assert_eq!(raw, "me 3 %");
        sh.status = 0;
        let (raw, styled) = render_prompt(&sh, "%F{red}x%f%(?.ok.bad)");
        assert_eq!(raw, "xok");
        assert!(styled.contains("\x1b[31m"));
    }

    #[test]
    fn path_completion_escapes() {
        let d = std::env::temp_dir().join(format!("vsh-comp-{}", std::process::id()));
        std::fs::create_dir_all(d.join("my dir")).unwrap();
        std::fs::write(d.join("my file"), "").unwrap();
        let mut got: Vec<String> = complete_path("my", &d, None).into_iter().map(|p| p.replacement).collect();
        got.sort();
        assert_eq!(got, ["my\\ dir/", "my\\ file "]);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
