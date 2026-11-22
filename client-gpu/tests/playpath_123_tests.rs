// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! PLAYPATH-GPU-123: standard launcher argv, title actions + `E`/`L`
//! keybinds, and auto-loaded vanilla JAR textures + `--assetsDir` sounds.

use std::path::{Path, PathBuf};

use mc_spine_client::protocol::{encode_slot_stack, ClientPlayEvent, PlayEvent};
use mc_spine_client::sound_assets::SoundLibrary;
use mc_spine_client_gpu::live_session::{discover_asset_layers, LiveSession, SessionPaths};
use mc_spine_client_gpu::window::{GpuApp, WindowInputEvent};
use mc_spine_client_gpu::{parse_gpu_args, GpuCliConfig};
use mc_spine_ui::draw::encode_png_rgba;
use mc_spine_ui::state::{Screen, UiAction};

/// Clean-room temp dir per test process (no external crates).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("mc-spine-playpath123-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Minimal stored-mode ZIP writer for synthetic JAR fixtures.
fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let lh_off = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&lh_off.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[test]
fn cli_accepts_full_official_launcher_argv_and_redacts_token() {
    let argv = [
        "--username",
        "Alex",
        "--version",
        "1.21.1",
        "--gameDir",
        "/tmp/g",
        "--assetsDir",
        "/tmp/a",
        "--assetIndex",
        "17",
        "--uuid",
        "12345678-1234-1234-1234-123456789abc",
        "--accessToken",
        "super-secret",
        "--clientId",
        "cid-1",
        "--xuid",
        "xuid-secret",
        "--clientToken",
        "client-secret",
        "--userType",
        "msa",
        "--versionType",
        "release",
        "--userProperties",
        "{}",
        "--quickPlayMultiplayer",
        "127.0.0.1:25565",
        "--width",
        "1280",
        "--height",
        "720",
    ];
    let cfg = parse_gpu_args(&argv).expect("full launcher argv must parse");
    assert_eq!(cfg.username, "Alex");
    assert_eq!(cfg.version_id.as_deref(), Some("1.21.1"));
    assert_eq!(cfg.mc_version, "1.21.1");
    assert_eq!(cfg.game_dir.as_deref(), Some(Path::new("/tmp/g")));
    assert_eq!(cfg.assets_dir.as_deref(), Some(Path::new("/tmp/a")));
    assert_eq!(cfg.asset_index.as_deref(), Some("17"));
    assert_eq!(cfg.connect, Some(("127.0.0.1".to_owned(), 25565)));
    assert_eq!(cfg.width, Some(1280));
    assert_eq!(cfg.height, Some(720));
    assert!(!cfg.show_version);

    // `=` forms and bare-`--version` version-print both behave.
    let eq = parse_gpu_args(&["--version=mc-spine-1.21.1", "--accessToken=tok"]).unwrap();
    assert_eq!(eq.version_id.as_deref(), Some("mc-spine-1.21.1"));
    assert_eq!(eq.mc_version, "1.21.1");
    assert!(parse_gpu_args(&["--version"]).unwrap().show_version);
    assert!(parse_gpu_args(&["-V"]).unwrap().show_version);

    // Zero secret leaks: no token value may appear in any Debug output.
    let debug = format!("{cfg:?}");
    for secret in ["super-secret", "xuid-secret", "client-secret"] {
        assert!(!debug.contains(secret), "Debug leaked {secret}: {debug}");
    }
    assert!(debug.contains("<redacted>"));
}

#[test]
fn e_key_opens_and_closes_own_player_inventory_and_title_actions_dispatch() {
    // A title-screen app with no auto-connect target.
    let mut app = GpuApp::new(GpuCliConfig {
        connect: Some(("127.0.0.1".to_owned(), 9)),
        ..GpuCliConfig::default()
    });
    assert_eq!(app.screen(), Screen::InGameHud);

    // Offline session with one live window-0 slot: stone at slot 10.
    let mut live = LiveSession::offline_with_null_audio();
    let mut payload = Vec::new();
    for index in 0..46 {
        let (item, count) = if index == 10 { (1, 3) } else { (0, 0) };
        payload.extend_from_slice(&encode_slot_stack(item, count));
    }
    payload.extend_from_slice(&encode_slot_stack(0, 0)); // carried
    live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetContent {
        window_id: 0,
        state_id: 7,
        slot_count: 46,
        raw_payload: payload,
    }))
    .unwrap();
    app.attach_live(live);

    // E opens the vanilla 46-slot player inventory fed by the live mirror.
    app.dispatch_input(WindowInputEvent::OpenInventory);
    assert!(app.own_inventory_open());
    assert_eq!(app.container_slot_count(), Some(46));
    let slot = app.container_slot(10).expect("slot 10 must exist");
    assert_eq!(slot.item_id, 1);
    assert_eq!(slot.count, 3);

    // E again closes it; a third E reopens with the mirror intact.
    app.dispatch_input(WindowInputEvent::OpenInventory);
    assert!(!app.own_inventory_open());
    assert!(app.container_slot_count().is_none());
    app.dispatch_input(WindowInputEvent::OpenInventory);
    assert!(app.own_inventory_open());
    app.dispatch_input(WindowInputEvent::OpenInventory);

    // L toggles the advancements overlay without depending on packets.
    assert!(!app.advancements_open());
    app.dispatch_input(WindowInputEvent::OpenAdvancements);
    assert!(app.advancements_open());
    app.dispatch_input(WindowInputEvent::OpenAdvancements);
    assert!(!app.advancements_open());

    // Title actions dispatch real transitions, never silent no-ops:
    // Multiplayer against the configured target spawns a connecting session.
    app.dispatch_ui_action(&UiAction::StartMultiplayer);
    assert_eq!(app.screen(), Screen::InGameHud);
    assert!(app.live().is_some());

    // Singleplayer with a guaranteed-missing server binary surfaces an
    // actionable status instead of silently doing nothing.
    let mut solo = GpuApp::new(GpuCliConfig {
        server_bin: Some(PathBuf::from("/nonexistent/mc-spine-integrated")),
        ..GpuCliConfig::default()
    });
    assert_eq!(solo.screen(), Screen::Title);
    solo.dispatch_ui_action(&UiAction::StartSingleplayer);
    assert_eq!(solo.screen(), Screen::Title);
    assert!(
        solo.status_messages()
            .iter()
            .any(|m| m.contains("Singleplayer")),
        "expected a Singleplayer status line, got {:?}",
        solo.status_messages()
    );

    // JoinWithCode against an instantly-refused relay surfaces the join
    // failure through the status channel too.
    let mut guest = GpuApp::new(GpuCliConfig {
        relay: Some("127.0.0.1:1".to_owned()),
        ..GpuCliConfig::default()
    });
    guest.dispatch_ui_action(&UiAction::JoinWithCode(0));
    assert!(
        guest
            .status_messages()
            .iter()
            .any(|m| m.contains("Join with code")),
        "expected a JoinWithCode status line, got {:?}",
        guest.status_messages()
    );
}

#[test]
fn auto_loads_textures_from_client_jar_and_sounds_from_assets_index() {
    let tmp = TempDir::new("assets");
    let game_dir = tmp.0.join("game");
    let assets_dir = tmp.0.join("assets");

    // Synthetic 1.21.1 client JAR with a real 16x16 PNG block texture.
    let rgba = vec![0xABu8; 16 * 16 * 4];
    let png = encode_png_rgba(16, 16, &rgba);
    let jar_dir = game_dir.join("versions").join("1.21.1");
    std::fs::create_dir_all(&jar_dir).unwrap();
    let jar = jar_dir.join("1.21.1.jar");
    std::fs::write(
        &jar,
        stored_zip(&[
            ("assets/minecraft/textures/block/stone.png", png.as_slice()),
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0".as_slice()),
        ]),
    )
    .unwrap();

    // Launcher index + object store: sounds.json plus one level-up OGG.
    let sounds_json = br#"{"entity.player.levelup":{"sounds":["entity/player/levelup"]}}"#.to_vec();
    let ogg_stub = b"OggS-stub".to_vec();
    let hash_sounds = "ab".repeat(20);
    let hash_ogg = "cd".repeat(20);
    let index_json = format!(
        "{{\"objects\":{{\
         \"minecraft/sounds.json\":{{\"hash\":\"{hash_sounds}\",\"size\":{}}},\
         \"minecraft/sounds/entity/player/levelup.ogg\":{{\"hash\":\"{hash_ogg}\",\"size\":{}}}\
         }}}}",
        sounds_json.len(),
        ogg_stub.len()
    );
    let indexes = assets_dir.join("indexes");
    std::fs::create_dir_all(&indexes).unwrap();
    std::fs::write(indexes.join("17.json"), index_json).unwrap();
    let obj_sounds = assets_dir.join("objects").join(&hash_sounds[..2]);
    std::fs::create_dir_all(&obj_sounds).unwrap();
    std::fs::write(obj_sounds.join(&hash_sounds), &sounds_json).unwrap();
    let obj_ogg = assets_dir.join("objects").join(&hash_ogg[..2]);
    std::fs::create_dir_all(&obj_ogg).unwrap();
    std::fs::write(obj_ogg.join(&hash_ogg), &ogg_stub).unwrap();

    let paths = SessionPaths {
        game_dir: Some(game_dir.clone()),
        assets_dir: Some(assets_dir.clone()),
        asset_index: Some("17".to_owned()),
        version_id: Some("1.21.1".to_owned()),
        ..SessionPaths::default()
    };

    // Textures: the JAR layer resolves automatically (no env override).
    let (layers, discovery, _) = discover_asset_layers(&paths);
    assert_eq!(discovery.jar_path.as_deref(), Some(jar.as_path()));
    assert!(discovery.jar_textures >= 1, "JAR contributed no textures");
    let jar_layer = layers.first().expect("JAR layer must be first");
    let sprite = jar_layer
        .sprites
        .get("minecraft:block/stone")
        .expect("stone sprite missing from JAR layer");
    assert_eq!((sprite.0, sprite.1), (16, 16));
    assert_eq!(sprite.2.len(), 16 * 16 * 4);

    // Sounds: the index maps the event to the content-addressed object.
    let library = SoundLibrary::load(&assets_dir, "17").expect("asset index must load");
    assert_eq!(library.event_count(), 1);
    let resolved = library
        .resolve("entity.player.levelup", 0)
        .expect("levelup event must resolve");
    assert!(resolved.path.is_file(), "resolved object must exist");
    assert!(resolved.path.starts_with(assets_dir.join("objects")));

    // The same paths reach the session's audio configuration.
    let session = LiveSession::offline_with_paths(paths);
    assert_eq!(
        session.resolved_assets_dir.as_deref(),
        Some(assets_dir.as_path())
    );
    assert_eq!(session.resolved_asset_index.as_deref(), Some("17"));
}
