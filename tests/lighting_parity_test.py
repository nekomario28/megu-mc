#!/usr/bin/env python3
"""Clean-room lighting fixture comparison against the pinned vanilla 1.21.1 jar.

Run with TMPDIR=$PWD/target/tmp. The probe starts an offline flat vanilla server
and reads protocol-767 Chunk Data and Update Light packets. Rust computes the
same fixtures from src/lighting.rs, including the four opaque floor layers.
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
VANILLA_JAR = Path(os.environ.get("MEGU_VANILLA_JAR", str(Path.home() / ".cache/megu-audit/load/server.jar")))
VANILLA_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
JAVA = "/usr/lib/jvm/java-21-openjdk/bin/java"
SECTION_COUNT = 26
SECTION_BYTES = 2048
ZERO_SECTION = bytes(SECTION_BYTES)
TARGET = (0, 0)
# Protocol-767 state IDs checked against assets/blocks_1211.json.
REQUIRED_BLOCK_UPDATES = {
    "removed_lantern": {(8, -40, 8): 0},
    "directional_wall": {
        (7, -40, 8): 2891,  # bottom north outer_left, dry
        (8, -39, 8): 2929,  # bottom west inner_right, dry
    },
}
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


def track_target(client):
    client.login(view=2)
    deadline = time.monotonic() + 25
    while TARGET not in client.chunks and time.monotonic() < deadline:
        try:
            client.read(0.75)
        except TimeoutError:
            pass
    assert TARGET in client.chunks, "vanilla chunk (0,0) not delivered"
    assert (client.min_y, client.height) == (-64, 384)
    assert client.pos is not None and (client.pos[0] // 16, client.pos[2] // 16) == (3, 0)
    assert (-1, 0) not in client.chunks, "target must be on the tracked border"
    return client.chunk_light[TARGET]


def capture(port, name):
    with Client(port, name) as client:
        return track_target(client)


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
        assert mask < (1 << SECTION_COUNT)
        masks.append(mask)
    assert not masks[0] & masks[2] and not masks[1] & masks[3]
    sections = []
    for mask in masks[:2]:
        count = reader.varint()
        assert count == mask.bit_count()
        payload = []
        for _ in range(count):
            assert reader.varint() == SECTION_BYTES
            payload.append(reader.take(SECTION_BYTES))
        sections.append(payload)
    assert reader.pos == len(body)
    return chunk, masks, sections


def edit_while_tracking(port, proc, action, name, expected_blocks=None):
    """Keep the target chunk ticketed while the vanilla light engine settles."""
    with Client(port, name) as client:
        before = track_target(client)
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
        if expected_blocks:
            placed = dict(client.updates)
            for pos, state in expected_blocks.items():
                assert placed.get(pos) == state, (
                    f"{name}: native state at {pos} is {placed.get(pos)}, expected {state}"
                )
        return before, light_packets


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
use lighting::{{FaceOcclusion, Facing, Half, IncrementalLighting, LightBlock, LightKind,
    Position, StairShape}};
use std::io::Write;

fn slab(half: Half) -> LightBlock {{
    LightBlock::with_occlusion(0, 0, FaceOcclusion::slab(half))
}}

fn stairs(half: Half, facing: Facing, shape: StairShape) -> LightBlock {{
    LightBlock::with_occlusion(0, 0, FaceOcclusion::stairs(half, facing, shape))
}}

fn main() {{
    let fixture = std::env::args().nth(1).expect("fixture");
    let mut blocks = Vec::new();
    let mut edits = Vec::new();
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
        "torch" => edits.push((pos, LightBlock::new(0, 14))),
        "lantern" | "removed_lantern" => edits.push((pos, LightBlock::new(0, 15))),
        "directional_wall" => edits.extend([
            (pos, LightBlock::new(0, 14)),
            (Position::new(9, -40, 8), slab(Half::Bottom)),
            (Position::new(10, -40, 8), slab(Half::Top)),
            (Position::new(8, -40, 9), stairs(Half::Bottom, Facing::North, StairShape::Straight)),
            (Position::new(8, -40, 7), stairs(Half::Top, Facing::South, StairShape::Straight)),
            (Position::new(7, -40, 7), stairs(Half::Bottom, Facing::West, StairShape::Straight)),
            (Position::new(7, -40, 8), stairs(Half::Bottom, Facing::North, StairShape::OuterLeft)),
            (Position::new(9, -39, 8), stairs(Half::Bottom, Facing::North, StairShape::Straight)),
            (Position::new(8, -39, 8), stairs(Half::Bottom, Facing::West, StairShape::InnerRight)),
        ]),
        "roof" | "opening" | "water" | "oak_leaves" | "ice" | "oak_slab" | "oak_stairs"
        | "top_slab" | "double_slab" | "top_stairs" => {{
            for x in 6..=10 {{
                for z in 6..=10 {{
                    if fixture != "roof" && x == 8 && z == 8 {{ continue; }}
                    blocks.push((Position::new(x, -40, z), LightBlock::new(15, 0)));
                }}
            }}
            let opening = match fixture.as_str() {{
                "water" | "oak_leaves" | "ice" => LightBlock::new(1, 0),
                "oak_slab" => slab(Half::Bottom),
                "top_slab" => slab(Half::Top),
                "double_slab" => LightBlock::with_occlusion(15, 0, FaceOcclusion::FULL),
                "oak_stairs" => stairs(Half::Bottom, Facing::North, StairShape::Straight),
                "top_stairs" => stairs(Half::Top, Facing::West, StairShape::Straight),
                _ => LightBlock::default(),
            }};
            if opening != LightBlock::default() {{
                edits.push((pos, opening));
            }}
        }},
        _ => panic!("unknown fixture"),
    }}
    let mut engine = IncrementalLighting::new(-64, 384);
    engine.load_chunk(0, 0, blocks);
    for (at, block) in edits {{
        engine.edit(at, block).unwrap();
    }}
    let removal = (fixture == "removed_lantern").then(|| {{
        assert_eq!(engine.level(LightKind::Block, pos), 15);
        engine.edit(pos, LightBlock::default()).unwrap()
    }});
    let mut out = std::io::stdout();
    for kind in [LightKind::Sky, LightKind::Block] {{
        for section in engine.sections((0, 0), kind) {{
            out.write_all(&section).unwrap();
        }}
    }}
    if let Some(removal) = removal {{
        let [chunk] = removal.chunks.as_slice() else {{ panic!("expected one changed chunk") }};
        assert_eq!(chunk.chunk, (0, 0));
        assert_eq!(chunk.sky_mask | chunk.empty_sky_mask, 0);
        assert!(chunk.block_sections.is_empty());
        out.write_all(&chunk.block_mask.to_le_bytes()).unwrap();
        out.write_all(&chunk.empty_block_mask.to_le_bytes()).unwrap();
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


def read_reference(binary, name):
    data = subprocess.check_output([str(binary), name], cwd=ROOT)
    array_bytes = 2 * SECTION_COUNT * SECTION_BYTES
    assert len(data) == array_bytes + (16 if name == "removed_lantern" else 0)
    arrays = [data[i:i + SECTION_BYTES] for i in range(0, array_bytes, SECTION_BYTES)]
    return arrays[:SECTION_COUNT], arrays[SECTION_COUNT:], data[array_bytes:]


def indexed_sections(mask, payloads):
    assert len(payloads) == mask.bit_count()
    return dict(zip((i for i in range(SECTION_COUNT) if mask & (1 << i)), payloads))


def assert_section(native, local, name, kind, section):
    if native != local:
        byte = next(i for i, (a, b) in enumerate(zip(native, local)) if a != b)
        shift = 0 if (native[byte] ^ local[byte]) & 15 else 4
        cell = byte * 2 + shift // 4
        pos = (cell % 16, -80 + section * 16 + cell // 256, cell // 16 % 16)
        raise AssertionError(
            f"{name}: {kind} section {section} at {pos} "
            f"native={(native[byte] >> shift) & 15} local={(local[byte] >> shift) & 15}"
        )


def check_fixture(snapshot, reference, name):
    masks, native_sky, native_block = snapshot
    sky, block, _ = reference

    # A vanilla data bit can carry an allocated array whose nibbles are all zero.
    native_block = indexed_sections(masks[1], native_block)
    emitted_mask = sum(1 << i for i, arr in enumerate(block) if any(arr))
    wire_mask = 0xe if name == "removed_lantern" else emitted_mask
    assert masks[1] == wire_mask, (
        f"{name}: native block mask {masks[1]:#x} != expected {wire_mask:#x}"
    )
    for index, local in enumerate(block):
        assert_section(native_block.get(index, ZERO_SECTION), local, name, "block", index)

    # Vanilla omits implicit fully lit upper sky sections from initial packets.
    assert masks[0] == 0xe, f"{name}: unexpected sky mask {masks[0]:#x}"
    for section, native in indexed_sections(masks[0], native_sky).items():
        assert_section(native, sky[section], name, "sky", section)
    print(
        f"{name}: PASS native sky_mask={masks[0]:#x} block_mask={masks[1]:#x} "
        f"empty_block_mask={masks[3]:#x} matched_block_sections={masks[1].bit_count()} "
        "matched_sky_sections=3"
    )


def check_removal(lit, settled, updates, reference):
    lit_masks, _, lit_payloads = lit
    final_masks, _, final_payloads = settled
    before = indexed_sections(lit_masks[1], lit_payloads)
    final = indexed_sections(final_masks[1], final_payloads)
    assert lit_masks[1] == 0xe and all(arr != ZERO_SECTION for arr in before.values())
    assert (final_masks[1], final_masks[3]) == (0xe, 0x1)
    assert all(arr == ZERO_SECTION for arr in final.values())

    # Replay the actual Update Light payloads on the tracking client's lit state.
    # Empty bits and materialized zero arrays both clear the client section.
    replayed = before.copy()
    wire_block_mask = wire_empty_mask = 0
    for chunk, masks, (_, payload) in updates:
        if chunk != TARGET:
            continue
        wire_block_mask |= masks[1]
        wire_empty_mask |= masks[3]
        replayed.update(indexed_sections(masks[1], payload))
        replayed.update(indexed_sections(masks[3], [ZERO_SECTION] * masks[3].bit_count()))
    assert (wire_block_mask, wire_empty_mask) == (0xe, 0), (
        f"unexpected target Update Light block masks {wire_block_mask:#x}/{wire_empty_mask:#x}"
    )
    assert all(replayed.get(i, ZERO_SECTION) == final.get(i, ZERO_SECTION)
               for i in range(SECTION_COUNT)), "Update Light replay differs from fresh chunk"

    changed = 0
    canonical_empty = 0
    for i in range(SECTION_COUNT):
        after = replayed.get(i, ZERO_SECTION)
        if after != before.get(i, ZERO_SECTION):
            changed |= 1 << i
            if after == ZERO_SECTION:
                canonical_empty |= 1 << i
    local_block = int.from_bytes(reference[2][:8], "little")
    local_empty = int.from_bytes(reference[2][8:], "little")
    assert changed == local_block | local_empty == 0xe
    assert canonical_empty == local_empty == 0xe and local_block == 0
    print(
        f"removed_lantern: PASS update block_mask={wire_block_mask:#x} "
        f"empty_block_mask={wire_empty_mask:#x} changed_block_sections={changed:#x} "
        f"canonical_empty_block_mask={canonical_empty:#x}"
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
                [JAVA, f"-Djava.io.tmpdir={tmpdir}", "-XX:-UsePerfData",
                 "-Xms256M", "-Xmx512M", "-jar", "server.jar", "nogui"],
                stdin=subprocess.PIPE, stdout=log, stderr=subprocess.STDOUT,
                cwd=server_dir, text=True,
            )
            try:
                wait_for_server(proc, port)
                command(proc, "forceload add -16 -16 31 31")
                # Vanilla broadcasts Update Light to clients on the tracked
                # border. At view distance two, (0,0) is on that border from (3,0).
                command(proc, "gamerule spawnRadius 0")
                command(proc, "setworldspawn 56 -60 8")
                command(proc, "setblock 8 -41 8 minecraft:stone")
                fixtures = [
                    ("air", None),
                    ("roof", "fill 6 -40 6 10 -40 10 minecraft:stone"),
                    ("opening", "setblock 8 -40 8 minecraft:air"),
                    ("water", "setblock 8 -40 8 minecraft:water"),
                    ("ice", "setblock 8 -40 8 minecraft:ice"),
                    ("oak_leaves", "setblock 8 -40 8 minecraft:oak_leaves[persistent=true]"),
                    ("oak_slab", "setblock 8 -40 8 minecraft:oak_slab[type=bottom]"),
                    ("oak_stairs", "setblock 8 -40 8 minecraft:oak_stairs[half=bottom,facing=north]"),
                    ("top_slab", "setblock 8 -40 8 minecraft:oak_slab[type=top]"),
                    ("double_slab", "setblock 8 -40 8 minecraft:oak_slab[type=double]"),
                    ("top_stairs", "setblock 8 -40 8 minecraft:oak_stairs[half=top,facing=west]"),
                    ("torch", "setblock 8 -40 8 minecraft:torch"),
                    ("lantern", "setblock 8 -40 8 minecraft:lantern"),
                    ("removed_lantern", "setblock 8 -40 8 minecraft:air"),
                    ("directional_wall", "\n".join([
                        "setblock 8 -40 8 minecraft:torch",
                        "setblock 9 -40 8 minecraft:oak_slab[type=bottom]",
                        "setblock 10 -40 8 minecraft:oak_slab[type=top]",
                        "setblock 8 -40 9 minecraft:oak_stairs[half=bottom,facing=north]",
                        "setblock 8 -40 7 minecraft:oak_stairs[half=top,facing=south]",
                        "setblock 7 -40 7 minecraft:oak_stairs[half=bottom,facing=west]",
                        "setblock 7 -40 8 minecraft:oak_stairs[half=bottom,facing=north,shape=outer_left]",
                        "setblock 9 -39 8 minecraft:oak_stairs[half=bottom,facing=north]",
                        "setblock 8 -39 8 minecraft:oak_stairs[half=bottom,facing=west,shape=inner_right]",
                    ])),
                ]
                lit = None
                for index, (name, action) in enumerate(fixtures):
                    if name == "torch":
                        edit_while_tracking(
                            port, proc, "fill 6 -40 6 10 -40 10 minecraft:air", "ClearRoof"
                        )
                    try:
                        if action:
                            tracked_before, updates = edit_while_tracking(
                                port, proc, action, f"EditP{index}",
                                REQUIRED_BLOCK_UPDATES.get(name),
                            )
                        snapshot = capture(port, f"LightP{index}")
                        reference = read_reference(runner, name)
                        check_fixture(snapshot, reference, name)
                        if name == "lantern":
                            lit = snapshot
                        if name == "removed_lantern":
                            assert tracked_before == lit, "removal client did not start lit"
                            check_removal(tracked_before, snapshot, updates, reference)
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
    print(f"vanilla: PASS jar sha1={VANILLA_SHA1} fixtures={len(fixtures)}")


if __name__ == "__main__":
    sys.exit(main())
