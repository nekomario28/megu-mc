"""Authenticated, temporary Java classpath for small 1.21.1 reference probes."""
from contextlib import contextmanager
import hashlib
import io
import os
from pathlib import Path
import subprocess
import tempfile
import zipfile

JAR_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
DEFAULT_JAR = Path(os.environ.get("MC_SPINE_VANILLA_SERVER_JAR", str(
    Path.home() / ".cache/mc-spine-audit/load/vanilla/server.jar")))


@contextmanager
def pinned_runtime(jar):
    # These are executable inputs: reject the wrong build before extraction or Java.
    payload = jar.read_bytes()
    if hashlib.sha1(payload).hexdigest() != JAR_SHA1:
        raise ValueError("not the pinned 1.21.1 server jar: " + str(jar))
    with tempfile.TemporaryDirectory(prefix="mc-vanilla-java-") as tmp:
        work = Path(tmp)
        with zipfile.ZipFile(io.BytesIO(payload)) as outer:
            inner = outer.read("META-INF/versions/1.21.1/server-1.21.1.jar")
            jars = [work / "server-game.jar"]
            jars[0].write_bytes(inner)
            # No archive-controlled extraction paths, signature stripping, or server startup.
            for i, name in enumerate(sorted(outer.namelist())):
                if name.startswith("META-INF/libraries/") and name.endswith(".jar"):
                    target = work / f"library-{i}.jar"
                    target.write_bytes(outer.read(name))
                    jars.append(target)
        yield work, os.pathsep.join(map(str, jars)), inner


def run_probe(work, classpath, source, main, *args):
    subprocess.run(["javac", "-proc:none", "-cp", classpath, "-d", str(work), str(source)],
                   check=True, timeout=60)
    subprocess.run(["java", "-Xmx768m", "-cp", str(work) + os.pathsep + classpath,
                    main, *map(str, args)], check=True, timeout=120, cwd=work)
