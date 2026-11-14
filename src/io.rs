//! Stream plumbing shared by builtins, utilities and external processes.
//!
//! Every command runs against an [`IoSet`]: three cheaply cloneable stream
//! descriptions. Builtins turn them into `Read`/`Write` objects, external
//! processes turn them into `Stdio`. Handles are reference counted, so a pipe
//! end closes once the last command holding it is done.

use std::fs::File;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone)]
pub enum Input {
    /// The shell's own standard input.
    Inherit,
    Null,
    File(Arc<File>),
    Pipe(Arc<PipeReader>),
    /// Here-document or here-string contents. The read position is shared,
    /// like a file offset, so `while read l; do ...; done <<< $x` advances.
    Bytes(Arc<Mutex<Mem>>),
}

pub struct Mem {
    data: Vec<u8>,
    pos: usize,
}

impl Input {
    pub fn bytes(data: Vec<u8>) -> Input {
        Input::Bytes(Arc::new(Mutex::new(Mem { data, pos: 0 })))
    }
}

struct MemReader(Arc<Mutex<Mem>>);
impl Read for MemReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut m = self.0.lock().unwrap();
        let n = buf.len().min(m.data.len() - m.pos);
        let start = m.pos;
        buf[..n].copy_from_slice(&m.data[start..start + n]);
        m.pos += n;
        Ok(n)
    }
}

#[derive(Clone)]
pub enum Output {
    Stdout,
    Stderr,
    Null,
    File(Arc<File>),
    Pipe(Arc<PipeWriter>),
}

#[derive(Clone)]
pub struct IoSet {
    pub stdin: Input,
    pub stdout: Output,
    pub stderr: Output,
}

impl Default for IoSet {
    fn default() -> Self {
        IoSet { stdin: Input::Inherit, stdout: Output::Stdout, stderr: Output::Stderr }
    }
}

pub fn pipe() -> io::Result<(Input, Output)> {
    let (r, w) = io::pipe()?;
    Ok((Input::Pipe(Arc::new(r)), Output::Pipe(Arc::new(w))))
}

impl Input {
    /// An unbuffered reader. Reading one byte at a time from it never consumes
    /// more than asked, which `read` relies on.
    pub fn reader(&self) -> Box<dyn Read + Send> {
        match self {
            Input::Inherit => inherit_stdin(),
            Input::Null => Box::new(io::empty()),
            Input::File(f) => Box::new(ArcFile(f.clone())),
            Input::Pipe(p) => Box::new(ArcPipeR(p.clone())),
            Input::Bytes(b) => Box::new(MemReader(b.clone())),
        }
    }

    /// The `Stdio` for a child, plus the bytes to feed it when the input is
    /// in-memory (the caller writes them through `ChildStdin` on a thread).
    pub fn stdio(&self) -> io::Result<(Stdio, Option<Vec<u8>>)> {
        Ok(match self {
            Input::Inherit => (Stdio::inherit(), None),
            Input::Null => (Stdio::null(), None),
            Input::File(f) => (Stdio::from(f.try_clone()?), None),
            Input::Pipe(p) => (Stdio::from(p.try_clone()?), None),
            Input::Bytes(b) => {
                // A child consumes whatever is left.
                let mut m = b.lock().unwrap();
                let rest = m.data[m.pos..].to_vec();
                m.pos = m.data.len();
                (Stdio::piped(), Some(rest))
            }
        })
    }
}

impl Output {
    pub fn writer(&self) -> Box<dyn Write + Send> {
        match self {
            Output::Stdout => Box::new(io::stdout()),
            Output::Stderr => Box::new(io::stderr()),
            Output::Null => Box::new(io::sink()),
            Output::File(f) => Box::new(io::BufWriter::new(ArcFile(f.clone()))),
            Output::Pipe(p) => Box::new(io::BufWriter::new(ArcPipeW(p.clone()))),
        }
    }

    pub fn stdio(&self) -> io::Result<Stdio> {
        Ok(match self {
            Output::Stdout => Stdio::from(io::stdout()),
            Output::Stderr => Stdio::from(io::stderr()),
            Output::Null => Stdio::null(),
            Output::File(f) => Stdio::from(f.try_clone()?),
            Output::Pipe(p) => Stdio::from(p.try_clone()?),
        })
    }

    pub fn is_terminal(&self) -> bool {
        use std::io::IsTerminal;
        match self {
            Output::Stdout => io::stdout().is_terminal(),
            Output::Stderr => io::stderr().is_terminal(),
            _ => false,
        }
    }
}

struct ArcFile(Arc<File>);
impl Read for ArcFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        check_interrupt()?;
        (&*self.0).read(buf)
    }
}
impl Write for ArcFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.0).write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ArcPipeR(Arc<PipeReader>);
impl Read for ArcPipeR {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            check_interrupt()?;
            match (&*self.0).read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => return r,
            }
        }
    }
}

struct ArcPipeW(Arc<PipeWriter>);
impl Write for ArcPipeW {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.0).write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
fn inherit_stdin() -> Box<dyn Read + Send> {
    // Raw fd 0, so that bytes `read` doesn't consume stay for the next command.
    struct RawStdin;
    impl Read for RawStdin {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                check_interrupt()?;
                let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
                if n >= 0 {
                    return Ok(n as usize);
                }
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }
    Box::new(RawStdin)
}

#[cfg(not(unix))]
fn inherit_stdin() -> Box<dyn Read + Send> {
    // The console needs std's UTF-16 conversion.
    Box::new(io::stdin())
}

/// Set by the Ctrl-C handler; long-running builtins poll it.
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

pub fn clear_interrupt() {
    INTERRUPTED.store(false, Ordering::Relaxed);
}

/// An error once Ctrl-C was pressed. It is deliberately not
/// `ErrorKind::Interrupted`, which std's read loops would silently retry.
pub fn check_interrupt() -> io::Result<()> {
    if interrupted() { Err(io::Error::other("interrupted")) } else { Ok(()) }
}
