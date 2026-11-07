//! Small helpers that would otherwise pull in extra crates: durations, URL and
//! base64url encoding, randomness, backoff, private files, file locks, peer
//! credentials and error sanitizing.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Locks a mutex, ignoring poisoning. A panicking worker thread must not take
/// every other watch in the daemon down with it.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Writes a diagnostic line to stderr, best effort. `eprintln!` panics when
/// stderr is a closed pipe; a diagnostic must never cost an event, a
/// notification or an exit code.
pub fn warn(msg: &str) {
    let mut line = String::with_capacity(msg.len() + 1);
    line.push_str(msg);
    line.push('\n');
    let _ = io::stderr().lock().write_all(line.as_bytes());
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

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    ring::hmac::sign(&key, msg).as_ref().to_vec()
}

pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
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
    pub scheme: String,
    pub host: String,
    /// Path without the query, always starting with `/`.
    pub path: String,
    pub query: String,
}

pub fn parse_url(s: &str) -> Option<Url> {
    let s = s.trim().trim_start_matches('<').trim_end_matches('>');
    if has_control_chars(s) || s.contains(' ') {
        return None;
    }
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
    if host.is_empty() {
        return None;
    }
    let default_port = if scheme == "https" { ":443" } else { ":80" };
    let authority = authority.strip_suffix(default_port).unwrap_or(&authority).to_string();
    Some(Url {
        origin: format!("{scheme}://{authority}"),
        scheme,
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

/// The only hosts plain `http://` is allowed for.
pub fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

/// Parses `s` and requires `https://`, or `http://` on a loopback host.
/// Anything else would put a bearer token or webhook payload on the wire in
/// clear text.
pub fn secure_url(s: &str, what: &str) -> Result<Url, String> {
    let u = parse_url(s).ok_or_else(|| format!("{what} is not a valid http(s) URL"))?;
    match u.scheme.as_str() {
        "https" => Ok(u),
        "http" if is_loopback_host(&u.host) => Ok(u),
        _ => Err(format!(
            "{what} must use https:// (plain http:// is only allowed for 127.0.0.1, [::1] and localhost)"
        )),
    }
}

pub fn has_control_chars(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

/// Keeps only characters that are safe to echo from an untrusted server: an
/// OAuth error code, for example. Never echo response bodies.
pub fn sanitize_code(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .take(64)
        .collect()
}

/// Printable ASCII, collapsed whitespace, bounded length.
pub fn sanitize_text(s: &str, max: usize) -> String {
    let cleaned: String = s.chars().map(|c| if c.is_ascii_graphic() { c } else { ' ' }).collect();
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

/// Describes a transport error without its Display text, which can carry a
/// full URL (and so a webhook secret embedded in a path or query).
pub fn describe_ureq_error(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "timed out".into(),
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::ConnectionFailed => "connection failed".into(),
        ureq::Error::Io(io) => format!("network error ({:?})", io.kind()),
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => "TLS error".into(),
        ureq::Error::TooManyRedirects | ureq::Error::RedirectFailed => "redirect refused".into(),
        ureq::Error::StatusCode(c) => format!("HTTP {c}"),
        _ => "request failed".into(),
    }
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

pub fn geteuid() -> u32 {
    unsafe { libc::geteuid() }
}

/// Requires `meta` to belong to us and be closed to group and others.
pub fn check_private(meta: &fs::Metadata, what: &Path) -> Result<(), String> {
    if meta.uid() != geteuid() {
        return Err(format!("{} is owned by uid {}, not by you", what.display(), meta.uid()));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{} is accessible to other users (mode {:o}); use a private 0700 directory",
            what.display(),
            meta.mode() & 0o777
        ));
    }
    Ok(())
}

/// Creates `dir` (and parents) if needed and makes it 0700. Refuses a symlink
/// or a directory owned by someone else. Only for directories t3-watch owns.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!("{} is not a directory", dir.display())));
    }
    if meta.uid() != geteuid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is owned by uid {}, not by you", dir.display(), meta.uid()),
        ));
    }
    if meta.mode() & 0o777 != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// For a directory the user chose: creates it 0700 if missing, otherwise
/// requires it to be a private directory we own. Never changes permissions.
pub fn verify_private_dir(dir: &Path) -> Result<(), String> {
    if fs::symlink_metadata(dir).is_err() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let meta = fs::symlink_metadata(dir).map_err(|e| format!("cannot inspect {}: {e}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory (symlinks are refused)", dir.display()));
    }
    check_private(&meta, dir)
}

/// Opens (creating if needed) a 0600 lock file without following symlinks,
/// and checks that it is a regular file we own.
pub fn open_lock_file(path: &Path) -> io::Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let meta = f.metadata()?;
    if !meta.is_file() || meta.uid() != geteuid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a regular file owned by you", path.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(f)
}

/// Takes an exclusive `flock` without blocking. `Ok(false)` means someone
/// else holds it. (`File::try_lock` would need Rust 1.89.)
pub fn try_flock(f: &File) -> io::Result<bool> {
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Ok(false),
        Some(libc::EINTR) => try_flock(f),
        _ => Err(err),
    }
}

/// Waits up to `timeout` for an exclusive `flock`.
pub fn flock_within(f: &File, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if try_flock(f)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Writes a file readable only by the owner (0600), atomically: a fresh
/// `O_EXCL` temp file next to it, fsync, rename, then fsync the directory, so
/// a crash leaves either the old or the new file and never a partial one.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure_private_dir(parent)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), hex(&random_bytes(6))));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The uid of the process on the other end of a Unix socket.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub fn peer_uid(s: &std::os::unix::net::UnixStream) -> io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    if unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn peer_uid(s: &std::os::unix::net::UnixStream) -> io::Result<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// Fails unless the peer runs as our effective uid.
pub fn check_peer(s: &std::os::unix::net::UnixStream) -> io::Result<()> {
    let uid = peer_uid(s)?;
    if uid != geteuid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("socket peer runs as uid {uid}, not as you"),
        ));
    }
    Ok(())
}

/// `2026-11-07T02:47:06.298Z` for a Unix time in milliseconds (UTC).
pub fn rfc3339_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        ms % 1000
    )
}

/// Text that may leave this machine (a question shown to a bot): control
/// characters become spaces, runs of whitespace collapse, the length is
/// bounded in characters, and anything that looks like a credential is
/// replaced. Unlike `sanitize_text` it keeps non-ASCII letters.
pub fn safe_text(s: &str, max: usize) -> String {
    let cleaned: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let joined = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = redact_secrets(&joined);
    if out.chars().count() > max {
        out = out.chars().take(max.saturating_sub(1)).collect::<String>() + "…";
    }
    out
}

const REDACTED: &str = "[redacted]";
const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "rk_",
    "pk_live",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxa-",
    "xoxb-",
    "xoxp-",
    "xoxr-",
    "xoxs-",
    "AKIA",
    "ASIA",
    "AIza",
    "eyJ",
    "npm_",
    "pypi-",
];
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "auth",
    "authorization",
    "credential",
    "private_key",
    "access_key",
    "client_secret",
    "session",
    "cookie",
];

fn is_secret_key(word: &str) -> bool {
    let w = word
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .to_ascii_lowercase();
    !w.is_empty()
        && SECRET_KEYS
            .iter()
            .any(|k| w == *k || w.ends_with(&format!("_{k}")) || w.ends_with(&format!("-{k}")))
}

fn looks_like_secret(piece: &str) -> bool {
    let p = piece.trim_matches(|c: char| "\"'`()[]{}<>,;".contains(c));
    if p.len() >= 12 && SECRET_PREFIXES.iter().any(|pre| p.starts_with(pre)) {
        return true;
    }
    // A long run of token characters with letters and digits mixed: API
    // keys, hex secrets, base64 blobs.
    p.len() >= 32
        && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-+=".contains(&b))
        && p.bytes().any(|b| b.is_ascii_digit())
        && p.bytes().any(|b| b.is_ascii_alphabetic())
}

/// Replaces likely credentials in free text: known key prefixes (`sk-`,
/// `ghp_`, JWTs...), long mixed alphanumeric runs, the word after `Bearer`
/// or `Basic`, values of `password=`/`token:`-style pairs, and URL userinfo.
/// Errs on the side of hiding too much.
pub fn redact_secrets(s: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut redact_next = false;
    for word in s.split(' ') {
        if word.is_empty() {
            out.push(String::new());
            continue;
        }
        // `Authorization: Bearer x`: keep the scheme, hide what follows.
        let lower = word.to_ascii_lowercase();
        if lower == "bearer" || lower == "basic" {
            redact_next = true;
            out.push(word.into());
            continue;
        }
        if redact_next {
            redact_next = false;
            out.push(REDACTED.into());
            continue;
        }
        // `key=value`, `key:value` and `?a=b&token=c`.
        let mut rebuilt = String::new();
        let mut rest = word;
        while !rest.is_empty() {
            let cut = rest.find(['&', '?', ',', ';']).map_or(rest.len(), |i| i + 1);
            let (part, tail) = rest.split_at(cut);
            rest = tail;
            let (body, delim) = match part.char_indices().last() {
                Some((i, c)) if "&?,;".contains(c) => (&part[..i], &part[i..]),
                _ => (part, ""),
            };
            let redacted = if let Some((k, v)) = body.split_once(['=', ':']).filter(|(k, _)| is_secret_key(k)) {
                if v.trim_matches(|c: char| "\"' ".contains(c)).is_empty() {
                    // `"password": "x"` with the value in the next word.
                    redact_next = true;
                    body.to_string()
                } else {
                    format!("{k}{}{REDACTED}", &body[k.len()..k.len() + 1])
                }
            } else if let Some(at) = body.find('@').filter(|_| body.contains("://")) {
                // https://user:pass@host -> https://[redacted]@host
                let scheme_end = body.find("://").unwrap() + 3;
                if at > scheme_end {
                    format!("{}{REDACTED}{}", &body[..scheme_end], &body[at..])
                } else {
                    body.to_string()
                }
            } else if looks_like_secret(body) {
                REDACTED.to_string()
            } else {
                body.to_string()
            };
            rebuilt.push_str(&redacted);
            rebuilt.push_str(delim);
        }
        if is_secret_key(word) && word.ends_with([':', '=']) {
            redact_next = true;
        }
        out.push(rebuilt);
    }
    out.join(" ")
}

#[cfg(test)]
thread_local! {
    /// `(skip, fail)`: let the next `skip` `spawn_thread` calls on this
    /// thread succeed, then fail the `fail` after them, so tests can exercise
    /// each error path.
    pub static FAIL_SPAWNS: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, 0)) };
}

/// `thread::Builder::spawn` with a name and stack size. Callers must handle
/// the error; nothing here falls back silently.
pub fn spawn_thread<F, T>(name: &str, stack: usize, f: F) -> io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    #[cfg(test)]
    if FAIL_SPAWNS.with(|c| match c.get() {
        (0, 0) => false,
        (0, fail) => {
            c.set((0, fail - 1));
            true
        }
        (skip, fail) => {
            c.set((skip - 1, fail));
            false
        }
    }) {
        return Err(io::Error::other("injected spawn failure"));
    }
    thread::Builder::new().name(name.into()).stack_size(stack).spawn(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("t3wu-{tag}-{}-{}", std::process::id(), hex(&random_bytes(4))));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_ms(1_793_933_226_298), "2026-11-06T02:47:06.298Z");
        assert_eq!(rfc3339_ms(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn secrets_are_redacted_from_free_text() {
        let cases = [
            ("use sk-live-ABCDEFGHIJKLMNOPQRST now", "sk-live"),
            ("Authorization: Bearer abc.def.ghi please", "abc.def"),
            ("set password=hunter2 then", "hunter2"),
            ("\"api_key\": \"zzz-top-secret\"", "zzz-top"),
            ("token:qwerty123", "qwerty123"),
            ("https://u:p4ss@example.com/x", "p4ss"),
            ("https://h.example/cb?code=1&token=abcd1234", "abcd1234"),
            (
                "key 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
                "9f86d081",
            ),
            ("jwt eyJhbGciOiJIUzI1NiJ9.e30.x", "eyJhbGci"),
            ("ghp_abcdefghijklmnopqrstuvwxyz0123", "ghp_abc"),
        ];
        for (input, secret) in cases {
            let out = redact_secrets(input);
            assert!(!out.contains(secret), "{input:?} -> {out:?}");
            assert!(out.contains("[redacted]"), "{input:?} -> {out:?}");
        }
        let plain = "Should I merge PR #3055 into main (Recommended)? Path /Users/krish/.t3/worktrees/x";
        assert_eq!(redact_secrets(plain), plain);
        assert_eq!(safe_text("a\nb\u{0}  c — é", 100), "a b c — é");
        assert_eq!(safe_text("abcdef", 4), "abc…");
    }

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
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
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
        assert!(parse_url("https://a.example/x\r\nInjected: 1").is_none());
        assert!(parse_url("https://user@a.example/").is_none());
        assert_eq!(
            normalize_origin("prod-x.t3coderelay.com/mcp").unwrap(),
            "https://prod-x.t3coderelay.com"
        );
    }

    #[test]
    fn plain_http_only_on_loopback() {
        assert!(secure_url("https://relay.example/mcp", "x").is_ok());
        for ok in ["http://127.0.0.1:3773", "http://localhost:9/x", "http://[::1]:8080/"] {
            assert!(secure_url(ok, "x").is_ok(), "{ok}");
        }
        for bad in [
            "http://relay.example",
            "http://127.0.0.2:80",
            "http://10.0.0.1",
            "ws://localhost",
        ] {
            assert!(secure_url(bad, "x").is_err(), "{bad}");
        }
    }

    #[test]
    fn sanitizers_drop_unsafe_text() {
        assert_eq!(sanitize_code("invalid_grant\r\n<script>"), "invalid_grantscript");
        assert_eq!(sanitize_text("a\r\n\tb\u{7f}c  d", 100), "a b c d");
        assert_eq!(sanitize_text("abcdef", 3), "abc");
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

    #[test]
    fn flock_excludes_a_second_handle() {
        let d = tmpdir("flock");
        let p = d.join("x.lock");
        let a = open_lock_file(&p).unwrap();
        let b = open_lock_file(&p).unwrap();
        assert!(try_flock(&a).unwrap());
        assert!(!try_flock(&b).unwrap());
        assert!(!flock_within(&b, Duration::from_millis(30)).unwrap());
        drop(a);
        assert!(try_flock(&b).unwrap());
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn lock_file_refuses_symlinks() {
        let d = tmpdir("symlink");
        let target = d.join("target");
        fs::write(&target, "keep").unwrap();
        let link = d.join("x.lock");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_lock_file(&link).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn write_private_is_atomic_and_private() {
        let d = tmpdir("write");
        let p = d.join("machines.json");
        write_private(&p, b"one").unwrap();
        write_private(&p, b"two").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        let leftovers: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != "machines.json")
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn verify_private_dir_rejects_shared_dirs() {
        let d = tmpdir("verify");
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_private_dir(&d).unwrap_err().contains("other users"));
        assert_eq!(fs::metadata(&d).unwrap().mode() & 0o777, 0o755, "must not chmod");
        let fresh = d.join("new");
        verify_private_dir(&fresh).unwrap();
        assert_eq!(fs::metadata(&fresh).unwrap().mode() & 0o777, 0o700);
        let link = d.join("link");
        std::os::unix::fs::symlink(&fresh, &link).unwrap();
        assert!(verify_private_dir(&link).is_err());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn peer_uid_is_ours_on_a_socketpair() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a).unwrap(), geteuid());
        check_peer(&b).unwrap();
    }

    #[test]
    fn injected_spawn_failures_are_reported() {
        FAIL_SPAWNS.with(|c| c.set((1, 1)));
        spawn_thread("x", 64 * 1024, || ()).unwrap().join().unwrap();
        assert!(spawn_thread("x", 64 * 1024, || ()).is_err());
        spawn_thread("x", 64 * 1024, || ()).unwrap().join().unwrap();
    }
}
