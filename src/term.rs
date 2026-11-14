//! Platform glue: signals / console control, terminal echo, CPU times.

use std::sync::atomic::Ordering;

use crate::io::INTERRUPTED;

#[cfg(unix)]
pub fn init(interactive: bool) {
    extern "C" fn on_int(_: libc::c_int) {
        INTERRUPTED.store(true, Ordering::Relaxed);
    }
    extern "C" fn ignore(_: libc::c_int) {}
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        // No SA_RESTART: a blocking read returns EINTR so Ctrl-C can stop
        // builtins waiting on the terminal.
        sa.sa_sigaction = on_int as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        if interactive {
            // A handler (not SIG_IGN) so children still get the default action.
            sa.sa_sigaction = ignore as *const () as usize;
            libc::sigaction(libc::SIGQUIT, &sa, std::ptr::null_mut());
            // No job control: keep Ctrl-Z from stopping the shell.
            libc::signal(libc::SIGTSTP, libc::SIG_IGN);
            libc::signal(libc::SIGTTIN, libc::SIG_IGN);
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        }
    }
}

#[cfg(windows)]
pub fn init(_interactive: bool) {
    use windows_sys::Win32::System::Console::*;
    unsafe extern "system" fn on_ctrl(kind: u32) -> windows_sys::core::BOOL {
        if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
            INTERRUPTED.store(true, Ordering::Relaxed);
            1
        } else {
            0
        }
    }
    unsafe {
        SetConsoleCtrlHandler(Some(on_ctrl), 1);
        // UTF-8 for programs writing bytes to the console, and ANSI colors.
        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
        for h in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = GetStdHandle(h);
            let mut mode = 0;
            if GetConsoleMode(handle, &mut mode) != 0 {
                SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
}

/// Turns terminal echo off until dropped (`read -s`).
pub struct NoEcho {
    #[cfg(unix)]
    saved: libc::termios,
    #[cfg(windows)]
    saved: u32,
}

impl NoEcho {
    #[cfg(unix)]
    pub fn new() -> Option<NoEcho> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let saved = t;
            t.c_lflag &= !libc::ECHO;
            libc::tcsetattr(0, libc::TCSANOW, &t);
            Some(NoEcho { saved })
        }
    }

    #[cfg(windows)]
    pub fn new() -> Option<NoEcho> {
        use windows_sys::Win32::System::Console::*;
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode = 0;
            if GetConsoleMode(h, &mut mode) == 0 {
                return None;
            }
            SetConsoleMode(h, mode & !ENABLE_ECHO_INPUT);
            Some(NoEcho { saved: mode })
        }
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.saved);
        }
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Console::*;
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), self.saved);
        }
    }
}

/// (user, system) CPU seconds of this process and its waited-for children.
#[cfg(unix)]
pub fn cpu_times() -> (f64, f64) {
    let mut total = (0.0, 0.0);
    for who in [libc::RUSAGE_SELF, libc::RUSAGE_CHILDREN] {
        unsafe {
            let mut ru: libc::rusage = std::mem::zeroed();
            if libc::getrusage(who, &mut ru) == 0 {
                total.0 += ru.ru_utime.tv_sec as f64 + ru.ru_utime.tv_usec as f64 / 1e6;
                total.1 += ru.ru_stime.tv_sec as f64 + ru.ru_stime.tv_usec as f64 / 1e6;
            }
        }
    }
    total
}

#[cfg(not(unix))]
pub fn cpu_times() -> (f64, f64) {
    (0.0, 0.0)
}
