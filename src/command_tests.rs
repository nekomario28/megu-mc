use super::*;

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
    assert_eq!(run(&player, "/help"), "commands: help list time");
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
    assert_eq!(brigadier_wire::CLIENTBOUND_COMMANDS, 0x10);
    assert_eq!(brigadier_wire::SERVERBOUND_COMMAND_SUGGESTION, 0x0b);
    assert_eq!(brigadier_wire::CLIENTBOUND_COMMAND_SUGGESTIONS, 0x0f);
    let basic = command_tree(false);
    let admin = command_tree(true);
    let names: Vec<_> = basic
        .children()
        .iter()
        .map(|node| node.name().to_owned())
        .collect();
    assert_eq!(names, ["help", "list", "time"]);
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
