# Release acceptance matrix: 0.15.0, engine 17

Build: 0.15.0, engine 17, rev `1e98e73c711d61ffdba77b614dcc7380ee573f3d`.
Review ordinal 18 approved the code at `af3bb3f`.

**The fleet and the release are not accepted.** Code approval is not
release acceptance. One gate still needs a person (T5, a real host
suspend), and one is an explicit protocol limit that is not waived (X3b).
Nothing is pushed, deployed, installed over a shared binary, or wired to a
production bot.

**Evidence kinds**

- **synthetic**: test fixtures (mock MCP or app-server, a fake `t3` CLI, a
  loopback receiver);
- **real, isolated**: real binaries and real Claude Code or Codex
  processes, in disposable sessions, isolated homes and loopback
  receivers;
- **real, production**: the parent's bot, its routine and its receipts;
- **parent-reported**: evidence the parent holds (on the Grok Bot box or
  in the bot's records), recorded here by path and id only. It is not
  re-verified from this worktree. No webhook URL, token or secret is
  recorded.

An exit code, or an HTTP 200 from a loopback receiver, is never counted as
a bot's continuation. In the Codex runs, approvals in the disposable
threads are declined by the probe's owner client (standing in for the
person who owns the thread). t3-watch answers none.

Times are UTC on 2026-11-09. The build host is the maintainer's Mac mini:
macOS 27.0.1, arm64, rustc 1.99.1, zig 0.17.0, codex-cli 0.166.0, Claude
Code 2.1.319.

## The native Linux run of 0.14.0 (gate T7: FAIL, fixed in 0.15.0)

Run on the Grok Bot Linux box: Debian, glibc 2.41, kernel 6.12.113,
x86_64, 2026-11-09T00:44:45Z. The archive and the tests archive checked
out (SHA-256 OK), and `--version` reported rev `bf58d758`. Its logs were
read from the box's evidence bundle.

- **Passed:** adversarial 18, consistency 11, hardening 33, integration
  24, native 11, outbox 7, workstream 19, and `e2e/durable.sh` (outage
  replay; SIGKILL with a hanging receiver).
- **Failed:**

| Failure on Linux | Root cause | Kind | Fix |
|---|---|---|---|
| compat: `a_hung_engine_5_daemon_with_a_full_backlog_is_found_by_its_lock` hung | On Linux a **blocking** connect to a Unix socket whose backlog is full waits until a slot frees; macOS refuses at once. The test filled the backlog with blocking connects. | test harness | `663c9ae`: backlogs are filled with non-blocking connects (`HungListener`) |
| (same root cause) | The product made the same blocking call. `daemon::connect`, used by every `wait`, `status` and `stop`, would wait forever on a hung daemon with a full backlog, and so would a starting daemon in `take_lock` | **product** | `663c9ae`: `util::connect_unix` retries non-blocking connects every 20 ms and gives up after 5 s with an error naming the daemon (exit 2); `take_lock` uses the non-blocking probe. New test: `a_client_meeting_a_hung_daemon_returns_instead_of_hanging` |
| codex: `a_dropped_connection_is_reopened_and_a_turn_finished_meanwhile_is_seen` (Broken pipe, never recovered) | The mock pings before each answer and closes after the third. The client's pong met the closed peer (EPIPE at once on Linux), and that write error ended the call although the answer was already buffered. macOS accepted the write while the peer was only shut down | **product** | `95f847a`: a pong that cannot be written is ignored and buffered frames are still read. The mock now closes every descriptor at once, which reproduces the failure on macOS too (old code: 2 of 3 runs time out; new code: 3 of 3 pass) |
| delivery ×2: `python3: can't open file '/private/tmp/t3wsrc/e2e/timeline.py'` | `CARGO_MANIFEST_DIR` is baked in at build time, as the macOS path | test harness | `b6a487f`: the script is built into the test (`include_str!`) and written next to the trace |
| suspend: `an_answer_buffered_across_a_suspend_never_completes_the_wait` (frozen, 1 s) | Linux ends a `SO_RCVTIMEO` read stopped by SIGSTOP with EINTR on SIGCONT, so the client drops the registration as slept through (`daemon.stale`) before reading the buffered answer. macOS restarts the read and finds the answer too late (`daemon.late`). Both refuse it, and the exit, outcome and webhook assertions held on Linux | test over-specified | `b6a487f`: the frozen case accepts either. New test `a_final_answer_read_after_the_deadline_is_not_used` reaches `daemon.late` without signals on every OS |
| unit: `watchdog_spawn_failure_kills_and_reaps_the_child` (Text file busy) | ETXTBSY: Linux refuses to run a file any process has open for writing, and a child forked by another test thread holds a just-written script open until its own `exec`. T3 updating its binary in place can do the same to a real watch | test race, reachable in production | `a135c50`: running `t3 thread show` is retried on ETXTBSY for up to 200 ms. New test: `a_t3_binary_still_open_for_writing_runs_once_it_is_closed` |

`1e98e73` makes this engine 17 (0.15.0): the daemon-side fixes change what
an engine-16 daemon would do for a new client.

**0.15.0 on the same box: PASS** (parent-reported). `linux-run.log` in
`/workspace/t3-watch-candidate-1e98e73` covers the 286 tests and
`e2e/durable.sh` scenarios A and B.

The Linux-specific branches of the new tests (the retried and refused
connect, ETXTBSY) can only run on Linux. On macOS they take the path macOS
takes.

## Commits since review ordinal 17 (branch `feat/t3-watch-v1`, unpushed)

| Commit | What |
|---|---|
| `417a5bb` | 0.14.0 engine 16, code approved by review ordinal 17 |
| `663c9ae` | Never block on a hung daemon's full backlog (Linux) |
| `95f847a` | A Codex pong that cannot be written no longer throws the answer away |
| `a135c50` | Retry running a `t3` binary that is briefly open for writing (ETXTBSY) |
| `b6a487f` | No build-host paths in the tests; the late-answer path covered on every OS |
| `1e98e73` | Engine 17 (0.15.0), capability `linux-io-hardening`; README Linux and hung-daemon notes |
| `af3bb3f` | The 0.15.0 matrix; code approved by review ordinal 18 |
| `c53741a` | Docs only: the bot-side gate results |
| this commit | Docs only: N4 and X6 confirmed by the parent's receipts |

## Artifacts

They live in the worktree's ignored `dist/0.15.0/`, with the logs in
`dist/0.15.0/evidence/`. All are built from an exact `git archive` of
`1e98e73`, and `--version` prints `t3-watch 0.15.0 (engine 17, rev
1e98e73c711d61ffdba77b614dcc7380ee573f3d)`.

They are built with
`RUSTFLAGS="--remap-path-prefix=$HOME=/build-home --remap-path-prefix=<source dir>=/build/src"`.
A plain build embeds the builder's home directory: 138 dependency
panic-location strings under `~/.cargo/registry` named the account. None of
the binaries below contains a `/Users/` or `/private/tmp` path. The test
binaries hold only the synthetic test strings `/Users/a` and
`/Users/someone`, and no `/private/tmp` path. A rebuild is not claimed to
match byte for byte.

| File | SHA-256 | Executed |
|---|---|---|
| `t3-watch-1e98e73-aarch64-apple-darwin` (1,970,016 bytes) | `9cb2653abe64738b5e2833dd0464f3bfb6ca4150946cb7a5f41cd22cbff2fa71` | Every real run below |
| `t3-watch-1e98e73-universal-apple-darwin` (arm64 + x86_64) | `94ebe7dded0534aaf8478631b3ed094d9f3cb881eea9093680fa2acafd8568a1` | arm64 only; x86_64 not run here (no Rosetta) |
| `t3-watch-0.15.0-x86_64-unknown-linux-gnu.tar.gz` | `1d7ec1e92b6b52d6eab3afe5b509abb570fae9b83d6c7c7bfc2e74ecdfbaa9f6` (binary `87eed7df…a026`, glibc 2.17 or older) | **PASS on Linux x86_64** (parent-reported: `linux-run.log`, see T7) |
| `t3-watch-0.15.0-aarch64-unknown-linux-gnu.tar.gz` | `e0035ce8e1057f13107ec2d43f8bb125080fe7ab4b83cbe036ddca563e050614` (binary `ed562c82…3ea9`, glibc 2.17 or older) | Not executed on Linux: the Linux run (T7) was on x86_64, and no aarch64 Linux host was used |
| `t3-watch-tests-0.15.0-x86_64-unknown-linux-gnu.tar.gz` | `c799e4a48e3b7cab8b2342f3216a16d21ad7b2bcfd469c51de59eab46d51842d` (12 test binaries, LTO off, plus the source) | **PASS on Linux x86_64** (parent-reported: 286 tests in `linux-run.log`, see T7) |
| `linux.sh`, `desktop-check.sh` | copies of `e2e/acceptance/linux.sh` and `e2e/native/codex/desktop-check.sh` | |

`dist/0.15.0/superseded/` holds the real-gate runs made with a build
without the path remapping (SHA-256 `55156763…0588`, same commit). They all
passed, but they are not counted: every run below was repeated with the
shipped binary.

## Build checks at `1e98e73`

| Check | Toolchain | Result | When | Log |
|---|---|---|---|---|
| `cargo test --locked` | rustc 1.99.1 | **PASS**: 286 passed, 0 failed | 01:23:07–01:24:56 | `evidence/suite-1e98e73.txt` |
| `cargo +1.85.1 test --locked` (MSRV) | rustc 1.85.1 | **PASS**: 286 passed, 0 failed | ends 01:32:12 | `evidence/msrv-1e98e73.txt` |
| `cargo fmt --check`; `cargo clippy --locked --all-targets -- -D warnings` | rustc 1.99.1 | **PASS**: clean | 01:30:10 | `evidence/lint-1e98e73.txt` |

## Gates

The rows run on this host used the shipped arm64 binary (`9cb2653a`).
The parent-reported rows used builds of rev `1e98e73`, whose hashes are in
the parent's records.

| # | Gate | Kind | Status | Evidence |
|---|---|---|---|---|
| T1 | The correct bot woken by a real completion webhook, and continuing | real, production | **PASS** (parent-reported) | watchId `1650566df28c0743`, notificationId `5c5f52008d347700b6f8ebeec62f4206`, disposable thread `mcp:launch:e22bc0d7…`. `/workspace/t3-watch-gates-1e98e73-03cb7361cb9947b7/FINAL-evidence.json` and `T1-RECEIVED.json`, with the earlier parent continuation evidence |
| T2 | A real `needs_input` notification, and the parent's continuation | real, production | **PASS** (parent-reported) | watchId `9aa64e5f7e064c90` (the `8cb27369…` family). The bot answered the disposable thread's question. `T2.RESULT`, same evidence directory |
| T3 | Durable delivery through a controlled outage and a daemon restart | real, isolated | **PASS** | `e2e/durable.sh`. With the receiver down, the client exited 4 with the event queued. After a daemon stop and restart, the receiver came up and the event was delivered on attempt 5 with the same id (`3eda608a…`), 2.0 s later. `evidence/durable-1e98e73.txt` |
| T3b | Durable replay to the **real** receiver, and the bot continuing | real, production | **PASS** (parent-reported) | The clean replay id `0903158322f9d1610fd19123acd696dd` was delivered on attempt 5 after the outage; a second run (`f28ec25e…`) corroborates. `/tmp/t3w-replay-clean.TmS7` and `T3b-clean-real-receiver-receipt-09031583.json` |
| T4 | Crash recovery: SIGKILL of the daemon mid-delivery | real, isolated | **PASS** | Same run: delivered on attempt 4 with the same id (`5d670c8e…`), 1.8 s after the receiver answered again. Delivery is at least once |
| T5 | A real host suspend and resume | real | **PENDING (person, not waived)** | Not done, and never simulated in its place: this host is shared and kept awake. `e2e/acceptance/suspend.sh` (step 1). A sleeping host sends nothing until it wakes, so never promise instant notification from sleeping hardware |
| T6 | Live local T3, read only | real | **PASS** | 01:35:51: a completed fixture thread, directly (334 ms, watchId `4db36c437cb1e5df`) and through the daemon (311 ms, `5a0da98f8417d0cc`); `--until workstream` on local exits 2. `evidence/live-local-1e98e73.txt` |
| T7 | Native Linux run of the latest archives and tests | real | **PASS** for 0.15.0 (parent-reported); 0.14.0 FAILED (above) | `linux-run.log` in `/workspace/t3-watch-candidate-1e98e73` on the Grok Bot box: 286 tests and `e2e/durable.sh` A and B |
| N1 | Claude Code: synthetic hook events | synthetic | **PASS** | `tests/native.rs`, 11 tests |
| N3 | Claude Code: real interactive permission dialog | real, isolated | **PASS** | Disposable session `574fd4ce-1e77-4856-90ce-66c6f7ec90d6`, 01:38:00. `needs_input` and its webhook arrived (tool `Bash`). The dialog was rejected with Escape and never approved, and the command did not run. The hook log holds `at`, `event`, `type`, `tool` and `subagent` only. `evidence/claude-interactive-1e98e73.txt` |
| N4 | Claude Code: same-session reuse wakes the bot, which continues | real, production | **PASS** (parent-confirmed) | Correlation `cbbacb6e4f0d4d1d`. Session `c4ee92be-9a09-453f-96ed-09a0f6340e63` was seeded at run 1, then a wait pinned to run 2 or later settled: watchId `24f5cc6a72c561cd`, notificationId `5dd7d903973ff20824a8eaed632d9cef`, run 2, HTTP 200 on attempt 1. The run ended 04:11:17.157Z, checked on this host in `/tmp/t3wn4/wait.json` (rev `1e98e73`, direct). The bot's routine fired at 04:11:17.479Z, 0.32 s later (x-request-id `00ec2595-ffc8-47f9-b16b-ee4400c12722`). The woken run named that session and run 2, and its continuation wrote `RECEIVED-24f5cc6a72c561cd.json` at about 04:11:44Z (`/workspace/t3-watch-gates-1e98e73-03cb7361cb9947b7/x6n4-cbbacb6e4f0d4d1d`). The owner harness confirmed the session as its disposable one |
| X1 | Codex: mock app-server | synthetic | **PASS** | `tests/codex.rs`, 17 tests |
| X2 | Codex: real app-server, disposable threads | real, isolated | **PASS** | `e2e/native/codex/run.sh`, 01:34:26–01:37:18, thread `01a1bdf1-408a-7b11-b190-43e417afa590`: <ul><li>needs-input and its webhook arrived 678 ms after the approval request;</li><li>in the watcher and control windows alike, only the app-server's own log database changed;</li><li>the owner's request stayed its own;</li><li>pinned wake 1.0 s after `turn/completed`;</li><li>unload at 60 s while polled, then a read after unload.</li></ul> `evidence/codex-run-sh-1e98e73.txt` |
| X3 | Codex: two approvals in one turn | real, isolated | **PASS** | Same run, at pauses of 3 s, 0.5 s and 0 s: <ul><li>one connected wait woke for request 2 under its own key (56 to 559 ms after it opened);</li><li>through the daemon, request 2 opened between waits and got its own key;</li><li>on the keeper drain path, a wait inside a round kept the request, and the next request reached the third wait;</li><li>a second `--no-daemon` wait counted the open request again, as documented.</li></ul> |
| X3b | Codex: a request opened and closed while nothing counts | protocol limit | **FAIL (explicit, not waived)** | It is never reported, though its owner answered it. `tests/codex.rs::a_request_that_opens_and_closes_while_nothing_counts_is_never_seen` |
| X4 | Codex: default socket through the official shared daemon | real, isolated | **PASS** | 01:37:53. Completion, then a pinned run 2 486 ms after `turn/completed`. The daemon was stopped and no LaunchAgent changed. `evidence/codex-daemon-socket-1e98e73.txt` |
| X5 | Codex: the desktop thread on the MacBook, read only | real | **PASS** (parent-reported) | `desktop-check.sh` against the research thread exited 0 at run ordinal 4. The thread's id is withheld from this tracked file; it is in `x5-desktop-check.out`. No prompt, no resume, no change to the research |
| X6 | Codex: the bot reuses a thread and continues | real, production | **PASS** (parent-confirmed) | Correlation `cbbacb6e4f0d4d1d`. The disposable thread is `01a1be81-857d-7483-a375-daadd3d77217`, not the research thread, whose id does not appear in this evidence. The watcher stayed read-only; the continuation turn was started through the official app-server `turn/start` on the same thread. The wait settled: watchId `5a10216c5de0876f`, notificationId `8f3bb34189ae635b8eeba62cf39de44f`, run 2, HTTP 200 on attempt 1. The run ended 04:12:20.117Z, checked on this host in `/tmp/t3wx6/wait.json` (rev `1e98e73`, direct). The bot's routine fired at 04:12:20.431Z, 0.31 s later (x-request-id `2ce9bb5f-7085-4791-b3be-5861be3327a6`). The woken run named that thread and run 2, and wrote `RECEIVED-5a10216c5de0876f.json` at about 04:12:44Z, in the same evidence directory. The owner harness confirmed it |

Further corroboration of a real wake: the parent's own matrix watcher
for the 0.15.0 matrix run (watchId `3de6713bf07c82a8`, event id
`03dca959d09f027a5d78fa35fac1f881`) woke the bot at 04:16:40.094Z, 0.45 s
after that run completed, and its continuation was the next instruction to
this worktree.

## Still open

1. **T5, a real host suspend (a person, on a machine that may sleep).**
   Run `sh e2e/acceptance/suspend.sh <t3-watch> <thread URL> [webhook URL]`
   with the thread running elsewhere. Sleep the machine and wake it a few
   minutes later. Record the printed `suspendedMs`, `elapsedAwakeMs` and
   `elapsedWallMs`, and the system's sleep and wake lines, against the
   webhook's arrival. Not waived, and never simulated in its place.
2. **X3b** stays an explicit protocol limit, not waived. A Codex request
   opened and answered while no t3-watch connection counts is never
   reported; bots should use the daemon.

## Publish-ready history

`feat/t3-watch-v1` is left as it is. A separate local branch,
`publish/t3-watch-scrubbed`, holds the same history with private details
removed from every commit message and every commit's files: the MacBook's
hostname, an account name, the research project's path, the build host's
name, a real thread id and environment id, a home path, and a person's
name in one doc. That includes the messages of `4dca907` and `17f71fb`.
The `LICENSE-MIT` copyright line and the commit authors are kept. Its
tip's tree is identical to `feat/t3-watch-v1`'s. It is not pushed.

## Scope notes: what ran on this host and what ran elsewhere

- `/concil`: no command, skill or plugin by that name exists. The
  `council` skill's default panel includes the review provider that must
  not be retried, so neither was run.
- **The runtime on this host** (names only, no contents read): the T3
  server runs always on. There is no t3-watch install, saved pairing,
  webhook receiver or bot routine. So T1, T2, T3b and X6 were not run on
  this host. The parent ran them against its own receiver and bot
  routine, where the webhook URL and secret are, and they pass on its
  evidence (the rows above). T7 and X5 likewise ran on the Grok Bot box
  and the MacBook.

## Side effects of these runs

- **Claude Code.** It recorded folder-trust answers for the disposable
  `/private/tmp/t3wcc*/proj` folders in `~/.claude.json` and the test
  prompts in `~/.claude/history.jsonl`; neither file was edited by hand.
  The sessions' `~/.claude/projects` and `~/.claude/session-env` entries
  were removed.
- **Codex.** Every app-server, daemon and mock started here was stopped,
  and their fixtures and sockets were removed. `~/.codex` was not used,
  and no real thread was read or touched.
