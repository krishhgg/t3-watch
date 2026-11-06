//! Read-only OAuth 2.0 pairing with a T3 MCP server (authorization code +
//! PKCE S256, public client from dynamic registration).
//!
//! t3-watch only ever requests `orchestration:read`. Tokens go to
//! machines.json (0600) and are never printed.

use crate::config::{self, Config, Machine};
use crate::util;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

pub const SCOPE: &str = "orchestration:read";
const PENDING_TTL_MS: u64 = 60 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct Discovery {
    pub origin: String,
    pub resource: String,
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(concat!("t3-watch/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn read_json(resp: ureq::http::Response<ureq::Body>, what: &str) -> Result<Value, String> {
    let status = resp.status().as_u16();
    let mut text = String::new();
    resp.into_body()
        .into_reader()
        .take(1 << 20)
        .read_to_string(&mut text)
        .map_err(|e| format!("{what}: reading response: {e}"))?;
    if !(200..300).contains(&status) {
        let snippet: String = text.chars().take(300).collect();
        return Err(format!("{what}: HTTP {status}: {snippet}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("{what}: invalid JSON: {e}"))
}

fn get_json(a: &ureq::Agent, url: &str, what: &str) -> Result<Value, String> {
    let resp = a
        .get(url)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| format!("{what}: {e}"))?;
    read_json(resp, what)
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

/// Accepts an origin (`https://prod-x.t3coderelay.com`), a bare host, or a
/// full thread URL, and returns the origin.
pub fn origin_from_arg(arg: &str) -> Result<String, String> {
    util::normalize_origin(arg).ok_or_else(|| format!("not a URL or host: {arg}"))
}

pub fn discover(origin: &str) -> Result<Discovery, String> {
    let a = agent();
    let prm_url = format!("{origin}/.well-known/oauth-protected-resource/mcp");
    let prm = get_json(&a, &prm_url, "protected resource metadata")?;
    let resource = str_field(&prm, "resource").unwrap_or_else(|| format!("{origin}/mcp"));
    let auth_server = prm
        .get("authorization_servers")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| origin.to_string());
    if let Some(scopes) = prm.get("scopes_supported").and_then(Value::as_array) {
        if !scopes.iter().any(|s| s.as_str() == Some(SCOPE)) {
            return Err(format!(
                "{origin} does not offer the {SCOPE} scope; refusing to request anything broader"
            ));
        }
    }
    let asm = get_json(
        &a,
        &format!("{auth_server}/.well-known/oauth-authorization-server"),
        "authorization server metadata",
    )?;
    if let Some(methods) = asm.get("code_challenge_methods_supported").and_then(Value::as_array) {
        if !methods.iter().any(|m| m.as_str() == Some("S256")) {
            return Err("authorization server does not support PKCE S256".into());
        }
    }
    Ok(Discovery {
        origin: origin.to_string(),
        resource,
        issuer: str_field(&asm, "issuer").unwrap_or(auth_server),
        authorization_endpoint: str_field(&asm, "authorization_endpoint")
            .ok_or("metadata has no authorization_endpoint")?,
        token_endpoint: str_field(&asm, "token_endpoint").ok_or("metadata has no token_endpoint")?,
        registration_endpoint: str_field(&asm, "registration_endpoint"),
    })
}

pub fn register(d: &Discovery, redirect_uri: &str) -> Result<String, String> {
    let url = d
        .registration_endpoint
        .as_deref()
        .ok_or("server does not support dynamic client registration")?;
    let body = json!({
        "client_name": "t3-watch (read-only watcher)",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": SCOPE,
    });
    let resp = agent()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body.to_string())
        .map_err(|e| format!("client registration: {e}"))?;
    let v = read_json(resp, "client registration")?;
    str_field(&v, "client_id").ok_or_else(|| "client registration returned no client_id".into())
}

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

impl Pkce {
    pub fn new() -> Self {
        let verifier = util::b64url(&util::random_bytes(48));
        let challenge = util::b64url(&util::sha256(verifier.as_bytes()));
        Pkce {
            verifier,
            challenge,
            state: util::b64url(&util::random_bytes(24)),
        }
    }
}

pub fn authorize_url(d: &Discovery, client_id: &str, redirect_uri: &str, p: &Pkce) -> String {
    let q = util::form_encode(&[
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
        ("state", &p.state),
        ("code_challenge", &p.challenge),
        ("code_challenge_method", "S256"),
        ("resource", &d.resource),
    ]);
    let sep = if d.authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{}{sep}{q}", d.authorization_endpoint)
}

struct Token {
    access_token: String,
    scope: Option<String>,
    expires_at_ms: Option<u64>,
}

fn exchange(
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    resource: &str,
) -> Result<Token, String> {
    let form = util::form_encode(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
        ("resource", resource),
    ]);
    let resp = agent()
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .send(form)
        .map_err(|e| format!("token exchange: {e}"))?;
    let v = read_json(resp, "token exchange")?;
    let access_token = str_field(&v, "access_token").ok_or("token response has no access_token")?;
    Ok(Token {
        access_token,
        scope: str_field(&v, "scope"),
        expires_at_ms: v
            .get("expires_in")
            .and_then(Value::as_u64)
            .map(|s| util::now_ms() + s * 1000),
    })
}

/// A short machine name from the origin host: `prod-05055d8a66f766cb` for a
/// relay, `local-3773` for a loopback server.
pub fn default_name(origin: &str) -> String {
    let Some(u) = util::parse_url(origin) else {
        return "t3".into();
    };
    let port = u
        .origin
        .rsplit_once(':')
        .map(|(_, p)| p)
        .filter(|p| p.chars().all(|c| c.is_ascii_digit()));
    let is_ip = u.host.parse::<std::net::IpAddr>().is_ok() || u.host.starts_with('[');
    if u.host == "localhost" || is_ip {
        return match port {
            Some(p) => format!("local-{p}"),
            None => "local-http".into(),
        };
    }
    u.host.split('.').next().unwrap_or("t3").to_string()
}

fn store(name: &str, d: &Discovery, client_id: &str, token: Token, make_default: bool) -> Result<Value, String> {
    let granted = token.scope.clone().unwrap_or_else(|| SCOPE.into());
    let extra: Vec<&str> = granted.split_whitespace().filter(|s| *s != SCOPE).collect();
    if !extra.is_empty() {
        eprintln!(
            "warning: server granted more than {SCOPE} ({}); t3-watch only calls read tools",
            extra.join(" ")
        );
    }
    let mut cfg = Config::load()?;
    cfg.machines.insert(
        name.to_string(),
        Machine {
            origin: d.origin.clone(),
            access_token: token.access_token,
            client_id: Some(client_id.to_string()),
            scope: Some(granted.clone()),
            issuer: Some(d.issuer.clone()),
            expires_at_ms: token.expires_at_ms,
            created_at_ms: Some(util::now_ms()),
        },
    );
    if make_default {
        cfg.default = Some(name.to_string());
    }
    cfg.save()?;
    Ok(json!({
        "event": "logged_in",
        "machine": name,
        "origin": d.origin,
        "scope": granted,
        "default": cfg.default.as_deref() == Some(name),
        "config": config::machines_path().display().to_string(),
    }))
}

fn check_callback(params: &[(String, String)], state: &str, issuer: &str) -> Result<String, String> {
    if let Some(err) = util::query_get(params, "error") {
        let desc = util::query_get(params, "error_description").unwrap_or("");
        return Err(format!("authorization failed: {err} {desc}").trim().to_string());
    }
    if util::query_get(params, "state") != Some(state) {
        return Err("state mismatch in callback; start the login again".into());
    }
    if let Some(iss) = util::query_get(params, "iss") {
        if iss.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(format!("callback issuer {iss} does not match {issuer}"));
        }
    }
    util::query_get(params, "code")
        .map(str::to_string)
        .ok_or_else(|| "callback has no code".into())
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Interactive login: a one-shot callback server on 127.0.0.1.
pub fn login(
    arg: &str,
    name: Option<&str>,
    port: u16,
    no_browser: bool,
    make_default: bool,
    wait: Duration,
) -> Result<Value, String> {
    let origin = origin_from_arg(arg)?;
    let name = name.map(str::to_string).unwrap_or_else(|| default_name(&origin));
    let d = discover(&origin)?;
    let listener =
        TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))?;
    let redirect_uri = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr().map_err(|e| e.to_string())?.port()
    );
    let client_id = register(&d, &redirect_uri)?;
    let pkce = Pkce::new();
    let url = authorize_url(&d, &client_id, &redirect_uri, &pkce);
    eprintln!("Approve read-only ({SCOPE}) access for t3-watch in your browser:\n\n  {url}\n");
    if !no_browser {
        open_browser(&url);
    }
    let params = accept_callback(&listener, wait)?;
    let code = check_callback(&params, &pkce.state, &d.issuer)?;
    let token = exchange(
        &d.token_endpoint,
        &client_id,
        &code,
        &redirect_uri,
        &pkce.verifier,
        &d.resource,
    )?;
    store(&name, &d, &client_id, token, make_default)
}

fn accept_callback(listener: &TcpListener, wait: Duration) -> Result<Vec<(String, String)>, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + wait;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = Vec::new();
                let mut chunk = [0u8; 2048];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 * 1024 {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf);
                let target = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("");
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                if path != "/callback" {
                    let _ =
                        stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let body = "<!doctype html><title>t3-watch</title><p>t3-watch is paired. You can close this tab.</p>";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                return Ok(util::parse_query(query));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Err("timed out waiting for the browser callback; try `t3-watch auth oauth-start` on headless machines".into());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("callback listener: {e}")),
        }
    }
}

/// Headless step 1: register, save the PKCE verifier, print the URL.
pub fn start(arg: &str, name: Option<&str>, redirect_port: u16, make_default: bool) -> Result<Value, String> {
    let origin = origin_from_arg(arg)?;
    let name = name.map(str::to_string).unwrap_or_else(|| default_name(&origin));
    let d = discover(&origin)?;
    let redirect_uri = format!("http://127.0.0.1:{redirect_port}/callback");
    let client_id = register(&d, &redirect_uri)?;
    let pkce = Pkce::new();
    let url = authorize_url(&d, &client_id, &redirect_uri, &pkce);
    let pending = json!({
        "name": name,
        "origin": d.origin,
        "resource": d.resource,
        "issuer": d.issuer,
        "authorizationEndpoint": d.authorization_endpoint,
        "tokenEndpoint": d.token_endpoint,
        "clientId": client_id,
        "redirectUri": redirect_uri,
        "codeVerifier": pkce.verifier,
        "makeDefault": make_default,
        "createdAtMs": util::now_ms(),
    });
    let dir = config::state_dir().join("pending");
    prune_pending(&dir);
    let path = dir.join(format!("{}.json", pkce.state));
    util::write_private(&path, pending.to_string().as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(json!({
        "event": "oauth_started",
        "machine": name,
        "scope": SCOPE,
        "authorizeUrl": url,
        "next": "Open authorizeUrl in any browser and approve. The browser then fails to load a 127.0.0.1 callback page; copy that page's full URL and run: t3-watch auth oauth-finish '<callback-url>'",
    }))
}

fn prune_pending(dir: &std::path::Path) {
    let now = util::now_ms();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let old = fs::read_to_string(e.path())
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("createdAtMs").and_then(Value::as_u64))
            .is_none_or(|t| now.saturating_sub(t) > PENDING_TTL_MS);
        if old {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Headless step 2: exchange the code from the pasted callback URL.
pub fn finish(callback: &str) -> Result<Value, String> {
    let callback = callback.trim();
    let query = callback.split_once('?').map(|(_, q)| q).unwrap_or(callback);
    let query = query.split('#').next().unwrap_or("");
    let params = util::parse_query(query);
    let state = util::query_get(&params, "state").ok_or("callback URL has no state parameter")?;
    if !state.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("callback state is malformed".into());
    }
    let path = config::state_dir().join("pending").join(format!("{state}.json"));
    let text = fs::read_to_string(&path).map_err(|_| {
        "no pending login matches this callback (expired or already used); run oauth-start again".to_string()
    })?;
    let p: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let s = |k: &str| str_field(&p, k).ok_or_else(|| format!("pending login is missing {k}"));
    if p.get("createdAtMs")
        .and_then(Value::as_u64)
        .is_none_or(|t| util::now_ms().saturating_sub(t) > PENDING_TTL_MS)
    {
        let _ = fs::remove_file(&path);
        return Err("pending login expired; run oauth-start again".into());
    }
    let d = Discovery {
        origin: s("origin")?,
        resource: s("resource")?,
        issuer: s("issuer")?,
        authorization_endpoint: s("authorizationEndpoint")?,
        token_endpoint: s("tokenEndpoint")?,
        registration_endpoint: None,
    };
    let code = check_callback(&params, state, &d.issuer)?;
    let client_id = s("clientId")?;
    let token = exchange(
        &d.token_endpoint,
        &client_id,
        &code,
        &s("redirectUri")?,
        &s("codeVerifier")?,
        &d.resource,
    )?;
    let _ = fs::remove_file(&path);
    let make_default = p.get("makeDefault").and_then(Value::as_bool).unwrap_or(false);
    store(&s("name")?, &d, &client_id, token, make_default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(
            default_name("https://prod-05055d8a66f766cb.t3coderelay.com"),
            "prod-05055d8a66f766cb"
        );
        assert_eq!(default_name("http://127.0.0.1:3773"), "local-3773");
        assert_eq!(default_name("http://localhost:3773"), "local-3773");
    }

    #[test]
    fn authorize_url_requests_read_scope_only() {
        let d = Discovery {
            origin: "https://r.example".into(),
            resource: "https://r.example/mcp".into(),
            issuer: "https://r.example".into(),
            authorization_endpoint: "https://r.example/oauth/mcp/authorize".into(),
            token_endpoint: "https://r.example/oauth/mcp/token".into(),
            registration_endpoint: None,
        };
        let p = Pkce::new();
        let url = authorize_url(&d, "cid", "http://127.0.0.1:9/callback", &p);
        let q = util::parse_query(url.split_once('?').unwrap().1);
        assert_eq!(util::query_get(&q, "scope"), Some("orchestration:read"));
        assert_eq!(util::query_get(&q, "code_challenge_method"), Some("S256"));
        assert_eq!(util::query_get(&q, "redirect_uri"), Some("http://127.0.0.1:9/callback"));
        assert_eq!(
            util::b64url(&util::sha256(p.verifier.as_bytes())),
            util::query_get(&q, "code_challenge").unwrap()
        );
    }

    #[test]
    fn callback_checks_state_and_issuer() {
        let q = util::parse_query("code=abc&state=s1&iss=https%3A%2F%2Fr.example");
        assert_eq!(check_callback(&q, "s1", "https://r.example").unwrap(), "abc");
        assert!(check_callback(&q, "s2", "https://r.example").is_err());
        assert!(check_callback(&q, "s1", "https://evil.example").is_err());
        let q = util::parse_query("error=access_denied&state=s1");
        assert!(check_callback(&q, "s1", "https://r.example")
            .unwrap_err()
            .contains("access_denied"));
    }
}
