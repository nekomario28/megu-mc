use super::*;

#[test]
fn schedule_duration_suffixes_use_game_ticks() {
    assert_eq!(TICKS_PER_SECOND, 20);
    assert_eq!(schedule_delay_ticks("1s"), 20);
    assert_eq!(schedule_delay_ticks("1d"), 24_000);
    assert_eq!(schedule_delay_ticks("20"), 20);
    assert_eq!(schedule_delay_ticks("20t"), 20);
}

#[test]
fn spawn_packet_uses_wrapped_command_yaw() {
    let angle = spawn_yaw(Some("270.5"), None).unwrap();
    assert_eq!(angle, -89.5);
    assert_eq!(spawn_yaw(Some("~45"), None), Some(45.0));
    assert_eq!(spawn_yaw(Some("NaN"), None), None);
    let payload = spawn_position_packet((4.0, 64.0, -3.0, angle));
    assert_eq!(read_pos(&payload[..8]), (4, 64, -3));
    assert_eq!(&payload[8..], &(-89.5f32).to_be_bytes());
}

#[test]
fn weather_durations_use_seconds_and_vanilla_default() {
    assert_eq!(weather_duration_ticks(Some("60")), 1200);
    assert_eq!(weather_duration_ticks(None), 6000);
}

#[test]
fn time_add_uses_signed_tick_arithmetic() {
    assert_eq!(add_world_time(1000, 1000), 2000);
}

#[test]
fn schedule_append_replace_and_clear_manage_matching_functions() {
    let mut scheduled = vec![("pack:tick".to_string(), 10), ("pack:other".to_string(), 15)];
    schedule_function(&mut scheduled, "pack:tick", 20, true);
    assert_eq!(scheduled.len(), 3);
    schedule_function(&mut scheduled, "pack:tick", 30, false);
    assert_eq!(scheduled, vec![("pack:other".into(), 15), ("pack:tick".into(), 30)]);
    assert_eq!(clear_scheduled_functions(&mut scheduled, Some("pack:tick")), 1);
    assert_eq!(clear_scheduled_functions(&mut scheduled, None), 1);
    assert!(scheduled.is_empty());
}

#[test]
fn help_reports_specific_usage_and_alias_is_read_only() {
    assert_eq!(
        command_usage("setblock"),
        "Usage: /setblock <x> <y> <z> <block> [replace|destroy|keep]"
    );
    assert!(!command_allowed_for_player("?", &[]));
    let ctx = command_context(None);
    assert_eq!(
        run_command_ctx(&ctx, "/help setblock", &mut None),
        "Usage: /setblock <x> <y> <z> <block> [replace|destroy|keep]"
    );
}

#[test]
fn forceload_can_query_negative_chunk_and_remove_all() {
    let _world = world_guard();
    let previous = std::mem::replace(&mut *forceloaded().lock().unwrap(), HashSet::new());
    let ctx = command_context(None);
    let added = run_command_ctx(&ctx, "/forceload add -1 -1", &mut None);
    let queried = run_command_ctx(&ctx, "/forceload query -1 -1", &mut None);
    let removed = run_command_ctx(&ctx, "/forceload remove all", &mut None);
    *forceloaded().lock().unwrap() = previous;
    assert!(added.contains("1 chunks"), "{added}");
    assert!(queried.contains("Chunk -1, -1 is force loaded"), "{queried}");
    assert!(removed.contains("Removed 1"), "{removed}");
}

#[test]
fn entity_selectors_include_live_non_players_and_select_nearest_or_random() {
    let _world = world_guard();
    let (mut player, _hold) = crate::effect_tests::client_pair();
    player.eid = 4244;
    *player.pos.lock().unwrap() = (0.5, 16.0, 0.5, 0.0, 0.0, false);
    let player_eid = player.eid;
    clients().lock().unwrap().push(Arc::new(player));
    drops().lock().unwrap().push(Drop {
        eid: 4245,
        x: 1.0,
        y: 16.0,
        z: 0.5,
        item: name_to_item("stone").unwrap(),
        cnt: 1,
        comps: Vec::new(),
        age: 0,
        target: None,
        thrower: None,
    });
    drops().lock().unwrap().push(Drop {
        eid: 4246,
        x: 4.0,
        y: 16.0,
        z: 0.5,
        item: name_to_item("dirt").unwrap(),
        cnt: 1,
        comps: Vec::new(),
        age: 0,
        target: None,
        thrower: None,
    });
    assert!(insert_live_mob(
        name_to_etype("cow").unwrap(),
        DimId::Overworld,
        (2.0, 16.0, 0.5, 0.0, 0.0),
        10.0,
        None,
    ));
    let mob_eid = mobs().lock().unwrap().last().unwrap().eid;

    let all_entities = resolve_selector("@e", None);
    let all_players = resolve_selector("@a", None);
    let nearest_item = resolve_selector("@n[type=item]", None);
    let random_item = resolve_selector("@r[type=item]", None);

    clients().lock().unwrap().retain(|c| c.eid != player_eid);
    drops().lock().unwrap().retain(|d| d.eid != 4245 && d.eid != 4246);
    remove_live_mob(mob_eid);

    assert!(all_entities.contains(&player_eid.to_string()));
    assert!(all_entities.contains(&"d:4245".to_string()));
    assert!(all_entities.contains(&"d:4246".to_string()));
    assert!(all_entities.contains(&format!("m:{mob_eid}")));
    assert!(all_players.contains(&player_eid.to_string()));
    assert!(all_players.iter().all(|eid| eid.parse::<i32>().is_ok()));
    assert_eq!(nearest_item, vec!["d:4245".to_string()]);
    assert_eq!(random_item.len(), 1);
    assert!(random_item[0] == "d:4245" || random_item[0] == "d:4246");
}

#[test]
fn unknown_setblock_name_does_not_resolve_to_air() {
    assert!(state_from_arg("not_a_block").is_none());
    assert_eq!(state_from_arg("minecraft:stone"), Some(1));
}

#[test]
fn summon_uses_the_mobs_initial_health() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let ctx = CmdCtx {
        me: None,
        x: 0.0,
        y: 64.0,
        z: 0.0,
    };
    assert_eq!(
        run_command_ctx(&ctx, "/summon minecraft:cow 0 64 0", &mut None),
        "Summoned minecraft:cow"
    );
    let mobs = mobs().lock().unwrap();
    let [cow] = mobs.as_slice() else {
        panic!("expected exactly one summoned mob");
    };
    assert_eq!(cow.etype, name_to_etype("cow").unwrap());
    assert_eq!(*cow.hp.lock().unwrap(), 10.0);
}

// `/tag <targets> list <tag>` is a console/RCON path: players may only run
// help / list / time query (`command_allowed_for_player`). Vanilla reports one
// line per matched target, so a loop that returns after the first target
// silently drops the rest of the selector's matches.

#[test]
fn tag_list_reports_every_matched_target() {
    let _world = world_guard();
    let (mut first, _hold_a) = crate::effect_tests::client_pair();
    let (mut second, _hold_b) = crate::effect_tests::client_pair();
    first.name = "tag-list-alpha".into();
    second.name = "tag-list-beta".into();
    // `client_pair` hands out one fixed entity id; the selector resolves
    // targets by it, so the two must differ.
    first.eid = 4242;
    second.eid = 4243;
    let (ea, eb) = (first.eid, second.eid);
    clients().lock().unwrap().push(Arc::new(first));
    clients().lock().unwrap().push(Arc::new(second));

    let ctx = CmdCtx {
        me: None,
        x: 0.5,
        y: 16.0,
        z: 0.5,
    };
    let added = run_command_ctx(&ctx, "/tag @a add repo_test_tag", &mut None);
    let listed = run_command_ctx(&ctx, "/tag @a list repo_test_tag", &mut None);

    // put the shared registries back the way they were
    clients()
        .lock()
        .unwrap()
        .retain(|c| c.eid != ea && c.eid != eb);
    {
        let mut tm = tags().lock().unwrap();
        for st in tm.values_mut() {
            st.remove("repo_test_tag");
        }
        tm.retain(|_, st| !st.is_empty());
    }

    assert!(added.contains('2'), "both targets must be tagged: {added}");
    assert!(
        listed.contains("tag-list-alpha"),
        "first target missing from {listed}"
    );
    assert!(
        listed.contains("tag-list-beta"),
        "second target missing from {listed}"
    );
}

fn command_context(me: Option<Arc<Client>>) -> CmdCtx {
    CmdCtx {
        me,
        x: 0.5,
        y: 16.0,
        z: 0.5,
    }
}

// These fixtures change process-wide command content and the working directory,
// so run each in a fresh test process.
struct CommandDirectory {
    path: std::path::PathBuf,
    previous: std::path::PathBuf,
}

impl CommandDirectory {
    fn enter() -> Self {
        let path = std::env::temp_dir().join(format!("spine-command-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&path).unwrap();
        Self { path, previous }
    }
}

impl std::ops::Drop for CommandDirectory {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous).unwrap();
        std::fs::remove_dir_all(&self.path).unwrap();
    }
}

fn zip_fixture(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
    fn put16(header: &mut [u8], offset: usize, value: usize) {
        header[offset..offset + 2].copy_from_slice(&(value as u16).to_le_bytes());
    }
    fn put32(header: &mut [u8], offset: usize, value: usize) {
        header[offset..offset + 4].copy_from_slice(&(value as u32).to_le_bytes());
    }

    let mut local = Vec::new();
    let mut central = Vec::new();
    for &(name, plain, deflate) in files {
        let data = if deflate {
            let mut out =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
            out.write_all(plain).unwrap();
            out.finish().unwrap()
        } else {
            plain.to_vec()
        };
        let mut crc = flate2::Crc::new();
        crc.update(plain);
        let mut directory = [0; 46];
        directory[..4].copy_from_slice(b"PK\x01\x02");
        put16(&mut directory, 4, 20);
        put16(&mut directory, 6, 20);
        put16(&mut directory, 10, if deflate { 8 } else { 0 });
        put32(&mut directory, 16, crc.sum() as usize);
        put32(&mut directory, 20, data.len());
        put32(&mut directory, 24, plain.len());
        put16(&mut directory, 28, name.len());
        put32(&mut directory, 42, local.len());
        central.extend(directory);
        central.extend(name.as_bytes());

        let mut header = [0; 30];
        header[..4].copy_from_slice(b"PK\x03\x04");
        header[4..26].copy_from_slice(&directory[6..28]);
        put16(&mut header, 26, name.len());
        local.extend(header);
        local.extend(name.as_bytes());
        local.extend(data);
    }
    let mut footer = [0; 22];
    footer[..4].copy_from_slice(b"PK\x05\x06");
    put16(&mut footer, 8, files.len());
    put16(&mut footer, 10, files.len());
    put32(&mut footer, 12, central.len());
    put32(&mut footer, 16, local.len());
    local.extend(central);
    local.extend(footer);
    local
}

#[test]
fn zip_datapack_rejects_unsafe_paths_and_corrupt_records() {
    for path in [
        "../escape",
        "data/../escape",
        "data\\..\\escape",
        "/absolute",
        "C:\\absolute",
        "data/\0name",
    ] {
        assert!(
            zip_datapack_entries(&zip_fixture(&[(path, b"body", false)])).is_none(),
            "{path:?}"
        );
    }
    let mut archive =
        zip_fixture(&[("data/test/function/main.mcfunction", b"time set 310", false)]);
    assert!(zip_datapack_entries(&archive).is_some());
    let altered_body = 30 + "data/test/function/main.mcfunction".len();
    archive[altered_body] = b'4';
    assert!(zip_datapack_entries(&archive).is_none(), "payload CRC");
    assert!(
        zip_datapack_entries(&archive[..archive.len() - 1]).is_none(),
        "truncated EOCD"
    );

    let mut mismatch = zip_fixture(&[("data/test/function/main.mcfunction", b"list", true)]);
    mismatch[30] = b'D';
    assert!(
        zip_datapack_entries(&mismatch).is_none(),
        "local name must agree with directory"
    );
}

#[test]
fn zip_datapack_bounds_claimed_sizes_and_inflated_output() {
    let expanded = vec![b'X'; ZIP_MAX_ENTRY_BYTES + 1];
    let mut archive = zip_fixture(&[("oversize", &expanded, true)]);
    assert!(
        zip_datapack_entries(&archive).is_none(),
        "entry exceeds maximum"
    );
    let central = zip_u32(&archive[archive.len() - 22..], 16);
    archive[22..26].copy_from_slice(&8u32.to_le_bytes());
    archive[central + 24..central + 28].copy_from_slice(&8u32.to_le_bytes());
    assert!(
        zip_datapack_entries(&archive).is_none(),
        "expansion exceeds advertised size"
    );

    let names: Vec<_> = (0..9).map(|i| format!("payload-{i}")).collect();
    let files: Vec<_> = names
        .iter()
        .map(|name| (&**name, &expanded[..ZIP_MAX_ENTRY_BYTES], true))
        .collect();
    assert!(
        zip_datapack_entries(&zip_fixture(&files)).is_none(),
        "archive total exceeds maximum"
    );
}

#[test]
fn operators_persist_and_gate_player_commands() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _directory = CommandDirectory::enter();
    std::env::remove_var("MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS");
    let (mut client, _socket) = crate::effect_tests::client_pair();
    client.name = "Fixture_Op".into();
    let player = command_context(Some(Arc::new(client)));
    let console = command_context(None);
    let path = std::path::Path::new(OPS_FILE);
    let run = |ctx, msg| run_command_ctx(ctx, msg, &mut None);

    assert!(run(&player, "/time query daytime").starts_with("The time is "));
    assert_eq!(run(&player, "/help"), "commands: help list time me msg tell w");
    // vanilla TimeArgument has a minimum of 0, so negative /time add is rejected
    assert_eq!(run(&console, "/time add -5"), "usage: /time add <ticks>");
    assert!(run(&player, "/op Fixture_Op").contains("permission"));
    assert!(run(&player, "/time set 123").contains("permission"));
    assert!(run(&console, "/op Fixture_Op").contains("a server operator"));
    assert_eq!(
        read_operators(path).unwrap(),
        Operators::from(["fixture_op".to_owned()])
    );
    assert_eq!(run(&player, "/time set 123"), "Set time to 123");

    OPERATORS.lock().unwrap().clear();
    load_operators(path).unwrap();
    assert!(player_is_admin("FIXTURE_OP"));
    assert!(run(&player, "/op Another").contains("a server operator"));
    assert!(run(&console, "/deop fIXTURE_Op").contains("no longer"));
    assert_eq!(
        read_operators(path).unwrap(),
        Operators::from(["another".to_owned()])
    );
    load_operators(path).unwrap();
    assert!(run(&player, "/time set 456").contains("permission"));
    assert_eq!(WORLD_TIME.load(Ordering::Relaxed), 123);

    std::fs::write(path, "[\"fixture_op\"] trailing").unwrap();
    assert!(load_operators(path).is_err());
    assert!(!player_is_admin("another"));
    assert!(set_operator(std::path::Path::new("missing/ops.json"), "fixture_op", true).is_err());
    assert!(!player_is_admin("fixture_op"));
}

#[test]
fn datapack_reload_replaces_caches_and_replays_the_load_hook() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _directory = CommandDirectory::enter();
    let root = std::path::Path::new(DATAPACK_DIRECTORY);
    std::fs::create_dir_all(root.join("folder/data/test/item_modifiers")).unwrap();
    std::fs::write(
        root.join("folder/data/test/item_modifiers/folder.json"),
        r#"[{"function":"minecraft:set_damage","damage":0.5}]"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.join("folder/data/test/function")).unwrap();
    std::fs::write(
        root.join("folder/data/test/function/hello.mcfunction"),
        "list\n",
    )
    .unwrap();

    let files: &[(&str, &[u8], bool)] = &[
        ("data/test/function/stored.mcfunction", b"list\n", false),
        ("data/test/functions/on_load.mcfunction", b"# ignored\ntime set 73\n", true),
        ("data/test/function/on_tick.mcfunction", b"list\n", true),
        ("data/minecraft/tags/functions/load.json", br#"{"values":["test:on_load"]}"#, true),
        ("data/minecraft/tags/function/tick.json", br#"{"values":["test:on_tick"]}"#, false),
        ("data/test/tags/items/bundle.json", br#"{"values":["minecraft:diamond"]}"#, true),
        ("data/test/tags/block/bundle.json", br#"{"values":["minecraft:diamond_block"]}"#, false),
        ("data/test/recipe/gem.json", br#"{"type":"crafting_shapeless","ingredients":[{"item":"minecraft:stick"}],"result":{"id":"minecraft:diamond"}}"#, true),
        ("data/test/recipe/smelting.json", br#"{"type":"smelting","ingredient":{"item":"minecraft:stick"},"result":{"id":"minecraft:diamond"}}"#, true),
        ("data/test/loot_table/entities/cow.json", br#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:diamond"}]}]}"#, true),
        ("data/test/advancement/first.json", br#"{"criteria":{"once":{"trigger":"minecraft:tick"}}}"#, true),
        ("data/test/predicate/any.json", br#"{"condition":"minecraft:random_chance","chance":1.0}"#, true),
    ];
    std::fs::write(root.join("test.zip"), zip_fixture(files)).unwrap();
    std::fs::write(
        root.join("bad.zip"),
        zip_fixture(&[
            (
                "data/test/function/rejected.mcfunction",
                b"time set 99",
                false,
            ),
            ("../escape", b"", false),
        ]),
    )
    .unwrap();

    let console = command_context(None);
    let run = |msg| run_command_ctx(&console, msg, &mut None);
    let revision = content_revision();
    assert_eq!(run("/reload"), "Reloading! (2 pack(s))");
    assert_eq!(content_revision(), revision + 1);
    assert_eq!(run("/datapack list"), "2 packs: folder, test.zip");
    assert_eq!(run("/datapack list enabled"), "2 packs: folder, test.zip");
    assert_eq!(lookup_function("test:hello").unwrap(), ["list"]);
    assert_eq!(lookup_function("test:stored").unwrap(), ["list"]);
    assert_eq!(lookup_function("test:on_tick").unwrap(), ["list"]);
    assert!(lookup_function("test:rejected").is_none());
    assert_eq!(WORLD_TIME.load(Ordering::Relaxed), 73);
    let enabled = |present| {
        assert_eq!(
            item_tags().lock().unwrap().contains_key("test:bundle"),
            present
        );
        assert_eq!(
            block_tags().lock().unwrap().contains_key("test:bundle"),
            present
        );
        assert!(block_tags().lock().unwrap().contains_key("minecraft:logs"));
        assert_eq!(!json_recipes().lock().unwrap().is_empty(), present);
        assert_eq!(!json_smelts().lock().unwrap().is_empty(), present);
        assert_eq!(!loot_tables().lock().unwrap().is_empty(), present);
        assert_eq!(!mob_loot_tables().lock().unwrap().is_empty(), present);
        assert_eq!(
            advancements().lock().unwrap().contains_key("test:first"),
            present
        );
        assert_eq!(
            predicates().lock().unwrap().contains_key("test:any"),
            present
        );
        assert_eq!(
            packs()
                .lock()
                .unwrap()
                .iter()
                .any(|pack| pack.tick == ["test:on_tick"]),
            present
        );
    };
    enabled(true);
    adv_event("FirstPlayer", "tick", None);
    assert!(adv_done().lock().unwrap()["FirstPlayer"].contains("test:first"));

    assert_eq!(run("/datapack disable absent"), "Unknown datapack 'absent'");
    assert_eq!(content_revision(), revision + 1);
    assert!(run("/datapack disable file/test.zip").starts_with("Disabled datapack"));
    assert_eq!(run("/datapack list available"), "1 packs: test.zip");
    assert_eq!(run("/reload"), "Reloading! (1 pack(s))");
    enabled(false);
    adv_event("SecondPlayer", "tick", None);
    assert!(!adv_done().lock().unwrap().contains_key("SecondPlayer"));
    WORLD_TIME.store(90, Ordering::Relaxed);

    assert!(run("/datapack enable test.zip").starts_with("Enabled datapack"));
    assert_eq!(run("/datapack list available"), "0 packs: ");
    enabled(true);
    assert_eq!(WORLD_TIME.load(Ordering::Relaxed), 73);
    adv_event("SecondPlayer", "tick", None);
    assert!(adv_done().lock().unwrap()["SecondPlayer"].contains("test:first"));
}

#[test]
fn brigadier_play_tree_and_completion_respect_permissions() {
    assert_eq!(brigadier_wire::CLIENTBOUND_COMMANDS, 0x11);
    assert_eq!(brigadier_wire::SERVERBOUND_COMMAND_SUGGESTION, 0x0b);
    assert_eq!(brigadier_wire::CLIENTBOUND_COMMAND_SUGGESTIONS, 0x10);
    let basic = command_tree(false);
    let admin = command_tree(true);
    let names: Vec<_> = basic
        .children()
        .iter()
        .map(|node| node.name().to_owned())
        .collect();
    assert_eq!(names, ["help", "list", "time", "me", "msg", "tell", "w"]);
    assert!(basic.child("time").unwrap().child("set").is_none());
    assert!(admin
        .child("time")
        .unwrap()
        .child("set")
        .unwrap()
        .child("ticks")
        .is_some());
    for root in [basic, admin] {
        assert!(!brigadier_wire::commands_payload(&root).unwrap().is_empty());
    }

    let completions = CommandCompletions {
        players: vec!["Fixture_Op".into()],
        ..Default::default()
    };
    let suggest = |text, admin| {
        let request = brigadier_wire::CommandSuggestionsRequest::new(313, text);
        let bytes = brigadier_wire::command_suggestions_request_payload(&request).unwrap();
        let request = brigadier_wire::parse_command_suggestions_request(&bytes).unwrap();
        command_suggestion_response(&request, admin, &completions)
    };
    let response = suggest("/op Fi", true);
    assert_eq!(response.transaction_id, 313);
    assert_eq!((response.start, response.length), (4, 2));
    assert_eq!(response.entries[0].text, "Fixture_Op");
    assert!(suggest("/op Fi", false).entries.is_empty());
    assert_eq!(suggest("time q", false).entries[0].text, "query");
    assert_eq!(suggest("/time set mi", true).entries[0].text, "midnight");
    assert!(brigadier_wire::command_suggestions_response_payload(&response).is_ok());
    assert!(brigadier_wire::parse_command_suggestions_request(&[0, 5, b'/']).is_err());
}

// --- Canonical vanilla 1.21.1 GameRule inventory (issue #169) ----------------
//
// Transcribed from `net.minecraft.world.level.GameRules` in the 1.21.1 dedicated
// server jar (obfuscated class `dcs`, sha256
// e3bc55693e93cda0188f2e60aea28113fc647c5e85a15fa3d1b347349231b4bb), method
// `<clinit>`. Every `register(...)` call there supplies a camelCase id, a
// category and a typed default built by `BooleanValue.create` (Bool; 0/1 below)
// or `IntegerValue.create` (Int). The ids are listed alphabetically for review.
//
// This is the *full vanilla inventory*, not the subset currently attached to
// live systems. The test below checks it against independently written
// bool/int oracles so a dropped rule, typo, or changed default fails closed.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GameRuleKind {
    Bool,
    Int,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VanillaGameRule {
    name: &'static str,
    kind: GameRuleKind,
    default: i32,
}

const VANILLA_1211_GAMERULES: &[VanillaGameRule] = &[
    VanillaGameRule { name: "announceAdvancements", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "blockExplosionDropDecay", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "commandBlockOutput", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "commandModificationBlockLimit", kind: GameRuleKind::Int, default: 32768 },
    VanillaGameRule { name: "disableElytraMovementCheck", kind: GameRuleKind::Bool, default: 0 },
    VanillaGameRule { name: "disableRaids", kind: GameRuleKind::Bool, default: 0 },
    VanillaGameRule { name: "doDaylightCycle", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "doEntityDrops", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "doFireTick", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "doImmediateRespawn", kind: GameRuleKind::Bool, default: 0 },
    VanillaGameRule { name: "doInsomnia", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "doLimitedCrafting", kind: GameRuleKind::Bool, default: 0 },
    VanillaGameRule { name: "doMobLoot", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "doMobSpawning", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "doPatrolSpawning", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "doTileDrops", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "doTraderSpawning", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "doVinesSpread", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "doWardenSpawning", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "doWeatherCycle", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "drowningDamage", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "enderPearlsVanishOnDeath", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "fallDamage", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "fireDamage", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "forgiveDeadPlayers", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "freezeDamage", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "globalSoundEvents", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "keepInventory", kind: GameRuleKind::Bool, default: 0 },
    VanillaGameRule { name: "lavaSourceConversion", kind: GameRuleKind::Bool, default: 0 },
    VanillaGameRule { name: "logAdminCommands", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "maxCommandChainLength", kind: GameRuleKind::Int, default: 65536 },
    VanillaGameRule { name: "maxCommandForkCount", kind: GameRuleKind::Int, default: 65536 },
    VanillaGameRule { name: "maxEntityCramming", kind: GameRuleKind::Int, default: 24 },
    VanillaGameRule { name: "mobExplosionDropDecay", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "mobGriefing", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "naturalRegeneration", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "playersNetherPortalCreativeDelay", kind: GameRuleKind::Int, default: 1 },
    VanillaGameRule { name: "playersNetherPortalDefaultDelay", kind: GameRuleKind::Int, default: 80 },
    VanillaGameRule { name: "playersSleepingPercentage", kind: GameRuleKind::Int, default: 100 },
    VanillaGameRule { name: "projectilesCanBreakBlocks", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "randomTickSpeed", kind: GameRuleKind::Int, default: 3 },
    VanillaGameRule { name: "reducedDebugInfo", kind: GameRuleKind::Bool, default: 0 },
    VanillaGameRule { name: "sendCommandFeedback", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "showDeathMessages", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "snowAccumulationHeight", kind: GameRuleKind::Int, default: 1 },
    VanillaGameRule { name: "spawnChunkRadius", kind: GameRuleKind::Int, default: 2 },
    VanillaGameRule { name: "spawnRadius", kind: GameRuleKind::Int, default: 10 },
    VanillaGameRule { name: "spectatorsGenerateChunks", kind: GameRuleKind::Bool, default: 1 },
    VanillaGameRule { name: "tntExplosionDropDecay", kind: GameRuleKind::Bool, default: 0 },
    VanillaGameRule { name: "universalAnger", kind: GameRuleKind::Bool, default: 0 },
    VanillaGameRule { name: "waterSourceConversion", kind: GameRuleKind::Bool, default: 1 },
];

// Independent double-entry oracle, written by kind with real booleans rather
// than the table's 0/1 integers so a transcription slip is unlikely to repeat.
const EXPECTED_1211_BOOL_RULES: &[(&str, bool)] = &[
    ("announceAdvancements", true),
    ("blockExplosionDropDecay", true),
    ("commandBlockOutput", true),
    ("disableElytraMovementCheck", false),
    ("disableRaids", false),
    ("doDaylightCycle", true),
    ("doEntityDrops", true),
    ("doFireTick", true),
    ("doImmediateRespawn", false),
    ("doInsomnia", true),
    ("doLimitedCrafting", false),
    ("doMobLoot", true),
    ("doMobSpawning", true),
    ("doPatrolSpawning", true),
    ("doTileDrops", true),
    ("doTraderSpawning", true),
    ("doVinesSpread", true),
    ("doWardenSpawning", true),
    ("doWeatherCycle", true),
    ("drowningDamage", true),
    ("enderPearlsVanishOnDeath", true),
    ("fallDamage", true),
    ("fireDamage", true),
    ("forgiveDeadPlayers", true),
    ("freezeDamage", true),
    ("globalSoundEvents", true),
    ("keepInventory", false),
    ("lavaSourceConversion", false),
    ("logAdminCommands", true),
    ("mobExplosionDropDecay", true),
    ("mobGriefing", true),
    ("naturalRegeneration", true),
    ("projectilesCanBreakBlocks", true),
    ("reducedDebugInfo", false),
    ("sendCommandFeedback", true),
    ("showDeathMessages", true),
    ("spectatorsGenerateChunks", true),
    ("tntExplosionDropDecay", false),
    ("universalAnger", false),
    ("waterSourceConversion", true),
];

const EXPECTED_1211_INT_RULES: &[(&str, i32)] = &[
    ("commandModificationBlockLimit", 32768),
    ("maxCommandChainLength", 65536),
    ("maxCommandForkCount", 65536),
    ("maxEntityCramming", 24),
    ("playersNetherPortalCreativeDelay", 1),
    ("playersNetherPortalDefaultDelay", 80),
    ("playersSleepingPercentage", 100),
    ("randomTickSpeed", 3),
    ("snowAccumulationHeight", 1),
    ("spawnChunkRadius", 2),
    ("spawnRadius", 10),
];

#[test]
fn vanilla_1211_gamerule_registry_inventory() {
    assert_eq!(
        VANILLA_1211_GAMERULES.len(),
        51,
        "vanilla 1.21.1 registers exactly 51 game rules"
    );

    let mut names = HashSet::new();
    for rule in VANILLA_1211_GAMERULES {
        assert!(names.insert(rule.name), "duplicate game rule id: {}", rule.name);
        assert!(
            rule.name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase())
                && rule.name.chars().all(|c| c.is_ascii_alphanumeric()),
            "game rule ids are camelCase: {}",
            rule.name
        );
        match rule.kind {
            GameRuleKind::Bool => assert!(
                rule.default == 0 || rule.default == 1,
                "{} must have a boolean default",
                rule.name
            ),
            GameRuleKind::Int => assert!(
                rule.default >= 0,
                "{} must have a non-negative integer default",
                rule.name
            ),
        }
    }
    assert!(
        VANILLA_1211_GAMERULES
            .windows(2)
            .all(|pair| pair[0].name < pair[1].name),
        "the canonical inventory is sorted by id"
    );

    let mut bools: Vec<(&str, bool)> = VANILLA_1211_GAMERULES
        .iter()
        .filter(|rule| rule.kind == GameRuleKind::Bool)
        .map(|rule| (rule.name, rule.default == 1))
        .collect();
    bools.sort_unstable();
    assert_eq!(
        bools.as_slice(),
        EXPECTED_1211_BOOL_RULES,
        "Bool game rules and their vanilla 1.21.1 defaults"
    );

    let mut ints: Vec<(&str, i32)> = VANILLA_1211_GAMERULES
        .iter()
        .filter(|rule| rule.kind == GameRuleKind::Int)
        .map(|rule| (rule.name, rule.default))
        .collect();
    ints.sort_unstable();
    assert_eq!(
        ints.as_slice(),
        EXPECTED_1211_INT_RULES,
        "Int game rules and their vanilla 1.21.1 defaults"
    );
}
