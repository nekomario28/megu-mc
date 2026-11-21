// ---------- Lane P3: complete datapack tags + extended recipe registry ----------
//
// `include!`d from commands_core.rs (same flat scope — no `mod`), so every name
// here is crate-visible just like the rest of the datapack loader. The loader
// proper (`DatapackContent`, `read_entry`, `reload_datapacks`) stays in
// commands_core.rs; this file adds:
//   * the full 1.21.1 tag category set with `#ref` expansion, cycle guard,
//     `"replace": true` and `{"id":..., "required": false}` handling,
//   * the extended `DatapackRegistry` (`stonecutting`, `smithing_transform`,
//     `smithing_trim`, `crafting_special_*`/`crafting_decorated_pot`),
//   * the live stores the P3 commands need: `/fillbiome` quart-cell overrides,
//     `/spectate` camera targets, and the `/place` dispatch log.

// ---------------------------------------------------------------------------
// Tag categories — vanilla 1.21.1 reads `data/<ns>/tags/<registry>/<name>.json`.
// ---------------------------------------------------------------------------

/// Canonical tag directory name. Vanilla plural directories (`tags/blocks`)
/// are legacy aliases for the singular registry dirs; `worldgen/<registry>`
/// keeps both path components.
fn tag_category(dir: &str) -> &str {
    match dir {
        "blocks" => "block",
        "items" => "item",
        "entity_types" => "entity_type",
        "fluids" => "fluid",
        "game_events" => "game_event",
        "functions" => "function",
        other => other,
    }
}

/// Split `tags/<rest>` into (`category`, `tag-name`). One path component for
/// the registry, except `worldgen/*` which takes two (`worldgen/biome`,
/// `worldgen/structure`, `worldgen/world_preset`, ...).
fn split_tag_path(rest: &str) -> Option<(String, &str)> {
    let first = rest.find('/')?;
    let (category, name) = if &rest[..first] == "worldgen" {
        let second = rest[first + 1..].find('/')?;
        (&rest[..first + 1 + second], &rest[first + 1 + second + 1..])
    } else {
        (tag_category(&rest[..first]), &rest[first + 1..])
    };
    if category.is_empty() || name.is_empty() {
        return None;
    }
    Some((category.to_owned(), name))
}

/// Top-level `,` split of a JSON array's inner text, ignoring commas inside
/// nested `{}`/`[]`/`""`. Escaped quotes never occur inside tag member ids.
fn jelems(arr: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut st, mut in_str) = (0i32, 0usize, false);
    for (i, ch) in arr.char_indices() {
        match ch {
            '"' => in_str = !in_str,
            '{' | '[' if !in_str => depth += 1,
            '}' | ']' if !in_str => depth -= 1,
            ',' if !in_str && depth == 0 => {
                out.push(&arr[st..i]);
                st = i + 1;
            }
            _ => {}
        }
    }
    out.push(&arr[st..]);
    out.into_iter().map(str::trim).filter(|e| !e.is_empty()).collect()
}

// ---------------------------------------------------------------------------
// DatapackContent impl — tag loading + optional-entry resolution.
// ---------------------------------------------------------------------------

impl DatapackContent {
    /// Merge one `tags/<category>/<name>.json` file into the staging registry.
    /// `"replace": true` clears the tag's accumulated members (and any deferred
    /// optional entries) before this pack's `values` land.
    fn read_tag_entry(&mut self, category: &str, tag: &str, body: &str) {
        let map = self.registry.tags.entry(category.to_owned()).or_default();
        if jstr(body, "replace") == Some("true") {
            map.remove(tag);
            self.optional_tags
                .retain(|(c, t, _)| c != category || t != tag);
        }
        let Some(values) = jarray(body, "values") else {
            return;
        };
        let entry = map.entry(tag.to_owned()).or_default();
        for elem in jelems(values) {
            let (id, required) = if elem.starts_with('{') {
                match jstr(elem, "id") {
                    Some(id) => (id, jstr(elem, "required") != Some("false")),
                    None => continue,
                }
            } else {
                (elem.trim_matches('"'), true)
            };
            if id.is_empty() {
                continue;
            }
            let member = if let Some(reference) = id.strip_prefix('#') {
                let r = reference.trim();
                if r.contains(':') {
                    format!("#{r}")
                } else {
                    format!("#minecraft:{r}")
                }
            } else if id.contains(':') {
                id.to_owned()
            } else {
                format!("minecraft:{id}")
            };
            if required {
                entry.push(member);
            } else {
                self.optional_tags
                    .push((category.to_owned(), tag.to_owned(), member));
            }
        }
    }

    /// Drop `required:false` members whose target stayed absent after every
    /// pack loaded — `#ref` targets that never appeared, or id targets in a
    /// category we can resolve (block/item/entity_type/biome). Categories with
    /// no resolver keep their entries (absence cannot be proven).
    fn resolve_optional_tags(&mut self) {
        let pending = std::mem::take(&mut self.optional_tags);
        for (category, tag, member) in pending {
            let keep = match member.strip_prefix('#') {
                Some(reference) => self
                    .registry
                    .tags
                    .get(&category)
                    .is_some_and(|map| map.contains_key(reference)),
                None => tag_member_resolvable(&category, &member),
            };
            if keep {
                self.registry
                    .tags
                    .get_mut(&category)
                    .expect("read_tag_entry seeds the category")
                    .entry(tag)
                    .or_default()
                    .push(member);
            }
        }
    }
}

/// `true` when a concrete tag member id exists in its registry. `block` goes
/// through the palette (`checked_state` namespaces `minecraft:` itself);
/// `item`/`entity_type`/`biome` have name lookup helpers.
fn tag_member_resolvable(category: &str, member: &str) -> bool {
    let bare = member.strip_prefix("minecraft:").unwrap_or(member);
    match category {
        "block" => breg().is_some_and(|reg| reg.checked_state(member, &[]).is_some()),
        "item" => name_to_item(bare).is_some(),
        "entity_type" => name_to_etype(bare).is_some(),
        "biome" | "worldgen/biome" => known_biome(member),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Tag query — nested `#ref` expansion with a cycle guard.
// ---------------------------------------------------------------------------

#[allow(dead_code)] // expansion API — consumed by tag lookups and later lanes
fn tag_expand_into(
    map: &HashMap<String, Vec<String>>,
    id: &str,
    visiting: &mut HashSet<String>,
    seen: &mut HashSet<String>,
    out: &mut Vec<String>,
) {
    // Already on the DFS stack: `#a -> #b -> #a` stops here instead of looping.
    if !visiting.insert(id.to_owned()) {
        return;
    }
    if let Some(members) = map.get(id) {
        for member in members {
            match member.strip_prefix('#') {
                Some(reference) => tag_expand_into(map, reference, visiting, seen, out),
                None => {
                    if seen.insert(member.clone()) {
                        out.push(member.clone());
                    }
                }
            }
        }
    }
    visiting.remove(id);
}

/// Fully expanded member list of one tag id (`ns:name` without `#`).
#[allow(dead_code)] // expansion API — consumed by tag lookups and later lanes
fn datapack_tag_members(category: &str, id: &str) -> Vec<String> {
    let registry = datapack_registry().lock().unwrap();
    let mut out = Vec::new();
    if let Some(map) = registry.tags.get(category) {
        tag_expand_into(map, id, &mut HashSet::new(), &mut HashSet::new(), &mut out);
    }
    out
}

fn tag_contains_rec(
    map: &HashMap<String, Vec<String>>,
    id: &str,
    member: &str,
    visiting: &mut HashSet<String>,
) -> bool {
    if !visiting.insert(id.to_owned()) {
        return false;
    }
    let found = map.get(id).is_some_and(|members| {
        members.iter().any(|m| match m.strip_prefix('#') {
            Some(reference) => tag_contains_rec(map, reference, member, visiting),
            None => m == member,
        })
    });
    visiting.remove(id);
    found
}

/// `#ref`-aware membership test used by `/fillbiome ... replace #tag`.
fn datapack_tag_contains(category: &str, id: &str, member: &str) -> bool {
    let registry = datapack_registry().lock().unwrap();
    registry
        .tags
        .get(category)
        .is_some_and(|map| tag_contains_rec(map, id, member, &mut HashSet::new()))
}

/// `true` when a `#tag` id is registered under the `biome` or `worldgen/biome`
/// tag category (both spellings appear in 1.21.1 datapacks).
fn biome_tag_known(tag: &str) -> bool {
    let registry = datapack_registry().lock().unwrap();
    ["biome", "worldgen/biome"]
        .iter()
        .any(|cat| registry.tags.get(*cat).is_some_and(|map| map.contains_key(tag)))
}

// ---------------------------------------------------------------------------
// Extended recipe registry.
// ---------------------------------------------------------------------------

/// `minecraft:stonecutting`: ingredient alternatives, result item, count.
/// Fields are read by the tests and by later recipe consumers (P1 lane).
#[cfg_attr(not(test), allow(dead_code))]
struct StonecuttingRecipe {
    ingredients: Vec<Ingr>,
    result: i64,
    count: u8,
}

/// `minecraft:smithing_transform` (template+base+addition → result) and
/// `minecraft:smithing_trim` (same slots; no result — vanilla keeps the base
/// item and applies the trim, so `result` is `None` there).
/// Fields are read by the tests and by later recipe consumers (P1 lane).
#[cfg_attr(not(test), allow(dead_code))]
struct SmithingRecipe {
    template: Vec<Ingr>,
    base: Vec<Ingr>,
    addition: Vec<Ingr>,
    result: Option<i64>,
}

/// Everything datapack-driven the core loader does not already publish to its
/// own global: the full tag category set plus the non-crafting recipe indexes.
/// Swapped atomically by `DatapackContent::publish` under its lock order.
#[derive(Default)]
struct DatapackRegistry {
    /// `tags/<category>/<name>` for every registry directory. `block`/`item`
    /// are also mirrored into the legacy `block_tags`/`item_tags` globals.
    tags: HashMap<String, HashMap<String, Vec<String>>>,
    stonecutting: Vec<StonecuttingRecipe>,
    smithing_transform: Vec<SmithingRecipe>,
    smithing_trim: Vec<SmithingRecipe>,
    /// recipe-id → recipe-type for `crafting_special_*` / `crafting_decorated_pot`.
    special_recipes: std::collections::BTreeMap<String, String>,
}

fn datapack_registry() -> &'static Mutex<DatapackRegistry> {
    static R: OnceLock<Mutex<DatapackRegistry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(DatapackRegistry::default()))
}

/// One `{"item"|"tag": ...}` slot or a bare `"ns:id"` member.
fn dp_ingredient(spec: &str) -> Option<Ingr> {
    if let Some(t) = jstr(spec, "tag") {
        let t = t.trim_start_matches('#');
        return Some(Ingr::Tag(if t.contains(':') {
            t.to_owned()
        } else {
            format!("minecraft:{t}")
        }));
    }
    let bare = jstr(spec, "item").unwrap_or_else(|| spec.trim().trim_matches('"'));
    if bare.is_empty() || bare.starts_with(['{', '[', '}', ']']) {
        return None;
    }
    name_to_item(bare.trim_start_matches("minecraft:")).map(Ingr::Item)
}

/// An `Ingredient` JSON field: `{"item"|"tag": ...}`, a bare `"ns:id"`, or an
/// array of those alternatives.
fn dp_ingredients(body: &str, key: &str) -> Vec<Ingr> {
    if let Some(arr) = jarray(body, key) {
        jelems(arr).iter().filter_map(|e| dp_ingredient(e)).collect()
    } else if let Some(blk) = jblock(body, key) {
        dp_ingredient(blk).into_iter().collect()
    } else {
        jstr(body, key)
            .map(|s| s.trim_matches('"'))
            .and_then(|s| name_to_item(s.trim_start_matches("minecraft:")).map(Ingr::Item))
            .into_iter()
            .collect()
    }
}

/// `result` accepts `"id"`, `"item"`, or a bare `"ns:id"` string — the schema
/// changed across 1.20.5/1.21.x and all three spellings ship in packs.
fn dp_result_item(body: &str) -> Option<i64> {
    let name = jblock(body, "result")
        .and_then(|b| jstr(b, "id").or_else(|| jstr(b, "item")))
        .or_else(|| jstr(body, "result").map(|s| s.trim_matches('"')));
    name.and_then(|n| name_to_item(n.trim_start_matches("minecraft:")))
}

impl DatapackContent {
    /// Recipe types beyond the shaped/shapeless/cooking readers. Returns
    /// `true` when the type was consumed here (even if a malformed file was
    /// dropped), `false` to fall through to the legacy readers.
    fn read_extended_recipe(&mut self, key: &str, body: &str) -> bool {
        let rtype = jstr(body, "type")
            .unwrap_or("")
            .trim_start_matches("minecraft:");
        match rtype {
            "stonecutting" => {
                let ingredients = dp_ingredients(body, "ingredient");
                let Some(result) = dp_result_item(body) else {
                    return true; // no resolvable result: drop, fail closed
                };
                if ingredients.is_empty() {
                    return true;
                }
                let count = jblock(body, "result")
                    .and_then(|b| jnum(b, "count"))
                    .or_else(|| jnum(body, "count"))
                    .unwrap_or(1)
                    .clamp(1, 64) as u8;
                self.registry.stonecutting.push(StonecuttingRecipe {
                    ingredients,
                    result,
                    count,
                });
            }
            "smithing_transform" | "smithing" => {
                let (template, base, addition) = (
                    dp_ingredients(body, "template"),
                    dp_ingredients(body, "base"),
                    dp_ingredients(body, "addition"),
                );
                if base.is_empty() {
                    return true;
                }
                self.registry.smithing_transform.push(SmithingRecipe {
                    template,
                    base,
                    addition,
                    result: dp_result_item(body),
                });
            }
            "smithing_trim" => {
                let (template, base, addition) = (
                    dp_ingredients(body, "template"),
                    dp_ingredients(body, "base"),
                    dp_ingredients(body, "addition"),
                );
                if base.is_empty() {
                    return true;
                }
                self.registry.smithing_trim.push(SmithingRecipe {
                    template,
                    base,
                    addition,
                    result: None,
                });
            }
            t if t.starts_with("crafting_special_") || t == "crafting_decorated_pot" => {
                self.registry
                    .special_recipes
                    .insert(key.to_owned(), rtype.to_owned());
            }
            _ => return false,
        }
        true
    }
}

// ---------------------------------------------------------------------------
// /fillbiome — quart-cell overrides + known-biome validation.
// ---------------------------------------------------------------------------

/// Vanilla 1.21.1 `minecraft:worldgen/biome` registry order — identical to
/// `worldgen.rs`'s `BIOME64`, so `worldgen::biome_at`'s `i64` indexes this
/// list. Copied because `BIOME64` is private to the worldgen owner file.
const VANILLA_BIOMES: [&str; 64] = [
    "badlands",
    "bamboo_jungle",
    "basalt_deltas",
    "beach",
    "birch_forest",
    "cherry_grove",
    "cold_ocean",
    "crimson_forest",
    "dark_forest",
    "deep_cold_ocean",
    "deep_dark",
    "deep_frozen_ocean",
    "deep_lukewarm_ocean",
    "deep_ocean",
    "desert",
    "dripstone_caves",
    "end_barrens",
    "end_highlands",
    "end_midlands",
    "eroded_badlands",
    "flower_forest",
    "forest",
    "frozen_ocean",
    "frozen_peaks",
    "frozen_river",
    "grove",
    "ice_spikes",
    "jagged_peaks",
    "jungle",
    "lukewarm_ocean",
    "lush_caves",
    "mangrove_swamp",
    "meadow",
    "mushroom_fields",
    "nether_wastes",
    "ocean",
    "old_growth_birch_forest",
    "old_growth_pine_taiga",
    "old_growth_spruce_taiga",
    "plains",
    "river",
    "savanna",
    "savanna_plateau",
    "small_end_islands",
    "snowy_beach",
    "snowy_plains",
    "snowy_slopes",
    "snowy_taiga",
    "soul_sand_valley",
    "sparse_jungle",
    "stony_peaks",
    "stony_shore",
    "sunflower_plains",
    "swamp",
    "taiga",
    "the_end",
    "the_void",
    "warm_ocean",
    "warped_forest",
    "windswept_forest",
    "windswept_gravelly_hills",
    "windswept_hills",
    "windswept_savanna",
    "wooded_badlands",
];

/// `true` for a known 1.21.1 biome: the 64 vanilla names plus any datapack
/// `worldgen/biome` file (any namespace). `/fillbiome` fails closed on `false`.
fn known_biome(id: &str) -> bool {
    let path = id.rsplit(':').next().unwrap_or(id);
    VANILLA_BIOMES.contains(&path) || worldgen::Gen::dp_files("biome").contains_key(id)
}

/// `/fillbiome` writes: per-dimension quart-position overrides, consulted by
/// `effective_biome_name` (and so `replace <filter>`) before the natural
/// generator answer.
fn biome_overrides() -> &'static Mutex<HashMap<DimId, HashMap<(i32, i32, i32), String>>> {
    static O: OnceLock<Mutex<HashMap<DimId, HashMap<(i32, i32, i32), String>>>> =
        OnceLock::new();
    O.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The biome a quart cell reports now: a `/fillbiome` override wins over the
/// generator's natural `worldgen::biome_at` answer.
fn effective_biome_name(dim: DimId, qx: i32, qy: i32, qz: i32) -> Option<String> {
    if let Some(name) = biome_overrides()
        .lock()
        .unwrap()
        .get(&dim)
        .and_then(|m| m.get(&(qx, qy, qz)))
    {
        return Some(name.clone());
    }
    worldgen::biome_at(qx, qy, qz)
        .and_then(|id| usize::try_from(id).ok())
        .and_then(|i| VANILLA_BIOMES.get(i))
        .map(|n| format!("minecraft:{n}"))
}

// ---------------------------------------------------------------------------
// /spectate + /place command stores.
// ---------------------------------------------------------------------------

/// `ServerPlayer.camera`: the entity each spectator client renders through.
/// `/spectate <target>` writes the target's eid; `/spectate` alone resets the
/// entry to the viewer's own eid.
fn spectator_cameras() -> &'static Mutex<HashMap<i32, i32>> {
    static C: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A `/place` request the command layer accepted: validated id, dimension and
/// anchor position. Templates are stamped into the live world before being
/// logged here; configured features cannot run without a chunk generation
/// context (there is no live `FC` seam — `FC` borrows `&mut Gen` and the Gen
/// lock is private to `worldgen.rs`), so their accepted dispatch is the
/// recorded effect. Fields are the record; only tests inspect them today.
#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct PlaceDispatch {
    dim: DimId,
    pos: (i32, i32, i32),
    id: String,
    kind: PlaceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaceKind {
    Feature,
    Template,
}

fn place_dispatches() -> &'static Mutex<Vec<PlaceDispatch>> {
    static D: OnceLock<Mutex<Vec<PlaceDispatch>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(Vec::new()))
}

/// `/place template` live stamp: load `structures/<id>.nbt`, refuse
/// entity-bearing templates (same fail-closed contract as `place_blocks`),
/// and write palette-0 block states (structure voids skipped, matching
/// vanilla `StructureTemplate.placeInWorld` semantics).
fn place_template_live(dim: DimId, anchor: (i32, i32, i32), id: &str) -> Result<usize, String> {
    let bytes = worldgen::Gen::dp_resource("structure", id, "nbt")
        .ok_or_else(|| format!("No such template '{id}'"))?;
    let (_, root) = crate::nbt::read_gzip(&bytes, 16 * 1024 * 1024)
        .map_err(|e| format!("Failed to read template '{id}': {e}"))?;
    let reg = breg().ok_or_else(|| "missing block registry".to_owned())?;
    let template = structure_template::Template::load(&root, reg)
        .map_err(|e| format!("Failed to load template '{id}': {e}"))?;
    if !template.entities.is_empty() {
        return Err(format!(
            "Template '{id}' carries entities, which are not supported"
        ));
    }
    let Some(palette) = template.palettes.first() else {
        return Ok(0);
    };
    let mut placed = 0usize;
    with_save_guard(|| {
        for block in palette {
            let (x, y, z) = (
                anchor.0 + block.pos.0,
                anchor.1 + block.pos.1,
                anchor.2 + block.pos.2,
            );
            if check_modification_bounds(dim, (x, y, z)).is_err() {
                continue;
            }
            if reg.name_of(block.state) == "minecraft:structure_void" {
                continue;
            }
            place_block_dim(dim, x, y, z, block.state);
            placed += 1;
        }
    });
    Ok(placed)
}

// ------------------------------------------------------------ p3_parity ---

#[cfg(test)]
mod p3_datapack_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// `CommandDirectory` lives inside `mod command_tests`; these fixtures
    /// need the same fresh-datapacks cwd so `datapacks/` is synthetic.
    struct P3Directory {
        path: PathBuf,
        previous: PathBuf,
    }

    impl P3Directory {
        fn enter() -> Self {
            let path = std::env::temp_dir()
                .join(format!("spine-p3-datapack-{}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            let previous = std::env::current_dir().unwrap();
            std::env::set_current_dir(&path).unwrap();
            Self { path, previous }
        }
    }

    impl std::ops::Drop for P3Directory {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.previous).unwrap();
            std::fs::remove_dir_all(&self.path).unwrap();
        }
    }

    fn write_file(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn p3_parity_datapack_tags_all_categories_nested_cycle_replace_optional() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        let _dir = P3Directory::enter();
        let root = Path::new("datapacks");
        // All nine 1.21.1 tag registries, under pack "a-first" (loads first).
        for category in [
            "block",
            "item",
            "entity_type",
            "fluid",
            "game_event",
            "biome",
            "damage_type",
            "banner_pattern",
            "enchantment",
        ] {
            write_file(
                &root.join(format!("a-first/data/test/tags/{category}/probe.json")),
                &format!("{{\"values\":[\"test:member_{category}\"]}}"),
            );
        }
        // Legacy plural directory alias.
        write_file(
            &root.join("a-first/data/test/tags/blocks/aliased.json"),
            "{\"values\":[\"minecraft:stone\"]}",
        );
        // Nested `#ref` plus a self-referential cycle: nested -> base -> cycle_b
        // -> cycle_a -> cycle_b (cut) -> dirt.
        write_file(
            &root.join("a-first/data/test/tags/item/base.json"),
            "{\"values\":[\"minecraft:stone\",\"#test:cycle_b\"]}",
        );
        write_file(
            &root.join("a-first/data/test/tags/item/cycle_a.json"),
            "{\"values\":[\"#test:cycle_b\"]}",
        );
        write_file(
            &root.join("a-first/data/test/tags/item/cycle_b.json"),
            "{\"values\":[\"#test:cycle_a\",\"minecraft:dirt\"]}",
        );
        write_file(
            &root.join("a-first/data/test/tags/item/nested.json"),
            "{\"values\":[\"#test:base\",\"minecraft:coal\"]}",
        );
        // `"replace": true` in the later pack clears the earlier pack's write.
        write_file(
            &root.join("a-first/data/test/tags/item/wiped.json"),
            "{\"values\":[\"minecraft:oak_log\"]}",
        );
        write_file(
            &root.join("b-second/data/test/tags/item/wiped.json"),
            "{\"replace\":true,\"values\":[\"minecraft:sand\"]}",
        );
        // `required:false` entries: a real item + an existing `#ref` are kept,
        // a missing item + a missing `#ref` are dropped cleanly.
        write_file(
            &root.join("b-second/data/test/tags/item/optional.json"),
            "{\"values\":[\
                {\"id\":\"minecraft:diamond\",\"required\":false},\
                {\"id\":\"test:no_such_item\",\"required\":false},\
                {\"id\":\"#test:ghost\",\"required\":false},\
                {\"id\":\"#test:base\",\"required\":false},\
                \"minecraft:iron_ingot\"\
            ]}",
        );
        reload_datapacks(root, None, false).expect("synthetic pack loads");

        {
            let registry = datapack_registry().lock().unwrap();
            for category in [
                "block",
                "item",
                "entity_type",
                "fluid",
                "game_event",
                "biome",
                "damage_type",
                "banner_pattern",
                "enchantment",
            ] {
                assert_eq!(
                    registry.tags[category]["test:probe"],
                    vec![format!("test:member_{category}")],
                    "category {category}"
                );
            }
            assert_eq!(
                registry.tags["block"]["test:aliased"],
                vec!["minecraft:stone".to_owned()],
                "plural alias tags/blocks maps to block"
            );
            assert_eq!(
                registry.tags["item"]["test:wiped"],
                vec!["minecraft:sand".to_owned()],
                "replace:true clears lower-priority pack entries"
            );
            assert_eq!(
                registry.tags["item"]["test:optional"],
                vec![
                    "minecraft:iron_ingot".to_owned(),
                    "minecraft:diamond".to_owned(),
                    "#test:base".to_owned()
                ],
                "required:false drops unresolved members only (deferred members append at resolve)"
            );
        }
        // Nested expansion and cycle guard terminate with the deduped member set.
        assert_eq!(
            datapack_tag_members("item", "test:nested"),
            vec!["minecraft:stone", "minecraft:dirt", "minecraft:coal"]
        );
        assert!(datapack_tag_contains("item", "test:nested", "minecraft:dirt"));
        assert!(!datapack_tag_contains("item", "test:nested", "minecraft:emerald"));
        assert!(datapack_tag_contains("item", "test:cycle_a", "minecraft:dirt"));
        // Builtins still seed the legacy block/item maps.
        assert!(block_tags().lock().unwrap().contains_key("minecraft:logs"));
        assert!(block_tags().lock().unwrap().contains_key("test:probe"));
    }

    #[test]
    fn p3_parity_datapack_recipes_stonecutting_smithing_and_specials() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        let _dir = P3Directory::enter();
        let recipes = Path::new("datapacks/a-first/data/test/recipes");
        write_file(
            &recipes.join("cut.json"),
            "{\"type\":\"minecraft:stonecutting\",\
             \"ingredient\":{\"item\":\"minecraft:stone\"},\
             \"result\":{\"id\":\"minecraft:stone_bricks\",\"count\":2}}",
        );
        write_file(
            &recipes.join("cut_bare.json"),
            "{\"type\":\"minecraft:stonecutting\",\
             \"ingredient\":[{\"item\":\"minecraft:cobblestone\"},{\"item\":\"minecraft:stone\"}],\
             \"result\":\"minecraft:gravel\"}",
        );
        write_file(
            &recipes.join("netherite.json"),
            "{\"type\":\"minecraft:smithing_transform\",\
             \"template\":{\"item\":\"minecraft:netherite_upgrade_smithing_template\"},\
             \"base\":{\"item\":\"minecraft:diamond_chestplate\"},\
             \"addition\":{\"item\":\"minecraft:netherite_ingot\"},\
             \"result\":{\"id\":\"minecraft:netherite_chestplate\"}}",
        );
        write_file(
            &recipes.join("trim.json"),
            "{\"type\":\"minecraft:smithing_trim\",\
             \"template\":{\"item\":\"minecraft:coast_armor_trim_smithing_template\"},\
             \"base\":{\"item\":\"minecraft:iron_chestplate\"},\
             \"addition\":{\"item\":\"minecraft:redstone\"}}",
        );
        write_file(
            &recipes.join("dye.json"),
            "{\"type\":\"minecraft:crafting_special_armordye\",\"category\":\"misc\"}",
        );
        write_file(
            &recipes.join("pot.json"),
            "{\"type\":\"minecraft:crafting_decorated_pot\",\"category\":\"misc\"}",
        );
        reload_datapacks(Path::new("datapacks"), None, false).expect("loads");

        let registry = datapack_registry().lock().unwrap();
        assert_eq!(registry.stonecutting.len(), 2);
        let (first, second) = (&registry.stonecutting[0], &registry.stonecutting[1]);
        assert!(
            matches!(first.ingredients[0], Ingr::Item(id) if id == name_to_item("stone").unwrap())
        );
        assert_eq!(first.result, name_to_item("stone_bricks").unwrap());
        assert_eq!(first.count, 2);
        assert_eq!(second.ingredients.len(), 2, "ingredient arrays index all alternatives");
        assert_eq!(second.result, name_to_item("gravel").unwrap());

        assert_eq!(registry.smithing_transform.len(), 1);
        let transform = &registry.smithing_transform[0];
        assert!(
            matches!(transform.template[0], Ingr::Item(id) if id == name_to_item("netherite_upgrade_smithing_template").unwrap())
        );
        assert!(
            matches!(transform.base[0], Ingr::Item(id) if id == name_to_item("diamond_chestplate").unwrap())
        );
        assert!(
            matches!(transform.addition[0], Ingr::Item(id) if id == name_to_item("netherite_ingot").unwrap())
        );
        assert_eq!(
            transform.result,
            Some(name_to_item("netherite_chestplate").unwrap())
        );

        assert_eq!(registry.smithing_trim.len(), 1);
        let trim = &registry.smithing_trim[0];
        assert_eq!(trim.base.len(), 1);
        assert_eq!(trim.result, None, "trims keep the base item in vanilla");

        assert_eq!(
            registry.special_recipes.get("test:dye").map(String::as_str),
            Some("crafting_special_armordye")
        );
        assert_eq!(
            registry.special_recipes.get("test:pot").map(String::as_str),
            Some("crafting_decorated_pot")
        );
    }

    #[test]
    fn p3_parity_datapack_tags_load_from_real_vanilla_pack() {
        crate::test_support::require_vanilla_data();
        if crate::test_support::isolated_world_test() {
            return;
        }
        // cwd is the repo root here: the real datapacks/ tree loads.
        reload_datapacks(Path::new("datapacks"), None, false).expect("vanilla loads");
        let registry = datapack_registry().lock().unwrap();
        let block = registry.tags.get("block").expect("block category");
        assert!(
            block.len() >= 184,
            "vanilla ships 184 block tag files, got {}",
            block.len()
        );
        assert!(
            block.contains_key("minecraft:logs"),
            "well-known vanilla tag missing"
        );
        // `tags/worldgen/biome/has_structure/*.json` lands under the two-level
        // `worldgen/biome` category — proving the nested category split.
        let biome = registry
            .tags
            .get("worldgen/biome")
            .expect("worldgen/biome category");
        assert!(
            biome.keys().any(|k| k.starts_with("minecraft:has_structure/")),
            "has_structure biome tags must load under worldgen/biome"
        );
        assert!(registry.tags.contains_key("worldgen/structure"));
        eprintln!(
            "vanilla tag counts: {:?}",
            registry
                .tags
                .iter()
                .map(|(c, m)| format!("{c}={}", m.len()))
                .collect::<Vec<_>>()
        );
    }
}
