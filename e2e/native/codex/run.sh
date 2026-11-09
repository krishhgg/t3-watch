#!/bin/sh
# The Codex adapter against a real `codex app-server`, isolated: its own
# CODEX_HOME whose model provider is mock_responses.py on 127.0.0.1:18801,
# so no OpenAI request is made and nothing of the user's Codex is touched.
# Every thread is a disposable one that client A (probe_client.py) creates.
#
#   e2e/native/codex/run.sh <fixture dir, short: /tmp/...> <t3-watch binary>
#
# Prints the probes' JSON lines. Stops what it started and removes the
# fixture dir at the end (CODEX_HOME there holds about 100 MB).
set -u
X=$1; BIN=$2
HERE=$(cd "$(dirname "$0")" && pwd)
case "$X" in /tmp/*|/private/tmp/*) ;; *) echo "use a fixture dir under /tmp (socket paths are short)" >&2; exit 2 ;; esac
rm -rf "${X:?}"; mkdir -p "$X/home" "$X/proj"
cp "$HERE/mock-provider.toml" "$X/home/config.toml"
# The app-server does not always exit on its first SIGTERM.
stop() {
  kill "$1" 2>/dev/null
  i=0; while kill -0 "$1" 2>/dev/null && [ $i -lt 20 ]; do sleep 0.25; i=$((i + 1)); done
  kill -9 "$1" 2>/dev/null; wait "$1" 2>/dev/null
}
# mock <tool calls per turn> <pause before each answer, s>
mock() {
  [ -n "${MOCK:-}" ] && stop "$MOCK"
  MOCK_CALLS=$1 MOCK_DELAY=$2 MOCK_TOOL_CALL="touch escalated" \
  MOCK_TOOL_ARGS='{"sandbox_permissions":"require_escalated","justification":"probe"}' \
    python3 -I "$HERE/mock_responses.py" 18801 "$X/mock.log" > "$X/mock.out" 2>&1 & MOCK=$!
  sleep 0.5
  # Another mock on the port would answer instead, with other settings.
  if ! kill -0 "$MOCK" 2>/dev/null; then
    echo "the mock provider did not start (is port 18801 taken?):" >&2; cat "$X/mock.out" >&2; exit 2
  fi
}
echo "== $("$BIN" --version); $(codex --version); $(date -u +%FT%TZ)"
mock 1 0
CODEX_HOME="$X/home" codex app-server --listen "unix://$X/cs.sock" > "$X/server.out" 2> "$X/server.err" & SERVER=$!
i=0; while [ ! -S "$X/cs.sock" ] && [ $i -lt 80 ]; do sleep 0.25; i=$((i + 1)); done
echo "== probe.py"
python3 -I "$HERE/probe.py" "$X" "$BIN"
for d in 3 0.5 0; do
  echo "== probe_two_requests.py, model pause ${d}s"
  mock 2 "$d"
  python3 -I "$HERE/probe_two_requests.py" "$X" "$BIN"
done
stop "$SERVER"; stop "$MOCK"
for h in "$X/w" "$X/w2" "$X/w3"; do T3_WATCH_HOME=$h "$BIN" stop > /dev/null 2>&1; done
target=$(readlink "$X/cs.sock" 2>/dev/null)
rm -rf "${X:?}"
[ -n "$target" ] && rm -f "$target" "$target.lock"
echo "== done $(date -u +%FT%TZ)"
