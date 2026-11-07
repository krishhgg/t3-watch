//! Regressions for the sixth review (Sol, ordinal 6): a host that sleeps
//! must neither stretch `--timeout` nor leave a long-poll stalled across the
//! sleep holding back a completion.
//!
//! Real suspends cannot happen in a test. `T3_WATCH_TEST_SUSPEND_FILE`
//! adds the number of milliseconds in a file to t3-watch's suspend-inclusive
//! clock, which is what a resume looks like to it: that clock jumps ahead
//! while the awake clock does not.

mod common;

use common::*;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const T: &str = "mcp:launch:sleepy";

fn mcp_env(mock: &MockMcp) -> Env {
    let env = Env::new();
    env.add_machine("mock", &mock.url, TOKEN);
    env
}

fn suspend(file: &Path, ms: u64) {
    fs::write(file, ms.to_string()).unwrap();
}

#[test]
fn a_suspend_longer_than_the_timeout_ends_the_wait_at_once() {
    for daemon in [false, true] {
        let mock = MockMcp::start();
        mock.set_thread(T, "running", 2);
        let env = mcp_env(&mock);
        let clock = env.home.join("suspend");
        suspend(&clock, 0);
        let mut args = vec!["wait", "--timeout", "10m"];
        if !daemon {
            args.push("--no-daemon");
        }
        args.push(T);
        let child = env
            .cmd(&args)
            .env("T3_WATCH_TEST_SUSPEND_FILE", &clock)
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        // The host "slept" for 11 minutes.
        let resumed = Instant::now();
        suspend(&clock, 11 * 60 * 1000);
        let (code, out, err) = finish(child, Duration::from_secs(20));
        assert_eq!(code, 3, "daemon={daemon}: {err}");
        assert!(
            resumed.elapsed() < Duration::from_secs(5),
            "daemon={daemon}: the expired timeout took {:?} to end the wait",
            resumed.elapsed()
        );
        assert!(out["elapsedMs"].as_u64().unwrap() >= 600_000, "{out}");
        assert!(out["elapsedAwakeMs"].as_u64().unwrap() < 60_000, "{out}");
        assert!(out["suspendedMs"].as_u64().unwrap() >= 600_000, "{out}");
        assert!(out["clock"].as_str().unwrap().contains("suspended"), "{out}");
        for k in ["startedAt", "endedAt", "observationEndedAt", "exitAt"] {
            assert!(out[k].as_str().is_some_and(|t| t.ends_with('Z')), "{k}: {out}");
        }
    }
}

#[test]
fn a_long_poll_stalled_across_a_suspend_is_restarted_on_resume() {
    for daemon in [false, true] {
        let mock = MockMcp::start();
        mock.set_thread(T, "running", 2);
        // The first long-poll's connection "dies" during the sleep: it never
        // answers.
        mock.with(|s| s.hang_waits = 1);
        let hook = MockHook::start();
        let env = mcp_env(&mock);
        let clock = env.home.join("suspend");
        suspend(&clock, 0);
        let mut args = vec!["wait", "--timeout", "10m", "--webhook-url", &hook.url];
        if !daemon {
            args.push("--no-daemon");
        }
        args.push(T);
        let child = env
            .cmd(&args)
            .env("T3_WATCH_TEST_SUSPEND_FILE", &clock)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while mock.calls("hung-wait") == 0 {
            assert!(Instant::now() < deadline, "the first long-poll never started");
            std::thread::sleep(Duration::from_millis(50));
        }
        // The run completes while the host sleeps for 5 minutes.
        mock.set_thread(T, "completed", 2);
        let resumed = Instant::now();
        suspend(&clock, 5 * 60 * 1000);
        let (code, out, err) = finish(child, Duration::from_secs(30));
        assert_eq!(code, 0, "daemon={daemon}: {err}");
        assert!(
            resumed.elapsed() < Duration::from_secs(5),
            "daemon={daemon}: completion seen {:?} after resume (the stalled poll was not restarted)",
            resumed.elapsed()
        );
        assert_eq!(out["outcome"], "completed");
        assert_eq!(hook.requests().len(), 1, "the settled event went out");
        let trace = fs::read_to_string(env.home.join("trace.jsonl")).unwrap();
        assert!(trace.contains("\"kind\":\"watch.restart\""), "{trace}");
    }
}

// ---- Sol ordinal 7 -----------------------------------------------------------

const W: &str = "mcp:launch:stale";
const L: &str = "lt-stale";

fn run_id(thread: &str, n: u64) -> String {
    format!("run:thread:{thread}:ordinal:{n}")
}

fn settled_hooks(hook: &MockHook) -> usize {
    hook.requests()
        .iter()
        .filter(|r| r.headers.get("x-t3-watch-event").map(String::as_str) == Some("settled"))
        .count()
}

#[test]
fn an_answer_from_before_a_suspend_never_completes_the_wait() {
    // Sol's repro: the workstream cycle's final header read answers with a
    // snapshot of completed run 2 taken before the host slept 5 s; by the
    // time it arrives the parent is running run 3. With `--timeout 1s` the
    // only right outcome is timeout; with time left, run 3.
    for (daemon, timeout) in [(false, "1s"), (true, "1s"), (false, "60s"), (true, "60s")] {
        let mock = MockMcp::start();
        mock.set_thread(W, "completed", 2);
        let hook = MockHook::start();
        let env = mcp_env(&mock);
        let clock = env.home.join("suspend");
        suspend(&clock, 0);
        // Read 1 primes the watch, read 2 follows the first long-poll.
        mock.with(|s| {
            s.after_read.insert(2, (W.into(), "running".into(), 3));
            s.suspend_after_read.insert(2, (clock.clone(), 5_000));
        });
        let mut args = vec![
            "wait",
            "--until",
            "workstream",
            "--timeout",
            timeout,
            "--webhook-url",
            &hook.url,
        ];
        if !daemon {
            args.push("--no-daemon");
        }
        args.push(W);
        let mut child = env
            .cmd(&args)
            .env("T3_WATCH_TEST_SUSPEND_FILE", &clock)
            .spawn()
            .unwrap();
        let what = format!("daemon={daemon} timeout={timeout}");
        if timeout == "60s" {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !fs::read_to_string(env.home.join("trace.jsonl"))
                .unwrap_or_default()
                .contains("\"kind\":\"watch.restart\"")
            {
                assert!(
                    Instant::now() < deadline,
                    "{what}: the stale answer was never discarded"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            still_running(&mut child, Duration::from_millis(500));
            mock.set_thread(W, "completed", 3);
        }
        let (code, out, err) = finish(child, Duration::from_secs(20));
        assert!(
            mock.with(|s| s.suspend_after_read.is_empty()),
            "{what}: the stale read never happened"
        );
        let seen = format!(
            "{what}: exit {code}, outcome {}, run {}, delegatedSettled {}, settled hooks {}",
            out["outcome"],
            out["threads"][0]["runOrdinal"],
            out["threads"][0]["delegatedSettled"],
            settled_hooks(&hook)
        );
        if timeout == "1s" {
            assert_eq!(code, 3, "{seen}; {err}");
            assert_eq!(out["outcome"], "timeout", "{what}: {out}");
            assert_ne!(out["threads"][0]["delegatedSettled"], true, "{what}: {out}");
            assert_eq!(settled_hooks(&hook), 0, "{what}: a settled event went out");
        } else {
            assert_eq!(code, 0, "{seen}; {err}");
            assert_eq!(out["outcome"], "completed", "{what}: {out}");
            assert_eq!(out["threads"][0]["runOrdinal"], 3, "{seen}");
            assert_eq!(out["threads"][0]["resultRunId"], run_id(W, 3), "{what}: {out}");
            let hooks = hook.requests();
            assert_eq!(hooks.len(), 1, "{what}");
            assert!(
                String::from_utf8_lossy(&hooks[0].body).contains(&run_id(W, 3)),
                "{what}"
            );
        }
        let trace = fs::read_to_string(env.home.join("trace.jsonl")).unwrap();
        assert!(trace.contains("\"kind\":\"watch.restart\""), "{what}: {trace}");
    }
}

#[test]
fn a_local_answer_from_before_a_suspend_never_completes_the_wait() {
    for (daemon, timeout) in [(false, "3s"), (true, "3s"), (false, "60s"), (true, "60s")] {
        let env = Env::new();
        let t3 = MockT3::new(&env);
        t3.set(L, "running", 2);
        let hook = MockHook::start();
        let clock = env.home.join("suspend");
        suspend(&clock, 0);
        let mut args = vec![
            "wait",
            "--transport",
            "local",
            "--t3-bin",
            t3.bin_str(),
            "--interval",
            "100ms",
            "--timeout",
            timeout,
            "--webhook-url",
            &hook.url,
        ];
        if !daemon {
            args.push("--no-daemon");
        }
        args.push(L);
        let mut child = env
            .cmd(&args)
            .env("T3_WATCH_TEST_SUSPEND_FILE", &clock)
            .spawn()
            .unwrap();
        let what = format!("daemon={daemon} timeout={timeout}");
        let deadline = Instant::now() + Duration::from_secs(10);
        while t3.calls() == 0 {
            assert!(Instant::now() < deadline, "{what}: t3 was never called");
            std::thread::sleep(Duration::from_millis(20));
        }
        t3.stale_once(L, ("completed", 2), ("running", 3), &clock, 5_000);
        let stale = t3.dir.join("stale").join(L);
        let deadline = Instant::now() + Duration::from_secs(10);
        while stale.exists() {
            assert!(Instant::now() < deadline, "{what}: the stale answer was never read");
            std::thread::sleep(Duration::from_millis(20));
        }
        if timeout == "60s" {
            still_running(&mut child, Duration::from_millis(800));
            t3.set(L, "completed", 3);
        }
        let (code, out, err) = finish(child, Duration::from_secs(20));
        let seen = format!(
            "{what}: exit {code}, outcome {}, run {}, settled hooks {}",
            out["outcome"],
            out["threads"][0]["runOrdinal"],
            settled_hooks(&hook)
        );
        if timeout == "3s" {
            assert_eq!(code, 3, "{seen}; {err}");
            assert_eq!(out["outcome"], "timeout", "{what}: {out}");
            assert_eq!(settled_hooks(&hook), 0, "{what}: a settled event went out");
        } else {
            assert_eq!(code, 0, "{seen}; {err}");
            assert_eq!(out["threads"][0]["runOrdinal"], 3, "{seen}");
            assert_eq!(settled_hooks(&hook), 1, "{what}");
        }
        let trace = fs::read_to_string(env.home.join("trace.jsonl")).unwrap();
        assert!(trace.contains("\"kind\":\"watch.restart\""), "{what}: {trace}");
    }
}

#[test]
fn a_daemon_lost_after_a_suspend_never_restarts_the_timeout() {
    // Sol's repro: a 10-minute wait, the host sleeps 11 minutes, the daemon
    // dies. The client must not register again with a fresh budget, and a
    // daemon that is alive but silent must not hold it past the deadline.
    for kill in [true, false] {
        let mock = MockMcp::start();
        mock.set_thread(T, "running", 2);
        let env = mcp_env(&mock);
        let clock = env.home.join("suspend");
        suspend(&clock, 0);
        let child = env
            .cmd(&["wait", "--timeout", "10m", T])
            .env("T3_WATCH_TEST_SUSPEND_FILE", &clock)
            .spawn()
            .unwrap();
        let s = env.wait_status("the wait to register", Duration::from_secs(10), |s| {
            Env::subscribers(s) == 1
        });
        let pid = s["pid"].as_u64().unwrap() as i32;
        // Stopped first, so this daemon cannot notice the suspend itself.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        let resumed = Instant::now();
        suspend(&clock, 11 * 60 * 1000);
        if kill {
            assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        }
        // The thread completes later: the wait must already be over.
        std::thread::sleep(Duration::from_millis(1500));
        mock.set_thread(T, "completed", 2);
        let (code, out, err) = finish(child, Duration::from_secs(20));
        if !kill {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let seen = format!(
            "kill={kill}: exit {code}, outcome {}, elapsedMs {}, daemonReconnects {}, ended {:?} after the resume",
            out["outcome"],
            out["elapsedMs"],
            out["daemonReconnects"],
            resumed.elapsed()
        );
        assert_eq!(code, 3, "{seen}; {err}");
        assert_eq!(out["outcome"], "timeout", "kill={kill}: {out}");
        assert!(
            resumed.elapsed() < Duration::from_secs(10),
            "kill={kill}: the expired wait took {:?} to end",
            resumed.elapsed()
        );
        assert!(out["elapsedMs"].as_u64().unwrap() >= 600_000, "kill={kill}: {out}");
        let trace = fs::read_to_string(env.home.join("trace.jsonl")).unwrap();
        assert_eq!(
            trace.matches("\"kind\":\"daemon.accepted\"").count(),
            1,
            "kill={kill}: registered again: {trace}"
        );
    }
}

// ---- Sol ordinal 8, P1: an answer buffered across a suspend ---------------

/// A socket between the client and a real daemon that holds the first
/// connection's answer (everything after `accepted`) until released; later
/// connections pass straight through.
struct HoldingProxy {
    held: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    forwarded: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
    /// When set, a released answer goes out one byte at a time, this many
    /// ms apart; `dripped` counts the bytes sent.
    drip_ms: Arc<AtomicUsize>,
    dripped: Arc<AtomicUsize>,
}

fn holding_proxy(listen: &Path, daemon: PathBuf) -> HoldingProxy {
    let listener = UnixListener::bind(listen).unwrap();
    fs::set_permissions(listen, fs::Permissions::from_mode(0o600)).unwrap();
    let p = HoldingProxy {
        held: Arc::new(AtomicBool::new(false)),
        release: Arc::new(AtomicBool::new(false)),
        forwarded: Arc::new(AtomicBool::new(false)),
        connections: Arc::new(AtomicUsize::new(0)),
        drip_ms: Arc::new(AtomicUsize::new(0)),
        dripped: Arc::new(AtomicUsize::new(0)),
    };
    let (held, release, forwarded, connections) = (
        p.held.clone(),
        p.release.clone(),
        p.forwarded.clone(),
        p.connections.clone(),
    );
    let (drip_ms, dripped) = (p.drip_ms.clone(), p.dripped.clone());
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let n = connections.fetch_add(1, Ordering::SeqCst);
            let Ok(up) = UnixStream::connect(&daemon) else {
                continue;
            };
            let (mut from_client, mut to_daemon) = (client.try_clone().unwrap(), up.try_clone().unwrap());
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut from_client, &mut to_daemon);
                let _ = to_daemon.shutdown(Shutdown::Write);
            });
            let (held, release, forwarded) = (held.clone(), release.clone(), forwarded.clone());
            let (drip_ms, dripped) = (drip_ms.clone(), dripped.clone());
            std::thread::spawn(move || {
                let mut from_daemon = BufReader::new(up);
                let mut to_client = client;
                let mut line = String::new();
                loop {
                    line.clear();
                    if from_daemon.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let hold = n == 0 && !line.contains("\"event\":\"accepted\"");
                    if hold {
                        held.store(true, Ordering::SeqCst);
                        while !release.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    let drip = drip_ms.load(Ordering::SeqCst) as u64;
                    if hold && drip > 0 {
                        let mut gone = false;
                        for b in line.as_bytes() {
                            if to_client.write_all(&[*b]).is_err() {
                                gone = true;
                                break;
                            }
                            dripped.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(drip));
                        }
                        if gone {
                            break;
                        }
                    } else if to_client.write_all(line.as_bytes()).is_err() {
                        break;
                    }
                    if hold {
                        forwarded.store(true, Ordering::SeqCst);
                    }
                }
                let _ = to_client.shutdown(Shutdown::Both);
            });
        }
    });
    p
}

fn until(what: &str, ok: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn an_answer_buffered_across_a_suspend_never_completes_the_wait() {
    // Sol's repros: a genuine daemon's completed-run-2 answer sat in a socket
    // proxy while the host slept 5 s and the parent moved to run 3. Read as
    // the host woke (ordinal 8), both `--timeout 1s` and `--timeout 60s`
    // returned exit 0 with run 2 and sent `settled`. Held 2.2 s more after
    // the resume, while the client found the socket empty (ordinal 9),
    // `--timeout 60s` still did: an empty read proves nothing about an
    // answer on its way.
    for (frozen, timeout) in [(false, "60s"), (false, "1s"), (true, "1s"), (true, "60s")] {
        let mock = MockMcp::start();
        mock.set_thread(W, "completed", 2);
        let hook = MockHook::start();
        let env = mcp_env(&mock);
        fs::set_permissions(&env.home, fs::Permissions::from_mode(0o700)).unwrap();
        let clock = env.home.join("suspend");
        suspend(&clock, 0);
        let mut daemon = env
            .cmd(&["daemon", "--idle-exit", "60s"])
            .env("T3_WATCH_TEST_SUSPEND_FILE", &clock)
            .spawn()
            .unwrap();
        env.wait_status("daemon", Duration::from_secs(10), |s| s["running"] == true);
        let sdir = env.home.join("s");
        fs::create_dir_all(&sdir).unwrap();
        fs::set_permissions(&sdir, fs::Permissions::from_mode(0o700)).unwrap();
        let proxy_sock = sdir.join("proxy.sock");
        let proxy = holding_proxy(&proxy_sock, env.home.join(SOCK));
        let child = env
            .cmd(&[
                "wait",
                "--until",
                "workstream",
                "--timeout",
                timeout,
                "--webhook-url",
                &hook.url,
                W,
            ])
            .env("T3_WATCH_SOCKET", &proxy_sock)
            .env("T3_WATCH_TEST_SUSPEND_FILE", &clock)
            .spawn()
            .unwrap();
        let what = format!("frozen={frozen} timeout={timeout}");
        until("the daemon's answer", || proxy.held.load(Ordering::SeqCst));
        let pid = child.id() as i32;
        if frozen {
            // The host sleeps with the answer on its way: everything stops,
            // the parent starts run 3, and the answer lands as it wakes.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
            mock.set_thread(W, "running", 3);
            suspend(&clock, 5_000);
            proxy.release.store(true, Ordering::SeqCst);
            until("the answer to be forwarded", || proxy.forwarded.load(Ordering::SeqCst));
            assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
        } else {
            // The host wakes; the answer is still held for 2.2 s, while the
            // client's reads find nothing.
            mock.set_thread(W, "running", 3);
            suspend(&clock, 5_000);
            std::thread::sleep(Duration::from_millis(2_200));
            proxy.release.store(true, Ordering::SeqCst);
        }
        if timeout == "60s" {
            std::thread::sleep(Duration::from_millis(1_500));
            mock.set_thread(W, "completed", 3);
        }
        let (code, out, err) = finish(child, Duration::from_secs(30));
        let seen = format!(
            "{what}: exit {code}, outcome {}, run {}, elapsedMs {}, settled hooks {}",
            out["outcome"],
            out["threads"][0]["runOrdinal"],
            out["elapsedMs"],
            settled_hooks(&hook)
        );
        let trace = fs::read_to_string(env.home.join("trace.jsonl")).unwrap();
        if timeout == "1s" {
            assert_eq!(code, 3, "{seen}; {err}");
            assert_eq!(out["outcome"], "timeout", "{seen}");
            assert_eq!(settled_hooks(&hook), 0, "{seen}");
            let kind = if frozen { "daemon.late" } else { "daemon.stale" };
            assert!(trace.contains(&format!("\"kind\":\"{kind}\"")), "{what}: {trace}");
        } else {
            assert_eq!(code, 0, "{seen}; {err}");
            assert_eq!(out["threads"][0]["runOrdinal"], 3, "{seen}");
            assert_eq!(out["daemonReregisteredAfterSuspend"], 1, "{what}: {out}");
            assert!(proxy.connections.load(Ordering::SeqCst) >= 2, "{what}");
            assert_eq!(settled_hooks(&hook), 1, "{seen}");
            assert!(trace.contains("\"kind\":\"daemon.stale\""), "{what}: {trace}");
        }
        env.run(&["stop"]);
        let _ = daemon.wait();
    }
}

#[test]
fn a_frame_trickling_in_never_holds_back_the_suspend_and_deadline_checks() {
    // Sol ordinal 10: a genuine daemon's accepted line, then its answer one
    // byte every 200 ms; the host sleeps 11 minutes into a 10-minute wait.
    // The client read on while bytes kept coming and ended only when the
    // frame was complete.
    let mock = MockMcp::start();
    mock.set_thread(W, "completed", 2);
    let env = mcp_env(&mock);
    fs::set_permissions(&env.home, fs::Permissions::from_mode(0o700)).unwrap();
    let clock = env.home.join("suspend");
    suspend(&clock, 0);
    let mut daemon = env
        .cmd(&["daemon", "--idle-exit", "60s"])
        .env("T3_WATCH_TEST_SUSPEND_FILE", &clock)
        .spawn()
        .unwrap();
    env.wait_status("daemon", Duration::from_secs(10), |s| s["running"] == true);
    let sdir = env.home.join("s");
    fs::create_dir_all(&sdir).unwrap();
    fs::set_permissions(&sdir, fs::Permissions::from_mode(0o700)).unwrap();
    let proxy_sock = sdir.join("proxy.sock");
    let proxy = holding_proxy(&proxy_sock, env.home.join(SOCK));
    proxy.drip_ms.store(200, Ordering::SeqCst);
    let child = env
        .cmd(&["wait", "--timeout", "10m", W])
        .env("T3_WATCH_SOCKET", &proxy_sock)
        .env("T3_WATCH_TEST_SUSPEND_FILE", &clock)
        .spawn()
        .unwrap();
    until("the daemon's answer", || proxy.held.load(Ordering::SeqCst));
    proxy.release.store(true, Ordering::SeqCst);
    until("the answer to start trickling", || {
        proxy.dripped.load(Ordering::SeqCst) >= 3
    });
    let resumed = Instant::now();
    suspend(&clock, 660_000);
    let (code, out, err) = finish(child, Duration::from_secs(15));
    let took = resumed.elapsed();
    let sent = proxy.dripped.load(Ordering::SeqCst);
    assert_eq!(
        code, 3,
        "exit {code} after {took:?}, {sent} bytes trickled: {out} {err}"
    );
    assert_eq!(out["outcome"], "timeout", "{out}");
    assert!(
        took < Duration::from_secs(3),
        "the expired wait ended {took:?} after the resume, {sent} bytes into the frame"
    );
    env.run(&["stop"]);
    let _ = daemon.wait();
}
