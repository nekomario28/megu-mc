#!/usr/bin/env bash
# Isolate one ore feature and compare its cells cell-for-cell.
#
#   tools/ore_fixture.sh <tag> <keep> [chunks]
#
# `<keep>` is a placed_feature name (e.g. `ore_gravel`); every other `ore_*` and
# every `disk_*` placement is replaced with `count: 0`, in the vanilla world's
# control pack *and* in the repository's own datapacks, so both sides read the same
# files. Then only that one feature can place, and the two cell sets compare
# directly (the carver work's `CARVE_ONLY` in fixture form).
#
# Needs the generated `datapacks/vanilla` (tools/vanilla_data/gen_vanilla_data.sh). The control
# pack for this port is written to `datapacks/ctlfix` and removed on exit (`trap ... EXIT`).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TAG="$1"; KEEP="$2"; N="${3:-2}"
DIR="/tmp/orefix-$TAG"
BASE="${CARVER_FIXTURE_BASE:-/tmp/carverfresh}"
X0="${CARVER_FIXTURE_X0:-0}"; Z0="${CARVER_FIXTURE_Z0:-0}"

REPO_PF="$ROOT/datapacks/vanilla/data/minecraft/worldgen/placed_feature"
CTL_REL="datapacks/ctlfix/data/minecraft/worldgen/placed_feature"
CTL="$ROOT/$CTL_REL"

zero_all_but_keep() {  # $1 = target dir
  local out="$1"
  mkdir -p "$out"
  for f in "$REPO_PF"/ore_*.json "$REPO_PF"/disk_*.json; do
    local name; name="$(basename "$f")"
    [ "$name" = "$KEEP.json" ] && continue
    local feat
    feat="$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['feature'])" "$f")"
    printf '{"feature": "%s", "placement": [{"type": "minecraft:count", "count": 0}]}\n' \
      "$feat" > "$out/$name"
  done
}

restore_repo() {
  rm -rf "$CTL"
  rmdir -p "$CTL_REL" 2>/dev/null || true
}
trap restore_repo EXIT

rm -rf "$DIR"; mkdir -p "$DIR"
cp "$BASE/server.jar" "$DIR/"
cp -r "$BASE/libraries" "$BASE/versions" "$DIR/"
echo "eula=true" > "$DIR/eula.txt"
sed 's/^level-name=.*/level-name=world/' "$BASE/server.properties" > "$DIR/server.properties"

# the vanilla world's control pack
mkdir -p "$DIR/world/datapacks"
zero_all_but_keep "$DIR/world/datapacks/ctlfix/data/minecraft/worldgen/placed_feature"
echo "{\"pack\": {\"pack_format\": 48, \"description\": \"$TAG\"}}" \
  > "$DIR/world/datapacks/ctlfix/pack.mcmeta"

lox=$((X0*16)); loz=$((Z0*16)); hix=$(((X0+N)*16-1)); hiz=$(((Z0+N)*16-1))
( cd "$DIR" && ( sleep 8
                 printf 'forceload add %d %d %d %d\n' "$lox" "$loz" "$hix" "$hiz"
                 sleep 60
                 printf 'save-all flush\n'
                 sleep 10
                 printf 'stop\n' ) \
  | timeout 300 systemd-run --scope --user -p MemoryMax=3G --unit="of-$TAG-$$" \
      java -Xmx2G -Xms512M ${CARVE_AGENT:+-javaagent:$CARVE_AGENT} ${CARVE_AGENT_OPTS:-} \
      -jar server.jar nogui > run.log 2>&1 || true )
missing=0
for (( dx=0; dx<N; dx++ )); do
  for (( dz=0; dz<N; dz++ )); do
    python3 "$ROOT/tools/vanilla_region_blocks.py" "$DIR/world" $((X0+dx)) $((Z0+dz)) >/dev/null 2>&1 || missing=$((missing+1))
  done
done
if [ "$missing" -gt 0 ]; then
  echo "ore fixture: FAIL - $missing of $((N*N)) chunks are missing" >&2
  exit 2
fi

# the same pack for this port, which reads every directory under datapacks/
zero_all_but_keep "$CTL"

cd "$ROOT"
MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP="$DIR/dump.bin" MC_SPINE_WG_DUMP_N="$N" \
  MC_SPINE_WG_DUMP_X="$X0" MC_SPINE_WG_DUMP_Z="$Z0" \
  ./target/release/mc-spine >/dev/null 2>&1
# the placed feature's *block* (ore_gravel places gravel), read from its config
BLOCK="$(python3 - "$REPO_PF/$KEEP.json" <<'PY2'
import json, os, sys
placed = json.load(open(sys.argv[1]))
name = placed["feature"].split(":")[-1]
p = os.path.join(os.path.dirname(sys.argv[1]), "..", "configured_feature", name + ".json")
d = json.load(open(p))c = d.get("config", d)
print(c["targets"][0]["state"].get("Name", "minecraft:air").replace("minecraft:", ""))
PY2
)"
echo "isolated: $KEEP places $BLOCK"
python3 tools/ore_diff.py "$DIR/dump.bin" "$DIR/world" "$BLOCK" --n "$N" --x0 "$X0" --z0 "$Z0"
