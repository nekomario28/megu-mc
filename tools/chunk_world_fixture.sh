#!/usr/bin/env bash
# Generate a pinned-vanilla saved region and diff mc-spine's generation dump.
#
#   tools/chunk_world_fixture.sh <tag> <cx> <cz> <n> <mode>
#
# mode:
#   terrain    no features, no carvers, no structures
#   carvers    no features, real carvers, no structures
#   features   real features/carvers, no structures
#   structures real features/carvers/structures
#   sculk_veins / sculk_one_patch
#              features, with a datapack that isolates one deep-dark step-7 feature
#              (`tools/vanilla_sculk_pack.py`; apply the same override to the port's pack)
#
# The reference is always seed 0 from tools/fixture_base.sh. Every chunk is
# force-loaded after the server is up; the reference must be complete before the
# diff, and the diff is written to /tmp/chunkfix-<tag>/diff.txt. This is the
# historical forest/R1 saved-world fixture: native ticks run before saving.
# For a true paused generation boundary use carve-agent/native_fixture.sh.
# A missing chunk or an empty region is an error, not a zero result.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TAG="$1"; CX="$2"; CZ="$3"; N="${4:-3}"; MODE="${5:-features}"
DIR="/tmp/chunkfix-$TAG"
BASE="${CARVER_FIXTURE_BASE:-/tmp/carverfresh}"
"$ROOT/tools/fixture_base.sh" "$BASE" >/dev/null
rm -rf "$DIR"; mkdir -p "$DIR/world"
cp "$BASE/server.jar" "$DIR/"
cp -r "$BASE/libraries" "$BASE/versions" "$DIR/"
sed 's/^level-name=.*/level-name=world/' "$BASE/server.properties" > "$DIR/server.properties"
# The base properties were chosen for a surface fixture. A deep-dark capture needs the
# chunks actually generated (a shrunken view distance or a low simulation distance makes
# the server save a chunk that was never decorated), and a fixed seed so the reference is
# reproducible: `SEED` defaults to 0, which is the seed every other fixture uses.
sed -i "s/^level-seed=.*/level-seed=${SEED:-0}/" "$DIR/server.properties"
sed -i 's/^view-distance=.*/view-distance=10/' "$DIR/server.properties"
sed -i 's/^simulation-distance=.*/simulation-distance=10/' "$DIR/server.properties"
echo "eula=true" > "$DIR/eula.txt"
case "$MODE" in
  terrain|carvers|features|sculk_veins|sculk_one_patch) echo 'generate-structures=false' >> "$DIR/server.properties" ;;
  structures) ;;
  *) echo "unknown mode $MODE" >&2; exit 2 ;;
esac
case "$MODE" in
  terrain) python3 "$ROOT/tools/vanilla_nofeat_pack.py" "$DIR/server.jar" "$DIR/world" >/dev/null ;;
  carvers) python3 "$ROOT/tools/vanilla_nofeat_pack.py" "$DIR/server.jar" "$DIR/world" --keep-carvers >/dev/null ;;
  # `sculk_vein` and `sculk_patch_deep_dark` share step 7 of the deep-dark feature list, so
  # a whole-region comparison cannot say which of the two moved. These two modes narrow the
  # reference to one of them; the caller has to apply the same override to the port's own
  # pack (`tools/vanilla_sculk_pack.py` prints the paths).
  sculk_veins) python3 "$ROOT/tools/vanilla_sculk_pack.py" "$DIR/server.jar" "$DIR/world" --variant veins ;;
  sculk_one_patch) python3 "$ROOT/tools/vanilla_sculk_pack.py" "$DIR/server.jar" "$DIR/world" --variant one-patch ;;
esac
lox=$((CX*16)); loz=$((CZ*16)); hix=$(((CX+N)*16-1)); hiz=$(((CZ+N)*16-1))
echo "chunk fixture $TAG: mode $MODE, chunks ($CX,$CZ)+${N}x${N}, blocks ($lox,$loz)-($hix,$hiz)"
( cd "$DIR"; ( deadline=$((SECONDS+90))
                 until grep -Fq 'Done (' run.log 2>/dev/null; do
                   if (( SECONDS >= deadline )); then
                     echo 'chunk fixture: server did not become ready' >&2
                     printf 'stop\n'; exit 2
                   fi
                   sleep 0.1
                 done
                 printf 'forceload add %d %d %d %d\n' "$lox" "$loz" "$hix" "$hiz"
                 sleep 75
                 printf 'save-all flush\n'
                 sleep 10
                 printf 'stop\n' ) \
  | timeout 360 systemd-run --scope --user -p MemoryMax=3G --unit="cw-$TAG-$$" \
      java -Xmx2G -Xms512M -jar server.jar nogui > run.log 2>&1 ) || true
if grep -qi 'unexpected error occurred' "$DIR/run.log"; then
  echo "chunk fixture: server command error (see $DIR/run.log)" >&2
  exit 2
fi
missing=0
for (( dx=0; dx<N; dx++ )); do
  for (( dz=0; dz<N; dz++ )); do
    if ! python3 "$ROOT/tools/vanilla_region_blocks.py" "$DIR/world" $((CX+dx)) $((CZ+dz)) >/dev/null 2>&1; then
      missing=$((missing+1))
    fi
  done
done
if [ "$missing" -gt 0 ]; then
  echo "chunk fixture: FAIL - $missing of $((N*N)) chunks are missing from the reference" >&2
  exit 2
fi
cd "$ROOT"
envs=(MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP="$DIR/dump.bin" MC_SPINE_WG_DUMP_N="$N"
      MC_SPINE_WG_DUMP_X="$CX" MC_SPINE_WG_DUMP_Z="$CZ" MC_SPINE_SEED=0)
case "$MODE" in
  terrain) envs+=(MC_SPINE_NOFEAT=1 MC_SPINE_NOCARVE=1) ;;
  carvers) envs+=(MC_SPINE_NOFEAT=1) ;;
esac
env "${envs[@]}" ./target/release/mc-spine >/dev/null 2>&1
python3 tools/carver_diff.py "$DIR/dump.bin" "$DIR/world" --n "$N" --x0 "$CX" --z0 "$CZ" --tick > "$DIR/diff.txt" || true
cat "$DIR/diff.txt"
echo "chunk fixture: reference/dump/diff under $DIR"
