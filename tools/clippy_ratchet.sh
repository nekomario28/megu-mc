#!/usr/bin/env bash
# Root-crate Clippy warning ceiling, measured with the pinned rust-toolchain.toml.
set -euo pipefail
cd "$(dirname "$0")/.."

baseline_file=tools/clippy-warning-baseline.txt
if [[ ! -f "$baseline_file" ]]; then
  echo "missing $baseline_file" >&2
  exit 2
fi
baseline=$(sed -n '/^[0-9][0-9]*$/p' "$baseline_file")
if [[ ! "$baseline" =~ ^[0-9]+$ ]]; then
  echo "expected one integer warning baseline in $baseline_file" >&2
  exit 2
fi

output=$(mktemp)
clean=$(mktemp)
trap 'rm -f "$output" "$clean"' EXIT
if ! CARGO_TERM_COLOR=never cargo clippy --all-targets --locked 2>&1 | tee "$output"; then
  echo "root cargo clippy failed" >&2
  exit 1
fi
sed $'s/\x1b\\[[0-9;]*m//g' "$output" > "$clean"
warnings=$(grep -c '^warning:' "$clean" || true)
if (( warnings > baseline )); then
  echo "clippy ratchet: $warnings warnings, baseline $baseline" >&2
  exit 1
fi
echo "clippy ratchet: $warnings warnings, baseline $baseline"
if (( warnings < baseline )); then
  echo "lower tools/clippy-warning-baseline.txt to lock in the reduction"
fi
