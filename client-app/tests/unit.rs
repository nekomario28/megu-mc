// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Unit and integration tests for mc-spine-client (CLI, redaction, Protocol 767,
//! RFC 1950 zlib, chunk decode -> client-port greedy meshing + vertex packing,
//! and client-kernel physics prediction).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;
use std::time::Duration;

use mc_spine_client::cli::{parse_args, parse_host_port, redact_from_argv, ScriptMode};
use mc_spine_client::protocol::{
    decode_chunk_packet, decode_packet_body, decode_section_blocks_update, encode_packet,
    offline_uuid, pack_position, parse_uuid, unpack_position, write_string, write_varint,
    write_varlong, zlib_compress_stored, zlib_decompress, DimensionMeta, PlayEvent, ProtocolClient,
    TeleportSync, WireReader,
};
use mc_spine_client::sim::{PlayerPhysics, TickInput};
use mc_spine_client::world::{pack_vertices, sort_translucent, BlockTable, WorldState};
use mc_spine_client_mesh::ffi::PackedVertex;

#[test]
fn cli_launcher_flags_and_token_redaction() {
    let secret_tok = "eyJhbGciOiJSUzI1NiJ9.SUPER_SECRET_TOKEN_VALUE";
    let secret_xuid = "253549999888777";
    let secret_ctok = "CLIENT_TOKEN_XYZ_987";
    let argv = vec![
        "--username",
        "Steve",
        "--uuid",
        "069a79f4-44e9-4726-a5be-fca90e38aaf5",
        "--accessToken",
        secret_tok,
        "--clientToken=CLIENT_TOKEN_XYZ_987",
        "--clientId",
        "launcher-cid",
        "--xuid",
        secret_xuid,
        "--userType",
        "msa",
        "--version",
        "1.21.1-mc-spine",
        "--versionType",
        "release",
        "--gameDir",
        "/tmp/game",
        "--assetsDir",
        "/tmp/assets",
        "--assetIndex",
        "17",
        "--quickPlayMultiplayer",
        "127.0.0.1:25569",
        "--headless",
        "--ticks",
        "25",
        "--view-distance",
        "4",
        "--script",
        "walk",
        "--json-summary",
    ];
    let cfg = parse_args(&argv).expect("parse launcher args");
    assert_eq!(cfg.username, "Steve");
    assert_eq!(
        cfg.target_endpoint().unwrap(),
        Some(("127.0.0.1".into(), 25569))
    );
    assert!(cfg.headless);
    assert_eq!(cfg.ticks, 25);
    assert_eq!(cfg.view_distance, 4);
    assert_eq!(cfg.script, ScriptMode::Walk);
    assert!(cfg.json_summary);
    assert!(cfg.has_access_token());
    assert!(cfg.has_xuid());

    // Debug representation must never leak any secret.
    let dbg = format!("{cfg:?}");
    assert!(!dbg.contains(secret_tok));
    assert!(!dbg.contains(secret_xuid));
    assert!(!dbg.contains(secret_ctok));
    assert!(dbg.contains("<REDACTED>"));

    // Arbitrary error strings and Bearer headers must be sanitized.
    let raw_err = format!("failed auth with {secret_tok}, xuid={secret_xuid}, Bearer abc.def.ghi");
    let clean = cfg.redact(&raw_err);
    assert!(!clean.contains(secret_tok));
    assert!(!clean.contains(secret_xuid));
    assert!(!clean.contains("abc.def.ghi"));
    assert!(clean.contains("Bearer <REDACTED>"));

    // Parse error referencing a secret must also be redacted.
    let bad_argv = vec!["--accessToken", secret_tok, secret_tok];
    let err = parse_args(&bad_argv).unwrap_err();
    let clean_err = redact_from_argv(&err, &bad_argv);
    assert!(!err.contains(secret_tok));
    assert!(!clean_err.contains(secret_tok));
    assert!(err.contains("<REDACTED>"));
}

#[test]
fn host_port_parsing() {
    assert_eq!(
        parse_host_port("127.0.0.1:25565", 25565).unwrap(),
        ("127.0.0.1".into(), 25565)
    );
    assert_eq!(
        parse_host_port("example.com", 25570).unwrap(),
        ("example.com".into(), 25570)
    );
    assert_eq!(
        parse_host_port("[::1]:25577", 25565).unwrap(),
        ("::1".into(), 25577)
    );
}

#[test]
fn wire_varint_varlong_position_uuid() {
    for val in [0, 1, 127, 128, 767, 26_684, -1, -2147483648, 2147483647] {
        let mut buf = Vec::new();
        write_varint(&mut buf, val);
        let mut r = WireReader::new(&buf);
        assert_eq!(r.read_varint().unwrap(), val);
        assert_eq!(r.remaining(), 0);
    }

    for val in [0i64, 1, -1, 123_456_789_012, i64::MIN, i64::MAX] {
        let mut buf = Vec::new();
        write_varlong(&mut buf, val);
        let mut r = WireReader::new(&buf);
        assert_eq!(r.read_varlong().unwrap(), val);
        assert_eq!(r.remaining(), 0);
    }

    for pos in [
        [0, 0, 0],
        [-1, -64, -1],
        [12345, 319, -54321],
        [-33554432, -2048, 33554431],
    ] {
        let packed = pack_position(pos);
        assert_eq!(unpack_position(packed), pos);
    }

    let u = offline_uuid("WireTester");
    assert_eq!(u[6] >> 4, 3);
    assert_eq!(u[8] >> 6, 2);
    assert_eq!(
        parse_uuid("069a79f4-44e9-4726-a5be-fca90e38aaf5").unwrap(),
        parse_uuid("069a79f444e94726a5befca90e38aaf5").unwrap()
    );
}

#[test]
fn rfc1950_zlib_stored_and_huffman_and_packet_framing() {
    // 1. Stored-block roundtrips (empty, small, >65535 multi-block)
    for size in [0usize, 1, 17, 4096, 70_000] {
        let data: Vec<u8> = (0..size).map(|i| ((i * 31 + 7) & 0xff) as u8).collect();
        let compressed = zlib_compress_stored(&data);
        let restored = zlib_decompress(&compressed, size + 16).expect("decompress stored");
        assert_eq!(restored, data);
    }

    // 2. Real fixed-Huffman RFC 1950 zlib stream ("hello mc-spine! " * 8, Python zlib.compress)
    let fixed_zlib: &[u8] = &[
        0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x4d, 0xd6, 0x2d, 0x2e, 0xc8, 0xcc,
        0x4b, 0x55, 0x54, 0xc8, 0xa0, 0x33, 0x1f, 0x00, 0x53, 0xfd, 0x2c, 0x89,
    ];
    let decoded = zlib_decompress(fixed_zlib, 1024).expect("decompress fixed Huffman zlib");
    assert_eq!(decoded, b"hello mc-spine! ".repeat(8));

    // 3. Real dynamic-Huffman RFC 1950 zlib stream (BTYPE=2, Python zlib.compress level=9)
    let dynamic_zlib: &[u8] = &[
        0x78, 0xda, 0xd5, 0x8d, 0x5d, 0x0e, 0xc2, 0x20, 0x10, 0x06, 0xaf, 0xb2, 0x47, 0xf0, 0xb7,
        0xa8, 0x6f, 0xed, 0x45, 0x1a, 0x64, 0x3f, 0xea, 0x46, 0x60, 0x09, 0xd0, 0x5a, 0x6f, 0x6f,
        0x3d, 0x81, 0xcf, 0x3e, 0x4e, 0x32, 0x93, 0x89, 0x92, 0xe0, 0x8a, 0xf5, 0xed, 0xa6, 0x0b,
        0xca, 0x4b, 0x4b, 0x60, 0xca, 0x45, 0x9b, 0x3a, 0x0d, 0x64, 0x3a, 0x43, 0x53, 0x01, 0xf8,
        0x4d, 0x11, 0xf5, 0x21, 0x69, 0xa2, 0x4d, 0x6a, 0x58, 0x29, 0x5b, 0xf7, 0xfc, 0x62, 0xfc,
        0xe7, 0x7c, 0xb7, 0x3f, 0x1c, 0x4f, 0xe7, 0xce, 0x5c, 0xae, 0xf6, 0xee, 0x18, 0xfe, 0x17,
        0xf7, 0xfd, 0x30, 0x90, 0x0f, 0xb3, 0xf0, 0x28, 0x31, 0xcf, 0xa1, 0x82, 0xb6, 0x51, 0x10,
        0x06, 0x55, 0xb8, 0x26, 0x9a, 0x46, 0x49, 0x8c, 0xf5, 0x03, 0x19, 0x1a, 0x7b, 0x13,
    ];
    let mut expected_dyn =
        b"minecraft:overworld protocol 767 greedy meshing vertex packing ".repeat(4);
    expected_dyn.extend_from_slice(&b"0123456789abcdef".repeat(3));
    expected_dyn.extend_from_slice(b"AABB fluid_impulse collide section_index");
    let decoded_dyn = zlib_decompress(dynamic_zlib, 1024).expect("decompress dynamic Huffman zlib");
    assert_eq!(decoded_dyn, expected_dyn);

    // 4. Uncompressed and compressed Protocol 767 frame roundtrips
    let payload = vec![0x42u8; 512];
    for comp in [None, Some(256), Some(1024), Some(0)] {
        let frame = encode_packet(0x27, &payload, comp);
        let mut r = WireReader::new(&frame);
        let len = r.read_varint().unwrap() as usize;
        let body = r.take(len).unwrap();
        let (pid, out) = decode_packet_body(body, comp).unwrap();
        assert_eq!(pid, 0x27);
        assert_eq!(out, payload);
    }
}

#[test]
fn protocol767_compressed_handshake_mock_server() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();

    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        let read_pkt = |s: &mut std::net::TcpStream, comp: Option<usize>| -> (i32, Vec<u8>) {
            let mut len = 0u32;
            for i in 0..5 {
                let mut b = [0u8; 1];
                s.read_exact(&mut b).unwrap();
                len |= u32::from(b[0] & 0x7f) << (7 * i);
                if b[0] & 0x80 == 0 {
                    break;
                }
            }
            let mut body = vec![0u8; len as usize];
            s.read_exact(&mut body).unwrap();
            decode_packet_body(&body, comp).unwrap()
        };

        // Handshake (0x00) + Login Start (0x00)
        let (pid, _) = read_pkt(&mut sock, None);
        assert_eq!(pid, 0x00);
        let (pid, _) = read_pkt(&mut sock, None);
        assert_eq!(pid, 0x00);

        // Send Set Compression (0x03) with threshold=16
        let mut sc = Vec::new();
        write_varint(&mut sc, 16);
        sock.write_all(&encode_packet(0x03, &sc, None)).unwrap();
        let comp = Some(16usize);

        // Send Login Success (0x02) compressed
        let mut ls = Vec::new();
        ls.extend_from_slice(&offline_uuid("Tester"));
        write_string(&mut ls, "Tester");
        write_varint(&mut ls, 0);
        ls.push(0);
        sock.write_all(&encode_packet(0x02, &ls, comp)).unwrap();

        // Expect Login Ack (0x03)
        let (pid, _) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x03);

        // Configuration: KeepAlive (0x04), Known Packs (0x0E), Finish Config (0x03)
        sock.write_all(&encode_packet(0x04, &42i64.to_be_bytes(), comp))
            .unwrap();
        let (pid, ka) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x04);
        assert_eq!(ka, 42i64.to_be_bytes());

        let mut kp = Vec::new();
        write_varint(&mut kp, 0);
        sock.write_all(&encode_packet(0x0e, &kp, comp)).unwrap();

        let (pid, _) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x00); // Client Info
        let (pid, _) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x07); // Known Packs

        sock.write_all(&encode_packet(0x03, &[], comp)).unwrap();
        let (pid, _) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x03); // Finish Config Ack

        // Play: KeepAlive (0x26) + Teleport (0x40)
        sock.write_all(&encode_packet(0x26, &99i64.to_be_bytes(), comp))
            .unwrap();
        let mut tp = Vec::new();
        tp.extend_from_slice(&10.5f64.to_be_bytes());
        tp.extend_from_slice(&65.0f64.to_be_bytes());
        tp.extend_from_slice(&(-4.5f64).to_be_bytes());
        tp.extend_from_slice(&0.0f32.to_be_bytes());
        tp.extend_from_slice(&0.0f32.to_be_bytes());
        tp.push(0);
        write_varint(&mut tp, 7);
        sock.write_all(&encode_packet(0x40, &tp, comp)).unwrap();

        let (pid, body) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x18);
        assert_eq!(body, 99i64.to_be_bytes());
        let (pid, body) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x00);
        assert_eq!(WireReader::new(&body).read_varint().unwrap(), 7);
    });

    let (mut cli, report) = ProtocolClient::connect(
        "127.0.0.1",
        port,
        "Tester",
        offline_uuid("Tester"),
        2,
        Duration::from_secs(5),
    )
    .expect("connect with compression");
    assert!(report.login_ok && report.config_ok);
    assert_eq!(report.compression_threshold, Some(16));

    let ev1 = cli.recv_play(Duration::from_secs(2)).unwrap().unwrap();
    match ev1 {
        PlayEvent::KeepAlive(id) => {
            assert_eq!(id, 99);
            cli.send_keep_alive(id).unwrap();
        }
        other => panic!("expected KeepAlive, got {other:?}"),
    }
    let ev2 = cli.recv_play(Duration::from_secs(2)).unwrap().unwrap();
    match ev2 {
        PlayEvent::Teleport(tp) => {
            assert_eq!(tp.teleport_id, 7);
            assert_eq!((tp.x, tp.y, tp.z), (10.5, 65.0, -4.5));
            cli.send_confirm_teleport(tp.teleport_id).unwrap();
        }
        other => panic!("expected Teleport, got {other:?}"),
    }
    server_thread.join().unwrap();
}

fn build_synthetic_chunk_packet(cx: i32, cz: i32, section_count: usize) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&cx.to_be_bytes());
    body.extend_from_slice(&cz.to_be_bytes());
    // Minimal unnamed Compound NBT heightmap: TAG_Compound (10) + TAG_End (0)
    body.push(10);
    body.push(0);

    let mut sec_bytes = Vec::new();
    for s in 0..section_count {
        if s == 4 {
            // Section y = 0..15: indirect 4-bit palette [air=0, stone=1, water=80]
            // Layer y=0 -> stone (256 cells), Layer y=1 -> water (256 cells), rest air.
            sec_bytes.extend_from_slice(&512i16.to_be_bytes());
            sec_bytes.push(4); // 4 bits/entry
            write_varint(&mut sec_bytes, 3); // palette len
            write_varint(&mut sec_bytes, 0); // idx 0 = air
            write_varint(&mut sec_bytes, 1); // idx 1 = stone
            write_varint(&mut sec_bytes, 80); // idx 2 = water
            let words_count: i32 = 4096 / 16; // 256 u64s
            write_varint(&mut sec_bytes, words_count);
            for w in 0..words_count {
                let val: u64 = if w < 16 {
                    // y=0: all 16 entries = palette index 1 (stone)
                    0x1111_1111_1111_1111
                } else if w < 32 {
                    // y=1: all 16 entries = palette index 2 (water)
                    0x2222_2222_2222_2222
                } else {
                    0
                };
                sec_bytes.extend_from_slice(&val.to_be_bytes());
            }
        } else {
            // Uniform air section
            sec_bytes.extend_from_slice(&0i16.to_be_bytes());
            sec_bytes.push(0); // 0 bits single-valued
            write_varint(&mut sec_bytes, 0); // air
            write_varint(&mut sec_bytes, 0); // 0 longs
        }
        // Biomes: single value biome 0
        sec_bytes.push(0);
        write_varint(&mut sec_bytes, 0);
        write_varint(&mut sec_bytes, 0);
    }

    write_varint(&mut body, sec_bytes.len() as i32);
    body.extend_from_slice(&sec_bytes);
    write_varint(&mut body, 0); // 0 block entities

    // Light masks: provide sky light for light section 5 (section index 4), empty block light
    write_varint(&mut body, 1);
    body.extend_from_slice(&(1u64 << 5).to_be_bytes()); // sky_mask
    write_varint(&mut body, 0); // block_mask
    write_varint(&mut body, 0); // empty_sky_mask
    write_varint(&mut body, 0); // empty_block_mask
    write_varint(&mut body, 1); // 1 sky array
    write_varint(&mut body, 2048);
    body.extend_from_slice(&[0xffu8; 2048]);
    write_varint(&mut body, 0); // 0 block arrays
    body
}

#[test]
fn chunk_decode_greedy_mesh_pack_vertices_and_sort_translucent() {
    assert_eq!(std::mem::size_of::<PackedVertex>(), 16);

    let json_path = Path::new("../assets/block_states_1211.json");
    let blocks = if json_path.is_file() {
        let t = BlockTable::from_json_file(json_path).expect("load block_states_1211.json");
        assert!(t.loaded_from_json);
        t
    } else {
        BlockTable::builtin()
    };

    let raw = build_synthetic_chunk_packet(0, 0, 24);
    let decoded = decode_chunk_packet(&raw, 24, 64).expect("decode synthetic chunk");
    assert_eq!(decoded.cx, 0);
    assert_eq!(decoded.cz, 0);
    assert_eq!(decoded.sections.len(), 24);

    let mut world = WorldState::new(DimensionMeta::default(), blocks);
    world.apply_chunk(decoded).expect("apply chunk");
    assert_eq!(world.chunks_decoded, 1);
    assert_eq!(world.get_block_state(5, 0, 5), Some(1));
    assert_eq!(world.get_block_state(5, 1, 5), Some(80));
    assert_eq!(world.get_block_state(5, 2, 5), Some(0));

    let rebuilt = world
        .rebuild_dirty_bounded(32, [8.0, 4.0, 8.0])
        .expect("rebuild dirty");
    assert!(rebuilt >= 1);
    assert!(world.metrics.total_quads > 0);
    assert_eq!(world.metrics.packed_vertices, world.metrics.total_quads * 4);
    assert!(world.metrics.translucent_quads > 0);

    // Block update (0x09) + section blocks update (0x49) dirty and remesh cleanly.
    world.apply_block_update([4, 2, 4], 1).unwrap();
    assert_eq!(world.get_block_state(4, 2, 4), Some(1));
    let mut sb = Vec::new();
    let sec_pos: i64 = 0; // (0, 0, 0)
    sb.extend_from_slice(&sec_pos.to_be_bytes());
    write_varint(&mut sb, 1);
    let rec: i64 = (1i64 << 12) | (3 << 8) | (3 << 4) | 2; // x=3, z=3, y=2 -> stone
    write_varlong(&mut sb, rec);
    let updates = decode_section_blocks_update(&sb).unwrap();
    assert_eq!(updates, vec![([3, 2, 3], 1)]);
    for (pos, st) in updates {
        world.apply_block_update(pos, st).unwrap();
    }
    assert!(world.pending_mesh_sections() > 0);
    world.rebuild_dirty_bounded(32, [8.0, 4.0, 8.0]).unwrap();
    assert_eq!(world.metrics.packed_vertices, world.metrics.total_quads * 4);

    // Direct pack_vertices & sort_translucent invariants on empty slice.
    assert!(pack_vertices(&[]).unwrap().is_empty());
    assert!(sort_translucent(&[], [0.0, 0.0, 0.0]).unwrap().is_empty());
}

#[test]
fn physics_prediction_gravity_wall_and_water_fluid_impulse() {
    let raw = build_synthetic_chunk_packet(0, 0, 24);
    let decoded = decode_chunk_packet(&raw, 24, 64).unwrap();
    let mut world = WorldState::new(DimensionMeta::default(), BlockTable::builtin());
    world.apply_chunk(decoded).unwrap();
    // Clear water at (8, 1, 8) area to create dry stone floor at y=0, plus wall at z=10
    for x in 6..=10 {
        for z in 6..=11 {
            world.apply_block_update([x, 1, z], 0).unwrap();
        }
    }
    for x in 6..=10 {
        world.apply_block_update([x, 1, 10], 1).unwrap();
        world.apply_block_update([x, 2, 10], 1).unwrap();
    }

    // 1. Gravity drop onto y=0 stone floor -> lands cleanly at y=1.0
    let mut player = PlayerPhysics::new([8.5, 4.0, 8.5], 0.0, 0.0);
    for _ in 0..20 {
        player.step(&world, TickInput::default()).unwrap();
    }
    assert!(player.on_ground);
    assert!((player.pos[1] - 1.0).abs() < 1e-9);

    // 2. Walk forward (+Z) into wall at z=10 -> collides and stops before z=9.7
    let mut hit_wall = false;
    for _ in 0..20 {
        let out = player
            .step(
                &world,
                TickInput {
                    forward: 0.2,
                    ..TickInput::default()
                },
            )
            .unwrap();
        if out.horizontal_collision {
            hit_wall = true;
        }
    }
    assert!(hit_wall);
    assert!(player.pos[2] <= 10.0 - 0.3 + 1e-6);

    // 3. Relative teleport drift tracking
    let drift = player.apply_teleport(&TeleportSync {
        x: 0.0,
        y: 2.0,
        z: 0.0,
        yaw: 0.0,
        pitch: 0.0,
        flags: 0x07, // relative XYZ
        teleport_id: 1,
    });
    assert!((drift - 2.0).abs() < 1e-9);

    // 4. Water tank immersion triggers client-kernel fluid_impulse
    for x in 1..=3 {
        for z in 1..=3 {
            for y in 1..=3 {
                world.apply_block_update([x, y, z], 80).unwrap();
            }
        }
    }
    let mut swimmer = PlayerPhysics::new([2.5, 2.0, 2.5], 0.0, 0.0);
    let out = swimmer.step(&world, TickInput::default()).unwrap();
    assert!(out.in_fluid);
    assert!(swimmer.in_water);
}
