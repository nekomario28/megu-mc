#!/usr/bin/env bash
# One port-side run of the sculk dump, with the full trace env block, provenance and sanity.
#
# Why: the native side got `sculk_capture.sh` and the port side was still nine environment
# variables typed by hand. Forgetting one is silent and changes what the trace can answer - a run
# without `FEATURE_ATTEMPT_TRACE` has no `SPASS`, so every pass-level comparison against it is
# meaningless, and `sculk_trace.py sanity` will (correctly) refuse it. Two env blocks that must
# travel together belong in one file.
#
# Usage: tools/sculk_port_run.sh <tag> [--n N] [--x CX] [--z CZ] [--arm cx,cz,gi,step]
#                                 [--limit N] [--dump PATH]
# Output: the port log path on stdout; provenance beside it.
set -euo pipefail
cd "$(dirname "$0")/.."

TAG=""
N=4
OX=-18
OZ=110
ARM=""
LIMIT=800
DUMP=""
while [ $# -gt 0 ]; do
  case "$1" in
    --n) N=$2; shift 2 ;;
    --x) OX=$2; shift 2 ;;
    --z) OZ=$2; shift 2 ;;
    --arm) ARM=$2; shift 2 ;;
    --limit) LIMIT=$2; shift 2 ;;
    --dump) DUMP=$2; shift 2 ;;
    # `--opt=value` as well as `--opt value`: the usage line shows the `=` form, and a parser that
    # silently accepted neither already cost one debug cycle on the capture side.
    --n=*) N=${1#--n=}; shift ;;
    --x=*) OX=${1#--x=}; shift ;;
    --z=*) OZ=${1#--z=}; shift ;;
    --arm=*) ARM=${1#--arm=}; shift ;;
    --limit=*) LIMIT=${1#--limit=}; shift ;;
    --dump=*) DUMP=${1#--dump=}; shift ;;
    --*) echo "sculk_port_run: unknown option $1" >&2; exit 2 ;;
    *) TAG=$1; shift ;;
  esac
done
if [ -z "$TAG" ]; then
  echo "usage: tools/sculk_port_run.sh <tag> [--n N --x CX --z CZ --arm cx,cz,gi,step --limit N]" >&2
  exit 2
fi
if [ ! -x target/release/mc-spine ]; then
  echo "sculk_port_run: build first (cargo build --release)" >&2
  exit 1
fi

OUT="${SCULK_TRACE_DIR:-$HOME/.cache/mc-spine-audit/sculk-traces}"
mkdir -p "$OUT"
DUMP="${DUMP:-/var/tmp/sculk-port-$TAG.bin}"
LOG="$OUT/port-$TAG.log"

# `MC_SPINE_WG_PROFILE=0` is the gate the dump path needs, with the warm-up pass switched off: any
# other value runs a profile pass first and the placement lines then come from a *different run*
# than the armed draws (sanity fails on that, by design).
env MC_SPINE_WG_PROFILE=0 \
    MC_SPINE_WG_DUMP="$DUMP" \
    MC_SPINE_WG_DUMP_N="$N" \
    MC_SPINE_WG_DUMP_X="$OX" \
    MC_SPINE_WG_DUMP_Z="$OZ" \
    MC_SPINE_SEED=0 \
    FEATURE_ATTEMPT_TRACE=1 \
    SPC_CURSOR_TRACE=1 \
    ${ARM:+FSEED_ARM=$ARM} \
    ${ARM:+FEATURE_DRAW_LIMIT=$LIMIT} \
    ./target/release/mc-spine 2>"$LOG" >/dev/null

{
  echo "tag: $TAG"
  echo "captured: $(date -Iseconds)"
  echo "dump box: ${N}x${N} from ${OX},${OZ} (seed 0)"
  echo "armed: ${ARM:-none} (limit $LIMIT)"
  echo "log: $LOG"
  echo "lines: $(wc -l < "$LOG")"
  echo "note: the port writes this file through its own stderr, in program order (no log framework"
  echo "      in between); it is the port-side counterpart of a SculkLog trace."
} > "$OUT/PROVENANCE-port-$TAG.txt"

python3 tools/sculk_trace.py sanity "$LOG" >&2 || {
  echo "sculk_port_run: sanity failed - do not compare with this log until it is fixed" >&2
  echo "$LOG"
  exit 3
}
echo "$LOG"
