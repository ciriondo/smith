//! Background shell tasks.
//!
//! `run_shell` with `background: true` starts a command and returns a task id. The command's
//! output is spooled to files in `/tmp`; `tool_wait` reads deltas from those files, reports the
//! status and can stop the process group. The registry is owned by the worker process and
//! bounded, because the container is the only lifetime bound for a task.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use smith_channel::ToolWaitResult;

use crate::cancel::CancelFlag;

/// How many tasks the registry keeps. Starting a new one prunes finished tasks first.
const TASK_CAPACITY: usize = 16;
/// How much output each stream keeps. The reader drains the rest to keep the child moving.
const SPOOL_CAP_BYTES: u64 = 1024 * 1024;
/// How much of each stream one wait returns.
const WAIT_CHUNK_BYTES: usize = 32 * 1024;
/// How often a wait polls a task and how long a kill waits at each signal.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const KILL_GRACE: Duration = Duration::from_secs(2);

struct TaskState {
    running: bool,
    exit_code: Option<i32>,
    signal: Option<i32>,
    stdout_read: u64,
    stderr_read: u64,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

impl TaskState {
    fn new() -> Self {
        Self {
            running: true,
            exit_code: None,
            signal: None,
            stdout_read: 0,
            stderr_read: 0,
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }
}

struct Task {
    id: String,
    pid: i32,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    state: Mutex<TaskState>,
}

impl Task {
    fn is_running(&self) -> bool {
        self.state.lock().unwrap().running
    }

    fn signal_group(&self, signal: Signal) {
        let _ = killpg(Pid::from_raw(self.pid), signal);
    }

    /// Read the bytes produced since the previous wait. The offset advances only past UTF-8
    /// boundaries, so a codepoint split by the chunk cap is returned whole by the next wait.
    fn read_delta(&self) -> Result<(String, String, bool, bool), String> {
        let (out_offset, err_offset, out_truncated, err_truncated) = {
            let state = self.state.lock().unwrap();
            (
                state.stdout_read,
                state.stderr_read,
                state.stdout_truncated,
                state.stderr_truncated,
            )
        };

        let (stdout, out_more, out_next) = read_stream(&self.stdout_path, out_offset)?;
        let (stderr, err_more, err_next) = read_stream(&self.stderr_path, err_offset)?;

        {
            let mut state = self.state.lock().unwrap();
            state.stdout_read = out_next;
            state.stderr_read = err_next;
        }

        Ok((
            stdout,
            stderr,
            out_truncated || err_truncated,
            out_more || err_more,
        ))
    }

    fn collect(&self, timed_out: bool, killed: bool) -> Result<ToolWaitResult, String> {
        // A task can finish between the wait loop and the read. Read again when that happens,
        // so the final bytes are part of this result and not stranded behind a `running:
        // false` status.
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut truncated = false;
        let mut has_more;
        loop {
            let was_running = self.is_running();
            let (out, err, lost, more) = self.read_delta()?;
            stdout.push_str(&out);
            stderr.push_str(&err);
            truncated |= lost;
            has_more = more;
            if was_running && !self.is_running() {
                continue;
            }
            break;
        }

        let state = self.state.lock().unwrap();
        Ok(ToolWaitResult {
            id: self.id.clone(),
            running: state.running,
            exit_code: state.exit_code,
            signal: state.signal,
            stdout,
            stderr,
            truncated,
            has_more,
            timed_out,
            killed,
        })
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.stdout_path);
        let _ = fs::remove_file(&self.stderr_path);
    }
}

/// The tasks started by one worker process.
pub struct TaskRegistry {
    tasks: Mutex<Vec<Arc<Task>>>,
    next: AtomicU64,
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self {
            tasks: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
        }
    }
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a command and return its task id. The caller supplies env, cwd and process group.
    pub fn start(&self, mut command: Command) -> Result<String, String> {
        let id = format!("t{}", self.next.fetch_add(1, Ordering::SeqCst));

        let mut tasks = self.tasks.lock().unwrap();
        if tasks.len() >= TASK_CAPACITY {
            tasks.retain(|task| task.is_running());
            if tasks.len() >= TASK_CAPACITY {
                return Err(format!(
                    "too many background runs (max {TASK_CAPACITY}); wait on or kill one first"
                ));
            }
        }

        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot start a shell: {e}"))?;
        let pid = child.id() as i32;

        let (stdout, stderr) = match (child.stdout.take(), child.stderr.take()) {
            (Some(stdout), Some(stderr)) => (stdout, stderr),
            _ => {
                let _ = child.kill();
                return Err("the background shell has no output pipes".to_string());
            }
        };

        let stdout_path = spool_path(pid, &id, "out");
        let stderr_path = spool_path(pid, &id, "err");
        let (stdout_file, stderr_file) =
            match (create_spool(&stdout_path), create_spool(&stderr_path)) {
                (Ok(stdout), Ok(stderr)) => (stdout, stderr),
                (stdout, stderr) => {
                    let _ = child.kill();
                    let _ = stdout;
                    let _ = stderr;
                    let _ = fs::remove_file(&stdout_path);
                    let _ = fs::remove_file(&stderr_path);
                    return Err("cannot create a background output file".to_string());
                }
            };

        let task = Arc::new(Task {
            id: id.clone(),
            pid,
            stdout_path,
            stderr_path,
            state: Mutex::new(TaskState::new()),
        });
        tasks.push(task.clone());
        drop(tasks);

        let out_task = task.clone();
        let out_reader = thread::spawn(move || {
            spool(
                stdout,
                stdout_file,
                SPOOL_CAP_BYTES,
                out_task,
                Stream::Stdout,
            );
        });
        let err_task = task.clone();
        let err_reader = thread::spawn(move || {
            spool(
                stderr,
                stderr_file,
                SPOOL_CAP_BYTES,
                err_task,
                Stream::Stderr,
            );
        });
        thread::spawn(move || reap(child, task, out_reader, err_reader));

        Ok(id)
    }

    /// Wait for a task, returning output produced since the previous wait.
    pub fn wait(
        &self,
        id: &str,
        timeout: Duration,
        kill: bool,
        cancel: &CancelFlag,
    ) -> Result<ToolWaitResult, String> {
        let task = self
            .find(id)
            .ok_or_else(|| format!("unknown background run '{id}'"))?;

        let mut killed = false;
        if kill && task.is_running() {
            killed = true;
            task.signal_group(Signal::SIGTERM);
            let grace = Instant::now() + KILL_GRACE;
            while task.is_running() && Instant::now() < grace {
                if cancel.is_cancelled() {
                    return Err("cancelled".to_string());
                }
                thread::sleep(POLL_INTERVAL);
            }
            if task.is_running() {
                task.signal_group(Signal::SIGKILL);
                let hard = Instant::now() + KILL_GRACE;
                while task.is_running() && Instant::now() < hard {
                    thread::sleep(POLL_INTERVAL);
                }
            }
        }

        let deadline = Instant::now() + timeout;
        let mut timed_out = false;
        while task.is_running() {
            if cancel.is_cancelled() {
                return Err("cancelled".to_string());
            }
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }

        task.collect(timed_out, killed)
    }

    /// Signal every running task. Used when the worker is shutting down.
    pub fn shutdown(&self) {
        let tasks: Vec<Arc<Task>> = self.tasks.lock().unwrap().drain(..).collect();
        for task in &tasks {
            task.signal_group(Signal::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        for task in &tasks {
            while task.is_running() && Instant::now() < deadline {
                thread::sleep(POLL_INTERVAL);
            }
        }
    }

    fn find(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .find(|task| task.id == id)
            .cloned()
    }
}

impl Drop for TaskRegistry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

fn spool_path(pid: i32, id: &str, stream: &str) -> PathBuf {
    std::env::temp_dir().join(format!("smith-task-{pid}-{id}.{stream}"))
}

fn create_spool(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .read(true)
        .open(path)
}

/// Drain a pipe into a spool file, keeping at most `cap` bytes. Reading continues after the cap
/// so the child never blocks on a full pipe.
fn spool(mut pipe: impl Read, mut file: File, cap: u64, task: Arc<Task>, stream: Stream) {
    let mut total = 0u64;
    let mut buffer = [0u8; 8192];
    loop {
        let read = match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };

        let keep = (cap.saturating_sub(total)).min(read as u64) as usize;
        if keep > 0 {
            if file.write_all(&buffer[..keep]).is_err() {
                mark_truncated(&task, stream);
            }
            total += keep as u64;
        }
        if keep < read {
            mark_truncated(&task, stream);
        }
    }
}

fn mark_truncated(task: &Task, stream: Stream) {
    let mut state = task.state.lock().unwrap();
    match stream {
        Stream::Stdout => state.stdout_truncated = true,
        Stream::Stderr => state.stderr_truncated = true,
    }
}

/// Reap the child after its readers finish, so a finished task never loses its last bytes.
fn reap(
    mut child: Child,
    task: Arc<Task>,
    stdout_reader: thread::JoinHandle<()>,
    stderr_reader: thread::JoinHandle<()>,
) {
    let status = child.wait();
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();

    let mut state = task.state.lock().unwrap();
    state.running = false;
    if let Ok(status) = status {
        state.exit_code = status.code();
        state.signal = status.signal();
    }
}

/// Read up to one chunk from a spool file, never cutting a UTF-8 codepoint.
fn read_stream(path: &Path, offset: u64) -> Result<(String, bool, u64), String> {
    let file = File::open(path).map_err(|e| format!("cannot read task output: {e}"))?;
    let length = file
        .metadata()
        .map_err(|e| format!("cannot measure task output: {e}"))?
        .len();

    let mut buffer = vec![0u8; WAIT_CHUNK_BYTES];
    let read = file
        .read_at(&mut buffer, offset)
        .map_err(|e| format!("cannot read task output: {e}"))?;
    let read = utf8_prefix(&buffer[..read]);
    buffer.truncate(read);

    let next = offset + read as u64;
    Ok((
        String::from_utf8_lossy(&buffer).into_owned(),
        next < length,
        next,
    ))
}

/// Return the largest prefix that does not end inside a UTF-8 codepoint. If the chunk starts
/// with invalid bytes, return the whole chunk so the reader cannot stall.
fn utf8_prefix(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) if e.valid_up_to() > 0 => e.valid_up_to(),
        Err(_) => bytes.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cancelled() -> CancelFlag {
        CancelFlag::new()
    }

    #[test]
    fn utf8_prefix_keeps_a_split_codepoint_for_the_next_read() {
        let text = "aé".as_bytes();
        // Cut inside the two-byte é.
        let cut = utf8_prefix(&text[..2]);
        assert_eq!(cut, 1);
        assert_eq!(utf8_prefix(text), 3);
    }

    #[test]
    fn unknown_task_is_an_error() {
        let registry = TaskRegistry::new();
        let error = registry
            .wait("t99", Duration::from_millis(10), false, &cancelled())
            .unwrap_err();
        assert!(error.contains("unknown"), "{error}");
    }
}
