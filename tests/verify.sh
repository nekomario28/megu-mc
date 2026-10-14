#!/usr/bin/env bash
# Everything that has to pass before a commit. This was six commands typed by hand
# each time, which is six chances to skip one.
set -euo pipefail
cd "$(dirname "$0")/.."
export PYTHONDONTWRITEBYTECODE=1

echo "== gate exit contracts"
python3 tests/gate_scripts.py
python3 tests/protocol767_test.py

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
  out=$(cargo test --offline --locked -j 2 -- --test-threads=8 2>&1)
  rc=$?
  echo "$out" | tail -2 | grep -E 'test result'
  if [ $rc -ne 0 ]; then
    echo "parallel suite failed on run $i (exit $rc)"
    echo "$out" | grep -A6 '^failures:' | head -20
    exit 1
  fi
done

echo "== wire smoke"
build_manifest=$(mktemp)
trap 'rm -f "$build_manifest"' EXIT
cargo build --offline --locked -j 2 --message-format=json-render-diagnostics >"$build_manifest"
binary=$(python3 tools/cargo_executable.py <"$build_manifest")
timeout 180 python3 tests/wire_smoke.py "$binary"
timeout 180 python3 tests/client_settings_wire.py "$binary"
timeout 180 python3 tests/gameplay_wire.py "$binary"

echo "== adapter checks"
cargo check --offline --locked -j 2 --features fabric-probe
cargo check --offline --locked -j 2 --features neoforge-events

# Last, because it is the slowest: prove the gates can still fail. A suite that is green over a gate
# that cannot fail reports safety it does not have, and this is the only check that would notice.
bash tools/mutation_check.sh --offline || exit 1

echo "verify: PASS"