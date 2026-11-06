//! Cancellation for one upstream operation (an MCP cycle or a `t3 thread
//! show`). When the last subscriber of a watch leaves, its worker cancels the
//! operation in flight instead of leaving it to run out its long-poll: HTTP
//! agents are shut down, which interrupts their blocked reads, and local `t3`
//! process groups are killed.

use crate::util::lock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Cancel {
    cancelled: AtomicBool,
    inner: Mutex<Parts>,
}

#[derive(Default)]
struct Parts {
    agents: Vec<ureq::Agent>,
    /// Process groups of unreaped children. A pid is removed before its
    /// child is reaped, so a kill can never reach a reused pid.
    pids: Vec<libc::pid_t>,
}

impl Cancel {
    pub fn new() -> Arc<Self> {
        Arc::new(Cancel::default())
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        let mut parts = lock(&self.inner);
        self.cancelled.store(true, Ordering::SeqCst);
        for agent in parts.agents.drain(..) {
            agent.shutdown();
        }
        for pid in parts.pids.drain(..) {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    }

    /// Registers an agent whose requests `cancel` must interrupt. Returns
    /// false, after shutting the agent down, if already cancelled.
    pub fn add_agent(&self, agent: &ureq::Agent) -> bool {
        let mut parts = lock(&self.inner);
        if self.is_cancelled() {
            agent.clone().shutdown();
            return false;
        }
        parts.agents.push(agent.clone());
        true
    }

    /// Registers a child's process group. Returns false if already cancelled;
    /// the caller then kills and reaps the child itself.
    pub fn add_pid(&self, pid: libc::pid_t) -> bool {
        let mut parts = lock(&self.inner);
        if self.is_cancelled() {
            return false;
        }
        parts.pids.push(pid);
        true
    }

    /// Must be called before the child is reaped.
    pub fn remove_pid(&self, pid: libc::pid_t) {
        lock(&self.inner).pids.retain(|p| *p != pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn cancel_interrupts_a_blocked_request() {
        // A server that accepts and then never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let _hold = std::thread::spawn(move || {
            let conns: Vec<_> = listener.incoming().take(1).collect();
            std::thread::sleep(Duration::from_secs(30));
            drop(conns);
        });
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        let cancel = Cancel::new();
        assert!(cancel.add_agent(&agent));
        let (c, a) = (cancel.clone(), agent.clone());
        let started = Instant::now();
        let call = std::thread::spawn(move || a.get(&url).call().is_err());
        std::thread::sleep(Duration::from_millis(200));
        c.cancel();
        assert!(call.join().unwrap(), "the request must fail once cancelled");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(cancel.is_cancelled());
        assert!(!cancel.add_agent(&agent), "late registrations are refused");
        assert!(!cancel.add_pid(1));
    }
}
