#!/usr/bin/env bash
# Print the bytecode of one vanilla class, taken from the pinned server jar.
#
#   tools/bytecode_probe.sh net.minecraft.world.entity.projectile.AbstractArrow [pattern]
#   tools/bytecode_probe.sh net.minecraft.world.entity.projectile.ThrowableProjectile '0\.03'
#
# Why this exists: several evidence pages cite javap output by class and bytecode
# offset (for example `AbstractArrow.javap.txt:747`). Those files were a temporary
# extractor's output and are not in the repository, so the citation could not be
# checked by anyone reading the repository - an independent review said exactly
# that. This script is the extractor: it maps the official class name to the
# obfuscated one with Mojang's official mappings for the pinned version, pulls the
# class out of the nested server jar, and disassembles it.
#
# Pinned inputs:
#   server.jar          sha1   59353fb40c36d304f2035d51e7d6e6baa98dc05c
#                       sha256 e3bc55693e93cda0188f2e60aea28113fc647c5e85a15fa3d1b347349231b4bb
#   official mappings   server.txt 03f8985492bda0afc0898465341eb0acef35f570
#   (the manifest and mapping URLs that produced them are in
#    ~/.cache/megu-work/mc121/{url.txt,map_url.txt})
set -euo pipefail

CLASS="${1:-}"
PATTERN="${2:-}"
[ -n "$CLASS" ] || { sed -n '2,4p' "$0" >&2; exit 2; }

JAR="${MEGU_VANILLA_JAR:-/tmp/carverfresh/server.jar}"
MAPS="${MEGU_VANILLA_MAPPINGS:-$HOME/.cache/megu-work/mc121/server.txt}"
JAR_SHA1=59353fb40c36d304f2035d51e7d6e6baa98dc05c

command -v javap >/dev/null || { echo "bytecode probe: javap is required (a JDK, not just a JRE)" >&2; exit 2; }
[ -f "$JAR" ] || { echo "bytecode probe: no server jar at $JAR (MEGU_VANILLA_JAR, or tools/fixture_base.sh)" >&2; exit 2; }
[ -f "$MAPS" ] || { echo "bytecode probe: no official mappings at $MAPS (MEGU_VANILLA_MAPPINGS)" >&2; exit 2; }

got="$(sha1sum "$JAR" | cut -d' ' -f1)"
[ "$got" = "$JAR_SHA1" ] || {
  echo "bytecode probe: $JAR is $got, not the pinned $JAR_SHA1; a different jar is a different reference" >&2
  exit 2
}

# `net.minecraft.world.entity.projectile.AbstractArrow -> cnd:` in the official mappings
obf="$(awk -v want="$CLASS" '$1 == want && $2 == "->" { sub(":", "", $3); print $3; exit }' "$MAPS")"
[ -n "$obf" ] || { echo "bytecode probe: $CLASS is not in $MAPS" >&2; exit 1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
# the outer jar is the bundler; the classes live in the nested one
unzip -o -q "$JAR" 'META-INF/versions/*' -d "$tmp/inner"
inner="$(find "$tmp/inner" -name 'server-*.jar' | head -1)"
[ -n "$inner" ] || { echo "bytecode probe: $JAR has no nested server jar" >&2; exit 2; }
unzip -o -q "$inner" "$obf.class" -d "$tmp/srv"

echo "# $CLASS -> $obf.class (jar sha1 $got)" >&2
if [ -n "$PATTERN" ]; then
  javap -p -c "$tmp/srv/$obf.class" | grep -nE "$PATTERN"
else
  javap -p -c "$tmp/srv/$obf.class"
fi
