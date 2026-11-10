# Limitations

- `--until workstream` needs the MCP transport and a T3 that returns
  `resultRun`; without it the wait fails closed (exit 2). Locally, use
  `--final-marker` with instructions that end the final synthesis with the
  marker. Neither contract can know about work started after T3 reported the
  workstream settled.
- Over MCP, a question or approval on a thread that still says `running` is
  seen within about 30 s, not instantly. The local transport reports
  needs-input without the request text, and on a delegated child thread an
  approval shows only as a count. The shape of `t3_pending_request read`
  answers was not observed live (no child was blocked on a question while
  this was built); unknown shapes give the request id without its text.
- The local transport costs one `t3 thread show` per watched thread per
  interval, about 0.4 s of CPU on the maintainer's Mac mini. For many local threads, the
  local server's own MCP endpoint should give long-polling instead
  (`t3-watch auth login http://127.0.0.1:3773`). Its OAuth discovery answers
  correctly, but that login needs a browser approval and was not run end to
  end here.
- T3's authorization server currently lists only the `authorization_code`
  grant, so it issues no refresh tokens. When its token expires or is
  revoked, `wait` exits 2 (after rereading machines.json) and you pair again.
  Servers that do issue refresh tokens are refreshed automatically.
- Windows is not supported (Unix sockets, `setsid`).
- Linux binaries are cross-built on macOS with zig as the C compiler and
  linker (x86_64 and aarch64 `-unknown-linux-gnu`, glibc 2.17 at most),
  together with the test binaries, which `e2e/acceptance/linux.sh` runs
  natively. The first native run (0.14.0, on Debian with glibc 2.41) failed
  six tests across five test binaries. 0.15.0 fixes them: three in t3-watch
  itself (the connect to a hung daemon, the Codex pong, a busy `t3`
  binary) and three in the tests; 0.15.0 passed its native run (gate T7 in
  `docs/acceptance-manifest.md`). 0.16.1 has not been run on Linux yet. CI
  (`.github/workflows/ci.yml`) has not run, since the branch is unpushed.
- A daemon that holds its socket but does not take a connection within
  5 s (hung, with a full backlog) makes `wait`, `status` and `stop` fail
  with an error naming it (exit 2), on Linux as on macOS, instead of
  waiting for it. `t3-watch wait --no-daemon` works without it.
- Without `--durable`, a notification still being delivered when the
  waiting process is killed is lost. With it, delivery is at least once:
  receivers must deduplicate on `X-T3-Watch-Id` / `Idempotency-Key`. After a
  reboot, queued entries wait for the next t3-watch command (or a supervised
  `t3-watch outbox run`). Heartbeats and progress events from before a
  daemon restart are not replayed.
- A watcher cannot report its own host's death; that needs a receiver-side
  monitor of `--heartbeat` events.
