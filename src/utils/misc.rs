//! Small utilities: echo, seq, yes, sleep, env, system info, date...

use std::io;
use std::time::{Duration, Instant};

use super::Ctx;
use super::args::{self, Spec};
use super::common::{display, local_tz, strftime, to_zoned, unescape, usage};
use super::printf::{Spec as FSpec, fmt_float};

pub fn true_(_ctx: &mut Ctx, _args: &[String]) -> io::Result<i32> {
    Ok(0)
}

pub fn false_(_ctx: &mut Ctx, _args: &[String]) -> io::Result<i32> {
    Ok(1)
}

/// zsh `echo`: escapes are interpreted unless `-E`.
pub fn echo(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let mut newline = true;
    let mut escapes = true;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-" {
            i += 1;
            break;
        }
        if a.len() < 2 || !a.starts_with('-') || !a[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E')) {
            break;
        }
        for c in a[1..].chars() {
            match c {
                'n' => newline = false,
                'e' => escapes = true,
                _ => escapes = false,
            }
        }
        i += 1;
    }
    let mut out = Vec::new();
    for (k, a) in args[i..].iter().enumerate() {
        if k > 0 {
            out.push(b' ');
        }
        if escapes {
            let (b, stop) = unescape(a, false);
            out.extend_from_slice(&b);
            if stop {
                ctx.stdout.write_all(&out)?;
                return Ok(0);
            }
        } else {
            out.extend_from_slice(a.as_bytes());
        }
    }
    if newline {
        out.push(b'\n');
    }
    ctx.stdout.write_all(&out)?;
    Ok(0)
}

pub fn yes(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let line = if args.is_empty() { "y".to_string() } else { args.join(" ") };
    let mut chunk = Vec::with_capacity(16 * 1024);
    while chunk.len() < 8 * 1024 {
        chunk.extend_from_slice(line.as_bytes());
        chunk.push(b'\n');
    }
    loop {
        ctx.check_interrupt()?;
        ctx.stdout.write_all(&chunk)?;
        ctx.stdout.flush()?;
    }
}

fn parse_duration(s: &str) -> Option<f64> {
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1.0),
        'm' => (&s[..s.len() - 1], 60.0),
        'h' => (&s[..s.len() - 1], 3600.0),
        'd' => (&s[..s.len() - 1], 86400.0),
        _ => (s, 1.0),
    };
    let v: f64 = num.parse().ok()?;
    if v.is_nan() || v < 0.0 { None } else { Some(v * mult) }
}

pub fn sleep(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    if args.is_empty() {
        return usage(ctx, "missing operand", 1);
    }
    let mut total = 0.0;
    for a in args {
        match parse_duration(a) {
            Some(v) => total += v,
            None => return usage(ctx, format!("invalid time interval '{a}'"), 1),
        }
    }
    let end = Instant::now() + Duration::from_secs_f64(total.min(1e9));
    loop {
        ctx.check_interrupt()?;
        let now = Instant::now();
        if now >= end {
            return Ok(0);
        }
        std::thread::sleep((end - now).min(Duration::from_millis(50)));
    }
}

/// Number of decimals written in a seq operand.
fn decimals(s: &str) -> usize {
    match s.find('.') {
        Some(i) => s[i + 1..].chars().take_while(|c| c.is_ascii_digit()).count(),
        None => 0,
    }
}

pub fn seq(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    // Negative numbers are operands, not options.
    let mut sep = "\n".to_string();
    let mut width = false;
    let mut format: Option<String> = None;
    let mut ops = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        let is_num = a.parse::<f64>().is_ok();
        match a.as_str() {
            "-w" | "--equal-width" if !is_num => width = true,
            "-s" | "-f" if i < args.len() => {
                if a == "-s" {
                    sep = args[i].clone();
                } else {
                    format = Some(args[i].clone());
                }
                i += 1;
            }
            "--" => ops.extend(args[i..].iter().cloned()),
            _ if a.starts_with("--separator=") => sep = a["--separator=".len()..].to_string(),
            _ if a.starts_with("--format=") => format = Some(a["--format=".len()..].to_string()),
            _ if a.starts_with("-s") && a.len() > 2 && !is_num => sep = a[2..].to_string(),
            _ if a.starts_with("-f") && a.len() > 2 && !is_num => format = Some(a[2..].to_string()),
            _ if a.starts_with('-') && a.len() > 1 && !is_num => {
                return usage(ctx, format!("invalid option -- '{}'", &a[1..]), 1);
            }
            _ => ops.push(a.clone()),
        }
        if a == "--" {
            break;
        }
    }
    let (first, incr, last) = match ops.as_slice() {
        [l] => ("1".to_string(), "1".to_string(), l.clone()),
        [f, l] => (f.clone(), "1".to_string(), l.clone()),
        [f, i, l] => (f.clone(), i.clone(), l.clone()),
        [] => return usage(ctx, "missing operand", 1),
        _ => return usage(ctx, format!("extra operand '{}'", ops[3]), 1),
    };
    let mut nums = [0.0f64; 3];
    for (k, s) in [&first, &incr, &last].iter().enumerate() {
        match s.parse::<f64>() {
            Ok(v) if !v.is_nan() => nums[k] = v,
            _ => return usage(ctx, format!("invalid floating point argument: '{s}'"), 1),
        }
    }
    let [f, inc, l] = nums;
    if inc == 0.0 {
        return usage(ctx, format!("invalid Zero increment value: '{incr}'"), 1);
    }
    let prec = decimals(&first).max(decimals(&incr));
    let render = |v: f64| -> String {
        if let Some(fm) = &format {
            render_format(fm, v)
        } else {
            format!("{:.*}", prec, v)
        }
    };
    let w = if width && format.is_none() { render(f).len().max(render(l).len()) } else { 0 };
    let mut n: u64 = 0;
    let mut first_out = true;
    let mut out = Vec::new();
    loop {
        let v = f + inc * n as f64;
        if (inc > 0.0 && v > l + 1e-10 * inc.abs()) || (inc < 0.0 && v < l - 1e-10 * inc.abs()) {
            break;
        }
        if !first_out {
            out.extend_from_slice(sep.as_bytes());
        }
        first_out = false;
        let mut s = render(v);
        if w > 0 && s.len() < w {
            let neg = s.starts_with('-');
            let digits = if neg { &s[1..] } else { &s[..] };
            s = format!("{}{}{}", if neg { "-" } else { "" }, "0".repeat(w - s.len()), digits);
        }
        out.extend_from_slice(s.as_bytes());
        n += 1;
        if out.len() > 32 * 1024 {
            ctx.check_interrupt()?;
            ctx.stdout.write_all(&out)?;
            out.clear();
        }
    }
    if !first_out {
        out.push(b'\n');
    }
    ctx.stdout.write_all(&out)?;
    Ok(0)
}

/// `seq -f`: one floating-point conversion inside literal text.
fn render_format(fm: &str, v: f64) -> String {
    let Some(start) = fm.find('%') else { return fm.to_string() };
    let rest = &fm[start + 1..];
    let mut spec = FSpec::default();
    let mut chars = rest.char_indices().peekable();
    while let Some(&(_, c)) = chars.peek() {
        match c {
            '-' => spec.left = true,
            '+' => spec.plus = true,
            ' ' => spec.space = true,
            '#' => spec.alt = true,
            '0' => spec.zero = true,
            _ => break,
        }
        chars.next();
    }
    let mut num = String::new();
    while let Some(&(_, c)) = chars.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        num.push(c);
        chars.next();
    }
    spec.width = num.parse().ok();
    if let Some(&(_, '.')) = chars.peek() {
        chars.next();
        let mut p = String::new();
        while let Some(&(_, c)) = chars.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            p.push(c);
            chars.next();
        }
        spec.prec = Some(p.parse().unwrap_or(0));
    }
    let Some((idx, conv)) = chars.next() else { return fm.to_string() };
    let tail = &rest[idx + conv.len_utf8()..];
    format!("{}{}{}", &fm[..start], fmt_float(v, conv, &spec), tail)
}

pub fn env(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "u", long_with_value: &["unset"], allow_number: false, stop_at_operand: true };
    let o = match args::parse(args, &spec) {
        Ok(o) => o,
        Err(e) => return usage(ctx, e, 125),
    };
    if let Some(e) = o.unknown("0iu", &["null", "ignore-environment", "unset"]) {
        return usage(ctx, e, 125);
    }
    let mut vars: Vec<(String, String)> = if o.flag('i') || o.long("ignore-environment") { Vec::new() } else { ctx.env.to_vec() };
    let unset: Vec<&str> = o.values('u').chain(o.long_value("unset")).collect();
    vars.retain(|(k, _)| !unset.iter().any(|u| super::env_key_eq(k, u)));
    for op in &o.operands {
        match op.split_once('=') {
            Some((k, v)) if !k.is_empty() => {
                vars.retain(|(x, _)| !super::env_key_eq(x, k));
                vars.push((k.to_string(), v.to_string()));
            }
            _ => return usage(ctx, "cannot run commands", 125),
        }
    }
    let end = if o.flag('0') || o.long("null") { b'\0' } else { b'\n' };
    for (k, v) in &vars {
        write!(ctx.stdout, "{k}={v}")?;
        ctx.stdout.write_all(&[end])?;
    }
    Ok(0)
}

pub fn printenv(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = match args::parse(args, &args::SIMPLE) {
        Ok(o) => o,
        Err(e) => return usage(ctx, e, 2),
    };
    if let Some(e) = o.unknown("0", &["null"]) {
        return usage(ctx, e, 2);
    }
    let end = if o.flag('0') || o.long("null") { b'\0' } else { b'\n' };
    if o.operands.is_empty() {
        for (k, v) in ctx.env {
            write!(ctx.stdout, "{k}={v}")?;
            ctx.stdout.write_all(&[end])?;
        }
        return Ok(0);
    }
    let mut status = 0;
    for name in &o.operands {
        match ctx.getenv(name).map(|s| s.to_string()) {
            Some(v) => {
                ctx.stdout.write_all(v.as_bytes())?;
                ctx.stdout.write_all(&[end])?;
            }
            None => status = 1,
        }
    }
    Ok(status)
}

pub fn pwd(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = match args::parse(args, &args::SIMPLE) {
        Ok(o) => o,
        Err(e) => return usage(ctx, e, 2),
    };
    if let Some(e) = o.unknown("LP", &["logical", "physical"]) {
        return usage(ctx, e, 2);
    }
    let physical = o.flag('P') || o.long("physical");
    let s = if physical {
        match std::fs::canonicalize(ctx.cwd) {
            Ok(p) => display(&p),
            Err(e) => return usage(ctx, super::common::msg(&e), 1),
        }
    } else {
        ctx.cwd.display().to_string()
    };
    writeln!(ctx.stdout, "{s}")?;
    Ok(0)
}

#[cfg(unix)]
fn user_name() -> Option<String> {
    super::fs::user_name(unsafe { libc::geteuid() })
}

pub fn whoami(ctx: &mut Ctx, _args: &[String]) -> io::Result<i32> {
    #[cfg(unix)]
    let name = user_name().or_else(|| ctx.getenv("USER").map(String::from));
    #[cfg(not(unix))]
    let name = ctx.getenv("USERNAME").map(String::from);
    match name {
        Some(n) => {
            writeln!(ctx.stdout, "{n}")?;
            Ok(0)
        }
        None => usage(ctx, "cannot find name for user ID", 1),
    }
}

#[cfg(unix)]
struct Uname {
    sysname: String,
    nodename: String,
    release: String,
    version: String,
    machine: String,
}

#[cfg(unix)]
fn uname_info() -> Option<Uname> {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return None;
    }
    fn s(f: &[libc::c_char]) -> String {
        let bytes: Vec<u8> = f.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
    Some(Uname {
        sysname: s(&u.sysname),
        nodename: s(&u.nodename),
        release: s(&u.release),
        version: s(&u.version),
        machine: s(&u.machine),
    })
}

fn host_name(ctx: &Ctx) -> String {
    #[cfg(unix)]
    {
        let _ = ctx;
        uname_info().map(|u| u.nodename).unwrap_or_else(|| "localhost".into())
    }
    #[cfg(not(unix))]
    {
        ctx.getenv("COMPUTERNAME").unwrap_or("localhost").to_string()
    }
}

pub fn hostname(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let mut h = host_name(ctx);
    if args.iter().any(|a| a == "-s" || a == "--short") {
        if let Some(i) = h.find('.') {
            h.truncate(i);
        }
    } else if let Some(a) = args.first() {
        return usage(ctx, format!("cannot set hostname to '{a}'"), 1);
    }
    writeln!(ctx.stdout, "{h}")?;
    Ok(0)
}

pub fn uname(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = match args::parse(args, &args::SIMPLE) {
        Ok(o) => o,
        Err(e) => return usage(ctx, e, 1),
    };
    let longs = ["all", "kernel-name", "nodename", "kernel-release", "kernel-version", "machine", "operating-system", "processor"];
    if let Some(e) = o.unknown("asnrvmop", &longs) {
        return usage(ctx, e, 1);
    }
    if let Some(x) = o.operands.first() {
        return usage(ctx, format!("extra operand '{x}'"), 1);
    }
    #[cfg(unix)]
    let (sys, node, rel, ver, mach) = match uname_info() {
        Some(u) => (u.sysname, u.nodename, u.release, u.version, u.machine),
        None => return usage(ctx, "cannot get system name", 1),
    };
    #[cfg(not(unix))]
    let (sys, node, rel, ver, mach) = (
        "Windows_NT".to_string(),
        host_name(ctx),
        "unknown".to_string(),
        "unknown".to_string(),
        std::env::consts::ARCH.to_string(),
    );
    let os = match std::env::consts::OS {
        "linux" => "GNU/Linux",
        "macos" => "Darwin",
        "windows" => "Windows",
        o => o,
    }
    .to_string();
    let all = o.flag('a') || o.long("all");
    let want = |c: char, l: &str| all || o.flag(c) || o.long(l);
    let mut parts = Vec::new();
    let any = ['s', 'n', 'r', 'v', 'm', 'o', 'p'].iter().any(|&c| o.flag(c)) || longs[1..].iter().any(|l| o.long(l));
    if want('s', "kernel-name") || !any && !all {
        parts.push(sys);
    }
    if want('n', "nodename") {
        parts.push(node);
    }
    if want('r', "kernel-release") {
        parts.push(rel);
    }
    if want('v', "kernel-version") {
        parts.push(ver);
    }
    if want('m', "machine") {
        parts.push(mach.clone());
    }
    if (o.flag('p') || o.long("processor")) && !all {
        parts.push(mach);
    }
    if want('o', "operating-system") {
        parts.push(os);
    }
    writeln!(ctx.stdout, "{}", parts.join(" "))?;
    Ok(0)
}

pub fn nproc(ctx: &mut Ctx, _args: &[String]) -> io::Result<i32> {
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    writeln!(ctx.stdout, "{n}")?;
    Ok(0)
}

pub fn clear(ctx: &mut Ctx, _args: &[String]) -> io::Result<i32> {
    ctx.stdout.write_all(b"\x1b[H\x1b[2J\x1b[3J")?;
    Ok(0)
}

/// Parse `date -d` input.
pub(super) fn parse_date(s: &str, tz: &jiff::tz::TimeZone) -> Option<jiff::Zoned> {
    let s = s.trim();
    let now = jiff::Zoned::now().with_time_zone(tz.clone());
    let day = |d: i64| -> Option<jiff::Zoned> {
        let date = now.date().checked_add(jiff::Span::new().days(d)).ok()?;
        date.to_zoned(tz.clone()).ok()
    };
    match s {
        "" | "now" => return Some(now),
        "today" => return day(0),
        "yesterday" => return day(-1),
        "tomorrow" => return day(1),
        _ => {}
    }
    if let Some(e) = s.strip_prefix('@') {
        let secs: i64 = e.parse().ok()?;
        return jiff::Timestamp::from_second(secs).ok().map(|t| t.to_zoned(tz.clone()));
    }
    if let Ok(z) = s.parse::<jiff::Zoned>() {
        return Some(z);
    }
    if let Ok(t) = s.parse::<jiff::Timestamp>() {
        return Some(t.to_zoned(tz.clone()));
    }
    if let Ok(dt) = s.parse::<jiff::civil::DateTime>() {
        return dt.to_zoned(tz.clone()).ok();
    }
    if let Ok(d) = s.parse::<jiff::civil::Date>() {
        return d.to_zoned(tz.clone()).ok();
    }
    None
}

pub fn date(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "dr", long_with_value: &["date", "reference"], allow_number: false, stop_at_operand: false };
    // `-I` takes an optional attached value; pre-split it out.
    let mut iso: Option<String> = None;
    let mut rest = Vec::new();
    for a in args {
        if let Some(v) = a.strip_prefix("-I") {
            iso = Some(if v.is_empty() { "date".into() } else { v.to_string() });
        } else if let Some(v) = a.strip_prefix("--iso-8601") {
            iso = Some(v.strip_prefix('=').filter(|v| !v.is_empty()).unwrap_or("date").to_string());
        } else {
            rest.push(a.clone());
        }
    }
    let o = match args::parse(&rest, &spec) {
        Ok(o) => o,
        Err(e) => return usage(ctx, e, 1),
    };
    if let Some(e) = o.unknown("udrR", &["utc", "universal", "date", "reference", "rfc-email"]) {
        return usage(ctx, e, 1);
    }
    let tz = if o.flag('u') || o.long("utc") || o.long("universal") { jiff::tz::TimeZone::UTC } else { local_tz(ctx) };
    let when = if let Some(d) = o.value('d').or(o.long_value("date")) {
        match parse_date(d, &tz) {
            Some(z) => z.with_time_zone(tz.clone()),
            None => return usage(ctx, format!("invalid date '{d}'"), 1),
        }
    } else if let Some(r) = o.value('r').or(o.long_value("reference")) {
        let p = ctx.path(r);
        match std::fs::metadata(&p).and_then(|m| m.modified()) {
            Ok(t) => match to_zoned(t, &tz) {
                Some(z) => z,
                None => return usage(ctx, format!("{r}: invalid time"), 1),
            },
            Err(e) => return usage(ctx, format!("{r}: {}", super::common::msg(&e)), 1),
        }
    } else {
        jiff::Zoned::now().with_time_zone(tz)
    };
    let mut fmt = "%a %b %e %H:%M:%S %Z %Y".to_string();
    if let Some(i) = iso {
        fmt = match i.as_str() {
            "date" => "%Y-%m-%d",
            "hours" => "%Y-%m-%dT%H%:z",
            "minutes" => "%Y-%m-%dT%H:%M%:z",
            "seconds" => "%Y-%m-%dT%H:%M:%S%:z",
            "ns" => "%Y-%m-%dT%H:%M:%S,%N%:z",
            other => return usage(ctx, format!("invalid argument '{other}' for '--iso-8601'"), 1),
        }
        .into();
    }
    if o.flag('R') || o.long("rfc-email") {
        fmt = "%a, %d %b %Y %H:%M:%S %z".into();
    }
    for op in &o.operands {
        match op.strip_prefix('+') {
            Some(f) => fmt = f.to_string(),
            None => return usage(ctx, format!("invalid date '{op}'"), 1),
        }
    }
    writeln!(ctx.stdout, "{}", strftime(&when, &fmt))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use crate::utils::run_capture;
    use std::path::Path;

    fn r(name: &str, args: &[&str]) -> (i32, String, String) {
        run_capture(name, args, b"", Path::new("."))
    }

    #[test]
    fn echo_modes() {
        assert_eq!(r("echo", &["a", "b"]).1, "a b\n");
        assert_eq!(r("echo", &["-n", "a"]).1, "a");
        assert_eq!(r("echo", &["a\\tb"]).1, "a\tb\n");
        assert_eq!(r("echo", &["-E", "a\\tb"]).1, "a\\tb\n");
        assert_eq!(r("echo", &["x\\cy", "z"]).1, "x");
        assert_eq!(r("echo", &["-x"]).1, "-x\n");
        assert_eq!(r("echo", &["-", "-n"]).1, "-n\n");
        assert_eq!(r("echo", &[]).1, "\n");
    }

    #[test]
    fn seq_forms() {
        assert_eq!(r("seq", &["3"]).1, "1\n2\n3\n");
        assert_eq!(r("seq", &["2", "4"]).1, "2\n3\n4\n");
        assert_eq!(r("seq", &["10", "-3", "1"]).1, "10\n7\n4\n1\n");
        assert_eq!(r("seq", &["-s", ",", "3"]).1, "1,2,3\n");
        assert_eq!(r("seq", &["-w", "8", "10"]).1, "08\n09\n10\n");
        assert_eq!(r("seq", &["0", "0.5", "1.5"]).1, "0.0\n0.5\n1.0\n1.5\n");
        assert_eq!(r("seq", &["-f", "n%03g", "2"]).1, "n001\nn002\n");
        assert_eq!(r("seq", &["-1", "1"]).1, "-1\n0\n1\n");
        assert_eq!(r("seq", &["5", "1"]).1, "");
        assert_eq!(r("seq", &["1", "0", "2"]).0, 1);
    }

    #[test]
    fn sleep_and_truth() {
        assert_eq!(r("sleep", &["0.01"]).0, 0);
        assert_eq!(r("sleep", &["x"]).0, 1);
        assert_eq!(r("sleep", &[]).0, 1);
        assert_eq!(r("true", &[]).0, 0);
        assert_eq!(r("false", &[]).0, 1);
    }

    #[test]
    fn env_printing() {
        assert_eq!(r("env", &[]).1, "HOME=/home/test\n");
        assert_eq!(r("env", &["-u", "HOME", "A=1"]).1, "A=1\n");
        assert_eq!(r("env", &["-i"]).1, "");
        assert_eq!(r("env", &["ls"]).0, 125);
        assert_eq!(r("printenv", &["HOME"]).1, "/home/test\n");
        assert_eq!(r("printenv", &["NOPE"]).0, 1);
    }

    #[test]
    fn info() {
        let d = std::env::temp_dir();
        let (s, out, _) = run_capture("pwd", &[], b"", &d);
        assert_eq!((s, out), (0, format!("{}\n", d.display())));
        assert_eq!(r("pwd", &["-P"]).0, 0);
        assert!(!r("uname", &[]).1.trim().is_empty());
        assert!(r("uname", &["-a"]).1.split(' ').count() >= 4);
        assert!(r("nproc", &[]).1.trim().parse::<usize>().unwrap() >= 1);
        assert_eq!(r("hostname", &[]).0, 0);
        assert_eq!(r("clear", &[]).1, "\x1b[H\x1b[2J\x1b[3J");
    }

    #[test]
    fn dates() {
        assert_eq!(r("date", &["-u", "-d", "@0", "+%Y-%m-%d %H:%M:%S"]).1, "1970-01-01 00:00:00\n");
        assert_eq!(r("date", &["-u", "-d", "2026-11-13T14:00:00Z", "-Iseconds"]).1, "2026-11-13T14:00:00+00:00\n");
        assert_eq!(r("date", &["-u", "-d", "2026-11-13", "-I"]).1, "2026-11-13\n");
        assert_eq!(r("date", &["-u", "-d", "@86400"]).1, "Fri Jan  2 00:00:00 UTC 1970\n");
        assert_eq!(r("date", &["-u", "-R", "-d", "@0"]).1, "Thu, 01 Jan 1970 00:00:00 +0000\n");
        assert_eq!(r("date", &["-d", "garbage"]).0, 1);
        assert_eq!(r("date", &["-u", "-d", "@0", "+%s"]).1, "0\n");
    }
}
