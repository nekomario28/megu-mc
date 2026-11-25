"""Authenticated, temporary Java classpath for small 1.21.1 reference probes."""
from contextlib import contextmanager
import errno
import hashlib
import io
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import zipfile

JAR_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
DEFAULT_JAR = Path(os.environ.get("MEGU_VANILLA_SERVER_JAR", str(
    Path.home() / ".cache/megu-audit/load/vanilla/server.jar")))


@contextmanager
def pinned_runtime(jar):
    # These are executable inputs: reject the wrong build before extraction or Java.
    payload = jar.read_bytes()
    if hashlib.sha1(payload).hexdigest() != JAR_SHA1:
        raise ValueError("not the pinned 1.21.1 server jar: " + str(jar))
    with tempfile.TemporaryDirectory(prefix="mc-vanilla-java-") as tmp:
        work = Path(tmp)
        with zipfile.ZipFile(io.BytesIO(payload)) as outer:
            required = sum(info.file_size for info in outer.infolist()
                           if info.filename == "META-INF/versions/1.21.1/server-1.21.1.jar"
                           or (info.filename.startswith("META-INF/libraries/")
                               and info.filename.endswith(".jar"))) + 16 * 1024 * 1024
            free = shutil.disk_usage(work).free
            if free < required:
                raise OSError(errno.ENOSPC,
                              f"native probe needs {required} bytes, only {free} free at "
                              f"{work.parent}; choose a filesystem with space via TMPDIR")
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


def run_probe(work, classpath, source, main, *args, extra_sources=(), capture=False):
    """Compile and run one probe. `capture=True` returns its stdout instead of echoing it.

    The native oracle runners need the probe's lines to validate and store them, and a
    subprocess that writes straight to our stdout cannot be parsed afterwards.
    """
    if shutil.which("javac"):
        subprocess.run(["javac", "-proc:none", "-cp", classpath, "-d", str(work), str(source),
                        *map(str, extra_sources)],
                       check=True, timeout=60)
        command = ["java", "-Xmx768m", "-cp", str(work) + os.pathsep + classpath,
                   main, *map(str, args)]
    else:
        if extra_sources:
            raise OSError("a JDK is required for multi-source Java probes")
        # Single-source probes can still run through the java launcher in
        # source-file mode. This keeps first-run data generation working on
        # installations that expose java without a separate javac executable;
        # a runtime without the compiler module fails the subprocess and the
        # caller still fails closed.
        command = ["java", "-Xmx768m", "-cp", classpath, str(source), *map(str, args)]
    if capture:
        return subprocess.run(command, check=True, timeout=300, cwd=work,
                              capture_output=True, text=True).stdout
    subprocess.run(command, check=True, timeout=120, cwd=work)
