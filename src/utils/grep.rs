//! `grep`, on the `regex` crate. Basic regular expressions are translated
//! to extended syntax; back-references are not supported.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, BufReader};

use regex::bytes::{Regex, RegexBuilder};

use super::Ctx;
use super::args::{self, Spec};
use super::common::{fnmatch, msg, read_line};
use super::fs::join_disp;

/// Translate a POSIX basic regular expression into the crate's syntax.
fn bre_to_ere(p: &str) -> String {
    let chars: Vec<char> = p.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut at_start = true;
    while i < chars.len() {
        let c = chars[i];
        if c == '[' {
            // copy the bracket expression verbatim
            let st = i;
            i += 1;
            if i < chars.len() && chars[i] == '^' {
                i += 1;
            }
            if i < chars.len() && chars[i] == ']' {
                i += 1;
            }
            while i < chars.len() && chars[i] != ']' {
                if chars[i] == '[' && i + 1 < chars.len() && matches!(chars[i + 1], ':' | '.' | '=') {
                    let close = chars[i + 1];
                    i += 2;
                    while i + 1 < chars.len() && !(chars[i] == close && chars[i + 1] == ']') {
                        i += 1;
                    }
                    i += 2;
                    continue;
                }
                i += 1;
            }
            let end = (i + 1).min(chars.len());
            let body: String = chars[st..end].iter().collect();
            // a literal '[' inside a class must be escaped for the crate
            let inner: String = body[1..].replace("[", "\\[").replace("\\[:", "[:").replace("\\[.", "[.").replace("\\[=", "[=");
            out.push('[');
            out.push_str(&inner);
            i = end;
            at_start = false;
            continue;
        }
        if c == '\\' && i + 1 < chars.len() {
            let n = chars[i + 1];
            i += 2;
            match n {
                '(' | ')' | '{' | '}' | '|' | '+' | '?' => {
                    out.push(n);
                    at_start = n == '(' || n == '|';
                    continue;
                }
                '<' | '>' | 'b' | 'B' | 'w' | 'W' | 's' | 'S' => {
                    out.push('\\');
                    out.push(n);
                }
                _ if n.is_ascii_alphanumeric() => {
                    out.push('\\');
                    out.push(n);
                }
                _ => out.push_str(&regex::escape(&n.to_string())),
            }
            at_start = false;
            continue;
        }
        match c {
            '(' | ')' | '{' | '}' | '|' | '+' | '?' => {
                out.push('\\');
                out.push(c);
            }
            '*' if at_start => out.push_str("\\*"),
            '^' if at_start => {
                out.push('^');
                i += 1;
                continue;
            }
            _ => out.push(c),
        }
        at_start = false;
        i += 1;
    }
    out
}

struct Grep {
    re: Regex,
    invert: bool,
    line_numbers: bool,
    count: bool,
    files_with: bool,
    files_without: bool,
    only: bool,
    quiet: bool,
    no_messages: bool,
    with_name: bool,
    before: usize,
    after: usize,
    max: Option<u64>,
    color: bool,
    text: bool,
    null: bool,
    include: Vec<String>,
    exclude: Vec<String>,
    exclude_dir: Vec<String>,
    recursive: bool,
    follow: bool,
}

#[derive(Default)]
struct State {
    matched: bool,
    error: bool,
    stop: bool,
    last_was_context_group: bool,
}

const C_FILE: &str = "\x1b[35m";
const C_NUM: &str = "\x1b[32m";
const C_SEP: &str = "\x1b[36m";
const C_MATCH: &str = "\x1b[01;31m";
const C_END: &str = "\x1b[0m";

impl Grep {
    fn prefix(&self, out: &mut Vec<u8>, name: &str, lineno: u64, sep: u8) {
        if self.with_name {
            if self.color {
                out.extend_from_slice(format!("{C_FILE}{name}{C_END}{C_SEP}{}{C_END}", sep as char).as_bytes());
            } else {
                out.extend_from_slice(name.as_bytes());
                out.push(sep);
            }
        }
        if self.line_numbers {
            if self.color {
                out.extend_from_slice(format!("{C_NUM}{lineno}{C_END}{C_SEP}{}{C_END}", sep as char).as_bytes());
            } else {
                out.extend_from_slice(lineno.to_string().as_bytes());
                out.push(sep);
            }
        }
    }

    fn print_line(&self, out: &mut Vec<u8>, name: &str, lineno: u64, line: &[u8], selected: bool) {
        if self.only {
            if !selected || self.invert {
                return;
            }
            for m in self.re.find_iter(line) {
                if m.is_empty() {
                    continue;
                }
                self.prefix(out, name, lineno, b':');
                if self.color {
                    out.extend_from_slice(C_MATCH.as_bytes());
                    out.extend_from_slice(m.as_bytes());
                    out.extend_from_slice(C_END.as_bytes());
                } else {
                    out.extend_from_slice(m.as_bytes());
                }
                out.push(b'\n');
            }
            return;
        }
        self.prefix(out, name, lineno, if selected { b':' } else { b'-' });
        if self.color && selected && !self.invert {
            let mut last = 0;
            for m in self.re.find_iter(line) {
                if m.is_empty() {
                    continue;
                }
                out.extend_from_slice(&line[last..m.start()]);
                out.extend_from_slice(C_MATCH.as_bytes());
                out.extend_from_slice(m.as_bytes());
                out.extend_from_slice(C_END.as_bytes());
                last = m.end();
            }
            out.extend_from_slice(&line[last..]);
        } else {
            out.extend_from_slice(line);
        }
        out.push(b'\n');
    }

    fn search(&self, ctx: &mut Ctx, r: &mut dyn BufRead, name: &str, st: &mut State) -> io::Result<()> {
        let binary = !self.text && r.fill_buf()?.contains(&0);
        let mut count: u64 = 0;
        let mut lineno: u64 = 0;
        let mut before: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
        let mut after_left = 0usize;
        let mut last_printed: Option<u64> = None;
        let context = self.before > 0 || self.after > 0;
        let listing = self.count || self.files_with || self.files_without || self.quiet;
        let mut line = Vec::new();
        let mut out = Vec::new();
        while read_line(r, &mut line)? {
            ctx.check_interrupt()?;
            lineno += 1;
            if line.last() == Some(&b'\n') {
                line.pop();
                if line.last() == Some(&b'\r') && cfg!(windows) {
                    line.pop();
                }
            }
            let selected = self.re.is_match(&line) != self.invert;
            if selected {
                if self.max.is_some_and(|m| count >= m) {
                    break;
                }
                count += 1;
                st.matched = true;
                if self.quiet {
                    st.stop = true;
                    return Ok(());
                }
                if self.files_with || self.files_without {
                    break;
                }
                if listing {
                    continue;
                }
                if binary {
                    break;
                }
                let first = before.front().map(|b| b.0).unwrap_or(lineno);
                if context && (last_printed.is_some_and(|l| first > l + 1) || (last_printed.is_none() && st.last_was_context_group)) {
                    out.extend_from_slice(if self.color { format!("{C_SEP}--{C_END}\n").into_bytes() } else { b"--\n".to_vec() }.as_slice());
                }
                for (n, l) in before.drain(..) {
                    self.print_line(&mut out, name, n, &l, false);
                }
                self.print_line(&mut out, name, lineno, &line, true);
                last_printed = Some(lineno);
                after_left = self.after;
            } else if !listing && !binary {
                if after_left > 0 {
                    after_left -= 1;
                    self.print_line(&mut out, name, lineno, &line, false);
                    last_printed = Some(lineno);
                } else if self.before > 0 {
                    before.push_back((lineno, std::mem::take(&mut line)));
                    if before.len() > self.before {
                        before.pop_front();
                    }
                }
            }
            if out.len() > 32 * 1024 {
                ctx.stdout.write_all(&out)?;
                out.clear();
            }
        }
        if last_printed.is_some() {
            st.last_was_context_group = true;
        }
        let end = if self.null { b'\0' } else { b'\n' };
        if self.count {
            if self.with_name {
                out.extend_from_slice(name.as_bytes());
                out.push(b':');
            }
            out.extend_from_slice(format!("{count}\n").as_bytes());
        } else if (self.files_with && count > 0) || (self.files_without && count == 0) {
            out.extend_from_slice(name.as_bytes());
            out.push(end);
        } else if binary && count > 0 && !listing {
            out.extend_from_slice(format!("Binary file {name} matches\n").as_bytes());
        }
        ctx.stdout.write_all(&out)?;
        if self.files_without && count == 0 {
            // -L "selects" files without matches
            st.matched = true;
        }
        Ok(())
    }

    fn report(&self, ctx: &mut Ctx, st: &mut State, m: String) {
        st.error = true;
        if !self.no_messages {
            ctx.err(m);
        }
    }

    fn file(&self, ctx: &mut Ctx, disp: &str, top: bool, st: &mut State) -> io::Result<()> {
        if st.stop {
            return Ok(());
        }
        if disp == "-" {
            let empty: &'static mut io::Empty = Box::leak(Box::new(io::empty()));
            let stdin = std::mem::replace(&mut ctx.stdin, empty);
            let r = self.search(ctx, stdin, "(standard input)", st);
            ctx.stdin = stdin;
            return r;
        }
        let p = ctx.path(disp);
        let meta = if top || self.follow { fs::metadata(&p) } else { fs::symlink_metadata(&p) };
        let meta = match meta {
            Ok(m) => m,
            Err(e) => {
                self.report(ctx, st, format!("{disp}: {}", msg(&e)));
                return Ok(());
            }
        };
        if meta.file_type().is_symlink() {
            return Ok(());
        }
        let base = super::fs::base_name(disp);
        if meta.is_dir() {
            if !self.recursive {
                self.report(ctx, st, format!("{disp}: Is a directory"));
                return Ok(());
            }
            if !top && self.exclude_dir.iter().any(|g| fnmatch(g, &base, false)) {
                return Ok(());
            }
            let mut entries: Vec<fs::DirEntry> = match fs::read_dir(&p).and_then(|r| r.collect::<Result<_, _>>()) {
                Ok(v) => v,
                Err(e) => {
                    self.report(ctx, st, format!("{disp}: {}", msg(&e)));
                    return Ok(());
                }
            };
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name().to_string_lossy().into_owned();
                self.file(ctx, &join_disp(disp, &name), false, st)?;
                if st.stop {
                    break;
                }
            }
            return Ok(());
        }
        if !top || self.recursive {
            if !self.include.is_empty() && !self.include.iter().any(|g| fnmatch(g, &base, false)) {
                return Ok(());
            }
            if self.exclude.iter().any(|g| fnmatch(g, &base, false)) {
                return Ok(());
            }
        }
        match fs::File::open(&p) {
            Ok(f) => {
                let mut br = BufReader::with_capacity(64 * 1024, f);
                match self.search(ctx, &mut br, disp, st) {
                    Ok(()) => {}
                    Err(e) if matches!(e.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::Interrupted) || crate::io::interrupted() => return Err(e),
                    Err(e) => self.report(ctx, st, format!("{disp}: {}", msg(&e))),
                }
            }
            Err(e) => self.report(ctx, st, format!("{disp}: {}", msg(&e))),
        }
        Ok(())
    }
}

pub fn grep(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec {
        with_value: "efABCm",
        long_with_value: &["regexp", "file", "after-context", "before-context", "context", "max-count", "include", "exclude", "exclude-dir"],
        allow_number: true,
        stop_at_operand: false,
    };
    let o = match args::parse(args, &spec) {
        Ok(o) => o,
        Err(e) => {
            ctx.err(e);
            return Ok(2);
        }
    };
    let longs = [
        "regexp", "file", "after-context", "before-context", "context", "max-count", "include", "exclude", "exclude-dir", "ignore-case", "invert-match",
        "line-number", "count", "files-with-matches", "files-without-match", "recursive", "dereference-recursive", "extended-regexp", "fixed-strings",
        "basic-regexp", "word-regexp", "line-regexp", "only-matching", "quiet", "silent", "no-messages", "with-filename", "no-filename", "color", "colour",
        "text", "null",
    ];
    if let Some(e) = o.unknown("efABCmivnclLrREFGwxoqsHhaZy", &longs) {
        ctx.err(e);
        return Ok(2);
    }
    let mut operands = o.operands.clone();
    let mut patterns: Vec<String> = o.values('e').map(String::from).collect();
    patterns.extend(o.long_value("regexp").map(String::from));
    for f in o.values('f').chain(o.long_value("file")) {
        match fs::read_to_string(ctx.path(f)) {
            Ok(s) => patterns.extend(s.lines().map(String::from)),
            Err(e) => {
                ctx.err(format!("{f}: {}", msg(&e)));
                return Ok(2);
            }
        }
    }
    if patterns.is_empty() {
        if operands.is_empty() {
            ctx.err("usage: grep [OPTION]... PATTERNS [FILE]...");
            return Ok(2);
        }
        patterns.push(operands.remove(0));
    }
    // a pattern with newlines is several patterns
    let patterns: Vec<String> = patterns.iter().flat_map(|p| p.split('\n').map(String::from).collect::<Vec<_>>()).collect();
    let fixed = o.flag('F') || o.long("fixed-strings");
    let extended = o.flag('E') || o.long("extended-regexp");
    let alts: Vec<String> = patterns
        .iter()
        .map(|p| if fixed { regex::escape(p) } else if extended { p.clone() } else { bre_to_ere(p) })
        .map(|p| format!("(?:{p})"))
        .collect();
    let mut re_s = alts.join("|");
    if o.flag('x') || o.long("line-regexp") {
        re_s = format!("^(?:{re_s})$");
    } else if o.flag('w') || o.long("word-regexp") {
        re_s = format!(r"\b{{start-half}}(?:{re_s})\b{{end-half}}");
    }
    let re = match RegexBuilder::new(&re_s).case_insensitive(o.flag('i') || o.flag('y') || o.long("ignore-case")).build() {
        Ok(r) => r,
        Err(e) => {
            let m = e.to_string();
            ctx.err(m.lines().last().unwrap_or("invalid regular expression").trim().to_string());
            return Ok(2);
        }
    };
    let num = |c: char, l: &str| -> Option<usize> { o.value(c).or(o.long_value(l)).and_then(|v| v.parse().ok()) };
    let ctxn = num('C', "context").or(o.number.as_ref().and_then(|n| n.parse().ok())).unwrap_or(0);
    let recursive = o.flag('r') || o.flag('R') || o.long("recursive") || o.long("dereference-recursive");
    let implicit = operands.is_empty() && recursive;
    let color = match o.long_value("color").or(o.long_value("colour")) {
        Some("always") => true,
        Some("never") => false,
        Some(_) => ctx.stdout_tty,
        None => (o.long("color") || o.long("colour")) && ctx.stdout_tty,
    };
    let g = Grep {
        re,
        invert: o.flag('v') || o.long("invert-match"),
        line_numbers: o.flag('n') || o.long("line-number"),
        count: o.flag('c') || o.long("count"),
        files_with: o.flag('l') || o.long("files-with-matches"),
        files_without: o.flag('L') || o.long("files-without-match"),
        only: o.flag('o') || o.long("only-matching"),
        quiet: o.flag('q') || o.long("quiet") || o.long("silent"),
        no_messages: o.flag('s') || o.long("no-messages"),
        with_name: if o.flag('h') || o.long("no-filename") {
            false
        } else {
            o.flag('H') || o.long("with-filename") || operands.len() > 1 || recursive
        },
        before: num('B', "before-context").unwrap_or(ctxn),
        after: num('A', "after-context").unwrap_or(ctxn),
        max: o.value('m').or(o.long_value("max-count")).and_then(|v| v.parse().ok()),
        color,
        text: o.flag('a') || o.long("text"),
        null: o.flag('Z') || o.long("null"),
        include: collect_long(args, "include"),
        exclude: collect_long(args, "exclude"),
        exclude_dir: collect_long(args, "exclude-dir"),
        recursive,
        follow: o.flag('R') || o.long("dereference-recursive"),
    };
    let mut st = State::default();
    if implicit {
        // walk "." but show bare names, as GNU grep does
        let mut entries: Vec<fs::DirEntry> = fs::read_dir(ctx.cwd).and_then(|r| r.collect::<Result<_, _>>()).unwrap_or_default();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            g.file(ctx, &name, false, &mut st)?;
            if st.stop {
                break;
            }
        }
    } else {
        let files = if operands.is_empty() { vec!["-".to_string()] } else { operands };
        for f in &files {
            g.file(ctx, f, true, &mut st)?;
            if st.stop {
                break;
            }
        }
    }
    Ok(if st.matched && (g.quiet || !st.error) {
        0
    } else if st.error {
        2
    } else {
        1
    })
}

/// All values of a repeatable long option (`--include=a --include b`).
fn collect_long(args: &[String], name: &str) -> Vec<String> {
    let mut v = Vec::new();
    let flag = format!("--{name}");
    let eq = format!("--{name}=");
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            break;
        }
        if let Some(x) = args[i].strip_prefix(&eq) {
            v.push(x.to_string());
        } else if args[i] == flag && i + 1 < args.len() {
            v.push(args[i + 1].clone());
            i += 1;
        }
        i += 1;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::bre_to_ere;
    use crate::utils::common::testutil::TempDir;
    use crate::utils::run_capture;
    use std::path::Path;

    fn g(args: &[&str], input: &str) -> (i32, String) {
        let (s, o, _) = run_capture("grep", args, input.as_bytes(), Path::new("."));
        (s, o)
    }

    #[test]
    fn bre() {
        assert_eq!(bre_to_ere(r"a\(b\)c"), "a(b)c");
        assert_eq!(bre_to_ere(r"foo\|bar"), "foo|bar");
        assert_eq!(bre_to_ere("a+b?"), r"a\+b\?");
        assert_eq!(bre_to_ere("*x"), r"\*x");
        assert_eq!(bre_to_ere("[[:digit:]]+"), r"[[:digit:]]\+");
        assert_eq!(bre_to_ere("a{2}"), r"a\{2\}");
        assert_eq!(bre_to_ere(r"a\{2\}"), "a{2}");
    }

    #[test]
    fn basic() {
        let input = "apple\nbanana\ncherry\nApple pie\n";
        assert_eq!(g(&["an"], input), (0, "banana\n".into()));
        assert_eq!(g(&["zzz"], input), (1, "".into()));
        assert_eq!(g(&["-i", "apple"], input).1, "apple\nApple pie\n");
        assert_eq!(g(&["-v", "e"], input).1, "banana\n");
        assert_eq!(g(&["-c", "a"], input).1, "2\n");
        assert_eq!(g(&["-n", "cherry"], input).1, "3:cherry\n");
        assert_eq!(g(&["-o", "an"], input).1, "an\nan\n");
        assert_eq!(g(&["-x", "apple"], input).1, "apple\n");
        assert_eq!(g(&["-w", "pie"], input).1, "Apple pie\n");
        assert_eq!(g(&["-w", "pi"], input).0, 1);
        assert_eq!(g(&["-E", "^(apple|cherry)$"], input).1, "apple\ncherry\n");
        assert_eq!(g(&["^\\(apple\\|cherry\\)$"], input).1, "apple\ncherry\n");
        assert_eq!(g(&["-F", "a.p"], "a.p\naxp\n").1, "a.p\n");
        assert_eq!(g(&["a.p"], "a.p\naxp\n").1, "a.p\naxp\n");
        assert_eq!(g(&["-e", "apple", "-e", "cherry"], input).1, "apple\ncherry\n");
        assert_eq!(g(&["-q", "banana"], input), (0, "".into()));
        assert_eq!(g(&["-m", "1", "a"], input).1, "apple\n");
        assert_eq!(g(&["("], "(x\n").1, "(x\n");
        assert_eq!(g(&["-E", "("], "x\n").0, 2);
        assert_eq!(g(&["--color=always", "an"], "banana\n").1, "b\x1b[01;31man\x1b[0m\x1b[01;31man\x1b[0ma\n");
    }

    #[test]
    fn context() {
        let input = "1\n2\nx\n4\n5\n6\n7\nx\n9\n";
        assert_eq!(g(&["-A", "1", "x"], input).1, "x\n4\n--\nx\n9\n");
        assert_eq!(g(&["-B", "1", "x"], input).1, "2\nx\n--\n7\nx\n");
        assert_eq!(g(&["-C", "1", "-n", "x"], input).1, "2-2\n3:x\n4-4\n--\n7-7\n8:x\n9-9\n");
        assert_eq!(g(&["-C", "3", "x"], input).1, "1\n2\nx\n4\n5\n6\n7\nx\n9\n");
    }

    #[test]
    fn files_and_recursion() {
        let d = TempDir::new("grep");
        d.write("a.txt", "hello\nworld\n");
        d.write("b.rs", "hello rust\n");
        d.write("sub/c.txt", "nothing\nhello again\n");
        d.write("bin.dat", "hel\0lo hello\n");
        let r = |a: &[&str]| run_capture("grep", a, b"", d.path());
        assert_eq!(r(&["hello", "a.txt", "b.rs"]).1, "a.txt:hello\nb.rs:hello rust\n");
        assert_eq!(r(&["-h", "hello", "a.txt", "b.rs"]).1, "hello\nhello rust\n");
        assert_eq!(r(&["-H", "world", "a.txt"]).1, "a.txt:world\n");
        assert_eq!(r(&["-l", "hello", "a.txt", "b.rs", "sub/c.txt"]).1, "a.txt\nb.rs\nsub/c.txt\n");
        assert_eq!(r(&["-L", "world", "a.txt", "b.rs"]).1, "b.rs\n");
        assert_eq!(r(&["-c", "hello", "a.txt", "b.rs"]).1, "a.txt:1\nb.rs:1\n");
        assert_eq!(r(&["-r", "hello", "."]).1, "./a.txt:hello\n./b.rs:hello rust\nBinary file ./bin.dat matches\n./sub/c.txt:hello again\n");
        assert_eq!(r(&["-r", "again"]).1, "sub/c.txt:hello again\n");
        assert_eq!(r(&["-r", "--include=*.txt", "hello"]).1, "a.txt:hello\nsub/c.txt:hello again\n");
        assert_eq!(r(&["-r", "--exclude-dir=sub", "--exclude", "*.dat", "hello"]).1, "a.txt:hello\nb.rs:hello rust\n");
        assert_eq!(r(&["-a", "lo hello", "bin.dat"]).1, "hel\0lo hello\n");
        let (s, _, e) = r(&["hello", "sub"]);
        assert_eq!((s, e.as_str()), (2, "grep: sub: Is a directory\n"));
        let (s, _, e) = r(&["-s", "hello", "nope", "a.txt"]);
        assert_eq!((s, e.as_str()), (2, ""));
        assert_eq!(r(&["-q", "hello", "nope", "a.txt"]).0, 0);
        assert_eq!(r(&["-rn", "again", "sub"]).1, "sub/c.txt:2:hello again\n");
    }
}
