#!/usr/bin/env bash
# Bounded seed-0, structures=false native fixture (1..3 chunks per side).
# Run inside a resource-gated MemoryMax=1800M/CPUQuota=100% scope.
# Usage: native_fixture.sh <NEW-output-dir> <chunk-x> <chunk-z> <n> [frozen|ticked]
# Uses an existing authenticated fixture base; never downloads or deletes a capture.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DIR="${1:?output directory}"; CX="${2:?chunk x}"; CZ="${3:?chunk z}"; N="${4:-3}"; MODE="${5:-frozen}"
BASE="${CARVER_FIXTURE_BASE:-/tmp/carverfresh}"
ASM="${ASM_JAR:-$HOME/.m2/repository/org/ow2/asm/asm/9.8/asm-9.8.jar}"
[[ "$CX" =~ ^-?[0-9]+$ && "$CZ" =~ ^-?[0-9]+$ && "$N" =~ ^[1-3]$ ]] || { echo 'invalid region (n must be 1..3)' >&2; exit 2; }
[[ "$MODE" == frozen || "$MODE" == ticked ]] || { echo 'mode must be frozen or ticked' >&2; exit 2; }
[[ ! -e "$DIR" ]] || { echo "refusing to overwrite capture: $DIR" >&2; exit 2; }
PYTHONPATH="$ROOT/tools" python3 - "$BASE/server.jar" <<'PY'
import hashlib,sys
from pathlib import Path
from vanilla_java import JAR_SHA1
if hashlib.sha1(Path(sys.argv[1]).read_bytes()).hexdigest()!=JAR_SHA1:
    raise SystemExit('not the authenticated 1.21.1 bundler')
PY
mkdir -p "$DIR/agent/c1" "$DIR/agent/c2" "$DIR/agent/j1" "$DIR/world"
DIR="$(cd "$DIR" && pwd)"
date -Iseconds > "$DIR/started.txt"
printf '%s\n' "$MODE" > "$DIR/mode.txt"
sha256sum "$ROOT/tools/carve-agent/Agent.java" "$ROOT/tools/carve-agent/CarveLog.java" "$BASE/server.jar" > "$DIR/inputs.sha256"
java -version > "$DIR/java-version.txt" 2>&1
javac -J-Xmx256m -nowarn -cp "$ASM" -d "$DIR/agent/c1" "$ROOT/tools/carve-agent/Agent.java"
javac -J-Xmx256m -nowarn -d "$DIR/agent/c2" "$ROOT/tools/carve-agent/CarveLog.java"
(cd "$DIR/agent/c2"; jar cf ../carvelog.jar .)
cp -r "$DIR/agent/c1/"* "$DIR/agent/j1/"
(cd "$DIR/agent/j1"; jar xf "$ASM" org; printf 'Premain-Class: Agent\n' > MANIFEST.MF; jar cfm ../carveagent.jar MANIFEST.MF .)
for p in server.jar libraries versions; do ln -s "$BASE/$p" "$DIR/$p"; done
# No public listener, authentication dependency, preexisting world, or spawn ticket.
cat > "$DIR/server.properties" <<'PROPERTIES'
level-name=world
level-seed=0
generate-structures=false
server-ip=127.0.0.1
server-port=0
online-mode=false
spawn-protection=0
view-distance=2
simulation-distance=2
sync-chunk-writes=true
PROPERTIES
printf 'eula=true\n' > "$DIR/eula.txt"
lox=$((CX*16)); loz=$((CZ*16)); hix=$(((CX+N)*16-1)); hiz=$(((CZ+N)*16-1))
box="$lox,-64,$loz,$hix,319,$hiz"
cd "$DIR"
commands() {
    fail() { printf 'stop\n'; echo "$*" >&2; exit 2; }
    deadline=$((SECONDS+90))
    until grep -Fq 'Done (' run.log 2>/dev/null; do
        ((SECONDS<deadline)) || fail 'native fixture: startup readiness timeout'
        sleep 0.1
    done
    if [[ "$MODE" == frozen ]]; then
        printf 'tick freeze\n'
        deadline=$((SECONDS+10))
        until grep -Fq 'The game is frozen' run.log; do
            ((SECONDS<deadline)) || fail 'native fixture: freeze acknowledgement timeout'
            sleep 0.1
        done
    fi
    printf 'time query gametime\nforceload add %d %d %d %d\n' "$lox" "$loz" "$hix" "$hiz"
    deadline=$((SECONDS+90))
    while :; do
        for ((dx=0;dx<N;dx++)); do for ((dz=0;dz<N;dz++)); do
            printf 'execute if loaded %d 64 %d run say MC_FULL_%d_%d\n' "$((lox+dx*16))" "$((loz+dz*16))" "$dx" "$dz"
        done; done
        sleep 1
        ready=$(grep -Eo 'MC_FULL_[0-2]_[0-2]$' run.log | sort -u | wc -l) || ready=0
        ((ready==N*N)) && break
        ((SECONDS<deadline)) || fail 'native fixture: full-chunk readiness timeout'
    done
    if [[ "$MODE" == ticked ]]; then sleep 35; fi
    printf 'time query gametime\nsave-all flush\n'
    deadline=$((SECONDS+30))
    until grep -Fq 'Saved the game' run.log; do
        ((SECONDS<deadline)) || fail 'native fixture: save acknowledgement timeout'
        sleep 0.1
    done
    printf 'stop\n'
}
commands | timeout 240 java -XX:ActiveProcessorCount=2 -Xmx1G -Xms256M \
    -javaagent:"$DIR/agent/carveagent.jar" -Dcarverquiet=true -Ddim=-64 -Ddecoseed=0 -Ddecoorder=1 \
    "-Dfeaturebox=$box" "-Druntimebox=$box" -jar server.jar nogui > run.log 2>&1
! grep -qi 'CARVEAGENT .*failed\|unexpected error occurred\|Incorrect argument for command\|Unknown or incomplete command' run.log
PYTHONPATH="$ROOT/tools" python3 - "$DIR" "$CX" "$CZ" "$N" "$MODE" <<'PY'
import array,json,re,sys
from pathlib import Path
from carver_diff import vanilla_chunk,load_states
root=Path(sys.argv[1]);cx,cz,n=map(int,sys.argv[2:5]);mode=sys.argv[5]
log=(root/'run.log').read_text();times=list(map(int,re.findall(r'The time is (\d+)',log)))
if len(times)!=2 or (mode=='frozen' and times[0]!=times[1]):
    raise SystemExit(f'invalid native game-time boundary: {times}')
table=load_states();out=array.array('I')
for z in range(cz,cz+n):
    for x in range(cx,cx+n):
        cells=vanilla_chunk(root/'world',x,z,table)
        if cells is None or min(cells)<0: raise SystemExit(f'incomplete/unmapped chunk {x},{z}')
        out.extend(cells)
if sys.byteorder!='little':out.byteswap()
(root/'native.bin').write_bytes(out.tobytes())
order=[]
for x,z in re.findall(r'DECO (-?\d+) (-?\d+)',log):
    x,z=int(x),int(z)
    if cx-1<=x<=cx+n and cz-1<=z<=cz+n:order.append((x,z))
if len(order)!=(n+2)**2 or len(set(order))!=len(order):raise SystemExit(f'invalid decoration footprint: {order}')
(root/'order.tsv').write_text(''.join(f'{x}\t{z}\n' for x,z in order))
(root/'boundary.json').write_text(json.dumps({'mode':mode,'game_times':times,'chunks':n*n,'decorated_chunks':len(order),'runtime_writes':len(re.findall(r'RUNTIME .*wrote=true',log))},indent=2)+'\n')
print((root/'boundary.json').read_text())
PY
sha256sum --check inputs.sha256 > inputs-verified.txt
date -Iseconds > finished.txt
printf 'native fixture captured: %s\n' "$DIR"
