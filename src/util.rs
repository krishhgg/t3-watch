//! Small helpers that would otherwise pull in extra crates: durations, URL and
//! base64url encoding, randomness, backoff, and private file writes.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Locks a mutex, ignoring poisoning. A panicking worker thread must not take
/// every other watch in the daemon down with it.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parses `90`, `90s`, `1500ms`, `5m`, `4h` or a sum like `1h30m`.
/// A bare number means seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    if let Ok(secs) = s.parse::<f64>() {
        if secs < 0.0 || !secs.is_finite() {
            return Err(format!("invalid duration: {s}"));
        }
        return Ok(Duration::from_millis((secs * 1000.0) as u64));
    }
    let mut total = Duration::ZERO;
    let mut rest = s;
    while !rest.is_empty() {
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(|| format!("missing unit in duration: {s}"))?;
        if num_len == 0 {
            return Err(format!("invalid duration: {s}"));
        }
        let n: f64 = rest[..num_len].parse().map_err(|_| format!("invalid duration: {s}"))?;
        rest = &rest[num_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let ms = match &rest[..unit_len] {
            "ms" => n,
            "s" => n * 1000.0,
            "m" => n * 60_000.0,
            "h" => n * 3_600_000.0,
            "d" => n * 86_400_000.0,
            u => return Err(format!("unknown duration unit {u:?} in {s}")),
        };
        total += Duration::from_millis(ms as u64);
        rest = &rest[unit_len..];
    }
    Ok(total)
}

/// Fills a buffer from the OS CSPRNG.
pub fn random_bytes(n: usize) -> Vec<u8> {
    use ring::rand::SecureRandom;
    let mut buf = vec![0u8; n];
    ring::rand::SystemRandom::new()
        .fill(&mut buf)
        .expect("OS random source unavailable");
    buf
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref().to_vec()
}

/// Unpadded base64url (RFC 4648 section 5), as PKCE requires.
pub fn b64url(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Decodes `%XX` escapes. With `plus_as_space`, `+` becomes a space, as in
/// `application/x-www-form-urlencoded` query strings.
pub fn pct_decode(s: &str, plus_as_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' if plus_as_space => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", pct_encode(k), pct_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (pct_decode(k, true), pct_decode(v, true)),
            None => (pct_decode(p, true), String::new()),
        })
        .collect()
}

pub fn query_get<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// The parts of an absolute URL that this tool needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Url {
    /// `scheme://host[:port]`, lowercased, without a trailing slash.
    pub origin: String,
    pub host: String,
    /// Path without the query, always starting with `/`.
    pub path: String,
    pub query: String,
}

pub fn parse_url(s: &str) -> Option<Url> {
    let s = s.trim().trim_start_matches('<').trim_end_matches('>');
    let (scheme, rest) = s.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = rest.split('#').next().unwrap_or("");
    let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = rest[..auth_end].to_ascii_lowercase();
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let tail = &rest[auth_end..];
    let (path, query) = match tail.split_once('?') {
        Some((p, q)) => (p, q),
        None => (tail, ""),
    };
    let host = if authority.starts_with('[') {
        authority[..authority.find(']').map_or(authority.len(), |i| i + 1)].to_string()
    } else {
        match authority.rsplit_once(':') {
            Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h.to_string(),
            _ => authority.clone(),
        }
    };
    let default_port = if scheme == "https" { ":443" } else { ":80" };
    let authority = authority.strip_suffix(default_port).unwrap_or(&authority).to_string();
    Some(Url {
        origin: format!("{scheme}://{authority}"),
        host,
        path: if path.is_empty() { "/".into() } else { path.to_string() },
        query: query.to_string(),
    })
}

/// Normalizes a user-supplied origin like `prod-x.t3coderelay.com/` or
/// `https://host/mcp` to `https://host`.
pub fn normalize_origin(s: &str) -> Option<String> {
    let s = s.trim();
    let with_scheme = if s.contains("://") {
        s.to_string()
    } else {
        format!("https://{s}")
    };
    parse_url(&with_scheme).map(|u| u.origin)
}

/// xorshift64* seeded from the OS. Only for backoff jitter, never for secrets.
pub struct Jitter(u64);

impl Jitter {
    pub fn new() -> Self {
        let b = random_bytes(8);
        Jitter(u64::from_le_bytes(b.try_into().unwrap()) | 1)
    }

    pub fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Exponential backoff with "equal jitter": half the delay is fixed and
    /// half is random, so retries from many watchers spread out without ever
    /// retrying immediately.
    pub fn backoff(&mut self, attempt: u32, base: Duration, cap: Duration) -> Duration {
        let exp = base.saturating_mul(1u32 << attempt.min(16)).min(cap);
        let half = exp / 2;
        half + half.mul_f64(self.next_f64())
    }
}

/// Creates `dir` (and parents) and restricts it to the owner (0700).
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Writes a file readable only by the owner (0600), atomically via rename so
/// a crash never leaves a half-written token file behind.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1500ms").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("0.5s").unwrap(), Duration::from_millis(500));
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("m").is_err());
    }

    #[test]
    fn base64url_matches_rfc_vectors() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(b"foob"), "Zm9vYg");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn pkce_s256_matches_rfc7636_appendix_b() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            b64url(&sha256(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn percent_round_trip() {
        let id = "mcp:launch:abc/def ghi";
        assert_eq!(pct_encode(id), "mcp%3Alaunch%3Aabc%2Fdef%20ghi");
        assert_eq!(pct_decode(&pct_encode(id), false), id);
        assert_eq!(pct_decode("a+b%2", true), "a b%2");
        assert_eq!(pct_decode("%zz%41", false), "%zzA");
    }

    #[test]
    fn urls() {
        let u = parse_url("https://Prod-1.T3CodeRelay.com:443/env/mcp%3Alaunch%3Ax?a=1#frag").unwrap();
        assert_eq!(u.origin, "https://prod-1.t3coderelay.com");
        assert_eq!(u.host, "prod-1.t3coderelay.com");
        assert_eq!(u.path, "/env/mcp%3Alaunch%3Ax");
        assert_eq!(u.query, "a=1");
        let u = parse_url("http://127.0.0.1:3773").unwrap();
        assert_eq!(u.origin, "http://127.0.0.1:3773");
        assert_eq!(u.host, "127.0.0.1");
        assert_eq!(u.path, "/");
        assert!(parse_url("ftp://x").is_none());
        assert!(parse_url("t3-thread://v2/a/b").is_none());
        assert_eq!(
            normalize_origin("prod-x.t3coderelay.com/mcp").unwrap(),
            "https://prod-x.t3coderelay.com"
        );
    }

    #[test]
    fn backoff_stays_in_bounds() {
        let mut j = Jitter::new();
        for attempt in 0..20 {
            let d = j.backoff(attempt, Duration::from_secs(1), Duration::from_secs(30));
            let full = Duration::from_secs(1)
                .saturating_mul(1 << attempt.min(16))
                .min(Duration::from_secs(30));
            assert!(d >= full / 2 && d <= full, "attempt {attempt}: {d:?}");
        }
    }
}
