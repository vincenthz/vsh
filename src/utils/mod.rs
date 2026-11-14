//! Builtin coreutils. They run inside the shell process (on the calling
//! thread), so they work identically on every platform and need nothing from
//! `$PATH`.
//!
//! A utility is a plain function over a [`Ctx`]: it gets its streams, the
//! shell's logical working directory and the exported environment. Utilities
//! must never touch process-global state (current directory, environment,
//! `std::io::stdout()` directly): several of them run concurrently in one
//! pipeline, each on its own thread.

use std::fmt::Display;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

pub mod args;
mod common;
mod find;
mod fs;
mod grep;
mod ls;
mod misc;
mod printf;
mod test;
mod text;

pub struct Ctx<'a> {
    /// The name the utility was invoked as (for error messages).
    pub name: &'a str,
    pub stdin: &'a mut dyn BufRead,
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
    /// True when stdout is a terminal (colors, column output).
    pub stdout_tty: bool,
    /// The shell's working directory. Relative paths resolve against it.
    pub cwd: &'a Path,
    /// Exported environment, sorted by name.
    pub env: &'a [(String, String)],
}

/// Arguments exclude argv[0]. Return the exit status; an `Err` is reported as
/// `name: error` with status 1 (a broken pipe is silent).
pub type UtilFn = fn(&mut Ctx, &[String]) -> io::Result<i32>;

impl Ctx<'_> {
    /// Resolve a user-supplied path against the shell's working directory.
    /// `/dev/null` maps to `NUL` on Windows.
    pub fn path(&self, p: &str) -> PathBuf {
        resolve(self.cwd, p)
    }

    /// Print `name: msg` on stderr.
    pub fn err(&mut self, msg: impl Display) {
        let _ = writeln!(self.stderr, "{}: {}", self.name, msg);
    }

    pub fn getenv(&self, key: &str) -> Option<&str> {
        self.env.iter().find(|(k, _)| env_key_eq(k, key)).map(|(_, v)| v.as_str())
    }

    /// An error once Ctrl-C was pressed; call it in long loops.
    pub fn check_interrupt(&self) -> io::Result<()> {
        crate::io::check_interrupt()
    }
}

pub fn resolve(cwd: &Path, p: &str) -> PathBuf {
    if cfg!(windows) && p == "/dev/null" {
        return PathBuf::from("NUL");
    }
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }
}

pub fn env_key_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) { a.eq_ignore_ascii_case(b) } else { a == b }
}

const UTILS: &[(&str, UtilFn)] = &[
    ("[", test::test),
    ("basename", fs::basename),
    ("cat", text::cat),
    ("clear", misc::clear),
    ("cp", fs::cp),
    ("cut", text::cut),
    ("date", misc::date),
    ("dirname", fs::dirname),
    ("du", fs::du),
    ("echo", misc::echo),
    ("env", misc::env),
    ("false", misc::false_),
    ("find", find::find),
    ("grep", grep::grep),
    ("head", text::head),
    ("hostname", misc::hostname),
    ("ln", fs::ln),
    ("ls", ls::ls),
    ("mkdir", fs::mkdir),
    ("mktemp", fs::mktemp),
    ("mv", fs::mv),
    ("nl", text::nl),
    ("nproc", misc::nproc),
    ("paste", text::paste),
    ("printenv", misc::printenv),
    ("printf", printf::printf),
    ("pwd", misc::pwd),
    ("readlink", fs::readlink),
    ("realpath", fs::realpath),
    ("rev", text::rev),
    ("rm", fs::rm),
    ("rmdir", fs::rmdir),
    ("seq", misc::seq),
    ("sleep", misc::sleep),
    ("sort", text::sort),
    ("stat", fs::stat),
    ("tac", text::tac),
    ("tail", text::tail),
    ("tee", text::tee),
    ("test", test::test),
    ("touch", fs::touch),
    ("tr", text::tr),
    ("true", misc::true_),
    ("uname", misc::uname),
    ("uniq", text::uniq),
    ("wc", text::wc),
    ("whoami", misc::whoami),
    ("yes", misc::yes),
    #[cfg(unix)]
    ("chmod", fs::chmod),
];

pub fn lookup(name: &str) -> Option<UtilFn> {
    UTILS.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
}

pub fn names() -> impl Iterator<Item = &'static str> {
    UTILS.iter().map(|(n, _)| *n)
}

/// Run a utility and turn its result into an exit status.
pub fn run(f: UtilFn, ctx: &mut Ctx, args: &[String]) -> i32 {
    let r = f(ctx, args);
    let flushed = ctx.stdout.flush();
    match r.and_then(|s| flushed.map(|_| s)) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => 141,
        Err(_) if crate::io::interrupted() => 130,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => 130,
        Err(e) => {
            ctx.err(e);
            1
        }
    }
}

/// Test helper: run `name args` with the given stdin in `cwd`, returning
/// (status, stdout, stderr).
#[cfg(test)]
pub fn run_capture(name: &str, args: &[&str], stdin: &[u8], cwd: &Path) -> (i32, String, String) {
    let f = lookup(name).unwrap_or_else(|| panic!("no util {name}"));
    let mut input = io::Cursor::new(stdin.to_vec());
    let mut out = Vec::new();
    let mut err = Vec::new();
    let env = vec![("HOME".to_string(), "/home/test".to_string())];
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let status = {
        let mut ctx = Ctx {
            name,
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            stdout_tty: false,
            cwd,
            env: &env,
        };
        run(f, &mut ctx, &args)
    };
    (status, String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&err).into_owned())
}
