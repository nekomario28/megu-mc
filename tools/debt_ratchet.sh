#!/usr/bin/env bash
# Debt that is frozen at today's count.
#
# Every entry names something the plan says should go away, pins how much of it exists now, and fails
# if the count grows. The point is not to clean it up today: it is that a number in the plan is a
# claim, and a claim with nothing checking it drifts. Reducing a count is a deliberate edit to this
# file, which is the moment to say in the commit message that the old measurement of anything
# touching it is void.
#
# Keep large, broad patterns out unless they represent a concrete regression risk. The
# lock/rwlock unwrap ceiling is intentionally narrow: it catches additional poison
# propagation paths while existing usages can be addressed incrementally.
#
# Run: bash tools/debt_ratchet.sh
set -uo pipefail
cd "$(dirname "$0")/.."

# A ratchet that cannot see the source reports zero and passes. Running this from the wrong
# directory, or looking for a pattern that no longer matches, must be an error rather than a
# silent pass - the check exists to catch a number growing, and it cannot catch that by counting
# nothing.
if [ ! -d src ]; then
  echo "cannot find src/ from $(pwd); run this from the repository root" >&2
  exit 2
fi

failed=0
blind=0

# count <pattern>; counts matching occurrences across the source.
#
# grep, not rg. ripgrep is not installed on the CI runner, and because a missing tool produces no
# output rather than an error the count came back zero for every entry and the BLIND guard failed the
# job. That guard was written for exactly this and it earned its place on its first CI run - the
# alternative was a ratchet that reported "nothing left to fix" on every push.
count() { grep -ohE "$1" src/*.rs 2>/dev/null | wc -l | tr -d ' '; }

# Exclude Rust line comments from the poison-lock count. Unlike the generic
# counter, this one only measures method-call spellings in non-comment lines.
count_noncomment() {
  grep -hEv '^[[:space:]]*//' src/*.rs 2>/dev/null | grep -oE "$1" | wc -l | tr -d ' '
}

# ratchet <label> <pattern> <limit> <why> [noncomment]
ratchet() {
  local label="$1" pattern="$2" limit="$3" why="$4" now display
  if [ "${5:-}" = "noncomment" ]; then
    now=$(count_noncomment "$pattern")
    display="$now / $limit"
  else
    now=$(count "$pattern")
    display="$now"
  fi
  if [ "$now" -eq 0 ] && [ "$limit" -gt 0 ]; then
    echo "BLIND $label: the pattern '$pattern' matches nothing, so this check proves nothing."
    echo "      Either the thing it counts was removed - lower the limit to 0 and say so - or the"
    echo "      pattern no longer describes it and the ratchet has been passing on nothing."
    blind=$((blind + 1))
  elif [ "$now" -gt "$limit" ]; then
    echo "GREW  $label: $now, limit $limit"
    echo "      $why"
    echo "      Adding one is allowed only by editing this file, so that the decision is visible."
    failed=$((failed + 1))
  elif [ "$now" -lt "$limit" ]; then
    echo "DOWN  $label: $now, limit $limit - lower the limit to lock the gain in."
  else
    echo "ok    $label: $display"
  fi
}

# The clock cannot be saved, replayed or compared. Every consumer now draws from the world
# stream, which is seeded and saved. The pattern is kept at zero so re-introducing a clock read is
# a deliberate edit rather than a silent regression.
ratchet "wall-clock reads (clock RNG must not return)" 'SystemTime|subsec_nanos' 0 \
  "Randomness must draw from world_rng. A new wall-clock read needs its own decision."

# A parked gate is a named gap. Parking one is sometimes right; parking one silently is how a suite
# stops meaning anything. The mutation check counts them too, from the other side.
ratchet "ignored tests (parked gates)" '#\[ignore' 1 \
  "see PLAN.md section 7; a parked gate proves nothing, so the count must not creep up."

# Count only direct Mutex/RwLock guard unwrap calls, excluding comment lines and
# other unwraps such as thread attachment. The initial ceiling is the live count.
ratchet "lock/rwlock unwraps (no new poison cascades)" \
  '\.(lock|read|write)\(\)\.unwrap\(\)' 3663 \
  "New poison-sensitive lock unwraps must not grow without an explicit review." noncomment

echo
if [ "$blind" -gt 0 ]; then
  echo "debt ratchet: $blind check(s) blind - the count is not being measured"
  exit 3
fi
echo "debt ratchet: $failed over limit"
[ "$failed" -eq 0 ]
