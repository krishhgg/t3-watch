//! Regressions for the fourth review (Sol, ordinal 4), P1: a new client
//! must never trust a daemon of another engine revision, and an old build's
//! outbox sender must never handle this build's entries.
//!
//! The old daemons here are fakes speaking the old protocol exactly: an
//! `accepted` line without an engine (or with another one), then a
//! `settled`/`completed` answer, which is what a 53d5956 or 6bd6c1e daemon
//! sends while its engine has known safety bugs.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const T: &str = "mcp:launch:compat";

fn mcp_env(mock: &MockMcp) -> Env {
    let env = Env::new();
    env.add_machine("mock", &mock.url, TOKEN);
    env
}

fn both(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// A daemon of another build: answers every request with `lines`.
fn fake_daemon(sock: &Path, lines: Vec<Value>) -> Arc<AtomicUsize> {
    let _ = fs::remove_file(sock);
    let listener = UnixListener::bind(sock).unwrap();
    fs::set_permissions(sock, fs::Permissions::from_mode(0o600)).unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let count = served.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            count.fetch_add(1, Ordering::SeqCst);
            let lines = lines.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(conn.try_clone().unwrap());
                let mut req = String::new();
                let _ = r.read_line(&mut req);
                let mut w = conn;
                for l in &lines {
                    let _ = writeln!(w, "{l}");
                }
                std::thread::sleep(Duration::from_millis(200));
            });
        }
    });
    served
}

fn completed_by_old_engine(accepted: Value) -> Vec<Value> {
    vec![
        accepted,
        json!({ "event": "settled", "outcome": "completed", "mode": "all", "via": "daemon", "elapsedMs": 5,
                "threads": [{ "threadId": T, "status": "completed", "outcome": "completed", "settled": true,
                              "runOrdinal": 2 }] }),
    ]
}

fn private_dir(env: &Env, name: &str) -> PathBuf {
    let d = env.home.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
    d
}

#[test]
fn a_daemon_of_another_engine_on_a_custom_socket_is_refused() {
    let accepted_lines = [
        // 53d5956 and 6bd6c1e: no engine at all, version 0.1.0.
        json!({ "event": "accepted", "pid": 4242, "version": "0.1.0" }),
        // A past engine.
        json!({ "event": "accepted", "pid": 4242, "version": "0.2.0", "engine": 4, "rev": "a92f977" }),
        // This engine number, without the capabilities this client needs.
        json!({ "event": "accepted", "pid": 4242, "version": "0.2.0", "engine": ENGINE, "rev": "x",
                "capabilities": ["outbox-receipts"] }),
    ];
    for accepted in accepted_lines {
        let mock = MockMcp::start();
        mock.set_thread(T, "running", 3);
        let hook = MockHook::start();
        let env = mcp_env(&mock);
        let sock = private_dir(&env, "s").join("shared.sock");
        let served = fake_daemon(&sock, completed_by_old_engine(accepted.clone()));
        let out = env
            .cmd(&[
                "wait",
                "--notify-on",
                "settled,needs_input",
                "--webhook-url",
                &hook.url,
                T,
            ])
            .env("T3_WATCH_SOCKET", &sock)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{accepted}: {}", both(&out));
        let v = output_json(&out);
        assert_eq!(v["error"]["kind"], "incompatible_daemon", "{v}");
        assert!(v["error"]["message"].as_str().unwrap().contains("engine"));
        assert_ne!(v["outcome"], "completed");
        assert_eq!(served.load(Ordering::SeqCst), 1);
        assert!(
            hook.requests().is_empty(),
            "nothing may be announced from an old engine"
        );
        let trace = fs::read_to_string(env.home.join("trace.jsonl")).unwrap();
        assert!(trace.contains("\"kind\":\"daemon.incompatible\""), "{trace}");
    }
}

#[test]
fn an_answer_without_any_accepted_line_is_not_trusted() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 3);
    let env = mcp_env(&mock);
    let sock = private_dir(&env, "s").join("shared.sock");
    let mut lines = completed_by_old_engine(json!(null));
    lines.remove(0);
    fake_daemon(&sock, lines);
    let out = env.cmd(&["wait", T]).env("T3_WATCH_SOCKET", &sock).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", both(&out));
    assert_eq!(output_json(&out)["error"]["kind"], "incompatible_daemon");
}

#[test]
fn an_old_daemon_at_the_old_default_socket_is_never_reached() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 3);
    let env = mcp_env(&mock);
    // A 6bd6c1e daemon would listen here and answer "completed".
    fs::set_permissions(&env.home, fs::Permissions::from_mode(0o700)).unwrap();
    let old = env.home.join("daemon-p2.sock");
    let served = fake_daemon(
        &old,
        completed_by_old_engine(json!({ "event": "accepted", "pid": 4242, "version": "0.1.0" })),
    );
    let out = env.run(&["wait", "--timeout", "1500ms", T]);
    // This build's own daemon answered: the thread still runs.
    assert_eq!(out.status.code(), Some(3), "{}", both(&out));
    let v = output_json(&out);
    assert_eq!(v["daemon"]["engine"], ENGINE, "{v}");
    assert_eq!(v["build"]["engine"], ENGINE);
    assert_eq!(served.load(Ordering::SeqCst), 0, "the old socket was contacted");
}

#[test]
fn this_daemon_refuses_a_request_from_another_engine() {
    let env = Env::new();
    let mut daemon = env.spawn(&["daemon", "--idle-exit", "30s"]);
    env.wait_status("daemon", Duration::from_secs(10), |s| s["running"] == true);
    assert_eq!(env.status()["build"]["engine"], ENGINE);
    // The request a 6bd6c1e client sends: no engine field.
    let old_request = json!({ "op": "wait", "targets": [{ "transport": "mcp", "machine": "m", "threadId": T }],
                              "mode": "all", "timeoutMs": 1000 });
    for req in [old_request.clone(), {
        let mut r = old_request.clone();
        r["engine"] = json!(4);
        r
    }] {
        let mut s = UnixStream::connect(env.home.join(SOCK)).unwrap();
        writeln!(s, "{req}").unwrap();
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["error"]["kind"], "incompatible_client", "{v}");
    }
    env.run(&["stop"]);
    daemon.wait().unwrap();
}

// ---- old outbox senders --------------------------------------------------

fn legacy_entry(id: &str, url: &str, created_ms: u64) -> Value {
    json!({ "version": 1, "id": id, "event": "settled",
            "body": json!({"event": "settled", "outcome": "completed"}).to_string(),
            "url": url, "headers": [], "secret": null,
            "createdMs": created_ms, "expiresMs": created_ms + 86_400_000,
            "attempts": 4, "retries": 0, "nextAttemptMs": created_ms + 2_000 })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn entries_left_by_an_older_build_are_migrated_only_when_no_client_can_own_them() {
    let env = Env::new();
    let hook = MockHook::start();
    let legacy = private_dir(&env, "outbox");
    let (old_id, young_id) = ("00aa11bb", "00cc22dd");
    fs::write(
        legacy.join(format!("{old_id}.json")),
        legacy_entry(old_id, &hook.url, now_ms() - 600_000).to_string(),
    )
    .unwrap();
    fs::write(
        legacy.join(format!("{young_id}.json")),
        legacy_entry(young_id, &hook.url, now_ms()).to_string(),
    )
    .unwrap();
    let run = env.run(&["outbox", "run", "--once"]);
    let r = output_json(&run);
    assert!(r["results"].to_string().contains("\"migrated\":true"), "{r}");
    // The old entry moved to this build's outbox and was delivered with its own id.
    let reqs = hook.requests();
    assert_eq!(reqs.len(), 1, "{r}");
    assert_eq!(reqs[0].headers["x-t3-watch-id"], old_id);
    assert!(!legacy.join(format!("{old_id}.json")).exists());
    // A young one may still belong to a running client of that build.
    assert!(legacy.join(format!("{young_id}.json")).exists());
}

#[test]
fn this_builds_entries_live_where_old_senders_never_look() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    let url = format!(
        "http://127.0.0.1:{}/hooks/x",
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    );
    let out = env.run(&["wait", "--no-daemon", "--durable", "--webhook-url", &url, T]);
    assert_eq!(out.status.code(), Some(4), "{}", both(&out));
    assert!(!env.home.join("outbox").exists(), "old senders read outbox/");
    assert!(!env.home.join("outbox-v2").exists(), "engine-4 senders read outbox-v2/");
    assert!(!env.home.join("outbox-v3").exists(), "engine-5 senders read outbox-v3/");
    let v2: Vec<_> = fs::read_dir(env.home.join(OUTBOX))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .collect();
    assert_eq!(v2.len(), 1);
}

#[test]
fn a_young_entry_left_by_a_crashed_old_client_is_kept_pending_and_delivered() {
    // Sol's repro: a valid legacy entry too young to migrate, no old client
    // or sender alive. It must count as pending everywhere, keep the daemon
    // up past its idle exit, and go out once eligible. Created 80 s ago so
    // it becomes eligible 10 s into the test (the grace is 90 s).
    let env = Env::new();
    let hook = MockHook::start();
    let legacy = private_dir(&env, "outbox");
    let id = "00ee33ff";
    fs::write(
        legacy.join(format!("{id}.json")),
        legacy_entry(id, &hook.url, now_ms() - 80_000).to_string(),
    )
    .unwrap();

    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(4), "a deferred entry is not an empty outbox");
    let r = output_json(&run);
    assert!(r["pending"].as_u64().unwrap() >= 1, "{r}");
    assert!(r["results"].to_string().contains("\"legacy\":\"outbox\""), "{r}");
    let list = output_json(&env.run(&["outbox", "list"]));
    assert_eq!(list["legacy"][0]["id"], id, "{list}");

    // The daemon (idle exit 2 s in tests) stays up for it.
    let mut daemon = env.spawn(&["daemon"]);
    env.wait_status("daemon", Duration::from_secs(10), |s| s["running"] == true);
    assert_eq!(env.status()["outbox"]["legacyPending"], 1);
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        daemon.try_wait().unwrap().is_none(),
        "the daemon left a legacy entry stranded"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while hook.requests().is_empty() {
        assert!(std::time::Instant::now() < deadline, "never delivered");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(hook.requests()[0].headers["x-t3-watch-id"], id);
    // Delivered and nothing left: now it may idle out.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while daemon.try_wait().unwrap().is_none() {
        assert!(std::time::Instant::now() < deadline, "the daemon never idled out");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(!legacy.join(format!("{id}.json")).exists());
    let receipt = fs::read_to_string(env.home.join(OUTBOX).join("receipts").join(format!("{id}.json"))).unwrap();
    assert!(receipt.contains("\"disposition\":\"delivered\""), "{receipt}");
}

// ---- Sol ordinal 6, P1: the engine-4 upgrade boundary -----------------------

#[test]
fn older_engine_daemons_at_their_own_default_sockets_are_never_reached() {
    // a92f977 and fa10d88 listen on daemon-e4.sock, 0.3.0 on daemon-e5.sock,
    // 0.4.0 on daemon-e6.sock, 0.5.0 on daemon-e7.sock, 0.6.0 on
    // daemon-e8.sock; none handles legacy entries or stale answers the way
    // this build must.
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 3);
    let env = mcp_env(&mock);
    fs::set_permissions(&env.home, fs::Permissions::from_mode(0o700)).unwrap();
    let served: Vec<_> = [
        (4, "0.2.0"),
        (5, "0.3.0"),
        (6, "0.4.0"),
        (7, "0.5.0"),
        (8, "0.6.0"),
        (9, "0.7.0"),
        (10, "0.8.0"),
    ]
    .into_iter()
    .map(|(engine, version)| {
        fake_daemon(
            &env.home.join(format!("daemon-e{engine}.sock")),
            completed_by_old_engine(json!({ "event": "accepted", "pid": 4242, "version": version,
                                                "engine": engine, "rev": "old" })),
        )
    })
    .collect();
    let out = env.run(&["wait", "--timeout", "1500ms", T]);
    assert_eq!(out.status.code(), Some(3), "{}", both(&out));
    let v = output_json(&out);
    assert_eq!(v["daemon"]["engine"], ENGINE);
    for s in &served {
        assert_eq!(s.load(Ordering::SeqCst), 0, "an older engine's socket was contacted");
    }
    let caps = v["daemon"]["capabilities"].to_string();
    for c in [
        "outbox-receipts",
        "outbox-legacy-drain",
        "outbox-legacy-in-place",
        "outbox-legacy-retain",
        "outbox-legacy-retain-held",
        "outbox-legacy-checked-convert",
        "native-claude-code-hooks",
        "suspend-aware",
        "suspend-invalidation",
    ] {
        assert!(caps.contains(c), "{c} missing from {caps}");
    }
}

fn v2_entry(id: &str, url: &str, created_ms: u64, attempts: u32) -> Value {
    let mut e = legacy_entry(id, url, created_ms);
    e["attempts"] = json!(attempts);
    e["retries"] = json!(0);
    e
}

fn entry_at(dir: &Path, id: &str) -> Option<Value> {
    fs::read_to_string(dir.join(format!("{id}.json")))
        .ok()
        .map(|t| serde_json::from_str(&t).unwrap())
}

#[test]
fn older_builds_entries_are_delivered_where_they_are_never_moved() {
    // Sol ordinal 7, P1: an a92f977 client, held back 100 s by a slow
    // progress hook, found its entry gone after engine 5 moved it at 91 s
    // (the receiver had answered 401) and reported the event delivered. To
    // an engine-4 or engine-5 client only a real 2xx may take its entry
    // away; anything else must leave it where it is, marked as the
    // sender's, so that client reports it queued.
    for store in ["outbox-v2", "outbox-v3"] {
        let env = Env::new();
        let hook = MockHook::start();
        let dir = private_dir(&env, store);
        let now = now_ms();
        let (waiting, held, finished) = ("0a0a0a01", "0c0c0c03", "0d0d0d04");
        // Never attempted, 91 s old: its client is still held back.
        let mut e = v2_entry(waiting, &hook.url, now - 91_000, 0);
        e["nextAttemptMs"] = json!(now - 1_000);
        fs::write(dir.join(format!("{waiting}.json")), e.to_string()).unwrap();
        // Leased by a live old process right now.
        fs::write(
            dir.join(format!("{held}.json")),
            v2_entry(held, &hook.url, now - 600_000, 4).to_string(),
        )
        .unwrap();
        let lease = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(format!("{held}.lock")))
            .unwrap();
        use std::os::fd::AsRawFd;
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        // Already finished there (dropped), entry left behind by a crash.
        fs::write(
            dir.join(format!("{finished}.json")),
            v2_entry(finished, &hook.url, now - 600_000, 1).to_string(),
        )
        .unwrap();
        fs::create_dir_all(dir.join("receipts")).unwrap();
        fs::write(
            dir.join("receipts").join(format!("{finished}.json")),
            json!({ "id": finished, "disposition": "dropped" }).to_string(),
        )
        .unwrap();

        hook.push(401, &[]);
        let run = env.run(&["outbox", "run", "--once"]);
        let r = output_json(&run);
        assert_eq!(run.status.code(), Some(4), "{store}: {r}");
        assert!(!r["results"].to_string().contains("migrated"), "{store}: {r}");
        let sent: Vec<String> = hook
            .requests()
            .iter()
            .map(|q| q.headers["x-t3-watch-id"].clone())
            .collect();
        assert_eq!(sent, [waiting.to_string()], "{store}: {r}");
        // Still where its client looks, with the attempt counted as the
        // sender's: a92f977's client then reports it queued.
        let kept = entry_at(&dir, waiting).unwrap_or_else(|| panic!("{store}: the entry left its store: {r}"));
        assert!(kept["retries"].as_u64().unwrap() >= 1, "{store}: {kept}");
        assert_eq!(kept["lastStatus"], 401, "{store}: {kept}");
        assert!(entry_at(&env.home.join(OUTBOX), waiting).is_none(), "{store}: copied");
        assert!(!dir.join("receipts").join(format!("{waiting}.json")).exists());
        assert!(entry_at(&dir, held).is_some(), "{store}: leased entry touched");
        // Dropped, but undelivered: an engine-4 client would read its
        // absence as delivery, and no trace says its client is done.
        // Kept for it, and its receipt taken back until it leaves: an
        // engine-6 or engine-7 sender removes an entry beside a receipt.
        if store == "outbox-v2" {
            let kept = entry_at(&dir, finished).unwrap_or_else(|| panic!("{store}: dropped entry removed"));
            assert_retained(&dir, &kept, "dropped");
        } else {
            assert!(entry_at(&dir, finished).is_none(), "{store}: finished entry kept");
            assert!(
                fs::read_to_string(dir.join("receipts").join(format!("{finished}.json")))
                    .unwrap()
                    .contains("dropped"),
                "{store}: its receipt changed"
            );
        }
        let list = output_json(&env.run(&["outbox", "list"]));
        assert_eq!(list["legacy"].as_array().unwrap().len(), 2, "{store}: {list}");
        assert!(list["legacy"].to_string().contains(store), "{store}: {list}");

        // The receiver takes it: only now does it leave, with a receipt.
        let run = env.run(&["outbox", "run", "--once"]);
        assert_eq!(run.status.code(), Some(4), "{store}: the leased one is still pending");
        assert_eq!(hook.requests().len(), 2, "{store}");
        assert!(entry_at(&dir, waiting).is_none(), "{store}: delivered but kept");
        let receipt = fs::read_to_string(dir.join("receipts").join(format!("{waiting}.json"))).unwrap();
        assert!(receipt.contains("\"disposition\":\"delivered\""), "{store}: {receipt}");
        drop(lease);
    }
}

#[test]
fn a_receipt_that_cannot_be_trusted_never_discards_an_event() {
    // Sol ordinal 7, P2: a pending entry beside the receipt `{}` was deleted
    // unsent and `outbox run --once` exited 0 with nothing pending.
    let bad = [
        "{}".to_string(),
        json!({ "id": "0f0f0f06", "disposition": "teleported" }).to_string(),
        json!({ "id": "0e0e0e0e", "disposition": "delivered" }).to_string(),
        json!({ "disposition": "delivered" }).to_string(),
        "not json".to_string(),
    ];
    for store in ["outbox-v2", "outbox-v3", OUTBOX] {
        for receipt in &bad {
            let env = Env::new();
            let hook = MockHook::start();
            let dir = private_dir(&env, store);
            let id = "0f0f0f06";
            fs::write(
                dir.join(format!("{id}.json")),
                v2_entry(id, &hook.url, now_ms() - 600_000, 1).to_string(),
            )
            .unwrap();
            fs::create_dir_all(dir.join("receipts")).unwrap();
            fs::write(dir.join("receipts").join(format!("{id}.json")), receipt).unwrap();
            let what = format!("{store} {receipt}");

            let list = output_json(&env.run(&["outbox", "list"]));
            assert!(list.to_string().contains(id), "{what}: not listed as pending: {list}");
            hook.push(503, &[]);
            let run = env.run(&["outbox", "run", "--once"]);
            let r = output_json(&run);
            assert_eq!(run.status.code(), Some(4), "{what}: {r}");
            assert!(r["pending"].as_u64().unwrap() >= 1, "{what}: {r}");
            assert!(r["results"].to_string().contains("receiptError"), "{what}: {r}");
            assert_eq!(hook.requests().len(), 1, "{what}: the event was not sent");
            assert!(entry_at(&dir, id).is_some(), "{what}: the event was discarded");

            let run = env.run(&["outbox", "run", "--once"]);
            assert_eq!(run.status.code(), Some(0), "{what}: {}", both(&run));
            assert_eq!(hook.requests().len(), 2, "{what}");
            assert!(entry_at(&dir, id).is_none(), "{what}");
            let now: Value =
                serde_json::from_str(&fs::read_to_string(dir.join("receipts").join(format!("{id}.json"))).unwrap())
                    .unwrap();
            assert_eq!(now["id"], id, "{what}: {now}");
            assert_eq!(now["disposition"], "delivered", "{what}: {now}");
        }
    }
}

// ---- Sol ordinal 6, P2: a receipt next to a surviving entry is final -------

#[test]
fn an_entry_left_beside_its_receipt_is_never_sent() {
    for disposition in ["dropped", "expired", "delivered"] {
        let env = Env::new();
        let hook = MockHook::start();
        let dir = private_dir(&env, OUTBOX);
        let id = "0e0e0e05";
        let entry = v2_entry(id, &hook.url, now_ms() - 600_000, 1);
        fs::write(dir.join(format!("{id}.json")), entry.to_string()).unwrap();
        fs::create_dir_all(dir.join("receipts")).unwrap();
        let receipt = json!({ "id": id, "disposition": disposition, "at": "2026-11-07T00:00:00.000Z",
                              "attempts": 1, "by": "outbox drop" });
        fs::write(dir.join("receipts").join(format!("{id}.json")), receipt.to_string()).unwrap();
        let run = env.run(&["outbox", "run", "--once"]);
        assert_eq!(run.status.code(), Some(0), "{disposition}: {}", both(&run));
        assert!(hook.requests().is_empty(), "{disposition}: a finished entry was sent");
        assert!(
            !dir.join(format!("{id}.json")).exists(),
            "{disposition}: entry left behind"
        );
        let kept: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("receipts").join(format!("{id}.json"))).unwrap())
                .unwrap();
        assert_eq!(kept, receipt, "{disposition}: the receipt was rewritten");
        assert_eq!(
            dir.join("dead").join(format!("{id}.json")).exists(),
            disposition == "expired"
        );
    }
}

// ---- Sol ordinal 8, P1: an undelivered engine-4 entry never just vanishes ---

/// Appends a line to the delivery trace as an engine-4 build writes it.
fn old_trace(env: &Env, kind: &str, id: &str, pid: u32) {
    let line = json!({ "at": "2026-11-07T00:00:00.000Z", "kind": kind, "pid": pid, "engine": 4,
                       "rev": "a92f977", "id": id });
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(env.home.join("trace.jsonl"))
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

fn exited_pid() -> u32 {
    let mut c = std::process::Command::new("true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

/// The leased older senders modelled below, each from its own source.
const OLDER: [u32; 4] = [4, 6, 7, 8];

/// Whether an older sender's forced pass takes an `outbox-v2/` entry out of
/// the store without delivering it, while the engine-4 client that made it
/// is still running (that client would then report it delivered):
///
/// - engine 4 (8f06a73 to fa10d88) reads no receipts: it dead-letters an
///   expired entry, defers one whose `notBefore` is ahead, sends the rest;
/// - engine 6 (83e013e) cleans up an entry beside a valid receipt, then
///   does what engine 4 does;
/// - engine 7 (4e3c16e) and engine 8 (26ee492) keep an expired entry, or
///   one beside an undelivered receipt, for a client the trace shows
///   running.
fn older_sender_removes(engine: u32, dir: &Path, e: &Value) -> bool {
    let id = e["id"].as_str().unwrap();
    let receipt: Option<Value> = fs::read_to_string(dir.join("receipts").join(format!("{id}.json")))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let valid_receipt = receipt.is_some_and(|r| {
        r["id"] == id && ["delivered", "expired", "dropped"].contains(&r["disposition"].as_str().unwrap_or(""))
    });
    let expired = now_ms() >= e["expiresMs"].as_u64().unwrap();
    match engine {
        4 => expired,
        6 => valid_receipt || expired,
        7 | 8 => false,
        _ => unreachable!(),
    }
}

/// The older senders that would remove this entry without delivering it.
fn exposed_to(dir: &Path, e: &Value) -> Vec<u32> {
    OLDER.into_iter().filter(|n| older_sender_removes(*n, dir, e)).collect()
}

/// A retained entry: an engine-4 client reports it queued (a sender retry
/// recorded) and no modelled older sender removes it.
fn assert_retained(dir: &Path, kept: &Value, how: &str) {
    assert_eq!(exposed_to(dir, kept), Vec::<u32>::new(), "{kept}");
    assert!(kept["retries"].as_u64().unwrap() >= 1, "{kept}");
    assert_eq!(kept["retainedAs"], how, "{kept}");
}

/// No step of this build leaves an entry exposed to an older sender it was
/// not exposed to before.
fn assert_no_more_exposed(before: &[u32], dir: &Path, e: &Value, when: &str) {
    let after = exposed_to(dir, e);
    assert!(
        after.iter().all(|n| before.contains(n)),
        "{when}: exposed to engines {after:?}, before only {before:?}: {e}"
    );
}

fn receipt_in(dir: &Path, id: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(dir.join("receipts").join(format!("{id}.json"))).unwrap()).unwrap()
}

#[test]
fn an_expired_engine_4_entry_stays_where_its_client_looks_until_that_client_is_done() {
    // Sol's repro: an a92f977 client (--durable-max-age 1s) held back by its
    // progress hook; its final entry expired and engine 6 dead-lettered it,
    // so the client found it gone and reported the event delivered.
    let env = Env::new();
    let hook = MockHook::start();
    let v2 = private_dir(&env, "outbox-v2");
    let now = now_ms();
    let (live, finished, crashed, untraced) = ("1a1a1a01", "1b1b1b02", "1c1c1c03", "1d1d1d04");
    for id in [live, finished, crashed, untraced] {
        let mut e = v2_entry(id, &hook.url, now - 5_000, 0);
        e["expiresMs"] = json!(now - 1_000);
        e["nextAttemptMs"] = json!(now + 85_000);
        fs::write(v2.join(format!("{id}.json")), e.to_string()).unwrap();
    }
    let me = std::process::id();
    // Its client is still running.
    old_trace(&env, "outbox.created", live, me);
    // Its client sent it and moved on.
    old_trace(&env, "outbox.created", finished, me);
    old_trace(&env, "notify.result", finished, me);
    // The process that made it is gone.
    old_trace(&env, "outbox.created", crashed, exited_pid());

    let run = env.run(&["outbox", "run", "--once"]);
    let r = output_json(&run);
    assert!(hook.requests().is_empty(), "an expired entry was sent: {r}");
    for id in [live, untraced] {
        let kept = entry_at(&v2, id).unwrap_or_else(|| panic!("{id} left outbox-v2 undelivered: {r}"));
        // What an engine-4 client reads as "queued", and what no older
        // sender (Sol ordinal 9: a92f977's own `outbox run --once`)
        // dead-letters.
        assert_retained(&v2, &kept, "expired");
        assert!(kept["retainedExpiresMs"].as_u64().unwrap() < now, "{kept}");
        assert!(!v2.join("dead").join(format!("{id}.json")).exists());
    }
    for id in [finished, crashed] {
        assert!(
            entry_at(&v2, id).is_none(),
            "{id} kept although its client is done: {r}"
        );
        assert!(v2.join("dead").join(format!("{id}.json")).exists(), "{id}");
        assert_eq!(receipt_in(&v2, id)["disposition"], "expired");
    }
    assert_eq!(run.status.code(), Some(0), "retained entries are not pending: {r}");
    let list = output_json(&env.run(&["outbox", "list"]));
    assert_eq!(list["retained"].as_array().unwrap().len(), 2, "{list}");
    assert_eq!(list["legacy"].as_array().unwrap().len(), 0, "{list}");

    // A later pass sends nothing and keeps them; once the client is done
    // with one, it goes as it would have: expired, with its receipt.
    old_trace(&env, "notify.result", live, me);
    let r = output_json(&env.run(&["outbox", "run", "--once"]));
    assert!(hook.requests().is_empty(), "{r}");
    assert!(entry_at(&v2, live).is_none(), "{r}");
    let dead: Value =
        serde_json::from_str(&fs::read_to_string(v2.join("dead").join(format!("{live}.json"))).unwrap()).unwrap();
    assert!(
        dead["expiresMs"].as_u64().unwrap() < now,
        "its real expiry is back: {dead}"
    );
    assert_eq!(receipt_in(&v2, live)["disposition"], "expired");
    let kept = entry_at(&v2, untraced).unwrap_or_else(|| panic!("nothing says its client is done: {r}"));
    assert_retained(&v2, &kept, "expired");
}

#[test]
fn a_dropped_engine_4_entry_stays_where_its_client_looks_until_that_client_is_done() {
    let env = Env::new();
    let hook = MockHook::start();
    let v2 = private_dir(&env, "outbox-v2");
    let now = now_ms();
    let id = "1e1e1e05";
    let mut e = v2_entry(id, &hook.url, now - 5_000, 0);
    e["nextAttemptMs"] = json!(now - 1_000);
    fs::write(v2.join(format!("{id}.json")), e.to_string()).unwrap();
    let me = std::process::id();
    old_trace(&env, "outbox.created", id, me);

    let out = env.run(&["outbox", "drop", id]);
    let d = output_json(&out);
    assert_eq!(out.status.code(), Some(0), "{d}");
    assert_eq!(d["store"], "outbox-v2", "{d}");
    assert!(d["retained"].is_string(), "{d}");
    let kept = entry_at(&v2, id).expect("a dropped engine-4 entry vanished");
    assert_retained(&v2, &kept, "dropped");

    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(0), "{}", both(&run));
    assert!(hook.requests().is_empty(), "a dropped entry was sent");
    assert!(entry_at(&v2, id).is_some());

    old_trace(&env, "notify.result", id, me);
    env.run(&["outbox", "run", "--once"]);
    assert!(entry_at(&v2, id).is_none(), "its client is done with it");
    assert_eq!(receipt_in(&v2, id)["disposition"], "dropped");
    assert!(hook.requests().is_empty());
}

#[test]
fn running_engine_5_and_6_senders_are_reported_as_unsafe_for_engine_4_entries() {
    // Engine 5 (0.3.0) moves outbox-v2 entries whatever their state, minding
    // only the lease; engine 6 (0.4.0) removes one beside a receipt. No
    // state on disk keeps such an entry from them.
    let env = Env::new();
    fs::set_permissions(&env.home, fs::Permissions::from_mode(0o700)).unwrap();
    let list = output_json(&env.run(&["outbox", "list"]));
    assert_eq!(list["incompatibleSenders"], json!([]), "{list}");
    fake_daemon(&env.home.join("daemon-e5.sock"), vec![]);
    fake_daemon(&env.home.join("daemon-e6.sock"), vec![]);
    let list = output_json(&env.run(&["outbox", "list"]));
    let engines: Vec<u64> = list["incompatibleSenders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["engine"].as_u64().unwrap())
        .collect();
    assert_eq!(engines, [5, 6], "{list}");
    assert!(list["incompatibleSenders"][0]["action"]
        .as_str()
        .unwrap()
        .contains("t3-watch stop"));
}

#[test]
fn a_held_entry_another_build_moved_out_of_outbox_v2_is_finished_not_stuck() {
    // An engine-5 sender moves a retained entry into outbox-v3, keeping its
    // 2100 hold but not its retainedAs: its engine-4 client has lost it
    // already, and left alone it would be pending forever.
    let env = Env::new();
    let hook = MockHook::start();
    let v3 = private_dir(&env, "outbox-v3");
    let id = "3a3a3a3a";
    let mut e = v2_entry(id, &hook.url, now_ms() - 5_000, 0);
    e["retries"] = json!(1);
    e["expiresMs"] = json!(4_102_444_800_000u64);
    e["notBeforeMs"] = json!(4_102_444_800_000u64);
    e["nextAttemptMs"] = json!(4_102_444_800_000u64);
    fs::write(v3.join(format!("{id}.json")), e.to_string()).unwrap();
    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(0), "{}", both(&run));
    assert!(hook.requests().is_empty());
    assert!(entry_at(&v3, id).is_none());
    assert!(v3.join("dead").join(format!("{id}.json")).exists());
    assert_eq!(receipt_in(&v3, id)["disposition"], "expired");
}

// ---- Sol ordinal 10 ----------------------------------------------------------

/// What a 4e3c16e (engine 7) sender leaves for an expired entry it keeps for
/// a live engine-4 client: the original, past expiry, a sender retry, and a
/// receipt beside it.
fn left_by_engine_7(env: &Env, hook: &MockHook, id: &str) -> PathBuf {
    let v2 = private_dir(env, "outbox-v2");
    let now = now_ms();
    let mut e = v2_entry(id, &hook.url, now - 5_000, 0);
    e["expiresMs"] = json!(now - 1_000);
    e["retries"] = json!(1);
    fs::write(v2.join(format!("{id}.json")), e.to_string()).unwrap();
    fs::create_dir_all(v2.join("receipts")).unwrap();
    fs::write(
        v2.join("receipts").join(format!("{id}.json")),
        json!({ "id": id, "event": "settled", "disposition": "expired", "at": "2026-11-07T00:00:00.000Z",
                "attempts": 0, "by": "outbox sender" })
        .to_string(),
    )
    .unwrap();
    fs::write(v2.join("sender.lock"), "").unwrap();
    old_trace(env, "outbox.created", id, std::process::id());
    v2
}

#[test]
fn a_receipt_conversion_cut_short_never_leaves_an_entry_more_exposed_and_is_repaired() {
    // Sol ordinals 10 and 12: this build died right before taking back the
    // receipt beside an entry held for a live a92f977 client. No older
    // sender can be kept out across that crash (one may already hold the
    // lock file open), so what matters is what each would do with what is
    // left: never more than with what was there before this build acted.
    let env = Env::new();
    let hook = MockHook::start();
    let id = "4a4a4a4a";
    let v2 = left_by_engine_7(&env, &hook, id);
    let receipt = v2.join("receipts").join(format!("{id}.json"));
    let before = exposed_to(&v2, &entry_at(&v2, id).unwrap());
    assert_eq!(before, [4, 6], "what 0.5.0 leaves is already exposed");

    let cut = env
        .cmd(&["outbox", "run", "--once"])
        .env("T3_WATCH_TEST_FAULT", "before-receipt-unlink")
        .output()
        .unwrap();
    assert_eq!(cut.status.code(), Some(97), "{}", both(&cut));
    let kept = entry_at(&v2, id).expect("the entry left outbox-v2");
    assert_eq!(kept["retainedAs"], "expired", "{kept}");
    assert!(receipt.exists());
    assert_no_more_exposed(&before, &v2, &kept, "cut short before the receipt went");
    assert_eq!(exposed_to(&v2, &kept), [6], "an engine-4 sender no longer would");
    assert!(
        v2.join("sender.lock").is_file(),
        "no fence: older senders lock it as before"
    );
    let list = output_json(&env.run(&["outbox", "list"]));
    assert!(list["retained"].to_string().contains(id), "{list}");

    // Restarted, it finishes the job: exposed to none of them.
    let run = env.run(&["outbox", "run", "--once"]);
    let r = output_json(&run);
    assert_eq!(run.status.code(), Some(0), "{r}");
    assert!(r["results"].to_string().contains("\"converted\":true"), "{r}");
    assert!(!receipt.exists(), "{r}");
    let kept = entry_at(&v2, id).expect("the entry left outbox-v2");
    assert_retained(&v2, &kept, "expired");
    assert!(hook.requests().is_empty());
}

#[test]
fn a_receipt_that_cannot_be_taken_back_is_reported_and_never_more_exposed() {
    // Sol ordinal 10: an ordinary failure to remove the receipt.
    let env = Env::new();
    let hook = MockHook::start();
    let id = "4b4b4b4b";
    let v2 = left_by_engine_7(&env, &hook, id);
    let before = exposed_to(&v2, &entry_at(&v2, id).unwrap());
    let receipts = v2.join("receipts");
    fs::set_permissions(&receipts, fs::Permissions::from_mode(0o500)).unwrap();

    let run = env.run(&["outbox", "run", "--once"]);
    let r = output_json(&run);
    assert_eq!(run.status.code(), Some(4), "work is left: {r}");
    let text = r["results"].to_string();
    assert!(text.contains("conversionError") && text.contains("atRisk"), "{r}");
    let kept = entry_at(&v2, id).expect("the entry left outbox-v2");
    assert_no_more_exposed(&before, &v2, &kept, "receipt removal refused");
    let mut daemon = env.spawn(&["daemon", "--idle-exit", "30s"]);
    env.wait_status("daemon", Duration::from_secs(10), |s| s["running"] == true);
    let o = &env.status()["outbox"];
    assert_eq!(o["legacyConversionsIncomplete"], 1, "{o}");
    env.run(&["stop"]);
    daemon.wait().unwrap();

    fs::set_permissions(&receipts, fs::Permissions::from_mode(0o700)).unwrap();
    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(0), "{}", both(&run));
    assert!(!receipts.join(format!("{id}.json")).exists());
    let kept = entry_at(&v2, id).expect("the entry left outbox-v2");
    assert_retained(&v2, &kept, "expired");
}

#[test]
fn every_step_this_build_takes_on_an_engine_4_entry_leaves_it_no_more_exposed() {
    // Each state this build can leave an outbox-v2 entry in, however it is
    // interrupted, checked against what every modelled older sender would do
    // with it: holding an expired entry (one write), and taking a receipt
    // back from beside one (two writes, cut short between them or refused).
    let now = now_ms();
    for (case, receipt, cut) in [
        ("expired, no receipt", false, false),
        ("receipt beside, finished", true, false),
        ("receipt beside, cut short", true, true),
    ] {
        let env = Env::new();
        let hook = MockHook::start();
        let id = "5a5a5a5a";
        let v2 = left_by_engine_7(&env, &hook, id);
        if !receipt {
            fs::remove_file(v2.join("receipts").join(format!("{id}.json"))).unwrap();
        }
        let start = entry_at(&v2, id).unwrap();
        assert!(start["expiresMs"].as_u64().unwrap() < now);
        let before = exposed_to(&v2, &start);
        let mut cmd = env.cmd(&["outbox", "run", "--once"]);
        if cut {
            cmd.env("T3_WATCH_TEST_FAULT", "before-receipt-unlink");
        }
        let out = cmd.output().unwrap();
        let after = entry_at(&v2, id).unwrap_or_else(|| panic!("{case}: entry gone: {}", both(&out)));
        assert_no_more_exposed(&before, &v2, &after, case);
        if !cut {
            assert_eq!(exposed_to(&v2, &after), Vec::<u32>::new(), "{case}: {after}");
        }
    }
}

#[test]
fn a_fence_left_by_0_7_0_is_cleared_so_every_sender_can_lock_the_store() {
    let env = Env::new();
    let v2 = private_dir(&env, "outbox-v2");
    fs::create_dir(v2.join("sender.lock")).unwrap();
    fs::write(v2.join("sender.fenced.lock"), "").unwrap();
    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(0), "{}", both(&run));
    assert!(v2.join("sender.lock").is_file());
    assert!(!v2.join("sender.fenced.lock").exists());
}

#[test]
fn an_engine_5_daemon_on_the_runtime_socket_is_reported_too() {
    // Sol: with T3_WATCH_HOME unset, daemons listen under
    // $XDG_RUNTIME_DIR/t3-watch/, which the report did not look at.
    let env = Env::new();
    fs::set_permissions(&env.home, fs::Permissions::from_mode(0o700)).unwrap();
    let rt = env.home.join("rt");
    let sock_dir = rt.join("t3-watch");
    fs::create_dir_all(&sock_dir).unwrap();
    fs::set_permissions(&sock_dir, fs::Permissions::from_mode(0o700)).unwrap();
    fake_daemon(&sock_dir.join("daemon-e5.sock"), vec![]);
    let out = std::process::Command::new(BIN)
        .args(["outbox", "list"])
        .env_remove("T3_WATCH_HOME")
        .env_remove("T3_WATCH_SOCKET")
        .env("XDG_CONFIG_HOME", env.home.join("cfg"))
        .env("XDG_RUNTIME_DIR", &rt)
        .output()
        .unwrap();
    let list = output_json(&out);
    let socket = list["incompatibleSenders"][0]["socket"].as_str().unwrap_or("");
    assert!(socket.ends_with("rt/t3-watch/daemon-e5.sock"), "{list}");
}

#[test]
fn a_held_entry_left_beside_its_receipt_is_repaired_on_the_next_pass() {
    // The state a conversion cut short leaves, whichever build cut it: the
    // entry already held, its receipt still beside it. Left so, an engine-6
    // sender removes the entry beside the receipt.
    let env = Env::new();
    let hook = MockHook::start();
    let id = "4c4c4c4c";
    let v2 = left_by_engine_7(&env, &hook, id);
    let path = v2.join(format!("{id}.json"));
    let mut held: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    held["retainedAs"] = json!("expired");
    held["retainedExpiresMs"] = held["expiresMs"].clone();
    for k in ["expiresMs", "notBeforeMs", "nextAttemptMs"] {
        held[k] = json!(4_102_444_800_000u64);
    }
    fs::write(&path, held.to_string()).unwrap();
    assert_eq!(exposed_to(&v2, &held), [6], "{held}");

    let run = env.run(&["outbox", "run", "--once"]);
    let r = output_json(&run);
    assert_eq!(run.status.code(), Some(0), "{r}");
    assert!(
        !v2.join("receipts").join(format!("{id}.json")).exists(),
        "the receipt beside the held entry is still there: {r}"
    );
    let kept = entry_at(&v2, id).expect("the entry left outbox-v2");
    assert_retained(&v2, &kept, "expired");
    assert!(v2.join("sender.lock").is_file(), "{r}");
}

#[test]
fn a_pass_cut_short_has_already_held_every_entry_it_had_not_reached() {
    // Sol ordinal 12 shape, two entries: cut short while taking back the
    // first receipt, the pass must not leave the other entry expired and
    // unheld, which an engine-4 sender (a92f977's own, running beside its
    // client) dead-letters, so that the client reports it delivered.
    let env = Env::new();
    let hook = MockHook::start();
    let ids = ["6a6a6a6a", "6b6b6b6b"];
    let mut v2 = PathBuf::new();
    for id in ids {
        v2 = left_by_engine_7(&env, &hook, id);
    }
    let before: Vec<Vec<u32>> = ids
        .iter()
        .map(|id| exposed_to(&v2, &entry_at(&v2, id).unwrap()))
        .collect();
    assert_eq!(before, [vec![4, 6], vec![4, 6]]);
    let cut = env
        .cmd(&["outbox", "run", "--once"])
        .env("T3_WATCH_TEST_FAULT", "before-receipt-unlink")
        .output()
        .unwrap();
    assert_eq!(cut.status.code(), Some(97), "{}", both(&cut));
    for (id, before) in ids.iter().zip(&before) {
        let e = entry_at(&v2, id).expect("an entry left outbox-v2");
        assert_no_more_exposed(before, &v2, &e, "cut short");
        assert!(
            !exposed_to(&v2, &e).contains(&4),
            "{id} is still what an engine-4 sender dead-letters: {e}"
        );
    }
    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(0), "{}", both(&run));
    for id in ids {
        assert_retained(&v2, &entry_at(&v2, id).unwrap(), "expired");
    }
}
