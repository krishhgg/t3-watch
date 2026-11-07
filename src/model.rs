//! Thread status rules shared by every backend.
//!
//! Each observation of a thread means one of four things to a caller: still
//! pending, completed (a success status), failed (any other terminal status)
//! or needs input (a question or approval waits on someone). Needs-input is
//! checked first: a blocked thread never finishes by itself, and it is never
//! reported as completion.

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

/// The settled statuses that count as success. `waiting` means needs-input;
/// every other settled status is a failure.
pub const SUCCESS_STATUSES: &[&str] = &["completed", "idle"];

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

/// Whether the workstream proof from `t3_thread_wait` with
/// `untilDelegatedWorkSettled` covers what `t3_thread_read` saw afterwards.
///
/// `result_run` is the wait's `resultRun`: the run T3 names as holding the
/// final answer once delegated work and the runs delivering its completions
/// have settled. It must be a run of the watched thread itself, at least as
/// new as the latest run the read saw. A run of another thread (a delegated
/// child's) never proves anything about the parent, whatever its ordinal. A
/// missing latest run id is unresolved unless the read said outright that
/// there are no runs.
pub fn workstream_covered(
    thread_id: &str,
    result_run: Option<&str>,
    latest: Option<&str>,
    run_count: Option<u64>,
) -> bool {
    let Some(latest) = latest else {
        return run_count == Some(0);
    };
    let (Some(latest), Some(result)) = (
        run_ordinal_of(thread_id, latest),
        result_run.and_then(|r| run_ordinal_of(thread_id, r)),
    ) else {
        return false;
    };
    result >= latest
}

/// What the caller is waiting for on each thread.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Predicate {
    /// The deciding run must have at least this ordinal. 0 disables it.
    pub min_run_ordinal: u64,
    /// Workstream mode (MCP only): the run, its delegated and background
    /// work, and the parent runs that deliver their completions must all
    /// have settled. Off means turn mode: the latest run finished.
    pub until_delegated: bool,
    /// The final assistant message of the deciding run must contain this.
    pub final_marker: Option<String>,
    /// A `pendingKey` the caller already knows about: that exact set of
    /// pending requests does not wake it again.
    pub acknowledged: Option<String>,
    /// A failed run does not end the wait; a later run of the same thread
    /// is followed instead (the failure is reported as progress).
    pub follow: bool,
}

/// What one thread means for the caller right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadOutcome {
    Pending,
    Completed,
    Failed,
    NeedsInput,
}

impl ThreadOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadOutcome::Pending => "pending",
            ThreadOutcome::Completed => "completed",
            ThreadOutcome::Failed => "failed",
            ThreadOutcome::NeedsInput => "needs_input",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, ThreadOutcome::Completed | ThreadOutcome::Failed)
    }
}

/// A question or approval waiting on a thread, holding only safe text.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRequest {
    /// `question`, `approval` or `request`.
    pub kind: String,
    pub item_id: String,
    pub request_id: Option<String>,
    /// `{header, question, options: [label...]}`, redacted and truncated.
    pub questions: Vec<Value>,
}

impl PendingRequest {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("kind".into(), json!(self.kind));
        m.insert("itemId".into(), json!(self.item_id));
        if let Some(r) = &self.request_id {
            m.insert("requestId".into(), json!(r));
        }
        // Approvals are the user's to give; a bot must not try to answer.
        m.insert("userOnly".into(), json!(self.kind != "question"));
        if !self.questions.is_empty() {
            m.insert("questions".into(), json!(self.questions));
        }
        Value::Object(m)
    }
}

/// One observation of a thread, from either backend.
#[derive(Debug, Clone)]
pub struct ThreadState {
    /// The thread this observation is for (what the caller asked about).
    pub thread_id: String,
    pub status: Option<String>,
    pub latest_run_id: Option<String>,
    /// The run in progress, if the server reported one.
    pub active_run_id: Option<String>,
    /// `runCount`, when the server reported it.
    pub run_count: Option<u64>,
    pub item_count: Option<u64>,
    /// Set when this thread is a delegated child of another thread.
    pub parent_thread_id: Option<String>,
    /// The server's response named a different thread than the one asked for.
    pub foreign_thread: bool,
    pub title: Option<String>,
    pub link: Option<String>,
    pub pending_requests: Option<u64>,
    /// Details of pending requests, when the backend can read them.
    pub requests: Vec<PendingRequest>,
    /// Ids from `t3_pending_request list` (questions only; T3 does not list
    /// approvals there).
    pub pending_ids: Vec<String>,
    /// `resultRun` from `t3_thread_wait` in workstream mode, and the status
    /// T3 gave for that run. The outcome of a proven workstream is that
    /// run's, not the header's.
    pub result_run_id: Option<String>,
    pub result_status: Option<String>,
    /// Workstream mode: a `t3_thread_wait` with `untilDelegatedWorkSettled`
    /// proved that the latest run, its delegated and background work and the
    /// runs delivering their completions have all settled.
    pub delegated_settled: bool,
    /// Why the workstream proof was not accepted, when it was not.
    pub proof_note: Option<String>,
    /// A later response in the same cycle contradicted this observation (a
    /// newer run, active work, a changed status): it may report input but
    /// never completion or failure until a fresh cycle confirms it.
    pub recheck: Option<String>,
    /// Turn mode: T3 reported delegated work still running when this turn
    /// ended. A hint for the caller, never a reason to keep waiting.
    pub delegated_pending_hint: Option<bool>,
    /// `--final-marker`: whether the deciding run's final assistant message
    /// contains it. `None` when not checked (yet).
    pub marker_seen: Option<bool>,
    /// When the request that produced this observation was sent. A new
    /// subscriber ignores anything older than its own registration, so a
    /// cached "completed" never wakes someone who just queued a new turn.
    pub observed_at: Instant,
}

impl ThreadState {
    pub fn from_thread_json(v: &Value, thread_id: &str, observed_at: Instant) -> Self {
        let t = v.get("thread").filter(|t| t.is_object()).unwrap_or(v);
        let s = |k: &str| {
            t.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        ThreadState {
            thread_id: thread_id.to_string(),
            status: t.get("status").and_then(Value::as_str).map(str::to_string),
            latest_run_id: s("latestRunId"),
            active_run_id: s("activeRunId"),
            run_count: t.get("runCount").and_then(Value::as_u64),
            item_count: t.get("itemCount").and_then(Value::as_u64),
            parent_thread_id: s("parentThreadId"),
            foreign_thread: s("threadId").is_some_and(|id| id != thread_id),
            title: s("title"),
            link: s("link").map(|l| l.trim_start_matches('<').trim_end_matches('>').to_string()),
            pending_requests: t.get("pendingRequestCount").and_then(Value::as_u64),
            requests: Vec::new(),
            pending_ids: Vec::new(),
            result_run_id: None,
            result_status: None,
            delegated_settled: false,
            proof_note: None,
            recheck: None,
            delegated_pending_hint: None,
            marker_seen: None,
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

    /// The run whose result decides the outcome: the workstream's result run
    /// once proven, otherwise the latest run.
    pub fn deciding_run(&self) -> Option<&str> {
        if self.delegated_settled {
            self.result_run_id.as_deref().or(self.latest_run_id.as_deref())
        } else {
            self.latest_run_id.as_deref()
        }
    }

    /// A question or approval is waiting: status `waiting`, or a pending
    /// request while the run still says `running` (an approval prompt blocks
    /// a run without changing its status).
    pub fn needs_input(&self) -> bool {
        self.status.as_deref() == Some("waiting")
            || self.pending_requests.unwrap_or(0) > 0
            || !self.requests.is_empty()
            || !self.pending_ids.is_empty()
    }

    /// A stable name for the current set of pending requests, so a caller
    /// can say "I know about these" (`--acknowledge`) and be woken only for
    /// new ones. Built from request ids when known, else from the count, the
    /// status and the latest run.
    pub fn pending_key(&self) -> String {
        // Request ids where known, so the timeline and `t3_pending_request
        // list` name the same request the same way.
        let mut ids: Vec<&str> = self
            .requests
            .iter()
            .map(|r| r.request_id.as_deref().unwrap_or(&r.item_id))
            .collect();
        ids.extend(self.pending_ids.iter().map(String::as_str));
        ids.sort_unstable();
        ids.dedup();
        let basis = if ids.is_empty() {
            format!(
                "count:{}:status:{}:run:{}",
                self.pending_requests.unwrap_or(0),
                self.status.as_deref().unwrap_or(""),
                self.latest_run_id.as_deref().unwrap_or("")
            )
        } else {
            format!("ids:{}", ids.join("\n"))
        };
        let digest = crate::util::sha256(format!("{}\n{basis}", self.thread_id).as_bytes());
        format!("pk-{}", &crate::util::hex(&digest)[..16])
    }

    /// Whether the thread is far enough along for its terminal status to
    /// count: the right run, the workstream proven, the marker seen.
    /// The status of the run whose result decides the outcome. In a proven
    /// workstream that is `resultRun`'s status as T3 reported it; the
    /// header's status stands in only when the header is about that same
    /// run. `None`: unknown, so neither success nor failure.
    pub fn deciding_status(&self) -> Option<&str> {
        if !self.delegated_settled {
            return self.status.as_deref();
        }
        if let Some(s) = self.result_status.as_deref() {
            return Some(s);
        }
        let same_run = match (self.result_run_id.as_deref(), self.latest_run_id.as_deref()) {
            (Some(r), Some(l)) => r == l || parse_run(r).is_some_and(|r| parse_run(l) == Some(r)),
            (None, _) => true,
            _ => false,
        };
        if same_run {
            self.status.as_deref()
        } else {
            None
        }
    }

    pub fn outcome(&self, p: &Predicate) -> ThreadOutcome {
        if self.foreign_thread {
            return ThreadOutcome::Pending;
        }
        if self.needs_input() {
            if p.acknowledged.as_deref() == Some(self.pending_key().as_str()) {
                return ThreadOutcome::Pending;
            }
            return ThreadOutcome::NeedsInput;
        }
        if self.recheck.is_some() || self.active_run_id.is_some() || self.class() != StatusClass::Settled {
            return ThreadOutcome::Pending;
        }
        if p.min_run_ordinal > 0 && self.ordinal().unwrap_or(0) < p.min_run_ordinal {
            return ThreadOutcome::Pending;
        }
        // In workstream mode nothing is final before the proof: a failed
        // parent turn's delegated work may still start new runs.
        if p.until_delegated && !self.delegated_settled {
            return ThreadOutcome::Pending;
        }
        let success = |s: &str| SUCCESS_STATUSES.contains(&s);
        let header_ok = self.status.as_deref().is_some_and(success);
        match self.deciding_status() {
            None => ThreadOutcome::Pending,
            Some(s) if classify(s) != StatusClass::Settled => ThreadOutcome::Pending,
            // A failed deciding run, or a failed latest run after a completed
            // result run, is a failure: never reported as success. It does
            // not need the final marker, which confirms success only.
            Some(s) if !success(s) || !header_ok => ThreadOutcome::Failed,
            Some(_) if p.final_marker.is_some() && self.marker_seen != Some(true) => ThreadOutcome::Pending,
            Some(_) => ThreadOutcome::Completed,
        }
    }

    /// The thread reached a terminal outcome (completed or failed).
    #[cfg(test)]
    pub fn satisfies(&self, p: &Predicate) -> bool {
        self.outcome(p).is_terminal()
    }

    /// Why an observation is not (yet) what it looks like.
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
        if let Some(note) = &self.recheck {
            return Some(note.clone());
        }
        if let Some(note) = &self.proof_note {
            return Some(note.clone());
        }
        if self.marker_seen == Some(false) {
            return Some("the final assistant message does not contain the final marker yet".into());
        }
        if self.delegated_pending_hint == Some(true) {
            return Some(
                "this turn finished, but T3 reports delegated work still running; \
                 use --until workstream to wait for the whole workstream"
                    .into(),
            );
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
        let outcome = self.outcome(p);
        let mut m = Map::new();
        m.insert("status".into(), json!(self.status));
        m.insert("outcome".into(), json!(outcome.as_str()));
        m.insert("settled".into(), json!(outcome.is_terminal()));
        m.insert("latestRunId".into(), json!(self.latest_run_id));
        m.insert("runOrdinal".into(), json!(self.ordinal()));
        if self.active_run_id.is_some() {
            m.insert("activeRunId".into(), json!(self.active_run_id));
        }
        if p.until_delegated {
            m.insert("delegatedSettled".into(), json!(self.delegated_settled));
        }
        if let Some(r) = &self.result_run_id {
            m.insert("resultRunId".into(), json!(r));
        }
        if let Some(r) = &self.result_status {
            m.insert("resultRunStatus".into(), json!(r));
        }
        if let Some(h) = self.delegated_pending_hint {
            m.insert("delegatedWorkPending".into(), json!(h));
        }
        if let Some(seen) = self.marker_seen {
            m.insert("finalMarkerSeen".into(), json!(seen));
        }
        if let Some(t) = &self.title {
            m.insert("title".into(), json!(t));
        }
        if let Some(parent) = &self.parent_thread_id {
            m.insert("parentThreadId".into(), json!(parent));
        }
        if let Some(n) = self.pending_requests.filter(|n| *n > 0) {
            m.insert("pendingRequestCount".into(), json!(n));
        }
        if self.needs_input() {
            m.insert("pendingKey".into(), json!(self.pending_key()));
            m.insert(
                "requests".into(),
                Value::Array(self.requests.iter().map(PendingRequest::to_json).collect()),
            );
            if !self.pending_ids.is_empty() {
                m.insert("pendingRequestIds".into(), json!(self.pending_ids));
            }
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

/// Item statuses after which a request no longer waits on anyone.
const RESOLVED: &[&str] = &[
    "completed",
    "cancelled",
    "canceled",
    "declined",
    "denied",
    "approved",
    "rejected",
    "failed",
    "expired",
    "resolved",
    "answered",
    "interrupted",
];

/// Unresolved requests among a thread's activity items: `user_input_request`
/// items (questions), and approval or permission items. Question text is
/// redacted and truncated; option descriptions are dropped.
pub fn pending_from_activity(items: &[Value]) -> Vec<PendingRequest> {
    let mut out = Vec::new();
    for item in items {
        let kind_raw = item.get("type").and_then(Value::as_str).unwrap_or("");
        let kind = if kind_raw == "user_input_request" {
            "question"
        } else if kind_raw.contains("approval") || kind_raw.contains("permission") {
            "approval"
        } else if kind_raw.ends_with("_request") {
            "request"
        } else {
            continue;
        };
        let status = item.get("status").and_then(Value::as_str).unwrap_or("");
        if RESOLVED.contains(&status) {
            continue;
        }
        let item_id = item.get("itemId").and_then(Value::as_str).unwrap_or("").to_string();
        let request_id = item_id
            .strip_prefix("turn-item:runtime-request:")
            .map(|r| crate::util::pct_decode(r, false));
        let mut questions = Vec::new();
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            match serde_json::from_str::<Value>(text) {
                Ok(Value::Array(qs)) => {
                    for q in qs.iter().take(5) {
                        let field = |k: &str| q.get(k).and_then(Value::as_str).map(|t| crate::util::safe_text(t, 600));
                        let options: Vec<String> = q
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|o| {
                                o.iter()
                                    .take(8)
                                    .filter_map(|x| x.get("label").and_then(Value::as_str))
                                    .map(|l| crate::util::safe_text(l, 120))
                                    .collect()
                            })
                            .unwrap_or_default();
                        questions.push(json!({
                            "header": field("header"),
                            "question": field("question"),
                            "options": options,
                            "multiSelect": q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                        }));
                    }
                }
                _ if !text.trim().is_empty() => {
                    questions.push(json!({ "question": crate::util::safe_text(text, 600) }));
                }
                _ => {}
            }
        }
        out.push(PendingRequest {
            kind: kind.to_string(),
            item_id,
            request_id,
            questions,
        });
    }
    out
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

    fn p() -> Predicate {
        Predicate::default()
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
    fn success_failure_and_input_are_distinct_outcomes() {
        for s in ACTIVE_STATUSES {
            assert_eq!(classify(s), StatusClass::Active);
            assert_eq!(state(s, &run(T, 1)).outcome(&p()), ThreadOutcome::Pending, "{s}");
        }
        for s in SETTLED_STATUSES {
            assert_eq!(classify(s), StatusClass::Settled);
        }
        for s in ["completed", "idle"] {
            assert_eq!(state(s, &run(T, 1)).outcome(&p()), ThreadOutcome::Completed, "{s}");
        }
        for s in ["failed", "error", "interrupted", "cancelled", "stopped", "rolled_back"] {
            let st = state(s, &run(T, 1));
            assert_eq!(st.outcome(&p()), ThreadOutcome::Failed, "{s}");
            assert_eq!(st.to_json(&p())["outcome"], "failed");
        }
        let waiting = state("waiting", &run(T, 1));
        assert_eq!(waiting.outcome(&p()), ThreadOutcome::NeedsInput);
        assert!(!waiting.satisfies(&p()));
        assert_eq!(waiting.to_json(&p())["settled"], false);
    }

    #[test]
    fn a_pending_request_while_running_needs_input() {
        let mut st = state("running", &run(T, 1));
        assert_eq!(st.outcome(&p()), ThreadOutcome::Pending);
        st.pending_requests = Some(1);
        assert_eq!(st.outcome(&p()), ThreadOutcome::NeedsInput);
        // Whatever run the caller waits for: a blocked thread will not get there.
        let later = Predicate {
            min_run_ordinal: 9,
            until_delegated: true,
            ..Default::default()
        };
        assert_eq!(st.outcome(&later), ThreadOutcome::NeedsInput);
        let j = st.to_json(&p());
        assert_eq!(j["outcome"], "needs_input");
        assert_eq!(j["pendingRequestCount"], 1);
        assert!(j["pendingKey"].as_str().unwrap().starts_with("pk-"));
    }

    #[test]
    fn acknowledged_input_waits_on_but_a_new_request_wakes() {
        let mut st = state("running", &run(T, 1));
        st.pending_requests = Some(1);
        st.requests = vec![PendingRequest {
            kind: "question".into(),
            item_id: "turn-item:runtime-request:a".into(),
            request_id: Some("a".into()),
            questions: vec![],
        }];
        let key = st.pending_key();
        assert_eq!(key, st.clone().pending_key(), "stable");
        let ack = Predicate {
            acknowledged: Some(key.clone()),
            ..Default::default()
        };
        assert_eq!(st.outcome(&ack), ThreadOutcome::Pending);
        st.requests.push(PendingRequest {
            kind: "approval".into(),
            item_id: "turn-item:runtime-request:b".into(),
            request_id: Some("b".into()),
            questions: vec![],
        });
        assert_ne!(st.pending_key(), key);
        assert_eq!(st.outcome(&ack), ThreadOutcome::NeedsInput);
        // Answered and finished: the acknowledgement no longer matters.
        assert_eq!(state("completed", &run(T, 1)).outcome(&ack), ThreadOutcome::Completed);
    }

    #[test]
    fn unknown_and_empty_statuses_never_settle() {
        for s in ["", "archived_v9", "COMPLETED", " completed"] {
            let st = state(s, &run(T, 1));
            assert_eq!(st.class(), StatusClass::Unknown, "{s:?}");
            assert_eq!(st.outcome(&p()), ThreadOutcome::Pending, "{s:?}");
            assert!(st.diagnostic().is_some(), "{s:?}");
            assert_eq!(st.to_json(&p())["settled"], false);
        }
        let mut missing = state("completed", &run(T, 1));
        missing.status = None;
        assert!(!missing.satisfies(&p()));
        assert!(missing.to_json(&p())["diagnostic"]
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
        assert!(
            !state("failed", &run(T, 1)).satisfies(&p),
            "an older failure is not ours"
        );
        assert!(!state("running", &run(T, 2)).satisfies(&p));
        assert!(state("completed", &run(T, 2)).satisfies(&p));
        assert!(state("completed", &run(T, 3)).satisfies(&p));
        let mut no_runs = state("idle", "");
        no_runs.latest_run_id = None;
        assert!(!no_runs.satisfies(&p));
    }

    #[test]
    fn another_threads_run_never_counts_as_this_threads_ordinal() {
        let p2 = Predicate {
            min_run_ordinal: 2,
            ..Default::default()
        };
        let st = state("completed", &run("mcp:launch:child", 100));
        assert_eq!(st.ordinal(), None);
        assert!(!st.satisfies(&p2));
        assert!(st.diagnostic().unwrap().contains("not a run of this thread"));
        // Without an ordinal requirement the status alone still settles.
        assert!(st.satisfies(&p()));
    }

    #[test]
    fn an_answer_about_another_thread_never_settles() {
        let st = ThreadState::from_thread_json(
            &json!({"thread": {"threadId": "mcp:launch:other", "status": "completed", "latestRunId": run(T, 1)}}),
            T,
            Instant::now(),
        );
        assert!(!st.satisfies(&p()));
        assert!(st.diagnostic().unwrap().contains("different thread"));
    }

    #[test]
    fn workstream_needs_a_proof_and_a_marker_when_asked() {
        let ws = Predicate {
            until_delegated: true,
            ..Default::default()
        };
        let mut s = state("completed", &run(T, 1));
        assert!(!s.satisfies(&ws));
        s.delegated_settled = true;
        assert!(s.satisfies(&ws));
        assert_eq!(s.to_json(&ws)["delegatedSettled"], true);
        let marked = Predicate {
            final_marker: Some("FINAL".into()),
            ..ws.clone()
        };
        assert!(!s.satisfies(&marked));
        s.marker_seen = Some(false);
        assert!(!s.satisfies(&marked));
        assert!(s.diagnostic().unwrap().contains("final marker"));
        s.marker_seen = Some(true);
        assert!(s.satisfies(&marked));
        // A failed parent turn waits for its delegated work like a
        // completed one, but never needs the marker.
        let mut failed = state("failed", &run(T, 1));
        assert_eq!(failed.outcome(&marked), ThreadOutcome::Pending);
        failed.delegated_settled = true;
        assert_eq!(failed.outcome(&marked), ThreadOutcome::Failed);
        assert_eq!(state("failed", &run(T, 1)).outcome(&p()), ThreadOutcome::Failed);
    }

    #[test]
    fn a_proven_workstream_takes_the_result_runs_status() {
        let ws = Predicate {
            until_delegated: true,
            ..Default::default()
        };
        // The header says run 6 completed; T3 says the result run 4 failed.
        let mut s = state("completed", &run(T, 6));
        s.delegated_settled = true;
        s.result_run_id = Some(run(T, 4));
        for bad in ["failed", "interrupted", "cancelled"] {
            s.result_status = Some(bad.into());
            assert_eq!(s.outcome(&ws), ThreadOutcome::Failed, "{bad}");
        }
        s.result_status = Some("completed".into());
        assert_eq!(s.outcome(&ws), ThreadOutcome::Completed);
        // A failed latest run after a completed result run is no success.
        s.status = Some("failed".into());
        assert_eq!(s.outcome(&ws), ThreadOutcome::Failed);
        // Without the result run's status, only the same run's header counts.
        s.status = Some("completed".into());
        s.result_status = None;
        assert_eq!(s.outcome(&ws), ThreadOutcome::Pending);
        s.result_run_id = Some(run(T, 6));
        assert_eq!(s.outcome(&ws), ThreadOutcome::Completed);
    }

    #[test]
    fn a_rechecked_observation_reports_input_but_never_an_end() {
        let mut s = state("completed", &run(T, 2));
        s.recheck = Some("the thread moved on to run 3".into());
        assert_eq!(s.outcome(&p()), ThreadOutcome::Pending);
        assert!(s.diagnostic().unwrap().contains("moved on"));
        s.status = Some("failed".into());
        assert_eq!(s.outcome(&p()), ThreadOutcome::Pending);
        s.pending_requests = Some(1);
        assert_eq!(s.outcome(&p()), ThreadOutcome::NeedsInput);
        // A settled status with an active run is not settled either.
        let mut active = state("completed", &run(T, 2));
        active.active_run_id = Some(run(T, 3));
        assert_eq!(active.outcome(&p()), ThreadOutcome::Pending);
    }

    #[test]
    fn workstream_proof_needs_the_same_thread_and_a_new_enough_run() {
        let (p1, p2, p3, p6) = (run(T, 1), run(T, 2), run(T, 3), run(T, 6));
        let child = run("mcp:launch:child", 100);
        // Sol's repro: the result run is a child's; the read sees parent run 2.
        assert!(!workstream_covered(T, Some(&child), Some(&p2), Some(2)));
        // The research incident: run 2 ended while children ran; the read
        // already shows the synthesis run 6, so a proof for run 2 is stale.
        assert!(!workstream_covered(T, Some(&p2), Some(&p6), Some(6)));
        assert!(workstream_covered(T, Some(&p6), Some(&p6), Some(6)));
        assert!(workstream_covered(T, Some(&p3), Some(&p2), Some(3)));
        assert!(!workstream_covered(T, Some(&p1), Some(&p2), Some(2)));
        assert!(!workstream_covered(T, None, Some(&p1), Some(1)));
        // Ids without a parseable identity never prove anything.
        assert!(!workstream_covered(T, Some("opaque-id"), Some("opaque-id"), None));
        assert!(!workstream_covered(T, Some(&p2), Some("opaque-id"), None));
        // A missing latest run is unresolved unless the server said "no runs".
        assert!(!workstream_covered(T, Some(&p1), None, None));
        assert!(!workstream_covered(T, Some(&p1), None, Some(1)));
        assert!(workstream_covered(T, None, None, Some(0)));
    }

    #[test]
    fn pending_requests_are_read_from_activity_items_safely() {
        let question = json!([{
            "id": "q1", "header": "Decode site",
            "question": "Use token sk-live-ABCDEFGHIJKLMNOPQRSTUVWX1234 or not?",
            "options": [{"label": "Yes (Recommended)", "description": "long text"}, {"label": "No"}],
            "multiSelect": false
        }]);
        let items = vec![
            json!({"type": "user_input_request", "status": "pending", "text": question.to_string(),
                   "itemId": "turn-item:runtime-request:runtime-request%3Aabc"}),
            json!({"type": "user_input_request", "status": "completed", "text": "[]", "itemId": "old"}),
            json!({"type": "approval_request", "status": "in_progress", "text": "Run rm -rf build?", "itemId": "ap1"}),
            json!({"type": "command_execution", "status": "running", "text": "$ ls", "itemId": "c"}),
        ];
        let found = pending_from_activity(&items);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].kind, "question");
        assert_eq!(found[0].request_id.as_deref(), Some("runtime-request:abc"));
        assert_eq!(found[0].to_json()["userOnly"], false);
        let q = &found[0].questions[0];
        assert_eq!(q["header"], "Decode site");
        assert_eq!(q["options"], json!(["Yes (Recommended)", "No"]));
        assert!(!q.to_string().contains("sk-live-ABCDEF"), "{q}");
        assert!(!q.to_string().contains("long text"));
        assert_eq!(found[1].kind, "approval");
        assert_eq!(found[1].to_json()["userOnly"], true);
    }

    #[test]
    fn json_strips_link_brackets() {
        let j = state("completed", &run(T, 4)).to_json(&p());
        assert_eq!(j["link"], "t3-thread://v2/e/t");
        assert_eq!(j["runOrdinal"], 4);
        assert_eq!(j["settled"], true);
        assert_eq!(j["outcome"], "completed");
        assert!(j.get("diagnostic").is_none());
    }
}
