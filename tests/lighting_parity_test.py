#!/usr/bin/env python3
"""Clean-room lighting fixture comparison against the pinned vanilla 1.21.1 jar.

Run with TMPDIR=$PWD/target/tmp. The probe starts an offline flat vanilla server
and reads protocol-767 Chunk Data light arrays; Rust computes the same fixtures
from src/lighting.rs. The flat world's four opaque floor layers are included.
No decompiled or external lighting implementation is used.
"""

import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

import protocol767
from protocol767 import Client


ROOT = Path(__file__).resolve().parents[1]
VANILLA_JAR = Path("/home/yuu/tmpx/carverfresh/server.jar")
VANILLA_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
protocol767.STRICT = False


def wait_for_server(proc, port):
    for _ in range(180):
        if proc.poll() is not None:
            raise AssertionError(f"vanilla server exited with {proc.returncode}")
        with socket.socket() as sock:
            sock.settimeout(0.2)
            if sock.connect_ex(("127.0.0.1", port)) == 0:
                return
        time.sleep(0.25)
    raise TimeoutError("vanilla server did not listen in 45 seconds")


def command(proc, action):
    proc.stdin.write(action + "\n")
    proc.stdin.flush()
    # Commands are consumed by the server thread on the next tick.
    time.sleep(1.5)


def capture(port, name):
    with Client(port, name) as client:
        client.login(view=2)
        deadline = time.monotonic() + 25
        while (0, 0) not in client.chunk_light and time.monotonic() < deadline:
            try:
                client.read(0.75)
            except TimeoutError:
                pass
        assert (0, 0) in client.chunk_light, "vanilla chunk (0,0) not delivered"
        assert (client.min_y, client.height) == (-64, 384)
        return client.chunk_light[(0, 0)]


def decode_update_light(body):
    """Decode the vanilla 1.21.1 Update Light payload from the wire."""
    reader = protocol767.Reader(body)
    chunk = (protocol767.signed(reader.varint(), 32),
             protocol767.signed(reader.varint(), 32))
    masks = []
    for _ in range(4):
        words = reader.varint()
        assert words <= 1
        mask = sum(reader.number(">Q") << (64 * i) for i in range(words))
        assert mask < (1 << 26)
        masks.append(mask)
    sections = []
    for mask in masks[:2]:
        count = reader.varint()
        assert count == mask.bit_count()
        payload = []
        for _ in range(count):
            assert reader.varint() == 2048
            payload.append(reader.take(2048))
        sections.append(payload)
    assert reader.pos == len(body)
    return chunk, masks, sections


def edit_while_tracking(port, proc, action, name):
    """Keep the target chunk ticketed while the vanilla light engine settles."""
    with Client(port, name) as client:
        client.login(view=2)
        deadline = time.monotonic() + 25
        while len(client.chunks) < 25 and time.monotonic() < deadline:
            try:
                client.read(0.75)
            except TimeoutError:
                pass
        assert (0, 0) in client.chunk_light, "target chunk was not tracked"
        command(proc, action)
        deadline = time.monotonic() + 2
        block_packets = 0
        light_packets = []
        while time.monotonic() < deadline:
            try:
                pid, body = client.read(0.3)
                if pid in (0x09, 0x49):
                    block_packets += 1
                elif pid == 0x2a:
                    light_packets.append(decode_update_light(body))
            except TimeoutError:
                pass
        print(
            f"{name}: native_block_update_packets={block_packets} "
            f"native_update_light_packets={len(light_packets)} "
            f"light_masks={[(chunk, masks) for chunk, masks, _ in light_packets]}"
        )
        assert block_packets > 0, f"{name}: client did not receive block update"
        return light_packets


def verify_registry_fixtures():
    """Use the repository's pinned-jar state facts for fixture attenuation."""
    states = json.loads((ROOT / "assets/block_states_1211.json").read_text())
    blocks = json.loads((ROOT / "assets/blocks_1211.json").read_text())
    assert states["jar_sha1"] == VANILLA_SHA1
    defaults = {block["name"]: block["defaultState"] for block in blocks}
    for name, opacity, emission in [
        ("water", 1, 0),
        ("oak_leaves", 1, 0),
        ("ice", 1, 0),
        ("oak_slab", 0, 0),
        ("oak_stairs", 0, 0),
        ("torch", 0, 14),
        ("lantern", 0, 15),
    ]:
        state = defaults[name]
        assert states["light_block"][state] == opacity, name
        assert states["luminance"][state] == emission, name


def rust_reference(tmp):
    runner = tmp / "lighting_reference.rs"
    source_path = (ROOT / "src" / "lighting.rs").as_posix()
    runner.write_text(
        f'''#[path = "{source_path}"]
#[allow(dead_code)]
mod lighting;
use lighting::{{IncrementalLighting, LightBlock, LightKind, Position}};
use std::io::Write;

fn main() {{
    let fixture = std::env::args().nth(1).expect("fixture");
    let mut blocks = Vec::new();
    let pos = Position::new(8, -40, 8);
    // The vanilla superflat preset has bedrock, two dirt layers, and grass.
    for x in 0..16 {{
        for z in 0..16 {{
            for y in -64..=-61 {{
                blocks.push((Position::new(x, y, z), LightBlock::new(15, 0)));
            }}
        }}
    }}
    // The torch/lantern have a solid supporting block in the real fixture.
    blocks.push((Position::new(8, -41, 8), LightBlock::new(15, 0)));
    match fixture.as_str() {{
        "air" => {{}},
        "torch" => blocks.push((pos, LightBlock::new(0, 14))),
        "lantern" => blocks.push((pos, LightBlock::new(0, 15))),
        "roof" | "opening" | "water" | "oak_leaves" | "ice" | "oak_slab" | "oak_stairs" => {{
            for x in 6..=10 {{
                for z in 6..=10 {{
                    if fixture != "roof" && x == 8 && z == 8 {{ continue; }}
                    blocks.push((Position::new(x, -40, z), LightBlock::new(15, 0)));
                }}
            }}
            if ["water", "oak_leaves", "ice"].contains(&fixture.as_str()) {{
                blocks.push((pos, LightBlock::new(1, 0)));
            }}
        }},
        _ => panic!("unknown fixture"),
    }}
    let mut engine = IncrementalLighting::new(-64, 384);
    engine.load_chunk(0, 0, blocks);
    let mut out = std::io::stdout();
    for kind in [LightKind::Sky, LightKind::Block] {{
        for section in engine.sections((0, 0), kind) {{
            out.write_all(&section).unwrap();
        }}
    }}
}}
'''
    )
    binary = tmp / "lighting_reference"
    subprocess.run(
        ["rustc", "--edition", "2021", "-O", "-D", "warnings", str(runner), "-o", str(binary)],
        check=True, cwd=ROOT,
    )
    return binary


def check_fixture(binary, snapshot, name):
    masks, native_sky, native_block = snapshot
    data = subprocess.check_output([str(binary), name], cwd=ROOT)
    section_count = 26
    section_bytes = 2048
    assert len(data) == 2 * section_count * section_bytes
    sky = [data[i * section_bytes:(i + 1) * section_bytes] for i in range(section_count)]
    block = [data[(section_count + i) * section_bytes:
                  (section_count + i + 1) * section_bytes] for i in range(section_count)]

    # Vanilla's initial Chunk Data packet only serializes initialized sections.
    # All nonempty block-light arrays must agree byte-for-byte, including masks.
    emitted_mask = sum(1 << i for i, arr in enumerate(block) if any(arr))
    assert masks[1] == emitted_mask, (
        f"{name}: native block mask {masks[1]:#x} != local {emitted_mask:#x}"
    )
    assert len(native_block) == masks[1].bit_count()
    for index, native in zip((i for i in range(section_count) if masks[1] & (1 << i)), native_block):
        assert native == block[index], (
            f"{name}: block section {index} native={hashlib.sha256(native).hexdigest()[:16]} "
            f"local={hashlib.sha256(block[index]).hexdigest()[:16]}"
        )

    # Vanilla omits implicit fully lit upper sky sections from initial packets.
    assert masks[0] == 0xe, f"{name}: unexpected sky mask {masks[0]:#x}"
    for index, section in enumerate((1, 2, 3)):
        assert native_sky[index] == sky[section], (
            f"{name}: sky section {section} "
            f"native={hashlib.sha256(native_sky[index]).hexdigest()[:16]} "
            f"local={hashlib.sha256(sky[section]).hexdigest()[:16]}"
        )
    print(
        f"{name}: PASS native sky_mask={masks[0]:#x} block_mask={masks[1]:#x} "
        f"matched_block_sections={masks[1].bit_count()} matched_sky_sections=3"
    )


def main():
    assert VANILLA_JAR.is_file(), f"missing reference jar {VANILLA_JAR}"
    assert hashlib.sha1(VANILLA_JAR.read_bytes()).hexdigest() == VANILLA_SHA1
    verify_registry_fixtures()
    tmpdir = Path(os.environ["TMPDIR"]).resolve()
    tmpdir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="light-vanilla-", dir=tmpdir) as work:
        tmp = Path(work)
        server_dir = tmp / "server"
        server_dir.mkdir()
        (server_dir / "server.jar").symlink_to(VANILLA_JAR)
        (server_dir / "libraries").symlink_to(VANILLA_JAR.parent / "libraries", target_is_directory=True)
        (server_dir / "eula.txt").write_text("eula=true\n")
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        (server_dir / "server.properties").write_text(
            f"server-ip=127.0.0.1\nserver-port={port}\nonline-mode=false\n"
            "enforce-secure-profile=false\nnetwork-compression-threshold=-1\n"
            "level-type=minecraft:flat\ngenerate-structures=false\n"
            "view-distance=2\nsimulation-distance=2\ndifficulty=peaceful\n"
        )
        runner = rust_reference(tmp)
        with (tmp / "vanilla.log").open("w") as log:
            proc = subprocess.Popen(
                ["java", f"-Djava.io.tmpdir={tmpdir}", "-XX:-UsePerfData",
                 "-Xms256M", "-Xmx512M", "-jar", "server.jar", "nogui"],
                stdin=subprocess.PIPE, stdout=log, stderr=subprocess.STDOUT,
                cwd=server_dir, text=True,
            )
            try:
                wait_for_server(proc, port)
                command(proc, "forceload add -16 -16 31 31")
                command(proc, "setblock 8 -41 8 minecraft:stone")
                fixtures = [
                    ("air", None),
                    ("roof", "fill 6 -40 6 10 -40 10 minecraft:stone"),
                    ("opening", "setblock 8 -40 8 minecraft:air"),
                    ("water", "setblock 8 -40 8 minecraft:water"),
                    ("ice", "setblock 8 -40 8 minecraft:ice"),
                    ("oak_leaves", "setblock 8 -40 8 minecraft:oak_leaves[persistent=true]"),
                    ("oak_slab", "setblock 8 -40 8 minecraft:oak_slab"),
                    ("oak_stairs", "setblock 8 -40 8 minecraft:oak_stairs"),
                    ("torch", "setblock 8 -40 8 minecraft:torch"),
                    ("lantern", "setblock 8 -40 8 minecraft:lantern"),
                ]
                for index, (name, action) in enumerate(fixtures):
                    if name == "torch":
                        edit_while_tracking(
                            port, proc, "fill 6 -40 6 10 -40 10 minecraft:air", "ClearRoof"
                        )
                    if action:
                        edit_while_tracking(port, proc, action, f"EditP{index}")
                    try:
                        check_fixture(runner, capture(port, f"LightP{index}"), name)
                    except AssertionError:
                        log_lines = (tmp / "vanilla.log").read_text(errors="replace").splitlines()
                        for line in log_lines:
                            if "Changed the block" in line or "Successfully filled" in line:
                                print("vanilla command log:", line)
                        raise
            finally:
                if proc.poll() is None:
                    command(proc, "stop")
                    try:
                        proc.wait(timeout=12)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait(timeout=5)
                assert proc.returncode == 0, (
                    f"vanilla exited {proc.returncode}: "
                    f"{(tmp / 'vanilla.log').read_text(errors='replace')[-1000:]}"
                )
    print(f"vanilla: PASS jar sha1={VANILLA_SHA1} fixtures=10")


if __name__ == "__main__":
    sys.exit(main())
