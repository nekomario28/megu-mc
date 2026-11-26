// SPDX-License-Identifier: LGPL-3.0-or-later
//! Lane GPU-RECIPE-TUTORIAL-WIRE: feeds real datapack content into the recipe
//! browser's [`RecipeIndex`].
//!
//! Sources (all read **without extracting a single byte to disk**):
//!
//! * `<gameDir>/mods/*.jar` — mod JARs in deterministic filename order,
//!   including one level of `META-INF/jarjar`/`META-INF/jars` nesting via
//!   [`megu_core::zip_archive::LazyZipIndex`].
//! * `<gameDir>/versions/<id>/<id>.jar` — the vanilla client JAR for the
//!   launcher `--version` id (falls back to `1.21.1`), which ships every
//!   vanilla `data/minecraft/...` document.
//! * `<gameDir>/saves/<world>/datapacks/<pack>` — datapack directories and
//!   `.zip` packs carrying a `data/` root.
//! * `MEGU_RECIPE_INGEST_JARS` — optional extra JAR list (host-path
//!   separated), mirroring the `MEGU_RESOURCE_PACK_DIR` env-override
//!   pattern; empty entries are skipped.
//!
//! Inside each source the recognizer scans:
//!
//! * `data/<ns>/recipe(s)/**/*.json` — vanilla + Create + **any** unknown
//!   `<ns>:<type>` recipe JSON: the type's namespace is auto-registered
//!   through [`RecipeIndex::register_mod_namespace`] so unknown types land
//!   in a declared [`megu_ui::recipe_browser::RecipeCategory::CustomModCategory`]
//!   instead of silently failing closed.
//! * `data/<ns>/tags/item(s)/**/*.json` — item tag membership, including
//!   nested `#ns:tag` references resolved with a bounded fixpoint.
//! * `data/<ns>/loot_table(s)/entities/**/*.json` — world-drop mob drops.
//! * `data/<ns>/loot_table(s)/chests/**/*.json` — world-drop dungeon loot.
//!
//! Everything is bounded (caps below) and fail-closed per entry: one
//! malformed JSON is recorded in [`IngestReport::notes`] and never aborts
//! the remaining entries — the [`JarReadReport`](crate::vanilla_jar::JarReadReport)
//! convention of reporting skips instead of faking a clean negative.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};
use megu_core::zip_archive::LazyZipIndex;
use megu_proto::table::ProtocolTable;
use megu_ui::draw::UiColor;
use megu_ui::recipe_browser::{
    item_color, AnimatedPart, DungeonLootEntry, FuelEntry, ItemEntry, MobDropEntry, MachineTutorialAnimation,
    MachineTutorialAxis, MachineTutorialScene, MachineTutorialStep, RecipeBrowserState, RecipeIndex, RecipeIngredient,
};
use megu_ui::state::UiLang;

use crate::live_session::{jar_version_ids, SessionPaths};

// ---------------------------------------------------------------------------
// Bounded caps — a hostile or enormous pack degrades to a note, never a hang.
// ---------------------------------------------------------------------------

/// Mod JARs scanned per boot.
pub const MAX_JARS: usize = 512;
/// Datapack roots scanned per boot.
pub const MAX_DATAPACKS: usize = 64;
/// Single JSON document cap (recipe/tag/loot files are tiny).
pub const MAX_JSON_BYTES: usize = 2 * 1024 * 1024;
/// JSON parser recursion + node budget per document.
const MAX_JSON_DEPTH: usize = 32;
const MAX_JSON_NODES: usize = 65_536;
/// Files walked inside one datapack directory.
const MAX_DATAPACK_FILES: usize = 8_192;
/// Index-wide totals.
pub const MAX_RECIPES: usize = 262_144;
pub const MAX_ITEMS: usize = 131_072;
pub const MAX_TAGS: usize = 65_536;
pub const MAX_LOOT_ROWS: usize = 65_536;
/// Distinct namespaces auto-registered.
pub const MAX_NAMESPACES: usize = 4_096;
/// Nested `#tag` reference expansion depth.
const MAX_TAG_DEPTH: usize = 8;
/// Concrete `(item, tag)` assignments applied per ingest.
pub const MAX_TAG_MEMBERS: usize = 262_144;
/// Raw member strings kept per tag file.
const MAX_TAG_VALUES: usize = 16_384;
/// Per-source failure notes retained (the tail folds into a counter).
const MAX_NOTES: usize = 128;

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// What an ingest pass did — every skipped or failed source is counted so a
/// dropped JAR never reads as a clean negative result.
#[derive(Clone, Debug, Default)]
pub struct IngestReport {
    /// Mod/client/datapack ZIP archives opened.
    pub sources_scanned: usize,
    /// Archives/dirs that failed to open or index.
    pub sources_failed: usize,
    /// `data/**/*.json` candidates considered.
    pub entries_seen: usize,
    pub recipes_loaded: usize,
    /// Recipe JSONs that failed closed (`load_recipe_json` → `None`).
    pub recipes_failed: usize,
    /// Item-tag documents parsed.
    pub tags_loaded: usize,
    /// Concrete `(item, tag)` assignments applied.
    pub tag_members: usize,
    /// Namespaces auto-registered for custom recipe categories.
    pub namespaces_registered: usize,
    pub mob_drops: usize,
    pub dungeon_loot: usize,
    /// New [`ItemEntry`] catalog rows created.
    pub items_registered: usize,
    /// `furnace_fuels` data-map rows loaded (world-resource Fuels tab).
    pub fuels: usize,
    /// Total `data/` JSONs that could not be parsed at all.
    pub json_failed: usize,
    /// Bounded per-entry failure record (`"<source>: <entry>: <reason>"`).
    pub notes: Vec<String>,
}

impl IngestReport {
    /// One-line summary for the boot status channel.
    pub fn summary_line(&self) -> String {
        format!(
            "Recipe ingest: {} recipes, {} items, {} tags, {} mob drops, {} chest loot \
             from {} sources ({} failed entries)",
            self.recipes_loaded,
            self.items_registered,
            self.tags_loaded,
            self.mob_drops,
            self.dungeon_loot,
            self.sources_scanned,
            self.recipes_failed + self.json_failed + self.sources_failed,
        )
    }

    /// `true` when no archive or datapack was found — no status noise.
    pub fn any_source(&self) -> bool {
        self.sources_scanned > 0 || self.sources_failed > 0
    }

    fn note(&mut self, source: &str, entry: &str, reason: &str) {
        if self.notes.len() < MAX_NOTES {
            self.notes.push(format!("{source}: {entry}: {reason}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Source discovery
// ---------------------------------------------------------------------------

/// Every mod + vanilla + datapack archive the launcher paths imply, in
/// deterministic scan order (mod JARs by filename, vanilla version JAR next,
/// datapacks last).
pub fn collect_jar_paths(paths: &SessionPaths) -> Vec<PathBuf> {
    let mut jars = Vec::new();
    if let Some(game_dir) = paths.game_dir.as_deref() {
        if let Ok(read) = fs::read_dir(game_dir.join("mods")) {
            let mut mods: Vec<PathBuf> = read
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
                })
                .collect();
            mods.sort_by(|a, b| {
                a.file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .cmp(&b.file_name().map(|n| n.to_string_lossy().to_lowercase()))
            });
            mods.truncate(MAX_JARS);
            jars.extend(mods);
        }
        for version_id in jar_version_ids(paths) {
            let jar = game_dir
                .join("versions")
                .join(&version_id)
                .join(format!("{version_id}.jar"));
            if jar.is_file() {
                jars.push(jar);
                break;
            }
        }
    }
    if let Ok(raw) = std::env::var("MEGU_RECIPE_INGEST_JARS") {
        jars.extend(
            std::env::split_paths(&raw)
                .filter(|p| p.is_file())
                .take(MAX_JARS.saturating_sub(jars.len())),
        );
    }
    jars
}

/// Datapack roots: `saves/<world>/datapacks/<pack>` dirs and `.zip` files.
fn collect_datapack_roots(paths: &SessionPaths) -> Vec<PathBuf> {
    let Some(game_dir) = paths.game_dir.as_deref() else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    let Ok(worlds) = fs::read_dir(game_dir.join("saves")) else {
        return roots;
    };
    for world in worlds.flatten().take(MAX_DATAPACKS) {
        let Ok(packs) = fs::read_dir(world.path().join("datapacks")) else {
            continue;
        };
        for pack in packs.flatten() {
            let path = pack.path();
            if path.is_dir()
                || (path.is_file()
                    && path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip")))
            {
                roots.push(path);
                if roots.len() >= MAX_DATAPACKS {
                    return roots;
                }
            }
        }
    }
    roots
}

// ---------------------------------------------------------------------------
// Entry classification
// ---------------------------------------------------------------------------

enum DataKind<'a> {
    /// `data/<ns>/recipe(s)/<rel>.json` → recipe id `<ns>:<rel>`.
    Recipe { ns: &'a str, rel: &'a str },
    /// `data/<ns>/tags/item(s)/<rel>.json` → tag id `<ns>:<rel>`.
    ItemTag { ns: &'a str, rel: &'a str },
    /// `.../loot_table(s)/entities/<rel>.json` → mob id `<ns>:<rel>`.
    EntityLoot { ns: &'a str, rel: &'a str },
    /// `.../loot_table(s)/chests/<rel>.json` → chest id `<ns>:chests/<rel>`.
    ChestLoot { ns: &'a str, rel: &'a str },
    /// `data/<ns>/data_maps/item/furnace_fuels.json` — NeoForge fuel map.
    FuelDataMap,
}

fn classify(entry: &str) -> Option<DataKind<'_>> {
    let rest = entry.strip_prefix("data/")?;
    let (ns, path) = rest.split_once('/')?;
    if ns.is_empty() {
        return None;
    }
    for prefix in ["recipes/", "recipe/"] {
        if let Some(rel) = path.strip_prefix(prefix) {
            return rel
                .strip_suffix(".json")
                .filter(|rel| !rel.is_empty())
                .map(|rel| DataKind::Recipe { ns, rel });
        }
    }
    for prefix in ["tags/item/", "tags/items/"] {
        if let Some(rel) = path.strip_prefix(prefix) {
            return rel
                .strip_suffix(".json")
                .filter(|rel| !rel.is_empty())
                .map(|rel| DataKind::ItemTag { ns, rel });
        }
    }
    if path == "data_maps/item/furnace_fuels.json" {
        return Some(DataKind::FuelDataMap);
    }
    for root in ["loot_table/", "loot_tables/"] {
        if let Some(rest) = path.strip_prefix(root) {
            for (sub, is_entity) in [("entities/", true), ("chests/", false)] {
                if let Some(rel) = rest.strip_prefix(sub) {
                    return rel.strip_suffix(".json").filter(|r| !r.is_empty()).map(|rel| {
                        if is_entity {
                            DataKind::EntityLoot { ns, rel }
                        } else {
                            DataKind::ChestLoot { ns, rel }
                        }
                    });
                }
            }
            return None;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Boot-time ingestion: scans every source [`SessionPaths`] implies and
/// folds recipes, item tags and world-drop loot into `browser.index`.
///
/// Zero disk extraction: archives are indexed by [`LazyZipIndex`]'s EOCD +
/// Central Directory pass and entries inflate in memory only.
pub fn ingest_mod_recipes_into_ui(
    paths: &SessionPaths,
    browser: &mut RecipeBrowserState,
) -> IngestReport {
    let mut work = IngestWork::new(&mut browser.index);
    for jar in collect_jar_paths(paths) {
        work.ingest_jar(&jar);
    }
    for root in collect_datapack_roots(paths) {
        if root.is_dir() {
            work.ingest_dir(&root);
        } else {
            work.ingest_jar(&root);
        }
    }
    work.finish()
}

/// Direct JAR-list ingestion (tests and manual probes) — same code path as
/// the session-boot scan but with an explicit source list.
pub fn ingest_mod_jars_into_ui(jars: &[PathBuf], browser: &mut RecipeBrowserState) -> IngestReport {
    let mut work = IngestWork::new(&mut browser.index);
    for jar in jars {
        work.ingest_jar(jar);
    }
    work.finish()
}

// ---------------------------------------------------------------------------
// Lang files — English (`en_us`) + the active display locale
// ---------------------------------------------------------------------------

/// Bounded caps for the lang pass (separate from the `data/` pass).
const MAX_LANG_FILES: usize = 4_096;
const MAX_LANG_ENTRIES: usize = 524_288;

/// Read every `assets/<ns>/lang/{en_us,<locale>}.json` in `jar` (mod or
/// vanilla JAR — [`crate::vanilla_jar::read_selected_entries`] walks one
/// level of Jar-in-Jar too, so bundled mods contribute their lang files)
/// and merge the `item.*`/`block.*` names into `index` under `locale`.
///
/// `en_us` always loads; `locale` loads in addition when it differs, so
/// search matches English + the active display language for **any** mod
/// and **any** locale — zero hardcoding.
pub fn ingest_lang_into_index(
    jar_paths: &[PathBuf],
    locales: &[String],
    index: &mut RecipeIndex,
) -> IngestReport {
    let mut report = IngestReport::default();
    let want: BTreeSet<String> = std::iter::once("en_us".to_owned())
        .chain(locales.iter().map(|l| l.to_ascii_lowercase()))
        .collect();
    for jar in jar_paths {
        let source = jar
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| jar.display().to_string());
        let (entries, read) = match crate::vanilla_jar::read_selected_entries(jar, |name| {
            if !name.starts_with("assets/") || !name.ends_with(".json") {
                return false;
            }
            let Some((ns, cat, sub)) = lang_subpath(name) else {
                return false;
            };
            let _ = ns;
            cat == "lang" && want.contains(&sub)
        }) {
            Ok(ok) => ok,
            Err(err) => {
                report.sources_failed += 1;
                report.note(&source, "<index>", &err.to_string());
                continue;
            }
        };
        report.sources_scanned += 1;
        report.entries_seen += read.entries_seen;
        for (name, reason) in &read.skipped {
            report.note(&source, name, reason);
        }
        for entry in entries.into_iter().take(MAX_LANG_FILES) {
            let Some((_, _, locale)) = lang_subpath(&entry.name) else {
                continue;
            };
            let Ok(text) = String::from_utf8(entry.bytes) else {
                report.json_failed += 1;
                continue;
            };
            let Some(root) = parse_json(&text) else {
                report.json_failed += 1;
                continue;
            };
            let Some(obj) = root.as_object() else {
                continue;
            };
            let mut map = BTreeMap::new();
            for (key, value) in obj.iter().take(MAX_LANG_ENTRIES) {
                if let Json::Str(name) = value {
                    map.insert(key.clone(), name.clone());
                }
            }
            index.apply_lang_map(&locale, &map);
            report.tags_loaded += map.len();
        }
    }
    report
}

/// `assets/<ns>/lang/<locale>.json` → `(ns, "lang", locale)` — the
/// locale is the filename stem, lowercased to match
/// [`RecipeBrowserState::active_locale`](megu_ui::recipe_browser::RecipeBrowserState::active_locale).
fn lang_subpath(name: &str) -> Option<(String, String, String)> {
    let rest = name.strip_prefix("assets/")?;
    let (ns, tail) = rest.split_once('/')?;
    let (category, file) = tail.split_once('/')?;
    let stem = file.strip_suffix(".json")?;
    if ns.is_empty() || category.is_empty() || stem.is_empty() {
        return None;
    }
    Some((
        ns.to_owned(),
        category.to_owned(),
        stem.to_ascii_lowercase(),
    ))
}

/// Convenience wrapper: all JAR sources [`SessionPaths`] implies.
pub fn ingest_lang_into_ui(paths: &SessionPaths, browser: &mut RecipeBrowserState) -> IngestReport {
    let locale = browser.active_locale.clone();
    ingest_lang_into_index(&collect_jar_paths(paths), &[locale], &mut browser.index)
}

// ---------------------------------------------------------------------------
// UI item-icon atlas — pack decoded `assets/*/textures/{item,block}` sprites
// ---------------------------------------------------------------------------

/// Build the UI item-icon atlas from the *already decoded* pack layers the
/// block atlas pipeline assembled (zero extra JAR reads). Highest-priority
/// layer wins per id, matching resource-pack semantics; `item/` sprites
/// outrank `block/` for the same id; `ns:item/foo` → icon id `ns:foo`.
pub fn build_item_icon_atlas(
    layers: &[megu_client_mesh::asset_pipeline::ResourcePackLayer],
) -> megu_ui::draw::ItemIconAtlas {
    let mut builder = megu_ui::draw::ItemIconAtlasBuilder::new();
    for layer in layers.iter().rev() {
        for (key, (w, h, rgba)) in &layer.sprites {
            let Some((ns, stem)) = key.split_once(':') else {
                continue;
            };
            for prefix in ["item/", "block/"] {
                if let Some(path) = stem.strip_prefix(prefix) {
                    builder.add(&format!("{ns}:{path}"), *w, *h, rgba);
                }
            }
        }
    }
    builder.finish()
}

/// Humanized catalog title (`"some_mod:my_item/thing"` → `"Thing"` → title case).
pub fn humanize_id(id: &str) -> String {
    let path = id.rsplit([':', '/']).next().unwrap_or(id);
    let mut out = String::with_capacity(path.len());
    let mut cap = true;
    for ch in path.chars() {
        if ch == '_' || ch == '-' {
            out.push(' ');
            cap = true;
        } else if cap {
            out.extend(ch.to_uppercase());
            cap = false;
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() {
        id.to_owned()
    } else {
        out
    }
}

/// Numeric `minecraft:item` registry id → namespaced id via the built-in
/// protocol-767 table (cached — the table is rebuilt only once per process).
/// Unregistered ids (modded servers) degrade to `unknown:item_<id>` so the
/// recipe browser still has a deterministic focus target.
pub fn item_name_for_id(item_id: u32) -> String {
    static TABLE: OnceLock<ProtocolTable> = OnceLock::new();
    let table = TABLE.get_or_init(ProtocolTable::v767_builtin);
    table
        .registry_name("minecraft:item", item_id)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("unknown:item_{item_id}"))
}

/// Deterministic machine-tutorial scene for `item`: a small platform, the item as a
/// colored center voxel, and — when the index knows a producing recipe —
/// the recipe's inputs arranged in a ring with a narrated assembly step.
pub fn machine_tutorial_scene_for(index: &RecipeIndex, item: &str) -> MachineTutorialScene {
    let name = index.display_name(item);
    let mut scene = MachineTutorialScene::new(&name);
    for x in -2..=2 {
        for z in -2..=2 {
            scene.add_block(x, 0, z, UiColor::rgb(88, 88, 98));
        }
    }
    scene.add_block(0, 1, 0, item_color(item));

    let mut intro = MachineTutorialStep::new(
        &format!("{name} — how it is made"),
        &format!("{name} の作り方"),
        60,
    );
    intro.highlight = Some((0, 1, 0));
    scene.steps.push(intro);

    if let Some(recipe) = index.chosen_recipe_for(item, &BTreeMap::new()) {
        const RING: [(i32, i32); 8] = [
            (-1, -1),
            (0, -1),
            (1, -1),
            (1, 0),
            (1, 1),
            (0, 1),
            (-1, 1),
            (-1, 0),
        ];
        for (ingredient, slot) in recipe.inputs.iter().zip(RING) {
            let resolved = index.resolve_ingredient(ingredient);
            scene.add_block(slot.0, 1, slot.1, item_color(&resolved));
        }
        let cat = recipe.category.title(UiLang::En);
        let cat_ja = recipe.category.title(UiLang::Ja);
        let mut step = MachineTutorialStep::new(
            &format!("{name} ← {cat}"),
            &format!("{name} ← {cat_ja}"),
            90,
        );
        step.highlight = Some((0, 1, 0));
        step.animated_parts.push(AnimatedPart {
            pos: (0, 1, 0),
            anim: MachineTutorialAnimation::Spin {
                axis: MachineTutorialAxis::Y,
                rpm: 6.0,
            },
        });
        scene.steps.push(step);
    }
    scene
}

// ---------------------------------------------------------------------------
// Ingest worker — shared by the jar/datapack paths
// ---------------------------------------------------------------------------

struct IngestWork<'a> {
    index: &'a mut RecipeIndex,
    report: IngestReport,
    /// Catalog ids already present (seeded once) — avoids `index.item`'s
    /// O(n) scan becoming O(n²) over a large ingest.
    known_items: BTreeSet<String>,
    /// `data/<ns>/...` tag id → raw members (`item` or `#other/tag`).
    tag_defs: BTreeMap<String, Vec<String>>,
    namespaces_seen: BTreeSet<String>,
    recipe_ids_seen: BTreeSet<String>,
    /// Loot doc ids already walked — a table duplicated between a parent
    /// JAR and its nested jar (or re-encountered via `loot_table` +
    /// `loot_tables`) must not emit doubled world-drop rows.
    loot_docs_seen: BTreeSet<String>,
}

impl<'a> IngestWork<'a> {
    fn new(index: &'a mut RecipeIndex) -> Self {
        Self {
            known_items: index.items.iter().map(|e| e.id.clone()).collect(),
            index,
            report: IngestReport::default(),
            tag_defs: BTreeMap::new(),
            namespaces_seen: BTreeSet::new(),
            recipe_ids_seen: BTreeSet::new(),
            loot_docs_seen: BTreeSet::new(),
        }
    }

    /// After all sources: resolve nested `#tag` refs and stamp memberships,
    /// then seed the tag-driven vanilla fuel fallback when no data map
    /// declared fuels (vanilla 1.21.1 ships no `furnace_fuels` map — its
    /// burn table lives in code; these tag memberships are the data-driven
    /// part of it, so the fallback emits `#tag` rows, never item lists).
    fn finish(mut self) -> IngestReport {
        self.apply_tags();
        if self.index.fuels.is_empty() {
            // `(tag, burn_ticks)` — the NeoForge vanilla data map's own
            // tag-keyed share (200 ticks = 1 item smelted).
            const FALLBACK_TAGS: &[(&str, u32)] = &[
                ("minecraft:logs", 300),
                ("minecraft:planks", 300),
                ("minecraft:wooden_slabs", 150),
                ("minecraft:coals", 1600),
                ("minecraft:wool", 100),
                ("minecraft:boats", 1200),
            ];
            for (tag, burn) in FALLBACK_TAGS {
                if !self
                    .index
                    .items
                    .iter()
                    .any(|e| e.tags.iter().any(|t| t == tag))
                {
                    continue; // absent tag — no fuel row to claim.
                }
                self.index.fuels.push(FuelEntry {
                    item: format!("#{tag}"),
                    burn_ticks: *burn,
                });
            }
        }
        self.report
    }

    fn ensure_item(&mut self, id: &str) {
        if id.is_empty() || self.known_items.contains(id) {
            return;
        }
        if self.index.items.len() >= MAX_ITEMS {
            return;
        }
        self.known_items.insert(id.to_owned());
        self.index.add_item(ItemEntry::new(id, &humanize_id(id)));
        self.report.items_registered += 1;
    }

    fn register_type_namespace(&mut self, ty: &str) {
        let Some(ns) = ty.split(':').next().filter(|ns| !ns.is_empty()) else {
            return;
        };
        if !self.namespaces_seen.insert(ns.to_owned()) {
            return;
        }
        if self.namespaces_seen.len() > MAX_NAMESPACES {
            return;
        }
        let title = humanize_id(ns);
        self.index.register_mod_namespace(ns, &title, &title);
        self.report.namespaces_registered += 1;
    }

    fn ingest_jar(&mut self, jar: &Path) {
        let source = jar
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| jar.display().to_string());
        let zip = match LazyZipIndex::open_filtered(jar, |name| {
            name.starts_with("data/") && name.ends_with(".json")
        }) {
            Ok(zip) => zip,
            Err(err) => {
                self.report.sources_failed += 1;
                self.report.note(&source, "<index>", &err.to_string());
                return;
            }
        };
        self.report.sources_scanned += 1;
        let mut names: Vec<String> = zip.entries().iter().map(|e| e.name.clone()).collect();
        names.sort_unstable();
        for name in &names {
            self.report.entries_seen += 1;
            let kind = classify(name);
            if kind.is_none() {
                continue;
            }
            let bytes = match zip.read_entry(name, MAX_JSON_BYTES) {
                Ok(bytes) => bytes,
                Err(err) => {
                    self.report.json_failed += 1;
                    self.report.note(&source, name, &err.to_string());
                    continue;
                }
            };
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    self.report.json_failed += 1;
                    self.report.note(&source, name, "not utf-8");
                    continue;
                }
            };
            self.ingest_document(&source, &text, kind.unwrap());
        }
    }

    fn ingest_dir(&mut self, root: &Path) {
        let source = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        let data_root = root.join("data");
        let mut files = Vec::new();
        walk_dir(&data_root, &data_root, &mut files);
        if files.is_empty() {
            return;
        }
        self.report.sources_scanned += 1;
        files.sort_unstable();
        for rel in files.iter().take(MAX_DATAPACK_FILES) {
            let entry = format!("data/{rel}");
            self.report.entries_seen += 1;
            let Some(kind) = classify(&entry) else {
                continue;
            };
            let meta = fs::metadata(data_root.join(rel));
            if meta.is_ok_and(|m| m.len() > MAX_JSON_BYTES as u64) {
                self.report.json_failed += 1;
                self.report.note(&source, &entry, "json exceeds byte cap");
                continue;
            }
            match fs::read_to_string(data_root.join(rel)) {
                Ok(text) => self.ingest_document(&source, &text, kind),
                Err(err) => {
                    self.report.json_failed += 1;
                    self.report.note(&source, &entry, &err.to_string());
                }
            }
        }
    }

    /// One `data/**/*.json` document, classified — failure of a single
    /// document is a note, never an abort.
    fn ingest_document(&mut self, source: &str, text: &str, kind: DataKind<'_>) {
        match kind {
            DataKind::Recipe { ns, rel } => {
                // Auto-register the recipe *type*'s namespace (not the
                // file's) so `my_factory` files can declare `create:mixing`
                // AND unknown `other_mod:type` recipes still categorize.
                match parse_json(text).and_then(|j| {
                    j.as_object()
                        .and_then(|o| o.get("type"))
                        .and_then(Json::as_str)
                        .map(str::to_owned)
                }) {
                    Some(ty) => self.register_type_namespace(&ty),
                    None => {
                        self.report.recipes_failed += 1;
                        self.report
                            .note(source, &format!("{ns}:{rel}"), "missing recipe type");
                        return;
                    }
                }
                let recipe_id = format!("{ns}:{rel}");
                if !self.recipe_ids_seen.insert(recipe_id.clone()) {
                    return; // later mod overwrote nothing — first wins.
                }
                if self.index.recipes.len() >= MAX_RECIPES {
                    return;
                }
                let normalized = normalize_recipe_document(text);
                match self.index.load_recipe_json(&recipe_id, &normalized) {
                    Some(recipe) => {
                        self.report.recipes_loaded += 1;
                        self.register_recipe_items(&recipe);
                    }
                    None => {
                        self.report.recipes_failed += 1;
                        self.report.note(source, &recipe_id, "recipe parse failed closed");
                    }
                }
            }
            DataKind::ItemTag { ns, rel } => self.ingest_tag(source, ns, rel, text),
            DataKind::EntityLoot { ns, rel } => {
                let mob = format!("{ns}:{rel}");
                if self.loot_docs_seen.insert(mob.clone()) {
                    self.ingest_loot(source, &mob, None, text);
                }
            }
            DataKind::ChestLoot { ns, rel } => {
                let chest = format!("{ns}:chests/{rel}");
                if self.loot_docs_seen.insert(chest.clone()) {
                    self.ingest_loot(source, "", Some(&chest), text);
                }
            }
            DataKind::FuelDataMap => self.ingest_fuel_map(source, text),
        }
    }

    /// NeoForge `furnace_fuels` item data map:
    /// `{"values":{"<id|#tag>":{"burn_time":N}}}` → [`FuelEntry`] rows.
    /// Entries keyed by `#tag` stay symbolic — the Fuels tab resolves
    /// them through the item-tag index.
    fn ingest_fuel_map(&mut self, source: &str, text: &str) {
        let Some(root) = parse_json(text) else {
            self.report.json_failed += 1;
            self.report
                .note(source, "furnace_fuels", "data map malformed");
            return;
        };
        let Some(values) = root
            .as_object()
            .and_then(|o| o.get("values"))
            .and_then(Json::as_object)
        else {
            return; // an empty/disabled map is legal — nothing to add.
        };
        for (id, payload) in values.iter().take(MAX_TAG_VALUES) {
            let Some(burn) = payload
                .as_object()
                .and_then(|o| o.get("burn_time"))
                .and_then(Json::as_f64)
            else {
                continue;
            };
            if !(1.0..=1_000_000.0).contains(&burn) {
                continue;
            }
            if !id.is_empty() && self.index.fuels.len() < MAX_LOOT_ROWS {
                self.index.fuels.push(FuelEntry {
                    item: id.clone(),
                    burn_ticks: burn.round() as u32,
                });
                self.report.fuels += 1;
            }
            if let Some(item) = id.strip_prefix('#') {
                let _ = item; // tag fuel stays symbolic; members resolve at view time.
            } else {
                self.ensure_item(id);
            }
        }
    }

    /// Every concrete item a recipe touches becomes a catalog row so the
    /// browser can search/focus it (tag slots resolve lazily at query time).
    fn register_recipe_items(&mut self, recipe: &megu_ui::recipe_browser::Recipe) {
        let mut ids: Vec<&str> = Vec::new();
        for ing in &recipe.inputs {
            if let RecipeIngredient::Item { id, .. } = ing {
                ids.push(id);
            }
        }
        for step in &recipe.sequence_steps {
            for ing in &step.inputs {
                if let RecipeIngredient::Item { id, .. } = ing {
                    ids.push(id);
                }
            }
        }
        for out in &recipe.outputs {
            ids.push(&out.item);
        }
        for (id, ..) in &recipe.chanced_outputs {
            ids.push(id);
        }
        if !recipe.transitional_item.is_empty() {
            ids.push(&recipe.transitional_item);
        }
        for id in ids {
            self.ensure_item(id);
        }
    }

    /// `{"replace":…,"values":[…]}` — merges members into `tag_defs`;
    /// nested `#tag` references resolve in `finish`.
    fn ingest_tag(&mut self, source: &str, ns: &str, rel: &str, text: &str) {
        if self.tag_defs.len() >= MAX_TAGS {
            return;
        }
        let Some(root) = parse_json(text) else {
            self.report.json_failed += 1;
            self.report
                .note(source, &format!("{ns}:{rel}"), "tag json malformed");
            return;
        };
        let Some(obj) = root.as_object() else {
            self.report.json_failed += 1;
            self.report
                .note(source, &format!("{ns}:{rel}"), "tag root not an object");
            return;
        };
        let tag = format!("{ns}:{rel}");
        if obj
            .get("replace")
            .and_then(Json::as_bool)
            .unwrap_or(false)
        {
            self.tag_defs.insert(tag.clone(), Vec::new());
        }
        let values = self.tag_defs.entry(tag).or_default();
        if let Some(arr) = obj.get("values").and_then(Json::as_array) {
            for value in arr {
                // `"ns:item"`, `"#ns:tag"`, or `{"id":"ns:item","required":…}`.
                let member = match value {
                    Json::Str(s) => Some(s.clone()),
                    Json::Obj(o) => o.get("id").and_then(Json::as_str).map(str::to_owned),
                    _ => None,
                };
                if let Some(member) = member {
                    if !member.is_empty() && values.len() < MAX_TAG_VALUES {
                        values.push(member);
                    }
                }
            }
        }
        self.report.tags_loaded += 1;
    }

    /// Resolve all collected tag definitions and stamp `ItemEntry.tags`.
    fn apply_tags(&mut self) {
        let defs = std::mem::take(&mut self.tag_defs);
        let mut resolved: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut visiting: BTreeSet<String> = BTreeSet::new();
        let tags: Vec<String> = defs.keys().cloned().collect();
        for tag in &tags {
            let mut items = Vec::new();
            expand_tag(tag, &defs, &mut resolved, &mut visiting, 0, &mut items);
        }
        for (tag, items) in resolved {
            for id in items {
                if self.report.tag_members >= MAX_TAG_MEMBERS {
                    return;
                }
                self.ensure_item(&id);
                if let Some(entry) = self.index.items.iter_mut().find(|e| e.id == id) {
                    if !entry.tags.contains(&tag) {
                        entry.tags.push(tag.clone());
                        self.report.tag_members += 1;
                    }
                }
            }
        }
    }

    /// One entity/chest loot table → [`MobDropEntry`] / [`DungeonLootEntry`]
    /// rows. `pools[].entries` walk flattens `alternatives`/`group`/`sequence`
    /// children and weights chance by sibling share × average rolls.
    fn ingest_loot(&mut self, source: &str, mob: &str, chest: Option<&str>, text: &str) {
        let Some(root) = parse_json(text) else {
            self.report.json_failed += 1;
            self.report
                .note(source, mob, "loot table json malformed");
            return;
        };
        let Some(obj) = root.as_object() else {
            self.report.json_failed += 1;
            self.report.note(source, mob, "loot root not an object");
            return;
        };
        let Some(pools) = obj.get("pools").and_then(Json::as_array) else {
            return; // tables without pools are legal — nothing to drop.
        };
        for pool in pools.iter().take(256) {
            let Some(pobj) = pool.as_object() else {
                continue;
            };
            let rolls = json_count_range(pobj.get("rolls")).1;
            let pool_conditions = conditions_of(pobj.get("conditions"));
            let Some(entries) = pobj.get("entries").and_then(Json::as_array) else {
                continue;
            };
            let total: f64 = entries.iter().map(entry_weight).sum::<f64>().max(1.0);
            for entry in entries.iter().take(512) {
                let share = entry_weight(entry) / total * rolls;
                let conditions = merge_conditions(&pool_conditions, &conditions_of(
                    entry.as_object().and_then(|o| o.get("conditions")),
                ));
                self.walk_loot_entry(
                    source,
                    mob,
                    chest,
                    entry,
                    share,
                    &conditions,
                    0,
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_loot_entry(
        &mut self,
        source: &str,
        mob: &str,
        chest: Option<&str>,
        entry: &Json,
        share: f64,
        conditions: &LootConditions,
        depth: usize,
    ) {
        if depth > 8 || self.mob_drops_plus_loot() >= MAX_LOOT_ROWS {
            return;
        }
        let Some(obj) = entry.as_object() else {
            return;
        };
        let ty = obj
            .get("type")
            .and_then(Json::as_str)
            .unwrap_or("")
            .rsplit(':')
            .next()
            .unwrap_or("")
            .to_owned();
        match ty.as_str() {
            "item" => {
                let Some(name) = obj.get("name").and_then(Json::as_str) else {
                    return;
                };
                let (min, max) = set_count_range(obj.get("functions"));
                let looting = looting_bonus(obj.get("functions"));
                self.emit_loot_row(
                    source,
                    mob,
                    chest,
                    name,
                    share * conditions.chance,
                    (min, max),
                    looting,
                    &conditions.label,
                    entry_weight(entry) as u32,
                );
            }
            "tag" => {
                let Some(tag) = obj.get("name").and_then(Json::as_str) else {
                    return;
                };
                // `expand: true` — every member as a separate roll;
                // otherwise the tag resolves to one representative item.
                let expand = obj.get("expand").and_then(Json::as_bool).unwrap_or(false);
                let members: Vec<String> = if expand {
                    self.tag_member_ids(tag)
                } else {
                    self.index.first_in_tag(tag).map_or_else(
                        || self.tag_member_ids(tag).into_iter().take(1).collect(),
                        |id| vec![id],
                    )
                };
                for id in members {
                    let (min, max) = set_count_range(obj.get("functions"));
                    let looting = looting_bonus(obj.get("functions"));
                    self.emit_loot_row(
                        source,
                        mob,
                        chest,
                        &id,
                        share * conditions.chance,
                        (min, max),
                        looting,
                        &conditions.label,
                        entry_weight(entry) as u32,
                    );
                }
            }
            "alternatives" | "sequence" | "group" => {
                let Some(children) = obj.get("children").and_then(Json::as_array) else {
                    return;
                };
                let child_total: f64 =
                    children.iter().map(entry_weight).sum::<f64>().max(1.0);
                for child in children.iter().take(64) {
                    let child_conditions = merge_conditions(
                        conditions,
                        &conditions_of(child.as_object().and_then(|o| o.get("conditions"))),
                    );
                    self.walk_loot_entry(
                        source,
                        mob,
                        chest,
                        child,
                        share * entry_weight(child) / child_total,
                        &child_conditions,
                        depth + 1,
                    );
                }
            }
            _ => {} // loot_table refs / empty / dynamic — no item to emit.
        }
    }

    fn mob_drops_plus_loot(&self) -> usize {
        self.report.mob_drops + self.report.dungeon_loot
    }

    /// Concrete members of `tag`: catalog memberships when tags have
    /// already been applied, else the raw collected defs (loot documents
    /// are walked before `apply_tags` stamps `ItemEntry.tags`).
    fn tag_member_ids(&self, tag: &str) -> Vec<String> {
        let mut members: Vec<String> = self
            .index
            .items
            .iter()
            .filter(|e| e.tags.iter().any(|t| t == tag))
            .map(|e| e.id.clone())
            .collect();
        if members.is_empty() {
            let mut resolved = BTreeMap::new();
            let mut visiting = BTreeSet::new();
            expand_tag(
                tag,
                &self.tag_defs,
                &mut resolved,
                &mut visiting,
                0,
                &mut members,
            );
            members.sort();
            members.dedup();
        }
        members
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_loot_row(
        &mut self,
        _source: &str,
        mob: &str,
        chest: Option<&str>,
        item: &str,
        chance: f64,
        count: (u32, u32),
        looting: u32,
        condition: &str,
        weight: u32,
    ) {
        self.ensure_item(item);
        if let Some(chest) = chest {
            self.index.dungeon_loot.push(DungeonLootEntry {
                chest: chest.to_owned(),
                item_id: item.to_owned(),
                min_count: count.0,
                max_count: count.1,
                weight,
                chance: chance.clamp(0.0, 1.0) as f32,
            });
            self.report.dungeon_loot += 1;
        } else {
            self.index.mob_drops.push(MobDropEntry {
                mob_id: mob.to_owned(),
                item_id: item.to_owned(),
                min_count: count.0,
                max_count: count.1,
                chance: chance.clamp(0.0, 1.0) as f32,
                looting_bonus: looting,
                condition: condition.to_owned(),
            });
            self.report.mob_drops += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Tag expansion
// ---------------------------------------------------------------------------

/// Flatten `tag`'s member list into concrete item ids (`#ns:tag` refs
/// recurse with a depth cap + in-progress cycle guard), memoizing into
/// `resolved`.
fn expand_tag(
    tag: &str,
    defs: &BTreeMap<String, Vec<String>>,
    resolved: &mut BTreeMap<String, Vec<String>>,
    visiting: &mut BTreeSet<String>,
    depth: usize,
    out: &mut Vec<String>,
) {
    if let Some(items) = resolved.get(tag) {
        out.extend(items.iter().cloned());
        return;
    }
    if depth > MAX_TAG_DEPTH || !visiting.insert(tag.to_owned()) {
        return; // cycle — fail closed, leave members empty.
    }
    let mut items = Vec::new();
    if let Some(members) = defs.get(tag) {
        for member in members {
            if let Some(nested) = member.strip_prefix('#') {
                expand_tag(nested, defs, resolved, visiting, depth + 1, &mut items);
            } else {
                items.push(member.clone());
            }
        }
    }
    items.sort();
    items.dedup();
    visiting.remove(tag);
    resolved.insert(tag.to_owned(), items.clone());
    out.extend(items);
}

// ---------------------------------------------------------------------------
// Loot-table value extraction helpers
// ---------------------------------------------------------------------------

/// `(min, avg)` for a `rolls`-style field: plain number or
/// `{min,max}`/`{type:"minecraft:uniform",min,max}`/`{type:"minecraft:binomial",n,p}`.
fn json_count_range(value: Option<&Json>) -> (u32, f64) {
    match value {
        Some(Json::Num(n)) => {
            let n = n.clamp(0.0, 255.0);
            (n as u32, n)
        }
        Some(Json::Obj(o)) => {
            if let Some(inner) = o.get("count") {
                return json_count_range(Some(inner));
            }
            let min = o.get("min").and_then(Json::as_f64).unwrap_or(0.0);
            let max = o.get("max").and_then(Json::as_f64).unwrap_or(min);
            if let Some(n) = o.get("n").and_then(Json::as_f64) {
                return (0, n.clamp(0.0, 255.0) / 2.0);
            }
            ((min as u32), (min + max) / 2.0)
        }
        _ => (1, 1.0),
    }
}

/// `set_count` range for one entry's `functions` array.
fn set_count_range(functions: Option<&Json>) -> (u32, u32) {
    let Some(arr) = functions.and_then(Json::as_array) else {
        return (1, 1);
    };
    for f in arr {
        let Some(obj) = f.as_object() else {
            continue;
        };
        let name = obj
            .get("function")
            .and_then(Json::as_str)
            .unwrap_or("")
            .rsplit(':')
            .next()
            .unwrap_or("");
        if name == "set_count" {
            return match obj.get("count") {
                Some(Json::Num(n)) => {
                    let n = n.clamp(0.0, 255.0) as u32;
                    (n, n)
                }
                Some(Json::Obj(o)) => {
                    let min = o.get("min").and_then(Json::as_f64).unwrap_or(0.0) as u32;
                    let max = o
                        .get("max")
                        .and_then(Json::as_f64)
                        .unwrap_or(f64::from(min)) as u32;
                    (min, max.max(min))
                }
                _ => (1, 1),
            };
        }
    }
    (1, 1)
}

/// `looting_enchant` count's upper bound → extra items per Looting level.
fn looting_bonus(functions: Option<&Json>) -> u32 {
    let Some(arr) = functions.and_then(Json::as_array) else {
        return 0;
    };
    for f in arr {
        let Some(obj) = f.as_object() else {
            continue;
        };
        let name = obj
            .get("function")
            .and_then(Json::as_str)
            .unwrap_or("")
            .rsplit(':')
            .next()
            .unwrap_or("");
        if name == "looting_enchant" || name == "looting" {
            return match obj.get("count") {
                Some(Json::Num(n)) => n.clamp(0.0, 255.0) as u32,
                Some(Json::Obj(o)) => o.get("max").and_then(Json::as_f64).unwrap_or(0.0) as u32,
                _ => 1,
            };
        }
    }
    0
}

struct LootConditions {
    /// Multiplicative chance product from `random_chance*` conditions.
    chance: f64,
    /// Compact label of gameplay conditions (`"killed_by_player"`).
    label: String,
}

fn conditions_of(conditions: Option<&Json>) -> LootConditions {
    let mut out = LootConditions {
        chance: 1.0,
        label: String::new(),
    };
    let Some(arr) = conditions.and_then(Json::as_array) else {
        return out;
    };
    for cond in arr.iter().take(8) {
        let Some(obj) = cond.as_object() else {
            continue;
        };
        let name = obj
            .get("condition")
            .and_then(Json::as_str)
            .unwrap_or("")
            .rsplit(':')
            .next()
            .unwrap_or("")
            .to_owned();
        match name.as_str() {
            "random_chance" | "random_chance_with_looting" | "random_chance_with_enchanted_bonus" => {
                let p = obj
                    .get("chance")
                    .and_then(Json::as_f64)
                    .unwrap_or(1.0)
                    .clamp(0.0, 1.0);
                out.chance *= p;
            }
            "" => {}
            other => {
                if !out.label.is_empty() {
                    out.label.push('+');
                }
                if out.label.len() < 48 {
                    out.label.push_str(other);
                }
            }
        }
    }
    out
}

fn merge_conditions(a: &LootConditions, b: &LootConditions) -> LootConditions {
    let mut label = a.label.clone();
    if !b.label.is_empty() {
        if !label.is_empty() {
            label.push('+');
        }
        if label.len() < 48 {
            label.push_str(&b.label);
        }
    }
    LootConditions {
        chance: a.chance * b.chance,
        label,
    }
}

/// `weight` field of a loot entry (default 1).
fn entry_weight(entry: &Json) -> f64 {
    entry
        .as_object()
        .and_then(|o| o.get("weight"))
        .and_then(Json::as_f64)
        .unwrap_or(1.0)
        .max(0.0)
}

/// Recursive bounded walk of a datapack directory collecting `data/`-relative
/// json file paths (`"ns/path/file.json"` fragments).
fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<String>) {
    if out.len() >= MAX_DATAPACK_FILES {
        return;
    }
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        if out.len() >= MAX_DATAPACK_FILES {
            return;
        }
        let path = entry.path();
        if path.is_dir() {
            walk_dir(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NeoForge ingredient/result normalization
// ---------------------------------------------------------------------------
//
// `RecipeIndex::load_recipe_json` speaks the vanilla grammar; NeoForge packs
// wrap real ingredients in `{"type":"neoforge:compound","children":[...]}` (or
// `"ingredients":[...]`) and nest result stacks as `{"item":{"id","count"}}`.
// Rather than teaching client-ui new syntax, the ingest layer rewrites those
// two forms to the plain vanilla shape before parsing — recipe browser's deterministic
// "first alternative" pick, matching the array-of-alternatives rule.

fn normalize_recipe_document(text: &str) -> String {
    let Some(mut root) = parse_json(text) else {
        return text.to_owned();
    };
    let Some(obj) = root.as_object_mut() else {
        return text.to_owned();
    };
    let mut changed = false;
    for key in ["ingredient", "ingredients", "template", "base", "addition"] {
        if let Some(v) = obj.get_mut(key) {
            changed |= normalize_slot(v, true);
        }
    }
    if let Some(Json::Obj(keymap)) = obj.get_mut("key") {
        for v in keymap.values_mut() {
            changed |= normalize_slot(v, true);
        }
    }
    // Immersive-Engineering-style schemas use `input` for the ingredient.
    if !obj.contains_key("ingredient") && !obj.contains_key("ingredients") {
        if let Some(mut input) = obj.get("input").cloned() {
            normalize_slot(&mut input, true);
            obj.insert("ingredient".to_owned(), input);
            changed = true;
        }
    }
    for key in ["result", "results", "transitionalItem"] {
        if let Some(v) = obj.get_mut(key) {
            changed |= normalize_slot(v, false);
        }
    }
    // IE `secondaries` — extra chanced outputs folded into `results`.
    if let Some(Json::Arr(secs)) = obj.get("secondaries") {
        let mut extra: Vec<Json> = Vec::new();
        for sec in secs.iter().take(8) {
            let Some(output) = sec.as_object().and_then(|o| o.get("output")) else {
                continue;
            };
            let mut entry = normalize_result(output);
            if let Json::Obj(em) = &mut entry {
                if let Some(chance) = sec.as_object().and_then(|o| o.get("chance")) {
                    em.insert("chance".to_owned(), chance.clone());
                }
            }
            extra.push(entry);
        }
        if !extra.is_empty() {
            match obj.get_mut("results") {
                Some(Json::Arr(results)) => results.extend(extra),
                _ => {
                    obj.insert("results".to_owned(), Json::Arr(extra));
                }
            }
            changed = true;
        }
    }
    if let Some(Json::Arr(seq)) = obj.get_mut("sequence") {
        for step in seq.iter_mut() {
            if let Some(so) = step.as_object_mut() {
                for key in ["ingredient", "ingredients"] {
                    if let Some(v) = so.get_mut(key) {
                        changed |= normalize_slot(v, true);
                    }
                }
            }
        }
    }
    if changed {
        json_to_string(&root)
    } else {
        text.to_owned()
    }
}

/// Normalize one slot value in place — arrays map element-wise. Returns
/// whether anything changed (so unchanged documents keep their original
/// bytes, preserving the raw text in parse-error notes).
fn normalize_slot(v: &mut Json, ingredients: bool) -> bool {
    let nv = match &mut *v {
        Json::Arr(a) => Json::Arr(
            a.iter()
                .map(|x| {
                    if ingredients {
                        normalize_ingredient(x)
                    } else {
                        normalize_result(x)
                    }
                })
                .collect(),
        ),
        other => {
            if ingredients {
                normalize_ingredient(other)
            } else {
                normalize_result(other)
            }
        }
    };
    if nv != *v {
        *v = nv;
        return true;
    }
    false
}

/// `{"item":{"id":"x","count":n}}`-in-item-position hoist shared by
/// ingredient and result slots. Returns `None` when no nested stack exists.
fn hoist_nested_item(m: &BTreeMap<String, Json>) -> Option<Json> {
    for key in ["item", "id"] {
        let Some(Json::Obj(inner)) = m.get(key) else {
            continue;
        };
        let id = inner
            .get("id")
            .or_else(|| inner.get("item"))
            .and_then(Json::as_str)?;
        let mut out = m.clone();
        out.insert(key.to_owned(), Json::Str(id.to_owned()));
        if !out.contains_key("count") {
            if let Some(count) = inner.get("count") {
                out.insert("count".to_owned(), count.clone());
            }
        }
        return Some(Json::Obj(out));
    }
    None
}

/// Unwrap NeoForge compound-typed ingredient wrappers down to the first
/// concrete child (`item`/`id`/`tag`/`fluid` slots pass through untouched).
fn normalize_ingredient(v: &Json) -> Json {
    match v {
        Json::Arr(a) => Json::Arr(a.iter().map(normalize_ingredient).collect()),
        Json::Obj(m) => {
            if let Some(hoisted) = hoist_nested_item(m) {
                return hoisted;
            }
            let concrete = m
                .get("item")
                .or_else(|| m.get("id"))
                .and_then(Json::as_str)
                .is_some()
                || m.contains_key("tag")
                || m.contains_key("fluid")
                || m.contains_key("fluidTag");
            if !concrete {
                // `neoforge:components` fluid slots use the plural key.
                if let Some(f) = m.get("fluids").and_then(Json::as_str) {
                    let mut out = m.clone();
                    out.remove("fluids");
                    out.insert("fluid".to_owned(), Json::Str(f.to_owned()));
                    return Json::Obj(out);
                }
                for key in ["children", "ingredients"] {
                    if let Some(first) = m.get(key).and_then(Json::as_array).and_then(|a| a.first())
                    {
                        return normalize_ingredient(first);
                    }
                }
                // `neoforge:difference` — display the base set.
                if let Some(base) = m.get("base") {
                    return normalize_ingredient(base);
                }
            }
            v.clone()
        }
        other => other.clone(),
    }
}

/// Hoist nested result stacks `{"item":{"id":"x","count":n}}` (and the same
/// under `"id"`) into the flat `{"item":"x","count":n}` vanilla form, map
/// IE `basePredicate` results, and fold `chance > 1` percentages (Create's
/// 0-100+ scale) into the parser's 0-1 fraction.
fn normalize_result(v: &Json) -> Json {
    match v {
        Json::Arr(a) => Json::Arr(a.iter().map(normalize_result).collect()),
        Json::Obj(m) => {
            // IE `{"basePredicate":{"item":"x"},"count":n}` result form.
            if let Some(Json::Obj(bp)) = m.get("basePredicate") {
                if let Some(id) = bp
                    .get("item")
                    .or_else(|| bp.get("id"))
                    .and_then(Json::as_str)
                {
                    let mut out = BTreeMap::new();
                    out.insert("item".to_owned(), Json::Str(id.to_owned()));
                    for key in ["count", "chance"] {
                        if let Some(val) = m.get(key) {
                            out.insert(key.to_owned(), val.clone());
                        }
                    }
                    return Json::Obj(out);
                }
            }
            let mut out = hoist_nested_item(m).unwrap_or_else(|| v.clone());
            if let Json::Obj(om) = &mut out {
                if let Some(&Json::Num(chance)) = om.get("chance") {
                    if chance > 1.0 {
                        om.insert(
                            "chance".to_owned(),
                            Json::Num((chance / 100.0).clamp(0.0, 1.0)),
                        );
                    }
                }
            }
            out
        }
        other => other.clone(),
    }
}

fn json_to_string(v: &Json) -> String {
    let mut out = String::with_capacity(256);
    write_json(v, &mut out);
    out
}

fn write_json(v: &Json, out: &mut String) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) => {
            if n.fract() == 0.0 && n.abs() < 9.0e15 {
                out.push_str(&(*n as i64).to_string());
            } else {
                out.push_str(&n.to_string());
            }
        }
        Json::Str(s) => {
            out.push('"');
            for ch in s.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => {
                        out.push_str(&format!("\\u{:04x}", c as u32));
                    }
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        Json::Arr(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(x, out);
            }
            out.push(']');
        }
        Json::Obj(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(&Json::Str(k.clone()), out);
                out.push(':');
                write_json(x, out);
            }
            out.push('}');
        }
    }
}

// ---------------------------------------------------------------------------
// Fail-closed JSON parsing — canonical `megu_core::json` reader adapted to
// this module's mutable `Json` tree (numbers are f64, depth/node bounded)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    fn as_object(&self) -> Option<&BTreeMap<String, Json>> {
        match self {
            Self::Obj(m) => Some(m),
            _ => None,
        }
    }

    fn as_object_mut(&mut self) -> Option<&mut BTreeMap<String, Json>> {
        match self {
            Self::Obj(m) => Some(m),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Arr(a) => Some(a),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

/// Bounds for [`parse_json`], matching the retired parser: nesting deeper
/// than [`MAX_JSON_DEPTH`] levels rejected, at most [`MAX_JSON_NODES`]
/// value nodes, and no byte ceiling of its own — callers already bound
/// each document at [`MAX_JSON_BYTES`].
const JSON_LIMITS: JsonLimits = JsonLimits {
    max_depth: MAX_JSON_DEPTH + 1,
    max_nodes: MAX_JSON_NODES,
    max_bytes: usize::MAX,
};

fn parse_json(src: &str) -> Option<Json> {
    let value = parse_json_with_limits(src, JSON_LIMITS, false).ok()?;
    Some(json_value_to_json(&value))
}

/// Fold the canonical [`JsonValue`] tree into this module's [`Json`]:
/// document-ordered members collapse into a `BTreeMap` with
/// last-duplicate-key wins, exactly like the retired parser's `insert`.
fn json_value_to_json(value: &JsonValue) -> Json {
    match value {
        JsonValue::Null => Json::Null,
        JsonValue::Bool(b) => Json::Bool(*b),
        JsonValue::Number(n) => Json::Num(*n),
        JsonValue::String(s) => Json::Str(s.clone()),
        JsonValue::Array(items) => Json::Arr(items.iter().map(json_value_to_json).collect()),
        JsonValue::Object(members) => {
            let mut map = BTreeMap::new();
            for (key, member) in members {
                map.insert(key.clone(), json_value_to_json(member));
            }
            Json::Obj(map)
        }
    }
}
