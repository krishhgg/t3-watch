---
name: t3-code-watch
description: Wait for T3 Code threads to finish without spending tokens. Use after launching or messaging a T3 thread (t3_thread_launch, t3_thread_send, delegate_task) when you would otherwise poll t3_thread_read or keep a subagent open. Starts `t3-watch wait` as a background command, ends the turn, and resumes when the command exits with one JSON line that says completed, failed, needs input, timeout or error.
---

# Watching T3 Code threads with t3-watch

`t3-watch wait` blocks in the operating system until the threads you name
finish, fail, or stop to ask for input. It then prints one JSON object and
exits with a code that says which. Your harness wakes you when the
background command exits (on the machine it watches; see Harness recipes),
so you spend no tokens while the thread works.

## When to use it

- You started a long T3 thread and have nothing else to do until it finishes.
- You sent a follow-up turn with `t3_thread_send` and need its result.
- You are fanning out several threads and want to resume when all of them (or
  the first one) finish.

Skip it when the thread is already done or when you have other useful work
to do in the meantime. Check back later instead.

## Steps

1. Launch or message the thread and note its `threadId` (or its full T3
   thread URL) and the run ordinal of the turn you care about. A run id looks
   like `run:thread:<id>:ordinal:2`, so that turn's ordinal is 2.
2. Pick the contract (next section), start the watcher as a background
   command and end your turn right away. Do not poll it.

   ```sh
   t3-watch wait <threadId-or-url> --expect-min-run-ordinal 2 --timeout 2h
   ```

3. When the command exits, act on the exit code and `outcome` (table below).

## Turn or whole workstream

A turn that delegates work ends long before that work does: the children's
results come back as new runs of the parent, and the final synthesis is the
last of them. Choose what "done" means:

- `--until turn` (default): the latest run finished. Right for a thread that
  does its work in one turn. If the finished turn left delegated work
  running, the result says `delegatedWorkPending: true` with a diagnostic.
- `--until workstream` (MCP only: a paired machine or a thread URL): the run,
  all its delegated work, and the parent runs that deliver the results have
  settled, as proven by T3 itself. Use it whenever the thread delegates
  (reviewers, subagents, background tasks) and you need the final answer.
  For a local thread id it exits 2 at once; pair the machine instead.
- `--final-marker <TEXT>` adds an explicit contract for either mode: tell the
  thread to end its final answer with TEXT, and the wait only completes once
  that run's final assistant message contains it. It works locally too, but
  not on a delegated child thread (watch the parent).

## Always pass the run you want after a follow-up

Right after `t3_thread_send`, the thread may still report the previous turn
as `completed`. Pass `--expect-min-run-ordinal <N>` for the run you are
waiting for, or `--after-run <runId>` with the last run id you already
handled (from the previous result's `latestRunId` or `resultRunId`). If the
run may fail and be resent, add `--follow`: a failed run is reported as
`run_failed` progress and the same watcher follows the resend.

## Reading the result

```json
{"event":"settled","outcome":"completed","until":"workstream","elapsedMs":81234,"via":"daemon",
 "threads":[{"machine":"mini","threadId":"mcp:launch:...","status":"completed","outcome":"completed",
             "settled":true,"latestRunId":"run:thread:...:ordinal:6","runOrdinal":6,
             "resultRunId":"run:thread:...:ordinal:6","delegatedSettled":true,"title":"...","link":"..."}],
 "startedAt":"...","endedAt":"...","watchId":"..."}
```

| Exit | `outcome` | What to do |
| --- | --- | --- |
| 0 | `completed` | Read the thread (`t3_thread_read`) and continue. |
| 6 | `failed` | A run ended `failed`, `interrupted`, `cancelled`, `stopped` or `error`. Read it, decide whether to resend, then wait with `--after-run <latestRunId>`. |
| 5 | `needs_input` | A question or approval blocks the thread (see below). |
| 3 | `timeout` | Nothing finished in time. `upstream.reachable: false` means T3 was unreachable from the watcher, not that the thread is stuck. Re-run if you still want to wait. |
| 2 | `error` | `error.kind` says why: `auth` (pair again), `not_found` (wrong id), `unsupported` (the server cannot prove a workstream, or a marker on a child thread), `usage`, `config` or `limit`. Do not retry blindly. |
| 4 | `completed` | Completed, but the webhook or `--on-settle` program failed or is only queued; `notification` says which. Continue as for 0. |

## When it needs input

Exit 5 means the thread waits on someone, even if its status still says
`running`. Each entry in `threads[].requests` has `kind`, `requestId`,
`userOnly` and, over MCP, the question text with options (credentials
redacted).

- `kind: "question"`, `userOnly: false`: you may answer it with
  `t3_pending_request` (`action: "respond"`) if you know the answer, or pass
  it to the user.
- `kind: "approval"`, `userOnly: true`: only the user can approve, in T3.
  Tell them; never try to approve it yourself.
- To keep waiting while someone else handles it, run the same wait again
  with `--acknowledge <pendingKey>` from the result. A new request wakes you
  again.

t3-watch itself never answers or approves anything.

## Choosing the target

| You have | Pass |
| --- | --- |
| A thread on this machine | the bare thread id (local transport, no setup) |
| A relay URL `https://prod-....t3coderelay.com/<envId>/<threadId>` | the URL as is; it picks the saved machine and environment |
| A `t3-thread://v2/<envId>/<threadId>` link | the link, plus `--machine <name>` if it is not on the default machine |
| Several threads | all of them; add `--mode any` to resume on the first one |

Remote machines need a one-time read-only pairing (`t3-watch auth login
<relay-origin>`, or `oauth-start` and `oauth-finish` on a headless box). The
token only has `orchestration:read`, and t3-watch only calls read tools, so
the watcher can never send messages, answer questions or run anything.

## Harness recipes

The exit of a background command wakes you only when your harness watches
that process on the machine it runs on. A background shell started on
another machine through a remote tool (a remote `machineId` shell) can exit
without waking you; there, and anywhere you are unsure, use a webhook (see
below) as the wake-up and treat the exit as a backup.

Claude Code: run it with the Bash tool and `run_in_background: true`, then end
the turn. You get a notification when it exits.

Cursor, Codex and other agents with background terminals: start it as a
background or detached command and stop. Read the output when the terminal
reports completion.

Grok Bot or any shell-driven bot: run it as a background shell job and hook
its completion:

```sh
t3-watch wait "$THREAD" --until workstream --expect-min-run-ordinal "$ORD" > /tmp/watch.json; notify-bot /tmp/watch.json
```

Shell scripts: `t3-watch wait a b c && echo "all done"`. Only a completed
wait exits 0, so `&&`, `||` and `set -e` do the right thing.

Bots with no background terminal (a chat bot behind a webhook, a cron job):
let t3-watch call them, durably. Keep the URL and secret in the environment,
not in argv:

```sh
BOT_URL=https://bot.example/hooks/t3 BOT_SECRET=... \
  t3-watch wait "$THREAD" --until workstream --notify-on all --durable --heartbeat 5m \
    --webhook-url-env BOT_URL --webhook-secret-env BOT_SECRET &
```

The bot receives the same JSON in a POST, signed with
`X-T3-Watch-Signature: sha256=HMAC(secret, "<X-T3-Watch-Timestamp>.<X-T3-Watch-Id>.<body>")`.
`X-T3-Watch-Event` is `settled`, `needs_input`, `timeout`, `error`, or
progress (`run_failed`, `upstream_unreachable`, `upstream_restored`,
`heartbeat`). With `--durable`, an undelivered event is kept on disk and the
daemon retries it until a 2xx (never before a `Retry-After` you sent); drop
repeats of an `X-T3-Watch-Id` (also sent as `Idempotency-Key`) you have
already handled, and order a wait's events by `eventSeq`, since a retried
event can arrive after a later one. With `--heartbeat`, a
receiver that stops getting heartbeats for about two intervals (and got no
final event) should assume the watcher or its host is gone: a dead host
cannot notify anyone. `--on-settle <program> --on-settle-arg <arg>` runs a
local program with the JSON on stdin instead; its output goes to `hooks.log`
in the state directory. Files given to `--webhook-url-file` or
`--webhook-secret-file` must be yours and `chmod 600`.

## Cost model

Every `t3-watch wait` on a machine shares one background daemon. Twenty agents
watching the same thread cost one upstream long-poll (remote) or one `t3
thread show` per interval (local), not twenty. The daemon exits by itself 60
seconds after its last waiter leaves, unless queued notifications remain.
Use `t3-watch status` to see what it is watching and `t3-watch outbox list`
for queued notifications.
