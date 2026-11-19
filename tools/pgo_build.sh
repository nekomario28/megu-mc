#!/usr/bin/env bash
# Phase B-3b (Layer A4): reproducible PGO build recipe.
#
# 1.  Instrumented release build: RUSTFLAGS="-Cprofile-generate=$PGO_DIR/raw"
#     into the separate target subdir target/pgo/gen.
# 2.  Deterministic training dumps (Overworld 64ch full j=1 & j=8, NOFEAT,
#     Nether 4242, End 0) written into $PGO_DIR/tmp and deleted immediately
#     after each run, on the instrumented binary.
# 3.  llvm-profdata merge -o $PGO_DIR/merged.profdata $PGO_DIR/raw.
# 4.  Optimized release build: RUSTFLAGS="-Cprofile-use=$PGO_DIR/merged.profdata
#     -Cllvm-args=-pgo-warn-missing-function" into target/pgo/use
#     (copied to --out when that was requested).
# 5.  Bit-identity check: the PGO binary must reproduce the 64-chunk full
#     dump's sha256 recorded in tools/perf_bench.py
#     (EXPECTED_SHA_DEFAULT["full"]).
#
# llvm-profdata is located as: the active toolchain's rustup sysroot
# lib/rustlib/*/bin/llvm-profdata, then every other installed rustup
# toolchain's sysroot (same path), then PATH, then /usr/bin/llvm-profdata*
# (and /usr/local/bin/llvm-profdata*).  The first candidate that can merge
# the raw profiles is used; when none is found or none can merge them the
# script exits non-zero with the actionable message
# "rustup component add llvm-tools".
#
# Usage: tools/pgo_build.sh [--out <path>] [--dry-run] [--jobs N]
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

PGO_DIR="target/pgo"
GEN_DIR="$PGO_DIR/gen"
USE_DIR="$PGO_DIR/use"
RAW_DIR="$PGO_DIR/raw"
# Absolute path for the merge output and the profile-use file: rustc resolves
# `-Cprofile-use` against the cwd of the process cargo spawns it in, and with
# `--target-dir` that is not the workspace root here (observed on rustc 1.98.0 +
# cargo 1.98.0: "file `target/pgo/merged.profdata` passed to `-C profile-use`
# does not exist" when passed relatively, even though the file exists from the
# workspace root).  The same absolute path is used for the merge output.
MERGED="$(pwd)/target/pgo/merged.profdata"
TRAIN_DIR="$PGO_DIR/tmp"
BIN_GEN="$GEN_DIR/release/mc-spine"
BIN_USE="$USE_DIR/release/mc-spine"

# Keep in sync with tools/perf_bench.py EXPECTED_SHA_DEFAULT["full"]: sha256 of
# the 64-chunk seed-0 N=8 overworld full dump (25,165,824 B).
EXPECT_FULL_SHA_256="670fdfe3472812afeeba823a30e08f472ea8eb69965d6ba0c54b30a2e9949def"

OUT=""
DRY_RUN=0
JOBS=2

usage() {
  cat <<'EOF'
Usage: tools/pgo_build.sh [--out <path>] [--dry-run] [--jobs N]

Options:
  --out <path>   copy the PGO binary to <path> after building it
  --dry-run      print the plan and the discovered llvm-profdata; build nothing
  --jobs N       cargo parallelism (default 2)
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --out) OUT="${2:?--out requires a path}"; shift 2 ;;
    --out=*) OUT="${1#*=}"; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --jobs) JOBS="${2:?--jobs requires a number}"; shift 2 ;;
    --jobs=*) JOBS="${1#*=}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "tools/pgo_build.sh: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

find_llvm_profdata_candidates() {
  # Prints every llvm-profdata path found, in priority order:
  #   1. the active toolchain's rustup sysroot: lib/rustlib/*/bin/llvm-profdata
  #   2. every other installed rustup toolchain's sysroot (same path)
  #   3. PATH
  #   4. /usr/bin/llvm-profdata* (and /usr/local/bin/llvm-profdata*)
  local sysroot d
  local out=()
  if command -v rustc >/dev/null 2>&1; then
    sysroot="$(rustc --print sysroot 2>/dev/null || true)"
    if [ -n "$sysroot" ]; then
      for d in "$sysroot"/lib/rustlib/*/bin; do
        [ -x "$d/llvm-profdata" ] && out+=("$d/llvm-profdata")
      done
    fi
  fi
  local rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
  if command -v rustup >/dev/null 2>&1 && [ -d "$rustup_home" ]; then
    local t
    for t in $(rustup toolchain list 2>/dev/null | awk '{print $1}' | LC_ALL=C sort); do
      for d in "$rustup_home/toolchains/$t/lib/rustlib"/*/bin; do
        [ -x "$d/llvm-profdata" ] && out+=("$d/llvm-profdata")
      done
    done
  fi
  if command -v llvm-profdata >/dev/null 2>&1; then
    out+=("$(command -v llvm-profdata)")
  fi
  local f
  for f in /usr/bin/llvm-profdata* /usr/local/bin/llvm-profdata*; do
    [ -x "$f" ] && out+=("$f")
  done
  # dedupe preserving order
  local seen="" d0 uniq=()
  for d0 in ${out[@]+"${out[@]}"}; do
    [ -n "$d0" ] || continue
    case " $seen " in
      *" $d0 "*) continue ;;
    esac
    seen="$seen $d0"
    uniq+=("$d0")
  done
  for d0 in ${uniq[@]+"${uniq[@]}"}; do
    printf '%s\n' "$d0"
  done
}

PROFDATA_CANDIDATES=()
while IFS= read -r c; do
  [ -n "$c" ] && PROFDATA_CANDIDATES+=("$c")
done < <(find_llvm_profdata_candidates)

if [ "${#PROFDATA_CANDIDATES[@]}" -eq 0 ]; then
  echo "tools/pgo_build.sh: llvm-profdata not found; install it: rustup component add llvm-tools" >&2
  exit 1
fi

if [ "$DRY_RUN" -eq 1 ]; then
  echo "llvm-profdata candidates (priority order):"
  for c in "${PROFDATA_CANDIDATES[@]}"; do echo "  $c"; done
  echo "step 1 instrumented build: RUSTFLAGS=-Cprofile-generate=$RAW_DIR cargo build --release --offline --locked -j $JOBS --target-dir $GEN_DIR"
  echo "step 2 training dumps: overworld-j1, overworld-j8, nofeat-j8, nether-4242, end-0 -> $TRAIN_DIR (deleted after each run)"
  echo "step 3 merge: first candidate that merges the .profraw files in $RAW_DIR"
  echo "step 4 optimized build: RUSTFLAGS='-Cprofile-use=$MERGED -Cllvm-args=-pgo-warn-missing-function' cargo build --release --offline --locked -j $JOBS --target-dir $USE_DIR"
  echo "step 5 full-dump bit-identity check against $EXPECT_FULL_SHA_256"
  echo "final binary: ${OUT:-$BIN_USE}"
  exit 0
fi

mkdir -p "$RAW_DIR" "$TRAIN_DIR"

# ---- Step 1: instrumented release build into target/pgo/gen -----------------
RUSTFLAGS="-Cprofile-generate=$RAW_DIR" \
  cargo build --release --offline --locked -j "$JOBS" --target-dir "$GEN_DIR"

# ---- Step 2: deterministic training workloads on the instrumented binary ----
# Same dump recipe as tools/perf_bench.py run_one(): MC_SPINE_WG_PROFILE=0,
# DUMP_N=8, DUMP_X=0, DUMP_Z=0, SEED/PRESET per arm.
train_dump() {
  local name="$1" preset="$2" seed="$3" nofeat="$4" jobs="$5" path="$TRAIN_DIR/$1.bin"
  env MC_SPINE_WG_PROFILE=0 MC_SPINE_SEED="$seed" MC_SPINE_WG_PRESET="$preset" \
      MC_SPINE_WG_DUMP="$path" MC_SPINE_WG_DUMP_N=8 MC_SPINE_WG_DUMP_X=0 MC_SPINE_WG_DUMP_Z=0 \
      MC_SPINE_GEN_JOBS="$jobs" ${nofeat:+MC_SPINE_NOFEAT=1} \
      "$BIN_GEN" >/dev/null 2>&1 || {
        echo "tools/pgo_build.sh: training dump $name failed (preset=$preset seed=$seed nofeat=$nofeat jobs=$jobs)" >&2
        exit 1
      }
  local sha
  sha="$(sha256sum "$path" | cut -d' ' -f1)"
  rm -f "$path"
  printf '[pgo_build] training dump %-12s sha256=%s\n' "$name" "$sha"
}

train_dump overworld-j1 overworld 0 "" 1
train_dump overworld-j8 overworld 0 "" 8
train_dump nofeat-j8      overworld 0 1 8
train_dump nether-4242    nether   4242 "" 8
train_dump end-0          end      0 "" 8

# ---- Step 3: merge raw profiles, first candidate that works -----------------
RAW_FILES="$(find "$RAW_DIR" -type f -name '*.profraw' | sort)"
if [ -z "$RAW_FILES" ]; then
  echo "tools/pgo_build.sh: no .profraw files under $RAW_DIR" >&2
  exit 1
fi
MERGED_OK=0
for PROFDATA in "${PROFDATA_CANDIDATES[@]}"; do
  echo "[pgo_build] merging: $PROFDATA merge -o $MERGED $RAW_FILES"
  if "$PROFDATA" merge -o "$MERGED" $RAW_FILES; then
    MERGED_OK=1
    break
  fi
  echo "[pgo_build] $PROFDATA could not merge the raw profiles; trying the next candidate" >&2
done
if [ "$MERGED_OK" -ne 1 ]; then
  echo "tools/pgo_build.sh: no llvm-profdata could merge the raw profiles; install or upgrade one: rustup component add llvm-tools" >&2
  exit 1
fi

# ---- Step 4: optimized release build into target/pgo/use --------------------
RUSTFLAGS="-Cprofile-use=$MERGED -Cllvm-args=-pgo-warn-missing-function" \
  cargo build --release --offline --locked -j "$JOBS" --target-dir "$USE_DIR"

# ---- Step 5: bit-identity check on the 64-chunk full dump -------------------
FULL_DUMP="$TRAIN_DIR/pgo-full-check.bin"
env MC_SPINE_WG_PROFILE=0 MC_SPINE_SEED=0 MC_SPINE_WG_PRESET=overworld \
    MC_SPINE_WG_DUMP="$FULL_DUMP" MC_SPINE_WG_DUMP_N=8 MC_SPINE_WG_DUMP_X=0 MC_SPINE_WG_DUMP_Z=0 \
    MC_SPINE_GEN_JOBS=8 "$BIN_USE" >/dev/null 2>&1 || {
      echo "tools/pgo_build.sh: PGO full dump run failed" >&2
      exit 1
    }
FULL_SHA="$(sha256sum "$FULL_DUMP" | cut -d' ' -f1)"
rm -f "$FULL_DUMP"
printf '[pgo_build] PGO full dump sha256=%s\n' "$FULL_SHA"
if [ "$FULL_SHA" != "$EXPECT_FULL_SHA_256" ]; then
  echo "tools/pgo_build.sh: bit-identity check FAILED: got $FULL_SHA, expected $EXPECT_FULL_SHA_256" >&2
  exit 1
fi
echo "[pgo_build] bit-identity check PASS: PGO full dump matches the recorded digest"

if [ -n "$OUT" ]; then
  cp -f "$BIN_USE" "$OUT"
  echo "[pgo_build] PGO binary: $OUT"
else
  echo "[pgo_build] PGO binary: $BIN_USE"
fi
echo "[pgo_build] done"
