//! End-to-end tests for the hardening work: wake notifications, delegated
//! run binding, status classification, strict OAuth discovery, config
//! transactions and refresh, bounded SSE, socket safety, backend keys,
//! limits, client-side path resolution, IPC framing and loopback login.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

const T: &str = "mcp:launch:hard1";

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn mcp_env(mock: &MockMcp) -> Env {
    let env = Env::new();
    env.add_machine("mock", &mock.url, TOKEN);
    env
}

fn without_notification(v: &Value) -> Value {
    let mut v = v.clone();
    v.as_object_mut().unwrap().remove("notification");
    v
}

fn stdout_and_stderr(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Runs oauth-start for `name`, approves in the "browser", and returns the
/// callback URL the browser would have been sent to.
fn approved_callback(env: &Env, mock: &MockMcp, name: &str) -> String {
    let out = env.run(&["auth", "oauth-start", &mock.url, "--name", name]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout_and_stderr(&out));
    let url = output_json(&out)["authorizeUrl"].as_str().unwrap().to_string();
    let (status, location) = http_get(&url);
    assert_eq!(status, 302);
    location.unwrap()
}

// ---- 0: one daemon per socket, flock-based --------------------------------

#[test]
fn only_one_daemon_runs_per_socket() {
    let env = Env::new();
    let mut first = env.spawn(&["daemon", "--idle-exit", "30s"]);
    env.wait_status("first daemon", secs(10), |s| s["running"] == true);
    let pid = env.status()["pid"].as_u64().unwrap();
    assert_eq!(pid, first.id() as u64);
    let started = Instant::now();
    let second = env.run(&["daemon"]);
    assert_eq!(second.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&second.stderr).contains("another daemon holds the lock"));
    assert!(
        started.elapsed() < secs(3),
        "the loser must give up at once when the winner answers"
    );
    assert_eq!(env.status()["pid"].as_u64().unwrap(), pid);
    env.run(&["stop"]);
    first.wait().unwrap();
}

// ---- 1: notifications ------------------------------------------------------

#[test]
fn webhook_gets_a_signed_result_and_secrets_stay_out_of_output() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let hook = MockHook::start();
    let env = mcp_env(&mock);
    let secret_file = env.home.join("hook-secret");
    fs::write(&secret_file, "s3cret-key\n").unwrap();
    let out = env
        .cmd(&[
            "wait",
            "--no-daemon",
            "--webhook-url-env",
            "T3W_URL",
            "--webhook-secret-file",
            secret_file.to_str().unwrap(),
            "--webhook-header-env",
            "Authorization=T3W_AUTH",
            T,
        ])
        .env("T3W_URL", &hook.url)
        .env("T3W_AUTH", "Bearer hook-token-xyz")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stdout_and_stderr(&out));
    let v = output_json(&out);
    assert_eq!(v["event"], "settled");
    assert_eq!(v["notification"]["delivered"], true);
    assert_eq!(v["notification"]["webhook"]["attempts"], 1);
    assert_eq!(v["notification"]["webhook"]["status"], 200);

    let reqs = hook.requests();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.path, "/hooks/secret-path-123");
    assert_eq!(r.headers["authorization"], "Bearer hook-token-xyz");
    assert_eq!(r.headers["x-t3-watch-event"], "settled");
    assert_eq!(r.headers["content-type"], "application/json");
    let id = &r.headers["x-t3-watch-id"];
    assert_eq!(v["notification"]["id"], id.as_str());
    let ts = &r.headers["x-t3-watch-timestamp"];
    let mut msg = format!("{ts}.{id}.").into_bytes();
    msg.extend_from_slice(&r.body);
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"s3cret-key");
    let expected = format!("sha256={}", hex(ring::hmac::sign(&key, &msg).as_ref()));
    assert_eq!(r.headers["x-t3-watch-signature"], expected);
    let sent: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(
        sent,
        without_notification(&v),
        "the webhook gets exactly the printed result"
    );

    let all = stdout_and_stderr(&out);
    for secret in ["secret-path-123", "s3cret-key", "hook-token-xyz", "receiver body"] {
        assert!(!all.contains(secret), "{secret} leaked into output");
    }
}

#[test]
fn webhook_retries_transient_errors_and_honors_retry_after() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let hook = MockHook::start();
    hook.push(503, &[("Retry-After", "1")]);
    hook.push(502, &[]);
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--webhook-url", &hook.url, T]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout_and_stderr(&out));
    let v = output_json(&out);
    assert_eq!(v["notification"]["webhook"]["attempts"], 3);
    let reqs = hook.requests();
    assert_eq!(reqs.len(), 3);
    assert!(
        reqs[1].at - reqs[0].at >= Duration::from_millis(950),
        "Retry-After ignored"
    );
    let ids: Vec<_> = reqs.iter().map(|r| r.headers["x-t3-watch-id"].clone()).collect();
    assert!(ids.iter().all(|i| *i == ids[0]), "retries keep one event id");
}

#[test]
fn webhook_permanent_errors_and_redirects_exit_4_but_keep_settled() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);

    let hook = MockHook::start();
    hook.push(400, &[]);
    let out = env.run(&["wait", "--no-daemon", "--webhook-url", &hook.url, T]);
    assert_eq!(out.status.code(), Some(4), "{}", stdout_and_stderr(&out));
    let v = output_json(&out);
    assert_eq!(v["event"], "settled");
    assert_eq!(v["threads"][0]["settled"], true);
    assert_eq!(v["notification"]["delivered"], false);
    assert_eq!(v["notification"]["webhook"]["attempts"], 1, "4xx must not be retried");
    assert_eq!(v["notification"]["webhook"]["status"], 400);
    assert_eq!(hook.requests().len(), 1);

    let redirect = MockHook::start();
    redirect.push(302, &[("Location", "https://elsewhere.example/steal")]);
    let out = env.run(&["wait", "--no-daemon", "--webhook-url", &redirect.url, T]);
    assert_eq!(out.status.code(), Some(4));
    let v = output_json(&out);
    assert!(v["notification"]["webhook"]["error"]
        .as_str()
        .unwrap()
        .contains("redirect"));
    assert_eq!(redirect.requests().len(), 1, "redirects are not followed");
}

#[test]
fn webhook_gives_up_when_retry_after_exceeds_the_budget() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let hook = MockHook::start();
    hook.push(429, &[("Retry-After", "3600")]);
    let env = mcp_env(&mock);
    let started = Instant::now();
    let out = env.run(&["wait", "--no-daemon", "--webhook-url", &hook.url, T]);
    assert_eq!(out.status.code(), Some(4));
    assert!(started.elapsed() < secs(10));
    let v = output_json(&out);
    assert_eq!(v["notification"]["webhook"]["attempts"], 1);
    assert!(v["notification"]["webhook"]["error"]
        .as_str()
        .unwrap()
        .contains("budget"));
}

#[test]
fn insecure_or_malformed_notification_options_are_rejected_up_front() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    let hook = MockHook::start();

    let out = env.run(&["wait", "--no-daemon", "--webhook-url", "http://hooks.example/x", T]);
    assert_eq!(out.status.code(), Some(2));
    let msg = output_json(&out)["error"]["message"].as_str().unwrap().to_string();
    assert!(msg.contains("https") && !msg.contains("hooks.example"), "{msg}");

    let out = env
        .cmd(&[
            "wait",
            "--no-daemon",
            "--webhook-url",
            &hook.url,
            "--webhook-header-env",
            "X-Token=T3W_H",
            T,
        ])
        .env("T3W_H", "a\r\nInjected: 1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(output_json(&out)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("CR, LF or NUL"));

    let out = env.run(&["wait", "--no-daemon", "--webhook-url-env", "T3W_SURELY_UNSET", T]);
    assert_eq!(out.status.code(), Some(2));

    let out = env.run(&["wait", "--no-daemon", "--on-settle-arg", "x", T]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(mock.calls("initialize"), 0, "nothing may run for a rejected request");
    assert!(hook.requests().is_empty());
}

#[test]
fn on_settle_hook_gets_json_on_stdin_without_a_shell_and_without_secrets() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    let hook_srv = MockHook::start();
    let dir = env.home.join("hook");
    fs::create_dir_all(&dir).unwrap();
    let prog = dir.join("on-settle.sh");
    script(
        &prog,
        r#"cat > "$1"
printf '%s\n' "$@" > "$1.args"
env > "$1.env"
echo HOOK-STDOUT-MARKER"#,
    );
    let out_file = dir.join("payload.json");
    let pwned = dir.join("pwned");
    let injection = format!("$(touch {})", pwned.display());
    let out = env
        .cmd(&[
            "wait",
            "--no-daemon",
            "--on-settle",
            prog.to_str().unwrap(),
            "--on-settle-arg",
            out_file.to_str().unwrap(),
            "--on-settle-arg",
            &injection,
            "--on-settle-arg",
            "; echo injected",
            "--webhook-url",
            &hook_srv.url,
            "--webhook-secret-env",
            "T3W_SECRET",
            T,
        ])
        .env("T3W_SECRET", "supersecret-value")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stdout_and_stderr(&out));
    let v = output_json(&out);
    assert_eq!(v["notification"]["onSettle"]["delivered"], true);
    assert_eq!(v["notification"]["onSettle"]["exitCode"], 0);
    let payload: Value = serde_json::from_str(&fs::read_to_string(&out_file).unwrap()).unwrap();
    assert_eq!(payload, without_notification(&v));
    let args = fs::read_to_string(dir.join("payload.json.args")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(
        args,
        [out_file.to_str().unwrap(), injection.as_str(), "; echo injected"]
    );
    assert!(!pwned.exists(), "arguments must not go through a shell");
    let hook_env = fs::read_to_string(dir.join("payload.json.env")).unwrap();
    assert!(!hook_env.contains("supersecret-value") && !hook_env.contains("T3W_SECRET"));
    assert!(hook_env.contains("T3_WATCH_EVENT=settled"));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("HOOK-STDOUT-MARKER"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("HOOK-STDOUT-MARKER"));
}

#[test]
fn slow_hook_is_killed_with_its_children_and_exit_is_4() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    let prog = env.home.join("slow.sh");
    script(&prog, "sleep 30 &\necho $! > \"$1\"\nwait");
    let pidfile = env.home.join("child.pid");
    let started = Instant::now();
    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--on-settle",
        prog.to_str().unwrap(),
        "--on-settle-arg",
        pidfile.to_str().unwrap(),
        "--on-settle-timeout",
        "1s",
        T,
    ]);
    assert_eq!(out.status.code(), Some(4), "{}", stdout_and_stderr(&out));
    assert!(started.elapsed() < secs(10));
    let v = output_json(&out);
    assert_eq!(v["event"], "settled");
    assert_eq!(v["notification"]["onSettle"]["timedOut"], true);
    let child: i32 = fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    let deadline = Instant::now() + secs(2);
    while pid_alive(child) {
        assert!(Instant::now() < deadline, "the hook's background child survived");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn notify_on_all_also_reports_timeouts_and_auth_errors() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 1);
    let env = mcp_env(&mock);
    let hook = MockHook::start();

    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--timeout",
        "500ms",
        "--webhook-url",
        &hook.url,
        T,
    ]);
    assert_eq!(out.status.code(), Some(3));
    assert!(output_json(&out).get("notification").is_none());
    assert!(hook.requests().is_empty(), "default is settle-only");

    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--timeout",
        "500ms",
        "--notify-on",
        "all",
        "--webhook-url",
        &hook.url,
        T,
    ]);
    assert_eq!(out.status.code(), Some(3), "a delivered timeout keeps exit 3");
    assert_eq!(output_json(&out)["notification"]["delivered"], true);
    let reqs = hook.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].headers["x-t3-watch-event"], "timeout");

    env.add_machine("mock", &mock.url, "revoked-token");
    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--notify-on",
        "all",
        "--webhook-url",
        &hook.url,
        T,
    ]);
    assert_eq!(out.status.code(), Some(2));
    let reqs = hook.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[1].headers["x-t3-watch-event"], "error");
    let body: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    assert_eq!(body["error"]["kind"], "auth");
}

#[test]
fn closed_stdout_neither_panics_nor_skips_the_notification() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 1);
    let env = mcp_env(&mock);
    let hook = MockHook::start();
    let mut child = env.spawn(&["wait", "--no-daemon", "--webhook-url", &hook.url, T]);
    drop(child.stdout.take());
    still_running(&mut child, Duration::from_millis(300));
    mock.set_thread(T, "completed", 1);
    let (code, _, err) = finish(child, secs(10));
    assert_eq!(code, 0, "stderr: {err}");
    assert!(!err.contains("panicked"), "{err}");
    assert_eq!(hook.requests().len(), 1);
}

// ---- 2: delegated work is bound to the run --------------------------------

#[test]
fn delegated_proof_is_bound_to_the_run_the_read_saw() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    // Run 2 starts (and finishes its own turn) between the wait and the read,
    // but its delegated work is still running.
    mock.with(|s| s.advance_after_wait = Some((T.into(), "completed".into(), 2, true)));
    let env = mcp_env(&mock);
    let mut child = env.spawn(&["wait", "--no-daemon", "--until-delegated-settled", "--slice", "1s", T]);
    still_running(&mut child, Duration::from_millis(1500));
    assert!(
        mock.calls("t3_thread_wait") >= 2,
        "the newer run must be waited on again"
    );
    mock.with(|s| {
        s.delegated_pending.remove(T);
    });
    let (code, out, err) = finish(child, secs(10));
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out["threads"][0]["runOrdinal"], 2);
    assert_eq!(out["threads"][0]["delegatedSettled"], true);
    assert_eq!(out["untilDelegatedSettled"], true);
}

// ---- 4: unknown statuses never settle --------------------------------------

#[test]
fn unknown_or_empty_statuses_never_settle() {
    for status in ["archived_v9", ""] {
        let mock = MockMcp::start();
        mock.set_thread(T, status, 1);
        let env = mcp_env(&mock);
        let out = env.run(&["wait", "--no-daemon", "--timeout", "1500ms", T]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "status {status:?}: {}",
            stdout_and_stderr(&out)
        );
        let v = output_json(&out);
        assert_eq!(v["threads"][0]["settled"], false);
        assert!(
            v["threads"][0]["diagnostic"].as_str().unwrap().contains("status"),
            "{v}"
        );
    }
}

// ---- 5: strict discovery and https -----------------------------------------

#[test]
fn discovery_rejects_mismatched_or_insecure_metadata() {
    type Tweak = fn(&mut MockState, &str);
    let cases: [(&str, Tweak, &str); 5] = [
        (
            "issuer",
            |s, _| {
                s.asm_override.insert("issuer".into(), json!("http://127.0.0.1:1"));
            },
            "issuer",
        ),
        (
            "resource",
            |s, base| {
                s.prm_override.insert("resource".into(), json!(format!("{base}/other")));
            },
            "resource",
        ),
        (
            "plain pkce",
            |s, _| {
                s.asm_override
                    .insert("code_challenge_methods_supported".into(), json!(["plain"]));
            },
            "S256",
        ),
        (
            "no pkce",
            |s, _| {
                s.asm_override
                    .insert("code_challenge_methods_supported".into(), Value::Null);
            },
            "PKCE",
        ),
        (
            "http endpoint",
            |s, _| {
                s.asm_override
                    .insert("token_endpoint".into(), json!("http://evil.example/token"));
            },
            "https",
        ),
    ];
    for (name, tweak, expect) in cases {
        let mock = MockMcp::start();
        let base = mock.url.clone();
        mock.with(|s| tweak(s, &base));
        let env = Env::new();
        let out = env.run(&["auth", "oauth-start", &mock.url, "--name", "x"]);
        assert_eq!(out.status.code(), Some(2), "{name}: {}", stdout_and_stderr(&out));
        let msg = output_json(&out)["error"]["message"].as_str().unwrap().to_string();
        assert!(msg.contains(expect), "{name}: {msg}");
        assert!(
            mock.state.0.lock().unwrap().register_bodies.is_empty(),
            "{name}: registered anyway"
        );
    }
}

#[test]
fn issuer_with_a_path_uses_the_rfc8414_metadata_location() {
    let mock = MockMcp::start();
    mock.with(|s| s.issuer_path = "/tenant/a".into());
    let env = Env::new();
    let callback = approved_callback(&env, &mock, "tenant");
    let out = env.run(&["auth", "oauth-finish", &callback]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout_and_stderr(&out));
    assert_eq!(
        env.machines()["machines"]["tenant"]["issuer"],
        format!("{}/tenant/a", mock.url)
    );
}

#[test]
fn plain_http_mcp_origins_are_refused_before_any_request() {
    let env = Env::new();
    env.add_machine("insecure", "http://relay.example", TOKEN);
    let out = env.run(&["wait", "--no-daemon", "--machine", "insecure", T]);
    assert_eq!(out.status.code(), Some(2));
    let v = output_json(&out);
    assert_eq!(v["error"]["kind"], "config");
    assert!(v["error"]["message"].as_str().unwrap().contains("https"));
    let out = env.run(&["auth", "discover", "http://relay.example"]);
    assert_eq!(out.status.code(), Some(2));
}

// ---- 6: config transactions and token refresh ------------------------------

#[test]
fn concurrent_oauth_finishes_keep_every_machine() {
    let mock = MockMcp::start();
    let env = Env::new();
    let callbacks: Vec<String> = (0..6)
        .map(|i| approved_callback(&env, &mock, &format!("m{i}")))
        .collect();
    let children: Vec<_> = callbacks
        .iter()
        .map(|c| env.spawn(&["auth", "oauth-finish", c]))
        .collect();
    for c in children {
        let (code, out, err) = finish(c, secs(30));
        assert_eq!(code, 0, "{out} {err}");
    }
    let m = env.machines();
    let names: Vec<&String> = m["machines"].as_object().unwrap().keys().collect();
    assert_eq!(names.len(), 6, "a concurrent writer lost machines: {names:?}");
    let ids: std::collections::HashSet<_> = m["machines"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| v["credentialId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 6, "each pairing gets its own credential id");
}

#[test]
fn oauth_finish_saves_before_retiring_the_pending_login() {
    let mock = MockMcp::start();
    let env = Env::new();
    let callback = approved_callback(&env, &mock, "late");
    let pending = env.home.join("pending");
    // Saving cannot work while machines.json is a directory.
    fs::create_dir_all(env.home.join("machines.json")).unwrap();
    let out = env.run(&["auth", "oauth-finish", &callback]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        fs::read_dir(&pending).unwrap().count(),
        1,
        "pending login must survive a failed save"
    );
    fs::remove_dir(env.home.join("machines.json")).unwrap();
    let out = env.run(&["auth", "oauth-finish", &callback]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout_and_stderr(&out));
    assert_eq!(fs::read_dir(&pending).unwrap().count(), 0);
    assert_eq!(env.machines()["machines"]["late"]["accessToken"], TOKEN);
}

#[test]
fn expired_token_is_refreshed_once_and_saved() {
    let mock = MockMcp::start();
    mock.with(|s| {
        s.issue_refresh = Some("rt-1".into());
        s.refresh_to = Some("token-2".into());
    });
    let env = Env::new();
    let callback = approved_callback(&env, &mock, "mock");
    assert_eq!(env.run(&["auth", "oauth-finish", &callback]).status.code(), Some(0));
    assert_eq!(env.machines()["machines"]["mock"]["refreshToken"], "rt-1");
    assert_eq!(
        mock.state.0.lock().unwrap().register_bodies[0]["grant_types"],
        json!(["authorization_code", "refresh_token"])
    );

    mock.set_thread(T, "running", 1);
    let mut child = env.spawn(&["wait", "--no-daemon", "--slice", "300ms", T]);
    still_running(&mut child, Duration::from_millis(500));
    mock.with(|s| s.token = "token-2".into()); // revokes the first token
    still_running(&mut child, Duration::from_millis(1200));
    mock.set_thread(T, "completed", 1);
    let (code, out, err) = finish(child, secs(10));
    assert_eq!(code, 0, "{out} {err}");
    assert_eq!(mock.calls("refresh"), 1);
    let saved = env.machines();
    assert_eq!(saved["machines"]["mock"]["accessToken"], "token-2");
    assert_eq!(saved["machines"]["mock"]["refreshToken"], "rt-1");
}

#[test]
fn a_token_from_an_external_login_is_picked_up_on_401() {
    let mock = MockMcp::start();
    mock.with(|s| s.token = "token-A".into());
    mock.set_thread(T, "running", 1);
    let env = Env::new();
    env.add_machine("mock", &mock.url, "token-A");
    let mut child = env.spawn(&["wait", "--slice", "300ms", T]);
    env.wait_status("subscriber", secs(10), |s| Env::subscribers(s) == 1);
    // Someone runs `auth login` again elsewhere; the old token stops working.
    env.add_machine("mock", &mock.url, "token-B");
    mock.with(|s| s.token = "token-B".into());
    still_running(&mut child, Duration::from_millis(1200));
    let status = env.status();
    assert_eq!(status["machines"][0]["tokenRecoveries"], 1, "{status}");
    mock.set_thread(T, "completed", 1);
    let (code, _, err) = finish(child, secs(10));
    assert_eq!(code, 0, "{err}");
}

// ---- 7: bounded SSE --------------------------------------------------------

#[test]
fn an_endless_event_stream_is_cut_off_without_waking() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    mock.with(|s| {
        s.sse = true;
        s.sse_flood = Some(20 << 20);
    });
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--timeout", "3s", T]);
    assert_eq!(out.status.code(), Some(3), "{}", stdout_and_stderr(&out));
    let v = output_json(&out);
    assert!(
        v["threads"][0]["lastError"].as_str().unwrap().contains("larger than"),
        "{v}"
    );
}

// ---- 8: socket and lock file safety ----------------------------------------

#[test]
fn something_else_at_the_socket_path_is_never_deleted() {
    let env = Env::new();
    let t3 = MockT3::new(&env);
    t3.set("lt", "completed", 1);
    let sock = env.home.join("daemon.sock");
    fs::write(&sock, "not a socket").unwrap();
    let out = env.run(&["wait", "--transport", "local", "--t3-bin", t3.bin_str(), "lt"]);
    assert_eq!(out.status.code(), Some(2), "{}", stdout_and_stderr(&out));
    assert!(output_json(&out)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not a socket"));
    assert_eq!(fs::read_to_string(&sock).unwrap(), "not a socket");
}

#[test]
fn a_symlinked_lock_file_is_refused() {
    let env = Env::new();
    let victim = env.home.join("victim");
    fs::write(&victim, "keep").unwrap();
    std::os::unix::fs::symlink(&victim, env.home.join("daemon.sock.lock")).unwrap();
    let out = env.run(&["daemon"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("lock"));
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
}

#[test]
fn clients_refuse_a_socket_open_to_other_users() {
    let env = Env::new();
    let t3 = MockT3::new(&env);
    t3.set("lt", "running", 1);
    let mut child = env.spawn(&["wait", "--transport", "local", "--t3-bin", t3.bin_str(), "lt"]);
    env.wait_status("subscriber", secs(10), |s| Env::subscribers(s) == 1);
    let sock = env.home.join("daemon.sock");
    fs::set_permissions(&sock, std::os::unix::fs::PermissionsExt::from_mode(0o666)).unwrap();
    let out = env.run(&["status"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(output_json(&out)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("other users"));
    fs::set_permissions(&sock, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
}

// ---- 9: backends are keyed by origin and pairing ---------------------------

#[test]
fn re_pairing_a_machine_never_reuses_the_old_backend() {
    let mock = MockMcp::start();
    mock.with(|s| {
        s.extra_tokens.insert("token-B".into());
    });
    mock.set_thread(T, "running", 1);
    let env = Env::new();
    let write = |token: &str, cred: &str| {
        let cfg = json!({"machines": {"mock": {"origin": mock.url, "accessToken": token, "credentialId": cred}}});
        fs::write(env.home.join("machines.json"), cfg.to_string()).unwrap();
    };
    write(TOKEN, "cred-1");
    let first = env.spawn(&["wait", "--machine", "mock", T]);
    env.wait_status("first", secs(10), |s| Env::subscribers(s) == 1);
    write("token-B", "cred-2");
    let second = env.spawn(&["wait", "--machine", "mock", T]);
    let status = env.wait_status("second", secs(10), |s| Env::subscribers(s) == 2);
    assert_eq!(status["watches"].as_array().unwrap().len(), 2, "{status}");
    let machines = status["machines"].as_array().unwrap();
    assert_eq!(machines.len(), 2);
    assert_ne!(machines[0]["fingerprint"], machines[1]["fingerprint"]);
    let deadline = Instant::now() + secs(5);
    while mock.calls("initialize") < 2 {
        assert!(
            Instant::now() < deadline,
            "the new pairing never opened its own session"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(mock.calls("initialize"), 2, "exactly one session per pairing");
    mock.set_thread(T, "completed", 1);
    for c in [first, second] {
        let (code, _, err) = finish(c, secs(10));
        assert_eq!(code, 0, "{err}");
    }
}

// ---- 10: limits and prompt cleanup -----------------------------------------

#[test]
fn slice_is_capped_at_ten_minutes() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    assert_eq!(
        env.run(&["wait", "--no-daemon", "--slice", "2h", T]).status.code(),
        Some(0)
    );
    assert_eq!(mock.state.0.lock().unwrap().wait_args[0]["timeoutMs"], 600_000);
}

#[test]
fn watch_and_connection_limits_are_enforced() {
    let mock = MockMcp::start();
    mock.set_thread("a", "running", 1);
    mock.set_thread("b", "running", 1);
    let env = mcp_env(&mock);
    let out = env
        .cmd(&["wait", "--no-daemon", "a", "b"])
        .env("T3_WATCH_MAX_WATCHES", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(output_json(&out)["error"]["kind"], "limit");

    let spawn_limited = || {
        env.cmd(&["wait", "a"])
            .env("T3_WATCH_MAX_CONNECTIONS", "2")
            .spawn()
            .unwrap()
    };
    let first = spawn_limited();
    let second = spawn_limited();
    std::thread::sleep(Duration::from_millis(1500));
    let out = env
        .cmd(&["wait", "a"])
        .env("T3_WATCH_MAX_CONNECTIONS", "2")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stdout_and_stderr(&out));
    assert_eq!(output_json(&out)["error"]["kind"], "limit");
    // Control requests never count against the wait limit, so a full daemon
    // can still be inspected and stopped.
    let status = env.status();
    assert_eq!(status["running"], true, "{status}");
    assert_eq!(status["activeWaits"], 2);
    mock.set_thread("a", "completed", 1);
    for c in [first, second] {
        let (code, _, err) = finish(c, secs(10));
        assert_eq!(code, 0, "{err}");
    }
}

#[test]
fn a_dropped_client_releases_its_long_poll_watch_at_once() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 1);
    let env = mcp_env(&mock);
    let mut child = env.spawn(&["wait", "--slice", "60s", T]);
    env.wait_status("subscriber", secs(10), |s| Env::subscribers(s) == 1);
    child.kill().unwrap();
    child.wait().unwrap();
    let started = Instant::now();
    env.wait_status("watch removed", secs(5), |s| {
        s["watches"].as_array().is_some_and(|w| w.is_empty())
    });
    assert!(started.elapsed() < secs(3), "the watch lingered for its long poll");
}

// ---- 12: local paths are resolved by the client ----------------------------

#[test]
fn relative_local_paths_are_resolved_by_the_client() {
    let env = Env::new();
    let a = MockT3::in_dir(env.home.join("a"));
    let b = MockT3::in_dir(env.home.join("b"));
    a.set("rel", "running", 1);
    b.set("rel", "running", 1);
    // Same relative path, different working directories: two different
    // binaries, so two different watches. The daemon runs from `/`, so it
    // could only find either one through an absolute path.
    let in_a = env
        .cmd(&[
            "wait",
            "--transport",
            "local",
            "--t3-bin",
            "./t3",
            "--interval",
            "100ms",
            "rel",
        ])
        .current_dir(&a.dir)
        .spawn()
        .unwrap();
    let in_b = env
        .cmd(&["wait", "--transport", "local", "--interval", "100ms", "rel"])
        .current_dir(&b.dir)
        .env("T3_WATCH_T3_BIN", "t3")
        .spawn()
        .unwrap();
    let status = env.wait_status("two watches", secs(10), |s| {
        s["watches"].as_array().is_some_and(|w| w.len() == 2)
    });
    let bins: Vec<String> = status["watches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["t3Bin"].as_str().unwrap().to_string())
        .collect();
    assert!(bins.iter().all(|p| Path::new(p).is_absolute()), "{bins:?}");
    assert_ne!(bins[0], bins[1]);
    a.set("rel", "completed", 1);
    b.set("rel", "completed", 1);
    for c in [in_a, in_b] {
        let (code, _, err) = finish(c, secs(10));
        assert_eq!(code, 0, "{err}");
    }
}

// ---- 14: IPC framing and protocol version ----------------------------------

#[test]
fn the_daemon_rejects_oversized_or_unterminated_requests() {
    let env = Env::new();
    let mut daemon = env.spawn(&["daemon", "--idle-exit", "30s"]);
    env.wait_status("daemon", secs(10), |s| s["running"] == true);
    let sock = env.home.join("daemon.sock");

    let s = UnixStream::connect(&sock).unwrap();
    let mut w = s.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = w.write_all(&vec![b'x'; 300 * 1024]);
    });
    let mut reply = String::new();
    let _ = BufReader::new(&s).read_line(&mut reply);
    writer.join().unwrap();
    assert!(reply.contains("longer than"), "{reply}");

    let mut s = UnixStream::connect(&sock).unwrap();
    s.write_all(br#"{"op":"ping"}"#).unwrap();
    s.shutdown(std::net::Shutdown::Write).unwrap();
    let mut reply = String::new();
    s.read_to_string(&mut reply).unwrap();
    assert!(reply.contains("newline"), "{reply}");

    assert_eq!(env.status()["running"], true, "the daemon survives bad clients");
    env.run(&["stop"]);
    daemon.wait().unwrap();
}

#[test]
fn an_unsupported_mcp_protocol_version_is_a_hard_error() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 1);
    mock.with(|s| s.protocol_version = "1999-01-01".into());
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", T]);
    assert_eq!(out.status.code(), Some(2));
    let v = output_json(&out);
    assert_eq!(v["error"]["kind"], "fatal");
    assert!(v["error"]["message"].as_str().unwrap().contains("protocol"));
}

// ---- 15: loopback login and error hygiene ----------------------------------

#[test]
fn loopback_login_ignores_stray_requests_and_reports_after_saving() {
    let mock = MockMcp::start();
    let env = Env::new();
    let mut child = env
        .cmd(&["auth", "login", &mock.url, "--no-browser", "--name", "lb"])
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
    let callback_base = q["redirect_uri"].clone();
    let origin = callback_base.trim_end_matches("/callback");
    assert_eq!(http_get(&format!("{origin}/favicon.ico")).0, 404);
    assert_eq!(http_get(&format!("{callback_base}?state=wrong&code=x")).0, 400);
    let (status, location) = http_get(&url);
    assert_eq!(status, 302);
    let (status, _, body) = http_get_full(&location.unwrap());
    assert_eq!(status, 200);
    assert!(body.contains("paired"), "{body}");
    assert!(env.home.join("machines.json").exists(), "success shown before the save");
    let (code, out, _) = finish(child, secs(10));
    assert_eq!(code, 0);
    assert_eq!(out["machine"], "lb");
}

#[test]
fn token_endpoint_errors_never_echo_the_response_body() {
    let mock = MockMcp::start();
    mock.with(|s| {
        s.token_error_body =
            Some(r#"{"error":"invalid_grant","error_description":"SECRET-ECHO-123 refresh_token=leaky"}"#.into())
    });
    let env = Env::new();
    let callback = approved_callback(&env, &mock, "x");
    let out = env.run(&["auth", "oauth-finish", &callback]);
    assert_eq!(out.status.code(), Some(2));
    let all = stdout_and_stderr(&out);
    assert!(all.contains("invalid_grant"), "{all}");
    assert!(!all.contains("SECRET-ECHO-123") && !all.contains("leaky"), "{all}");
}

// ---- 8: no client ever sees a half-made socket ----------------------------

#[test]
fn racing_first_clients_never_see_a_socket_open_to_others() {
    // Twenty clients start at once with no daemon running. Before the daemon
    // set a private umask, a client could catch the socket between bind()
    // and chmod() and refuse it as world-accessible.
    for round in 0..6 {
        let env = Env::new();
        let t3 = MockT3::new(&env);
        t3.set("race", "completed", 1);
        let children: Vec<_> = (0..20)
            .map(|_| env.spawn(&["wait", "--transport", "local", "--t3-bin", t3.bin_str(), "race"]))
            .collect();
        for c in children {
            let (code, out, err) = finish(c, secs(20));
            assert_eq!(code, 0, "round {round}: {out} {err}");
        }
    }
}
