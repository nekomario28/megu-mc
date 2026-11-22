#!/usr/bin/env bash
# Compare vanilla 1.21.1 and mc-spine under a moving viewer, not at idle.
#
# tools/idle_compare.sh measures a server nobody is on. That number says nothing
# about the goal, because the work a lightweight server exists to make cheap is
# the work a player causes: streaming chunks for a view that moves, ticking the
# world around it, and unloading what falls behind. This runs the same protocol
# client against both servers with the same seed, world type, view distance and
# movement script, and samples CPU/RSS over the same window. The driver reports
# the chunks each server actually delivered, so the CPU is reported per chunk as
# well as raw: a cheaper number from a server that streamed less work is not a
# cheaper server.
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
# Allow the client's bounded play-ready wait (120s) before its measured drive.
LOAD_TIMEOUT_EXTRA=150

VPID=
SPID=
cleanup() {
  if [ -n "$VPID" ]; then
    kill "$VPID" 2>/dev/null || true
    wait "$VPID" 2>/dev/null || true
  fi
  if [ -n "$SPID" ]; then
    kill "$SPID" 2>/dev/null || true
    wait "$SPID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

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
# The viewer is a protocol driver with no world model: at ground level vanilla
# rejects its position whenever the terrain rises, which froze the sweep after
# two blocks. Creative skips that collision check (the ten-block move limit
# still applies, and the driver climbs inside it), so both servers stream
# chunks for the same moving view.
gamemode=creative
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

sample_load() { # pid port loadout -> "cpu(s) rss(MiB) threads chunks ms/chunk"
  local pid=$1 port=$2 out=$3 hz c0 c1 rss thr load chunks warm
  # WARM=1 spends one window driving first, so the measured window does not
  # contain the spawn/ login burst. The sweep is deterministic, so the warm pass
  # covers the first half of the path and the measured pass the second.
  if [ "${WARM:-0}" = 1 ]; then
    if ! warm=$( ( cd "$ROOT" && timeout $((SECONDS_WINDOW + LOAD_TIMEOUT_EXTRA)) \
        python3 tools/load_client.py "$port" "$SECONDS_WINDOW" LoadWarm --view-distance 4 ) 2>&1 ); then
      printf 'warm-up failed on port %s: %s\n' "$port" "$warm" >&2
      return 1
    fi
    if [[ "$warm" != LOAD\ ok\ * ]]; then
      printf 'warm-up did not finish on port %s: %s\n' "$port" "$warm" >&2
      return 1
    fi
  fi
  hz=$(getconf CLK_TCK)
  c0=$(awk '{print $14+$15}' "/proc/$pid/stat")
  # Let the viewer settle, then measure the same window of movement.
  sleep 3
  # A failed driver has no valid CPU-per-chunk measurement. Abort the comparison
  # rather than printing a plausible-looking row for an invalid window.
  if ! load=$( ( cd "$ROOT" && timeout $((SECONDS_WINDOW + LOAD_TIMEOUT_EXTRA)) \
      python3 tools/load_client.py "$port" "$SECONDS_WINDOW" LoadA --view-distance 4 ) 2>&1 ); then
    printf 'measured load failed on port %s: %s\n' "$port" "$load" >&2
    return 1
  fi
  if [[ "$load" != LOAD\ ok\ * ]]; then
    printf 'measured load incomplete on port %s: %s\n' "$port" "$load" >&2
    return 1
  fi
  c1=$(awk '{print $14+$15}' "/proc/$pid/stat")
  rss=$(awk '/VmRSS/{print $2}' "/proc/$pid/status")
  thr=$(awk '/Threads/{print $2}' "/proc/$pid/status")
  printf '%s\n' "$load" > "$out"
  # The driver reports the chunks each server actually delivered. Without the
  # count a CPU number is unattributable: the same window cost 4.2 s for nine
  # vanilla chunks and 20.6 s for eighty-one mc-spine chunks.
  chunks=$(printf '%s\n' "$load" | sed -n 's/.*chunks_login=\([0-9]*\).*chunks_drive=\([0-9]*\).*/\1 \2/p' \
           | awk 'NF{print $1+$2; exit}')
  [ -n "$chunks" ] || chunks=0
  awk -v c0="$c0" -v c1="$c1" -v hz="$hz" -v rss="$rss" -v thr="$thr" -v chunks="$chunks" \
      'BEGIN{cpu=(c1-c0)/hz; ms=(chunks>0)?cpu*1000/chunks:-1;
             printf "%.4f %.1f %d %d %.4f", cpu, rss/1024, thr, chunks, ms}'
}

row() { # label "cpu rss threads chunks ms"
  awk -v label="$1" -v m="$2" 'BEGIN{
    split(m, a, " ");
    ms = (a[5] >= 0) ? sprintf("%.2f", a[5]) : "-";
    printf "%-12s %8.3f %9.1f %8d %9d %10s\n", label, a[1], a[2], a[3], a[4], ms;
  }'
}

jar_ok() { [ -f "$1" ] && [ "$(sha1sum "$1" | cut -d' ' -f1)" = "$JAR_SHA1" ]; }

mkdir -p "$WORK"
if ! jar_ok "$JAR"; then
  # Reuse a pinned jar already on the host before reaching for the network: an
  # offline run must measure the same build, not fail because it cannot fetch.
  for candidate in "${MC_SPINE_VANILLA_JAR_CACHE:-$HOME/tmpx/carverfresh/server.jar}" /tmp/idle-compare/server.jar; do
    if jar_ok "$candidate"; then
      echo "reusing the pinned vanilla server jar from $candidate"
      cp "$candidate" "$JAR"
      break
    fi
  done
fi
if ! jar_ok "$JAR"; then
  echo "fetching the vanilla 1.21.1 server bundle"
  URL=$(curl -sS https://piston-meta.mojang.com/mc/game/version_manifest_v2.json \
        | python3 -c 'import json,sys;print([v for v in json.load(sys.stdin)["versions"] if v["id"]=="1.21.1"][0]["url"])')
  URL=$(curl -sS "$URL" | python3 -c 'import json,sys;print(json.load(sys.stdin)["downloads"]["server"]["url"])')
  curl -sS -o "$JAR" "$URL"
  # A fetch that lands a different build is not the pinned comparison point.
  jar_ok "$JAR" || { echo "fetched jar does not match the pinned sha1 $JAR_SHA1" >&2; exit 1; }
fi

echo "-- vanilla, normal, seed $SEED, view 4 / sim 5, moving viewer ${SECONDS_WINDOW}s"
setup_vanilla
( cd "$VANILLA_DIR" && exec java -Xmx1G -jar server.jar nogui ) > "$VANILLA_DIR/server.log" 2>&1 &
VPID=$!
wait_for_line "$VANILLA_DIR/server.log" "$VPID" 'Done (' 180
VAN=$(sample_load "$VPID" "$VAN_PORT" "$WORK/van-load.txt")
kill "$VPID" 2>/dev/null || true; wait "$VPID" 2>/dev/null || true
VPID=

echo "-- mc-spine, datapack worldgen, seed $SEED, moving viewer ${SECONDS_WINDOW}s"
# Build into the directory this script runs from. With CARGO_TARGET_DIR exported (the project's
# own workflow sets it) a bare `cargo build` writes elsewhere and the run below executes a stale
# `$ROOT/target/release/mc-spine`, silently measuring a binary nobody just built.
cargo build --release --offline --locked -j 4 --target-dir "$ROOT/target" --manifest-path "$ROOT/Cargo.toml" >/dev/null
rm -rf "$SPINE_DIR"; mkdir -p "$SPINE_DIR"
ln -s "$ROOT/datapacks" "$SPINE_DIR/datapacks"
# The registry is read from assets/ and the configuration handshake needs it;
# without this symlink breg() is None and the config exchange never completes.
ln -s "$ROOT/assets" "$SPINE_DIR/assets"
[ -f "$ROOT/config_packets.bin" ] && ln -s "$ROOT/config_packets.bin" "$SPINE_DIR/config_packets.bin" || true
# No resource pack, so the configuration exchange matches vanilla's and the
# driver reaches finish-configuration without a pack handshake. Anti-cheat off: the
# driver climbs 4 blocks per packet with no world model, which vanilla only accepts
# because it runs in creative; a survival mc-spine viewer would be pulled back.
( cd "$SPINE_DIR" && MC_SPINE_PORT="$SPINE_PORT" MC_SPINE_SEED="$SEED" MC_SPINE_RESOURCE_PACK_URL="" \
    MC_SPINE_COMPRESSION_THRESHOLD=-1 \
    MC_SPINE_ANTICHEAT=off exec "$ROOT/target/release/mc-spine" ) > "$SPINE_DIR/spine.log" 2>&1 &
SPID=$!
wait_for_line "$SPINE_DIR/spine.log" "$SPID" 'listening' 60
SPI=$(sample_load "$SPID" "$SPINE_PORT" "$WORK/spine-load.txt")
kill "$SPID" 2>/dev/null || true
wait "$SPID" 2>/dev/null || true
SPID=

printf '\n%-12s %8s %9s %8s %9s %10s\n' "load ${SECONDS_WINDOW}s" "cpu(s)" "rss(MiB)" "threads" "chunks" "ms/chunk"
row vanilla "$VAN"
row mc-spine "$SPI"
printf 'vanilla   driver: %s\n' "$(cat "$WORK/van-load.txt")"
printf 'mc-spine  driver: %s\n' "$(cat "$WORK/spine-load.txt")"
echo
echo "caveat: mc-spine implements fewer features, so this is not a like-for-like feature comparison."
