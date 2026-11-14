//! `find` (no -exec: pipe into the shell instead).

use std::fs::{self, Metadata};
use std::io;
use std::path::Path;
use std::time::SystemTime;

use super::Ctx;
use super::common::{fnmatch, msg, q, usage};
use super::fs::join_disp;

enum Expr {
    True,
    False,
    Name(String, bool),
    Path(String, bool),
    Type(char),
    Empty,
    Newer(SystemTime),
    Size(std::cmp::Ordering, u64, u64),
    Print(bool),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

struct Parser<'a> {
    t: &'a [String],
    i: usize,
    has_action: bool,
    maxdepth: Option<usize>,
    mindepth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&str> {
        self.t.get(self.i).map(|s| s.as_str())
    }

    fn or(&mut self, ctx: &Ctx) -> Result<Expr, String> {
        let mut l = self.and(ctx)?;
        while matches!(self.peek(), Some("-o" | "-or")) {
            self.i += 1;
            let r = self.and(ctx)?;
            l = Expr::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and(&mut self, ctx: &Ctx) -> Result<Expr, String> {
        let mut l = self.not(ctx)?;
        loop {
            match self.peek() {
                Some("-a" | "-and") => {
                    self.i += 1;
                }
                Some("-o" | "-or" | ")") | None => break,
                _ => {}
            }
            let r = self.not(ctx)?;
            l = Expr::And(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn not(&mut self, ctx: &Ctx) -> Result<Expr, String> {
        if matches!(self.peek(), Some("!" | "-not")) {
            self.i += 1;
            return Ok(Expr::Not(Box::new(self.not(ctx)?)));
        }
        self.primary(ctx)
    }

    fn arg(&mut self, opt: &str) -> Result<String, String> {
        match self.t.get(self.i) {
            Some(a) => {
                self.i += 1;
                Ok(a.clone())
            }
            None => Err(format!("missing argument to '{opt}'")),
        }
    }

    fn primary(&mut self, ctx: &Ctx) -> Result<Expr, String> {
        let Some(tok) = self.t.get(self.i).cloned() else {
            return Err("expected an expression".into());
        };
        self.i += 1;
        Ok(match tok.as_str() {
            "(" => {
                let e = self.or(ctx)?;
                if self.peek() != Some(")") {
                    return Err("invalid expression; expected ')'".into());
                }
                self.i += 1;
                e
            }
            "-true" => Expr::True,
            "-false" => Expr::False,
            "-name" => Expr::Name(self.arg(&tok)?, false),
            "-iname" => Expr::Name(self.arg(&tok)?, true),
            "-path" | "-wholename" => Expr::Path(self.arg(&tok)?, false),
            "-ipath" | "-iwholename" => Expr::Path(self.arg(&tok)?, true),
            "-type" => {
                let a = self.arg(&tok)?;
                match a.as_str() {
                    "f" | "d" | "l" | "p" | "s" | "b" | "c" => Expr::Type(a.chars().next().unwrap()),
                    _ => return Err(format!("Unknown argument to -type: {a}")),
                }
            }
            "-empty" => Expr::Empty,
            "-newer" => {
                let f = self.arg(&tok)?;
                match fs::metadata(ctx.path(&f)).and_then(|m| m.modified()) {
                    Ok(t) => Expr::Newer(t),
                    Err(e) => return Err(format!("{}: {}", q(&f), msg(&e))),
                }
            }
            "-size" => {
                let a = self.arg(&tok)?;
                let (ord, rest) = match a.strip_prefix('+') {
                    Some(r) => (std::cmp::Ordering::Greater, r),
                    None => match a.strip_prefix('-') {
                        Some(r) => (std::cmp::Ordering::Less, r),
                        None => (std::cmp::Ordering::Equal, a.as_str()),
                    },
                };
                let (num, unit) = match rest.chars().last() {
                    Some(c) if c.is_ascii_alphabetic() => (&rest[..rest.len() - 1], c),
                    _ => (rest, 'b'),
                };
                let unit = match unit {
                    'c' => 1,
                    'w' => 2,
                    'b' => 512,
                    'k' => 1024,
                    'M' => 1 << 20,
                    'G' => 1 << 30,
                    _ => return Err(format!("invalid -size type '{unit}'")),
                };
                match num.parse::<u64>() {
                    Ok(n) => Expr::Size(ord, n, unit),
                    Err(_) => return Err(format!("invalid argument '{a}' to -size")),
                }
            }
            "-print" => {
                self.has_action = true;
                Expr::Print(false)
            }
            "-print0" => {
                self.has_action = true;
                Expr::Print(true)
            }
            "-maxdepth" | "-mindepth" => {
                let a = self.arg(&tok)?;
                let n: usize = a.parse().map_err(|_| format!("Expected a positive decimal integer argument to {tok}, but got '{a}'"))?;
                if tok == "-maxdepth" {
                    self.maxdepth = Some(n);
                } else {
                    self.mindepth = n;
                }
                Expr::True
            }
            "-exec" | "-execdir" | "-ok" | "-delete" => {
                return Err(format!("{tok} is not supported; pipe the output into the shell (e.g. with a while read loop)"));
            }
            other => return Err(format!("unknown predicate '{other}'")),
        })
    }
}

struct Walk<'a> {
    expr: &'a Expr,
    maxdepth: Option<usize>,
    mindepth: usize,
    failed: bool,
}

fn is_empty(p: &Path, m: &Metadata) -> bool {
    if m.is_dir() {
        fs::read_dir(p).map(|mut r| r.next().is_none()).unwrap_or(false)
    } else {
        m.is_file() && m.len() == 0
    }
}

fn type_matches(m: &Metadata, c: char) -> bool {
    let ft = m.file_type();
    match c {
        'f' => ft.is_file(),
        'd' => ft.is_dir(),
        'l' => ft.is_symlink(),
        #[cfg(unix)]
        'p' | 's' | 'b' | 'c' => {
            use std::os::unix::fs::FileTypeExt;
            match c {
                'p' => ft.is_fifo(),
                's' => ft.is_socket(),
                'b' => ft.is_block_device(),
                _ => ft.is_char_device(),
            }
        }
        _ => false,
    }
}

fn eval(ctx: &mut Ctx, e: &Expr, path: &Path, disp: &str, m: &Metadata) -> io::Result<bool> {
    Ok(match e {
        Expr::True => true,
        Expr::False => false,
        Expr::Name(p, i) => {
            let base = super::fs::base_name(disp);
            fnmatch(p, &base, *i)
        }
        Expr::Path(p, i) => fnmatch(p, disp, *i),
        Expr::Type(c) => type_matches(m, *c),
        Expr::Empty => is_empty(path, m),
        Expr::Newer(t) => m.modified().map(|x| x > *t).unwrap_or(false),
        Expr::Size(ord, n, unit) => {
            let units = m.len().div_ceil(*unit);
            units.cmp(n) == *ord
        }
        Expr::Print(nul) => {
            ctx.stdout.write_all(disp.as_bytes())?;
            ctx.stdout.write_all(if *nul { b"\0" } else { b"\n" })?;
            true
        }
        Expr::Not(x) => !eval(ctx, x, path, disp, m)?,
        Expr::And(a, b) => eval(ctx, a, path, disp, m)? && eval(ctx, b, path, disp, m)?,
        Expr::Or(a, b) => eval(ctx, a, path, disp, m)? || eval(ctx, b, path, disp, m)?,
    })
}

fn walk(ctx: &mut Ctx, w: &mut Walk, path: &Path, disp: &str, depth: usize) -> io::Result<()> {
    ctx.check_interrupt()?;
    let m = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            ctx.err(format!("{}: {}", q(disp), msg(&e)));
            w.failed = true;
            return Ok(());
        }
    };
    if depth >= w.mindepth {
        eval(ctx, w.expr, path, disp, &m)?;
    }
    if m.is_dir() && w.maxdepth.is_none_or(|max| depth < max) {
        let mut entries: Vec<fs::DirEntry> = match fs::read_dir(path).and_then(|r| r.collect::<Result<_, _>>()) {
            Ok(v) => v,
            Err(e) => {
                ctx.err(format!("{}: {}", q(disp), msg(&e)));
                w.failed = true;
                return Ok(());
            }
        };
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            walk(ctx, w, &e.path(), &join_disp(disp, &name), depth + 1)?;
        }
    }
    Ok(())
}

pub fn find(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let split = args.iter().position(|a| (a.starts_with('-') && a.len() > 1) || a == "(" || a == "!").unwrap_or(args.len());
    let (paths, exprs) = args.split_at(split);
    let mut p = Parser { t: exprs, i: 0, has_action: false, maxdepth: None, mindepth: 0 };
    let expr = if exprs.is_empty() {
        Expr::True
    } else {
        match p.or(ctx) {
            Ok(e) => e,
            Err(m) => return usage(ctx, m, 1),
        }
    };
    if p.i < exprs.len() {
        return usage(ctx, format!("paths must precede expression: {}", q(&exprs[p.i])), 1);
    }
    let expr = if p.has_action { expr } else { Expr::And(Box::new(expr), Box::new(Expr::Print(false))) };
    let mut w = Walk { expr: &expr, maxdepth: p.maxdepth, mindepth: p.mindepth, failed: false };
    let paths: Vec<String> = if paths.is_empty() { vec![".".into()] } else { paths.to_vec() };
    for start in &paths {
        let sp = ctx.path(start);
        walk(ctx, &mut w, &sp, start, 0)?;
    }
    Ok(if w.failed { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use crate::utils::common::testutil::TempDir;
    use crate::utils::run_capture;

    #[test]
    fn expressions() {
        let d = TempDir::new("find");
        d.write("a.rs", "fn main(){}");
        d.write("b.txt", "");
        d.write("src/c.rs", "x");
        d.write("src/deep/d.RS", "y");
        std::fs::create_dir(d.path().join("emptydir")).unwrap();
        let r = |a: &[&str]| run_capture("find", a, b"", d.path()).1;
        assert_eq!(r(&[]), ".\n./a.rs\n./b.txt\n./emptydir\n./src\n./src/c.rs\n./src/deep\n./src/deep/d.RS\n");
        assert_eq!(r(&["-name", "*.rs"]), "./a.rs\n./src/c.rs\n");
        assert_eq!(r(&[".", "-iname", "*.rs", "-type", "f"]), "./a.rs\n./src/c.rs\n./src/deep/d.RS\n");
        assert_eq!(r(&["src", "-type", "d"]), "src\nsrc/deep\n");
        assert_eq!(r(&["-maxdepth", "1", "-type", "f"]), "./a.rs\n./b.txt\n");
        assert_eq!(r(&["-mindepth", "2", "-type", "f"]), "./src/c.rs\n./src/deep/d.RS\n");
        assert_eq!(r(&["-empty"]), "./b.txt\n./emptydir\n");
        assert_eq!(r(&["-type", "f", "!", "-name", "*.rs"]), "./b.txt\n./src/deep/d.RS\n");
        assert_eq!(r(&["-name", "a.rs", "-o", "-name", "b.txt"]), "./a.rs\n./b.txt\n");
        assert_eq!(r(&["(", "-name", "a.rs", "-o", "-name", "c.rs", ")", "-print"]), "./a.rs\n./src/c.rs\n");
        assert_eq!(r(&["-path", "./src/*", "-type", "f"]), "./src/c.rs\n./src/deep/d.RS\n");
        assert_eq!(r(&["-size", "+0c", "-type", "f"]), "./a.rs\n./src/c.rs\n./src/deep/d.RS\n");
        assert_eq!(r(&["-name", "a.rs", "-print0"]), "./a.rs\0");
        let (s, _, e) = run_capture("find", &["-bogus"], b"", d.path());
        assert_eq!((s, e.as_str()), (1, "find: unknown predicate '-bogus'\n"));
        let (s, _, e) = run_capture("find", &["nope"], b"", d.path());
        assert_eq!((s, e.as_str()), (1, "find: 'nope': No such file or directory\n"));
    }
}
