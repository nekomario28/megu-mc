#!/usr/bin/env bash
# Rebuild `datapacks/vanilla/data/minecraft/{worldgen,tags/block}` from the pinned
# 1.21.1 server jar.
#
#   tools/vanilla_pack_from_jar.sh [dir]     # default: datapacks/vanilla
#
# The repository's copy was not taken from the pinned build: its biomes use the newer
# `attributes` (mob-spawn) shape, its noises carry `base_amplitude`/`octave_count`
# instead of `amplitudes`/`firstOctave`, and its densities use `minecraft:gradient`
# with `axis`/`from_coordinate` where 1.21.1 has `minecraft:y_clamped_gradient` with
# `from_y`/`to_y`. Everything else (feature, placed_feature, carver, providers,
# material rules) was byte-identical, so the pack was a partial conversion - and a
# conversion of the *data the reference server actually generates from* is a
# divergence source that no fixture comparison can see.
#
# This script takes the worldgen tree and the block tags straight out of the pinned
# jar, so the data and the reference worlds come from the same build. It is the same
# subset the parser reads (`dp_tag_files("block")` and the worldgen loader).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${1:-$ROOT/datapacks/vanilla}"
JAR="${MC_SPINE_VANILLA_SERVER_JAR:-}"
JAR_SHA1="${MC_SPINE_VANILLA_JAR_SHA1:-59353fb40c36d304f2035d51e7d6e6baa98dc05c}"

DONORS=(
  "${MC_SPINE_VANILLA_SERVER:-}"
  "$HOME/.cache/mc-spine-audit/load/vanilla"
  "/data/lane-scratch/mc-vanilla-eval"
  "/data/lane-scratch/wg-oracle"
  "${CARVER_FIXTURE_BASE:-}"
)
if [ -z "$JAR" ]; then
  for d in "${DONORS[@]}"; do
    [ -n "$d" ] && [ -f "$d/server.jar" ] && JAR="$d/server.jar" && break
  done
fi
if [ -z "$JAR" ] || [ ! -f "$JAR" ]; then
  echo "vanilla pack: no pinned 1.21.1 server.jar found (looked in ${DONORS[*]})." >&2
  exit 1
fi
got="$(sha1sum "$JAR" | cut -d' ' -f1)"
if [ "$got" != "$JAR_SHA1" ]; then
  echo "vanilla pack: $JAR has sha1 $got, expected $JAR_SHA1." >&2
  echo "  The data must come from the same build the fixtures were measured against." >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
echo "vanilla pack: reading $JAR"
unzip -q "$JAR" 'META-INF/versions/*' -d "$work"
inner="$(find "$work/META-INF/versions" -name 'server-*.jar' | head -1)"
[ -n "$inner" ] || { echo "vanilla pack: no nested server jar in $JAR" >&2; exit 1; }
rm -rf "$work/data"
unzip -q "$inner" 'data/minecraft/worldgen/*' 'data/minecraft/tags/block/*' -d "$work"

files="$(find "$work/data" -type f | wc -l)"
echo "vanilla pack: $files files (worldgen + block tags)"
[ "$files" -gt 300 ] || { echo "vanilla pack: only $files files, refusing to install" >&2; exit 1; }

# every known pack needs the same worldgen layout; a missing one means the subset
# changed and the parser would silently see an empty tree
# The 1.21.1 layout is its own: `configured_feature` (not `feature`), no
# material_rule / material_condition / block_state_provider directories, and the
# structures/templates this port does not load. Require what the loader reads.
for d in biome configured_carver configured_feature density_function noise \
         noise_settings placed_feature; do
  if [ ! -d "$work/data/minecraft/worldgen/$d" ]; then
    echo "vanilla pack: worldgen/$d is missing from the jar" >&2
    exit 1
  fi
  n="$(find "$work/data/minecraft/worldgen/$d" -type f | wc -l)"
  [ "$n" -gt 0 ] || { echo "vanilla pack: worldgen/$d is empty" >&2; exit 1; }
done
if [ -d "$work/data/minecraft/worldgen/feature" ]; then
  echo "vanilla pack: unexpected worldgen/feature in a 1.21.1 jar" >&2
  exit 1
fi
n="$(find "$work/data/minecraft/tags/block" -type f | wc -l)"
[ "$n" -gt 100 ] || { echo "vanilla pack: only $n block tags" >&2; exit 1; }

# The climate preset JSON names a built-in; it does not contain the intervals or
# search tree. Export those from the same jar before touching the installed pack.
python3 "$ROOT/tools/biome_tree_from_jar.py" --jar "$JAR" --output "$work/multi_noise_biome_tree.json"

rm -rf "$DEST/data/minecraft/worldgen" "$DEST/data/minecraft/tags"
mkdir -p "$DEST/data/minecraft"
cp -r "$work/data/minecraft/worldgen" "$DEST/data/minecraft/worldgen"
mkdir -p "$DEST/data/minecraft/tags"
cp -r "$work/data/minecraft/tags/block" "$DEST/data/minecraft/tags/block"
cp "$work/multi_noise_biome_tree.json" "$DEST/multi_noise_biome_tree.json"
python3 "$ROOT/tools/vanilla_structure_assets.py" --jar "$JAR" --output "$DEST" --write
echo "vanilla pack: installed into $DEST"
