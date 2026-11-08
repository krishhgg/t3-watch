#!/bin/sh
# Native Linux run of a release archive (gate T7 in
# docs/acceptance-manifest.md). Run it on the Linux host, as a normal user,
# with python3 on PATH.
#
#   sh linux.sh <dir with the archives> [expected rev]
#
# The directory holds t3-watch-<ver>-<target>.tar.gz and its .sha256, and,
# for the test suite, t3-watch-tests-<ver>-<target>.tar.gz and its .sha256:
# the test binaries cross-built on macOS, and the source they were built
# from, which unpack under /tmp/t3wx and /tmp/t3wsrc (the paths they were
# built with). The archive's own binary replaces the one the tests were
# built with, so the suite runs against the shipped binary. Also runs
# e2e/durable.sh from that source with it. Everything goes under /tmp;
# remove /tmp/t3wx, /tmp/t3wsrc and the printed work dir afterwards.
set -u
D=$(cd "$1" && pwd); REV=${2:-}
case "$(uname -m)" in
  x86_64) TARGET=x86_64-unknown-linux-gnu ;;
  aarch64|arm64) TARGET=aarch64-unknown-linux-gnu ;;
  *) echo "unsupported machine $(uname -m)"; exit 2 ;;
esac
echo "host $(hostname) $(uname -srm); $(ldd --version 2>&1 | head -1); $(date -u +%FT%TZ)"
TAR=$(ls "$D"/t3-watch-*-"$TARGET".tar.gz | grep -v tests | head -1)
(cd "$D" && sha256sum -c "$(basename "$TAR").sha256") || exit 1
W=$(mktemp -d /tmp/t3wlinux.XXXX)
tar -xzf "$TAR" -C "$W"
BIN=$(ls "$W"/*/t3-watch)
"$BIN" --version
if [ -n "$REV" ] && ! "$BIN" --version | grep -q "rev $REV"; then
  echo "FAIL: --version does not name rev $REV"; exit 1
fi
FAILED=0
TESTS=$(ls "$D"/t3-watch-tests-*-"$TARGET".tar.gz 2>/dev/null | head -1)
if [ -n "$TESTS" ]; then
  (cd "$D" && sha256sum -c "$(basename "$TESTS").sha256") || exit 1
  rm -rf /tmp/t3wx /tmp/t3wsrc && tar -xzf "$TESTS" -C /
  cp "$BIN" "/tmp/t3wx/$TARGET/release/t3-watch"
  for t in /tmp/t3wx/"$TARGET"/release/deps/*; do
    [ -f "$t" ] && [ -x "$t" ] || continue
    name=$(basename "$t")
    if (cd /tmp && "$t" > "$W/$name.log" 2>&1); then r=ok; else r=FAILED; FAILED=1; fi
    echo "$name: $r $(grep -E '^test result' "$W/$name.log" | tail -1)"
  done
  echo "--- e2e/durable.sh with the archive's binary"
  sh /tmp/t3wsrc/e2e/durable.sh "$W/durable" "$BIN" 18823
  grep -E "client exit|outbox \{|SIGKILL|receiver" "$W/durable/timeline.txt"
fi
echo "Evidence: $W"
[ $FAILED = 0 ] && echo "RESULT: PASS" || echo "RESULT: FAIL"
