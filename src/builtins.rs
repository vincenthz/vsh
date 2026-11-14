//! Shell builtins: commands that need the shell's own state (variables,
//! directory, functions, control flow). The coreutils live in `utils`.

use std::io::{Read, Write};
use std::path::PathBuf;

use crate::exec::{Exec, Flow, Lookup, Resolved, flow_status};
use crate::fmt::quote;
use crate::io::IoSet;
use crate::parser::is_name;
use crate::shell::{OPTION_NAMES, Shell, Value, display_path, join_logical};

pub type BuiltinFn = fn(&mut Shell, &[String], &IoSet) -> Exec;

/// Marks an array literal passed to a declaration (`local a=(x y)`); see the parser.
pub const ARRAY_OPEN: &str = "\u{0}(";
pub const ARRAY_CLOSE: &str = "\u{0})";

const BUILTINS: &[(&str, BuiltinFn)] = &[
    (":", colon),
    (".", source),
    ("alias", alias),
    ("builtin", builtin),
    ("cd", cd),
    ("chdir", cd),
    ("command", command),
    ("continue", continue_),
    ("break", break_),
    ("declare", typeset),
    ("dirs", dirs),
    ("env", env),
    ("eval", eval),
    ("exec", exec),
    ("exit", exit),
    ("export", export),
    ("functions", functions),
    ("getopts", getopts),
    ("hash", colon),
    ("history", history),
    ("integer", typeset),
    ("jobs", jobs),
    ("let", let_),
    ("local", typeset),
    ("logout", exit),
    ("popd", popd),
    ("pushd", pushd),
    ("read", read),
    ("readonly", typeset),
    ("rehash", colon),
    ("return", return_),
    ("set", set),
    ("setopt", setopt),
    ("shift", shift),
    ("source", source),
    ("time", time),
    ("trap", trap),
    ("type", type_),
    ("typeset", typeset),
    ("umask", umask),
    ("unalias", unalias),
    ("unfunction", unfunction),
    ("unset", unset),
    ("unsetopt", setopt),
    ("wait", wait),
    ("whence", whence),
    ("where", where_),
    ("which", which),
    ("xargs", xargs),
];

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    BUILTINS.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
}

pub fn names() -> impl Iterator<Item = &'static str> {
    BUILTINS.iter().map(|(n, _)| *n)
}

/// Names that take `NAME=(...)` array arguments.
pub fn is_declaration(name: &str) -> bool {
    matches!(name, "local" | "typeset" | "declare" | "export" | "readonly" | "integer")
}

struct Out {
    out: Box<dyn Write + Send>,
    err: Box<dyn Write + Send>,
}

impl Out {
    fn new(io: &IoSet) -> Out {
        Out { out: io.stdout.writer(), err: io.stderr.writer() }
    }
    fn line(&mut self, s: impl AsRef<str>) {
        let _ = writeln!(self.out, "{}", s.as_ref());
    }
    fn err(&mut self, s: impl AsRef<str>) {
        let _ = writeln!(self.err, "{}", s.as_ref());
    }
}

impl Drop for Out {
    fn drop(&mut self) {
        let _ = self.out.flush();
        let _ = self.err.flush();
    }
}

fn colon(_: &mut Shell, _: &[String], _: &IoSet) -> Exec {
    Ok(0)
}

fn cd(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut o = Out::new(io);
    let mut args: Vec<&str> = argv[1..].iter().map(|s| s.as_str()).collect();
    let mut physical = false;
    while let Some(a) = args.first() {
        match *a {
            "-P" => physical = true,
            "-L" => physical = false,
            "--" => {
                args.remove(0);
                break;
            }
            _ => break,
        }
        args.remove(0);
    }
    let (target, print) = match args.as_slice() {
        [] => match sh.home() {
            Some(h) => (display_path(&h), false),
            None => {
                o.err("cd: HOME not set");
                return Ok(1);
            }
        },
        ["-"] => match sh.get("OLDPWD") {
            Some(p) => (p, true),
            None => {
                o.err("cd: OLDPWD not set");
                return Ok(1);
            }
        },
        [dir] => (dir.to_string(), false),
        [old, new] => {
            // zsh: cd OLD NEW replaces OLD with NEW in $PWD.
            let pwd = display_path(&sh.cwd);
            if !pwd.contains(old) {
                o.err(format!("cd: string not in pwd: {old}"));
                return Ok(1);
            }
            (pwd.replacen(old, new, 1), true)
        }
        _ => {
            o.err("cd: too many arguments");
            return Ok(1);
        }
    };
    let mut dest = join_logical(&sh.cwd, &target);
    if !dest.is_dir() && !target.starts_with('/') && !target.starts_with('.') {
        // CDPATH
        if let Some(cdpath) = sh.get("CDPATH") {
            for base in cdpath.split(crate::path::PATH_SEP).filter(|b| !b.is_empty()) {
                let cand = join_logical(&crate::utils::resolve(&sh.cwd, base), &target);
                if cand.is_dir() {
                    dest = cand;
                    break;
                }
            }
        }
    }
    match std::fs::metadata(&dest) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            o.err(format!("cd: not a directory: {target}"));
            return Ok(1);
        }
        Err(e) => {
            o.err(format!("cd: {}: {target}", crate::fmt::io_error(&e)));
            return Ok(1);
        }
    }
    if let Err(e) = std::fs::read_dir(&dest) {
        o.err(format!("cd: {}: {target}", crate::fmt::io_error(&e)));
        return Ok(1);
    }
    if physical && let Ok(c) = std::fs::canonicalize(&dest) {
        dest = PathBuf::from(display_path(&c));
    }
    let old = display_path(&sh.cwd);
    sh.set_str("OLDPWD", old);
    sh.export("OLDPWD");
    sh.set_cwd(dest);
    if print {
        o.line(sh.tilde_path(&sh.cwd.clone()));
    }
    Ok(0)
}

fn pushd(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let prev = sh.cwd.clone();
    let target = match argv.get(1) {
        Some(d) => d.clone(),
        None => match sh.dirstack.pop() {
            Some(d) => display_path(&d),
            None => {
                Out::new(io).err("pushd: no other directory");
                return Ok(1);
            }
        },
    };
    let st = cd(sh, &["cd".into(), target], io)?;
    if st == 0 {
        sh.dirstack.push(prev);
        dirs(sh, &[], io)?;
    }
    Ok(st)
}

fn popd(sh: &mut Shell, _argv: &[String], io: &IoSet) -> Exec {
    let Some(d) = sh.dirstack.pop() else {
        Out::new(io).err("popd: directory stack empty");
        return Ok(1);
    };
    let st = cd(sh, &["cd".into(), display_path(&d)], io)?;
    if st == 0 {
        dirs(sh, &[], io)?;
    }
    Ok(st)
}

fn dirs(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    if argv.get(1).map(|s| s.as_str()) == Some("-c") {
        sh.dirstack.clear();
        return Ok(0);
    }
    let mut parts = vec![sh.tilde_path(&sh.cwd.clone())];
    for d in sh.dirstack.iter().rev() {
        parts.push(sh.tilde_path(d));
    }
    Out::new(io).line(parts.join(" "));
    Ok(0)
}

fn var_decl(name: &str, v: &Value) -> String {
    match v {
        Value::Str(s) => format!("{name}={}", quote(s)),
        Value::Array(a) => format!("{name}=( {} )", a.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ")),
    }
}

/// Split declaration arguments into (name, value) pairs, decoding array
/// literals.
fn decl_args(args: &[String]) -> Vec<(String, Option<Value>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if let Some(name) = a.strip_suffix(ARRAY_OPEN) {
            let mut elems = Vec::new();
            while i < args.len() && args[i] != ARRAY_CLOSE {
                elems.push(args[i].clone());
                i += 1;
            }
            i += 1;
            out.push((name.trim_end_matches('=').to_string(), Some(Value::Array(elems))));
            continue;
        }
        match a.split_once('=') {
            Some((n, v)) => out.push((n.to_string(), Some(Value::Str(v.to_string())))),
            None => out.push((a.clone(), None)),
        }
    }
    out
}

fn export(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut o = Out::new(io);
    let mut unexport = false;
    let mut args = &argv[1..];
    while let Some(a) = args.first().filter(|a| a.starts_with('-') && a.len() > 1) {
        match a.as_str() {
            "-n" => unexport = true,
            "-p" => {}
            "--" => {
                args = &args[1..];
                break;
            }
            _ => {
                o.err(format!("export: bad option: {a}"));
                return Ok(1);
            }
        }
        args = &args[1..];
    }
    if args.is_empty() {
        let mut names: Vec<&String> = sh.vars.iter().filter(|(_, v)| v.exported).map(|(k, _)| k).collect();
        names.sort();
        for n in names {
            o.line(format!("export {}", var_decl(n, &sh.vars[n].value)));
        }
        return Ok(0);
    }
    let mut status = 0;
    for (name, value) in decl_args(args) {
        if !is_name(&name) {
            o.err(format!("export: not an identifier: {name}"));
            status = 1;
            continue;
        }
        if let Some(v) = value
            && !sh.set_var(&name, v)
        {
            status = 1;
            continue;
        }
        if unexport {
            if let Some(v) = sh.vars.get_mut(&name) {
                v.exported = false;
            }
        } else {
            sh.export(&name);
        }
    }
    Ok(status)
}

fn typeset(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut o = Out::new(io);
    let cmd = argv[0].as_str();
    let (mut array, mut exported, mut readonly, mut global, mut funcs, mut print) =
        (false, false, cmd == "readonly", false, false, false);
    let mut args = &argv[1..];
    while let Some(a) = args.first().filter(|a| (a.starts_with('-') || a.starts_with('+')) && a.len() > 1) {
        if a == "--" {
            args = &args[1..];
            break;
        }
        for c in a[1..].chars() {
            match c {
                'a' => array = true,
                'x' => exported = true,
                'r' => readonly = true,
                'g' => global = true,
                'f' => funcs = true,
                'p' => print = true,
                'i' | 'l' | 'u' | 'U' | 'L' | 'Z' | 'R' | 'F' | 'E' | 'T' | 'H' | 'h' | 't' | 'm' => {}
                'A' => {
                    o.err(format!("{cmd}: associative arrays are not supported"));
                    return Ok(1);
                }
                _ => {
                    o.err(format!("{cmd}: bad option: -{c}"));
                    return Ok(1);
                }
            }
        }
        args = &args[1..];
    }
    if funcs {
        drop(o);
        return functions(sh, &[&["functions".to_string()], args].concat(), io);
    }
    if args.is_empty() || print && args.is_empty() {
        let mut names: Vec<&String> = sh.vars.keys().collect();
        names.sort();
        for n in names {
            let v = &sh.vars[n];
            if readonly && !v.readonly {
                continue;
            }
            let flags = format!("{}{}", if v.exported { "-x " } else { "" }, if v.readonly { "-r " } else { "" });
            o.line(format!("typeset {flags}{}", var_decl(n, &v.value)));
        }
        return Ok(0);
    }
    let local = !global && !sh.locals.is_empty() && matches!(cmd, "local" | "typeset" | "declare" | "integer");
    let mut status = 0;
    for (name, value) in decl_args(args) {
        if !is_name(&name) {
            o.err(format!("{cmd}: not an identifier: {name}"));
            status = 1;
            continue;
        }
        if print {
            if let Some(v) = sh.vars.get(&name) {
                o.line(format!("typeset {}", var_decl(&name, &v.value)));
            }
            continue;
        }
        let existed = sh.vars.contains_key(&name);
        if local {
            sh.make_local(&name);
        }
        let value = match value {
            Some(v) => Some(v),
            None if array => Some(Value::Array(Vec::new())),
            None if local || !existed => Some(Value::Str(String::new())),
            None => None,
        };
        if let Some(v) = value {
            let v = match (array, v) {
                (true, Value::Str(s)) if s.is_empty() => Value::Array(Vec::new()),
                (true, Value::Str(s)) => Value::Array(vec![s]),
                (_, v) => v,
            };
            if local {
                // A new local starts fresh, even if the outer one was exported.
                sh.vars.remove(&name);
            }
            if !sh.set_var(&name, v) {
                status = 1;
                continue;
            }
        }
        if exported {
            sh.export(&name);
        }
        if readonly && let Some(v) = sh.vars.get_mut(&name) {
            v.readonly = true;
        }
    }
    Ok(status)
}

fn unset(sh: &mut Shell, argv: &[String], _io: &IoSet) -> Exec {
    let mut funcs = false;
    let mut status = 0;
    for a in &argv[1..] {
        match a.as_str() {
            "-f" => funcs = true,
            "-v" => funcs = false,
            n if funcs => {
                sh.funcs.remove(n);
            }
            n => {
                if !sh.unset(n) {
                    status = 1;
                }
            }
        }
    }
    Ok(status)
}

fn unfunction(sh: &mut Shell, argv: &[String], _io: &IoSet) -> Exec {
    for n in &argv[1..] {
        sh.funcs.remove(n);
    }
    Ok(0)
}

fn functions(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut o = Out::new(io);
    let mut names: Vec<String> =
        if argv.len() > 1 { argv[1..].to_vec() } else { sh.funcs.keys().cloned().collect::<Vec<_>>() };
    names.sort();
    let mut status = 0;
    for n in names {
        match sh.funcs.get(&n) {
            Some(body) => o.line(format!("{n} () {}", crate::fmt::command(body, 0))),
            None => status = 1,
        }
    }
    Ok(status)
}

fn set(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut o = Out::new(io);
    if argv.len() == 1 {
        let mut names: Vec<&String> = sh.vars.keys().collect();
        names.sort();
        for n in names {
            o.line(var_decl(n, &sh.vars[n].value));
        }
        return Ok(0);
    }
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--" {
            sh.args = argv[i + 1..].to_vec();
            return Ok(0);
        }
        let on = a.starts_with('-');
        if !(on || a.starts_with('+')) || a.len() < 2 {
            sh.args = argv[i..].to_vec();
            return Ok(0);
        }
        for c in a[1..].chars() {
            let name = match c {
                'e' => "errexit",
                'x' => "xtrace",
                'u' => "nounset",
                'f' => "noglob",
                'o' => {
                    i += 1;
                    match argv.get(i) {
                        Some(n) => {
                            if !sh.opts.set(n, on) {
                                o.err(format!("set: no such option: {n}"));
                                return Ok(1);
                            }
                        }
                        None => {
                            for n in OPTION_NAMES {
                                let v = sh.opts.get(n).unwrap_or(false);
                                o.line(format!("{n:<16}{}", if v { "on" } else { "off" }));
                            }
                        }
                    }
                    continue;
                }
                _ => {
                    o.err(format!("set: bad option: {c}"));
                    return Ok(1);
                }
            };
            sh.opts.set(name, on);
        }
        i += 1;
    }
    Ok(0)
}

fn setopt(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut o = Out::new(io);
    let on = argv[0] == "setopt";
    if argv.len() == 1 {
        for n in OPTION_NAMES {
            if sh.opts.get(n) == Some(on) && *n != "nomatch" {
                o.line(n);
            }
        }
        return Ok(0);
    }
    let mut status = 0;
    for n in &argv[1..] {
        if !sh.opts.set(n, on) {
            o.err(format!("{}: no such option: {n}", argv[0]));
            status = 1;
        }
    }
    Ok(status)
}

fn alias(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut o = Out::new(io);
    let args: Vec<&String> = argv[1..].iter().filter(|a| !matches!(a.as_str(), "-L" | "--")).collect();
    if args.is_empty() {
        let mut names: Vec<&String> = sh.aliases.keys().collect();
        names.sort();
        for n in names {
            o.line(format!("{n}={}", quote(&sh.aliases[n])));
        }
        return Ok(0);
    }
    let mut status = 0;
    for a in args {
        match a.split_once('=') {
            Some((n, v)) => {
                sh.aliases.insert(n.to_string(), v.to_string());
            }
            None => match sh.aliases.get(a.as_str()) {
                Some(v) => o.line(format!("{a}={}", quote(v))),
                None => status = 1,
            },
        }
    }
    Ok(status)
}

fn unalias(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut status = 0;
    for a in &argv[1..] {
        if a == "-a" {
            sh.aliases.clear();
        } else if sh.aliases.remove(a).is_none() {
            Out::new(io).err(format!("unalias: no such hash table element: {a}"));
            status = 1;
        }
    }
    Ok(status)
}

fn source(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let Some(file) = argv.get(1) else {
        Out::new(io).err(format!("{}: not enough arguments", argv[0]));
        return Ok(1);
    };
    let mut path = crate::utils::resolve(&sh.cwd, file);
    if !path.exists() && !crate::path::has_dir(file) {
        // Like zsh, fall back to a PATH search.
        if let Some(p) = sh.get("PATH").and_then(|pv| {
            pv.split(crate::path::PATH_SEP).map(|d| crate::utils::resolve(&sh.cwd, d).join(file)).find(|p| p.is_file())
        }) {
            path = p;
        }
    }
    let args = if argv.len() > 2 { Some(argv[2..].to_vec()) } else { None };
    sh.source_file(&path, args, io)
}

fn exit(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let code = match argv.get(1) {
        Some(a) => match a.parse::<i32>() {
            Ok(n) => n & 0xff,
            Err(_) => {
                Out::new(io).err(format!("exit: bad number: {a}"));
                2
            }
        },
        None => sh.status,
    };
    Err(Flow::Exit(code))
}

fn return_(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let code = match argv.get(1) {
        Some(a) => match sh.arith(a) {
            Ok(n) => n as i32,
            Err(e) => {
                Out::new(io).err(format!("return: {e}"));
                return Ok(1);
            }
        },
        None => sh.status,
    };
    Err(Flow::Return(code))
}

fn loop_count(sh: &Shell, argv: &[String], io: &IoSet) -> Result<usize, i32> {
    if sh.loop_depth == 0 {
        Out::new(io).err(format!("{}: not in while, until, select, or repeat loop", argv[0]));
        return Err(1);
    }
    match argv.get(1).map(|a| a.parse::<usize>()) {
        None => Ok(1),
        Some(Ok(n)) if n > 0 => Ok(n),
        _ => {
            Out::new(io).err(format!("{}: argument is not positive: {}", argv[0], argv[1]));
            Err(1)
        }
    }
}

fn break_(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    match loop_count(sh, argv, io) {
        Ok(n) => Err(Flow::Break(n)),
        Err(s) => Ok(s),
    }
}

fn continue_(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    match loop_count(sh, argv, io) {
        Ok(n) => Err(Flow::Continue(n)),
        Err(s) => Ok(s),
    }
}

fn shift(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let n = match argv.get(1) {
        Some(a) => match sh.arith(a) {
            Ok(n) if n >= 0 => n as usize,
            _ => {
                Out::new(io).err(format!("shift: bad number: {a}"));
                return Ok(1);
            }
        },
        None => 1,
    };
    if n > sh.args.len() {
        Out::new(io).err("shift: shift count must be <= $#");
        return Ok(1);
    }
    sh.args.drain(..n);
    Ok(0)
}

fn read(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut o = Out::new(io);
    let (mut raw, mut silent, mut prompt, mut delim, mut nchars, mut array) =
        (false, false, None::<String>, b'\n', None::<usize>, None::<String>);
    let mut names = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        i += 1;
        if !a.starts_with('-') || a.len() < 2 || !names.is_empty() {
            names.push(a.clone());
            continue;
        }
        let mut chars = a[1..].chars();
        while let Some(c) = chars.next() {
            let mut value = || -> Option<String> {
                let rest: String = chars.clone().collect();
                if !rest.is_empty() {
                    chars = "".chars();
                    Some(rest)
                } else {
                    i += 1;
                    argv.get(i - 1).cloned()
                }
            };
            match c {
                'r' => raw = true,
                's' => silent = true,
                'p' => prompt = value(),
                'd' => delim = value().and_then(|d| d.bytes().next()).unwrap_or(0),
                'n' | 'k' => nchars = value().and_then(|n| n.parse().ok()),
                'a' | 'A' => array = value(),
                't' | 'u' => {
                    value();
                }
                'e' | 'E' | 'q' | 'z' => {}
                _ => {
                    o.err(format!("read: bad option: -{c}"));
                    return Ok(2);
                }
            }
        }
    }
    // zsh: read 'name?prompt'
    if let Some(first) = names.first_mut()
        && let Some((n, p)) = first.split_once('?')
    {
        prompt = Some(p.to_string());
        *first = n.to_string();
    }
    let interactive_in = matches!(io.stdin, crate::io::Input::Inherit) && {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
    };
    if let Some(p) = &prompt
        && interactive_in
    {
        let _ = write!(o.err, "{p}");
        let _ = o.err.flush();
    }
    let _echo = if silent && interactive_in { crate::term::NoEcho::new() } else { None };
    let mut reader = io.stdin.reader();
    let mut bytes = Vec::new();
    let mut got_delim = false;
    let mut byte = [0u8; 1];
    loop {
        if nchars.is_some_and(|n| String::from_utf8_lossy(&bytes).chars().count() >= n) {
            got_delim = true;
            break;
        }
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                let b = byte[0];
                if !raw && b == b'\\' {
                    match reader.read(&mut byte) {
                        Ok(1) if byte[0] == b'\n' => continue,
                        Ok(1) => {
                            // Keep escaped IFS characters literal by marking them.
                            bytes.push(0);
                            bytes.push(byte[0]);
                            continue;
                        }
                        _ => break,
                    }
                }
                if b == delim && nchars.is_none() {
                    got_delim = true;
                    break;
                }
                bytes.push(b);
            }
            Err(_) => {
                drop(_echo);
                return Ok(if crate::io::interrupted() { 130 } else { 1 });
            }
        }
    }
    if silent && interactive_in {
        let _ = writeln!(o.err);
    }
    if bytes.last() == Some(&b'\r') && delim == b'\n' {
        bytes.pop();
    }
    let line = String::from_utf8_lossy(&bytes).into_owned();
    let ifs = sh.ifs();
    let fields = split_read(&line, &ifs, if array.is_some() { usize::MAX } else { names.len().max(1) });
    if let Some(arr) = array {
        sh.set_var(&arr, Value::Array(fields));
    } else if names.is_empty() {
        sh.set_str("REPLY", unmark(&line));
    } else {
        for (k, name) in names.iter().enumerate() {
            let v = fields.get(k).cloned().unwrap_or_default();
            if !sh.set_str(name, v) {
                return Ok(1);
            }
        }
    }
    Ok(if got_delim || !bytes.is_empty() && delim != b'\n' { 0 } else { 1 })
}

fn unmark(s: &str) -> String {
    s.replace('\0', "")
}

/// IFS splitting for `read`: the last field takes the rest of the line.
fn split_read(line: &str, ifs: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let is_sep = |i: usize| ifs.contains(chars[i]) && (i == 0 || chars[i - 1] != '\0');
    let mut fields = Vec::new();
    let mut i = 0;
    while i < chars.len() && is_sep(i) && chars[i].is_whitespace() {
        i += 1;
    }
    while i < chars.len() {
        if fields.len() + 1 == max {
            let rest: String = chars[i..].iter().collect();
            let rest = rest.trim_end_matches(|c: char| ifs.contains(c) && c.is_whitespace());
            fields.push(unmark(rest));
            return fields;
        }
        let start = i;
        while i < chars.len() && !is_sep(i) {
            i += 1;
        }
        fields.push(unmark(&chars[start..i].iter().collect::<String>()));
        // Skip one separator plus surrounding IFS whitespace.
        while i < chars.len() && is_sep(i) && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() && is_sep(i) {
            i += 1;
            while i < chars.len() && is_sep(i) && chars[i].is_whitespace() {
                i += 1;
            }
        }
    }
    fields
}

fn eval(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let src = argv[1..].join(" ");
    sh.run_source(&src, io)
}

fn let_(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut last = 0;
    for a in &argv[1..] {
        match sh.arith(a) {
            Ok(v) => last = v,
            Err(e) => {
                Out::new(io).err(format!("let: {e}"));
                return Ok(2);
            }
        }
    }
    Ok((last == 0) as i32)
}

fn exec(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let args: Vec<String> = argv[1..].iter().skip_while(|a| *a == "--").cloned().collect();
    if args.is_empty() {
        return Ok(0);
    }
    if let Resolved::External(path) = sh.resolve(&args[0], Lookup::EXTERNAL, None) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let _ = std::io::stdout().flush();
            let mut cmd = std::process::Command::new(&path);
            cmd.arg0(&args[0]).args(&args[1..]).env_clear().envs(sh.env()).current_dir(&sh.cwd);
            if let (Ok((stdin, _)), Ok(out), Ok(err)) = (io.stdin.stdio(), io.stdout.stdio(), io.stderr.stdio()) {
                cmd.stdin(stdin).stdout(out).stderr(err);
            }
            let e = cmd.exec();
            Out::new(io).err(format!("exec: {}: {}", crate::fmt::io_error(&e), args[0]));
            return Ok(127);
        }
        #[cfg(not(unix))]
        {
            let st = sh.run_external(&path, &args, &[], io);
            return Err(Flow::Exit(st));
        }
    }
    let st = flow_status(sh.run_command(&args, &[], io, Lookup::ALL));
    Err(Flow::Exit(st))
}

fn builtin(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let Some(name) = argv.get(1) else { return Ok(0) };
    if lookup(name).is_none() && crate::utils::lookup(name).is_none() {
        Out::new(io).err(format!("builtin: no such builtin: {name}"));
        return Ok(1);
    }
    sh.run_command(&argv[1..], &[], io, Lookup { functions: false, builtins: true, utils: true })
}

/// `command [-v|-V|-p] name args`: skip functions; run the `$PATH` program,
/// falling back to the builtin utility when there is none.
fn command(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut args = &argv[1..];
    let mut mode = None;
    while let Some(a) = args.first() {
        match a.as_str() {
            "-v" | "-V" => mode = Some(a.clone()),
            "-p" => {}
            "--" => {
                args = &args[1..];
                break;
            }
            _ => break,
        }
        args = &args[1..];
    }
    if args.is_empty() {
        return Ok(0);
    }
    if let Some(m) = mode {
        let mut o = Out::new(io);
        let mut status = 0;
        for n in args {
            match describe(sh, n, if m == "-v" { Style::Whence } else { Style::Type }, false).first() {
                Some(line) => o.line(line),
                None => {
                    if m == "-V" {
                        o.err(format!("{n} not found"));
                    }
                    status = 1;
                }
            }
        }
        return Ok(status);
    }
    let name = &args[0];
    if lookup(name).is_some() {
        return sh.run_command(args, &[], io, Lookup { functions: false, builtins: true, utils: false });
    }
    match sh.resolve(name, Lookup::EXTERNAL, None) {
        Resolved::External(p) => Ok(sh.run_external(&p, args, &[], io)),
        _ => sh.run_command(args, &[], io, Lookup { functions: false, builtins: false, utils: true }),
    }
}

/// `env [-i] [-u NAME] [NAME=value]... [cmd args...]`
fn env(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut clear = false;
    let mut unset = Vec::new();
    let mut assigns: Vec<(String, String)> = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        match a.as_str() {
            "-i" | "-" => clear = true,
            "-u" => {
                i += 1;
                if let Some(n) = argv.get(i) {
                    unset.push(n.clone());
                }
            }
            "--" => {
                i += 1;
                break;
            }
            s if s.starts_with("-u") => unset.push(s[2..].to_string()),
            s if s.contains('=') && !s.starts_with('=') => {
                let (n, v) = s.split_once('=').unwrap();
                assigns.push((n.into(), v.into()));
            }
            _ => break,
        }
        i += 1;
    }
    let cmd = &argv[i.min(argv.len())..];
    if cmd.is_empty() && !clear && unset.is_empty() && assigns.is_empty() {
        let f = crate::utils::lookup("env").expect("env util");
        return Ok(sh.run_util(f, &argv[..1], &[], io));
    }
    let mut sub = sh.clone();
    if clear {
        for v in sub.vars.values_mut() {
            v.exported = false;
        }
    }
    for n in &unset {
        if let Some(v) = sub.vars.get_mut(n) {
            v.exported = false;
        }
    }
    if cmd.is_empty() {
        let mut o = Out::new(io);
        let mut env = sub.env();
        for (n, v) in assigns {
            env.retain(|(k, _)| *k != n);
            env.push((n, v));
        }
        env.sort();
        for (k, v) in env {
            o.line(format!("{k}={v}"));
        }
        return Ok(0);
    }
    let r = sub.run_command(cmd, &assigns, io, Lookup { functions: false, builtins: false, utils: true });
    Ok(flow_status(r))
}

enum Style {
    /// `type`: "ls is /bin/ls"
    Type,
    /// `whence` / `command -v`: just the name, path or alias
    Whence,
    /// `which` (zsh): aliases and functions shown in full
    Which,
}

/// Describe every interpretation of a name (or only the first unless `all`).
fn describe(sh: &Shell, name: &str, style: Style, all: bool) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(a) = sh.aliases.get(name) {
        out.push(match style {
            Style::Type => format!("{name} is an alias for {a}"),
            Style::Whence => format!("alias {name}={}", quote(a)),
            Style::Which => format!("{name}: aliased to {a}"),
        });
    }
    if let Some(f) = sh.funcs.get(name) {
        out.push(match style {
            Style::Type => format!("{name} is a shell function"),
            Style::Whence => name.to_string(),
            Style::Which => format!("{name} () {}", crate::fmt::command(f, 0)),
        });
    }
    if lookup(name).is_some() {
        out.push(match style {
            Style::Type => format!("{name} is a shell builtin"),
            Style::Whence => name.to_string(),
            Style::Which => format!("{name}: shell built-in command"),
        });
    }
    if crate::utils::lookup(name).is_some() {
        out.push(match style {
            Style::Type => format!("{name} is a vsh builtin utility"),
            Style::Whence => name.to_string(),
            Style::Which => format!("{name}: vsh built-in utility"),
        });
    }
    let path = sh.get("PATH");
    let ext = sh.get("PATHEXT");
    let found = if all {
        crate::path::find_all(name, path.as_deref(), ext.as_deref(), &sh.cwd)
    } else {
        crate::path::find(name, path.as_deref(), ext.as_deref(), &sh.cwd).into_iter().collect()
    };
    for p in found {
        let p = display_path(&p);
        out.push(match style {
            Style::Type => format!("{name} is {p}"),
            _ => p,
        });
    }
    if !all {
        out.truncate(1);
    }
    out
}

fn describe_cmd(sh: &mut Shell, argv: &[String], io: &IoSet, style: fn(bool) -> Style, mut all: bool) -> Exec {
    let mut o = Out::new(io);
    let mut verbose = false;
    let mut status = 0;
    for a in &argv[1..] {
        match a.as_str() {
            "-a" => all = true,
            "-v" => verbose = true,
            "-p" | "-s" | "-w" | "-c" | "-f" | "-m" | "-S" | "--" => {}
            name => {
                let lines = describe(sh, name, style(verbose), all);
                if lines.is_empty() {
                    if argv[0] == "type" || argv[0] == "which" || verbose {
                        o.line(format!("{name} not found"));
                    }
                    status = 1;
                }
                for l in lines {
                    o.line(l);
                }
            }
        }
    }
    Ok(status)
}

fn type_(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    describe_cmd(sh, argv, io, |_| Style::Type, false)
}

fn whence(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    describe_cmd(sh, argv, io, |v| if v { Style::Type } else { Style::Whence }, false)
}

fn which(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    describe_cmd(sh, argv, io, |_| Style::Which, false)
}

fn where_(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    describe_cmd(sh, argv, io, |_| Style::Which, true)
}

fn history(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut o = Out::new(io);
    let mut h = sh.history.lock().unwrap();
    if argv.get(1).map(|s| s.as_str()) == Some("-c") {
        h.clear();
        return Ok(0);
    }
    let n = argv.get(1).and_then(|a| a.trim_start_matches('-').parse::<usize>().ok()).unwrap_or(16);
    let start = h.len().saturating_sub(n);
    for (i, line) in h.iter().enumerate().skip(start) {
        o.line(format!("{:>5}  {}", i + 1, line));
    }
    Ok(0)
}

fn jobs(sh: &mut Shell, _argv: &[String], io: &IoSet) -> Exec {
    let mut o = Out::new(io);
    let mut jobs = sh.jobs.lock().unwrap();
    jobs.reap();
    for j in &jobs.list {
        let state = match j.status {
            None => "running".to_string(),
            Some(0) => "done".to_string(),
            Some(s) => format!("exit {s}"),
        };
        o.line(format!("[{}]    {:<10} {}", j.id, state, j.cmdline));
    }
    Ok(0)
}

fn wait(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let ids: Vec<usize> = if argv.len() > 1 {
        let mut ids = Vec::new();
        for a in &argv[1..] {
            match a.trim_start_matches('%').parse() {
                Ok(n) => ids.push(n),
                Err(_) => {
                    Out::new(io).err(format!("wait: job not found: {a}"));
                    return Ok(127);
                }
            }
        }
        ids
    } else {
        sh.jobs.lock().unwrap().list.iter().map(|j| j.id).collect()
    };
    let mut status = 0;
    for id in ids {
        let job = sh.jobs.lock().unwrap().take(id);
        match job {
            Some(mut j) => {
                status = match (j.status, j.handle.take()) {
                    (Some(s), _) => s,
                    (None, Some(h)) => {
                        // Block without holding the job table lock.
                        loop {
                            if h.is_finished() {
                                break h.join().unwrap_or(1);
                            }
                            if crate::io::interrupted() {
                                // Put it back; the job keeps running.
                                sh.jobs.lock().unwrap().list.push(crate::jobs::Job { handle: Some(h), ..j });
                                return Ok(130);
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                    (None, None) => 0,
                };
            }
            None => {
                Out::new(io).err(format!("wait: job not found: %{id}"));
                status = 127;
            }
        }
    }
    Ok(status)
}

fn time(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    if argv.len() < 2 {
        return Ok(0);
    }
    let start = std::time::Instant::now();
    let before = crate::term::cpu_times();
    let r = sh.run_command(&argv[1..], &[], io, Lookup::ALL);
    let total = start.elapsed().as_secs_f64();
    let after = crate::term::cpu_times();
    let (user, sys) = (after.0 - before.0, after.1 - before.1);
    let cpu = if total > 0.0 { ((user + sys) / total * 100.0).round() as i64 } else { 0 };
    let line = argv[1..].iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    Out::new(io).err(format!("{line}  {user:.2}s user {sys:.2}s system {cpu}% cpu {total:.3} total"));
    r
}

fn trap(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    if argv.len() < 3 {
        if let Some(t) = &sh.exit_trap {
            Out::new(io).line(format!("trap -- {} EXIT", quote(t)));
        }
        return Ok(0);
    }
    let action = &argv[1];
    for sig in &argv[2..] {
        match sig.as_str() {
            "EXIT" | "0" => sh.exit_trap = if action == "-" || action.is_empty() { None } else { Some(action.clone()) },
            // Signal traps aren't supported; accept them so scripts keep going.
            _ => {}
        }
    }
    Ok(0)
}

fn getopts(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    if argv.len() < 3 {
        Out::new(io).err("getopts: not enough arguments");
        return Ok(2);
    }
    let spec = argv[1].as_str();
    let var = argv[2].as_str();
    let args: Vec<String> = if argv.len() > 3 { argv[3..].to_vec() } else { sh.args.clone() };
    let silent = spec.starts_with(':');
    let spec = spec.trim_start_matches(':');
    let mut optind: usize = sh.get("OPTIND").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
    if sh.optpos == 0 {
        sh.optpos = 1;
    }
    let Some(arg) = args.get(optind - 1) else {
        sh.set_str(var, "?");
        sh.optpos = 0;
        return Ok(1);
    };
    if sh.optpos == 1 && (arg == "--" || !arg.starts_with('-') || arg == "-") {
        if arg == "--" {
            optind += 1;
        }
        sh.set_str("OPTIND", optind.to_string());
        sh.set_str(var, "?");
        sh.optpos = 0;
        return Ok(1);
    }
    let chars: Vec<char> = arg.chars().collect();
    let c = chars[sh.optpos];
    sh.optpos += 1;
    let at_end = sh.optpos >= chars.len();
    let advance = |sh: &mut Shell, optind: &mut usize| {
        *optind += 1;
        sh.optpos = 1;
    };
    match spec.find(c) {
        Some(i) if c != ':' => {
            let takes = spec[i + c.len_utf8()..].starts_with(':');
            if takes {
                let value = if !at_end {
                    let v: String = chars[sh.optpos..].iter().collect();
                    advance(sh, &mut optind);
                    Some(v)
                } else {
                    advance(sh, &mut optind);
                    let v = args.get(optind - 1).cloned();
                    if v.is_some() {
                        optind += 1;
                    }
                    v
                };
                match value {
                    Some(v) => {
                        sh.set_str("OPTARG", v);
                        sh.set_str(var, c.to_string());
                    }
                    None => {
                        if silent {
                            sh.set_str("OPTARG", c.to_string());
                            sh.set_str(var, ":");
                        } else {
                            Out::new(io).err(format!("vsh: argument expected after -{c} option"));
                            sh.set_str(var, "?");
                        }
                    }
                }
            } else {
                if at_end {
                    advance(sh, &mut optind);
                }
                sh.set_str(var, c.to_string());
            }
        }
        _ => {
            if at_end {
                advance(sh, &mut optind);
            }
            if silent {
                sh.set_str("OPTARG", c.to_string());
            } else {
                Out::new(io).err(format!("vsh: bad option: -{c}"));
            }
            sh.set_str(var, "?");
        }
    }
    sh.set_str("OPTIND", optind.to_string());
    Ok(0)
}

fn umask(_sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    #[cfg(unix)]
    {
        let mut o = Out::new(io);
        match argv.get(1) {
            None => {
                let m = unsafe { libc::umask(0) };
                unsafe { libc::umask(m) };
                o.line(format!("{:03o}", m));
            }
            Some(v) => match u32::from_str_radix(v, 8) {
                Ok(m) if m <= 0o777 => unsafe {
                    libc::umask(m as libc::mode_t);
                },
                _ => {
                    o.err(format!("umask: bad umask: {v}"));
                    return Ok(1);
                }
            },
        }
        Ok(0)
    }
    #[cfg(not(unix))]
    {
        if argv.len() == 1 {
            Out::new(io).line("000");
        }
        Ok(0)
    }
}

/// `xargs [-0] [-d C] [-n N] [-I R] [-r] [-t] [cmd args...]`
fn xargs(sh: &mut Shell, argv: &[String], io: &IoSet) -> Exec {
    let mut o = Out::new(io);
    let (mut nul, mut delim, mut max_args, mut replace, mut no_empty, mut trace) =
        (false, None::<char>, None::<usize>, None::<String>, false, false);
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        let val = |i: &mut usize, flag: &str| -> Option<String> {
            if a.len() > flag.len() {
                Some(a[flag.len()..].to_string())
            } else {
                *i += 1;
                argv.get(*i).cloned()
            }
        };
        match a {
            "-0" | "--null" => nul = true,
            "-r" | "--no-run-if-empty" => no_empty = true,
            "-t" | "--verbose" => trace = true,
            "--" => {
                i += 1;
                break;
            }
            s if s.starts_with("-n") => max_args = val(&mut i, "-n").and_then(|v| v.parse().ok()),
            s if s.starts_with("-L") => max_args = val(&mut i, "-L").and_then(|v| v.parse().ok()),
            s if s.starts_with("-I") => replace = val(&mut i, "-I"),
            s if s.starts_with("-d") => delim = val(&mut i, "-d").and_then(|v| unescape_char(&v)),
            s if s.starts_with('-') && s.len() > 1 => {
                o.err(format!("xargs: invalid option -- '{}'", &s[1..]));
                return Ok(1);
            }
            _ => break,
        }
        i += 1;
    }
    let mut cmd: Vec<String> = argv[i.min(argv.len())..].to_vec();
    if cmd.is_empty() {
        cmd.push("echo".into());
    }
    let mut input = Vec::new();
    if let Err(e) = io.stdin.reader().read_to_end(&mut input) {
        if crate::io::interrupted() {
            return Ok(130);
        }
        o.err(format!("xargs: {}", crate::fmt::io_error(&e)));
        return Ok(1);
    }
    let text = String::from_utf8_lossy(&input);
    let items: Vec<String> = if nul {
        text.split('\0').filter(|s| !s.is_empty()).map(String::from).collect()
    } else if let Some(d) = delim {
        let mut v: Vec<String> = text.split(d).map(String::from).collect();
        if v.last().is_some_and(|s| s.is_empty()) {
            v.pop();
        }
        v
    } else if replace.is_some() {
        text.lines().map(|l| l.trim_start().to_string()).filter(|l| !l.is_empty()).collect()
    } else {
        match split_xargs(&text) {
            Ok(v) => v,
            Err(e) => {
                o.err(format!("xargs: {e}"));
                return Ok(1);
            }
        }
    };
    drop(o);
    if items.is_empty() && (no_empty || replace.is_some()) {
        return Ok(0);
    }
    let limit = if cfg!(windows) { 30_000 } else { 128 * 1024 };
    let mut batches: Vec<Vec<String>> = Vec::new();
    if let Some(r) = &replace {
        for it in &items {
            batches.push(cmd.iter().map(|a| a.replace(r.as_str(), it)).collect());
        }
    } else {
        let base: usize = cmd.iter().map(|a| a.len() + 1).sum();
        let mut cur = cmd.clone();
        let mut len = base;
        for it in items {
            let full = max_args.is_some_and(|n| cur.len() - cmd.len() >= n) || len + it.len() + 1 > limit;
            if full && cur.len() > cmd.len() {
                batches.push(std::mem::replace(&mut cur, cmd.clone()));
                len = base;
            }
            len += it.len() + 1;
            cur.push(it);
        }
        if cur.len() > cmd.len() || batches.is_empty() {
            batches.push(cur);
        }
    }
    let mut status = 0;
    for b in batches {
        if crate::io::interrupted() {
            return Ok(130);
        }
        if trace {
            let mut e = io.stderr.writer();
            let _ = writeln!(e, "{}", b.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "));
            let _ = e.flush();
        }
        let io = IoSet { stdin: crate::io::Input::Null, ..io.clone() };
        let st = flow_status(sh.clone().run_command(&b, &[], &io, Lookup { functions: false, builtins: false, utils: true }));
        match st {
            0 => {}
            127 | 126 => return Ok(st),
            255 => return Ok(124),
            _ => status = 123,
        }
    }
    Ok(status)
}

fn unescape_char(s: &str) -> Option<char> {
    match s {
        "\\n" => Some('\n'),
        "\\t" => Some('\t'),
        "\\0" => Some('\0'),
        _ => s.chars().next(),
    }
}

/// POSIX xargs input: blanks separate, quotes and backslashes group.
fn split_xargs(text: &str) -> Result<Vec<String>, String> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut it = text.chars();
    while let Some(c) = it.next() {
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if have {
                    items.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            '\'' | '"' => {
                have = true;
                loop {
                    match it.next() {
                        Some(q) if q == c => break,
                        Some('\n') | None => return Err(format!("unmatched {} quote", if c == '"' { "double" } else { "single" })),
                        Some(x) => cur.push(x),
                    }
                }
            }
            '\\' => {
                have = true;
                if let Some(n) = it.next() {
                    cur.push(n);
                }
            }
            c => {
                have = true;
                cur.push(c);
            }
        }
    }
    if have {
        items.push(cur);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_splitting() {
        assert_eq!(split_read("  a b   c d ", " \t\n", 3), ["a", "b", "c d"]);
        assert_eq!(split_read("a:b::c", ":", 4), ["a", "b", "", "c"]);
        assert_eq!(split_read("one", " ", 3), ["one"]);
    }

    #[test]
    fn xargs_splitting() {
        assert_eq!(split_xargs("a 'b c' \"d\"\\ e\n f").unwrap(), ["a", "b c", "d e", "f"]);
    }
}
