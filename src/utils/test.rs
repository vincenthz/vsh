//! `test` and `[`.

use std::fs;
use std::io;
use std::path::Path;

use super::Ctx;

struct Err2(String);

pub fn test(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let mut a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    if ctx.name == "[" {
        if a.last() != Some(&"]") {
            ctx.err("missing ']'");
            return Ok(2);
        }
        a.pop();
    }
    match eval_top(ctx, &a) {
        Ok(true) => Ok(0),
        Ok(false) => Ok(1),
        Err(Err2(m)) => {
            ctx.err(m);
            Ok(2)
        }
    }
}

fn is_unary(op: &str) -> bool {
    matches!(
        op,
        "-b" | "-c" | "-d" | "-e" | "-f" | "-g" | "-h" | "-L" | "-k" | "-p" | "-r" | "-s" | "-S" | "-t" | "-u" | "-w" | "-x"
            | "-z" | "-n" | "-O" | "-G"
    )
}

fn is_binary(op: &str) -> bool {
    matches!(
        op,
        "=" | "==" | "!=" | "<" | ">" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" | "-nt" | "-ot" | "-ef"
    )
}

/// POSIX rules by argument count, falling back to a full parser.
fn eval_top(ctx: &Ctx, a: &[&str]) -> Result<bool, Err2> {
    match a.len() {
        0 => Ok(false),
        1 => Ok(!a[0].is_empty()),
        2 => {
            if a[0] == "!" {
                Ok(a[1].is_empty())
            } else if is_unary(a[0]) {
                unary(ctx, a[0], a[1])
            } else {
                Err(Err2(format!("{}: unary operator expected", a[0])))
            }
        }
        3 => {
            if is_binary(a[1]) {
                binary(ctx, a[0], a[1], a[2])
            } else if a[0] == "!" {
                Ok(!eval_top(ctx, &a[1..])?)
            } else if a[0] == "(" && a[2] == ")" {
                Ok(!a[1].is_empty())
            } else if a[1] == "-a" || a[1] == "-o" {
                let (l, r) = (!a[0].is_empty(), !a[2].is_empty());
                Ok(if a[1] == "-a" { l && r } else { l || r })
            } else {
                Err(Err2(format!("{}: binary operator expected", a[1])))
            }
        }
        4 if a[0] == "!" => Ok(!eval_top(ctx, &a[1..])?),
        4 if a[0] == "(" && a[3] == ")" => eval_top(ctx, &a[1..3]),
        _ => {
            let mut p = Parser { ctx, a, i: 0 };
            let v = p.or()?;
            if p.i < a.len() {
                return Err(Err2(format!("{}: unexpected argument", a[p.i])));
            }
            Ok(v)
        }
    }
}

struct Parser<'a, 'b> {
    ctx: &'a Ctx<'b>,
    a: &'a [&'a str],
    i: usize,
}

impl<'a> Parser<'a, '_> {
    fn peek(&self, k: usize) -> Option<&'a str> {
        self.a.get(self.i + k).copied()
    }
    fn or(&mut self) -> Result<bool, Err2> {
        let mut v = self.and()?;
        while self.peek(0) == Some("-o") {
            self.i += 1;
            let r = self.and()?;
            v = v || r;
        }
        Ok(v)
    }
    fn and(&mut self) -> Result<bool, Err2> {
        let mut v = self.not()?;
        while self.peek(0) == Some("-a") {
            self.i += 1;
            let r = self.not()?;
            v = v && r;
        }
        Ok(v)
    }
    fn not(&mut self) -> Result<bool, Err2> {
        if self.peek(0) == Some("!") && self.peek(1).is_some() {
            self.i += 1;
            return Ok(!self.not()?);
        }
        self.primary()
    }
    fn primary(&mut self) -> Result<bool, Err2> {
        let Some(t) = self.peek(0) else {
            return Err(Err2("argument expected".into()));
        };
        if t == "(" {
            self.i += 1;
            let v = self.or()?;
            if self.peek(0) != Some(")") {
                return Err(Err2("')' expected".into()));
            }
            self.i += 1;
            return Ok(v);
        }
        if let (Some(op), Some(r)) = (self.peek(1), self.peek(2))
            && is_binary(op) {
                self.i += 3;
                return binary(self.ctx, t, op, r);
            }
        if is_unary(t)
            && let Some(arg) = self.peek(1) {
                self.i += 2;
                return unary(self.ctx, t, arg);
            }
        self.i += 1;
        Ok(!t.is_empty())
    }
}

fn int(s: &str) -> Result<i64, Err2> {
    s.trim().parse::<i64>().map_err(|_| Err2(format!("integer expression expected: {s}")))
}

fn binary(ctx: &Ctx, l: &str, op: &str, r: &str) -> Result<bool, Err2> {
    Ok(match op {
        "=" | "==" => l == r,
        "!=" => l != r,
        "<" => l < r,
        ">" => l > r,
        "-eq" => int(l)? == int(r)?,
        "-ne" => int(l)? != int(r)?,
        "-lt" => int(l)? < int(r)?,
        "-le" => int(l)? <= int(r)?,
        "-gt" => int(l)? > int(r)?,
        "-ge" => int(l)? >= int(r)?,
        "-nt" | "-ot" => {
            let m = |p: &str| fs::metadata(ctx.path(p)).and_then(|m| m.modified()).ok();
            match (m(l), m(r)) {
                (Some(a), Some(b)) => if op == "-nt" { a > b } else { a < b },
                (Some(_), None) => op == "-nt",
                (None, Some(_)) => op == "-ot",
                _ => false,
            }
        }
        "-ef" => same_file(&ctx.path(l), &ctx.path(r)),
        _ => return Err(Err2(format!("{op}: binary operator expected"))),
    })
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

#[cfg(unix)]
fn access(p: &Path, mode: libc::c_int) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return false };
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

fn unary(ctx: &Ctx, op: &str, arg: &str) -> Result<bool, Err2> {
    if op == "-z" {
        return Ok(arg.is_empty());
    }
    if op == "-n" {
        return Ok(!arg.is_empty());
    }
    if op == "-t" {
        use std::io::IsTerminal;
        let fd = int(arg)?;
        return Ok(match fd {
            0 => io::stdin().is_terminal(),
            1 => ctx.stdout_tty,
            2 => io::stderr().is_terminal(),
            _ => false,
        });
    }
    let p = ctx.path(arg);
    if arg.is_empty() {
        return Ok(false);
    }
    if op == "-h" || op == "-L" {
        return Ok(fs::symlink_metadata(&p).map(|m| m.file_type().is_symlink()).unwrap_or(false));
    }
    let Ok(m) = fs::metadata(&p) else { return Ok(false) };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let ft = m.file_type();
        Ok(match op {
            "-e" => true,
            "-f" => ft.is_file(),
            "-d" => ft.is_dir(),
            "-b" => ft.is_block_device(),
            "-c" => ft.is_char_device(),
            "-p" => ft.is_fifo(),
            "-S" => ft.is_socket(),
            "-s" => m.len() > 0,
            "-g" => m.mode() & 0o2000 != 0,
            "-u" => m.mode() & 0o4000 != 0,
            "-k" => m.mode() & 0o1000 != 0,
            "-r" => access(&p, libc::R_OK),
            "-w" => access(&p, libc::W_OK),
            "-x" => access(&p, libc::X_OK),
            "-O" => m.uid() == unsafe { libc::geteuid() },
            "-G" => m.gid() == unsafe { libc::getegid() },
            _ => false,
        })
    }
    #[cfg(not(unix))]
    {
        let ft = m.file_type();
        Ok(match op {
            "-e" | "-r" | "-O" | "-G" => true,
            "-f" => ft.is_file(),
            "-d" => ft.is_dir(),
            "-s" => m.len() > 0,
            "-w" => !m.permissions().readonly(),
            "-x" => ft.is_dir() || super::common::is_windows_exe(&p),
            _ => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::utils::common::testutil::TempDir;
    use crate::utils::run_capture;

    fn t(args: &[&str]) -> i32 {
        run_capture("test", args, b"", std::path::Path::new(".")).0
    }

    #[test]
    fn strings_and_numbers() {
        assert_eq!(t(&[]), 1);
        assert_eq!(t(&["x"]), 0);
        assert_eq!(t(&[""]), 1);
        assert_eq!(t(&["-n", ""]), 1);
        assert_eq!(t(&["-z", ""]), 0);
        assert_eq!(t(&["a", "=", "a"]), 0);
        assert_eq!(t(&["a", "!=", "a"]), 1);
        assert_eq!(t(&["!", "=", "x"]), 1);
        assert_eq!(t(&["3", "-lt", "10"]), 0);
        assert_eq!(t(&["3", "-gt", "10"]), 1);
        assert_eq!(t(&["!", "3", "-gt", "10"]), 0);
        assert_eq!(t(&["1", "-eq", "1", "-a", "2", "-eq", "3"]), 1);
        assert_eq!(t(&["1", "-eq", "1", "-o", "2", "-eq", "3"]), 0);
        assert_eq!(t(&["(", "1", "-eq", "2", ")", "-o", "x"]), 0);
        assert_eq!(t(&["a", "-eq", "1"]), 2);
        assert_eq!(t(&["-n"]), 0);
    }

    #[test]
    fn bracket() {
        let (s, _, e) = run_capture("[", &["a", "=", "a"], b"", std::path::Path::new("."));
        assert_eq!((s, e.as_str()), (2, "[: missing ']'\n"));
        assert_eq!(run_capture("[", &["a", "=", "a", "]"], b"", std::path::Path::new(".")).0, 0);
    }

    #[test]
    fn files() {
        let d = TempDir::new("test");
        d.write("f", "data");
        d.write("empty", "");
        std::fs::create_dir(d.path().join("dir")).unwrap();
        let r = |a: &[&str]| run_capture("test", a, b"", d.path()).0;
        assert_eq!(r(&["-e", "f"]), 0);
        assert_eq!(r(&["-f", "f"]), 0);
        assert_eq!(r(&["-d", "f"]), 1);
        assert_eq!(r(&["-d", "dir"]), 0);
        assert_eq!(r(&["-s", "f"]), 0);
        assert_eq!(r(&["-s", "empty"]), 1);
        assert_eq!(r(&["-e", "nope"]), 1);
        assert_eq!(r(&["-r", "f"]), 0);
        assert_eq!(r(&["f", "-ef", "f"]), 0);
        assert_eq!(r(&["f", "-ef", "empty"]), 1);
        assert_eq!(r(&["f", "-nt", "nope"]), 0);
    }
}
