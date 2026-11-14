//! Helpers shared by the utilities.

use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind};
use std::path::Path;
use std::time::SystemTime;

use super::Ctx;

/// An io::Error as a GNU-style message, without Rust's " (os error N)" tail
/// and with the same wording on every platform.
pub fn msg(e: &io::Error) -> String {
    match e.kind() {
        ErrorKind::NotFound => return "No such file or directory".into(),
        ErrorKind::PermissionDenied => return "Permission denied".into(),
        ErrorKind::AlreadyExists => return "File exists".into(),
        ErrorKind::IsADirectory => return "Is a directory".into(),
        ErrorKind::NotADirectory => return "Not a directory".into(),
        ErrorKind::DirectoryNotEmpty => return "Directory not empty".into(),
        ErrorKind::CrossesDevices => return "Invalid cross-device link".into(),
        _ => {}
    }
    let s = e.to_string();
    match s.find(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// `'name'` quoting used in GNU messages.
pub fn q(s: &str) -> String {
    format!("'{s}'")
}

/// A path for display: drops the `\\?\` prefix `canonicalize` adds on Windows.
pub fn display(p: &Path) -> String {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => rest.to_string(),
        _ => s.into_owned(),
    }
}

fn is_fatal(e: &io::Error) -> bool {
    matches!(e.kind(), ErrorKind::BrokenPipe | ErrorKind::Interrupted) || crate::io::interrupted()
}

/// Run `f` over the named input (`-` is stdin). Open and read errors are
/// reported as `name: file: error` and give `Ok(false)`; broken pipes and
/// interrupts propagate.
pub fn with_input(
    ctx: &mut Ctx,
    file: &str,
    f: impl FnOnce(&mut Ctx, &mut dyn BufRead) -> io::Result<()>,
) -> io::Result<bool> {
    let r = if file == "-" {
        // Lend ctx.stdin to `f` while `f` also gets the context.
        let empty: &'static mut io::Empty = Box::leak(Box::new(io::empty()));
        let stdin = std::mem::replace(&mut ctx.stdin, empty);
        let r = f(ctx, stdin);
        ctx.stdin = stdin;
        r
    } else {
        match open_file(ctx, file) {
            Ok(file_r) => {
                let mut br = BufReader::with_capacity(64 * 1024, file_r);
                f(ctx, &mut br)
            }
            Err(e) => Err(e),
        }
    };
    match r {
        Ok(()) => Ok(true),
        Err(e) if is_fatal(&e) => Err(e),
        Err(e) => {
            ctx.err(format!("{}: {}", file, msg(&e)));
            Ok(false)
        }
    }
}

/// Open a file for reading, refusing directories with a clear error.
pub fn open_file(ctx: &Ctx, file: &str) -> io::Result<File> {
    let p = ctx.path(file);
    if p.is_dir() {
        return Err(io::Error::new(ErrorKind::IsADirectory, "Is a directory"));
    }
    File::open(p)
}

/// Read one line including its `\n`. Returns false at EOF.
pub fn read_line(r: &mut dyn BufRead, buf: &mut Vec<u8>) -> io::Result<bool> {
    buf.clear();
    Ok(r.read_until(b'\n', buf)? > 0)
}

/// Read all lines (without terminators) from an input.
pub fn read_lines(r: &mut dyn BufRead) -> io::Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    while read_line(r, &mut buf)? {
        crate::io::check_interrupt()?;
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        out.push(std::mem::take(&mut buf));
    }
    Ok(out)
}

/// `ls -h` / `du -h` sizes: 1024-based, one decimal below 10, rounded up.
pub fn human(n: u64) -> String {
    if n < 1024 {
        return n.to_string();
    }
    let units = ['K', 'M', 'G', 'T', 'P', 'E'];
    let mut v = n as f64;
    let mut u = 0;
    loop {
        v /= 1024.0;
        if v < 1024.0 || u == units.len() - 1 {
            break;
        }
        u += 1;
    }
    if v < 10.0 {
        let r = (v * 10.0).ceil() / 10.0;
        if r >= 10.0 { format!("{}{}", r as u64, units[u]) } else { format!("{:.1}{}", r, units[u]) }
    } else {
        let r = v.ceil();
        if r >= 1024.0 && u + 1 < units.len() { format!("1.0{}", units[u + 1]) } else { format!("{}{}", r as u64, units[u]) }
    }
}

/// Parse a count with optional multiplier suffix (b, K/k, M, G, T, kB, MB...).
pub fn parse_count(s: &str) -> Option<u64> {
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digits == 0 {
        return None;
    }
    let n: u64 = s[..digits].parse().ok()?;
    let mult: u64 = match &s[digits..] {
        "" => 1,
        "b" => 512,
        "K" | "k" | "KiB" => 1024,
        "kB" | "KB" => 1000,
        "M" | "MiB" => 1 << 20,
        "MB" => 1_000_000,
        "G" | "GiB" => 1 << 30,
        "GB" => 1_000_000_000,
        "T" | "TiB" => 1 << 40,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// Shell-style wildcard match: `*`, `?`, `[...]` (with `!`/`^` negation and
/// ranges) and backslash escapes.
pub fn fnmatch(pat: &str, s: &str, icase: bool) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = if icase {
        (pat.to_lowercase().chars().collect(), s.to_lowercase().chars().collect())
    } else {
        (pat.chars().collect(), s.chars().collect())
    };
    fn class(p: &[char], mut i: usize, c: char) -> Option<(bool, usize)> {
        // p[i] is just after '['
        let neg = i < p.len() && (p[i] == '!' || p[i] == '^');
        if neg {
            i += 1;
        }
        let mut matched = false;
        let mut first = true;
        while i < p.len() {
            if p[i] == ']' && !first {
                return Some((matched != neg, i + 1));
            }
            first = false;
            if p[i] == '[' && i + 1 < p.len() && p[i + 1] == ':'
                && let Some(end) = p[i + 2..].iter().position(|&x| x == ':').map(|e| e + i + 2)
                    && end + 1 < p.len() && p[end + 1] == ']' {
                        let name: String = p[i + 2..end].iter().collect();
                        if char_class(&name, c) {
                            matched = true;
                        }
                        i = end + 2;
                        continue;
                    }
            let mut lo = p[i];
            if lo == '\\' && i + 1 < p.len() {
                i += 1;
                lo = p[i];
            }
            if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
                let hi = p[i + 2];
                if lo <= c && c <= hi {
                    matched = true;
                }
                i += 3;
            } else {
                if lo == c {
                    matched = true;
                }
                i += 1;
            }
        }
        None
    }
    fn go(p: &[char], t: &[char]) -> bool {
        let (mut pi, mut ti) = (0, 0);
        let mut star: Option<(usize, usize)> = None;
        while ti < t.len() {
            if pi < p.len() {
                match p[pi] {
                    '*' => {
                        star = Some((pi, ti));
                        pi += 1;
                        continue;
                    }
                    '?' => {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                    '[' => {
                        if let Some((ok, next)) = class(p, pi + 1, t[ti]) {
                            if ok {
                                pi = next;
                                ti += 1;
                                continue;
                            }
                        } else if t[ti] == '[' {
                            pi += 1;
                            ti += 1;
                            continue;
                        }
                    }
                    '\\' if pi + 1 < p.len() => {
                        if p[pi + 1] == t[ti] {
                            pi += 2;
                            ti += 1;
                            continue;
                        }
                    }
                    c => {
                        if c == t[ti] {
                            pi += 1;
                            ti += 1;
                            continue;
                        }
                    }
                }
            }
            match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            }
        }
        while pi < p.len() && p[pi] == '*' {
            pi += 1;
        }
        pi == p.len()
    }
    go(&p, &t)
}

/// POSIX character classes, ASCII semantics.
pub fn char_class(name: &str, c: char) -> bool {
    match name {
        "alpha" => c.is_ascii_alphabetic(),
        "digit" => c.is_ascii_digit(),
        "alnum" => c.is_ascii_alphanumeric(),
        "upper" => c.is_ascii_uppercase(),
        "lower" => c.is_ascii_lowercase(),
        "space" => matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'),
        "blank" => c == ' ' || c == '\t',
        "punct" => c.is_ascii_punctuation(),
        "xdigit" => c.is_ascii_hexdigit(),
        "cntrl" => c.is_ascii_control(),
        "print" => (' '..='~').contains(&c),
        "graph" => c.is_ascii_graphic(),
        _ => false,
    }
}

/// The local time zone, honouring the shell's exported `TZ`.
pub fn local_tz(ctx: &Ctx) -> jiff::tz::TimeZone {
    if let Some(tz) = ctx.getenv("TZ") {
        let name = tz.strip_prefix(':').unwrap_or(tz);
        if let Ok(z) = jiff::tz::TimeZone::get(name) {
            return z;
        }
        if let Ok(z) = jiff::tz::TimeZone::posix(name) {
            return z;
        }
    }
    jiff::tz::TimeZone::system()
}

pub fn to_zoned(t: SystemTime, tz: &jiff::tz::TimeZone) -> Option<jiff::Zoned> {
    jiff::Timestamp::try_from(t).ok().map(|ts| ts.to_zoned(tz.clone()))
}

/// strftime with C-locale meanings for the specifiers jiff renders
/// differently (%c, %x, %X, %D...).
pub fn strftime(z: &jiff::Zoned, fmt: &str) -> String {
    let mut f = String::with_capacity(fmt.len());
    let mut it = fmt.chars().peekable();
    while let Some(c) = it.next() {
        if c == '%' {
            match it.peek() {
                Some('c') => {
                    it.next();
                    f.push_str("%a %b %e %H:%M:%S %Y");
                }
                Some('x') => {
                    it.next();
                    f.push_str("%m/%d/%y");
                }
                Some('X') => {
                    it.next();
                    f.push_str("%H:%M:%S");
                }
                Some(&n) => {
                    it.next();
                    f.push('%');
                    f.push(n);
                }
                None => f.push_str("%%"),
            }
        } else {
            f.push(c);
        }
    }
    let mut out = String::new();
    use std::fmt::Write;
    if write!(out, "{}", z.strftime(&f)).is_err() {
        return fmt.to_string();
    }
    out
}

/// Decode backslash escapes. `echo` mode supports `\c` (stop), `\0NNN`;
/// `printf` mode supports `\NNN`. Returns the bytes and whether `\c` was seen.
pub fn unescape(s: &str, printf_mode: bool) -> (Vec<u8>, bool) {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 >= b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let c = b[i + 1];
        i += 2;
        match c {
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'e' | b'E' => out.push(0x1b),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(11),
            b'\\' => out.push(b'\\'),
            b'c' => return (out, true),
            b'0'..=b'7' => {
                // echo: \0NNN (the 0 is a prefix); printf: \NNN.
                let (start, max) = if c == b'0' && !printf_mode { (i, 3) } else { (i - 1, 3) };
                let mut j = start;
                let mut v: u32 = 0;
                while j < b.len() && j - start < max && (b'0'..=b'7').contains(&b[j]) {
                    v = v * 8 + (b[j] - b'0') as u32;
                    j += 1;
                }
                out.push(v as u8);
                i = j;
            }
            b'x' => {
                let mut j = i;
                let mut v: u32 = 0;
                while j < b.len() && j - i < 2 && b[j].is_ascii_hexdigit() {
                    v = v * 16 + (b[j] as char).to_digit(16).unwrap();
                    j += 1;
                }
                if j == i {
                    out.extend_from_slice(b"\\x");
                } else {
                    out.push(v as u8);
                    i = j;
                }
            }
            b'u' | b'U' => {
                let max = if c == b'u' { 4 } else { 8 };
                let mut j = i;
                let mut v: u32 = 0;
                while j < b.len() && j - i < max && b[j].is_ascii_hexdigit() {
                    v = v * 16 + (b[j] as char).to_digit(16).unwrap();
                    j += 1;
                }
                match char::from_u32(v).filter(|_| j > i) {
                    Some(ch) => {
                        let mut tmp = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                        i = j;
                    }
                    None => {
                        out.push(b'\\');
                        out.push(c);
                    }
                }
            }
            _ => {
                out.push(b'\\');
                out.push(c);
            }
        }
    }
    (out, false)
}

/// Windows "executable" extensions.
#[cfg(not(unix))]
pub fn is_windows_exe(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| ["exe", "bat", "cmd", "com", "ps1"].iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
}

/// Report a usage error and return the given status.
pub fn usage(ctx: &mut Ctx, m: impl std::fmt::Display, status: i32) -> io::Result<i32> {
    ctx.err(m);
    Ok(status)
}

/// A small non-cryptographic random source (for mktemp names).
pub fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    if let Ok(d) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        h.write_u128(d.as_nanos());
    }
    h.write_u32(std::process::id());
    h.finish()
}

#[cfg(test)]
pub mod testutil {
    use std::path::PathBuf;

    /// A unique temporary directory removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> TempDir {
            let p = std::env::temp_dir().join(format!("vsh-test-{}-{}-{:x}", tag, std::process::id(), super::random_u64()));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
        pub fn write(&self, name: &str, data: &str) {
            let p = self.0.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, data).unwrap();
        }
        pub fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.0.join(name)).unwrap()
        }
        pub fn exists(&self, name: &str) -> bool {
            self.0.join(name).symlink_metadata().is_ok()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob() {
        assert!(fnmatch("*.rs", "main.rs", false));
        assert!(!fnmatch("*.rs", "main.rc", false));
        assert!(fnmatch("a?c", "abc", false));
        assert!(fnmatch("[a-c]x", "bx", false));
        assert!(!fnmatch("[!a-c]x", "bx", false));
        assert!(fnmatch("[[:digit:]]*", "1abc", false));
        assert!(fnmatch("*.TXT", "a.txt", true));
        assert!(fnmatch("a\\*", "a*", false));
        assert!(!fnmatch("a\\*", "ab", false));
        assert!(fnmatch("*a*b*", "xxaxxbxx", false));
    }

    #[test]
    fn sizes() {
        assert_eq!(human(500), "500");
        assert_eq!(human(1024), "1.0K");
        assert_eq!(human(1500), "1.5K");
        assert_eq!(human(10 * 1024), "10K");
        assert_eq!(human(5 * 1024 * 1024), "5.0M");
        assert_eq!(parse_count("2K"), Some(2048));
        assert_eq!(parse_count("x"), None);
    }

    #[test]
    fn escapes() {
        assert_eq!(unescape("a\\nb\\t", false).0, b"a\nb\t");
        assert_eq!(unescape("\\0101", false).0, b"A");
        assert_eq!(unescape("\\101", true).0, b"A");
        assert_eq!(unescape("x\\cy", false), (b"x".to_vec(), true));
        assert_eq!(unescape("\\u00e9", false).0, "é".as_bytes());
    }
}
