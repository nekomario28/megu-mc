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
    pattern='^(client settings|gameplay) wire: PASS$'
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
  src/worldgen.rs '                    populate(self, sx, sz);' \
  '                    let _ = (&mut populate, sx, sz);' \
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
  src/main.rs 'if w.get(&(p.0 >> 4, p.2 >> 4)).and_then(|c| c.get(&local_idx(p.0, p.1, p.2))).is_some() {' \
  'if false {' \
  a_removed_generated_emitter_stops_lighting

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
  src/main.rs '        if !set_block_if(px, py, pz, 0, Some(st)) { return; }' \
  '        if falling().lock().unwrap().iter().any(|f| f.x.floor() == px as f64 && f.y.floor() == py as f64 && f.z.floor() == pz as f64) { return; } if !set_block_if(px, py, pz, 0, Some(st)) { return; }' \
  a_new_block_in_a_passing_entity_cell_still_falls

check "the falling set comes from the asset, not a hand list" \
  src/main.rs '        if reg.behaviour_of(st) != crate::block_registry::B_FALLING { continue }' \
  '        if !matches!(st, 112 | 117 | 118) { continue }' \
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
  src/main.rs '            if !region.is_warm(f.x as i32, f.z as i32) { keep.push(f); continue; }' \
  '            if false { keep.push(f); continue; }' \
  a_falling_block_waits_in_a_cold_chunk

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
  src/main.rs 'signs().lock().unwrap().retain(|&(sx, sy, sz, _), _| !(sx == x && sy == y && sz == z));' \
  'signs().lock().unwrap().retain(|_, _| true);' \
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

check "the frozen population digest notices shared provider changes" \
  src/generation_providers.rs '            IP::Uniform(a, b) => r.next_inbetween(*a, *b),' \
  '            IP::Uniform(a, b) => (*a + *b) / 2,' \
  feature_population_output_matches_the_pre_refactor_capture

check "the deep-state memo returns the value it computed" \
  src/worldgen.rs 'if let Some(&v) = self.deep_c.get(&(x, y, z)) { return v; }' \
  'if self.deep_c.contains_key(&(x, y, z)) { return 0; }' \
  feature_population_output_matches_the_pre_refactor_capture

check "the noise-value cache returns the value it computed" \
  src/generation_math.rs 'if slot.0 == key { Some(slot.1) } else { None }' \
  'if slot.0 == key { Some(slot.1 + 1.0) } else { None }' \
  feature_population_output_matches_the_pre_refactor_capture

check "the queue cap cannot visit a cell before its real backoff" \
  src/main.rs '    backoff().lock().unwrap().get(&(x, y, z)).map(|(t, _)| *t <= now).unwrap_or(true)' \
  '    backoff().lock().unwrap().get(&(x, y, z)).map(|(_t, _)| true).unwrap_or(true)' \
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
  src/main.rs '            0x0a => {' '            0x7f => {' \
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
  src/main.rs 'write_varint(p, 1); p.extend_from_slice(&0x3ffffffi64.to_be_bytes()); // skyLightMask' \
  'write_varint(p, 1); p.extend_from_slice(&0x3fffffei64.to_be_bytes()); // skyLightMask' \
  tests/gameplay_wire.py 'gameplay wire: FAIL light-mask-count'

check "an update packet sends block light only" \
  src/main.rs '    write_varint(&mut p, 0); // skyLightMask: sky is unchanged' \
  '    write_varint(&mut p, 1); // skyLightMask: sky is unchanged' \
  update_light_packet_sends_block_light_only

echo "mutation check: $passed proved, $failed not a gate, $parked parked, $skipped skipped"
if [ "$parked" -gt 0 ]; then
  echo "  $parked gate(s) are parked and prove nothing until they run again."
fi
# Every registered mutation is required; an unexecuted assertion is not success.
[ "$failed" -eq 0 ] && [ "$parked" -eq 0 ] && [ "$skipped" -eq 0 ] && [ "$passed" -gt 0 ]
