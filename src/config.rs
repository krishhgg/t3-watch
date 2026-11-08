//! Paths, the machines file, and local `t3` discovery.
//!
//! Layout under the state directory (0700):
//!   machines.json     saved machines and their read-only tokens (0600)
//!   config.lock       held while machines.json is read, changed and written
//!   pending/          in-progress headless OAuth logins (0600 files)
//!   daemon-e<N>.sock  the multiplexer socket for engine revision N (0600)
//!   daemon-e<N>.sock.lock held by the running daemon
//!   outbox-v4/        durable notifications not yet delivered (0700, entries,
//!                     per-entry lease files and receipts 0600)
//!   outbox/           entries of builds before engine 4, migrated into outbox-v4/
//!   outbox-v2/, outbox-v3/ entries of engine-4 and engine-5 builds, delivered
//!                     where they are, the way those builds' senders would;
//!                     undelivered outbox-v2/ ones kept, held back from older
//!                     senders, until their client is done
//!   trace.jsonl       delivery trace: waits, proofs, events, HTTP attempts
//!   hooks.log         output of --on-settle programs (0600)
//!   daemon.log        daemon stderr

use crate::util;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `$T3_WATCH_HOME`, else `$XDG_CONFIG_HOME/t3-watch`, else `~/.config/t3-watch`.
pub fn state_dir() -> PathBuf {
    env_path("T3_WATCH_HOME")
        .or_else(|| env_path("XDG_CONFIG_HOME").map(|p| p.join("t3-watch")))
        .unwrap_or_else(|| home_dir().join(".config").join("t3-watch"))
}

/// True when the socket location was chosen with `T3_WATCH_SOCKET`, so its
/// directory belongs to the user rather than to t3-watch.
pub fn custom_socket() -> bool {
    env_path("T3_WATCH_SOCKET").is_some()
}

/// The daemon socket. `$T3_WATCH_SOCKET` names it outright; otherwise it
/// is this engine's default (see `default_socket_path`).
pub fn socket_path() -> PathBuf {
    env_path("T3_WATCH_SOCKET").unwrap_or_else(|| default_socket_path(crate::version::ENGINE))
}

/// Where a daemon of engine revision `engine` listens by default, as every
/// build since engine 4 resolves it: `$T3_WATCH_HOME` wins so tests and
/// multiple users stay isolated; otherwise `$XDG_RUNTIME_DIR/t3-watch` when
/// set (Linux), else the state directory. The file name carries the engine,
/// so a daemon left running by a build with other result semantics is never
/// even reached by default (it would report results this build reads
/// differently); old and new daemons run side by side until the old one
/// idles out. The handshake checks the revision as well, which is what
/// protects a custom socket.
pub fn default_socket_path(engine: u32) -> PathBuf {
    let name = format!("daemon-e{engine}.sock");
    if env_path("T3_WATCH_HOME").is_none() {
        if let Some(rt) = env_path("XDG_RUNTIME_DIR") {
            return rt.join("t3-watch").join(name);
        }
    }
    state_dir().join(name)
}

/// Durable notifications waiting for delivery (`--durable`). Versioned with
/// the engine: a sender from an older build (which shares the state
/// directory but not this format's receipts and pending rules) never sees
/// these entries.
pub fn outbox_dir() -> PathBuf {
    state_dir().join("outbox-v4")
}

/// Where builds before engine 4 kept their entries (no leases).
pub fn legacy_outbox_dir() -> PathBuf {
    state_dir().join("outbox")
}

/// Where engine-4 builds keep theirs (per-entry leases).
pub fn legacy_v2_outbox_dir() -> PathBuf {
    state_dir().join("outbox-v2")
}

/// Where engine-5 builds keep theirs (per-entry leases and receipts).
pub fn legacy_v3_outbox_dir() -> PathBuf {
    state_dir().join("outbox-v3")
}

/// Append-only delivery trace (`trace.jsonl`, 0600).
pub fn trace_path() -> PathBuf {
    state_dir().join("trace.jsonl")
}

/// T3 Code's data directory: `$T3CODE_HOME` or `~/.t3`. Client side only:
/// the daemon always gets an absolute path from the client.
pub fn t3_base_dir() -> PathBuf {
    env_path("T3CODE_HOME").unwrap_or_else(|| home_dir().join(".t3"))
}

/// `$T3_WATCH_T3_BIN`, read by the client only.
pub fn t3_bin_override() -> Option<PathBuf> {
    env_path("T3_WATCH_T3_BIN")
}

/// Makes `p` absolute against the current directory and resolves symlinks
/// when it exists, so the daemon (which runs from `/`) sees the same file.
pub fn absolute(p: &Path) -> Result<PathBuf, String> {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|e| format!("cannot resolve {}: {e}", p.display()))?
            .join(p)
    };
    Ok(fs::canonicalize(&joined).unwrap_or(joined))
}

/// A saved remote T3 MCP server.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    pub origin: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub scope: Option<String>,
    pub issuer: Option<String>,
    pub token_endpoint: Option<String>,
    pub resource: Option<String>,
    pub expires_at_ms: Option<u64>,
    pub created_at_ms: Option<u64>,
    /// Random id minted at pairing time. Token refreshes keep it; a new
    /// `auth login` replaces it, so the daemon never mixes the two.
    pub credential_id: Option<String>,
}

impl Machine {
    fn from_json(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Machine {
            origin: s("origin")?,
            access_token: s("accessToken").unwrap_or_default(),
            refresh_token: s("refreshToken"),
            client_id: s("clientId"),
            scope: s("scope"),
            issuer: s("issuer"),
            token_endpoint: s("tokenEndpoint"),
            resource: s("resource"),
            expires_at_ms: v.get("expiresAtMs").and_then(Value::as_u64),
            created_at_ms: v.get("createdAtMs").and_then(Value::as_u64),
            credential_id: s("credentialId"),
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "origin": self.origin,
            "accessToken": self.access_token,
            "refreshToken": self.refresh_token,
            "clientId": self.client_id,
            "scope": self.scope,
            "issuer": self.issuer,
            "tokenEndpoint": self.token_endpoint,
            "resource": self.resource,
            "expiresAtMs": self.expires_at_ms,
            "createdAtMs": self.created_at_ms,
            "credentialId": self.credential_id,
        })
    }

    /// Identifies this pairing without revealing the token. Stable across
    /// refreshes when the machine has a `credentialId`.
    pub fn fingerprint(&self) -> String {
        let basis = self.credential_id.as_deref().unwrap_or(&self.access_token);
        util::hex(&util::sha256(format!("{}\n{basis}", self.origin).as_bytes()))[..16].to_string()
    }

    /// Everything except the tokens, for `auth list`.
    pub fn public_json(&self, name: &str, is_default: bool) -> Value {
        json!({
            "name": name,
            "origin": self.origin,
            "scope": self.scope,
            "default": is_default,
            "expiresAtMs": self.expires_at_ms,
            "hasToken": !self.access_token.is_empty(),
            "canRefresh": self.refresh_token.is_some(),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub default: Option<String>,
    pub machines: BTreeMap<String, Machine>,
}

pub fn machines_path() -> PathBuf {
    state_dir().join("machines.json")
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let path = machines_path();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
        let mut cfg = Config {
            default: v.get("default").and_then(Value::as_str).map(str::to_string),
            machines: BTreeMap::new(),
        };
        if let Some(Value::Object(ms)) = v.get("machines") {
            for (name, m) in ms {
                if let Some(m) = Machine::from_json(m) {
                    cfg.machines.insert(name.clone(), m);
                }
            }
        }
        Ok(cfg)
    }

    fn save(&self) -> Result<(), String> {
        let mut ms = Map::new();
        for (name, m) in &self.machines {
            ms.insert(name.clone(), m.to_json());
        }
        let v = json!({ "default": self.default, "machines": ms });
        let text = serde_json::to_string_pretty(&v).unwrap() + "\n";
        let path = machines_path();
        util::write_private(&path, text.as_bytes()).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Runs one read-modify-write of machines.json under `config.lock`, so
    /// concurrent logins, logouts and token refreshes never overwrite each
    /// other. The config is reloaded inside the lock. If `f` fails, nothing
    /// is written.
    pub fn transaction<T>(f: impl FnOnce(&mut Config) -> Result<T, String>) -> Result<T, String> {
        Self::update(|cfg| f(cfg).map(|t| (t, true)))
    }

    /// Like `transaction`, but `f` also says whether it changed anything;
    /// an unchanged config is not rewritten. Every error is about locking,
    /// reading or writing the file unless `f` itself returns one.
    pub fn update<T>(f: impl FnOnce(&mut Config) -> Result<(T, bool), String>) -> Result<T, String> {
        let dir = state_dir();
        util::ensure_private_dir(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let lock_path = dir.join("config.lock");
        let lock = util::open_lock_file(&lock_path).map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
        match util::flock_within(&lock, Duration::from_secs(60)) {
            Ok(true) => {}
            Ok(false) => return Err(format!("timed out waiting for {}", lock_path.display())),
            Err(e) => return Err(format!("cannot lock {}: {e}", lock_path.display())),
        }
        let mut cfg = Config::load()?;
        let (out, changed) = f(&mut cfg)?;
        if changed {
            cfg.save()?;
        }
        Ok(out)
    }

    /// Finds the saved machine whose origin matches.
    pub fn by_origin(&self, origin: &str) -> Option<(&String, &Machine)> {
        self.machines
            .iter()
            .find(|(_, m)| m.origin.eq_ignore_ascii_case(origin))
    }
}

/// Compares version strings like `0.1.7-nightly.20261102.4066` by their
/// numeric runs, so `0.1.10` sorts after `0.1.9`.
fn version_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap_or(u64::MAX))
        .collect()
}

/// Finds T3's local CLI inside a data directory: the newest
/// `<base>/runtime/versions/*/t3`, then `<base>/bin/t3`. Reads no environment
/// variables, so the daemon gets the same answer as the client.
pub fn find_t3_in_base(base_dir: &Path) -> Option<PathBuf> {
    let mut versions: Vec<(Vec<u64>, PathBuf)> = fs::read_dir(base_dir.join("runtime").join("versions"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let bin = e.path().join("t3");
            bin.is_file()
                .then(|| (version_key(&e.file_name().to_string_lossy()), bin))
        })
        .collect();
    versions.sort();
    if let Some((_, bin)) = versions.pop() {
        return Some(bin);
    }
    let shim = base_dir.join("bin").join("t3");
    shim.is_file().then_some(shim)
}

/// `t3` on `PATH`, for the client when the data directory has none.
pub fn find_t3_on_path() -> Option<PathBuf> {
    env::var_os("PATH").and_then(|paths| env::split_paths(&paths).map(|d| d.join("t3")).find(|p| p.is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        let mut v = vec!["0.1.9", "0.1.10-nightly.20261102.4066", "0.1.10-nightly.20261020.1"];
        v.sort_by_key(|s| version_key(s));
        assert_eq!(
            v,
            ["0.1.9", "0.1.10-nightly.20261020.1", "0.1.10-nightly.20261102.4066"]
        );
    }

    #[test]
    fn fingerprint_tracks_the_pairing_not_the_name() {
        let m = Machine {
            origin: "https://r.example".into(),
            access_token: "a".into(),
            credential_id: Some("cred-1".into()),
            ..Default::default()
        };
        let mut refreshed = m.clone();
        refreshed.access_token = "b".into();
        assert_eq!(m.fingerprint(), refreshed.fingerprint(), "a refresh keeps the pairing");
        let mut repaired = m.clone();
        repaired.credential_id = Some("cred-2".into());
        assert_ne!(m.fingerprint(), repaired.fingerprint(), "a new login is a new pairing");
        let mut moved = m.clone();
        moved.origin = "https://other.example".into();
        assert_ne!(m.fingerprint(), moved.fingerprint());
        let legacy = Machine {
            credential_id: None,
            ..m.clone()
        };
        assert_ne!(
            legacy.fingerprint(),
            Machine {
                access_token: "z".into(),
                ..legacy.clone()
            }
            .fingerprint()
        );
        assert_eq!(m.fingerprint().len(), 16);
        assert!(!m.fingerprint().contains("cred-1"));
    }

    #[test]
    fn absolute_resolves_relative_paths() {
        let p = absolute(Path::new(".")).unwrap();
        assert!(p.is_absolute());
        assert_eq!(p, fs::canonicalize(env::current_dir().unwrap()).unwrap());
        let missing = absolute(Path::new("no/such/dir")).unwrap();
        assert!(missing.is_absolute() && missing.ends_with("no/such/dir"));
    }
}
