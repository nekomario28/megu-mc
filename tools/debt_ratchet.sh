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
cd "$(dirname "$0")/.." || exit

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

# count_ws <grep -E pattern> [find args...]
#
# Same contract as count(), but across every .rs in the repo — all standalone
# crates and tools/ — rather than only the root crate's src/. Build output and
# .git are excluded; callers pass extra find path filters (e.g.
# -not -path '*/tests/*') to scope the count. find, not git ls-files, so the
# ratchet still measures something inside a source snapshot with no .git.
count_ws() {
  local pattern="$1"
  shift
  find . -name '*.rs' -not -path '*/target/*' -not -path './.git/*' "$@" \
      -exec grep -ohE "$pattern" {} + 2>/dev/null | wc -l | tr -d ' '
}

# report <label> <now> <limit> <why> [display]
report() {
  local label="$1" now="$2" limit="$3" why="$4" display="${5:-}"
  display="${display:-$now}"
  if [ "$now" -eq 0 ] && [ "$limit" -gt 0 ]; then
    echo "BLIND $label: the pattern matches nothing, so this check proves nothing."
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

# ratchet <label> <pattern> <limit> <why> [noncomment]
ratchet() {
  local label="$1" pattern="$2" limit="$3" why="$4" now display
  if [ "${5:-}" = "noncomment" ]; then
    now=$(count_noncomment "$pattern")
    report "$label" "$now" "$limit" "$why" "$now / $limit"
  else
    now=$(count "$pattern")
    report "$label" "$now" "$limit" "$why"
  fi
}

# ratchet_ws <label> <pattern> <limit> <why> [find args...]
# Workspace-wide variant of ratchet(): the pattern is counted across every
# .rs outside the excluded paths, so a duplicate that lands in a standalone
# crate or tools/ is caught the same as one in src/.
ratchet_ws() {
  local label="$1" pattern="$2" limit="$3" why="$4"
  shift 4
  report "$label" "$(count_ws "$pattern" "$@")" "$limit" "$why"
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
  '\.(lock|read|write)\(\)\.unwrap\(\)' 0 \
  "Poison-sensitive lock unwraps are extinct; any new one needs an explicit review." noncomment

# Workspace-wide ratchets. These count every .rs in the repo — root src/ plus
# all standalone crates and tools/ — so a regression cannot hide in a crate the
# root-src counters never see. Baselines are the exact counts on main; lowering
# one is a deliberate edit here, raising one is a reviewed decision.

# An allow(dead_code) is a silenced warning; at this count it is load-bearing
# only because each site was accepted once. New ones must be argued here.
# 2632 -> 2644: XMOD1-CORE staged the 12 live_cap_* cross-mod bus API entry
# points (insert/extract/transfer/register families) for the menus_and_items.rs
# hopper + main.rs game_tick consumers landing in the A1-SERVER lane; each is
# waived in audit/wiring_connectivity_waivers.json until its caller lands.
# 2644 -> 2680: upstream lanes landed 36 more allows ahead of this commit
# (vanilla-mob batch 247b2f6c +19, aeronautics & client batches the rest).
# 2680 -> 2439: the deduplication & dead-code sweep merged duplicate cfg_attr
# pairs and removed or wired staged helpers across lanes.
# 2439 -> 2436: the branch-mining helpers went live through the canonical
# break-progress model and shed their silencers.
# 2436 -> 2420: removed 49 stale dead_code allowances across src/.
ratchet_ws "allow(dead_code) attributes (workspace)" 'allow\(dead_code\)' 2420 \
  "Dead code should be deleted or wired, not silenced. A new allowance needs an explicit decision."

# SHA-256 keeps being re-implemented per crate (mod-adapter-core, proto,
# server-files, state-store, root) instead of shared through megu-core. The
# ceiling freezes today's duplicates; consolidation lowers it.
# 4 -> 2: server-files/installer and src/generation_math now re-export core::crypto::Sha256/sha256.
ratchet_ws "duplicate sha256/Sha256 impls outside core" \
  '^(pub(\(crate\)|\(super\))? )?(struct Sha256|fn sha256)\b' 2 \
  "SHA-256 belongs in megu-core; a new copy is a new thing to keep correct." \
  -not -path '*/tests/*' -not -path './core/src/*'

# Hand-rolled JSON parsers outside core are extinct: every crate delegates to
# megu_core::json, so the ceiling is locked at 0 permanently.
ratchet_ws "duplicate JsonParser/parse_json impls outside core" \
  '^(pub(\(crate\)|\(super\))? )?(struct JsonParser|fn parse_json)\b' 0 \
  "JSON parsing belongs in megu-core; a new copy is a new parser to keep correct." \
  -not -path '*/tests/*' -not -path './core/src/*'

# CRC32 (IEEE) likewise: the canonical table lives in core/src/hash.rs; every
# other top-level copy is a duplicate. Test modules are excluded by the
# column-0 anchor and the */tests/* path filter.
ratchet_ws "duplicate crc32 impls outside core/src/hash.rs" \
  '^(pub(\(crate\)|\(super\))? )?(const )?fn crc32\(' 3 \
  "CRC32 belongs in core/src/hash.rs; a new copy is a new table to keep correct." \
  -not -path '*/tests/*' -not -path './core/src/hash.rs'

echo
if [ "$blind" -gt 0 ]; then
  echo "debt ratchet: $blind check(s) blind - the count is not being measured"
  exit 3
fi
echo "debt ratchet: $failed over limit"
[ "$failed" -eq 0 ]
