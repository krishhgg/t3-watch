#!/usr/bin/env bash
# Live end-to-end checks against the T3 Code server on this machine, through
# the real daemon and the local transport. Read-only on T3: it only runs
# `t3 thread show`. Webhooks go to e2e/receiver.py on 127.0.0.1, an audit
# fixture that enforces the signature and Authorization header; passing here
# is not evidence that a production receiver got or acted on an event.
#
# Usage: e2e/live.sh <settled-thread-id> <running-thread-id> [t3-watch binary] [relay origin]
#   settled-thread-id  a top-level thread that has completed
#   running-thread-id  a thread that stays running for about two minutes
#   WAITING_THREAD=<id> optionally, a thread blocked on a question or approval
#
# Prints PASS/FAIL per check and exits non-zero if any check failed. Leaves its
# work directory (printed at the end) for inspection; remove it when done.
set -uo pipefail

SETTLED=${1:?settled thread id}
RUNNING=${2:?running thread id}
BIN=${3:-target/release/t3-watch}
RELAY=${4:-}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")
# Short: the daemon socket lives here and Unix socket paths are limited.
WORK=$(mktemp -d /tmp/t3w-e2e.XXXX)
export T3_WATCH_HOME=$WORK/home
export T3_WATCH_IDLE_EXIT=120s
mkdir -m 700 "$T3_WATCH_HOME"
FAILED=0

j() { python3 -c "import json,sys; v=json.loads(sys.stdin.read() or 'null'); print(eval(sys.argv[1]))" "$1"; }
check() { # check <name> <condition-command...>
  local name=$1; shift
  if "$@"; then echo "PASS  $name"; else echo "FAIL  $name"; FAILED=1; fi
}
threads_of() { if [ -d /proc ]; then ls /proc/"$1"/task | wc -l; else ps -M -p "$1" | tail -n +2 | wc -l; fi; }
fds_of() { if [ -d /proc ]; then ls /proc/"$1"/fd | wc -l; else lsof -n -P -p "$1" 2>/dev/null | awk 'NR>1 && $4 ~ /^[0-9]/' | wc -l; fi; }
not_contains() { local text=$1; shift; for n in "$@"; do case $text in *"$n"*) return 1 ;; esac; done; }
t3_shows() { pgrep -f "thread show --base-dir .* $RUNNING" | wc -l; }
events() { python3 -c "import json,sys; print([json.loads(l)['event'] for l in open(sys.argv[1])])" "$1"; }
start_receiver() { # start_receiver <log> <portfile> [port]
  python3 "$HERE/receiver.py" "$WORK/secret" "Bearer e2e-auth" "$1" "$2" ${3:-} & RECV=$!
  for i in $(seq 50); do [ -s "$2" ] && break; sleep 0.1; done
}

echo "== t3-watch $("$BIN" --version | cut -d' ' -f2) at $BIN"
echo "== work dir $WORK"

# 1. A finished thread completes through the daemon (turn mode).
out=$("$BIN" wait "$SETTLED"); code=$?
echo "   $(echo "$out" | j '[v["event"], v["outcome"], v["threads"][0]["status"], v["threads"][0]["runOrdinal"], v["via"], str(v["elapsedMs"]) + "ms"]')"
check "completed thread exits 0 via the daemon" [ "$code" = 0 ]
check "outcome is completed, contract is turn" [ "$(echo "$out" | j 'v["outcome"] + "/" + v["until"]')" = completed/turn ]
check "result has startedAt, endedAt and watchId" [ "$(echo "$out" | j 'all(k in v for k in ("startedAt","endedAt","watchId"))')" = True ]
ORD=$(echo "$out" | j 'v["threads"][0]["runOrdinal"]')
LATEST=$(echo "$out" | j 'v["threads"][0]["latestRunId"]')
DPID=$("$BIN" status | j 'v["pid"]')
sleep 1
T0=$(threads_of "$DPID"); F0=$(fds_of "$DPID")
echo "   daemon pid $DPID baseline: $T0 threads, $F0 fds"

# 2. A newer run than the thread has keeps waiting (exit 3), by ordinal or by run id.
out=$("$BIN" wait --timeout 3s --expect-min-run-ordinal $((ORD + 1)) "$SETTLED"); code=$?
check "stale run ordinal waits until the timeout (exit 3)" [ "$code" = 3 ]
out=$("$BIN" wait --timeout 3s --after-run "$LATEST" "$SETTLED"); code=$?
check "--after-run <latest run> waits until the timeout (exit 3)" [ "$code" = 3 ]

# 3. Workstream waits are refused on the local transport (fail closed).
out=$("$BIN" wait --until workstream "$SETTLED"); code=$?
check "--until workstream on a local thread exits 2" [ "$code" = 2 ]
out=$("$BIN" wait --until-delegated-settled "$SETTLED"); code=$?
check "--until-delegated-settled on a local thread exits 2" [ "$code" = 2 ]

# 3b. A final marker that the last message does not contain keeps waiting.
out=$("$BIN" wait --timeout 3s --final-marker "t3w-e2e-marker-$RANDOM$RANDOM" "$SETTLED"); code=$?
check "a missing --final-marker keeps waiting (exit 3)" [ "$code" = 3 ]
check "finalMarkerSeen is false" [ "$(echo "$out" | j 'v["threads"][0].get("finalMarkerSeen")')" = False ]

# 4. Five waits on a running thread share one poller; killed ones leave.
pids=()
for i in 1 2 3 4 5; do "$BIN" wait --interval 1s --timeout 8s "$RUNNING" > "$WORK/w$i.json" 2>/dev/null & pids+=($!); done
sleep 3
st=$("$BIN" status)
echo "   status while 5 wait: $(echo "$st" | j '[len(v["watches"]), sum(w["subscribers"] for w in v["watches"]), v["upstreamOps"]]')"
check "five waits share one watch" [ "$(echo "$st" | j 'len(v["watches"])')" = 1 ]
check "at most one t3 call in flight" [ "$(echo "$st" | j 'v["upstreamOps"]["live"]')" -le 1 ]
kill "${pids[0]}" "${pids[1]}" "${pids[2]}"
sleep 1
check "killed clients are unsubscribed" [ "$("$BIN" status | j 'sum(w["subscribers"] for w in v["watches"])')" = 2 ]
codes=""; for p in "${pids[3]}" "${pids[4]}"; do wait "$p"; codes="$codes$?"; done
check "the remaining waits time out (exit 3) while the thread runs" [ "$codes" = 33 ]
sleep 2
st=$("$BIN" status)
echo "   status after: $(echo "$st" | j '[len(v["watches"]), v["upstreamOps"]]')"
check "no watch left" [ "$(echo "$st" | j 'len(v["watches"])')" = 0 ]
check "no upstream operation left" [ "$(echo "$st" | j 'v["upstreamOps"]["live"]')" = 0 ]
check "no t3 thread show process left" [ "$(t3_shows)" -eq 0 ]

# 5. Waits that give up mid-call cancel their t3 process.
before=$("$BIN" status | j 'v["upstreamOps"]["cancelled"]')
for i in $(seq 10); do "$BIN" wait --interval 100ms --timeout 150ms "$RUNNING" > /dev/null; done
sleep 1.5
st=$("$BIN" status)
echo "   cancelled operations: $before -> $(echo "$st" | j 'v["upstreamOps"]["cancelled"]'), live $(echo "$st" | j 'v["upstreamOps"]["live"]')"
check "short waits cancel in-flight calls" [ "$(echo "$st" | j 'v["upstreamOps"]["cancelled"]')" -gt "$before" ]
check "no t3 thread show process left after cancellation" [ "$(t3_shows)" -eq 0 ]
T1=$(threads_of "$DPID"); F1=$(fds_of "$DPID")
echo "   daemon now: $T1 threads, $F1 fds (baseline $T0, $F0)"
check "daemon threads back to baseline" [ "$T1" -le "$T0" ]
check "daemon descriptors back to baseline" [ "$F1" -le "$F0" ]

# 6. Webhooks to a controlled receiver on 127.0.0.1, signed and authorized.
printf 'e2e-secret-%s' "$RANDOM$RANDOM" > "$WORK/secret"; chmod 600 "$WORK/secret"
start_receiver "$WORK/hooks.jsonl" "$WORK/port"
export HOOK_URL="http://127.0.0.1:$(cat "$WORK/port")/t3-watch/e2e" HOOK_AUTH="Bearer e2e-auth"
hook() { "$BIN" wait "$@" --webhook-url-env HOOK_URL --webhook-secret-file "$WORK/secret" --webhook-header-env Authorization=HOOK_AUTH; }
out=$(hook "$SETTLED"); code=$?
check "completed wait with a webhook exits 0" [ "$code" = 0 ]
check "notification reported delivered" [ "$(echo "$out" | j 'v["notification"]["delivered"]')" = True ]
check "output does not echo the webhook URL or secret" not_contains "$out" "t3-watch/e2e" "e2e-secret" "e2e-auth"
out=$(hook --timeout 2s --notify-on all "$RUNNING"); code=$?
check "timeout with --notify-on all exits 3" [ "$code" = 3 ]
# Heartbeats while waiting on the running thread.
out=$(hook --timeout 3500ms --heartbeat 1s "$RUNNING" 2>/dev/null); code=$?
check "heartbeat wait times out (exit 3)" [ "$code" = 3 ]
sleep 0.5
echo "   receiver log:"; sed 's/^/     /' "$WORK/hooks.jsonl"
check "receiver got settled, timeout, then heartbeats" python3 -c "
import json,sys; e=[json.loads(l)['event'] for l in open('$WORK/hooks.jsonl')]
sys.exit(0 if e[:2] == ['settled', 'timeout'] and e[2:].count('heartbeat') >= 2 else 1)"
check "every signature, Authorization and Idempotency-Key verified" python3 -c "
import json,sys; e=[json.loads(l) for l in open('$WORK/hooks.jsonl')]
sys.exit(0 if e and all(x['signatureOk'] and x['authorizationOk'] and x['idempotencyOk'] for x in e) else 1)"
# The fixture refuses a wrong Authorization header, and t3-watch says so.
out=$(HOOK_AUTH="Bearer wrong" hook "$SETTLED"); code=$?
check "a rejected webhook (401) exits 4 with delivered false" [ "$code/$(echo "$out" | j 'v["notification"]["webhook"].get("status")')" = 4/401 ]
kill "$RECV" 2>/dev/null

# 6b. Durable delivery: the receiver is down, the event is queued, then the
# receiver comes back on the same port and the outbox delivers it.
# Stop the daemon first so `outbox run --once` is the only sender.
"$BIN" stop > /dev/null
DPORT=$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1])")
out=$(HOOK_URL="http://127.0.0.1:$DPORT/t3-watch/durable" hook --durable --no-daemon "$SETTLED"); code=$?
check "durable wait with the receiver down exits 4 (queued)" [ "$code" = 4 ]
check "notification is queued" [ "$(echo "$out" | j 'v["notification"].get("queued")')" = True ]
QID=$(echo "$out" | j 'v["notification"]["id"]')
check "outbox lists the entry without secrets" [ "$("$BIN" outbox list | j 'len(v["entries"])')" = 1 ]
start_receiver "$WORK/durable.jsonl" "$WORK/dport" "$DPORT"
out=$("$BIN" outbox run --once); code=$?
check "outbox run --once delivers it (exit 0)" [ "$code" = 0 ]
kill "$RECV" 2>/dev/null
check "the receiver got the queued event with the same id" python3 -c "
import json,sys; e=[json.loads(l) for l in open('$WORK/durable.jsonl')]
sys.exit(0 if len(e) == 1 and e[0]['id'] == '$QID' and e[0]['signatureOk'] and e[0]['idempotencyOk'] else 1)"

# 7. An --on-settle program gets the result JSON on stdin, no shell involved;
# its output goes to hooks.log, not to our stdout or stderr.
err=$("$BIN" wait "$SETTLED" --on-settle /bin/sh --on-settle-arg -c \
  --on-settle-arg "cat > '$WORK/hook.json'; echo e2e-hook-output" 2>&1 >/dev/null); code=$?
check "on-settle hook ran and the wait exits 0" [ "$code" = 0 ]
check "hook received the completed result" [ "$(j 'v["outcome"]' < "$WORK/hook.json")" = completed ]
check "hook output went to hooks.log, not stderr" sh -c "grep -q e2e-hook-output '$T3_WATCH_HOME/hooks.log' && ! echo '$err' | grep -q e2e-hook-output"

# 8. Optional: a thread blocked on a question or approval (read-only).
if [ -n "${WAITING_THREAD:-}" ]; then
  out=$("$BIN" wait --timeout 2m "$WAITING_THREAD"); code=$?
  echo "   $(echo "$out" | j '[v["event"], v["threads"][0]["status"], v["threads"][0].get("pendingRequestCount"), v["threads"][0].get("pendingKey")]')"
  check "a blocked thread exits 5 with needs_input" [ "$code" = 5 ]
fi

# 9. Optional: strict OAuth discovery against a relay.
if [ -n "$RELAY" ]; then
  out=$("$BIN" auth discover "$RELAY"); code=$?
  check "strict discovery accepts $RELAY" [ "$code" = 0 ]
  echo "   refreshTokens offered: $(echo "$out" | j 'v["refreshTokens"]')"
fi

"$BIN" stop > /dev/null
echo "== work dir $WORK (remove when done)"
[ "$FAILED" = 0 ] && echo "ALL PASSED" || echo "SOME CHECKS FAILED"
exit "$FAILED"
