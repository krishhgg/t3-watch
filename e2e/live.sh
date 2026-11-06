#!/usr/bin/env bash
# Live end-to-end checks against the T3 Code server on this machine, through
# the real daemon and the local transport. Read-only on T3: it only runs
# `t3 thread show`. Webhooks go to e2e/receiver.py on 127.0.0.1.
#
# Usage: e2e/live.sh <settled-thread-id> <running-thread-id> [t3-watch binary] [relay origin]
#   settled-thread-id  a thread that has finished (completed, idle, ...)
#   running-thread-id  a thread that stays running for about a minute
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

echo "== t3-watch $("$BIN" --version | cut -d' ' -f2) at $BIN"
echo "== work dir $WORK"

# 1. A finished thread settles through the daemon.
out=$("$BIN" wait "$SETTLED"); code=$?
echo "   $(echo "$out" | j '[v["event"], v["threads"][0]["status"], v["threads"][0]["runOrdinal"], v["via"], str(v["elapsedMs"]) + "ms"]')"
check "settled thread exits 0 via the daemon" [ "$code" = 0 ]
ORD=$(echo "$out" | j 'v["threads"][0]["runOrdinal"]')
DPID=$("$BIN" status | j 'v["pid"]')
sleep 1
T0=$(threads_of "$DPID"); F0=$(fds_of "$DPID")
echo "   daemon pid $DPID baseline: $T0 threads, $F0 fds"

# 2. A newer run ordinal than the thread has keeps waiting (exit 3).
out=$("$BIN" wait --timeout 3s --expect-min-run-ordinal $((ORD + 1)) "$SETTLED"); code=$?
check "stale run ordinal waits until the timeout (exit 3)" [ "$code" = 3 ]

# 3. Delegated-work waits are refused on the local transport.
out=$("$BIN" wait --until-delegated-settled "$SETTLED"); code=$?
check "--until-delegated-settled on a local thread exits 2" [ "$code" = 2 ]

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

# 6. Webhook to a controlled receiver on 127.0.0.1, signed and authorized.
printf 'e2e-secret-%s' "$RANDOM$RANDOM" > "$WORK/secret"; chmod 600 "$WORK/secret"
python3 "$HERE/receiver.py" "$WORK/secret" "Bearer e2e-auth" "$WORK/hooks.jsonl" "$WORK/port" & RECV=$!
for i in $(seq 50); do [ -s "$WORK/port" ] && break; sleep 0.1; done
export HOOK_URL="http://127.0.0.1:$(cat "$WORK/port")/t3-watch/e2e" HOOK_AUTH="Bearer e2e-auth"
out=$("$BIN" wait "$SETTLED" --webhook-url-env HOOK_URL --webhook-secret-file "$WORK/secret" \
  --webhook-header-env Authorization=HOOK_AUTH); code=$?
check "settled wait with a webhook exits 0" [ "$code" = 0 ]
check "notification reported delivered" [ "$(echo "$out" | j 'v["notification"]["delivered"]')" = True ]
check "output does not echo the webhook URL or secret" not_contains "$out" "t3-watch/e2e" "e2e-secret" "e2e-auth"
out=$("$BIN" wait --timeout 2s --notify-on all "$RUNNING" --webhook-url-env HOOK_URL \
  --webhook-secret-file "$WORK/secret" --webhook-header-env Authorization=HOOK_AUTH); code=$?
check "timeout with --notify-on all exits 3" [ "$code" = 3 ]
sleep 0.5
kill "$RECV" 2>/dev/null
echo "   receiver log:"; sed 's/^/     /' "$WORK/hooks.jsonl"
check "receiver got settled then timeout" [ "$(python3 -c "import json; print([json.loads(l)['event'] for l in open('$WORK/hooks.jsonl')])")" = "['settled', 'timeout']" ]
check "every signature and Authorization header verified" python3 -c "
import json,sys; e=[json.loads(l) for l in open('$WORK/hooks.jsonl')]
sys.exit(0 if e and all(x['signatureOk'] and x['authorizationOk'] for x in e) else 1)"

# 7. An --on-settle program gets the result JSON on stdin, no shell involved.
out=$("$BIN" wait "$SETTLED" --on-settle /bin/sh --on-settle-arg -c --on-settle-arg "cat > '$WORK/hook.json'"); code=$?
check "on-settle hook ran and the wait exits 0" [ "$code" = 0 ]
check "hook received the settled result" [ "$(j 'v["event"]' < "$WORK/hook.json")" = settled ]

# 8. Optional: strict OAuth discovery against a relay.
if [ -n "$RELAY" ]; then
  out=$("$BIN" auth discover "$RELAY"); code=$?
  check "strict discovery accepts $RELAY" [ "$code" = 0 ]
  echo "   refreshTokens offered: $(echo "$out" | j 'v["refreshTokens"]')"
fi

"$BIN" stop > /dev/null
echo "== work dir $WORK (remove when done)"
[ "$FAILED" = 0 ] && echo "ALL PASSED" || echo "SOME CHECKS FAILED"
exit "$FAILED"
