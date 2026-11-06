//! Paths, the machines file, and local `t3` discovery.
//!
//! Layout under the state directory (0700):
//!   machines.json   saved machines and their read-only tokens (0600)
//!   pending/        in-progress headless OAuth logins (0600 files)
//!   daemon.sock     the multiplexer socket (0600)
//!   daemon.sock.lock held by the running daemon
//!   daemon.log      daemon stderr

use crate::util;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

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

/// The daemon socket. `$T3_WATCH_HOME` wins so tests and multiple users stay
/// isolated; otherwise `$XDG_RUNTIME_DIR/t3-watch` when set (Linux), else the
/// state directory.
pub fn socket_path() -> PathBuf {
    if let Some(p) = env_path("T3_WATCH_SOCKET") {
        return p;
    }
    if env_path("T3_WATCH_HOME").is_none() {
        if let Some(rt) = env_path("XDG_RUNTIME_DIR") {
            return rt.join("t3-watch").join("daemon.sock");
        }
    }
    state_dir().join("daemon.sock")
}

/// T3 Code's data directory: `$T3CODE_HOME` or `~/.t3`.
pub fn t3_base_dir() -> PathBuf {
    env_path("T3CODE_HOME").unwrap_or_else(|| home_dir().join(".t3"))
}

/// A saved remote T3 MCP server.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    pub origin: String,
    pub access_token: String,
    pub client_id: Option<String>,
    pub scope: Option<String>,
    pub issuer: Option<String>,
    pub expires_at_ms: Option<u64>,
    pub created_at_ms: Option<u64>,
}

impl Machine {
    fn from_json(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Machine {
            origin: s("origin")?,
            access_token: s("accessToken").unwrap_or_default(),
            client_id: s("clientId"),
            scope: s("scope"),
            issuer: s("issuer"),
            expires_at_ms: v.get("expiresAtMs").and_then(Value::as_u64),
            created_at_ms: v.get("createdAtMs").and_then(Value::as_u64),
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "origin": self.origin,
            "accessToken": self.access_token,
            "clientId": self.client_id,
            "scope": self.scope,
            "issuer": self.issuer,
            "expiresAtMs": self.expires_at_ms,
            "createdAtMs": self.created_at_ms,
        })
    }

    /// Everything except the token, for `auth list`.
    pub fn public_json(&self, name: &str, is_default: bool) -> Value {
        json!({
            "name": name,
            "origin": self.origin,
            "scope": self.scope,
            "default": is_default,
            "expiresAtMs": self.expires_at_ms,
            "hasToken": !self.access_token.is_empty(),
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

    pub fn save(&self) -> Result<(), String> {
        let mut ms = Map::new();
        for (name, m) in &self.machines {
            ms.insert(name.clone(), m.to_json());
        }
        let v = json!({ "default": self.default, "machines": ms });
        let text = serde_json::to_string_pretty(&v).unwrap() + "\n";
        let path = machines_path();
        util::write_private(&path, text.as_bytes()).map_err(|e| format!("cannot write {}: {e}", path.display()))
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

/// Finds T3's local CLI: the newest `<base>/runtime/versions/*/t3`, then
/// `<base>/bin/t3`, then `t3` on `PATH`.
pub fn find_t3_bin(base_dir: &Path) -> Option<PathBuf> {
    if let Some(p) = env_path("T3_WATCH_T3_BIN") {
        return Some(p);
    }
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
    if shim.is_file() {
        return Some(shim);
    }
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
}
