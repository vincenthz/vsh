//! `[[ ... ]]` conditional expressions.

use std::path::Path;

use crate::ast::{CondTok, Word};
use crate::glob;
use crate::shell::{Shell, Value};

/// 0 true, 1 false, 2 error.
pub fn eval(sh: &mut Shell, toks: &[CondTok]) -> i32 {
    let mut c = C { sh, toks, pos: 0 };
    let r = c.or();
    match r {
        Ok(_) if c.pos < toks.len() => {
            eprintln!("vsh: condition expected near token {}", c.pos + 1);
            2
        }
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            eprintln!("vsh: {e}");
            2
        }
    }
}

struct C<'a> {
    sh: &'a mut Shell,
    toks: &'a [CondTok],
    pos: usize,
}

const UNARY: &[&str] = &[
    "-a", "-b", "-c", "-d", "-e", "-f", "-g", "-h", "-k", "-L", "-n", "-o", "-p", "-r", "-s", "-S", "-t", "-u", "-v", "-w", "-x", "-z", "-O",
    "-G", "-N",
];
const BINARY: &[&str] = &["=", "==", "!=", "=~", "-eq", "-ne", "-lt", "-le", "-gt", "-ge", "-nt", "-ot", "-ef"];

impl C<'_> {
    fn op(&self) -> Option<&str> {
        match self.toks.get(self.pos) {
            Some(CondTok::Op(o)) => Some(o),
            _ => None,
        }
    }

    fn word_plain(&self, i: usize) -> Option<String> {
        match self.toks.get(i) {
            Some(CondTok::Word(w)) => w.as_plain(),
            Some(CondTok::Op(o)) if o == "<" || o == ">" => Some(o.clone()),
            _ => None,
        }
    }

    fn or(&mut self) -> Result<bool, String> {
        let mut v = self.and()?;
        while self.op() == Some("||") {
            self.pos += 1;
            let r = self.and()?;
            v = v || r;
        }
        Ok(v)
    }

    fn and(&mut self) -> Result<bool, String> {
        let mut v = self.not()?;
        while self.op() == Some("&&") {
            self.pos += 1;
            let r = self.not()?;
            v = v && r;
        }
        Ok(v)
    }

    fn not(&mut self) -> Result<bool, String> {
        if self.op() == Some("!") || self.word_plain(self.pos).as_deref() == Some("!") && self.toks.len() > self.pos + 1 {
            self.pos += 1;
            return Ok(!self.not()?);
        }
        self.primary()
    }

    fn word(&mut self) -> Result<Word, String> {
        match self.toks.get(self.pos) {
            Some(CondTok::Word(w)) => {
                self.pos += 1;
                Ok(w.clone())
            }
            _ => Err("condition expected".into()),
        }
    }

    fn primary(&mut self) -> Result<bool, String> {
        if self.op() == Some("(") {
            self.pos += 1;
            let v = self.or()?;
            if self.op() != Some(")") {
                return Err("missing )".into());
            }
            self.pos += 1;
            return Ok(v);
        }
        // Binary?
        if let Some(op) = self.word_plain(self.pos + 1)
            && (BINARY.contains(&op.as_str()) || op == "<" || op == ">")
        {
            let lhs = self.word()?;
            self.pos += 1;
            let rhs = self.word()?;
            return self.binary(&lhs, &op, &rhs);
        }
        if let Some(op) = self.word_plain(self.pos)
            && UNARY.contains(&op.as_str())
            && matches!(self.toks.get(self.pos + 1), Some(CondTok::Word(_)))
        {
            self.pos += 1;
            let w = self.word()?;
            let arg = self.sh.expand_str(&w).map_err(|e| e.0)?;
            return Ok(self.unary(&op, &arg));
        }
        let w = self.word()?;
        let s = self.sh.expand_str(&w).map_err(|e| e.0)?;
        Ok(!s.is_empty())
    }

    fn unary(&mut self, op: &str, arg: &str) -> bool {
        match op {
            "-n" => !arg.is_empty(),
            "-z" => arg.is_empty(),
            "-v" => self.sh.vars.contains_key(arg),
            "-o" => self.sh.opts.get(arg).unwrap_or(false),
            "-t" => {
                use std::io::IsTerminal;
                match arg {
                    "0" => std::io::stdin().is_terminal(),
                    "1" => std::io::stdout().is_terminal(),
                    "2" => std::io::stderr().is_terminal(),
                    _ => false,
                }
            }
            _ => file_test(op, &crate::utils::resolve(&self.sh.cwd, arg)),
        }
    }

    fn binary(&mut self, lhs: &Word, op: &str, rhs: &Word) -> Result<bool, String> {
        let l = self.sh.expand_str(lhs).map_err(|e| e.0)?;
        match op {
            "=" | "==" | "!=" => {
                let pat = self.sh.expand_pattern(rhs).map_err(|e| e.0)?;
                let m = glob::matches(&pat, &l);
                return Ok(if op == "!=" { !m } else { m });
            }
            "=~" => {
                let r = self.sh.expand_str(rhs).map_err(|e| e.0)?;
                let re = regex::Regex::new(&r).map_err(|e| format!("bad regex: {e}"))?;
                return Ok(match re.captures(&l) {
                    Some(c) => {
                        let whole = c.get(0).map(|m| m.as_str().to_string()).unwrap_or_default();
                        let groups: Vec<String> =
                            c.iter().skip(1).map(|m| m.map(|m| m.as_str().to_string()).unwrap_or_default()).collect();
                        self.sh.set_str("MATCH", whole.clone());
                        self.sh.set_var("match", Value::Array(groups.clone()));
                        self.sh.set_var("BASH_REMATCH", Value::Array([whole].into_iter().chain(groups).collect()));
                        true
                    }
                    None => false,
                });
            }
            _ => {}
        }
        let r = self.sh.expand_str(rhs).map_err(|e| e.0)?;
        Ok(match op {
            "<" => l < r,
            ">" => l > r,
            "-nt" | "-ot" | "-ef" => {
                let a = crate::utils::resolve(&self.sh.cwd, &l);
                let b = crate::utils::resolve(&self.sh.cwd, &r);
                file_compare(op, &a, &b)
            }
            _ => {
                let a = self.sh.arith(&l).map_err(|e| e.0)?;
                let b = self.sh.arith(&r).map_err(|e| e.0)?;
                match op {
                    "-eq" => a == b,
                    "-ne" => a != b,
                    "-lt" => a < b,
                    "-le" => a <= b,
                    "-gt" => a > b,
                    _ => a >= b,
                }
            }
        })
    }
}

pub fn file_test(op: &str, p: &Path) -> bool {
    let lmeta = std::fs::symlink_metadata(p);
    if op == "-h" || op == "-L" {
        return lmeta.map(|m| m.file_type().is_symlink()).unwrap_or(false);
    }
    let Ok(m) = std::fs::metadata(p) else { return false };
    match op {
        "-e" | "-a" => true,
        "-f" => m.is_file(),
        "-d" => m.is_dir(),
        "-s" => m.len() > 0,
        "-r" => access(p, 4),
        "-w" => access(p, 2),
        "-x" => access(p, 1),
        #[cfg(unix)]
        _ => {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            let ft = m.file_type();
            match op {
                "-b" => ft.is_block_device(),
                "-c" => ft.is_char_device(),
                "-p" => ft.is_fifo(),
                "-S" => ft.is_socket(),
                "-u" => m.mode() & 0o4000 != 0,
                "-g" => m.mode() & 0o2000 != 0,
                "-k" => m.mode() & 0o1000 != 0,
                "-O" => m.uid() == unsafe { libc::geteuid() },
                "-G" => m.gid() == unsafe { libc::getegid() },
                "-N" => m.mtime() > m.atime(),
                _ => false,
            }
        }
        #[cfg(not(unix))]
        _ => false,
    }
}

#[cfg(unix)]
fn access(p: &Path, mode: i32) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return false };
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

#[cfg(not(unix))]
fn access(p: &Path, mode: i32) -> bool {
    let Ok(m) = std::fs::metadata(p) else { return false };
    match mode {
        2 => !m.permissions().readonly(),
        1 => {
            m.is_dir()
                || p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    ["exe", "com", "bat", "cmd"].iter().any(|x| x.eq_ignore_ascii_case(e))
                })
        }
        _ => true,
    }
}

fn file_compare(op: &str, a: &Path, b: &Path) -> bool {
    let (ma, mb) = (std::fs::metadata(a), std::fs::metadata(b));
    match op {
        "-nt" => match (ma.and_then(|m| m.modified()), mb.and_then(|m| m.modified())) {
            (Ok(x), Ok(y)) => x > y,
            (Ok(_), Err(_)) => true,
            _ => false,
        },
        "-ot" => match (ma.and_then(|m| m.modified()), mb.and_then(|m| m.modified())) {
            (Ok(x), Ok(y)) => x < y,
            (Err(_), Ok(_)) => true,
            _ => false,
        },
        _ => match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        },
    }
}
