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
# Select here, never copy this script to /tmp: its cwd is owned by this path.
set -uo pipefail
cd "$(dirname "$0")/.."

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
# in it, and the failure did not reproduce once the source was rebuilt. Refuse to start over a
# leftover, and restore on every exit path we control.
if ls src/*.mutation-backup >/dev/null 2>&1; then
  echo "refusing to run: a previous mutation was left applied:" >&2
  ls src/*.mutation-backup >&2
  echo 'restore with: for f in src/*.mutation-backup; do cp "$f" "${f%.mutation-backup}" && touch "${f%.mutation-backup}" && rm "$f"; done' >&2
  exit 2
fi

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
trap 'restore; rm -f "$log" "$artifacts"' EXIT

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
    pattern='^((client settings|gameplay|containers) wire: PASS|population plan: PASS)$'
  else
    pattern='^test result: ok\. [1-9][0-9]* passed; 0 failed; 0 ignored'
  fi
  if [ "$status" = green ] && grep -qE "$pattern" "$log"; then return 0; fi
  echo "FAIL  $name: $phase control did not pass the required test"
  tail -15 "$log"
  return 1
}

# check <name> <file> <from> <to> <test-filter-or-wire-script> [exact-wire-failure]
check() {
  local name="$1" mut_file_local="$2" from="$3" to="$4" test_filter="$5" expected_failure="${6:-}"
  if [ -n "${MUTATION_FILTER:-}" ] && [[ "$test_filter" != *"$MUTATION_FILTER"* ]]; then
    return
  fi
  local status compiled caught=no
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

  # cargo prints "error: test failed" for a failing test, so the exit code and the word error cannot
  # tell a compile failure from an assertion failure. The lines the harness itself prints can.
  if [ "$compiled" = no ] || grep -qE '^error\[' "$log"; then
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
  elif { [[ "$test_filter" == tests/*.py ]] && [ -n "$expected_failure" ] && grep -Fxq "$expected_failure" "$log"; } ||
       { [[ "$test_filter" != tests/*.py ]] && grep -qE 'test result: FAILED' "$log"; }; then
    caught=yes
  else
    echo "FAIL  $name"
    echo "      the run neither compiled, matched, nor reported a failing test."
    tail -15 "$log"
    failed=$((failed + 1))
  fi
  # A failing test is not proof that the mutation caused the failure. Measured
  # 2026-11-11: three entries were recorded as proved only because a load-sensitive
  # test happened to fail in that run; re-running the same campaign reported them as
  # not a gate. With MUTATION_STABILITY=1 the mutant has to fail a second time, so a
  # flake cannot be counted as a catch. Costs one extra run per caught mutant.
  if [ "$caught" = yes ] && [ -n "${MUTATION_STABILITY:-}" ]; then
    run_filter "$test_filter"
    if [ "$status" = green ]; then
      echo "FAIL  $name"
      echo "      the mutant run failed once and passed on a re-run, so the failure was a flake,"
      echo "      not a catch. The test is not a stable gate."
      caught=no
      failed=$((failed + 1))
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
  src/worldgen.rs '    pub(crate) fn begin_carve(&mut self, cx: i32, cz: i32) -> bool {
        self.carved.insert((cx, cz))
    }' \
  '    pub(crate) fn begin_carve(&mut self, _cx: i32, _cz: i32) -> bool {
        true
    }' \
  feature::generation_contract_tests::a_chunk_claims_its_carver_pass_once

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
    is_replaceable(
        reg,
        cur,
        &["minecraft:replaceable_by_trees", "minecraft:logs"],
    )
}' \
  'fn fancy_branch_open(reg: &BReg, cur: u32) -> bool {
    is_replaceable(
        reg,
        cur,
        &["minecraft:logs"],
    )
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

check "lazy feature lookup applies its population input and retains cached overrides" \
  src/worldgen.rs '            for sx in x0..=x1 {
                for sz in z0..=z1 {
                    self.populate_in_order([(sx, sz)], &mut populate);' \
  '            for sx in x0..=x1 {
                for sz in z0..=z1 {
                    self.pop_done.insert((sx, sz));
                    let _ = (&mut populate, sx, sz);'  \
  worldgen::wg_tests::lazy_population_keeps_neighborhood_order_and_cached_overrides
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
  src/main.rs 'gen.get(&(cx + dx, cz + dz))' \
  'gen.get(&(cx, cz))' \
  block_light_sees_a_neighbours_generated_source

check "a removed generated emitter stops lighting" \
  src/main.rs '                if w.get(&(p.0 >> 4, p.2 >> 4))
                    .and_then(|c| c.get(&local_idx(p.0, p.1, p.2)))
                    .is_some()
                {' \
  '                if false {' \
  a_removed_generated_emitter_stops_lighting
check "sky spill loses one level per step" \
  src/main.rs '            let nl = level.saturating_sub(opacity(qx, qy, qz).max(1));' \
  '            let nl = level;' \
  sky_light_reaches_sideways_through_an_opening

check "a knocked-out gravity block becomes an entity" \
  src/main.rs '        let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
        falling().lock().unwrap().push(Fall {
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
  src/main.rs '    let active: Vec<Fall> = falling().lock().unwrap().clone();' \
  '    let active: Vec<Fall> = std::mem::take(&mut *falling().lock().unwrap());' \
  beginning_a_falling_tick_leaves_the_queue_visible

check "a save is serialized with the falling transition" \
  src/main.rs '    let _save = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());' \
  '    let _save = ();' \
  a_save

echo
check "the falling entities are written into the save" \
  src/main.rs '(*f).extend_from_slice(b"FAL2");' \
  '(*f).extend_from_slice(b"XXXX");' \
  the_falling_entities_survive_a_save_and_load_round_trip

check "a migrated legacy cell is explicitly air" \
  src/main.rs '                .insert(local_idx(fx, fy, fz), 0);' \
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
  src/main.rs '    write_varint(&mut *f, in_flight.len() as i64);' \
  '    write_varint(&mut *f, 0i64);' \
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
  src/main.rs '        if let Some(slots) = chests().lock().unwrap().remove(&(x, y, z)) {' \
  '        if let Some(slots) = chests().lock().unwrap().remove(&(x, y, z + 1000)) {' \
  a_broken_chest_leaves_no_orphan_entry_across_a_save_and_load

check "a state whose metadata row cannot be matched is reported as unpaired" \
  src/block_registry.rs '.and_then(|index| pbr.and_then(|block| jarr(block, "states")?.get(index)))' \
  '.and_then(|index| pbr.and_then(|block| jarr(block, "states")?.get(index + 1)))' \
  every_wire_state_has_complete_metadata

check "the client's view distance is read from its packet" \
  src/main.rs '    let view = *p.first()? as i8 as i32;' \
  '    let view = *p.get(1)? as i8 as i32;' \
  client_information_carries_the_view_distance_after_the_locale

check "containers must be decoded, not merely remain absent" \
  src/main.rs 'fn load_world_from(payload: &[u8]) -> Result<(), &'"'"'static str> {' \
  'fn load_world_from(payload: &[u8]) -> Result<(), &'"'"'static str> { return Ok(());' \
  a_broken_chest_leaves_no_orphan_entry_across_a_save_and_load

check "a broken furnace cannot retain its slots" \
  src/main.rs '        if let Some(f) = furnaces().lock().unwrap().remove(&(x, y, z)) {' \
  '        if let Some(f) = furnaces().lock().unwrap().remove(&(x, y, z + 1000)) {' \
  furnace_and_sign_round_trip_keeps_only_live_owners

check "broken sign text cannot survive its block" \
  src/main.rs '        signs()
            .lock()
            .unwrap()
            .retain(|&(sx, sy, sz, _), _| !(sx == x && sy == y && sz == z));' \
  '        signs().lock().unwrap().retain(|_, _| true);' \
  furnace_and_sign_round_trip_keeps_only_live_owners
check "solid metadata must reach every note block state" \
  src/block_registry.rs '                    reg.st_fl[st] = flags;' \
  '                    reg.st_fl[st] = 0;' \
  note_block_keeps_solid_and_light_values_for_every_instrument

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
  src/feature.rs '            let y = i64::from(ctx.heightmap(*kind, pos.0, pos.2));' \
  '            let y = i64::from(ctx.hm_ws(pos.0, pos.2));' \
  feature::generation_contract_tests::surface_relative_filter_keeps_its_heightmap_and_long_bounds
check "dungeon loot uses the wrapped nextLong method" \
  src/feature.rs '                            let _loot_seed = rng.next_i64();' \
  '                            let _loot_seed = rng.next_long();' \
  feature::generation_contract_tests::monster_room_cells_and_rng_match_native
check "simple double plants write both halves" \
  src/feature.rs '                    for (dy, half) in [(0, "lower"), (1, "upper")] {' \
  '                    for (dy, half) in [(0, "lower")] {' \
  feature::generation_contract_tests::simple_block_places_both_halves_or_neither
check "fresh-generation mushrooms do not infer shade from a canopy" \
  src/generation_providers.rs '            reg.in_tag(below, "minecraft:mushroom_grow_block")' \
  '            is_dirtish(reg, below)' \
  feature::generation_contract_tests::mushrooms_in_fresh_generation_need_the_special_substrate
check "matching_fluids uses the fluid rather than block type" \
  src/generation_providers.rs '            let f = reg.fluid_name(st);' \
  '            let f = reg.name_of(st);' \
  feature::generation_contract_tests::matching_fluids_reads_the_fluid_state_not_the_block_name
check "multiface growth performs native spreading" \
  src/feature.rs '        if rng.next_f32() < cfg.spread {' \
  '        if rng.next_f32() < 0.0 {' \
  feature::generation_contract_tests::multiface_cells_and_following_rng_match_native

check "the ore vein radius matches vanilla's own output" \
  src/feature.rs '        ds[k as usize * 4 + 3] = (s1 as f64 * hh + 1.0) / 2.0;' \
  '        ds[k as usize * 4 + 3] = (s1 as f64 * hh + 1.0) / 2.0 + 0.01;' \
  the_ore_vein_geometry_matches_vanillas_own_output

check "the frozen population digest notices shared provider changes" \
  src/generation_providers.rs '            IP::Uniform(a, b) => r.next_inbetween(*a, *b),' \
  '            IP::Uniform(a, b) => (*a + *b) / 2,' \
  feature_population_output_matches_the_pre_refactor_capture

check "the deep-state memo returns the value it computed" \
  src/worldgen.rs '        if let Some(&v) = self.deep_c.get(&(x, y, z)) {
            return v;
        }' \
  '        if self.deep_c.contains_key(&(x, y, z)) {
            return 0;
        }' \
  feature_population_output_matches_the_pre_refactor_capture
check "the noise-value cache returns the value it computed" \
  src/generation_math.rs '            if slot.0 == key {
                Some(slot.1)
            } else {' \
  '            if slot.0 == key {
                Some(slot.1 + 1.0)
            } else {' \
  feature_population_output_matches_the_pre_refactor_capture
check "the shared biome cache returns the value it computed" \
  src/worldgen.rs '        self.bioq.insert(key, l);' \
  '        self.bioq.insert(key, l.wrapping_add(1));' \
  the_shared_biome_cache_returns_the_same_leaf_for_the_same_quart

check "the queue cap cannot visit a cell before its real backoff" \
  src/main.rs '    backoff()
        .lock()
        .unwrap()
        .get(&(x, y, z))
        .map(|(t, _)| *t <= now)
        .unwrap_or(true)' \
  '    backoff()
        .lock()
        .unwrap()
        .get(&(x, y, z))
        .map(|(_t, _)| true)
        .unwrap_or(true)' \
  a_backoff_past_the_queue_cap_waits_for_its_real_deadline
check "a reload continues the world random stream" \
  src/main.rs '            world_rng::restore(u64::from_be_bytes(payload[p..p + 8].try_into().unwrap()));' \
  '            let _ = &payload[p..p + 8];' \
  persistence_tests::the_world_rng_stream_continues_across_a_save_and_load_round_trip

check "configuration information before known packs reaches chunk streaming" \
  src/main.rs '                client_view = v;
            }
            Some((0x07, _))' \
  '                let _ = v;
            }
            Some((0x07, _))' \
  tests/client_settings_wire.py 'client settings wire: FAIL config-early-window'

check "configuration information after known packs reaches chunk streaming" \
  src/main.rs '                client_view = v;
            }
            Some((0x03, _))' \
  '                let _ = v;
            }
            Some((0x03, _))' \
  tests/client_settings_wire.py 'client settings wire: FAIL config-late-window'

check "play information reaches the live chunk window" \
  src/main.rs '                let effective = requested.clamp(2, SERVER_VIEW_DISTANCE);' \
  '                let effective = view;' \
  tests/client_settings_wire.py 'client settings wire: FAIL play-view-3'
check "breaking changes server state, not only the predicted wire update" \
  src/main.rs '                    if break_now {
                        set_block(x, y, z, 0);' \
  '                    if break_now {
                        let _ = old;' \
  tests/gameplay_wire.py 'gameplay wire: FAIL fresh-reader-block'
check "chat returns the actual sender and message" \
  src/main.rs '                    let line = format!("<{}> {}", me.name, m);' \
  '                    let line = format!("<{}> {}", me.name, "");' \
  tests/gameplay_wire.py 'gameplay wire: FAIL chat-echo'

check "light arrays agree with their section masks" \
  src/main.rs '    write_varint(p, 1);
    p.extend_from_slice(&0x3ffffffi64.to_be_bytes()); // skyLightMask' \
  '    write_varint(p, 1);
    p.extend_from_slice(&0x3fffffei64.to_be_bytes()); // skyLightMask' \
  tests/gameplay_wire.py 'gameplay wire: FAIL light-mask-count'
check "an update packet sends block light only" \
  src/main.rs '    write_varint(&mut p, 0); // skyLightMask: sky is unchanged' \
  '    write_varint(&mut p, 1); // skyLightMask: sky is unchanged' \
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

check "the heightmap a feature reads is the floor the blocks are at" \
  src/generation_context.rs '        self.heightmap_above(x, z, start, kind)' \
  '        base' \
  heightmap_reads_the_floor_the_blocks_are_at_not_the_noise_surface

check "the projectile sweep retains start-inside contact" \
  src/main.rs '    bounds.contains(start) || bounds.clip(start, end).is_some()' \
  '    bounds.clip(start, end).is_some()' \
  the_projectile_sweep_covers_the_whole_tick

check "an arrow hits the target it crosses on its first tick" \
  src/main.rs '    bounds.contains(start) || bounds.clip(start, end).is_some()' \
  '    bounds.clip(start, end).is_some()' \
  the_production_arrow_loop_hits_a_target_it_leaves_the_bow_inside

check "a container transfer fills only the room in a matching stack" \
  src/main.rs '    let moved = amount.min(source.1).min(room);' \
  '    let moved = amount.min(source.1).min(room).min(0);' \
  container_transfers_match_real_1_21_1_classes

check "a chest quick move searches the player inventory in reverse" \
  src/main.rs '            let rem = move_to_slots(inv, (9..=44).rev(), st);' \
  '            let rem = move_to_slots(inv, 9..=44, st);' \
  container_transfers_match_real_1_21_1_classes

check "a chest item reaches the placement path" \
  src/main.rs '        299 => 2955,' \
  '        299 => return None,' \
  container_chest_item_reaches_the_placed_default_state

check "the real chest click dispatch preserves the mouse button" \
  src/main.rs '                                    click_slot(gmap, &mut cur, gslot, btn as i8);' \
  '                                    click_slot(gmap, &mut cur, gslot, 0);' \
  tests/container_wire.py 'containers wire: FAIL chest-half-pickup'

# Inventory pickup: prove the native loop AND its actual tick caller.
check "pickup drains beyond the first partially filled slot" \
  src/main.rs '        if source.1 == 0 || source.1 == before {' \
  '        if true {' \
  pickup_inventory_matches_real_1_21_1_classes

check "the pickup tick forwards the selected hotbar slot" \
  src/main.rs '            let accepted = pickup_into_inventory(&mut inv, selected, creative, &mut source);' \
  '            let accepted = pickup_into_inventory(&mut inv, 0, creative, &mut source);' \
  pickup_tick_drains_across_selected_offhand_then_hotbar

check "partial insertion is not a successful ItemEntity take callback" \
  src/main.rs '        if accepted {
            broadcast_seen(deid, |_c| (0x6f, collect_pkt(deid, c.eid, original)));' \
  '        if original > 0 {
            broadcast_seen(deid, |_c| (0x6f, collect_pkt(deid, c.eid, original)));' \
  pickup_tick_keeps_the_partial_entity_without_a_take_event

check "a full nearby player does not monopolize the drop" \
  src/main.rs '                    taken.push((*d_eid, c.clone()));' \
  '                    taken.push((*d_eid, c.clone())); break;' \
  pickup_tick_tries_the_next_player_but_never_inserts_a_stale_stack

check "the pickup tick forwards infinite materials to inventory insertion" \
  src/main.rs '        let creative = *c.gm.lock().unwrap() == 1;' \
  '        let creative = false;' \
  pickup_tick_tries_the_next_player_but_never_inserts_a_stale_stack

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
  src/main.rs '    value as i8 // Java int -> byte wraps; a direct Rust float -> i8 saturates.' \
  '    scaled as i8 // broken: saturation instead of byte wrapping' \
  entity_angles_head_update_matches_native_floor

check "entity head updates use floor rather than truncation" \
  src/main.rs '    angle_byte(f64::from(deg * 256.0 / 360.0), AngleRounding::Floor)' \
  '    angle_byte(f64::from(deg * 256.0 / 360.0), AngleRounding::Truncate)' \
  entity_angles_head_update_matches_native_floor

check "entity rotations multiply before division even at subnormal inputs" \
  src/main.rs '    angle_byte(f64::from(deg * 256.0 / 360.0), AngleRounding::Floor)' \
  '    angle_byte(f64::from(deg / 360.0 * 256.0), AngleRounding::Floor)' \
  entity_angles_head_update_matches_native_floor

check "entity teleports truncate rather than sharing spawn rounding" \
  src/main.rs '    angle_byte(f64::from(deg * 256.0 / 360.0), AngleRounding::Truncate)' \
  '    angle_byte(f64::from(deg * 256.0 / 360.0), AngleRounding::Floor)' \
  entity_angles_teleport_matches_native_constructor

check "entity spawn head angles keep the constructor double precision" \
  src/main.rs '        angle_byte(f64::from(yaw) * 256.0 / 360.0, AngleRounding::Floor) as u8,' \
  '        angle(yaw) as u8,' \
  entity_angles_spawn_matches_native_constructors

# Binary/geometry primitives: actual-class vectors plus production dispatch.
check "section palettes switch to registry IDs above the indirect limit" \
  src/main.rs '    let direct = bits > max_bits;' \
  '    let direct = bits > max_bits + 1;' \
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
  src/main.rs '                            elems.push(d[p..end].to_vec());' \
  '                            elems.push(nbt_string(&String::from_utf8_lossy(&d[p + 3..end])));' \
  primitives_nbt_lore_append_preserves_java_string_bytes

check "AABB clip excludes the finite segment endpoint" \
  src/main.rs '                && t < nearest' \
  '                && t <= nearest' \
  primitives_aabb_clip_matches_native_finite_segments

check "AABB clip keeps the pinned epsilon" \
  src/main.rs '        const EPSILON: f64 = 1.0e-7;' \
  '        const EPSILON: f64 = 0.0;' \
  primitives_aabb_clip_matches_native_finite_segments

check "arrow tick uses the continuous segment rather than just its end" \
  src/main.rs '    bounds.contains(start) || bounds.clip(start, end).is_some()' \
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
  src/worldgen.rs 'rarity * self.noises[*ni].get(xd, yd, zd).abs()' \
  'rarity * self.noises[*ni].get(xd, yd, zd).abs() * 0.0' \
  weird_sampler_installed_nodes_are_nonzero

check "weird sampler divides the vertical coordinate by rarity" \
  src/worldgen.rs 'let yd = y as f64 / rarity;' \
  'let yd = y as f64 * rarity;' \
  weird_sampler_matches_native_mapper_and_samples

check "weird sampler takes absolute noise before scaling" \
  src/worldgen.rs 'rarity * self.noises[*ni].get(xd, yd, zd).abs()' \
  'rarity * self.noises[*ni].get(xd, yd, zd)' \
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
  src/worldgen.rs 'rarity * self.noises[*ni].get(xd, yd, zd).abs()' \
  'self.noises[*ni].get(xd, yd, zd).abs()' \
  worldgen::weird_sampler_tests::weird_sampler_matches_native_mapper_and_samples

check "aquifer preliminary surface uses the pinned threshold" \
  src/worldgen.rs 'if self.eval(id, ax, y, az, &mut HashMap::new()) > 0.390625 {' \
  'if self.eval(id, ax, y, az, &mut HashMap::new()) > 0.0 {' \
  aquifer_matches_the_pinned_chunk_through_its_public_api

check "aquifer preliminary surface scans quart columns" \
  src/worldgen.rs 'let ax = (x >> 2) << 2;' \
  'let ax = (x >> 4) << 4;' \
  aquifer_matches_the_pinned_chunk_through_its_public_api

check "aquifer spline keeps the native float interpolation order" \
  src/worldgen.rs 'let lerp = |delta: f32, start: f32, end: f32| start + delta * (end - start);' \
  'let lerp = |delta: f32, start: f32, end: f32| (1.0 - delta) * start + delta * end;' \
  aquifer_matches_the_pinned_chunk_through_its_public_api

check "aquifer deep test keeps the depth threshold" \
  src/worldgen.rs '&& self.eval(c[4], x, y, z, &mut memo) > 0.9f32 as f64' \
  '&& self.eval(c[4], x, y, z, &mut memo) > -2.0' \
  aquifer_deep_level_reads_erosion_and_depth_in_f32

check "aquifer deep test reads erosion and depth" \
  src/worldgen.rs $'self.eval(c[3], x, y, z, &mut memo) < -0.225f32 as f64\n                    && self.eval(c[4], x, y, z, &mut memo) > 0.9f32 as f64' \
  $'self.eval(c[2], x, y, z, &mut memo) < -0.225f32 as f64\n                    && self.eval(c[3], x, y, z, &mut memo) > 0.9f32 as f64' \
  aquifer_deep_level_reads_erosion_and_depth_in_f32

check "aquifer deep test promotes the f32 threshold" \
  src/worldgen.rs '< -0.225f32 as f64' \
  '< -0.225' \
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
  src/worldgen.rs '                for c in ch {' \
  '                for c in ch.iter().rev() {' \
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
  src/worldgen.rs '                    sda += 1;' \
  '                    sda += 0;' \
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
  src/generation_providers.rs '| "minecraft:warped_fungus" => reg.in_tag(below, "minecraft:nylium"),' \
  '| "minecraft:warped_fungus" => reg.in_tag(below, "minecraft:netherrack"),' \
  feature::generation_contract_tests::nether_forest_vegetation_matches_native_cells_and_rng

check "nether forest vegetation keeps the ceiling guard" \
  src/feature.rs '    if y < ctx.g.min_y + 1 || y + 1 >= ctx.g.min_y + ctx.g.height {' \
  '    if y < ctx.g.min_y + 1 {' \
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
  src/worldgen.rs '            *GEN_TICK_QUEUES.lock().unwrap() = Some(g.generated_ticks.clone());' \
  '            *GEN_TICK_QUEUES.lock().unwrap() = None;' \
  worldgen::wg_tests::runtime_drains_only_published_ticks_without_the_gen_lock

check "runtime tick drain does not wait for the generation lock" \
  src/worldgen.rs 'pub fn take_generated_leaf_ticks() -> Vec<GeneratedTick> {
    let queues = GEN_TICK_QUEUES.lock().unwrap().clone();' \
  'pub fn take_generated_leaf_ticks() -> Vec<GeneratedTick> {
    let _guard = gen_lock();
    let queues = GEN_TICK_QUEUES.lock().unwrap().clone();' \
  worldgen::wg_tests::runtime_drains_only_published_ticks_without_the_gen_lock

check "generated handoff retains native negative suborders" \
  src/scheduled_ticks.rs 'sub_order: -*remaining,' 'sub_order: 0,' \
  tick_tests::generated_fluid_handoff_matches_native_proto_unpack

check "generated handoff counts each chunk rather than the whole batch" \
  src/scheduled_ticks.rs '        for (pos, _) in &pending {
            *counts.entry((pos.0 >> 4, pos.2 >> 4)).or_default() += 1;
        }
        for (pos, block) in pending {
            let remaining = counts.get_mut(&(pos.0 >> 4, pos.2 >> 4)).unwrap();' \
  '        for _ in &pending {
            *counts.entry((0, 0)).or_default() += 1;
        }
        for (pos, block) in pending {
            let remaining = counts.get_mut(&(0, 0)).unwrap();' \
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
  src/main.rs "    o.push('\"');" "    o.push(' ');" \
  sign_side_nbt_matches_the_shape_the_game_writes

check "a sign side carries filtered_messages" \
  src/main.rs '    for list in ["filtered_messages", "messages"] {' '    for list in ["messages"] {' \
  sign_side_nbt_matches_the_shape_the_game_writes

check "the open window reads the live store, not a copy" \
  src/main.rs '    let ch = all.get(&pos).cloned().unwrap_or_default();' '    let ch: HashMap<i16, Stack> = HashMap::new();' \
  a_placed_chest_shows_its_staged_contents_in_the_open_window

check "staged containers reach the live store" \
  src/main.rs '        for (pos, payload) in staged {' '        for (pos, payload) in Vec::<(( i32,i32,i32), crate::staged_block_entity::StagedBlockEntity)>::new() {' \
  staged_container_contents_reach_the_store_and_survive_a_save

check "staged containers are taken from the generator once" \
  src/worldgen.rs '        std::mem::take(&mut self.staged_block_entities)' '        Vec::new()' \
  staged_container_contents_reach_the_store_and_survive_a_save

check "the backoff guard holds a visit past the queue cap" \
  src/main.rs 'fn sched_ready(x: i32, y: i32, z: i32) -> bool {
    let now = TICK_IDX.load(Ordering::Relaxed);' 'fn sched_ready(x: i32, y: i32, z: i32) -> bool {
    let now = TICK_IDX.load(Ordering::Relaxed) + 10_000;' \
  a_backoff_past_the_queue_cap_waits_for_its_real_deadline

check "freeze_top_layer reads MOTION_BLOCKING rather than WORLD_SURFACE" \
  src/feature.rs '                    let top = ctx.heightmap(HeightmapKind::MotionBlocking, x, z);' '                    let top = ctx.heightmap(HeightmapKind::WorldSurface, x, z);' \
  freeze_top_layer_matches_native_oracle

check "freeze_top_layer temperature guard gates snow placement" \
  src/feature.rs '                        && ctx.cold_enough_to_snow(biome, x, top, z)' '                        && true' \
  freeze_top_layer_matches_native_oracle

check "freeze_top_layer sets snowy=true on the dirt below placed snow" \
  src/feature.rs '                                ctx.set(x, below_y, z, reg.with_prop(below, "snowy", "true"));' '                                let _ = below_y;' \
  freeze_top_layer_matches_native_oracle

check "freeze_top_layer refuses snow on snow_layer_cannot_survive_on blocks" \
  src/feature.rs '    if reg.in_tag(below, "minecraft:snow_layer_cannot_survive_on") {
        return false;
    }' '    if reg.in_tag(below, "minecraft:snow_layer_cannot_survive_on") {
        return true;
    }' \
  freeze_top_layer_matches_native_oracle

check "the canonical state keeps its properties" \
  src/block_registry.rs '            format!("{}[{}]", name, parts.join(","))' '            name' \
  overworld_chunk_matches_native_capture_state_for_state

check "an Overworld chunk falls back to the deep state" \
  src/worldgen.rs '            let st = self.deep_state_cached(x, y, z, cc);' '            let st = self.def_st;' \
  overworld_chunk_matches_native_capture_state_for_state

check "an entity offset flips the axis native flips" \
  src/structure_transform.rs '        Mirror::LeftRight => z = 1.0 - z,' '        Mirror::LeftRight => x = 1.0 - x,' \
  structure_transform::tests::entity_offset_transform_matches_native

check "an entity offset keeps the pivot" \
  src/structure_transform.rs '            f64::from(px.wrapping_add(pz).wrapping_add(1)) - z,' '            f64::from(px.wrapping_sub(pz).wrapping_add(1)) - z,' \
  structure_transform::tests::entity_offset_transform_matches_native

check "a placed sign stages its front lines" \
  src/structure_template.rs '            return Ok(StagedBlockEntity::Sign(Self::sign_lines(tag)?));' '            return Ok(StagedBlockEntity::Sign(Default::default()));' \
  template_sign_text_matches_native_and_reaches_the_sign_store

check "a sign message that is not a string component is refused" \
  src/structure_template.rs '                return Err("a sign message must be a string component");' '                lines[slot] = String::new(); continue;' \
  unsupported_sign_components_are_refused

check "staged signs reach the sign store" \
  src/main.rs '                        (pos.0, pos.1, pos.2, 1),' '                        (pos.0, pos.1, pos.2, 0),' \
  template_sign_text_matches_native_and_reaches_the_sign_store

check "a furnace is staged as a furnace, not a chest" \
  src/structure_template.rs '"minecraft:furnace" | "minecraft:blast_furnace" | "minecraft:smoker" => {' '"minecraft:dispenser" => {' \
  template_furnace_contents_reach_the_furnace_store

check "a container with no store is refused" \
  src/structure_template.rs '                return Err("template container NBT on a block with no supported store");' '                ContainerKind::Chest' \
  unsupported_container_states_are_refused

check "furnace contents land in the furnace store" \
  src/main.rs '                                    0 => furnace.input = stack,' '                                    0 => furnace.output = stack,' \
  template_furnace_contents_reach_the_furnace_store

check "a placed container stages its contents" \
  src/structure_template.rs '                    ctx.block_entities
                        .push((p, Self::staged_payload(tag, state, reg)?));' '                    let _ = Self::staged_payload(tag, state, reg)?;' \
  template_container_items_match_native_placement

check "template NBT other than container Items is refused" \
  src/structure_template.rs '            if key != "Items" {' '            if false {' \
  the_fossil_contract_still_refuses_a_container

check "a block write is not split from its tick booking by a save" \
  src/main.rs '    with_save_guard(|| set_block_if_inner(x, y, z, st, expected))' '    set_block_if_inner(x, y, z, st, expected)' \
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
  src/tree.rs 'crate::voxel_faces::exposed_faces(shape.positions())' \
  'crate::voxel_faces::exposed_faces(shape.positions()).into_iter().rev()' \
  tree::tests::leaf_edge_schedules_match_native_order

check "solid template consumes shared voxel faces in native order" \
  src/structure_template.rs 'crate::voxel_faces::exposed_faces(written)' \
  'crate::voxel_faces::exposed_faces(written).into_iter().rev()' \
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
  src/main.rs '        && std::env::var_os("MC_SPINE_WG_PROFILE").is_none()' \
  '        && false' tests/population_plan_test.py 'population plan: FAIL missing-profile'

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
  src/generation_context.rs '        if let Some(st) = self.g.feature_override(x, y, z) {' \
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

echo "mutation check: $passed proved, $failed not a gate, $parked parked, $skipped skipped"
if [ "$parked" -gt 0 ]; then
  echo "  $parked gate(s) are parked and prove nothing until they run again."
fi
# Every registered mutation is required; an unexecuted assertion is not success.
[ "$failed" -eq 0 ] && [ "$parked" -eq 0 ] && [ "$skipped" -eq 0 ] && [ "$passed" -gt 0 ]
