#!/usr/bin/env bash
# Generate a pinned-vanilla chunk region and diff mc-spine's same-stage dump.
#
#   tools/chunk_world_fixture.sh <tag> <cx> <cz> <n> <mode>
#
# mode:
#   terrain    no features, no carvers, no structures
#   carvers    no features, real carvers, no structures
#   features   real features/carvers, no structures
#   structures real features/carvers/structures
#
# The reference is always seed 0 from tools/fixture_base.sh. Every chunk is
# force-loaded after the server is up; the reference must be complete before the
# diff, and the diff is written to /tmp/chunkfix-<tag>/diff.txt. This is the
# same-stage forest/R1 fixture; a missing chunk or an empty region is an error,
# not a zero result.
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
echo "eula=true" > "$DIR/eula.txt"
case "$MODE" in
  terrain|carvers|features) echo 'generate-structures=false' >> "$DIR/server.properties" ;;
  structures) ;;
  *) echo "unknown mode $MODE" >&2; exit 2 ;;
esac
case "$MODE" in
  terrain) python3 "$ROOT/tools/vanilla_nofeat_pack.py" "$DIR/server.jar" "$DIR/world" >/dev/null ;;
  carvers) python3 "$ROOT/tools/vanilla_nofeat_pack.py" "$DIR/server.jar" "$DIR/world" --keep-carvers >/dev/null ;;
esac
lox=$((CX*16)); loz=$((CZ*16)); hix=$(((CX+N)*16-1)); hiz=$(((CZ+N)*16-1))
echo "chunk fixture $TAG: mode $MODE, chunks ($CX,$CZ)+${N}x${N}, blocks ($lox,$loz)-($hix,$hiz)"
( cd "$DIR"; ( sleep 8
                 printf 'forceload add %d %d %d %d\n' "$lox" "$loz" "$hix" "$hiz"
                 sleep 75
                 printf 'save-all flush\n'
                 sleep 10
                 printf 'stop\n' ) \
  | timeout 360 systemd-run --scope --user -p MemoryMax=3G --unit="cw-$TAG-$$" \
      java -Xmx2G -Xms512M -jar server.jar nogui > run.log 2>&1 ) || true
if grep -q 'Unexpected error occurred' "$DIR/run.log"; then
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
