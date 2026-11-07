//! End-to-end tests: the real `t3-watch` binary against a mock T3 MCP server,
//! a mock OAuth server, and a mock local `t3` CLI.

mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::time::{Duration, Instant};

const T: &str = "mcp:launch:abc123";
const SECS: fn(u64) -> Duration = Duration::from_secs;

fn mcp_env(mock: &MockMcp) -> Env {
    let env = Env::new();
    env.add_machine("mock", &mock.url, TOKEN);
    env
}

#[test]
fn json_response_settles_after_thread_completes() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 1);
    let env = mcp_env(&mock);
    let mut child = env.spawn(&["wait", "--no-daemon", "--machine", "mock", T]);
    still_running(&mut child, Duration::from_millis(400));
    mock.set_thread(T, "completed", 1);
    let (code, out, err) = finish(child, SECS(10));
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out["event"], "settled");
    assert_eq!(out["outcome"], "completed");
    assert_eq!(out["until"], "turn");
    assert_eq!(out["threads"][0]["status"], "completed");
    assert_eq!(out["threads"][0]["runOrdinal"], 1);
    assert_eq!(out["threads"][0]["link"], format!("t3-thread://v2/env-mock/{T}"));
    assert_eq!(mock.calls("initialize"), 1);
    assert_eq!(mock.calls("initialized"), 1);
    assert_eq!(mock.calls("t3_thread_wait"), 1);
    // One header read before the first wait, one after it.
    assert_eq!(mock.calls("t3_thread_read"), 2);
    let args = &mock.state.0.lock().unwrap().wait_args[0];
    assert_eq!(args["timeoutMs"], 30000);
    assert!(args.get("environmentId").is_none());
}

#[test]
fn sse_responses_are_parsed() {
    let mock = MockMcp::start();
    mock.with(|s| s.sse = true);
    mock.set_thread(T, "running", 3);
    let env = mcp_env(&mock);
    let mut child = env.spawn(&["wait", "--no-daemon", T]);
    still_running(&mut child, Duration::from_millis(300));
    mock.set_thread(T, "waiting", 3);
    let (code, out, err) = finish(child, SECS(10));
    // Waiting for input is its own outcome, never completion.
    assert_eq!(code, 5, "stderr: {err}");
    assert_eq!(out["event"], "needs_input");
    assert_eq!(out["threads"][0]["status"], "waiting");
    assert_eq!(out["threads"][0]["outcome"], "needs_input");
    assert_eq!(out["threads"][0]["runOrdinal"], 3);
}

#[test]
fn expired_session_is_reinitialized_without_waking() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 1);
    let env = mcp_env(&mock);
    // Short slices so the client goes back to the server several times.
    let mut child = env.spawn(&["wait", "--no-daemon", "--slice", "200ms", T]);
    still_running(&mut child, Duration::from_millis(500));
    mock.expire_sessions();
    still_running(&mut child, Duration::from_millis(700));
    mock.set_thread(T, "completed", 1);
    let (code, out, err) = finish(child, SECS(10));
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out["event"], "settled");
    assert!(mock.calls("expired-session") >= 1, "session never expired");
    assert_eq!(mock.calls("initialize"), 2, "expected exactly one re-initialize");
}

#[test]
fn cloudflare_errors_are_retried_without_waking() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let page = "<html><title>Error 1033</title>Cloudflare Tunnel error</html>".to_string();
    mock.with(|s| {
        s.fail_queue.push_back((530, page.clone()));
        s.fail_queue.push_back((502, "bad gateway".into()));
        s.fail_queue.push_back((504, "gateway timeout".into()));
    });
    let env = mcp_env(&mock);
    let started = Instant::now();
    let (code, out, err) = finish(env.spawn(&["wait", "--no-daemon", T]), SECS(30));
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out["event"], "settled");
    assert_eq!(mock.calls("failed:530"), 1);
    assert_eq!(mock.calls("failed:502"), 1);
    assert_eq!(mock.calls("failed:504"), 1);
    // Backoff is 0.5-1s, then 1-2s, then 2-4s.
    assert!(
        started.elapsed() >= Duration::from_millis(3500),
        "retried too fast: {:?}",
        started.elapsed()
    );
}

#[test]
fn transient_errors_show_up_on_timeout() {
    let mock = MockMcp::start();
    mock.with(|s| (0..20).for_each(|_| s.fail_queue.push_back((530, "error code: 1033".into()))));
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--timeout", "2s", T]);
    assert_eq!(out.status.code(), Some(3));
    let v = output_json(&out);
    assert_eq!(v["event"], "timeout");
    assert!(v["threads"][0]["lastError"].as_str().unwrap().contains("1033"), "{v}");
}

#[test]
fn revoked_token_fails_fast_with_exit_2() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 1);
    let env = Env::new();
    env.add_machine("mock", &mock.url, "revoked-token");
    let started = Instant::now();
    let out = env.run(&["wait", "--no-daemon", T]);
    assert_eq!(out.status.code(), Some(2));
    assert!(started.elapsed() < SECS(5));
    let v = output_json(&out);
    assert_eq!(v["event"], "error");
    assert_eq!(v["error"]["kind"], "auth");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("revoked-token"));
}

#[test]
fn unknown_thread_fails_with_exit_2() {
    let mock = MockMcp::start();
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "mcp:launch:missing"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(output_json(&out)["error"]["kind"], "not_found");
}

#[test]
fn expect_min_run_ordinal_ignores_stale_completed_run() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    let mut child = env.spawn(&["wait", "--no-daemon", "--expect-min-run-ordinal", "2", T]);
    still_running(&mut child, Duration::from_millis(1500));
    mock.set_thread(T, "running", 2);
    still_running(&mut child, Duration::from_millis(500));
    mock.set_thread(T, "completed", 2);
    let (code, out, err) = finish(child, SECS(20));
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out["threads"][0]["runOrdinal"], 2);
    assert_eq!(out["expectMinRunOrdinal"], 2);
}

#[test]
fn thread_url_selects_machine_and_environment() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    let url = format!("{}/env-peer-42/{}", mock.url, T.replace(':', "%3A"));
    let out = env.run(&["wait", "--no-daemon", "--until-delegated-settled", &url]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let args = mock.state.0.lock().unwrap().wait_args[0].clone();
    assert_eq!(args["threadId"], T);
    assert_eq!(args["environmentId"], "env-peer-42");
    assert_eq!(args["untilDelegatedWorkSettled"], true);
}

#[test]
fn mode_any_returns_on_first_settled_thread() {
    let mock = MockMcp::start();
    mock.set_thread("t-a", "running", 1);
    mock.set_thread("t-b", "running", 1);
    let env = mcp_env(&mock);
    let mut child = env.spawn(&["wait", "--no-daemon", "--mode", "any", "t-a", "t-b"]);
    still_running(&mut child, Duration::from_millis(300));
    mock.set_thread("t-b", "failed", 1);
    let (code, out, _) = finish(child, SECS(10));
    // The first thread to finish failed: exit 6, not success.
    assert_eq!(code, 6);
    assert_eq!(out["event"], "settled");
    assert_eq!(out["outcome"], "failed");
    let threads = out["threads"].as_array().unwrap();
    assert_eq!(threads[0]["settled"], false);
    assert_eq!(threads[1]["settled"], true);
    assert_eq!(threads[1]["status"], "failed");
}

#[test]
fn mode_all_waits_for_every_thread() {
    let mock = MockMcp::start();
    mock.set_thread("t-a", "running", 1);
    mock.set_thread("t-b", "running", 1);
    let env = mcp_env(&mock);
    let mut child = env.spawn(&["wait", "t-a", "t-b"]);
    still_running(&mut child, Duration::from_millis(300));
    mock.set_thread("t-a", "completed", 1);
    still_running(&mut child, Duration::from_millis(500));
    mock.set_thread("t-b", "interrupted", 1);
    let (code, out, _) = finish(child, SECS(10));
    assert_eq!(code, 6, "one thread was interrupted: {out}");
    assert_eq!(out["outcome"], "failed");
    assert_eq!(out["threads"][0]["outcome"], "completed");
    assert_eq!(out["threads"][1]["outcome"], "failed");
    assert_eq!(out["via"], "daemon");
    assert!(out["threads"].as_array().unwrap().iter().all(|t| t["settled"] == true));
}

#[test]
fn twenty_daemon_clients_share_one_upstream_long_poll() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 1);
    let env = mcp_env(&mock);
    let children: Vec<_> = (0..20).map(|_| env.spawn(&["wait", "--machine", "mock", T])).collect();
    let status = env.wait_status("20 subscribers", SECS(15), |s| Env::subscribers(s) == 20);
    assert_eq!(status["watches"].as_array().unwrap().len(), 1, "{status}");
    assert_eq!(status["activeWaits"], 20);
    mock.set_thread(T, "completed", 1);
    for child in children {
        let (code, out, err) = finish(child, SECS(15));
        assert_eq!(code, 0, "stderr: {err}");
        assert_eq!(out["event"], "settled");
        assert_eq!(out["via"], "daemon");
    }
    let calls = mock.state.0.lock().unwrap().calls.clone();
    assert_eq!(
        mock.calls("t3_thread_wait"),
        1,
        "20 clients must cost one upstream t3_thread_wait: {calls:?} {}",
        env.status()
    );
    assert_eq!(
        mock.calls("t3_thread_read"),
        2,
        "one header read before the wait, one after"
    );
    assert_eq!(mock.with(|s| s.probes), 1, "one delegated-work probe for all 20");
    assert_eq!(mock.calls("initialize"), 1, "one MCP session for the machine");
}

#[test]
fn killed_client_is_unsubscribed_and_daemon_idles_out() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 1);
    let env = mcp_env(&mock);
    let mut child = env.spawn(&["wait", T]);
    env.wait_status("1 subscriber", SECS(10), |s| Env::subscribers(s) == 1);
    let pid = env.status()["pid"].as_u64().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    env.wait_status("0 subscribers", SECS(5), |s| s["activeWaits"] == 0);
    // T3_WATCH_IDLE_EXIT=2s in tests.
    let deadline = Instant::now() + SECS(10);
    while env.status()["running"] == true {
        assert!(Instant::now() < deadline, "daemon {pid} did not exit when idle");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!env.home.join("daemon-p2.sock").exists());
}

#[test]
fn state_dir_and_socket_are_private() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    let out = env.run(&["wait", T]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(mode(&env.home), 0o700);
    assert_eq!(mode(&env.home.join("daemon-p2.sock")) & 0o077, 0);
    assert_eq!(mode(&env.home.join("daemon.log")), 0o600);
}

#[test]
fn local_backend_respects_run_ordinal_and_dedups() {
    let env = Env::new();
    let t3 = MockT3::new(&env);
    t3.set("lt-1", "completed", 1);
    let args = [
        "wait",
        "--transport",
        "local",
        "--t3-bin",
        t3.bin_str(),
        "--interval",
        "100ms",
        "--expect-min-run-ordinal",
        "2",
        "lt-1",
    ];
    let mut clients: Vec<_> = (0..5).map(|_| env.spawn(&args)).collect();
    env.wait_status("5 local subscribers", SECS(10), |s| Env::subscribers(s) == 5);
    for c in clients.iter_mut() {
        still_running(c, Duration::from_millis(50));
    }
    t3.set("lt-1", "running", 2);
    std::thread::sleep(Duration::from_millis(400));
    let before = t3.calls();
    std::thread::sleep(Duration::from_millis(1000));
    let per_sec = t3.calls() - before;
    // One poller for five clients at 100ms: about 10 calls a second, not 50.
    assert!(per_sec <= 13, "{per_sec} t3 calls in 1s for 5 clients");
    t3.set("lt-1", "completed", 2);
    for c in clients {
        let (code, out, err) = finish(c, SECS(10));
        assert_eq!(code, 0, "stderr: {err}");
        assert_eq!(out["threads"][0]["machine"], "local");
        assert_eq!(out["threads"][0]["runOrdinal"], 2);
        assert_eq!(out["threads"][0]["title"], "mock local");
    }
}

#[test]
fn local_backend_missing_thread_exits_2() {
    let env = Env::new();
    let t3 = MockT3::new(&env);
    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--transport",
        "local",
        "--t3-bin",
        t3.bin_str(),
        "nope",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(output_json(&out)["error"]["kind"], "not_found");
}

#[test]
fn timeout_exits_3_with_last_known_state() {
    let env = Env::new();
    let t3 = MockT3::new(&env);
    t3.set("lt-2", "running", 1);
    let out = env.run(&[
        "wait",
        "--transport",
        "local",
        "--t3-bin",
        t3.bin_str(),
        "--interval",
        "100ms",
        "--timeout",
        "700ms",
        "lt-2",
    ]);
    assert_eq!(out.status.code(), Some(3));
    let v = output_json(&out);
    assert_eq!(v["event"], "timeout");
    assert_eq!(v["threads"][0]["status"], "running");
}

#[test]
fn headless_oauth_flow_stores_private_read_only_token() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = Env::new();
    let out = env.run(&["auth", "oauth-start", &mock.url, "--name", "relay", "--default"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let started = output_json(&out);
    let url = started["authorizeUrl"].as_str().unwrap();
    let reg = mock.state.0.lock().unwrap().register_bodies[0].clone();
    assert_eq!(reg["scope"], "orchestration:read");
    assert_eq!(reg["token_endpoint_auth_method"], "none");

    // The "browser": approve and capture the redirect it would fail to load.
    let (status, location) = http_get(url);
    assert_eq!(status, 302);
    let callback = location.unwrap();
    assert!(callback.starts_with("http://127.0.0.1:8976/callback?code="));

    let out = env.run(&["auth", "oauth-finish", &callback]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let done = output_json(&out);
    assert_eq!(done["machine"], "relay");
    assert_eq!(done["scope"], "orchestration:read");
    assert_eq!(done["default"], true);
    for o in [&out.stdout, &out.stderr] {
        assert!(!String::from_utf8_lossy(o).contains(TOKEN), "token leaked to output");
    }
    assert_eq!(mode(&env.home.join("machines.json")), 0o600);
    assert_eq!(mode(&env.home), 0o700);
    let saved: Value = serde_json::from_str(&fs::read_to_string(env.home.join("machines.json")).unwrap()).unwrap();
    assert_eq!(saved["machines"]["relay"]["accessToken"], TOKEN);
    assert_eq!(
        fs::read_dir(env.home.join("pending")).unwrap().count(),
        0,
        "pending login not cleaned up"
    );

    // A replayed callback is rejected.
    assert_eq!(env.run(&["auth", "oauth-finish", &callback]).status.code(), Some(2));

    let list = output_json(&env.run(&["auth", "list"]));
    assert_eq!(list["machines"][0]["name"], "relay");
    assert!(!list.to_string().contains(TOKEN));

    // The saved default machine now serves bare thread ids.
    let out = env.run(&["wait", "--no-daemon", T]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(output_json(&out)["threads"][0]["machine"], "relay");
}

#[test]
fn loopback_oauth_login_completes() {
    let mock = MockMcp::start();
    let env = Env::new();
    let mut child = env
        .cmd(&["auth", "login", &mock.url, "--no-browser", "--name", "lb"])
        .spawn()
        .unwrap();
    // Read the authorize URL the CLI prints to stderr.
    let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut url = String::new();
    let deadline = Instant::now() + SECS(10);
    while !url.starts_with("http") {
        assert!(Instant::now() < deadline);
        url.clear();
        std::io::BufRead::read_line(&mut stderr, &mut url).unwrap();
        url = url.trim().to_string();
    }
    let (status, location) = http_get(&url);
    assert_eq!(status, 302);
    let (status, _) = http_get(&location.unwrap());
    assert_eq!(status, 200);
    let (code, out, _) = finish(child, SECS(10));
    assert_eq!(code, 0);
    assert_eq!(out["machine"], "lb");
    assert_eq!(mode(&env.home.join("machines.json")), 0o600);
}

#[test]
fn usage_errors_exit_2() {
    let env = Env::new();
    assert_eq!(env.run(&["wait"]).status.code(), Some(2));
    assert_eq!(env.run(&["wait", "--bogus", "x"]).status.code(), Some(2));
    assert_eq!(env.run(&["wait", "--timeout", "5parsecs", "x"]).status.code(), Some(2));
    let out = env.run(&["wait", "https://unknown.example/env/thread"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(output_json(&out)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("auth login"));
}

#[test]
fn persistent_400_is_a_hard_error_not_a_retry_loop() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 1);
    mock.with(|s| s.reject_tools_with = Some(400));
    let env = mcp_env(&mock);
    let started = Instant::now();
    let out = env.run(&["wait", "--no-daemon", T]);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(started.elapsed() < SECS(5));
    assert_eq!(output_json(&out)["error"]["kind"], "fatal");
    // One retry on a fresh session, then give up.
    assert_eq!(mock.calls("initialize"), 2);
    assert_eq!(mock.calls("rejected:400"), 2);
}

#[test]
fn until_delegated_is_rejected_for_local_threads() {
    let env = Env::new();
    let t3 = MockT3::new(&env);
    t3.set("lt-3", "completed", 1);
    // Explicit local transport, and a bare id that auto-resolves to local.
    for extra in [&["--transport", "local"][..], &[][..]] {
        let mut args = vec!["wait", "--t3-bin", t3.bin_str(), "--until-delegated-settled"];
        args.extend_from_slice(extra);
        args.push("lt-3");
        let out = env.run(&args);
        assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stdout));
        let v = output_json(&out);
        assert_eq!(v["error"]["kind"], "usage");
        assert!(v["error"]["message"].as_str().unwrap().contains("mcp transport"), "{v}");
    }
    assert_eq!(t3.calls(), 0, "nothing may be polled for a rejected request");
    assert_eq!(env.status()["running"], false, "no daemon for a rejected request");
}

#[test]
fn shared_local_watch_polls_at_the_shortest_interval() {
    let env = Env::new();
    let t3 = MockT3::new(&env);
    t3.set("lt-4", "running", 1);
    let base = ["wait", "--transport", "local", "--t3-bin", t3.bin_str()];
    let slow = env.spawn(&[&base[..], &["--interval", "1h", "lt-4"]].concat());
    env.wait_status("slow subscriber", SECS(10), |s| Env::subscribers(s) == 1);
    let fast = env.spawn(&[&base[..], &["--interval", "100ms", "lt-4"]].concat());
    env.wait_status("both subscribers", SECS(10), |s| Env::subscribers(s) == 2);
    std::thread::sleep(Duration::from_millis(300));
    t3.set("lt-4", "completed", 1);
    // The fast client's interval drives the shared worker, so both wake
    // within a few polls instead of an hour.
    for c in [fast, slow] {
        let (code, out, err) = finish(c, SECS(5));
        assert_eq!(code, 0, "stderr: {err}");
        assert_eq!(out["threads"][0]["status"], "completed");
    }
}

#[test]
fn custom_socket_dir_must_be_private_and_is_never_chmodded() {
    let env = Env::new();
    let t3 = MockT3::new(&env);
    t3.set("lt-5", "completed", 1);
    let shared = env.home.join("shared");
    fs::create_dir_all(&shared).unwrap();
    fs::set_permissions(&shared, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let sock = shared.join("w.sock");
    let out = env
        .cmd(&["wait", "--transport", "local", "--t3-bin", t3.bin_str(), "lt-5"])
        .env("T3_WATCH_SOCKET", &sock)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(output_json(&out)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("other users"));
    assert_eq!(
        mode(&shared),
        0o755,
        "t3-watch must not chmod a directory it does not own"
    );
    assert!(!sock.exists());

    // A private directory of the user's choosing works, lock file included.
    let private = env.home.join("private");
    let sock = private.join("w.sock");
    let out = env
        .cmd(&["wait", "--transport", "local", "--t3-bin", t3.bin_str(), "lt-5"])
        .env("T3_WATCH_SOCKET", &sock)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(mode(&private), 0o700);
    assert_eq!(mode(&private.join("w.sock.lock")), 0o600);
    let _ = env.cmd(&["stop"]).env("T3_WATCH_SOCKET", &sock).output();
}
