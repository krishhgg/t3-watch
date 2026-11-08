#!/bin/sh
# Durable replay to the REAL webhook receiver through a controlled, isolated
# outage (gate T3b in docs/acceptance-manifest.md). Run it where the bot's
# webhook URL and secret already are. Nothing here reads, prints or stores
# them: the URL goes to e2e/relay.py through T3W_RELAY_TARGET, the secret to
# t3-watch through --webhook-secret-env.
#
#   T3W_RELAY_TARGET=<bot webhook URL> T3W_SECRET=<webhook secret> \
#     e2e/acceptance/real-receiver-replay.sh <t3-watch> <thread URL> [port]
#
# The thread is a disposable T3 test thread that you make complete while the
# relay is down (the script tells you when). The outage is this watcher's
# path only: the relay is a loopback process that is not running yet. Steps:
#   1. wait --durable through the relay (down) until the thread completes;
#      the event is queued (exit 4);
#   2. the daemon is stopped and the entry checked from disk;
#   3. the relay comes up, a daemon starts, and the queued event goes to the
#      real receiver with its original id;
#   4. you record the bot's receipt and continuation with e2e/timeline.py.
# Work dir: a new /tmp/t3w-replay.XXXX, kept for the evidence; the relay
# log there has ids, attempts and statuses only.
set -u
BIN=$1; THREAD=$2; PORT=${3:-18840}
HERE=$(cd "$(dirname "$0")/.." && pwd)
: "${T3W_RELAY_TARGET:?set T3W_RELAY_TARGET to the receiver URL}"
: "${T3W_SECRET:?set T3W_SECRET to the webhook secret}"
W=$(mktemp -d /tmp/t3w-replay.XXXX)
now() { date -u +%FT%T.000Z; }
log() { echo "$(now) $*" | tee -a "$W/timeline.txt"; }
"$BIN" --version | tee "$W/version.txt"
log "relay on 127.0.0.1:$PORT is down (not started)"
"$BIN" wait --durable --notify-on all --timeout 2h \
  --webhook-url "http://127.0.0.1:$PORT/hook" --webhook-secret-env T3W_SECRET \
  "$THREAD" > "$W/wait.json" 2> "$W/wait.err" & C=$!
sleep 2
log "wait started (pid $C). Make the test thread complete now; this waits for it."
wait $C; CODE=$?
log "wait exit $CODE (4 = completed, notification queued)"
python3 -I -c 'import json,sys; v=json.load(open(sys.argv[1])); n=v.get("notification") or {}; print(json.dumps({"outcome": v.get("outcome"), "watchId": v.get("watchId"), "endedAt": v.get("endedAt"), "id": n.get("id"), "queued": n.get("queued"), "delivered": n.get("delivered")}))' "$W/wait.json" | tee -a "$W/timeline.txt"
"$BIN" outbox list > "$W/outbox-1.json"; log "outbox: $(python3 -I -c 'import json,sys; print([e["id"] for e in json.load(open(sys.argv[1])).get("entries", [])])' "$W/outbox-1.json")"
"$BIN" stop > /dev/null 2>&1; sleep 1
log "daemon stopped; running: $("$BIN" status | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["running"])')"
"$BIN" outbox list > "$W/outbox-2.json"; log "outbox with no daemon: $(python3 -I -c 'import json,sys; print([e["id"] for e in json.load(open(sys.argv[1])).get("entries", [])])' "$W/outbox-2.json")"
python3 -I "$HERE/relay.py" "$PORT" "$W/relay.jsonl" & R=$!
sleep 0.5; log "relay up (pid $R)"
"$BIN" daemon > "$W/daemon.err" 2>&1 & D=$!
log "daemon started (pid $D)"
i=0
while [ $i -lt 600 ]; do
  left=$("$BIN" outbox list | python3 -I -c 'import json,sys; print(len(json.load(sys.stdin).get("entries", [])))')
  [ "$left" = 0 ] && break
  sleep 1; i=$((i + 1))
done
log "outbox entries left: $left"
log "relay log:"; cat "$W/relay.jsonl" | tee -a "$W/timeline.txt"
kill $R 2>/dev/null
echo
echo "Now record the bot's own receipt and continuation (not this script's):"
echo "  python3 -I $HERE/timeline.py <state dir>/trace.jsonl --watch <watchId above> \\"
echo "    --receipt <time the bot's routine logged the request> \\"
echo "    --continuation <time of the bot's first message or action naming the thread> --bot <name>"
echo "Evidence: $W"
