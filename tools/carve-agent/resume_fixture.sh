#!/usr/bin/env bash
# Resume a copy of a frozen capture; never modify the original world.
# Run in a resource-gated, capped scope. Outputs are not proof of a frozen
# start unless the pre-unfreeze snapshot and runtime log are checked.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SOURCE="${1:?frozen capture}"; DIR="${2:?new output directory}"
[[ -f "$SOURCE/finished.txt" && -f "$SOURCE/agent/carveagent.jar" ]] || { echo 'incomplete frozen capture' >&2; exit 2; }
[[ ! -e "$DIR" ]] || { echo 'refusing to overwrite output' >&2; exit 2; }
SOURCE="$(cd "$SOURCE" && pwd)"
AGENT="${CARVE_AGENT:-$SOURCE/agent/carveagent.jar}"
[[ -f "$AGENT" && -f "$(dirname "$AGENT")/carvelog.jar" ]] || { echo 'missing agent or its logger' >&2; exit 2; }
python3 - "$SOURCE/boundary.json" <<'PY'
import json,sys
from pathlib import Path
b=json.load(open(sys.argv[1]))
if b['mode']!='frozen' or b['game_times'][0]!=b['game_times'][1] or b['runtime_writes']:
    raise SystemExit('source is not a verified frozen boundary')
order=Path(sys.argv[1]).with_name('order.tsv').read_text().splitlines()
expected={(x,z) for x in range(-65,-60) for z in range(31,36)}
if {tuple(map(int,line.split())) for line in order} != expected or len(order)!=25:
    raise SystemExit('source is not the required R1 region')
PY
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
cp -a "$SOURCE/world" "$DIR/world"
cp "$SOURCE/server.properties" "$SOURCE/eula.txt" "$DIR/"
for item in server.jar libraries versions; do ln -s "$SOURCE/$item" "$DIR/$item"; done
date -Iseconds > "$DIR/started.txt"
sha256sum "$AGENT" "$(dirname "$AGENT")/carvelog.jar" > "$DIR/agent.sha256"
cd "$DIR"
tick_args=()
if [[ "${MC_TICK_TRACE:-0}" == 1 ]]; then
    tick_args+=("-Dtickbox=${MC_TICK_BOX:--1024,-64,512,-977,319,559}")
fi
commands() {
    fail() { printf 'stop\n'; echo "$*" >&2; exit 2; }
    deadline=$((SECONDS+90))
    until grep -Fq 'Done (' run.log 2>/dev/null; do
        ((SECONDS<deadline)) || fail 'resume: startup readiness timeout'
        sleep 0.1
    done
    printf 'tick freeze\n'
    deadline=$((SECONDS+15))
    until grep -Fq 'The game is frozen' run.log; do
        ((SECONDS<deadline)) || fail 'resume: freeze acknowledgement timeout'
        sleep 0.1
    done
    printf 'time query gametime\n'
    deadline=$((SECONDS+90))
    while :; do
        for dx in 0 1 2; do for dz in 0 1 2; do
            printf 'execute if loaded %d 64 %d run say RESUME_FULL_%d_%d\n' "$((-1024+dx*16))" "$((512+dz*16))" "$dx" "$dz"
        done; done
        sleep 1
        ready=$(grep -Eo 'RESUME_FULL_[0-2]_[0-2]$' run.log | sort -u | wc -l) || ready=0
        ((ready==9)) && break
        ((SECONDS<deadline)) || fail 'resume: full-chunk readiness timeout'
    done
    printf 'save-all flush\n'
    deadline=$((SECONDS+30))
    until grep -Fq 'Saved the game' run.log; do
        ((SECONDS<deadline)) || fail 'resume: initial save timeout'
        sleep 0.1
    done
    cp -a world snapshot-frozen
    printf 'say RESUME_START\ntick unfreeze\n'
    sleep 35
    printf 'tick freeze\n'
    deadline=$((SECONDS+20))
    until [[ $(grep -Fc 'The game is frozen' run.log) -ge 2 ]]; do
        ((SECONDS<deadline)) || fail 'resume: final freeze timeout'
        sleep 0.1
    done
    printf 'time query gametime\nsave-all flush\n'
    deadline=$((SECONDS+30))
    until [[ $(grep -Fc 'Saved the game' run.log) -ge 2 ]]; do
        ((SECONDS<deadline)) || fail 'resume: final save timeout'
        sleep 0.1
    done
    cp -a world snapshot-ticked
    printf 'stop\n'
}
commands | timeout 240 java -XX:ActiveProcessorCount=2 -Xmx1G -Xms256M \
    -javaagent:"$AGENT" -Dcarverquiet=true -Ddim=-64 \
    "-Druntimebox=${MC_RUNTIME_BOX:--1024,-64,512,-977,319,559}" "${tick_args[@]}" -jar server.jar nogui > run.log 2>&1
! grep -qi 'CARVEAGENT .*failed\|unexpected error occurred\|Incorrect argument for command\|Unknown or incomplete command' run.log
date -Iseconds > finished.txt
