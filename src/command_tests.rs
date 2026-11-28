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
    let previous = std::mem::take(&mut *forceloaded().lock().unwrap_or_else(|e| e.into_inner()));
    let ctx = command_context(None);
    let added = run_command_ctx(&ctx, "/forceload add -1 -1", &mut None);
    let queried = run_command_ctx(&ctx, "/forceload query -1 -1", &mut None);
    let removed = run_command_ctx(&ctx, "/forceload remove all", &mut None);
    *forceloaded().lock().unwrap_or_else(|e| e.into_inner()) = previous;
    assert!(added.contains("1 chunks"), "{added}");
    assert!(queried.contains("Chunk -1, -1 is force loaded"), "{queried}");
    assert!(removed.contains("Removed 1"), "{removed}");
}

#[test]
fn entity_selectors_include_live_non_players_and_select_nearest_or_random() {
    let _world = world_guard();
    let (mut player, _hold) = crate::effect_tests::client_pair();
    player.eid = 4244;
    player.name = "selector-test".into();
    *player.gm.lock().unwrap_or_else(|e| e.into_inner()) = 1;
    *player.pos.lock().unwrap_or_else(|e| e.into_inner()) = (0.5, 16.0, 0.5, 0.0, 0.0, false);
    let player_eid = player.eid;
    clients().lock().unwrap_or_else(|e| e.into_inner()).push(Arc::new(player));
    drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
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
    drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
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
    let mob_eid = mobs().lock().unwrap_or_else(|e| e.into_inner()).last().unwrap().eid;

    {
        let mut board = scoreboard().lock().unwrap_or_else(|e| e.into_inner());
        apply_scoreboard_command(&mut board, &["objectives", "add", "selector_test", "dummy"]).unwrap();
        board.set("selector-test", "selector_test", 7).unwrap();
    }
    let player_filter = resolve_selector("@a[name=selector-test,gamemode=creative,level=0,scores={selector_test=7}]", None);
    scoreboard().lock().unwrap_or_else(|e| e.into_inner()).remove_objective("selector_test");
    let all_entities = resolve_selector("@e", None);
    let all_players = resolve_selector("@a", None);
    let nearest_item = resolve_selector("@n[type=item]", None);
    let random_item = resolve_selector("@r[type=item]", None);
    let filtered = resolve_selector(r#"@e[type=item,nbt={Item:{id:"minecraft:stone"}},distance=..2,sort=nearest,limit=1]"#, None);
    tags().lock().unwrap_or_else(|e| e.into_inner()).entry("d:4246".into()).or_default().insert("marked".into());
    let tagged = resolve_selector("@e[type=item,tag=marked,sort=furthest,limit=1]", None);
    tags().lock().unwrap_or_else(|e| e.into_inner()).remove("d:4246");

    clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != player_eid);
    drops().lock().unwrap_or_else(|e| e.into_inner()).retain(|d| d.eid != 4245 && d.eid != 4246);
    remove_live_mob(mob_eid);

    assert_eq!(player_filter, vec![player_eid.to_string()]);
    assert!(all_entities.contains(&player_eid.to_string()));
    assert!(all_entities.contains(&"d:4245".to_string()));
    assert!(all_entities.contains(&"d:4246".to_string()));
    assert!(all_entities.contains(&format!("m:{mob_eid}")));
    assert!(all_players.contains(&player_eid.to_string()));
    assert!(all_players.iter().all(|eid| eid.parse::<i32>().is_ok()));
    assert_eq!(nearest_item, vec!["d:4245".to_string()]);
    assert_eq!(filtered, vec!["d:4245".to_string()]);
    assert_eq!(tagged, vec!["d:4246".to_string()]);
    assert_eq!(random_item.len(), 1);
    assert!(random_item[0] == "d:4245" || random_item[0] == "d:4246");
}

#[test]
fn unknown_setblock_name_does_not_resolve_to_air() {
    let _guard = crate::world_guard();
    assert!(state_from_arg("not_a_block").is_none());
    if breg().is_some() {
        assert_eq!(state_from_arg("minecraft:stone"), Some(1));
    }
}

#[test]
fn summon_uses_the_mobs_initial_health() {
    let _guard = crate::world_guard();
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
    let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
    let [cow] = mobs.as_slice() else {
        panic!("expected exactly one summoned mob");
    };
    assert_eq!(cow.etype, name_to_etype("cow").unwrap());
    assert_eq!(*cow.hp.lock().unwrap_or_else(|e| e.into_inner()), 10.0);
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
    clients().lock().unwrap_or_else(|e| e.into_inner()).push(Arc::new(first));
    clients().lock().unwrap_or_else(|e| e.into_inner()).push(Arc::new(second));

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
        .unwrap_or_else(|e| e.into_inner())
        .retain(|c| c.eid != ea && c.eid != eb);
    {
        let mut tm = tags().lock().unwrap_or_else(|e| e.into_inner());
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
    let _guard = crate::world_guard();
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _directory = CommandDirectory::enter();
    std::env::remove_var("MEGU_ALLOW_PLAYER_ADMIN_COMMANDS");
    let (mut client, _socket) = crate::effect_tests::client_pair();
    client.name = "Fixture_Op".into();
    let player = command_context(Some(Arc::new(client)));
    let console = command_context(None);
    let path = std::path::Path::new(OPS_FILE);
    let run = |ctx, msg| run_command_ctx(ctx, msg, &mut None);

    assert!(run(&player, "/time query daytime").starts_with("The time is "));
    assert_eq!(
        run(&player, "/help"),
        "commands: help list time me msg tell w teammsg tm trigger"
    );
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

    OPERATORS.lock().unwrap_or_else(|e| e.into_inner()).clear();
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

    std::fs::write(path, "{\"unexpected\": \"object\"}").unwrap();
    assert!(load_operators(path).is_err());
    assert!(!player_is_admin("another"));
    assert!(set_operator(std::path::Path::new("missing/ops.json"), "fixture_op", true).is_err());
    assert!(!player_is_admin("fixture_op"));
}

#[test]
fn datapack_reload_replaces_caches_and_replays_the_load_hook() {
    let _guard = crate::world_guard();
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
            item_tags().lock().unwrap_or_else(|e| e.into_inner()).contains_key("test:bundle"),
            present
        );
        assert_eq!(
            block_tags().lock().unwrap_or_else(|e| e.into_inner()).contains_key("test:bundle"),
            present
        );
        assert!(block_tags().lock().unwrap_or_else(|e| e.into_inner()).contains_key("minecraft:logs"));
        assert_eq!(!json_recipes().lock().unwrap_or_else(|e| e.into_inner()).is_empty(), present);
        assert_eq!(!json_smelts().lock().unwrap_or_else(|e| e.into_inner()).is_empty(), present);
        assert_eq!(!loot_tables().lock().unwrap_or_else(|e| e.into_inner()).is_empty(), present);
        assert_eq!(!mob_loot_tables().lock().unwrap_or_else(|e| e.into_inner()).is_empty(), present);
        assert_eq!(
            advancements().lock().unwrap_or_else(|e| e.into_inner()).contains_key("test:first"),
            present
        );
        assert_eq!(
            predicates().lock().unwrap_or_else(|e| e.into_inner()).contains_key("test:any"),
            present
        );
        assert_eq!(
            packs()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|pack| pack.tick == vec![Arc::<str>::from("test:on_tick")]),
            present
        );
    };
    enabled(true);
    adv_event("FirstPlayer", "tick", None);
    assert!(adv_done().lock().unwrap_or_else(|e| e.into_inner())["FirstPlayer"].contains("test:first"));

    assert_eq!(run("/datapack disable absent"), "Unknown datapack 'absent'");
    assert_eq!(content_revision(), revision + 1);
    assert!(run("/datapack disable file/test.zip").starts_with("Disabled datapack"));
    assert_eq!(run("/datapack list available"), "1 packs: test.zip");
    assert_eq!(run("/reload"), "Reloading! (1 pack(s))");
    enabled(false);
    adv_event("SecondPlayer", "tick", None);
    assert!(!adv_done().lock().unwrap_or_else(|e| e.into_inner()).contains_key("SecondPlayer"));
    WORLD_TIME.store(90, Ordering::Relaxed);

    assert!(run("/datapack enable test.zip").starts_with("Enabled datapack"));
    assert_eq!(run("/datapack list available"), "0 packs: ");
    enabled(true);
    assert_eq!(WORLD_TIME.load(Ordering::Relaxed), 73);
    adv_event("SecondPlayer", "tick", None);
    assert!(adv_done().lock().unwrap_or_else(|e| e.into_inner())["SecondPlayer"].contains("test:first"));
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
    assert_eq!(
        names,
        ["help", "list", "time", "me", "msg", "tell", "w", "teammsg", "tm", "trigger"]
    );
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
    let suggest = |text, level| {
        let request = brigadier_wire::CommandSuggestionsRequest::new(313, text);
        let bytes = brigadier_wire::command_suggestions_request_payload(&request).unwrap();
        let request = brigadier_wire::parse_command_suggestions_request(&bytes).unwrap();
        command_suggestion_response(&request, level, &completions)
    };
    let response = suggest("/op Fi", 4);
    assert_eq!(response.transaction_id, 313);
    assert_eq!((response.start, response.length), (4, 2));
    assert_eq!(response.entries[0].text, "Fixture_Op");
    assert!(suggest("/op Fi", 0).entries.is_empty());
    assert_eq!(suggest("time q", 0).entries[0].text, "query");
    assert_eq!(suggest("/time set mi", 4).entries[0].text, "midnight");
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

#[test]
fn gamerule_commands_query_and_set_all_vanilla_rules() {
    let _world = world_guard();
    let ctx = command_context(None);
    for rule in VANILLA_1211_GAMERULES {
        let query = |command: &str| run_command_ctx(&ctx, command, &mut None);
        let original = query(&format!("/gamerule {}", rule.name));
        let (_, previous) = original.split_once(" = ").expect(&original);
        let new_value = match rule.kind {
            GameRuleKind::Bool => if previous == "true" { "false" } else { "true" },
            GameRuleKind::Int => "17",
        };
        let result = query(&format!("/gamerule {} {}", rule.name, new_value));
        assert!(result.ends_with(new_value), "{result}");
        assert_eq!(query(&format!("/gamerule {}", rule.name)), format!("{} = {}", rule.name, new_value));
        query(&format!("/gamerule {} {}", rule.name, previous));
    }
}

#[test]
fn command_dispatch_rejects_unclosed_quotes_and_incomplete_title() {
    let ctx = command_context(None);
    assert!(run_command_ctx(&ctx, "/title @a title \"unclosed", &mut None).starts_with("Invalid command:"));
    assert!(run_command_ctx(&ctx, "/title", &mut None).starts_with("usage:"));
    assert_eq!(run_command_ctx(&ctx, r#"/title @a title {"text": "hello world"}"#, &mut None), "Title title to 0 player(s)");
    assert!(run_command_ctx(&ctx, "/gamerule randomTickSpeed 1 extra", &mut None).starts_with("usage:"));
}

#[test]
fn clear_max_count_applies_per_player_and_zero_only_counts() {
    let _world = world_guard();
    let (mut first, _hold_a) = crate::effect_tests::client_pair();
    let (mut second, _hold_b) = crate::effect_tests::client_pair();
    first.eid = 76301;
    second.eid = 76302;
    let stone = name_to_item("stone").unwrap();
    first.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(0, (stone, 10, Vec::new()));
    second.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(0, (stone, 10, Vec::new()));
    let first = Arc::new(first);
    let second = Arc::new(second);
    clients().lock().unwrap_or_else(|e| e.into_inner()).extend([first.clone(), second.clone()]);
    let ctx = command_context(None);
    let counted = run_command_ctx(&ctx, "/clear @a stone 0", &mut None);
    let unchanged = (first.inv.lock().unwrap_or_else(|e| e.into_inner())[&0].1, second.inv.lock().unwrap_or_else(|e| e.into_inner())[&0].1);
    let cleared = run_command_ctx(&ctx, "/clear @a stone 3", &mut None);
    let remaining = (first.inv.lock().unwrap_or_else(|e| e.into_inner())[&0].1, second.inv.lock().unwrap_or_else(|e| e.into_inner())[&0].1);
    clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != first.eid && c.eid != second.eid);
    assert_eq!(counted, "Cleared 20 item(s)");
    assert_eq!(unchanged, (10, 10));
    assert_eq!(cleared, "Cleared 6 item(s)");
    assert_eq!(remaining, (7, 7));
}

#[test]
fn effect_clear_rejects_unknown_effect() {
    let ctx = command_context(None);
    assert_eq!(run_command_ctx(&ctx, "/effect clear @a not_an_effect", &mut None),
        "unknown effect 'not_an_effect'");
}

#[test]
fn enchant_rejects_invalid_levels_and_kill_removes_selected_items() {
    let _world = world_guard();
    let ctx = command_context(None);
    assert_eq!(run_command_ctx(&ctx, "/enchant nobody sharpness nope", &mut None), "Invalid enchantment level");
    drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
        eid: 5251, x: 0.5, y: 16.0, z: 0.5,
        item: name_to_item("stone").unwrap(), cnt: 1, comps: Vec::new(),
        age: 0, target: None, thrower: None,
    });
    let result = run_command_ctx(&ctx, "/kill @e[type=item,nbt={Item:{id:\"minecraft:stone\"}},limit=1]", &mut None);
    assert_eq!(result, "Killed 1");
    assert!(!drops().lock().unwrap_or_else(|e| e.into_inner()).iter().any(|item| item.eid == 5251));
}

// ---------- /spreadplayers, /particle, /playsound, raw loot tables (#197) ----------

fn packet_client(eid: i32, name: &str, x: f64, y: f64, z: f64) -> (Arc<Client>, std::net::TcpStream) {
    let (mut client, receiver) = crate::effect_tests::client_pair();
    client.eid = eid;
    client.name = name.into();
    *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (x, y, z, 0.0, 0.0, true);
    receiver
        .set_read_timeout(Some(std::time::Duration::from_millis(300)))
        .unwrap();
    (Arc::new(client), receiver)
}

fn frame_varint(r: &mut std::net::TcpStream) -> Option<(i64, usize)> {
    let mut value = 0i64;
    for i in 0..5 {
        let mut b = [0u8; 1];
        std::io::Read::read_exact(r, &mut b).ok()?;
        value |= ((b[0] & 0x7f) as i64) << (7 * i);
        if b[0] & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// One uncompressed frame: varint length, varint packet id, payload.
fn read_frame(r: &mut std::net::TcpStream) -> Option<(i64, Vec<u8>)> {
    let (len, _) = frame_varint(r)?;
    let (id, used) = frame_varint(r)?;
    let mut payload = vec![0u8; (len as usize).checked_sub(used)?];
    std::io::Read::read_exact(r, &mut payload).ok()?;
    Some((id, payload))
}

fn be_f64(payload: &[u8], at: usize) -> f64 {
    f64::from_be_bytes(payload[at..at + 8].try_into().unwrap())
}

fn be_f32(payload: &[u8], at: usize) -> f32 {
    f32::from_be_bytes(payload[at..at + 4].try_into().unwrap())
}

fn be_i32(payload: &[u8], at: usize) -> i32 {
    i32::from_be_bytes(payload[at..at + 4].try_into().unwrap())
}

#[test]
fn spreadplayers_rejects_bad_arguments_and_empty_targets() {
    let _world = world_guard();
    let ctx = command_context(None);
    let run = |msg| run_command_ctx(&ctx, msg, &mut None);
    assert!(run("/spreadplayers").starts_with("usage:"));
    assert!(run("/spreadplayers 0 0 5").starts_with("usage:"));
    assert_eq!(run("/spreadplayers 0 0 -1 10 false @a"), "Distance is too small.");
    assert_eq!(run("/spreadplayers 0 0 5 5 false @a"), "Max range is too small.");
    assert_eq!(
        run("/spreadplayers 0 0 5 10 maybe @a"),
        "'maybe' is not true or false"
    );
    assert_eq!(run("/spreadplayers 0 0 5 10 under 9999 false @a"), "Invalid max height");
    assert_eq!(run("/spreadplayers 0 0 5 10 under -100 false @a"), "Invalid max height");
    assert!(run("/spreadplayers 0 0 5 10 under @a").starts_with("usage:"));
    assert_eq!(run("/spreadplayers 0 0 5 10 false nobody_online"), "No entity found");
    // `under` at the dimension floor leaves no scannable column, so the
    // 10,000-attempt spread must fail deterministically.
    let (a, _ra) = packet_client(8301, "spread-floor", 0.5, 16.0, 0.5);
    clients().lock().unwrap_or_else(|e| e.into_inner()).push(a.clone());
    assert!(run("/spreadplayers 0 0 1 10 under -64 false spread-floor")
        .starts_with("Could not spread"));
    clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != a.eid);
}

#[test]
#[cfg(megu_vanilla_data)]
fn spreadplayers_places_players_and_mobs_deterministically() {
    let _world = world_guard();
    let (a, _ra) = packet_client(8311, "spread-a", 0.5, 16.0, 0.5);
    let (b, _rb) = packet_client(8312, "spread-b", 1.5, 16.0, 0.5);
    clients().lock().unwrap_or_else(|e| e.into_inner()).extend([a.clone(), b.clone()]);
    assert!(insert_live_mob(
        name_to_etype("cow").unwrap(),
        DimId::Overworld,
        (2.0, 16.0, 0.5, 0.0, 0.0),
        10.0,
        None,
    ));
    let mob_eid = mobs().lock().unwrap_or_else(|e| e.into_inner()).last().unwrap().eid;
    // Tag the fixture mob so the selector cannot pick up mobs left behind by
    // earlier tests in this process.
    tags()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(format!("e:{mob_eid}"))
        .or_default()
        .insert("spread_fixture".into());

    let ctx = command_context(None);
    let run = |msg| run_command_ctx(&ctx, msg, &mut None);
    let state = world_rng::saved_state();
    let first = run("/spreadplayers 0 0 5 20 false spread-a spread-b @e[tag=spread_fixture]");
    let pa = *a.pos.lock().unwrap_or_else(|e| e.into_inner());
    let pb = *b.pos.lock().unwrap_or_else(|e| e.into_inner());
    let mob_pos = {
        let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
        let m = mobs.iter().find(|m| m.eid == mob_eid).unwrap();
        let p = m.pos.lock().unwrap_or_else(|e| e.into_inner());
        *p
    };
    // Replay from the same RNG state: vanilla spread is deterministic.
    *a.pos.lock().unwrap_or_else(|e| e.into_inner()) = (0.5, 16.0, 0.5, 0.0, 0.0, true);
    *b.pos.lock().unwrap_or_else(|e| e.into_inner()) = (1.5, 16.0, 0.5, 0.0, 0.0, true);
    {
        let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
        let m = mobs.iter().find(|m| m.eid == mob_eid).unwrap();
        *m.pos.lock().unwrap_or_else(|e| e.into_inner()) = (2.0, 16.0, 0.5, 0.0, 0.0);
    }
    world_rng::restore(state);
    let second = run("/spreadplayers 0 0 5 20 false spread-a spread-b @e[tag=spread_fixture]");
    let pa2 = *a.pos.lock().unwrap_or_else(|e| e.into_inner());
    let pb2 = *b.pos.lock().unwrap_or_else(|e| e.into_inner());
    let mob_pos2 = {
        let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
        let m = mobs.iter().find(|m| m.eid == mob_eid).unwrap();
        let p = m.pos.lock().unwrap_or_else(|e| e.into_inner());
        *p
    };

    clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != 8311 && c.eid != 8312);
    remove_live_mob(mob_eid);
    tags().lock().unwrap_or_else(|e| e.into_inner()).remove(&format!("e:{mob_eid}"));

    assert!(first.starts_with("Successfully spread 3 entities"), "{first}");
    assert_eq!(first, second);
    assert_eq!(pa, pa2);
    assert_eq!(pb, pb2);
    assert_eq!(mob_pos, mob_pos2);
    for pos in [pa, pb] {
        assert!(pos.0 >= -20.5 && pos.0 <= 20.5, "x {}", pos.0);
        assert!(pos.2 >= -20.5 && pos.2 <= 20.5, "z {}", pos.2);
        // feet land on an integer surface Y inside the dimension.
        assert!((pos.1 - pos.1.round()).abs() < 1e-9, "y {}", pos.1);
        assert!(pos.1 >= f64::from(DimId::Overworld.min_y()), "y {}", pos.1);
    }
    let dx = pa.0 - pb.0;
    let dz = pa.2 - pb.2;
    assert!(dx * dx + dz * dz >= 3.5 * 3.5, "piles must keep spreadDistance");
    assert!((mob_pos.1 - mob_pos.1.round()).abs() < 1e-9);

    // `under` bounds the surface scan by maxHeight.
    let (c, _rc) = packet_client(8313, "spread-under", 0.5, 16.0, 0.5);
    clients().lock().unwrap_or_else(|e| e.into_inner()).push(c.clone());
    let under = run("/spreadplayers 0 0 1 10 under 100 false spread-under");
    let pc = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
    clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|cl| cl.eid != c.eid);
    assert!(under.starts_with("Successfully spread"), "{under}");
    assert!(pc.1 <= 100.0 && pc.1 >= -64.0, "y {}", pc.1);
}

#[test]
fn particle_validates_ids_and_encodes_viewer_scoped_packets() {
    let _world = world_guard();
    let (a, mut ra) = packet_client(8321, "particle-a", 0.5, 16.0, 0.5);
    let (b, mut rb) = packet_client(8322, "particle-b", 100.0, 16.0, 0.5);
    clients().lock().unwrap_or_else(|e| e.into_inner()).extend([a.clone(), b.clone()]);
    let ctx = command_context(None);
    let run = |msg| run_command_ctx(&ctx, msg, &mut None);

    assert!(run("/particle").starts_with("usage:"));
    assert_eq!(run("/particle not_a_particle"), "Unknown particle 'not_a_particle'");
    assert!(run("/particle flame 0 0").starts_with("usage:"));
    assert_eq!(
        run("/particle flame 0 0 0 0 0 0 0 -1"),
        "Count must be a non-negative integer"
    );
    assert!(run("/particle flame 0 0 0 0 0 0 -0.5 1").starts_with("usage:"));
    assert_eq!(run("/particle flame 1000 16 0.5"), "No players in range");

    // Full form with force + explicit viewer: only that player is in range
    // anyway for `normal`, but force widens it to 512 — the viewer argument
    // is what keeps `particle-b` (100 blocks out) from receiving it.
    let ok = run("/particle flame 0.5 16 0.5 0.1 0.2 0.3 0.5 7 force particle-a");
    assert!(ok.starts_with("Displayed"), "{ok}");
    let (id, payload) = read_frame(&mut ra).expect("particle packet for viewer");
    assert_eq!(id, 0x29);
    assert_eq!(payload[0], 1, "longDistance/force flag");
    assert_eq!(be_f64(&payload, 1), 0.5);
    assert_eq!(be_f64(&payload, 9), 16.0);
    assert_eq!(be_f64(&payload, 17), 0.5);
    assert_eq!(be_f32(&payload, 25), 0.1);
    assert_eq!(be_f32(&payload, 29), 0.2);
    assert_eq!(be_f32(&payload, 33), 0.3);
    assert_eq!(be_f32(&payload, 37), 0.5);
    assert_eq!(be_i32(&payload, 41), 7);
    // trailing varint is the particle type id (flame = 31)
    let mut pid = 0i64;
    for (i, byte) in payload[45..].iter().enumerate() {
        pid |= ((*byte & 0x7f) as i64) << (7 * i);
        if byte & 0x80 == 0 {
            break;
        }
    }
    assert_eq!(pid, i64::from(particle_id("flame").unwrap()));
    assert!(read_frame(&mut rb).is_none(), "non-viewer must not receive it");

    // Simple form: one particle at the executor position, normal range.
    let simple = run("/particle flame");
    assert!(simple.starts_with("Displayed"), "{simple}");
    let (id, payload) = read_frame(&mut ra).expect("simple particle packet");
    assert_eq!(id, 0x29);
    assert_eq!(payload[0], 0);
    assert_eq!(be_f64(&payload, 1), ctx.x);
    assert_eq!(be_i32(&payload, 41), 0, "simple form sends a single particle");
    assert!(read_frame(&mut rb).is_none(), "out of normal range");

    // `normal` mode uses the 32-block radius: move b to 40 blocks and give it
    // force explicitly through a second viewer selector.
    *b.pos.lock().unwrap_or_else(|e| e.into_inner()) = (40.5, 16.0, 0.5, 0.0, 0.0, true);
    let ranged = run("/particle flame 0.5 16 0.5 0 0 0 0 1 normal particle-b");
    assert_eq!(ranged, "No players in range");
    let forced = run("/particle flame 0.5 16 0.5 0 0 0 0 1 force particle-b");
    assert!(forced.starts_with("Displayed"), "{forced}");
    let (id, payload) = read_frame(&mut rb).expect("force reaches 40 blocks");
    assert_eq!(id, 0x29);
    assert_eq!(payload[0], 1);
    assert_eq!(be_i32(&payload, 41), 1);
    assert!(read_frame(&mut ra).is_none(), "viewer-scoped: a gets nothing");

    clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != 8321 && c.eid != 8322);
}

#[test]
fn playsound_validates_arguments_and_encodes_sound_packets() {
    let _world = world_guard();
    let (a, mut ra) = packet_client(8331, "sound-a", 0.5, 16.0, 0.5);
    clients().lock().unwrap_or_else(|e| e.into_inner()).push(a.clone());
    std::env::set_var("MEGU_ALLOW_PLAYER_ADMIN_COMMANDS", "1");
    let ctx = command_context(Some(a.clone()));
    let run = |msg| run_command_ctx(&ctx, msg, &mut None);

    assert!(run("/playsound").starts_with("usage:"));
    assert_eq!(run("/playsound not_a_sound"), "Unknown sound 'not_a_sound'");
    assert_eq!(
        run("/playsound entity.player.levelup nobody_online"),
        "No players found"
    );
    assert!(run("/playsound entity.player.levelup @a 0 0").starts_with("usage:"));
    assert_eq!(
        run("/playsound entity.player.levelup @a 0 0 0 -1"),
        "Volume must be at least 0.0"
    );
    assert_eq!(
        run("/playsound entity.player.levelup @a 0 0 0 1 3.0"),
        "Pitch must be between 0.0 and 2.0"
    );
    assert_eq!(
        run("/playsound entity.player.levelup @a 0 0 0 1 1 2.0"),
        "minVolume must be between 0.0 and 1.0"
    );

    // No targets argument: defaults to the executing player.
    let basic = run("/playsound entity.player.levelup");
    assert_eq!(basic, "Played sound to 1 player(s)");
    let (id, payload) = read_frame(&mut ra).expect("sound packet");
    assert_eq!(id, 0x68);
    // ItemSoundHolder: registry id + 1 as varint, then source category.
    let (sid, used) = {
        let mut v = 0i64;
        let mut n = 0;
        for (i, &b) in payload.iter().enumerate().take(5) {
            v |= ((b & 0x7f) as i64) << (7 * i);
            n = i + 1;
            if b & 0x80 == 0 {
                break;
            }
        }
        (v, n)
    };
    assert_eq!(sid, i64::from(sound_id("entity.player.levelup").unwrap()) + 1);
    // source varint follows the holder id
    let source = payload[used] & 0x7f;
    assert_eq!(i64::from(source), 0, "master");
    let base = used + 1;
    // executor position is the default sound position, volume/pitch default 1
    assert_eq!(be_i32(&payload, base), 4, "x = 0.5 * 8");
    assert_eq!(be_i32(&payload, base + 4), 128, "y = 16 * 8");
    assert_eq!(be_i32(&payload, base + 8), 4, "z = 0.5 * 8");
    assert_eq!(be_f32(&payload, base + 12), 1.0);
    assert_eq!(be_f32(&payload, base + 16), 1.0);
    assert_eq!(payload.len(), base + 28, "seed i64 closes the packet");

    // Full form: source, named target, position, volume, pitch, minVolume.
    let full = run("/playsound entity.player.levelup music sound-a 1 16 2 2.0 1.5 0.5");
    assert_eq!(full, "Played sound to 1 player(s)");
    let (id, payload) = read_frame(&mut ra).expect("full sound packet");
    assert_eq!(id, 0x68);
    let (sid, used) = {
        let mut v = 0i64;
        let mut n = 0;
        for (i, &b) in payload.iter().enumerate().take(5) {
            v |= ((b & 0x7f) as i64) << (7 * i);
            n = i + 1;
            if b & 0x80 == 0 {
                break;
            }
        }
        (v, n)
    };
    assert_eq!(sid, i64::from(sound_id("entity.player.levelup").unwrap()) + 1);
    assert_eq!(i64::from(payload[used] & 0x7f), 1, "music source");
    let base = used + 1;
    assert_eq!(be_i32(&payload, base), 8, "x = 1 * 8");
    assert_eq!(be_i32(&payload, base + 4), 128, "y = 16 * 8");
    assert_eq!(be_i32(&payload, base + 8), 16, "z = 2 * 8");
    assert_eq!(be_f32(&payload, base + 12), 2.0);
    assert_eq!(be_f32(&payload, base + 16), 1.5);
    assert_eq!(payload.len(), base + 28);

    // minVolume: a target outside 16*volume hears the sound re-centred two
    // blocks from itself toward the source, at minVolume.
    *a.pos.lock().unwrap_or_else(|e| e.into_inner()) = (200.0, 16.0, 0.5, 0.0, 0.0, true);
    let distant = run("/playsound entity.player.levelup @a 0 16 0 1.0 1.0 0.5");
    assert_eq!(distant, "Played sound to 1 player(s)");
    let (_id, payload) = read_frame(&mut ra).expect("minVolume packet");
    // holder varint (2 bytes for this id) + 1-byte master source
    let base = 3;
    // position ~ (198, 16, 0.5): two blocks from the player toward (0,16,0)
    let x = be_i32(&payload, base);
    assert!((x - 1584).abs() <= 2, "re-centred x {x}");
    assert_eq!(be_i32(&payload, base + 4), 128);
    assert_eq!(be_f32(&payload, base + 12), 0.5, "minVolume");
    assert_eq!(be_f32(&payload, base + 16), 1.0);

    clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != 8331);
    std::env::remove_var("MEGU_ALLOW_PLAYER_ADMIN_COMMANDS");
}

#[test]
fn datapack_raw_loot_tables_keep_json_and_follow_reload() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _directory = CommandDirectory::enter();
    let root = std::path::Path::new(DATAPACK_DIRECTORY);
    // Folder packs are recognised by their data/ tree; "folder" sorts before
    // "test.zip", so the zip is the later (winning) source for shared ids.
    std::fs::create_dir_all(root.join("folder/data/test/loot_table/chests")).unwrap();
    std::fs::write(
        root.join("folder/data/test/loot_table/chests/shared.json"),
        r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:apple"}]}],"note":"folder"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.join("folder/data/test/loot_tables/plural")).unwrap();
    std::fs::write(
        root.join("folder/data/test/loot_tables/plural/only.json"),
        r#"{"pools":[],"note":"plural-folder"}"#,
    )
    .unwrap();
    let files: &[(&str, &[u8], bool)] = &[
        (
            "data/test/loot_table/chests/shared.json",
            br#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:diamond"}]}],"note":"zip"}"#,
            false,
        ),
        (
            "data/test/loot_table/chests/zipped.json",
            br#"{"pools":[{"rolls":2,"entries":[{"type":"minecraft:item","name":"minecraft:emerald"}]}]}"#,
            true,
        ),
        (
            "data/other/loot_table/no_pools.json",
            br#"{"note":"raw only, nothing the pool parser keeps"}"#,
            false,
        ),
    ];
    std::fs::write(root.join("test.zip"), zip_fixture(files)).unwrap();

    let console = command_context(None);
    let run = |msg| run_command_ctx(&console, msg, &mut None);
    assert_eq!(run("/reload"), "Reloading! (2 pack(s))");

    // Raw JSON survives even where the bounded pool parser keeps nothing.
    assert_eq!(
        loot_table_json("other:no_pools").as_deref(),
        Some(r#"{"note":"raw only, nothing the pool parser keeps"}"#)
    );
    assert!(!loot_tables().lock().unwrap_or_else(|e| e.into_inner()).contains_key("other:no_pools"));
    // Singular and plural folder spellings share the same resource id space.
    assert_eq!(
        loot_table_json("test:plural/only").as_deref(),
        Some(r#"{"pools":[],"note":"plural-folder"}"#)
    );
    // Zip (later source) overrides the folder pack on the shared id, and the
    // parsed pool registry agrees with the raw winner.
    let shared = loot_table_json("test:chests/shared").unwrap();
    assert!(shared.contains("\"note\":\"zip\""), "{shared}");
    assert!(loot_tables().lock().unwrap_or_else(|e| e.into_inner())["test:chests/shared"]
        .iter()
        .any(|pool| !pool.entries.is_empty()));
    assert!(loot_table_json("test:chests/zipped").unwrap().contains("emerald"));
    assert!(loot_table_json("chests/shared").is_none(), "bare path is minecraft:");
    assert!(loot_table_json("missing:table").is_none());

    // Disabling the zip and reloading replaces the cache wholesale: zip-only
    // ids disappear and shared ids fall back to the folder body.
    assert!(run("/datapack disable file/test.zip").starts_with("Disabled datapack"));
    assert_eq!(run("/reload"), "Reloading! (1 pack(s))");
    assert!(loot_table_json("test:chests/zipped").is_none());
    assert!(loot_table_json("other:no_pools").is_none());
    let shared = loot_table_json("test:chests/shared").unwrap();
    assert!(shared.contains("\"note\":\"folder\""), "{shared}");
    assert!(loot_table_json("test:plural/only").is_some());
}

/// The W1 wiring lane end-to-end: every newly live command family reaches its
/// engine through `run_command_ctx`, and the `team=` selector reads the live
/// `TeamRegistry`.
#[test]
fn live_commands_dispatch_ban_whitelist_team_tick_locate_random_and_recipe() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _world = world_guard();
    let _directory = CommandDirectory::enter();
    let console = command_context(None);
    let run = |msg: &str| run_command_ctx(&console, msg, &mut None);

    let (mut alpha, _ha) = crate::effect_tests::client_pair();
    let (mut beta, _hb) = crate::effect_tests::client_pair();
    alpha.eid = 8401;
    alpha.name = "wl_alpha".into();
    beta.eid = 8402;
    beta.name = "wl_beta".into();
    let (ea, eb) = (alpha.eid, beta.eid);
    clients().lock().unwrap_or_else(|e| e.into_inner()).extend([Arc::new(alpha), Arc::new(beta)]);

    // /ban + /banlist + /pardon reach BanRegistry (and persist under ".").
    assert!(run("/ban wl_alpha").starts_with("Banned wl_alpha"));
    assert!(ban_registry().lock().unwrap_or_else(|e| e.into_inner()).player_ban("wl_alpha").is_some());
    assert!(run("/banlist").contains("wl_alpha"));
    assert!(run("/pardon wl_alpha").contains("wl_alpha"));
    assert!(ban_registry().lock().unwrap_or_else(|e| e.into_inner()).player_ban("wl_alpha").is_none());

    // /whitelist add/on/list/off/remove.
    assert!(run("/whitelist add wl_beta").contains("wl_beta"));
    assert!(run("/whitelist on").contains("turned on"));
    assert!(whitelist_registry().lock().unwrap_or_else(|e| e.into_inner()).enabled);
    assert!(run("/whitelist list").contains("wl_beta"));
    assert!(run("/whitelist off").contains("turned off"));
    assert!(run("/whitelist remove wl_beta").contains("wl_beta"));

    // /team add + join feed the live registry the `team=` selector reads.
    assert_eq!(run("/team add wl_team"), "Created team wl_team");
    assert_eq!(run("/team join wl_team wl_alpha"), "Added wl_alpha to team wl_team");
    assert_eq!(p3b_team_of("wl_alpha").as_deref(), Some("wl_team"));
    assert_eq!(resolve_selector("@a[team=wl_team]", None), vec![ea.to_string()]);
    assert_eq!(resolve_selector("@a[team=!wl_team]", None), vec![eb.to_string()]);

    // /tick freeze/step/unfreeze reach TickController, and the game_tick gate
    // skips world ticks while frozen except for queued steps.
    assert_eq!(run("/tick freeze"), "Froze the game");
    let before = TICK_IDX.load(Ordering::Relaxed);
    game_tick(|_, tick| tick());
    assert_eq!(TICK_IDX.load(Ordering::Relaxed), before, "frozen world must not tick");
    assert_eq!(run("/tick step 1"), "Stepping forward 1 tick(s)");
    game_tick(|_, tick| tick());
    assert_eq!(TICK_IDX.load(Ordering::Relaxed), before + 1, "step budget runs one tick");
    game_tick(|_, tick| tick());
    assert_eq!(TICK_IDX.load(Ordering::Relaxed), before + 1, "budget exhausted");
    assert_eq!(run("/tick unfreeze"), "The game is no longer frozen");

    // /locate and /random dispatch (found-or-not is world-dependent).
    let located = run("/locate biome minecraft:plains");
    assert!(!located.starts_with("usage:") && !located.starts_with("unknown"), "{located}");
    let rolled = run("/random roll 1..100");
    assert!(rolled.contains("Server rolled"), "{rolled}");

    // /recipe give|take against the live recipe universe.
    recipe_ids().lock().unwrap_or_else(|e| e.into_inner()).insert("fixture:stone".to_owned());
    assert_eq!(run("/recipe give wl_beta fixture:stone"), "Gave 1 new recipes to wl_beta");
    assert_eq!(run("/recipe take wl_beta fixture:stone"), "Took 1 recipes from wl_beta");
    assert_eq!(run("/recipe give wl_beta fixture:bogus"), "Unknown recipe 'fixture:bogus'");

    // Restore every shared registry the test mutated.
    run("/tick unfreeze");
    teams().lock().unwrap_or_else(|e| e.into_inner()).remove("wl_team").ok();
    whitelist_registry().lock().unwrap_or_else(|e| e.into_inner()).enabled = false;
    recipe_ids().lock().unwrap_or_else(|e| e.into_inner()).remove("fixture:stone");
    recipe_books().lock().unwrap_or_else(|e| e.into_inner()).clear();
    clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != ea && c.eid != eb);
}

/// Vanilla `hasPermission(level)`: level-0 players get only the everyone set,
/// level-2 adds cheats, level-3 adds moderation, level-4 adds server control.
/// The same levels drive the advertised command tree. Also proves `/return`
/// short-circuits a `.mcfunction` (and `$`-macro expansion when `with` args
/// are supplied).
#[test]
fn live_permission_levels_gate_commands_and_function_return_stops_execution() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _world = world_guard();
    let _directory = CommandDirectory::enter();
    std::env::remove_var("MEGU_ALLOW_PLAYER_ADMIN_COMMANDS");

    let fixture = |name: &str, eid: i32| {
        let (mut client, _hold) = crate::effect_tests::client_pair();
        client.name = name.to_owned();
        client.eid = eid;
        let client = Arc::new(client);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(client.clone());
        command_context(Some(client))
    };
    let plain = fixture("perm_none", 8411);
    let level2 = fixture("perm_two", 8412);
    let level3 = fixture("perm_three", 8413);
    let level4 = fixture("perm_four", 8414);
    {
        let mut ops = ops_registry().lock().unwrap_or_else(|e| e.into_inner());
        ops.entries.clear();
        ops.set("perm_two", 2, false);
        ops.set("perm_three", 3, false);
        ops.set("perm_four", 4, false);
    }

    // Run through the real dispatcher: each source is gated by its level.
    assert_eq!(player_permission_level("perm_none"), 0);
    assert_eq!(player_permission_level("perm_two"), 2);
    assert_eq!(player_permission_level("perm_three"), 3);
    assert_eq!(player_permission_level("perm_four"), 4);
    for ctx in [&plain, &level2, &level3, &level4] {
        assert!(run_command_ctx(ctx, "/list", &mut None).contains("players:"));
        // `/me` broadcasts and returns no reply text.
        assert!(run_command_ctx(ctx, "/me waves", &mut None).is_empty());
    }
    assert!(run_command_ctx(&plain, "/gamemode creative", &mut None).contains("permission"));
    assert!(run_command_ctx(&plain, "/team list", &mut None).contains("permission"));
    assert!(run_command_ctx(&level2, "/team list", &mut None).contains("no teams"));
    assert!(run_command_ctx(&level2, "/gamemode creative", &mut None).contains("gamemode"));
    assert!(run_command_ctx(&level2, "/ban perm_none", &mut None).contains("permission"));
    assert!(run_command_ctx(&level2, "/tick freeze", &mut None).contains("permission"));
    assert!(run_command_ctx(&level3, "/ban perm_none", &mut None).starts_with("Banned perm_none"));
    assert!(run_command_ctx(&level3, "/tick freeze", &mut None) == "Froze the game");
    run_command_ctx(&level3, "/tick unfreeze", &mut None);
    assert!(run_command_ctx(&level3, "/pardon perm_none", &mut None).contains("perm_none"));
    assert!(run_command_ctx(&level3, "/save-off", &mut None).contains("permission"));
    assert!(run_command_ctx(&level4, "/save-off", &mut None).contains("disabled"));
    run_command_ctx(&level4, "/save-on", &mut None);

    // The advertised literal tree honours the same levels (tab completion).
    let none_tree = command_tree_at_level(0);
    let two_tree = command_tree_at_level(2);
    let three_tree = command_tree_at_level(3);
    let four_tree = command_tree_at_level(4);
    for name in ["help", "list", "me", "msg", "teammsg", "tm", "trigger", "return"] {
        assert!(none_tree.child(name).is_some(), "level0 missing {name}");
    }
    for name in ["team", "tick", "locate", "random", "recipe", "reload", "ban", "op", "stop"] {
        assert!(none_tree.child(name).is_none(), "level0 must not see {name}");
    }
    for name in ["team", "locate", "random", "return", "recipe", "reload"] {
        assert!(two_tree.child(name).is_some(), "level2 missing {name}");
    }
    for name in ["ban", "pardon", "banlist", "whitelist", "kick", "op", "deop"] {
        assert!(two_tree.child(name).is_none(), "level2 must not see {name}");
        assert!(three_tree.child(name).is_some(), "level3 missing {name}");
    }
    for name in ["stop", "save-off"] {
        assert!(three_tree.child(name).is_none(), "level3 must not see {name}");
        assert!(four_tree.child(name).is_some(), "level4 missing {name}");
    }
    // `tick` is a level-3 literal in Vanilla 1.21.1.
    assert!(two_tree.child("tick").is_none());
    assert!(three_tree.child("tick").is_some());

    // `/return` ends the function's remaining lines; a macro function expands
    // `$(var)` only when `with` args are supplied.
    {
        let mut functions = HashMap::new();
        functions.insert(
            "test:w1_early".to_owned(),
            vec!["return 5".to_owned(), "time set 999999".to_owned()],
        );
        functions.insert(
            "test:w1_macro".to_owned(),
            vec!["$time set $(t)".to_owned()],
        );
        packs().lock().unwrap_or_else(|e| e.into_inner()).push(Datapack {
            name: "w1test".to_owned(),
            functions,
            load: Vec::new(),
            tick: Vec::new(),
        });
    }
    let before_time = WORLD_TIME.load(Ordering::Relaxed);
    let console = command_context(None);
    assert_eq!(run_command_ctx(&console, "/function test:w1_early", &mut None), "ran 1 commands");
    assert_ne!(WORLD_TIME.load(Ordering::Relaxed), 999999, "line after /return must not run");
    assert_eq!(run_command_ctx(&console, "/function test:w1_macro", &mut None), "ran 0 commands");
    assert_eq!(
        run_command_ctx(&console, "/function test:w1_macro with {t:7777}", &mut None),
        "ran 1 commands"
    );
    assert_eq!(WORLD_TIME.load(Ordering::Relaxed), 7777);

    // Restore shared state.
    WORLD_TIME.store(before_time, Ordering::Relaxed);
    packs().lock().unwrap_or_else(|e| e.into_inner()).retain(|p| p.name != "w1test");
    {
        let mut ops = ops_registry().lock().unwrap_or_else(|e| e.into_inner());
        ops.entries.clear();
    }
    ban_registry().lock().unwrap_or_else(|e| e.into_inner()).players.clear();
    clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| !(8411..=8414).contains(&c.eid));
}

#[test]
fn execute_non_player_sources_keep_dispatcher_permission_level() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _world = world_guard();
    let _directory = CommandDirectory::enter();
    std::env::remove_var("MEGU_ALLOW_PLAYER_ADMIN_COMMANDS");

    let fixture = |name: &str, eid: i32| {
        let (mut client, _hold) = crate::effect_tests::client_pair();
        client.name = name.to_owned();
        client.eid = eid;
        command_context(Some(Arc::new(client)))
    };
    let level2 = fixture("perm_two", 8611);
    let level3 = fixture("perm_three", 8612);
    let console = command_context(None);
    {
        let mut ops = ops_registry().lock().unwrap_or_else(|e| e.into_inner());
        ops.entries.clear();
        ops.set("perm_two", 2, false);
        ops.set("perm_three", 3, false);
    }

    let zombie = new_live_mob(
        name_to_etype("zombie").unwrap(),
        DimId::Overworld,
        (0.5, 16.0, 0.5, 0.0, 0.0),
        20.0,
        None,
    );
    let zombie_eid = zombie.eid;
    mobs().lock().unwrap_or_else(|e| e.into_inner()).push(zombie);
    let mob_eids = || {
        mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|mob| mob.eid)
            .collect::<HashSet<_>>()
    };
    let run = |ctx: &CmdCtx, msg: &str| run_command_ctx(ctx, msg, &mut None);
    let summon_and_run = |ctx: &CmdCtx, msg: &str| {
        let before = mob_eids();
        let reply = run(ctx, msg);
        for eid in mob_eids().difference(&before) {
            remove_live_mob(*eid);
        }
        reply
    };

    let denied = run(
        &level2,
        "/execute as @e[type=zombie,limit=1] run op ExecVictim",
    );
    assert!(denied.contains("permission"), "{denied}");
    assert!(!player_is_admin("ExecVictim"));
    let denied = run(
        &level2,
        "/execute as @e[type=zombie,limit=1] run ban ExecVictim",
    );
    assert!(denied.contains("permission"), "{denied}");
    assert!(
        ban_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .players
            .iter()
            .all(|entry| !entry.name.eq_ignore_ascii_case("ExecVictim"))
    );

    let denied = summon_and_run(&level2, "/execute summon minecraft:pig run stop");
    if !denied.contains("permission") {
        crate::test_support::child_note(&format!("execute stop escaped: {denied}"));
    }
    assert!(denied.contains("permission"), "{denied}");
    let denied = summon_and_run(
        &level2,
        "/execute summon minecraft:pig run ban ExecVictim",
    );
    assert!(denied.contains("permission"), "{denied}");
    let denied = summon_and_run(
        &level2,
        "/execute summon minecraft:pig run megu policy set max_frame_mib 32",
    );
    assert!(denied.contains("permission"), "{denied}");

    let reply = run(
        &level2,
        "/execute as @e[type=zombie,limit=1] run time set 1000",
    );
    assert_eq!(reply, "executed 1 command(s)", "{reply}");
    let reply = run(
        &level3,
        "/execute as @e[type=zombie,limit=1] run op LevelThreeOp",
    );
    assert_eq!(reply, "executed 1 command(s)", "{reply}");
    assert!(player_is_admin("LevelThreeOp"));
    let reply = run(
        &console,
        "/execute as @e[type=zombie,limit=1] run op ConsoleVictim",
    );
    assert_eq!(reply, "executed 1 command(s)", "{reply}");
    assert!(player_is_admin("ConsoleVictim"));

    remove_live_mob(zombie_eid);
}

/// W5: the console/RCON `status` diagnostic. A console source (me = None,
/// always level 4) gets the plain-language `format_operator_status` report;
/// a player never does — the non-Vanilla literal is absent from the advertised
/// command tree at every level and falls through to the unknown-command reply
/// in dispatch, even for an operator-level player.
#[test]
fn w5_console_status_reports_health_and_players_cannot_see_status() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _world = world_guard();
    reset_live_load_balancer();

    let console = command_context(None);
    let report = run_command_ctx(&console, "/status", &mut None);
    assert!(report.starts_with("Status: "), "{report}");
    assert!(report.contains("TPS"), "{report}");
    assert!(report.contains("ms/tick"), "{report}");
    assert!(report.contains("Players: "), "{report}");
    assert!(report.contains("Loaded chunks: "), "{report}");
    assert!(report.contains("Memory: "), "{report}");
    for jargon in ["ewma", "mspt", "stage", "budget", "scale", "alpha"] {
        assert!(
            !report.to_lowercase().contains(jargon),
            "internal term {jargon:?} leaked into the operator report: {report}"
        );
    }

    // `status` is not a Vanilla literal: it must never reach a player's
    // advertised command tree, whatever their permission level.
    for level in [0u8, 2, 3, 4] {
        assert!(
            command_tree_at_level(level).child("status").is_none(),
            "level {level} command tree exposes non-Vanilla status"
        );
    }

    let (mut player, _hold) = crate::effect_tests::client_pair();
    player.name = "w5_status_probe".to_owned();
    player.eid = 8461;
    let player = Arc::new(player);
    clients().lock().unwrap_or_else(|e| e.into_inner()).push(player.clone());
    let pctx = command_context(Some(player));

    // A level-0 player is stopped by the permission gate.
    assert!(!run_command_ctx(&pctx, "/status", &mut None).starts_with("Status:"));
    // Even a level-4 player source is not the console: `status` stays
    // console/RCON only.
    ops_registry().lock().unwrap_or_else(|e| e.into_inner()).set("w5_status_probe", 4, false);
    assert_eq!(
        run_command_ctx(&pctx, "/status", &mut None),
        "Unknown command: status"
    );

    // Restore shared state.
    ops_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .retain(|e| e.name != "w5_status_probe");
    clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != 8461);
    reset_live_load_balancer();
}

/// W5: the live smoother. `observe_live_tick` (the call the tick loop makes
/// once per measured tick) and `current_load_budget` must walk Smooth →
/// Overloaded → Smooth one stage at a time, staying fully transparent while
/// ticks are healthy.
#[test]
fn w5_live_balancer_walks_smooth_to_overloaded_and_back() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _world = world_guard();
    reset_live_load_balancer();

    // Healthy ticks stay at full rate and report Smooth.
    for _ in 0..4 {
        assert_eq!(observe_live_tick(Duration::from_millis(20)), LoadBudget::FULL);
    }
    assert_eq!(current_load_budget(), LoadBudget::FULL);
    assert_eq!(health_label(operator_health_snapshot().mspt_ms), "Smooth");

    // Sustained overload: the rolling mean climbs gradually and the stage
    // follows it — one step per tick, never a jump — while the shared
    // budget/snapshot accessors report the same state the tick loop sees.
    let budget_stage = |b: LoadBudget| {
        LOAD_BUDGETS.iter().position(|candidate| *candidate == b).unwrap() as u8
    };
    let mut stage = 0u8;
    for _ in 0..20 {
        let budget = observe_live_tick(Duration::from_millis(90));
        assert_eq!(current_load_budget(), budget);
        let next = budget_stage(budget);
        assert!(next >= stage && next <= stage + 1, "stage jumped {stage} -> {next}");
        stage = next;
    }
    assert_eq!(stage, LOAD_STAGE_MAX, "sustained overload must reach the tightest stage");
    assert!(current_load_budget().mob_spawn_scale < 1.0);
    assert_eq!(health_label(operator_health_snapshot().mspt_ms), "Overloaded");

    // Recovery walks the stage back down, again at most one step per tick,
    // until the budget is fully open and the report reads Smooth.
    let mut budget = current_load_budget();
    for _ in 0..60 {
        budget = observe_live_tick(Duration::from_millis(20));
        let next = budget_stage(budget);
        assert!(next <= stage && next + 1 >= stage, "recovery jumped {stage} -> {next}");
        stage = next;
    }
    assert_eq!(budget, LoadBudget::FULL);
    assert_eq!(current_load_budget(), LoadBudget::FULL);
    assert_eq!(health_label(operator_health_snapshot().mspt_ms), "Smooth");
    reset_live_load_balancer();
}

/// W5: the spawn helper the boot path calls. The search it delegates to walks
/// past ocean columns, a lava crust and reaches the one dry island — and the
/// live entry point on the real world always answers a finite, in-bounds
/// point whose cells are never a contact-damage block.
#[test]
#[cfg(megu_vanilla_data)]
fn w5_safe_initial_world_spawn_avoids_ocean_lava_and_returns_dry_ground() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _world = world_guard();
    let reg = breg().expect("block registry");
    let (stone, grass, water, lava) = (
        reg.state_of("stone", None),
        reg.state_of("grass_block", None),
        reg.state_of("water", None),
        reg.state_of("lava", None),
    );
    // Open ocean everywhere (floor at 40, water to 63): the spiral crosses a
    // lava crust at (8, 0) before the single dry island at (16, 0).
    let surface = |x: i32, z: i32| match (x, z) {
        (8, 0) | (16, 0) => 64,
        _ => 63,
    };
    let block = |x: i32, y: i32, z: i32| -> u32 {
        match (x, z) {
            (16, 0) => {
                if y <= 63 { stone } else if y == 64 { grass } else { 0 }
            }
            (8, 0) => {
                if y <= 63 { stone } else if y == 64 { lava } else { 0 }
            }
            _ => {
                if y <= 40 { stone } else if y <= 63 { water } else { 0 }
            }
        }
    };
    let (x, feet, z) = safe_spawn_or_fallback(&surface, &block, 62, 250);
    assert_eq!(
        (x, feet, z),
        (16.5, 65.0, 0.5),
        "the search must skip every ocean column and the lava crust"
    );
    let (bx, by, bz) = (x.floor() as i32, feet as i32, z.floor() as i32);
    assert_eq!(spawn_cell_class(block(bx, by, bz)), SpawnCell::Air);
    assert_eq!(spawn_cell_class(block(bx, by + 1, bz)), SpawnCell::Air);
    assert_eq!(spawn_cell_class(block(bx, by - 1, bz)), SpawnCell::Solid);

    // The live call the boot path makes: finite, inside the search bounds, and
    // none of the body/ground cells is a contact-damage block.
    let (x, y, z) = compute_safe_initial_world_spawn();
    assert!(x.is_finite() && y.is_finite() && z.is_finite());
    let bound = SPAWN_SEARCH_RADIUS as f64 + 0.5;
    assert!(x.abs() <= bound && z.abs() <= bound, "spawn ({x}, {z}) escaped the search radius");
    let (bx, by, bz) = (x.floor() as i32, y.floor() as i32, z.floor() as i32);
    for (dy, what) in [(0i32, "feet"), (1, "headroom"), (-1, "ground")] {
        assert_ne!(
            spawn_cell_class(block_at_dim(DimId::Overworld, bx, by + dy, bz)),
            SpawnCell::Danger,
            "spawn {what} cell must never be a contact-damage block"
        );
    }
}
