#!/usr/bin/env python3
"""Build a bounded adapter against actual pinned NeoForge game-bus/event APIs."""
import argparse, hashlib, json, os, pathlib, shutil, subprocess, sys, urllib.request
ROOT=pathlib.Path(__file__).resolve().parent
sys.path.insert(0,str(ROOT.parent/'fabric-probe'))
from build import sha, jar

def main():
    p=argparse.ArgumentParser();p.add_argument('--java-home',required=True);args=p.parse_args()
    cache=ROOT/'.cache';cache.mkdir(exist_ok=True)
    build=ROOT/'.build'
    if build.exists():shutil.rmtree(build)
    build.mkdir();paths={}
    pins=json.loads((ROOT/'dependencies.json').read_text())
    for name,pin in pins.items():
        path=cache/pin['url'].rsplit('/',1)[1]
        if not path.exists():
            with urllib.request.urlopen(pin['url'],timeout=30) as response:data=response.read(20*1024*1024+1)
            if len(data)>20*1024*1024 or hashlib.sha256(data).hexdigest()!=pin['sha256']:raise RuntimeError('Pinned download mismatch: '+name)
            path.write_bytes(data)
        if sha(path)!=pin['sha256']:raise RuntimeError('Pinned hash mismatch: '+name)
        paths[name]=path
    libraries=[path for name,path in paths.items() if name!='mod' and not name.endswith('_sources')]
    sources=sorted((ROOT/'java').rglob('*.java'))+[ROOT.parent/'fabric-probe/java/net/mcspine/probe/Host.java']
    classes=build/'classes';classes.mkdir()
    subprocess.run([str(pathlib.Path(args.java_home)/'bin/javac'),'-J-Xmx128m','--release','21','-proc:none','-cp',os.pathsep.join(map(str,libraries)),'-d',str(classes),*map(str,sources)],check=True)
    contents={str(path.relative_to(classes)):path.read_bytes() for path in classes.rglob('*.class')}
    contents['net/mcspine/neoforge/shim-surface.json']=(ROOT/'shim-surface.json').read_bytes()
    jar(build/'game.jar',contents)
    (build/'classpath.txt').write_text(os.pathsep.join(map(str,[build/'game.jar',*libraries])))
    (build/'build-receipt.json').write_text(json.dumps({'pins':pins,'sources':{str(path):sha(path) for path in sources},'surface_sha256':sha(ROOT/'shim-surface.json'),'jar_sha256':sha(build/'game.jar')},indent=2)+'\n')
    print(build/'classpath.txt')
if __name__=='__main__':main()
