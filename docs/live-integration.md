# Live integration plan

Exact, read-only steps for running t3-watch against real T3 servers and a
real bot. Pairing approval and the bot wake are done by whoever holds those
credentials; t3-watch's exit code and a webhook's 2xx prove delivery to the
receiver, not that a bot acted on it. Nothing here sends a message to a
thread, answers a question or approves anything: the watcher's MCP client
refuses every tool except `t3_thread_wait`, `t3_thread_read` and
`t3_pending_request` with `action` `list` or `read`.

## 0. Binary

macOS: `cargo build --release --locked` gives `target/release/t3-watch`.
Linux: unpack the candidate archive and check its digest:

```sh
shasum -a 256 -c t3-watch-<ver>-x86_64-unknown-linux-gnu.tar.gz.sha256
tar -xzf t3-watch-<ver>-x86_64-unknown-linux-gnu.tar.gz
./t3-watch-<ver>-x86_64-unknown-linux-gnu/t3-watch --version
```

`t3-watch --version` prints the version, the engine revision and the
source revision, e.g. `t3-watch 0.7.0 (engine 9, rev 1a2b3c...)`; packaged
binaries always carry the commit they were built from.

Upgrading over a running older build (`a92f977` or `fa10d88`, engine 4;
`ac91a64` and other `0.3.0` builds, engine 5; `0.4.0`, engine 6; `0.5.0`,
engine 7; `0.6.0`, engine 8): this build never talks to their daemons
(sockets `daemon-e4.sock` to `daemon-e8.sock`, or older; on a shared
`T3_WATCH_SOCKET` it refuses them, by engine and by missing capabilities).
It keeps its entries in `outbox-v4/`, which engine-4 and engine-5 senders
never read; `0.4.0` to `0.6.0` use the same directory with the same rules.
Their daemons keep serving their own running clients and idle out
afterwards; to retire one sooner, run its own binary's `t3-watch stop` once
its clients are done. Entries older builds leave in `outbox-v2/` or
`outbox-v3/` are never moved: this build's daemon delivers them in place,
under their leases and sender lock, so a client of theirs that is still
running reports its event queued or delivered truthfully (README, "Durable
delivery", item 7). An `outbox-v2/` entry that expires or is dropped stays
where its engine-4 client looks, held back so that engine-4, engine-6,
engine-7 and engine-8 senders leave it alone too, until the trace shows
that client is done with it (item 8). Where an older build left a receipt
beside such an entry, this build takes it back behind a fence that keeps
every leased older sender out of `outbox-v2/` until it is done; their
senders then log `cannot open the sender lock: Is a directory` for that
store, which is expected. A `0.3.0` (engine 5) sender moves such entries
regardless; it was never released, `outbox list` and `status` report its
daemon at a default socket under `incompatibleSenders`, and it must be
stopped (and checked for on custom sockets and as a standalone `outbox
run`) before engine-4 clients can rely on retention. Entries in `outbox/`
(before engine 4) move over after the 90 s inline grace. All of them count
as pending until delivered or retained, so the daemon does not idle out
with an event unsent.

`<state>/trace.jsonl` records each wait, the daemon build that served it,
the first proof or settled observation (with T3's `completedAt`), the
payload, outbox entry and every HTTP attempt with its retry deadline, and
the exit code. To locate a late or missing wake, line it up with the run's
`completedAt` and the receiver's own log.

## 1. Local transport (no pairing), on the machine that runs T3

```sh
e2e/live.sh <completed top-level thread> <thread that stays running ~2 min> \
  target/release/t3-watch https://<relay-origin>
WAITING_THREAD=<thread blocked on input> e2e/live.sh ...   # adds the needs-input check
```

This checks completion, stale ordinals, `--after-run`, the local refusal of
`--until workstream`, a missing `--final-marker`, poller sharing,
cancellation and resource accounting, signed webhooks with Authorization and
Idempotency-Key, a webhook the receiver rejects (401, exit 4), heartbeats, a
durable delivery that succeeds after the receiver comes back, hook output
going to `hooks.log`, and strict OAuth discovery. Result on the Mac mini
(2026-11-06 Pacific, after the ordinal-3 review fixes): all 37 checks passed.

`e2e/receiver.py` is an audit fixture: it answers 200 only to correctly
signed and authorized requests (401 otherwise) and logs what it saw. A pass
shows what t3-watch sent; it is not evidence that a production receiver got
an event or acted on it (step 4).

## 2. Pair each remote machine (parent, once per machine)

```sh
t3-watch auth oauth-start https://prod-05055d8a66f766cb.t3coderelay.com --name mini
#   open authorizeUrl, approve as Krish, copy the URL of the page that fails to load
t3-watch auth oauth-finish 'http://127.0.0.1:8976/callback?code=...&state=...'
t3-watch auth list    # shows the machine, never the token
```

Repeat for the G14 relay origin with `--name g14`. The token has
`orchestration:read` only.

## 3. MCP checks against live threads (read-only)

Use thread URLs (`https://<relay>/<envId>/<threadId>`); they pick the saved
machine and the environment.

a. A single turn, turn mode (the default). Expect exit 0, `outcome`
   `completed`, and `delegatedWorkPending` true or false:

   ```sh
   t3-watch wait '<relay>/<envId>/<completed thread>' --timeout 2m --pretty
   ```

b. A thread blocked on input, such as the G14 thread at ordinal 25 with two
   questions. Expect exit 5, `event: needs_input`, `requests[]` with
   `kind: question`, `requestId`, the two questions (redacted), and a
   `pendingKey`. Do not answer; Krish has the card.

   ```sh
   t3-watch wait '<g14 relay>/<g14 envId>/mcp%3Alaunch%3A67e4871ff4ee1351ebc1238952799f2667134fa348528f10ee0d9f1120f70cd3' \
     --until workstream --timeout 5m --pretty
   ```

   Then `--acknowledge <pendingKey>` with `--timeout 2m` should time out
   (exit 3) while the same questions stay pending.

c. A delegating thread, whole workstream, with the production notification
   path to a controlled receiver (`e2e/receiver.py` or the bot's real
   endpoint):

   ```sh
   export BOT_URL=... BOT_SECRET=... BOT_AUTH='Bearer ...'
   t3-watch wait '<relay>/<envId>/<thread>' --until workstream --expect-min-run-ordinal <N> \
     --timeout 6h --notify-on all --durable --heartbeat 5m --notify-unreachable 2m \
     --webhook-url-env BOT_URL --webhook-secret-env BOT_SECRET \
     --webhook-header-env Authorization=BOT_AUTH > watch.json; echo "exit $?" >> watch.json
   ```

   Expect, in order at the receiver: heartbeats every 5 minutes, possibly
   `upstream_unreachable`/`upstream_restored` around tunnel trouble, and one
   `settled` event whose `threads[0].resultRunId` is the synthesis run (for
   the research incident, ordinal 6, not 2). `t3-watch outbox list` should be
   empty at the end; if the receiver was down, the entry is retried by the
   daemon until a 2xx.

d. Outage behavior, observed rather than induced: during a tunnel outage,
   `t3-watch status` shows each watch's `upstream` (`unreachableSince`,
   `consecutiveFailures`, `lastError`, for example
   `HTTP 401 without a bearer challenge ... Unauthorized: Tunnel not found`
   or `HTTP 530 (Cloudflare 1033 ...)`), and no wait ends as completed until
   T3 answers again.

## 4. Bot wake (parent)

The parent verifies that the bot acted on the `settled` or `needs_input`
event (for example, a reply in the bot's channel naming the thread), and
reports that separately from the watcher's exit code. A receiver should
drop repeats of an `X-T3-Watch-Id` it has handled, order a wait's events by
`eventSeq`, and a receiver-side monitor should alert when a watch's
heartbeats stop for about two intervals without a final event.

Do not rely on the exit of a background shell on another machine to wake a
bot: a remote (`machineId`) background shell can exit without waking the bot
that started it. The webhook to the bot's routine, or the exit of a process
the bot's own harness watches on its own machine, is the wake-up.

Status as reported by the parent (not re-verified from this worktree):

| Acceptance | Result |
| --- | --- |
| Real webhook from a box daemon to the bot's T3 Watch Wake routine, `settled` | Passed: watchId `c2366ae100a095fb` (Sol review run, ordinal 3), `endedAt` 2026-11-07T06:40:20.657Z, woke the correct bot with no user ping |
| `needs_input` wake through the same routine | Pending |
| Replay of a queued event after receiver downtime (`--durable`) | Pending |
| Native Linux run of the x86_64 / aarch64 archives | Pending |

## 5. Acceptance harness (completion, needs-input, durable replay)

The release gate is the correct bot's receipt and continuation, with
timestamps, without a user ping, separately for completion, needs-input and
durable replay. t3-watch can prove what it did; only the bot's own records
show what happened after. `e2e/timeline.py` joins the two:

```sh
# On the watcher host, after the event:
python3 -I e2e/timeline.py ~/.config/t3-watch/trace.jsonl --watch <watchId from the result> \
  --receipt <time the bot's routine logged the request> \
  --continuation <time of the bot's first message or action naming the thread> \
  --bot <bot name>
```

It prints each step (run `completedAt`, first proof or settled observation,
payload end, outbox entry, every HTTP attempt and retry deadline, the 2xx,
then the receipt and continuation you supply) with the time since
completion, and a JSON summary to record. The acceptance event is the
notification of the wait's final outcome (or `--id`); only its own 2xx
counts, and progress events are listed apart. If the final outcome had no
notification (a timeout without `--notify-on all`), the summary says the
acceptance event is missing. It verifies nothing by itself.

The result's `startedAt`, `endedAt`, `exitAt`, `elapsedMs` (suspended time
included), `elapsedAwakeMs`, `elapsedWallMs` and `suspendedMs` separate a
host that slept from a slow server or a slow receiver. Run them through
the timeline for the box incidents (a `--timeout 10m` watch that ran 18.5
minutes, a webhook 15.6 minutes after completion): a large `suspendedMs`
or `watch.restart` lines point at a sleeping host; none, at the server,
the network or the receiver. After a sleep longer than the budget the
right outcome is `timeout`, not a late completion, so watchers that must
wake a bot run with `--notify-on all`, as below.

Per scenario:

1. **Completion.** A workstream wait with `--durable --notify-on all`, as
   in step 3c. Record the summary with the bot's receipt and continuation.
2. **Needs-input.** A thread that asks a question (a test thread told to
   ask one; never answer it from the watcher). Expect exit 5, a
   `needs_input` event with the request id and question, and the bot
   continuing by relaying the question (or answering it, if it is a
   question the bot may answer; approvals stay with the user).
3. **Durable replay.** Make the receiver unreachable from the watcher host
   only (for example a firewall rule for its address), run a wait to
   completion, confirm `t3-watch outbox list` holds the event and the trace
   shows failed attempts, restore reachability, and confirm the daemon's
   `outbox.attempt` with a 2xx, then the bot's receipt and continuation.
   The event id is the same in every attempt.
4. **Native Linux.** Unpack the archive on the Linux host, check that
   `t3-watch --version` prints the expected `rev`, run `e2e/live.sh` there
   against that host's T3, then scenarios 1 to 3 with that binary.

## Investigation notes (2026-11-06)

### The watcher that exited at run 2

The research watcher used `--expect-min-run-ordinal 2` in what is now turn
mode, so it was satisfied by run 2's own completion while four reviewers it
had launched were still running. The reviewers' completions arrived as
parent runs 3 to 5 and the synthesis as run 6. That was the watcher doing
what it was asked, with no way to ask for more locally. Over MCP,
`--until workstream` now waits on T3's own delegated-work proof, and
`--final-marker` gives an explicit contract on either transport. Turn mode
now says `delegatedWorkPending: true` when it ends while delegated work
runs.

### The parent's v2 watcher: 5,084,044 ms elapsed vs a shell that ended at 18:26

Facts: v2 reported `elapsedMs` 5,084,044 (1 h 24 min 44 s). The thread
completed ordinal 4 at 14:04 PT. The outer shell log says the command ended
at 18:26 PT. 14:04 minus 1 h 24 m 44 s is 12:39:16; 18:26 minus the same is
17:01:16. The mini's tunnel recovered at about 18:24 PT. These numbers alone
do not identify a cause. Candidates, and what would tell them apart:

1. **v2's `elapsedMs` counted only the last daemon registration.** In v2,
   `elapsedMs` came from the daemon, and a client that lost its daemon
   reconnected and re-registered with a fresh clock. If that happened at
   about 17:01, the wait could have ended at 18:26 with 84.7 minutes on the
   last registration. Evidence: `daemon.log` in that watcher's state
   directory (each start is logged with a millisecond timestamp), and
   whether 17:01 matches a daemon start. This build reports the client's own
   `elapsedMs`, `startedAt`, `endedAt`, `daemonReconnects` and
   `registrationElapsedMs`, so the next occurrence answers this directly.
2. **The wait ended at about 14:04 and the shell did not.** A compound
   command after the watcher (`; curl ...`, a retry loop, a sleep), or the
   harness recording "ended" when it next collected output. Evidence: the
   exact outer command and the harness's own start and end records; this
   build's `endedAt` shows when t3-watch itself finished.
3. **An inherited pipe kept the shell open.** In v2, an `--on-settle` hook's
   stdout and stderr were t3-watch's stderr; a background process started by
   the hook kept that pipe open, and a harness waiting for EOF would see the
   command "running" until the process exited. Evidence: whether v2 ran with
   `--on-settle`, and the process tree at the time. This build sends hook
   output to `hooks.log`, so a hook can no longer hold the caller's pipes.
4. **The watcher only saw the completion after the outage.** If v2 watched
   over the mini's relay and the tunnel was down until about 18:24, a wait
   that started (or re-registered) at about 17:01 would settle at about
   18:25. Evidence: v2's `lastError` or the daemon's `retries` count, and
   whether v2 used the MCP transport at all. This build reports `upstream`
   per thread and supports `--notify-unreachable`.
5. **Clock or time zone mismatch** between the machine that wrote the shell
   log and the T3 server. Evidence: both clocks' UTC offsets at the time.

Not established: which of these happened. The legacy
`engine_watch_then_post.sh` incident is separate and does not explain it: that
watcher ran from about 13:41 to its 14,400 s timeout at about 17:41 and then
failed its single `curl` on DNS (`Could not resolve host: api2.cursor.sh`).
`--durable` is the fix for that class of loss.
