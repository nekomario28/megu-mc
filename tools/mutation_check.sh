#!/usr/bin/env bash
# Break the implementation on purpose and require the named test to go red.
#
# The reason this exists: a test that cannot fail is worse than no test at all, because it occupies
# the place where a check should be and reports green. Two gates in this repository were exactly
# that. A test asserting that nineteen block names were classified as falling stayed green when the
# implementation was reverted to a hand list of three, because it read the registry instead of the
# behaviour; and a test that called the scheduler by hand before asserting the scheduling stayed
# green over a placement path that scheduled nothing.
#
# Each entry below is a mutation paired with the test that must notice it. A mutation that leaves its
# test green is reported as a failure of this script, not of the test suite.
#
# Run: bash tools/mutation_check.sh [--offline]
# Focused runs: MUTATION_FILTER=<test-filter-substring> (zero matches fail).
# Use MUTATION_RELEASE=1 and CARGO_BUILD_JOBS=1 for bounded release checks.
# MUTATION_STABILITY=1 makes every catch fail twice (0 or 1; anything else is an error).
#
# The campaign never mutates this tree. It copies its inputs into a private snapshot, points
# CARGO_TARGET_DIR inside that snapshot, and runs there; the copy and the target are removed on
# the way out. Before 2026-11-16 the script rewrote src/*.rs in place for the whole build window
# and a SIGKILL left the mutant (plus .mutation-backup) behind for the next build to compile.
# Measured: mutant bytes present for 0.621 s per entry under a fake cargo, and left behind after
# a kill (audit/test-causality-tc-20261116/probe-window.json). A lock cannot fix that: the write
# happens before the build, and a cargo or an editor that does not take the lock still reads the
# live file.
set -uo pipefail
cd "$(dirname "$0")/.."

# Strictness is a contract, not a truthiness test: "0" is not "on" and a typo is not "off".
case "${MUTATION_STABILITY:-0}" in
  0 | "") stability=0 ;;
  1) stability=1 ;;
  *)
    echo "MUTATION_STABILITY must be 0 or 1, got '$MUTATION_STABILITY'" >&2
    exit 2
    ;;
  esac

# What the campaign reads from the tree it measures: the crate, the harness, the data the suite
# loads, and the file the *server* reads from its working directory. The build
# `include_bytes!`s fixtures under `assets/`, `tests/` and `evidence/`, so a copy without
# `evidence/` does not compile (measured: 13 missing-fixture errors). `config_packets.bin` is not a
# build input, which is why it was missed until 2026-11-16: the wire helper's `local_server`
# symlinks it into the server's cwd and `src/play_protocol.rs::load_config_packets` reads it there, so a copy
# without it runs a server with empty registries and every wire gate then measures different inputs
# than this tree does. `src/main.rs::load_pack_info` used to read a second cwd file,
# `resourcepack.zip`; that file was removed as unprovenanced (see THIRD_PARTY.md) and the server
# now simply advertises no resource pack, so there is nothing to copy. Everything else (specs,
# .git, target, caches) is not an input and is not copied.
# The manifest also resolves three optional path crates (`create-offload`), so their
# manifests travel with the copy or cargo cannot read the dependency graph at all.
INPUTS=(Cargo.toml Cargo.lock rust-toolchain.toml src tests tools assets datapacks evidence \
    core proto mod-adapter-core create-adapter mod-offload state-store)
RUNTIME_INPUTS=(config_packets.bin)

fail() {
  echo "FAIL  $*" >&2
  exit 2
}

# One owner for "which bytes are the inputs". The copy, the manifest and the exclusions all read
# this one selection, so a file cannot be copied without being hashed. Until 2026-11-16 they were
# separate policies - `find -type f` for the manifest and `tar` over the whole directory for the
# copy - so a nested `target/` or `__pycache__` under an input directory was copied and never
# hashed.
#
# Emits `<kind>\t<path>`: `file` for a regular file, `link` for a symlink. A symlink is neither
# copied nor followed: a mutable `src/foo.rs` link travels back to a file outside the copy, and the
# mutant is written with `pathlib.write_text`, so the origin of a link is not ours to mutate.
# Failing closed is the contract; a tree that needs links uses regular files instead.
selection() {  # $1 root
  local root="$1" path
  (
    cd "$root" || exit 1
    for path in "${INPUTS[@]}" "${RUNTIME_INPUTS[@]}"; do
      if [ -L "$path" ]; then
        printf 'link\t%s\n' "$path"
      elif [ -d "$path" ]; then
        find "$path" -not -path '*/__pycache__/*' -not -name '*.pyc' \
          -not -name '*.mutation-backup' -not -path '*/target/*' -not -path '*/.git/*' \
          \( -type f -printf 'file\t%p\n' -o -type l -printf 'link\t%p\n' \) || exit 1
      elif [ -f "$path" ]; then
        printf 'file\t%s\n' "$path"
      fi
    done
  )
}

# Deterministic identity of a selection: path and bytes, sorted. A failed read or hash has to be a
# failure of the manifest, not a partial or empty answer that compares equal to another failed one.
manifest() {  # $1 root, $2 file list, $3 output
  local root="$1" list="$2" out="$3"
  [ -s "$list" ] || return 1
  ( cd "$root" && xargs -r -d '\n' sha256sum < "$list" | LC_ALL=C sort > "$out" )
}

# A path that may not exist yet, resolved physically and without creating anything: a symlinked
# MUTATION_SNAPSHOT_PARENT has to be refused before a directory or a copy is made, and a lexical
# comparison cannot see the alias.
physical_path() {  # $1 path
  local path="$1" parts=() part base
  while [ ! -e "$path" ] && [ "$path" != "/" ] && [ "$path" != "." ]; do
    parts=("$(basename "$path")" ${parts[@]+"${parts[@]}"})
    path=$(dirname "$path") || return 1
  done
  base=$(cd "$path" 2>/dev/null && pwd -P) || return 1
  for part in ${parts[@]+"${parts[@]}"}; do base="$base/$part"; done
  printf '%s\n' "$base"
}

# The snapshot. One invocation: copy the inputs, prove the copy, then continue the campaign in this
# same shell with its cwd and its CARGO_TARGET_DIR inside the snapshot.
#
# Until 2026-11-16 this re-entered the copy's own script through MUTATION_SNAPSHOT_ROOT, and that
# variable was an entry point: `MUTATION_SNAPSHOT_ROOT=$PWD` skipped the snapshot entirely and put
# the campaign back on the live tree with the caller's CARGO_TARGET_DIR, which is exactly the bug
# the snapshot exists to prevent. It is refused here, before anything is created, copied or
# measured.
if [ -n "${MUTATION_SNAPSHOT_ROOT:-}" ]; then
  echo "refusing to run: MUTATION_SNAPSHOT_ROOT marked the old internal recursion, not an entry" >&2
  echo "      point, and a campaign honours no such request. Nothing was copied or measured." >&2
  exit 2
fi

source_root=$(pwd -P) || fail "could not resolve the working tree"
if ls src/*.mutation-backup >/dev/null 2>&1; then
  echo "refusing to run: a previous mutation was left applied in the source tree:" >&2
  ls src/*.mutation-backup >&2
  echo 'restore with: for f in src/*.mutation-backup; do cp "$f" "${f%.mutation-backup}" && touch "${f%.mutation-backup}" && rm "$f"; done' >&2
  exit 2
fi
if [ ! -d "$source_root/src" ]; then
  echo "refusing to run: no src/ under $source_root, so there is nothing to measure" >&2
  exit 2
fi

snapshot_parent=${MUTATION_SNAPSHOT_PARENT:-${XDG_CACHE_HOME:-$HOME/.cache}/megu/mutation-runs}
resolved_parent=$(physical_path "$snapshot_parent") ||
  fail "could not resolve the snapshot parent $snapshot_parent"
snapshot_parent=$resolved_parent
case "$snapshot_parent/" in
  "$source_root"/*)
    echo "refusing to run: the snapshot parent $snapshot_parent is inside the tree it copies." >&2
    echo "      Nothing was created or measured; point MUTATION_SNAPSHOT_PARENT elsewhere." >&2
    exit 2
    ;;
esac
mkdir -p "$snapshot_parent" || fail "could not create $snapshot_parent"
# A cold private target measured 853 MiB for this crate, and the copy is 82 MiB, so a campaign
# needs roughly 1 GiB and must not start with less than 1.5 GiB of headroom. /tmp is a small
# tmpfs on some hosts, which is why the default parent is the cache directory and not TMPDIR.
avail_kb=$(df -Pk "$snapshot_parent" | awk 'NR == 2 { print $4 }')
if [ "${avail_kb:-0}" -lt 1572864 ]; then
  echo "refusing to run: $snapshot_parent has $(( ${avail_kb:-0} / 1024 )) MiB free and a cold" >&2
  echo "private target needs about 1 GiB. Free space, or set MUTATION_SNAPSHOT_PARENT." >&2
  exit 2
fi
snapshot=$(mktemp -d "$snapshot_parent/run.XXXXXXXX") || fail "could not create a private snapshot"
resolved=$(cd "$snapshot" && pwd -P) || fail "could not resolve the private snapshot $snapshot"
snapshot=$resolved
case "$snapshot/" in
  "$source_root"/*)
    echo "refusing to run: the snapshot $snapshot is inside the tree it copies" >&2
    rm -rf "$snapshot"
    exit 2
    ;;
esac
work=$(mktemp -d) || fail "could not create a scratch directory for the manifests"
cleanup() {
  [ -n "${snapshot:-}" ] && rm -rf "$snapshot"
  [ -n "${work:-}" ] && rm -rf "$work"
}
trap cleanup EXIT
# EXIT runs on these, so the snapshot and the scratch directory are removed on a signal too.
trap 'exit 130' INT
trap 'exit 143' TERM

if ! selection "$source_root" > "$work/selection"; then
  echo "FAIL  the input set under $source_root could not be enumerated; a find or a permission" >&2
  echo "      error is not an empty tree. Nothing was copied or measured." >&2
  exit 2
fi
if [ ! -s "$work/selection" ]; then
  echo "refusing to run: the input set under $source_root is empty, so there is nothing to measure" >&2
  exit 2
fi
if grep -q $'^link\t' "$work/selection"; then
  echo "refusing to run: the inputs contain symlinks, which cannot be copied without leaving a" >&2
  echo "      path back into the tree being mutated. Make these regular files:" >&2
  grep $'^link\t' "$work/selection" | cut -f2- | sed 's/^/      /' >&2
  exit 2
fi
if ! cut -f2- "$work/selection" > "$work/files"; then
  fail "the input selection could not be reduced to paths"
fi
for path in "${RUNTIME_INPUTS[@]}"; do
  if [ ! -f "$source_root/$path" ]; then
    echo "refusing to run: $path is missing from $source_root. src/play_protocol.rs reads it from the" >&2
    echo "      working directory, so the wire gates would measure a server without it. Restore" >&2
    echo "      the file instead of copying a tree that does not have it." >&2
    exit 2
  fi
done

if ! manifest "$source_root" "$work/files" "$work/before"; then
  echo "FAIL  the input manifest of $source_root could not be taken (a read or a hash error)." >&2
  echo "      Nothing was copied or measured." >&2
  exit 2
fi
if [ ! -s "$work/before" ]; then
  fail "the input manifest of $source_root is empty, so a comparison would prove nothing"
fi
selected=$(wc -l < "$work/files") || fail "could not count the input selection"
hashed=$(wc -l < "$work/before") || fail "could not count the input manifest"
if [ "$selected" -ne "$hashed" ]; then
  echo "FAIL  $selected inputs were selected but $hashed were hashed; a failed read must not" >&2
  echo "      shrink the manifest that is compared against the copy." >&2
  exit 2
fi
if ! tar -cf - -C "$source_root" -T "$work/files" | tar -xf - -C "$snapshot"; then
  echo "FAIL  the private snapshot did not copy; nothing was measured." >&2
  exit 2
fi
if ! manifest "$snapshot" "$work/files" "$work/copied"; then
  echo "FAIL  the copy's manifest could not be taken (a read or a hash error). A failing hash is" >&2
  echo "      not a matching copy. Nothing was measured." >&2
  exit 2
fi
if ! manifest "$source_root" "$work/files" "$work/after"; then
  echo "FAIL  the source's manifest could not be re-taken after the copy (a read or a hash error)." >&2
  echo "      Nothing was measured." >&2
  exit 2
fi
if ! cmp -s "$work/before" "$work/copied"; then
  echo "FAIL  the snapshot is not byte-identical to the source it was copied from:" >&2
  diff "$work/before" "$work/copied" | head -20 >&2
  exit 2
fi
if ! cmp -s "$work/before" "$work/after"; then
  echo "FAIL  the source tree changed while it was being copied, so a campaign here would mix" >&2
  echo "      two revisions. Nothing was measured; re-run when the tree is quiet." >&2
  diff "$work/before" "$work/after" | head -20 >&2
  exit 2
fi

if ! identity=$(sha256sum < "$work/before" | cut -d' ' -f1); then
  fail "could not reduce the input manifest to an identity"
fi
echo "mutation check: private snapshot $snapshot"
echo "  source   $source_root"
echo "  identity $identity ($selected files: ${INPUTS[*]} ${RUNTIME_INPUTS[*]})"
echo "  target   $snapshot/target (the caller's CARGO_TARGET_DIR is not used)"
echo "  the snapshot is removed on exit; a killed run can leave it behind and only there"

# "The manifests matched" is not the same claim as "the inputs are in the copy": both sides read
# one selection, so an input the selection forgot is absent from both. These check the input set
# and the server's cwd inputs against the copy by name and by hash.
for path in "${INPUTS[@]}" "${RUNTIME_INPUTS[@]}"; do
  if [ -e "$source_root/$path" ] && [ ! -e "$snapshot/$path" ]; then
    echo "FAIL  $path is an input of this tree but is not in the snapshot; the copy dropped it." >&2
    exit 2
  fi
done
for path in "${RUNTIME_INPUTS[@]}"; do
  if ! source_sha=$(sha256sum "$source_root/$path" | cut -d' ' -f1); then
    fail "could not hash $source_root/$path"
  fi
  if ! copy_sha=$(sha256sum "$snapshot/$path" | cut -d' ' -f1); then
    fail "could not hash $snapshot/$path"
  fi
  case "${source_sha:-}" in
    '' | *[!0-9a-f]*) fail "$path: the source hash is not a digest" ;;
  esac
  if [ "$source_sha" != "$copy_sha" ]; then
    echo "FAIL  $snapshot/$path is not byte-identical to $source_root/$path" >&2
    exit 2
  fi
  echo "  wire input $path $copy_sha (read from the server's cwd by src/play_protocol.rs)"
  unset source_sha copy_sha
done

cd "$snapshot" || fail "could not enter the private snapshot"
export CARGO_TARGET_DIR="$snapshot/target"

CARGO_FLAGS=(--locked -j "${CARGO_BUILD_JOBS:-2}")
if [ "${MUTATION_RELEASE:-0}" = 1 ]; then
  CARGO_FLAGS+=(--release)
fi
if [ "${1:-}" = "--offline" ] || [ "${CARGO_OFFLINE:-0}" = "1" ]; then
  CARGO_FLAGS=(--offline "${CARGO_FLAGS[@]}")
fi

# A mutation that is left applied is worse than no check at all: the next build compiles a source
# nobody chose, and a red suite then points at tests that are fine. This happened while writing this
# script - two runs reported three falling tests failing, against a tree that still had a mutation
# in it, and the failure did not reproduce once the source was rebuilt. The snapshot above already
# refuses to start over a leftover; `restore` puts every entry's bytes back on the exit paths this
# script controls, and `verify_restored` checks that it worked rather than trusting it.
passed=0
failed=0
skipped=0
parked=0
log=$(mktemp)
artifacts=$(mktemp)
declare -A baseline_ok=()

# Content hash of the file as it was before the mutation. Restoring the bytes is the point;
# restoring a timestamp is not, and neither is trusting that the restore worked. A mutation that
# survives in the tree is worse than a failing check, so this is verified rather than assumed.
hash_of() { sha256sum "$1" | cut -d' ' -f1; }

# A caught mutant whose failing assertion is thrown away is a count, not evidence. With
# MUTATION_LOG_DIR set, keep the run's own output next to the caller's receipt: the mutant
# log is what proves the named test actually went red, and the restored log is the control.
keep_log() {
  [ -n "${MUTATION_LOG_DIR:-}" ] || return 0
  mkdir -p "$MUTATION_LOG_DIR"
  local slug
  slug=$(printf '%s' "$1" | tr -cs 'A-Za-z0-9._-' '-' | sed 's/^-//; s/-$//')
  cp "$log" "$MUTATION_LOG_DIR/$slug.$2.log"
}

restore() {
  if [ -n "${backup:-}" ] && [ -f "$backup" ]; then
    # Copy, then touch, rather than mv. `mv` gives the file back the backup's timestamp, which is
    # older than the test binary just built from the mutated source, so cargo decides it is up to
    # date and keeps using that binary. Every test run after a mutation check then measures the
    # mutation - which is exactly how three falling tests appeared to fail deterministically at
    # eight threads, and why blaming "a leftover mutation" was right about the cause and wrong
    # about the mechanism. The source is correct on disk; the binary was not rebuilt from it.
    cp "$backup" "$mut_file"
    rm -f "$backup"
    touch "$mut_file"
  fi
}
# mid-mutation, in the snapshot, which `cleanup` removes anyway; the snapshot is the only copy the
# campaign ever writes to.
trap 'restore; rm -f "$log" "$artifacts"; cleanup' EXIT

# One execution owner for positive controls and mutants. Status is local to check().
run_filter() {
  local filter="$1" executable
  compiled=yes
  status=failed
  if [[ "$filter" == tests/*.py ]]; then
    if cargo build ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} --message-format=json-render-diagnostics >"$artifacts" 2>"$log"; then
      if executable=$(python3 tools/cargo_executable.py <"$artifacts" 2>>"$log"); then
        if python3 "$filter" "$executable" >>"$log" 2>&1; then status=green; fi
      fi
    else
      compiled=no
    fi
  elif cargo test ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} "$filter" -- --test-threads=1 >"$log" 2>&1; then
    status=green
  fi
}

control() {
  local name="$1" filter="$2" phase="$3" pattern
  run_filter "$filter"
  if [[ "$filter" == tests/*.py ]]; then
    pattern='^((client settings|gameplay|containers|dimension) wire: PASS|population plan: PASS|mineshaft dispatch: PASS)$'
  else
    pattern='^test result: ok\. [1-9][0-9]* passed; 0 failed; 0 ignored'
  fi
  if [ "$status" = green ] && grep -qE "$pattern" "$log"; then keep_log "$name" "$phase"; return 0; fi
  echo "FAIL  $name: $phase control did not pass the required test"
  tail -15 "$log"
  return 1
}

# One owner for "what did this run actually do", used for the first run and for the stability
# re-run alike. Before 2026-11-16 the re-run asked only whether its output was *green*, so a run
# that never compiled and a run whose harness crashed both counted as the second failure that
# proves a catch: eleven deterministic scenarios against the unchanged script, two of them
# (second run compile error, second run SIGSEGV) recorded as proved (measured, see
# audit/test-causality-followup-20261116/stability-contract.json).
#
# verdict:
#   green    the selected test(s) passed -- not a catch
#   caught   a selected test reported a failure: `test result: FAILED` with at least one failed
#            test, or, for a wire gate, the exact line it was registered to print
#   compile  the mutation did not build, so no assertion ran
#   zero     the filter matched no test
#   ignored  the selected test is #[ignore]d
#   crash    non-zero exit that reported neither a failing test nor the expected wire line
classify() {
  local filter="$1" expected_failure="$2"
  verdict=crash
  # cargo prints "error: test failed" for a failing test, so the exit code and the word "error"
  # cannot tell a compile failure from an assertion failure. The lines the harness itself prints
  # can: a rustc diagnostic, or cargo's own "could not compile" summary.
  if [ "$compiled" = no ] || grep -qE '^error\[|^error: could not compile' "$log"; then
    verdict=compile
    return
  fi
  if grep -qE 'running 0 tests' "$log"; then
    verdict=zero
    return
  fi
  if [ "$status" = green ]; then
    # The test ran nothing because it is parked with #[ignore]. That is neither a gate nor a silent
    # pass: it is a named gap, and it has to be loud in the summary or parking becomes a way to make
    # this job green.
    if grep -qE 'test result: ok\. 0 passed; 0 failed; [1-9][0-9]* ignored' "$log"; then
      verdict=ignored
    else
      verdict=green
    fi
    return
  fi
  if [[ "$filter" == tests/*.py ]]; then
    # A wire gate names the exact line it must print. Any other non-zero exit is a broken run and
    # not a catch, however red the output happens to look.
    if [ -n "$expected_failure" ] && grep -Fxq "$expected_failure" "$log"; then
      verdict=caught
    fi
    return
  fi
  if grep -qE 'test result: FAILED\. [0-9]+ passed; [1-9][0-9]* failed' "$log"; then
    verdict=caught
  fi
}

# check <name> <file> <from> <to> <test-filter-or-wire-script> [exact-wire-failure]
check() {
  local name="$1" mut_file_local="$2" from="$3" to="$4" test_filter="$5" expected_failure="${6:-}"
  if [ -n "${MUTATION_FILTER:-}" ] && [[ "$test_filter" != *"$MUTATION_FILTER"* ]]; then
    return
  fi
  local status compiled caught=no verdict
  mut_file="$mut_file_local"
  backup="$mut_file.mutation-backup"
  local sha_before
  sha_before=$(hash_of "$mut_file")
  # A test filter already proved clean in this run against the verified unmutated source
  # does not need its baseline re-executed before every subsequent mutant; every mutant still
  # runs `restored` after `verify_restored` to rebuild and re-prove the unmutated binary.
  if [ -z "${baseline_ok[$test_filter]+x}" ]; then
    if ! control "$name" "$test_filter" baseline; then
      failed=$((failed + 1))
      return
    fi
    baseline_ok["$test_filter"]=1
  fi
  cp "$mut_file" "$backup"

  if ! MUT_FILE="$mut_file" MUT_FROM="$from" MUT_TO="$to" python3 - <<'PY'
import os, pathlib, sys
p = pathlib.Path(os.environ['MUT_FILE'])
s = p.read_text()
frm, to = os.environ['MUT_FROM'], os.environ['MUT_TO']
if s.count(frm) != 1:
    sys.exit(2)
p.write_text(s.replace(frm, to, 1))
PY
  then
    echo "SKIP  $name"
    echo "      anchor is not present exactly once in $mut_file; the implementation moved, so this"
    echo "      mutation no longer describes it. Update the entry rather than deleting it."
    skipped=$((skipped + 1))
    restore
    verify_restored "$name" "$sha_before"
    return
  fi

  run_filter "$test_filter"
  keep_log "$name" mutant
  classify "$test_filter" "$expected_failure"
  case "$verdict" in
    compile)
      echo "FAIL  $name"
      echo "      the mutation did not compile, so no assertion ran. Narrow it."
      failed=$((failed + 1))
      ;;
    zero)
      echo "FAIL  $name"
      echo "      '$test_filter' matched no test, so nothing was checked."
      failed=$((failed + 1))
      ;;
    ignored)
      echo "PARK  $name"
      echo "      '$test_filter' is #[ignore]d, so the mutation had nothing to hit. See specs/PLAN.md."
      parked=$((parked + 1))
      ;;
    green)
      echo "FAIL  $name"
      echo "      '$test_filter' stayed green with the implementation broken. That test is not a gate."
      failed=$((failed + 1))
      ;;
    caught) caught=yes ;;
    *)
      echo "FAIL  $name"
      echo "      the run neither compiled, matched, nor reported a failing test."
      tail -15 "$log"
      failed=$((failed + 1))
      ;;
  esac
  # A failing test is not proof that the mutation caused the failure. Measured 2026-11-11: three
  # entries were recorded as proved only because a load-sensitive test happened to fail in that
  # run; re-running the same campaign reported them as not a gate. With MUTATION_STABILITY=1 the
  # mutant has to fail a second time, and the second run is classified by the same owner: a re-run
  # that did not report a failing test is an unproven result, not a catch and not a flake. Costs
  # one extra run per caught mutant.
  if [ "$caught" = yes ] && [ "$stability" = 1 ]; then
    run_filter "$test_filter"
    keep_log "$name" mutant-rerun
    classify "$test_filter" "$expected_failure"
    if [ "$verdict" != caught ]; then
      caught=no
      failed=$((failed + 1))
      echo "FAIL  $name"
      if [ "$verdict" = green ]; then
        echo "      the mutant run failed once and passed on a re-run, so the failure was a flake,"
        echo "      not a catch. The test is not a stable gate."
      else
        echo "      the mutant run failed once, but the re-run did not report a failing test"
        echo "      ($verdict), so the first failure is unproven: an infrastructure result, not a catch."
      fi
    fi
  fi
  restore
  verify_restored "$name" "$sha_before"
  if ! control "$name" "$test_filter" restored; then
    unset "baseline_ok[$test_filter]"
    if [ "$caught" = yes ]; then failed=$((failed + 1)); fi
  elif [ "$caught" = yes ]; then
    echo "ok    $name"
    passed=$((passed + 1))
  fi
}

# A restore that did not put the bytes back leaves the tree mutated, and every later measurement is
# of a source nobody chose. Checking the hash is the only way to know, because the failure mode is
# silent: the file compiles, the suite runs, and the answer is about the mutation. There is no
# `git checkout --` fallback here on purpose: this file may be a copy inside someone else's
# worktree, and a checkout would reach the repository behind it. The owned backup is the only
# recovery, and if that failed the run stops.
verify_restored() {
  local name="$1" want="$2" got
  got=$(hash_of "$mut_file")
  if [ "$want" != "$got" ]; then
    echo "BROKEN $name"
    echo "      $mut_file did not come back byte-identical after the mutation."
    echo "      This campaign runs on the private snapshot $snapshot; the tree you"
    echo "      edited was not touched. Stopping rather than measuring a mutated copy."
    exit 3
  fi
}

check "an unclipped tree rejects a clipped top" \
  src/tree.rs 'top >= height || min_clipped.is_some_and(|min| top >= min)' \
  'true' \
  tree::tests::a_clipped_tree_needs_a_min_clipped_height_that_the_clip_reaches

check "a tree parser reads the pinned dirt provider" \
  src/tree.rs 'dirt_provider: js(j, "dirt_provider")
            .or_else(|| js(j, "below_trunk_provider"))' \
  'dirt_provider: js(j, "below_trunk_provider")' \
  tree::tests::a_tree_reads_the_dirt_provider_field_the_pinned_data_uses

check "surface material does not reach deep solids" \
  src/worldgen.rs 'fn surface_material_applies(y: i32, surface_top: i32) -> bool {
    y >= surface_top.saturating_sub(4)
}' \
  'fn surface_material_applies(_y: i32, _surface_top: i32) -> bool {
    false
}' \
  worldgen::wg_tests::the_surface_material_window_stops_at_deep_solids

check "a non-fluid aquifer state above nonpositive density stays solid" \
  src/worldgen.rs '    } else {
        SurfaceCell::Solid
    }
}' \
  '    } else {
        SurfaceCell::Air
    }
}' \
  worldgen::wg_tests::a_nonpositive_density_with_an_aquifer_barrier_is_still_solid

check "a chunk claims its carver pass once" \
  src/worldgen.rs '        if let Some(rec) = self.carve_c.get_mut(&key) {
            if rec.blocks_live {
                return false;
            }' \
  '        if let Some(rec) = self.carve_c.get_mut(&key) {' \
  feature::generation_contract_tests::a_chunk_claims_its_carver_pass_once

check "a carve claim does not outlive the block output it stands for" \
  src/worldgen.rs '        if let Some(rec) = self.carve_c.get_mut(&key) {
            rec.blocks_live = false;
        }' \
  '' \
  worldgen::wg_tests::repeated_stage_updates_keep_one_slot_per_chunk

check "the feature bucket bound does not truncate the WG retention to 384" \
  src/worldgen.rs '        if let Some(rec) = self.carve_c.get_mut(&key) {
            rec.blocks_live = false;
        }' \
  '        self.carve_c.remove(&key);' \
  worldgen::wg_tests::post_carve_wg_outlives_the_feature_cache_but_not_its_claim

check "an evicted post-carve WG value takes its claim with it" \
  src/worldgen.rs '        self.carve_c.remove(&key);
        self.forget_feature_bucket(key);' \
  '        self.forget_feature_bucket(key);' \
  worldgen::wg_tests::the_wg_retention_bound_drops_the_value_and_the_claim_together

check "the fancy centre layer is half the trunk half-height" \
  src/tree.rs '    if g == 0.0 {
        // Vanilla assigns `h2 = f` for the exact centre layer, then still
        // returns `h2 * 0.5`; returning h2 here made the centre limb twice as
        // long and moved its candidate by a cell.
        h2 * 0.5' \
  '    if g == 0.0 {
        h2' \
  tree::tests::the_centre_fancy_layer_is_half_the_trunk_half_height

check "a fancy branch treats replaceables and logs as free" \
  src/tree.rs 'fn fancy_branch_open(reg: &BReg, cur: u32) -> bool {
    tree_tables(reg).is_replaceable_or_log(reg, cur)
}' \
  'fn fancy_branch_open(reg: &BReg, cur: u32) -> bool {
    tree_tables(reg).is_replaceable_by_trees(reg, cur)
}' \
  tree::tests::a_fancy_branch_passes_through_replaceables_and_logs_only

check "a fancy tree starts its attachment list below the branch end" \
  src/tree.rs 'list.push(((start.0, start.1 + j - 5, start.2), m));' \
  'list.push(((start.0, start.1 + k, start.2), m));' \
  feature_population_output_matches_the_pre_refactor_capture

check "the tree path passes the dirt provider to the trunk placer" \
  src/tree.rs '        &t.dirt_provider,' \
  '        &None,' \
  feature_population_output_matches_the_pre_refactor_capture

check "the tree path uses the clip decision" \
  src/tree.rs 'if !clip_allows_top(top, height, t.min_clipped) {' \
  'if false && !clip_allows_top(top, height, t.min_clipped) {' \
  feature_population_output_matches_the_pre_refactor_capture

# The default population path is a queue that decorates a request set's three rings; a lookup that
# "claims" the chunk without running the population input at all leaves an empty feature layer, and
# the discipline's cross-chunk writes are what make an order observable in returned values.
check "lazy feature lookup applies its population input and retains cached overrides" \
  src/worldgen.rs '                if nbr {
                    self.populate_request_box(cx, cz, cx, cz, &mut populate);
                } else {' \
  '                if nbr {
                    let _ = (&mut populate, cx, cz);
                    self.pop_done.insert((cx, cz));
                } else {'  \
  worldgen::wg_tests::default_dispatch_keeps_discipline_order_and_cached_overrides
check "a placed gravity block is scheduled by its own cell" \
  src/main.rs '    for (px, py, pz) in [(x, y, z), (x, y.saturating_add(1), z)] {' \
  '    for (px, py, pz) in [(x, y.saturating_add(1), z), (x, y.saturating_add(1), z)] {' \
  every_falling_block_becomes_an_entity_and_lands

check "scheduled falling conversion queues air, not a ghost" \
  src/main.rs '        note_block_update(px, py, pz, 0);
        let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);' \
  '        note_block_update(px, py, pz, st);
        let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);' \
  placing_unsupported_sand_queues_air_not_a_ghost

check "a block above a removed support is scheduled" \
  src/main.rs '    for (px, py, pz) in [(x, y, z), (x, y.saturating_add(1), z)] {' \
  '    for (px, py, pz) in [(x, y, z), (x, y, z)] {' \
  removing_a_support_schedules_the_block_above_after_its_own_booking_fired

check "block light uses generated sources, not only the overlay" \
  src/main.rs '                sources.push((*p, *lum));' \
  '                let _ = (p, lum);' \
  block_light_uses_generated_sources

check "block light sees a neighbour's generated source" \
  src/main.rs '                let Some(g) = gen.get(&(cx + dx, cz + dz)) else {' \
  '                let Some(g) = gen.get(&(cx, cz)) else {' \
  block_light_sees_a_neighbours_generated_source

check "a removed generated emitter stops lighting" \
  src/main.rs '                    if guard
                        .get(&(dim, p.0 >> 4, p.2 >> 4))
                        .and_then(|c| c.get(&local_idx(p.0, p.1, p.2)))
                        .is_some()
                    {' \
  '                    if false {' \
  a_removed_generated_emitter_stops_lighting
check "sky spill loses one level per step" \
  src/main.rs '            let nl = level.saturating_sub(opacity(qx, qy, qz).max(1));' \
  '            let nl = level;' \
  sky_light_reaches_sideways_through_an_opening

# The BFS above and the nibble packer have their own gates; what nothing called was the assembly
# that wires them together (3x3 floors, `floor_of`, the spill) -- only the two production call
# sites did. Dropping the spill at the call site left every older sky-light assertion green
# (measured 2026-11-16, registry_tests::sky_light_reads_lit_at_the_surface_and_dark_under_it),
# which is the false green this entry closes.
check "the light packet assembles spill into its floors" \
  src/main.rs '    let spill = sky_spill(dim, cx, cz, floor_of, op);' \
  '    let spill = Default::default();' \
  registry_tests::sky_light_assembly_lights_through_an_opening_and_stops_at_a_barrier

check "a knocked-out gravity block becomes an entity" \
  src/main.rs '        let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
        falling().lock().unwrap_or_else(|e| e.into_inner()).push(Fall {
            eid,
            x: px as f64 + 0.5,
            y: py as f64,
            z: pz as f64 + 0.5,
            dy: 0.0,
            state: st,
            time: 0,
        });' \
  '        let _eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);' \
  concurrent_knockout_and_falling_conserve_blocks
check "light sections start one below the world" \
  src/main.rs 'const LIGHT_MIN_Y: i32 = MIN_Y - 16;' \
  'const LIGHT_MIN_Y: i32 = MIN_Y;' \
  light_sections_start_one_below_the_world

check "a falling block passes through replaceable blocks" \
  src/main.rs '    reg.is_air(st) || reg.is_liquid(st) || reg.is_replaceable(st)' \
  '    reg.is_air(st) || reg.is_liquid(st)' \
  a_falling_block_passes_through_a_replaceable_block

# Removed 2026-11-11: this entry mutated UPD_DEFER's push, which is reached only at
# neighbour-update depth 64 or more. The two-game-tick falling model converts one
# block per callback, so no gravity scenario reaches that depth and the mutant
# survived the column test. The deferral branch is now untested rather than
# falsely covered; a cascade that can still exceed the depth would need its own
# test and entry.
check "a passing entity must not suppress a new block in the same cell" \
  src/main.rs '        if !set_block_if(px, py, pz, 0, Some(st)) {
            return;
        }' \
  '        if falling().lock().unwrap().iter().any(|f| f.x.floor() == px as f64 && f.y.floor() == py as f64 && f.z.floor() == pz as f64) { return; } if !set_block_if(px, py, pz, 0, Some(st)) {
            return;
        }' \
  a_new_block_in_a_passing_entity_cell_still_falls
check "the falling set comes from the asset, not a hand list" \
  src/main.rs '        if reg.behaviour_of(st) != crate::block_registry::B_FALLING {
            continue;
        }' \
  '        if !matches!(st, 112 | 117 | 118) {
            continue;
        }' \
  every_falling_block_becomes_an_entity_and_lands
check "gravity is 0.04 per tick, not doubled" \
  src/main.rs '            f.dy -= 0.04;' \
  '            f.dy -= 0.08;' \
  sand_falls_under_vanilla_gravity_until_it_lands

check "a landed block is placed on the support's top face" \
  src/main.rs '                landings.push((f, bx, support + 1, bz));' \
  '                landings.push((f, bx, support + 2, bz));' \
  sand_falls_under_vanilla_gravity_until_it_lands

check "a fast fall sweeps for a thin platform" \
  src/main.rs '            let old_floor = f.y.floor() as i32;' \
  '            let old_floor = new_y.floor() as i32;' \
  a_fast_falling_block_lands_on_a_thin_platform_it_crosses

check "a falling tick leaves the queue visible to a save" \
  src/main.rs '    let active: Vec<Fall> = falling().lock().unwrap_or_else(|e| e.into_inner()).clone();' \
  '    let active: Vec<Fall> = std::mem::take(&mut *falling().lock().unwrap_or_else(|e| e.into_inner()));' \
  beginning_a_falling_tick_leaves_the_queue_visible

check "a save is serialized with the falling transition" \
  src/save_v2.rs '    let _save = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());' \
  '    let _save = ();' \
  a_save

echo
check "the falling entities are written into the save" \
  src/save_v2.rs '(*f).extend_from_slice(b"FAL2");' \
  '(*f).extend_from_slice(b"XXXX");' \
  the_falling_entities_survive_a_save_and_load_round_trip

check "a migrated legacy cell is explicitly air" \
  src/save_v2.rs '                .insert(local_idx(fx, fy, fz), 0);' \
  '                .insert(local_idx(fx, fy, fz), 1);' \
  a_legacy_fal1_save_converts_its_blocks_to_entities

check "a block that drops twice keeps its second drop" \
  src/block_registry.rs '                reg.drops_second[st] = drop1;' \
  '                reg.drops_second[st] = -1;' \
  a_potted_plant_drops_both_entries_the_asset_lists

check "a falling block does not move in a cold chunk" \
  src/main.rs '            if !region.is_warm(f.x as i32, f.z as i32) {
                keep.push(f);
                continue;
            }' \
  '            if false {
                keep.push(f);
                continue;
            }' \
  a_falling_entity_stops_moving_when_its_chunk_goes_cold
check "a save carries the falling entities it reloads" \
  src/save_v2.rs '    megu_core::varint::write_varlong_i64(in_flight.len() as i64, &mut *f);' \
  '    megu_core::varint::write_varlong_i64(0i64, &mut *f);' \
  a_support_placed_under_a_falling_entity_is_landed_on

check "a knockout and a save in one tick conserve the column" \
  src/main.rs '        end_falling_tick(&processed, keep);' \
  '        end_falling_tick(&processed, Vec::new());' \
  a_knockout_and_a_save_in_the_same_tick_conserve_the_block

check "adjacent knockouts conserve every block" \
  src/main.rs '            if fallable_state(current) && set_block_if(bx, top, bz, fa.state, Some(current)) {' \
  '            if false {' \
  two_knockouts_of_adjacent_supports_conserve_every_block

check "a conversion does not leave a second drop behind" \
  src/main.rs '                despawn.push(f.eid);' \
  '                despawn.push(f.eid); keep.push(f.clone());' \
  a_save_during_entity_conversion_has_no_duplicate_drop

check "a block in a cold chunk keeps its position across ticks" \
  src/main.rs '            if !region.is_warm(f.x as i32, f.z as i32) {
                keep.push(f);
                continue;
            }' \
  '            if !region.is_warm(f.x as i32, f.z as i32) {
            }' \
  region_change_mid_fall_keeps_the_entity
check "breaking a chest drops the slots it owned" \
  src/main.rs '            if let Some(slots) = chests()
                .lock()
                .unwrap()
                .remove(&ContainerKey::ow((x, y, z)))' \
  '            if let Some(slots) = chests()
                .lock()
                .unwrap()
                .remove(&ContainerKey::ow((x, y + 1000, z)))' \
  a_broken_chest_leaves_no_orphan_entry_across_a_save_and_load

check "a state whose metadata row cannot be matched is reported as unpaired" \
  src/block_registry.rs 'match state_flags.get(st) {' \
  'match state_flags.get(st + 1) {' \
  every_wire_state_has_complete_metadata

check "the client's view distance is read from its packet" \
  src/play_protocol.rs '    let view = *p.first()? as i8 as i32;' \
  '    let view = *p.get(1)? as i8 as i32;' \
  client_information_carries_the_view_distance_after_the_locale

check "containers must be decoded, not merely remain absent" \
  src/save_v2.rs 'fn load_world_from(payload: &[u8]) -> Result<(), &'"'"'static str> {' \
  'fn load_world_from(payload: &[u8]) -> Result<(), &'"'"'static str> { return Ok(());' \
  a_broken_chest_leaves_no_orphan_entry_across_a_save_and_load

check "a broken furnace cannot retain its slots" \
  src/main.rs '            if let Some(f) = furnaces().lock().unwrap_or_else(|e| e.into_inner()).remove(&ow_block((x, y, z))) {' \
  '            if let Some(f) = furnaces().lock().unwrap_or_else(|e| e.into_inner()).remove(&ow_block((x, y, z + 1000))) {' \
  furnace_and_sign_round_trip_keeps_only_live_owners

check "broken sign text cannot survive its block" \
  src/main.rs '            signs()
                .lock()
                .unwrap()
                .retain(|&(dim, sx, sy, sz, _), _| {
                    !(dim == DimId::Overworld && sx == x && sy == y && sz == z)
                });' \
  '            signs()
                .lock()
                .unwrap()
                .retain(|_, _| true);' \
  furnace_and_sign_round_trip_keeps_only_live_owners
# The note block's per-state solidity and light now come from the pinned fact table, so this
# mutation moves to the column it reads. The pumpkin `state_flags` load it used to break is
# covered by the metadata-offset entries above.
check "solid metadata must reach every note block state" \
  src/block_registry.rs '        self.pred_of(st) & FACT_BLOCKS_MOTION != 0' \
  '        self.pred_of(st) & FACT_IS_AIR != 0' \
  note_block_keeps_solid_and_light_values_for_every_instrument

check "the pinned facts separate the predicates vanilla separates" \
  src/block_registry.rs '        self.pred_of(st) & FACT_IS_AIR != 0' \
  '        st == 0' \
  state_facts_answer_the_predicates_vanilla_separates

check "an empty collision shape is not a full one" \
  src/block_registry.rs '        self.pred_of(st) & FACT_COLLISION_EMPTY != 0' \
  '        self.pred_of(st) & FACT_COLLISION_FULL != 0' \
  state_facts_answer_the_predicates_vanilla_separates

check "a fluid state is not a liquid block" \
  src/block_registry.rs '        self.pred_of(st) & FACT_FLUID_EMPTY == 0' \
  '        self.pred_of(st) & FACT_LIQUID != 0' \
  state_facts_answer_the_predicates_vanilla_separates

check "the light columns are emission then block, not one shifted" \
  src/block_registry.rs '        self.light_of_byte(st) >> 4' \
  '        self.light_of_byte(st) & 0xf' \
  state_facts_answer_the_predicates_vanilla_separates

check "the face planes are indexed by support type, not by direction" \
  src/block_registry.rs '        (faces >> (support as usize * 6)) & 0x3f & (1 << direction) != 0' \
  '        (faces >> direction) & 0x3f & (1 << direction) != 0' \
  face_support_answers_the_support_type_the_caller_asks_for

check "a fact record is five bytes, one per state" \
  src/block_registry.rs '    let at = st as usize * STATE_FACT_BYTES;' \
  '    let at = st as usize;' \
  pinned_fact_tables_agree_with_the_older_oracle_assets

check "property remapping cannot reuse positional state offsets" \
  src/block_registry.rs '                let o = off as i64 + (nd as i64 - digit as i64) * mult as i64;' \
  '                let o = off as i64 + nd as i64 * mult as i64;' \
  setting_a_property_lower_than_its_current_value_stays_inside_the_block

check "a predicate reads solidity, not replaceability" \
  src/generation_providers.rs '        Pr::Solid => reg.is_solid(ctx.get(x, y, z)),' \
  '        Pr::Solid => reg.is_replaceable(ctx.get(x, y, z)),' \
  provider_values_and_following_rng_match_pre_extraction

check "block columns read native provider fields" \
  src/feature.rs 'p_bsp(js(e, "provider").or_else(|| js(e, "contents"))?)?' \
  'p_bsp(js(e, "contents")?)?' \
  block_column_provider_accepts_native_and_legacy_keys

check "biased integer heights use the nested bound" \
  src/generation_providers.rs '                a + r.next_bound(bound)' \
  '                a + r.next_bound(b - a + 1)' \
  block_column_up_matches_native_cells_and_rng

check "block-column predicates read their configured offset" \
  src/generation_providers.rs 'supports_offset && offset != [0, 0, 0]' \
  'supports_offset && offset == [0, 0, 0]' \
  block_column_up_matches_native_cells_and_rng

check "randomized state providers sample their integer provider" \
  src/generation_providers.rs '                    let value = vals.get(r);' \
  '                    let value = vals.min_inclusive();' \
  block_column_down_matches_native_cells_and_rng

check "selectors cannot shrink after a nested parser failure" \
  src/feature.rs '        "simple_random_selector" => {
            let mut v = vec![];
            for e in jarr(j, "features")? {
                v.push(pf_any(e)?);' \
  '        "simple_random_selector" => {
            let mut v = vec![];
            for e in jarr(j, "features")? {
                if let Some(pf) = pf_any(e) { v.push(pf); }' \
  required_feature_children_cannot_silently_disappear

check "sequences cannot shrink after a nested parser failure" \
  src/feature.rs '        "sequence" => {
            let mut v = vec![];
            for e in jarr(j, "features")? {
                v.push(pf_any(e)?);' \
  '        "sequence" => {
            let mut v = vec![];
            for e in jarr(j, "features")? {
                if let Some(pf) = pf_any(e) { v.push(pf); }' \
  required_feature_children_cannot_silently_disappear

check "required placement modifiers cannot disappear" \
  src/feature.rs '                    return None;
                }
            }
        }
    }
    Some(Pf {' \
  '                    continue;
                }
            }
        }
    }
    Some(Pf {' \
  required_feature_children_cannot_silently_disappear

check "all-of predicates cannot drop an unsupported child" \
  src/generation_providers.rs '        "all_of" => Pr::All(
            jarr(j, "predicates")?
                .iter()
                .map(p_pred)
                .collect::<Option<_>>()?,
        ),' \
  '        "all_of" => Pr::All(jarr(j, "predicates")?.iter().filter_map(p_pred).collect()),' \
  required_feature_children_cannot_silently_disappear

check "any-of predicates cannot drop an unsupported child" \
  src/generation_providers.rs '        "any_of" => Pr::Any(
            jarr(j, "predicates")?
                .iter()
                .map(p_pred)
                .collect::<Option<_>>()?,
        ),' \
  '        "any_of" => Pr::Any(jarr(j, "predicates")?.iter().filter_map(p_pred).collect()),' \
  required_feature_children_cannot_silently_disappear

check "a context override outranks the cached terrain read" \
  src/generation_context.rs '        if let Some(&s) = self.wr.get(&(x, y, z)) {' \
  '        if let Some(&s) = self.rc.get(&(x, y, z)) {' \
  context_overrides_heights_bounds_and_biomes_track_live_state

check "placement modifiers advance positions in vanilla's draw order" \
  src/feature.rs '            let (x, z) = (pos.0 + rng.next_bound(16), pos.2 + rng.next_bound(16));' \
  '            let z = pos.2 + rng.next_bound(16);
            let x = pos.0 + rng.next_bound(16);' \
  feature_population_output_matches_the_pre_refactor_capture
check "certain air-exposure discard still places buried ore without an RNG draw" \
  src/feature.rs '    if discard < 1.0 && r.next_f32() >= discard {' \
  '    if discard >= 1.0 { return false; }
    if r.next_f32() >= discard {' \
  ore_air_exposure_matches_native_decisions_and_rng
# Native feature-contract guards; select this group with
# MUTATION_FILTER=feature::generation_contract_tests::
check "ore early-out reads the frozen worldgen floor" \
  src/feature.rs '            if o <= ctx.hm_oc_wg(xp, zp) {' \
  '            if o <= ctx.hm_oc(xp, zp) {' \
  feature::generation_contract_tests::ore_uses_frozen_worldgen_floor
check "surface-relative placement keeps its configured heightmap" \
  src/feature.rs '        PM::SurfaceRel(kind, min_i, max_i) => {
            let y = i64::from(ctx.heightmap(*kind, pos.0, pos.2));
            let lo = y + i64::from(*min_i);
            let hi = y + i64::from(*max_i);
            if lo <= i64::from(pos.1) && i64::from(pos.1) <= hi {
                out.push(pos);' \
  '        PM::SurfaceRel(kind, min_i, max_i) => {
            let y = i64::from(ctx.hm_ws(pos.0, pos.2));
            let lo = y + i64::from(*min_i);
            let hi = y + i64::from(*max_i);
            if lo <= i64::from(pos.1) && i64::from(pos.1) <= hi {
                out.push(pos);' \
  feature::generation_contract_tests::surface_relative_filter_keeps_its_heightmap_and_long_bounds
check "dungeon loot uses the wrapped nextLong method" \
  src/feature.rs '                            let _loot_seed = rng.next_i64();' \
  '                            let _loot_seed = rng.next_long();' \
  feature::generation_contract_tests::monster_room_cells_and_rng_match_native
check "simple double plants write both halves" \
  src/feature.rs '                    for (dy, half) in [(0, "lower"), (1, "upper")] {' \
  '                    for (dy, half) in [(0, "lower")] {' \
  feature::generation_contract_tests::simple_block_places_both_halves_or_neither
check "mushroom canSurvive asks isSolidRender, not the dirt tag" \
  src/generation_providers.rs '                || (ctx.feature_brightness(x, y, z) < 13 && reg.is_solid_render(below))' \
  '                || (ctx.feature_brightness(x, y, z) < 13 && true)' \
  feature::generation_contract_tests::nether_mushrooms_survive_a_solid_render_support
check "an uninitialized sky light reads 0 in every dimension" \
  src/generation_context.rs '        15
    }' \
  '        0
    }' \
  feature::generation_contract_tests::mushroom_survival_uses_feature_time_light_and_support
# The chunk-promotion pass' two new branches. `worldgen`'s own test pins the fill's mark
# gate on the measured cell; `structure_template`'s pins the fluid tap.
check "the fill marks a settled aquifer fluid" \
  src/worldgen.rs '        let fill_mark = if self.aq_sched {
            MARK_FILL
        } else {
            0
        };' \
  '        let fill_mark = 0;' \
  worldgen::wg_tests::the_fill_marks_a_settled_aquifer_fluid_and_nothing_else
check "the promotion pass ticks a marked fluid cell" \
  src/template_structures.rs '        if reg.fluid_name(state) != "minecraft:empty" {
            if let Some(tick) = ticker {
                tick(world, pos);
            }
        }' \
  '        if false {
            if let Some(tick) = ticker {
                tick(world, pos);
            }
        }' \
  structure_template::template_structures_tests::postprocess_ticks_a_marked_source_fluid_into_its_neighbour
# The promotion footprint. A recorded plan is a window of native's decorated region, so a
# promotion window that leaves the plan cannot be evidence of a missing pass; and a fluid
# replaces a cell whose collision shape is empty instead of refusing it.
check "a recorded plan's window is native's decoration, not this model's" \
  src/worldgen.rs '                let c = (cx + dx, cz + dz);
                !in_window(c) || self.pop_done.contains(&c)' \
  '                let c = (cx + dx, cz + dz);
                self.pop_done.contains(&c)' \
  worldgen::wg_tests::a_recorded_plan_window_answers_for_the_neighbours_it_does_not_cover
check "a fluid replaces a cell whose collision shape is empty" \
  src/flowing_fluid.rs '            Cell::Passable => self.put(p, Cell::Fluid(fluid)),' \
  '            Cell::Passable => Ok(()),' \
  flowing_fluid::tests::a_source_replaces_an_empty_collision_shape_cell_sideways
check "a traced column names the layer that answered it" \
  src/generation_context.rs '        } else if self.rc.contains_key(&(x, y, z)) {
            "rc"' \
  '        } else if self.rc.contains_key(&(x, y, z)) {
            "terrain"' \
  feature::generation_contract_tests::cel_column_trace_tags_the_layer_that_answered
check "matching_fluids uses the fluid rather than block type" \
  src/generation_providers.rs '            let f = reg.fluid_name(st);' \
  '            let f = reg.name_of(st);' \
  feature::generation_contract_tests::matching_fluids_reads_the_fluid_state_not_the_block_name
check "multiface growth performs native spreading" \
  src/feature.rs '        if rng.next_f32() < cfg.spread {' \
  '        if rng.next_f32() < 0.0 {' \
  feature::generation_contract_tests::multiface_cells_and_following_rng_match_native
check "the lake grid is anchored down only" \
  src/feature.rs '            let anchor = (pos.0, pos.1 - 4, pos.2);' \
  '            let anchor = (pos.0 - 8, pos.1 - 4, pos.2 - 8);' \
  feature::generation_contract_tests::lake_grid_anchor_is_down_only
check "the vegetation patch walks its surface set in HashSet order" \
  src/feature.rs '            let spread = (h ^ ((h as u32) >> 16) as i32) as u32;' \
  '            let spread = i as u32;' \
  feature::generation_contract_tests::java_hashset_order_matches_java_util_hashset_iteration

check "the ore vein radius matches vanilla's own output" \
  src/feature.rs '        ds[ki + 3] = (s1 as f64 * hh + 1.0) / 2.0;' \
  '        ds[ki + 3] = (s1 as f64 * hh + 1.0) / 2.0 + 0.01;' \
  the_ore_vein_geometry_matches_vanillas_own_output

check "the frozen population digest notices shared provider changes" \
  src/generation_providers.rs '            IP::Uniform(a, b) => r.next_inbetween(*a, *b),' \
  '            IP::Uniform(a, b) => (*a + *b) / 2,' \
  feature_population_output_matches_the_pre_refactor_capture

check "the deep-state memo returns the value it computed" \
  src/worldgen.rs '        if let Some(&v) = self.deep_c.get(&(x, y, z)) {
            inc_if(m3_read, M3_DEEP_HIT);
            return v;
        }' \
  '        if self.deep_c.contains_key(&(x, y, z)) {
            inc_if(m3_read, M3_DEEP_HIT);
            return 0;
        }' \
  deep_state_memo_returns_the_value_it_computed
check "the noise-value cache returns the value it computed" \
  src/generation_math.rs '            if slot.0 == key {
                slot.1
            } else {' \
  '            if slot.0 == key {
                slot.1 + 1.0
            } else {' \
  feature_population_output_matches_the_pre_refactor_capture
check "the shared biome cache returns the value it computed" \
  src/worldgen.rs '                    self.bioq.insert(pos, leaf.unwrap_or(u16::MAX));' \
  '                    self.bioq.insert(pos, leaf.unwrap_or(u16::MAX).wrapping_add(1));' \
  worldgen::wg_tests::surface_biomes_follow_fuzzy_palettes_and_preserve_climate_ties

check "the queue cap cannot visit a cell before its real backoff" \
  src/main.rs '    backoff()
        .lock()
        .unwrap()
        .get(&(DimId::Overworld, x, y, z))
        .map(|(t, _)| *t <= now)
        .unwrap_or(true)' \
  '    backoff()
        .lock()
        .unwrap()
        .get(&(DimId::Overworld, x, y, z))
        .map(|(_t, _)| true)
        .unwrap_or(true)' \
  a_backoff_past_the_queue_cap_waits_for_its_real_deadline
check "a reload continues the world random stream" \
  src/save_v2.rs '            world_rng::restore(u64::from_be_bytes(payload[p..p + 8].try_into().unwrap()));' \
  '            let _ = &payload[p..p + 8];' \
  persistence_tests::the_world_rng_stream_continues_across_a_save_and_load_round_trip

# The tick save section's footer has one owner. Both consumers (`prefix` and
# `read`) take the section start from `framed`, so the guard that refuses a
# footer framing no body is what makes their answers agree. Without it the old
# `prefix` returned an empty preceding section and let the reader start from
# nothing. This is the entry that moves with the footer's owner.
check "a footer that frames no body is refused by both of its consumers" \
  src/tick_persistence.rs '    if size == 0 {' \
  '    if false {' \
  tick_persistence::tests::a_footer_that_frames_no_body_is_refused_by_both_consumers

# The config session now records the client's view distance at exactly one
# site (`ConfigLink`), whichever side of the known-packs exchange the client
# information packet arrives on. The two wire windows share the anchor; each
# still proves its own gate against the same store.
check "configuration information before known packs reaches chunk streaming" \
  src/play_protocol.rs '            Some(v) => {
                link.client_view = v;
                ConfigPoll::Serviced' \
  '            Some(v) => {
                let _ = v;
                ConfigPoll::Serviced' \
  tests/client_settings_wire.py 'client settings wire: FAIL config-early-window'

check "configuration information after known packs reaches chunk streaming" \
  src/play_protocol.rs '            Some(v) => {
                link.client_view = v;
                ConfigPoll::Serviced' \
  '            Some(v) => {
                let _ = v;
                ConfigPoll::Serviced' \
  tests/client_settings_wire.py 'client settings wire: FAIL config-late-window'

check "play information reaches the live chunk window" \
  src/play_protocol.rs '                let effective = requested.clamp(2, SERVER_VIEW_DISTANCE);' \
  '                let effective = view;' \
  tests/client_settings_wire.py 'client settings wire: FAIL play-view-3'
check "breaking changes server state, not only the predicted wire update" \
  src/play_protocol.rs '    #[cfg(not(feature = "neoforge-events"))]
    set_block_dim(dim, x, y, z, 0);
    note_block_update_dim(dim, x, y, z, 0);' \
  '    #[cfg(not(feature = "neoforge-events"))]
    set_block_dim(dim, x, y, z, old);
    note_block_update_dim(dim, x, y, z, 0);' \
  tests/gameplay_wire.py 'gameplay wire: FAIL fresh-reader-block'
check "chat returns the actual sender and message" \
  src/play_protocol.rs '            let disguised_frame =
                encode_disguised_chat_0x1e(&chat_nbt(m), live_chat_type_index(), &me.name);' \
  '            let disguised_frame =
                encode_disguised_chat_0x1e(&chat_nbt(""), live_chat_type_index(), &me.name);' \
  tests/gameplay_wire.py 'gameplay wire: FAIL chat-echo'

check "light arrays agree with their section masks" \
  src/main.rs '        write_varint(
            p,
            mask.iter().map(|w| w.count_ones() as i64).sum::<i64>(),
        );' \
  '        write_varint(
            p,
            mask.iter().map(|w| w.count_ones() as i64).sum::<i64>() + 1,
        );' \
  tests/gameplay_wire.py 'gameplay wire: FAIL light-mask-count'
check "an update packet sends block light only" \
  src/main.rs '        [Vec::new(), mask, Vec::new(), empty],' \
  '        [Vec::new(), mask, vec![1], empty],' \
  update_light_packet_sends_block_light_only

check "a large dripstone body is built in the empty cells" \
  src/feature.rs 'if drip_is_empty_or_water_or_lava(ctx, q) {' \
  'if drip_is_dripstone_base(reg, st) {' \
  large_dripstone_hangs_and_stands_a_tapered_spike

check "the wind displaces a large dripstone cell by the distance to the origin" \
  src/feature.rs '                    p.0 + (vx * dy).floor() as i32,' \
  '                    p.0,' \
  large_dripstone_sways_when_both_spikes_clear_the_wind_gate

check "a dripstone tip rejects a lava ceiling, not a stone one" \
  src/feature.rs 'Some(c) if roll1 && !drip_is_lava(ctx, (pos.0, c, pos.2)) => {' \
  'Some(c) if roll1 && !drip_can_be_adjacent_to_water(ctx, (pos.0, c, pos.2)) => {' \
  dripstone_cluster_hangs_stalactites_below_and_stands_stalagmites_above

check "a pointed dripstone carries the direction it was grown in" \
  src/feature.rs 'let st = drip_pointed(reg, up, t);' \
  'let st = drip_pointed(reg, true, t);' \
  dripstone_cluster_hangs_stalactites_below_and_stands_stalagmites_above

check "an iceberg re-anchors itself to sea level" \
  src/feature.rs '    let pos = (origin.0, crate::worldgen::sea_level(), origin.2);' \
  '    let pos = (origin.0, origin.1, origin.2);' \
  iceberg_builds_a_shape_around_sea_level

check "an iceberg is built out of the config's state" \
  src/feature.rs '    let st = if snow {
        reg.state_of("minecraft:snow_block", None)
    } else {
        state
    };' \
  '    let st = if snow {
        reg.state_of("minecraft:snow_block", None)
    } else {
        reg.state_of("minecraft:snow_block", None)
    };' \
  iceberg_builds_a_shape_around_sea_level

check "a desert well's roof centre is sandstone, not a slab" \
  src/feature.rs '            let st = if dx == 0 && dz == 0 { sandstone } else { slab };' \
  '            let st = if dx == 0 && dz == 0 { slab } else { slab };' \
  desert_well_builds_its_shape_on_sand

check "a desert well picks its suspicious sand with nextInt(5)" \
  src/feature.rs '    let i1 = rng.next_bound(shaft.len() as i32) as usize;' \
  '    let i1 = rng.next_bound(shaft.len() as i32 - 1) as usize;' \
  desert_well_builds_its_shape_on_sand

check "an ice spike's tip is the drawn height above the drawn base" \
  src/feature.rs '    let height = rng.next_bound(4) + 7;' \
  '    let height = rng.next_bound(4) + 6;' \
  ice_spike_builds_a_tapering_spike_on_snow

check "an ice spike is built out of packed ice" \
  src/feature.rs '                        ctx.set(q.0, q.1, q.2, packed);' \
  '                        ctx.set(q.0, q.1, q.2, ice_block);' \
  ice_spike_builds_a_tapering_spike_on_snow

check "a geode walks its generation box x-fastest, as betweenClosed does" \
  src/feature.rs '    for z in (origin.2 + g.min_gen_offset)..=(origin.2 + g.max_gen_offset) {
        for y in (origin.1 + g.min_gen_offset)..=(origin.1 + g.max_gen_offset) {
            for x in (origin.0 + g.min_gen_offset)..=(origin.0 + g.max_gen_offset) {' \
  '    for x in (origin.0 + g.min_gen_offset)..=(origin.0 + g.max_gen_offset) {
        for y in (origin.1 + g.min_gen_offset)..=(origin.1 + g.max_gen_offset) {
            for z in (origin.2 + g.min_gen_offset)..=(origin.2 + g.max_gen_offset) {' \
  geode_walks_the_generation_box_in_between_closed_order

check "a geode's crack blanks only the neighbours that hold a fluid" \
  src/feature.rs '                            if fluid != "minecraft:empty" {' \
  '                            if !reg.is_air(ns) {' \
  geode_crack_resets_only_neighbours_that_hold_a_fluid

check "a geode hangs a crystal only in air or a water source, not a liquid" \
  src/feature.rs '                || (reg.name_of(ns) == "minecraft:water"
                    && reg.get_prop(ns, "level").is_some_and(|l| l == "0"));' \
  '                || (reg.is_liquid(ns)
                    && reg.get_prop(ns, "level").is_some_and(|l| l == "0"));' \
  geode_hangs_crystals_only_in_air_or_a_water_source

check "the heightmap a feature reads is the floor the blocks are at" \
  src/generation_context.rs '        self.heightmap_above(x, z, start, kind)' \
  '        base' \
  heightmap_reads_the_floor_the_blocks_are_at_not_the_noise_surface

check "a WG heightmap read is the carved top, not the uncarved terrain" \
  src/generation_context.rs '            if let Some(v) = self.wg_after_carve_height(
                matches!(kind, HeightmapKind::OceanFloorWg),
                x,
                z,
            ) {
                return v;
            }
' \
  '' \
  feature::generation_contract_tests::wg_heightmap_keeps_the_carved_top_and_ignores_the_feature_layer

check "the projectile sweep retains start-inside contact" \
  src/combat_and_health.rs '    bounds.contains(start) || bounds.clip(start, end).is_some()' \
  '    bounds.clip(start, end).is_some()' \
  the_projectile_sweep_covers_the_whole_tick

check "an arrow hits the target it crosses on its first tick" \
  src/combat_and_health.rs '    bounds.contains(start) || bounds.clip(start, end).is_some()' \
  '    bounds.clip(start, end).is_some()' \
  the_production_arrow_loop_hits_a_target_it_leaves_the_bow_inside

check "a container transfer fills only the room in a matching stack" \
  src/menus_and_items.rs '    let moved = amount.min(source.1).min(room);' \
  '    let moved = amount.min(source.1).min(room).min(0);' \
  container_transfers_match_real_1_21_1_classes

check "a chest quick move searches the player inventory in reverse" \
  src/menus_and_items.rs '            let rem = move_to_slots(inv, (9..=44).rev(), st);' \
  '            let rem = move_to_slots(inv, 9..=44, st);' \
  container_transfers_match_real_1_21_1_classes

check "a chest item reaches the placement path" \
  src/menus_and_items.rs '        299 => 2955,' \
  '        299 => return None,' \
  container_chest_item_reaches_the_placed_default_state

check "the real chest click dispatch preserves the mouse button" \
  src/play_protocol.rs '                                    click_slot(gmap, &mut cur, gslot, btn as i8);' \
  '                                    click_slot(gmap, &mut cur, gslot, 0);' \
  tests/container_wire.py 'containers wire: FAIL chest-half-pickup'

# The chest world-side contract (M2 slice 3). Each mutant restores one sentence of the
# 1.21.1 mechanism the port had wrong, and each one was measured against the test named
# beside it: `Containers.dropContentsOnDestroy` is guarded by the *block* changing,
# `candidatePartnerFacing` requires the same `Block`, `updateShape` unpairs the survivor,
# `DoubleBlockCombiner` puts the right half first, `ContainerOpenersCounter` broadcasts the
# new count, the menu dies with the block, and a block container exists only while its
# block does.
check "a chest state change does not destroy the container" \
  src/main.rs '        let identity_changed = match breg() {
            Some(reg) => reg.name_of(old) != reg.name_of(st),
            None => true,
        };' \
  '        let identity_changed = st != old;' \
  a_chest_state_change_keeps_its_contents

check "a chest pairs only with a chest of its own kind" \
  src/menus_and_items.rs '    let st = block_at(pos.0, pos.1, pos.2);
    if breg()?.name_of(st) != name {
        return None;
    }
    if chest_type(st)? != "single" {' \
  '    let st = block_at(pos.0, pos.1, pos.2);
    if chest_type(st)? != "single" {' \
  chest_placement_pairs_only_with_a_matching_single_chest

check "breaking one half reverts the survivor to single" \
  src/menus_and_items.rs '    if chest_connected_direction(own)? == dir {
        return Some(reg.with_prop(own, "type", "single"));
    }' \
  '    if chest_connected_direction(own)? == dir.opposite() {
        return Some(reg.with_prop(own, "type", "single"));
    }' \
  breaking_a_half_reverts_the_survivor_to_single

check "the combined container puts the right half in slots 0..=26" \
  src/menus_and_items.rs '    match own {
        "right" => Some((pos, other)),
        _ => Some((other, pos)),
    }' \
  '    match own {
        "right" => Some((other, pos)),
        _ => Some((pos, other)),
    }' \
  a_double_chest_window_opens_right_half_first

check "the lid broadcast carries the new opener count" \
  src/menus_and_items.rs '            broadcast_block_event(audience, pos, 1, now as u8);' \
  '            broadcast_block_event(audience, pos, 1, previous as u8);' \
  two_viewers_move_the_opener_count_and_broadcast_it

check "a removed container closes its window on windowId 1" \
  src/menus_and_items.rs '        if had {
            *c.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        if had {
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x12, vec![1u8]);
            }
            closed += 1;' \
  '        if had {
            *c.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        if had {
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x12, Vec::new());
            }
            closed += 1;' \
  a_removed_container_closes_the_window_built_on_it

check "a block container stops existing when its block does" \
  src/menus_and_items.rs '            is_ender_key((pos.1, pos.2, pos.3))
                || container_kind(block_at_dim(pos.0, pos.1, pos.2, pos.3)).is_some()' \
  '            true' \
  breaking_a_chest_drops_each_stack_once_and_leaves_no_entry

# Inventory pickup: prove the native loop AND its actual tick caller.
check "pickup drains beyond the first partially filled slot" \
  src/menus_and_items.rs '        if source.1 == 0 || source.1 == before {' \
  '        if true {' \
  pickup_inventory_matches_real_1_21_1_classes

check "the pickup tick forwards the selected hotbar slot" \
  src/menus_and_items.rs '            let accepted = pickup_into_inventory(&mut inv, selected, creative, &mut source);' \
  '            let accepted = pickup_into_inventory(&mut inv, 0, creative, &mut source);' \
  pickup_tick_drains_across_selected_offhand_then_hotbar

check "partial insertion is not a successful ItemEntity take callback" \
  src/menus_and_items.rs '        if accepted {
            broadcast_seen(deid, |_c| (0x6f, collect_pkt(deid, c.eid, original)));' \
  '        if original > 0 {
            broadcast_seen(deid, |_c| (0x6f, collect_pkt(deid, c.eid, original)));' \
  pickup_tick_keeps_the_partial_entity_without_a_take_event

check "a full nearby player does not monopolize the drop" \
  src/menus_and_items.rs '                    taken.push((*d_eid, c.clone()));' \
  '                    taken.push((*d_eid, c.clone())); break;' \
  pickup_tick_tries_the_next_player_but_never_inserts_a_stale_stack

check "the pickup tick forwards infinite materials to inventory insertion" \
  src/menus_and_items.rs '    for (deid, c) in taken {
        let selected = *c.held.lock().unwrap_or_else(|e| e.into_inner());
        let creative = *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1;' \
  '    for (deid, c) in taken {
        let selected = *c.held.lock().unwrap_or_else(|e| e.into_inner());
        let creative = false;' \
  pickup_tick_tries_the_next_player_but_never_inserts_a_stale_stack

check "a real client observes the pickup delay the actor path already enforced" \
  src/menus_and_items.rs '            if !d.pickup_delay_elapsed() {
                continue;
            }
            let mut source = (d.item, d.cnt, d.comps.clone());' \
  '            let mut source = (d.item, d.cnt, d.comps.clone());' \
  pickup_tick_refuses_a_drop_inside_its_pickup_delay

check "a targeted drop refuses a closer non-target player" \
  src/menus_and_items.rs '            (Some(owner), Some(uuid)) => owner == uuid,' \
  '            (Some(_owner), Some(_uuid)) => true,' \
  pickup_tick_enforces_target_uuid_over_closer_non_target

check "spectator and dead players do not pick up drops" \
  src/menus_and_items.rs '        if spectator || !alive {' \
  '        if false && (spectator || !alive) {' \
  pickup_tick_excludes_spectator_and_dead_players

check "player pickup uses the 1.21.1 inflated touch AABB" \
  src/menus_and_items.rs '    player_bounding_box(x, y, z, sneaking, fall_fly).inflate(1.0, 0.5, 1.0)' \
  '    player_bounding_box(x, y, z, sneaking, fall_fly)' \
  pickup_tick_matches_1_21_1_player_touch_aabb_boundaries

# Pinned surface-loader slice: codecs, weather dispatch, and its shared noise owner.
check "the pinned inline surface rule is actually installed" \
  src/worldgen.rs 'ns.get("surface_rule").or_else(|| ns.get("material_rule"))' \
  'ns.get("material_rule")' \
  pinned_surface_rule_places_bedrock_and_deepslate

check "surface block states retain their properties" \
  src/worldgen.rs '            Some(props) => Some(breg()?.state_of(&name, Some(props))),' \
  '            Some(_props) => Some(default),' \
  pinned_surface_block_states_match_the_native_codec

check "the surface temperature condition uses biome weather" \
  src/worldgen.rs '.is_some_and(|weather| weather.cold(c.x, c.y, c.z))' \
  '.is_some_and(|weather| !weather.cold(c.x, c.y, c.z))' \
  pinned_surface_temperature_matches_the_native_biome

check "the frozen-temperature simplex stack keeps octave zero at its own index" \
  src/generation_math.rs '        let zi = j;' \
  '        let zi = -first;' \
  pinned_surface_temperature_matches_the_native_biome

check "simplex frequency starts at the highest octave, not the layer count" \
  src/generation_math.rs '        let mut e = 2.0f64.powi(self.highest_octave);' \
  '        let mut e = 2.0f64.powi(self.layers.len() as i32 - 1);' \
  pinned_surface_temperature_matches_the_native_biome

check "a surface sequence cannot silently discard an undecoded child" \
  src/worldgen.rs '                    .map(|c| self.rule_node(c, files, rf, cf, res))
                    .collect::<Option<_>>()?,' \
  '                    .filter_map(|c| self.rule_node(c, files, rf, cf, res))
                    .collect(),' \
  surface_sequence_rejects_a_child_that_did_not_parse

# Entity angle bytes: different constructors intentionally have different policies.
check "entity angles narrow through int before wrapping to a byte" \
  src/play_protocol.rs '    value as i8 // Java int -> byte wraps; a direct Rust float -> i8 saturates.' \
  '    scaled as i8 // broken: saturation instead of byte wrapping' \
  entity_angles_head_update_matches_native_floor

check "entity head updates use floor rather than truncation" \
  src/play_protocol.rs '    angle_byte(f64::from(deg * 256.0 / 360.0), AngleRounding::Floor)' \
  '    angle_byte(f64::from(deg * 256.0 / 360.0), AngleRounding::Truncate)' \
  entity_angles_head_update_matches_native_floor

check "entity rotations multiply before division even at subnormal inputs" \
  src/play_protocol.rs '    angle_byte(f64::from(deg * 256.0 / 360.0), AngleRounding::Floor)' \
  '    angle_byte(f64::from(deg / 360.0 * 256.0), AngleRounding::Floor)' \
  entity_angles_head_update_matches_native_floor

check "entity teleports truncate rather than sharing spawn rounding" \
  src/play_protocol.rs '    angle_byte(f64::from(deg * 256.0 / 360.0), AngleRounding::Truncate)' \
  '    angle_byte(f64::from(deg * 256.0 / 360.0), AngleRounding::Floor)' \
  entity_angles_teleport_matches_native_constructor

check "entity spawn head angles keep the constructor double precision" \
  src/play_protocol.rs '        angle_byte(f64::from(yaw) * 256.0 / 360.0, AngleRounding::Floor) as u8,' \
  '        angle(yaw) as u8,' \
  entity_angles_spawn_matches_native_constructors

# Binary/geometry primitives: actual-class vectors plus production dispatch.
check "section palettes switch to registry IDs above the indirect limit" \
  src/main.rs '    let direct = direct || indirect_bits > max_bits;' \
  '    let direct = indirect_bits > max_bits + 1;' \
  primitives_section_encoding_matches_native_palettes

check "section packing leaves unused high bits instead of straddling words" \
  src/main.rs '    let per = 64 / bits;' \
  '    let per = (64 / bits).min(2);' \
  primitives_section_encoding_matches_native_palettes

check "section counts exclude all three pinned air states" \
  src/main.rs '!matches!(state, 0 | 12958 | 12959)' \
  'state != 0' \
  primitives_section_encoding_matches_native_palettes

check "NBT NUL uses modified UTF-8 instead of a zero byte" \
  src/main.rs '            match unit {' \
  $'            match unit {\n                0 => { v.push(0); continue; }' \
  primitives_nbt_writers_match_native_modified_utf8

check "NBT astral scalars retain both UTF-16 surrogates" \
  src/main.rs '        for &unit in ch.encode_utf16(&mut [0; 2]).iter() {' \
  '        for &unit in [ch as u16].iter() {' \
  primitives_nbt_writers_match_native_modified_utf8

check "lore append preserves opaque modified UTF-8 bytes" \
  src/commands_core.rs '                            elems.push(d[p..end].to_vec());' \
  '                            elems.push(nbt_string(&String::from_utf8_lossy(&d[p + 3..end])));' \
  primitives_nbt_lore_append_preserves_java_string_bytes

check "AABB clip excludes the finite segment endpoint" \
  src/combat_and_health.rs '                && t < nearest' \
  '                && t <= nearest' \
  primitives_aabb_clip_matches_native_finite_segments

check "AABB clip keeps the pinned epsilon" \
  src/combat_and_health.rs '        const EPSILON: f64 = 1.0e-7;' \
  '        const EPSILON: f64 = 0.0;' \
  primitives_aabb_clip_matches_native_finite_segments

check "arrow tick uses the continuous segment rather than just its end" \
  src/combat_and_health.rs '    bounds.contains(start) || bounds.clip(start, end).is_some()' \
  '    bounds.contains(end)' \
  primitives_arrow_tick_hits_between_the_old_samples

# Pinned weird sampler: mapper, production path, context and native noise seams.
check "weird sampler type_1 thresholds are strict" \
  src/worldgen.rs $'Self::Type1 => {\n                if input < -0.5 {' \
  $'Self::Type1 => {\n                if input <= -0.5 {' \
  weird_sampler_matches_native_mapper_and_samples

check "weird sampler type_2 thresholds are strict" \
  src/worldgen.rs '                if input < -0.75 {' \
  '                if input <= -0.75 {' \
  weird_sampler_matches_native_mapper_and_samples

check "weird sampler installed definitions do not become zero" \
  src/worldgen.rs '                let rarity = mapper.rarity(self.eval_c(*input, x, y, z, memo, cc));
                let xd = x as f64 / rarity;
                let yd = y as f64 / rarity;
                let zd = z as f64 / rarity;
                rarity * self.noises[*ni].get(xd, yd, zd).abs()' \
  '                let rarity = mapper.rarity(self.eval_c(*input, x, y, z, memo, cc));
                let xd = x as f64 / rarity;
                let yd = y as f64 / rarity;
                let zd = z as f64 / rarity;
                rarity * self.noises[*ni].get(xd, yd, zd).abs() * 0.0' \
  weird_sampler_installed_nodes_are_nonzero

check "weird sampler divides the vertical coordinate by rarity" \
  src/worldgen.rs '                let rarity = mapper.rarity(self.eval_c(*input, x, y, z, memo, cc));
                let xd = x as f64 / rarity;
                let yd = y as f64 / rarity;' \
  '                let rarity = mapper.rarity(self.eval_c(*input, x, y, z, memo, cc));
                let xd = x as f64 / rarity;
                let yd = y as f64 * rarity;' \
  weird_sampler_matches_native_mapper_and_samples

check "weird sampler takes absolute noise before scaling" \
  src/worldgen.rs '                let rarity = mapper.rarity(self.eval_c(*input, x, y, z, memo, cc));
                let xd = x as f64 / rarity;
                let yd = y as f64 / rarity;
                let zd = z as f64 / rarity;
                rarity * self.noises[*ni].get(xd, yd, zd).abs()' \
  '                let rarity = mapper.rarity(self.eval_c(*input, x, y, z, memo, cc));
                let xd = x as f64 / rarity;
                let yd = y as f64 / rarity;
                let zd = z as f64 / rarity;
                rarity * self.noises[*ni].get(xd, yd, zd)' \
  weird_sampler_matches_native_mapper_and_samples

check "weird sampler keeps its input in grid reachability" \
  src/worldgen.rs 'DF::WeirdScaledSampler { input, .. } => out.push(*input),' \
  'DF::WeirdScaledSampler { .. } => {}' \
  weird_sampler_input_is_reachable_through_the_interpolated_node

check "weird sampler retains the callers interpolation context" \
  src/worldgen.rs 'mapper.rarity(self.eval_c(*input, x, y, z, memo, cc))' \
  'mapper.rarity(self.eval_c(*input, x, y, z, memo, None))' \
  weird_sampler_uses_the_callers_interpolation_context

check "weird sampler failed named parses do not cache a zero" \
  src/worldgen.rs '                self.names.remove(name);' \
  '                // broken: retain the failed recursion placeholder' \
  weird_sampler_failed_named_reference_is_retried_not_cached_as_zero

check "weird sampler native noise uses the pinned final normalization" \
  src/generation_math.rs 'NATIVE_VALUE_FACTOR_NUMERATOR / (0.1 * (1.0 + 1.0 / span.wrapping_add(1) as f64))' \
  '(NATIVE_VALUE_FACTOR_NUMERATOR * 2.0) / (0.1 * (1.0 + 1.0 / span.wrapping_add(1) as f64))' \
  weird_sampler_matches_native_mapper_and_samples

check "weird sampler native noise scales after adding both stacks" \
  src/generation_math.rs '(first + second) * native.value_factor' \
  'first * native.value_factor + second * native.value_factor' \
  weird_sampler_matches_native_mapper_and_samples

check "weird sampler native noise counts gaps in the amplitude span" \
  src/generation_math.rs 'let span = native_amplitude_span(amps);' \
  'let span = amps.iter().filter(|a| **a != 0.0).count() as i32 - 1;' \
  weird_sampler_matches_native_mapper_and_samples

check "weird sampler native noise keeps per-octave multiplication order" \
  src/generation_math.rs 'first += o.a * o.n.get(x * o.f, y * o.f, z * o.f) * weight;' \
  'first += (o.a * weight) * o.n.get(x * o.f, y * o.f, z * o.f);' \
  worldgen::weird_sampler_tests::weird_sampler_matches_native_mapper_and_samples

check "weird sampler multiplies the absolute noise by rarity" \
  src/worldgen.rs '                let rarity = mapper.rarity(self.eval_c(*input, x, y, z, memo, cc));
                let xd = x as f64 / rarity;
                let yd = y as f64 / rarity;
                let zd = z as f64 / rarity;
                rarity * self.noises[*ni].get(xd, yd, zd).abs()' \
  '                let rarity = mapper.rarity(self.eval_c(*input, x, y, z, memo, cc));
                let xd = x as f64 / rarity;
                let yd = y as f64 / rarity;
                let zd = z as f64 / rarity;
                self.noises[*ni].get(xd, yd, zd).abs()' \
  worldgen::weird_sampler_tests::weird_sampler_matches_native_mapper_and_samples

check "aquifer preliminary surface uses the pinned threshold" \
  src/worldgen.rs '            _ => (id, top, self.noise_cell_h.max(1), 0.390625),' \
  '            _ => (id, top, self.noise_cell_h.max(1), 0.0),' \
  aquifer_matches_the_pinned_chunk_through_its_public_api

check "aquifer preliminary surface scans quart columns" \
  src/worldgen.rs '                let (ax, az) = (k.0 << 2, k.1 << 2);' \
  '                let (ax, az) = ((k.0 << 2) & !15, (k.1 << 2) & !15);' \
  aquifer_matches_the_pinned_chunk_through_its_public_api

check "aquifer spline keeps the native float interpolation order" \
  src/worldgen.rs '        let lerp = |delta: f32, start: f32, end: f32| start + delta * (end - start);
        (lerp(t, v0, v1) + t * (1.0 - t) * lerp(t, d0t, d1t)) as f64
    }

    fn eval0_c(' \
  '        let lerp = |delta: f32, start: f32, end: f32| (1.0 - delta) * start + delta * end;
        (lerp(t, v0, v1) + t * (1.0 - t) * lerp(t, d0t, d1t)) as f64
    }

    fn eval0_c(' \
  aquifer_matches_the_pinned_chunk_through_its_public_api

check "aquifer deep test keeps the depth threshold" \
  src/worldgen.rs '&& self.eval_flat(c[4], x, y, z, &mut flat) > 0.9f32 as f64' \
  '&& self.eval_flat(c[4], x, y, z, &mut flat) > -2.0' \
  aquifer_deep_level_reads_erosion_and_depth_in_f32

check "aquifer deep test reads erosion and depth" \
  src/worldgen.rs $'                    let is_deep = self.eval_flat(c[3], x, y, z, &mut flat) < -0.225f32 as f64\n                        && self.eval_flat(c[4], x, y, z, &mut flat) > 0.9f32 as f64;' \
  $'                    let is_deep = self.eval_flat(c[2], x, y, z, &mut flat) < -0.225f32 as f64\n                        && self.eval_flat(c[3], x, y, z, &mut flat) > 0.9f32 as f64;' \
  aquifer_deep_level_reads_erosion_and_depth_in_f32

check "aquifer deep test promotes the f32 threshold" \
  src/worldgen.rs 'self.eval_flat(c[3], x, y, z, &mut flat) < -0.225f32 as f64' \
  'self.eval_flat(c[3], x, y, z, &mut flat) < -0.225' \
  aquifer_deep_level_reads_erosion_and_depth_in_f32

check "converted bare carver lists still parse" \
  src/feature.rs '        _ => carvers,' \
  '        _ => return None,' \
  converted_carver_lists_still_parse

check "biome carvers parse a single AIR id" \
  src/feature.rs 'J::Str(s) => Some(vec![ns_name(s)]),' \
  'J::Str(_) => None,' \
  carver_lists_match_the_pinned_biomes

check "biome carvers read the AIR object key" \
  src/feature.rs 'J::Obj(_) => match carvers.get("air") {' \
  'J::Obj(_) => match carvers.get("liquid") {' \
  carver_lists_match_the_pinned_biomes

check "biome carvers preserve the list order" \
  src/feature.rs 'J::Arr(a) => Some(a.iter().map(|v| ns_name(v.str())).collect()),' \
  'J::Arr(a) => Some(a.iter().rev().map(|v| ns_name(v.str())).collect()),' \
  carver_lists_match_the_pinned_biomes

check "nextInt takes the 32-bit unsigned redraw threshold" \
  src/generation_math.rs 'let threshold = u64::from((bound.wrapping_neg() as u32) % (bound as u32));' \
  'let threshold = (bound.wrapping_neg() as u64) % (bound as u64);' \
  next_int_streams_match_the_game_for_every_class_seed_and_bound

check "biome tree scans children in array order for ties" \
  src/worldgen.rs '        for ci in start..end {' \
  '        for ci in (start..end).rev() {' \
  biome_tree_breaks_ties_the_way_vanilla_scans_children

check "spawner entries keep the pinned entity type" \
  src/feature.rs 'kind: ns_name(&jstr(j, "type")?),' \
  'kind: "minecraft:zombie".to_string(),' \
  biome_spawn_tables_match_the_pinned_classes

check "nightly hostile respects the biome weighted roll" \
  src/feature.rs 'let mut point = (roll % total as u64) as i64;' \
  'let mut point = 0i64;' \
  nightly_hostile_type_uses_the_pinned_biome_spawners

check "surface steep keeps the four-block threshold" \
  src/worldgen.rs 'let s = t(lx, lz + 1) >= t(lx, lz - 1) + 4 || t(lx - 1, lz) >= t(lx + 1, lz) + 4;' \
  'let s = t(lx, lz + 1) >= t(lx, lz - 1) + 3 || t(lx - 1, lz) >= t(lx + 1, lz) + 3;' \
  surface_steep_reads_the_world_surface_heightmap

check "surface stone depth selects the ceiling/below fields" \
  src/worldgen.rs 'let sd = if ceil { c.sdb } else { c.sda };' \
  'let sd = if ceil { c.sda } else { c.sdb };' \
  surface_conditions_match_the_pinned_context_formulas

check "surface scan keeps stone depth above the column" \
  src/worldgen.rs '                        for sy in (min_y..y).rev() {
                            if cells[(sy - min_y) as usize] != SurfaceCell::Solid {
                                min = sy + 1;
                                break;
                            }
                        }
                    }
                    sda += 1;' \
  '                        for sy in (min_y..y).rev() {
                            if cells[(sy - min_y) as usize] != SurfaceCell::Solid {
                                min = sy + 1;
                                break;
                            }
                        }
                    }
                    sda += 0;' \
  surface_build_scan_matches_the_pinned_classes_on_synthetic_columns

check "End decoration selects its own source feature indices" \
  src/feature.rs '    let order = fr.order.get(&g.preset).expect("dimension feature order");' \
  '    let order = fr.order.get(&NoisePreset::Overworld).expect("dimension feature order");' \
  worldgen::wg_tests::end_features_match_native_full_feature_chunks

check "basalt pillar keeps a dead hang-off direction dead" \
  src/feature.rs '                hang[i] = false;' \
  '                hang[i] = true;' \
  feature::generation_contract_tests::basalt_pillar_matches_native_cells_and_rng

check "basalt pillar reads the ceiling above the origin" \
  src/feature.rs '|| ctx.is_air(pos.0, pos.1 + 1, pos.2)' \
  '|| ctx.is_air(pos.0, pos.1 - 1, pos.2)' \
  feature::generation_contract_tests::basalt_pillar_matches_native_cells_and_rng

check "void start platform stays inside the start chunks" \
  src/feature.rs '    if cx.abs().max(cz.abs()) > 1 {' \
  '    if cx.abs().max(cz.abs()) > 0 {' \
  feature::generation_contract_tests::void_start_platform_matches_native_cells

check "end platform covers the obsidian row" \
  src/feature.rs '            for dy in -1i32..3 {' \
  '            for dy in 0i32..3 {' \
  feature::generation_contract_tests::end_platform_matches_native_cells

check "nether forest vegetation reads the nylium tag under the plant" \
  src/feature.rs '    if !reg.in_tag(ctx.get(pos.0, pos.1 - 1, pos.2), "minecraft:nylium") {' \
  '    if !reg.in_tag(ctx.get(pos.0, pos.1 - 1, pos.2), "minecraft:netherrack") {' \
  feature::generation_contract_tests::nether_forest_vegetation_matches_native_cells_and_rng

check "nether roots accept soul soil (the BushBlock dirt/farmland set too)" \
  src/generation_providers.rs '        "minecraft:crimson_roots" | "minecraft:warped_roots" | "minecraft:nether_sprouts" => {
            reg.in_tag(below, "minecraft:nylium")
                || reg.name_of(below) == "minecraft:soul_soil"' \
  '        "minecraft:crimson_roots" | "minecraft:warped_roots" | "minecraft:nether_sprouts" => {
            reg.in_tag(below, "minecraft:nylium")
                || reg.name_of(below) == "minecraft:soul_sand"' \
  feature::generation_contract_tests::nether_roots_sprouts_and_fungi_keep_their_own_may_place_on

check "basalt columns treat cave_air and void_air as no support" \
  src/feature.rs '    !reg.is_air(below_st) && !BC_CANNOT_PLACE_ON.contains(&reg.name_of(below_st))' \
  '    reg.name_of(below_st) != "minecraft:air" && !BC_CANNOT_PLACE_ON.contains(&reg.name_of(below_st))' \
  feature::generation_contract_tests::basalt_columns_need_a_non_air_support

check "count_on_every_layer re-samples the count inside every layer pass" \
  src/feature.rs '    let mut layer = 0i32;
    loop {
        let n = ip.get(rng).max(0);' \
  '    let mut layer = 0i32;
    let n = ip.get(rng).max(0);
    loop {' \
  feature::generation_contract_tests::count_on_every_layer_walks_motion_blocking_layers

check "count_on_every_layer counts a fluid cell as an empty layer" \
  src/feature.rs '    reg.is_air(st) || matches!(reg.name_of(st), "minecraft:water" | "minecraft:lava")' \
  '    reg.is_air(st)' \
  feature::generation_contract_tests::count_on_every_layer_walks_motion_blocking_layers

check "count_on_every_layer will not stack a layer on bedrock" \
  src/feature.rs '        if !cel_empty(reg, below) && cel_empty(reg, cur) && reg.name_of(below) != "minecraft:bedrock" {' \
  '        if !cel_empty(reg, below) && cel_empty(reg, cur) {' \
  feature::generation_contract_tests::count_on_every_layer_walks_motion_blocking_layers

check "the count_on_every_layer column trace lists the layers the walk places on" \
  src/feature.rs '        if boundary && reg.name_of(below) != "minecraft:bedrock" {' \
  '        if boundary {' \
  feature::generation_contract_tests::cel_layer_trace_lists_exactly_the_layers_cel_find_y_walks

check "feature writes stay inside the FEATURES write window" \
  src/worldgen.rs '        (cx - px).abs() <= 1 && (cz - pz).abs() <= 1' \
  '        (cx - px).abs() <= 1 && (cz - pz).abs() <= 2' \
  feature::generation_contract_tests::feature_writes_outside_the_population_window_are_dropped

check "nether forest vegetation keeps the ceiling guard" \
  src/feature.rs '    if y < ctx.g.world_min_y + 1 || y + 1 >= ctx.g.world_min_y + ctx.g.world_height {' \
  '    if y < ctx.g.world_min_y + 1 {' \
  feature::generation_contract_tests::nether_forest_vegetation_matches_native_cells_and_rng

check "delta feature draws a double, not a float" \
  src/feature.rs '    let flag = rng.next_f64() < 0.9;' \
  '    let flag = rng.next_f32() < 0.9;' \
  feature::generation_contract_tests::delta_feature_matches_native_cells_and_rng

check "manhattan iteration keeps the native order" \
  src/feature.rs '    cells.sort_by_key(|&(d, x, y, z)| (d, x, y, z.abs(), i32::from(z < 0)));' \
  '    cells.sort_by_key(|&(d, x, y, z)| (d, x, y, z, i32::from(z < 0)));' \
  feature::generation_contract_tests::within_manhattan_keeps_the_native_order

check "netherrack replace blobs samples the radius three times" \
  src/feature.rs '    let (i, j, k) = (radius.get(rng), radius.get(rng), radius.get(rng));' \
  '    let (i, j, k) = { let r = radius.get(rng); (r, r, r) };' \
  feature::generation_contract_tests::netherrack_replace_blobs_matches_native_cells_and_rng

check "twisting vines grows upward from the planted cell" \
  src/feature.rs '    let (start, step) = if down { (0, -1) } else { (1, 1) };' \
  '    let (start, step) = if down { (0, -1) } else { (0, 1) };' \
  feature::generation_contract_tests::twisting_vines_matches_native_cells_and_rng

check "weeping vines keeps the wart neighbour count at one" \
  src/feature.rs '        if adjacent == 1 {
            ctx.set(x, y, z, wart);' \
  '        if adjacent >= 1 {
            ctx.set(x, y, z, wart);' \
  feature::generation_contract_tests::weeping_vines_matches_native_cells_and_rng

check "end gateway clears its own y layer" \
  src/feature.rs '                } else if dy == 0 {
                    air' \
  '                } else if dy == 1 {
                    air' \
  feature::generation_contract_tests::end_gateway_matches_native_cells

check "huge brown mushroom keeps the cap's corner-adjacent rim" \
  src/feature.rs '                st = reg.with_prop(st, "west", rim(west || (dz_edge && dx == 1 - r)));' \
  '                st = reg.with_prop(st, "west", rim(west));' \
  feature::generation_contract_tests::huge_brown_mushroom_matches_native_cells_and_height

check "huge brown mushroom omits the four cap corners" \
  src/feature.rs '            if dx_edge && dz_edge {
                continue;
            }' \
  '            if false && dx_edge && dz_edge {
                continue;
            }' \
  feature::generation_contract_tests::huge_brown_mushroom_matches_native_cells_and_height

check "independent generators cannot share generated tick queues" \
  src/worldgen.rs '            generated_ticks: GeneratedTickQueues::default(),
        };' \
  '            generated_ticks: GEN_TICK_QUEUES.lock().unwrap().get_or_insert_with(GeneratedTickQueues::default).clone(),
        };' \
  worldgen::wg_tests::generated_ticks_belong_to_their_generator

check "runtime drains the published generator queues" \
  src/worldgen.rs '        *GEN_TICK_QUEUES.lock().unwrap_or_else(|e| e.into_inner()) = Some(ticks);' \
  '        *GEN_TICK_QUEUES.lock().unwrap_or_else(|e| e.into_inner()) = None;' \
  worldgen::wg_tests::runtime_drains_only_published_ticks_without_the_gen_lock

check "runtime tick drain does not wait for the generation lock" \
  src/worldgen.rs 'pub fn take_generated_leaf_ticks() -> Vec<GeneratedTick> {
    let queues = GEN_TICK_QUEUES.lock().unwrap_or_else(|e| e.into_inner()).clone();' \
  'pub fn take_generated_leaf_ticks() -> Vec<GeneratedTick> {
    let _guard = gen_lock();
    let queues = GEN_TICK_QUEUES.lock().unwrap_or_else(|e| e.into_inner()).clone();' \
  worldgen::wg_tests::runtime_drains_only_published_ticks_without_the_gen_lock

check "generated handoff retains native negative suborders" \
  src/scheduled_ticks.rs 'sub_order: -*remaining,' 'sub_order: 0,' \
  tick_tests::generated_fluid_handoff_matches_native_proto_unpack

check "generated handoff counts each chunk rather than the whole batch" \
  src/scheduled_ticks.rs '        for (pos, _) in &pending {
            *counts.entry((pos.0 >> 4, pos.2 >> 4)).or_default() += 1;
        }
        for (pos, block) in pending {
            let Some(remaining) = counts.get_mut(&(pos.0 >> 4, pos.2 >> 4)) else {
                continue;
            };' \
  '        for _ in &pending {
            *counts.entry((0, 0)).or_default() += 1;
        }
        for (pos, block) in pending {
            let Some(remaining) = counts.get_mut(&(0, 0)) else {
                continue;
            };' \
  tick_tests::generated_fluid_handoff_matches_native_proto_unpack

check "generated handoff uses the activation time" \
  src/scheduled_ticks.rs 'due: now,' 'due: 0,' \
  tick_tests::generated_fluid_handoff_matches_native_proto_unpack

check "generated handoff increments each native negative suborder" \
  src/scheduled_ticks.rs '*remaining -= 1;' '*remaining += 1;' \
  tick_tests::generated_fluid_handoff_matches_native_proto_unpack

check "fluid handoff does not consume the ordinary runtime counter" \
  src/main.rs 'self.queue.unpack_generated(pending, now);' \
  'self.next_order += pending.len() as i64; self.queue.unpack_generated(pending, now);' \
  tick_tests::generated_fluid_handoff_matches_native_proto_unpack

check "a sign message is a JSON string component" \
  src/play_protocol.rs "    o.push('\"');" "    o.push(' ');" \
  sign_side_nbt_matches_the_shape_the_game_writes

check "a sign side omits filtered_messages" \
  src/play_protocol.rs '    nbt_str(v, "messages"); // TAG_List<String>' '    nbt_str(v, "filtered_messages"); // TAG_List<String>' \
  sign_side_nbt_matches_the_shape_the_game_writes

check "the open window reads the live store, not a copy" \
  src/menus_and_items.rs '    let ch = all.get(&first).cloned().unwrap_or_default();' '    let ch: HashMap<i16, Stack> = HashMap::new();' \
  a_placed_chest_shows_its_staged_contents_in_the_open_window

check "staged containers reach the live store" \
  src/main.rs '    for (pos, payload) in staged {' '    for (pos, payload) in Vec::<(( i32,i32,i32), crate::generation_payload::GenerationPayload)>::new() {' \
  staged_container_contents_reach_the_store_and_survive_a_save

check "staged containers are taken from the generator once" \
  src/worldgen.rs '        std::mem::take(&mut self.staged_payloads)' '        Vec::new()' \
  staged_container_contents_reach_the_store_and_survive_a_save

check "the backoff guard holds a visit past the queue cap" \
  src/main.rs 'fn sched_ready(x: i32, y: i32, z: i32) -> bool {
    let now = TICK_IDX.load(Ordering::Relaxed);' 'fn sched_ready(x: i32, y: i32, z: i32) -> bool {
    let now = TICK_IDX.load(Ordering::Relaxed) + 10_000;' \
  a_backoff_past_the_queue_cap_waits_for_its_real_deadline

check "freeze_top_layer reads MOTION_BLOCKING rather than WORLD_SURFACE" \
  src/feature.rs '                    let top = ctx.heightmap(HeightmapKind::MotionBlocking, x, z);
                    let Some(biome) = ctx.biome_leaf(x, top, z) else {' '                    let top = ctx.heightmap(HeightmapKind::WorldSurface, x, z);
                    let Some(biome) = ctx.biome_leaf(x, top, z) else {' \
  freeze_top_layer_matches_native_oracle

check "freeze_top_layer temperature guard gates snow placement" \
  src/feature.rs '                        && ctx.cold_enough_to_snow(biome, x, top, z)' '                        && true' \
  freeze_top_layer_matches_native_oracle

check "freeze_top_layer sets snowy=true on the dirt below placed snow" \
  src/feature.rs '                                ctx.set(x, below_y, z, reg.with_prop(below, "snowy", "true"));' '                                let _ = below_y;' \
  freeze_top_layer_matches_native_oracle

check "freeze_top_layer refuses snow on snow_layer_cannot_survive_on blocks" \
  src/feature.rs '    if cannot {
        return false;
    }' '    if cannot {
        return true;
    }' \
  freeze_top_layer_matches_native_oracle

check "end_spike seed cache masks nextLong with 0xFFFF" \
  src/feature.rs '    let key = (seed_rng.next_i64() as u64) & 0xFFFF;' \
  '    let key = seed_rng.next_i64() as u64;' \
  end_spike_matches_native_oracle

check "end_spike clears surrounding air only above y = 65" \
  src/feature.rs '                } else if y > 65 && !ctx.out_of_h(y) {' \
  '                } else if y >= 65 && !ctx.out_of_h(y) {' \
  end_spike_matches_native_oracle

check "end_spike sets iron_bars cage edge connections when guarded" \
  src/feature.rs '                        if connect_x && dz != -2 {
                            st = reg.with_prop(st, "north", "true");
                        }' \
  '                        if false && connect_x && dz != -2 {
                            st = reg.with_prop(st, "north", "true");
                        }' \
  end_spike_matches_native_oracle

check "end_spike consumes crystal yaw next_f32 draw" \
  src/feature.rs '    let _crystal_yaw = rng.next_f32() * 360.0;' \
  '    let _crystal_yaw = 0.0f32;' \
  end_spike_matches_native_oracle

check "the canonical state keeps its properties" \
  src/block_registry.rs '            format!("{}[{}]", name, parts.join(","))' '            name' \
  overworld_chunk_matches_native_capture_state_for_state

check "an Overworld chunk falls back to the deep state" \
  src/worldgen.rs '            let st = self.deep_state_cached(x, y, z, cc);
            let st = self.vein_override(cc, x, y, z, st);' \
  '            let st = self.def_st;
            let st = self.vein_override(cc, x, y, z, st);' \
  overworld_chunk_matches_native_capture_state_for_state

check "an entity offset flips the axis native flips" \
  src/structure_transform.rs '        Mirror::LeftRight => z = 1.0 - z,' '        Mirror::LeftRight => x = 1.0 - x,' \
  structure_transform::tests::entity_offset_transform_matches_native

check "an entity offset keeps the pivot" \
  src/structure_transform.rs '            f64::from(px.wrapping_add(pz).wrapping_add(1)) - z,' '            f64::from(px.wrapping_sub(pz).wrapping_add(1)) - z,' \
  structure_transform::tests::entity_offset_transform_matches_native

check "a placed sign stages its front lines" \
  src/structure_template.rs '            return Ok(GenerationPayload::Sign(Self::sign_lines(tag)?));' '            return Ok(GenerationPayload::Sign(Default::default()));' \
  template_sign_text_matches_native_and_reaches_the_sign_store

check "a sign message that is not a string component is refused" \
  src/structure_template.rs '                return Err("a sign message must be a string component");' '                lines[slot] = String::new(); continue;' \
  unsupported_sign_components_are_refused

check "staged signs reach the sign store" \
  src/main.rs '                    .insert((DimId::Overworld, pos.0, pos.1, pos.2, 1), lines);' '                    .insert((DimId::Overworld, pos.0, pos.1, pos.2, 0), lines);' \
  template_sign_text_matches_native_and_reaches_the_sign_store

check "a furnace is staged as a furnace, not a chest" \
  src/generation_payload.rs '            "minecraft:furnace" | "minecraft:blast_furnace" | "minecraft:smoker" => {
                ContainerKind::Furnace
            }' '            "minecraft:furnace" | "minecraft:blast_furnace" | "minecraft:smoker" => {
                ContainerKind::Chest
            }' \
  template_furnace_contents_reach_the_furnace_store

check "a furnace's loot table is not dropped for want of a store" \
  src/generation_payload.rs '            "minecraft:furnace" | "minecraft:blast_furnace" | "minecraft:smoker" => {
                ContainerKind::Furnace
            }' '            "minecraft:furnace" | "minecraft:blast_furnace" | "minecraft:smoker" => {
                return None;
            }' \
  jigsaw_container_states_reach_the_store_that_owns_them

check "a jigsaw piece stages its container Items" \
  src/jigsaw_structures.rs 'Ok(payload) => ctx.payloads.push((p, payload)),' 'Ok(_payload) => {}' \
  jigsaw_furnace_contents_reach_the_furnace_store

# Native runs `StructureTemplate.placeInWorld`'s two shape passes for every caller,
# including `SinglePoolElement`. Dropping them leaves a fence/pane/wall placed by an
# earlier piece unconnected to a block a later piece writes beside it (measured on
# seed 4242 chunk (-96,35): a mineshaft rail beside a trial-chamber wall).
check "a jigsaw piece runs placeInWorld's shape passes" \
  src/jigsaw_structures.rs '        let changed = crate::structure_template::apply_shape_passes(&written, ctx, reg);' \
  '        let changed: Vec<(Pos, u32)> = Vec::new();' \
  jigsaw_piece_shape_pass_connects_an_earlier_fence_to_its_wall

check "a jungle temple dispenser is staged as a dispenser, not a chest" \
  src/single_piece_structures.rs '                kind: ContainerKind::BlockContainer(9),' '                kind: ContainerKind::Chest,' \
  single_piece_container_loot_is_staged_and_archaeology_is_not

check "a small container rolls only into its own slots" \
  src/menus_and_items.rs '                                        ContainerKind::BlockContainer(size) => {
                                            i16::from(size)
                                        }' '                                        ContainerKind::BlockContainer(_) => 27i16,' \
  container_loot_roll_stays_inside_the_block_its_table_came_from

check "a container with no store is refused" \
  src/structure_template.rs '                return Err("template container NBT on a block with no supported store");' '                ContainerKind::Chest' \
  unsupported_container_states_are_refused

check "furnace contents land in the furnace store" \
  src/main.rs '                                0 => furnace.input = stack,' '                                0 => furnace.output = stack,' \
  template_furnace_contents_reach_the_furnace_store

check "a placed container stages its contents" \
  src/structure_template.rs '                    ctx.payloads
                        .push((p, Self::staged_payload(tag, state, reg)?));' '                    let _ = Self::staged_payload(tag, state, reg)?;' \
  template_container_items_match_native_placement

check "template NBT other than container Items is refused" \
  src/structure_template.rs '            if key != "Items" {' '            if false {' \
  the_fossil_contract_still_refuses_a_container

check "a block write is not split from its tick booking by a save" \
  src/main.rs '    with_save_guard(|| set_block_if_inner_dim(dim, x, y, z, st, expected))' '    set_block_if_dim_edit(dim, x, y, z, st, expected)' \
  tick_tests::a_block_write_waits_for_an_in_progress_save_before_booking

check "fluid handoff adopts the generated positions" \
  src/main.rs 'self.owned.extend(pending.iter().map(|&(pos, _)| pos));' 'let _ = pending.iter();' \
  tick_tests::generated_fluid_handoff_matches_native_proto_unpack

check "dispatcher fixture cannot initialize the parent generator" \
  src/tick_tests.rs $'if is_parent {\n        let mut child' $'if false {\n        let mut child' \
  tick_tests::generated_dispatchers_unpack_the_published_generator_queues

check "fluid dispatcher imports its published generator queue" \
  src/main.rs 'owner.unpack_generated(generated, now);' 'let _ = generated;' \
  tick_tests::generated_dispatchers_unpack_the_published_generator_queues

check "block dispatcher imports its published generator queue" \
  src/main.rs '        block_ticks()
            .lock()
            .unwrap()
            .unpack_generated(generated, now);' '        let _ = generated;' \
  tick_tests::generated_dispatchers_unpack_the_published_generator_queues

check "shared voxel faces retain native axis order" \
  src/voxel_faces.rs '2 | 3 => (0, p.0, p.1, p.2.wrapping_add(positive)),' \
  '2 | 3 => (3, p.0, p.1, p.2.wrapping_add(positive)),' \
  voxel_faces::tests::exposed_faces_match_actual_native_sequence

check "shared voxel faces omit internal occupied neighbors" \
  src/voxel_faces.rs 'if !occupied.contains(&q) {' 'if occupied.contains(&q) {' \
  voxel_faces::tests::exposed_faces_match_actual_native_sequence

check "shared voxel faces deduplicate written positions" \
  src/voxel_faces.rs 'let occupied: HashSet<_> = points.into_iter().collect();' \
  'let occupied: Vec<_> = points.into_iter().collect();' \
  voxel_faces::tests::exposed_faces_match_actual_native_sequence

check "shared voxel faces preserve transition direction" \
  src/voxel_faces.rs 'faces.push((direction, p, q));' 'faces.push((direction ^ 1, p, q));' \
  voxel_faces::tests::exposed_faces_match_actual_native_sequence

check "tree consumes shared voxel faces in native order" \
  src/tree.rs '            for x in 0..=xs {
                let cur = x < xs && filled[x * yz + line];' \
  '            for x in (0..=xs).rev() {
                let cur = x < xs && filled[x * yz + line];' \
  tree::tests::leaf_edge_schedules_match_native_order

check "solid template consumes shared voxel faces in native order" \
  src/structure_template.rs '        for (direction, p, q) in crate::voxel_faces::exposed_faces(written.iter().copied()) {' \
  '        for (direction, p, q) in crate::voxel_faces::exposed_faces(written.iter().copied()).into_iter().rev() {' \
  feature::generation_contract_tests::fossil_feature_matches_actual_native_placement

check "fossil parser resolves the production type rather than Unknown" \
  src/feature.rs '"fossil" => Cf::Fossil(Box::new(Fossil::parse(j)?)),' \
  '"fossil" => Cf::Unknown("fossil".to_owned()),' \
  feature::generation_contract_tests::fossil_production_dispatch_matches_native

check "fossil production dispatch executes placement" \
  src/feature.rs 'config.place(ctx, rng, pos);' 'let _ = (config, ctx, rng, pos);' \
  feature::generation_contract_tests::fossil_production_dispatch_matches_native

check "fossil overlay uses the selected paired index" \
  src/feature.rs 'self.overlay[index]' 'self.overlay[(index + 1) % self.overlay.len()]' \
  feature::generation_contract_tests::fossil_feature_matches_actual_native_placement

check "fossil reads the frozen ocean floor stage" \
  src/feature.rs 'ctx.hm_oc_wg(centered.0.wrapping_add(dx), centered.1.wrapping_add(dz))' \
  'ctx.hm_oc(centered.0.wrapping_add(dx), centered.1.wrapping_add(dz))' \
  feature::generation_contract_tests::fossil_feature_matches_actual_native_placement

check "fossil centers the rotated footprint" \
  src/feature.rs 'Rotation::Clockwise90 | Rotation::Counterclockwise90 => (sz, sx),' \
  'Rotation::Clockwise90 | Rotation::Counterclockwise90 => (sx, sz),' \
  feature::generation_contract_tests::fossil_feature_matches_actual_native_placement

check "fossil depth clamps above the dimension minimum" \
  src/feature.rs '(floor.wrapping_sub(15).wrapping_sub(rng.next_bound(10))).max(ctx.g.min_y + 10)' \
  '(floor.wrapping_sub(15).wrapping_sub(rng.next_bound(10)))' \
  feature::generation_contract_tests::fossil_feature_matches_actual_native_placement

check "fossil empty-corner threshold retains equality" \
  src/feature.rs 'if empty > self.max_empty_corners {' 'if empty >= self.max_empty_corners {' \
  feature::generation_contract_tests::fossil_feature_matches_actual_native_placement

check "fossil writes the base before processing the overlay" \
  src/feature.rs $'        base.place_solid_blocks(ctx, rng, placement, &self.base_processors, clip)\n            .expect("prevalidated fossil base");\n        self.overlay[index]\n            .place_solid_blocks(ctx, rng, placement, &self.overlay_processors, clip)\n            .expect("prevalidated fossil overlay");' \
  $'        self.overlay[index].place_solid_blocks(ctx, rng, placement, &self.overlay_processors, clip).unwrap();\n        base.place_solid_blocks(ctx, rng, placement, &self.base_processors, clip).unwrap();' \
  feature::generation_contract_tests::fossil_feature_matches_actual_native_placement

check "solid template palette selection advances the shared RNG" \
  src/structure_template.rs 'let palette = rng.next_bound(self.palettes.len() as i32) as usize;' 'let palette = 0;' \
  feature::generation_contract_tests::fossil_feature_matches_actual_native_placement

check "solid template clipping does not skip processor draws" \
  src/structure_template.rs '|_| rng.next_f32(),' \
  '|p| if p.0 < clip[0] || p.1 < clip[1] || p.2 < clip[2] || p.0 > clip[3] || p.1 > clip[4] || p.2 > clip[5] { 0.0 } else { rng.next_f32() },' \
  feature::generation_contract_tests::fossil_feature_matches_actual_native_placement

check "solid template clipping retains the upper window face" \
  src/structure_template.rs '|| p.0 > clip[3]' '|| p.0 >= clip[3]' \
  feature::generation_contract_tests::fossil_window_clipping_matches_native

check "solid template states rotate at placement" \
  src/structure_template.rs $'                block.state,\n                placement.mirror,\n                placement.rotation,' \
  $'                block.state,\n                placement.mirror,\n                crate::structure_transform::Rotation::None,' \
  feature::generation_contract_tests::fossil_feature_matches_actual_native_placement

check "solid template boundary schedules source water" \
  src/structure_template.rs '"minecraft:water" => ctx.g.schedule_generated_fluid(q, 0),' '"minecraft:water" => (),' \
  feature::generation_contract_tests::fossil_feature_matches_actual_native_placement

check "solid template boundary schedules source lava" \
  src/structure_template.rs '"minecraft:lava" => ctx.g.schedule_generated_fluid(q, 2),' '"minecraft:lava" => (),' \
  feature::generation_contract_tests::fossil_feature_matches_actual_native_placement

check "fossil parser rejects unimplemented template outputs" \
  src/feature.rs 'template.validate_solid_placement(processors, reg).ok()?;' 'let _ = (template, processors, reg);' \
  feature::generation_contract_tests::fossil_required_resources_and_children_fail_closed

check "datapack resource lookup keeps the highest-priority pack" \
  src/worldgen.rs 'for pack in packs.iter().rev() {' 'for pack in packs.iter() {' \
  worldgen::wg_tests::datapack_resources_respect_priority_and_confine_id_paths

check "non-directory pack entries do not mask lower resources" \
  src/worldgen.rs '|| e.kind() == std::io::ErrorKind::NotADirectory' '|| false' \
  worldgen::wg_tests::datapack_resources_respect_priority_and_confine_id_paths

check "datapack resource namespaces cannot escape data roots" \
  src/worldgen.rs '            || matches!(namespace, "." | "..")' '' \
  worldgen::wg_tests::datapack_resources_respect_priority_and_confine_id_paths

check "datapack resource paths cannot traverse the resource kind" \
  src/worldgen.rs 'std::path::Component::Normal(_)' 'std::path::Component::Normal(_) | std::path::Component::ParentDir' \
  worldgen::wg_tests::datapack_resources_respect_priority_and_confine_id_paths

check "template rot eligibility uses the original state snapshot" \
  src/structure_template.rs 'reg.name_of(original.state)' 'reg.name_of(current.state)' \
  structure_template::tests::processor_stage_matches_native_cells_and_rng

check "template rules see preceding processor output" \
  src/structure_template.rs 'r.input.matches(current.state, reg)' 'r.input.matches(original.state, reg)' \
  structure_template::tests::processor_stage_matches_native_cells_and_rng

check "template protection reads the world rather than the candidate" \
  src/structure_template.rs 'reg.in_tag(world(current.pos), tag)' 'reg.in_tag(current.state, tag)' \
  structure_template::tests::processor_stage_matches_native_cells_and_rng

check "template processors use transformed world coordinates" \
  src/structure_template.rs 'p.0.wrapping_add(placement.origin.0)' 'p.0' \
  structure_template::tests::processor_stage_matches_native_cells_and_rng

check "template state rotation belongs after processors" \
  src/structure_template.rs 'let mut current = original.clone();' \
  'let mut current = original.clone(); current.state = crate::structure_transform::state(current.state, placement.mirror, placement.rotation).unwrap();' \
  structure_template::tests::processor_stage_matches_native_cells_and_rng

check "template rot still draws at full integrity" \
  src/structure_template.rs '&& draw(current.pos) > *integrity' \
  '&& *integrity < 1.0 && draw(current.pos) > *integrity' \
  structure_template::tests::processor_stage_matches_native_cells_and_rng

check "template rot retains equality at zero integrity" \
  src/structure_template.rs 'draw(current.pos) > *integrity' 'draw(current.pos) >= *integrity' \
  structure_template::tests::processor_stage_matches_native_cells_and_rng

check "template processor lists do not drop unsupported children" \
  src/structure_template.rs 'processors.iter().map(|p| Self::parse(p, reg)).collect()' \
  'processors.iter().filter_map(|p| Self::parse(p, reg).ok()).map(Ok).collect()' \
  structure_template::tests::processor_lists_reject_unsupported_required_children

check "template integrity codec rejects negative zero" \
  src/structure_template.rs ' || integrity.is_sign_negative()' '' \
  structure_template::tests::processor_lists_reject_unsupported_required_children

check "template gravity keeps original block y offset" \
  src/structure_template.rs 'current.pos.1 = surface.wrapping_add(*offset).wrapping_add(original.pos.1);' \
  'current.pos.1 = surface.wrapping_add(*offset);' \
  structure_template::tests::gravity_processor_shifts_y_by_heightmap_offset_and_original_y

check "template blackstone replace drops unrelated state properties" \
  src/structure_template.rs 'for prop in ["facing", "half", "type"] {' \
  'for prop in ["facing", "half", "type", "waterlogged"] {' \
  structure_template::tests::blackstone_replace_processor_preserves_facing_half_type_and_drops_unrelated_props

check "template lava submerged skips collision-full blocks" \
  src/structure_template.rs '&& !reg.is_collision_shape_full_block(current.state)' \
  '&& true' \
  structure_template::tests::lava_submerged_block_processor_replaces_non_full_blocks_in_lava

check "template capped processor derives factory seed from world seed LCG" \
  src/structure_template.rs 'let factory_seed = crate::generation_math::Lcg::new(world_seed).next_i64();' \
  'let factory_seed = world_seed as i64;' \
  structure_template::tests::capped_processor_uses_world_seed_positional_rng_and_shuffled_limit

check "template capped processor enforces limit count" \
  src/structure_template.rs 'if replaced >= cap {' \
  'if replaced > cap {' \
  structure_template::tests::capped_processor_uses_world_seed_positional_rng_and_shuffled_limit

check "template rottable tag membership evaluates tag" \
  src/structure_template.rs 'Some(RottableBlocks::Tag(tag)) => reg.in_tag(original.state, tag),' \
  'Some(RottableBlocks::Tag(_)) => false,' \
  structure_template::tests::rule_processor_supports_tag_blockstate_random_axis_pos_and_rottable_tag

check "template tag_match rule evaluates tag" \
  src/structure_template.rs 'Self::Tag(tag) => reg.in_tag(state, tag),' \
  'Self::Tag(_) => false,' \
  structure_template::tests::rule_processor_supports_tag_blockstate_random_axis_pos_and_rottable_tag

check "template loot container staging reads LootTable" \
  src/structure_template.rs 'if let Some(loot_tag) = tag.get("LootTable") {' \
  'if let Some(loot_tag) = tag.get("MissingLootTable") {' \
  structure_template::tests::loot_table_chest_and_decorative_block_entities_place_and_materialize

check "template placement passes decorative block entities through without staging failure" \
  src/structure_template.rs '.filter(|tag| !Self::is_passthrough_block_entity(state, tag, reg))' \
  '.filter(|_| true)' \
  structure_template::tests::loot_table_chest_and_decorative_block_entities_place_and_materialize

check "template loot container materialization rolls mineshaft cart loot" \
  src/menus_and_items.rs 'if loot_table == "minecraft:chests/abandoned_mineshaft" {' \
  'if false {' \
  structure_template::tests::loot_table_chest_and_decorative_block_entities_place_and_materialize

check "template placement updates inside boundary block shapes" \
  src/structure_template.rs '            if let Some(state) = shape_at(ctx, p, inside, outside, direction) {' \
  '            if let Some(state) = None::<u32> {' \
  structure_template::tests::place_blocks_updates_inside_boundary_shapes_and_schedules_inside_fluids

check "template placement schedules generated water on inside boundary" \
  src/structure_template.rs '"minecraft:water" => ctx.g.schedule_generated_fluid(p, 0),' \
  '"minecraft:water" => (),' \
  structure_template::tests::place_blocks_updates_inside_boundary_shapes_and_schedules_inside_fluids

check "sculk vein spreads from a present face toward an absent face" \
  src/feature.rs 'if is_vein && !(spc_has_face(reg, state, from) && !spc_has_face(reg, state, to)) {' \
  'if is_vein && !(spc_has_face(reg, state, to) && !spc_has_face(reg, state, from)) {' \
  feature::generation_contract_tests::sculk_vein_spreads_from_its_existing_face

check "sculk wrap-around uses the source corner" \
  src/feature.rs 'let side = (pos.0 + ox, pos.1 + oy, pos.2 + oz);' \
  'let side = (target.0 + ox, target.1 + oy, target.2 + oz);' \
  feature::generation_contract_tests::sculk_vein_wrap_checks_the_source_corner

check "sculk vein rereads faces after spread" \
  src/feature.rs 'if kind != SpcKind::Sculk {' 'if kind == SpcKind::Other {' \
  feature::generation_contract_tests::sculk_vein_cursor_remembers_faces_added_by_spread

check "sculk exhausted cursor discharges its captured state" \
  src/feature.rs '        spc_decay(ctx, reg, cursor, kind, state);
        return;' \
  '        let live = ctx.get(cursor.pos.0, cursor.pos.1, cursor.pos.2);
        spc_decay(ctx, reg, cursor, kind, live);
        return;' \
  feature::generation_contract_tests::sculk_cursor_discharges_the_captured_state_after_conversion

check "sculk moving cursor discharges its captured state" \
  src/feature.rs '        spc_decay(ctx, reg, cursor, kind, state);
        cursor.pos = next;' \
  '        let live = ctx.get(cursor.pos.0, cursor.pos.1, cursor.pos.2);
        spc_decay(ctx, reg, cursor, kind, live);
        cursor.pos = next;' \
  feature::generation_contract_tests::sculk_cursor_discharges_the_captured_state_after_conversion

check "sculk known empty faces are not unknown" \
  src/feature.rs 'cursor.facings = Some(spc_face_mask(reg, state));' \
  'cursor.facings = (spc_face_mask(reg, state) != 0).then_some(spc_face_mask(reg, state));' \
  feature::generation_contract_tests::sculk_cursor_keeps_empty_faces_distinct_from_unknown

check "sculk decay numerator squares the excess distance" \
  src/feature.rs 'let over = excess * excess;' 'let over = excess;' \
  feature::generation_contract_tests::sculk_charge_decay_matches_native

check "sculk decay denominator uses square(int), not ceil" \
  src/feature.rs 'let denom = (24 - SPC_NO_GROWTH_RADIUS).pow(2) as f32;' \
  'let denom = (24 - SPC_NO_GROWTH_RADIUS) as f32;' \
  feature::generation_contract_tests::sculk_charge_decay_matches_native

check "recorded population mode accepts observed extra sources" \
  src/worldgen.rs 'if !allow_extra && (x < x0 || x > x1 || z < z0 || z > z1) {' \
  'if x < x0 || x > x1 || z < z0 || z > z1 {' \
  worldgen::wg_tests::recorded_population_plan_preserves_extras_without_hiding_missing_sources

check "recorded population mode does not silently drop extra sources" \
  src/worldgen.rs '        order.push((x, z));' \
  '        if x0 <= x && x <= x1 && z0 <= z && z <= z1 { order.push((x, z)); }' \
  worldgen::wg_tests::recorded_population_plan_preserves_extras_without_hiding_missing_sources

check "recorded population extras cannot replace a missing required source" \
  src/worldgen.rs '    let covered = seen
        .iter()
        .filter(|&&(x, z)| x0 <= x && x <= x1 && z0 <= z && z <= z1)
        .count();' \
  '    let covered = expected;' \
  worldgen::wg_tests::recorded_population_plan_preserves_extras_without_hiding_missing_sources

check "typed feature draws exclude raw positional RNGs" \
  src/generation_math.rs 'let source = if wrapped { "DRAW" } else { "RAW_DRAW" };' \
  'let source = "DRAW";' \
  generation_math::tests::draw_trace_separates_wrapped_and_raw_sources

check "dump-only requests cannot fall through to runtime persistence" \
  src/main.rs '    if std::env::var_os("MEGU_WG_DUMP").is_some()
        && std::env::var_os("MEGU_WG_PROFILE").is_none()' \
  '    if std::env::var_os("MEGU_WG_DUMP").is_some()
        && false' tests/population_plan_test.py 'population plan: FAIL missing-profile'

# Both huge mushrooms and huge fungus double height. Bind this mutation to the
# mushroom's initial height draw; a bare `height *= 2` now matches both bodies.
check "huge brown mushroom doubles its height on a nextInt(12) draw" \
  src/feature.rs '    let mut height = rng.next_bound(3) + 4;
    if rng.next_bound(12) == 0 {
        height *= 2;
    }' \
  '    let mut height = rng.next_bound(3) + 4;
    if rng.next_bound(12) == 0 {
        height += 0;
    }' \
  feature::generation_contract_tests::huge_brown_mushroom_matches_native_cells_and_height

check "huge brown mushroom accepts the whole dirt tag as ground" \
  src/feature.rs '    if !reg.in_tag(below, "minecraft:dirt") && !reg.in_tag(below, "minecraft:mushroom_grow_block") {' \
  '    if !reg.in_tag(below, "minecraft:mushroom_grow_block") {' \
  feature::generation_contract_tests::huge_brown_mushroom_matches_native_cells_and_height

check "huge red mushroom checks the centre column only" \
  src/feature.rs '            if (y < b && y >= b - 3) || y == b {' \
  '            if y > 3 {' \
  feature::generation_contract_tests::huge_red_mushroom_matches_native_cells_and_layers

check "huge red mushroom writes only the rim below its top layer" \
  src/feature.rs '                if dy < height && dx_edge == dz_edge {' \
  '                if dy < height && dx_edge && dz_edge {' \
  feature::generation_contract_tests::huge_red_mushroom_matches_native_cells_and_layers

check "huge red mushroom shrinks its top layer by one radius" \
  src/feature.rs '        let r = if dy < height { radius } else { radius - 1 };' \
  '        let r = radius;' \
  feature::generation_contract_tests::huge_red_mushroom_matches_native_cells_and_layers

check "huge red mushroom sets up on the two top layers" \
  src/feature.rs '                    st = reg.with_prop(st, "up", side(dy >= height - 1));' \
  '                    st = reg.with_prop(st, "up", side(dy >= height));' \
  feature::generation_contract_tests::huge_red_mushroom_matches_native_cells_and_layers

check "huge red mushroom uses the foliageRadius-2 rim threshold" \
  src/feature.rs '    let rim = radius - 2;' \
  '    let rim = radius - 1;' \
  feature::generation_contract_tests::huge_red_mushroom_matches_native_cells_and_layers

check "pointed dripstone hangs and stands in the drawn direction" \
  src/feature.rs '        !rng.next_bool() // `nextBoolean() ? DOWN : UP`' \
  '        rng.next_bool() // `nextBoolean() ? DOWN : UP`' \
  feature::generation_contract_tests::pointed_dripstone_matches_native_cells_and_rng

check "pointed dripstone gates its taller tip on the configured chance" \
  src/feature.rs '    let taller = rng.next_f32() < c.taller && drip_is_empty_or_water(ctx, beyond);' \
  '    let taller = rng.next_f32() < 1.0 && drip_is_empty_or_water(ctx, beyond);' \
  feature::generation_contract_tests::pointed_dripstone_matches_native_cells_and_rng

check "pointed dripstone only grows its taller tip into empty or water" \
  src/feature.rs '    let taller = rng.next_f32() < c.taller && drip_is_empty_or_water(ctx, beyond);' \
  '    let taller = rng.next_f32() < c.taller;' \
  feature::generation_contract_tests::pointed_dripstone_matches_native_cells_and_rng

check "pointed dripstone spreads with a six-way direction draw" \
  src/feature.rs '    VALUES[rng.next_bound(6) as usize]' \
  '    VALUES[rng.next_bound(4) as usize]' \
  feature::generation_contract_tests::pointed_dripstone_matches_native_cells_and_rng

check "pointed dripstone spreads in the horizontal plane order" \
  src/feature.rs '    // `Direction.Plane.HORIZONTAL`: NORTH, EAST, SOUTH, WEST
    for (dx, dz) in [(0, -1), (1, 0), (0, 1), (-1, 0)] {' \
  '    // `Direction.Plane.HORIZONTAL`: NORTH, EAST, SOUTH, WEST
    for (dx, dz) in [(1, 0), (0, -1), (0, 1), (-1, 0)] {' \
  feature::generation_contract_tests::pointed_dripstone_matches_native_cells_and_rng

check "coral shuffle draws nextInt(i) and keeps the last slot for the swap" \
  src/feature.rs '        let j = rng.next_bound(i as i32) as usize;' \
  '        let j = rng.next_bound(i as i32 + 1) as usize;' \
  feature::generation_contract_tests::coral_tree_matches_native_cells_and_rng

check "coral claw runs its tail along the drawn direction, not the arm" \
  src/feature.rs '            m.0 += ddx;' \
  '            m.0 += adx;' \
  feature::generation_contract_tests::coral_claw_matches_native_cells_and_rng

check "coral mushroom keeps only the box's faces" \
  src/feature.rs '                    && (lx == 0 || lx == xb || ly == 0 || ly == yb || lz == 0 || lz == zb);' \
  '                    && (lx == 0 || lx == xb || ly == 0 || ly == yb || lz == 0 || lz == zb || true);' \
  feature::generation_contract_tests::coral_mushroom_matches_native_cells_and_rng

check "coral mushroom lowers its cap by the drawn length" \
  src/feature.rs '                    coral_piece(ctx, rng, (pos.0 + lx, pos.1 + ly - len, pos.2 + lz), state);' \
  '                    coral_piece(ctx, rng, (pos.0 + lx, pos.1 + ly, pos.2 + lz), state);' \
  feature::generation_contract_tests::coral_mushroom_matches_native_cells_and_rng

check "coral piece accepts a coral plant as its own cell" \
  src/feature.rs '    if !(water(at) || reg.in_tag(at, "minecraft:corals")) || !water(at_above) {' \
  '    if !water(at) || !water(at_above) {' \
  feature::generation_contract_tests::coral_tree_matches_native_cells_and_rng

check "huge fungus extends its hat from the block below" \
  src/feature.rs '    if reg.name_of(ctx.get(p.0, p.1 - 1, p.2)) == reg.name_of(hat) {' \
  '    if reg.name_of(ctx.get(p.0, p.1 + 1, p.2)) == reg.name_of(hat) {' \
  feature::generation_contract_tests::huge_fungus_matches_native_cells_and_rng

check "huge fungus widens its widest hat rows" \
  src/feature.rs '        if layers > 8 && dy < start + 4 {' \
  '        if layers > 9 && dy < start + 4 {' \
  feature::generation_contract_tests::huge_fungus_matches_native_cells_and_rng

check "huge fungus keeps the top hat row solid" \
  src/feature.rs '                let interior = !x_edge && !z_edge && dy != height;' \
  '                let interior = !x_edge && !z_edge;' \
  feature::generation_contract_tests::huge_fungus_matches_native_cells_and_rng

check "huge fungus doubles a half of its vine lengths" \
  src/feature.rs '    let mut len = rng.next_inbetween(1, 5);
    if rng.next_bound(7) == 0 {
        len *= 2;' \
  '    let mut len = rng.next_inbetween(1, 5);
    if rng.next_bound(8) == 0 {
        len *= 2;' \
  feature::generation_contract_tests::huge_fungus_crimson_matches_native_cells_and_rng

check "huge fungus only grows its hat into replaceable cells" \
  src/feature.rs '                if !hf_can_place(ctx, p, c, false) {' \
  '                if !hf_can_place(ctx, p, c, true) {' \
  feature::generation_contract_tests::huge_fungus_matches_native_cells_and_rng

check "feature writes preserve frozen WG heightmaps while moving live heights" \
  src/generation_context.rs '    pub(crate) fn heightmap(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {' \
  '    pub(crate) fn heightmap(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
        let kind = match kind {
            HeightmapKind::WorldSurfaceWg => HeightmapKind::WorldSurface,
            HeightmapKind::OceanFloorWg => HeightmapKind::OceanFloor,
            other => other,
        };' \
  feature::generation_contract_tests::wg_heightmaps_stay_frozen_while_live_heightmaps_follow_writes

check "basalt columns draws the candidate's y as well" \
  src/feature.rs '        let y = pos.1 + rng.next_bound(1);' \
  '        let y = pos.1;' \
  feature::generation_contract_tests::basalt_columns_matches_native_cells_and_rng

check "basalt columns walks down to a cell that can start a column" \
  src/feature.rs '        if bc_can_start(ctx, reg, sea, m) {' \
  '        if bc_air_or_lava_ocean(ctx, reg, sea, m) {' \
  feature::generation_contract_tests::basalt_columns_matches_native_cells_and_rng

check "basalt columns' origin must have a buildable block below" \
  src/feature.rs '    if !bc_can_start(ctx, reg, sea, pos) {' \
  '    if !bc_air_or_lava_ocean(ctx, reg, sea, pos) {' \
  feature::generation_contract_tests::basalt_columns_matches_native_cells_and_rng

check "basalt columns keeps bedrock out of its start cells" \
  src/feature.rs 'const BC_CANNOT_PLACE_ON: [&str; 10] = [
    "minecraft:lava",
    "minecraft:bedrock",' \
  'const BC_CANNOT_PLACE_ON: [&str; 10] = [
    "minecraft:lava",
    "minecraft:magma_block",' \
  feature::generation_contract_tests::basalt_columns_matches_native_cells_and_rng

check "basalt columns' clustered roll picks 50 attempts" \
  src/feature.rs '    let attempts = if clustered { 50 } else { 15 };' \
  '    let attempts = if clustered { 15 } else { 50 };' \
  feature::generation_contract_tests::basalt_columns_matches_native_cells_and_rng

check "feature context reads carver overrides" \
  src/generation_context.rs '        if let Some(st) = self.feature_override(x, y, z) {' \
  '        if let Some(st) = Option::<u32>::None {' \
  feature_context_sees_carver_writes

check "normal mineshaft layout consumes the discarded double before its room" \
  src/mineshaft_layout.rs '    random.next_f64();' '    // omitted draw' \
  mineshaft_layout::tests::normal_layout_matches_native_pieces_and_rng_boundaries

check "normal mineshaft layout permits children at parent depth eight" \
  src/mineshaft_layout.rs 'if parent_depth > 8' 'if parent_depth > 7' \
  mineshaft_layout::tests::normal_layout_matches_native_pieces_and_rng_boundaries

check "rail corridors skip the spider RNG roll" \
  src/mineshaft_layout.rs 'let spider = !rails && self.random.next_bound(23) == 0;' \
  'let spider = self.random.next_bound(23) == 0;' \
  mineshaft_layout::tests::normal_layout_matches_native_pieces_and_rng_boundaries

check "mineshaft pieces recursively append their children" \
  src/mineshaft_layout.rs '        self.grow(index);' '        // omitted expansion' \
  mineshaft_layout::tests::normal_layout_matches_native_pieces_and_rng_boundaries

check "normal mineshaft height adjustment also moves room entrances" \
  src/mineshaft_layout.rs '*entrance = translated(*entrance, 0, vertical_shift, 0);' \
  '*entrance = translated(*entrance, 0, 0, 0);' \
  mineshaft_layout::tests::normal_layout_matches_native_pieces_and_rng_boundaries

check "large feature seeding uses the requested start chunk x" \
  src/generation_math.rs 'i64::from(x).wrapping_mul(a)' 'i64::from(0).wrapping_mul(a)' \
  mineshaft_layout::tests::normal_layout_matches_native_pieces_and_rng_boundaries

# Admission, real structure dispatch and typed-stage writer contracts.
check "mineshaft admission retries a rejected weighted entry" \
  src/mineshaft.rs '        choices.remove(selected);' '        return None;' \
  mineshaft::tests::native_admission_retries_with_fresh_layout_and_noise_biomes

check "mineshaft decoration follows registry slots rather than selector order" \
  src/mineshaft.rs '    result.sort_by_key(|(_, e)| e.slot);' \
  '    result.sort_by_key(|(_, e)| std::cmp::Reverse(e.slot));' \
  mineshaft::tests::native_registry_dispatch_order_is_independent_of_selector_order

check "mineshaft dispatcher uses the native structure slot" \
  src/mineshaft.rs '.wrapping_add(e.slot as u64)' '.wrapping_add(0)' \
  mineshaft::tests::native_dispatcher_preserves_registry_slot_and_shared_piece_rng

check "mineshaft pieces share their caller RNG rather than reseeding" \
  src/mineshaft_piece.rs '    for (i, piece) in pieces.iter_mut().enumerate() {' \
  '    for (i, piece) in pieces.iter_mut().enumerate() {
        *random = Xor::seed_worldgen(0);' \
  mineshaft::tests::native_dispatcher_preserves_registry_slot_and_shared_piece_rng

check "mineshaft writer respects its chunk clip" \
  src/mineshaft_piece.rs '        if !contains(&self.clip, p) {
            return;
        }
        let old = self.ctx.get(p.0, p.1, p.2);' \
  '        let old = self.ctx.get(p.0, p.1, p.2);' \
  mineshaft::tests::native_writer_order_clipping_and_rng_on_authored_scenes

check "mineshaft interior reads typed OCEAN_FLOOR_WG" \
  src/mineshaft_piece.rs 'HeightmapKind::OceanFloorWg' 'HeightmapKind::WorldSurfaceWg' \
  mineshaft::tests::native_writer_order_clipping_and_rng_on_authored_scenes

check "mineshaft interior compares transformed y plus one" \
  src/mineshaft_piece.rs 'let p = self.pos((p.0, p.1 + 1, p.2));' 'let p = self.pos(p);' \
  mineshaft::tests::native_writer_order_clipping_and_rng_on_authored_scenes

check "mineshaft production reaches floors below nominal corridor bounds" \
  src/mineshaft_piece.rs 'self.floor((x, -1, z));' 'self.floor((x, 0, z));' \
  mineshaft::tests::production_populate_reaches_admission_dispatch_and_out_of_bounds_floor

check "mineshaft Mesa target uses an inclusive height draw" \
  src/mineshaft_layout.rs 'builder.random.next_bound(surface - sea_level + 1)' \
  'builder.random.next_bound(surface - sea_level)' \
  mineshaft::tests::native_mesa_adjustment_uses_generator_height_and_preserves_rng

check "mineshaft anchors reject falling CENTER supports" \
  src/block_registry.rs 'v & 192 == 64' 'v & 64 != 0' \
  mineshaft::tests::mineshaft_support_distinguishes_full_faces_center_and_falling_blocks

check "shared fastutil mix preserves mineshaft reference iteration" \
  src/generation_math.rs '    h ^ (h >> 16)' '    key' \
  mineshaft::tests::native_reference_iteration_survives_duplicates_zero_and_rehash

check "shared fastutil mix preserves scheduled chunk ordering" \
  src/generation_math.rs '    h ^ (h >> 16)' '    key' \
  scheduled_ticks::tests::native_collection_order_and_deduplication

check "mineshaft structure dispatch is not gated by placed features" \
  src/feature.rs '    for step in 0..order.len().max(structures.step_count()) {
        // Structures have steps even when the biome has no placed features.
        // Their registry slots and RNG streams are independent.
        structures.place_step(&mut ctx, step, pop, sx, sz);' \
  '    for step in 0..order.len() {
        // Structures have steps even when the biome has no placed features.
        // Their registry slots and RNG streams are independent.
        structures.place_step(&mut ctx, step, pop, sx, sz);' \
  tests/mineshaft_dispatch_test.py 'mineshaft dispatch: FAIL structures without placed features'

check "mineshaft spawner block stages its native block entity" \
  src/mineshaft_piece.rs '.push((p, crate::generation_payload::GenerationPayload::Spawner));' '.extend(std::iter::empty());' \
  mineshaft::tests::native_writer_order_clipping_and_rng_on_authored_scenes

check "mineshaft chest entities retain their actual native loot seed" \
  src/mineshaft_piece.rs 'GenerationPayload::ChestMinecart { loot_seed },' 'GenerationPayload::ChestMinecart { loot_seed: 0 },' \
  mineshaft::tests::native_writer_order_clipping_and_rng_on_authored_scenes

check "structure types own independent native reference containers" \
  src/mineshaft.rs 'groups.entry(entry).or_default().push(key);' 'groups.entry(0).or_default().push(key);' \
  mineshaft::tests::native_reference_containers_do_not_share_capacity_between_types

check "generated objects are serialized before scheduled ticks" \
  src/save_v2.rs '    generated.append(f);' '    let _ = &generated;' \
  mineshaft::tests::generated_entities_and_mutated_spawner_nbt_resume_in_a_new_process_once

check "generated object source claims survive restart" \
  src/save_v2.rs '            g.payload_sources = generated.sources;' '            g.payload_sources.clear();' \
  mineshaft::tests::generated_entities_and_mutated_spawner_nbt_resume_in_a_new_process_once

check "mineshaft one-shot piece flags survive restart" \
  src/save_v2.rs '            g.mineshaft_spawners = generated.spider_flags;' '            g.mineshaft_spawners.clear();' \
  mineshaft::tests::generated_entities_and_mutated_spawner_nbt_resume_in_a_new_process_once

check "regenerating cached blocks cannot recreate generated entities" \
  src/worldgen.rs 'if self.payload_sources.insert(source) {' 'if { self.payload_sources.insert(source); true } {' \
  mineshaft::tests::generated_entities_and_mutated_spawner_nbt_resume_in_a_new_process_once

check "generated chest minecarts reach the actual entity tracker" \
  src/play_protocol.rs '    for cart in generated_content().lock().unwrap_or_else(|e| e.into_inner()).carts.iter() {' '    for cart in Vec::<generated_content::Cart>::new().iter() {' \
  mineshaft::tests::generated_entities_and_mutated_spawner_nbt_resume_in_a_new_process_once

check "generated spawner packets use the native registry type" \
  src/main.rs 'Some((pos, 9, bytes))' 'Some((pos, 8, bytes))' \
  mineshaft::tests::generated_entities_and_mutated_spawner_nbt_resume_in_a_new_process_once

check "block reads cannot take a save lock while holding entity locks" \
  src/main.rs '    worldgen::block_state(x, y, z, feature::populate)' '    with_save_guard(|| worldgen::block_state(x, y, z, feature::populate))' \
  mineshaft::tests::queued_generation_payloads_never_take_the_save_lock_on_a_cell_read

check "generated spawner handoff preserves a user air edit" \
  src/main.rs '                    .or_insert(state);
                if actual == state {
                    generated_content()
                        .lock()
                        .unwrap()
                        .spawners
                        .entry(pos)
                        .or_insert_with(|| generated_content::spawner_nbt(pos));' '                    .and_modify(|v| *v=state).or_insert(state);
                if actual == state {
                    generated_content()
                        .lock()
                        .unwrap()
                        .spawners
                        .entry(pos)
                        .or_insert_with(|| generated_content::spawner_nbt(pos));' \
  mineshaft::tests::queued_generation_payloads_never_take_the_save_lock_on_a_cell_read

check "deferred cart loot unpacks its seeded abandoned_mineshaft table" \
  src/main.rs '                let slots = roll_cart_loot(seed);' '                let slots = HashMap::new();' \
  mineshaft_runtime_tests::cart_deferred_loot_window_click_persistence_and_break_over_packets

check "unpacked empty chest minecarts persist their empty container without re-rolling" \
  src/save_v2.rs '                        fields.insert(crate::nbt::NbtString::text("Items"), stacks_to_items_nbt(&slots));' \
  '                        if !slots.is_empty() { fields.insert(crate::nbt::NbtString::text("Items"), stacks_to_items_nbt(&slots)); }' \
  mineshaft_runtime_tests::cart_deferred_loot_window_click_persistence_and_break_over_packets

check "breaking a chest minecart removes its runtime container entry" \
  src/main.rs '            if let Some(slots) = chests().lock().unwrap_or_else(|e| e.into_inner()).remove(&ContainerKey::Cart(uuid)) {' \
  '            if let Some(slots) = chests().lock().unwrap_or_else(|e| e.into_inner()).get(&ContainerKey::Cart(uuid)).cloned() {' \
  mineshaft_runtime_tests::cart_deferred_loot_window_click_persistence_and_break_over_packets

check "cave-spider spawner light check rejects lit blocks" \
  src/main.rs '        let bl = nibble_at(bl_secs, bx, by, bz);' \
  '        let bl = 0;' \
  mineshaft_runtime_tests::spawner_runtime_tick_light_cap_break_and_persistence

check "cave-spider spawner counts living nearby spiders for MaxNearbyEntities" \
  src/main.rs '            .filter(|m| m.dim == DimId::Overworld && m.etype == cave_spider && *m.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0)' \
  '            .filter(|m| m.dim == DimId::Overworld && m.etype == cave_spider && *m.hp.lock().unwrap_or_else(|e| e.into_inner()) < 0.0)' \
  mineshaft_runtime_tests::spawner_runtime_tick_light_cap_break_and_persistence

check "breaking a spawner block removes its owned NBT entry" \
  src/main.rs '    if Some(st) != name_to_state("spawner") {
        generated_content().lock().unwrap_or_else(|e| e.into_inner()).spawners.remove(&(x,y,z));
    }' \
  '    if Some(st) != name_to_state("spawner") {
        let _ = (x, y, z);
    }' \
  mineshaft_runtime_tests::spawner_runtime_tick_light_cap_break_and_persistence

check "unpacked chest minecarts use comparator fill signal in drag calculation" \
  src/main.rs '            .filter(|(_, _, _, loot_pending)| !*loot_pending)' \
  '            .filter(|(_, _, _, loot_pending)| *loot_pending)' \
  mineshaft_runtime_tests::cart_runtime_tick_motion_collision_comparator_tracker_and_persistence

check "moving chest minecarts broadcast entity teleport packets to trackers" \
  src/main.rs '        if send_pos {
            broadcast_seen(eid, |_c| {
                (
                    0x70,' \
  '        if false && send_pos {
            broadcast_seen(eid, |_c| {
                (
                    0x70,' \
  mineshaft_runtime_tests::cart_runtime_tick_motion_collision_comparator_tracker_and_persistence

check "chest minecart tick persists updated motion into generated NBT" \
  src/main.rs '                cart.set_motion(state.velocity);' \
  '                let _ = state.velocity;' \
  mineshaft_runtime_tests::cart_runtime_tick_motion_collision_comparator_tracker_and_persistence

check "buried treasure neighbor casing tests block below neighbor rather than above" \
  src/single_piece_structures.rs '                        let below_np = ctx.get(np.0, np.1 - 1, np.2);' \
  '                        let below_np = ctx.get(np.0, np.1 + 1, np.2);' \
  mineshaft_piece::single_piece_tests::single_piece_structures_match_all_1211_native_oracle_vectors

check "swamp hut stilts fill downward starting below corner log base" \
  src/single_piece_structures.rs '                w.fill_column_down(oak_log, x, -1, z);' \
  '                w.fill_column_down(oak_log, x, 0, z);' \
  mineshaft_piece::single_piece_tests::single_piece_structures_match_all_1211_native_oracle_vectors

check "desert pyramid collapsed roof picker includes the upper inclusive bound" \
  src/single_piece_structures.rs '        let rx = roof_rng.next_inbetween(roof_x0, roof_x1);' \
  '        let rx = roof_rng.next_inbetween(roof_x0, roof_x1 - 1);' \
  mineshaft_piece::single_piece_tests::single_piece_structures_match_all_1211_native_oracle_vectors

check "desert pyramid afterPlace uses exclusive nextInt(5, 8) count" \
  src/single_piece_structures.rs '        let mut count = unique_positions.len().min((rng.next_bound(8 - 5) + 5) as usize);' \
  '        let mut count = unique_positions.len().min(rng.next_inbetween(5, 8) as usize);' \
  mineshaft_piece::single_piece_tests::single_piece_structures_match_all_1211_native_oracle_vectors

check "jungle temple moss selector preserves 0.4f cobblestone threshold" \
  src/single_piece_structures.rs '            if r.next_f32() < 0.4f32 {' \
  '            if r.next_f32() < 0.5f32 {' \
  mineshaft_piece::single_piece_tests::single_piece_structures_match_all_1211_native_oracle_vectors

check "igloo basement probability draws nextDouble rather than nextFloat" \
  src/template_structures.rs '        if rng.next_f64() < 0.5 {' \
  '        if (rng.next_f32() as f64) < 0.5 {' \
  structure_template::template_structures_tests::template_structures_match_1211_native_fixture

check "nether fossil column scan requires air above soul sand or sturdy floor" \
  src/template_structures.rs '        if reg.is_air(above) && (reg.name_of(below) == "minecraft:soul_sand" || sturdy) {' \
  '        if reg.is_air(above) && sturdy {' \
  structure_template::template_structures_tests::template_structures_match_1211_native_fixture

check "beached shipwreck height lowers by half height plus random offset" \
  src/template_structures.rs '                    min_height - size.1 / 2 - post_rng.next_bound(3)' \
  '                    min_height - size.1 / 2' \
  structure_template::template_structures_tests::template_structures_match_1211_native_fixture

check "igloo top snow plug skips ladder shaft when basement is present" \
  src/template_structures.rs '                    if !reg.is_air(state) && reg.name_of(state) != "minecraft:ladder" {' \
  '                    if !reg.is_air(state) {' \
  structure_template::template_structures_tests::template_structures_match_1211_native_fixture

check "ruined portal cold variant replaces lava with netherrack" \
  src/template_structures.rs '            if props.cold {' \
  '            if false && props.cold {' \
  structure_template::template_structures_tests::template_structures_match_1211_native_fixture

check "jigsaw expansion hack applies to candidate boxes up to 16 blocks tall" \
  src/jigsaw_structures.rs '                            if structure.use_expansion_hack && bb_yspan(cand_zero_bb) <= 16 {' \
  '                            if false && structure.use_expansion_hack && bb_yspan(cand_zero_bb) <= 16 {' \
  feature::jigsaw_structures::tests::jigsaw_structures_match_1211_native_fixture

check "jigsaw legacy single pool element appends STRUCTURE_AND_AIR after jigsaw replacement" \
  src/jigsaw_structures.rs '    if legacy {
        // `Projection.getProcessors()` is appended before this, and `ekw.a(...)` appends
        // `STRUCTURE_AND_AIR` last: after `JigsawReplacement` has turned jigsaw blocks into
        // their `final_state` (often `minecraft:air`), every air cell is dropped.
        processors.push(Processor::BlockIgnore(vec![
            "minecraft:air".to_owned(),
            "minecraft:structure_block".to_owned(),
        ]));
    }' \
  '    if false && legacy {}' \
  feature::jigsaw_structures::tests::jigsaw_structures_match_1211_native_fixture

check "jigsaw capped processor shuffles candidate indices before applying limit" \
  src/jigsaw_structures.rs '            util_shuffle(&mut indices, &mut cap_rng);' \
  '            let _ = &mut cap_rng;' \
  feature::jigsaw_structures::tests::jigsaw_structures_match_1211_native_fixture

check "jigsaw replacement final_state parser accepts trailing brackets like vanilla BlockStateParser" \
  src/jigsaw_structures.rs '        let props_str = rest.split_once('\'']'\'').map(|(p, _)| p)?;' \
  '        let props_str = rest.strip_suffix('\'']'\'')?;' \
  feature::jigsaw_structures::tests::jigsaw_structures_match_1211_native_fixture

# Wave 3B: Beardifier + unified StructureSet admission. The fixture is native
# (`tools/beardifier_pipeline_oracle.py`): a germ that only moves a value inside the
# port cannot be seen at all, so every mutation below changes a constant the native
# run pinned.

check "beardifier kernel keeps vanilla's /16 exponent scale" \
  src/worldgen.rs '    std::f64::consts::E.powf(-squared / 16.0) as f32' \
  '    std::f64::consts::E.powf(-squared / 8.0) as f32' \
  feature::pipeline_tests::beardifier_density_and_structure_admission_match_1211_native

check "beardifier ground-plane term uses Mth.fastInvSqrt, not 1/sqrt" \
  src/worldgen.rs '    let f = -d * fast_inv_sqrt(squared / 2.0) / 2.0;' \
  '    let f = -d * (squared / 2.0).sqrt().recip() / 2.0;' \
  feature::pipeline_tests::beardifier_density_and_structure_admission_match_1211_native

check "beardifier kernel's +0.5 belongs to the y offset" \
  src/worldgen.rs '                    v[i * 576 + j * 24 + k] = beard_kernel_value(
                        j as i32 - BEARD_KERNEL_RADIUS,
                        k as i32 - BEARD_KERNEL_RADIUS,' \
  '                    v[i * 576 + j * 24 + k] = beard_kernel_value(
                        k as i32 - BEARD_KERNEL_RADIUS,
                        j as i32 - BEARD_KERNEL_RADIUS,' \
  feature::pipeline_tests::beardifier_density_and_structure_admission_match_1211_native

check "structure exclusion zone actually forbids the chunk" \
  src/structure_pipeline.rs '            !mineshaft::exclusion_forbids(count, x, z, |ox, oz| {
                set_admits(d, other, seed, ox, oz, depth + 1)
            })' \
  '            {
                let _ = (count, other);
                true
            }' \
  feature::pipeline_tests::beardifier_density_and_structure_admission_match_1211_native

check "preliminary (structure-free) columns stay out of the column cache" \
  src/worldgen.rs '        if self.beard_depth == 0 {' \
  '        if true {' \
  feature::pipeline_tests::preliminary_columns_are_not_cached_as_final

# `Structure.findValidGenerationPoint`: the biome gate reads the start's own position. The
# mutant restores the pre-fix chunk-centre read, which in the Nether answers a different
# biome (seed 4242 (-24,22): centre soul_sand_valley, scan position basalt_deltas).
check "a template structure's biome gate reads the start, not the chunk centre" \
  src/structure_pipeline.rs '            if !valid_biome_fast(fc.g, &def.biomes, biome_mask, stub) {
                return None;
            }
            let pieces = with_templates(|templates| build(&mut fc, templates, &mut gen, reg).ok())?;' \
  '            if !valid_biome_fast(fc.g, &def.biomes, biome_mask, (cx * 16 + 8, 64, cz * 16 + 8)) {
                return None;
            }
            let pieces = with_templates(|templates| build(&mut fc, templates, &mut gen, reg).ok())?;' \
  feature::pipeline_tests::nether_fossil_biome_gate_reads_the_start_not_the_chunk_centre

check "fortress fillColumnDown stops above the build floor" \
  src/fortress_monument.rs '        let floor = self.ctx.g.min_y + 1;' \
  '        let floor = self.ctx.g.min_y + 2;' \
  mineshaft_piece::fortress_monument_tests::fortress_and_monument_match_all_1211_native_oracle_vectors

check "monument water box compares against sea level inclusively" \
  src/fortress_monument.rs '                    if wy >= sea_level && st != s.water {' \
  '                    if wy > sea_level && st != s.water {' \
  mineshaft_piece::fortress_monument_tests::fortress_and_monument_match_all_1211_native_oracle_vectors

check "fortress turn pieces draw nextInt(3) for their chest flag" \
  src/fortress_monument.rs '            chest_pending = rng.next_bound(3) == 0;' \
  '            chest_pending = rng.next_bound(3) != 0;' \
  mineshaft_piece::fortress_monument_tests::fortress_and_monument_match_all_1211_native_oracle_vectors

check "monument core claims the east/north/up corner room" \
  src/fortress_monument.rs '    let core_east_north_up = g.room(core_east_north).connections[DIR_UP].expect("core east/north/up");' \
  '    let core_east_north_up = core_east_north;' \
  mineshaft_piece::fortress_monument_tests::fortress_and_monument_match_all_1211_native_oracle_vectors
check "end city admission reads the first occupied height and refuses below sixty" \
  src/end_city_mansion.rs '    if pos.1 < 60 {
        return None;
    }
    let builder: EndCityMansionPieceBuilder = Box::new(move |_world, templates, rng, reg| {
        let mut pieces = Vec::new();
        let _ = start_house_tower' \
  '    if pos.1 <= 60 {
        return None;
    }
    let builder: EndCityMansionPieceBuilder = Box::new(move |_world, templates, rng, reg| {
        let mut pieces = Vec::new();
        let _ = start_house_tower' \
  structure_template::end_city_mansion_tests::end_city_mansion_match_1211_native_fixture

check "end city house tower anchors its base floor at the caller's offset" \
  src/end_city_mansion.rs '            let base_offset = pos.unwrap_or((0, 0, 0));' \
  '            let base_offset = (0, 0, 0);' \
  structure_template::end_city_mansion_tests::end_city_mansion_match_1211_native_fixture

check "mansion room origin corner probes the base grid for a corridor" \
  src/end_city_mansion.rs '        if !base.edges_to(origin_x, origin_y, GRID_CORRIDOR) {
            origin_x = flip(origin_x, min_x, max_x);' \
  '        if !rooms.edges_to(origin_x, origin_y, GRID_CORRIDOR) {
            origin_x = flip(origin_x, min_x, max_x);' \
  structure_template::end_city_mansion_tests::end_city_mansion_match_1211_native_fixture

check "mansion 2x2 room mirror is the front-back flip only for the west-south entrance" \
  src/end_city_mansion.rs '            (Dir::West, Dir::South) => (15, 0, rotation, Mirror::from_index(2).unwrap()),' \
  '            (Dir::West, Dir::South) => (15, 0, rotation, Mirror::from_index(1).unwrap()),' \
  structure_template::end_city_mansion_tests::end_city_mansion_match_1211_native_fixture

check "mansion foundation stops at the first solid cell above sea level" \
  src/end_city_mansion.rs '                if !is_air_state(reg, state) && !reg.is_liquid(state) {' \
  '                if !is_air_state(reg, state) && reg.is_liquid(state) {' \
  structure_template::end_city_mansion_tests::mansion_foundation_fills_air_columns_above_sea_level
# The piece stream is `Lcg`, seeded with `seed + attempt`; the chunk-position mix lives
# in `Lcg::for_large_feature` and is mutated by the "large feature seeding" entry above.
check "stronghold retry re-seeds the legacy stream with the attempt index" \
  src/stronghold_structures.rs 'seed.wrapping_add(attempts as u64 - 1),' 'seed,' \
  mineshaft_piece::stronghold_tests::stronghold_layouts_and_writes_match_all_1211_native_oracle_vectors

check "stronghold smooth-stone selector keeps the native cracked-stone threshold" \
  src/stronghold_structures.rs '        if f < 0.2 {' \
  '        if f < 0.1 {' \
  mineshaft_piece::stronghold_tests::stronghold_layouts_and_writes_match_all_1211_native_oracle_vectors

check "stronghold library piece refuses to spawn above depth four" \
  src/stronghold_structures.rs '                        StrongholdKind::Library => depth > 4,' \
  '                        StrongholdKind::Library => true,' \
  mineshaft_piece::stronghold_tests::stronghold_layouts_and_writes_match_all_1211_native_oracle_vectors

check "stronghold small-door iron door keeps the block default facing" \
  src/stronghold_structures.rs '                let lower = state(self.reg, "minecraft:iron_door", &[]);' \
  '                let lower = state(self.reg, "minecraft:iron_door", &[("facing", "east")]);' \
  mineshaft_piece::stronghold_tests::stronghold_layouts_and_writes_match_all_1211_native_oracle_vectors

# bonus_chest (`ebo`). The feature is a shuffled walk, one heightmap read per cell, a
# chest plus one `nextLong`, and four torch attempts; each of those is a separate
# sentence in the native body, so each gets its own mutant.

check "bonus chest shuffles the chunk's X list before the Z list" \
  src/feature.rs '    feature_util_shuffle(&mut xs, rng);
    feature_util_shuffle(&mut zs, rng);' \
  '    feature_util_shuffle(&mut zs, rng);
    feature_util_shuffle(&mut xs, rng);' \
  feature::generation_contract_tests::bonus_chest_places_chest_loot_seed_and_supported_torches

check "bonus chest reads the no-leaves heightmap" \
  src/feature.rs 'let y = ctx.heightmap(HeightmapKind::MotionBlockingNoLeaves, x, z);' \
  'let y = ctx.heightmap(HeightmapKind::MotionBlocking, x, z);' \
  feature::generation_contract_tests::bonus_chest_skips_a_collision_shape_and_accepts_a_plant

check "bonus chest's cell test is an empty collision shape, not a missing one" \
  src/feature.rs 'if !(reg.is_air(cur) || reg.is_collision_empty(cur)) {' \
  'if !reg.is_air(cur) {' \
  feature::generation_contract_tests::bonus_chest_skips_a_collision_shape_and_accepts_a_plant

check "bonus chest reads the support cell below each torch, not two below" \
  src/feature.rs '                let below = ctx.get(x + dx, y - 1, z + dz);' \
  '                let below = ctx.get(x + dx, y - 2, z + dz);' \
  feature::generation_contract_tests::bonus_chest_places_chest_loot_seed_and_supported_torches

check "bonus chest spends one nextLong on the spawn-bonus loot seed" \
  src/feature.rs '            let loot_seed = rng.next_i64();' \
  '            let loot_seed = 0;' \
  feature::generation_contract_tests::bonus_chest_places_chest_loot_seed_and_supported_torches

check "bonus chest refuses a chest outside the build height" \
  src/feature.rs '            if ctx.out_of_h(y) {
                // A column filled to the build limit' \
  '            if false {
                // A column filled to the build limit' \
  feature::generation_contract_tests::bonus_chest_writes_nothing_when_the_heightmap_is_outside_the_world

# StructurePiece.placeBlock's postprocess marks and the two shape rules behind them.
check "a multiface face is cleared only when its own support is gone" \
  src/template_structures.rs '            && !reg.supports_multiface(neighbor, direction)' \
  '            && false' \
  structure_template::template_structures_tests::postprocess_clears_a_multiface_face_whose_support_is_gone

check "the postprocess mark set keeps iron bars and drops glass panes" \
  src/block_registry.rs '                | "minecraft:ladder"
                | "minecraft:iron_bars"' \
  '                | "minecraft:ladder"
                | "minecraft:glass_pane"' \
  structure_template::template_structures_tests::the_postprocess_shape_set_is_exactly_the_native_eleven

check "a mineshaft fence is marked for the owning chunk's promotion" \
  src/mineshaft_piece.rs '        if self.reg.needs_postprocess_shape(st) {
            self.ctx.mark_shape(p.0, p.1, p.2);
        }' \
  '        if false {
            self.ctx.mark_shape(p.0, p.1, p.2);
        }' \
  worldgen::wg_tests::structure_chunk_matches_native_capture_state_for_state

check "a shape pass waits for the whole 3x3, not just its own chunk" \
  src/feature.rs '            if !ctx.g.promotion_window_ready(cx, cz) {
                continue;
            }' \
  '            if false {
                continue;
            }' \
  worldgen::wg_tests::structure_chunk_matches_native_capture_state_for_state

check "an end-ship frame faces where the piece rotation puts it" \
  src/end_city_mansion.rs '                        facing: rotate_direction(rotation, Dir::South).data_3d(),' \
  '                        facing: Dir::South.data_3d(),' \
  structure_template::end_city_mansion_tests::end_ship_elytra_marker_stages_a_rotated_item_frame_with_the_elytra

check "an end-ship frame carries the elytra" \
  src/end_city_mansion.rs '                        item: Some(("minecraft:elytra".to_string(), 1)),' \
  '                        item: None,' \
  structure_template::end_city_mansion_tests::end_ship_elytra_marker_stages_a_rotated_item_frame_with_the_elytra

check "a hanging spawn stages its own payload, not a mob" \
  src/template_structures.rs '        if let Some(hanging) = entity.hanging {' \
  '        if let Some(hanging) = entity.hanging.filter(|_| false) {' \
  structure_template::template_structures_tests::a_hanging_spawn_stages_an_item_frame_payload

check "a template's hanging entity turns its facing with the piece" \
  src/template_structures.rs '        facing: rotate_direction(rotation, mirrored_direction(mirror, direction)).data_3d(),' \
  '        facing: rotate_direction(rotation, direction).data_3d(),' \
  structure_template::template_structures_tests::a_hanging_entity_reads_its_facing_and_stack_from_template_nbt

check "the tree decorator lists are bucket-ordered, not insertion-ordered" \
  src/tree.rs '        let spread = h ^ ((h as u32 >> 16) as i32);
        let b = (spread as usize) & mask;' \
  '        let b = (h as usize) & mask;' \
  feature::generation_contract_tests::tree_decorator_lists_use_hash_bucket_order_then_y

check "a trunk keeps a log below it instead of burying it in dirt" \
  src/tree.rs '        Some(reg) if !force && tree_tables(reg).is_log(reg, ctx.get(p.0, p.1, p.2)) => return,' \
  '        Some(reg) if !force && false => return,' \
  feature::generation_contract_tests::trunk_dirt_joins_the_log_list_and_a_log_below_is_kept

check "the live heightmaps use each type's own predicate" \
  src/generation_context.rs 'if reg.is_some_and(|r| r.heightmap_counts(st, kind)) {' \
  'if reg.is_some_and(|r| r.heightmap_counts(st, HeightmapKind::MotionBlocking)) {' \
  feature::generation_contract_tests::live_heightmap_types_differ_on_leaves

check "a frame's spawn carries its facing as the object data" \
  src/main.rs '        0.0,
        i64::from(facing),
    )' \
  '        0.0,
        0,
    )' \
  mineshaft_runtime_tests::item_frame_payloads_materialize_spawn_and_survive_a_save

# Track 4L: the taiga village lane. Each mutant restores one sentence of the reference body that
# the port had wrong, and each one was measured to move cells of the (4,-32)+3x3 fixture.

check "a sapling's support is the dirt tag or farmland, not dirt_path" \
  src/generation_providers.rs 'reg.in_tag(below, "minecraft:dirt") || reg.name_of(below) == "minecraft:farmland"' \
  'is_dirtish(reg, below) || reg.name_of(below) == "minecraft:farmland"' \
  feature::generation_contract_tests::bush_block_family_support_is_the_dirt_tag_or_farmland

check "spore blossom hangs from the cell above, not two above" \
  src/generation_providers.rs 'reg.side_solid(ctx.get(x, y + 1, z), 0) && !is_water_st(reg, at)' \
  'reg.side_solid(ctx.get(x, y + 2, z), 0) && !is_water_st(reg, at)' \
  feature::generation_contract_tests::spore_blossom_needs_a_sturdy_ceiling_and_a_dry_cell

check "small dripleaf's placeable supports do not need water in the cell" \
  src/generation_providers.rs 'reg.in_tag(below, "minecraft:small_dripleaf_placeable")
                || (is_water_st(reg, at)' \
  'is_water_st(reg, at) && reg.in_tag(below, "minecraft:small_dripleaf_placeable")
                || (is_water_st(reg, at)' \
  feature::generation_contract_tests::small_dripleaf_supports_are_the_placeable_tag_or_dirt_under_water

check "vines try the UP support before any horizontal one" \
  src/feature.rs 'for d in [1u8, 2, 3, 4, 5] {' \
  'for d in [2u8, 3, 4, 5] {' \
  feature::generation_contract_tests::vines_attach_to_the_first_support_in_direction_values_order

check "the vine property names the support direction, not a fixed one" \
  src/feature.rs '                    1 => "up",' \
  '                    1 => "north",' \
  feature::generation_contract_tests::vines_attach_to_the_first_support_in_direction_values_order

check "a vegetation patch column whose ground is already the patch block still joins the set" \
  src/feature.rs 'let mut add_point = true;' \
  'let mut add_point = false;' \
  feature::generation_contract_tests::vegetation_patch_keeps_a_column_whose_ground_is_already_the_patch_block

check "underwater magma's column scan tests the fifth position" \
  src/feature.rs 'while steps < *floor && is_water_block(ctx, (pos.0, y, pos.2)) {' \
  'while steps + 1 < *floor && is_water_block(ctx, (pos.0, y, pos.2)) {' \
  feature::generation_contract_tests::underwater_magma_scans_five_positions_and_ignores_the_cell_above

check "underwater magma's validity test does not look at the cell above" \
  src/feature.rs 'if wet_air(ctx, p) || wet_air(ctx, (p.0, p.1 - 1, p.2)) {' \
  'if wet_air(ctx, p) || wet_air(ctx, (p.0, p.1 - 1, p.2)) || wet_air(ctx, (p.0, p.1 + 1, p.2)) {' \
  feature::generation_contract_tests::underwater_magma_scans_five_positions_and_ignores_the_cell_above

check "underwater magma draws in betweenClosed order (x fastest, then y, then z)" \
  src/feature.rs '            for dz in -r..=r {
                for dy in -r..=r {
                    for dx in -r..=r {' \
  '            for dx in -r..=r {
                for dy in -r..=r {
                    for dz in -r..=r {' \
  feature::generation_contract_tests::underwater_magma_binds_its_probability_draws_in_betweenclosed_order

check "a disk walks its radius box in betweenClosed order (z slowest)" \
  src/feature.rs '            for cz in -rr..=rr {
                for cx in -rr..=rr {' \
  '            for cx in -rr..=rr {
                for cz in -rr..=rr {' \
  feature::generation_contract_tests::disk_walks_the_radius_box_in_between_closed_order

check "attached_to_leaves draws its probability before the required-empty check" \
  src/tree.rs '                    if rng.next_f32() >= *prob {
                        continue;
                    }
                    let mut ok = true;
                    for i in 1..=*req_empty {
                        let cp = (lp.0 + ox * i, lp.1 + oy * i, lp.2 + oz * i);
                        if !ctx.is_air(cp.0, cp.1, cp.2) {
                            ok = false;
                            break;
                        }
                    }
                    if ok {' \
  '                    let mut ok = true;
                    for i in 1..=*req_empty {
                        let cp = (lp.0 + ox * i, lp.1 + oy * i, lp.2 + oz * i);
                        if !ctx.is_air(cp.0, cp.1, cp.2) {
                            ok = false;
                            break;
                        }
                    }
                    if ok && rng.next_f32() < *prob {' \
  feature::generation_contract_tests::attached_to_leaves_draws_its_probability_before_the_required_empty_check

check "blue ice runs exactly two hundred attempts" \
  src/feature.rs '            for _ in 0..200 {
                let j = rng.next_bound(5) - rng.next_bound(6);' \
  '            for _ in 0..199 {
                let j = rng.next_bound(5) - rng.next_bound(6);' \
  feature::generation_contract_tests::blue_ice_spends_the_native_two_ints_per_attempt_stream

check "the horizontal plane is walked north, east, south, west" \
  src/tree.rs '    match i {
        0 => 2, // north
        1 => 5, // east
        2 => 3, // south
        _ => 4, // west
    }' \
  '    match i {
        0 => 3, // south
        1 => 4, // west
        2 => 5, // east
        _ => 2, // north
    }' \
  feature::generation_contract_tests::horizontal_plane_order_is_north_east_south_west

check "cocoa binds its draws to the plane order, not the reversed array" \
  src/tree.rs '                    // `for (Direction dir : Direction.Plane.HORIZONTAL)`: NORTH, EAST,
                    // SOUTH, WEST, one `nextFloat()` per direction. `p = pos.relative(
                    // dir.getOpposite())` and `FACING = dir` (`ji.g()` is read before the
                    // offset, so the cocoa sits on the near side of the log).
                    for f in plane_horizontal() {' \
  '                    // `for (Direction dir : Direction.Plane.HORIZONTAL)`: NORTH, EAST,
                    // SOUTH, WEST, one `nextFloat()` per direction. `p = pos.relative(
                    // dir.getOpposite())` and `FACING = dir` (`ji.g()` is read before the
                    // offset, so the cocoa sits on the near side of the log).
                    for f in [3u8, 4, 5, 2] {' \
  feature::generation_contract_tests::cocoa_binds_its_four_probability_draws_to_north_east_south_west


# --- F1: the world window (dimension_type) vs the generator window (noise_settings) ---
#
# 1.21.1 declares both: the Nether and the End generate 0..127 and store 0..255. These
# entries pin the split at every place a "widen everything" edit would silently collapse it.
check "a dimension window that cuts the generator's terrain is refused" \
  src/worldgen.rs '    if min_y > gen_min_y || min_y + height < gen_min_y + gen_height {
        return None;
    }' \
  '    if false {
        return None;
    }' \
  worldgen::wg_tests::dimension_window_falls_back_to_the_generator_when_the_object_is_unusable

check "the loaded world height is the dimension_type file's, not the generator's" \
  src/worldgen.rs '                        g.world_min_y = min_y;
                        g.world_height = height;' \
  '                        g.world_min_y = min_y;
                        g.world_height = g.height;' \
  worldgen::wg_tests::dimension_type_declares_a_world_window_containing_the_generator

check "the terrain reader answers air above the generator window" \
  src/worldgen.rs '        if y < self.min_y || y >= self.min_y + self.height {
            return 0;
        }
        let cc = match self.cols(x >> 4, z >> 4) {' \
  '        if false {
            return 0;
        }
        let cc = match self.cols(x >> 4, z >> 4) {' \
  worldgen::wg_tests::nether_and_end_dumps_cover_the_world_window_with_air_above_the_generator

check "the FEATURES storage gate is the world window" \
  src/worldgen.rs '        if y < self.world_min_y || y >= self.world_min_y + self.world_height {
            return;
        }' \
  '        if y < self.min_y || y >= self.min_y + self.height {
            return;
        }' \
  feature::pipeline_tests::end_city_stores_cells_above_the_generator_top

check "isOutsideBuildHeight is the world window" \
  src/generation_context.rs '        y < self.g.world_min_y || y >= self.g.world_min_y + self.g.world_height' \
  '        y < self.g.min_y || y >= self.g.min_y + self.g.height' \
  feature::generation_contract_tests::out_of_h_follows_the_world_window_not_the_generator

check "a heightmap scan reaches the world top" \
  src/generation_context.rs '        let mut y = from.min(self.g.world_min_y + self.g.world_height - 1);' \
  '        let mut y = from.min(self.g.min_y + self.g.height - 1);' \
  feature::pipeline_tests::end_city_stores_cells_above_the_generator_top

check "a template piece writes above the generator top" \
  src/template_structures.rs '        if y < self.g.world_min_y || y >= self.g.world_min_y + self.g.world_height {
            return false;
        }' \
  '        if y < self.g.min_y || y >= self.g.min_y + self.g.height {
            return false;
        }' \
  feature::pipeline_tests::end_city_stores_cells_above_the_generator_top

check "height_range still samples the generator window" \
  src/feature.rs '        PM::HeightRange(hp) => out.push((pos.0, hp.get(rng, ctx.g.min_y, ctx.g.height), pos.2)),' \
  '        PM::HeightRange(hp) => out.push((pos.0, hp.get(rng, ctx.g.world_min_y, ctx.g.world_height), pos.2)),' \
  feature::window_tests::height_range_positions_sample_the_generator_window

check "nether_fossil anchors on the generator window" \
  src/template_structures.rs '    let min_y = world.gen_min_y();
    let depth = world.gen_height();' \
  '    let min_y = world.min_y();
    let depth = world.height();' \
  feature::pipeline_tests::nether_fossil_scans_the_noise_column_not_the_live_world
# Brigadier port (issue #51, slice 1): src/brigadier.rs is a standalone module, so these two
# mutations need no world state - a reader that keeps `escaped` set after a valid escape, and a
# redirect hop that resumes from the node it left instead of from the redirect target.
check "a quoted string clears the escape flag after consuming an escaped character" \
  src/brigadier.rs '                if c == terminator || c == SYNTAX_ESCAPE {
                    result.push(c);
                    escaped = false;
                } else {' \
  '                if c == terminator || c == SYNTAX_ESCAPE {
                    result.push(c);
                } else {' \
  brigadier_tests::reader_quoted_strings_unescape_only_the_terminator_and_the_escape

check "a redirect hop parses from the redirect target, not from the node it left" \
  src/brigadier.rs '                    let parse = self.parse_nodes(&target, &reader, &mut child_context);' \
  '                    let parse = self.parse_nodes(node, &reader, &mut child_context);' \
  brigadier_tests::a_redirect_modifier_threads_the_modified_source_through_every_hop
# Brigadier wire format (issue #51, slice 2): src/brigadier_wire.rs writes the tree the dispatcher
# already holds, so both mutations are byte-level and need no world state - a child index pointing
# one past the node it names, and the two numbers of a suggestion answer's replaced range
# transposed. Both produce a packet the client accepts and then resolves into a different tree or
# a wrong replacement range, which is why the assertions are byte equality and not `is_ok`.
check "a child index names the node it points at" \
  src/brigadier_wire.rs '        write_varint(out, wire_varint(child_index, "child index")?);' \
  '        write_varint(out, wire_varint(child_index + 1, "child index")?);' \
  brigadier_wire_tests::node_indices_are_assigned_breadth_first

check "a suggestion answer keeps start and length in their own fields" \
  src/brigadier_wire.rs '    write_varint(&mut out, wire_varint(response.start, "suggestion start")?);
    write_varint(&mut out, wire_varint(response.length, "suggestion length")?);' \
  '    write_varint(&mut out, wire_varint(response.length, "suggestion start")?);
    write_varint(&mut out, wire_varint(response.start, "suggestion length")?);' \
  brigadier_wire_tests::the_response_serializes_to_id_start_length_count_and_entries

# Tick-cost lane (M3): the two merge scans are bucketed, so the two things that decide
# *which* entity is absorbed are the neighbourhood and the tie-break. Both are pinned
# against a verbatim copy of the full pairwise scan, and a bucket that quietly loses a
# neighbour - or that keeps the wrong one of two equally old orbs - must go red there.
check "an xp orb merge searches the whole 26-cell neighbourhood, not just upward" \
  src/combat_and_health.rs '        for dx in -1..=1i32 {
            for dy in -1..=1i32 {
                for dz in -1..=1i32 {
                    let key = (
                        cx.checked_add(dx),' \
  '        for dx in 0..=1i32 {
            for dy in -1..=1i32 {
                for dz in -1..=1i32 {
                    let key = (
                        cx.checked_add(dx),' \
  merge_tests::xp_merge_absorbs_exactly_what_the_full_pairwise_scan_absorbs

check "two equally old xp orbs keep the lower index" \
  src/combat_and_health.rs '                let (keep, eat) = if aage <= bage { (i, j) } else { (j, i) };
                xl[keep].val = xl[keep].val.saturating_add(xl[eat].val);' \
  '                let (keep, eat) = if aage < bage { (i, j) } else { (j, i) };
                xl[keep].val = xl[keep].val.saturating_add(xl[eat].val);' \
  merge_tests::xp_merge_absorbs_exactly_what_the_full_pairwise_scan_absorbs

# hasPostProcess lane: `LevelChunk.postProcessGeneration` has two halves in this port and one
# test for each. The mark half is `WorldGenRegion.setBlock`'s gate - the written state's
# `hasPostProcess`, and nothing else - and the consumption half is the lit threshold
# `MushroomBlock.canSurvive` asks at promotion, which is the read the write-time check could not
# make. Either mutation puts the four Nether residual cells back into the chunk
# (`evidence/gameplay/feature-parity/haspostprocess-contract-20261118.md`).
check "only a hasPostProcess state is marked for post-processing" \
  src/generation_context.rs '        if self.g.pop_active.is_some() && reg.is_some_and(|r| r.has_post_process(st)) {' \
  '        if self.g.pop_active.is_some() {' \
  generation_contract_tests::a_written_post_process_state_files_a_mark_on_the_owning_chunk

check "the promotion shape pass keeps a mushroom whose raw brightness is under 13" \
  src/template_structures.rs '        return if lit < 13 && reg.is_solid_render(below) {' \
  '        return if lit <= 13 && reg.is_solid_render(below) {' \
  generation_contract_tests::promotion_removes_a_mushroom_the_lit_engine_reaches

# Population-order discipline (the default population dispatch). The default is a named member of
# vanilla's order family, so the two things that make it a discipline are the level propagation and
# which level is popped: propagating +2 per Chebyshev step instead of +1 stops the demand two rings
# short, and popping the highest non-empty level inverts the priority. Both leave a plausible
# sequence behind - the first batch is still the min corner's 3x3 window - which is why the golden
# ordered list, not a shape check, is the gate.
check "a request's queue level is its Chebyshev ring + 31, one level per step" \
  src/worldgen.rs '            .unwrap_or(0);
        POP_REQUEST_LEVEL + ring
    };' \
  '            .unwrap_or(0);
        POP_REQUEST_LEVEL + ring * 2
    };' \
  worldgen::wg_tests::default_dispatch_runs_the_named_population_order

check "the population queue pops the lowest non-empty level" \
  src/worldgen.rs '    while let Some(level) = buckets
        .iter()
        .find(|(_, bucket)| !bucket.is_empty())
        .map(|(level, _)| *level)' \
  '    while let Some(level) = buckets
        .iter()
        .rev()
        .find(|(_, bucket)| !bucket.is_empty())
        .map(|(level, _)| *level)' \
  worldgen::wg_tests::default_dispatch_runs_the_named_population_order

check "a compiled cache node still returns its input" \
  src/worldgen.rs '            DF::Cache(a) | DF::BlendD(a) => self.emit(*a),' \
  '            DF::Cache(_) => {
                self.ops.push(Op::Push(0.0));
                self.stack_push(1);
            }
            DF::BlendD(a) => self.emit(*a),' \
  worldgen::wg_tests::compiled_density_matches_interpreter_without_chunk_grids

check "a compiled interpolated node reads its own grid slot" \
  src/worldgen.rs '                let slot = id as u32;' \
  '                let slot = id as u32 + 1;' \
  worldgen::wg_tests::compiled_density_matches_interpreter_per_block

# The tree edge pass's vine half (`StructureTemplate.updateShapeAtEdge` -> `VineBlock.updateShape`).
# Both entries pin a rule the reference draft got backwards, so each one is also the defect the
# measurement had to survive: the draft short circuited on UP, and read the cell below instead of
# the one above.
check "the vine edge pass short circuits on DOWN, not UP" \
  src/tree.rs '    if toward_neighbor == 0 {' \
  '    if toward_neighbor == 1 {' \
  tree::tests::the_tree_edge_pass_applies_vine_update_shape

check "the vine edge pass reads the cell above, not the one below" \
  src/tree.rs '    let above = (cell.0, cell.1 + 1, cell.2);' \
  '    let above = (cell.0, cell.1 - 1, cell.2);' \
  tree::tests::the_tree_edge_pass_applies_vine_update_shape

# Terrain parallel lane (lever 2: one chunk's terrain per worker). Two things have to hold for
# the parallel arm to be worth anything, and neither is visible in a byte-identical dump: every
# chunk of a round must actually be claimed by some worker (an off-by-one in the claim leaves
# the tail to the serial path, which produces the same bytes, so only a "did every slot get
# filled" assertion can see it), and a worker must fill the chunk whose columns it was handed
# (a misaligned column slice still produces a full round of rows, just not that chunk's rows).
check "a terrain round claims every chunk of the group" \
  src/worldgen.rs '            let start = i * per;' \
  '            let start = i * per + 1;' \
  worldgen::wg_tests::terrain_prefetch_fills_more_than_one_chunk_at_a_time

check "a prefetched section is consumed by its own section key" \
  src/worldgen.rs '        let mut arr: Box<[u32; 4096]> = match self.terr_c.get(&(cx, sy, cz)) {' \
  '        let mut arr: Box<[u32; 4096]> = match self.terr_c.get(&(cx, sy + 16, cz)) {' \
  worldgen::wg_tests::a_prefetched_section_is_identical_to_the_serial_section_with_features

# `ClampedNormalInt` (`bpr`) is `(int) Mth.clamp(Mth.nextGaussian(random, mean, deviation),
# minInclusive, maxInclusive)`: `ayo.c` narrows the gaussian to f32 before the multiply, the clamp
# is f32, and the `f2i` cast truncates toward zero. `round()` moved every offset whose fraction
# reached .5 by one cell, which is what sent `pointed_dripstone`'s `random_offset` candidates
# through the biome filter at a different cell (47 cells at jungle (74,-41), seed 4242).
check "clamped_normal as an IntProvider truncates toward zero, it does not round" \
  src/generation_providers.rs '                };
                v as i32
            }' \
  '                };
                v.round() as i32
            }' \
  feature::generation_contract_tests::clamped_normal_int_provider_truncates_toward_zero_like_f2i

# The clamp is `v < min ? min : Math.min(v, max)`, so the two bounds are not interchangeable: the
# fixture's `min_inclusive`/`max_inclusive` are ints the JSON declares in that order, and reading
# them the other way round turns every draw into a constant.
check "clamped_normal's int clamp keeps min_inclusive and max_inclusive apart" \
  src/generation_providers.rs '                let v = if v < *lo as f32 {
                    *lo as f32
                } else {
                    v.min(*hi as f32)
                };' \
  '                let v = if v < *hi as f32 {
                    *hi as f32
                } else {
                    v.min(*lo as f32)
                };' \
  feature::generation_contract_tests::clamped_normal_int_provider_truncates_toward_zero_like_f2i

# PalettedSection (M3 Slice P2a): non-straddling u64 packing/unpacking, Single fast path, and Direct 15-bit masking.
check "paletted_read_shift_matches_pack_layout" \
  src/paletted.rs '            SectionPalette::Direct { words } => {
                megu_core::bits::get(words, 15, idx).unwrap_or(0)
            }' \
  '            SectionPalette::Direct { words } => {
                megu_core::bits::get(words, 14, idx).unwrap_or(0)
            }' \
  paletted_tests::paletted_exact_bit_transitions_and_wire_identity

check "paletted_single_returns_stored_state" \
  src/paletted.rs '            SectionPalette::Single(st) => *st,' \
  '            SectionPalette::Single(_) => 0,' \
  paletted_tests::paletted_single_variants_and_non_air_air_ids

check "paletted_direct_keeps_all_15_bits" \
  src/paletted.rs '            values[j] = (st & 0x7fff) as u16;' \
  '            values[j] = (st & 0x3fff) as u16;' \
  paletted_tests::paletted_exact_bit_transitions_and_wire_identity

# Entity collision + step-up (M2 Gameplay Slice 4a): 1.21.1 Entity.collide / collideWithShapes / BlockCollisions.
check "entity_collide_axis_tie_prefers_x" \
  src/entity_collision.rs '    let z_first = x.abs() < z.abs();' \
  '    let z_first = x.abs() <= z.abs();' \
  entity_collision_tests::entity_collide_axis_priority_z_first_when_x_lt_z_and_x_first_on_tie

check "entity_collide_step_up_respects_max_up_step" \
  src/entity_collision.rs '            if f > max_up_step {
                break;
            }' \
  '            if f > max_up_step + 0.5 {
                break;
            }' \
  entity_collision_tests::entity_collide_step_up_over_slab_and_refuses_full_block_when_max_up_step_0_6

check "entity_collide_block_window_catches_1_5_fence" \
  src/entity_collision.rs '    let by0 = (min[1] - 1e-7).floor() as i32 - 1;' \
  '    let by0 = (min[1] - 1e-7).floor() as i32;' \
  entity_collision_tests::entity_collide_fence_1_5_block_prevents_step_over_and_catches_window

# Fluid height, eye immersion, fluid push & lava contact (M2 Gameplay Slice 4b): 1.21.1 FlowingFluid & Entity fluid contracts.
check "entity_fluid_has_same_above_returns_full_height" \
  src/entity_fluid.rs '    let h = if fluid_has_same_above_with(fs.kind, x, y, z, get_block) {
        1.0f32
    } else {' \
  '    let h = if false && fluid_has_same_above_with(fs.kind, x, y, z, get_block) {
        1.0f32
    } else {' \
  entity_fluid_tests::entity_fluid_height_and_has_same_above_water_lava_waterlogged_and_kelp

check "entity_fluid_is_in_lava_uses_0_001_deflate" \
  src/entity_fluid.rs '    let deflated = aabb.deflate(0.001);' \
  '    let deflated = aabb.deflate_xyz(0.1, 0.4, 0.1);' \
  entity_fluid_tests::entity_fluid_is_in_lava_deflate_0_001_boundary_and_flowing_lava

check "entity_fluid_eye_immersion_respects_pose_and_surface" \
  src/entity_fluid.rs '    fk == kind && f64::from(by as f32 + h) > eye_y' \
  '    fk == kind' \
  entity_fluid_tests::entity_fluid_eye_immersion_player_poses_and_mobs

# Chest world-side contract (M2 Gameplay Slice 3c):
# `ChestBlock.isChestBlockedAt` on both halves, multi-viewer `broadcastChanges`, and `recheckOpeners`.
check "chest_contract_double_chest_blocks_when_either_half_blocked" \
  src/menus_and_items.rs '    for pos in chest_window_halves(window) {
        if is_chest_blocked_at(pos) {
            return true;
        }
    }
    false' \
  '    for pos in chest_window_halves(window) {
        if !is_chest_blocked_at(pos) {
            return false;
        }
    }
    true' \
  tick_tests::chest_contract_blocked_single_double_cat_and_unblocked_barrel

check "chest_contract_broadcast_changes_diffs_both_halves" \
  src/menus_and_items.rs '                if sync.last_slots[idx] != *stack {' \
  '                if idx < 27 && sync.last_slots[idx] != *stack {' \
  tick_tests::chest_contract_multi_viewer_broadcast_changes

check "chest_contract_recheck_openers_enforces_reach" \
  src/menus_and_items.rs '        if !chest_still_valid(c, win, anchor) {' \
  '        if !window_live(win) {' \
  tick_tests::chest_contract_recheck_openers_reach_and_lid_close

# Entity push, cramming & spatial grid (M2 Gameplay Slice 4c + M3 P5): 1.21.1 Entity.push & LivingEntity.pushEntities.
check "entity_push_clamps_inverse_distance_at_one" \
  src/entity_push.rs '        let scale = (1.0 / d).min(1.0);' \
  '        let scale = 1.0 / d;' \
  entity_push_tests::entity_push_exact_constants_sign_clamp_and_guards

check "entity_push_cramming_checks_non_passengers_above_max_minus_one" \
  src/entity_push.rs '        if non_passengers > (max_cramming - 1) as usize {' \
  '        if non_passengers >= (max_cramming - 1) as usize {' \
  entity_push_tests::entity_push_cramming_threshold_passengers_and_gamerule_zero

check "entity_push_no_physics_spectator_skips_impulse" \
  src/entity_push.rs '    if same_vehicle || self_ent.no_physics || other_ent.no_physics {' \
  '    if same_vehicle {' \
  entity_push_tests::entity_push_exact_constants_sign_clamp_and_guards

# The cramming hit is a `LivingEntity.hurt` call, so its invulnerability window gates the rate.
check "entity_hurt_window_rejects_equal_hits_inside_ten_ticks" \
  src/entity_push.rs 'const ENTITY_HURT_INVULNERABLE_TICKS: u64 = 10;' \
  'const ENTITY_HURT_INVULNERABLE_TICKS: u64 = 0;' \
  entity_push_tests::entity_hurt_window_rejects_equal_hits_for_ten_ticks

# ...and the call site has to ask the window, not just the damage amount, or a pile loses two hits
# inside two ticks (the integration assertion measures exactly that).
check "entity_push_cramming_asks_the_hurt_window_before_damaging" \
  src/entity_push.rs '        if accepted {' \
  '        if dmg > 0.0 {' \
  entity_push_tests::entity_push_integration_mob_tick_walls_spectator_cramming_and_breeding

# `bsw.a(Lbsr;Leyi;Leyi$a;Lbsr;)Z` denies a *different-team* candidate when either side is
# PUSH_OTHER_TEAMS; the constant names read inverted, so the bytecode has to be pinned.
check "entity_selector_push_other_teams_denies_different_team" \
  src/entity_push.rs '        || candidate_rule == TeamCollisionRule::PushOtherTeams)
        && !same_team' \
  '        || candidate_rule == TeamCollisionRule::PushOtherTeams)
        && same_team' \
  entity_push_tests::entity_selector_team_rules_match_the_bytecode_deny_shape

# P2-1-T0: StructurePiece.placeBlock SHAPE_CHECK_BLOCKS marks on template & jigsaw placement.
check "p2_1_t0_jigsaw_place_marks_shape_check_blocks" \
  src/jigsaw_structures.rs '        if reg.needs_postprocess_shape(placed) {
            target.mark_shape(p);
        }' \
  '        if false {
            target.mark_shape(p);
        }' \
  feature::jigsaw_structures::tests::jigsaw_piece_marks_shape_check_block_and_promotion_disconnects_it

check "p2_1_t0_template_place_marks_shape_check_blocks" \
  src/template_structures.rs '        if reg.needs_postprocess_shape(state) {
            world.mark_shape(block.pos);
        }' \
  '        if false {
            world.mark_shape(block.pos);
        }' \
  structure_template::template_structures_tests::place_in_world_marks_transformed_shape_check_block_on_fc

# Promotion-only BushBlock / DoublePlantBlock removal (`village-taiga` fern at (82,69,-500)).
check "bush_block_postprocess_removes_unsupported_plants" \
  src/template_structures.rs '        let updated = postprocess_bush_shape(reg, state, world, pos)
            .unwrap_or_else(|| update_from_neighbour_shapes(reg, state, world, pos));' \
  '        let updated = postprocess_bush_shape(reg, state, world, pos)
            .filter(|_| false)
            .unwrap_or_else(|| update_from_neighbour_shapes(reg, state, world, pos));' \
  structure_template::template_structures_tests::postprocess_shape_marks_removes_unsupported_bush_blocks_only_at_promotion

# Feature.markAboveForPostProcessing (`ece.a`): DiskFeature & LakeFeature's two sites.
check "disk_feature_marks_above_for_post_processing" \
  src/feature.rs '                            ctx.set(x, y, z, st);
                            mark_above_for_post_processing(ctx, reg, x, y, z);' \
  '                            ctx.set(x, y, z, st);' \
  feature::mark_above_tests::disk_feature_marks_above_for_post_processing_stopping_on_air

check "lake_feature_cave_air_marks_above_for_post_processing" \
  src/feature.rs '                                if y >= 4 {
                                    mark_above_for_post_processing(ctx, reg, bp.0, bp.1, bp.2);
                                }' \
  '                                if false && y >= 4 {
                                    mark_above_for_post_processing(ctx, reg, bp.0, bp.1, bp.2);
                                }' \
  feature::mark_above_tests::lake_feature_marks_above_at_cave_air_and_barrier_sites

check "lake_feature_barrier_marks_above_for_post_processing" \
  src/feature.rs '                                    ctx.set(bp.0, bp.1, bp.2, barrier_st);
                                    mark_above_for_post_processing(ctx, reg, bp.0, bp.1, bp.2);' \
  '                                    ctx.set(bp.0, bp.1, bp.2, barrier_st);' \
  feature::mark_above_tests::lake_feature_marks_above_at_cave_air_and_barrier_sites

# Chest world-side contract remainder (M2 Gameplay Slice 3d):
# lazy block loot materialization, BarrelBlock open property, waterlogged dry partner Solid,
# and EnderChestBlockEntity anchored 0x08 opener counting.
check "chest_contract_3d_block_loot_lazy_defers_materialization" \
  src/main.rs '                pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()).insert(
                    ow_block(pos),
                    PendingBlockLoot {
                        kind,
                        loot_table,
                        loot_seed,
                    },
                );' \
  '                let rolled = roll_loot_container_slots(kind, &loot_table, loot_seed);
                chests().lock().unwrap_or_else(|e| e.into_inner()).insert(ContainerKey::Block(ow_block(pos)), rolled);' \
  tick_tests::chest_contract_3d_block_loot_stays_lazy_until_first_open

check "chest_contract_3d_block_loot_spectator_gate_unpacks_for_viewers" \
  src/menus_and_items.rs '    if !me.is_spectator() {
        unpack_window_loot_if_needed(window);
        let locked = [window.0, window.1.unwrap_or(window.0)].iter().any(|key| {' \
  '    if me.is_spectator() {
        unpack_window_loot_if_needed(window);
        let locked = [window.0, window.1.unwrap_or(window.0)].iter().any(|key| {' \
  tick_tests::chest_contract_3d_block_loot_stays_lazy_until_first_open

check "chest_contract_3d_barrel_toggles_open_without_lid_event" \
  src/menus_and_items.rs '            if let Some(reg) = breg().filter(|reg| reg.name_of(st) == "minecraft:barrel") {' \
  '            if let Some(reg) = breg().filter(|_| false) {' \
  tick_tests::chest_contract_3d_barrel_toggles_open_property_without_block_event

check "chest_contract_3d_dry_chest_is_solid_for_fluid" \
  src/main.rs 'else if fluid_washes_away(reg, st) {' \
  'else if true {' \
  tick_tests::chest_contract_3d_waterlogged_double_chest_schedules_and_spreads_without_dry_partner_error

check "chest_contract_3d_ender_chest_counts_anchor_openers" \
  src/menus_and_items.rs '            if matches!(a, ContainerKey::Block(ek) if is_ender_key((ek.1, ek.2, ek.3))) && b.is_none() {' \
  '            if false && matches!(a, ContainerKey::Block(ek) if is_ender_key((ek.1, ek.2, ek.3))) && b.is_none() {' \
  tick_tests::chest_contract_3d_ender_chest_emits_block_event_and_invalidates_on_break

# P2-3-S1: the per-dimension registry must select the dimension it is asked for. Routing
# every `with_gen_dim` back to the Overworld leaves the Overworld green and makes the Nether and
# the End answer with the Overworld's window (-64/384, sky light) instead of their own
# (0/256, no sky light), so the dimension test has to notice.
check "p2_3_s1_with_gen_dim_selects_the_dimension_it_was_asked_for" \
  src/worldgen.rs '    gen_lock_dim(dimension).as_mut().map(f)' \
  '    gen_lock_dim(DimId::Overworld).as_mut().map(f)' \
  worldgen::wg_tests::p2_3_s1_three_dimensions_answer_independently

# ...and `init_dim` must install the dimension it was asked for, not the active one: written into
# the Overworld's slot, the Nether's generator is unreachable by name while the Overworld holds
# Nether terrain, which is the same silent substitution one layer down.
check "p2_3_s1_init_dim_installs_the_dimension_it_names" \
  src/worldgen.rs '    install(dimension, seed, res, false)' \
  '    install(DimId::Overworld, seed, res, false)' \
  worldgen::wg_tests::p2_3_s1_three_dimensions_answer_independently

# M2 Gameplay Slice 4d: `LivingEntity.travel` fluid branches (`src/entity_fluid.rs`).
# `btn.a(Lexc;)V` offsets 90–113/255–269 give the water slow-down and the widened `0.8f` vertical
# drag; `bsr.bm()` offsets 9–35 pick `0.007` for the ultra-warm dimension; `btn.a(DZLexc;)Lexc;`
# offsets 0–100 hold the `-0.003` terminal frame; `bsr.di()` offsets 0–19 is the
# `eyeHeight < 0.4F` jump threshold.
check "travel_contract_4d_water_vertical_drag_is_0_8f" \
  src/entity_fluid.rs 'const WATER_VERTICAL_DRAG: f64 = 0.800000011920929;' \
  'const WATER_VERTICAL_DRAG: f64 = 0.9;' \
  tick_tests::travel_contract_4d_water_travel_drag_sprint_dolphins_and_impulse

check "travel_contract_4d_water_sprint_slow_down_is_0_9f" \
  src/entity_fluid.rs '            let mut f = if ctx.sprinting {
                WATER_SPRINT_SLOW_DOWN
            } else {
                WATER_SLOW_DOWN
            };' \
  '            let mut f = {
                WATER_SLOW_DOWN
            };' \
  tick_tests::travel_contract_4d_water_travel_drag_sprint_dolphins_and_impulse

check "travel_contract_4d_lava_push_scale_ultra_warm_is_0_007" \
  src/entity_fluid.rs '            if ultra_warm {
                LAVA_ULTRA_WARM_MOTION_SCALE
            } else {
                LAVA_TEMPERATE_MOTION_SCALE
            }' \
  '            if ultra_warm {
                LAVA_TEMPERATE_MOTION_SCALE
            } else {
                LAVA_ULTRA_WARM_MOTION_SCALE
            }' \
  tick_tests::travel_contract_4d_lava_push_scale_dimension

check "travel_contract_4d_fluid_falling_terminal_frame_is_negative" \
  src/entity_fluid.rs '        -FLUID_FALLING_TERMINAL
    } else {' \
  '        FLUID_FALLING_TERMINAL
    } else {' \
  tick_tests::travel_contract_4d_fluid_falling_adjusted_movement_gravity_and_clamp

check "travel_contract_4d_jump_threshold_reads_eye_height" \
  src/entity_fluid.rs '    if eye_height < 0.4 {
        0.0
    } else {
        0.4
    }' \
  '    if eye_height < 0.4 {
        0.4
    } else {
        0.4
    }' \
  tick_tests::travel_contract_4d_water_jump_threshold_and_input_vector_rotation

check "travel_contract_4d_mob_tick_applies_the_vertical_travel_delta" \
  src/entity_fluid.rs '    mp.0 += out.moved[0];
    mp.1 += out.moved[1];
    mp.2 += out.moved[2];' \
  '    mp.0 += out.moved[0];
    mp.2 += out.moved[2];' \
  tick_tests::travel_contract_4d_mob_tick_water_and_lava_sink_without_fall_damage

# `mob_snap_y` must defer a mob in lava to the travel branch: `surface_y` would otherwise lift it
# to the first non-air column above the lava pocket.
check "travel_contract_4d_mob_snap_y_defers_lava_to_travel" \
  src/main.rs '        || mob_in_water(m.etype, mp.0, mp.1, mp.2)
        || mob_in_lava(m.etype, mp.0, mp.1, mp.2)' \
  '        || mob_in_water(m.etype, mp.0, mp.1, mp.2)
        || (false && mob_in_lava(m.etype, mp.0, mp.1, mp.2))' \
  tick_tests::travel_contract_4d_mob_tick_water_and_lava_sink_without_fall_damage

# P2-3-S2a: the block overlay's key carries a dimension. With the key cut back to the chunk, one
# `(cx, cz)` is one entry for all three dimensions, so a Nether edit and an Overworld edit at the
# same coordinate are the same cell: the last write wins and both reads answer with it.
check "p2_3_s2a_block_at_dim_reads_the_dimension_it_names" \
  src/main.rs '    let hit = w
        .get(&(dim, x >> 4, z >> 4))' \
  '    let hit = w
        .get(&(DimId::Overworld, x >> 4, z >> 4))' \
  persistence_tests::p2_3_s2a_one_cell_in_three_dimensions_stays_independent

# ...and a write in one dimension must land in that dimension. Storing it under the Overworld's key
# overwrites the Overworld's edit at the same chunk and leaves the dimension that was written empty.
check "p2_3_s2a_set_block_dim_stores_the_dimension_it_names" \
  src/main.rs '    let key: DimChunkKey = (dim, x >> 4, z >> 4);' \
  '    let key: DimChunkKey = (DimId::Overworld, x >> 4, z >> 4);' \
  persistence_tests::p2_3_s2a_one_cell_in_three_dimensions_stays_independent

# A read that misses the overlay asks the generator of the dimension it was asked about. The
# Overworld's `base_state` resolves its own noise settings and its -64/384 window, so a Nether cell
# below y=0 comes back as Overworld deepslate instead of the air the Nether's window defines — the
# substitution that looks like a working overlay until a Nether chunk is served.
check "p2_3_s2a_a_miss_reads_the_dimensions_own_generator" \
  src/main.rs '    worldgen::with_gen_dim(dim, |g| g.block_state_at(x, y, z, crate::feature::populate))' \
  '    worldgen::with_gen_dim(DimId::Overworld, |g| g.block_state_at(x, y, z, crate::feature::populate))' \
  persistence_tests::p2_3_s2a_one_cell_in_three_dimensions_stays_independent

# A companion file's table must load into its own dimension. Read into the Overworld's key, a Nether
# save restores nothing in the Nether and overwrites the Overworld chunk of the same coordinate.
check "p2_3_s2a_a_companion_table_loads_into_its_own_dimension" \
  src/save_v2.rs '        chunks.insert((dim, cx, cz), edits);' \
  '        chunks.insert((DimId::Overworld, cx, cz), edits);' \
  persistence_tests::p2_3_s2a_legacy_overworld_save_and_dim_overlay_tables_round_trip

# P2-3-S2c: the legacy sign section must hold the Overworld's signs only. Dropping the filter puts
# Nether and End signs in the legacy bytes, so a non-Overworld edit shifts the Overworld save.
check "p2_3_s2c_legacy_sign_section_is_overworld_only" \
  src/save_v2.rs 'let ow_signs: Vec<_> = sg.iter().filter(|(k, _)| k.0 == DimId::Overworld).collect();' \
  'let ow_signs: Vec<_> = sg.iter().collect();' \
  persistence_tests::p2_3_s2c_signs_stay_isolated_per_dimension_and_round_trip

# The footer must fail closed on a dimension tag it does not know. Mapping unknown bytes to the
# Overworld would silently relocate a sign or a player from a dimension this build cannot name.
check "p2_3_s2_dim_footer_rejects_unknown_dimension" \
  src/save_v2.rs 'DimId::from_index(b as usize).ok_or("invalid dimension tag")' \
  'Ok(DimId::from_index(b as usize).unwrap_or(DimId::Overworld))' \
  persistence_tests::p2_3_s2_dim_footer_rejects_unknown_dimension_and_truncation

# P2-3-S2d: a player's saved dimension comes from the footer. Writing Overworld there makes a
# Nether or End login point at the Overworld after reload.
check "p2_3_s2d_footer_carries_the_players_real_dimension" \
  src/save_v2.rs 'dim_players.push((name.clone(), dim));' \
  'dim_players.push((name.clone(), DimId::Overworld));' \
  persistence_tests::p2_3_s2d_pos_store_preserves_dimension_and_defaults_legacy_to_overworld

# P2-3-S2b: the legacy chest table must carry Overworld entries only. Accepting every block key
# puts Nether and End chests in the old wire layout, shifting the Overworld-only bytes and leaving
# DIM2 without the chests that should have gone there.
check "p2_3_s2b_legacy_chest_section_is_overworld_only" \
  src/save_v2.rs 'ContainerKey::Block((DimId::Overworld, x, y, z)) => Some(((*x, *y, *z), slots)),' \
  'ContainerKey::Block((_, x, y, z)) => Some(((*x, *y, *z), slots)),' \
  persistence_tests::p2_3_s2b_overworld_only_save_emits_no_dim2_footer_and_legacy_loads_as_overworld

# A footer entry whose dimension byte names no known dimension must fail closed. Mapping it to a
# real dimension instead relocates the entry and hides the corrupt tag.
check "p2_3_s2b_dim2_footer_rejects_unknown_dimension" \
  src/save_v2.rs 'let dim = DimId::from_index(b as usize).ok_or("invalid DIM2 dimension tag")?;' \
  'let dim = DimId::from_index(b as usize).unwrap_or(DimId::End);' \
  persistence_tests::p2_3_s2b_dim2_footer_rejects_unknown_dimension_and_truncation

# DIM2 furnaces must restore into the dimension they were saved from. Filing them under Overworld
# both loses the Nether/End state and overwrites the Overworld furnace at the same coordinate.
check "p2_3_s2b_dim2_furnace_loads_into_its_own_dimension" \
  src/save_v2.rs '            for (pos, fr) in dim2.furnaces {
                fs.insert(pos, fr);
            }' \
  '            for (pos, fr) in dim2.furnaces {
                fs.insert((DimId::Overworld, pos.1, pos.2, pos.3), fr);
            }' \
  persistence_tests::p2_3_s2b_chests_furnaces_and_crops_stay_isolated_per_dimension_and_round_trip

# P2-3-S3: the chunk and light a dimension serves must use that dimension's window. Each
# mutant below keeps the Overworld path identical and breaks exactly one Nether/End promise,
# so the packet-level test has to notice it rather than trusting the helper that was edited.
check "p2_3_s3_nether_and_end_have_zero_sky_mask" \
  src/main.rs '    let has_sky = worldgen::has_skylight(dim);' \
  '    let has_sky = true;' \
  tick_tests::p2_3_s3_nether_and_end_chunk_packets_emit_16_sections_and_zero_sky_mask

# Sixteen sections in the Nether/End, not the Overworld's twenty-four. A hard-coded 24 also
# changes the light and biome loops' reach, which the packet's section count catches.
check "p2_3_s3_nether_and_end_emit_16_block_sections" \
  src/main.rs '    let sections = dim.sections();' \
  '    let sections = 24;' \
  tick_tests::p2_3_s3_nether_and_end_chunk_packets_emit_16_sections_and_zero_sky_mask

# MOTION_BLOCKING is relative to the chunk's own min_y. Subtracting the Overworld's -64
# makes every Nether/End height 64 too large while the Overworld bytes stay the same.
check "p2_3_s3_heightmap_uses_the_dimension_min_y" \
  src/main.rs '        let val = (*t - min_y).max(0) as u64;' \
  '        let val = (*t - MIN_Y).max(0) as u64;' \
  tick_tests::p2_3_s3_nether_and_end_chunk_packets_emit_16_sections_and_zero_sky_mask

# Block light is anchored one section below min_y: -16 in the Nether/End, not the
# Overworld's -80. Using the Overworld anchor shifts a Nether torch from section 3 to 7.
check "p2_3_s3_block_light_anchors_at_dimension_light_min_y" \
  src/main.rs '    let light_sections = dim.light_sections();
    let light_min_y = dim.light_min_y();' \
  '    let light_sections = dim.light_sections();
    let light_min_y = LIGHT_MIN_Y;' \
  tick_tests::p2_3_s3_nether_and_end_block_light_anchors_at_light_min_y_minus_16

# A band read that answers from the Overworld's key makes a Nether write invisible to its own
# band (and lets an Overworld cell answer for it) at the same (cx, cz).
check "p2_3_s3_band_cache_is_keyed_by_dimension" \
  src/main.rs 'fn band_for(dim: DimId, cx: i32, cz: i32) -> Option<std::sync::Arc<OpBand>> {
    op_bands().get((dim, cx, cz))
}' \
  'fn band_for(dim: DimId, cx: i32, cz: i32) -> Option<std::sync::Arc<OpBand>> {
    op_bands().get((DimId::Overworld, cx, cz))
}' \
  tick_tests::p2_3_s3_chunk_caches_isolate_dimensions_on_same_coords

# P2-3-S4: the dimension a client is in has to reach the wire id, the 0x47 payload, the
# chunk window, the block-delta audience and the persisted position. Each mutant below
# breaks one of those links and leaves the rest of the Overworld path identical.
check "p2_3_s4_wire_id_is_the_registry_id" \
  src/play_protocol.rs '        return dim_type_wire_ids()[dim.index()];' \
  '        return dim.index() as i32;' \
  tick_tests::p2_3_s4_dimension_type_wire_ids_come_from_the_pinned_registry

check "p2_3_s4_join_game_declares_every_dimension" \
  src/play_protocol.rs '    write_varint(&mut p, dimensions.len() as i64);
    for d in dimensions {' \
  '    write_varint(&mut p, 1);
    for d in dimensions {' \
  tick_tests::p2_3_s4_join_game_declares_all_three_dimensions_and_names_spawn_dim

check "p2_3_s4_spawn_info_uses_wire_id" \
  src/play_protocol.rs '    write_varint(&mut p, dim_type_wire_id(dim) as i64);' \
  '    write_varint(&mut p, dim.index() as i64);' \
  tick_tests::p2_3_s4_join_game_declares_all_three_dimensions_and_names_spawn_dim

check "p2_3_s4_switch_clears_the_old_window" \
  src/play_protocol.rs '        me.chunks.lock().unwrap_or_else(|e| e.into_inner()).clear();' \
  '        me.chunks.lock().unwrap_or_else(|e| e.into_inner()).insert((9999, 9999));' \
  tick_tests::p2_3_s4_switch_client_dimension_emits_respawn_clears_chunks_and_syncs_position

check "p2_3_s4_respawn_names_the_destination" \
  src/play_protocol.rs '    let mut rp = spawn_info_dim(new_dim, gm);' \
  '    let mut rp = spawn_info_dim(DimId::Overworld, gm);' \
  tick_tests::p2_3_s4_switch_client_dimension_emits_respawn_clears_chunks_and_syncs_position

check "p2_3_s4_portal_horizontal_scaling" \
  src/play_protocol.rs '        (DimId::Nether, bx as f64 / 8.0, bz as f64 / 8.0)' \
  '        (DimId::Nether, bx as f64, bz as f64)' \
  tick_tests::p2_3_s4_portal_scaling_clamps_y_and_the_arrival_guard_blocks_bounce

check "p2_3_s4_logout_saves_the_live_dimension" \
  src/save_v2.rs '    let dim = c.dim.lock().unwrap_or_else(|e| e.into_inner());
    let p = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
    (*dim, p.0, p.1, p.2, p.3, p.4)' \
  '    let _dim = c.dim.lock().unwrap_or_else(|e| e.into_inner());
    let p = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
    (DimId::Overworld, p.0, p.1, p.2, p.3, p.4)' \
  tick_tests::p2_3_s4_logout_persists_the_clients_live_dimension

check "p2_3_s4_chunk_deltas_respect_the_dimension_filter" \
  src/play_protocol.rs '        let Ok(mut socket) = client.sock.lock() else {
            continue;
        };
        if *client.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim {
            continue;
        }
        let (id, packet) = mk(client);' \
  '        let Ok(mut socket) = client.sock.lock() else {
            continue;
        };
        let (id, packet) = mk(client);' \
  tick_tests::p2_3_s4_broadcast_chunk_dim_filters_other_dimensions

# The same filter, observed end-to-end with two live clients on the wire. This gate is not
# selected by MUTATION_FILTER=p2_3_s4 (its filter names the script), but the full campaign runs it.
check "p2_3_s4_block_deltas_are_dimension_scoped" \
  src/play_protocol.rs '        let Ok(mut socket) = client.sock.lock() else {
            continue;
        };
        if *client.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim {
            continue;
        }
        let (id, packet) = mk(client);' \
  '        let Ok(mut socket) = client.sock.lock() else {
            continue;
        };
        let (id, packet) = mk(client);' \
  tests/dimension_wire.py 'dimension wire: FAIL nether-edit-leaked-to-overworld'

echo "mutation check: $passed proved, $failed not a gate, $parked parked, $skipped skipped"
if [ "$parked" -gt 0 ]; then
  echo "  $parked gate(s) are parked and prove nothing until they run again."
fi
# Every registered mutation is required; an unexecuted assertion is not success.
[ "$failed" -eq 0 ] && [ "$parked" -eq 0 ] && [ "$skipped" -eq 0 ] && [ "$passed" -gt 0 ]
