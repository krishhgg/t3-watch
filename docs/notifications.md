# Notifications

## Wake notifications

An agent whose harness watches the background command on the same machine
is woken by the process exiting. That is not universal: a background shell
started on another machine through a remote tool (a remote `machineId`
shell, for example) can exit without waking the bot that started it. In
those setups, and for a bot without a background terminal at all, have
t3-watch call the bot instead. The waiting client (never the shared daemon)
delivers the notification after the wait ends and before it exits.

```sh
export BOT_HOOK_URL='https://bot.example/hooks/t3'      # keep URLs and secrets out of argv
export BOT_HOOK_SECRET='...'
t3-watch wait mcp:launch:abc123 \
  --webhook-url-env BOT_HOOK_URL --webhook-secret-env BOT_HOOK_SECRET \
  --webhook-header-env Authorization=BOT_HOOK_AUTH \
  --on-settle /usr/local/bin/wake-bot --on-settle-arg --channel --on-settle-arg ops
```

- The webhook gets `POST` with the result JSON as the body.
  `X-T3-Watch-Event` is the event (`settled`, `needs_input`, `timeout`,
  `error`, or progress: `run_failed`, `upstream_unreachable`,
  `upstream_restored`, `requests_recovered`, `heartbeat`). `X-T3-Watch-Id` and `Idempotency-Key`
  carry the same id on every retry of one event, inline or from the outbox;
  `X-T3-Watch-Attempt` counts the attempts. With a secret,
  `X-T3-Watch-Timestamp` and `X-T3-Watch-Signature: sha256=<hex>` carry an
  HMAC-SHA256 over `<timestamp>.<id>.<body>`, fresh for each attempt.
- The URL must be `https://` (plain `http://` only on 127.0.0.1, [::1] or
  localhost). Redirects are not followed. Network errors, 408, 429 and 5xx are
  retried, at most 4 attempts within 60 s, honoring a `Retry-After` in seconds;
  if that would overrun the 60 s, delivery stops. Other 4xx fail at once.
- `--webhook-url-env`/`-file`, `--webhook-header-env` and
  `--webhook-secret-env`/`-file` read their values before the daemon or any
  hook starts, and the variables are removed from the environment so no child
  process inherits them. `--webhook-url` also works but puts the URL in `ps`.
  A `-file` must be a regular file owned by you with no group or other
  access (`chmod 600`); a symlink is followed and its target checked, and a
  FIFO or device is refused without blocking.
  Header names and values with CR, LF or NUL are refused. Output reports
  attempts, status and an error category only, never the URL, headers, secret
  or response body.
- `--on-settle` runs the program directly (no shell) with the
  `--on-settle-arg` values, the result JSON on stdin and `T3_WATCH_EVENT` in
  its environment. Its stdout and stderr go to `hooks.log` in the state
  directory (0600), never to t3-watch's stdout or stderr: a hook that leaves
  a background process holding an inherited pipe would otherwise keep a
  harness that reads t3-watch's output waiting after t3-watch has exited.
  After `--on-settle-timeout` (default 60 s) its whole process group is
  killed.
- `--notify-on` takes `all` or a comma list of `settled`, `needs_input`,
  `timeout`, `error`, `run_failed`, `upstream` and `recovered` (Codex
  requests found in the hook log). The default is everything
  but `timeout` and `error`; `all` adds those, so an expired token cannot
  leave an unattended bot waiting forever. Once the notification options
  themselves are valid, this includes errors before the wait starts: an
  unreadable machines.json, an unknown machine, a bad `--mode` or duration.
  A bot that relies on the webhook to wake it should run with `--notify-on
  all`: after a long suspend the honest outcome can be `timeout` (the
  budget counts suspended time), and under the default nothing would wake
  it.
- When the threads completed but a notification failed (or was only
  queued), the result adds `"notification":{"delivered":false,...}` and the
  exit code is 4. Exit codes 5 and 6 are kept as they are; check
  `notification.delivered` there.

## Durable delivery

The legacy shell watcher this replaces timed out, then lost its only
notification to a DNS failure (`Could not resolve host`) with no retry.
With `--durable`, a webhook is never fire-and-forget:

1. Before the first attempt, the event is written to
   `<state>/outbox-v4/<id>.json` (0600 in a 0700 directory, through an
   fsynced temp file and a rename). A crash from here on loses nothing.
2. The waiting client tries it inline (up to 4 attempts within 60 s) and
   records the result the moment the webhook is done, even while an
   `--on-settle` hook beside it is still running. A 2xx removes the entry.
3. Otherwise the client exits (4 if the threads completed) with
   `notification.queued: true` and the outbox id, and makes sure the daemon
   runs. The daemon's sender retries every entry with exponential backoff
   (2 s doubling, with jitter, capped at 5 min) until a 2xx, then removes
   it. A receiver's `Retry-After` (in seconds) is a floor, not part of the
   backoff: it is saved with the entry as `notBefore`, across the hand-off
   from the client to the sender, the 5-minute cap never shortens it, and
   even `outbox run --once` waits for it. DNS failures, refused
   connections, 5xx and 4xx alike are retried; after `--durable-max-age`
   (default 7 days) the entry moves to `outbox-v4/dead/`.
4. The daemon does not idle-exit while it has entries to send: this
   build's, an older build's it may deliver (item 7), and receipts it
   still has to take back. Entries it may not touch (a blocked
   `outbox-v2/`, item 8), retained entries and dead letters do not keep it
   up, and an entry stops being retried once it expires, delivered or
   not. It also stays up while it keeps the connection of a native Codex
   watch that ended mid-turn (at most 15 minutes, and only until that
   turn ends; `status` shows `codexKept`, docs/native-sessions.md). A
   daemon that is killed, or a host that is off or asleep, sends
   nothing until a t3-watch command starts the daemon again (any `wait`
   does) or a supervised `t3-watch outbox run` is running. One sender runs
   at a time (`outbox-v4/sender.lock`).
5. Each entry has a lease (an flock on `<id>.lock`). Whoever sends it, the
   client inline or the sender, holds the lease for the attempt and for
   recording its result, and rereads the entry first: the two never send
   one entry at once, and neither writes over the other's newer state
   (`outbox run --once` reports a leased entry as `inFlight`). A process
   that dies drops its lease, so the sender takes over at once.
6. How an entry leaves is written down, never inferred. Before an entry is
   removed (`delivered`, `dropped`) or moved to `dead/` (`expired`), a
   receipt `outbox-v4/receipts/<id>.json` (kept 7 days) records the
   disposition, under the entry's lease. A client that finds its entry gone
   reports what the receipt says: only `delivered` counts as delivered; an
   expired, dropped, unreadable or receipt-less entry is
   `"delivered": false` with its `disposition` (`expired`, `dropped`,
   `unreadable`, `unknown`), and a completed wait then exits 4. A receipt
   is final: whoever holds an entry's lease checks for one before sending,
   and an entry that survived beside its receipt (a crash between the two
   writes) is only cleaned up as the receipt says, never sent, and the
   receipt is never rewritten.
   A receipt counts only if it names the entry's id and a known
   disposition. Anything else (`{}`, another id, an unknown disposition,
   not JSON) is reported as `receiptError` and ignored: the entry stays
   pending and is sent as usual, and its real outcome replaces the bad
   receipt.
7. Older builds keep their own outboxes, which this build's senders read
   but theirs never see. Engine-4 builds (`0.2.0` up to `fa10d88`) use
   `outbox-v2/` and engine-5 builds (`0.3.0`) `outbox-v3/`. Their entries
   are never moved: a client of theirs that found its entry gone would
   report the event delivered, and no amount of age proves that client is
   finished (a slow progress hook can hold it back for minutes). This
   build delivers them where they are (`outbox-v2/` only once the upgrade
   precondition in item 8 holds), as their own senders would: under
   the entry's lease and that store's `sender.lock`, after its next attempt
   time, counting each attempt as a sender retry. An old client that gets
   back to its entry then finds it either still there with a retry recorded
   (and reports it queued, not delivered) or gone after a real 2xx, with a
   receipt in that store. Builds before engine 4 used `outbox/`, without
   leases; their clients only report delivery on their own 2xx, so those
   entries move here after the 90 s inline grace, under that build's
   `outbox/sender.lock`. Until delivered or expired, those this build may
   send count as pending: `outbox list` (under `legacy`, with their store),
   `status` (`legacyPending`), the exit code of `outbox run --once`, and
   the daemon, which stays up while any remain. Entries of a blocked
   `outbox-v2/` (item 8) are counted apart (`legacyBlocked`) and do not keep
   it up.
8. An older build's entry can also leave undelivered: it expires, or
   `outbox drop` drops it (`drop` finds entries in `outbox-v2/` and
   `outbox-v3/` too). Engine-5 clients read the receipt, so an
   `outbox-v3/` entry leaves as here. Engine-4 clients read a missing entry
   as delivered whatever the receipt says, so an `outbox-v2/` entry stays
   where it is, in a state such a client reads as queued and older senders
   leave alone:
   - a sender retry is recorded, so an engine-4 client reports the event
     queued and does not send it itself;
   - it is not expired, and its `notBefore` and next attempt are
     2100-01-01, so engine-4 senders (which check expiry first, then
     `notBefore`, even on `--once`) and engine-6 and engine-7 senders defer
     it instead of dead-lettering or sending it;
   - no receipt sits beside it (engine-6 and engine-7 senders clean up an
     entry beside a receipt): how it left and its real expiry are kept in
     the entry, as `retainedAs` and `retainedExpiresMs`.

   It goes only once the delivery trace, which engine-4 builds write too,
   shows its client is done with it: that client's `notify.result` for the
   entry, or the process that created it no longer running. Nothing else
   counts, not age. It then leaves as it would have, with its real expiry,
   a receipt, and (expired) a move to `dead/`. Until then it is listed under
   `retained` in `outbox list` and as `legacyRetained` in `status`, and is
   not pending. Without trace lines for it (a trace that was not writable,
   or rotated away), it stays until you remove it yourself once no engine-4
   client can be running.

   An older build may have left a receipt beside such an entry (a `0.5.0`
   sender keeps it that way, and any build can crash between writing a
   receipt and removing the entry). An engine-6 sender removes an entry
   beside a receipt, so the entry is held and the receipt taken back, in
   that order. No build can keep an older sender out of `outbox-v2/` across
   its own crash: its locks die with it, and a sender that already opened
   the lock file keeps it. So instead each step leaves the entry no more
   exposed than before to engine-4, engine-6, engine-7 and engine-8
   senders: holding it ends what an engine-4 sender would do (dead-letter
   it once expired), and taking the receipt back ends what an engine-6
   sender would do. No step protects it from an engine-5 sender, which
   moves `outbox-v2/` entries whatever their state, held ones included;
   the upgrade precondition (item 8) requires engine-5 senders stopped
   before this build touches `outbox-v2/` at all. Each pass
   holds every entry that must be kept (one write each) before it takes
   any receipt back, so a pass cut short never leaves an entry it had not
   reached yet for an engine-4 sender to dead-letter. A step
   that fails (a save, or a receipt that cannot be removed) is reported as
   `conversionError` with what the entry is still exposed to, counts as
   pending (`outbox run --once` exits 4; `status` shows
   `legacyConversionsIncomplete`), and the next pass repairs what is left,
   including a held entry with its receipt still beside it.

   What this build cannot change is what older senders do on their own:
   an engine-5 build (`0.3.0`, never released) moves `outbox-v2/` entries
   whatever their state, minding only the entry lease (and moves a held
   entry, whose recorded retry ends its 90 s grace); an engine-6 build
   (`0.4.0`) removes an entry beside a receipt; and an engine-4 build's own
   sender dead-letters an entry that expires before this build has held it.
   So this build **leaves `outbox-v2/` untouched** (no lock file, hold,
   delivery, receipt or fence change) until both of these hold:
   - an operator has run `t3-watch outbox legacy-ack --senders-stopped`,
     stating that no engine-5 or engine-6 sender (a daemon, one on a custom
     `T3_WATCH_SOCKET`, or an `outbox run` without a daemon) runs with
     access to this state directory; `--revoke` withdraws it;
   - no engine-5 or engine-6 daemon answers at a default socket
     (`$XDG_RUNTIME_DIR/t3-watch/`, or the state directory, which is also
     checked). A daemon counts as alive if it holds its socket lock
     (`<socket>.lock`): engine-5 and engine-6 daemons take it before they
     bind and keep it until they exit, so one that has stopped accepting is
     found by the lock alone. As a fallback, a connect that succeeds or is
     queued counts too. That fallback cannot see a listener whose backlog is
     full: macOS refuses the connect (`ECONNREFUSED`).
     One that is alive refuses even an acknowledged store, and refuses the
     acknowledgement itself.

   Until then `outbox run --once`, `outbox list`, `status` and `outbox
   drop` on such an entry fail closed: exit 4 (`drop`: 2) with
   `upgradePrecondition` saying why (`status` under `outbox`, with or
   without a running daemon). The entries are counted as `legacyBlocked`
   and do not keep the daemon up, and the daemon logs the refusal once. The absence of those sockets is never taken as
   enough: a custom-socket daemon or an `outbox run` cannot be seen from
   here. A busy `outbox-v2/` lock is an error too, since the sender holding
   it may be an older build's. An entry engine 5 moved, still held to
   2100, is finished as expired wherever it ends up. A `sender.lock`
   directory left by `0.7.0` stays up, keeping every leased older sender
   out, until this build has held every entry that needs it and taken back
   every receipt; each entry's hold and receipt removal happen under one
   lease. Only then does it come down.

Every attempt for an entry carries the same `X-T3-Watch-Id` and
`Idempotency-Key`. A receiver that processed a request whose answer was lost,
or a sender that crashed between the 2xx and removing the entry, produces a
duplicate with the same id; receivers should drop repeats of an id they
have seen.

Events of one wait can arrive out of order (a queued progress event may be
retried after the final event lands), so each body carries `eventSeq`
(1, 2, ... per wait, progress and final alike) and `watchId`. Progress
events are written to the outbox as they happen, before anything is logged,
and the final event is written before the client waits for any progress
delivery still in flight. Diagnostics on stderr are best effort: a closed
stdout or stderr never changes the exit code or costs an event.

```sh
t3-watch outbox list            # ids, events, destination origin, attempts, next attempt, last error
t3-watch outbox run --once      # try every entry now, Retry-After permitting (exit 4 while some remain)
t3-watch outbox run             # the sender in the foreground, for launchd/systemd
t3-watch outbox drop <id>
t3-watch outbox legacy-ack --senders-stopped   # see item 8: allow work on outbox-v2/
```

An entry holds the webhook URL, extra headers and signing secret, protected
like the OAuth tokens in machines.json; `outbox list` shows only the
destination's origin. If the machine reboots, nothing resends until a
t3-watch command starts the daemon again (any `wait` does), unless
`t3-watch outbox run` is supervised by launchd or systemd.

## Outages, heartbeats and a dead host

t3-watch separates what it can prove from what only someone else can:

- **T3 unreachable from the watcher.** Network errors, 5xx, Cloudflare 530
  / 1033 and a 401 or 403 without a `WWW-Authenticate: Bearer` challenge
  (what a tunnel answers while its origin is away, for example
  `Unauthorized: Tunnel not found`) are an outage, not an auth failure. The
  watcher keeps retrying, and since only a fresh proof can settle a wait, an
  outage never ends one as completed. Each thread in the result shows
  `upstream: {reachable: false, unreachableSince, unreachableForMs,
  consecutiveFailures, lastError}` while it lasts, `t3-watch status` shows
  the same per watch, and `--notify-unreachable <dur>` sends
  `upstream_unreachable` progress once an outage has lasted that long and
  `upstream_restored` when it ends.
- **The watcher (or its host) is gone.** A dead or powered-off host cannot
  report its own death. `--heartbeat <dur>` POSTs a signed `heartbeat`
  event every `<dur>` with the `watchId`, host, pid, `seq`, start time and
  each watch's upstream state; the final result carries the same `watchId`.
  A receiver-side monitor that has seen no heartbeat for about two intervals
  and no final event should treat the watcher as gone and restart it
  elsewhere. Heartbeats are best effort and never queued: a missing one is
  the signal.

## Delivery trace

Every wait, the daemon and the outbox sender append one JSON line per step
to `<state>/trace.jsonl` (0600, rotated at 4 MiB): `wait.start`,
`daemon.start`, `daemon.accepted` (or `daemon.incompatible`) with the
daemon's build, `watch.settled` or `watch.proof` (the first time a watch
saw the run settled or proven, next to T3's `completedAt`),
`wait.outcome`, `notify.prepared`, `outbox.created`, every
`notify.attempt` and `outbox.attempt` with its HTTP status, error category
and `Retry-After`, `outbox.queued` with the next attempt time,
`notify.result`, `outbox.delivered` and `wait.exit` with the exit code.
Every line carries `at`, `pid`, `engine` and `rev`, and ids, statuses and
times only: never a URL, header, secret or message text. Threads appear as
parsed ids, never as the arguments given (a thread URL's query can carry a
token), and messages that echo an argument drop its userinfo, query and
fragment. Tracing never blocks or fails a wait: the trace, `hooks.log` and
`daemon.log` are opened non-blocking and only written when they are
regular files owned by you, so a FIFO or device at one of those paths
costs log lines, nothing else. When a wake comes late or never, the trace
shows how far the watcher got and when, even if its stdout went nowhere.
`e2e/timeline.py <trace.jsonl>` prints one wait's steps with the time since
the run completed. It judges one acceptance event, the notification of the
wait's final outcome (matched by the `wait.outcome` line's event and
`eventSeq`), or `--id`, by that event's own 2xx; progress events are listed
apart and never stand in for it. When the final outcome had no
notification (a timeout under the default `--notify-on`), it reports the
acceptance event as missing and not delivered.
