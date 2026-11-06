//! Thread status rules shared by every backend.

use serde_json::{json, Map, Value};
use std::time::Instant;

/// Statuses that mean a thread still has work in flight. Everything else
/// (`completed`, `waiting`, `failed`, `interrupted`, `cancelled`,
/// `rolled_back`, `idle`, and any status added later) counts as settled.
pub const ACTIVE: [&str; 4] = ["preparing", "queued", "starting", "running"];

pub fn is_active(status: &str) -> bool {
    ACTIVE.contains(&status)
}

/// Extracts `N` from a run id like `run:thread:<id>:ordinal:N`.
pub fn run_ordinal(run_id: &str) -> Option<u64> {
    run_id.rsplit_once(":ordinal:")?.1.parse().ok()
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
    /// The last `t3_thread_wait` returned without timing out.
    pub wait_returned: bool,
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
            wait_returned: false,
            observed_at,
        }
    }

    pub fn ordinal(&self) -> Option<u64> {
        self.latest_run_id.as_deref().and_then(run_ordinal)
    }

    pub fn satisfies(&self, p: &Predicate) -> bool {
        let Some(status) = self.status.as_deref() else {
            return false;
        };
        if is_active(status) {
            return false;
        }
        if p.min_run_ordinal > 0 && self.ordinal().unwrap_or(0) < p.min_run_ordinal {
            return false;
        }
        !p.until_delegated || self.wait_returned
    }

    pub fn to_json(&self, p: &Predicate) -> Value {
        let mut m = Map::new();
        m.insert("status".into(), json!(self.status));
        m.insert("settled".into(), json!(self.satisfies(p)));
        m.insert("latestRunId".into(), json!(self.latest_run_id));
        m.insert("runOrdinal".into(), json!(self.ordinal()));
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
    fn settled_rules() {
        let p = Predicate::default();
        for s in ACTIVE {
            assert!(!state(s, "r:ordinal:1").satisfies(&p), "{s}");
        }
        for s in [
            "completed",
            "waiting",
            "failed",
            "interrupted",
            "cancelled",
            "rolled_back",
            "idle",
        ] {
            assert!(state(s, "r:ordinal:1").satisfies(&p), "{s}");
        }
        let mut unknown = state("completed", "r:ordinal:1");
        unknown.status = None;
        assert!(!unknown.satisfies(&p));
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
    fn delegated_needs_wait_to_return() {
        let p = Predicate {
            until_delegated: true,
            ..Default::default()
        };
        let mut s = state("completed", "run:thread:t:ordinal:1");
        assert!(!s.satisfies(&p));
        s.wait_returned = true;
        assert!(s.satisfies(&p));
    }

    #[test]
    fn json_strips_link_brackets() {
        let j = state("completed", "run:thread:t:ordinal:4").to_json(&Predicate::default());
        assert_eq!(j["link"], "t3-thread://v2/e/t");
        assert_eq!(j["runOrdinal"], 4);
        assert_eq!(j["settled"], true);
    }
}
