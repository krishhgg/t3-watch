#!/bin/sh
# Durable delivery gates with the real binary, isolated (own T3_WATCH_HOME,
# a fake local t3 CLI, a loopback receiver; no network change):
#   A. receiver down when the thread completes, daemon stopped and
#      restarted while the event is queued, then the receiver comes up;
#   B. the daemon is killed (SIGKILL) while its delivery attempt is in
#      flight, then restarted.
#   e2e/durable.sh <work dir> <t3-watch binary> <port>
# Prints nothing; the result is <work dir>/timeline.txt. Remove the work
# dir afterwards.
set -u
W=$1; BIN=$2; PORT=$3
TOOLS=$(cd "$(dirname "$0")" && pwd)
rm -rf "$W"; mkdir -p "$W/home" "$W/t3/threads"; chmod 700 "$W/home"
cat > "$W/t3/t3" <<SH
#!/bin/sh
for a; do last="\$a"; done
read status ord < "$W/t3/threads/\$last"
printf '{"thread":{"threadId":"%s","title":"durable","status":"%s","latestRunId":"run:thread:%s:ordinal:%s","pendingRequestCount":0},"messages":[]}\n' "\$last" "\$status" "\$last" "\$ord"
SH
chmod 755 "$W/t3/t3"
export T3_WATCH_HOME="$W/home" T3_WATCH_IDLE_EXIT=120s
unset T3_WATCH_SOCKET T3_WATCH_NO_DAEMON
ts() { python3 -I -c 'import time; print("%.3f" % time.time())'; }
log() { echo "$(ts) $*" >> "$W/timeline.txt"; }
olist() {
  "$BIN" outbox list | python3 -I -c 'import json,sys
v=json.load(sys.stdin); e=v.get("entries") or []
print(json.dumps({"pending": len(e), "ids": [x.get("id") for x in e], "attempts": [x.get("attempts") for x in e], "lastError": [x.get("lastError") for x in e]}))'
}
pending() { olist | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["pending"])'; }
dpid() { "$BIN" status | python3 -I -c 'import json,sys; v=json.load(sys.stdin); print(v.get("pid") or "")'; }
summ() { python3 -I -c 'import json,sys; v=json.load(open(sys.argv[1])); print(json.dumps({k: v.get(k) for k in ("outcome","via","notification","error")}))' "$1"; }
until_pending0() {
  i=0; while [ "$(pending)" != 0 ] && [ $i -lt 400 ]; do sleep 0.5; i=$((i + 1)); done
}
WAIT="--durable --transport local --t3-bin $W/t3/t3 --interval 200ms --timeout 60s --webhook-url http://127.0.0.1:$PORT/hook"

"$BIN" --version > "$W/version.txt"
# ---- A: receiver down, daemon restart --------------------------------------
log "A: nothing listens on 127.0.0.1:$PORT"
echo "running 1" > "$W/t3/threads/dr-a"
"$BIN" wait $WAIT dr-a > "$W/A-client.json" 2> "$W/A-client.err" & C=$!
sleep 1.5
echo "completed 1" > "$W/t3/threads/dr-a"; log "A: thread completed"
wait $C; log "A: client exit $?: $(summ "$W/A-client.json")"
log "A: outbox $(olist)"
log "A: stopping daemon pid $(dpid)"
"$BIN" stop > /dev/null; sleep 1
log "A: daemon running after stop: $("$BIN" status | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["running"])')"
log "A: outbox with no daemon $(olist)"
echo ok > "$W/mode"
python3 -I "$TOOLS/recvctl.py" "$PORT" "$W/receiver.jsonl" "$W/mode" & RECV=$!
sleep 0.5; log "A: receiver up (200)"
"$BIN" daemon --idle-exit 120s 2> "$W/daemon-A.err" & D=$!
log "A: daemon restarted pid $D"
until_pending0
log "A: outbox $(olist)"

# ---- B: SIGKILL during the delivery attempt ---------------------------------
echo hang > "$W/mode"; log "B: receiver records, then never answers"
echo "running 1" > "$W/t3/threads/dr-b"
"$BIN" wait $WAIT dr-b > "$W/B-client.json" 2> "$W/B-client.err" & C=$!
sleep 1.5
echo "completed 1" > "$W/t3/threads/dr-b"; log "B: thread completed"
i=0; while ! grep -q '"mode": "hang"' "$W/receiver.jsonl" 2>/dev/null && [ $i -lt 200 ]; do sleep 0.1; i=$((i + 1)); done
log "B: receiver holds attempt: $(grep '"mode": "hang"' "$W/receiver.jsonl" | tail -1)"
P=$(dpid); kill -9 "$P"; log "B: SIGKILL daemon pid $P"
wait $C; log "B: client exit $?: $(summ "$W/B-client.json")"
log "B: outbox after the kill $(olist)"
echo ok > "$W/mode"; log "B: receiver answers 200 again"
P2=$(dpid)
if [ -z "$P2" ]; then
  "$BIN" daemon --idle-exit 120s 2> "$W/daemon-B.err" & P2=$!
  log "B: daemon restarted pid $P2"
else
  log "B: a daemon is already running again, pid $P2 (started by a client)"
fi
until_pending0
log "B: outbox $(olist)"
"$BIN" stop > /dev/null
kill $RECV 2>/dev/null; wait 2>/dev/null
echo "--- receiver ---" >> "$W/timeline.txt"; cat "$W/receiver.jsonl" >> "$W/timeline.txt"
