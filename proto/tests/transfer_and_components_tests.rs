// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_proto::item_components::{
    plain_text_component, sanitize_and_roundtrip_components, sanitize_text_component,
    AttributeModifier, BannerPattern, BannerPatternKind, BeeData, BlockPredicate, BlockProperty,
    BlockPropertyValue, BookPage, Enchantment, FireworkExplosion, Food, FoodEffect, Holder, IdSet,
    Instrument, JukeboxPlayable, JukeboxSong, JukeboxSongRef, LodestoneTracker, PotionContents,
    PotionEffect, PotionEffectDetails, Profile, ProfileProperty, SoundEvent, Tool, ToolRule, Trim,
    TrimMaterial, TrimPattern, VerifiedComponent, WrittenBook, WrittenPage, MAX_COMPONENT_BYTES,
    MAX_CONTAINER_SLOTS, MAX_LIST_ENTRIES, MAX_STACK_COMPONENTS_BYTES,
};
use megu_proto::table::{PacketDir, PacketPhase, ProtocolTable};
use megu_proto::wire::{read_frame, write_frame, BlockPos, WireReader, WireWriter};
use megu_proto::{
    C2SPacket, CodecV767, EquipmentSlot, ItemStack, ProtocolCodec, RawComponent, S2CPacket,
    MAX_COOKIE_PAYLOAD_BYTES,
};

fn framed(phase: PacketPhase, direction: PacketDir, name: &str, body: &[u8]) -> Vec<u8> {
    let id = ProtocolTable::v767_builtin()
        .packet_id(phase, direction, name)
        .unwrap();
    let mut frame = Vec::new();
    write_frame(&mut frame, id as i32, body);
    frame
}

fn test_s2c(packet: S2CPacket, phase: PacketPhase, name: &str, expected_id: i32) {
    let mut bytes = Vec::new();
    let codec = CodecV767::new();
    codec.encode_s2c(&packet, &mut bytes).unwrap();
    assert_eq!(read_frame(&bytes).unwrap().0, expected_id);
    assert_eq!(
        ProtocolTable::v767_builtin().packet_id(phase, PacketDir::Clientbound, name),
        Some(expected_id as u32)
    );
    assert_eq!(codec.decode_s2c_in(phase, &bytes).unwrap(), packet);
    for cut in 0..bytes.len() {
        assert!(
            codec.decode_s2c_in(phase, &bytes[..cut]).is_err(),
            "cut={cut}"
        );
    }
    bytes.push(0);
    assert!(codec.decode_s2c_in(phase, &bytes).is_err());
}

fn test_c2s(packet: C2SPacket, phase: PacketPhase, expected_id: i32) {
    let mut bytes = Vec::new();
    let codec = CodecV767::new();
    codec.encode_c2s(&packet, &mut bytes).unwrap();
    assert_eq!(read_frame(&bytes).unwrap().0, expected_id);
    assert_eq!(codec.decode_c2s_in(phase, &bytes).unwrap(), packet);
    for cut in 0..bytes.len() {
        assert!(
            codec.decode_c2s_in(phase, &bytes[..cut]).is_err(),
            "cut={cut}"
        );
    }
    bytes.push(0);
    assert!(codec.decode_c2s_in(phase, &bytes).is_err());
}

#[test]
fn cookie_and_transfer_v767_phase_ids_and_roundtrips() {
    let key = "minecraft:gateway".to_owned();
    let max_cookie = vec![0xa5; MAX_COOKIE_PAYLOAD_BYTES];
    for (phase, s2c_id, c2s_id) in [
        (PacketPhase::Login, 0x05, 0x04),
        (PacketPhase::Configuration, 0x00, 0x01),
        (PacketPhase::Play, 0x16, 0x11),
    ] {
        let request = match phase {
            PacketPhase::Login => S2CPacket::LoginCookieRequest { key: key.clone() },
            PacketPhase::Configuration => S2CPacket::ConfigCookieRequest { key: key.clone() },
            PacketPhase::Play => S2CPacket::PlayCookieRequest { key: key.clone() },
            _ => unreachable!(),
        };
        test_s2c(request, phase, "cookie_request", s2c_id);
        for payload in [None, Some(Vec::new()), Some(max_cookie.clone())] {
            let response = match phase {
                PacketPhase::Login => C2SPacket::LoginCookieResponse {
                    key: key.clone(),
                    payload,
                },
                PacketPhase::Configuration => C2SPacket::ConfigCookieResponse {
                    key: key.clone(),
                    payload,
                },
                PacketPhase::Play => C2SPacket::PlayCookieResponse {
                    key: key.clone(),
                    payload,
                },
                _ => unreachable!(),
            };
            test_c2s(response, phase, c2s_id);
        }
    }

    for (phase, store_id, transfer_id) in [
        (PacketPhase::Configuration, 0x0a, 0x0b),
        (PacketPhase::Play, 0x6b, 0x73),
    ] {
        let store = match phase {
            PacketPhase::Configuration => S2CPacket::ConfigStoreCookie {
                key: key.clone(),
                payload: max_cookie.clone(),
            },
            _ => S2CPacket::PlayStoreCookie {
                key: key.clone(),
                payload: max_cookie.clone(),
            },
        };
        test_s2c(store, phase, "store_cookie", store_id);
        let transfer = match phase {
            PacketPhase::Configuration => S2CPacket::ConfigTransfer {
                host: "example.net".into(),
                port: 25565,
            },
            _ => S2CPacket::PlayTransfer {
                host: "example.net".into(),
                port: 25565,
            },
        };
        test_s2c(transfer, phase, "transfer", transfer_id);
    }
}

#[test]
fn cookie_transfer_reject_bad_lengths_hosts_ports_and_keys() {
    let codec = CodecV767::new();
    for packet in [
        S2CPacket::ConfigStoreCookie {
            key: "minecraft:k".into(),
            payload: vec![0; MAX_COOKIE_PAYLOAD_BYTES + 1],
        },
        S2CPacket::PlayStoreCookie {
            key: "minecraft:k".into(),
            payload: vec![0; MAX_COOKIE_PAYLOAD_BYTES + 1],
        },
        S2CPacket::ConfigTransfer {
            host: String::new(),
            port: 25565,
        },
        S2CPacket::PlayTransfer {
            host: "x".into(),
            port: 0,
        },
        S2CPacket::PlayTransfer {
            host: "x".into(),
            port: 70000,
        },
        S2CPacket::PlayTransfer {
            host: "a".repeat(256),
            port: 25565,
        },
        S2CPacket::PlayTransfer {
            host: "host/path".into(),
            port: 25565,
        },
        S2CPacket::LoginCookieRequest {
            key: "BAD:KEY".into(),
        },
        S2CPacket::ConfigCookieRequest {
            key: "foo::bar".into(),
        },
    ] {
        assert!(
            codec.encode_s2c(&packet, &mut Vec::new()).is_err(),
            "{packet:?}"
        );
    }
    for phase in [
        PacketPhase::Login,
        PacketPhase::Configuration,
        PacketPhase::Play,
    ] {
        let packet = match phase {
            PacketPhase::Login => C2SPacket::LoginCookieResponse {
                key: "minecraft:k".into(),
                payload: Some(vec![0; 5121]),
            },
            PacketPhase::Configuration => C2SPacket::ConfigCookieResponse {
                key: "minecraft:k".into(),
                payload: Some(vec![0; 5121]),
            },
            _ => C2SPacket::PlayCookieResponse {
                key: "minecraft:k".into(),
                payload: Some(vec![0; 5121]),
            },
        };
        assert!(codec.encode_c2s(&packet, &mut Vec::new()).is_err());
        let mut w = WireWriter::new();
        w.write_string("minecraft:k");
        w.write_bool(true);
        w.write_bytes(&vec![0; MAX_COOKIE_PAYLOAD_BYTES + 1]);
        let bytes = framed(
            phase,
            PacketDir::Serverbound,
            "cookie_response",
            w.as_slice(),
        );
        assert!(codec.decode_c2s_in(phase, &bytes).is_err());
        let mut invalid = WireWriter::new();
        invalid.write_string("minecraft:k");
        invalid.write_u8(2);
        assert!(codec
            .decode_c2s_in(
                phase,
                &framed(
                    phase,
                    PacketDir::Serverbound,
                    "cookie_response",
                    invalid.as_slice()
                )
            )
            .is_err());
    }
    for phase in [PacketPhase::Configuration, PacketPhase::Play] {
        let mut w = WireWriter::new();
        w.write_string("minecraft:k");
        w.write_bytes(&vec![0; 5121]);
        assert!(codec
            .decode_s2c_in(
                phase,
                &framed(phase, PacketDir::Clientbound, "store_cookie", w.as_slice())
            )
            .is_err());
        for (host, port) in [
            ("", 80),
            ("localhost", 0),
            ("localhost", 70000),
            ("bad host", 42),
        ] {
            let mut w = WireWriter::new();
            w.write_string(host);
            w.write_varint(port);
            assert!(codec
                .decode_s2c_in(
                    phase,
                    &framed(phase, PacketDir::Clientbound, "transfer", w.as_slice())
                )
                .is_err());
        }
    }
}

fn example_components() -> Vec<VerifiedComponent> {
    let text = plain_text_component("Creative item").unwrap();
    let explosion = FireworkExplosion {
        shape: 2,
        colors: vec![0x00335577],
        fade_colors: vec![0x00aabbcc],
        trail: true,
        twinkle: false,
    };
    let enchant = Enchantment { id: 1, level: 3 };
    let effect = PotionEffect {
        id: 2,
        amplifier: 1,
        duration: 100,
        ambient: false,
        show_particles: true,
        show_icon: true,
        hidden: Some(Box::new(PotionEffectDetails {
            amplifier: 0,
            duration: 40,
            ambient: true,
            show_particles: true,
            show_icon: false,
            hidden: None,
        })),
    };
    let nested = ItemStack {
        item_id: 1,
        count: 1,
        added: vec![VerifiedComponent::RepairCost(3).to_raw().unwrap()],
        removed: vec![4],
    };
    vec![
        VerifiedComponent::CustomName(text.clone()),
        VerifiedComponent::ItemName(text.clone()),
        VerifiedComponent::Lore(vec![
            text.clone(),
            vec![0],
            plain_text_component("Second").unwrap(),
        ]),
        VerifiedComponent::Rarity(3),
        VerifiedComponent::Enchantments {
            entries: vec![enchant.clone()],
            show_in_tooltip: true,
        },
        VerifiedComponent::StoredEnchantments {
            entries: vec![enchant],
            show_in_tooltip: false,
        },
        VerifiedComponent::EnchantmentGlintOverride(true),
        VerifiedComponent::Unbreakable(true),
        VerifiedComponent::Damage(25),
        VerifiedComponent::RepairCost(15),
        VerifiedComponent::DyedColor {
            rgb: 0x00cc8822,
            show_in_tooltip: true,
        },
        VerifiedComponent::BaseColor(4),
        VerifiedComponent::PotionContents(PotionContents {
            potion: Some(2),
            custom_color: Some(0x0066cc99),
            effects: vec![effect],
            custom_name: Some("custom".into()),
        }),
        VerifiedComponent::SuspiciousStewEffects(vec![(2, 100)]),
        VerifiedComponent::OminousBottleAmplifier(4),
        VerifiedComponent::Fireworks {
            flight_duration: 2,
            explosions: vec![explosion.clone()],
        },
        VerifiedComponent::FireworkExplosion(explosion),
        VerifiedComponent::BannerPatterns(vec![BannerPattern {
            pattern: BannerPatternKind::Inline {
                asset_id: "minecraft:stripe".into(),
                translation_key: "stripe".into(),
            },
            color: 5,
        }]),
        VerifiedComponent::HideAdditionalTooltip,
        VerifiedComponent::HideTooltip,
        VerifiedComponent::Instrument(Instrument::Registry(1)),
        VerifiedComponent::BlockState(vec![("facing".into(), "north".into())]),
        VerifiedComponent::Profile(Profile {
            name: Some("player".into()),
            uuid: Some([0x5a; 16]),
            properties: vec![ProfileProperty {
                name: "textures".into(),
                value: "encoded".into(),
                signature: Some("signature".into()),
            }],
        }),
        VerifiedComponent::Container(vec![nested, ItemStack::empty()]),
    ]
}

fn additional_v767_components() -> Vec<VerifiedComponent> {
    let empty_compound = vec![10, 0];
    let text = plain_text_component("Hello").unwrap();
    let nested = ItemStack {
        item_id: 1,
        count: 1,
        added: vec![VerifiedComponent::RepairCost(1).to_raw().unwrap()],
        removed: vec![],
    };
    let predicate = BlockPredicate {
        blocks: Some(IdSet::Ids(vec![1, 2])),
        properties: Some(vec![BlockProperty {
            name: "facing".into(),
            value: BlockPropertyValue::Exact("north".into()),
        }]),
        nbt: Some(empty_compound.clone()),
    };
    vec![
        VerifiedComponent::CustomData(empty_compound.clone()),
        VerifiedComponent::MaxStackSize(99),
        VerifiedComponent::MaxDamage(250),
        VerifiedComponent::CanPlaceOn {
            predicates: vec![predicate.clone()],
            show_in_tooltip: true,
        },
        VerifiedComponent::CanBreak {
            predicates: vec![predicate],
            show_in_tooltip: false,
        },
        VerifiedComponent::AttributeModifiers {
            entries: vec![AttributeModifier {
                attribute: 2,
                id: "minecraft:attack_speed".into(),
                amount: 1.5,
                operation: 1,
                slot: 3,
            }],
            show_in_tooltip: true,
        },
        VerifiedComponent::CustomModelData(1),
        VerifiedComponent::CreativeSlotLock,
        VerifiedComponent::IntangibleProjectile(empty_compound.clone()),
        VerifiedComponent::Food(Food {
            nutrition: 4,
            saturation: 0.5,
            can_always_eat: true,
            seconds_to_eat: 1.6,
            converts_to: ItemStack::empty(),
            effects: vec![FoodEffect {
                effect: 1,
                probability: 0.25,
            }],
        }),
        VerifiedComponent::FireResistant,
        VerifiedComponent::Tool(Tool {
            rules: vec![ToolRule {
                blocks: IdSet::Tag("minecraft:mineable/pickaxe".into()),
                speed: Some(6.0),
                correct_drop_for_blocks: Some(true),
            }],
            default_mining_speed: 1.0,
            damage_per_block: 1,
        }),
        VerifiedComponent::MapColor(0xff0055aa),
        VerifiedComponent::MapId(25),
        VerifiedComponent::MapDecorations(empty_compound.clone()),
        VerifiedComponent::MapPostProcessing(1),
        VerifiedComponent::ChargedProjectiles(vec![nested.clone()]),
        VerifiedComponent::BundleContents(vec![nested]),
        VerifiedComponent::WritableBookContent(vec![BookPage {
            text: "Sample page".into(),
            filtered: Some("Filtered page".into()),
        }]),
        VerifiedComponent::WrittenBookContent(WrittenBook {
            title: "Test book".into(),
            filtered_title: None,
            author: "Author".into(),
            generation: 1,
            pages: vec![WrittenPage {
                text: text.clone(),
                filtered: Some(text.clone()),
            }],
            resolved: true,
        }),
        VerifiedComponent::Trim(Trim {
            material: Holder::Inline(TrimMaterial {
                asset_name: "quartz".into(),
                ingredient: 1,
                overrides: vec![("minecraft:iron".into(), "iron".into())],
                description: text.clone(),
            }),
            pattern: Holder::Inline(TrimPattern {
                asset_id: "minecraft:spire".into(),
                template: 1,
                description: text.clone(),
                decal: false,
            }),
            show_in_tooltip: true,
        }),
        VerifiedComponent::DebugStickState(empty_compound.clone()),
        VerifiedComponent::EntityData(empty_compound.clone()),
        VerifiedComponent::BucketEntityData(empty_compound.clone()),
        VerifiedComponent::BlockEntityData(empty_compound.clone()),
        VerifiedComponent::JukeboxPlayable(JukeboxPlayable {
            song: JukeboxSongRef::Holder(Holder::Inline(JukeboxSong {
                sound: Holder::Inline(SoundEvent {
                    name: "minecraft:music_disc.13".into(),
                    fixed_range: Some(16.0),
                }),
                description: text.clone(),
                duration: 5.0,
                comparator_output: 2,
            })),
            show_in_tooltip: true,
        }),
        VerifiedComponent::Recipes(empty_compound.clone()),
        VerifiedComponent::LodestoneTracker(LodestoneTracker {
            position: Some(("minecraft:overworld".into(), BlockPos::new(12, 80, 13))),
            tracked: true,
        }),
        VerifiedComponent::NoteBlockSound("minecraft:block.note_block.harp".into()),
        VerifiedComponent::PotDecorations(vec![1, 2, 3, 4]),
        VerifiedComponent::Bees(vec![BeeData {
            nbt: empty_compound.clone(),
            ticks_in_hive: 40,
            min_ticks_in_hive: 80,
        }]),
        VerifiedComponent::Lock(empty_compound.clone()),
        VerifiedComponent::ContainerLoot(empty_compound),
    ]
}

#[test]
fn all_57_v767_component_ids_roundtrip_and_fail_closed() {
    let mut cases = example_components();
    cases.extend(additional_v767_components());
    assert_eq!(cases.len(), 57, "one sample per registry entry");
    let mut seen = [false; 57];
    let table = ProtocolTable::v767_builtin();
    for value in cases {
        let raw = value.to_raw().unwrap();
        assert!((raw.type_id as usize) < seen.len());
        assert!(
            !std::mem::replace(&mut seen[raw.type_id as usize], true),
            "duplicate ID {}",
            raw.type_id
        );
        assert!(table
            .registry_name("minecraft:data_component_type", raw.type_id)
            .is_some());
        assert_eq!(
            VerifiedComponent::decode(&raw).unwrap(),
            value,
            "ID {}",
            raw.type_id
        );
        assert_eq!(
            VerifiedComponent::decode(&raw).unwrap().to_raw().unwrap(),
            raw
        );
        let sanitized = sanitize_and_roundtrip_components(std::slice::from_ref(&raw));
        assert_eq!(
            sanitized.components,
            vec![raw.clone()],
            "ID {}",
            raw.type_id
        );
        let mut oversized = raw.clone();
        oversized.data.resize(MAX_COMPONENT_BYTES + 1, 0);
        assert!(
            VerifiedComponent::decode(&oversized).is_err(),
            "oversized ID {}",
            raw.type_id
        );
        assert_eq!(
            sanitize_and_roundtrip_components(&[oversized]).dropped_oversized,
            1
        );
        if raw.data.is_empty() {
            let mut extra = raw.clone();
            extra.data.push(0);
            assert!(
                VerifiedComponent::decode(&extra).is_err(),
                "trailing byte ID {}",
                raw.type_id
            );
        } else {
            let mut truncated = raw.clone();
            truncated.data.pop();
            assert!(
                VerifiedComponent::decode(&truncated).is_err(),
                "truncated ID {}",
                raw.type_id
            );
            assert_eq!(
                sanitize_and_roundtrip_components(&[truncated]).dropped_invalid,
                1
            );
        }
    }
    assert!(
        seen.into_iter().all(|value| value),
        "all 57 component IDs must be covered"
    );
}

#[test]
fn expanded_components_reject_malformed_bounds_and_nested_stacks() {
    for value in [0, 100, i32::MAX as u32] {
        assert!(VerifiedComponent::MaxStackSize(value).to_raw().is_err());
        let mut writer = WireWriter::new();
        writer.write_varint(value as i32);
        assert!(VerifiedComponent::decode(&RawComponent {
            type_id: 1,
            data: writer.into_vec(),
        })
        .is_err());
    }
    assert!(
        VerifiedComponent::ChargedProjectiles(vec![ItemStack::empty(); 4])
            .to_raw()
            .is_err()
    );
    assert!(
        VerifiedComponent::BundleContents(vec![ItemStack::empty(); MAX_LIST_ENTRIES + 1])
            .to_raw()
            .is_err()
    );
    assert!(VerifiedComponent::WritableBookContent(vec![
        BookPage {
            text: "ok".into(),
            filtered: None,
        };
        MAX_LIST_ENTRIES + 1
    ])
    .to_raw()
    .is_err());
    assert!(VerifiedComponent::WritableBookContent(vec![BookPage {
        text: "\u{1b}unsafe".into(),
        filtered: None,
    }])
    .to_raw()
    .is_err());
    assert!(VerifiedComponent::MapPostProcessing(2).to_raw().is_err());
    assert!(VerifiedComponent::AttributeModifiers {
        entries: vec![AttributeModifier {
            attribute: 1,
            id: "example".into(),
            amount: f64::NAN,
            operation: 0,
            slot: 0,
        }],
        show_in_tooltip: true,
    }
    .to_raw()
    .is_err());
    assert!(VerifiedComponent::Food(Food {
        nutrition: 1,
        saturation: 1.0,
        can_always_eat: false,
        seconds_to_eat: 1.0,
        converts_to: ItemStack::empty(),
        effects: vec![FoodEffect {
            effect: 1,
            probability: 1.1
        }],
    })
    .to_raw()
    .is_err());

    // Nesting a stack-bearing component inside any other stack-bearing
    // component exceeds the single permitted layer.
    let nested_container = VerifiedComponent::Container(vec![]).to_raw().unwrap();
    let inner = ItemStack {
        item_id: 1,
        count: 1,
        added: vec![nested_container],
        removed: vec![],
    };
    assert!(VerifiedComponent::Container(vec![inner.clone()])
        .to_raw()
        .is_err());
    assert!(VerifiedComponent::ChargedProjectiles(vec![inner.clone()])
        .to_raw()
        .is_err());
    assert!(VerifiedComponent::BundleContents(vec![inner.clone()])
        .to_raw()
        .is_err());
    assert!(VerifiedComponent::Food(Food {
        nutrition: 1,
        saturation: 1.0,
        can_always_eat: false,
        seconds_to_eat: 1.0,
        converts_to: inner,
        effects: vec![],
    })
    .to_raw()
    .is_err());

    // A 33-level nested compound is within the byte budget but exceeds the
    // NBT depth limit. The same guard applies to direct and wire inputs.
    let mut deep_nbt = vec![10];
    for _ in 0..33 {
        deep_nbt.extend_from_slice(&[10, 0, 1, b'x']);
    }
    deep_nbt.extend(std::iter::repeat_n(0, 34));
    assert!(VerifiedComponent::CustomData(deep_nbt.clone())
        .to_raw()
        .is_err());
    assert!(VerifiedComponent::decode(&RawComponent {
        type_id: 0,
        data: deep_nbt,
    })
    .is_err());

    // NBT array elements also obey MAX_LIST_ENTRIES even when the total
    // encoded data is smaller than MAX_COMPONENT_BYTES.
    let mut oversized_array = vec![10, 7, 0, 1, b'a'];
    oversized_array.extend_from_slice(&((MAX_LIST_ENTRIES + 1) as i32).to_be_bytes());
    oversized_array.resize(oversized_array.len() + MAX_LIST_ENTRIES + 1, 0);
    oversized_array.push(0);
    assert!(VerifiedComponent::CustomData(oversized_array)
        .to_raw()
        .is_err());
}

#[test]
fn all_named_creative_components_typed_wire_and_sanitizer_roundtrip() {
    let components = example_components();
    assert_eq!(components.len(), 24); // The requirement enumerates 24 distinct variants.
    let table = ProtocolTable::v767_builtin();
    for component in components {
        let raw = component.to_raw().unwrap();
        assert_eq!(VerifiedComponent::decode(&raw).unwrap(), component);
        assert_eq!(
            VerifiedComponent::decode(&raw).unwrap().to_raw().unwrap(),
            raw
        );
        assert!(table
            .registry_name("minecraft:data_component_type", raw.type_id)
            .is_some());
        let sanitized = sanitize_and_roundtrip_components(std::slice::from_ref(&raw));
        assert_eq!(sanitized.kept, 1, "{component:?}");
        assert_eq!(sanitized.components, vec![raw.clone()]);
        if raw.data.len() > 1 {
            let truncated = RawComponent {
                type_id: raw.type_id,
                data: raw.data[..raw.data.len() - 1].to_vec(),
            };
            assert!(
                VerifiedComponent::decode(&truncated).is_err(),
                "{component:?}"
            );
        }
    }
}

#[test]
fn set_equipment_decodes_all_57_components_through_normal_packet_path() {
    let codec = CodecV767::new();
    let mut values = example_components();
    values.extend(additional_v767_components());
    let raw = values
        .into_iter()
        .map(|value| value.to_raw().unwrap())
        .collect();
    let packet = S2CPacket::SetEquipment {
        entity_id: 42,
        slots: vec![EquipmentSlot {
            slot: 5,
            item: ItemStack {
                item_id: 1,
                count: 1,
                added: raw,
                removed: vec![],
            },
        }],
    };
    let mut bytes = Vec::new();
    codec.encode_s2c(&packet, &mut bytes).unwrap();
    assert_eq!(
        codec.decode_s2c_in(PacketPhase::Play, &bytes).unwrap(),
        packet
    );

    // Noncanonical chat NBT can contain events. The packet decoder removes
    // unsafe fields and returns a canonical, still-decodable component.
    let mut unsafe_text = WireWriter::new();
    unsafe_text.write_u8(10);
    add_nbt_string(&mut unsafe_text, "text", "safe");
    add_nbt_string(&mut unsafe_text, "selector", "@a");
    add_nbt_string(&mut unsafe_text, "font", "minecraft:uniform");
    unsafe_text.write_u8(0);
    let expected = sanitize_text_component(unsafe_text.as_slice()).unwrap();
    let packet = S2CPacket::SetEquipment {
        entity_id: 42,
        slots: vec![EquipmentSlot {
            slot: 0,
            item: ItemStack {
                item_id: 1,
                count: 1,
                added: vec![RawComponent {
                    type_id: 5,
                    data: unsafe_text.into_vec(),
                }],
                removed: vec![],
            },
        }],
    };
    bytes.clear();
    codec.encode_s2c(&packet, &mut bytes).unwrap();
    let decoded = codec.decode_s2c_in(PacketPhase::Play, &bytes).unwrap();
    let S2CPacket::SetEquipment { slots, .. } = decoded else {
        panic!("wrong packet");
    };
    assert_eq!(slots[0].item.added[0].data, expected);
}

#[test]
fn set_equipment_rejects_component_bombs_in_the_live_wire_decoder() {
    let codec = CodecV767::new();
    let mut body = WireWriter::new();
    body.write_varint(42); // entity id
    body.write_u8(0); // final equipment slot
    body.write_varint(1); // stack count
    body.write_varint(1); // item id
    body.write_varint(1); // one added component
    body.write_varint(0); // no removed components
    body.write_varint(5); // custom_name
    body.write_u8(8); // NBT string root
    body.write_u16(4094);
    body.raw(&vec![b'x'; 4094]);
    let oversized = framed(
        PacketPhase::Play,
        PacketDir::Clientbound,
        "set_equipment",
        body.as_slice(),
    );
    assert!(codec.decode_s2c_in(PacketPhase::Play, &oversized).is_err());

    let mut nested = WireWriter::new();
    nested.write_varint(42);
    nested.write_u8(0);
    nested.write_varint(1);
    nested.write_varint(1);
    nested.write_varint(1);
    nested.write_varint(0);
    nested.write_varint(52); // outer container
    nested.write_varint(1); // one contained stack
    nested.write_varint(1); // contained stack count
    nested.write_varint(1); // contained item id
    nested.write_varint(1); // one nested component
    nested.write_varint(0);
    nested.write_varint(52); // forbidden container inside container
    nested.write_varint(0);
    assert!(codec
        .decode_s2c_in(
            PacketPhase::Play,
            &framed(
                PacketPhase::Play,
                PacketDir::Clientbound,
                "set_equipment",
                nested.as_slice()
            )
        )
        .is_err());

    for component_id in [7, 9] {
        // lore and enchantments
        let mut excessive = WireWriter::new();
        excessive.write_varint(42);
        excessive.write_u8(0);
        excessive.write_varint(1);
        excessive.write_varint(1);
        excessive.write_varint(1);
        excessive.write_varint(0);
        excessive.write_varint(component_id);
        excessive.write_varint(65);
        assert!(codec
            .decode_s2c_in(
                PacketPhase::Play,
                &framed(
                    PacketPhase::Play,
                    PacketDir::Clientbound,
                    "set_equipment",
                    excessive.as_slice()
                )
            )
            .is_err());
    }
}

#[test]
fn component_rejects_bombs_duplicates_and_invalid_values() {
    let good = VerifiedComponent::Damage(2).to_raw().unwrap();
    let bad = RawComponent {
        type_id: 5,
        data: vec![0; MAX_COMPONENT_BYTES + 1],
    };
    let unknown = RawComponent {
        type_id: 10_000,
        data: vec![0],
    };
    let malformed = RawComponent {
        type_id: 8,
        data: vec![9],
    };
    let sanitized =
        sanitize_and_roundtrip_components(&[good.clone(), bad, unknown, malformed, good]);
    assert_eq!(sanitized.kept, 1);
    assert_eq!(sanitized.dropped_unknown, 1);
    assert_eq!(sanitized.dropped_oversized, 1);
    assert_eq!(sanitized.dropped_invalid, 2);

    let big = plain_text_component(&"x".repeat(3400)).unwrap();
    let stack = [
        VerifiedComponent::CustomName(big.clone()).to_raw().unwrap(),
        VerifiedComponent::ItemName(big.clone()).to_raw().unwrap(),
        VerifiedComponent::Lore(vec![big]).to_raw().unwrap(),
    ];
    let budget = sanitize_and_roundtrip_components(&stack);
    assert_eq!(budget.kept, 2);
    assert_eq!(budget.dropped_oversized, 1);
    assert!(
        budget
            .components
            .iter()
            .map(|c| c.data.len() + 1)
            .sum::<usize>()
            <= MAX_STACK_COMPONENTS_BYTES
    );

    let too_many = (0..=MAX_LIST_ENTRIES)
        .map(|n| Enchantment {
            id: n as u32,
            level: 1,
        })
        .collect();
    assert!(VerifiedComponent::Enchantments {
        entries: too_many,
        show_in_tooltip: true
    }
    .to_raw()
    .is_err());
    assert!(VerifiedComponent::Lore(vec![
        plain_text_component("x").unwrap();
        MAX_LIST_ENTRIES + 1
    ])
    .to_raw()
    .is_err());
    assert!(
        VerifiedComponent::Container(vec![ItemStack::empty(); MAX_CONTAINER_SLOTS + 1])
            .to_raw()
            .is_err()
    );

    let inner = VerifiedComponent::Container(vec![ItemStack::empty()])
        .to_raw()
        .unwrap();
    let nested = ItemStack {
        item_id: 1,
        count: 1,
        added: vec![inner],
        removed: vec![],
    };
    assert!(VerifiedComponent::Container(vec![nested]).to_raw().is_err());

    let too_deep = PotionEffectDetails {
        amplifier: 0,
        duration: 1,
        ambient: false,
        show_particles: true,
        show_icon: true,
        hidden: None,
    };
    let mut hidden = too_deep;
    for _ in 0..20 {
        hidden = PotionEffectDetails {
            hidden: Some(Box::new(hidden)),
            amplifier: 0,
            duration: 1,
            ambient: false,
            show_particles: true,
            show_icon: true,
        };
    }
    let oversized_recursion = VerifiedComponent::PotionContents(PotionContents {
        potion: None,
        custom_color: None,
        custom_name: None,
        effects: vec![PotionEffect {
            id: 1,
            amplifier: 0,
            duration: 1,
            ambient: false,
            show_particles: true,
            show_icon: true,
            hidden: Some(Box::new(hidden)),
        }],
    });
    assert!(oversized_recursion.to_raw().is_err());
}

fn add_nbt_string(w: &mut WireWriter, key: &str, value: &str) {
    w.write_u8(8);
    w.write_u16(key.len() as u16);
    w.raw(key.as_bytes());
    w.write_u16(value.len() as u16);
    w.raw(value.as_bytes());
}

#[test]
fn text_sanitizer_strips_executable_and_lookup_fields_and_keeps_formatting() {
    assert_eq!(sanitize_text_component(&[0]).unwrap(), vec![0]);
    assert!(sanitize_text_component(&[0, 1]).is_err());
    let mut w = WireWriter::new();
    w.write_u8(10);
    add_nbt_string(&mut w, "text", "safe");
    add_nbt_string(&mut w, "selector", "@a");
    add_nbt_string(&mut w, "font", "minecraft:uniform");
    add_nbt_string(&mut w, "color", "red");
    w.write_u8(1);
    w.write_u16(4);
    w.raw(b"bold");
    w.write_i8(1);
    for name in ["clickEvent", "hoverEvent", "score", "nbt"] {
        w.write_u8(10);
        w.write_u16(name.len() as u16);
        w.raw(name.as_bytes());
        add_nbt_string(&mut w, "action", "run_command");
        w.write_u8(0);
    }
    w.write_u8(0);
    let sanitized = sanitize_text_component(w.as_slice()).unwrap();
    assert_eq!(sanitize_text_component(&sanitized).unwrap(), sanitized);
    let text = String::from_utf8_lossy(&sanitized);
    for forbidden in [
        "selector",
        "font",
        "clickEvent",
        "hoverEvent",
        "score",
        "nbt",
        "run_command",
    ] {
        assert!(!text.contains(forbidden), "{forbidden}");
    }
    for kept in ["text", "safe", "color", "red", "bold"] {
        assert!(text.contains(kept), "{kept}");
    }
    let name = RawComponent {
        type_id: 5,
        data: w.into_vec(),
    };
    let output = sanitize_and_roundtrip_components(&[name]);
    assert_eq!(output.kept, 1);
    assert_eq!(output.components[0].data, sanitized);
    let mut reader = WireReader::new(&sanitized);
    reader.read_anonymous_nbt().unwrap();
    reader.finish().unwrap();
}
