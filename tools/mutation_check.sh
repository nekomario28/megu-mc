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
    pattern='^(client settings|gameplay|containers) wire: PASS$'
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
  if ! control "$name" "$test_filter" baseline; then
    failed=$((failed + 1))
    return
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
  restore
  verify_restored "$name" "$sha_before"
  if ! control "$name" "$test_filter" restored; then
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

check "lazy feature lookup applies its population input and retains cached overrides" \
  src/worldgen.rs '            for sx in x0..=x1 {
                for sz in z0..=z1 {
                    if !self.pop_done.contains(&(sx, sz)) {
                        self.pop_done.insert((sx, sz));
                        populate(self, sx, sz);
                    }' \
  '            for sx in x0..=x1 {
                for sz in z0..=z1 {
                    if !self.pop_done.contains(&(sx, sz)) {
                        self.pop_done.insert((sx, sz));
                        let _ = (&mut populate, sx, sz);
                    }' \
  worldgen::wg_tests::lazy_population_keeps_neighborhood_order_and_cached_overrides
check "a placed gravity block is scheduled by its own cell" \
  src/main.rs '    for (px, py, pz) in [(x, y, z), (x, y + 1, z)] {' \
  '    for (px, py, pz) in [(x, y + 1, z), (x, y + 1, z)] {' \
  every_falling_block_becomes_an_entity_and_lands

check "placing unsupported sand queues air, not a ghost" \
  src/main.rs '    note_block_update(wx, wy, wz, block_at(wx, wy, wz));' \
  '    note_block_update(wx, wy, wz, st);' \
  placing_unsupported_sand_queues_air_not_a_ghost

check "a block above a removed support is scheduled" \
  src/main.rs '    for (px, py, pz) in [(x, y, z), (x, y + 1, z)] {' \
  '    for (px, py, pz) in [(x, y, z), (x, y, z)] {' \
  sand_falls_under_vanilla_gravity_until_it_lands

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

check "a long unsupported column converts in one pass" \
  src/main.rs '        UPD_DEFER.with(|d| d.borrow_mut().push((x, y, z)));' \
  '        let _ = (x, y, z);' \
  a_long_unsupported_column_converts_in_one_pass

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
  a_falling_block_waits_in_a_cold_chunk
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

check "a renamed block is still paired between the two assets" \
  src/block_registry.rs '("minecraft:chain", "minecraft:iron_chain")' \
  '("minecraft:chain", "minecraft:not_a_block")' \
  every_wire_state_has_complete_metadata

check "the client's view distance is read from its packet" \
  src/main.rs '    let view = *p.first()? as i8 as i32;' \
  '    let view = *p.get(1)? as i8 as i32;' \
  client_information_carries_the_view_distance_after_the_locale

check "containers must be decoded, not merely remain absent" \
  src/main.rs 'fn load_world_from(payload: &[u8]) {' \
  'fn load_world_from(payload: &[u8]) { return;' \
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
  src/block_registry.rs '                        reg.st_fl[st] = flags;' \
  '                        reg.st_fl[st] = 0;' \
  note_block_keeps_solid_and_light_values_for_every_instrument

check "property remapping cannot reuse positional state offsets" \
  src/block_registry.rs '        out = out.checked_mul(prop.vals.len())?.checked_add(digit)?;' \
  '        out = offset;' \
  property_names_and_values_determine_metadata_position

check "a predicate reads solidity, not replaceability" \
  src/generation_providers.rs '        Pr::Solid => reg.is_solid(ctx.get(x, y, z)),' \
  '        Pr::Solid => reg.is_replaceable(ctx.get(x, y, z)),' \
  provider_values_and_following_rng_match_pre_extraction

check "a context override outranks the cached terrain read" \
  src/generation_context.rs '        if let Some(&s) = self.wr.get(&(x, y, z)) {' \
  '        if let Some(&s) = self.rc.get(&(x, y, z)) {' \
  context_overrides_heights_bounds_and_biomes_keep_existing_results

check "placement modifiers advance positions in vanilla's draw order" \
  src/feature.rs '            let (x, z) = (pos.0 + rng.next_bound(16), pos.2 + rng.next_bound(16));' \
  '            let z = pos.2 + rng.next_bound(16);
            let x = pos.0 + rng.next_bound(16);' \
  feature_population_output_matches_the_pre_refactor_capture
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
  src/generation_context.rs '        let terrain = self.heightmap_above(x, z, start, base, false);' \
  '        let terrain = base;' \
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

echo "mutation check: $passed proved, $failed not a gate, $parked parked, $skipped skipped"
if [ "$parked" -gt 0 ]; then
  echo "  $parked gate(s) are parked and prove nothing until they run again."
fi
# Every registered mutation is required; an unexecuted assertion is not success.
[ "$failed" -eq 0 ] && [ "$parked" -eq 0 ] && [ "$skipped" -eq 0 ] && [ "$passed" -gt 0 ]
