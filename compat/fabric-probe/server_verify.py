#!/usr/bin/env python3
"""Exercise normal listeners, movement packets, game tick callbacks and persisted state."""
import argparse, hashlib, json, os, pathlib, re, shutil, socket, struct, subprocess, tempfile, threading, time
ROOT = pathlib.Path(__file__).resolve().parent

def varint(value):
    out = bytearray()
    while True:
        byte = value & 127; value >>= 7
        out.append(byte | (128 if value else 0))
        if not value: return bytes(out)

def text(value):
    value = value.encode(); return varint(len(value)) + value

def exact(sock, count):
    out = b''
    while len(out) < count:
        part = sock.recv(count-len(out))
        if not part: raise EOFError('Server disconnected')
        out += part
    return out

def read_varint(sock):
    value = 0
    for i in range(5):
        byte = exact(sock,1)[0]; value |= (byte & 127) << (7*i)
        if byte < 128: return value
    raise ValueError('Invalid frame length')

def frame(sock, pid, payload=b''):
    payload = varint(pid)+payload; sock.sendall(varint(len(payload))+payload)

def packet(sock):
    body = exact(sock,read_varint(sock)); value = 0
    for i,byte in enumerate(body):
        value |= (byte & 127) << (7*i)
        if byte < 128: return value,body[i+1:]
    raise ValueError('Invalid packet')

def join(port):
    sock = socket.create_connection(('127.0.0.1',port),timeout=15)
    frame(sock,0,varint(767)+text('127.0.0.1')+struct.pack('>H',port)+varint(2))
    frame(sock,0,text('LandingTest')+bytes(range(16)))
    assert packet(sock)[0] == 2
    frame(sock,3)
    while True:
        pid,_ = packet(sock)
        if pid == 14: frame(sock,7,varint(0))
        if pid == 3: frame(sock,3); break
    health=[]
    def drain():
        try:
            while True:
                pid,body=packet(sock)
                if pid==0x26: frame(sock,0x18,body)
                if pid==0x40: frame(sock,0,varint(1))
                if pid==0x5d: health.append(struct.unpack('>f',body[:4])[0])
        except (OSError,EOFError): pass
    threading.Thread(target=drain,daemon=True).start()
    return sock,health

def command(port, value):
    with socket.create_connection(('127.0.0.1',port),timeout=10) as sock:
        def request(kind, body):
            payload=struct.pack('<ii',1,kind)+body.encode()+b'\0\0'
            sock.sendall(struct.pack('<i',len(payload))+payload)
            return exact(sock,struct.unpack('<i',exact(sock,4))[0])
        auth=request(3,'probe-only'); assert struct.unpack('<i',auth[:4])[0]==1
        return request(2,value)[8:-2].decode()

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--binary',required=True,type=pathlib.Path)
    parser.add_argument('--output',required=True,type=pathlib.Path)
    parser.add_argument('--neoforge',action='store_true')
    args=parser.parse_args(); binary=args.binary.resolve(); build=ROOT/'.build'
    records=[]
    inputs={binary:hashlib.sha256(binary.read_bytes()).hexdigest()}
    for folder in ([ROOT] + ([ROOT.parent/'neoforge-events'] if args.neoforge else [])):
        for path in (folder/'.cache').glob('*.jar'): inputs[path]=hashlib.sha256(path.read_bytes()).hexdigest()
        for path in (folder/'.build').glob('*.jar'): inputs[path]=hashlib.sha256(path.read_bytes()).hexdigest()
    assert hashlib.sha256((ROOT/'.cache/seedprotect-1.21.1-fabric-1.0.0.jar').read_bytes()).hexdigest()=='ccd48d96239f485846ced57968e73c6040df4a66ca6ea02cb180a29a5cff7a3f'
    if args.neoforge:
        assert hashlib.sha256((ROOT.parent/'neoforge-events/.cache/seedprotect-1.0.0.jar').read_bytes()).hexdigest()=='90d3318ce049d2e684b08d12ab6e35410a9d8e159afc43757102ab127c028acf'
    with tempfile.TemporaryDirectory(prefix='mc-spine-normal-') as scratch:
        modes=['native','fabric_baseline','fabric_seedprotect']
        if args.neoforge: modes += ['neoforge_baseline','neoforge_seedprotect']
        for index,mode in enumerate(modes):
            cwd=pathlib.Path(scratch)/mode; cwd.mkdir()
            shutil.copyfile(ROOT.parent.parent/'config_packets.bin',cwd/'config_packets.bin')
            (cwd/'mods').mkdir()
            if mode=='fabric_seedprotect': shutil.copyfile(ROOT/'.cache/seedprotect-1.21.1-fabric-1.0.0.jar',cwd/'mods/seedprotect.jar')
            port=25650+index*2; rcon=port+1
            env={**os.environ,'JAVA_HOME':'/usr/lib/jvm/java-21-openjdk','MC_SPINE_WG':'old','MC_SPINE_PORT':str(port),'MC_SPINE_RCON_PORT':str(rcon),'MC_SPINE_RCON_PASS':'probe-only'}
            env.pop('MC_SPINE_FABRIC_CLASSPATH',None)
            env.pop('MC_SPINE_NEOFORGE_CLASSPATH',None)
            if mode.startswith('fabric_'): env['MC_SPINE_FABRIC_CLASSPATH']=str(build/'classpath-seedprotect.txt')
            if mode.startswith('neoforge_'): env['MC_SPINE_NEOFORGE_CLASSPATH']=str(ROOT.parent/'neoforge-events/.build/classpath.txt')
            if mode=='neoforge_seedprotect': shutil.copyfile(ROOT.parent/'neoforge-events/.cache/seedprotect-1.0.0.jar',cwd/'mods/seedprotect.jar')
            log=cwd/'capture.log'
            with log.open('w') as output:
                process=subprocess.Popen([str(binary)],cwd=cwd,env=env,stdout=output,stderr=subprocess.STDOUT)
                sock=None
                try:
                    end=time.monotonic()+25
                    while time.monotonic()<end:
                        if process.poll() is not None: raise RuntimeError(log.read_text())
                        try: command(rcon,'list');break
                        except OSError: time.sleep(.1)
                    else: raise TimeoutError('Normal server did not start')
                    sock,health=join(port)
                    end=time.monotonic()+25
                    while 'LandingTest' not in command(rcon,'list'):
                        if time.monotonic()>end: raise TimeoutError('Client did not enter normal play')
                        time.sleep(.1)
                    command(rcon,'gamerule doMobSpawning false')
                    command(rcon,'gamerule mobGriefing true')
                    command(rcon,'gamerule naturalRegeneration false')
                    command(rcon,'kill @e[type=zombie]')
                    command(rcon,'kill @e[type=cow]')
                    command(rcon,'gamemode survival LandingTest')
                    # Use real play packets. LandingTest starts grounded; fly up, then land.
                    command(rcon,'setblock 8 70 8 farmland')
                    frame(sock,0x1a,struct.pack('>ddd?',8.5,77.0,8.5,False))
                    time.sleep(.1)
                    frame(sock,0x1a,struct.pack('>ddd?',8.5,70.9375,8.5,True))
                    time.sleep(.4)
                    block=command(rcon,'execute if block 8 70 8 dirt run say NORMAL_DIRT')
                    farmland=command(rcon,'execute if block 8 70 8 farmland run say NORMAL_FARMLAND')
                    expected='farmland' if mode.endswith('_seedprotect') else 'dirt'
                    assert (block == 'executed 1 command(s)') == (expected=='dirt'),(mode,block,farmland,log.read_text())
                    assert (farmland == 'executed 1 command(s)') == (expected=='farmland'),(mode,block,farmland)
                    assert health and health[-1] == 16.0,(mode,health)
                    # An onGround-only packet must use the same operation and damage path.
                    command(rcon,'setblock 8 70 8 farmland')
                    frame(sock,0x1a,struct.pack('>ddd?',8.5,77.0,8.5,False))
                    frame(sock,0x1a,struct.pack('>ddd?',8.5,70.9375,8.5,False))
                    frame(sock,0x1d,b'\x01')
                    time.sleep(.4)
                    assert health[-1] == 12.0,(mode,health)
                    assert (command(rcon,'execute if block 8 70 8 dirt run say NORMAL_DIRT') == 'executed 1 command(s)') == (expected=='dirt')
                    # Adult cow follows the normal 250ms mob tick, with the player nearby.
                    command(rcon,'setblock 8 70 8 farmland')
                    command(rcon,'give LandingTest wheat')
                    command(rcon,'summon cow 8.5 77 8.5')
                    time.sleep(2)
                    cow_pos=command(rcon,'data get entity @e[type=cow,limit=1] Pos')
                    cow_health=command(rcon,'data get entity @e[type=cow,limit=1] Health')
                    assert (command(rcon,'execute if block 8 70 8 dirt run say NORMAL_DIRT') == 'executed 1 command(s)') == (expected=='dirt'),(mode,cow_pos,cow_health,log.read_text())
                    assert 'No entity' not in cow_pos and 'has: 17;' in cow_health,(mode,cow_pos,cow_health,log.read_text())
                    record={'mode':mode,'expected_block':expected,'player_health_packets':health.copy(),'cow_position':cow_pos,'cow_health':cow_health}
                finally:
                    if sock: sock.close()
                    if process.poll() is None: process.terminate()
                    process.wait(timeout=8)
            assert process.returncode==0,log.read_text()
            world=cwd/'world_overlay.bin'; assert world.is_file()
            reload=subprocess.run([str(binary),'--fabric-trample-reload','4286' if expected=='farmland' else '10'],cwd=cwd,env=env,text=True,capture_output=True,timeout=10)
            assert reload.returncode==0,(mode,reload.stderr)
            record.update({'reload':json.loads(reload.stdout.splitlines()[-1]),'world_sha256':hashlib.sha256(world.read_bytes()).hexdigest(),'log':log.read_text()})
            records.append(record)
        refusals=[('fabric_missing_invoke','fabric','Critical injection failure'),('fabric_unknown_mod','fabric','Unsupported mod hash'),('conflicting_adapters','both','Select only one Java loader adapter')]
        if args.neoforge: refusals.append(('neoforge_unknown_mod','neoforge','ShimCheck rejected NeoForge mod:'))
        for index,(name,adapter,reason) in enumerate(refusals):
            cwd=pathlib.Path(scratch)/name;cwd.mkdir();(cwd/'mods').mkdir()
            env={**os.environ,'JAVA_HOME':'/usr/lib/jvm/java-21-openjdk','MC_SPINE_WG':'old','MC_SPINE_PORT':str(25670+index*2),'MC_SPINE_RCON_PORT':str(25671+index*2),'MC_SPINE_RCON_PASS':'probe-only'}
            env.pop('MC_SPINE_FABRIC_CLASSPATH',None);env.pop('MC_SPINE_NEOFORGE_CLASSPATH',None)
            if adapter in ['fabric','both']:env['MC_SPINE_FABRIC_CLASSPATH']=str(build/'classpath-seedprotect.txt')
            if adapter in ['neoforge','both']:env['MC_SPINE_NEOFORGE_CLASSPATH']=str(ROOT.parent/'neoforge-events/.build/classpath.txt')
            if name=='fabric_missing_invoke':
                env['MC_SPINE_FABRIC_CLASSPATH']=str(build/'classpath-seedprotect-missing-invoke.txt')
                shutil.copyfile(ROOT/'.cache/seedprotect-1.21.1-fabric-1.0.0.jar',cwd/'mods/seedprotect.jar')
            if name.endswith('unknown_mod'):(cwd/'mods/unsupported.jar').write_bytes(b'not-a-supported-artifact')
            result=subprocess.run([str(binary)],cwd=cwd,env=env,text=True,capture_output=True,timeout=15)
            assert result.returncode!=0 and reason in result.stderr,(name,result.stderr)
            assert 'listening :' not in result.stderr and not (cwd/'world_overlay.bin').exists(),(name,result.stderr)
            records.append({'mode':name,'startup_rejection':reason,'stderr':result.stderr})
    for path,value in inputs.items():assert hashlib.sha256(path.read_bytes()).hexdigest()==value,str(path)+': changed during verification'
    receipt={'inputs_sha256':{str(path):value for path,value in inputs.items()},'rust_sources':{path.name:hashlib.sha256(path.read_bytes()).hexdigest() for path in (ROOT.parent.parent/'src').glob('*.rs')},'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'cases':records,'scope':'normal TCP player landing and adult cow tick; original pinned Fabric and NeoForge artifacts; saved Rust-world reload; NeoForge full FML startup NOT RUN'}
    args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(receipt,indent=2)+'\n')
    print(json.dumps({'passed':len(records),'output':str(args.output)}))
if __name__=='__main__':main()
