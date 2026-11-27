#!/usr/bin/env bash
# Clippy warning ceiling for the root package and every standalone crate.
#
# Until 2026-11-27 this ratchet ran `cargo clippy` in the root only, so the
# thirteen path-dependency crates (core, proto, client-*, server-files,
# state-store, mod-*, create-adapter, wasm-mod-runtime) could accumulate
# warnings with no gate ever seeing them. Every top-level directory carrying
# a Cargo.toml is now linted with --all-targets --locked under the pinned
# rust-toolchain, and each crate has its own ceiling in
# tools/clippy-warning-baseline.txt. A crate that appears or disappears
# without a baseline edit fails closed.
#
# Indexed arrays, not `declare -A`: macOS still ships bash 3.2.
set -euo pipefail
cd "$(dirname "$0")/.."

baseline_file=tools/clippy-warning-baseline.txt
if [[ ! -f "$baseline_file" ]]; then
  echo "missing $baseline_file" >&2
  exit 2
fi

# Discovered crate set: the root package plus every top-level crate dir.
crates=()
manifests=()
if [[ -f Cargo.toml ]]; then
  crates+=(".")
  manifests+=("Cargo.toml")
fi
for manifest in */Cargo.toml; do
  [[ -f "$manifest" ]] || continue
  crates+=("$(dirname "$manifest")")
  manifests+=("$manifest")
done
if [[ ${#crates[@]} -eq 0 ]]; then
  echo "no Cargo.toml found; the ratchet would measure nothing" >&2
  exit 2
fi

# Baseline: "<crate> <count>" lines, crate "." is the root package. The name
# set must match the discovered crates exactly — a new crate without a
# ceiling, or a stale ceiling for a removed crate, is an error, not a skip.
base_names=()
base_counts=()
while IFS= read -r raw || [[ -n "$raw" ]]; do
  line="${raw%%#*}"
  line="${line#"${line%%[![:space:]]*}"}"
  line="${line%"${line##*[![:space:]]}"}"
  [[ -z "$line" ]] && continue
  name="${line%%[[:space:]]*}"
  rest="${line#"$name"}"
  rest="${rest#"${rest%%[![:space:]]*}"}"
  count="${rest%%[[:space:]]*}"
  extra="${rest#"$count"}"
  extra="${extra#"${extra%%[![:space:]]*}"}"
  if [[ -z "$name" || -z "$count" || -n "$extra" || ! "$count" =~ ^[0-9]+$ ]]; then
    echo "malformed baseline line in $baseline_file: '$raw'" >&2
    exit 2
  fi
  # 10# keeps a leading-zero ceiling decimal; bare (( 08 )) is an octal error.
  count=$((10#$count))
  for existing in "${base_names[@]}"; do
    if [[ "$existing" == "$name" ]]; then
      echo "duplicate baseline entry for crate '$name'" >&2
      exit 2
    fi
  done
  base_names+=("$name")
  base_counts+=("$count")
done < "$baseline_file"

baseline_for() {
  local want="$1" i
  for i in "${!base_names[@]}"; do
    if [[ "${base_names[$i]}" == "$want" ]]; then
      printf '%s\n' "${base_counts[$i]}"
      return 0
    fi
  done
  return 1
}

for name in "${crates[@]}"; do
  if ! baseline_for "$name" >/dev/null; then
    echo "crate '$name' has no ceiling in $baseline_file" >&2
    exit 2
  fi
done
for name in "${base_names[@]}"; do
  known=0
  for crate in "${crates[@]}"; do
    if [[ "$crate" == "$name" ]]; then
      known=1
      break
    fi
  done
  if [[ $known -eq 0 ]]; then
    echo "baseline entry '$name' matches no crate; remove it or restore the crate" >&2
    exit 2
  fi
done

failed=0
for i in "${!crates[@]}"; do
  name="${crates[$i]}"
  manifest="${manifests[$i]}"
  limit="$(baseline_for "$name")"
  output=$(mktemp)
  clean=$(mktemp)
  if ! CARGO_TERM_COLOR=never cargo clippy --manifest-path "$manifest" --all-targets --locked 2>&1 | tee "$output"; then
    echo "clippy ratchet: $name: cargo clippy failed" >&2
    rm -f "$output" "$clean"
    exit 1
  fi
  sed $'s/\x1b\\[[0-9;]*m//g' "$output" > "$clean"
  # Count diagnostics only: cargo also prints a "warning: `crate` (target)
  # generated N warning(s)" recap per target, which would inflate the count
  # whenever a crate gains a bin/test/example target.
  warnings=$(grep '^warning:' "$clean" | grep -cv 'generated [0-9][0-9]* warnings\?$' || true)
  rm -f "$output" "$clean"
  if (( warnings > limit )); then
    echo "clippy ratchet: $name: $warnings warnings, baseline $limit" >&2
    failed=1
  else
    echo "clippy ratchet: $name: $warnings warnings, baseline $limit"
    if (( warnings < limit )); then
      echo "  lower the '$name' ceiling in $baseline_file to lock in the reduction"
    fi
  fi
done
exit "$failed"
