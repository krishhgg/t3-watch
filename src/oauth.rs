//! Read-only OAuth 2.0 pairing with a T3 MCP server (authorization code +
//! PKCE S256, public client from dynamic registration).
//!
//! t3-watch only ever requests `orchestration:read`. Discovery follows RFC
//! 9728 (protected resource metadata) and RFC 8414 (authorization server
//! metadata) strictly, and every endpoint must be `https://` (plain `http://`
//! only on loopback). Tokens go to machines.json (0600) and are never printed;
//! error messages carry an OAuth error code at most, never a response body.

use crate::config::{self, Config, Machine};
use crate::util;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub const SCOPE: &str = "orchestration:read";
const PENDING_TTL_MS: u64 = 60 * 60 * 1000;
const MAX_METADATA: u64 = 1 << 20;

#[derive(Debug, Clone)]
pub struct Discovery {
    pub origin: String,
    pub resource: String,
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    /// The server lists `refresh_token` in `grant_types_supported`.
    pub supports_refresh: bool,
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

/// Reads a JSON response. On an error status it reports the OAuth `error`
/// code if there is one, and nothing else from the body.
fn read_json(resp: ureq::http::Response<ureq::Body>, what: &str) -> Result<Value, String> {
    let status = resp.status().as_u16();
    let mut bytes = Vec::new();
    resp.into_body()
        .into_reader()
        .take(MAX_METADATA + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{what}: reading response failed ({:?})", e.kind()))?;
    if bytes.len() as u64 > MAX_METADATA {
        return Err(format!("{what}: response larger than {MAX_METADATA} bytes"));
    }
    let parsed = serde_json::from_slice::<Value>(&bytes);
    if !(200..300).contains(&status) {
        let code = parsed
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(util::sanitize_code))
            .filter(|c| !c.is_empty());
        return Err(match code {
            Some(c) => format!("{what}: HTTP {status} ({c})"),
            None => format!("{what}: HTTP {status}"),
        });
    }
    parsed.map_err(|_| format!("{what}: response is not valid JSON"))
}

fn get_json(a: &ureq::Agent, url: &str, what: &str) -> Result<Value, String> {
    let resp = a
        .get(url)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| format!("{what}: {}", util::describe_ureq_error(&e)))?;
    read_json(resp, what)
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

/// Accepts an origin (`https://prod-x.t3coderelay.com`), a bare host, or a
/// full thread URL, and returns the origin. Refuses plain http off loopback.
pub fn origin_from_arg(arg: &str) -> Result<String, String> {
    let origin = util::normalize_origin(arg).ok_or_else(|| format!("not a URL or host: {arg}"))?;
    Ok(util::secure_url(&origin, "server origin")?.origin)
}

/// A metadata endpoint URL, required to be https (or loopback http).
fn endpoint(v: &Value, key: &str) -> Result<String, String> {
    let url = str_field(v, key).ok_or_else(|| format!("authorization server metadata has no {key}"))?;
    util::secure_url(&url, key)?;
    Ok(url)
}

/// RFC 8414 section 3.1: the well-known segment goes between the host and
/// the issuer's path, so `https://h/tenant` maps to
/// `https://h/.well-known/oauth-authorization-server/tenant`.
pub fn as_metadata_url(issuer: &str) -> Result<String, String> {
    let u = util::secure_url(issuer, "authorization server issuer")?;
    if !u.query.is_empty() || issuer.contains('#') {
        return Err("authorization server issuer must not have a query or fragment".into());
    }
    let path = u.path.trim_end_matches('/');
    Ok(format!("{}/.well-known/oauth-authorization-server{path}", u.origin))
}

pub fn discover(origin: &str) -> Result<Discovery, String> {
    let origin = origin_from_arg(origin)?;
    let a = agent();
    let expected_resource = format!("{origin}/mcp");
    let prm = get_json(
        &a,
        &format!("{origin}/.well-known/oauth-protected-resource/mcp"),
        "protected resource metadata",
    )?;
    let resource = str_field(&prm, "resource").ok_or("protected resource metadata has no resource")?;
    if resource.trim_end_matches('/') != expected_resource {
        return Err(format!(
            "protected resource metadata names {:?}, not {expected_resource}",
            util::sanitize_text(&resource, 200)
        ));
    }
    if let Some(scopes) = prm.get("scopes_supported").and_then(Value::as_array) {
        if !scopes.iter().any(|s| s.as_str() == Some(SCOPE)) {
            return Err(format!(
                "{origin} does not offer the {SCOPE} scope; refusing to request anything broader"
            ));
        }
    }
    let issuer = prm
        .get("authorization_servers")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .ok_or("protected resource metadata lists no authorization server")?
        .to_string();
    let asm = get_json(&a, &as_metadata_url(&issuer)?, "authorization server metadata")?;
    // RFC 8414 section 3.3: the metadata must name the issuer we asked for.
    if str_field(&asm, "issuer").as_deref() != Some(issuer.as_str()) {
        return Err(format!(
            "authorization server metadata issuer does not match {}",
            util::sanitize_text(&issuer, 200)
        ));
    }
    let methods = asm
        .get("code_challenge_methods_supported")
        .and_then(Value::as_array)
        .ok_or("authorization server does not advertise PKCE support")?;
    if !methods.iter().any(|m| m.as_str() == Some("S256")) {
        return Err("authorization server does not support PKCE S256".into());
    }
    if let Some(types) = asm.get("response_types_supported").and_then(Value::as_array) {
        if !types.iter().any(|t| t.as_str() == Some("code")) {
            return Err("authorization server does not support the authorization code flow".into());
        }
    }
    let registration_endpoint = match asm.get("registration_endpoint") {
        Some(_) => Some(endpoint(&asm, "registration_endpoint")?),
        None => None,
    };
    // RFC 8414: an absent list means authorization_code and implicit only.
    let supports_refresh = asm
        .get("grant_types_supported")
        .and_then(Value::as_array)
        .is_some_and(|g| g.iter().any(|t| t.as_str() == Some("refresh_token")));
    Ok(Discovery {
        origin,
        resource,
        issuer,
        authorization_endpoint: endpoint(&asm, "authorization_endpoint")?,
        token_endpoint: endpoint(&asm, "token_endpoint")?,
        registration_endpoint,
        supports_refresh,
    })
}

pub fn register(d: &Discovery, redirect_uri: &str) -> Result<String, String> {
    let url = d
        .registration_endpoint
        .as_deref()
        .ok_or("server does not support dynamic client registration")?;
    // Ask for refresh tokens only where the server says it issues them, so a
    // strict registration endpoint does not reject the client.
    let grants: &[&str] = if d.supports_refresh {
        &["authorization_code", "refresh_token"]
    } else {
        &["authorization_code"]
    };
    let body = json!({
        "client_name": "t3-watch (read-only watcher)",
        "redirect_uris": [redirect_uri],
        "grant_types": grants,
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": SCOPE,
    });
    let resp = agent()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body.to_string())
        .map_err(|e| format!("client registration: {}", util::describe_ureq_error(&e)))?;
    let v = read_json(resp, "client registration")?;
    str_field(&v, "client_id")
        .filter(|c| !c.is_empty() && !util::has_control_chars(c))
        .ok_or_else(|| "client registration returned no client_id".into())
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

pub struct Token {
    access_token: String,
    refresh_token: Option<String>,
    scope: Option<String>,
    expires_at_ms: Option<u64>,
}

fn token_request(token_endpoint: &str, form: &[(&str, &str)], what: &str) -> Result<Token, String> {
    util::secure_url(token_endpoint, "token endpoint")?;
    let resp = agent()
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .send(util::form_encode(form))
        .map_err(|e| format!("{what}: {}", util::describe_ureq_error(&e)))?;
    let v = read_json(resp, what)?;
    let access_token = str_field(&v, "access_token")
        .filter(|t| !t.is_empty() && !util::has_control_chars(t))
        .ok_or_else(|| format!("{what}: response has no usable access_token"))?;
    Ok(Token {
        access_token,
        refresh_token: str_field(&v, "refresh_token").filter(|t| !t.is_empty() && !util::has_control_chars(t)),
        scope: str_field(&v, "scope"),
        expires_at_ms: v
            .get("expires_in")
            .and_then(Value::as_u64)
            .map(|s| util::now_ms() + s * 1000),
    })
}

fn exchange(
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    resource: &str,
) -> Result<Token, String> {
    token_request(
        token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
            ("resource", resource),
        ],
        "token exchange",
    )
}

fn refresh(token_endpoint: &str, client_id: &str, refresh_token: &str, resource: &str) -> Result<Token, String> {
    token_request(
        token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("resource", resource),
            ("scope", SCOPE),
        ],
        "token refresh",
    )
}

/// Recovers from a 401 for the machine `name` at `origin` whose token
/// `used` was rejected. Inside the config lock: if machines.json already has
/// a different token for that origin (another login or refresh), use it;
/// otherwise refresh with the saved refresh token. Never returns a token
/// issued for a different origin.
pub fn recover_token(name: &str, origin: &str, used: &str) -> Option<String> {
    Config::transaction(|cfg| {
        let key = match cfg.machines.get(name) {
            Some(m) if m.origin.eq_ignore_ascii_case(origin) => name.to_string(),
            _ => match cfg.by_origin(origin) {
                Some((n, _)) => n.clone(),
                None => return Ok(None),
            },
        };
        let m = cfg.machines.get_mut(&key).expect("key was just found");
        if !m.access_token.is_empty() && m.access_token != used {
            return Ok(Some(m.access_token.clone()));
        }
        let (Some(rt), Some(endpoint), Some(client_id)) =
            (m.refresh_token.clone(), m.token_endpoint.clone(), m.client_id.clone())
        else {
            return Ok(None);
        };
        let resource = m.resource.clone().unwrap_or_else(|| format!("{}/mcp", m.origin));
        let t = refresh(&endpoint, &client_id, &rt, &resource)?;
        m.access_token = t.access_token.clone();
        if t.refresh_token.is_some() {
            m.refresh_token = t.refresh_token;
        }
        m.expires_at_ms = t.expires_at_ms;
        Ok(Some(t.access_token))
    })
    .ok()
    .flatten()
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
    let extra: Vec<String> = granted
        .split_whitespace()
        .filter(|s| *s != SCOPE)
        .map(util::sanitize_code)
        .collect();
    if !extra.is_empty() {
        eprintln!(
            "warning: server granted more than {SCOPE} ({}); t3-watch only calls read tools",
            extra.join(" ")
        );
    }
    let machine = Machine {
        origin: d.origin.clone(),
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        client_id: Some(client_id.to_string()),
        scope: Some(granted.clone()),
        issuer: Some(d.issuer.clone()),
        token_endpoint: Some(d.token_endpoint.clone()),
        resource: Some(d.resource.clone()),
        expires_at_ms: token.expires_at_ms,
        created_at_ms: Some(util::now_ms()),
        credential_id: Some(util::hex(&util::random_bytes(16))),
    };
    let can_refresh = machine.refresh_token.is_some();
    let is_default = Config::transaction(|cfg| {
        cfg.machines.insert(name.to_string(), machine);
        if make_default {
            cfg.default = Some(name.to_string());
        }
        Ok(cfg.default.as_deref() == Some(name))
    })?;
    Ok(json!({
        "event": "logged_in",
        "machine": name,
        "origin": d.origin,
        "scope": util::sanitize_text(&granted, 200),
        "default": is_default,
        "canRefresh": can_refresh,
        "config": config::machines_path().display().to_string(),
    }))
}

fn check_callback(params: &[(String, String)], state: &str, issuer: &str) -> Result<String, String> {
    if util::query_get(params, "state") != Some(state) {
        return Err("state mismatch in callback; start the login again".into());
    }
    if let Some(err) = util::query_get(params, "error") {
        let desc = util::query_get(params, "error_description")
            .map(|d| format!(": {}", util::sanitize_text(d, 160)))
            .unwrap_or_default();
        return Err(format!("authorization failed ({}){desc}", util::sanitize_code(err)));
    }
    if let Some(iss) = util::query_get(params, "iss") {
        if iss != issuer {
            return Err("callback issuer does not match the authorization server".into());
        }
    }
    util::query_get(params, "code")
        .filter(|c| !c.is_empty())
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
    let (mut browser, result) = match accept_callback(&listener, wait, &pkce.state, &d.issuer) {
        Ok((stream, code)) => {
            let saved = exchange(
                &d.token_endpoint,
                &client_id,
                &code,
                &redirect_uri,
                &pkce.verifier,
                &d.resource,
            )
            .and_then(|token| store(&name, &d, &client_id, token, make_default));
            (Some(stream), saved)
        }
        Err((stream, e)) => (stream, Err(e)),
    };
    // The browser only hears "paired" once the credentials are on disk.
    if let Some(s) = browser.as_mut() {
        let (status, text) = match &result {
            Ok(_) => ("200 OK", "t3-watch is paired. You can close this tab."),
            Err(_) => (
                "400 Bad Request",
                "t3-watch could not finish pairing. See the terminal for details.",
            ),
        };
        respond(s, status, text);
    }
    result
}

fn respond(stream: &mut TcpStream, status: &str, text: &str) {
    let body = format!("<!doctype html><title>t3-watch</title><p>{text}</p>");
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Reads one HTTP request head (bounded) and returns its method and target.
fn read_request_line(stream: &mut TcpStream) -> Option<(String, String)> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 * 1024 {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next()?.split_whitespace();
    Some((parts.next()?.to_string(), parts.next()?.to_string()))
}

type CallbackResult = Result<(TcpStream, String), (Option<TcpStream>, String)>;

/// Waits for the browser's callback. Requests for other paths or with a
/// different `state` (a favicon fetch, a stale tab, a port scan) get a 404 or
/// 400 and are ignored until the deadline. A matching callback returns its
/// connection still open, so the page can report whether saving worked.
fn accept_callback(listener: &TcpListener, wait: Duration, state: &str, issuer: &str) -> CallbackResult {
    listener.set_nonblocking(true).map_err(|e| (None, e.to_string()))?;
    let deadline = Instant::now() + wait;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let Some((method, target)) = read_request_line(&mut stream) else {
                    continue;
                };
                let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                if method != "GET" || path != "/callback" {
                    respond(&mut stream, "404 Not Found", "Not found.");
                    continue;
                }
                let params = util::parse_query(query);
                if util::query_get(&params, "state") != Some(state) {
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        "This link does not belong to the pending t3-watch login.",
                    );
                    continue;
                }
                return match check_callback(&params, state, issuer) {
                    Ok(code) => Ok((stream, code)),
                    Err(e) => Err((Some(stream), e)),
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Err((
                        None,
                        "timed out waiting for the browser callback; try `t3-watch auth oauth-start` on headless machines"
                            .into(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err((None, format!("callback listener: {e}"))),
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

/// Headless step 2: exchange the code from the pasted callback URL, save the
/// credentials, and only then retire the pending login.
pub fn finish(callback: &str) -> Result<Value, String> {
    let callback = callback.trim();
    let query = callback.split_once('?').map(|(_, q)| q).unwrap_or(callback);
    let query = query.split('#').next().unwrap_or("");
    let params = util::parse_query(query);
    let state = util::query_get(&params, "state").ok_or("callback URL has no state parameter")?;
    if state.is_empty() || !state.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("callback state is malformed".into());
    }
    let path = config::state_dir().join("pending").join(format!("{state}.json"));
    let text = fs::read_to_string(&path).map_err(|_| {
        "no pending login matches this callback (expired or already used); run oauth-start again".to_string()
    })?;
    let p: Value = serde_json::from_str(&text).map_err(|_| "pending login file is corrupt".to_string())?;
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
        supports_refresh: false,
    };
    util::secure_url(&d.origin, "origin")?;
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
    let make_default = p.get("makeDefault").and_then(Value::as_bool).unwrap_or(false);
    let out = store(&s("name")?, &d, &client_id, token, make_default)?;
    let _ = fs::remove_file(&path);
    Ok(out)
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
    fn origins_must_be_secure() {
        assert_eq!(
            origin_from_arg("prod-1.t3coderelay.com").unwrap(),
            "https://prod-1.t3coderelay.com"
        );
        assert_eq!(
            origin_from_arg("http://127.0.0.1:3773/x").unwrap(),
            "http://127.0.0.1:3773"
        );
        assert!(origin_from_arg("http://relay.example").is_err());
    }

    #[test]
    fn metadata_url_follows_rfc8414() {
        assert_eq!(
            as_metadata_url("https://as.example").unwrap(),
            "https://as.example/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            as_metadata_url("https://as.example/").unwrap(),
            "https://as.example/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            as_metadata_url("https://as.example/tenant/a").unwrap(),
            "https://as.example/.well-known/oauth-authorization-server/tenant/a"
        );
        assert!(as_metadata_url("http://as.example").is_err());
        assert!(as_metadata_url("https://as.example/?x=1").is_err());
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
            supports_refresh: false,
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
        let q = util::parse_query("error=access_denied%0D%0AX&error_description=no%0Athanks&state=s1");
        let e = check_callback(&q, "s1", "https://r.example").unwrap_err();
        assert!(e.contains("(access_deniedX)") && e.contains("no thanks"), "{e}");
        assert!(!e.contains('\r') && !e.contains('\n'));
    }
}
