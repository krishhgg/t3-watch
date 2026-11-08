#!/bin/sh
# Read-only check of a Codex desktop thread (gate X5 in
# docs/acceptance-manifest.md). Run it on the Mac that has the thread, as
# its user, while the desktop app has the thread open.
#
#   e2e/native/codex/desktop-check.sh <t3-watch> [thread id]
#
# It starts nothing and changes nothing in Codex: no daemon start, no
# thread/resume, no prompt. t3-watch sends initialize, thread/read and
# thread/turns/list only, and its own state goes to a temporary home that
# is removed afterwards. If no app-server socket exists, it says so and
# stops: reaching the thread then needs `codex app-server daemon start`,
# which is a change on that Mac for its user to approve.
set -u
BIN=$1; TID=${2:-01a1b891-e945-7f70-b755-db5ece6dcaff}
CH=${CODEX_HOME:-$HOME/.codex}
SOCK=${T3_WATCH_CODEX_SOCKET:-$CH/app-server-control/app-server-control.sock}
echo "host $(hostname) user $(whoami) $(uname -sm); $(date -u +%FT%TZ)"
"$BIN" --version
codex --version 2>&1 | head -1
echo "--- codex app-server daemon version (read-only)"
codex app-server daemon version 2>&1 | head -3
echo "--- socket $SOCK"
if [ ! -e "$SOCK" ]; then
  echo "no app-server socket: the desktop app does not expose one here."
  echo "RESULT: BLOCKED (no socket; starting a daemon needs the user's approval)"
  exit 3
fi
ls -l "$SOCK"
H=$(mktemp -d /tmp/t3wdesk.XXXX)
T3_WATCH_HOME=$H T3_WATCH_CODEX_SOCKET=$SOCK "$BIN" wait --no-daemon --timeout 5s "codex://threads/$TID" > "$H/out.json" 2> "$H/err.txt"
CODE=$?
python3 -I -c 'import json,sys
v = json.load(open(sys.argv[1])); t = (v.get("threads") or [{}])[0]
print(json.dumps({"exit": int(sys.argv[2]), "outcome": v.get("outcome"), "status": t.get("status"),
      "runOrdinal": t.get("runOrdinal"), "pendingRequestCount": t.get("pendingRequestCount"),
      "title": t.get("title"), "lastError": t.get("lastError") or (t.get("upstream") or {}).get("lastError")}))' "$H/out.json" "$CODE"
rm -rf "${H:?}"
case $CODE in
  0|3|5|6) echo "RESULT: the thread was read (exit $CODE: 0 completed, 3 still running or no turn, 5 waiting on its user, 6 failed)" ;;
  *) echo "RESULT: FAIL (exit $CODE)" ;;
esac
