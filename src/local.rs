//! Local backend: asks T3's own CLI for a thread's state.
//!
//!   t3 thread show --base-dir <dir> --messages 0 --json <threadId>
//!
//! Needs no OAuth because it runs as the same user as the T3 server.

use crate::mcp::CallError;
use crate::model::ThreadState;
use crate::util;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const SHOW_TIMEOUT: Duration = Duration::from_secs(20);

/// Kills the child's whole process group and reaps it. Only call while the
/// child is unreaped, so its pid (and group id) cannot have been reused.
pub fn kill_and_reap(child: &mut Child) {
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

fn transient(message: String) -> CallError {
    CallError::Transient { message, status: None }
}

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
        .map_err(|e| transient(format!("cannot run {}: {e}", t3_bin.display())))?;

    // A watchdog kills a hung `t3` so one stuck call cannot stall the watch.
    // It only runs while the child is unreaped, so the pid cannot be reused.
    let pid = child.id() as libc::pid_t;
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = util::spawn_thread("t3-watchdog", 32 * 1024, move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = done_rx.recv_timeout(SHOW_TIMEOUT) {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    });
    let watchdog = match watchdog {
        Ok(w) => w,
        Err(e) => {
            // Without a watchdog a hung `t3` could stall this watch forever.
            kill_and_reap(&mut child);
            return Err(transient(format!("cannot start watchdog thread: {e}")));
        }
    };

    let mut err_pipe = child.stderr.take();
    let err_reader = util::spawn_thread("t3-stderr", 32 * 1024, move || {
        let mut s = String::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.take(64 * 1024).read_to_string(&mut s);
        }
        s
    });
    let err_reader = match err_reader {
        Ok(h) => h,
        Err(e) => {
            // An unread stderr pipe can fill up and block the child.
            kill_and_reap(&mut child);
            let _ = done_tx.send(());
            let _ = watchdog.join();
            return Err(transient(format!("cannot start stderr reader thread: {e}")));
        }
    };
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = (&mut out).take(8 << 20).read_to_string(&mut stdout);
    }
    let stderr = err_reader.join().unwrap_or_default();
    let _ = done_tx.send(());
    let _ = watchdog.join();
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

    let status = status.map_err(|e| transient(format!("waiting for t3: {e}")))?;
    if !status.success() {
        let msg = util::sanitize_text(stderr.trim(), 300);
        let msg = if msg.is_empty() {
            format!("t3 exited with {status}")
        } else {
            msg
        };
        let lower = msg.to_ascii_lowercase();
        if lower.contains("no longer available") || lower.contains("not found") || lower.contains("does not exist") {
            return Err(CallError::NotFound(msg));
        }
        // Server restarting, upgrade in progress, killed by the watchdog: retry.
        return Err(transient(msg));
    }
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).map_err(|e| transient(format!("t3 printed invalid JSON: {e}")))?;
    let state = ThreadState::from_thread_json(&v, observed_at);
    if state.status.is_none() {
        return Err(transient("t3 output has no thread status".into()));
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// A fake `t3` that records its pid and would otherwise run for a while.
    fn slow_script(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "t3wl-{tag}-{}-{}",
            std::process::id(),
            util::hex(&util::random_bytes(4))
        ));
        fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let bin = dir.join("t3");
        fs::write(
            &bin,
            format!("#!/bin/sh\necho $$ > '{}'\nsleep 30\necho '{{}}'\n", pidfile.display()),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        (bin, pidfile)
    }

    fn assert_gone(pidfile: &Path) {
        // The script may not have written its pid before being killed.
        if let Ok(pid) = fs::read_to_string(pidfile) {
            let pid: libc::pid_t = pid.trim().parse().unwrap();
            assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "child {pid} still exists");
        }
    }

    #[test]
    fn watchdog_spawn_failure_kills_and_reaps_the_child() {
        let (bin, pidfile) = slow_script("wd");
        util::FAIL_SPAWNS.with(|c| c.set((0, 1)));
        let started = Instant::now();
        let err = show(&bin, Path::new("/tmp"), "t").unwrap_err();
        assert!(err.message().contains("watchdog"), "{err:?}");
        assert!(matches!(err, CallError::Transient { .. }));
        assert!(started.elapsed() < Duration::from_secs(5), "child was not killed");
        assert_gone(&pidfile);
    }

    #[test]
    fn stderr_reader_spawn_failure_kills_and_reaps_the_child() {
        let (bin, pidfile) = slow_script("err");
        // The watchdog spawns; the stderr reader does not.
        util::FAIL_SPAWNS.with(|c| c.set((1, 1)));
        let started = Instant::now();
        let err = show(&bin, Path::new("/tmp"), "t").unwrap_err();
        assert!(err.message().contains("stderr"), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "child was not killed");
        assert_gone(&pidfile);
    }
}
