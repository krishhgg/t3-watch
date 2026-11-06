//! Local backend: asks T3's own CLI for a thread's state.
//!
//!   t3 thread show --base-dir <dir> --messages 0 --json <threadId>
//!
//! Needs no OAuth because it runs as the same user as the T3 server.

use crate::mcp::CallError;
use crate::model::ThreadState;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const SHOW_TIMEOUT: Duration = Duration::from_secs(20);

pub fn show(t3_bin: &Path, base_dir: &Path, thread_id: &str) -> Result<ThreadState, CallError> {
    let observed_at = Instant::now();
    let mut child = Command::new(t3_bin)
        .args(["thread", "show", "--base-dir"])
        .arg(base_dir)
        .args(["--messages", "0", "--json", thread_id])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so the watchdog can kill any helpers it
        // started that would otherwise keep our pipes open.
        .process_group(0)
        .spawn()
        // A T3 update can swap the binary out from under us, and fork can
        // fail with EAGAIN: both are worth retrying, not waking anyone for.
        .map_err(|e| CallError::Transient {
            message: format!("cannot run {}: {e}", t3_bin.display()),
            status: None,
        })?;

    // A watchdog kills a hung `t3` so one stuck call cannot stall the watch.
    // It only runs while the child is unreaped, so the pid cannot be reused.
    let pid = child.id() as libc::pid_t;
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = thread::Builder::new()
        .stack_size(32 * 1024)
        .spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = done_rx.recv_timeout(SHOW_TIMEOUT) {
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
        })
        .ok();

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut err_pipe = child.stderr.take();
    let err_reader = thread::Builder::new().stack_size(32 * 1024).spawn(move || {
        let mut s = String::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.take(64 * 1024).read_to_string(&mut s);
        }
        s
    });
    if let Some(mut out) = child.stdout.take() {
        let _ = (&mut out).take(8 << 20).read_to_string(&mut stdout);
    }
    if let Ok(h) = err_reader {
        stderr = h.join().unwrap_or_default();
    }
    let _ = done_tx.send(());
    if let Some(w) = watchdog {
        let _ = w.join();
    }
    // Both pipes are closed, so the child is exiting. Reap it, with a bound.
    let reap_deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) if Instant::now() < reap_deadline => thread::sleep(Duration::from_millis(2)),
            Ok(None) => {
                let _ = child.kill();
                break child.wait();
            }
            Err(e) => break Err(e),
        }
    };

    let status = status.map_err(|e| CallError::Transient {
        message: format!("waiting for t3: {e}"),
        status: None,
    })?;
    if !status.success() {
        let msg = stderr.trim();
        let msg = if msg.is_empty() {
            format!("t3 exited with {status}")
        } else {
            msg.chars().take(300).collect()
        };
        let lower = msg.to_ascii_lowercase();
        if lower.contains("no longer available") || lower.contains("not found") || lower.contains("does not exist") {
            return Err(CallError::NotFound(msg));
        }
        // Server restarting, upgrade in progress, killed by the watchdog: retry.
        return Err(CallError::Transient {
            message: msg,
            status: None,
        });
    }
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|e| CallError::Transient {
        message: format!("t3 printed invalid JSON: {e}"),
        status: None,
    })?;
    let state = ThreadState::from_thread_json(&v, observed_at);
    if state.status.is_none() {
        return Err(CallError::Transient {
            message: "t3 output has no thread status".into(),
            status: None,
        });
    }
    Ok(state)
}
