# Release acceptance matrix: 0.16.1, engine 19

Build: 0.16.1, engine 19, rev `13fd055668daff0e984109a159bc59a7c5620d05`.
Review ordinal 21 approved 0.16.0 (`e32c09b`, no P0 or P1) and listed
three findings, fixed here; this build has not been reviewed yet.

**The fleet and the release are not accepted.** Code approval is not
release acceptance. X3b passes on this host (real app-server, isolated)
through Codex's `PermissionRequest` hook, with the limits listed under
X3b, and X7 (the hook in the user's own Codex) is parent-confirmed. The
gates the parent ran for 0.15.0 (T1, T2, T3b, T7, N4, X5, X6) are pending
for this build. Nothing is pushed, deployed, installed over a shared
binary, or wired to a production bot.

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

## X3b: how it works

The app-server keeps no record of past requests; Codex's documented
`PermissionRequest` hook runs for every approval request, whatever is
connected. `t3-watch hook codex` is that hook, installed by the user as an
async hook (`--print-settings` prints the block; t3-watch never edits
Codex's settings) and trusted in `/hooks` or Settings > Hooks. It appends
the time, turn id and tool name of each request to
`<state>/native/codex/<session>.requests.jsonl`, never the command or its
input, prints nothing, always exits 0 and returns no decision. The
counting connection matches each record to a request it counted live; one
no count matches opened while nothing counted and is reported in
`recoveredRequests` and once as `requests_recovered` progress. It never
becomes a needs-input wake: its owner answered it. `hooks/list`
(read-only) sets `requestRecovery`. Watching stays read-only: no
`thread/resume`, no answer, no decision. Details and limits:
`docs/native-sessions.md`, "Requests missed while nothing counts".

## Review ordinal 21: findings and fixes

| Finding | Fix | Tests (each new behavior test fails on `e32c09b`) |
|---|---|---|
| P2: a replaced, truncated or deleted hook log was read again, then ignored: the old record count skipped the new lines, so later missed approvals in the turn were never reported | `78d3d74`: the cursor keeps the log's device and inode, an offset on a line it can check (the turn's first record, else the last complete line) with that line's time, and the newest record time read. The same file with the same line there is read on from the cursor; anything else is read whole, and only records newer than every one already read are new. A 0.16.0 cursor is trusted once if its bytes still fit (`7bfaa8d`) | `tests/codex.rs::a_replaced_truncated_or_deleted_hook_log_loses_and_repeats_nothing` (truncated, truncated and regrown, deleted and recreated, replaced by a copy, a half-written last line; one process per step), `a_count_stored_by_0_16_0_is_taken_over_without_reporting_its_records_again` |
| P3: a record written more than 60 s after its live count was taken as a new request (a spurious `requests_recovered`, or an extra wake) | `78d3d74`: no time limit on matching. A count waits for its record until a request counted after it has had its record for 15 s (Codex starts a session's background hooks in order and stops each after the printed 10 s timeout), or the turn ends | `a_hook_record_that_lands_long_after_its_count_is_still_that_request` (a record two minutes late), and unit tests `a_record_that_lands_late_still_matches_its_count`, `a_count_with_no_record_is_given_up_once_a_later_record_has_settled`, `records_match_newest_first_and_never_a_count_made_after_them` |
| P3: `open_log` left an existing log's wider mode | `78d3d74`: an existing regular file it owns is set back to 0600, as `open_lock_file` does | `util::tests::an_existing_log_with_wider_access_is_made_private`; the hook test makes a 0644 log 0600 again |

## Commits since the 0.16.0 matrix (`e32c09b`, branch `feat/t3-watch-v1`, unpushed)

| Commit | What |
|---|---|
| `78d3d74` | The hook log cursor survives replacement; late records still match; logs kept 0600 (review ordinal 21) |
| `7bfaa8d` | A 0.16.0 hook cursor is taken over without reporting its records again |
| `13fd055` | Engine 19 (0.16.1), capability `native-codex-hook-cursor` |
| this commit | The 0.16.1 matrix |

The 0.16.0 commits (`f2a14db`, `0cd9b9f`, `1720230`, `4632305`, `e32c09b`)
added the hook log; review ordinal 21 approved them.

## Artifacts

They live in the worktree's ignored `dist/0.16.1/`, with the logs in
`dist/0.16.1/evidence/`. All are built from an exact `git archive` of
`13fd055`, and `--version` prints `t3-watch 0.16.1 (engine 19, rev
13fd055668daff0e984109a159bc59a7c5620d05)`.

They are built with
`RUSTFLAGS="--remap-path-prefix=$HOME=/build-home --remap-path-prefix=<source dir>=/build/src"`,
so no binary embeds the builder's home. None of the binaries below contains
a `/Users/` or `/private/tmp` path. The test binaries hold only the
synthetic test strings `/Users/a` and `/Users/someone`, and no
`/private/tmp` path. A rebuild is not claimed to match byte for byte.

| File | SHA-256 | Executed |
|---|---|---|
| `t3-watch-13fd055-aarch64-apple-darwin` (2,003,248 bytes) | `0a039a14bd3629da12aa53d96f5d646b895a3d25289f9fd771b0bfbdd40231aa` | Every real run below |
| `t3-watch-13fd055-universal-apple-darwin` (arm64 + x86_64) | `353684293301d0bf93024031d80acb75865834b51770f323989c09e0bd55ae84` | Not run; x86_64 is not run here (no Rosetta) |
| `t3-watch-0.16.1-x86_64-unknown-linux-gnu.tar.gz` | `60b85c7c3392eace8e74a7f5e3b2ff6c261dc6a99f221177bc97cca207f59cf2` (binary `8c3494ed…068d`, glibc 2.17 or older) | **Not yet run on Linux** (T7, the parent's box) |
| `t3-watch-0.16.1-aarch64-unknown-linux-gnu.tar.gz` | `0ad4b329ef4074cea523bc5708014bc69c2c1fdd5f6cedd3cd39be876815e620` (binary `1331e938…c8ec`, glibc 2.17 or older) | Not run: no aarch64 Linux host |
| `t3-watch-tests-0.16.1-x86_64-unknown-linux-gnu.tar.gz` | `e3fdacf1a5f232d42f53bb596486fcb4fbaedb7d8d56964662c0a295c8fb86cd` (12 test binaries, LTO off, plus the source) | **Not yet run on Linux** (T7) |
| `linux.sh`, `desktop-check.sh` | copies of `e2e/acceptance/linux.sh` (`1dbe6485…5293`) and `e2e/native/codex/desktop-check.sh` (`c95b18ad…25ec`), both unchanged since 0.16.0 | |

## Build checks at `13fd055`

| Check | Toolchain | Result | When | Log |
|---|---|---|---|---|
| `cargo test --locked` | rustc 1.99.1 | **PASS**: 298 passed, 0 failed | 06:29:25–06:31:17 | `evidence/suite-13fd055.txt` |
| `cargo +1.85.1 test --locked` (MSRV) | rustc 1.85.1 | **PASS**: 298 passed, 0 failed | 06:31:27–06:33:26 | `evidence/msrv-13fd055.txt` |
| `cargo fmt --check`; `cargo clippy --locked --all-targets -- -D warnings` | rustc 1.99.1 | **PASS**: clean | 06:31:17 | `evidence/lint-13fd055.txt` |

The timing flake seen once with 0.16.0
(`adversarial::a_cancelled_local_poll_kills_its_t3_process`, outside the
hook code) did not recur in either run. It is not fixed.

## Gates

The rows run on this host used the shipped arm64 binary (`0a039a14`). The
parent-reported results are for 0.15.0 (rev `1e98e73`) unless the row says
otherwise; this build changes the engine, so the parent's gates are
pending for it.

| # | Gate | Kind | Status | Evidence |
|---|---|---|---|---|
| T1 | The correct bot woken by a real completion webhook, and continuing | real, production | **PENDING (parent) for 0.16.1**; PASS for 0.15.0 (parent-reported) | 0.15.0: watchId `1650566df28c0743`, notificationId `5c5f52008d347700b6f8ebeec62f4206`, `/workspace/t3-watch-gates-1e98e73-03cb7361cb9947b7/FINAL-evidence.json` and `T1-RECEIVED.json` |
| T2 | A real `needs_input` notification, and the parent's continuation | real, production | **PENDING (parent) for 0.16.1**; PASS for 0.15.0 (parent-reported) | 0.15.0: watchId `9aa64e5f7e064c90`, `T2.RESULT`, same evidence directory |
| T3 | Durable delivery through a controlled outage and a daemon restart | real, isolated | **PASS** | `e2e/durable.sh`. With the receiver down, the client exited 4 with the event queued. After a daemon stop and restart, the receiver came up and the event was delivered on attempt 5 with the same id (`21cf1b47…`), 2.0 s later. `evidence/durable-13fd055.txt` |
| T3b | Durable replay to the **real** receiver, and the bot continuing | real, production | **PENDING (parent) for 0.16.1**; PASS for 0.15.0 (parent-reported) | 0.15.0: replay id `0903158322f9d1610fd19123acd696dd`, attempt 5; `T3b-clean-real-receiver-receipt-09031583.json` |
| T4 | Crash recovery: SIGKILL of the daemon mid-delivery | real, isolated | **PASS** | Same run: delivered on attempt 4 with the same id (`8b49ac51…`), 1.8 s after the receiver answered again. Delivery is at least once |
| T5 | A real host suspend and resume | real | **PASS** (parent-reported for 0.15.0, carried: the suspend path is unchanged); evidence not recorded here | Review ordinal 21 reports a real-suspend pass for 0.15.0. Its evidence (ids, path) has not reached this worktree and is not recorded here; the 0.15.0 and 0.16.0 matrices listed T5 as pending. Since `1e98e73`, `src/daemon.rs`, `tests/suspend.rs` and `e2e/acceptance/suspend.sh` are unchanged and the wait clock only gained a field (`started_ms`); `tests/suspend.rs` (8 tests) passes here. Never simulated in its place |
| T6 | Live local T3, read only | real | **PASS** | 06:39:19: a completed fixture thread, directly (328 ms, watchId `f02040dab42053e6`) and through the daemon (307 ms, `d29faae795fe7a76`); `--until workstream` on local exits 2. `evidence/live-local-13fd055.txt` |
| T7 | Native Linux run of the latest archives and tests | real | **PENDING (parent) for 0.16.1**; PASS for 0.15.0 (parent-reported) | `sh dist/0.16.1/linux.sh dist/0.16.1 13fd055` on the Grok Bot box. 0.15.0: `linux-run.log` in `/workspace/t3-watch-candidate-1e98e73`, 286 tests and `e2e/durable.sh` A and B |
| N1 | Claude Code: synthetic hook events | synthetic | **PASS** | `tests/native.rs`, 11 tests |
| N3 | Claude Code: real interactive permission dialog | real, isolated | **PASS** | Disposable session `f08112c9-44a8-478f-a1ba-51d1c1115097`, 06:39:37. `needs_input` and its webhook arrived (tool `Bash`). The dialog was rejected with Escape and never approved, and the command did not run. Same-session reuse woke a pinned wait 1.6 s after the next prompt; a `--resume` reuse 1.6 s after the resume started. The hook log holds `at`, `event`, `type`, `tool` and `subagent` only. `evidence/claude-interactive-13fd055.txt` |
| N4 | Claude Code: same-session reuse wakes the bot, which continues | real, production | **PENDING (parent) for 0.16.1**; PASS for 0.15.0 (parent-confirmed) | 0.15.0: correlation `cbbacb6e4f0d4d1d`, watchId `24f5cc6a72c561cd`, `RECEIVED-24f5cc6a72c561cd.json` |
| X1 | Codex: mock app-server | synthetic | **PASS** | `tests/codex.rs`, 25 tests, and 3 matching unit tests in `src/codex.rs` |
| X2 | Codex: real app-server, disposable threads | real, isolated | **PASS** | `e2e/native/codex/run.sh`, 06:34:51–06:37:50, thread `01a1bf04-4e12-7f53-9e46-31f4620b4183`: <ul><li>needs-input and its webhook arrived 737 ms after the approval request;</li><li>in the watcher and control windows alike, only the app-server's own log database changed;</li><li>the owner's request stayed its own;</li><li>pinned wake 1.0 s after `turn/completed`;</li><li>unload at 60 s while polled, then a read after unload.</li></ul> `evidence/codex-run-sh-13fd055.txt` |
| X3 | Codex: two approvals in one turn | real, isolated | **PASS** | Same run, at pauses of 3 s, 0.5 s and 0 s: <ul><li>one connected wait woke for request 2 under its own key (79 to 593 ms after it opened);</li><li>through the daemon, request 2 opened between waits and got its own key;</li><li>on the keeper drain path, a wait inside a round kept the request, and the next request reached the third wait;</li><li>a second `--no-daemon` wait without the hook counted the open request again, as documented.</li></ul> |
| X3b | Codex: a request opened and closed while nothing counts | real, isolated | **PASS** with the hook installed and trusted; limits below | Same run, `probe_missed.py`, with the printed hook block in the fixture's `CODEX_HOME` and the binary on a path with a space (quoted command): <ul><li>untrusted, the hook left no record for two requests;</li><li>with a wait counting throughout, nothing was recovered (`requestRecovery: "hook"`);</li><li>with nothing counting while two requests opened and closed, the next wait listed both in `recoveredRequests` and reported them once as `requests_recovered`; a later wait listed them without reporting them again;</li><li>with the daemon stopped after it counted request 1, a wait through a new daemon recovered request 2 only;</li><li>the log held `atMs`, `event`, `tool` and `turn` only.</li></ul> Mock: the recovery with and without the daemon, the review ordinal 21 regressions above, and six more hook tests |
| X4 | Codex: default socket through the official shared daemon | real, isolated | **PASS** | 06:39:30. Completion, then a pinned run 2 522 ms after `turn/completed`. The daemon was stopped and no LaunchAgent changed. `evidence/codex-daemon-socket-13fd055.txt` |
| X5 | Codex: the desktop thread on the MacBook, read only | real | **PENDING (parent) for 0.16.1**; PASS for 0.15.0 (parent-reported) | `dist/0.16.1/desktop-check.sh` against the research thread. 0.15.0 exited 0 at run ordinal 4 (`x5-desktop-check.out`; the thread's id is withheld from this file) |
| X6 | Codex: the bot reuses a thread and continues | real, production | **PENDING (parent) for 0.16.1**; PASS for 0.15.0 (parent-confirmed) | 0.15.0: correlation `cbbacb6e4f0d4d1d`, disposable thread `01a1be81-857d-7483-a375-daadd3d77217`, watchId `5a10216c5de0876f`, `RECEIVED-5a10216c5de0876f.json` |
| X7 | Codex: the hook in the user's own Codex (desktop app or CLI) | real | **PASS** (parent-confirmed, with 0.16.0) | On the user's MacBook: t3-watch 0.16.0 installed at `~/.local/bin/t3-watch`, the `PermissionRequest` async hook block from `--print-settings` added to `~/.codex/config.toml` and trusted by the user, and `desktop-check.sh` reporting `requestRecovery: "hook"` against Codex app-server 0.167.0. The user did the install and trust; t3-watch edited nothing. 0.16.1 keeps the same block and log line. Whether Codex asks for trust again when the binary at that path is replaced was not checked |

**X3b limits** (also in `docs/native-sessions.md`, "Limits"):

- recovery needs the hook installed and trusted by the user (X7); without
  it (`requestRecovery: "none"`) such a request is still not reported;
- user-input requests (`waitingOnUserInput`) have no Codex hook event and
  are not recovered;
- approvals Codex does not run `PermissionRequest` for are not recovered;
  only shell-command escalations were run here, while Codex's
  documentation also names `apply_patch`, MCP tools and managed-network
  approvals;
- a hook that is trusted but records nothing (a moved binary) loses
  requests that open and close unseen; a replaced open request then still
  gets the extra wake, never a missed one;
- a count that never gets a record (an approval without the hook, or one
  counted before the hook was installed) can take the record of one later
  request of the same turn that opened and closed unseen, until a request
  counted after it has had its record for 15 s or the turn ends;
- matching relies on Codex starting a session's background hooks in order
  and on the printed `timeout = 10`: a record more than 15 s out of order
  is reported as recovered although its request was seen (none was seen
  here);
- after the log is replaced, a record not newer than the newest one
  already read is taken as a copy (a clock step back could hide a new one);
- subagent threads (records under the parent's `session_id`) were not run;
- a request another hook allowed or denied is reported as recovered,
  though it never reached a person.

## Still open

1. **Review of this build.** No review has seen `78d3d74`..`13fd055`.
2. **The parent's gates for 0.16.1:** T1, T2, T3b, N4 and X6 with this
   build; T7 with `sh dist/0.16.1/linux.sh dist/0.16.1 13fd055` on the
   Grok Bot box; X5 with `dist/0.16.1/desktop-check.sh` on the MacBook.
3. **T5's evidence.** The ids and path of the 0.15.0 real-suspend run, to
   record them here. Until then T5 rests on the parent's report.

## History: the native Linux run of 0.14.0 (gate T7: FAIL, fixed in 0.15.0)

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
  this host. The parent ran them for 0.15.0 against its own receiver and
  bot routine, where the webhook URL and secret are; T7 and X5 ran on the
  Grok Bot box and the MacBook. For 0.16.1 they wait for the parent. X7 was done on the user's MacBook by the user, and confirmed by the parent.
- **The user's Codex** (`~/.codex`) was not read, used or changed: every
  hook check ran in a fixture `CODEX_HOME`, where the hook was trusted the
  way `/hooks` records it. Nothing was installed in the user's Codex.

## Side effects of these runs

- **Claude Code.** It recorded folder-trust answers for the disposable
  `/private/tmp/t3wcc*/proj` folders in `~/.claude.json` and the test
  prompts in `~/.claude/history.jsonl`; neither file was edited by hand.
  The sessions' `~/.claude/projects` and `~/.claude/session-env` entries
  were removed.
- **Codex.** Every app-server, daemon and mock started here was stopped,
  and their fixtures and sockets were removed. `~/.codex` was not used,
  and no real thread was read or touched. The mocks ran on port 18821,
  because the parent's X6 fixture still holds 18801; it was left alone.
