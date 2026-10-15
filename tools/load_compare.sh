#!/usr/bin/env bash
# Compare vanilla 1.21.1 and mc-spine under a moving viewer, not at idle.
#
# tools/idle_compare.sh measures a server nobody is on. That number says nothing
# about the goal, because the work a lightweight server exists to make cheap is
# the work a player causes: streaming chunks for a view that moves, ticking the
# world around it, and unloading what falls behind. This runs the same protocol
# client against both servers with the same seed, world type, view distance and
# movement script, and samples CPU/RSS over the same window.
#
#   tools/load_compare.sh [seconds] [seed]
#
# It is still not an apples-to-apples feature comparison: mc-spine implements
# fewer features, so part of any difference is "does less", not "does it
# cheaper". Compression is disabled on both sides so the two speak the identical
# protocol and neither pays for a codec the other does not.
set -euo pipefail

SECONDS_WINDOW="${1:-20}"
SEED="${2:-0}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${MC_SPINE_LOAD_WORK:-$HOME/.cache/mc-spine-audit/load}"
VANILLA_DIR="$WORK/vanilla"
SPINE_DIR="$WORK/spine"
JAR="$WORK/server.jar"
JAR_SHA1="59353fb40c36d304f2035d51e7d6e6baa98dc05c"
VAN_PORT=25601
SPINE_PORT=25602

setup_vanilla() {
  rm -rf "$VANILLA_DIR"; mkdir -p "$VANILLA_DIR"; cp "$JAR" "$VANILLA_DIR/server.jar"
  printf 'eula=true\n' > "$VANILLA_DIR/eula.txt"
  cat > "$VANILLA_DIR/server.properties" <<EOF
level-seed=$SEED
level-name=world
online-mode=false
view-distance=4
simulation-distance=5
spawn-protection=0
max-players=2
server-port=$VAN_PORT
enable-rcon=false
sync-chunk-writes=false
network-compression-threshold=-1
level-type=minecraft:normal
EOF
}

wait_for_line() { # logfile pid marker timeout_s
  for _ in $(seq 1 "$4"); do
    sleep 1
    grep -q "$3" "$1" 2>/dev/null && return 0
    kill -0 "$2" 2>/dev/null || { echo "server exited early:"; tail -8 "$1"; return 1; }
  done
  echo "server did not reach '$3' in $4 s"; tail -8 "$1"; return 1
}

sample_load() { # pid port loadout
  local pid=$1 port=$2 out=$3 hz c0 c1 rss thr load
  hz=$(getconf CLK_TCK)
  c0=$(awk '{print $14+$15}' "/proc/$pid/stat")
  # Let the viewer settle, then measure the same window of movement.
  sleep 3
  load=$( ( cd "$ROOT" && timeout $((SECONDS_WINDOW + 30)) python3 tools/load_client.py "$port" "$SECONDS_WINDOW" LoadA ) 2>&1 )
  [ $? -eq 0 ] || load="LOAD failed: $load"
  c1=$(awk '{print $14+$15}' "/proc/$pid/stat")
  rss=$(awk '/VmRSS/{print $2}' "/proc/$pid/status")
  thr=$(awk '/Threads/{print $2}' "/proc/$pid/status")
  printf '%s\n' "$load" > "$out"
  awk -v c0="$c0" -v c1="$c1" -v hz="$hz" -v rss="$rss" -v thr="$thr" \
      'BEGIN{printf "%.3f %.1f %d", (c1-c0)/hz, rss/1024, thr}'
}

mkdir -p "$WORK"
if [ ! -f "$JAR" ] || [ "$(sha1sum "$JAR" | cut -d' ' -f1)" != "$JAR_SHA1" ]; then
  if [ -f /tmp/idle-compare/server.jar ] && \
     [ "$(sha1sum /tmp/idle-compare/server.jar | cut -d' ' -f1)" = "$JAR_SHA1" ]; then
    cp /tmp/idle-compare/server.jar "$JAR"
  else
    echo "fetching the vanilla 1.21.1 server bundle"
    URL=$(curl -sS https://piston-meta.mojang.com/mc/game/version_manifest_v2.json \
          | python3 -c 'import json,sys;print([v for v in json.load(sys.stdin)["versions"] if v["id"]=="1.21.1"][0]["url"])')
    URL=$(curl -sS "$URL" | python3 -c 'import json,sys;print(json.load(sys.stdin)["downloads"]["server"]["url"])')
    curl -sS -o "$JAR" "$URL"
  fi
fi

echo "-- vanilla, normal, seed $SEED, view 4 / sim 5, moving viewer ${SECONDS_WINDOW}s"
setup_vanilla
( cd "$VANILLA_DIR" && exec java -Xmx1G -jar server.jar nogui ) > "$VANILLA_DIR/server.log" 2>&1 &
VPID=$!
wait_for_line "$VANILLA_DIR/server.log" "$VPID" 'Done (' 180
VAN=$(sample_load "$VPID" "$VAN_PORT" "$WORK/van-load.txt")
kill "$VPID" 2>/dev/null || true; wait "$VPID" 2>/dev/null || true

echo "-- mc-spine, datapack worldgen, seed $SEED, moving viewer ${SECONDS_WINDOW}s"
cargo build --release --offline --locked -j 4 --manifest-path "$ROOT/Cargo.toml" >/dev/null
rm -rf "$SPINE_DIR"; mkdir -p "$SPINE_DIR"
ln -s "$ROOT/datapacks" "$SPINE_DIR/datapacks"
# The registry is read from assets/ and the configuration handshake needs it;
# without this symlink breg() is None and the config exchange never completes.
ln -s "$ROOT/assets" "$SPINE_DIR/assets"
[ -f "$ROOT/config_packets.bin" ] && ln -s "$ROOT/config_packets.bin" "$SPINE_DIR/config_packets.bin" || true
# No resource pack, so the configuration exchange matches vanilla's and the
# driver reaches finish-configuration without a pack handshake.
( cd "$SPINE_DIR" && MC_SPINE_PORT="$SPINE_PORT" MC_SPINE_SEED="$SEED" MC_SPINE_RESOURCE_PACK_URL="" \
    exec "$ROOT/target/release/mc-spine" ) > "$SPINE_DIR/spine.log" 2>&1 &
SPID=$!
wait_for_line "$SPINE_DIR/spine.log" "$SPID" 'listening' 60
SPI=$(sample_load "$SPID" "$SPINE_PORT" "$WORK/spine-load.txt")
kill "$SPID" 2>/dev/null || true

printf '\n%-12s %8s %9s %8s\n' "load ${SECONDS_WINDOW}s" "cpu(s)" "rss(MiB)" "threads"
printf '%-12s ' vanilla;  echo "$VAN"
printf '%-12s ' mc-spine; echo "$SPI"
printf 'vanilla   driver: %s\n' "$(cat "$WORK/van-load.txt")"
printf 'mc-spine  driver: %s\n' "$(cat "$WORK/spine-load.txt")"
echo
echo "caveat: mc-spine implements fewer features, so this is not a like-for-like feature comparison."
