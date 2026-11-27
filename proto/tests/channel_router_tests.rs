// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_proto::channel_router::{
    classify_channel, decode_register_payload, encode_register_payload, validate_channel_name,
    ChannelDirection, ChannelFirewall, ChannelFirewallConfig, ChannelHandlerKind, ChannelNameError,
    DropReason, FirewallDisconnectReason, FirewallVerdict, OperatorRuleError, RegisterPayloadError,
    ThrottleReason, MAX_REGISTERED_CHANNELS_PER_PEER,
};

fn inspect(
    firewall: &mut ChannelFirewall,
    peer: u64,
    tick: u64,
    channel: &str,
    payload: &[u8],
) -> FirewallVerdict {
    firewall.inspect(peer, tick, ChannelDirection::Serverbound, channel, payload)
}

#[test]
fn known_capability_names_route_exactly() {
    let mappings = [
        ("minecraft:brand", ChannelHandlerKind::Brand),
        ("minecraft:register", ChannelHandlerKind::Register),
        ("minecraft:unregister", ChannelHandlerKind::Unregister),
        ("megu:season_v1", ChannelHandlerKind::NativeSeasonSync),
        ("megu:mod_sync_v1", ChannelHandlerKind::NativeModSync),
        ("megu:magic_v1", ChannelHandlerKind::NativeMagicSync),
        (
            "megu:accessory_v1",
            ChannelHandlerKind::NativeAccessorySync,
        ),
        ("megu:media_v1", ChannelHandlerKind::NativeMediaTransfer),
        ("megu:subworld_sync_v1", ChannelHandlerKind::NativeSubworldSync),
        ("megu:subworld_helm_v1", ChannelHandlerKind::NativeSubworldSync),
        ("megu:claims_v1", ChannelHandlerKind::NativeClaimsSync),
        (
            "openpartiesandclaims:claims_sync",
            ChannelHandlerKind::NativeClaimsSync,
        ),
        (
            "ftbchunks:claims_sync",
            ChannelHandlerKind::NativeClaimsSync,
        ),
        (
            "appleskin:exhaustion_sync",
            ChannelHandlerKind::AppleSkinHud,
        ),
        (
            "appleskin:saturation_sync",
            ChannelHandlerKind::AppleSkinHud,
        ),
        ("xaerominimap:main", ChannelHandlerKind::MapFairPlay),
        ("xaeroworldmap:main", ChannelHandlerKind::MapFairPlay),
        ("journeymap:map", ChannelHandlerKind::MapFairPlay),
    ];
    let mut firewall = ChannelFirewall::new(ChannelFirewallConfig::default());
    for (channel, kind) in mappings {
        assert_eq!(classify_channel(channel), kind);
        let payload = if matches!(
            kind,
            ChannelHandlerKind::Register | ChannelHandlerKind::Unregister
        ) {
            &b""[..]
        } else {
            &b"data"[..]
        };
        assert_eq!(
            inspect(&mut firewall, 1, 0, channel, payload),
            FirewallVerdict::Route {
                handler: kind,
                channel: channel.into(),
            }
        );
    }
    assert_eq!(firewall.metrics().routed, mappings.len() as u64);
}

#[test]
fn resource_locations_are_strictly_validated() {
    for channel in [
        "minecraft:brand",
        "megu:path/sub.file-1",
        "x:a",
        "a.b:c_d",
    ] {
        assert_eq!(validate_channel_name(channel), Ok(()));
    }
    for channel in [
        "",
        "no_namespace",
        ":foo",
        "foo:",
        "a:b:c",
        "A:b",
        "a:B",
        "a:b c",
        "a:b\0c",
        "a:b\nc",
        "a:/../b",
        "a:./b",
        "a:..",
        "..:x",
        "a:é",
    ] {
        assert!(validate_channel_name(channel).is_err(), "{channel:?}");
    }
    assert_eq!(
        validate_channel_name(&format!("a:{}", "b".repeat(126))),
        Ok(())
    );
    assert_eq!(
        validate_channel_name(&format!("a:{}", "b".repeat(127))),
        Err(ChannelNameError::TooLong)
    );
    assert_eq!(
        validate_channel_name("a:../b"),
        Err(ChannelNameError::Traversal)
    );
}

#[test]
fn register_codec_roundtrips_and_checks_all_boundaries() {
    let channels = ["minecraft:brand", "create:network", "voicechat:voice"];
    let encoded = encode_register_payload(&channels).unwrap();
    assert_eq!(encoded, b"minecraft:brand\0create:network\0voicechat:voice");
    assert_eq!(decode_register_payload(&encoded).unwrap(), channels);
    assert!(encode_register_payload::<&str>(&[]).unwrap().is_empty());
    assert_eq!(decode_register_payload(&[]).unwrap(), Vec::<String>::new());
    assert_eq!(
        decode_register_payload(b"a:b\0"),
        Err(RegisterPayloadError::EmptyChannel)
    );
    assert_eq!(
        decode_register_payload(b"a:b\0\0c:d"),
        Err(RegisterPayloadError::EmptyChannel)
    );
    assert_eq!(
        decode_register_payload(b"a:b\0a:b"),
        Err(RegisterPayloadError::DuplicateChannel)
    );
    assert_eq!(
        decode_register_payload(b"a:\xff"),
        Err(RegisterPayloadError::InvalidUtf8)
    );
    assert_eq!(
        decode_register_payload(b"a:../x"),
        Err(RegisterPayloadError::InvalidChannel(
            ChannelNameError::Traversal
        ))
    );
    assert_eq!(
        decode_register_payload(&vec![b'a'; 32_769]),
        Err(RegisterPayloadError::PayloadTooLarge)
    );
    let too_many: Vec<String> = (0..=MAX_REGISTERED_CHANNELS_PER_PEER)
        .map(|index| format!("m:n{index}"))
        .collect();
    assert_eq!(
        encode_register_payload(&too_many),
        Err(RegisterPayloadError::TooManyChannels)
    );
    assert_eq!(
        decode_register_payload(too_many.join("\0").as_bytes()),
        Err(RegisterPayloadError::TooManyChannels)
    );
    let many_long: Vec<String> = (0..MAX_REGISTERED_CHANNELS_PER_PEER)
        .map(|index| format!("x:{}{index:03}", "a".repeat(123)))
        .collect();
    assert_eq!(
        encode_register_payload(&many_long),
        Err(RegisterPayloadError::PayloadTooLarge)
    );
}

#[test]
fn registration_changes_are_atomic_and_per_peer() {
    let mut firewall = ChannelFirewall::new(ChannelFirewallConfig::default());
    let list = encode_register_payload(&["create:main", "voicechat:voice"]).unwrap();
    assert!(matches!(
        inspect(&mut firewall, 10, 0, "minecraft:register", &list),
        FirewallVerdict::Route {
            handler: ChannelHandlerKind::Register,
            ..
        }
    ));
    assert!(firewall.is_peer_channel_supported(10, "create:main"));
    assert!(!firewall.is_peer_channel_supported(11, "create:main"));
    assert!(matches!(
        inspect(
            &mut firewall,
            10,
            0,
            "minecraft:register",
            b"new:valid\0a:../bad"
        ),
        FirewallVerdict::Throttle {
            reason: ThrottleReason::InvalidRegisterPayload,
            ..
        }
    ));
    assert!(!firewall.is_peer_channel_supported(10, "new:valid"));
    assert!(matches!(
        inspect(&mut firewall, 10, 0, "minecraft:unregister", b"create:main"),
        FirewallVerdict::Route {
            handler: ChannelHandlerKind::Unregister,
            ..
        }
    ));
    assert!(!firewall.is_peer_channel_supported(10, "create:main"));
    assert!(firewall.is_peer_channel_supported(10, "voicechat:voice"));
}

#[test]
fn registration_limit_is_enforced_against_existing_channels() {
    let config = ChannelFirewallConfig {
        max_packets_per_second_per_peer: 1000,
        ..ChannelFirewallConfig::default()
    };
    let mut firewall = ChannelFirewall::new(config);
    let channels: Vec<String> = (0..256).map(|index| format!("mod:c{index}")).collect();
    let payload = encode_register_payload(&channels).unwrap();
    assert!(matches!(
        inspect(&mut firewall, 1, 0, "minecraft:register", &payload),
        FirewallVerdict::Route { .. }
    ));
    assert_eq!(
        inspect(&mut firewall, 1, 0, "minecraft:register", b"mod:extra"),
        FirewallVerdict::Throttle {
            reason: ThrottleReason::RegisteredChannelLimit,
            strikes: 1,
        }
    );
    assert!(!firewall.is_peer_channel_supported(1, "mod:extra"));
    assert!(firewall.is_peer_channel_supported(1, "mod:c255"));
}

#[test]
fn unknown_mod_channels_drop_without_kicking_a_benign_peer() {
    let mut firewall = ChannelFirewall::new(ChannelFirewallConfig::default());
    for tick in 0..100 {
        assert_eq!(
            inspect(&mut firewall, 23, tick, "anothermod:telemetry", b"ping"),
            FirewallVerdict::DropSilently {
                reason: DropReason::UnknownChannel
            }
        );
    }
    assert_eq!(firewall.peer_unknown_channel_count(23), 1);
    assert_eq!(firewall.metrics().dropped, 100);
    assert_eq!(firewall.metrics().bytes_inspected, 400);
    assert_eq!(firewall.metrics().disconnected, 0);
}

#[test]
fn oversized_serverbound_packets_are_struck_and_disconnected() {
    let config = ChannelFirewallConfig {
        strike_disconnect_threshold: 3,
        ..ChannelFirewallConfig::default()
    };
    let mut firewall = ChannelFirewall::new(config);
    let huge = vec![0xaa; 2 * 1024 * 1024];
    for strike in 1..3 {
        assert_eq!(
            inspect(&mut firewall, 1, 0, "minecraft:brand", &huge),
            FirewallVerdict::Throttle {
                reason: ThrottleReason::OversizedPayload,
                strikes: strike,
            }
        );
    }
    assert_eq!(
        inspect(&mut firewall, 1, 0, "minecraft:brand", &huge),
        FirewallVerdict::Disconnect {
            reason: FirewallDisconnectReason::OversizedPayload,
            strikes: 3,
        }
    );
    assert!(matches!(
        inspect(&mut firewall, 1, 10, "minecraft:brand", b"ok"),
        FirewallVerdict::Disconnect { .. }
    ));
    assert_eq!(firewall.metrics().disconnected, 1);
    assert_eq!(
        firewall.metrics().bytes_inspected,
        (huge.len() * 3 + 2) as u64
    );
}

#[test]
fn packet_floods_throttle_deterministically_until_disconnect() {
    let config = ChannelFirewallConfig {
        max_packets_per_second_per_peer: 2,
        strike_disconnect_threshold: 3,
        ..ChannelFirewallConfig::default()
    };
    let mut firewall = ChannelFirewall::new(config);
    for channel in ["minecraft:brand", "othermod:safe"] {
        assert!(matches!(
            inspect(&mut firewall, 5, 0, channel, b"x"),
            FirewallVerdict::Route { .. } | FirewallVerdict::DropSilently { .. }
        ));
    }
    for strikes in 1..3 {
        assert_eq!(
            inspect(&mut firewall, 5, 0, "othermod:safe", b"x"),
            FirewallVerdict::Throttle {
                reason: ThrottleReason::PacketRateExceeded,
                strikes,
            }
        );
    }
    assert_eq!(
        inspect(&mut firewall, 5, 0, "minecraft:brand", b"x"),
        FirewallVerdict::Disconnect {
            reason: FirewallDisconnectReason::PacketRateExceeded,
            strikes: 3,
        }
    );
}

#[test]
fn byte_floods_throttle_even_when_packet_budget_remains() {
    let config = ChannelFirewallConfig {
        max_packets_per_second_per_peer: 100,
        max_bytes_per_second_per_peer: 8,
        strike_disconnect_threshold: 2,
        ..ChannelFirewallConfig::default()
    };
    let mut firewall = ChannelFirewall::new(config);
    assert!(matches!(
        inspect(&mut firewall, 4, 0, "minecraft:brand", b"12345"),
        FirewallVerdict::Route { .. }
    ));
    assert_eq!(
        inspect(&mut firewall, 4, 0, "unknown:spam", b"12345"),
        FirewallVerdict::Throttle {
            reason: ThrottleReason::ByteRateExceeded,
            strikes: 1,
        }
    );
    assert_eq!(
        inspect(&mut firewall, 4, 0, "minecraft:brand", b"12345"),
        FirewallVerdict::Disconnect {
            reason: FirewallDisconnectReason::ByteRateExceeded,
            strikes: 2,
        }
    );
}

#[test]
fn unknown_channel_flood_has_a_hard_memory_limit() {
    let config = ChannelFirewallConfig {
        max_packets_per_second_per_peer: 1000,
        max_bytes_per_second_per_peer: 1_000_000,
        strike_disconnect_threshold: 2,
        max_unknown_channels_per_peer: usize::MAX,
        ..ChannelFirewallConfig::default()
    };
    let mut firewall = ChannelFirewall::new(config);
    for index in 0..MAX_REGISTERED_CHANNELS_PER_PEER {
        let channel = format!("evil:channel_{index}");
        assert_eq!(
            inspect(&mut firewall, 9, 0, &channel, b"?"),
            FirewallVerdict::DropSilently {
                reason: DropReason::UnknownChannel
            }
        );
    }
    assert_eq!(firewall.peer_unknown_channel_count(9), 256);
    assert_eq!(
        inspect(&mut firewall, 9, 0, "evil:overflow_0", b"?"),
        FirewallVerdict::Throttle {
            reason: ThrottleReason::UnknownChannelLimit,
            strikes: 1,
        }
    );
    assert_eq!(
        inspect(&mut firewall, 9, 0, "evil:overflow_1", b"?"),
        FirewallVerdict::Disconnect {
            reason: FirewallDisconnectReason::UnknownChannelLimit,
            strikes: 2,
        }
    );
    assert_eq!(firewall.peer_unknown_channel_count(9), 256);
}

#[test]
fn tokens_refill_on_ticks_and_remove_peer_releases_state() {
    let config = ChannelFirewallConfig {
        max_packets_per_second_per_peer: 2,
        max_bytes_per_second_per_peer: 8,
        strike_disconnect_threshold: 5,
        ..ChannelFirewallConfig::default()
    };
    let mut firewall = ChannelFirewall::new(config);
    let registered = encode_register_payload(&["mod:a"]).unwrap();
    // The registered list consumes 5 bytes; begin the refill scenario separately.
    assert!(matches!(
        inspect(&mut firewall, 12, 0, "minecraft:register", &registered),
        FirewallVerdict::Route { .. }
    ));
    assert!(firewall.is_peer_channel_supported(12, "mod:a"));
    assert!(matches!(
        inspect(&mut firewall, 12, 0, "minecraft:brand", b"12"),
        FirewallVerdict::Route { .. }
    ));
    assert!(matches!(
        inspect(&mut firewall, 12, 0, "minecraft:brand", b"12"),
        FirewallVerdict::Throttle {
            reason: ThrottleReason::PacketRateExceeded,
            ..
        }
    ));
    assert!(matches!(
        inspect(&mut firewall, 12, 10, "minecraft:brand", b"12"),
        FirewallVerdict::Route { .. }
    ));
    // An out-of-order tick cannot mint extra tokens.
    assert!(matches!(
        inspect(&mut firewall, 12, 9, "minecraft:brand", b"12"),
        FirewallVerdict::Throttle {
            reason: ThrottleReason::PacketRateExceeded,
            ..
        }
    ));
    assert!(firewall.remove_peer(12));
    assert!(!firewall.is_peer_channel_supported(12, "mod:a"));
    assert_eq!(firewall.peer_unknown_channel_count(12), 0);
    assert_eq!(firewall.peer_count(), 0);
    assert!(!firewall.remove_peer(12));
    assert!(matches!(
        inspect(&mut firewall, 12, 10, "minecraft:brand", b"12"),
        FirewallVerdict::Route { .. }
    ));
}

#[test]
fn operator_allowlist_and_explicit_bulk_permission_are_bounded() {
    let mut firewall = ChannelFirewall::new(ChannelFirewallConfig::default());
    assert_eq!(
        firewall.allow_bulk_channel("voicechat:audio"),
        Err(OperatorRuleError::NotAllowed)
    );
    assert_eq!(firewall.allow_operator_channel("create:*"), Ok(()));
    assert_eq!(firewall.allow_operator_channel("voicechat:audio"), Ok(()));
    assert_eq!(
        firewall.allow_operator_channel("invalid:*:*"),
        Err(OperatorRuleError::InvalidChannel(
            ChannelNameError::InvalidFormat
        ))
    );
    assert_eq!(
        firewall.handler_for("create:network"),
        ChannelHandlerKind::OperatorAllowedModChannel
    );
    assert_eq!(
        firewall.handler_for("carryon:main"),
        ChannelHandlerKind::UnknownDropSilently
    );
    assert!(matches!(
        inspect(&mut firewall, 1, 0, "create:network", b"small"),
        FirewallVerdict::Route {
            handler: ChannelHandlerKind::OperatorAllowedModChannel,
            ..
        }
    ));
    let large = vec![0x44; 64 * 1024];
    assert!(matches!(
        inspect(&mut firewall, 2, 0, "voicechat:audio", &large),
        FirewallVerdict::Throttle {
            reason: ThrottleReason::OversizedPayload,
            ..
        }
    ));
    assert_eq!(firewall.allow_bulk_channel("voicechat:audio"), Ok(()));
    assert!(matches!(
        inspect(&mut firewall, 3, 0, "voicechat:audio", &large),
        FirewallVerdict::Route {
            handler: ChannelHandlerKind::OperatorAllowedModChannel,
            ..
        }
    ));
    assert!(matches!(
        firewall.inspect(
            4,
            0,
            ChannelDirection::Clientbound,
            "megu:media_v1",
            &large
        ),
        FirewallVerdict::Route {
            handler: ChannelHandlerKind::NativeMediaTransfer,
            ..
        }
    ));
    assert!(matches!(
        inspect(&mut firewall, 5, 0, "megu:media_v1", &large),
        FirewallVerdict::Throttle {
            reason: ThrottleReason::OversizedPayload,
            ..
        }
    ));
    assert_eq!(firewall.allow_bulk_channel("megu:media_v1"), Ok(()));
    assert!(matches!(
        inspect(&mut firewall, 6, 0, "megu:media_v1", &large),
        FirewallVerdict::Route {
            handler: ChannelHandlerKind::NativeMediaTransfer,
            ..
        }
    ));
}

#[test]
fn hostile_names_and_registration_payloads_accumulate_strikes() {
    let mut firewall = ChannelFirewall::new(ChannelFirewallConfig {
        strike_disconnect_threshold: 2,
        ..ChannelFirewallConfig::default()
    });
    assert_eq!(
        inspect(&mut firewall, 1, 0, "../bad", b"x"),
        FirewallVerdict::Throttle {
            reason: ThrottleReason::MalformedChannel,
            strikes: 1,
        }
    );
    assert_eq!(
        inspect(&mut firewall, 1, 0, "minecraft:register", b"bad\0bad"),
        FirewallVerdict::Disconnect {
            reason: FirewallDisconnectReason::InvalidRegisterPayload,
            strikes: 2,
        }
    );
}
