#!/usr/bin/env bash
# Compare an idle server against vanilla under the conditions the goal names: the same
# seed, the same world type, the same view and simulation distance, and no players.
#
# Both generate real terrain from seeds 0. megu reads its vanilla worldgen from
# ./datapacks, so the pack directory is linked into its run directory; without it the
# server silently falls back to a flat generator and the comparison would be measuring
# two different things.
#
#   tools/idle_compare.sh [seconds] [seed]
#
# The goal is to be lightweight while behaving like vanilla, and the only way that
# means anything is against vanilla on equal terms. This is the cheapest such comparison
# - an idle server with nobody on it - and it is deliberately the first one, because a
# number that only exists on one side is not a comparison.
#
# What it does NOT do: put players on, or make the two servers do the same work. megu
# implements fewer features, so some of the difference is "does less" rather than "does
# it cheaper". Read the output with that in mind.
set -euo pipefail

SECONDS_WINDOW="${1:-12}"
SEED="${2:-0}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/idle-compare"
VANILLA_DIR="$WORK/vanilla"
SPINE_DIR="$WORK/spine"
JAR="$WORK/server.jar"
JAR_SHA1="59353fb40c36d304f2035d51e7d6e6baa98dc05c" # 1.21.1 official server

sample() { # pid -> "cpu_seconds rss_kib threads"
  local pid=$1 hz c0 c1
  hz=$(getconf CLK_TCK)
  c0=$(awk '{print $14+$15}' "/proc/$pid/stat")
  sleep "$SECONDS_WINDOW"
  c1=$(awk '{print $14+$15}' "/proc/$pid/stat")
  awk -v c0="$c0" -v c1="$c1" -v hz="$hz" -v rss="$(awk '/VmRSS/{print $2}' "/proc/$pid/status")" \
      -v thr="$(awk '/Threads/{print $2}' "/proc/$pid/status")" \
      'BEGIN{printf "%.3f %.1f %d", (c1-c0)/hz, rss/1024, thr}'
}

wait_for_done() { # logfile pid
  for _ in $(seq 1 60); do
    sleep 5
    grep -q 'Done (' "$1" 2>/dev/null && return 0
    kill -0 "$2" 2>/dev/null || { echo "server exited early:"; tail -5 "$1"; return 1; }
  done
  echo "server did not reach Done within 300s"; return 1
}

mkdir -p "$WORK"
if [ ! -f "$JAR" ] || [ "$(sha1sum "$JAR" | cut -d' ' -f1)" != "$JAR_SHA1" ]; then
  echo "fetching the vanilla 1.21.1 server bundle (self-extracting, libraries included)"
  URL=$(curl -sS https://piston-meta.mojang.com/mc/game/version_manifest_v2.json \
        | python3 -c 'import json,sys;print([v for v in json.load(sys.stdin)["versions"] if v["id"]=="1.21.1"][0]["url"])')
  URL=$(curl -sS "$URL" | python3 -c 'import json,sys;print(json.load(sys.stdin)["downloads"]["server"]["url"])')
  curl -sS -o "$JAR" "$URL"
fi

echo "-- vanilla, flat, seed $SEED, no players"
rm -rf "$VANILLA_DIR"; mkdir -p "$VANILLA_DIR"; cp "$JAR" "$VANILLA_DIR/server.jar"
printf 'eula=true\n' > "$VANILLA_DIR/eula.txt"
cat > "$VANILLA_DIR/server.properties" <<EOF
level-seed=$SEED
level-name=world
online-mode=false
view-distance=4
simulation-distance=4
spawn-protection=0
max-players=1
server-port=25599
enable-rcon=false
sync-chunk-writes=false
EOF
( cd "$VANILLA_DIR" && exec java -Xmx1G -jar server.jar nogui ) > "$VANILLA_DIR/server.log" 2>&1 &
VPID=$!
wait_for_done "$VANILLA_DIR/server.log" "$VPID"
VAN=$(sample "$VPID")
kill "$VPID" 2>/dev/null || true
wait "$VPID" 2>/dev/null || true

echo "-- megu, flat, seed $SEED, no players"
cargo build --release --offline --locked -j 4 --manifest-path "$ROOT/Cargo.toml" >/dev/null
rm -rf "$SPINE_DIR"; mkdir -p "$SPINE_DIR"
ln -s "$ROOT/datapacks" "$SPINE_DIR/datapacks"
( cd "$SPINE_DIR" && MEGU_PORT=25598 MEGU_SEED="$SEED" \
    exec "$ROOT/target/release/megu" ) > "$SPINE_DIR/spine.log" 2>&1 &
SPID=$!
for _ in $(seq 1 20); do sleep 1; grep -q 'listening' "$SPINE_DIR/spine.log" 2>/dev/null && break; done
SPI=$(sample "$SPID")
kill "$SPID" 2>/dev/null || true

printf '\n%-14s %10s %10s %8s\n' "idle ${SECONDS_WINDOW}s" "cpu(s)" "rss(MiB)" "threads"
printf '%-14s %10s %10s %8s\n' vanilla   $(echo "$VAN" | awk '{printf "%s %s %s", $1, $2, $3}')
printf '%-14s %10s %10s %8s\n' megu  $(echo "$SPI" | awk '{printf "%s %s %s", $1, $2, $3}')
echo
grep -hE '\[wg\]' "$SPINE_DIR/spine.log" || true
grep -oE 'Done \([0-9.]+s\)' "$VANILLA_DIR/server.log" | tail -1
