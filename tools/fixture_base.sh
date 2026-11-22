#!/usr/bin/env bash
# Assemble the vanilla 1.21.1 server tree the fixtures start from.
#
#   tools/fixture_base.sh [dir]        # default: $CARVER_FIXTURE_BASE or /tmp/carverfresh
#
# `carver_fixture.sh` and `ore_fixture.sh` read `$CARVER_FIXTURE_BASE` and expect
# an installed server there (server.jar, libraries/, versions/, server.properties).
# Nothing in the repository created it, so a missing base made a fixture compare
# against nothing, and the missing base only showed up as a strange diff.
#
# The jar is pinned by sha1: it is the build the recorded fixtures were measured
# against, and a different build is a different reference. The donor directory is
# only a carrier of that jar plus the libraries it extracted; the settings that
# affect world generation are written here, one by one, so the fixture conditions
# are visible rather than inherited.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="${1:-${CARVER_FIXTURE_BASE:-/tmp/carverfresh}}"
JAR_SHA1="${MEGU_VANILLA_JAR_SHA1:-59353fb40c36d304f2035d51e7d6e6baa98dc05c}"
SEED="${MEGU_FIXTURE_SEED:-0}"

DONORS=(
  "${MEGU_VANILLA_SERVER:-}"
  "$HOME/.cache/megu-audit/load/vanilla"
  "/data/lane-scratch/mc-vanilla-eval"
  "/data/lane-scratch/wg-oracle"
)
donor=""
for d in "${DONORS[@]}"; do
  [ -n "$d" ] || continue
  if [ -f "$d/server.jar" ] && [ -d "$d/libraries" ] && [ -d "$d/versions" ]; then donor="$d"; break; fi
done
if [ -z "$donor" ]; then
  echo "fixture base: no installed 1.21.1 server found." >&2
  echo "  Looked in: ${DONORS[*]}" >&2
  echo "  Point MEGU_VANILLA_SERVER at a directory holding server.jar, libraries/ and versions/." >&2
  echo "  The jar must be sha1 $JAR_SHA1." >&2
  exit 2
fi

got="$(sha1sum "$donor/server.jar" | cut -d' ' -f1)"
if [ "$got" != "$JAR_SHA1" ]; then
  echo "fixture base: $donor/server.jar is $got, not the pinned $JAR_SHA1." >&2
  echo "  A different build is a different reference; refusing to build the base from it." >&2
  exit 2
fi

mkdir -p "$DIR"
cp -f "$donor/server.jar" "$DIR/server.jar"
for sub in libraries versions; do
  # the extracted libraries are the same for the pinned build; copy once
  [ -e "$DIR/$sub" ] || cp -r "$donor/$sub" "$DIR/$sub"
done
cat > "$DIR/server.properties" <<EOF
# written by tools/fixture_base.sh — the world-generation settings a fixture compares under
level-name=world
level-seed=$SEED
level-type=minecraft\\:normal
generator-settings={}
spawn-protection=0
online-mode=false
enable-rcon=false
enable-query=false
EOF
echo "eula=true" > "$DIR/eula.txt"

echo "fixture base: $DIR"
echo "  server.jar sha1 $got"
echo "  seed $SEED, level-type minecraft:normal, spawn-protection 0"
echo "  next: tools/carver_fixture.sh <tag> - - - 2    # or tools/ore_fixture.sh <tag> <keep>"
