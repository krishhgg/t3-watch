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
use std::path::Path;
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
