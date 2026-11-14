//! vsh: a small zsh-flavoured shell with builtin coreutils.

mod arith;
mod ast;
mod builtins;
mod cond;
mod exec;
mod expand;
mod fmt;
mod glob;
mod io;
mod jobs;
mod parser;
mod path;
mod repl;
mod shell;
mod term;
mod utils;

use std::io::{IsTerminal, Read};

use exec::{Flow, flow_status};
use io::IoSet;
use shell::Shell;

const USAGE: &str = "usage: vsh [-eux] [-c command [name [args...]]] [script [args...]]
  -c CMD   run CMD and exit
  -i       force interactive mode
  -f       don't read ~/.vshrc
  -e -u -x set errexit / nounset / xtrace
  -o NAME  set an option";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut sh = Shell::new();
    let mut command = None;
    let mut force_interactive = false;
    let mut no_rc = false;
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--version" => {
                println!("vsh {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            "--" => {
                i += 1;
                break;
            }
            "-c" => {
                i += 1;
                match args.get(i) {
                    Some(c) => command = Some(c.clone()),
                    None => {
                        eprintln!("vsh: -c: option requires an argument");
                        std::process::exit(2);
                    }
                }
            }
            "-o" => {
                i += 1;
                if let Some(n) = args.get(i)
                    && !sh.opts.set(n, true)
                {
                    eprintln!("vsh: no such option: {n}");
                    std::process::exit(2);
                }
            }
            s if s.starts_with('-') && s.len() > 1 => {
                for c in s[1..].chars() {
                    match c {
                        'i' => force_interactive = true,
                        'f' => no_rc = true,
                        'l' | 's' => {}
                        'e' => sh.opts.errexit = true,
                        'u' => sh.opts.nounset = true,
                        'x' => sh.opts.xtrace = true,
                        _ => {
                            eprintln!("vsh: bad option: -{c}\n{USAGE}");
                            std::process::exit(2);
                        }
                    }
                }
            }
            _ => break,
        }
        i += 1;
    }
    let rest = &args[i.min(args.len())..];
    let io = IoSet::default();

    let status = if let Some(cmd) = command {
        if let Some((a0, more)) = rest.split_first() {
            sh.arg0 = a0.clone();
            sh.args = more.to_vec();
        }
        term::init(false);
        run(&mut sh, |sh| sh.run_source(&cmd, &io))
    } else if let Some((script, more)) = rest.split_first() {
        sh.arg0 = script.clone();
        term::init(false);
        let path = utils::resolve(&sh.cwd, script);
        run(&mut sh, |sh| sh.source_file(&path, Some(more.to_vec()), &io))
    } else if force_interactive || (std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        sh.opts.interactive = true;
        term::init(true);
        if !no_rc && let Some(home) = sh.home() {
            let rc = home.join(".vshrc");
            if rc.is_file() {
                let _ = sh.source_file(&rc, None, &io);
            }
        }
        let status = repl::run(&mut sh);
        finish(&mut sh, status)
    } else {
        term::init(false);
        let mut src = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut src) {
            eprintln!("vsh: stdin: {e}");
            std::process::exit(1);
        }
        run(&mut sh, |sh| sh.run_source(&src, &io))
    };
    std::process::exit(status);
}

fn run(sh: &mut Shell, f: impl FnOnce(&mut Shell) -> exec::Exec) -> i32 {
    let status = match f(sh) {
        Ok(s) | Err(Flow::Exit(s)) | Err(Flow::Return(s)) => s,
        Err(_) => sh.status,
    };
    finish(sh, status)
}

/// Run the EXIT trap and wait for background jobs.
fn finish(sh: &mut Shell, status: i32) -> i32 {
    if let Some(trap) = sh.exit_trap.take() {
        sh.status = status;
        let _ = flow_status(sh.run_source(&trap, &IoSet::default()));
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());
    status
}
