#!/usr/bin/env bash
# One native capture of the sculk trace, with provenance and a sanity gate.
#
# Why: a capture used to be five steps typed by hand (build the agent, pick a trace path, set
# JAVA_OPTS, run the fixture, then remember where the trace went), and the trace is the input to
# every comparison - so a capture that silently produced an unreadable trace cost the whole
# round-trip. This wraps the steps, writes provenance next to the trace, and runs
# `sculk_trace.py sanity` on the result *before* returning.
#
# Usage: tools/sculk_capture.sh <tag> [--chunk CX CZ] [--n N] [--mode features]
#                               [--jar PATH] [--settle SECONDS] [--no-sanity]
#                               [--jvm-opt -Dfoo=bar]...   (repeatable; passed to the server JVM)
# Output: the trace path on stdout; provenance and the fixture log beside it.
set -euo pipefail
cd "$(dirname "$0")/.."

TAG=""
CHUNK_X=-16
CHUNK_Z=112
N=1
MODE=features
JAR=""
SETTLE=75
RUN_SANITY=1
EXTRA_OPTS=""
while [ $# -gt 0 ]; do
  case "$1" in
    --chunk) CHUNK_X=$2; CHUNK_Z=$3; shift 3 ;;
    --n) N=$2; shift 2 ;;
    --mode) MODE=$2; shift 2 ;;
    --jar) JAR=$2; shift 2 ;;
    --settle) SETTLE=$2; shift 2 ;;
    --no-sanity) RUN_SANITY=0; shift ;;
    --jvm-opt) EXTRA_OPTS="$EXTRA_OPTS $2"; shift 2 ;;
    # `--jvm-opt=-Dfoo=bar` as well: the usage line shows the `=` form, and a mismatch between
    # what the usage shows and what the parser accepts silently made the *value* become the tag.
    --jvm-opt=*) EXTRA_OPTS="$EXTRA_OPTS ${1#--jvm-opt=}"; shift ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    --*) echo "sculk_capture: unknown option $1" >&2; exit 2 ;;
    *) TAG=$1; shift ;;
  esac
done
if [ -z "$TAG" ]; then
  echo "usage: tools/sculk_capture.sh <tag> [--chunk CX CZ] [--n N] [--mode features] [--jar PATH]" >&2
  exit 2
fi

OUT="${SCULK_TRACE_DIR:-$HOME/.cache/mc-spine-audit/sculk-traces}"
FIXTURE_DIR="${FIXTURE_DIR:-/var/tmp}"
mkdir -p "$OUT"

if [ -z "$JAR" ]; then
  JAR="$(tools/build_carve_agent.sh | tail -2 | head -1)"
  echo "sculk_capture: built agent $JAR" >&2
fi
if [ ! -f "$JAR" ]; then
  echo "sculk_capture: no agent jar at $JAR" >&2
  exit 1
fi
if [ ! -f "$(dirname "$JAR")/carvelog.jar" ]; then
  echo "sculk_capture: carvelog.jar must sit next to $JAR (the fixture puts it on the bootstrap"
  echo "               classpath)" >&2
  exit 1
fi

TRACE="$OUT/sculk-trace-$TAG.txt"
rm -f "$TRACE"
STAMP="$(date -Iseconds)"
JAR_SHA="$(sha256sum "$JAR" | cut -d' ' -f1)"

echo "sculk_capture: capturing $TAG (chunk $CHUNK_X,$CHUNK_Z x$N, mode $MODE, settle ${SETTLE}s)" >&2
FIXTURE_DIR="$FIXTURE_DIR" \
JAVA_AGENT="$JAR" \
JAVA_OPTS="-Dsculktrace=$TRACE -Dfeaturebox=-265,-47,1780,-257,-38,1790$EXTRA_OPTS" \
FIXTURE_SETTLE="$SETTLE" \
  tools/chunk_world_fixture.sh "$TAG" "$CHUNK_X" "$CHUNK_Z" "$N" "$MODE" \
  > "$OUT/fixture-$TAG.log" 2>&1 || {
    echo "sculk_capture: fixture failed; last lines of $OUT/fixture-$TAG.log:" >&2
    tail -5 "$OUT/fixture-$TAG.log" >&2
    exit 1
  }

if [ ! -s "$TRACE" ]; then
  echo "sculk_capture: the agent wrote no trace to $TRACE" >&2
  exit 1
fi

{
  echo "tag: $TAG"
  echo "captured: $STAMP"
  echo "chunk: $CHUNK_X,$CHUNK_Z n=$N mode=$MODE settle=$SETTLE"
  echo "seed: 0 (fixture default)"
  echo "agent: $JAR sha256=$JAR_SHA"
  echo "extra jvm opts:$EXTRA_OPTS"
  echo "trace: $TRACE"
  echo "fixture log: $OUT/fixture-$TAG.log"
  echo "lines: $(wc -l < "$TRACE")"
  echo "note: line order in this file is program order (SculkLog's own writer); never regroup a"
  echo "      game log positionally - tools/sculk_trace.py refuses that on purpose."
} > "$OUT/PROVENANCE-$TAG.txt"

python3 tools/sculk_trace.py summary "$TRACE" >&2
if [ "$RUN_SANITY" = 1 ]; then
  if ! python3 tools/sculk_trace.py sanity "$TRACE"; then
    echo "sculk_capture: sanity failed - the trace is captured and kept, but do not compare with" >&2
    echo "               it until the problem above is fixed" >&2
    echo "$TRACE"
    exit 3
  fi
fi
echo "$TRACE"
