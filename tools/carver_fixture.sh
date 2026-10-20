#!/usr/bin/env bash
# Controlled carver fixture: generate a fresh keep-carvers world with a chosen
# set of configured_carver overrides, then diff mc-spine against it.
#
#   tools/carver_fixture.sh <tag> <cave-json> <extra-json> <canyon-json> [chunks]
#
# Each <*-json> is either a path to a configured_carver json (installed for both
# the vanilla world's datapack and the repo's, so both sides read the same file)
# or "zero" for a probability-0 stub or "-" to leave the file alone.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TAG="$1"; CAVE="$2"; EXTRA="$3"; CANYON="$4"; N="${5:-2}"
DIR="/tmp/carverfix-$TAG"
REPO_CFG="$ROOT/datapacks/vanilla/data/minecraft/worldgen/configured_carver"

rm -rf "$DIR"
mkdir -p "$DIR/world/datapacks/ctl/data/minecraft/worldgen/configured_carver"
cp /tmp/carverfresh/server.jar "$DIR/"
cp -r /tmp/carverfresh/libraries /tmp/carverfresh/versions "$DIR/"
echo "eula=true" > "$DIR/eula.txt"
sed 's/^level-name=.*/level-name=world/' /tmp/carverfresh/server.properties > "$DIR/server.properties"
python3 "$ROOT/tools/vanilla_nofeat_pack.py" "$DIR/server.jar" "$DIR/world" --keep-carvers >/dev/null
echo "{\"pack\": {\"pack_format\": 48, \"description\": \"$TAG\"}}" \
  > "$DIR/world/datapacks/ctl/pack.mcmeta"

zero() { cat <<'EOF'
{"type": "minecraft:cave", "config": {"probability": 0.0,
  "y": {"type": "minecraft:constant", "value": {"absolute": 0}},
  "yScale": {"type": "minecraft:constant", "value": 0.5},
  "lava_level": {"above_bottom": 8},
  "replaceable": "#minecraft:overworld_carver_replaceables",
  "floor_level": {"type": "minecraft:constant", "value": -1.0},
  "horizontal_radius_multiplier": {"type": "minecraft:constant", "value": 1.0},
  "vertical_radius_multiplier": {"type": "minecraft:constant", "value": 1.0}}}
EOF
}

install_cfg() {  # $1 = carver name, $2 = path|zero|-
  local name="$1" src="$2"
  local out="$DIR/world/datapacks/ctl/data/minecraft/worldgen/configured_carver/$name.json"
  case "$src" in
    -) return 0 ;;
    zero) zero > "$out" ;;
    *) cp "$src" "$out" ;;
  esac
}
install_cfg cave "$CAVE"
install_cfg cave_extra_underground "$EXTRA"
install_cfg canyon "$CANYON"

# the repo (mc-spine's data) must read the same files
for pair in "cave:$CAVE" "cave_extra_underground:$EXTRA" "canyon:$CANYON"; do
  name="${pair%%:*}"; src="${pair#*:}"
  case "$src" in
    -) ;;
    zero) zero > "$REPO_CFG/$name.json" ;;
    *) [ "$src" -ef "$REPO_CFG/$name.json" ] || cp "$src" "$REPO_CFG/$name.json" ;;
  esac
done

# force-load the chunk area and stop (run from the server dir: it reads its
# properties, eula and world relative to the working directory)
lo=$((-N*16)); hi=$((N*16-1))
( cd "$DIR" && printf 'forceload add %d %d %d %d\nsave-all flush\nstop\n' "$lo" "$lo" "$hi" "$hi" \
  | timeout 240 systemd-run --scope --user -p MemoryMax=3G --unit="cf-$TAG-$$" \
      java -Xmx2G -Xms512M -jar server.jar nogui > run.log 2>&1 || true )

cd "$ROOT"
MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP="$DIR/dump.bin" MC_SPINE_WG_DUMP_N="$N" \
  MC_SPINE_WG_DUMP_X=0 MC_SPINE_WG_DUMP_Z=0 MC_SPINE_NOFEAT=1 \
  ./target/release/mc-spine >/dev/null 2>&1
python3 tools/carver_diff.py "$DIR/dump.bin" "$DIR/world" --n "$N" --x0 0 --z0 0
cp /tmp/carver_backup/*.json "$REPO_CFG"/
