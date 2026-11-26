// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client::protocol::DimensionMeta;
use megu_client::world::{BlockTable, WorldState};
use megu_client_gpu::live_session::{LiveSession, SessionPaths};
use megu_client_gpu::mod_profile_tx::{GenerationModEntry, ModProfileStore, SOURCE_USER_EDIT};
use megu_client_gpu::mod_translator::{
    load_auto_translation_pack_ja_jp, set_auto_translation_pack_enabled, AUTO_TRANSLATION_PACK_DIR,
};
use megu_client_gpu::window::{
    profile_activation_diff_line, soft_reload_summary_line, GpuApp, SoftReloadSummary,
};
use megu_client_gpu::GpuCliConfig;
use megu_core::hash::crc32_ieee;
use megu_ui::mod_manager::{
    MissingDependencyConsentDecision, MissingDependencyConsentRow, MissingDependencyConsentUiState,
    ModManagerAction, ModManagerEntry, ModManagerState, ModManagerWidgetId, PlamoTranslatePreset,
    PlamoTranslatorStatus,
};
use megu_ui::state::{UiInputEvent, UiLang};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("megu-w1c-{label}-{}-{nanos}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create temp root");
    root
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn w16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    fn w32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, bytes) in entries {
        let name = name.as_bytes();
        let offset = out.len() as u32;
        let crc = crc32_ieee(bytes);
        w32(&mut out, 0x0403_4b50);
        w16(&mut out, 20);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, crc);
        w32(&mut out, bytes.len() as u32);
        w32(&mut out, bytes.len() as u32);
        w16(&mut out, name.len() as u16);
        w16(&mut out, 0);
        out.extend_from_slice(name);
        out.extend_from_slice(bytes);
        w32(&mut central, 0x0201_4b50);
        w16(&mut central, 20);
        w16(&mut central, 20);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, crc);
        w32(&mut central, bytes.len() as u32);
        w32(&mut central, bytes.len() as u32);
        w16(&mut central, name.len() as u16);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, 0);
        w32(&mut central, offset);
        central.extend_from_slice(name);
    }
    let cd_offset = out.len() as u32;
    let cd_len = central.len() as u32;
    out.extend_from_slice(&central);
    w32(&mut out, 0x0605_4b50);
    w16(&mut out, 0);
    w16(&mut out, 0);
    w16(&mut out, entries.len() as u16);
    w16(&mut out, entries.len() as u16);
    w32(&mut out, cd_len);
    w32(&mut out, cd_offset);
    w16(&mut out, 0);
    out
}

fn test_block_table() -> BlockTable {
    let json = r##"{
        "blocks": [
            ["minecraft:air", 0, 1],
            ["minecraft:grass_block", 1, 1]
        ],
        "flags": [1, 8],
        "full_sides": [0, 63],
        "shapes": [{"min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0]}],
        "collision_shapes": [[], [0]]
    }"##;
    BlockTable::from_json_bytes(json.as_bytes()).expect("block table")
}

fn live_session(root: &Path) -> LiveSession {
    let mods = root.join("mods");
    fs::create_dir_all(&mods).expect("mods dir");
    LiveSession::connect_with_paths(
        "127.0.0.1".into(),
        1,
        "W1CTest".into(),
        "767".into(),
        SessionPaths {
            game_dir: Some(root.to_path_buf()),
            server_mods_dir: Some(mods),
            ..SessionPaths::default()
        },
    )
}

fn mrs1_v2_frame(base_block: u32, item_id: u32, name: &str, facts: u8) -> Vec<u8> {
    let mut frame = b"MRS1".to_vec();
    frame.extend_from_slice(&2u16.to_be_bytes());
    frame.extend_from_slice(&base_block.to_be_bytes());
    frame.extend_from_slice(&item_id.to_be_bytes());
    frame.extend_from_slice(&1u32.to_be_bytes());
    frame.extend_from_slice(&base_block.to_be_bytes());
    frame.extend_from_slice(&(name.len() as u16).to_be_bytes());
    frame.extend_from_slice(name.as_bytes());
    frame.extend_from_slice(&0u16.to_be_bytes());
    frame.push(facts);
    frame.extend_from_slice(&1u32.to_be_bytes());
    frame.extend_from_slice(&item_id.to_be_bytes());
    let item = "create:wrench";
    frame.extend_from_slice(&(item.len() as u16).to_be_bytes());
    frame.extend_from_slice(item.as_bytes());
    let crc = crc32_ieee(&frame);
    frame.extend_from_slice(&crc.to_be_bytes());
    frame
}

fn write_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn msy1_frame(entries: &[(&str, &str, [u8; 32])]) -> Vec<u8> {
    let mut out = b"MSY1".to_vec();
    write_u16(&mut out, entries.len() as u16);
    for (mod_id, file_name, digest) in entries {
        out.push(mod_id.len() as u8);
        out.extend_from_slice(mod_id.as_bytes());
        out.push(file_name.len() as u8);
        out.extend_from_slice(file_name.as_bytes());
        let url = format!("https://cdn.modrinth.com/data/test/{file_name}");
        write_u16(&mut out, url.len() as u16);
        out.extend_from_slice(url.as_bytes());
        out.extend_from_slice(&1024u64.to_be_bytes());
        out.extend_from_slice(digest);
    }
    out
}

fn kinetic_record(out: &mut Vec<u8>, dim: i32, pos: [i32; 3], rpm: f32) {
    out.extend_from_slice(&dim.to_be_bytes());
    for value in pos {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.extend_from_slice(&rpm.to_be_bytes());
    out.extend_from_slice(&100.0f32.to_be_bytes());
    out.extend_from_slice(&25.0f32.to_be_bytes());
    out.push(0);
}

fn varint(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn kinetic_chunk(dim: i32, generation: u64, index: u32, total: u32, rpm: f32) -> Vec<u8> {
    let mut out = b"KNT1".to_vec();
    out.push(1);
    out.extend_from_slice(&dim.to_be_bytes());
    out.extend_from_slice(&generation.to_be_bytes());
    out.extend_from_slice(&(index as i32).to_be_bytes());
    out.extend_from_slice(&(total as i32).to_be_bytes());
    varint(&mut out, 1);
    kinetic_record(&mut out, dim, [7, 64, -3], rpm);
    out
}

#[test]
fn diff_toast_formatting_and_in_process_report_preserve_windowless_host() {
    let summary = SoftReloadSummary {
        blocks_delta: 42,
        recipes_delta: 18,
        loot_overrides: 3,
        translations_delta: 310,
        generation_id: Some("gen-test".into()),
    };
    assert_eq!(
        soft_reload_summary_line(&summary, UiLang::En),
        "Reloaded: +42 blocks / +18 recipes / +310 ja_jp keys"
    );
    assert_eq!(
        soft_reload_summary_line(&summary, UiLang::Ja),
        "再読込: +42 ブロック / +18 レシピ / +310 ja_jpキー"
    );

    let root = temp_root("soft-reload");
    let mut config = GpuCliConfig {
        offline_demo: true,
        game_dir: Some(root.clone()),
        ..GpuCliConfig::default()
    };
    config.resource_pack_dir = Some(root.join("resourcepacks"));
    let mut app = GpuApp::new(config);
    let report = app.soft_reload_in_process("test");
    assert!(report.window_preserved);
    assert!(!report.restarted_integrated_server);
    assert_eq!(report.summary.translations_delta, 0);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn mod_manager_emits_rollback_apply_and_consent_decisions() {
    let mut state = ModManagerState::new(vec![ModManagerEntry::new("create", "Create", "6.0")]);
    state.ui_scale = 1;
    assert_eq!(
        state.activate(ModManagerWidgetId::RollbackProfile),
        vec![ModManagerAction::RollbackModProfile]
    );

    let (draw, hits) = state.render(1280, 900);
    assert!(!draw.commands.is_empty());
    let hit = hits
        .iter()
        .find(|hit| hit.id == ModManagerWidgetId::PlamoAction)
        .expect("PLaMo action hit region");
    let actions = state.handle_input(
        UiInputEvent::Click {
            x: hit.rect.x + hit.rect.w as i32 / 2,
            y: hit.rect.y + hit.rect.h as i32 / 2,
        },
        1280,
        900,
    );
    assert_eq!(
        actions,
        vec![ModManagerAction::InstallOrTranslatePlamo(
            PlamoTranslatePreset::Standard2B
        )]
    );
    assert!(state.plamo_panel.busy);
    state.plamo_panel.busy = false;

    state.missing_dependency_consent = Some(MissingDependencyConsentUiState {
        rows: vec![MissingDependencyConsentRow {
            mod_id: "kotlinforforge".into(),
            version_number: "4.11.0".into(),
            filename: "kotlinforforge-4.11.0.jar".into(),
            sha512: "ab".repeat(64),
            size_bytes: 123_456,
            requesting_mods: vec!["create".into()],
        }],
        unsatisfied: Vec::new(),
        isolated_mods_if_declined: vec!["create".into()],
        decision: None,
    });
    assert_eq!(
        state.activate(ModManagerWidgetId::ConsentApproveAutoFetch),
        vec![ModManagerAction::ResolveMissingDependencyConsent(
            MissingDependencyConsentDecision::ApproveAutoFetchAndSoftReload
        )]
    );
    assert_eq!(
        state
            .missing_dependency_consent
            .as_ref()
            .and_then(|c| c.decision),
        Some(MissingDependencyConsentDecision::ApproveAutoFetchAndSoftReload)
    );
    assert_eq!(
        state.activate(ModManagerWidgetId::ConsentIsolateMods),
        vec![ModManagerAction::ResolveMissingDependencyConsent(
            MissingDependencyConsentDecision::IsolateUnsatisfiedMods
        )]
    );
}

#[test]
fn plamo_labels_and_auto_translation_pack_toggle_are_instant() {
    let mut state = ModManagerState::new(Vec::new());
    state.plamo_panel.status = PlamoTranslatorStatus::NotInstalled;
    assert_eq!(
        state.plamo_panel.action_label(),
        "[翻訳モデルをインストール]"
    );
    state.plamo_panel.status = PlamoTranslatorStatus::Ready;
    assert_eq!(state.plamo_panel.action_label(), "[未翻訳 Mod を日本語化]");
    state.plamo_panel.preset = PlamoTranslatePreset::Lightweight08B;
    assert_eq!(
        state.activate(ModManagerWidgetId::PlamoPreset),
        vec![ModManagerAction::SelectPlamoPreset(
            PlamoTranslatePreset::Standard2B
        )]
    );
    assert_eq!(
        state.activate(ModManagerWidgetId::ToggleAutoTranslationPack),
        vec![ModManagerAction::ToggleAutoTranslationPack(true)]
    );

    let root = temp_root("auto-pack");
    let pack = root.join(AUTO_TRANSLATION_PACK_DIR);
    fs::create_dir_all(pack.join("assets/create/lang")).expect("pack lang dir");
    fs::write(
        pack.join("pack.mcmeta"),
        br#"{"pack":{"pack_format":34,"description":"test"}}"#,
    )
    .expect("pack meta");
    fs::write(
        pack.join("assets/create/lang/ja_jp.json"),
        r#"{"item.create.wrench":"レンチ"}"#.as_bytes(),
    )
    .expect("pack lang");
    let mut selected = Vec::new();
    set_auto_translation_pack_enabled(&mut selected, true);
    assert_eq!(selected, vec![AUTO_TRANSLATION_PACK_DIR.to_owned()]);
    assert_eq!(
        load_auto_translation_pack_ja_jp(&root)
            .expect("load generated pack")
            .get("item.create.wrench")
            .map(String::as_str),
        Some("レンチ")
    );
    set_auto_translation_pack_enabled(&mut selected, false);
    assert!(selected.is_empty());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn transactional_profile_rollback_restores_known_good_generation() {
    let root = temp_root("rollback");
    let profile = root.join("profile");
    let store = ModProfileStore::open(&profile).expect("open store");
    let jar = stored_zip(&[("fabric.mod.json", br#"{"schemaVersion":1,"id":"create"}"#)]);
    let mut first = store
        .stage_generation(SOURCE_USER_EDIT)
        .expect("stage first");
    fs::write(first.generation_dir.join("create.jar"), &jar).expect("write first jar");
    let first_entry = GenerationModEntry {
        mod_id: "create".into(),
        version: "1.0".into(),
        filename: "create.jar".into(),
        sha512: megu_client_gpu::mod_profile_tx::hash_bytes_sha512_hex(&jar),
        sha256: None,
        source_reason: SOURCE_USER_EDIT.into(),
        modrinth_project_id: None,
        modrinth_version_id: None,
        requested_by: vec!["create".into()],
    };
    store
        .publish_staged_generation(&mut first, SOURCE_USER_EDIT, vec![first_entry.clone()])
        .expect("publish first");
    let first_id = store
        .active_generation_id()
        .expect("read active")
        .expect("first active");
    store
        .record_known_good_generation(&first_id)
        .expect("record known good");

    let mut second = store
        .stage_generation(SOURCE_USER_EDIT)
        .expect("stage second");
    fs::write(second.generation_dir.join("create.jar"), &jar).expect("write second jar");
    store
        .publish_staged_generation(&mut second, SOURCE_USER_EDIT, vec![first_entry])
        .expect("publish second");
    let rollback = store
        .rollback_to_last_good_generation()
        .expect("known good rollback");
    assert_eq!(rollback.restored_generation_id, first_id);
    assert_eq!(rollback.diff.to_generation_id, first_id);
    assert!(!profile_activation_diff_line(&rollback.diff, UiLang::En).is_empty());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn live_mrs1_installs_dynamic_ids_and_msy1_produces_structured_diff() {
    let root = temp_root("live-registry");
    let mut live = live_session(&root);
    live.world = WorldState::new(DimensionMeta::default(), test_block_table());
    let dynamic_id = 1_000_042u32;
    let frame = mrs1_v2_frame(dynamic_id, 1_100_000, "create:arc_lamp", 0xff);
    live.ingest_custom_channel_payload_for_test("megu:mod_registry_sync_v1", &frame);
    assert_eq!(
        live.world.blocks.state_name(dynamic_id),
        Some("create:arc_lamp")
    );
    let facts = live
        .world
        .blocks
        .dynamic_light_fact(dynamic_id)
        .expect("dynamic light facts");
    assert_eq!(facts.emission, 15);
    assert_eq!(facts.opacity, 15);

    let jar_bytes = stored_zip(&[("fabric.mod.json", br#"{"schemaVersion":1,"id":"create"}"#)]);
    fs::write(root.join("mods/create.jar"), &jar_bytes).expect("write local mod");
    let digest = megu_core::crypto::sha256(&jar_bytes);
    let matching = msy1_frame(&[("create", "create.jar", digest)]);
    live.ingest_custom_channel_payload_for_test("megu:mod_sync_v1", &matching);
    assert!(live
        .mod_sync_diff
        .as_ref()
        .expect("matching diff")
        .matches());
    assert_eq!(live.mod_sync_rejected, 0);

    let mut other_digest = digest;
    other_digest[0] ^= 0xff;
    let mismatch = msy1_frame(&[
        ("create", "create.jar", other_digest),
        ("extra", "extra.jar", digest),
    ]);
    live.ingest_custom_channel_payload_for_test("megu:mod_sync_v1", &mismatch);
    let diff = live.mod_sync_diff.as_ref().expect("mismatch diff");
    assert!(!diff.matches());
    assert!(diff.updated.iter().any(|entry| entry.mod_id == "create"));
    assert!(diff.added.iter().any(|entry| entry.mod_id == "extra"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn kinetic_chunk_reassembly_preserves_dimensioned_positions() {
    let root = temp_root("kinetic");
    let mut live = live_session(&root);
    let first_dim0 = kinetic_chunk(0, 9, 0, 2, 32.0);
    let second_dim0 = kinetic_chunk(0, 9, 1, 2, 48.0);
    live.ingest_custom_channel_payload_for_test("create:kinetic_sync_v1", &first_dim0);
    assert_eq!(live.kinetic_pending_snapshot_groups(), 1);
    live.ingest_custom_channel_payload_for_test("create:kinetic_sync_v1", &second_dim0);
    assert_eq!(live.kinetic_pending_snapshot_groups(), 0);
    assert_eq!(
        live.kinetic_telemetry_at_in_dimension(0, (7, 64, -3))
            .map(|record| record.speed_rpm),
        Some(48.0)
    );

    let mut dim1 = Vec::new();
    varint(&mut dim1, 1);
    kinetic_record(&mut dim1, 1, [7, 64, -3], 96.0);
    live.ingest_custom_channel_payload_for_test("create:kinetic_update_v1", &dim1);
    assert_eq!(
        live.kinetic_telemetry_at_in_dimension(1, (7, 64, -3))
            .map(|record| record.speed_rpm),
        Some(96.0)
    );
    assert_eq!(
        live.kinetic_telemetry_at_in_dimension(0, (7, 64, -3))
            .map(|record| record.speed_rpm),
        Some(48.0)
    );
    let _ = fs::remove_dir_all(root);
}
