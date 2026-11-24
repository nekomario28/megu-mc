// SPDX-License-Identifier: LGPL-3.0-or-later
//! Loopback proof for [`megu_proto::session::ServerSession`]: a
//! `1.21.1`/`767` client and a `26.3`/`777` client both complete status
//! ping/pong and the full Handshake -> Login -> Configuration -> Play flow
//! against one `VersionRouter::with_v767_and_v777()`, with distinct wire ids
//! and wire shapes on the play surface, and a clean `login_disconnect`
//! rejection for an unsupported protocol.

use std::sync::Arc;

use megu_proto::codec::CodecV767;
use megu_proto::router::TableDrivenCodec;
use megu_proto::session::{
    text_component, BroadcastBatch, BroadcastBatchStats, ServerSession, ServerSessionConfig,
    SessionEvent, SessionOutput, SessionPhase, SharedPacketFrame,
};
use megu_proto::table::{PacketDir, PacketPhase, ProtocolTable};
use megu_proto::version::ProtocolVersion;
use megu_proto::wire::{read_frame, write_frame, ProtoError, WireReader, WireWriter};
use megu_proto::{
    C2SPacket, EquipmentSlot, ItemStack, LoginProperty, MenuKind, ProtocolCodec, RegistryDataEntry,
    S2CPacket, TeamOperation, VersionRouter,
};

// ---------------------------------------------------------------------------
// Client-side helpers — the "other end" of the loopback, per version.
// ---------------------------------------------------------------------------

/// A client's view of the wire for one protocol: encodes C2S frames and
/// decodes S2C frames with that version's concrete codec.
enum ClientCodec {
    V767(CodecV767),
    V777(TableDrivenCodec),
}

impl ClientCodec {
    fn new(protocol: i32) -> Self {
        match protocol {
            767 => Self::V767(CodecV767::new()),
            777 => Self::V777(TableDrivenCodec::for_version(
                ProtocolVersion::V26_3,
                ProtocolTable::v777_builtin(),
            )),
            other => panic!("no test client codec for protocol {other}"),
        }
    }

    fn table(&self) -> &ProtocolTable {
        match self {
            Self::V767(c) => c.table(),
            Self::V777(c) => c.table(),
        }
    }

    fn decode_s2c(&self, phase: PacketPhase, frame: &[u8]) -> S2CPacket {
        match self {
            Self::V767(c) => c.decode_s2c_in(phase, frame).unwrap(),
            Self::V777(c) => c.decode_s2c_in(phase, frame).unwrap(),
        }
    }

    fn encode_c2s(&self, packet: &C2SPacket) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::V767(c) => c.encode_c2s(packet, &mut out).unwrap(),
            Self::V777(c) => c.encode_c2s(packet, &mut out).unwrap(),
        }
        out
    }

    /// Frame a C2S packet the canonical IR does not model (status_request,
    /// ping_request, finish_configuration has an IR form but exercises the
    /// same path here).
    fn c2s_named(&self, phase: PacketPhase, name: &str, body: &[u8]) -> Vec<u8> {
        let id = self
            .table()
            .packet_id(phase, PacketDir::Serverbound, name)
            .unwrap_or_else(|| panic!("c2s {phase}/{name} missing from client table"));
        let mut out = Vec::new();
        write_frame(&mut out, id as i32, body);
        out
    }
}

fn router() -> Arc<VersionRouter> {
    Arc::new(VersionRouter::with_v767_and_v777())
}

fn config(router: &Arc<VersionRouter>, strict: bool) -> ServerSessionConfig {
    ServerSessionConfig {
        router: router.clone(),
        motd: "dual-protocol test".to_string(),
        max_players: 20,
        compression_threshold: Some(256),
        strict_version_negotiation: strict,
    }
}

fn frames(outputs: &[SessionOutput]) -> Vec<&[u8]> {
    outputs.iter().filter_map(SessionOutput::as_frame).collect()
}

fn events(outputs: &[SessionOutput]) -> Vec<&SessionEvent> {
    outputs.iter().filter_map(SessionOutput::as_event).collect()
}

fn frame_id(frame: &[u8]) -> i32 {
    read_frame(frame).unwrap().0
}

fn frame_body(frame: &[u8]) -> Vec<u8> {
    read_frame(frame).unwrap().1.to_vec()
}

fn handshake_frame(protocol: i32, next_state: i32) -> Vec<u8> {
    let mut w = WireWriter::new();
    w.write_varint(protocol);
    w.write_string("localhost");
    w.write_u16(25565);
    w.write_varint(next_state);
    let mut out = Vec::new();
    write_frame(&mut out, 0x00, w.as_slice());
    out
}

fn login_start_body(name: &str, uuid: &[u8; 16]) -> Vec<u8> {
    let mut w = WireWriter::new();
    w.write_string(name);
    w.write_uuid(uuid);
    w.into_vec()
}

const UUID_767: [u8; 16] = [0x76; 16];
const UUID_777: [u8; 16] = [0x77; 16];
const SESSION_777: [u8; 16] = [0x5e; 16];

// ---------------------------------------------------------------------------
// Status ping/pong for both versions on one router.
// ---------------------------------------------------------------------------

#[test]
fn status_ping_pong_works_for_767_and_777() {
    let router = router();
    for protocol in [767, 777] {
        let client = ClientCodec::new(protocol);
        let mut session = ServerSession::new(config(&router, true));

        let out = session.on_frame(&handshake_frame(protocol, 1)).unwrap();
        assert_eq!(session.phase(), &SessionPhase::Status);
        match events(&out).as_slice() {
            [SessionEvent::Negotiated {
                protocol_version,
                next_state,
                matched,
                ..
            }] => {
                assert_eq!(*protocol_version, protocol);
                assert_eq!(*next_state, 1);
                assert!(matched);
            }
            other => panic!("unexpected handshake outputs: {other:?}"),
        }

        // Status Request -> Status Response advertising the matched version.
        let out = session
            .on_frame(&client.c2s_named(PacketPhase::Status, "status_request", &[]))
            .unwrap();
        let sent = frames(&out);
        assert_eq!(sent.len(), 1);
        assert_eq!(frame_id(sent[0]), 0x00, "status_response id");
        let body = frame_body(sent[0]);
        let mut r = WireReader::new(&body);
        let json = r.read_string(1 << 16).unwrap();
        r.finish().unwrap();
        let (want_name, want_protocol) = if protocol == 767 {
            ("1.21.1", "767")
        } else {
            ("26.3", "777")
        };
        assert!(
            json.contains(&format!("\"name\":\"{want_name}\"")),
            "status JSON lacks version name {want_name}: {json}"
        );
        assert!(
            json.contains(&format!("\"protocol\":{want_protocol}")),
            "status JSON lacks protocol {want_protocol}: {json}"
        );
        assert!(json.contains("\"text\":\"dual-protocol test\""), "{json}");
        assert_eq!(events(&out), &[&SessionEvent::StatusRequested]);

        // Ping Request -> Pong echoing the nonce exactly.
        let nonce: i64 = 0x1122_3344_5566_7788;
        let out = session
            .on_frame(&client.c2s_named(PacketPhase::Status, "ping_request", &nonce.to_be_bytes()))
            .unwrap();
        let sent = frames(&out);
        assert_eq!(sent.len(), 1);
        assert_eq!(frame_id(sent[0]), 0x01, "pong_response id");
        assert_eq!(frame_body(sent[0]), nonce.to_be_bytes());
        assert_eq!(events(&out), &[&SessionEvent::PingResponded { nonce }]);
    }
}

/// Non-strict mode still answers a status ping from an unregistered
/// protocol, advertising the default `1.21.1`/`767`.
#[test]
fn non_strict_status_ping_on_unknown_protocol_advertises_default() {
    let router = router();
    let mut session = ServerSession::new(config(&router, false));
    let out = session.on_frame(&handshake_frame(754, 1)).unwrap();
    assert_eq!(session.phase(), &SessionPhase::Status);
    match events(&out).as_slice() {
        [SessionEvent::Negotiated { matched: false, .. }] => {}
        other => panic!("unexpected outputs: {other:?}"),
    }
    // No codec was negotiated: ids 0x00/0x01 are the invariant status ids.
    let mut req = Vec::new();
    write_frame(&mut req, 0x00, &[]);
    let out = session.on_frame(&req).unwrap();
    let sent = frames(&out);
    assert_eq!(frame_id(sent[0]), 0x00);
    let body = frame_body(sent[0]);
    let mut r = WireReader::new(&body);
    let json = r.read_string(1 << 16).unwrap();
    assert!(json.contains("\"name\":\"1.21.1\""), "{json}");
    assert!(json.contains("\"protocol\":767"), "{json}");
}

// ---------------------------------------------------------------------------
// The full Handshake -> Login -> Configuration -> Play loopback, both
// versions interleaved on the same router.
// ---------------------------------------------------------------------------

#[test]
fn v767_and_v777_complete_full_flow_interleaved_on_one_router() {
    let router = router();
    let c767 = ClientCodec::new(767);
    let c777 = ClientCodec::new(777);
    let mut s767 = ServerSession::new(config(&router, true));
    let mut s777 = ServerSession::new(config(&router, true));

    // -- Handshake (next_state = login) -------------------------------------
    for (session, protocol) in [(&mut s767, 767), (&mut s777, 777)] {
        let out = session.on_frame(&handshake_frame(protocol, 2)).unwrap();
        match events(&out).as_slice() {
            [SessionEvent::Negotiated {
                protocol_version,
                next_state: 2,
                matched: true,
                ..
            }] => assert_eq!(*protocol_version, protocol),
            other => panic!("unexpected handshake outputs: {other:?}"),
        }
        assert_eq!(session.protocol_version(), Some(protocol));
        assert!(matches!(session.phase(), SessionPhase::Login { .. }));
    }

    // -- login_start -> LoginRequested ---------------------------------------
    let out = s767
        .on_frame(&c767.c2s_named(
            PacketPhase::Login,
            "login_start",
            &login_start_body("Steve767", &UUID_767),
        ))
        .unwrap();
    assert_eq!(
        events(&out),
        &[&SessionEvent::LoginRequested {
            username: "Steve767".to_string(),
            uuid: UUID_767,
            protocol: 767,
        }]
    );
    let out = s777
        .on_frame(&c777.c2s_named(
            PacketPhase::Login,
            "login_start",
            &login_start_body("Alex777", &UUID_777),
        ))
        .unwrap();
    assert_eq!(
        events(&out),
        &[&SessionEvent::LoginRequested {
            username: "Alex777".to_string(),
            uuid: UUID_777,
            protocol: 777,
        }]
    );

    // -- accept_login: set_compression then login_success ---------------------
    let f767 = s767.accept_login(UUID_767, "Steve767", vec![]).unwrap();
    assert_eq!(f767.len(), 2, "set_compression + login_success");
    assert_eq!(
        c767.decode_s2c(PacketPhase::Login, &f767[0]),
        S2CPacket::SetCompression { threshold: 256 }
    );
    assert_eq!(
        c767.decode_s2c(PacketPhase::Login, &f767[1]),
        S2CPacket::LoginSuccess {
            uuid: UUID_767,
            username: "Steve767".to_string(),
            properties: vec![],
        }
    );

    // 777 carries a Game-Profile session id the 767 shape lacks.
    let props = vec![LoginProperty {
        name: "textures".to_string(),
        value: "AAA=".to_string(),
        signature: None,
    }];
    let f777 = s777
        .accept_login_with_session(UUID_777, "Alex777", props.clone(), SESSION_777)
        .unwrap();
    assert_eq!(f777.len(), 2);
    assert_eq!(
        c777.decode_s2c(PacketPhase::Login, &f777[0]),
        S2CPacket::SetCompression { threshold: 256 }
    );
    let body = frame_body(&f777[1]);
    let mut r = WireReader::new(&body);
    assert_eq!(r.read_uuid().unwrap(), UUID_777);
    assert_eq!(r.read_string(16).unwrap(), "Alex777");
    let n = r.read_varint().unwrap();
    assert_eq!(n, 1);
    assert_eq!(r.read_string(64).unwrap(), "textures");
    assert_eq!(r.read_string(64).unwrap(), "AAA=");
    assert!(!r.read_bool().unwrap());
    assert_eq!(
        r.read_uuid().unwrap(),
        SESSION_777,
        "26.x session id trailer"
    );
    r.finish().unwrap();

    assert!(matches!(
        s767.phase(),
        SessionPhase::Login {
            awaiting_ack: true,
            ..
        }
    ));
    assert!(matches!(
        s777.phase(),
        SessionPhase::Login {
            awaiting_ack: true,
            ..
        }
    ));

    // -- login_acknowledged -> Configuration ----------------------------------
    for (session, client) in [(&mut s767, &c767), (&mut s777, &c777)] {
        let out = session
            .on_frame(&client.c2s_named(PacketPhase::Login, "login_acknowledged", &[]))
            .unwrap();
        assert_eq!(events(&out), &[&SessionEvent::EnteredConfiguration]);
        assert_eq!(session.phase(), &SessionPhase::Configuration);
    }

    // -- config-phase packets: keep_alive decodes, unmodelled ones forward ------
    for (session, client) in [(&mut s767, &c767), (&mut s777, &c777)] {
        let out = session
            .on_frame(&client.c2s_named(
                PacketPhase::Configuration,
                "keep_alive",
                &9i64.to_be_bytes(),
            ))
            .unwrap();
        assert_eq!(
            events(&out),
            &[&SessionEvent::Packet(C2SPacket::ConfigKeepAlive { id: 9 })]
        );
        // `client_information` is a real config packet the canonical IR does
        // not model — it forwards by name, not an error.
        let out = session
            .on_frame(&client.c2s_named(
                PacketPhase::Configuration,
                "client_information",
                &[0x01, 0x02],
            ))
            .unwrap();
        match events(&out).as_slice() {
            [SessionEvent::Unhandled { phase, name, body }] => {
                assert_eq!(*phase, PacketPhase::Configuration);
                assert_eq!(name, "client_information");
                assert_eq!(body, &[0x01, 0x02]);
            }
            other => panic!("unexpected outputs: {other:?}"),
        }
    }

    // -- registry_data + finish_configuration ---------------------------------
    let entries = vec![RegistryDataEntry {
        entry_id: "minecraft:plains".to_string(),
        data: Some(vec![10, 0]), // anonymous-NBT empty compound
    }];
    for (session, client) in [(&mut s767, &c767), (&mut s777, &c777)] {
        let frame = session
            .send_registry_data("minecraft:worldgen/biome", entries.clone())
            .unwrap();
        assert_eq!(frame_id(&frame), 0x07, "registry_data id");
        assert_eq!(
            client.decode_s2c(PacketPhase::Configuration, &frame),
            S2CPacket::ConfigRegistryData {
                registry_id: "minecraft:worldgen/biome".to_string(),
                entries: entries.clone(),
            }
        );
        let frame = session.finish_configuration().unwrap();
        assert_eq!(frame_id(&frame), 0x03, "finish_configuration id");
        assert_eq!(
            client.decode_s2c(PacketPhase::Configuration, &frame),
            S2CPacket::ConfigFinishConfiguration
        );
    }

    // -- C2S finish_configuration -> Play --------------------------------------
    for (session, client) in [(&mut s767, &c767), (&mut s777, &c777)] {
        let out = session
            .on_frame(&client.c2s_named(PacketPhase::Configuration, "finish_configuration", &[]))
            .unwrap();
        assert_eq!(events(&out), &[&SessionEvent::EnteredPlay]);
        assert_eq!(session.phase(), &SessionPhase::Play);
    }

    // -- S2C play packets: same IR, distinct wire ids ---------------------------
    let block = megu_proto::BlockPos::new(1, 64, -2);
    let title = text_component("Dual Window");
    let cases: [(S2CPacket, i32, i32, &str); 9] = [
        (S2CPacket::KeepAlive { id: 42 }, 0x26, 0x2d, "keep_alive"),
        (
            S2CPacket::BlockUpdate {
                pos: block,
                state_id: 8_596,
            },
            0x09,
            0x08,
            "block_update",
        ),
        (
            S2CPacket::ForgetLevelChunk { cx: -3, cz: 7 },
            0x21,
            0x26,
            "forget_level_chunk",
        ),
        (
            S2CPacket::TickingState {
                tick_rate: 20.0,
                is_frozen: false,
            },
            0x71,
            0x82,
            "ticking_state",
        ),
        (
            S2CPacket::TickingStep { steps: 3 },
            0x72,
            0x83,
            "ticking_step",
        ),
        (
            S2CPacket::OpenScreen {
                container_id: 1,
                menu: MenuKind::Generic9x3,
                title: title.clone(),
            },
            0x33,
            0x3c,
            "open_screen",
        ),
        (
            S2CPacket::SetEquipment {
                entity_id: 99,
                slots: vec![EquipmentSlot {
                    slot: 0,
                    item: ItemStack::empty(),
                }],
            },
            0x5b,
            0x68,
            "set_equipment",
        ),
        (
            S2CPacket::SetPassengers {
                vehicle: 7,
                passengers: vec![11, 12],
            },
            0x5f,
            0x6d,
            "set_passengers",
        ),
        (
            S2CPacket::SetPlayerTeam {
                name: "dual".to_string(),
                operation: TeamOperation::Remove,
            },
            0x60,
            0x6f,
            "set_player_team",
        ),
    ];
    for (packet, id767, id777, name) in &cases {
        let f = s767.send_play(packet).unwrap();
        assert_eq!(frame_id(&f), *id767, "767 {name}");
        assert_eq!(
            &c767.decode_s2c(PacketPhase::Play, &f),
            packet,
            "767 {name} round-trip"
        );
        let f = s777.send_play(packet).unwrap();
        assert_eq!(frame_id(&f), *id777, "777 {name}");
        assert_eq!(
            &c777.decode_s2c(PacketPhase::Play, &f),
            packet,
            "777 {name} round-trip"
        );
    }
    let f = s767
        .send_play(&S2CPacket::TabList {
            header: text_component("h"),
            footer: text_component("f"),
        })
        .unwrap();
    assert_eq!(frame_id(&f), 0x6d);
    let f = s777
        .send_play(&S2CPacket::TabList {
            header: text_component("h"),
            footer: text_component("f"),
        })
        .unwrap();
    assert_eq!(frame_id(&f), 0x7d);

    // -- C2S play packets decoded through each version's codec -----------------
    for (session, client, ka_id) in [(&mut s767, &c767, 0x18), (&mut s777, &c777, 0x1c)] {
        let out = session
            .on_frame(&client.encode_c2s(&C2SPacket::KeepAlive { id: 42 }))
            .unwrap();
        assert_eq!(
            frame_id(&client.encode_c2s(&C2SPacket::KeepAlive { id: 0 })),
            ka_id
        );
        assert_eq!(
            events(&out),
            &[&SessionEvent::Packet(C2SPacket::KeepAlive { id: 42 })]
        );
        let out = session
            .on_frame(&client.encode_c2s(&C2SPacket::PaddleBoat {
                left: true,
                right: false,
            }))
            .unwrap();
        assert_eq!(
            events(&out),
            &[&SessionEvent::Packet(C2SPacket::PaddleBoat {
                left: true,
                right: false,
            })]
        );
    }

    // player_input: two floats + flag byte on 767, one Input flag byte on 777.
    let pi767 = c767.encode_c2s(&C2SPacket::PlayerInput {
        sideways: 1.0,
        forward: -1.0,
        jump: true,
        unmount: false,
    });
    assert_eq!(frame_body(&pi767).len(), 9, "767 player_input body");
    let out = s767.on_frame(&pi767).unwrap();
    assert_eq!(
        events(&out),
        &[&SessionEvent::Packet(C2SPacket::PlayerInput {
            sideways: 1.0,
            forward: -1.0,
            jump: true,
            unmount: false,
        })]
    );

    let pi777 = c777.encode_c2s(&C2SPacket::PlayerInputFlags {
        forward: true,
        backward: false,
        left: false,
        right: false,
        jump: true,
        shift: false,
        sprint: true,
    });
    assert_eq!(frame_body(&pi777).len(), 1, "777 player_input body");
    let out = s777.on_frame(&pi777).unwrap();
    assert_eq!(
        events(&out),
        &[&SessionEvent::Packet(C2SPacket::PlayerInputFlags {
            forward: true,
            backward: false,
            left: false,
            right: false,
            jump: true,
            shift: false,
            sprint: true,
        })]
    );

    // -- Play disconnect -------------------------------------------------------
    let f = s767
        .send_play(&S2CPacket::PlayDisconnect {
            reason: text_component("bye 767"),
        })
        .unwrap();
    assert_eq!(frame_id(&f), 0x1d);
    let f = s777
        .send_play(&S2CPacket::PlayDisconnect {
            reason: text_component("bye 777"),
        })
        .unwrap();
    assert_eq!(frame_id(&f), 0x20);
}

// ---------------------------------------------------------------------------
// Unsupported protocol: login attempt -> login_disconnect 0x00 + Closed.
// ---------------------------------------------------------------------------

#[test]
fn unsupported_protocol_login_is_rejected_cleanly() {
    let router = router();
    let mut session = ServerSession::new(config(&router, true));
    let out = session.on_frame(&handshake_frame(754, 2)).unwrap();

    let sent = frames(&out);
    assert_eq!(sent.len(), 1, "exactly the disconnect frame");
    assert_eq!(frame_id(sent[0]), 0x00, "login_disconnect id");
    let body = frame_body(sent[0]);
    // The reason is an anonymous-NBT text component naming the versions.
    let mut r = WireReader::new(&body);
    let span = r.read_anonymous_nbt().unwrap().to_vec();
    r.finish().unwrap();
    let text = String::from_utf8_lossy(&span);
    assert!(text.contains("754"), "{text}");
    assert!(text.contains("1.21.1"), "{text}");
    assert!(text.contains("26.3"), "{text}");

    assert_eq!(
        events(&out),
        &[&SessionEvent::UnsupportedVersion {
            requested: 754,
            supported: vec![767, 777],
        }]
    );
    assert_eq!(session.phase(), &SessionPhase::Closed);
    assert!(session.is_closed());

    // Anything after Closed fails closed, no panic.
    assert!(session.on_frame(&handshake_frame(767, 2)).is_err());
}

// ---------------------------------------------------------------------------
// Fail-closed edges.
// ---------------------------------------------------------------------------

#[test]
fn out_of_phase_and_malformed_frames_fail_closed() {
    let router = router();
    let client = ClientCodec::new(767);
    let mut s = ServerSession::new(config(&router, true));
    s.on_frame(&handshake_frame(767, 2)).unwrap();

    // A play-phase packet id arriving during Login has no login name.
    let stray = client.encode_c2s(&C2SPacket::KeepAlive { id: 1 });
    assert!(matches!(
        s.on_frame(&stray),
        Err(ProtoError::UnknownPacketId { .. })
    ));

    // login_acknowledged before login_success is a protocol violation.
    let early = client.c2s_named(PacketPhase::Login, "login_acknowledged", &[]);
    assert!(matches!(s.on_frame(&early), Err(ProtoError::Malformed(_))));

    // App-side sends are phase-gated too.
    assert!(s.send_play(&S2CPacket::KeepAlive { id: 1 }).is_err());
    assert!(s.finish_configuration().is_err());
    assert!(s.accept_login(UUID_767, "nope", vec![]).is_err());

    // Oversized handshake string fails at the wire layer, not the session.
    let mut w = WireWriter::new();
    w.write_varint(767);
    w.write_varint(400); // claimed 400-byte address > 255-char bound
    w.raw(&[b'a'; 400]);
    w.write_u16(25565);
    w.write_varint(2);
    let mut frame = Vec::new();
    write_frame(&mut frame, 0x00, w.as_slice());
    let mut fresh = ServerSession::new(config(&router, true));
    assert!(fresh.on_frame(&frame).is_err());
}

/// `send_play` refuses to leak a non-play IR variant onto a play connection.
#[test]
fn send_play_rejects_non_play_variants() {
    let router = router();
    let client = ClientCodec::new(767);
    let mut s = ServerSession::new(config(&router, true));
    s.on_frame(&handshake_frame(767, 2)).unwrap();
    s.on_frame(&client.c2s_named(
        PacketPhase::Login,
        "login_start",
        &login_start_body("Steve767", &UUID_767),
    ))
    .unwrap();
    s.accept_login(UUID_767, "Steve767", vec![]).unwrap();
    s.on_frame(&client.c2s_named(PacketPhase::Login, "login_acknowledged", &[]))
        .unwrap();
    s.finish_configuration().unwrap();
    s.on_frame(&client.c2s_named(PacketPhase::Configuration, "finish_configuration", &[]))
        .unwrap();
    assert_eq!(s.phase(), &SessionPhase::Play);

    assert!(matches!(
        s.send_play(&S2CPacket::SetCompression { threshold: 1 }),
        Err(ProtoError::Malformed(_))
    ));
    assert!(matches!(
        s.send_play(&S2CPacket::ConfigFinishConfiguration),
        Err(ProtoError::Malformed(_))
    ));
}

fn enter_play(
    router: &Arc<VersionRouter>,
    protocol: i32,
    client_index: u8,
    compression_threshold: Option<i32>,
) -> ServerSession {
    let client = ClientCodec::new(protocol);
    let mut options = config(router, true);
    options.compression_threshold = compression_threshold;
    let mut session = ServerSession::new(options);
    let uuid = [client_index; 16];
    let name = format!("Player{client_index}");

    session.on_frame(&handshake_frame(protocol, 2)).unwrap();
    session
        .on_frame(&client.c2s_named(
            PacketPhase::Login,
            "login_start",
            &login_start_body(&name, &uuid),
        ))
        .unwrap();
    session
        .accept_login_with_session(uuid, &name, vec![], SESSION_777)
        .unwrap();
    session
        .on_frame(&client.c2s_named(PacketPhase::Login, "login_acknowledged", &[]))
        .unwrap();
    session.finish_configuration().unwrap();
    session
        .on_frame(&client.c2s_named(PacketPhase::Configuration, "finish_configuration", &[]))
        .unwrap();
    assert_eq!(session.phase(), &SessionPhase::Play);
    session
}

#[test]
fn fifty_clients_share_two_encoded_play_frames_across_767_and_777() {
    let router = router();
    let packet = S2CPacket::KeepAlive { id: 0x1234_5678 };
    let mut sessions: Vec<_> = (0..50)
        .map(|i| {
            let protocol = if i < 25 { 767 } else { 777 };
            enter_play(&router, protocol, i, Some(256))
        })
        .collect();
    let mut batch = BroadcastBatch::new(&packet);
    let delivered: Vec<SharedPacketFrame> = sessions
        .iter()
        .map(|session| batch.frame_for(session).unwrap())
        .collect();

    assert_eq!(
        batch.stats(),
        BroadcastBatchStats {
            unique_encodes: 2,
            fanout_deliveries: 50,
        }
    );
    assert!(!Arc::ptr_eq(&delivered[0].bytes, &delivered[25].bytes));

    let v767 = ClientCodec::new(767);
    let v777 = ClientCodec::new(777);
    for (i, (session, shared)) in sessions.iter_mut().zip(&delivered).enumerate() {
        let protocol = if i < 25 { 767 } else { 777 };
        let first_same_version = if protocol == 767 { 0 } else { 25 };
        assert!(Arc::ptr_eq(
            &shared.bytes,
            &delivered[first_same_version].bytes
        ));
        assert_eq!(shared.as_bytes(), session.send_play(&packet).unwrap());
        let (client, expected_id) = if protocol == 767 {
            (&v767, 0x26)
        } else {
            (&v777, 0x2d)
        };
        assert_eq!(frame_id(shared.as_bytes()), expected_id);
        assert_eq!(
            client.decode_s2c(PacketPhase::Play, shared.as_bytes()),
            packet
        );
    }
}

#[test]
fn broadcast_rejects_non_play_sessions_and_packets_without_counting_them() {
    let router = router();
    let packet = S2CPacket::KeepAlive { id: 5 };
    let mut batch = BroadcastBatch::new(&packet);
    let mut pending = ServerSession::new(config(&router, true));
    assert!(matches!(
        batch.frame_for(&pending),
        Err(ProtoError::Malformed(_))
    ));
    pending.on_frame(&handshake_frame(767, 2)).unwrap();
    assert!(matches!(
        batch.frame_for(&pending),
        Err(ProtoError::Malformed(_))
    ));

    let mut playing = enter_play(&router, 767, 1, Some(256));
    let invalid_packet = S2CPacket::SetCompression { threshold: 256 };
    let mut invalid_batch = BroadcastBatch::new(&invalid_packet);
    assert!(matches!(
        invalid_batch.frame_for(&playing),
        Err(ProtoError::Malformed(_))
    ));
    assert_eq!(invalid_batch.stats(), BroadcastBatchStats::default());

    batch.frame_for(&playing).unwrap();
    playing.close();
    assert!(matches!(
        batch.frame_for(&playing),
        Err(ProtoError::Malformed(_))
    ));
    assert_eq!(
        batch.stats(),
        BroadcastBatchStats {
            unique_encodes: 1,
            fanout_deliveries: 1,
        }
    );
}

#[test]
fn broadcast_separates_compression_variants_and_rejects_foreign_router() {
    let router = router();
    let mut plain = enter_play(&router, 767, 1, None);
    let mut compressed = enter_play(&router, 767, 2, Some(256));
    let packet = S2CPacket::TickingStep { steps: 3 };
    let mut batch = BroadcastBatch::new(&packet);

    let plain_frame = batch.frame_for(&plain).unwrap();
    let compressed_frame = batch.frame_for(&compressed).unwrap();
    assert!(!Arc::ptr_eq(&plain_frame.bytes, &compressed_frame.bytes));
    assert_eq!(plain_frame.as_bytes(), plain.send_play(&packet).unwrap());
    assert_eq!(
        compressed_frame.as_bytes(),
        compressed.send_play(&packet).unwrap()
    );

    let foreign_router = Arc::new(VersionRouter::with_v767_and_v777());
    let foreign = enter_play(&foreign_router, 767, 3, None);
    assert!(matches!(
        batch.frame_for(&foreign),
        Err(ProtoError::Malformed(_))
    ));
    assert_eq!(
        batch.stats(),
        BroadcastBatchStats {
            unique_encodes: 2,
            fanout_deliveries: 2,
        }
    );
}
