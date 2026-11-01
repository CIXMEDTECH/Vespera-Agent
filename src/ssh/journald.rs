//! Follow sshd entries in the systemd journal through `journalctl`.
//!
//! A subprocess is used instead of linking libsystemd so the agent stays a
//! fully static musl binary.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::LogLine;

/// PID of the running `journalctl`, 0 when none.
static CHILD: AtomicU32 = AtomicU32::new(0);

/// Kill and reap the `journalctl` child. Used right before a self-update re-exec.
#[cfg(unix)]
pub fn kill_child() {
    let pid = CHILD.swap(0, Ordering::SeqCst) as libc::pid_t;
    if pid > 0 {
        // SAFETY: plain syscalls on a PID we spawned and have not reaped yet.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    }
}

pub fn available() -> bool {
    Path::new("/run/systemd/journal/socket").exists() && which("journalctl").is_some()
}

pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| p.is_file())
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "MESSAGE")]
    message: Option<serde_json::Value>,
    #[serde(rename = "__REALTIME_TIMESTAMP")]
    realtime: Option<String>,
    #[serde(rename = "__CURSOR")]
    cursor: Option<String>,
}

pub async fn run(state_dir: PathBuf, tx: mpsc::Sender<LogLine>) {
    let cursor_path = state_dir.join("journal.cursor");
    loop {
        let cursor = std::fs::read_to_string(&cursor_path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let mut cmd = Command::new("journalctl");
        cmd.args(["-f", "-o", "json", "--no-pager", "-q"]);
        match &cursor {
            Some(c) => {
                cmd.arg(format!("--after-cursor={c}"));
            }
            None => {
                cmd.args(["-n", "0"]);
            }
        }
        cmd.args(["_COMM=sshd", "_COMM=sshd-session", "_COMM=sshd-auth"]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null()).kill_on_drop(true);

        match cmd.spawn() {
            Ok(mut child) => {
                CHILD.store(child.id().unwrap_or(0), Ordering::SeqCst);
                tracing::info!(resume = cursor.is_some(), "ssh monitor: following journald");
                let stdout = child.stdout.take().expect("piped stdout");
                let mut lines = BufReader::new(stdout).lines();
                let mut last_cursor: Option<String> = None;
                let mut saved_cursor = cursor.clone();
                let mut save_tick = tokio::time::interval(Duration::from_secs(5));
                loop {
                    tokio::select! {
                        line = lines.next_line() => {
                            let Ok(Some(line)) = line else { break };
                            let Ok(e) = serde_json::from_str::<Entry>(&line) else { continue };
                            if let Some(c) = e.cursor { last_cursor = Some(c); }
                            let Some(serde_json::Value::String(msg)) = e.message else { continue };
                            let ts = e.realtime.and_then(|t| t.parse::<i64>().ok()).map(|us| us / 1_000_000)
                                .unwrap_or_else(super::now_secs);
                            if tx.send(LogLine { ts, msg }).await.is_err() { return; }
                        }
                        _ = save_tick.tick() => {
                            if last_cursor.is_some() && last_cursor != saved_cursor {
                                let _ = std::fs::write(&cursor_path, last_cursor.as_deref().unwrap_or_default());
                                saved_cursor = last_cursor.clone();
                            }
                        }
                    }
                }
                if let Some(c) = &last_cursor {
                    let _ = std::fs::write(&cursor_path, c);
                }
                CHILD.store(0, Ordering::SeqCst);
                let _ = child.kill().await;
                tracing::warn!("ssh monitor: journalctl exited, restarting in 10s");
            }
            Err(e) => tracing::warn!("ssh monitor: failed to start journalctl: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
