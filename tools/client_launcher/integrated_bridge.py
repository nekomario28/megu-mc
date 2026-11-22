#!/usr/bin/env python3
"""Run megu for a local client and inject its Quick Play multiplayer address.

Usage: python3 integrated_bridge.py --game-dir DIR [--world NAME] -- CLIENT [ARGS...]
The default world is megu, overridable with MEGU_INTEGRATED_WORLD.
Set MEGU_SERVER_BIN for an explicit server executable and MEGU_SERVER_HOME
for its runtime data (config_packets.bin, assets, and optional datapacks/vanilla).
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time


LOOPBACK = "127.0.0.1"
QUICK_PLAY_TARGETS = {"--quickPlayMultiplayer", "--quickPlaySingleplayer", "--quickPlayRealms"}


def server_binary(game_dir):
    configured = os.environ.get("MEGU_SERVER_BIN")
    candidates = [configured] if configured else [
        str(game_dir / "megu"), str(game_dir / "bin" / "megu"), "megu",
    ]
    for candidate in candidates:
        found = shutil.which(candidate)
        if found:
            return Path(found).resolve()
    raise ValueError("megu server not found or not executable (MEGU_SERVER_BIN, --gameDir, PATH)")


def link_runtime_data(world, binary, game_dir):
    configured = os.environ.get("MEGU_SERVER_HOME")
    candidates = [Path(configured).resolve()] if configured else [
        game_dir, Path.cwd(), *binary.parents,
    ]
    home = next((root for root in candidates
                 if (root / "config_packets.bin").is_file() and (root / "assets").is_dir()), None)
    if home is None and configured:
        raise ValueError("MEGU_SERVER_HOME must contain config_packets.bin and assets")
    if home is not None:
        for name in ("config_packets.bin", "assets", "datapacks/vanilla"):
            source, target = home / name, world / name
            if (source.exists() and not target.exists() and not target.is_symlink()
                    and not target.parent.is_symlink()):
                target.parent.mkdir(exist_ok=True)
                target.symlink_to(source, target_is_directory=source.is_dir())


def client_command(client, endpoint):
    # Minecraft launchers may already supply a Quick Play target. Keep its path
    # and other game arguments unchanged and give it a single loopback target.
    args = iter(client[1:])
    command = [client[0]]
    for arg in args:
        if arg in QUICK_PLAY_TARGETS:
            next(args, None)
        elif arg.partition("=")[0] not in QUICK_PLAY_TARGETS:
            command.append(arg)
    return [*command, "--quickPlayMultiplayer", endpoint]


def stop_process(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def log_server(server, endpoint, ready):
    banner = f" listening {endpoint} "
    with server.stdout:
        for line in server.stdout:
            if line.startswith("megu ") and banner in line:
                ready.set()
            print(line, end="", file=sys.stderr, flush=True)


def handoff_json(command, endpoint):
    return json.dumps({"mode": "integrated", "argumentCount": len(command) - 1,
                       "endpoint": endpoint}) + "\n"


def write_manifest(process, command, endpoint):
    # Legacy opt-in only: the default channel is the MEGU_HANDOFF_JSON
    # environment variable, so launches create zero temporary files.
    if os.environ.get("MEGU_HANDOFF_FILE") != "1":
        return
    destination = os.environ.get("MEGU_HANDOFF_MANIFEST")
    if not destination:
        return
    path = Path(destination).absolute()
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(dir=path.parent, prefix=".megu-handoff-")
    temp = Path(name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump({"mode": "integrated", "pid": process.pid,
                       "argumentCount": len(command) - 1, "endpoint": endpoint}, stream)
            stream.write("\n")
        temp.replace(path)
    finally:
        temp.unlink(missing_ok=True)


def run_server(binary, world, client, stopping):
    # The server announces its configured port, so select a free one ourselves.
    # Waiting for this child's banner avoids handing off to a different process
    # if it binds the port in the gap between selection and server startup.
    with socket.socket() as reserved:
        reserved.bind((LOOPBACK, 0))
        port = reserved.getsockname()[1]
    endpoint = f"{LOOPBACK}:{port}"
    env = dict(os.environ, MEGU_BIND_ADDR=LOOPBACK, MEGU_PORT=str(port),
               MEGU_WORLD_DIR=str(world), MEGU_RESOURCE_PACK_URL="",
               MEGU_MAX_CONN_PER_IP="1")
    env.pop("MEGU_RCON_PORT", None)
    server = subprocess.Popen([str(binary)], cwd=world, env=env, stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                              encoding="utf-8", errors="replace")
    ready = threading.Event()
    logs = threading.Thread(target=log_server, args=(server, endpoint, ready), daemon=True)
    try:
        logs.start()
        deadline = time.monotonic() + 30
        while not stopping():
            if server.poll() is not None:
                raise ValueError(f"server exited during startup ({server.returncode})")
            if ready.wait(0.05):
                break
            if time.monotonic() >= deadline:
                raise ValueError(f"server did not become ready at {endpoint}")
        if stopping():
            return 1
        if server.poll() is not None:
            raise ValueError(f"server exited during startup ({server.returncode})")
        command = client_command(client, endpoint)
        client_env = dict(os.environ,
                          MEGU_HANDOFF_JSON=handoff_json(command, endpoint))
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, env=client_env)
        try:
            write_manifest(process, command, endpoint)
            while not stopping():
                try:
                    result = process.wait(timeout=0.1)
                    return result if result >= 0 else 128 - result
                except subprocess.TimeoutExpired:
                    if server.poll() is not None:
                        raise ValueError(f"server exited while client was running ({server.returncode})")
            return 1
        finally:
            stop_process(process)
    finally:
        stop_process(server)
        if logs.ident is not None:
            logs.join(timeout=1)


def lock_world(lock):
    try:
        if os.name == "nt":
            import msvcrt
            msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as error:
        raise ValueError("this integrated world is already in use") from error


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--game-dir", type=Path, required=True)
    parser.add_argument("--world", default=os.environ.get("MEGU_INTEGRATED_WORLD", "megu"))
    parser.add_argument("--parent-stdin", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("client", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    client = args.client[1:] if args.client[:1] == ["--"] else args.client
    if not client:
        parser.error("a client command is required")
    if args.world in ("", ".", "..") or any(char in args.world for char in "/\\:"):
        parser.error("--world must be a single directory name")

    stopped = False

    def stop(*_):
        nonlocal stopped
        stopped = True

    def watch_parent():
        os.read(sys.stdin.fileno(), 1)
        stop()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    if args.parent_stdin:
        threading.Thread(target=watch_parent, daemon=True).start()
    try:
        game_dir = args.game_dir.resolve()
        binary = server_binary(game_dir)
        saves = (game_dir / "saves").resolve()
        world = (saves / args.world).resolve()
        if world.parent != saves:
            raise ValueError("integrated world must be inside the saves directory")
        world.mkdir(parents=True, exist_ok=True)
        with (world / ".megu-integrated.lock").open("ab") as lock:
            lock_world(lock)
            link_runtime_data(world, binary, game_dir)
            return run_server(binary, world, client, lambda: stopped)
    except (OSError, ValueError) as error:
        print(f"megu integrated: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
