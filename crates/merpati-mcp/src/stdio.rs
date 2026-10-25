//! stdio transport: the server is a child process speaking newline-delimited
//! JSON-RPC on stdin/stdout; stderr lines become log events.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};

use crate::client::{Outbound, Shared};
use crate::error::McpError;
use crate::http::MAX_MESSAGE_BYTES;
use crate::types::McpEvent;

/// What is kept of one stderr line (the rest of a longer line is skipped).
const MAX_STDERR_LINE: usize = 64 * 1024;

/// How [`read_line_capped`] ended.
#[derive(Debug, PartialEq)]
enum LineRead {
    Eof,
    Line,
    /// The line is longer than the limit; `buf` holds its first bytes.
    TooLong,
}

/// Reads up to and including the next `\n` into `buf`, holding at most `max`
/// bytes of it. A longer line returns [`LineRead::TooLong`] right away, or
/// with `skip_rest` once the rest of it has been read and dropped.
async fn read_line_capped<R: AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
    skip_rest: bool,
) -> std::io::Result<LineRead> {
    let mut too_long = false;
    loop {
        let available = r.fill_buf().await?;
        if available.is_empty() {
            return Ok(match (too_long, buf.is_empty()) {
                (true, _) => LineRead::TooLong,
                (false, true) => LineRead::Eof,
                (false, false) => LineRead::Line,
            });
        }
        let (take, done) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if !too_long {
            let room = max.saturating_sub(buf.len());
            buf.extend_from_slice(&available[..take.min(room)]);
            too_long = take > room;
        }
        r.consume(take);
        if done {
            return Ok(if too_long { LineRead::TooLong } else { LineRead::Line });
        }
        if too_long && !skip_rest {
            return Ok(LineRead::TooLong);
        }
    }
}

pub(crate) struct StdioHandle {
    exited: watch::Receiver<bool>,
}

impl StdioHandle {
    /// Waits (bounded) until the process has exited and been reaped.
    pub async fn wait_exit(&self, limit: Duration) {
        let mut rx = self.exited.clone();
        let _ = tokio::time::timeout(limit, rx.wait_for(|done| *done)).await;
    }
}

/// Directories commonly holding `npx`, `uvx`, `node`, ... that are missing
/// from the PATH of GUI apps (launched from Finder / a desktop entry).
fn extra_path_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(unix) {
        for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
            dirs.push(PathBuf::from(d));
        }
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            for d in [".local/bin", ".cargo/bin", ".bun/bin", ".deno/bin", ".volta/bin"] {
                dirs.push(home.join(d));
            }
        }
    }
    dirs
}

/// PATH for the child: the user's override if given, else this process's
/// PATH plus [`extra_path_dirs`].
fn child_path(env: &[(String, String)]) -> (OsString, bool) {
    if let Some((_, v)) = env.iter().find(|(k, _)| k.eq_ignore_ascii_case("PATH")) {
        return (OsString::from(v), false);
    }
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for d in extra_path_dirs() {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    (std::env::join_paths(dirs).unwrap_or_default(), true)
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Finds `command` the way a shell would, including Windows `PATHEXT`
/// (so `npx` finds `npx.cmd`).
fn find_program(command: &str, cwd: Option<&Path>, path: &OsString) -> Option<PathBuf> {
    let has_sep = command.contains('/') || (cfg!(windows) && command.contains('\\'));
    if has_sep {
        let p = PathBuf::from(command);
        return Some(match cwd {
            Some(dir) if p.is_relative() => dir.join(p),
            _ => p,
        });
    }
    let exts: Vec<String> = if cfg!(windows) {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        std::iter::once(String::new()).chain(pathext.split(';').filter(|e| !e.is_empty()).map(str::to_string)).collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(path) {
        for ext in &exts {
            let candidate = dir.join(format!("{command}{ext}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn signal_group(pid: Option<u32>, sig: i32) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok())
        && pid > 0
    {
        // SAFETY: plain syscall; the child leads its own process group.
        unsafe {
            libc::kill(-pid, sig);
        }
    }
}

fn describe_exit(status: std::io::Result<std::process::ExitStatus>, shared: &Shared) -> String {
    let mut reason = match status {
        Ok(s) => match s.code() {
            Some(code) => format!("Server process exited with code {code}"),
            None => format!("Server process terminated ({s})"),
        },
        Err(e) => format!("Server process lost: {e}"),
    };
    let tail = shared.stderr_tail();
    if !tail.is_empty() {
        reason.push_str(". Last stderr output:\n");
        reason.push_str(&tail);
    }
    reason
}

pub(crate) fn start(
    command: &str,
    args: &[String],
    env: &[(String, String)],
    cwd: Option<&Path>,
    shared: &Arc<Shared>,
    mut outbox: mpsc::UnboundedReceiver<Outbound>,
) -> Result<StdioHandle, McpError> {
    if command.trim().is_empty() {
        return Err(McpError::Config("No command given for the stdio server".into()));
    }
    if let Some(dir) = cwd
        && !dir.is_dir()
    {
        return Err(McpError::Config(format!("Working directory {} does not exist", dir.display())));
    }
    let (path, augmented) = child_path(env);
    let program = find_program(command, cwd, &path).unwrap_or_else(|| PathBuf::from(command));
    let mut cmd = Command::new(&program);
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    if augmented {
        cmd.env("PATH", &path);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child: Child = cmd.spawn().map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::NotFound {
            ": program not found. Install it or use an absolute path"
        } else {
            ""
        };
        McpError::Spawn(format!("Could not start \"{command}\": {e}{hint}"))
    })?;
    let pid = child.id();
    let stdin = child.stdin.take().ok_or_else(|| McpError::Spawn("stdin not captured".into()))?;
    let stdout = child.stdout.take().ok_or_else(|| McpError::Spawn("stdout not captured".into()))?;
    let stderr = child.stderr.take().ok_or_else(|| McpError::Spawn("stderr not captured".into()))?;
    let cancel = shared.cancel.clone();

    // Writer: one JSON message per line. Dropping stdin on cancel tells the
    // server to exit.
    {
        let shared = shared.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut stdin = stdin;
            loop {
                let msg = tokio::select! {
                    _ = cancel.cancelled() => break,
                    m = outbox.recv() => match m { Some(m) => m, None => break },
                };
                let mut line = msg.json.into_bytes();
                line.push(b'\n');
                let res = async {
                    stdin.write_all(&line).await?;
                    stdin.flush().await
                }
                .await;
                if let Err(e) = res {
                    // Usually the process died; let the exit watcher report
                    // the exit code and stderr first.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    shared.mark_closed(format!("Writing to the server failed: {e}"));
                    break;
                }
            }
            let _ = stdin.shutdown().await;
        });
    }

    // Reader.
    {
        let shared = shared.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                let n = tokio::select! {
                    _ = cancel.cancelled() => return,
                    n = read_line_capped(&mut reader, &mut buf, MAX_MESSAGE_BYTES, false) => n,
                };
                match n {
                    Ok(LineRead::Eof) | Err(_) => break,
                    Ok(LineRead::TooLong) => {
                        shared.mark_closed(format!(
                            "The server sent a message larger than {} on stdout",
                            crate::sse::size_text(MAX_MESSAGE_BYTES)
                        ));
                        return;
                    }
                    Ok(LineRead::Line) => {
                        let line = String::from_utf8_lossy(&buf);
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        if line.starts_with('{') || line.starts_with('[') {
                            shared.handle_incoming(line);
                        } else {
                            shared.emit(McpEvent::Log { level: "stdout".into(), message: line.to_string() });
                        }
                    }
                }
            }
            // Give the exit-status watcher a moment to report the real cause.
            tokio::time::sleep(Duration::from_millis(500)).await;
            shared.mark_closed("The server closed its stdout");
        });
    }

    // stderr. Keeps draining for a few seconds after the session closed so
    // shutdown messages are not lost (bounded: a grandchild may hold the pipe).
    {
        let shared = shared.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            let mut deadline: Option<tokio::time::Instant> = None;
            loop {
                let n = match deadline {
                    None => tokio::select! {
                        _ = cancel.cancelled() => {
                            deadline = Some(tokio::time::Instant::now() + Duration::from_secs(3));
                            continue;
                        }
                        n = read_line_capped(&mut reader, &mut buf, MAX_STDERR_LINE, true) => n,
                    },
                    Some(d) => {
                        match tokio::time::timeout_at(d, read_line_capped(&mut reader, &mut buf, MAX_STDERR_LINE, true))
                            .await
                        {
                            Ok(n) => n,
                            Err(_) => return,
                        }
                    }
                };
                match n {
                    Ok(LineRead::Eof) | Err(_) => return,
                    Ok(read) => {
                        let mut line = String::from_utf8_lossy(&buf).trim_end().to_string();
                        if read == LineRead::TooLong {
                            line.push_str(" [...]");
                        }
                        buf.clear();
                        shared.push_stderr(&line);
                        shared.emit(McpEvent::Stderr(line));
                    }
                }
            }
        });
    }

    // Exit watcher: owns the child; terminates it once the session closes.
    let (exit_tx, exit_rx) = watch::channel(false);
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            tokio::select! {
                status = child.wait() => {
                    // Let the stderr reader drain the last lines.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    shared.mark_closed(describe_exit(status, &shared));
                    // What the server left running in its group (`npx`
                    // exiting before its `node`) goes with it.
                    #[cfg(unix)]
                    signal_group(pid, libc::SIGKILL);
                }
                _ = cancel.cancelled() => {
                    // stdin is closed by the writer; most servers exit now.
                    if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
                        #[cfg(unix)]
                        {
                            signal_group(pid, libc::SIGTERM);
                            if tokio::time::timeout(Duration::from_secs(1), child.wait()).await.is_err() {
                                signal_group(pid, libc::SIGKILL);
                            }
                        }
                        let _ = child.kill().await;
                    }
                    #[cfg(unix)]
                    signal_group(pid, libc::SIGKILL);
                    #[cfg(not(unix))]
                    let _ = pid;
                }
            }
            let _ = exit_tx.send(true);
        });
    }

    Ok(StdioHandle { exited: exit_rx })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_on_path() {
        let (path, _) = child_path(&[]);
        let sh = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(find_program(sh, None, &path).is_some());
        assert!(find_program("definitely-not-a-real-program-xyz", None, &path).is_none());
        let rel = find_program("./srv", Some(Path::new("/tmp")), &path).unwrap();
        assert_eq!(rel, PathBuf::from("/tmp/./srv"));
    }

    #[tokio::test]
    async fn lines_are_capped() {
        let mut r = BufReader::with_capacity(4, &b"short\nxxxxxxxxxxxxxxxxxxxx\nafter"[..]);
        let mut buf = Vec::new();
        assert_eq!(read_line_capped(&mut r, &mut buf, 8, true).await.unwrap(), LineRead::Line);
        assert_eq!(buf, b"short\n");
        buf.clear();
        // The rest of a long line is skipped.
        assert_eq!(read_line_capped(&mut r, &mut buf, 8, true).await.unwrap(), LineRead::TooLong);
        assert_eq!(buf, b"xxxxxxxx");
        buf.clear();
        assert_eq!(read_line_capped(&mut r, &mut buf, 8, true).await.unwrap(), LineRead::Line);
        assert_eq!(buf, b"after");
        buf.clear();
        assert_eq!(read_line_capped(&mut r, &mut buf, 8, true).await.unwrap(), LineRead::Eof);
        // Without skipping, it returns as soon as the limit is passed.
        let mut r = BufReader::with_capacity(4, &b"0123456789\nnext\n"[..]);
        assert_eq!(read_line_capped(&mut r, &mut buf, 8, false).await.unwrap(), LineRead::TooLong);
        assert_eq!(buf, b"01234567");
        // A line of exactly the limit fits.
        let mut r = BufReader::new(&b"1234567\n"[..]);
        buf.clear();
        assert_eq!(read_line_capped(&mut r, &mut buf, 8, false).await.unwrap(), LineRead::Line);
    }
}
