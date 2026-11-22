#!/usr/bin/env bash
# Everything that has to pass before a commit. This was six commands typed by hand
# each time, which is six chances to skip one.
set -euo pipefail

without_mutations=0
case "${1:-}" in
  "") ;;
  --without-mutations)
    without_mutations=1
    shift
    ;;
  *)
    echo "usage: $0 [--without-mutations]" >&2
    exit 2
    ;;
esac
if (($# != 0)); then
  echo "usage: $0 [--without-mutations]" >&2
  exit 2
fi

cd "$(dirname "$0")/.."
mkdir -p target/tmp
export TMPDIR="${TMPDIR:-$PWD/target/tmp}"
export JAVA_TOOL_OPTIONS="${JAVA_TOOL_OPTIONS:--Djava.io.tmpdir=$TMPDIR -XX:-UsePerfData}"
export PYTHONDONTWRITEBYTECODE=1
# The wire tests generate spawn chunks before the play phase. On a shared host the
# 10s default can expire under load without a code failure (observed: the mutation
# check's baseline control failed twice in one run and passed when re-run alone).
# Keep a bound, but give the server load headroom.
export MC_PROBE_DEADLINE="${MC_PROBE_DEADLINE:-30}"

echo "== portable fluid trace comparator"
python3 tests/fluid_trace_diff_test.py

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
python3 tests/sculk_gate_test.py
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
python3 tests/fixture_suite_test.py
python3 tests/chunk_triage_test.py
python3 tests/port_triage_test.py
python3 tests/structure_capture_test.py
python3 tests/p0_2_audit_test.py
python3 tests/public_export_test.py
python3 tests/shimcheck_test.py
python3 tests/neoforge_bootstrap_test.py
python3 tests/neoforge_s1_events_test.py
python3 tests/neoforge_s2_registry_test.py
python3 tests/neoforge_s3_batch_test.py
python3 tests/neoforge_s4_network_test.py
python3 tools/mod_compat_audit/test_audit.py
python3 tests/fabric_shim_test.py
python3 tests/client_launcher_test.py
python3 tests/megu_start_test.py
python3 tests/megu_mods_test.py
python3 tests/load_client_test.py
python3 tools/provenance_check.py
python3 tools/provenance_check.py --self-test
python3 tests/mod_fallback_test.py
python3 tests/culling_perf_gate.py --self-test
python3 tests/client_fork_e2e.py --self-test
python3 tests/perf_bench_test.py
python3 tests/pgo_build_test.py
python3 tools/vanilla_data/version_pack.py --self-test
python3 tools/parity_cache.py --self-test
python3 tools/release_bundle.py --self-test
python3 tests/release_bundle_test.py
python3 tests/release_workflow_test.py
python3 tests/alpha_release_smoke.py --self-test
python3 tests/multi_mod_e2e_wire.py --self-test
python3 tests/dev_check_test.py
python3 tests/megu_translate_test.py
python3 tests/megu_bench_test.py
python3 tests/mini_jre_test.py
python3 tests/vanilla_parity_audit_test.py
python3 tools/vanilla_parity_audit.py --format text --strict --min-c2s-handled 58 --min-gamerules-wired 51
python3 tests/modloader_parity_audit_test.py
python3 tools/modloader_parity_audit.py --format text --strict --min-overall-pct 100
python3 tests/vanilla_live_diff_test.py
python3 tests/live_parity_e2e_wire.py --self-test
python3 tests/mod_suite_live_wire.py --self-test
python3 website/check_site.py
python3 tests/verify_gate_coverage_test.py

echo "== module layering"
python3 tests/layers_gate_test.py
bash tools/layers_check.sh || exit 1

echo "== debt ratchet"
bash tools/debt_ratchet.sh || exit 1

# The trap is installed before the first stage that writes a standalone crate's target, so a red
# standalone crate does not leave its target behind for the next run to build against.
build_manifest=$(mktemp)
cleanup_verify_artifacts() {
  rm -f "$build_manifest"
  rm -rf client-port/target client-kernel/target client-app/target client-ui/target client-gpu/target mod-offload/target \
      mod-adapter-core/target create-adapter/target proto/target state-store/target \
      server-files/target core/target
  # The companion suite only writes repo-local output when CARGO_TARGET_DIR is
  # unset; a caller-provided target dir is the caller's to keep.
  if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    rm -rf target/companion-e2e
  fi
}
trap cleanup_verify_artifacts EXIT

echo "== standalone crates"
cargo test --manifest-path client-port/Cargo.toml --offline --locked -j 2
cargo test --manifest-path client-kernel/Cargo.toml --offline --locked -j 2
cargo test --manifest-path client-app/Cargo.toml --offline --locked -j 2
cargo test --manifest-path client-ui/Cargo.toml --offline --locked -j 2
cargo test --manifest-path client-gpu/Cargo.toml --offline --locked -j 2
cargo test --manifest-path mod-offload/Cargo.toml --offline --locked -j 2
cargo test --manifest-path mod-adapter-core/Cargo.toml --offline --locked -j 2
cargo test --manifest-path create-adapter/Cargo.toml --offline --locked -j 2
cargo test --manifest-path proto/Cargo.toml --offline --locked -j 2
cargo test --manifest-path state-store/Cargo.toml --offline --locked -j 2
cargo test --manifest-path server-files/Cargo.toml --offline --locked -j 2
cargo test --manifest-path core/Cargo.toml --offline --locked -j 2

echo "== companion suite e2e"
python3 tests/companion_suite_e2e.py --target-dir "${CARGO_TARGET_DIR:-$PWD/target/companion-e2e}"

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
cargo build --offline --locked -j 2 --message-format=json-render-diagnostics >"$build_manifest"
binary=$(python3 tools/cargo_executable.py <"$build_manifest")
timeout 180 python3 tests/wire_smoke.py "$binary"
timeout 180 python3 tests/protocol767.py "$binary"
timeout 180 python3 tests/alpha_release_smoke.py "$binary"
timeout 180 python3 tests/dimension_wire.py "$binary"
timeout 180 python3 tests/client_settings_wire.py "$binary"
timeout 180 python3 tests/gameplay_wire.py "$binary"
timeout 180 python3 tests/m2_gameplay_wire.py "$binary"
timeout 180 python3 tests/container_wire.py "$binary"
timeout 180 python3 tests/population_plan_test.py "$binary"
timeout 180 python3 tests/mineshaft_dispatch_test.py "$binary"
timeout 180 python3 tests/gp5_gp7_wire.py "$binary"
timeout 180 python3 tests/gp3_gp4_gp5_wire.py "$binary"
timeout 180 python3 tests/gp1_gp6_wire.py "$binary"
timeout 180 python3 tests/gp2_nether_spawn_wire.py "$binary"
timeout 180 python3 tests/command_brigadier_ops_wire.py "$binary"
timeout 180 python3 tests/command_datapack_wire.py
timeout 180 python3 tests/w1_w3_live_wire.py "$binary"
timeout 180 python3 tests/live_parity_e2e_wire.py "$binary"
timeout 180 python3 tools/vanilla_live_diff.py --megu-bin "$binary" --format text --strict
timeout 180 python3 tests/client_prediction_wire.py --binary "$binary"
timeout 180 python3 tests/client_inventory_pickup_wire.py --binary "$binary"
timeout 180 python3 tests/client_kernel_wire.py "$binary"
timeout 180 python3 tests/client_mesh_wire.py
timeout 180 python3 tests/client_vanilla_parity.py

echo "== adapter checks"
cargo check --offline --locked -j 2 --features fabric-probe
cargo check --offline --locked -j 2 --features neoforge-events
cargo check --offline --locked -j 2 --features create-offload
cargo test --offline --locked -j 2 --features create-offload -- --test-threads=1 aeronautics_and_dim_bridge_tests
cargo check --offline --locked -j 2 --features mod-suite
cargo test --offline --locked -j 2 --features mod-suite -- --test-threads=1 wire_mods_
cargo test --offline --locked -j 2 --features mod-suite -- --test-threads=1 live_mod_bridge

# Last, because it is the slowest: prove the gates can still fail. A suite that is green over a gate
# that cannot fail reports safety it does not have, and this is the only check that would notice.
# MUTATION_STABILITY=1 is the default here because a single red run cannot tell a catch from a
# load-sensitive flake (measured 2026-11-11: three entries were recorded as proved and re-ran as
# not a gate). It costs one extra run per caught mutant. The campaign copies the tree into a
# private snapshot and builds in a private target, so this stage no longer touches the working
# tree other stages may be reading.
if ((without_mutations)); then
  echo "== full mutation campaign (not run: --without-mutations)"
  echo "verify: PASS (full mutation campaign not run)"
else
  MUTATION_STABILITY=1 bash tools/mutation_check.sh --offline || exit 1
  echo "verify: PASS"
fi
