#!/usr/bin/env bash
# Everything that has to pass before a commit. This was six commands typed by hand
# each time, which is six chances to skip one.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== tests, serial"
cargo test --offline --locked -j 2 -- --test-threads=1

echo "== tests, eight threads, three times"
for i in 1 2 3; do
  cargo test --offline --locked -j 2 -- --test-threads=8 | tail -2 | grep -E 'test result'
done

echo "== wire smoke"
cargo build --offline --locked -j 2
timeout 180 python3 tests/wire_smoke.py target/debug/mc-spine

echo "== adapter checks"
cargo check --offline --locked -j 2 --features fabric-probe
cargo check --offline --locked -j 2 --features neoforge-events

echo "verify: PASS"
