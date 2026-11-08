# Acceptance evidence manifest: 0.12.0, engine 14

This build is **ready for independent acceptance and not accepted**. The
code at `910b684` (0.10.0, engine 12) passed review ordinal 14. Nothing
after it has been reviewed. No production bot integration is enabled, and
nothing has been pushed, deployed, or installed over a shared binary.

Kinds of evidence used below:

- **synthetic**: test fixtures (mock MCP or app-server, a fake `t3` CLI, a
  local receiver);
- **real, isolated**: real binaries and real Claude Code or Codex
  processes, in disposable sessions, isolated homes and loopback
  receivers;
- **real, production**: the parent's bot, its routine and receipts. The
  parent holds these, and this worktree cannot reproduce them.

An exit code or an HTTP 200 from a local receiver does not show that a
bot woke or continued.

Times are UTC. The host is the Mac mini: macOS 27.0.1, arm64.

## Commits (branch `feat/t3-watch-v1`, unpushed)

| Commit | What |
|---|---|
| `910b684` | 0.10.0 engine 12: code approved by review ordinal 14 (baseline) |
| `6792303` | status fails closed like `outbox list`; truthful daemon lifetime; live-daemon detection (lock or non-blocking connect) |
| `bdcf393` | engine 13: Claude Code requests tracked one by one; an idle session without a turn never settles |
| `082a639` | test: the stale engine-6 socket is bound but never listens (a parallel-fork race in the test) |
| `86bd181` | engine 14 (0.12.0): native Codex threads read from the app-server, never resumed |
| `7a97700` | docs: real interactive Claude Code approval and reuse results |
| this commit | e2e tools for the native and durable gates; this manifest |

The code in `7a97700` and later is identical to `86bd181`. Only docs and
`e2e/` changed after it.

## Artifacts

They live in the worktree's ignored `dist/0.12.0/`, with the logs in
`dist/0.12.0/evidence/`. A rebuild of the same commit is not claimed to
match byte for byte.

| File | Built from | SHA-256 | Notes |
|---|---|---|---|
| `t3-watch-86bd181-aarch64-apple-darwin` | `86bd181`, rustc 1.99.1, `cargo build --release --locked`, `T3_WATCH_BUILD_REV` set | `466d4f0c164e50bcdb20dc2bc5acba9515821509626ba2e01fb436acb4882ae8` | 1,953,456 bytes. `--version`: `t3-watch 0.12.0 (engine 14, rev 86bd1811d134a56bfec4fe00f5c00e6eb88c0938)`. Every real run below used this file |
| `t3-watch-0.12.0-x86_64-unknown-linux-gnu.tar.gz` | `7a97700`, zig 0.17.0 cross-build | `2705bb714ab55606823b29ae22dc710fdf8dc289b48ef8b090e143cc680a733a` | needs glibc 2.17 at most; **not executed on Linux** |
| `t3-watch-0.12.0-aarch64-unknown-linux-gnu.tar.gz` | `7a97700`, zig 0.17.0 cross-build | `4853a3031d8b59d840887ee7049c604ca915ae37bc06b555e2f57c04315b847b` | needs glibc 2.17 at most; **not executed on Linux** |

The last Linux execution evidence is the parent's smoke of `910b684` (the
old baseline).

## Build checks

| Check | Commit | Toolchain | Result | When | Log |
|---|---|---|---|---|---|
| Full locked suite (`cargo test --locked`) | `86bd181` | rustc 1.99.1 | **273 passed, 0 failed** | 10:36:00–10:37:08 | `evidence/suite-86bd181.txt` |
| MSRV (`cargo +1.85.1 test --locked`) | `7a97700` | rustc 1.85.1 | **273 passed, 0 failed** | 10:53:40–10:54:58 | `evidence/msrv-7a97700.txt` |
| `cargo fmt --check`; `cargo clippy --locked --all-targets -- -D warnings` | `7a97700` | rustc 1.99.1 | clean | 10:55:12 | `evidence/lint-7a97700.txt` |

The first full run of the Codex work hung on a stale test: it expected
`codex://` to be refused, and with no timeout it waited forever. That test
was replaced in `86bd181`. One parallel run of the reconnect test timed
out while another project's tests loaded the mini; the next 4 of 4 runs
passed.

## Gates

| # | Gate | Kind | Status | Evidence |
|---|---|---|---|---|
| T1 | Correct bot woken by a real completion webhook | real, production | **PASS (parent-held, engine 12)** | Mini, review ordinal 14, delivered in under 1 s; earlier watchId `c2366ae100a095fb`. Not re-run on 0.12.0 |
| T2 | Real `needs_input` wake **and** the parent's continuation | real, production | **BLOCKED (parent)** | Needs a T3 test thread that asks a question, and the bot's routine. The mechanism is shown by N3 and X2 |
| T3 | Durable delivery through a controlled outage and a daemon restart | real, isolated | **PASS (mechanism)** | Receiver down on loopback; the client exits 4 with the event queued (4 attempts); `t3-watch stop`; the entry is still listed with no daemon; receiver up, daemon restarted; delivered 2.0 s later, same id, attempt 5. `evidence/durable-86bd181.txt`, `evidence/durable-e2e-sh-86bd181.txt` |
| T3b | The real receiver continues after a durable replay | real, production | **BLOCKED (parent)** | Protocol below |
| T4 | Crash recovery: SIGKILL of the daemon mid-delivery | real, isolated | **PASS** | The receiver held attempt 1 open; `kill -9` of the daemon; a client restarted a daemon. When the receiver answered again, the event went through with the same id on attempt 4, 1.8 s later. It was delivered at least once: the receiver logged 3 unanswered attempts with that id, so receivers must deduplicate on `X-T3-Watch-Id`. Same logs |
| T5 | Actual host suspend and resume, with the real latency | real | **BLOCKED (human)** | No host sleep was done. Synthetic suspend tests pass in the suite. Protocol below |
| T6 | Live local T3, read only | real | **PASS** | The completed fixture thread `mcp:launch:7c9315ba…` through the local CLI: completed run 2, both direct (443 ms) and through the daemon (310 ms); `--until workstream` on local still exits 2. `evidence/live-local-86bd181.txt` |
| T7 | Native Linux run of the archives | real | **BLOCKED (no Linux host here)** | Archives and hashes above |
| N1 | Claude Code: synthetic hook events | synthetic | **PASS** | `tests/native.rs` (11 tests) |
| N2 | Claude Code: real print session wakes a wait | real, isolated | **PASS (2.1.312, earlier)** | `docs/native-sessions.md`, check 2 |
| N3 | Claude Code: real interactive permission dialog | real, isolated | **PASS** | Claude Code 2.1.319, disposable session `25916d4d-…-dcb`. `needs_input` with its webhook (tool `Bash`); rejected with Escape; never approved; the command did not run. `evidence/claude-interactive-86bd181.txt` (run 4) and `evidence/claude-run4-hooklogs/` |
| N4 | Claude Code: reuse wakes a pinned wait | real, isolated | **PASS (mechanism)**; the bot's continuation is **BLOCKED (parent)** | Same live session, pinned and acknowledged: `settled` run 2 in 1.5 s. `--resume` of a print session: `settled` run 2 in 4.1 s, including the model's reply |
| X1 | Codex: mock app-server | synthetic | **PASS** | `tests/codex.rs` (8 tests): read methods only, no server request answered, framing, reconnect, paging, pins, numbered requests across processes |
| X2 | Codex: real app-server, disposable threads | real, isolated | **PASS** | codex-cli 0.166.0 with a mock model provider (no OpenAI request): fresh thread keeps waiting; needs-input exit 5 and webhook, 79 ms to 1.0 s after the approval request (within one poll interval); no file changes beyond the app-server's own log database, in both the watcher and control windows; completion; pinned wake 10 ms to 1.0 s after `turn/completed`; unload at 60 s while polled; read after unload. `evidence/codex-probe-86bd181.txt`; `evidence/codex-run-sh-86bd181.txt` (`e2e/native/codex/run.sh`, 11:17:23–11:19:57, exit 0) |
| X3 | Codex: two requests in one turn | real, isolated | **PASS with a model pause of 0.5 s or more; LIMIT with none** | An acknowledged wait woke for request 2 under its own key (55–577 ms) with 3 s or 0.5 s pauses. With no pause, the second request was missed until the turn ended. `evidence/codex-two-requests-86bd181.txt` |
| X4 | Codex: default socket through the official shared daemon | real, isolated | **PASS** | `codex app-server daemon start` in an isolated `CODEX_HOME`; t3-watch with no socket override: completed run 1, pinned run 2 in 489 ms; daemon stopped; no LaunchAgent change. `evidence/codex-daemon-socket-86bd181.txt` |
| X5 | Codex: the desktop thread on the MacBook | real, read only | **BLOCKED (MacBook not reachable from here)** | Read-only protocol in `docs/native-sessions.md`, "Checking the desktop target on the MacBook". Desktop compatibility is still unverified |
| X6 | Codex: the bot reuses a thread and continues | real, production | **BLOCKED (parent)** | |
| B1 | MSRV, fmt, clippy, locked suite | build | **PASS** | above |

## Reproduce

From the worktree, with `BIN` set to a built `t3-watch`:

```sh
cargo test --locked                                    # suite
cargo +1.85.1 test --locked                            # MSRV (toolchain needed)
cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings
T3_WATCH_BUILD_REV=$(git rev-parse HEAD) cargo build --release --locked
sh e2e/durable.sh /tmp/t3wdur "$BIN" 18821             # T3, T4: see /tmp/t3wdur/timeline.txt
sh e2e/native/codex/run.sh /tmp/t3wcxr "$BIN"          # X2, X3 (needs codex 0.166.0)
python3 -I e2e/native/claude_interactive.py /tmp/t3wcc "$BIN" "$(uuidgen | tr A-Z a-z)"   # N3, N4
```

`claude_interactive.py` spends a few small haiku requests on the user's
Claude Code login. Its docstring lists what to remove afterwards. The
version in `e2e/` differs from the one run 4 used only in its docstring,
and it no longer saves the screen to a file. X4 was run by hand: the
commands are in `evidence/codex-daemon-socket-86bd181.txt` and above.

## Steps that need a person or the parent

1. **T2, needs-input with continuation.** Create a T3 test thread that asks
   a question. Run `t3-watch wait --durable --notify-on all <thread URL>`
   with the bot's webhook. Expect exit 5 and a `needs_input` event. Record
   the bot's receipt and continuation with `e2e/timeline.py`
   (`docs/live-integration.md`, step 5). Nobody answers from the watcher.
2. **T3b, durable replay to the real receiver.** On the watcher host only,
   block the receiver's address (for example a `pf` rule), run a wait to
   completion, and check that `t3-watch outbox list` holds the event.
   Remove the rule, confirm the 2xx `outbox.attempt` in the trace, then
   record the bot's receipt and continuation.
3. **T5, suspend and resume.** On the host that runs the watcher, start
   `t3-watch wait --durable --notify-on all --timeout 30m <thread>`. A
   person then sleeps the machine (`pmset sleepnow` or the Apple menu),
   lets the watched turn finish while it sleeps, and wakes it after a few
   minutes. Record the result's `suspendedMs`, `elapsedAwakeMs` and
   `endedAt`, the webhook's arrival time, and the wake time. No CPU runs
   while the host sleeps: the event is sent after the wake, at the next
   poll, so it is late by the sleep. If the sleep outlasts `--timeout`,
   the right outcome is `timeout`, not a late completion.
4. **T7, native Linux.** Unpack either archive, check it with
   `shasum -a 256 -c`, and check that `t3-watch --version` names rev
   `7a9770068354da03d4f87e268080e4a3d5a03e33`. Run `e2e/live.sh` against
   that host's T3, then the scenarios above with that binary.
5. **X5, the Codex desktop target.** The read-only checks listed in
   `docs/native-sessions.md`. No prompt is sent to
   `01a1b891-e945-7f70-b755-db5ece6dcaff`, and nothing is migrated or
   copied.
6. **N4 and X6, bot reuse.** The parent's bot reuses a disposable session
   or thread. It reads `runOrdinal`, waits with
   `--expect-min-run-ordinal <that + 1>` (plus `--acknowledge` for a
   request it already relayed), and submits. Record the receipt and the
   continuation.

## Side effects and notes

- **Claude Code.** It recorded the trust answers for
  `/private/tmp/t3wcc*/proj` in `~/.claude.json` and the test prompts in
  `~/.claude/history.jsonl`; neither file was edited by hand. The
  disposable sessions' `~/.claude/projects/-private-tmp-t3wcc*-proj` and
  `~/.claude/session-env/<id>` entries were removed. The interactive
  sessions left no transcript, so `--resume` of one found nothing. The
  print sessions did leave one.
- **Codex.** Every app-server, daemon and mock started for these checks was
  stopped. Their fixtures and their control sockets under
  `/private/tmp/codex-daemon-501/` were removed. `~/.codex` was not used.
- **T3's own `codex app-server`.** T3's own app-server (child of `t3
  serve`) was replaced by `t3 serve` at 10:29:22. None of these commands
  targeted it: every kill named a fixture path or port.
- **Shared load.** Another project's tests ran on the mini during these
  checks.
