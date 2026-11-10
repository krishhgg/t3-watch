# Security

- Tokens live in `machines.json` (0600) inside the 0700 state directory and
  are never printed, logged or passed on the command line. Every change to
  machines.json (login, `oauth-finish`, `default`, `logout`, token refresh)
  happens under `config.lock`, rereads the file inside the lock, and is
  written through a fresh `O_EXCL` temp file that is fsynced, renamed and
  followed by an fsync of the directory.
- t3-watch only requests `orchestration:read` and refuses servers whose
  metadata lists scopes without it. Whatever a server grants, the MCP client
  refuses any tool call other than `t3_thread_wait`, `t3_thread_read` and
  `t3_pending_request` with `action` `list` or `read`; it cannot answer a
  question, approve, send or interrupt.
- Pending request text is redacted and bounded before it leaves the
  machine (see [When a thread needs input](needs-input.md)).
- Outbox entries hold webhook URLs, headers and signing secrets in 0600
  files in a 0700 directory, like tokens in machines.json; listings show the
  destination origin only.
- Discovery is strict. The protected resource metadata must name
  `<origin>/mcp`, the authorization server metadata (fetched from the RFC 8414
  location, including issuers with a path) must name the same issuer, PKCE
  `S256` must be advertised, and the authorization, token and registration
  endpoints must be `https://` (loopback `http://` excepted). Refresh tokens
  are only requested from servers that list the `refresh_token` grant.
- OAuth uses PKCE S256, a random `state`, and checks the `iss` callback
  parameter when the server sends it. Headless logins expire after an hour,
  a callback URL works once, and `oauth-finish` saves the credentials before
  it deletes the pending login. `oauth-finish` first claims the pending login
  with an atomic rename, so concurrent finishes of one callback exchange the
  code once; a failed exchange or save puts the claim back. The loopback login
  ignores stray requests (a favicon, a stale tab with another `state`) and
  only shows "paired" once the credentials are saved. Its `--timeout` is an
  absolute deadline: it bounds every accept and every header read, so slow or
  endless connections cannot stretch it. Errors from the server show
  its OAuth `error` code at most, never the response body.
- The daemon socket is 0600 inside a 0700 directory, and the daemon runs with
  umask 077 so the socket is never briefly open to others. Clients refuse a
  socket or directory that is not theirs or is open to group or others, even
  with `T3_WATCH_SOCKET`; a directory you choose must already be private and
  is never chmodded. Lock files are opened with `O_NOFOLLOW`, and anything
  that is not a socket at the socket path is left alone. Both ends check the
  other's uid (`getpeereid` on macOS, `SO_PEERCRED` on Linux).
