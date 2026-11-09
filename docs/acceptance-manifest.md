# Release acceptance matrix: 0.15.0, engine 17

This build is **ready for independent review and not accepted**. Review
ordinal 17 approved the code at `417a5bb` (0.14.0, engine 16). The first
native Linux run of that build then **failed** gate T7: six tests across
five test binaries, plus one test that hung. The commits below fix every
failure; they have not been reviewed or run on Linux yet. Code approval is
not release acceptance: each BLOCKED gate needs the parent's bot, a
person, or a machine this host cannot reach. Nothing is pushed, deployed,
installed over a shared binary, or wired to a production bot.

**Evidence kinds**

- **synthetic**: test fixtures (mock MCP or app-server, a fake `t3` CLI, a
  loopback receiver);
- **real, isolated**: real binaries and real Claude Code or Codex
  processes, in disposable sessions, isolated homes and loopback
  receivers;
- **real, production**: the parent's bot, its routine and its receipts.

An exit code, or an HTTP 200 from a loopback receiver, is never counted as
a bot's continuation. In the Codex runs, approvals in the disposable
threads are declined by the probe's owner client (standing in for the
person who owns the thread). t3-watch answers none.

Times are UTC on 2026-11-09. The build host is the maintainer's Mac mini:
macOS 27.0.1, arm64, rustc 1.99.1, zig 0.17.0, codex-cli 0.166.0, Claude
Code 2.1.319.

## The native Linux run of 0.14.0 (gate T7: FAIL)

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
| this commit | This matrix |

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
| `t3-watch-0.15.0-x86_64-unknown-linux-gnu.tar.gz` | `1d7ec1e92b6b52d6eab3afe5b509abb570fae9b83d6c7c7bfc2e74ecdfbaa9f6` (binary `87eed7df…a026`, glibc 2.17 or older) | **Not yet on Linux** |
| `t3-watch-0.15.0-aarch64-unknown-linux-gnu.tar.gz` | `e0035ce8e1057f13107ec2d43f8bb125080fe7ab4b83cbe036ddca563e050614` (binary `ed562c82…3ea9`, glibc 2.17 or older) | **Not yet on Linux** |
| `t3-watch-tests-0.15.0-x86_64-unknown-linux-gnu.tar.gz` | `c799e4a48e3b7cab8b2342f3216a16d21ad7b2bcfd469c51de59eab46d51842d` (12 test binaries, LTO off, plus the source) | **Not yet on Linux** |
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

| # | Gate | Kind | Status | Evidence (with the shipped arm64 binary `9cb2653a`) |
|---|---|---|---|---|
| T1 | The correct bot woken by a real completion webhook, and continuing | real, production | **BLOCKED (parent)** | Passed with engine 12 (parent-held); needs this build (step 1) |
| T2 | A real `needs_input` notification, and the parent's continuation | real, production | **BLOCKED (parent)** | Step 2 |
| T3 | Durable delivery through a controlled outage and a daemon restart | real, isolated | **PASS** | `e2e/durable.sh`. The receiver was down: the client exited 4 with the event queued. After a daemon stop and restart, the receiver came up and the event was delivered on attempt 5 with the same id (`3eda608a…`), 2.0 s later. `evidence/durable-1e98e73.txt` |
| T3b | Durable replay to the **real** receiver, and the bot continuing | real, production | **BLOCKED (parent)** | `e2e/acceptance/real-receiver-replay.sh` with `e2e/relay.py` (step 3) |
| T4 | Crash recovery: SIGKILL of the daemon mid-delivery | real, isolated | **PASS** | Same run: delivered on attempt 4 with the same id (`5d670c8e…`), 1.8 s after the receiver answered again. Delivery is at least once |
| T5 | A real host suspend and resume | real | **BLOCKED (person)** | This host is shared and kept awake, and a VM suspend needs a macOS image (30 to 80 GB) against about 2 GB free. `e2e/acceptance/suspend.sh` (step 4). A sleeping host sends nothing until it wakes; never promise instant notification from sleeping hardware |
| T6 | Live local T3, read only | real | **PASS** | 01:35:51: a completed fixture thread, directly (334 ms, watchId `4db36c437cb1e5df`) and through the daemon (311 ms, `5a0da98f8417d0cc`); `--until workstream` on local exits 2. `evidence/live-local-1e98e73.txt` |
| T7 | Native Linux run of the latest archives and tests | real | **FAIL** (0.14.0, above); 0.15.0 **not yet run** | Step 5, on the Grok Bot Linux box |
| N1 | Claude Code: synthetic hook events | synthetic | **PASS** | `tests/native.rs`, 11 tests |
| N3 | Claude Code: real interactive permission dialog | real, isolated | **PASS** | Disposable session `574fd4ce-1e77-4856-90ce-66c6f7ec90d6`, 01:38:00. `needs_input` and its webhook arrived (tool `Bash`). The dialog was rejected with Escape and never approved, and the command did not run. The hook log holds `at`, `event`, `type`, `tool` and `subagent` only. `evidence/claude-interactive-1e98e73.txt` |
| N4 | Claude Code: same-session and `--resume` reuse | real, isolated | **PASS (mechanism)**; the bot's continuation is **BLOCKED (parent)** | Same session: `settled` run 2, 1.5 s after the prompt. `--resume` of print session `574fd4ce-…-90d7`: `settled` run 2, 4.6 s after the resume started |
| X1 | Codex: mock app-server | synthetic | **PASS** | `tests/codex.rs`, 17 tests |
| X2 | Codex: real app-server, disposable threads | real, isolated | **PASS** | `e2e/native/codex/run.sh`, 01:34:26–01:37:18, thread `01a1bdf1-408a-7b11-b190-43e417afa590`. Needs-input and its webhook arrived 678 ms after the approval request. In the watcher and control windows alike, only the app-server's own log database changed, and the owner's request stayed its own. The pinned wake came 1.0 s after `turn/completed`, the thread unloaded at 60 s while polled, and a read after unload worked. `evidence/codex-run-sh-1e98e73.txt` |
| X3 | Codex: two approvals in one turn | real, isolated | **PASS** | Same run, at pauses of 3 s, 0.5 s and 0 s. One connected wait woke for request 2 under its own key (56 to 559 ms after it opened). Through the daemon, request 2 opened between waits and got its own key. On the keeper drain path (rounds stretched to 3 s), a wait inside a round kept the request, and the next request reached the third wait. A second `--no-daemon` wait counted the open request again, as documented |
| X3b | Codex: a request opened and closed while nothing counts | protocol limit | **FAIL (explicit)** | It is never reported, though its owner answered it. `tests/codex.rs::a_request_that_opens_and_closes_while_nothing_counts_is_never_seen` |
| X4 | Codex: default socket through the official shared daemon | real, isolated | **PASS** | 01:37:53. Completion, then a pinned run 2 486 ms after `turn/completed`. The daemon was stopped and no LaunchAgent changed. `evidence/codex-daemon-socket-1e98e73.txt` |
| X5 | Codex: the desktop thread on the MacBook, read only | real | **BLOCKED (MacBook not reachable)** | The MacBook refuses this host's SSH key. Desktop compatibility is **unverified** (step 6) |
| X6 | Codex: the bot reuses a thread and continues | real, production | **BLOCKED (parent)** | Step 7 |

## Steps for the parent or a person

1. **T1, completion (parent).** Run `t3-watch 0.15.0` with a disposable T3
   test thread: `t3-watch wait --durable --notify-on all <thread URL>`
   with the bot's webhook. Record the receipt and continuation with
   `e2e/timeline.py`.
2. **T2, needs-input (parent).** The same, with a test thread that asks a
   question. Nobody answers from the watcher.
3. **T3b, replay to the real receiver (parent).** Run
   `T3W_RELAY_TARGET=<webhook URL> T3W_SECRET=<secret> sh e2e/acceptance/real-receiver-replay.sh <t3-watch> <thread URL>`.
4. **T5, suspend (person, on a machine that may sleep).** Run
   `sh e2e/acceptance/suspend.sh <t3-watch> <thread URL> [webhook URL]`.
5. **T7, Linux (on the Grok Bot box).** Copy `dist/0.15.0/` there and run
   `sh linux.sh <that dir> 1e98e73c711d61ffdba77b614dcc7380ee573f3d`. It
   checks the hashes and `--version`, runs the 12 test binaries against
   the archive's binary, and runs `e2e/durable.sh`. T7 passes only if
   every binary reports `ok` and the script prints `RESULT: PASS`.
6. **X5, the Codex desktop thread (person, on the MacBook as its user).**
   Run `sh desktop-check.sh <universal binary> <thread id>`. It is read
   only: no daemon start, no resume, no prompt.
7. **N4 and X6, bot reuse (parent).** The bot reads `runOrdinal`, waits
   through the daemon with `--expect-min-run-ordinal <that + 1>` (plus
   `--acknowledge` for a request it already relayed), and continues.

## Publish-ready history

`feat/t3-watch-v1` is left as it is. A separate local branch,
`publish/t3-watch-scrubbed`, holds the same history with private details
removed from every commit message and every commit's files: the MacBook's
hostname, an account name, the research project's path, the build host's
name, a real thread id and environment id, a home path, and a person's
name in one doc. That includes the messages of `4dca907` and `17f71fb`.
The `LICENSE-MIT` copyright line and the commit authors are kept. Its
tip's tree is identical to `feat/t3-watch-v1`'s. It is not pushed.

## Not run, and why

- `/concil`: no command, skill or plugin by that name exists. The
  `council` skill's default panel includes the review provider that must
  not be retried, so neither was run.
- **The runtime on this host** (names only, no contents read): the T3
  server runs always on. There is no t3-watch install, saved pairing,
  webhook receiver or bot routine. So T1, T2, T3b and X6 run where the
  parent holds the webhook URL and secret.

## Side effects of these runs

- **Claude Code.** It recorded folder-trust answers for the disposable
  `/private/tmp/t3wcc*/proj` folders in `~/.claude.json` and the test
  prompts in `~/.claude/history.jsonl`; neither file was edited by hand.
  The sessions' `~/.claude/projects` and `~/.claude/session-env` entries
  were removed.
- **Codex.** Every app-server, daemon and mock started here was stopped,
  and their fixtures and sockets were removed. `~/.codex` was not used,
  and no real thread was read or touched.
