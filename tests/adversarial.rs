//! Adversarial regressions for the second review of t3-watch:
//!
//! - P1: a delegated child's `resultRun` must never prove a newer parent run;
//!   missing run identity stays unresolved.
//! - P1: cancelled long-polls must end for real, counted against a global
//!   limit until they do, checked on the wire and in the OS.
//! - P1: a token refresh that fails for a temporary reason is retried, not
//!   reported as an auth failure; `invalid_grant` is final; concurrent
//!   rotation refreshes once.
//! - P2: strict 2025-06-18 handshake, SSE server requests, startup errors
//!   with `--notify-on all`, exclusive pending-login use, absolute OAuth
//!   callback deadline, and private regular secret files.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const P: &str = "mcp:launch:parent";

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn mcp_env(mock: &MockMcp) -> Env {
    let env = Env::new();
    env.add_machine("mock", &mock.url, TOKEN);
    env
}

fn port_of(url: &str) -> u16 {
    url.rsplit(':').next().unwrap().trim_end_matches('/').parse().unwrap()
}

fn both(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn eventually(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn private(path: &std::path::Path) {
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
}

fn approved_callback(env: &Env, mock: &MockMcp, name: &str) -> String {
    let out = env.run(&["auth", "oauth-start", &mock.url, "--name", name]);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    let url = output_json(&out)["authorizeUrl"].as_str().unwrap().to_string();
    let (status, location) = http_get(&url);
    assert_eq!(status, 302);
    location.unwrap()
}

// ---- P1: delegated proof needs the parent's own run -----------------------

fn delegated(mock: &MockMcp, timeout: &str) -> (Option<i32>, Value) {
    let env = mcp_env(mock);
    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--until-delegated-settled",
        "--expect-min-run-ordinal",
        "2",
        "--timeout",
        timeout,
        P,
    ]);
    (out.status.code(), output_json(&out))
}

#[test]
fn a_childs_result_run_never_proves_a_newer_parent_run() {
    // Sol's repro: the wait proved parent run 1 and child run 100; the read
    // then shows parent run 2.
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 2);
    mock.with(|s| {
        s.wait_override
            .insert("runId".into(), json!(format!("run:thread:{P}:ordinal:1")));
        s.wait_override
            .insert("resultRun".into(), json!("run:thread:mcp%3Alaunch%3Achild:ordinal:100"));
    });
    let (code, v) = delegated(&mock, "2s");
    assert_eq!(code, Some(3), "{v}");
    let t = &v["threads"][0];
    assert_eq!(t["settled"], false);
    assert_eq!(t["delegatedSettled"], false);
    assert_eq!(t["runOrdinal"], 2);
    assert!(
        mock.calls("t3_thread_wait") >= 2,
        "the parent's newer run must be waited on again"
    );
}

#[test]
fn a_parent_proof_of_the_run_the_read_saw_settles() {
    for proof in [
        json!({"resultRun": {"runId": format!("run:thread:{P}:ordinal:2"), "status": "completed"}}),
        // resultRun may name a later run of the same thread.
        json!({"runId": format!("run:thread:{P}:ordinal:1"), "resultRun": {"runId": format!("run:thread:{P}:ordinal:3")}}),
        // Percent-encoded thread ids are the same identity.
        json!({"resultRun": {"runId": "run:thread:mcp%3Alaunch%3Aparent:ordinal:2", "status": "completed"}}),
    ] {
        let mock = MockMcp::start();
        mock.set_thread(P, "completed", 2);
        mock.with(|s| s.wait_override = proof.as_object().unwrap().clone());
        let (code, v) = delegated(&mock, "5s");
        assert_eq!(code, Some(0), "{proof}: {v}");
        assert_eq!(v["threads"][0]["delegatedSettled"], true);
        assert_eq!(v["until"], "workstream");
    }
}

#[test]
fn a_settled_wait_without_result_run_fails_closed() {
    // The wait's own runId is the turn, not the workstream: without T3's
    // resultRun nothing proves the delegated work and its continuations
    // have settled, so this is an error, never completion.
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 2);
    mock.with(|s| s.omit_result_run = true);
    let (code, v) = delegated(&mock, "5s");
    assert_eq!(code, Some(2), "{v}");
    assert_eq!(v["error"]["kind"], "unsupported");
    assert_eq!(v["outcome"], "error");
    assert!(v["error"]["message"].as_str().unwrap().contains("resultRun"));
}

#[test]
fn missing_run_identity_stays_unresolved() {
    // An opaque run id proves nothing.
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 2);
    mock.with(|s| {
        s.wait_override.insert("runId".into(), json!("opaque-run-7"));
        s.wait_override.insert(
            "resultRun".into(),
            json!({"runId": "opaque-run-7", "status": "completed"}),
        );
    });
    assert_eq!(delegated(&mock, "1500ms").0, Some(3));

    // No latestRunId and no runCount: unknown, not "no runs".
    let mock = MockMcp::start();
    mock.set_thread(P, "idle", 0);
    mock.with(|s| {
        s.read_override.insert("latestRunId".into(), Value::Null);
        s.read_override.insert("runCount".into(), Value::Null);
    });
    let env = mcp_env(&mock);
    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--until-delegated-settled",
        "--timeout",
        "1500ms",
        P,
    ]);
    assert_eq!(out.status.code(), Some(3), "{}", both(&out));

    // An explicit runCount of 0 is evidence of no runs: settled.
    let mock = MockMcp::start();
    mock.set_thread(P, "idle", 0);
    mock.with(|s| {
        s.read_override.insert("latestRunId".into(), Value::Null);
    });
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--until-delegated-settled", "--timeout", "5s", P]);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
}

#[test]
fn a_latest_run_of_another_thread_never_satisfies_the_ordinal() {
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 1);
    mock.with(|s| {
        s.read_override
            .insert("latestRunId".into(), json!("run:thread:mcp%3Alaunch%3Achild:ordinal:9"));
    });
    let env = mcp_env(&mock);
    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--expect-min-run-ordinal",
        "2",
        "--timeout",
        "1500ms",
        P,
    ]);
    assert_eq!(out.status.code(), Some(3), "{}", both(&out));
    let t = &output_json(&out)["threads"][0];
    assert_eq!(t["runOrdinal"], Value::Null);
    assert!(t["diagnostic"].as_str().unwrap().contains("not a run of this thread"));
}

// ---- P1: cancelled operations really end ------------------------------------

#[test]
fn cancelled_long_polls_end_on_the_wire_and_in_the_os() {
    let mock = MockMcp::start();
    mock.set_thread(P, "running", 1);
    let env = mcp_env(&mock);
    let port = port_of(&mock.url);
    let short_wait = || {
        env.cmd(&["wait", "--timeout", "300ms", "--slice", "60s", P])
            .env("T3_WATCH_MAX_WATCHES", "1")
            // Keep the daemon up for the whole test, so only cancellation
            // (never an idle exit) can end the long-polls.
            .env("T3_WATCH_IDLE_EXIT", "120s")
            .output()
            .unwrap()
    };

    // The first wait starts the daemon (with the limit) and sets a baseline.
    assert_eq!(short_wait().status.code(), Some(3));
    let pid = env.status()["pid"].as_u64().unwrap() as u32;
    eventually("the first long-poll to end", secs(5), || {
        mock.state.0.lock().unwrap().active_waits == 0 && tcp_connections_to(pid, port) == 0
    });
    let (threads0, fds0) = (thread_count(pid), fd_count(pid));
    assert!(threads0 > 0 && fds0 > 0);

    for i in 0..10 {
        let out = short_wait();
        assert_eq!(out.status.code(), Some(3), "wait {i}: {}", both(&out));
    }

    // Every long-poll was cancelled before the next one started: the mock
    // saw "wait N, cancelled N, wait N+1, ..." with matching request ids.
    let events = mock.state.0.lock().unwrap().events.clone();
    let waits: Vec<&Value> = events.iter().filter(|(k, _)| k == "wait").map(|(_, id)| id).collect();
    assert_eq!(waits.len(), 11, "{events:?}");
    for pair in events.windows(2) {
        if pair[0].0 == "wait" {
            assert_eq!(
                pair[1].0, "cancelled",
                "a new long-poll started before the last was cancelled: {events:?}"
            );
            assert_eq!(pair[1].1, pair[0].1, "cancellation names the wrong request: {events:?}");
        }
    }

    // On the wire and in the OS, nothing is left behind.
    eventually("all long-polls to end", secs(5), || {
        let s = mock.state.0.lock().unwrap();
        s.active_waits == 0 && s.calls.get("client-gone").copied().unwrap_or(0) >= 11
    });
    eventually("no connections to the mock", secs(5), || {
        tcp_connections_to(pid, port) == 0
    });
    eventually("the budget to drain", secs(5), || {
        let s = env.status();
        s["upstreamOps"]["live"] == 0 && s["watches"].as_array().is_some_and(|w| w.is_empty())
    });
    assert_eq!(env.status()["upstreamOps"]["max"], 1);
    eventually("threads and descriptors back to baseline", secs(5), || {
        thread_count(pid) <= threads0 && fd_count(pid) <= fds0
    });
}

#[test]
fn a_cancelled_local_poll_kills_its_t3_process() {
    let env = Env::new();
    let dir = env.home.join("slow");
    fs::create_dir_all(&dir).unwrap();
    let pidfile = dir.join("pid");
    let bin = dir.join("t3");
    script(
        &bin,
        &format!("echo $$ > '{}'\nsleep 30\necho '{{}}'", pidfile.display()),
    );
    let out = env.run(&[
        "wait",
        "--transport",
        "local",
        "--t3-bin",
        bin.to_str().unwrap(),
        "--timeout",
        "500ms",
        "x",
    ]);
    assert_eq!(out.status.code(), Some(3), "{}", both(&out));
    let child: i32 = fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    eventually(
        "the t3 child to be killed",
        secs(3),
        || unsafe { libc::kill(child, 0) } != 0,
    );
    eventually("the budget to drain", secs(3), || {
        env.status()["upstreamOps"]["live"] == 0
    });
}

// ---- P1: token refresh failures are typed --------------------------------

fn paired(mock: &MockMcp) -> Env {
    let env = Env::new();
    let callback = approved_callback(&env, mock, "mock");
    assert_eq!(env.run(&["auth", "oauth-finish", &callback]).status.code(), Some(0));
    env
}

#[test]
fn a_refresh_that_fails_with_503_is_retried_not_fatal() {
    let mock = MockMcp::start();
    mock.with(|s| {
        s.issue_refresh = Some("rt-1".into());
        s.refresh_to = Some("token-2".into());
        s.refresh_failures = [503, 503].into();
    });
    let env = paired(&mock);
    mock.set_thread(P, "running", 1);
    let mut child = env.spawn(&["wait", "--no-daemon", "--slice", "300ms", P]);
    still_running(&mut child, Duration::from_millis(400));
    mock.with(|s| s.token = "token-2".into()); // the first token stops working
                                               // Two failed refreshes with backoff in between, then a good one.
    eventually("three refresh attempts", secs(15), || mock.calls("refresh") >= 3);
    still_running(&mut child, Duration::from_millis(500));
    mock.set_thread(P, "completed", 1);
    let (code, out, err) = finish(child, secs(15));
    assert_eq!(code, 0, "{out} {err}");
    assert_eq!(mock.calls("refresh"), 3);
    assert_eq!(env.machines()["machines"]["mock"]["accessToken"], "token-2");
}

#[test]
fn an_unreachable_token_endpoint_is_transient() {
    let mock = MockMcp::start();
    mock.set_thread(P, "running", 1);
    mock.with(|s| s.token = "a-token-we-do-not-have".into());
    let env = Env::new();
    let cfg = json!({"machines": {"mock": {
        "origin": mock.url, "accessToken": TOKEN, "refreshToken": "rt-x",
        "clientId": "client-1", "tokenEndpoint": "http://127.0.0.1:1/token"}}});
    fs::write(env.home.join("machines.json"), cfg.to_string()).unwrap();
    let out = env.run(&["wait", "--no-daemon", "--timeout", "3s", P]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "must keep waiting, not exit 2: {}",
        both(&out)
    );
    let last = output_json(&out)["threads"][0]["lastError"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(last.contains("token recovery failed"), "{last}");
}

#[test]
fn invalid_grant_is_final() {
    let mock = MockMcp::start();
    mock.with(|s| {
        s.issue_refresh = Some("rt-1".into());
        s.refresh_failures = [400].into();
    });
    let env = paired(&mock);
    mock.set_thread(P, "running", 1);
    mock.with(|s| s.token = "someone-elses".into());
    let started = Instant::now();
    let out = env.run(&["wait", "--no-daemon", P]);
    assert_eq!(out.status.code(), Some(2), "{}", both(&out));
    assert!(started.elapsed() < secs(5));
    let v = output_json(&out);
    assert_eq!(v["error"]["kind"], "auth");
    assert!(v["error"]["message"].as_str().unwrap().contains("invalid_grant"), "{v}");
}

#[test]
fn concurrent_refresh_rotation_refreshes_once() {
    let mock = MockMcp::start();
    mock.with(|s| {
        s.issue_refresh = Some("rt-1".into());
        s.rotate_refresh = true;
    });
    let env = paired(&mock);
    mock.set_thread(P, "running", 1);
    let clients: Vec<_> = (0..3)
        .map(|_| env.spawn(&["wait", "--no-daemon", "--slice", "300ms", P]))
        .collect();
    std::thread::sleep(Duration::from_millis(500));
    // Revoke the paired token: every client hits 401 at about the same time.
    mock.with(|s| s.token = "revoked".into());
    eventually("the rotation", secs(10), || mock.state.0.lock().unwrap().rotations == 1);
    std::thread::sleep(Duration::from_millis(1500));
    mock.set_thread(P, "completed", 1);
    for c in clients {
        let (code, out, err) = finish(c, secs(15));
        assert_eq!(code, 0, "a client lost the rotation race: {out} {err}");
    }
    assert_eq!(mock.calls("refresh"), 1, "the refresh token was replayed");
    let saved = env.machines();
    assert_eq!(saved["machines"]["mock"]["accessToken"], "token-r1");
    assert_eq!(saved["machines"]["mock"]["refreshToken"], "rt-r1");
}

// ---- P2: protocol --------------------------------------------------------------

#[test]
fn a_strict_june_2025_server_gets_the_version_header_everywhere() {
    let mock = MockMcp::start();
    mock.with(|s| s.protocol_version = "2025-06-18".into());
    mock.set_thread(P, "completed", 1);
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--timeout", "5s", P]);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    assert_eq!(mock.calls("missing-version-header"), 0);
    assert_eq!(mock.calls("initialized"), 1);
}

#[test]
fn server_requests_in_an_event_stream_are_answered() {
    let mock = MockMcp::start();
    mock.with(|s| {
        s.sse = true;
        s.server_requests = vec![
            json!({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 77, "method": "roots/list"}),
        ];
    });
    mock.set_thread(P, "completed", 1);
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", P]);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    let replies = mock.state.0.lock().unwrap().client_responses.clone();
    let ping = replies.iter().find(|r| r["id"] == "srv-1").expect("ping unanswered");
    assert_eq!(ping["result"], json!({}));
    let roots = replies.iter().find(|r| r["id"] == 77).expect("roots/list unanswered");
    assert_eq!(roots["error"]["code"], -32601);
}

// ---- P2: --notify-on all covers startup errors ---------------------------

#[test]
fn notify_on_all_reports_errors_before_the_wait_starts() {
    let hook = MockHook::start();
    let env = Env::new();
    fs::write(env.home.join("machines.json"), "{ not json").unwrap();
    let out = env.run(&[
        "wait",
        "--notify-on",
        "all",
        "--webhook-url",
        &hook.url,
        "--machine",
        "x",
        P,
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(output_json(&out)["error"]["kind"], "config");
    let reqs = hook.requests();
    assert_eq!(reqs.len(), 1, "the config error was not delivered");
    assert_eq!(reqs[0].headers["x-t3-watch-event"], "error");

    fs::remove_file(env.home.join("machines.json")).unwrap();
    let out = env.run(&[
        "wait",
        "--notify-on",
        "all",
        "--webhook-url",
        &hook.url,
        "--mode",
        "sometimes",
        P,
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(hook.requests().len(), 2, "a bad option value was not delivered");

    // Settle-only notifications stay quiet about errors.
    let out = env.run(&["wait", "--webhook-url", &hook.url, "--mode", "sometimes", P]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(hook.requests().len(), 2);
}

// ---- P2: a pending login is used once ------------------------------------

#[test]
fn concurrent_finishes_of_one_login_exchange_the_code_once() {
    let mock = MockMcp::start();
    let env = Env::new();
    let callback = approved_callback(&env, &mock, "once");
    let finishers: Vec<_> = (0..6)
        .map(|_| env.spawn(&["auth", "oauth-finish", &callback]))
        .collect();
    let codes: Vec<i32> = finishers.into_iter().map(|c| finish(c, secs(20)).0).collect();
    assert_eq!(codes.iter().filter(|c| **c == 0).count(), 1, "{codes:?}");
    assert_eq!(codes.iter().filter(|c| **c == 2).count(), 5, "{codes:?}");
    assert_eq!(mock.calls("exchange"), 1, "the code was exchanged more than once");
    assert_eq!(fs::read_dir(env.home.join("pending")).unwrap().count(), 0);
}

// ---- P2: the callback deadline is absolute -------------------------------

#[test]
fn login_deadline_holds_against_slow_and_endless_connections() {
    let mock = MockMcp::start();
    let env = Env::new();
    let mut child = env
        .cmd(&[
            "auth",
            "login",
            &mock.url,
            "--no-browser",
            "--name",
            "dl",
            "--timeout",
            "2s",
        ])
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut url = String::new();
    while !url.starts_with("http") {
        url.clear();
        stderr.read_line(&mut url).unwrap();
        url = url.trim().to_string();
    }
    let q = parse_query(url.split_once('?').unwrap().1);
    let addr = q["redirect_uri"]
        .trim_start_matches("http://")
        .trim_end_matches("/callback")
        .to_string();
    let stop = Arc::new(AtomicBool::new(false));
    // A client that trickles one byte at a time and never finishes.
    let slow = {
        let (addr, stop) = (addr.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                if let Ok(mut s) = TcpStream::connect(&addr) {
                    for b in b"GET /callback?state=x HTTP/1.1\r\n" {
                        if stop.load(Ordering::SeqCst) || s.write_all(&[*b]).is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(300));
                    }
                }
            }
        })
    };
    // Plus an endless stream of connections that send nothing.
    let flood = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                if let Ok(s) = TcpStream::connect(&addr) {
                    held.push(s);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let started = Instant::now();
    let (code, out, _) = finish(child, secs(15));
    let took = started.elapsed();
    stop.store(true, Ordering::SeqCst);
    slow.join().unwrap();
    flood.join().unwrap();
    assert_eq!(code, 2, "{out}");
    assert!(out["error"]["message"].as_str().unwrap().contains("timed out"), "{out}");
    assert!(took < secs(5), "the 2s deadline stretched to {took:?}");
}

// ---- P2: secret files must be private regular files ----------------------

#[test]
fn notification_secret_files_must_be_private_regular_files() {
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 1);
    let env = mcp_env(&mock);
    let hook = MockHook::start();
    let run = |file: &std::path::Path| {
        env.run(&[
            "wait",
            "--no-daemon",
            "--webhook-url",
            &hook.url,
            "--webhook-secret-file",
            file.to_str().unwrap(),
            P,
        ])
    };

    let open_file = env.home.join("readable");
    fs::write(&open_file, "k").unwrap();
    fs::set_permissions(&open_file, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let out = run(&open_file);
    assert_eq!(out.status.code(), Some(2));
    assert!(both(&out).contains("chmod 600"), "{}", both(&out));

    // A FIFO would block a plain open forever.
    let fifo = env.home.join("fifo");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let started = Instant::now();
    let out = run(&fifo);
    assert_eq!(out.status.code(), Some(2));
    assert!(started.elapsed() < secs(3), "reading a FIFO blocked");
    assert!(both(&out).contains("not a regular file"));

    let dir = env.home.join("adir");
    fs::create_dir(&dir).unwrap();
    assert!(both(&run(&dir)).contains("not a regular file"));

    // A symlink to a private file of ours is fine: the target is checked.
    let real = env.home.join("real-secret");
    fs::write(&real, "k").unwrap();
    private(&real);
    let link = env.home.join("link-secret");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let out = run(&link);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    assert!(hook.requests().len() == 1, "only the valid run notifies");
}
