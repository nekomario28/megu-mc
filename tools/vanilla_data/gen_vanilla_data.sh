#!/usr/bin/env bash
# Regenerate the Minecraft 1.21.1 data mc-spine needs from the vanilla server jar:
#
#   assets/block_states_1211.json               per-state light, flags and solid faces
#   datapacks/vanilla/biome_climate_tree.json   the game's overworld and nether biome search trees
#   tests/data/biome_probe_1211.tsv             points looked up through the game's own tree
#
#   tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/server.jar [--mappings DIR] [--fetch-fabric-mappings]
#
# The jar is the official 1.21.1 *bundler* server.jar, sha1 below (the same build
# tools/fixture_base.sh pins). It is not downloaded or modified here: you supply it.
#
# The jar is obfuscated, so the extractor needs names. It uses Fabric's published intermediary and
# Yarn mappings for 1.21.1 (CC0, hosted on maven.fabricmc.net, not by Mojang), pinned by sha256.
# Put them in --mappings DIR, or pass --fetch-fabric-mappings to download exactly those two files.
#
# Needs Java 21 (java on PATH) and python3. About 15 s.
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

JAR="${MC_SPINE_VANILLA_JAR:-}"
MAPS="${MC_SPINE_FABRIC_MAPPINGS:-$HOME/.cache/mc-spine-vanilla-data}"
FETCH=0
while [ $# -gt 0 ]; do
  case "$1" in
    --jar) JAR="$2"; shift 2 ;;
    --mappings) MAPS="$2"; shift 2 ;;
    --fetch-fabric-mappings) FETCH=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$JAR" ] && [ -f "$JAR" ] || { echo "give the 1.21.1 bundler server.jar: --jar PATH" >&2; exit 2; }
got="$(sha1sum "$JAR" | cut -d' ' -f1)"
[ "$got" = "$JAR_SHA1" ] || { echo "jar sha1 is $got, expected $JAR_SHA1 (the 1.21.1 bundler server.jar)" >&2; exit 2; }

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

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/run" "$WORK/map" "$WORK/out"
unzip -q -o "$MAPS/$INTER" mappings/mappings.tiny -d "$WORK/map/inter"
unzip -q -o "$MAPS/$YARN" mappings/mappings.tiny -d "$WORK/map/yarn"
cp "$JAR" "$WORK/run/server.jar"
( cd "$WORK/run" && java -DbundlerMainClass=net.minecraft.data.Main -jar server.jar --reports --server --output gen >gen.log 2>&1 ) \
  || { cat "$WORK/run/gen.log" >&2; exit 1; }
CP="$(find "$WORK/run/libraries" -name '*.jar' | tr '\n' ' ')"
# shellcheck disable=SC2086
java "$HERE/Extract.java" "$WORK/out" "$WORK/map/inter/mappings/mappings.tiny" "$WORK/map/yarn/mappings/mappings.tiny" \
  "$WORK/run/versions/1.21.1/server-1.21.1.jar" $CP
python3 "$HERE/build_vanilla_data.py" "$WORK/out" "$WORK/run/gen/reports" "$ROOT" "$JAR_SHA1"
