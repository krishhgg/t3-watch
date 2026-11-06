//! Thread status rules shared by every backend.

use serde_json::{json, Map, Value};
use std::time::Instant;

/// Statuses that mean a thread still has work in flight.
pub const ACTIVE_STATUSES: &[&str] = &[
    "preparing",
    "queued",
    "starting",
    "running",
    "resuming",
    "cancelling",
    "interrupting",
    "stopping",
    "rolling_back",
    "pending",
];

/// Statuses that mean the thread has stopped working. Anything not in either
/// list (including an empty string) is unknown and never counts as settled:
/// a new status from a newer T3 must not wake callers early.
pub const SETTLED_STATUSES: &[&str] = &[
    "completed",
    "idle",
    "waiting",
    "failed",
    "error",
    "interrupted",
    "cancelled",
    "stopped",
    "rolled_back",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    Active,
    Settled,
    Unknown,
}

pub fn classify(status: &str) -> StatusClass {
    if ACTIVE_STATUSES.contains(&status) {
        StatusClass::Active
    } else if SETTLED_STATUSES.contains(&status) {
        StatusClass::Settled
    } else {
        StatusClass::Unknown
    }
}

/// A run id parsed as `run:thread:<percent-encoded thread id>:ordinal:<N>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRef {
    pub thread_id: String,
    pub ordinal: u64,
}

/// Parses a T3 run id. Anything that does not have this exact shape, with a
/// non-empty thread and a decimal ordinal, has no usable identity.
pub fn parse_run(run_id: &str) -> Option<RunRef> {
    let rest = run_id.strip_prefix("run:thread:")?;
    let (thread, ordinal) = rest.rsplit_once(":ordinal:")?;
    if thread.is_empty() || ordinal.is_empty() || !ordinal.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(RunRef {
        thread_id: crate::util::pct_decode(thread, false),
        ordinal: ordinal.parse().ok()?,
    })
}

/// The ordinal of `run_id`, only if it is a run of `thread_id`.
pub fn run_ordinal_of(thread_id: &str, run_id: &str) -> Option<u64> {
    parse_run(run_id)
        .filter(|r| r.thread_id == thread_id)
        .map(|r| r.ordinal)
}

/// Whether a `t3_thread_wait` that settled the `proven` runs (its `runId`
/// and `resultRun`) covers the run `t3_thread_read` saw afterwards.
///
/// Coverage needs full identity: a proven run counts only if it is a run of
/// the watched thread with an ordinal at least the latest one. A run of
/// another thread (a delegated child's `resultRun`, say) never proves
/// anything about the parent, whatever its ordinal. A missing latest run id
/// is unresolved unless the read said outright that the thread has no runs.
pub fn delegated_covered(thread_id: &str, proven: &[&str], latest: Option<&str>, run_count: Option<u64>) -> bool {
    let Some(latest) = latest else {
        return run_count == Some(0);
    };
    let Some(latest) = run_ordinal_of(thread_id, latest) else {
        return false;
    };
    proven
        .iter()
        .filter_map(|p| run_ordinal_of(thread_id, p))
        .any(|ordinal| ordinal >= latest)
}

/// What the caller is waiting for on each thread.
#[derive(Debug, Clone, Copy, Default)]
pub struct Predicate {
    /// The latest run must have at least this ordinal. 0 disables the check.
    pub min_run_ordinal: u64,
    /// Delegated and background work must have settled too (MCP only).
    pub until_delegated: bool,
}

/// One observation of a thread, from either backend.
#[derive(Debug, Clone)]
pub struct ThreadState {
    /// The thread this observation is for (what the caller asked about).
    pub thread_id: String,
    pub status: Option<String>,
    pub latest_run_id: Option<String>,
    /// `runCount`, when the server reported it.
    pub run_count: Option<u64>,
    /// The server's response named a different thread than the one asked for.
    pub foreign_thread: bool,
    pub title: Option<String>,
    pub link: Option<String>,
    pub pending_requests: Option<u64>,
    /// `resultRun` from `t3_thread_wait` when waiting on delegated work.
    pub result_run_id: Option<String>,
    /// A `t3_thread_wait` with `untilDelegatedWorkSettled` proved that the
    /// delegated work of `latest_run_id` (or a later run of the same thread)
    /// has settled.
    pub delegated_settled: bool,
    /// When the request that produced this observation was sent. A new
    /// subscriber ignores anything older than its own registration, so a
    /// cached "completed" never wakes someone who just queued a new turn.
    pub observed_at: Instant,
}

impl ThreadState {
    pub fn from_thread_json(v: &Value, thread_id: &str, observed_at: Instant) -> Self {
        let t = v.get("thread").filter(|t| t.is_object()).unwrap_or(v);
        let s = |k: &str| t.get(k).and_then(Value::as_str).map(str::to_string);
        ThreadState {
            thread_id: thread_id.to_string(),
            status: s("status"),
            latest_run_id: s("latestRunId").filter(|r| !r.is_empty()),
            run_count: t.get("runCount").and_then(Value::as_u64),
            foreign_thread: s("threadId").is_some_and(|id| id != thread_id),
            title: s("title"),
            link: s("link").map(|l| l.trim_start_matches('<').trim_end_matches('>').to_string()),
            pending_requests: t.get("pendingRequestCount").and_then(Value::as_u64),
            result_run_id: None,
            delegated_settled: false,
            observed_at,
        }
    }

    pub fn class(&self) -> StatusClass {
        self.status.as_deref().map_or(StatusClass::Unknown, classify)
    }

    /// The latest run's ordinal, only when that run belongs to this thread.
    pub fn ordinal(&self) -> Option<u64> {
        self.latest_run_id
            .as_deref()
            .and_then(|r| run_ordinal_of(&self.thread_id, r))
    }

    pub fn satisfies(&self, p: &Predicate) -> bool {
        if self.foreign_thread || self.class() != StatusClass::Settled {
            return false;
        }
        if p.min_run_ordinal > 0 && self.ordinal().unwrap_or(0) < p.min_run_ordinal {
            return false;
        }
        !p.until_delegated || self.delegated_settled
    }

    /// Why an observation can never settle, when the server sent something
    /// this version does not understand.
    pub fn diagnostic(&self) -> Option<String> {
        if self.foreign_thread {
            return Some("server answered for a different thread; treated as not settled".into());
        }
        if let Some(run) = self.latest_run_id.as_deref() {
            if self.ordinal().is_none() {
                return Some(format!(
                    "latest run id {:?} is not a run of this thread; treated as unknown",
                    crate::util::sanitize_text(run, 120)
                ));
            }
        }
        match self.status.as_deref() {
            None | Some("") => Some("server reported no thread status".into()),
            Some(s) if classify(s) == StatusClass::Unknown => Some(format!(
                "unrecognized thread status {:?}; treated as not settled",
                crate::util::sanitize_text(s, 64)
            )),
            _ => None,
        }
    }

    pub fn to_json(&self, p: &Predicate) -> Value {
        let mut m = Map::new();
        m.insert("status".into(), json!(self.status));
        m.insert("settled".into(), json!(self.satisfies(p)));
        m.insert("latestRunId".into(), json!(self.latest_run_id));
        m.insert("runOrdinal".into(), json!(self.ordinal()));
        if p.until_delegated {
            m.insert("delegatedSettled".into(), json!(self.delegated_settled));
        }
        if let Some(t) = &self.title {
            m.insert("title".into(), json!(t));
        }
        if let Some(n) = self.pending_requests.filter(|n| *n > 0) {
            m.insert("pendingRequestCount".into(), json!(n));
        }
        if let Some(r) = &self.result_run_id {
            m.insert("resultRunId".into(), json!(r));
        }
        if let Some(l) = &self.link {
            m.insert("link".into(), json!(l));
        }
        if let Some(d) = self.diagnostic() {
            m.insert("diagnostic".into(), json!(d));
        }
        Value::Object(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "mcp:launch:parent";

    fn run(thread: &str, n: u64) -> String {
        format!("run:thread:{}:ordinal:{n}", crate::util::pct_encode(thread))
    }

    fn state(status: &str, latest: &str) -> ThreadState {
        ThreadState::from_thread_json(
            &json!({"thread": {"threadId": T, "status": status, "latestRunId": latest, "link": "<t3-thread://v2/e/t>"}}),
            T,
            Instant::now(),
        )
    }

    #[test]
    fn run_ids_are_parsed_with_their_thread() {
        let r = parse_run("run:thread:mcp%3Alaunch%3Aabc:ordinal:12").unwrap();
        assert_eq!((r.thread_id.as_str(), r.ordinal), ("mcp:launch:abc", 12));
        // Raw (unencoded) thread ids work too.
        assert_eq!(
            parse_run("run:thread:mcp:launch:abc:ordinal:3").unwrap().thread_id,
            "mcp:launch:abc"
        );
        for bad in [
            "run:thread:x",
            "run:thread:x:ordinal:",
            "run:thread::ordinal:1",
            "r:ordinal:1",
            "run:thread:x:ordinal:+1",
            "run:thread:x:ordinal:1a",
        ] {
            assert!(parse_run(bad).is_none(), "{bad}");
        }
        assert_eq!(run_ordinal_of(T, &run(T, 4)), Some(4));
        assert_eq!(run_ordinal_of(T, &run("mcp:launch:child", 4)), None);
    }

    #[test]
    fn known_statuses_are_classified() {
        let p = Predicate::default();
        for s in ACTIVE_STATUSES {
            assert_eq!(classify(s), StatusClass::Active);
            assert!(!state(s, &run(T, 1)).satisfies(&p), "{s}");
        }
        for s in SETTLED_STATUSES {
            assert_eq!(classify(s), StatusClass::Settled);
            assert!(state(s, &run(T, 1)).satisfies(&p), "{s}");
        }
    }

    #[test]
    fn unknown_and_empty_statuses_never_settle() {
        let p = Predicate::default();
        for s in ["", "archived_v9", "COMPLETED", " completed"] {
            let st = state(s, &run(T, 1));
            assert_eq!(st.class(), StatusClass::Unknown, "{s:?}");
            assert!(!st.satisfies(&p), "{s:?}");
            assert!(st.diagnostic().is_some(), "{s:?}");
            assert_eq!(st.to_json(&p)["settled"], false);
        }
        let mut missing = state("completed", &run(T, 1));
        missing.status = None;
        assert!(!missing.satisfies(&p));
        assert!(missing.to_json(&p)["diagnostic"]
            .as_str()
            .unwrap()
            .contains("no thread status"));
        assert!(state("completed", &run(T, 1)).diagnostic().is_none());
    }

    #[test]
    fn stale_ordinal_never_settles() {
        let p = Predicate {
            min_run_ordinal: 2,
            ..Default::default()
        };
        assert!(!state("completed", &run(T, 1)).satisfies(&p));
        assert!(!state("running", &run(T, 2)).satisfies(&p));
        assert!(state("completed", &run(T, 2)).satisfies(&p));
        assert!(state("completed", &run(T, 3)).satisfies(&p));
        let mut no_runs = state("idle", "");
        no_runs.latest_run_id = None;
        assert!(!no_runs.satisfies(&p));
    }

    #[test]
    fn another_threads_run_never_counts_as_this_threads_ordinal() {
        let p = Predicate {
            min_run_ordinal: 2,
            ..Default::default()
        };
        let st = state("completed", &run("mcp:launch:child", 100));
        assert_eq!(st.ordinal(), None);
        assert!(!st.satisfies(&p));
        assert!(st.diagnostic().unwrap().contains("not a run of this thread"));
        // Without an ordinal requirement the status alone still settles.
        assert!(st.satisfies(&Predicate::default()));
    }

    #[test]
    fn an_answer_about_another_thread_never_settles() {
        let st = ThreadState::from_thread_json(
            &json!({"thread": {"threadId": "mcp:launch:other", "status": "completed", "latestRunId": run(T, 1)}}),
            T,
            Instant::now(),
        );
        assert!(!st.satisfies(&Predicate::default()));
        assert!(st.diagnostic().unwrap().contains("different thread"));
    }

    #[test]
    fn delegated_needs_a_proof() {
        let p = Predicate {
            until_delegated: true,
            ..Default::default()
        };
        let mut s = state("completed", &run(T, 1));
        assert!(!s.satisfies(&p));
        s.delegated_settled = true;
        assert!(s.satisfies(&p));
        assert_eq!(s.to_json(&p)["delegatedSettled"], true);
    }

    #[test]
    fn delegated_proof_needs_the_same_thread_and_a_new_enough_run() {
        let (p1, p2, p3) = (run(T, 1), run(T, 2), run(T, 3));
        let child = run("mcp:launch:child", 100);
        // Sol's repro: wait proved parent 1 and child 100; the read sees parent 2.
        assert!(!delegated_covered(T, &[&p1, &child], Some(&p2), Some(2)));
        // A child's run never covers the parent, whatever its ordinal.
        assert!(!delegated_covered(T, &[&child], Some(&p1), Some(1)));
        // The wait proved the run the read saw, or a later one of the same thread.
        assert!(delegated_covered(T, &[&p2], Some(&p2), Some(2)));
        assert!(delegated_covered(T, &[&p1, &p3], Some(&p2), Some(3)));
        assert!(!delegated_covered(T, &[&p1], Some(&p2), Some(2)));
        assert!(!delegated_covered(T, &[], Some(&p1), Some(1)));
        // Ids without a parseable identity never prove anything.
        assert!(!delegated_covered(T, &["opaque-id"], Some("opaque-id"), None));
        assert!(!delegated_covered(T, &[&p2], Some("opaque-id"), None));
        // A missing latest run is unresolved unless the server said "no runs".
        assert!(!delegated_covered(T, &[&p1], None, None));
        assert!(!delegated_covered(T, &[&p1], None, Some(1)));
        assert!(delegated_covered(T, &[], None, Some(0)));
    }

    #[test]
    fn json_strips_link_brackets() {
        let j = state("completed", &run(T, 4)).to_json(&Predicate::default());
        assert_eq!(j["link"], "t3-thread://v2/e/t");
        assert_eq!(j["runOrdinal"], 4);
        assert_eq!(j["settled"], true);
        assert!(j.get("diagnostic").is_none());
    }
}
