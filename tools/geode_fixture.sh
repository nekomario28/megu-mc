#!/usr/bin/env bash
# Geode fixture: a vanilla reference that actually contains a geode.
#
#   tools/geode_fixture.sh [cx] [cz] [n] [seed]
#
# Why: `minecraft:geode` had no arm in `p_cf`, so every geode placement parsed as
# `Cf::Unknown` and placed nothing - silently, because `CF_MISSING` only fires for
# a *configured feature* that fails to parse. The port's own placement trace shows
# where one fires (seed 12, chunk (0,0): rarity draw 0.001475 < 1/24 and the biome
# filter passes at (5,-32,3)), so this fixture asks vanilla for the same region and
# compares the geode's blocks.
#
# Gates, in the order they can waste your time:
#   1. every chunk of the region is in the reference (a missing chunk is dropped
#      from the comparison without a word);
#   2. the reference holds at least one geode block, or the comparison is empty;
#   3. amethyst_block / budding_amethyst / calcite / smooth_basalt are compared
#      cell by cell against the port's dump of the same region.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CX="${1:-0}"; CZ="${2:-0}"; N="${3:-2}"; SEED="${4:-12}"
DIR="${GEODE_FIXTURE_DIR:-/tmp/geodefix-$CX-$CZ-$SEED}"
BASE="${CARVER_FIXTURE_BASE:-/tmp/carverfresh}"

"$ROOT/tools/fixture_base.sh" "$BASE" >/dev/null

rm -rf "$DIR"; mkdir -p "$DIR"
cp "$BASE/server.jar" "$DIR/"
cp -r "$BASE/libraries" "$BASE/versions" "$DIR/"
cp "$BASE/server.properties" "$DIR/server.properties"
echo "eula=true" > "$DIR/eula.txt"
# the fixture's seed has to be the one the region was chosen for
sed -i "s/^level-seed=.*/level-seed=$SEED/" "$DIR/server.properties"

lox=$((CX * 16)); loz=$((CZ * 16))
hix=$(((CX + N) * 16 - 1)); hiz=$(((CZ + N) * 16 - 1))
echo "geode fixture: seed $SEED, chunks ($CX,$CZ)+${N}x${N} = blocks ($lox,$loz)-($hix,$hiz)"
( cd "$DIR" && ( sleep 8
                 printf 'forceload add %d %d %d %d\n' "$lox" "$loz" "$hix" "$hiz"
                 sleep 60
                 printf 'save-all flush\n'
                 sleep 10
                 printf 'stop\n' ) \
  | timeout 300 systemd-run --scope --user -p MemoryMax=3G --unit="gf-$$" \
      java -Xmx2G -Xms512M -jar server.jar nogui > run.log 2>&1 || true )

if grep -q "Unexpected error occurred" "$DIR/run.log"; then
  echo "geode fixture: the server refused a command (see $DIR/run.log)" >&2
  grep -n "Unexpected error occurred" -B2 "$DIR/run.log" >&2 || true
  exit 2
fi

# A geode is a small cluster; count the cells that say one is there at all.
total=0
missing=0
for (( dx=0; dx<N; dx++ )); do
  for (( dz=0; dz<N; dz++ )); do
    chunk=$((CX+dx)),$((CZ+dz))
    if ! counts=$(python3 "$ROOT/tools/vanilla_region_blocks.py" "$DIR/world" $((CX+dx)) $((CZ+dz)) 2>/dev/null); then
      echo "geode fixture: chunk ($chunk) is not in the reference world" >&2
      missing=$((missing + 1))
      continue
    fi
    c=$(printf '%s\n' "$counts" | grep -E "minecraft:(amethyst_block|budding_amethyst|calcite|smooth_basalt)$" | awk '{s+=$1} END{print s+0}') || c=0
    total=$((total + c))
  done
done
if [ "$missing" -gt 0 ]; then
  echo "geode fixture: FAIL - $missing of $((N*N)) chunks were not generated; the reference is not complete" >&2
  exit 2
fi
echo "geode fixture: reference chunks all present ($((N*N))), geode cells in the region: $total"
if [ "$total" -eq 0 ]; then
  echo "geode fixture: FAIL - the reference region holds no geode; the comparison would be empty" >&2
  exit 2
fi

PORT_DUMP="$DIR/port.bin"
( cd "$ROOT" && MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP="$PORT_DUMP" \
    MC_SPINE_WG_DUMP_N="$N" MC_SPINE_WG_DUMP_X="$CX" MC_SPINE_WG_DUMP_Z="$CZ" \
    MC_SPINE_SEED="$SEED" ./target/release/mc-spine >/dev/null 2>&1 )

failed=0
for b in amethyst_block budding_amethyst calcite smooth_basalt; do
  out=$(python3 "$ROOT/tools/ore_diff.py" "$PORT_DUMP" "$DIR/world" "$b" \
          --n "$N" --x0 "$CX" --z0 "$CZ" 2>&1) || failed=1
  echo "$out"
done
echo "geode fixture: reference and dump under $DIR"
exit "$failed"
