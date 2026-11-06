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

/// Extracts `N` from a run id like `run:thread:<id>:ordinal:N`.
pub fn run_ordinal(run_id: &str) -> Option<u64> {
    run_id.rsplit_once(":ordinal:")?.1.parse().ok()
}

/// True when one of the `proven` runs is the `latest` run or a later one.
/// Used to bind a delegated-work proof from `t3_thread_wait` to the run that
/// `t3_thread_read` saw afterwards.
pub fn run_covered(proven: &[&str], latest: Option<&str>) -> bool {
    let Some(latest) = latest else {
        // No runs at all: nothing can still be delegated.
        return true;
    };
    proven.iter().any(|p| {
        *p == latest
            || match (run_ordinal(p), run_ordinal(latest)) {
                (Some(a), Some(b)) => a >= b,
                _ => false,
            }
    })
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
    pub status: Option<String>,
    pub latest_run_id: Option<String>,
    pub title: Option<String>,
    pub link: Option<String>,
    pub pending_requests: Option<u64>,
    /// `resultRun` from `t3_thread_wait` when waiting on delegated work.
    pub result_run_id: Option<String>,
    /// A `t3_thread_wait` with `untilDelegatedWorkSettled` proved that the
    /// delegated work of `latest_run_id` (or a later run) has settled.
    pub delegated_settled: bool,
    /// When the request that produced this observation was sent. A new
    /// subscriber ignores anything older than its own registration, so a
    /// cached "completed" never wakes someone who just queued a new turn.
    pub observed_at: Instant,
}

impl ThreadState {
    pub fn from_thread_json(v: &Value, observed_at: Instant) -> Self {
        let t = v.get("thread").filter(|t| t.is_object()).unwrap_or(v);
        let s = |k: &str| t.get(k).and_then(Value::as_str).map(str::to_string);
        ThreadState {
            status: s("status"),
            latest_run_id: s("latestRunId"),
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

    pub fn ordinal(&self) -> Option<u64> {
        self.latest_run_id.as_deref().and_then(run_ordinal)
    }

    pub fn satisfies(&self, p: &Predicate) -> bool {
        if self.class() != StatusClass::Settled {
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

    fn state(status: &str, run: &str) -> ThreadState {
        ThreadState::from_thread_json(
            &json!({"thread": {"status": status, "latestRunId": run, "link": "<t3-thread://v2/e/t>"}}),
            Instant::now(),
        )
    }

    #[test]
    fn ordinals() {
        assert_eq!(run_ordinal("run:thread:mcp%3Alaunch%3Aabc:ordinal:12"), Some(12));
        assert_eq!(run_ordinal("run:thread:x"), None);
        assert_eq!(run_ordinal("run:thread:x:ordinal:"), None);
    }

    #[test]
    fn known_statuses_are_classified() {
        let p = Predicate::default();
        for s in ACTIVE_STATUSES {
            assert_eq!(classify(s), StatusClass::Active);
            assert!(!state(s, "r:ordinal:1").satisfies(&p), "{s}");
        }
        for s in SETTLED_STATUSES {
            assert_eq!(classify(s), StatusClass::Settled);
            assert!(state(s, "r:ordinal:1").satisfies(&p), "{s}");
        }
    }

    #[test]
    fn unknown_and_empty_statuses_never_settle() {
        let p = Predicate::default();
        for s in ["", "archived_v9", "COMPLETED", " completed"] {
            let st = state(s, "r:ordinal:1");
            assert_eq!(st.class(), StatusClass::Unknown, "{s:?}");
            assert!(!st.satisfies(&p), "{s:?}");
            assert!(st.diagnostic().is_some(), "{s:?}");
            assert_eq!(st.to_json(&p)["settled"], false);
        }
        let mut missing = state("completed", "r:ordinal:1");
        missing.status = None;
        assert!(!missing.satisfies(&p));
        assert!(missing.to_json(&p)["diagnostic"]
            .as_str()
            .unwrap()
            .contains("no thread status"));
        assert!(state("completed", "r:ordinal:1").diagnostic().is_none());
    }

    #[test]
    fn stale_ordinal_never_settles() {
        let p = Predicate {
            min_run_ordinal: 2,
            ..Default::default()
        };
        assert!(!state("completed", "run:thread:t:ordinal:1").satisfies(&p));
        assert!(!state("running", "run:thread:t:ordinal:2").satisfies(&p));
        assert!(state("completed", "run:thread:t:ordinal:2").satisfies(&p));
        assert!(state("completed", "run:thread:t:ordinal:3").satisfies(&p));
        let mut no_runs = state("idle", "");
        no_runs.latest_run_id = None;
        assert!(!no_runs.satisfies(&p));
    }

    #[test]
    fn delegated_needs_a_proof() {
        let p = Predicate {
            until_delegated: true,
            ..Default::default()
        };
        let mut s = state("completed", "run:thread:t:ordinal:1");
        assert!(!s.satisfies(&p));
        s.delegated_settled = true;
        assert!(s.satisfies(&p));
        assert_eq!(s.to_json(&p)["delegatedSettled"], true);
    }

    #[test]
    fn delegated_proof_is_bound_to_the_run() {
        let r = |n: u64| format!("run:thread:t:ordinal:{n}");
        // The wait proved run 1; the read already shows run 2: not covered.
        assert!(!run_covered(&[&r(1)], Some(&r(2))));
        assert!(run_covered(&[&r(2)], Some(&r(2))));
        // resultRun can name a later run than runId.
        assert!(run_covered(&[&r(1), &r(3)], Some(&r(2))));
        assert!(!run_covered(&[], Some(&r(1))));
        assert!(run_covered(&["opaque-id"], Some("opaque-id")));
        assert!(!run_covered(&["opaque-a"], Some("opaque-b")));
        assert!(run_covered(&[], None));
    }

    #[test]
    fn json_strips_link_brackets() {
        let j = state("completed", "run:thread:t:ordinal:4").to_json(&Predicate::default());
        assert_eq!(j["link"], "t3-thread://v2/e/t");
        assert_eq!(j["runOrdinal"], 4);
        assert_eq!(j["settled"], true);
        assert!(j.get("diagnostic").is_none());
    }
}
