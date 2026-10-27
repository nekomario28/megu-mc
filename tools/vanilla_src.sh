#!/usr/bin/env bash
# Decompile vanilla classes from the pinned 1.21.1 server jar, for reading.
#
#   tools/vanilla_src.sh net.minecraft.world.level.levelgen.feature.PointedDripstoneFeature
#   tools/vanilla_src.sh net.minecraft.world.level.levelgen.feature.HugeRedMushroomFeature \
#                        net.minecraft.world.level.levelgen.feature.AbstractHugeMushroomFeature
#
# Why this exists: `tools/bytecode_probe.sh` answers "what does the JVM do", which is the
# authority for draw counts and short circuits, but reading control flow out of javap costs
# about ten minutes per class and loses the conditions (the red mushroom's skip rule — write
# when `dy >= height || dxEdge != dzEdge` — is one line of Java and a `if_icmpne` puzzle in
# bytecode). This script is the cheap half of the same reading: it maps the official class
# name through Mojang's official mappings to the obfuscated one, pulls the class *and its
# nested classes* out of the nested server jar, and decompiles them with the Vineflower build
# already on this host.
#
# The output is Mojang's code. It stays outside the repository on purpose: it is a reading aid
# for the port, not redistributable source. The script refuses any output directory inside the
# repository, and the jar is checked against its pinned SHA-1 before anything is extracted.
#
# Pinned inputs (same chain as bytecode_probe.sh):
#   server.jar          sha1   59353fb40c36d304f2035d51e7d6e6baa98dc05c
#   official mappings   server.txt 03f8985492bda0afc0898465341eb0acef35f570
# Overridable with MC_SPINE_VANILLA_JAR, MC_SPINE_VANILLA_MAPPINGS, MC_SPINE_VINEFLOWER and
# MC_SPINE_VANILLA_SRC (the output directory, default ~/.cache/mc-spine-work/mc121/src).
set -euo pipefail

JAR="${MC_SPINE_VANILLA_JAR:-$HOME/.cache/mc-spine-audit/load/vanilla/server.jar}"
MAPS="${MC_SPINE_VANILLA_MAPPINGS:-$HOME/.cache/mc-spine-work/mc121/server.txt}"
VINE="${MC_SPINE_VINEFLOWER:-$HOME/.cache/mc-spine-work/mc121/vineflower-1.11.1.jar}"
OUT="${MC_SPINE_VANILLA_SRC:-$HOME/.cache/mc-spine-work/mc121/src}"
JAR_SHA1=59353fb40c36d304f2035d51e7d6e6baa98dc05c
MAPS_SHA1=03f8985492bda0afc0898465341eb0acef35f570
REPO="$(cd "$(dirname "$0")/.." && pwd)"

[ "$#" -gt 0 ] || { sed -n '2,6p' "$0" >&2; exit 2; }

command -v java >/dev/null || { echo "vanilla_src: java is required" >&2; exit 2; }
command -v unzip >/dev/null || { echo "vanilla_src: unzip is required" >&2; exit 2; }
for f in "$JAR" "$MAPS" "$VINE"; do
  [ -f "$f" ] || { echo "vanilla_src: missing $f" >&2; exit 2; }
done

# a different jar is a different reference, and a different mappings file renames classes
[ "$(sha1sum "$JAR" | cut -d' ' -f1)" = "$JAR_SHA1" ] || {
  echo "vanilla_src: $JAR is not the pinned 1.21.1 server jar ($JAR_SHA1)" >&2; exit 2; }
[ "$(sha1sum "$MAPS" | cut -d' ' -f1)" = "$MAPS_SHA1" ] || {
  echo "vanilla_src: $MAPS is not the pinned official mappings ($MAPS_SHA1)" >&2; exit 2; }

case "$(realpath -m "$OUT")" in
  "$REPO"|"$REPO"/*)
    echo "vanilla_src: refusing to write decompiled Mojang code inside the repository ($OUT)" >&2
    echo "vanilla_src: set MC_SPINE_VANILLA_SRC to a directory outside $REPO" >&2
    exit 2;;
esac
mkdir -p "$OUT"

# One extraction of the nested jar, reused while the outer jar's SHA-1 matches the stamp.
inner_dir="$OUT/.inner"
stamp="$inner_dir/.jar-sha1"
if [ "$(cat "$stamp" 2>/dev/null || true)" != "$JAR_SHA1" ]; then
  rm -rf "$inner_dir"; mkdir -p "$inner_dir"
  unzip -o -q "$JAR" 'META-INF/versions/*' -d "$inner_dir/outer"
  inner="$(find "$inner_dir/outer" -name 'server-*.jar' | head -1)"
  [ -n "$inner" ] || { echo "vanilla_src: $JAR has no nested server jar" >&2; exit 2; }
  cp "$inner" "$inner_dir/server.jar"
  rm -rf "$inner_dir/outer"
  printf '%s\n' "$JAR_SHA1" >"$stamp"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
classes="$work/classes"
mkdir -p "$classes"

# Resolve official -> obfuscated (`net.minecraft.core.BlockPos -> jd:`) and pull each class
# together with its nested classes (`jd$a.class`), because a decompiled outer class is
# half-readable when its record/mutable companions are missing.
for cls in "$@"; do
  obf="$(awk -v want="$cls" '$1 == want && $2 == "->" { sub(":", "", $3); print $3; exit }' "$MAPS")"
  [ -n "$obf" ] || { echo "vanilla_src: $cls is not in $MAPS" >&2; exit 1; }
  unzip -o -q "$inner_dir/server.jar" "$obf.class" -d "$classes"
  [ -f "$classes/$obf.class" ] || { echo "vanilla_src: no $obf.class in the jar for $cls" >&2; exit 1; }
  # nested classes are optional: an empty glob is `unzip`'s "filename not matched", not an error
  unzip -o -q "$inner_dir/server.jar" "$obf\$*.class" -d "$classes" 2>/dev/null || true
  printf '%s -> %s\n' "$cls" "$obf" >&2
  printf '%s/%s.java\n' "$OUT" "$obf"
done

java -jar "$VINE" "$classes" "$OUT" >/dev/null 2>&1 || {
  echo "vanilla_src: vineflower failed (run it without redirection to see why)" >&2; exit 1; }

