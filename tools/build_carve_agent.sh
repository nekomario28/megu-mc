#!/usr/bin/env bash
# Build the carve-agent jars (`carveagent.jar` + `carvelog.jar`).
#
# Why this is a script: the recipe lived in `tools/carve-agent/README.md` and was retyped by
# hand on every capture, which is where two silent failures came from in one session - a javac
# error piped through `head` (so the exit status was the pipe's), and a jar built from a
# half-applied source edit. Both produced a *stale* jar that still loaded and still printed
# traces, so the measurement looked fine and was not.
#
# Usage: tools/build_carve_agent.sh [output-dir]
#   default output-dir: ~/.cache/mc-spine-audit/carve-agent
# Prints the two jar paths on stdout (last two lines) and a SHA-256 of the agent jar.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="${1:-$HOME/.cache/mc-spine-audit/carve-agent}"
SRC=tools/carve-agent

# The ASM jar lives in the local Maven cache, or in a mod-loader's bundled libraries. Take the
# highest version found rather than a hard-coded one.
if [ -n "${ASM_JAR:-}" ]; then
  ASM="$ASM_JAR"
  if [ ! -f "$ASM" ]; then
    echo "build_carve_agent: ASM_JAR does not exist: $ASM" >&2
    exit 1
  fi
else
  ASM="$(ls ~/.m2/repository/org/ow2/asm/asm/*/asm-*.jar 2>/dev/null | sort -V | tail -1 || true)"
  if [ -z "$ASM" ]; then
    ASM="$(find "$HOME/Documents" -name 'asm-9*.jar' -path '*libraries*' 2>/dev/null | sort -V | tail -1 || true)"
  fi
  if [ -z "$ASM" ] || [ ! -f "$ASM" ]; then
    echo "build_carve_agent: no ASM jar found (set ASM_JAR or install in ~/.m2 or ~/Documents)" >&2
    exit 1
  fi
fi
# Preserve an explicit relative ASM_JAR even inside the temporary jar-build directories.
ASM="$(realpath "$ASM")"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/agent" "$WORK/logger" "$WORK/jar/META-INF" "$OUT"

# 1. the agent itself (ASM on the compile classpath)
javac -nowarn -cp "$ASM" -d "$WORK/agent" "$SRC/Agent.java"

# 2. the loggers: bootstrap-visible, no game types, so they compile with no classpath
javac -nowarn -d "$WORK/logger" "$SRC/CarveLog.java" "$SRC/SculkLog.java"

# 3. carvelog.jar = the loggers alone (appended to the bootstrap loader)
(cd "$WORK/logger" && jar cf "$WORK/carvelog.jar" .)

# 4. carveagent.jar = the agent + ASM + a Premain-Class manifest
cp -r "$WORK/agent/." "$WORK/jar/"
(cd "$WORK/agent" && jar xf "$ASM" org)
cp -r "$WORK/agent/org" "$WORK/jar/"
printf 'Premain-Class: Agent\n' > "$WORK/jar/META-INF/MANIFEST.MF"
(cd "$WORK/jar" && jar cfm "$WORK/carveagent.jar" META-INF/MANIFEST.MF .)

# 5. verify the jars actually carry what the recipe intends, before anything measures with them.
# The loggers are bootstrap-visible and live in carvelog.jar *only*; the agent jar carries the
# agent plus ASM.
#
# The listing is captured once into a file and matched with `grep -F`: `unzip -l | grep -q`
# under `pipefail` fails *intermittently*, because `grep -q` exits at the first match and unzip
# then dies of SIGPIPE (measured here: the same check failed for an entry that was in the
# listing). A gate that fails at random is worse than no gate.
unzip -l "$WORK/carveagent.jar" > "$WORK/agent.entries"
unzip -l "$WORK/carvelog.jar" > "$WORK/logger.entries"
fail() {
  echo "build_carve_agent: $1" >&2
  cat "${2:-$WORK/agent.entries}" >&2
  exit 1
}
for entry in Agent.class SculkTransformer.class org/objectweb/asm/ClassWriter.class; do
  grep -qF -- "$entry" "$WORK/agent.entries" || fail "carveagent.jar is missing $entry"
done
for entry in CarveLog.class SculkLog.class; do
  grep -qF -- "$entry" "$WORK/logger.entries" || fail "carvelog.jar is missing $entry" \
    "$WORK/logger.entries"
done
# The hooks are what the traces depend on. The *marker strings* live in the loggers
# (carvelog.jar, bootstrap-visible), while the *wiring* - which game class gets which hook - lives
# in the agent. Both halves are checked: a jar whose loggers stopped printing a marker the tools
# parse, or whose agent lost a class name, produces a trace that looks like a measurement and
# cannot be read.
markers_in_logger() {
  python3 - "$WORK/carvelog.jar" <<'PYCHECK'
import sys, zipfile
blob = b""
with zipfile.ZipFile(sys.argv[1]) as z:
    for name in z.namelist():
        if name.endswith(".class"):
            blob += z.read(name)
want = [b"SPATCH", b"SPLACE", b"SPASS", b"SDRAW", b"SCURSOR", b"SSPREADALL", b"SRNG",
        b"VSHAPE", b"EDGE", b"BPRED", b"HMP"]
missing = [w.decode() for w in want if w not in blob]
if missing:
    print("logger is missing markers: " + ", ".join(missing), file=sys.stderr)
    sys.exit(1)
PYCHECK
}
markers_in_logger || fail "carvelog.jar does not carry every trace marker"

wiring_in_agent() {
  python3 - "$WORK/carveagent.jar" <<'PYCHECK'
import sys, zipfile
blob = b""
with zipfile.ZipFile(sys.argv[1]) as z:
    for name in z.namelist():
        if name.endswith(".class") and "/" not in name:
            blob += z.read(name)
# The hooks the trace tooling depends on, by the game class each one instruments.
want = [b"SculkLog", b"ebq", b"edi", b"eiv", b"dyn", b"duz", b"dmu", b"dov", b"eij", b"eir"]
missing = [w.decode() for w in want if w not in blob]
if missing:
    print("agent does not wire hooks for: " + ", ".join(missing), file=sys.stderr)
    sys.exit(1)
PYCHECK
}
wiring_in_agent || fail "the compiled agent does not wire every hook the tools read"

# Marker membership cannot detect a hook that stops recording too early. Execute
# the transformed ConfiguredFeature return while its caller's window is still armed.
mkdir -p "$WORK/probe"
javac -nowarn -cp "$ASM:$WORK/agent:$WORK/logger" -d "$WORK/probe" "$SRC/AgentContractProbe.java"
java -Xmx128m -cp "$ASM:$WORK/agent:$WORK/logger:$WORK/probe" AgentContractProbe "$WORK/probe/trace.txt" >&2

cp "$WORK/carveagent.jar" "$WORK/carvelog.jar" "$OUT/"
cp "$SRC/Agent.java" "$SRC/CarveLog.java" "$SRC/SculkLog.java" "$OUT/" 2>/dev/null || true

echo "build_carve_agent: ASM $ASM" >&2
sha256sum "$OUT/carveagent.jar" >&2
echo "$OUT/carveagent.jar"
echo "$OUT/carvelog.jar"
