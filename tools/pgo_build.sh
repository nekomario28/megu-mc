#!/usr/bin/env bash
# Phase B-3b (Layer A4): reproducible PGO build recipe.
#
# --target server (default; the original recipe):
# 1.  Instrumented release build: RUSTFLAGS="-Cprofile-generate=<abs>/$PGO_DIR/raw"
#     into the separate target subdir target/pgo/gen (stale .profraw files from
#     earlier runs are removed first so they cannot leak into the merge).
# 2.  Deterministic training dumps (Overworld 64ch full j=1 & j=8, NOFEAT,
#     Nether 4242, End 0) written into $PGO_DIR/tmp and deleted immediately
#     after each run, on the instrumented binary.  The overworld-j8 arm is the
#     64-chunk seed-0 N=8 full dump; its sha256 is the reference digest.
# 3.  llvm-profdata merge -o $PGO_DIR/merged.profdata $PGO_DIR/raw.
# 4.  Optimized release build: RUSTFLAGS="-Cprofile-use=$PGO_DIR/merged.profdata
#     -Cllvm-args=-pgo-warn-missing-function" into target/pgo/use
#     (copied to --out when that was requested).
# 5.  Bit-identity check: the PGO binary's 64-chunk seed-0 full dump must have
#     the same sha256 as the instrumented binary's (step 2 reference), so the
#     check never goes stale when worldgen output legitimately changes.  With
#     --expected-sha <64-hex> the PGO binary must match that digest instead.
#
# --target client-port:
#     instrumented `cargo run --manifest-path client-port/Cargo.toml --bin
#     mesh_bench -- 500` (target/pgo/client-port/gen), merge into
#     target/pgo/client-port/merged.profdata, PGO-use release build of the
#     client-port `--lib` + `--bin mesh_bench` (target/pgo/client-port/use),
#     then `mesh_bench 50` must print "2037 quads, 8148 vertices".
#
# --target client-app:
#     instrumented `cargo test --manifest-path client-app/Cargo.toml --release
#     --offline --locked` (target/pgo/client-app/gen), merge into
#     target/pgo/client-app/merged.profdata, PGO-use release build of
#     `--bin mc-spine-client` (target/pgo/client-app/use).
#
# --target all: server, then client-port, then client-app.
#
# --native-cpu appends -Ctarget-cpu=native to RUSTFLAGS of every instrumented
# and optimized build (opt-in; the result is not portable to other CPUs).
#
# llvm-profdata is located as: the active toolchain's rustup sysroot
# lib/rustlib/*/bin/llvm-profdata, then every other installed rustup
# toolchain's sysroot (same path), then PATH, then /usr/bin/llvm-profdata*
# (and /usr/local/bin/llvm-profdata*).  The first candidate that can merge
# the raw profiles is used; when none is found or none can merge them the
# script exits non-zero with the actionable message
# "rustup component add llvm-tools".
#
# Exit codes: 0 success, 1 build/training/merge/verification failure,
# 2 usage error (unknown argument, unknown --target, malformed --expected-sha).
#
# Usage: tools/pgo_build.sh [--out <path>] [--dry-run] [--jobs N]
#                           [--target server|client-port|client-app|all]
#                           [--expected-sha <64-hex>] [--native-cpu]
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT="$(pwd)"

PGO_DIR="target/pgo"
GEN_DIR="$PGO_DIR/gen"
USE_DIR="$PGO_DIR/use"
RAW_DIR="$PGO_DIR/raw"
# Absolute paths for every path that ends up inside RUSTFLAGS: rustc resolves
# `-Cprofile-use` against the cwd of the process cargo spawns it in, and with
# `--target-dir` that is not the workspace root here (observed on rustc 1.98.0 +
# cargo 1.98.0: "file `target/pgo/merged.profdata` passed to `-C profile-use`
# does not exist" when passed relatively, even though the file exists from the
# workspace root).  `-Cprofile-generate` is resolved by the instrumented
# process at run time, and `cargo test` runs test binaries from the package
# directory, so the raw-profile directories are absolute too.
MERGED="$ROOT/target/pgo/merged.profdata"
TRAIN_DIR="$PGO_DIR/tmp"
BIN_GEN="$GEN_DIR/release/mc-spine"
BIN_USE="$USE_DIR/release/mc-spine"

CP_MANIFEST="client-port/Cargo.toml"
CP_DIR="$PGO_DIR/client-port"
CP_GEN_DIR="$CP_DIR/gen"
CP_USE_DIR="$CP_DIR/use"
CP_RAW_DIR="$CP_DIR/raw"
CP_MERGED="$ROOT/$CP_DIR/merged.profdata"
CP_BIN_USE="$CP_USE_DIR/release/mesh_bench"
CP_TRAIN_ARG=500
CP_VERIFY_ARG=50
CP_EXPECT="2037 quads, 8148 vertices"

CA_MANIFEST="client-app/Cargo.toml"
CA_DIR="$PGO_DIR/client-app"
CA_GEN_DIR="$CA_DIR/gen"
CA_USE_DIR="$CA_DIR/use"
CA_RAW_DIR="$CA_DIR/raw"
CA_MERGED="$ROOT/$CA_DIR/merged.profdata"
CA_BIN_USE="$CA_USE_DIR/release/mc-spine-client"

OUT=""
DRY_RUN=0
JOBS=2
TARGET="server"
EXPECTED_SHA=""
EXPECTED_SHA_SET=0
NATIVE_CPU=0

usage() {
  cat <<'USAGE'
Usage: tools/pgo_build.sh [--out <path>] [--dry-run] [--jobs N]
                          [--target server|client-port|client-app|all]
                          [--expected-sha <64-hex>] [--native-cpu]

Options:
  --out <path>          copy the PGO binary to <path> after building it
                        (server: mc-spine, client-port: mesh_bench,
                        client-app: mc-spine-client; with --target all <path>
                        is a directory that receives all three binaries)
  --dry-run             print the plan and the discovered llvm-profdata; build nothing
  --jobs N              cargo parallelism (default 2)
  --target T            what to PGO-build: server (default), client-port,
                        client-app or all (server, client-port, client-app)
  --expected-sha <hex>  64-hex sha256 the PGO server binary's 64-chunk seed-0
                        full dump must match (server/all only); default: match
                        the instrumented binary's own full dump bit for bit
  --native-cpu          add -Ctarget-cpu=native to RUSTFLAGS (non-portable)
  -h, --help            show this help
USAGE
}

usage_error() {
  echo "tools/pgo_build.sh: $*" >&2
  usage >&2
  exit 2
}

require_value() {
  # require_value <flag> [<value>...]: the flag needs a non-empty next argument.
  if [ "$#" -lt 2 ] || [ -z "$2" ]; then
    usage_error "$1 requires a value"
  fi
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --out) OUT="${2:?--out requires a path}"; shift 2 ;;
    --out=*) OUT="${1#*=}"; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --jobs) JOBS="${2:?--jobs requires a number}"; shift 2 ;;
    --jobs=*) JOBS="${1#*=}"; shift ;;
    --target) require_value "$@"; TARGET="$2"; shift 2 ;;
    --target=*) TARGET="${1#*=}"; shift ;;
    --expected-sha) require_value "$@"; EXPECTED_SHA="$2"; EXPECTED_SHA_SET=1; shift 2 ;;
    --expected-sha=*) EXPECTED_SHA="${1#*=}"; EXPECTED_SHA_SET=1; shift ;;
    --native-cpu) NATIVE_CPU=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage_error "unknown argument: $1" ;;
  esac
done

case "$TARGET" in
  server) TARGETS=(server) ;;
  client-port) TARGETS=(client-port) ;;
  client-app) TARGETS=(client-app) ;;
  all) TARGETS=(server client-port client-app) ;;
  *) usage_error "unknown --target '$TARGET' (expected server, client-port, client-app or all)" ;;
esac

if [ "$EXPECTED_SHA_SET" -eq 1 ]; then
  if ! [[ "$EXPECTED_SHA" =~ ^[0-9a-fA-F]{64}$ ]]; then
    usage_error "--expected-sha must be exactly 64 hex characters, got '$EXPECTED_SHA'"
  fi
  EXPECTED_SHA="$(printf '%s' "$EXPECTED_SHA" | tr 'A-F' 'a-f')"
  case "$TARGET" in
    server|all) ;;
    *) usage_error "--expected-sha only applies to --target server or all (got --target $TARGET)" ;;
  esac
fi

EXTRA_RUSTFLAGS=""
if [ "$NATIVE_CPU" -eq 1 ]; then
  EXTRA_RUSTFLAGS=" -Ctarget-cpu=native"
fi
gen_rustflags() { printf '%s' "-Cprofile-generate=$1$EXTRA_RUSTFLAGS"; }
use_rustflags() { printf '%s' "-Cprofile-use=$1 -Cllvm-args=-pgo-warn-missing-function$EXTRA_RUSTFLAGS"; }

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

final_binary() {
  # final_binary <target>: where that target's PGO binary ends up.
  local built
  case "$1" in
    server) built="$BIN_USE" ;;
    client-port) built="$CP_BIN_USE" ;;
    client-app) built="$CA_BIN_USE" ;;
  esac
  if [ -z "$OUT" ]; then
    printf '%s' "$built"
  elif [ "$TARGET" = all ]; then
    printf '%s' "${OUT%/}/$(basename "$built")"
  else
    printf '%s' "$OUT"
  fi
}

if [ "$DRY_RUN" -eq 1 ]; then
  echo "llvm-profdata candidates (priority order):"
  for c in "${PROFDATA_CANDIDATES[@]}"; do echo "  $c"; done
  echo "targets: ${TARGETS[*]}"
  echo "native cpu: $([ "$NATIVE_CPU" -eq 1 ] && echo "on (-Ctarget-cpu=native)" || echo off)"
  for t in "${TARGETS[@]}"; do
    case "$t" in
      server)
        echo "== target server =="
        echo "step 1 instrumented build: RUSTFLAGS='$(gen_rustflags "$ROOT/$RAW_DIR")' cargo build --release --offline --locked -j $JOBS --target-dir $GEN_DIR"
        echo "step 2 training dumps: overworld-j1, overworld-j8, nofeat-j8, nether-4242, end-0 -> $TRAIN_DIR (deleted after each run); overworld-j8 = 64-chunk seed-0 full dump reference from $BIN_GEN"
        echo "step 3 merge: first candidate that merges the .profraw files in $RAW_DIR -> $MERGED"
        echo "step 4 optimized build: RUSTFLAGS='$(use_rustflags "$MERGED")' cargo build --release --offline --locked -j $JOBS --target-dir $USE_DIR"
        if [ "$EXPECTED_SHA_SET" -eq 1 ]; then
          echo "step 5 full-dump bit-identity check: $BIN_USE against --expected-sha $EXPECTED_SHA"
        else
          echo "step 5 full-dump bit-identity check: $BIN_USE against the 64-chunk seed-0 full dump sha256 of $BIN_GEN"
        fi
        ;;
      client-port)
        echo "== target client-port =="
        echo "step 1 instrumented training: RUSTFLAGS='$(gen_rustflags "$ROOT/$CP_RAW_DIR")' cargo run --manifest-path $CP_MANIFEST --release --offline --locked -j $JOBS --target-dir $CP_GEN_DIR --bin mesh_bench -- $CP_TRAIN_ARG"
        echo "step 2 merge: first candidate that merges the .profraw files in $CP_RAW_DIR -> $CP_MERGED"
        echo "step 3 optimized build: RUSTFLAGS='$(use_rustflags "$CP_MERGED")' cargo build --manifest-path $CP_MANIFEST --release --offline --locked -j $JOBS --target-dir $CP_USE_DIR --lib --bin mesh_bench"
        echo "step 4 verify: $CP_BIN_USE $CP_VERIFY_ARG must print '$CP_EXPECT'"
        ;;
      client-app)
        echo "== target client-app =="
        echo "step 1 instrumented training: RUSTFLAGS='$(gen_rustflags "$ROOT/$CA_RAW_DIR")' cargo test --manifest-path $CA_MANIFEST --release --offline --locked -j $JOBS --target-dir $CA_GEN_DIR"
        echo "step 2 merge: first candidate that merges the .profraw files in $CA_RAW_DIR -> $CA_MERGED"
        echo "step 3 optimized build: RUSTFLAGS='$(use_rustflags "$CA_MERGED")' cargo build --manifest-path $CA_MANIFEST --release --offline --locked -j $JOBS --target-dir $CA_USE_DIR --bin mc-spine-client"
        echo "step 4 verify: $CA_BIN_USE exists and is executable"
        ;;
    esac
    echo "final binary ($t): $(final_binary "$t")"
  done
  exit 0
fi

case "$ROOT" in
  *[[:space:]]*)
    echo "tools/pgo_build.sh: repository path '$ROOT' contains whitespace, which RUSTFLAGS cannot carry; move the checkout" >&2
    exit 1
    ;;
esac
# CARGO_ENCODED_RUSTFLAGS takes precedence over RUSTFLAGS and would silently
# drop the profile flags.
unset CARGO_ENCODED_RUSTFLAGS

reset_raw_dir() {
  rm -rf "$1"
  mkdir -p "$1"
}

merge_profiles() {
  # merge_profiles <raw dir> <merged output>: first candidate that works.
  local raw="$1" merged="$2" f p
  local files=()
  while IFS= read -r f; do
    [ -n "$f" ] && files+=("$f")
  done < <(find "$raw" -type f -name '*.profraw' 2>/dev/null | LC_ALL=C sort)
  if [ "${#files[@]}" -eq 0 ]; then
    echo "tools/pgo_build.sh: no .profraw files under $raw" >&2
    exit 1
  fi
  mkdir -p "$(dirname "$merged")"
  for p in "${PROFDATA_CANDIDATES[@]}"; do
    echo "[pgo_build] merging: $p merge -o $merged (${#files[@]} .profraw files from $raw)"
    if "$p" merge -o "$merged" "${files[@]}"; then
      return 0
    fi
    echo "[pgo_build] $p could not merge the raw profiles; trying the next candidate" >&2
  done
  echo "tools/pgo_build.sh: no llvm-profdata could merge the raw profiles; install or upgrade one: rustup component add llvm-tools" >&2
  exit 1
}

copy_out() {
  # copy_out <target> <built binary>
  [ -n "$OUT" ] || return 0
  local dest
  dest="$(final_binary "$1")"
  mkdir -p "$(dirname "$dest")"
  cp -f "$2" "$dest"
}

# Same dump recipe as tools/perf_bench.py run_one(): MC_SPINE_WG_PROFILE=0,
# DUMP_N=8, DUMP_X=0, DUMP_Z=0, SEED/PRESET per arm.  Sets DUMP_SHA.
DUMP_SHA=""
run_dump() {
  local bin="$1" name="$2" preset="$3" seed="$4" nofeat="$5" jobs="$6"
  local path="$TRAIN_DIR/$name.bin" log="$TRAIN_DIR/$name.log"
  DUMP_SHA=""
  rm -f "$path"
  if ! env MC_SPINE_WG_PROFILE=0 MC_SPINE_SEED="$seed" MC_SPINE_WG_PRESET="$preset" \
      MC_SPINE_WG_DUMP="$path" MC_SPINE_WG_DUMP_N=8 MC_SPINE_WG_DUMP_X=0 MC_SPINE_WG_DUMP_Z=0 \
      MC_SPINE_GEN_JOBS="$jobs" ${nofeat:+MC_SPINE_NOFEAT=1} \
      "$bin" >"$log" 2>&1; then
    echo "tools/pgo_build.sh: dump $name failed on $bin (preset=$preset seed=$seed nofeat=$nofeat jobs=$jobs); last log lines:" >&2
    tail -n 20 "$log" >&2 || true
    rm -f "$path"
    exit 1
  fi
  if [ ! -f "$path" ]; then
    echo "tools/pgo_build.sh: dump $name on $bin exited 0 but wrote no $path" >&2
    exit 1
  fi
  DUMP_SHA="$(sha256sum "$path" | cut -d' ' -f1)"
  rm -f "$path" "$log"
}

build_server() {
  local raw_abs="$ROOT/$RAW_DIR"
  reset_raw_dir "$raw_abs"
  mkdir -p "$TRAIN_DIR"

  # ---- Step 1: instrumented release build into target/pgo/gen ---------------
  RUSTFLAGS="$(gen_rustflags "$raw_abs")" \
    cargo build --release --offline --locked -j "$JOBS" --target-dir "$GEN_DIR"

  # ---- Step 2: deterministic training workloads on the instrumented binary --
  local ref_sha=""
  local arm
  for arm in "overworld-j1 overworld 0 - 1" "overworld-j8 overworld 0 - 8" \
             "nofeat-j8 overworld 0 1 8" "nether-4242 nether 4242 - 8" "end-0 end 0 - 8"; do
    local name preset seed nofeat jobs
    read -r name preset seed nofeat jobs <<<"$arm"
    if [ "$nofeat" = "-" ]; then nofeat=""; fi
    run_dump "$BIN_GEN" "$name" "$preset" "$seed" "$nofeat" "$jobs"
    printf '[pgo_build] training dump %-12s sha256=%s\n' "$name" "$DUMP_SHA"
    # overworld-j8 is exactly the 64-chunk seed-0 N=8 full dump of step 5.
    if [ "$name" = overworld-j8 ]; then ref_sha="$DUMP_SHA"; fi
  done

  # ---- Step 3: merge raw profiles --------------------------------------------
  merge_profiles "$raw_abs" "$MERGED"

  # ---- Step 4: optimized release build into target/pgo/use ------------------
  RUSTFLAGS="$(use_rustflags "$MERGED")" \
    cargo build --release --offline --locked -j "$JOBS" --target-dir "$USE_DIR"

  # ---- Step 5: bit-identity check on the 64-chunk full dump -----------------
  run_dump "$BIN_USE" pgo-full-check overworld 0 "" 8
  local full_sha="$DUMP_SHA" expect source
  printf '[pgo_build] PGO full dump sha256=%s\n' "$full_sha"
  if [ "$EXPECTED_SHA_SET" -eq 1 ]; then
    expect="$EXPECTED_SHA"
    source="--expected-sha"
    if [ "$ref_sha" != "$EXPECTED_SHA" ]; then
      echo "[pgo_build] note: instrumented full dump sha256=$ref_sha differs from --expected-sha" >&2
    fi
  else
    expect="$ref_sha"
    source="the instrumented binary's full dump"
  fi
  if [ -z "$expect" ] || [ "$full_sha" != "$expect" ]; then
    echo "tools/pgo_build.sh: bit-identity check FAILED: got $full_sha, expected $expect ($source)" >&2
    exit 1
  fi
  echo "[pgo_build] bit-identity check PASS: PGO full dump matches $source"
  copy_out server "$BIN_USE"
  echo "[pgo_build] PGO binary: $(final_binary server)"
}

build_client_port() {
  local raw_abs="$ROOT/$CP_RAW_DIR"
  reset_raw_dir "$raw_abs"

  echo "[pgo_build] client-port: instrumented training run (mesh_bench $CP_TRAIN_ARG)"
  RUSTFLAGS="$(gen_rustflags "$raw_abs")" \
    cargo run --manifest-path "$CP_MANIFEST" --release --offline --locked -j "$JOBS" \
      --target-dir "$CP_GEN_DIR" --bin mesh_bench -- "$CP_TRAIN_ARG"

  merge_profiles "$raw_abs" "$CP_MERGED"

  RUSTFLAGS="$(use_rustflags "$CP_MERGED")" \
    cargo build --manifest-path "$CP_MANIFEST" --release --offline --locked -j "$JOBS" \
      --target-dir "$CP_USE_DIR" --lib --bin mesh_bench

  local out
  if ! out="$("$CP_BIN_USE" "$CP_VERIFY_ARG" 2>&1)"; then
    printf '%s\n' "$out" >&2
    echo "tools/pgo_build.sh: client-port verification run '$CP_BIN_USE $CP_VERIFY_ARG' failed" >&2
    exit 1
  fi
  printf '%s\n' "$out"
  case "$out" in
    *"$CP_EXPECT"*) ;;
    *)
      echo "tools/pgo_build.sh: client-port verification FAILED: '$CP_BIN_USE $CP_VERIFY_ARG' did not print '$CP_EXPECT'" >&2
      exit 1
      ;;
  esac
  echo "[pgo_build] client-port verification PASS: $CP_EXPECT"
  copy_out client-port "$CP_BIN_USE"
  echo "[pgo_build] PGO binary: $(final_binary client-port)"
}

build_client_app() {
  local raw_abs="$ROOT/$CA_RAW_DIR"
  reset_raw_dir "$raw_abs"

  echo "[pgo_build] client-app: instrumented training run (cargo test)"
  RUSTFLAGS="$(gen_rustflags "$raw_abs")" \
    cargo test --manifest-path "$CA_MANIFEST" --release --offline --locked -j "$JOBS" \
      --target-dir "$CA_GEN_DIR"

  merge_profiles "$raw_abs" "$CA_MERGED"

  RUSTFLAGS="$(use_rustflags "$CA_MERGED")" \
    cargo build --manifest-path "$CA_MANIFEST" --release --offline --locked -j "$JOBS" \
      --target-dir "$CA_USE_DIR" --bin mc-spine-client

  if [ ! -x "$CA_BIN_USE" ]; then
    echo "tools/pgo_build.sh: client-app build produced no executable $CA_BIN_USE" >&2
    exit 1
  fi
  copy_out client-app "$CA_BIN_USE"
  echo "[pgo_build] PGO binary: $(final_binary client-app)"
}

for t in "${TARGETS[@]}"; do
  echo "[pgo_build] ==== target $t ===="
  case "$t" in
    server) build_server ;;
    client-port) build_client_port ;;
    client-app) build_client_app ;;
  esac
done
echo "[pgo_build] done"
