#!/usr/bin/env python3
"""Reproducible mc-spine embedded-HotSpot baseline; artifacts stay in target/tmp."""
from __future__ import annotations

import hashlib
import json
import os
import pathlib
import shutil
import socket
import statistics
import subprocess
import sys
import time
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
TMP = ROOT / "target" / "tmp"
JAVA_HOME = pathlib.Path("/usr/lib/jvm/java-21-openjdk").resolve()
JAVA = JAVA_HOME / "bin" / "java"
JAVAC = JAVA_HOME / "bin" / "javac"
N_RUNS = 3
RSS_SETTLE_SECONDS = 12


def run(command: list[str], *, env: dict[str, str], cwd: pathlib.Path | None = None,
        capture: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, cwd=cwd or ROOT, env=env, text=True,
                          stdout=subprocess.PIPE if capture else None,
                          stderr=subprocess.STDOUT if capture else None, check=True)


def prepare_env() -> dict[str, str]:
    TMP.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["TMPDIR"] = str(TMP)
    env["JAVA_HOME"] = str(JAVA_HOME)
    jvm_lib = str(JAVA_HOME / "lib/server")
    env["LD_LIBRARY_PATH"] = jvm_lib + (":" + env["LD_LIBRARY_PATH"] if env.get("LD_LIBRARY_PATH") else "")
    # Rust's Invocation API inherits _JAVA_OPTIONS, including these flags.
    java_flags = f"-Djava.io.tmpdir={TMP} -XX:-UsePerfData"
    env["_JAVA_OPTIONS"] = (env.get("_JAVA_OPTIONS", "") + " " + java_flags).strip()
    return env


def fetch_pinned_neoforge_deps(env: dict[str, str]) -> tuple[list[pathlib.Path], pathlib.Path]:
    """Fetch only manifest-pinned runtime JARs, verify hashes, and stage in target/tmp."""
    manifest_path = ROOT / "compat/neoforge-events/dependencies.json"
    manifest = json.loads(manifest_path.read_text())
    cache = TMP / "ristretto-eval-deps"
    cache.mkdir(parents=True, exist_ok=True)
    staged: dict[str, pathlib.Path] = {}
    for name, pin in manifest.items():
        if name.endswith("_sources"):
            continue
        filename = pin["url"].rsplit("/", 1)[1]
        dest = cache / filename
        if not dest.is_file():
            request = urllib.request.Request(pin["url"], headers={"User-Agent": "mc-spine-jvm-eval/1"})
            with urllib.request.urlopen(request, timeout=45) as response:
                data = response.read(32 * 1024 * 1024 + 1)
            if len(data) > 32 * 1024 * 1024:
                raise RuntimeError(f"pinned artifact exceeds 32 MiB: {name}")
            dest.write_bytes(data)
        actual = hashlib.sha256(dest.read_bytes()).hexdigest()
        if actual != pin["sha256"]:
            raise RuntimeError(f"pinned artifact hash mismatch: {name}: {actual}")
        staged[name] = dest
    libraries = [path for name, path in staged.items() if name != "mod"]
    build = TMP / "ristretto-eval-classes"
    if build.exists():
        shutil.rmtree(build)
    classes = build / "classes"
    classes.mkdir(parents=True)
    java_sources = sorted((ROOT / "compat/neoforge-events/java").rglob("*.java"))
    java_sources.append(ROOT / "compat/fabric-probe/java/net/mcspine/probe/Host.java")
    java_sources.append(ROOT / "tools/jvm_eval/NeoForgeEvalHarness.java")
    run([str(JAVAC), "-J-Xmx128m", "--release", "21", "-proc:none", "-cp",
         os.pathsep.join(map(str, libraries)), "-d", str(classes), *map(str, java_sources)], env=env)
    shutil.copyfile(ROOT / "compat/neoforge-events/shim-surface.json",
                    classes / "net/mcspine/neoforge/shim-surface.json")
    game_jar = build / "game.jar"
    run([sys.executable, "-c", "import pathlib,sys,zipfile; r=pathlib.Path(sys.argv[1]); "
         "z=zipfile.ZipFile(sys.argv[2],'w',zipfile.ZIP_DEFLATED); "
         "[z.write(p,p.relative_to(r).as_posix()) for p in sorted(r.rglob('*')) if p.is_file()]; z.close()",
         str(classes), str(game_jar)], env=env)
    classpath = [game_jar, *libraries]
    classpath_file = build / "classpath.txt"
    classpath_file.write_text(os.pathsep.join(map(str, classpath)) + "\n")
    return classpath, classpath_file


def fetch_pinned_fabric_artifacts() -> dict[str, pathlib.Path]:
    manifest = json.loads((ROOT / "compat/fabric-probe/dependencies.json").read_text())
    cache = TMP / "ristretto-eval-fabric-deps"
    cache.mkdir(parents=True, exist_ok=True)
    artifacts: dict[str, pathlib.Path] = {}
    for item in manifest["libraries"]:
        dest = cache / item["url"].rsplit("/", 1)[1]
        if not dest.is_file():
            request = urllib.request.Request(item["url"], headers={"User-Agent": "mc-spine-jvm-eval/1"})
            with urllib.request.urlopen(request, timeout=45) as response:
                data = response.read(32 * 1024 * 1024 + 1)
            if len(data) > 32 * 1024 * 1024:
                raise RuntimeError(f"pinned artifact exceeds 32 MiB: {item['name']}")
            dest.write_bytes(data)
        actual = hashlib.sha256(dest.read_bytes()).hexdigest()
        if actual != item["sha256"]:
            raise RuntimeError(f"pinned artifact hash mismatch: {item['name']}: {actual}")
        artifacts[item["name"]] = dest
    return artifacts


def percentiles(values: list[float]) -> dict[str, float]:
    ordered = sorted(values)
    def pct(p: float) -> float:
        index = round((len(ordered) - 1) * p / 100)
        return ordered[index]
    return {"p50_ms": pct(50), "p95_ms": pct(95), "min_ms": ordered[0], "max_ms": ordered[-1]}


def tcp_ready(binary: pathlib.Path, env: dict[str, str], port: int, cwd: pathlib.Path,
              timeout: float = 90) -> tuple[float, subprocess.Popen[str], pathlib.Path]:
    log = cwd / "server.log"
    stream = log.open("w")
    start = time.perf_counter()
    process = subprocess.Popen([str(binary)], cwd=cwd, env=env, stdout=stream,
                               stderr=subprocess.STDOUT, text=True)
    deadline = start + timeout
    ready = False
    while time.perf_counter() < deadline:
        if process.poll() is not None:
            stream.close()
            raise RuntimeError(f"server exited {process.returncode}; see {log}:\n{log.read_text(errors='replace')[-2500:]}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.15):
                ready = True
                break
        except OSError:
            time.sleep(0.01)
    if not ready:
        process.terminate()
        process.wait(timeout=10)
        stream.close()
        raise TimeoutError(f"TCP port {port} did not become ready; see {log}")
    elapsed = (time.perf_counter() - start) * 1000
    # Keep the stream open until caller terminates process.
    process._eval_log_stream = stream  # type: ignore[attr-defined]
    return elapsed, process, log


def reserve_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def rss_kib(pid: int) -> dict[str, int]:
    fields = {"VmRSS": "rss_kib", "VmHWM": "hwm_kib", "RssAnon": "anon_kib", "RssFile": "file_kib"}
    result: dict[str, int] = {}
    for line in pathlib.Path(f"/proc/{pid}/status").read_text().splitlines():
        key, _, rest = line.partition(":")
        if key in fields:
            result[fields[key]] = int(rest.split()[0])
    return result


def stop(process: subprocess.Popen[str]) -> None:
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
    stream = getattr(process, "_eval_log_stream", None)
    if stream:
        stream.close()


def run_server_trials(binary: pathlib.Path, env_base: dict[str, str], *, classpath_file: pathlib.Path | None,
                      label: str, memory: bool = False) -> dict[str, object]:
    timings: list[float] = []
    memory_sample: dict[str, int] | None = None
    for trial in range(N_RUNS):
        cwd = TMP / f"server-{label}-{trial}"
        if cwd.exists():
            shutil.rmtree(cwd)
        cwd.mkdir(parents=True)
        port = reserve_port()
        env = env_base.copy()
        env["MC_SPINE_BIND_ADDR"] = "127.0.0.1"
        env["MC_SPINE_PORT"] = str(port)
        env["MC_SPINE_RCON_PORT"] = "0"
        if classpath_file:
            env["MC_SPINE_NEOFORGE_CLASSPATH"] = str(classpath_file)
        else:
            env.pop("MC_SPINE_NEOFORGE_CLASSPATH", None)
        elapsed, process, log = tcp_ready(binary, env, port, cwd)
        timings.append(elapsed)
        if memory and trial == 0:
            time.sleep(RSS_SETTLE_SECONDS)
            memory_sample = rss_kib(process.pid)
        stop(process)
        if process.returncode not in (0, -15):
            raise RuntimeError(f"server {label} exited with {process.returncode}; see {log}")
    return {"trials": timings, "startup": percentiles(timings), "rss_after_12s": memory_sample}


def main() -> int:
    env = prepare_env()
    report: dict[str, object] = {
        "revision": run(["git", "rev-parse", "HEAD"], env=env).stdout.strip(),
        "java": run([str(JAVA), "-Djava.io.tmpdir=" + str(TMP), "-XX:-UsePerfData", "-version"], env=env).stdout.strip(),
        "tmpdir": str(TMP),
        "startup_trials": N_RUNS,
        "rss_settle_seconds": RSS_SETTLE_SECONDS,
    }

    run(["cargo", "build", "--offline", "--locked", "--features", "fabric-probe,neoforge-events", "-j", "1"], env=env)
    feature_bin = TMP / "mc-spine-jvm"
    shutil.copyfile(ROOT / "target/debug/mc-spine", feature_bin)
    feature_bin.chmod(0o755)
    run(["cargo", "build", "--offline", "--locked", "--no-default-features", "-j", "1"], env=env)
    rust_bin = TMP / "mc-spine-rust"
    shutil.copyfile(ROOT / "target/debug/mc-spine", rust_bin)
    rust_bin.chmod(0o755)

    classpath, classpath_file = fetch_pinned_neoforge_deps(env)
    (TMP / "empty-mods").mkdir(exist_ok=True)
    # Harness requires the mods directory to exist before the measured call.
    bootstrap_trials: list[int] = []
    event_trials: list[dict[str, int]] = []
    for _ in range(N_RUNS):
        event_samples = run([str(JAVA), "-Djava.io.tmpdir=" + str(TMP), "-XX:-UsePerfData",
                             "-Xms16m", "-Xmx128m", "-Xcheck:jni", "-cp", os.pathsep.join(map(str, classpath)),
                             "net.mcspine.jvm_eval.NeoForgeEvalHarness", str(TMP / "empty-mods")], env=env)
        event_line = next(line for line in event_samples.stdout.splitlines() if line.startswith("bootstrap_ns="))
        fields = dict(item.split("=", 1) for item in event_line.split())
        bootstrap_trials.append(int(fields["bootstrap_ns"]))
        event_trials.append({key: int(fields[key]) for key in ("event_ns_p50", "event_ns_p95", "event_ns_p99")})
    report["bootstrap_initialize"] = {
        "first_process_ns": bootstrap_trials[0],
        "warm_process_p50_ns": int(statistics.median(bootstrap_trials[1:])),
        "process_trials_ns": bootstrap_trials,
        "definition": "first independent JVM process versus later fresh JVMs with likely warm OS file cache; not same-process repeat",
    }
    report["isolated_neoforge_eventbus_dispatch"] = {
        "samples_per_process": 50000,
        "process_trials_ns": event_trials,
        "p50_ns_median": int(statistics.median(x["event_ns_p50"] for x in event_trials)),
        "p95_ns_median": int(statistics.median(x["event_ns_p95"] for x in event_trials)),
        "p99_ns_median": int(statistics.median(x["event_ns_p99"] for x in event_trials)),
        "scope": "one warmed no-op listener for the S1 FarmlandTrampleEvent shim type on actual EventBus 8.0.2; excludes Rust-Java JNI roundtrip and mod handler work",
    }
    fabric_artifacts = fetch_pinned_fabric_artifacts()
    report["pinned_dependency_sha256"] = {
        p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in classpath[1:]
    }
    report["pinned_fabric_artifact_sha256"] = {
        name: hashlib.sha256(path.read_bytes()).hexdigest() for name, path in fabric_artifacts.items()
    }

    report["pure_rust_server"] = run_server_trials(rust_bin, env, classpath_file=None, label="pure-rust", memory=True)
    report["embedded_hotspot_server"] = run_server_trials(feature_bin, env, classpath_file=classpath_file,
                                                            label="hotspot-neoforge-empty", memory=True)
    report["rss_delta_kib"] = {
        key: report["embedded_hotspot_server"]["rss_after_12s"][key]
             - report["pure_rust_server"]["rss_after_12s"][key]
        for key in ("rss_kib", "anon_kib", "file_kib", "hwm_kib")
    }
    print(json.dumps(report, indent=2, sort_keys=True))
    (TMP / "bench_jvm_baseline.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"benchmark failed: {type(error).__name__}: {error}", file=sys.stderr)
        raise
