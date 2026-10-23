#!/usr/bin/env bash
# Ocean feature fixture: a vanilla reference that actually contains kelp.
#
#   tools/kelp_fixture.sh <cx> <cz> [n] [seed]
#
# Why: the roadmap's first feature-layer gate needs a reference with *positive*
# kelp cells. Every earlier fixture had zero on both sides, so "kelp is
# implemented" rested on a synthetic water column rather than on vanilla. This
# one asks for it:
#
#   1. the region is force-loaded far from spawn, so the reference chunks are not
#      a spawn-preparation leftover and the decoration order is not the one the
#      recorded 2x2 fixtures had;
#   2. `forceload` is sent *after* the server is up. Piped in immediately it is
#      consumed without effect (measured: the same command generated nothing,
#      while the same command eight seconds later generated the region);
#   3. the reference must contain kelp or the script fails - a fixture that
#      compares nothing is not a pass;
#   4. the port's dump of the same region is diffed per block, and the shared /
#      ours-only / vanilla-only split is printed.
#
# The default region is the lukewarm ocean at chunk (79,-1) that seed 0 holds
# (measured: 57 kelp_plant cells in chunk (81,0), biome lukewarm_ocean blended
# and container).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CX="${1:-79}"; CZ="${2:--1}"; N="${3:-6}"; SEED="${4:-0}"
DIR="${KELP_FIXTURE_DIR:-/tmp/kelpfix-$CX-$CZ}"
BASE="${CARVER_FIXTURE_BASE:-/tmp/carverfresh}"

"$ROOT/tools/fixture_base.sh" "$BASE" >/dev/null

rm -rf "$DIR"; mkdir -p "$DIR"
cp "$BASE/server.jar" "$DIR/"
cp -r "$BASE/libraries" "$BASE/versions" "$DIR/"
cp "$BASE/server.properties" "$DIR/server.properties"
echo "eula=true" > "$DIR/eula.txt"

lox=$((CX * 16)); loz=$((CZ * 16))
hix=$(((CX + N) * 16 - 1)); hiz=$(((CZ + N) * 16 - 1))
echo "kelp fixture: seed $SEED, chunks ($CX,$CZ)+${N}x${N} = blocks ($lox,$loz)-($hix,$hiz)"
( cd "$DIR" && ( sleep 8
                 printf 'forceload add %d %d %d %d\n' "$lox" "$loz" "$hix" "$hiz"
                 sleep 60
                 printf 'save-all flush\n'
                 sleep 10
                 printf 'stop\n' ) \
  | timeout 300 systemd-run --scope --user -p MemoryMax=3G --unit="kf-$$" \
      java -Xmx2G -Xms512M -jar server.jar nogui > run.log 2>&1 || true )

if grep -q "Unexpected error occurred" "$DIR/run.log"; then
  echo "kelp fixture: the server refused a command (see $DIR/run.log)" >&2
  grep -n "Unexpected error occurred" -B2 "$DIR/run.log" >&2 || true
  exit 2
fi

# Two gates, because a fixture can be empty in two ways: a chunk that was never
# generated (the comparison silently drops it) and a region with no kelp at all
# (the comparison compares nothing).
total=0
missing=0
for (( dx=0; dx<N; dx++ )); do
  for (( dz=0; dz<N; dz++ )); do
    chunk=$((CX+dx)),$((CZ+dz))
    if ! counts=$(python3 "$ROOT/tools/vanilla_region_blocks.py" "$DIR/world" $((CX+dx)) $((CZ+dz)) 2>/dev/null); then
      echo "kelp fixture: chunk ($chunk) is not in the reference world" >&2
      missing=$((missing + 1))
      continue
    fi
    # a chunk with no kelp makes grep exit non-zero; that is zero cells, not a
    # reason to abort under `set -e`/`pipefail`
    c=$(printf '%s\n' "$counts" | grep -E "minecraft:kelp(_plant)?$" | awk '{s+=$1} END{print s+0}') || c=0
    total=$((total + c))
  done
done
if [ "$missing" -gt 0 ]; then
  echo "kelp fixture: FAIL - $missing of $((N*N)) chunks were not generated; the reference is not complete" >&2
  exit 2
fi
echo "kelp fixture: reference chunks all present ($((N*N))), kelp cells in the region: $total"
if [ "$total" -eq 0 ]; then
  echo "kelp fixture: FAIL - the reference region holds no kelp; the comparison would be empty" >&2
  exit 2
fi

PORT_DUMP="$DIR/port.bin"
( cd "$ROOT" && MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP="$PORT_DUMP" \
    MC_SPINE_WG_DUMP_N="$N" MC_SPINE_WG_DUMP_X="$CX" MC_SPINE_WG_DUMP_Z="$CZ" \
    MC_SPINE_SEED="$SEED" ./target/release/mc-spine >/dev/null 2>&1 )

failed=0
for b in kelp kelp_plant; do
  out=$(python3 "$ROOT/tools/ore_diff.py" "$PORT_DUMP" "$DIR/world" "$b" \
          --n "$N" --x0 "$CX" --z0 "$CZ" 2>&1) || failed=1
  echo "$out"
done
echo "kelp fixture: reference and dump under $DIR"
exit "$failed"
