"""Pinned inputs and Yarn-to-intermediary bindings for one existing mod contract."""
import hashlib, json, os, pathlib, re, subprocess, urllib.request, zipfile

def prepare(root, build, classes, libraries, java):
    pins = json.loads((root / 'seedprotect.json').read_text())
    paths = {}
    for name, pin in pins.items():
        path = root / '.cache' / pin['url'].rsplit('/', 1)[1]
        if not path.exists():
            with urllib.request.urlopen(pin['url'], timeout=30) as response: data = response.read(20*1024*1024+1)
            if len(data) > 20*1024*1024 or hashlib.sha256(data).hexdigest() != pin['sha256']: raise RuntimeError('Invalid pinned download: '+name)
            path.write_bytes(data)
        if hashlib.sha256(path.read_bytes()).hexdigest() != pin['sha256']: raise RuntimeError('Pinned hash mismatch: '+name)
        paths[name] = path
    with zipfile.ZipFile(paths['yarn']) as source: tiny = source.read('mappings/mappings.tiny').decode()
    assert tiny.splitlines()[0] == 'tiny\t2\t0\tintermediary\tnamed'
    all_classes = {a[1]:a[2] for line in tiny.splitlines() if (a:=line.split('\t'))[0]=='c'}
    selected = ['net/minecraft/class_2344', 'net/minecraft/class_1297', 'net/minecraft/class_2680', 'net/minecraft/class_1937', 'net/minecraft/class_2338']
    mappings = {'class.'+all_classes[name]: name for name in selected}
    methods = {}
    owner = None
    for line in tiny.splitlines():
        a = line.split('\t')
        if a[0]=='c': owner=a[1]
        if len(a)>4 and a[1]=='m' and ((owner=='net/minecraft/class_2344' and a[3]=='method_10125') or (owner=='net/minecraft/class_2248' and a[3]=='method_9554')):
            descriptor = re.sub(r'L([^;]+);', lambda m:'L'+all_classes[m[1]]+';', a[2])
            mappings['method.net/minecraft/block/FarmlandBlock.'+a[4]+descriptor]=a[3]
            methods[a[3]]={'declaring_owner':owner,'descriptor':a[2],'named':a[4]}
    assert set(methods)=={'method_10125','method_9554'}
    properties=build/'bindings.properties'
    properties.write_text('\n'.join(k+'='+v for k,v in sorted(mappings.items()))+'\n')
    remapped=build/'remapped'
    if remapped.exists():
        import shutil
        shutil.rmtree(remapped)
    subprocess.run([str(java),'-Xmx128m','-cp',os.pathsep.join(map(str,[classes,*libraries])),'net.mcspine.probe.RemapFacade',str(properties),str(classes),str(remapped)],check=True)
    entries={str(p.relative_to(remapped)):p.read_bytes() for parent in ['net/mcspine/probe','net/minecraft'] for p in (remapped/parent).rglob('*.class')}
    entries.pop('net/mcspine/probe/RemapFacade.class',None)
    entries.pop('net/mcspine/probe/RemapFacade$1.class',None)
    entries['META-INF/services/net.fabricmc.loader.impl.game.GameProvider']=b'net.mcspine.probe.SpineGameProvider\n'
    (build/'seedprotect-bindings.json').write_text(json.dumps({'pins':pins,'class_mappings':mappings,'methods':methods},indent=2)+'\n')
    bad = build/'bindings-missing-invoke.properties'
    bad.write_text('\n'.join(k+'='+v for k,v in sorted(mappings.items()) if '.setToDirt(' not in k)+'\n')
    broken=build/'remapped-missing-invoke'
    if broken.exists():
        import shutil
        shutil.rmtree(broken)
    subprocess.run([str(java),'-Xmx128m','-cp',os.pathsep.join(map(str,[classes,*libraries])),'net.mcspine.probe.RemapFacade',str(bad),str(classes),str(broken)],check=True)
    broken_entries={str(p.relative_to(broken)):p.read_bytes() for parent in ['net/mcspine/probe','net/minecraft'] for p in (broken/parent).rglob('*.class')}
    broken_entries['META-INF/services/net.fabricmc.loader.impl.game.GameProvider']=entries['META-INF/services/net.fabricmc.loader.impl.game.GameProvider']
    return entries, broken_entries, paths['slf4j']
