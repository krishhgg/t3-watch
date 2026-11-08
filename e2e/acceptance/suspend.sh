#!/bin/sh
# A real host suspend and resume (gate T5 in docs/acceptance-manifest.md).
# Run it on the machine that will sleep, with a thread that runs somewhere
# else (for example on another machine's T3) and completes while this one
# sleeps. A person sleeps the machine; nothing here does.
#
#   e2e/acceptance/suspend.sh <t3-watch> <thread URL> [webhook URL]
#
# No CPU runs while the host sleeps: the completion is seen at the first
# poll after the wake, so the webhook is late by the sleep. That lateness is
# the expected result, not a failure. If the sleep outlasts --timeout, the
# right outcome is `timeout`, not a late completion.
set -u
BIN=$1; THREAD=$2; HOOK=${3:-}
W=$(mktemp -d /tmp/t3w-suspend.XXXX)
"$BIN" --version | tee "$W/version.txt"
set -- wait --durable --notify-on all --timeout 2h
[ -n "$HOOK" ] && set -- "$@" --webhook-url "$HOOK"
date -u +"%FT%TZ start" | tee -a "$W/timeline.txt"
"$BIN" "$@" "$THREAD" > "$W/wait.json" 2> "$W/wait.err" & C=$!
echo "Waiting (pid $C). Now sleep this machine (Apple menu > Sleep, or close the lid),"
echo "let the thread complete meanwhile, and wake it a few minutes later."
wait $C; echo "exit $?" | tee -a "$W/timeline.txt"
date -u +"%FT%TZ wait ended" | tee -a "$W/timeline.txt"
python3 -I -c 'import json,sys
v = json.load(open(sys.argv[1])); t = (v.get("threads") or [{}])[0]
print(json.dumps({k: v.get(k) for k in ("outcome", "watchId", "startedAt", "endedAt", "elapsedMs",
      "elapsedAwakeMs", "elapsedWallMs", "suspendedMs", "notification")}, indent=1))
print(json.dumps({"runOrdinal": t.get("runOrdinal"), "status": t.get("status")}))' "$W/wait.json" | tee -a "$W/timeline.txt"
if command -v pmset > /dev/null; then
  echo "--- system sleep and wake (pmset -g log)" | tee -a "$W/timeline.txt"
  pmset -g log | grep -E " (Sleep|Wake|DarkWake) " | tail -6 | tee -a "$W/timeline.txt"
elif command -v journalctl > /dev/null; then
  echo "--- system sleep and wake (journalctl)" | tee -a "$W/timeline.txt"
  journalctl -b --no-pager -o short-iso -t systemd-sleep 2>/dev/null | tail -6 | tee -a "$W/timeline.txt"
fi
echo "Evidence: $W (compare the wake time with endedAt and the receiver's arrival time)"
