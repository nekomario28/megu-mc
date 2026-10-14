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
set -uo pipefail
cd "$(dirname "$0")/.."

CARGO_FLAGS=(--locked -j 2)
if [ "${1:-}" = "--offline" ] || [ "${CARGO_OFFLINE:-0}" = "1" ]; then
  CARGO_FLAGS=(--offline "${CARGO_FLAGS[@]}")
fi

# A mutation that is left applied is worse than no check at all: the next build compiles a source
# nobody chose, and a red suite then points at tests that are fine. This happened while writing this
# script - two runs reported three falling tests failing, against a tree that still had a mutation
# in it, and the failure did not reproduce once the source was rebuilt. Refuse to start over a
# leftover, and restore on every exit path we control.
if ls src/*.mutation-backup >/dev/null 2>&1; then
  echo "refusing to run: a previous mutation was left applied:" >&2
  ls src/*.mutation-backup >&2
  echo "restore with: for f in src/*.mutation-backup; do mv \"\$f\" \"\${f%.mutation-backup}\"; done" >&2
  exit 2
fi

passed=0
failed=0
skipped=0
parked=0
log=$(mktemp)

# Content hash of the file as it was before the mutation. Restoring the bytes is the point;
# restoring a timestamp is not, and neither is trusting that the restore worked. A mutation that
# survives in the tree is worse than a failing check, so this is verified rather than assumed.
hash_of() { sha256sum "$1" | cut -d' ' -f1; }

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
trap 'restore; rm -f "$log"' EXIT

# check <name> <file> <from> <to> <test-filter>
check() {
  local name="$1" mut_file_local="$2" from="$3" to="$4" test_filter="$5"
  mut_file="$mut_file_local"
  backup="$mut_file.mutation-backup"
  local sha_before
  sha_before=$(hash_of "$mut_file")
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

  local status
  if cargo test ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} "$test_filter" -- --test-threads=1 >"$log" 2>&1; then
    status=green
  else
    status=failed
  fi

  # cargo prints "error: test failed" for a failing test, so the exit code and the word error cannot
  # tell a compile failure from an assertion failure. The lines the harness itself prints can.
  if grep -qE '^error\[' "$log"; then
    echo "FAIL  $name"
    echo "      the mutation did not compile, so no assertion ran. Narrow it."
    failed=$((failed + 1))
  elif grep -qE 'running 0 tests' "$log"; then
    echo "FAIL  $name"
    echo "      '$test_filter' matched no test, so nothing was checked."
    failed=$((failed + 1))
  elif [ "$status" = green ] && grep -qE 'test result: ok\. 0 passed; 0 failed; [1-9][0-9]* ignored' "$log"; then
    # The test ran nothing because it is parked with #[ignore]. That is neither a gate nor a silent
    # pass: it is a named gap, and it has to be loud in the summary or parking becomes a way to make
    # this job green. Check the count of parked gates, not just the fact that there are some.
    echo "PARK  $name"
    echo "      '$test_filter' is #[ignore]d, so the mutation had nothing to hit. See specs/PLAN.md."
    parked=$((parked + 1))
  elif [ "$status" = green ]; then
    echo "FAIL  $name"
    echo "      '$test_filter' stayed green with the implementation broken. That test is not a gate."
    failed=$((failed + 1))
  elif grep -qE 'test result: FAILED' "$log"; then
    echo "ok    $name"
    passed=$((passed + 1))
  else
    echo "FAIL  $name"
    echo "      the run neither compiled, matched, nor reported a failing test. Inspect $log."
    failed=$((failed + 1))
  fi
  restore
  verify_restored "$name" "$sha_before"
}

# A restore that did not put the bytes back leaves the tree mutated, and every later measurement is
# of a source nobody chose. Checking the hash is the only way to know, because the failure mode is
# silent: the file compiles, the suite runs, and the answer is about the mutation.
verify_restored() {
  local name="$1" want="$2" got
  got=$(hash_of "$mut_file")
  if [ "$want" != "$got" ]; then
    echo "BROKEN $name"
    echo "      $mut_file did not come back byte-identical after the mutation."
    echo "      Restoring from git and stopping: a mutated tree must not be measured."
    git checkout -- "$mut_file" 2>/dev/null || true
    exit 3
  fi
}

FALLING_SELF='    if y > MIN_Y
        && reg.behaviour_of(block_at(x, y, z)) == crate::feature::B_FALLING
        && fallable(x, y - 1, z)
    {
        schedule(x, y, z);
    }
'
check "a placed gravity block is scheduled by its own cell" \
  src/main.rs "$FALLING_SELF" "" \
  every_falling_block_is_scheduled_and_moves_one_cell

check "the falling set comes from the asset, not a hand list" \
  src/main.rs 'if reg.behaviour_of(st) != crate::feature::B_FALLING {
            continue; // something else is here now' \
  'if !matches!(st, 112 | 117 | 118) {
            continue; // something else is here now' \
  every_falling_block_is_scheduled_and_moves_one_cell

check "a landed block is still scheduled when the support is removed" \
  src/main.rs '    if fallable(x, y, z) && reg.behaviour_of(block_at(x, y + 1, z)) == crate::feature::B_FALLING {' \
  '    if false {' \
  sand_falls_one_block_a_tick_until_it_lands

check "the merge keeps one entry per position" \
  src/main.rs '    for p in again {
        if !f.contains(&p) {
            f.push(p);
        }
    }' \
  '    f.extend(again);' \
  two_stacked_sand_blocks_fall_without_deadlocking

check "a falling block does not move while it is only scheduled" \
  src/main.rs '        set_block(x, y - 1, z, st);
        note_block_update(x, y, z, 0);' \
  '        set_block(x, y, z, 0);
        note_block_update(x, y, z, 0);' \
  sand_falls_one_block_a_tick_until_it_lands

echo
echo "mutation check: $passed proved, $failed not a gate, $parked parked, $skipped skipped"
if [ "$parked" -gt 0 ]; then
  echo "  $parked gate(s) are parked and prove nothing until they run again."
fi
[ "$failed" -eq 0 ]
