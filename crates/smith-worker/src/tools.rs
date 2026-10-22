//! The five tools, executed inside the sandbox.
//!
//! Each function resolves its paths through [`Workspace`], applies the caps in this module and
//! returns either the result JSON or a message for the model. Nothing here holds policy: the
//! allowlist and the mounts belong to the host.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use nix::fcntl::OFlag;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use smith_channel::{
    EditFileArgs, EditFileResult, ReadFileArgs, ReadFileResult, RunShellArgs, RunShellResult,
    ToolWaitArgs, WriteFileArgs, WriteFileResult,
};

use crate::cancel::CancelFlag;
use crate::tasks::TaskRegistry;
use crate::workspace::Workspace;

/// How much of a file `read_file` scans to count lines.
const MAX_SCAN_BYTES: u64 = 16 * 1024 * 1024;
/// How much content one `read_file` returns.
const MAX_CONTENT_BYTES: usize = 64 * 1024;
/// How many lines one `read_file` returns.
const MAX_CONTENT_LINES: usize = 2000;
/// How much of a NUL byte search counts as binary.
const BINARY_PROBE_BYTES: usize = 8 * 1024;
/// The cap on each captured stream of a shell command.
const MAX_STREAM_BYTES: usize = 32 * 1024;
const DEFAULT_TIMEOUT_SECS: u32 = 120;
const MAX_TIMEOUT_SECS: u32 = 600;
const DEFAULT_WAIT_SECS: u32 = 120;
const MAX_WAIT_SECS: u32 = 600;
/// A directory report is one path. Anything longer is not a report.
const CWD_REPORT_MAX_BYTES: usize = 4096;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// What a tool needs from the worker.
pub struct ToolContext<'a> {
    pub workspace: &'a Workspace,
    pub env: &'a BTreeMap<String, String>,
    pub cancel: &'a CancelFlag,
    pub tasks: &'a TaskRegistry,
}

/// Run one tool by name.
pub fn execute(name: &str, args: Value, ctx: &ToolContext<'_>) -> Result<String, String> {
    match name {
        "read_file" => read_file(parse(args)?, ctx),
        "write_file" => write_file(parse(args)?, ctx),
        "edit_file" => edit_file(parse(args)?, ctx),
        "run_shell" => run_shell(parse(args)?, ctx),
        "tool_wait" => tool_wait(parse(args)?, ctx),
        other => Err(format!("unknown tool '{other}'")),
    }
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}

fn to_json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| format!("cannot encode the result: {e}"))
}

pub fn read_file(args: ReadFileArgs, ctx: &ToolContext<'_>) -> Result<String, String> {
    let path = ctx.workspace.existing(&args.path)?;
    let metadata = fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if metadata.is_dir() {
        return Err(format!("{} is a directory", path.display()));
    }

    let mut file = File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_SCAN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let scan_truncated = bytes.len() as u64 > MAX_SCAN_BYTES;
    bytes.truncate(MAX_SCAN_BYTES as usize);

    if bytes.iter().take(BINARY_PROBE_BYTES).any(|byte| *byte == 0) {
        return Err(format!("{} is a binary file", path.display()));
    }

    let text = String::from_utf8_lossy(&bytes).into_owned();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let total_lines = lines.len() as u32;

    let start = args.start_line.unwrap_or(1).max(1);
    let end = args.end_line.unwrap_or(total_lines).min(total_lines);
    let mut content = String::new();
    let mut last = 0u32;
    let mut truncated = scan_truncated;

    if start <= end {
        for (index, line) in lines.iter().enumerate().skip((start - 1) as usize) {
            let number = index as u32 + 1;
            if number > end {
                break;
            }
            if number - start >= MAX_CONTENT_LINES as u32 {
                truncated = true;
                break;
            }
            if content.len() + line.len() > MAX_CONTENT_BYTES {
                if content.is_empty() {
                    content.push_str(take_prefix(line, MAX_CONTENT_BYTES));
                    last = number;
                }
                truncated = true;
                break;
            }
            content.push_str(line);
            last = number;
        }
        if last < end {
            truncated = true;
        }
    }

    let (start_line, end_line) = if content.is_empty() {
        (0, 0)
    } else {
        (start, last)
    };

    to_json(&ReadFileResult {
        path: path.display().to_string(),
        start_line,
        end_line,
        total_lines,
        truncated,
        content,
    })
}

pub fn write_file(args: WriteFileArgs, ctx: &ToolContext<'_>) -> Result<String, String> {
    let path = ctx.workspace.writable(&args.path)?;
    if path.is_dir() {
        return Err(format!("{} is a directory", path.display()));
    }
    create_parent(&path)?;
    atomic_write(&path, args.content.as_bytes())?;
    to_json(&WriteFileResult {
        path: path.display().to_string(),
        bytes: args.content.len() as u64,
    })
}

pub fn edit_file(args: EditFileArgs, ctx: &ToolContext<'_>) -> Result<String, String> {
    if args.old_string.is_empty() {
        return Err("old_string is empty".to_string());
    }
    let path = ctx.workspace.existing(&args.path)?;
    let metadata = fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if metadata.is_dir() {
        return Err(format!("{} is a directory", path.display()));
    }
    if metadata.len() > MAX_SCAN_BYTES {
        return Err(format!("{} is too large to edit", path.display()));
    }

    let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text =
        String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8 text", path.display()))?;

    let matches = text.matches(&args.old_string).count();
    if matches == 0 {
        return Err(format!("old_string does not occur in {}", path.display()));
    }
    if matches > 1 && !args.replace_all {
        return Err(format!(
            "old_string occurs {matches} times in {}; pass replace_all or use a longer string",
            path.display()
        ));
    }

    let updated = if args.replace_all {
        text.replace(&args.old_string, &args.new_string)
    } else {
        text.replacen(&args.old_string, &args.new_string, 1)
    };
    atomic_write(&path, updated.as_bytes())?;

    to_json(&EditFileResult {
        path: path.display().to_string(),
        replacements: if args.replace_all { matches as u32 } else { 1 },
    })
}

pub fn run_shell(args: RunShellArgs, ctx: &ToolContext<'_>) -> Result<String, String> {
    if args.command.trim().is_empty() {
        return Err("command is empty".to_string());
    }

    let cwd = match args.cwd.as_deref() {
        Some(path) => {
            let resolved = ctx.workspace.existing(path)?;
            if !resolved.is_dir() {
                return Err(format!("{} is not a directory", resolved.display()));
            }
            resolved
        }
        None => ctx.workspace.root().to_path_buf(),
    };

    let start_cwd = cwd.display().to_string();
    if ctx.cancel.is_cancelled() {
        return to_json(&RunShellResult {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            cancelled: true,
            task_id: None,
            running: false,
            cwd: Some(start_cwd),
            cwd_error: None,
        });
    }

    let mut command = Command::new("/bin/sh");
    command
        .current_dir(&cwd)
        .env_clear()
        .envs(ctx.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A separate group lets a timeout, a cancel or a task kill reach grandchildren.
        .process_group(0);

    if args.background {
        // A background run has no timeout. The model stops it with tool_wait and kill. Its
        // final directory is not reported; a background `cd` stays in that task.
        command.arg("-c").arg(&args.command);
        let task_id = ctx.tasks.start(command)?;
        return to_json(&RunShellResult {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            cancelled: false,
            task_id: Some(task_id),
            running: true,
            cwd: Some(start_cwd),
            cwd_error: None,
        });
    }

    // The trap reports the shell's final physical directory. The file is untrusted: a model can
    // write it directly, so the worker and host check the path before it is used.
    let cwd_file = std::env::temp_dir().join(format!(
        "smith-cwd-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let wrapped = format!(
        "trap 'pwd -P > \"$SMITH_CWD_FILE\" 2>/dev/null' EXIT\n{}",
        args.command
    );
    command
        .arg("-c")
        .arg(&wrapped)
        .env("SMITH_CWD_FILE", &cwd_file);

    let timeout = Duration::from_secs(
        args.timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS) as u64,
    );

    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start a shell: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "the child has no stdout pipe".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "the child has no stderr pipe".to_string())?;
    let out_reader = thread::spawn(move || read_capped(stdout, MAX_STREAM_BYTES));
    let err_reader = thread::spawn(move || read_capped(stderr, MAX_STREAM_BYTES));

    let deadline = Instant::now() + timeout;
    let id = child.id();
    let mut cancelled = false;
    let mut timed_out = false;
    let mut killed = false;

    let status = loop {
        match child
            .try_wait()
            .map_err(|e| format!("waiting for the shell failed: {e}"))?
        {
            Some(status) => break status,
            None => {
                if !killed {
                    cancelled = ctx.cancel.is_cancelled();
                    timed_out = !cancelled && Instant::now() >= deadline;
                    if cancelled || timed_out {
                        killed = true;
                        let _ = killpg(Pid::from_raw(id as i32), Signal::SIGKILL);
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    };

    let (stdout, out_truncated) = out_reader.join().unwrap_or_default();
    let (stderr, err_truncated) = err_reader.join().unwrap_or_default();

    let mut cwd_after = start_cwd;
    let mut cwd_error = None;
    if !timed_out && !cancelled {
        if let Some(text) = read_cwd_report(&cwd_file) {
            let reported = text.trim();
            if !reported.is_empty() {
                match fs::canonicalize(reported) {
                    Ok(path) if !path.starts_with(ctx.workspace.root()) => {
                        cwd_error = Some(format!("{} is outside the workspace", path.display()));
                    }
                    Ok(path) if !path.is_dir() => {
                        cwd_error = Some(format!("{} is not a directory", path.display()));
                    }
                    Ok(path) => cwd_after = path.display().to_string(),
                    Err(e) => {
                        cwd_error = Some(format!("cannot resolve the reported directory: {e}"))
                    }
                }
            }
        }
    }
    let _ = fs::remove_file(&cwd_file);

    to_json(&RunShellResult {
        exit_code: status.code(),
        stdout,
        stderr,
        truncated: out_truncated || err_truncated,
        timed_out,
        cancelled,
        task_id: None,
        running: false,
        cwd: Some(cwd_after),
        cwd_error,
    })
}

/// Read the shell's directory report.
///
/// The file is model-writable, so it is opened without following a symlink and without blocking
/// on a FIFO, and at most one path is read. A non-regular file, a symlink or an oversized file
/// yields no report.
fn read_cwd_report(path: &Path) -> Option<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take((CWD_REPORT_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > CWD_REPORT_MAX_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

pub fn tool_wait(args: ToolWaitArgs, ctx: &ToolContext<'_>) -> Result<String, String> {
    let timeout = Duration::from_secs(
        args.timeout_secs
            .unwrap_or(DEFAULT_WAIT_SECS)
            .clamp(1, MAX_WAIT_SECS) as u64,
    );
    let result = ctx.tasks.wait(&args.id, timeout, args.kill, ctx.cancel)?;
    to_json(&result)
}

fn create_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))
}

/// Write through a temporary file in the same directory, so an interrupted write does not
/// truncate the previous content.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    let temp = parent.join(format!(
        ".{}.smith-{}-{}",
        name.to_string_lossy(),
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));

    fs::write(&temp, bytes).map_err(|e| format!("{}: {e}", temp.display()))?;
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&temp);
            Err(format!("{}: {e}", path.display()))
        }
    }
}

/// Read a stream into at most `cap` bytes, draining the rest so the child never blocks.
fn read_capped<R: Read>(mut reader: R, cap: usize) -> (String, bool) {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                let take = n.min(room);
                kept.extend_from_slice(&buffer[..take]);
                if take < n {
                    truncated = true;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&kept).into_owned(), truncated)
}

/// Take at most `max` bytes of `text` without splitting a UTF-8 sequence.
fn take_prefix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;

    #[test]
    fn the_directory_report_reader_ignores_unsafe_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cwd");
        fs::write(&path, "/tmp").unwrap();
        assert_eq!(read_cwd_report(&path).as_deref(), Some("/tmp"));

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(read_cwd_report(&link), None);

        let fifo = dir.path().join("fifo");
        mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        assert_eq!(read_cwd_report(&fifo), None);

        let big = dir.path().join("big");
        fs::write(&big, vec![b'x'; CWD_REPORT_MAX_BYTES + 1]).unwrap();
        assert_eq!(read_cwd_report(&big), None);

        let full = dir.path().join("full");
        fs::write(&full, vec![b'x'; CWD_REPORT_MAX_BYTES]).unwrap();
        assert_eq!(
            read_cwd_report(&full).map(|text| text.len()),
            Some(CWD_REPORT_MAX_BYTES)
        );
    }
}
