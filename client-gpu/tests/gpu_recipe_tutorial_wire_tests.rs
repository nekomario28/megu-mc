// SPDX-License-Identifier: LGPL-3.0-or-later
//! Lane GPU-RECIPE-TUTORIAL-WIRE: clean-room verification that real mod-JAR
//! datapack content (recipes, item tags, world-drops loot) flows into
//! `RecipeBrowserState.index` with zero disk extraction, and that the
//! window layer dispatches `TransferToCraftingGrid` plus the `R`/`U`/`W`
//! hover hotkeys.
//!
//! Every fixture is synthesized in-process: a Deflate-compressed JAR is
//! written to a per-test temp dir and read back through `LazyZipIndex`.
//! No third-party JARs and no outside-fixture paths are referenced.

use std::path::PathBuf;

use megu_client::protocol::{encode_slot_stack, ClientPlayEvent, PlayEvent};
use megu_client_gpu::live_session::{LiveSession, SessionStatus};
use megu_client_gpu::recipe_ingest::{
    ingest_mod_jars_into_ui, ingest_mod_recipes_into_ui,
};
use megu_client_gpu::window::{GpuApp, RecipeFocus, WindowInputEvent};
use megu_client_gpu::GpuCliConfig;
use megu_ui::recipe_browser::{
    BrowserTab, DetailMode, RecipeBrowserAction, RecipeBrowserState, RecipeCategory,
};
use megu_ui::state::{Screen, UiAction};

// ---------------------------------------------------------------------------
// Clean-room fixtures (no external crates — mirrors the playpath_123 pattern)
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("megu-recipewire-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Recursive regular-file count — the zero-extraction witness.
    fn file_count(&self) -> usize {
        fn walk(dir: &PathBuf, out: &mut usize) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    *out += 1;
                }
            }
        }
        let mut n = 0;
        walk(&self.0, &mut n);
        n
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn w16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn w32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// LSB-first deflate bit writer (fixed-Huffman literals only — enough to
/// prove real DEFLATE entries decode; the format is pinned by
/// `core::zip_archive`'s own tests).
struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            n: 0,
        }
    }
    fn put(&mut self, v: u32, k: u32) {
        self.acc |= v << self.n;
        self.n += k;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    fn put_code(&mut self, code: u32, k: u32) {
        for i in (0..k).rev() {
            self.put((code >> i) & 1, 1);
        }
    }
    fn done(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// Single-block fixed-Huffman raw-deflate stream (RFC 1951 §3.2.6).
fn deflate_fixed(data: &[u8]) -> Vec<u8> {
    let mut w = Bits::new();
    w.put(1, 1);
    w.put(0b01, 2);
    for &b in data {
        if b < 144 {
            w.put_code(0x30 + b as u32, 8);
        } else {
            w.put_code(0x190 + b as u32 - 144, 9);
        }
    }
    w.put_code(0, 7); // 256: end of block
    w.done()
}

/// Minimal ZIP writer — `method 8` (Deflate) for every entry.
fn deflate_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, plain) in entries {
        let stored = deflate_fixed(plain);
        let lh_off = out.len() as u32;
        w32(&mut out, 0x0403_4b50);
        w16(&mut out, 20);
        w16(&mut out, 0);
        w16(&mut out, 8);
        w16(&mut out, 0);
        w16(&mut out, 0);
        let crc = megu_core::hash::crc32_ieee(plain);
        w32(&mut out, crc);
        w32(&mut out, stored.len() as u32);
        w32(&mut out, plain.len() as u32);
        w16(&mut out, name.len() as u16);
        w16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&stored);

        w32(&mut central, 0x0201_4b50);
        w16(&mut central, 20);
        w16(&mut central, 20);
        w16(&mut central, 0);
        w16(&mut central, 8);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, crc);
        w32(&mut central, stored.len() as u32);
        w32(&mut central, plain.len() as u32);
        w16(&mut central, name.len() as u16);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, 0);
        w32(&mut central, lh_off);
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    w32(&mut out, 0x0605_4b50);
    w16(&mut out, 0);
    w16(&mut out, 0);
    w16(&mut out, entries.len() as u16);
    w16(&mut out, entries.len() as u16);
    w32(&mut out, central.len() as u32);
    w32(&mut out, cd_offset);
    w16(&mut out, 0);
    out
}

// ---------------------------------------------------------------------------
// Test 1 — Deflate mod JAR → categories, tags, world-drops loot, craft tree, no temps
// ---------------------------------------------------------------------------

#[test]
fn recipe_ingest_loads_deflate_mod_jar_custom_categories_tags_and_world_drops_loot_zero_temp_files()
 {
    let tmp = TempDir::new("jar");
    let jar = tmp.0.join("my_factory-1.0.0.jar");

    let grinding = br#"{
        "type": "my_factory:grinding",
        "ingredients": [{ "tag": "my_factory:ingots" }],
        "results": [{ "id": "my_factory:dust", "count": 2 }],
        "processingTime": 100
    }"#;
    let mixing = br#"{
        "type": "create:mixing",
        "ingredients": [
            { "item": "minecraft:wheat" },
            { "item": "minecraft:water_bucket" }
        ],
        "results": [{ "id": "my_factory:dough", "count": 1 }],
        "heatRequirement": "heated"
    }"#;
    let smelt_ingot = br#"{
        "type": "minecraft:smelting",
        "ingredient": { "item": "my_factory:raw_ore" },
        "result": { "id": "my_factory:ingot_a", "count": 1 }
    }"#;
    let tag = br#"{
        "replace": false,
        "values": ["my_factory:ingot_a", "my_factory:ingot_b"]
    }"#;
    let entity_loot = br#"{
        "pools": [{
            "rolls": 1,
            "entries": [
                { "type": "minecraft:item", "name": "my_factory:ingot_a", "weight": 3 },
                {
                    "type": "minecraft:item",
                    "name": "minecraft:iron_nugget",
                    "weight": 1,
                    "functions": [{
                        "function": "minecraft:set_count",
                        "count": { "min": 2, "max": 4 }
                    }]
                }
            ]
        }]
    }"#;
    let chest_loot = br#"{
        "pools": [{
            "rolls": 2,
            "entries": [{ "type": "minecraft:item", "name": "my_factory:dust" }]
        }]
    }"#;

    std::fs::write(
        &jar,
        deflate_zip(&[
            (
                "data/my_factory/recipes/grind_ingots.json",
                grinding.as_slice(),
            ),
            ("data/my_factory/recipes/mix_dough.json", mixing.as_slice()),
            (
                "data/my_factory/recipes/smelt_ingot.json",
                smelt_ingot.as_slice(),
            ),
            ("data/my_factory/tags/item/ingots.json", tag.as_slice()),
            (
                "data/my_factory/loot_table/entities/golem.json",
                entity_loot.as_slice(),
            ),
            (
                "data/my_factory/loot_table/chests/factory.json",
                chest_loot.as_slice(),
            ),
        ]),
    )
    .unwrap();

    let mut browser = RecipeBrowserState::default();
    let report = ingest_mod_jars_into_ui(std::slice::from_ref(&jar), &mut browser);
    let index = &browser.index;

    // Source accounting — one archive, every entry classified, nothing
    // failed closed.
    assert_eq!(report.sources_scanned, 1);
    assert_eq!(report.sources_failed, 0);
    assert_eq!(report.recipes_failed, 0, "notes: {:?}", report.notes);
    assert_eq!(report.json_failed, 0, "notes: {:?}", report.notes);
    assert_eq!(report.recipes_loaded, 3);
    assert_eq!(report.tags_loaded, 1);
    assert!(report.mob_drops >= 2, "mob drops: {}", report.mob_drops);
    assert!(report.dungeon_loot >= 1);
    assert_eq!(index.recipes.len(), 3);

    // The unknown `my_factory:grinding` type auto-registers a declared
    // custom category (zero per-mod hardcoding), while `create:mixing`
    // keeps its built-in category.
    let grind = index
        .recipe("my_factory:grind_ingots")
        .expect("grinding recipe must load");
    assert_eq!(
        grind.category,
        RecipeCategory::CustomModCategory {
            id: "my_factory:grinding".to_owned(),
            title_en: "My Factory: grinding".to_owned(),
            title_ja: "My Factory".to_owned(),
        }
    );
    let mixed = index
        .recipe("my_factory:mix_dough")
        .expect("create:mixing recipe must load");
    assert_eq!(mixed.category, RecipeCategory::CreateMixing);
    assert!(report.namespaces_registered >= 2, "my_factory + create");

    // `#my_factory:ingots` resolves through the tag: both members carry it
    // and the ingredient slot resolves to a concrete item.
    for member in ["my_factory:ingot_a", "my_factory:ingot_b"] {
        let entry = index.item(member).expect("tag member must be catalogued");
        assert!(
            entry.tags.iter().any(|t| t == "my_factory:ingots"),
            "{member} must carry my_factory:ingots"
        );
    }
    assert_eq!(
        index.resolve_ingredient(&grind.inputs[0]),
        "my_factory:ingot_a"
    );
    assert!(index.ingredient_accepts(&grind.inputs[0], "my_factory:ingot_b"));

    // world-drops tables populated from the Deflate loot documents.
    let golem = index
        .mob_drops
        .iter()
        .filter(|d| d.mob_id == "my_factory:golem")
        .count();
    assert_eq!(golem, 2, "golem table emits two rows");
    let nugget = index
        .mob_drops
        .iter()
        .find(|d| d.item_id == "minecraft:iron_nugget")
        .expect("nugget row");
    assert_eq!((nugget.min_count, nugget.max_count), (2, 4));
    assert!(index
        .dungeon_loot
        .iter()
        .any(|d| d.chest == "my_factory:chests/factory" && d.item_id == "my_factory:dust"));

    // Craft tree: dust ← grinding(#ingots → ingot_a) ← smelting(raw_ore).
    let tree = index.recipe_tree("my_factory:dust", 2, &Default::default());
    let root = tree.root.as_ref().expect("tree root");
    assert_eq!(root.item, "my_factory:dust");
    assert_eq!(root.recipe_id.as_deref(), Some("my_factory:grind_ingots"));
    assert!(
        tree.raw_materials.contains_key("my_factory:raw_ore"),
        "tree must bottom out at raw_ore: {:?}",
        tree.raw_materials
    );

    // Zero extraction: the only file under the fixture dir is the JAR
    // itself — no spilled entry files, no unpacked dirs.
    assert_eq!(tmp.file_count(), 1, "ingest wrote files: {:?}", tmp.0);
}

#[test]
fn recipe_ingest_boot_scans_game_dir_mods_in_deterministic_order() {
    let tmp = TempDir::new("boot");
    let mods = tmp.0.join("game").join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    let recipe = br#"{
        "type": "alpha_mod:pressing",
        "ingredients": [{ "item": "alpha_mod:plate_raw" }],
        "results": [{ "id": "alpha_mod:plate" }]
    }"#;
    std::fs::write(
        mods.join("alpha_mod.jar"),
        deflate_zip(&[(
            "data/alpha_mod/recipes/press_plate.json",
            recipe.as_slice(),
        )]),
    )
    .unwrap();

    let app = GpuApp::new(GpuCliConfig {
        game_dir: Some(tmp.0.join("game")),
        ..GpuCliConfig::default()
    });
    let report = app.recipe_ingest_report();
    // The vanilla client JAR now resolves through the unified resolver, so
    // hosts with a real launcher install additionally ingest the vanilla
    // recipe set — exact totals are host-dependent. The fixture mod's
    // recipe must always be present.
    assert!(report.sources_scanned >= 1);
    assert!(report.recipes_loaded >= 1);
    assert!(app
        .recipe_browser()
        .index
        .recipe("alpha_mod:press_plate")
        .is_some());
    assert!(app.status_messages().iter().all(|m| !m.starts_with("Recipe ingest:")));
    // The JAR stayed a JAR — nothing unpacked next to it.
    assert_eq!(tmp.file_count(), 1);
}

// ---------------------------------------------------------------------------
// Test 2 — `+` transfer + R/U/W hotkey dispatch in window state
// ---------------------------------------------------------------------------

/// Attach a `Playing` offline session whose window-0 mirror holds a stone
/// stack at slot 10, then open `E` — the state R/U/W need.
fn app_with_inventory() -> (GpuApp, TempDir) {
    // Scratch `game_dir`: `Ctrl+O` / settings writes persist under it —
    // never into the crate root (where the next test's auto-load would
    // pick them up). The guard is returned so the dir outlives the app.
    let game = TempDir::new("game");
    let mut app = GpuApp::new(GpuCliConfig {
        game_dir: Some(game.0.clone()),
        ..GpuCliConfig::default()
    });
    let mut live = LiveSession::offline_with_null_audio();
    live.status = SessionStatus::Playing;
    let mut payload = Vec::new();
    for index in 0..46 {
        let (item, count) = if index == 10 { (1, 5) } else { (0, 0) };
        payload.extend_from_slice(&encode_slot_stack(item, count));
    }
    payload.extend_from_slice(&encode_slot_stack(0, 0)); // carried
    live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetContent {
        window_id: 0,
        state_id: 1,
        slot_count: 46,
        raw_payload: payload,
    }))
    .unwrap();
    app.attach_live(live);
    app.dispatch_input(WindowInputEvent::OpenInventory);
    assert!(app.own_inventory_open());
    (app, game)
}

#[test]
fn recipe_plus_autofill_and_r_u_w_hotkeys_dispatch_in_window_state() {
    let (mut app, _game) = app_with_inventory();
    assert_eq!(app.container_slot_count(), Some(46));

    // `+` transfer while Playing: the window-0 place_recipe request is
    // recorded on the session's bounded dispatch log — proof the action
    // was never dropped. `max_craft > 1` maps to make_all=true.
    app.dispatch_ui_action(&UiAction::RecipeBrowser(
        RecipeBrowserAction::TransferToCraftingGrid {
            recipe_id: "minecraft:stone_bricks".to_owned(),
            max_craft: 4,
        },
    ));
    let live = app.live().expect("live session attached");
    assert_eq!(
        live.place_recipe_requests,
        vec![(0u8, "minecraft:stone_bricks".to_owned(), true)]
    );
    assert!(app
        .status_messages()
        .iter()
        .any(|m| m.contains("minecraft:stone_bricks")));

    // Offline the same action surfaces an explicit status — never silent.
    let mut offline = GpuApp::new(GpuCliConfig::default());
    offline.dispatch_ui_action(&UiAction::RecipeBrowser(
        RecipeBrowserAction::TransferToCraftingGrid {
            recipe_id: "minecraft:stick".to_owned(),
            max_craft: 1,
        },
    ));
    assert!(
        offline
            .status_messages()
            .iter()
            .any(|m| m.contains("Place recipe minecraft:stick")),
        "offline transfer must surface a status: {:?}",
        offline.status_messages()
    );

    // Hover the stone stack (slot 10, protocol item 1 → minecraft:stone)
    // and fire each focus hotkey through the shared event path.
    app.set_container_hover(Some(10));

    app.dispatch_input(WindowInputEvent::RecipeBrowserFocus(RecipeFocus::Recipes));
    assert_eq!(app.screen(), Screen::RecipeBrowser);
    assert_eq!(
        app.recipe_browser().detail,
        Some(("minecraft:stone".to_owned(), DetailMode::Recipes))
    );

    app.dispatch_input(WindowInputEvent::RecipeBrowserFocus(RecipeFocus::Uses));
    assert_eq!(app.screen(), Screen::RecipeBrowser);
    assert_eq!(
        app.recipe_browser().detail,
        Some(("minecraft:stone".to_owned(), DetailMode::Uses))
    );

    app.dispatch_input(WindowInputEvent::RecipeBrowserFocus(RecipeFocus::MachineTutorial));
    assert_eq!(app.screen(), Screen::RecipeBrowser);
    assert_eq!(app.recipe_browser().tab, BrowserTab::MachineTutorials);
    assert!(
        app.recipe_browser().machine_tutorial.is_some(),
        "W must build a MachineTutorial scene for the hovered item"
    );

    // An empty hovered slot is a no-op — the key falls back to its normal
    // binding (R sorts, W moves) instead of opening a bogus detail.
    app.dispatch_input(WindowInputEvent::OpenInventory); // close
    app.dispatch_input(WindowInputEvent::OpenInventory); // reopen
    app.set_container_hover(Some(11)); // empty slot
    app.dispatch_input(WindowInputEvent::RecipeBrowserFocus(RecipeFocus::Recipes));
    assert_eq!(
        app.recipe_browser().detail,
        Some(("minecraft:stone".to_owned(), DetailMode::Uses)),
        "empty-slot focus must leave the browser state untouched"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — loose datapack directory ingestion (singular+plural layouts)
// ---------------------------------------------------------------------------

#[test]
fn recipe_ingest_reads_loose_datapack_dir_and_plural_layouts() {
    let tmp = TempDir::new("pack");
    // A datapack dir under saves/world/datapacks/ using the *plural*
    // `loot_tables/` + `tags/items/` layout (pre-1.21 naming).
    let pack = tmp
        .0
        .join("game")
        .join("saves")
        .join("world")
        .join("datapacks")
        .join("loose_pack");
    let loot_dir = pack.join("data").join("loose").join("loot_tables");
    let tag_dir = pack.join("data").join("loose").join("tags").join("items");
    let recipe_dir = pack.join("data").join("loose").join("recipes");
    std::fs::create_dir_all(loot_dir.join("entities")).unwrap();
    std::fs::create_dir_all(&tag_dir).unwrap();
    std::fs::create_dir_all(&recipe_dir).unwrap();

    std::fs::write(
        tag_dir.join("gems.json"),
        br#"{"values":["loose:ruby","loose:sapphire"]}"#,
    )
    .unwrap();
    std::fs::write(
        loot_dir.join("entities").join("sprite.json"),
        br#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"loose:ruby"}]}]}"#,
    )
    .unwrap();
    std::fs::write(
        recipe_dir.join("alloy.json"),
        br#"{
            "type": "loose:alloying",
            "ingredients": [{ "tag": "loose:gems" }],
            "results": [{ "id": "loose:alloy" }]
        }"#,
    )
    .unwrap();

    let paths = megu_client_gpu::live_session::SessionPaths {
        game_dir: Some(tmp.0.join("game")),
        ..Default::default()
    };
    let mut browser = RecipeBrowserState::default();
    let report = ingest_mod_recipes_into_ui(&paths, &mut browser);

    // Vanilla-JAR ingestion is host-dependent (unified resolver), so exact
    // totals vary; the loose pack's rows must always be present.
    assert!(report.sources_scanned >= 1);
    assert!(report.recipes_loaded >= 1);
    assert!(report.tags_loaded >= 1);
    assert!(report.mob_drops >= 1);
    assert!(browser
        .index
        .mob_drops
        .iter()
        .any(|d| d.mob_id == "loose:sprite" && d.item_id == "loose:ruby"));
    assert!(matches!(
        browser.index.recipe("loose:alloy").map(|r| &r.category),
        Some(RecipeCategory::CustomModCategory { id, .. }) if id == "loose:alloying"
    ));
    assert_eq!(
        browser
            .index
            .resolve_ingredient(&browser.index.recipe("loose:alloy").unwrap().inputs[0]),
        "loose:ruby"
    );
}

// ---------------------------------------------------------------------------
// Test 4 — Ctrl+O overlay toggle + `A`/`R`/`Backspace` overlay hotkeys
// ---------------------------------------------------------------------------

#[test]
fn ctrl_o_toggles_overlay_and_a_r_backspace_drive_overlay_state() {
    let (mut app, _game) = app_with_inventory();
    assert_eq!(app.container_slot_count(), Some(46));
    assert!(!app.recipe_overlay_enabled(), "overlay defaults OFF");

    // Ctrl+O flips `recipe_overlay_enabled` in place — twice restores it.
    app.set_modifiers(winit::keyboard::ModifiersState::CONTROL);
    assert!(app.handle_overlay_key(winit::keyboard::KeyCode::KeyO, true));
    assert!(app.recipe_overlay_enabled());
    assert!(app.handle_overlay_key(winit::keyboard::KeyCode::KeyO, true));
    assert!(!app.recipe_overlay_enabled());
    app.handle_overlay_key(winit::keyboard::KeyCode::KeyO, true);
    app.set_modifiers(winit::keyboard::ModifiersState::empty());

    // `A` bookmarks the hovered container stack (slot 10 = stone).
    app.set_container_hover(Some(10));
    assert!(app.handle_overlay_key(winit::keyboard::KeyCode::KeyA, true));
    assert!(app
        .recipe_browser()
        .bookmarks
        .iter()
        .any(|b| b == "minecraft:stone"));
    // Second `A` toggles it back off.
    assert!(app.handle_overlay_key(winit::keyboard::KeyCode::KeyA, true));
    assert!(!app
        .recipe_browser()
        .bookmarks
        .iter()
        .any(|b| b == "minecraft:stone"));

    // `R` opens Recipes *inside the overlay* — the owning screen stays
    // put (container is never navigated away to full-screen).
    let screen_before = app.screen();
    assert!(app.handle_overlay_key(winit::keyboard::KeyCode::KeyR, true));
    assert_eq!(app.screen(), screen_before);
    assert_eq!(
        app.recipe_browser().detail,
        Some(("minecraft:stone".to_owned(), DetailMode::Recipes))
    );
    // Backspace walks detail history back, keeping the container open.
    assert!(app.handle_overlay_key(winit::keyboard::KeyCode::Backspace, true));
    assert!(app.recipe_browser().detail.is_none());
    assert_eq!(app.screen(), screen_before);
    assert!(app.own_inventory_open());
}

// ---------------------------------------------------------------------------
// Test 5 — en_us + active-locale lang ingest (arbitrary locale, zero
// hardcoding), furnace_fuels data map, and the item-icon atlas
// ---------------------------------------------------------------------------

/// Minimal real PNG encoder: `w*h` RGBA8, one zlib stream per file —
/// enough for the lazy-JAR asset layer to decode like a mod sprite.
fn tiny_png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut row = Vec::with_capacity((w * 4 + 1) as usize);
    row.push(0); // filter: none
    for _ in 0..w {
        row.extend_from_slice(&rgba);
    }
    let raw = row.repeat(h as usize);
    // zlib stream: 0x78 0x01 + stored deflate blocks + adler32.
    let mut z = vec![0x78u8, 0x01];
    let mut chunks = raw.chunks(u16::MAX as usize).peekable();
    while let Some(block) = chunks.next() {
        let last = chunks.peek().is_none();
        z.push(if last { 1 } else { 0 });
        z.extend_from_slice(&(block.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    // adler32 of the uncompressed data.
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&(b << 16 | a).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut chunk = |tag: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(tag);
        out.extend_from_slice(data);
        let crc = megu_core::hash::crc32_ieee(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    };
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}

#[test]
fn lang_fuels_and_icon_atlas_ingest_arbitrary_mods_and_locales() {
    let tmp = TempDir::new("lang");
    let jar = tmp.0.join("my_mod-2.0.0.jar");

    let en = br#"{ "item.my_mod.widget": "Gizmo Widget" }"#;
    let fr = br#"{ "item.my_mod.widget": "Bidule Gizmo" }"#;
    let fuels = br##"{
        "values": {
            "my_mod:star_dust": { "burn_time": 2400 },
            "#my_mod:reactor_rods": { "burn_time": 4800 }
        }
    }"##;
    let rods_tag = br#"{
        "replace": false,
        "values": ["my_mod:rod_a", "my_mod:rod_b"]
    }"#;
    let sprite = tiny_png(16, 16, [30, 200, 120, 255]);
    std::fs::write(
        &jar,
        deflate_zip(&[
            ("assets/my_mod/lang/en_us.json", en.as_slice()),
            ("assets/my_mod/lang/fr_fr.json", fr.as_slice()),
            ("assets/my_mod/textures/item/widget.png", sprite.as_slice()),
            (
                "data/my_mod/data_maps/item/furnace_fuels.json",
                fuels.as_slice(),
            ),
            (
                "data/my_mod/tags/item/reactor_rods.json",
                rods_tag.as_slice(),
            ),
        ]),
    )
    .unwrap();

    // --- Lang: English + arbitrary active locale, zero hardcoding. ---
    let mut browser = RecipeBrowserState::default();
    let report = megu_client_gpu::recipe_ingest::ingest_lang_into_index(
        std::slice::from_ref(&jar),
        &["fr_fr".to_owned()],
        &mut browser.index,
    );
    assert_eq!(report.sources_scanned, 1);
    assert_eq!(report.json_failed, 0, "notes: {:?}", report.notes);
    let entry = browser
        .index
        .item("my_mod:widget")
        .expect("lang-only item is catalogued");
    assert_eq!(entry.display_name, "Gizmo Widget");
    assert_eq!(
        entry.localized_names.get("fr_fr").map(String::as_str),
        Some("Bidule Gizmo")
    );
    browser.active_locale = "fr_fr".to_owned();
    assert_eq!(
        browser
            .index
            .display_name_for_locale("my_mod:widget", "fr_fr"),
        "Bidule Gizmo"
    );
    assert_eq!(
        browser
            .index
            .display_name_for_locale("my_mod:widget", "en_us"),
        "Gizmo Widget"
    );
    // Search hits BOTH the English name and the active-locale name.
    browser.search = "gizmo".into();
    assert_eq!(browser.visible_items().len(), 1);
    browser.search = "bidule".into();
    let ids: Vec<&str> = browser
        .visible_items()
        .iter()
        .map(|&i| browser.index.items[i].id.as_str())
        .collect();
    assert_eq!(ids, ["my_mod:widget"]);

    // --- NeoForge furnace_fuels data map: item + symbolic #tag rows. ---
    let report2 = ingest_mod_jars_into_ui(std::slice::from_ref(&jar), &mut browser);
    assert!(report2.fuels >= 2, "fuels: {}", report2.fuels);
    assert!(browser
        .index
        .fuels
        .iter()
        .any(|f| f.item == "my_mod:star_dust" && f.burn_ticks == 2400));
    assert!(browser
        .index
        .fuels
        .iter()
        .any(|f| f.item == "#my_mod:reactor_rods" && f.burn_ticks == 4800));

    // --- Item-icon atlas from the same JAR's decoded PNG sprites. ---
    let (layer, read) =
        megu_client_gpu::vanilla_jar::zip_pack_layer(&jar).expect("pack layer reads");
    assert_eq!(read.entries_seen, 5);
    assert!(!layer.sprites.is_empty(), "sprite must decode");
    let atlas = megu_client_gpu::recipe_ingest::build_item_icon_atlas(&[layer]);
    assert!(atlas.uv("my_mod:widget").is_some(), "widget icon packed");
    assert!(!atlas.rgba.is_empty() && atlas.width >= 16);

    // Real icon quads get the atlas uv; unknown ids keep the deterministic
    // swatch + monogram fallback (never blank).
    use megu_ui::draw::{tessellate_draw_list_icons, UiDrawList, UiRect};
    let mut list = UiDrawList::new();
    list.item_icon(UiRect::new(0, 0, 18, 18), "my_mod:widget");
    let mesh = tessellate_draw_list_icons(64, 64, &list, &atlas);
    assert_eq!(mesh.vertices.len(), 4);
    let uv = atlas.uv("my_mod:widget").unwrap();
    // The atlas is resolved (icon uvs carry the negative-u icon-texture
    // marker — the same rect the atlas stored, shifted out of font space).
    assert!(
        (mesh.vertices[0].uv[0] - (uv[0] - 2.0)).abs() < 0.01,
        "real uv used: {:?} vs {:?}",
        mesh.vertices[0].uv,
        uv
    );

    let mut list2 = UiDrawList::new();
    list2.item_icon(UiRect::new(0, 0, 18, 18), "missing:thing");
    let mesh2 = tessellate_draw_list_icons(64, 64, &list2, &atlas);
    assert!(mesh2.vertices.len() >= 4, "fallback still tessellates");
    assert!(
        mesh2.vertices.iter().all(|v| v.uv[0] >= 0.0),
        "fallback must not sample the icon atlas"
    );
    assert_eq!(tmp.file_count(), 1, "zero extraction: {:?}", tmp.0);
}
