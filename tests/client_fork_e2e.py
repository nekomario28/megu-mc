#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
"""End-to-end test of the whole Client Fork stack.

CF-0 client-kernel + CF-2 client-port + client-app megu-client are built
(if absent), exported by CF-4 export_launcher_bundle.py into a Prism instance
and a Modrinth .mrpack, and the exported megu-wrapper is then executed
exactly as Prism would (WrapperCommand + a fake JVM command line) against a
real megu server on an ephemeral loopback port.

Usage: python3 tests/client_fork_e2e.py [server_binary] [--ticks N] [--self-test]
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
import zipfile


ROOT = Path(__file__).resolve().parent.parent
LAUNCHER_DIR = ROOT / "tools/client_launcher"
EXPORTER = LAUNCHER_DIR / "export_launcher_bundle.py"
WRAPPER_SOURCE = LAUNCHER_DIR / "megu-wrapper"

MESH_NAME = "libmegu_client_mesh.so"
KERNEL_NAME = "libmegu_kernel.so"
JAR_NAME = "megu-integrated.jar"
MRPACK_NAME = "megu-1.21.1.mrpack"
LOADER = "neoforge"
LOADER_VERSION = "21.1.215"

CARGO_FLAGS = ["-j", "2", "--offline", "--locked"]
ARTIFACTS = (
    # (crate directory, artifact relative to the crate directory)
    ("client-kernel", f"target/release/{KERNEL_NAME}"),
    ("client-port", f"target/release/{MESH_NAME}"),
    ("client-app", "target/release/megu-client"),
)
SERVER_FILES = ("assets", "datapacks", "config_packets.bin")

SECRET = "SECRET_SHOULD_NOT_LEAK"
USERNAME = "ForkE2E"
FAKE_JAVA = "/opt/fake-jdk-21/bin/java"
FAKE_JVM_ARGS = [
    "-Xmx2G",
    "-XX:+UseG1GC",
    "-Djava.library.path=/nonexistent/natives",
    "--add-opens", "java.base/java.lang=ALL-UNNAMED",
    "-cp", "/nonexistent/libraries/a.jar:/nonexistent/libraries/b.jar",
]
MAIN_CLASS = "net.minecraft.client.main.Main"
SHA256_RE = re.compile(r"[0-9a-fA-F]{64}")
PRIVATE_KEYS = ("--client-bin", "--client-sha256")
INTEGRITY_REJECTED = 2

SERVER_READY_TIMEOUT = 180
CLIENT_TIMEOUT = 600
BUILD_TIMEOUT = 7200


class E2EFailure(AssertionError):
    pass


def check(condition, message):
    if not condition:
        raise E2EFailure(message)


def log(message):
    print(f"[client-fork-e2e] {message}", flush=True)


# ---------------------------------------------------------------- helpers

def file_hash(path, algorithm="sha256"):
    digest = hashlib.new(algorithm)
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def parse_wrapper_command(cfg_text):
    """Return (wrapper_path, private_tokens, private_options) from instance.cfg.

    Prism splits WrapperCommand like a shell command line (double quotes keep
    paths containing spaces together). Only the private wrapper flags may
    follow the wrapper path; a missing or malformed SHA-256 pin is an error.
    """
    values = []
    for line in cfg_text.splitlines():
        key, equals, value = line.partition("=")
        if equals and key.strip() == "WrapperCommand":
            values.append(value.strip())
    check(len(values) == 1, f"instance.cfg must have exactly one WrapperCommand, found {len(values)}")
    try:
        tokens = shlex.split(values[0], posix=True)
    except ValueError as error:
        raise E2EFailure(f"WrapperCommand is not splittable: {error}") from error
    check(tokens, "WrapperCommand is empty")
    wrapper, private = tokens[0], tokens[1:]
    parsed = {}
    index = 0
    while index < len(private):
        key, equals, value = private[index].partition("=")
        check(key in PRIVATE_KEYS, f"unexpected WrapperCommand token {private[index]!r}")
        check(key not in parsed, f"duplicate {key} in WrapperCommand")
        if not equals:
            check(index + 1 < len(private), f"missing value for {key} in WrapperCommand")
            index += 1
            value = private[index]
        check(value, f"empty value for {key} in WrapperCommand")
        parsed[key] = value
        index += 1
    check("--client-sha256" in parsed, "WrapperCommand does not pin --client-sha256")
    check(SHA256_RE.fullmatch(parsed["--client-sha256"]) is not None,
          "WrapperCommand --client-sha256 is not 64 hexadecimal characters")
    return wrapper, private, parsed


def flip_byte(source, destination, offset=None):
    """Copy an executable and invert exactly one byte (default: the middle one)."""
    data = bytearray(Path(source).read_bytes())
    check(data, f"cannot tamper with empty file {source}")
    if offset is None:
        offset = len(data) // 2
    data[offset] ^= 0xFF
    destination = Path(destination)
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(bytes(data))
    destination.chmod(0o755)
    return offset


def assert_no_secret(secret, **streams):
    for name, text in streams.items():
        check(secret not in (text or ""), f"secret token leaked into {name}")


def game_arguments(port, ticks, token=SECRET):
    return [
        "--username", USERNAME,
        "--accessToken", token,
        "--headless",
        "--quickPlayMultiplayer", f"127.0.0.1:{port}",
        "--ticks", str(ticks),
        "--view-distance", "2",
        "--script", "walk",
        "--json-summary",
    ]


def wrapper_argv(wrapper, private_tokens, client_bin, game):
    return [str(wrapper), *private_tokens, "--client-bin", str(client_bin),
            FAKE_JAVA, *FAKE_JVM_ARGS, MAIN_CLASS, *game]


def wrapper_env():
    env = dict(os.environ)
    env.pop("MEGU_CLIENT_BIN", None)
    env.pop("MEGU_CLIENT_SHA256", None)
    return env


def run(argv, timeout, cwd=None, env=None):
    return subprocess.run(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          timeout=timeout, check=False,
                          encoding="utf-8", errors="replace")


def parse_summary(stdout):
    """Return the last JSON object printed on its own line(s) in stdout."""
    lines = stdout.splitlines()
    for end in range(len(lines) - 1, -1, -1):
        if not lines[end].rstrip().endswith("}"):
            continue
        for start in range(end, -1, -1):
            if not lines[start].lstrip().startswith("{"):
                continue
            try:
                value = json.loads("\n".join(lines[start:end + 1]))
            except ValueError:
                continue
            if isinstance(value, dict):
                return value
    raise E2EFailure("client printed no JSON summary on stdout")


def summary_field(summary, key):
    if key in summary:
        return summary[key]
    for value in summary.values():
        if isinstance(value, dict) and key in value:
            return value[key]
    raise E2EFailure(f"JSON summary lacks {key!r}")


def is_int(value):
    return isinstance(value, int) and not isinstance(value, bool)


def check_summary(summary):
    for key in ("login_ok", "config_ok", "play_ok"):
        check(summary_field(summary, key) is True, f"{key} is not true")
    chunks = summary_field(summary, "chunks_decoded")
    quads = summary_field(summary, "total_quads")
    vertices = summary_field(summary, "packed_vertices")
    corrections = summary_field(summary, "corrections_0x40")
    for key, value in (("chunks_decoded", chunks), ("total_quads", quads),
                       ("packed_vertices", vertices), ("corrections_0x40", corrections)):
        check(is_int(value), f"{key} is not an integer: {value!r}")
    check(chunks >= 25, f"chunks_decoded {chunks} < 25 (view distance 2 = 5x5)")
    check(quads > 0, "total_quads is 0: no geometry was meshed")
    check(vertices == quads * 4, f"packed_vertices {vertices} != total_quads*4 ({quads * 4})")
    check(corrections == 0, f"server sent {corrections} 0x40 position corrections")


def tail(text, lines=40):
    return "\n".join((text or "").splitlines()[-lines:])


# ---------------------------------------------------------------- build

def ensure_artifacts(root):
    paths = {}
    for crate, relative in ARTIFACTS:
        artifact = root / crate / relative
        if not artifact.is_file():
            log(f"building {crate} (release)")
            subprocess.run(["cargo", "build", *CARGO_FLAGS, "--release"], cwd=root / crate,
                           check=True, timeout=BUILD_TIMEOUT)
        check(artifact.is_file(), f"build did not produce {artifact}")
        paths[artifact.name] = artifact.resolve()
    return paths


def ensure_server(server, explicit):
    if server.is_file():
        return server.resolve()
    check(not explicit, f"server binary {server} does not exist")
    log("building megu (debug)")
    subprocess.run(["cargo", "build", *CARGO_FLAGS], cwd=ROOT, check=True, timeout=BUILD_TIMEOUT)
    check(server.is_file(), f"build did not produce {server}")
    return server.resolve()


# ---------------------------------------------------------------- export

def export_bundle(out_dir, client_bin, mesh, kernel):
    argv = [sys.executable, str(EXPORTER), "--format", "all", "--out-dir", str(out_dir),
            "--loader", LOADER, "--loader-version", LOADER_VERSION,
            "--client-bin", str(client_bin), "--mesh-library", str(mesh),
            "--kernel-library", str(kernel)]
    result = run(argv, timeout=BUILD_TIMEOUT, cwd=ROOT)
    check(result.returncode == 0,
          f"exporter exited {result.returncode}\n{tail(result.stdout)}\n{tail(result.stderr)}")


def verify_prism(prism, client_bin, mesh, kernel):
    expected = {
        "megu-wrapper": WRAPPER_SOURCE,
        "bin/megu-client": client_bin,
        f"native/{MESH_NAME}": mesh,
        f"native/{KERNEL_NAME}": kernel,
    }
    cfg = prism / "instance.cfg"
    check(cfg.is_file(), "prism/instance.cfg missing")
    for relative, source in expected.items():
        exported = prism / relative
        check(exported.is_file(), f"prism/{relative} missing")
        check(file_hash(exported) == file_hash(source), f"prism/{relative} hash mismatch")
    for relative in ("megu-wrapper", "bin/megu-client"):
        check(os.access(prism / relative, os.X_OK), f"prism/{relative} is not executable")

    wrapper, private, parsed = parse_wrapper_command(cfg.read_text(encoding="utf-8"))
    check(Path(wrapper).resolve() == (prism / "megu-wrapper").resolve(),
          f"WrapperCommand points at {wrapper}, not the exported wrapper")
    pinned = parsed["--client-sha256"].lower()
    check(pinned == file_hash(prism / "bin/megu-client"),
          "WrapperCommand --client-sha256 does not match exported bin/megu-client")
    check(pinned == file_hash(client_bin), "WrapperCommand pin does not match built client")
    return Path(wrapper), private, pinned


def verify_mrpack(out_dir, prism, mesh, kernel):
    bundle = out_dir / MRPACK_NAME
    check(bundle.is_file(), f"{MRPACK_NAME} missing")
    with zipfile.ZipFile(bundle) as archive:
        check(archive.testzip() is None, f"{MRPACK_NAME} has a corrupt entry")
        index = json.loads(archive.read("modrinth.index.json"))
        bundled = index.get("megu", {}).get("bundledOverrides")
        check(isinstance(bundled, dict), "modrinth.index.json lacks megu.bundledOverrides")
        check(index.get("dependencies", {}).get(LOADER) == LOADER_VERSION,
              "modrinth.index.json does not pin the NeoForge loader version")
        sources = {
            f"megu/native/{MESH_NAME}": Path(mesh).read_bytes(),
            f"megu/native/{KERNEL_NAME}": Path(kernel).read_bytes(),
            f"mods/{JAR_NAME}": (prism / ".minecraft/mods" / JAR_NAME).read_bytes(),
        }
        for path, content in sources.items():
            entry = bundled.get(path)
            check(isinstance(entry, dict), f"bundledOverrides lacks {path}")
            hashes = entry.get("hashes", {})
            check(hashes.get("sha1") == hashlib.sha1(content).hexdigest(),
                  f"bundledOverrides SHA-1 mismatch for {path}")
            check(hashes.get("sha512") == hashlib.sha512(content).hexdigest(),
                  f"bundledOverrides SHA-512 mismatch for {path}")
            check(entry.get("fileSize") == len(content), f"bundledOverrides size mismatch for {path}")
            check(archive.read(f"overrides/{path}") == content,
                  f"overrides/{path} content differs from the source artifact")


# ---------------------------------------------------------------- server

def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def start_server(server, workdir, port, flat):
    for name in SERVER_FILES:
        source = ROOT / name
        check(source.exists(), f"server input {source} missing")
        (workdir / name).symlink_to(source.resolve())
    env = dict(os.environ)
    for key in ("MEGU_RCON_PORT", "MEGU_RCON_PASS", "MEGU_WG_PROFILE"):
        env.pop(key, None)
    env.update(MEGU_PORT=str(port), MEGU_COMPRESSION_THRESHOLD="64")
    if flat:
        env["MEGU_WG"] = "old"
    else:
        env.pop("MEGU_WG", None)
        env["MEGU_SEED"] = "0"
    log_path = workdir / "server.log"
    stream = log_path.open("wb")
    process = subprocess.Popen([str(server)], cwd=workdir, env=env, stdin=subprocess.DEVNULL,
                               stdout=stream, stderr=subprocess.STDOUT)
    stream.close()
    deadline = time.monotonic() + SERVER_READY_TIMEOUT
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise E2EFailure(f"server exited {process.returncode} before listening\n"
                             + tail(log_path.read_text(errors="replace")))
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                return process, log_path
        except OSError:
            time.sleep(0.25)
    stop_server(process)
    raise E2EFailure(f"server did not listen on 127.0.0.1:{port} within {SERVER_READY_TIMEOUT}s")


def stop_server(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


# ---------------------------------------------------------------- scenarios

def run_client(wrapper, private, prism, port, ticks):
    argv = wrapper_argv(wrapper, private, prism / "bin/megu-client", game_arguments(port, ticks))
    result = run(argv, timeout=CLIENT_TIMEOUT, cwd=prism, env=wrapper_env())
    assert_no_secret(SECRET, stdout=result.stdout, stderr=result.stderr)
    check(result.returncode == 0,
          f"wrapped client exited {result.returncode}\n{tail(result.stdout)}\n{tail(result.stderr)}")
    summary = parse_summary(result.stdout)
    check_summary(summary)
    return summary


def run_tamper(wrapper, private, prism, scratch, port, ticks):
    tampered = scratch / "tampered/megu-client"
    flip_byte(prism / "bin/megu-client", tampered)
    check(file_hash(tampered) != file_hash(prism / "bin/megu-client"), "tampering had no effect")
    argv = wrapper_argv(wrapper, private, tampered, game_arguments(port, ticks))
    result = run(argv, timeout=60, cwd=prism, env=wrapper_env())
    assert_no_secret(SECRET, stdout=result.stdout, stderr=result.stderr)
    check(result.returncode == INTEGRITY_REJECTED,
          f"tampered client: wrapper exited {result.returncode}, expected {INTEGRITY_REJECTED}")
    check(result.stdout == "", "tampered client produced stdout: it was executed")
    check("SHA-256 mismatch" in result.stderr, "wrapper did not report the SHA-256 mismatch")


def e2e(server_arg, ticks, flat, keep):
    explicit = server_arg is not None
    server = ensure_server(Path(server_arg or ROOT / "target/debug/megu"), explicit)
    artifacts = ensure_artifacts(ROOT)
    client_bin = artifacts["megu-client"]
    mesh, kernel = artifacts[MESH_NAME], artifacts[KERNEL_NAME]

    scratch = Path(tempfile.mkdtemp(prefix="client-fork-e2e-"))
    process = None
    try:
        out_dir = scratch / "export"
        export_bundle(out_dir, client_bin, mesh, kernel)
        prism = out_dir / "prism"
        wrapper, private, pinned = verify_prism(prism, client_bin, mesh, kernel)
        log(f"prism instance verified (client sha256 {pinned})")
        verify_mrpack(out_dir, prism, mesh, kernel)
        log(f"{MRPACK_NAME} bundledOverrides verified")

        server_dir = scratch / "server"
        server_dir.mkdir()
        port = free_port()
        process, server_log = start_server(server, server_dir, port, flat)
        log(f"megu listening on 127.0.0.1:{port}")
        summary = run_client(wrapper, private, prism, port, ticks)
        log("client summary: " + json.dumps(summary, sort_keys=True))
        run_tamper(wrapper, private, prism, scratch, port, ticks)
        log("tampered client rejected with exit code 2")
        check(process.poll() is None,
              f"server died during the test (exit {process.returncode})\n"
              + tail(server_log.read_text(errors="replace")))
    finally:
        if process is not None:
            stop_server(process)
        if keep:
            log(f"kept {scratch}")
        else:
            shutil.rmtree(scratch, ignore_errors=True)
    log("PASS")


# ---------------------------------------------------------------- self-test

def expect_failure(function, *args, **kwargs):
    try:
        function(*args, **kwargs)
    except E2EFailure:
        return
    raise E2EFailure(f"{function.__name__}{args!r} unexpectedly passed")


def self_test():
    sha = "ab" * 32
    wrapper, private, parsed = parse_wrapper_command(
        "[General]\nInstanceType=OneSix\nname=megu 1.21.1\nOverrideCommands=true\n"
        f'WrapperCommand="/inst dir/prism/megu-wrapper" --client-sha256 {sha}\n')
    check(wrapper == "/inst dir/prism/megu-wrapper", f"bad wrapper path {wrapper!r}")
    check(private == ["--client-sha256", sha], f"bad private tokens {private!r}")
    check(parsed == {"--client-sha256": sha}, f"bad parsed options {parsed!r}")
    _, _, parsed = parse_wrapper_command(
        f"WrapperCommand=/w --client-bin=/x/megu-client --client-sha256={sha.upper()}")
    check(parsed["--client-bin"] == "/x/megu-client", "--key=value form not parsed")
    for bad in ("[General]\nname=x\n",
                'WrapperCommand="/w"\n',
                "WrapperCommand=/w --client-sha256 1234\n",
                f"WrapperCommand=/w --client-sha256 {sha} --evil 1\n",
                "WrapperCommand=/w --client-sha256\n",
                f'WrapperCommand="/w --client-sha256 {sha}\n',
                f"WrapperCommand=/w --client-sha256 {sha}\nWrapperCommand=/v\n"):
        expect_failure(parse_wrapper_command, bad)

    good = {"login_ok": True, "config_ok": True, "play_ok": True, "chunks_decoded": 25,
            "total_quads": 10, "packed_vertices": 40, "corrections_0x40": 0}
    check_summary(parse_summary("log line\n{not json}\n" + json.dumps(good, indent=2) + "\nbye\n"))
    check_summary({"summary": good})
    for key, value in (("login_ok", 1), ("play_ok", False), ("chunks_decoded", 24),
                       ("total_quads", 0), ("packed_vertices", 41), ("corrections_0x40", 1),
                       ("total_quads", True)):
        broken = dict(good, **{key: value})
        if key == "total_quads" and value == 0:
            broken["packed_vertices"] = 0
        expect_failure(check_summary, broken)
    missing = dict(good)
    del missing["corrections_0x40"]
    expect_failure(check_summary, missing)
    expect_failure(parse_summary, "no summary here\n")

    assert_no_secret(SECRET, stdout="ok", stderr="Bearer <REDACTED>")
    expect_failure(assert_no_secret, SECRET, stdout="", stderr=f"token={SECRET}")
    expect_failure(assert_no_secret, SECRET, stdout=f"x{SECRET}x", stderr="")

    with tempfile.TemporaryDirectory(prefix="client-fork-e2e-self-") as scratch:
        scratch = Path(scratch)
        original = scratch / "orig"
        original.write_bytes(bytes(range(256)) * 4)
        tampered = scratch / "copy/tampered"
        offset = flip_byte(original, tampered)
        a, b = original.read_bytes(), tampered.read_bytes()
        check(len(a) == len(b), "tampering changed the size")
        check([i for i in range(len(a)) if a[i] != b[i]] == [offset], "not exactly one byte flipped")
        check(file_hash(original) != file_hash(tampered), "hash unchanged after tampering")
        check(stat.S_IMODE(tampered.stat().st_mode) == 0o755, "tampered copy not executable")
        if WRAPPER_SOURCE.is_file():
            self_test_wrapper(scratch)
        else:
            log(f"skipping wrapper subprocess checks: {WRAPPER_SOURCE} missing")
    log("self-test PASS")


def self_test_wrapper(scratch):
    """Drive the real wrapper with a fake (script) client: no compiled binaries."""
    prism = scratch / "prism dir"
    (prism / "bin").mkdir(parents=True)
    wrapper = prism / "megu-wrapper"
    shutil.copyfile(WRAPPER_SOURCE, wrapper)
    wrapper.chmod(0o755)
    client = prism / "bin/megu-client"
    client.write_text(f"#!{sys.executable}\nimport json, sys\n"
                      "print(json.dumps({'argv': sys.argv[1:]}))\n", encoding="utf-8")
    client.chmod(0o755)
    cfg = f'WrapperCommand="{wrapper}" --client-sha256 {file_hash(client)}\n'
    wrapper_path, private, _ = parse_wrapper_command(cfg)
    game = game_arguments(25565, 3)

    result = run(wrapper_argv(wrapper_path, private, client, game), 60, cwd=prism, env=wrapper_env())
    check(result.returncode == 0, f"wrapper rejected pinned client: {result.stderr}")
    check(parse_summary(result.stdout)["argv"] == game,
          "wrapper did not forward exactly the game arguments")

    tampered = scratch / "tampered/megu-client"
    flip_byte(client, tampered, offset=len(client.read_bytes()) - 2)
    for token_args in (game, [arg.replace("--accessToken", f"--accessToken={SECRET}")
                              if arg == "--accessToken" else arg
                              for arg in game if arg != SECRET]):
        result = run(wrapper_argv(wrapper_path, private, tampered, token_args), 60,
                     cwd=prism, env=wrapper_env())
        check(result.returncode == INTEGRITY_REJECTED,
              f"tampered client: wrapper exited {result.returncode}")
        check(result.stdout == "", "tampered client was executed")
        check("SHA-256 mismatch" in result.stderr, "mismatch not reported")
        assert_no_secret(SECRET, stdout=result.stdout, stderr=result.stderr)

    # A failing diagnostic that would echo the token must come out redacted.
    result = run([str(wrapper), "--client-bin", str(client), f"Bearer {SECRET}",
                  "--accessToken", SECRET], 60, cwd=prism, env=wrapper_env())
    check(result.returncode == INTEGRITY_REJECTED, "non-java invocation was accepted")
    assert_no_secret(SECRET, stdout=result.stdout, stderr=result.stderr)


# ---------------------------------------------------------------- main

def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("server_binary", nargs="?",
                        help="megu server binary (default: target/debug/megu)")
    parser.add_argument("--ticks", type=int, default=20, help="client ticks to run (default 20)")
    parser.add_argument("--flat", action="store_true",
                        help="use the old flat generator (MEGU_WG=old) instead of seed 0")
    parser.add_argument("--keep-temp", action="store_true", help="keep the scratch directory")
    parser.add_argument("--self-test", action="store_true",
                        help="offline checks of the parser and tamper/redaction helpers")
    args = parser.parse_args()
    if args.ticks <= 0:
        parser.error("--ticks must be positive")
    try:
        if args.self_test:
            self_test()
        else:
            e2e(args.server_binary, args.ticks, args.flat, args.keep_temp)
    except (E2EFailure, subprocess.CalledProcessError, subprocess.TimeoutExpired,
            OSError, ValueError, KeyError, zipfile.BadZipFile) as error:
        print(f"[client-fork-e2e] FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
