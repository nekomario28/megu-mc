// SPDX-License-Identifier: LGPL-3.0-or-later

//! Create-specific kinetic delta wire codec layered over
//! [`mod_adapter_core::neoforge_wire`].

pub use mod_adapter_core::neoforge_wire::{
    build_config_registry_data_0x07, build_neoforge_frozen_registry_frames,
    decode_config_registry_data_0x07, read_mc_string, read_varint, write_mc_string, write_varint,
    CHANNEL_FROZEN_REGISTRY, CHANNEL_FROZEN_REGISTRY_SYNC_COMPLETED,
    CHANNEL_FROZEN_REGISTRY_SYNC_START, CONFIG_REGISTRY_DATA_PACKET_ID, MAX_REGISTRY_DATA_NAME_LEN,
};

use crate::factory::{DimBlockPos, KineticUpdateTag};

/// Canonical live-sync channel for Create kinetic deltas. The live server
/// (`src/live_mod_bridge.rs`) sends these as Play S2C `0x19` custom payloads:
/// `write_str("create:kinetic_sync_v1")` followed by the batch below.
pub const CHANNEL_CREATE_KINETIC_SYNC: &str = "create:kinetic_sync_v1";

/// Encode a batch of changed [`KineticUpdateTag`] deltas into a compact binary
/// custom-payload body suitable for Play `0x19` broadcast to tracked observers.
///
/// Wire layout: a VarInt tag count, then 29 bytes per record —
/// `i32be dim | i32be x | i32be y | i32be z | f32be speed_rpm |
/// f32be stress_capacity | f32be stress_load | u8 overstressed (0|1)`.
/// [`decode_kinetic_update_batch`] is the exact inverse and rejects malformed
/// tails (count > 4096, truncation, non-boolean flag, trailing bytes).
pub fn encode_kinetic_update_batch(tags: &[KineticUpdateTag]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + tags.len() * 29);
    write_varint(&mut out, tags.len() as u32);
    for tag in tags {
        out.extend_from_slice(&tag.pos.dim.to_be_bytes());
        out.extend_from_slice(&tag.pos.x.to_be_bytes());
        out.extend_from_slice(&tag.pos.y.to_be_bytes());
        out.extend_from_slice(&tag.pos.z.to_be_bytes());
        out.extend_from_slice(&(tag.speed_rpm as f32).to_be_bytes());
        out.extend_from_slice(&(tag.stress_capacity as f32).to_be_bytes());
        out.extend_from_slice(&(tag.stress_load as f32).to_be_bytes());
        out.push(u8::from(tag.overstressed));
    }
    out
}

fn decode_kinetic_records(
    bytes: &[u8],
    cursor: &mut usize,
    count: usize,
) -> Option<Vec<KineticUpdateTag>> {
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if cursor.checked_add(29)? > bytes.len() {
            return None;
        }
        let dim = i32::from_be_bytes(bytes[*cursor..*cursor + 4].try_into().ok()?);
        let x = i32::from_be_bytes(bytes[*cursor + 4..*cursor + 8].try_into().ok()?);
        let y = i32::from_be_bytes(bytes[*cursor + 8..*cursor + 12].try_into().ok()?);
        let z = i32::from_be_bytes(bytes[*cursor + 12..*cursor + 16].try_into().ok()?);
        let speed = f32::from_be_bytes(bytes[*cursor + 16..*cursor + 20].try_into().ok()?);
        let cap = f32::from_be_bytes(bytes[*cursor + 20..*cursor + 24].try_into().ok()?);
        let load = f32::from_be_bytes(bytes[*cursor + 24..*cursor + 28].try_into().ok()?);
        let overstressed = match bytes[*cursor + 28] {
            0 => false,
            1 => true,
            _ => return None,
        };
        *cursor += 29;
        out.push(KineticUpdateTag {
            pos: DimBlockPos::new(dim, x, y, z),
            speed_rpm: f64::from(speed),
            stress_capacity: f64::from(cap),
            stress_load: f64::from(load),
            overstressed,
        });
    }
    Some(out)
}

/// Decode a batch written by [`encode_kinetic_update_batch`].
pub fn decode_kinetic_update_batch(bytes: &[u8]) -> Option<Vec<KineticUpdateTag>> {
    let mut cursor = 0usize;
    let count = read_varint(bytes, &mut cursor)? as usize;
    if count > MAX_KINETIC_BATCH_RECORDS {
        return None;
    }
    let out = decode_kinetic_records(bytes, &mut cursor, count)?;
    (cursor == bytes.len()).then_some(out)
}

/// Maximum records in either the legacy single batch or one chunked frame.
pub const MAX_KINETIC_BATCH_RECORDS: usize = 4096;
/// Magic for the dimension-scoped chunked snapshot frame (`KNT1`).
pub const KINETIC_BATCH_MAGIC: [u8; 4] = *b"KNT1";
pub const KINETIC_BATCH_VERSION: u8 = 1;
const KINETIC_BATCH_HEADER_LEN: usize = 4 + 1 + 4 + 8 + 4 + 4;

/// Metadata carried by one chunked kinetic snapshot frame. `chunk_index` and
/// `chunk_total` are scoped to `(snapshot_generation, dimension_id)`, so a
/// client can reassemble every dimension independently.
#[derive(Clone, Debug, PartialEq)]
pub struct KineticBatchChunk {
    pub dimension_id: i32,
    pub snapshot_generation: u64,
    pub chunk_index: u32,
    pub chunk_total: u32,
    pub tags: Vec<KineticUpdateTag>,
}

fn encode_kinetic_chunk(
    dimension_id: i32,
    snapshot_generation: u64,
    chunk_index: u32,
    chunk_total: u32,
    tags: &[KineticUpdateTag],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(KINETIC_BATCH_HEADER_LEN + 4 + tags.len() * 29);
    out.extend_from_slice(&KINETIC_BATCH_MAGIC);
    out.push(KINETIC_BATCH_VERSION);
    out.extend_from_slice(&dimension_id.to_be_bytes());
    out.extend_from_slice(&snapshot_generation.to_be_bytes());
    out.extend_from_slice(&chunk_index.to_be_bytes());
    out.extend_from_slice(&chunk_total.to_be_bytes());
    write_varint(&mut out, tags.len() as u32);
    for tag in tags {
        out.extend_from_slice(&tag.pos.dim.to_be_bytes());
        out.extend_from_slice(&tag.pos.x.to_be_bytes());
        out.extend_from_slice(&tag.pos.y.to_be_bytes());
        out.extend_from_slice(&tag.pos.z.to_be_bytes());
        out.extend_from_slice(&(tag.speed_rpm as f32).to_be_bytes());
        out.extend_from_slice(&(tag.stress_capacity as f32).to_be_bytes());
        out.extend_from_slice(&(tag.stress_load as f32).to_be_bytes());
        out.push(u8::from(tag.overstressed));
    }
    out
}

/// Encode one snapshot as dimension-scoped chunks of at most
/// [`MAX_KINETIC_BATCH_RECORDS`] records. The legacy
/// [`encode_kinetic_update_batch`] helper is unchanged for small in-flight
/// delta users; this chunked form is what joining clients use when a large
/// factory snapshot cannot fit in one 4,096-record frame.
pub fn encode_kinetic_batches(tags: &[KineticUpdateTag], snapshot_generation: u64) -> Vec<Vec<u8>> {
    let mut by_dimension: std::collections::BTreeMap<i32, Vec<KineticUpdateTag>> =
        std::collections::BTreeMap::new();
    for tag in tags {
        by_dimension.entry(tag.pos.dim).or_default().push(*tag);
    }
    let mut out = Vec::new();
    for (dimension_id, dimension_tags) in by_dimension {
        let chunk_total = dimension_tags.len().div_ceil(MAX_KINETIC_BATCH_RECORDS) as u32;
        for (chunk_index, chunk) in dimension_tags.chunks(MAX_KINETIC_BATCH_RECORDS).enumerate() {
            out.push(encode_kinetic_chunk(
                dimension_id,
                snapshot_generation,
                chunk_index as u32,
                chunk_total,
                chunk,
            ));
        }
    }
    out
}

fn be_u32(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let end = cursor.checked_add(4)?;
    let value = u32::from_be_bytes(bytes.get(*cursor..end)?.try_into().ok()?);
    *cursor = end;
    Some(value)
}

/// Decode one dimension-scoped chunked frame written by
/// [`encode_kinetic_batches`], rejecting truncation, malformed record flags,
/// inconsistent chunk indexes and trailing bytes.
pub fn decode_kinetic_batch_chunk(bytes: &[u8]) -> Option<KineticBatchChunk> {
    let mut cursor = 0usize;
    if bytes.get(..4) != Some(&KINETIC_BATCH_MAGIC) {
        return None;
    }
    cursor += 4;
    if *bytes.get(cursor)? != KINETIC_BATCH_VERSION {
        return None;
    }
    cursor += 1;
    let dimension_id = i32::from_be_bytes(bytes.get(cursor..cursor + 4)?.try_into().ok()?);
    cursor += 4;
    let snapshot_generation = u64::from_be_bytes(bytes.get(cursor..cursor + 8)?.try_into().ok()?);
    cursor += 8;
    let chunk_index = be_u32(bytes, &mut cursor)?;
    let chunk_total = be_u32(bytes, &mut cursor)?;
    if chunk_total == 0 || chunk_index >= chunk_total || chunk_total > 4096 {
        return None;
    }
    let count = read_varint(bytes, &mut cursor)? as usize;
    if count > MAX_KINETIC_BATCH_RECORDS {
        return None;
    }
    let tags = decode_kinetic_records(bytes, &mut cursor, count)?;
    if cursor != bytes.len() || tags.iter().any(|tag| tag.pos.dim != dimension_id) {
        return None;
    }
    Some(KineticBatchChunk {
        dimension_id,
        snapshot_generation,
        chunk_index,
        chunk_total,
        tags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockstates::{
        core_create_block_defs, core_create_registry, CreateBlockRole, DEFAULT_BASE_STATE_ID,
    };
    use crate::data_loader::parse_create_recipe_json;
    use crate::factory::CreateFactory;
    use mod_adapter_core::blockstates::{BlockDef, BlockRegistry, PropertyDef, UniversalBlockRole};
    use mod_adapter_core::data_loader::{parse_mod_recipe_json, ItemFluidIdTable};
    use mod_adapter_core::fallback::{
        plan_mod_startup, DiscoveredMod, ExecutionTier, FallbackPolicy, StartupPlan,
    };
    use mod_adapter_core::profile::{
        builtin_reference_profiles, DimBlockPos as UniPos, UniversalModRuntime,
    };
    use mod_offload::kinetic_graph::NetworkStatus;
    use mod_offload::recipe_processor::{Resource, ResourceKind, Stack};
    use std::collections::BTreeSet;

    #[test]
    fn frozen_registry_frames_cover_all_expanded_create_states() {
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let frames = build_neoforge_frozen_registry_frames(&reg);
        assert_eq!(frames.len(), 3);
        let body = &frames[1].1;
        let mut cursor = 0usize;
        assert_eq!(
            read_mc_string(body, &mut cursor, 128).as_deref(),
            Some("minecraft:block")
        );
        let count = read_varint(body, &mut cursor).unwrap() as usize;
        assert_eq!(count, reg.state_count());
    }

    #[test]
    fn kinetic_update_batch_round_trips_and_rejects_truncation() {
        let tags = vec![KineticUpdateTag {
            pos: DimBlockPos::new(3, -8, 120, 44),
            speed_rpm: 32.0,
            stress_capacity: 512.0,
            stress_load: 128.0,
            overstressed: false,
        }];
        let encoded = encode_kinetic_update_batch(&tags);
        assert_eq!(decode_kinetic_update_batch(&encoded), Some(tags));
        assert!(decode_kinetic_update_batch(&encoded[..encoded.len() - 1]).is_none());
    }

    #[test]
    fn kinetic_batches_chunk_4096_records_per_dimension_and_round_trip() {
        let tag = |dim, index| KineticUpdateTag {
            pos: DimBlockPos::new(dim, index, 64, -7),
            speed_rpm: 32.0,
            stress_capacity: 512.0,
            stress_load: 128.0,
            overstressed: index % 2 == 0,
        };
        let mut tags: Vec<_> = (0..4097).map(|index| tag(0, index)).collect();
        tags.extend([tag(3, 0), tag(3, 1)]);
        let frames = encode_kinetic_batches(&tags, 77);
        assert_eq!(frames.len(), 3);
        let chunks: Vec<_> = frames
            .iter()
            .map(|frame| decode_kinetic_batch_chunk(frame).expect("valid chunk"))
            .collect();
        assert_eq!(chunks[0].dimension_id, 0);
        assert_eq!(chunks[0].snapshot_generation, 77);
        assert_eq!(chunks[0].chunk_total, 2);
        assert_eq!(chunks[0].tags.len(), MAX_KINETIC_BATCH_RECORDS);
        assert_eq!(chunks[1].chunk_index, 1);
        assert_eq!(chunks[1].tags.len(), 1);
        assert_eq!(chunks[2].dimension_id, 3);
        assert_eq!(chunks[2].chunk_total, 1);
        assert_eq!(chunks[2].tags.len(), 2);
        let recovered: Vec<_> = chunks.into_iter().flat_map(|chunk| chunk.tags).collect();
        assert_eq!(recovered, tags);
    }

    /// End-to-end multi-mod compatibility harness: 4-tier startup planning,
    /// Protocol 767 Configuration registry sync (`0x07` + `frozen_registry*`),
    /// and Play-phase simulation in both the Overworld (`dim=0`) and a custom
    /// dimension (`dim=3`), all surviving a Save v2 canonical-string remap
    /// across block reorder.
    #[test]
    fn multi_mod_world_config_play_and_save_v2_round_trip() {
        // ---- Startup classification across all four execution tiers. ------
        let mods = vec![
            DiscoveredMod {
                mod_id: "create".into(),
                has_java_bytecode: true,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: true,
                rejection_reasons: vec!["UNSUPPORTED_MIXIN_INJECTION".into()],
                mc_version_range: None,
            },
            DiscoveredMod {
                mod_id: "mekanism".into(),
                has_java_bytecode: true,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: false,
                rejection_reasons: vec![],
                mc_version_range: None,
            },
            DiscoveredMod {
                mod_id: "pipez".into(),
                has_java_bytecode: true,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: false,
                rejection_reasons: vec![],
                mc_version_range: None,
            },
            DiscoveredMod {
                mod_id: "shim_ok".into(),
                has_java_bytecode: true,
                shimcheck_compatible: true,
                has_unsupported_mixin_or_at: false,
                rejection_reasons: vec![],
                mc_version_range: None,
            },
            DiscoveredMod {
                mod_id: "mixin_heavy".into(),
                has_java_bytecode: true,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: true,
                rejection_reasons: vec!["UNSUPPORTED_MIXIN_INJECTION".into()],
                mc_version_range: None,
            },
        ];
        let set = |items: &[&str]| -> BTreeSet<String> {
            items.iter().map(|s| (*s).to_string()).collect()
        };
        let plan = plan_mod_startup(
            &mods,
            &set(&["create"]),
            &set(&["mekanism", "pipez"]),
            &set(&["create", "mekanism"]),
            FallbackPolicy::Quarantine {
                load_safe_data_only: true,
                force_drop_saved_blocks: false,
            },
        );
        let StartupPlan::RunNative {
            assignments,
            needs_bounded_jvm,
            quarantined_mods,
            load_quarantined_data_only,
        } = plan
        else {
            panic!("expected quarantined RunNative plan, got {plan:?}");
        };
        assert!(needs_bounded_jvm);
        assert!(load_quarantined_data_only);
        let tier_of = |id: &str| {
            assignments
                .iter()
                .find(|a| a.mod_id == id)
                .map(|a| a.tier)
                .unwrap()
        };
        assert_eq!(tier_of("create"), ExecutionTier::NativeFlagship);
        assert_eq!(tier_of("mekanism"), ExecutionTier::UniversalNative);
        assert_eq!(tier_of("pipez"), ExecutionTier::UniversalNative);
        assert_eq!(tier_of("shim_ok"), ExecutionTier::BoundedJvmShim);
        assert_eq!(quarantined_mods.len(), 1);
        assert_eq!(quarantined_mods[0].mod_id, "mixin_heavy");

        // ---- Configuration phase: one frozen `minecraft:block` space. -----
        let create_reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let uni_base = DEFAULT_BASE_STATE_ID + create_reg.state_count() as u32;
        let profiles = builtin_reference_profiles();
        let mut uni = UniversalModRuntime::from_profiles(uni_base, &profiles).unwrap();

        let mut block_names: Vec<String> =
            create_reg.blocks().iter().map(|b| b.id.clone()).collect();
        block_names.extend(uni.registry().blocks().iter().map(|b| b.id.clone()));
        assert_eq!(block_names.len(), 43);
        let block_body = build_config_registry_data_0x07("minecraft:block", &block_names).unwrap();
        assert_eq!(block_body[0], CONFIG_REGISTRY_DATA_PACKET_ID);
        assert_eq!(
            decode_config_registry_data_0x07(&block_body),
            Some(("minecraft:block".to_string(), block_names))
        );
        let item_names = vec![
            "create:iron_sheet".to_string(),
            "mekanism:bio_fuel".to_string(),
            "pipez:wrench".to_string(),
        ];
        let item_body = build_config_registry_data_0x07("minecraft:item", &item_names).unwrap();
        assert_eq!(
            decode_config_registry_data_0x07(&item_body),
            Some(("minecraft:item".to_string(), item_names))
        );

        // NeoForge frozen-registry snapshots cover every expanded state of
        // both tiers under the shared sync channels.
        for frames in [
            build_neoforge_frozen_registry_frames(&create_reg),
            build_neoforge_frozen_registry_frames(uni.registry()),
        ] {
            assert_eq!(frames.len(), 3);
            assert_eq!(frames[0].0, CHANNEL_FROZEN_REGISTRY_SYNC_START);
            assert_eq!(frames[1].0, CHANNEL_FROZEN_REGISTRY);
            assert_eq!(frames[2].0, CHANNEL_FROZEN_REGISTRY_SYNC_COMPLETED);
        }

        // ---- Play phase: both adapters live in dim 0 and dim 3. -----------
        let wheel_x = create_reg
            .with_property(
                create_reg.default_state("create:water_wheel").unwrap(),
                "facing",
                "east",
            )
            .unwrap();
        let shaft_x = create_reg
            .with_property(
                create_reg.default_state("create:shaft").unwrap(),
                "axis",
                "x",
            )
            .unwrap();
        let mut factory = CreateFactory::new(create_reg.clone());

        let mek_gen = uni
            .registry()
            .default_state("mekanism:heat_generator")
            .unwrap();
        let mek_cable = uni
            .registry()
            .default_state("mekanism:universal_cable")
            .unwrap();
        let mek_crusher = uni.registry().default_state("mekanism:crusher").unwrap();
        let pipez_pipe = uni.registry().default_state("pipez:item_pipe").unwrap();
        let mek_tank = uni
            .registry()
            .default_state("mekanism:basic_fluid_tank")
            .unwrap();

        let mut ids = ItemFluidIdTable::new();
        let crushing = parse_mod_recipe_json(
            "mekanism:crushing/bio_fuel",
            1,
            br#"{"type":"mekanism:crushing","input":{"item":"minecraft:wheat"},"output":{"id":"mekanism:bio_fuel","count":2},"processing_time":4}"#,
            &mut ids,
        )
        .unwrap()
        .unwrap();
        let pressing = parse_create_recipe_json(
            "create:pressing/iron_sheet",
            2,
            br#"{"type":"create:pressing","ingredients":[{"item":"minecraft:iron_ingot"}],"results":[{"id":"create:iron_sheet","count":1}],"processing_time":4}"#,
            &mut ids,
        )
        .unwrap()
        .unwrap();
        let wheat = Resource {
            kind: ResourceKind::Item,
            id: ids.item_id("minecraft:wheat").unwrap(),
        };
        let wheat_id = wheat.id;
        let bio_fuel = Resource {
            kind: ResourceKind::Item,
            id: ids.item_id("mekanism:bio_fuel").unwrap(),
        };
        let ingot = Resource {
            kind: ResourceKind::Item,
            id: ids.item_id("minecraft:iron_ingot").unwrap(),
        };
        let sheet = Resource {
            kind: ResourceKind::Item,
            id: ids.item_id("create:iron_sheet").unwrap(),
        };
        let water = ids.intern_fluid("minecraft:water");

        let mut uni_positions = Vec::new();
        for dim in [0, 3] {
            // Tier 0 flagship: water wheel drives a shaft, press attached.
            let p_wheel = DimBlockPos::new(dim, 0, 64, 0);
            let p_shaft = DimBlockPos::new(dim, 1, 64, 0);
            let p_press = DimBlockPos::new(dim, 2, 64, 0);
            factory.place_block(p_wheel, wheel_x);
            factory.place_block(p_shaft, shaft_x);
            assert_eq!(
                factory.kinetic_solution(p_shaft).unwrap().status,
                NetworkStatus::Running
            );
            let press_state = create_reg
                .with_property(
                    create_reg.default_state("create:mechanical_press").unwrap(),
                    "facing",
                    "east",
                )
                .unwrap();
            factory.place_block(p_press, press_state);
            factory
                .add_processor(
                    p_press,
                    900 + dim as u32,
                    16,
                    16,
                    vec![pressing.recipe.clone()],
                )
                .unwrap();
            factory
                .insert_processor_ingredient(
                    p_press,
                    900 + dim as u32,
                    Stack {
                        resource: ingot,
                        amount: 1,
                    },
                )
                .unwrap();

            // Tier 1 declarative runtime: generator -> cable -> crusher,
            // a sleeping item-pipe hop, and a two-tank pressure network.
            let u_gen = UniPos::new(dim, 0, 64, 8);
            let u_cable = UniPos::new(dim, 1, 64, 8);
            let u_crusher = UniPos::new(dim, 2, 64, 8);
            let u_pipe_a = UniPos::new(dim, 0, 64, 10);
            let u_pipe_b = UniPos::new(dim, 1, 64, 10);
            let u_tank_a = UniPos::new(dim, 0, 64, 12);
            let u_tank_b = UniPos::new(dim, 1, 64, 12);
            assert!(uni.place_block(u_gen, mek_gen, Vec::new()));
            assert!(uni.place_block(u_cable, mek_cable, Vec::new()));
            assert!(uni.network_running(u_cable));
            assert!(uni.place_block(u_crusher, mek_crusher, vec![crushing.recipe.clone()]));
            uni.insert_processor_input(
                u_crusher,
                Stack {
                    resource: wheat,
                    amount: 1,
                },
            );
            assert!(uni.place_block(u_pipe_a, pipez_pipe, Vec::new()));
            assert!(uni.place_block(u_pipe_b, pipez_pipe, Vec::new()));
            assert!(uni.link_transport(u_pipe_a, u_pipe_b));
            assert!(uni.insert_transport_item(u_pipe_a, wheat_id));
            assert!(uni.place_block(u_tank_a, mek_tank, Vec::new()));
            assert!(uni.place_block(u_tank_b, mek_tank, Vec::new()));
            assert_eq!(uni.insert_tank_fluid(u_tank_a, water, 4000), Some(4000));
            uni_positions.push((
                dim,
                u_crusher,
                u_pipe_b,
                u_tank_a,
                u_tank_b,
                900 + dim as u32,
            ));
        }

        let mut s5_records = 0usize;
        for tick in 1..=10 {
            for out in factory.step_tick(tick, 1_024, None).unwrap() {
                s5_records += out.batch.records;
            }
            for out in uni.step_tick(tick, 1_024, None) {
                s5_records += out.batch.records;
            }
        }
        assert!(s5_records >= 8);

        // Both dims produced identical, independent outcomes.
        for (dim, u_crusher, u_pipe_b, u_tank_a, u_tank_b, press_id) in uni_positions {
            let p_press = DimBlockPos::new(dim, 2, 64, 0);
            assert_eq!(
                factory
                    .extract_processor_product(
                        p_press,
                        press_id,
                        Stack {
                            resource: sheet,
                            amount: 1
                        }
                    )
                    .unwrap(),
                1,
                "dim {dim} press output"
            );
            assert_eq!(
                uni.extract_processor_output(
                    u_crusher,
                    Stack {
                        resource: bio_fuel,
                        amount: 2
                    }
                ),
                Some(2),
                "dim {dim} crusher output"
            );
            assert_eq!(uni.extract_transport_output(u_pipe_b), Some(wheat_id));
            assert_eq!(uni.tank_state(u_tank_a).unwrap().amount, 2000);
            assert_eq!(uni.tank_state(u_tank_b).unwrap().amount, 2000);
        }

        // ---- Save v2: canonical strings survive a block reorder. ----------
        let saved_shaft = create_reg.encode_canonical(shaft_x).unwrap().to_string();
        let saved_cable = uni
            .registry()
            .encode_canonical(mek_cable)
            .unwrap()
            .to_string();
        assert_eq!(saved_shaft, "create:shaft[axis=x]");
        assert_eq!(saved_cable, "mekanism:universal_cable[tier=basic]");

        // v2 freeze adds an alphabetically-earlier block to each mod, shifting
        // runtime state IDs while canonical strings stay authoritative.
        let mut create_v2 = core_create_block_defs();
        create_v2.push(BlockDef {
            id: "create:aa_addon_axle".into(),
            properties: vec![PropertyDef::new("axis", &["x", "y", "z"])],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Shaft,
        });
        let create_reg_v2 = BlockRegistry::build(DEFAULT_BASE_STATE_ID, create_v2).unwrap();
        let remapped_shaft = create_reg_v2.decode_canonical(&saved_shaft).unwrap();
        assert_ne!(remapped_shaft, shaft_x);
        assert_eq!(
            create_reg_v2.encode_canonical(remapped_shaft),
            Some(saved_shaft.as_str())
        );
        // Saved palettes accept property order permutations too.
        assert_eq!(
            create_reg_v2.decode_canonical("create:shaft[axis=x]"),
            Some(remapped_shaft)
        );

        let mut uni_defs_v2: Vec<BlockDef<UniversalBlockRole>> =
            profiles.iter().flat_map(|p| p.blocks.clone()).collect();
        uni_defs_v2.push(BlockDef {
            id: "ae2:aa_addon_cable".into(),
            properties: vec![],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: UniversalBlockRole::NetworkCable,
        });
        let uni_reg_v2 = BlockRegistry::build(uni_base, uni_defs_v2).unwrap();
        let remapped_cable = uni_reg_v2.decode_canonical(&saved_cable).unwrap();
        assert_ne!(remapped_cable, mek_cable);
        assert_eq!(
            uni_reg_v2.encode_canonical(remapped_cable),
            Some(saved_cable.as_str())
        );
    }
}
