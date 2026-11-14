//! Background jobs (`cmd &`). Each job is a thread running a copy of the
//! shell; there is no terminal job control (no fg/bg, no Ctrl-Z).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

pub struct Job {
    pub id: usize,
    pub cmdline: String,
    pub done: Arc<AtomicBool>,
    pub handle: Option<JoinHandle<i32>>,
    pub status: Option<i32>,
    pub reported: bool,
}

#[derive(Default)]
pub struct Jobs {
    pub list: Vec<Job>,
}

impl Jobs {
    pub fn add(&mut self, cmdline: String, done: Arc<AtomicBool>, handle: JoinHandle<i32>) -> usize {
        let id = self.list.iter().map(|j| j.id).max().unwrap_or(0) + 1;
        self.list.push(Job { id, cmdline, done, handle: Some(handle), status: None, reported: false });
        id
    }

    /// Collect finished jobs' statuses without blocking.
    pub fn reap(&mut self) {
        for j in &mut self.list {
            if j.status.is_none()
                && j.done.load(Ordering::Acquire)
                && let Some(h) = j.handle.take()
            {
                j.status = Some(h.join().unwrap_or(1));
            }
        }
    }

    /// Lines like `[1]  + done       sleep 3` for jobs that finished since the
    /// last call; they are then dropped from the table.
    pub fn take_finished(&mut self) -> Vec<String> {
        self.reap();
        let mut out = Vec::new();
        for j in &mut self.list {
            if let Some(s) = j.status
                && !j.reported
            {
                j.reported = true;
                let what = if s == 0 { "done".to_string() } else { format!("exit {s}") };
                out.push(format!("[{}]    {:<10} {}", j.id, what, j.cmdline));
            }
        }
        self.list.retain(|j| !j.reported);
        out
    }

    /// Remove a job and hand back its thread handle (for `wait`).
    pub fn take(&mut self, id: usize) -> Option<Job> {
        let i = self.list.iter().position(|j| j.id == id)?;
        Some(self.list.remove(i))
    }
}
