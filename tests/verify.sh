#!/usr/bin/env bash
# Everything that has to pass before a commit. This was six commands typed by hand
# each time, which is six chances to skip one.
set -euo pipefail
cd "$(dirname "$0")/.."
export PYTHONDONTWRITEBYTECODE=1
# The wire tests generate spawn chunks before the play phase. On a shared host the
# 10s default can expire under load without a code failure (observed: the mutation
# check's baseline control failed twice in one run and passed when re-run alone).
# Keep a bound, but give the server load headroom.
export MC_PROBE_DEADLINE="${MC_PROBE_DEADLINE:-30}"

echo "== gate exit contracts"
python3 tests/gate_scripts.py
python3 tests/evidence_receipt_test.py
python3 tests/protocol767_test.py
python3 tests/nbt_region_test.py
python3 tests/structure_oracle_test.py
python3 tests/template_neighbor_oracle_test.py
python3 tests/template_init_oracle_test.py
python3 tests/nofeat_pack_test.py
python3 tests/native_survival_inventory_test.py
python3 tests/native_survival_oracle_test.py
python3 tests/native_neighbor_trace_test.py
python3 tests/native_cargo_gate_test.py
python3 tests/feature_coverage_test.py
python3 tests/test_saved_tick_inventory.py
python3 tests/biome_tree_test.py
python3 tests/carver_diff_test.py
python3 tests/sculk_trace_test.py
python3 tests/carve_agent_build_test.py
python3 tests/vanilla_java_test.py
python3 tests/forest_stage_check_test.py
python3 tests/container_oracle_test.py
python3 tests/surface_oracle_test.py
python3 tests/surface_condition_oracle_test.py
python3 tests/surface_scan_oracle_test.py
python3 tests/packet_angles_oracle_test.py
python3 tests/primitives_oracle_test.py
python3 tests/aquifer_oracle_test.py
python3 tests/biome_carver_oracle_test.py
python3 tests/nextint_oracle_test.py
python3 tests/biome_tie_oracle_test.py
python3 tests/biome_spawn_oracle_test.py
python3 tests/weird_sampler_oracle_test.py
python3 tests/suite_compare_test.py
python3 tests/chunk_triage_test.py
python3 tests/port_triage_test.py
python3 tests/structure_capture_test.py
python3 tests/p0_2_audit_test.py
python3 tests/public_export_test.py

echo "== module layering"
python3 tests/layers_gate_test.py
bash tools/layers_check.sh || exit 1

echo "== debt ratchet"
bash tools/debt_ratchet.sh || exit 1

echo "== tests, serial"
cargo test --offline --locked -j 2 -- --test-threads=1

echo "== tests, eight threads, three times"
# The exit status has to be read from cargo, not from the pipeline. Piping through tail and grep
# made this stage unable to fail the script: grep succeeded, the pipeline reported success, and a red
# parallel run printed "test result: FAILED" and then let verify.sh continue to PASS.
for i in 1 2 3; do
  # Read the status inside an `if` so `set -e` does not exit before the failure is printed: a bare
  # `out=$(...)` aborts the script and the log ends with no failing test named.
  if ! out=$(cargo test --offline --locked -j 2 -- --test-threads=8 2>&1); then
    echo "parallel suite failed on run $i"
    if echo "$out" | grep -qE '^failures:'; then
      echo "$out" | grep -A14 '^failures:' | head -40
    else
      # A build failure has no `failures:` block; print the compiler output instead of nothing.
      echo "$out" | tail -30
    fi
    exit 1
  fi
  echo "$out" | tail -2 | grep -E 'test result'
done

echo "== wire smoke"
build_manifest=$(mktemp)
trap 'rm -f "$build_manifest"' EXIT
cargo build --offline --locked -j 2 --message-format=json-render-diagnostics >"$build_manifest"
binary=$(python3 tools/cargo_executable.py <"$build_manifest")
timeout 180 python3 tests/wire_smoke.py "$binary"
timeout 180 python3 tests/dimension_wire.py "$binary"
timeout 180 python3 tests/client_settings_wire.py "$binary"
timeout 180 python3 tests/gameplay_wire.py "$binary"
timeout 180 python3 tests/container_wire.py "$binary"
timeout 180 python3 tests/population_plan_test.py "$binary"
timeout 180 python3 tests/mineshaft_dispatch_test.py "$binary"

echo "== adapter checks"
cargo check --offline --locked -j 2 --features fabric-probe
cargo check --offline --locked -j 2 --features neoforge-events

# Last, because it is the slowest: prove the gates can still fail. A suite that is green over a gate
# that cannot fail reports safety it does not have, and this is the only check that would notice.
# MUTATION_STABILITY=1 is the default here because a single red run cannot tell a catch from a
# load-sensitive flake (measured 2026-11-11: three entries were recorded as proved and re-ran as
# not a gate). It costs one extra run per caught mutant. The campaign copies the tree into a
# private snapshot and builds in a private target, so this stage no longer touches the working
# tree other stages may be reading.
MUTATION_STABILITY=1 bash tools/mutation_check.sh --offline || exit 1

echo "verify: PASS"
