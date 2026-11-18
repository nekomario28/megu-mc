#!/usr/bin/env bash
# Regenerate every Minecraft 1.21.1 data file mc-spine reads, from the vanilla server jar:
#
#   assets/block_states_1211.json               per-state light, flags, solid faces and collision boxes
#   datapacks/vanilla/data/minecraft/           the game's own worldgen data (biome, configured_carver,
#                                               configured_feature, density_function, noise,
#                                               noise_settings, placed_feature, processor_list,
#                                               structure, structure_set, template_pool, world_preset,
#                                               flat_level_generator_preset,
#                                               multi_noise_biome_source_parameter_list),
#                                               tags/block and dimension_type - the data generator's
#                                               --server output
#   datapacks/vanilla/data/minecraft/structure/ the 1,180 structure templates, and the worldgen biome
#                                               and structure tags, taken from the jar's own members
#                                               (tools/vanilla_structure_assets.py, which records one
#                                               sha256 per member in structure-provenance.json)
#   datapacks/vanilla/multi_noise_biome_tree.json  the climate search tree src/worldgen.rs reads
#                                               (tools/biome_tree_from_jar.py)
#   datapacks/vanilla/pack.mcmeta
#   tests/data/biome_probe_1211.tsv             points looked up through the game's own tree
#   tests/data/vanilla_vectors_1211.tsv         random questions put to the game (xoroshiro draws,
#                                               climate noise, biome lookups, aquifer helpers) + its answers
#   tests/data/biome_probe_ties_1211.tsv        tie-heavy points (range edges) for the scan order
#   tests/data/random_next_int_1211.tsv         nextInt(bound) streams of the game's random classes
#   config_packets.bin                          the Configuration-phase packets a real 1.21.1 server
#                                               sends (registries, tags, ...), only with --accept-eula
#
#   tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/server.jar [--mappings DIR] \
#       [--fetch-fabric-mappings] [--accept-eula]
#   tools/vanilla_data/gen_vanilla_data.sh --verify [--pack DIR]
#       check an already-generated tree without the jar (inventory + member hashes)
#
# This branch tracks most of these outputs (see `.gitignore` and `THIRD_PARTY.md`), so a fresh
# clone does not need to run this; a tree that should not carry Mojang data does, once, and the
# checkout stays reproducible because the jar is pinned by sha1 below.
#
# --accept-eula: config_packets.bin is captured by starting the server jar once in a scratch
# directory (offline, loopback only, ~20 s), which needs Mojang's EULA (https://aka.ms/MinecraftEULA).
# Passing the flag says you accept it. Without it everything else is still generated, but the server
# cannot send clients their registries until you run this again with the flag.
#
# The jar is the official 1.21.1 *bundler* server.jar, sha1 below (the same build
# tools/fixture_base.sh pins). It is not downloaded or modified here: you supply it.
#
# The jar is obfuscated, so the extractor needs names. It uses Fabric's published intermediary and
# Yarn mappings for 1.21.1 (CC0, hosted on maven.fabricmc.net, not by Mojang), pinned by sha256.
# Put them in --mappings DIR, or pass --fetch-fabric-mappings to download exactly those two files.
#
# Every output is generated into a staging directory and swapped in only after
# tools/vanilla_data/verify_pack.py has counted the inventory and re-hashed every member, so a run
# that runs out of space, meets a different jar, or trips over a moved data-generator directory
# leaves the tree as it was instead of half-written.
#
# Needs Java 21 (java on PATH), python3, and ~1 GiB free in TMPDIR. About 20 s.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$ROOT/tools/vanilla_data"
JAR_SHA1="59353fb40c36d304f2035d51e7d6e6baa98dc05c"
INTER=intermediary-1.21.1-v2.jar
INTER_URL="https://maven.fabricmc.net/net/fabricmc/intermediary/1.21.1/$INTER"
INTER_SHA256=e8bd6a4c39e235ee2dc73063558610775c72a2572fa0c6b32915b149074012a6
YARN=yarn-1.21.1+build.3-v2.jar
YARN_URL="https://maven.fabricmc.net/net/fabricmc/yarn/1.21.1+build.3/$YARN"
YARN_SHA256=4ec570889585cb958b7d3c9dd94ce4064b16ab797dffec152bc8486c59a4010b

# The data generator's worldgen directories, as 1.21.1 writes them. A name that stops existing is a
# failure, not something to skip: the registry reads several of these by path.
WORLDGEN=(biome configured_carver configured_feature density_function flat_level_generator_preset
          multi_noise_biome_source_parameter_list noise noise_settings placed_feature processor_list
          structure structure_set template_pool world_preset)

JAR="${MC_SPINE_VANILLA_JAR:-}"
MAPS="${MC_SPINE_FABRIC_MAPPINGS:-$HOME/.cache/mc-spine-vanilla-data}"
DP="$ROOT/datapacks/vanilla"
STAGE="$ROOT/datapacks/vanilla.stage"
FETCH=0
EULA=0
VERIFY_ONLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --jar) JAR="$2"; shift 2 ;;
    --mappings) MAPS="$2"; shift 2 ;;
    --pack) DP="$2"; STAGE="$DP.stage"; shift 2 ;;
    --fetch-fabric-mappings) FETCH=1; shift ;;
    --accept-eula) EULA=1; shift ;;
    --verify) VERIFY_ONLY=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ "$VERIFY_ONLY" = 1 ]; then
  exec python3 "$HERE/verify_pack.py" --pack "$DP" --jar-sha1 "$JAR_SHA1"
fi

[ -n "$JAR" ] && [ -f "$JAR" ] || { echo "give the 1.21.1 bundler server.jar: --jar PATH" >&2; exit 2; }
got="$(sha1sum "$JAR" | cut -d' ' -f1)"
[ "$got" = "$JAR_SHA1" ] || { echo "jar sha1 is $got, expected $JAR_SHA1 (the 1.21.1 bundler server.jar)" >&2; exit 2; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK" "$STAGE"' EXIT

mkdir -p "$MAPS"
need() { # file url sha256
  if [ ! -f "$MAPS/$1" ]; then
    [ "$FETCH" = 1 ] || { echo "missing $MAPS/$1 (pass --fetch-fabric-mappings, or place it there; $2)" >&2; exit 2; }
    curl -sSfL -o "$MAPS/$1" "$2"
  fi
  echo "$3  $MAPS/$1" | sha256sum -c --quiet - || { echo "$1 does not match its pinned sha256" >&2; exit 2; }
}
need "$INTER" "$INTER_URL" "$INTER_SHA256"
need "$YARN" "$YARN_URL" "$YARN_SHA256"

mkdir -p "$WORK/run" "$WORK/map" "$WORK/out"
unzip -q -o "$MAPS/$INTER" mappings/mappings.tiny -d "$WORK/map/inter"
unzip -q -o "$MAPS/$YARN" mappings/mappings.tiny -d "$WORK/map/yarn"
cp "$JAR" "$WORK/run/server.jar"

# ---- the game's own answers about states, biomes and random draws ---------------------------
( cd "$WORK/run" && java -DbundlerMainClass=net.minecraft.data.Main -jar server.jar --reports --server --output gen >gen.log 2>&1 ) \
  || { cat "$WORK/run/gen.log" >&2; exit 1; }
CP="$(find "$WORK/run/libraries" -name '*.jar' | tr '\n' ' ')"
# shellcheck disable=SC2086
java "$HERE/Extract.java" "$WORK/out" "$WORK/map/inter/mappings/mappings.tiny" "$WORK/map/yarn/mappings/mappings.tiny" \
  "$WORK/run/versions/1.21.1/server-1.21.1.jar" $CP
# Writes assets/block_states_1211.json and the four tests/data/*.tsv files, and refuses to write
# anything unless every cross-check against the data generator's reports passes.
python3 "$HERE/build_vanilla_data.py" "$WORK/out" "$WORK/run/gen/reports" "$ROOT" "$JAR_SHA1"

# ---- the datapack, staged and verified -------------------------------------------------------
GEN="$WORK/run/gen/data/minecraft"
rm -rf "$STAGE"
mkdir -p "$STAGE/data/minecraft/worldgen" "$STAGE/data/minecraft/tags"
for d in "${WORLDGEN[@]}"; do
  [ -d "$GEN/worldgen/$d" ] || { echo "the data generator wrote no worldgen/$d; the jar is not the pinned 1.21.1 build" >&2; exit 1; }
  cp -r "$GEN/worldgen/$d" "$STAGE/data/minecraft/worldgen/$d"
done
cp -r "$GEN/tags/block" "$STAGE/data/minecraft/tags/block"
cp -r "$GEN/dimension_type" "$STAGE/data/minecraft/dimension_type"
# Templates and worldgen tags are jar members, not datagen output; that tool fails closed on a
# member count or an unexpected path, and records one sha256 per member.
python3 "$ROOT/tools/vanilla_structure_assets.py" --jar "$JAR" --output "$STAGE" --write
python3 "$ROOT/tools/biome_tree_from_jar.py" --jar "$JAR" --output "$STAGE/multi_noise_biome_tree.json"
printf '{"pack": {"pack_format": 48, "description": "vanilla worldgen data"}}\n' > "$STAGE/pack.mcmeta"
# Inventory and member hashes, then the swap: the tree is replaced only by a verified tree.
python3 "$HERE/verify_pack.py" --pack "$STAGE" --jar-sha1 "$JAR_SHA1"
rm -rf "$DP"
mv "$STAGE" "$DP"
echo "datapacks/vanilla: $(find "$DP/data" -name '*.json' | wc -l) json + $(find "$DP/data" -name '*.nbt' | wc -l) structure templates"

if [ "$EULA" = 1 ]; then
  python3 "$HERE/capture_config_packets.py" "$JAR" "$ROOT/config_packets.bin" --accept-eula
else
  echo "config_packets.bin not generated (needs --accept-eula, see the header of this script)" >&2
fi
