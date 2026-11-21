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

// ---------------------------------------------------------------------------
// Lane P5g / issue #194 — pack.mcmeta `pack_format` validation.
//
// `pack.pack_format` is the declared format (Vanilla 1.21.1 datapacks: 48).
// `pack.supported_formats` widens it into an inclusive range spelled three
// ways: scalar `48`, array `[41, 48]`, or object
// `{"min_inclusive": 41, "max_inclusive": 48}`. The 26.3 scheme replaces both
// with `pack.min_format` / `pack.max_format` `[major, minor]` tuples compared
// lexicographically. Anything unreadable or missing classifies `Invalid` —
// fail closed, never panic.
// ---------------------------------------------------------------------------

/// `(major, minor)` datapack format version. A scalar `pack_format` reads as
/// `(v, 0)`; the 26.3 `min_format`/`max_format` arrays supply both halves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(not(test), allow(dead_code))]
struct PackVersion(u32, u32);

/// Compatibility of a `pack.mcmeta` against the server's target format.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum PackFormatVerdict {
    Compatible,
    /// The newest format the pack accepts is below the target.
    TooOld { max_supported: PackVersion },
    /// The oldest format the pack accepts is above the target.
    TooNew { min_supported: PackVersion },
    /// Malformed or missing `pack` metadata — fail closed.
    Invalid,
}

/// Parsed `pack.mcmeta`: the format fields plus `filter.block` exclusion rules.
#[cfg_attr(not(test), allow(dead_code))]
struct PackMcmeta {
    pack_format: Option<u32>,
    /// Resolved inclusive `(min, max)` from `supported_formats`.
    supported: Option<(u32, u32)>,
    min_format: Option<PackVersion>,
    max_format: Option<PackVersion>,
    /// (`namespace` regex, `path` regex) pairs from `filter.block`; a resource
    /// is blocked when both patterns of any rule fully match (vanilla
    /// `Matcher.matches()` semantics).
    filter: Vec<(String, String)>,
    /// `false` when the `pack` section is absent or a field was malformed.
    valid: bool,
}

/// A non-negative integer JSON literal, or `None` for any other shape.
fn pack_u32(j: &J) -> Option<u32> {
    match j {
        J::Num(n) if n.fract() == 0.0 && *n >= 0.0 && *n <= u32::MAX as f64 => {
            Some(*n as u32)
        }
        _ => None,
    }
}

/// A `[major, minor]` tuple (26.3 `min_format`/`max_format`) or a bare scalar
/// read as `(v, 0)`. Arrays need at least two integer elements.
fn pack_version(j: &J) -> Option<PackVersion> {
    match j {
        J::Num(_) => pack_u32(j).map(|v| PackVersion(v, 0)),
        J::Arr(a) => {
            if a.len() < 2 {
                return None;
            }
            Some(PackVersion(pack_u32(&a[0])?, pack_u32(&a[1])?))
        }
        _ => None,
    }
}

/// `supported_formats`: scalar, `[min, ...]` array, or
/// `{min_inclusive, max_inclusive}` object. Returns `Err` on any other shape
/// so the caller can flag the metadata invalid.
fn pack_supported_range(j: &J) -> Result<(u32, u32), ()> {
    match j {
        J::Num(_) => pack_u32(j).map(|v| (v, v)).ok_or(()),
        J::Arr(a) => {
            if a.is_empty() {
                return Err(());
            }
            let mut lo = u32::MAX;
            let mut hi = 0u32;
            for v in a {
                let v = pack_u32(v).ok_or(())?;
                lo = lo.min(v);
                hi = hi.max(v);
            }
            Ok((lo, hi))
        }
        J::Obj(_) => {
            let lo = j.get("min_inclusive").and_then(pack_u32).ok_or(())?;
            let hi = j.get("max_inclusive").and_then(pack_u32).ok_or(())?;
            Ok((lo, hi))
        }
        _ => Err(()),
    }
}

impl PackMcmeta {
    /// Parse `pack.mcmeta` text. `Err` only when the input is not a JSON
    /// object at all; malformed `pack`/`filter` sections surface through
    /// `valid == false` instead of panicking.
    #[cfg_attr(not(test), allow(dead_code))]
    fn parse(json_str: &str) -> Result<PackMcmeta, String> {
        let j = jparse(json_str.as_bytes())
            .ok_or_else(|| "pack.mcmeta is not valid JSON".to_owned())?;
        if !matches!(j, J::Obj(_)) {
            return Err("pack.mcmeta root is not an object".to_owned());
        }
        let mut m = PackMcmeta {
            pack_format: None,
            supported: None,
            min_format: None,
            max_format: None,
            filter: Vec::new(),
            valid: true,
        };
        let Some(pack) = j.get("pack") else {
            m.valid = false;
            return Ok(m);
        };
        m.pack_format = pack.get("pack_format").and_then(pack_u32);
        if pack.get("pack_format").is_some() && m.pack_format.is_none() {
            m.valid = false;
        }
        if let Some(s) = pack.get("supported_formats") {
            match pack_supported_range(s) {
                Ok(range) => m.supported = Some(range),
                Err(()) => m.valid = false,
            }
        }
        if let Some(v) = pack.get("min_format") {
            m.min_format = pack_version(v);
            if m.min_format.is_none() {
                m.valid = false;
            }
        }
        if let Some(v) = pack.get("max_format") {
            m.max_format = pack_version(v);
            if m.max_format.is_none() {
                m.valid = false;
            }
        }
        if let Some(block) = j
            .get("filter")
            .and_then(|f| f.get("block"))
            .and_then(J::as_arr)
        {
            for rule in block {
                // An absent key compiles to vanilla's match-everything `.*`.
                let ns = rule
                    .get("namespace")
                    .map(|v| v.str().to_owned())
                    .unwrap_or_else(|| ".*".to_owned());
                let path = rule
                    .get("path")
                    .map(|v| v.str().to_owned())
                    .unwrap_or_else(|| ".*".to_owned());
                if mc_re_compile(&ns).is_none() || mc_re_compile(&path).is_none() {
                    m.valid = false;
                    continue;
                }
                m.filter.push((ns, path));
            }
        }
        Ok(m)
    }

    /// `true` when `namespace:path` is excluded by some `filter.block` rule.
    #[cfg_attr(not(test), allow(dead_code))]
    fn blocks(&self, namespace: &str, path: &str) -> bool {
        self.filter.iter().any(|(ns, p)| {
            mc_re_match(ns, namespace) && mc_re_match(p, path)
        })
    }
}

/// Validate `pack.mcmeta` JSON against the server's target format. The 26.3
/// tuple scheme wins when the pack declares it; otherwise the scalar
/// `pack_format`/`supported_formats` range is checked. `Err` reports input
/// that is not a JSON object; `Ok(Invalid)` is the fail-closed verdict for
/// malformed or missing pack metadata.
#[cfg_attr(not(test), allow(dead_code))]
fn parse_and_check_pack_mcmeta(
    json_str: &str,
    target_pack_format: u32,
    target_tuple: (u32, u32),
) -> Result<PackFormatVerdict, String> {
    let m = PackMcmeta::parse(json_str)?;
    if !m.valid {
        return Ok(PackFormatVerdict::Invalid);
    }
    if m.min_format.is_some() || m.max_format.is_some() {
        // 26.3: both halves are mandatory; one alone is malformed.
        let (Some(min), Some(max)) = (m.min_format, m.max_format) else {
            return Ok(PackFormatVerdict::Invalid);
        };
        let target = PackVersion(target_tuple.0, target_tuple.1);
        return Ok(if target > max {
            PackFormatVerdict::TooOld { max_supported: max }
        } else if target < min {
            PackFormatVerdict::TooNew { min_supported: min }
        } else {
            PackFormatVerdict::Compatible
        });
    }
    let Some(pack_format) = m.pack_format else {
        return Ok(PackFormatVerdict::Invalid);
    };
    let (lo, hi) = m.supported.unwrap_or((pack_format, pack_format));
    Ok(if target_pack_format > hi {
        PackFormatVerdict::TooOld {
            max_supported: PackVersion(hi, 0),
        }
    } else if target_pack_format < lo {
        PackFormatVerdict::TooNew {
            min_supported: PackVersion(lo, 0),
        }
    } else {
        PackFormatVerdict::Compatible
    })
}

// ---------------------------------------------------------------------------
// Bounded regex subset for `filter.block`. Vanilla compiles each rule's
// `namespace`/`path` strings as `java.util.regex.Pattern` and applies
// `Matcher.matches()` (full-input match). Supported: literals, `.`, `^`, `$`,
// `*`, `+`, `?`, `{m}`, `{m,}`, `{m,n}`, `(...)` groups, `|` alternation,
// `[...]` classes with ranges and `^` negation, `\d \D \w \W \s \S` escapes and
// escaped literals. Compilation and matching are linear — a malicious pack
// cannot hang the matcher.
// ---------------------------------------------------------------------------

/// Thompson NFA instruction for the `filter.block` matcher.
enum McInst {
    Char(char),
    Any,
    Class {
        neg: bool,
        ranges: Vec<(char, char)>,
        /// bit 0 digit, bit 1 word, bit 2 space
        predef: u8,
    },
    /// Zero-width assertion: `true` = `^`, `false` = `$`.
    Assert(bool),
    Split(usize, usize),
    Jmp(usize),
    Match,
}

#[derive(Clone)]
enum McRe {
    Lit(char),
    Any,
    Class {
        neg: bool,
        ranges: Vec<(char, char)>,
        predef: u8,
    },
    Start,
    End,
    Cat(Vec<McRe>),
    Alt(Vec<McRe>),
    Rep(Box<McRe>, u32, Option<u32>),
}

/// Bound on `{m,n}` expansion so a hostile `.{99999}` cannot blow up the
/// compiled program.
const MC_RE_MAX_REP: u32 = 64;
/// Bound on compiled program size (instructions) for the same reason.
const MC_RE_MAX_INSTS: usize = 4096;

struct McReParser<'a> {
    c: &'a [char],
    i: usize,
}

impl<'a> McReParser<'a> {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn alt(&mut self) -> Option<McRe> {
        let mut arms = vec![self.cat()?];
        while self.peek() == Some('|') {
            self.i += 1;
            arms.push(self.cat()?);
        }
        Some(if arms.len() == 1 {
            arms.pop().unwrap()
        } else {
            McRe::Alt(arms)
        })
    }

    fn cat(&mut self) -> Option<McRe> {
        let mut nodes = Vec::new();
        while let Some(ch) = self.peek() {
            if ch == '|' || ch == ')' {
                break;
            }
            nodes.push(self.rep()?);
        }
        Some(if nodes.len() == 1 {
            nodes.pop().unwrap()
        } else {
            McRe::Cat(nodes)
        })
    }

    fn rep(&mut self) -> Option<McRe> {
        let atom = self.atom()?;
        let (lo, hi) = match self.peek() {
            Some('*') => {
                self.i += 1;
                (0, None)
            }
            Some('+') => {
                self.i += 1;
                (1, None)
            }
            Some('?') => {
                self.i += 1;
                (0, Some(1))
            }
            Some('{') => {
                self.i += 1;
                let mut lo = 0u32;
                let mut digits = 0;
                while let Some(c) = self.peek().filter(|c| c.is_ascii_digit()) {
                    self.i += 1;
                    digits += 1;
                    lo = lo.checked_mul(10)?.checked_add(c as u32 - '0' as u32)?;
                }
                if digits == 0 {
                    return None;
                }
                let hi = match self.peek() {
                    Some('}') => {
                        self.i += 1;
                        Some(lo)
                    }
                    Some(',') => {
                        self.i += 1;
                        if self.peek() == Some('}') {
                            self.i += 1;
                            None
                        } else {
                            let mut h = 0u32;
                            let mut hd = 0;
                            while let Some(c) =
                                self.peek().filter(|c| c.is_ascii_digit())
                            {
                                self.i += 1;
                                hd += 1;
                                h = h.checked_mul(10)?.checked_add(c as u32 - '0' as u32)?;
                            }
                            if hd == 0 || self.peek() != Some('}') {
                                return None;
                            }
                            self.i += 1;
                            Some(h)
                        }
                    }
                    _ => return None,
                };
                (lo, hi)
            }
            _ => return Some(atom),
        };
        // Greedy/reluctant `?` suffix is accepted and ignored — matching is
        // set-based so laziness cannot change the result.
        if self.peek() == Some('?') {
            self.i += 1;
        }
        if lo > MC_RE_MAX_REP || hi.is_some_and(|h| h > MC_RE_MAX_REP) {
            return None;
        }
        if hi.is_some_and(|h| h < lo) {
            return None;
        }
        Some(McRe::Rep(Box::new(atom), lo, hi))
    }

    fn class(&mut self) -> Option<McRe> {
        let mut neg = false;
        if self.peek() == Some('^') {
            neg = true;
            self.i += 1;
        }
        let mut ranges = Vec::new();
        let mut predef = 0u8;
        let mut first = true;
        loop {
            let c = self.peek()?;
            if c == ']' && !first {
                self.i += 1;
                return Some(McRe::Class { neg, ranges, predef });
            }
            first = false;
            self.i += 1;
            let lo = if c == '\\' {
                match self.class_escape()? {
                    McRe::Lit(c) => c,
                    McRe::Class {
                        neg: e_neg,
                        predef: e_predef,
                        ..
                    } => {
                        if e_neg {
                            // `\D` inside `[...]` has no clean union form; fail.
                            return None;
                        }
                        predef |= e_predef;
                        continue;
                    }
                    _ => return None,
                }
            } else {
                c
            };
            if self.peek() == Some('-')
                && self.c.get(self.i + 1).is_some_and(|&c| c != ']')
            {
                self.i += 1;
                let hc = self.peek()?;
                self.i += 1;
                let hi = if hc == '\\' {
                    match self.class_escape()? {
                        McRe::Lit(c) => c,
                        _ => return None,
                    }
                } else {
                    hc
                };
                if hi < lo {
                    return None;
                }
                ranges.push((lo, hi));
            } else {
                ranges.push((lo, lo));
            }
        }
    }

    /// `\x` at atom or class level: a predef class or an escaped literal.
    fn class_escape(&mut self) -> Option<McRe> {
        let c = self.peek()?;
        self.i += 1;
        Some(match c {
            'd' => McRe::Class {
                neg: false,
                ranges: Vec::new(),
                predef: 1,
            },
            'D' => McRe::Class {
                neg: true,
                ranges: Vec::new(),
                predef: 1,
            },
            'w' => McRe::Class {
                neg: false,
                ranges: Vec::new(),
                predef: 2,
            },
            'W' => McRe::Class {
                neg: true,
                ranges: Vec::new(),
                predef: 2,
            },
            's' => McRe::Class {
                neg: false,
                ranges: Vec::new(),
                predef: 4,
            },
            'S' => McRe::Class {
                neg: true,
                ranges: Vec::new(),
                predef: 4,
            },
            'n' => McRe::Lit('\n'),
            't' => McRe::Lit('\t'),
            'r' => McRe::Lit('\r'),
            other => McRe::Lit(other),
        })
    }

    fn atom(&mut self) -> Option<McRe> {
        let c = self.peek()?;
        self.i += 1;
        match c {
            '(' => {
                // `(?:...)` non-capturing group prefix is accepted silently.
                if self.peek() == Some('?') {
                    self.i += 1;
                    if self.peek() == Some(':') {
                        self.i += 1;
                    }
                }
                let n = self.alt()?;
                if self.peek() != Some(')') {
                    return None;
                }
                self.i += 1;
                Some(n)
            }
            '[' => self.class(),
            '.' => Some(McRe::Any),
            '^' => Some(McRe::Start),
            '$' => Some(McRe::End),
            '\\' => self.class_escape(),
            ')' | ']' | '}' => None,
            _ => Some(McRe::Lit(c)),
        }
    }
}

/// Append the NFA for `node` to `prog`.
fn mc_re_emit(node: &McRe, prog: &mut Vec<McInst>) -> Option<()> {
    if prog.len() > MC_RE_MAX_INSTS {
        return None;
    }
    match node {
        McRe::Lit(c) => prog.push(McInst::Char(*c)),
        McRe::Any => prog.push(McInst::Any),
        McRe::Class {
            neg,
            ranges,
            predef,
        } => prog.push(McInst::Class {
            neg: *neg,
            ranges: ranges.clone(),
            predef: *predef,
        }),
        McRe::Start => prog.push(McInst::Assert(true)),
        McRe::End => prog.push(McInst::Assert(false)),
        McRe::Cat(nodes) => {
            for n in nodes {
                mc_re_emit(n, prog)?;
            }
        }
        McRe::Alt(arms) => {
            for (idx, arm) in arms.iter().enumerate() {
                if idx + 1 == arms.len() {
                    mc_re_emit(arm, prog)?;
                } else {
                    let split = prog.len();
                    prog.push(McInst::Split(0, 0));
                    let body = prog.len();
                    mc_re_emit(arm, prog)?;
                    let jmp = prog.len();
                    prog.push(McInst::Jmp(0));
                    let next = prog.len();
                    prog[split] = McInst::Split(body, next);
                    prog[jmp] = McInst::Jmp(0); // patched after the last arm
                }
            }
            // Stitch each arm's Jmp to the join point (current end).
            let end = prog.len();
            for (idx, inst) in prog.iter_mut().enumerate().rev() {
                let _ = idx;
                if matches!(inst, McInst::Jmp(0)) {
                    *inst = McInst::Jmp(end);
                }
            }
        }
        McRe::Rep(node, lo, hi) => {
            for _ in 0..*lo {
                mc_re_emit(node, prog)?;
            }
            match hi {
                None => {
                    // `(node)*` tail: L: Split(body, end); body: node; Jmp L.
                    let split = prog.len();
                    prog.push(McInst::Split(0, 0));
                    let body = prog.len();
                    mc_re_emit(node, prog)?;
                    prog.push(McInst::Jmp(split));
                    let end = prog.len();
                    prog[split] = McInst::Split(body, end);
                }
                Some(hi) => {
                    // `(hi - lo)` optional copies, each `Split(body, end); body; end:`.
                    for _ in *lo..*hi {
                        let split = prog.len();
                        prog.push(McInst::Split(0, 0));
                        mc_re_emit(node, prog)?;
                        let end = prog.len();
                        prog[split] = McInst::Split(split + 1, end);
                    }
                }
            }
        }
    }
    Some(())
}

/// Compile a pattern into an NFA program; `None` on a malformed pattern.
fn mc_re_compile(pattern: &str) -> Option<Vec<McInst>> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut parser = McReParser { c: &chars, i: 0 };
    let ast = parser.alt()?;
    if parser.i != chars.len() {
        return None;
    }
    let mut prog = Vec::new();
    mc_re_emit(&ast, &mut prog)?;
    prog.push(McInst::Match);
    Some(prog)
}

fn mc_re_predef(predef: u8, c: char) -> bool {
    (predef & 1 != 0 && c.is_ascii_digit())
        || (predef & 2 != 0 && (c.is_ascii_alphanumeric() || c == '_'))
        || (predef & 4 != 0 && c.is_whitespace())
}

/// Epsilon-closure: mark every consuming state reachable from `pc` at text
/// position `pos`. `Assert` states consume nothing — they gate the path
/// through to `pc + 1`.
fn mc_re_add(prog: &[McInst], states: &mut Vec<bool>, pc: usize, pos: usize, len: usize) {
    let mut stack = vec![pc];
    while let Some(pc) = stack.pop() {
        match &prog[pc] {
            McInst::Split(a, b) => {
                stack.push(*a);
                stack.push(*b);
            }
            McInst::Jmp(t) => stack.push(*t),
            McInst::Assert(start) => {
                if (*start && pos == 0) || (!*start && pos == len) {
                    stack.push(pc + 1);
                }
            }
            _ => {
                if !states[pc] {
                    states[pc] = true;
                }
            }
        }
    }
}

/// `Matcher.matches()` semantics: `true` only when the whole `text` matches.
fn mc_re_prog_match(prog: &[McInst], text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let n = prog.len();
    let mut cur = vec![false; n];
    mc_re_add(prog, &mut cur, 0, 0, chars.len());
    for (i, ch) in chars.iter().enumerate() {
        let mut next = vec![false; n];
        for (pc, live) in cur.iter().enumerate() {
            if !*live {
                continue;
            }
            let hit = match &prog[pc] {
                McInst::Char(c) => *c == *ch,
                McInst::Any => true,
                McInst::Class {
                    neg,
                    ranges,
                    predef,
                } => {
                    let inner = mc_re_predef(*predef, *ch)
                        || ranges.iter().any(|(lo, hi)| *lo <= *ch && *ch <= *hi);
                    inner != *neg
                }
                _ => false,
            };
            if hit {
                mc_re_add(prog, &mut next, pc + 1, i + 1, chars.len());
            }
        }
        cur = next;
    }
    cur[n - 1]
}

/// Compile-then-match convenience used by `PackMcmeta::blocks`. A pattern
/// that fails to compile never blocks a resource.
fn mc_re_match(pattern: &str, text: &str) -> bool {
    mc_re_compile(pattern).is_some_and(|prog| mc_re_prog_match(&prog, text))
}

// ---------------------------------------------------------------------------
// Lane P5g / issue #197 — nested & tag loot entries, conditions, functions.
//
// Vanilla 1.21.1 model, recovered from the server jar bytecode
// (META-INF/versions/1.21.1/server-1.21.1.jar):
//   erv  LootPool: pool conditions once, then `rolls + floor(bonus_rolls *
//        luck)` rolls; EACH roll re-expands every entry into weighted
//        candidates, keeps candidates with effective weight
//        `max(0, floor(weight + quality * luck)) > 0`, picks weighted.
//   esa  composite entries flatten before rolling: `ery` alternatives =
//        first child whose expand succeeds, `esd` group = every child, `esl`
//        sequence = children until the first failure.
//   esm  TagEntry: `expand: true` emits one candidate per tag member (each an
//        equal-weight sibling; entry functions are skipped on those stacks —
//        a vanilla quirk kept for parity), `expand: false` emits one
//        candidate producing every member.
//   esk  NestedLootTable delegates to the referenced table's full evaluation;
//        `erw` LootTable keeps a visited-table set on the context — a cycle
//        logs "Detected infinite loop in loot tables" and yields nothing.
//   esj  singleton candidates' stacks pass through the composed entry
//        functions; pool functions wrap the pool consumer and table functions
//        the table consumer, so every produced stack sees them.
//   euy  survives_explosion: `nextFloat() <= 1/radius`, true without a radius.
//   eug  furnace_smelt: transmutes to the smelt result keeping the count.
//   esp  explosion_decay: each item survives with `nextFloat() <= 1/radius`.
//   etx  set_damage: `floor((1 - clamp(provider + add_base,0,1)) * max_damage)`
//        with `add_base = 1 - damage/max` when `add` is set.
//   eso  apply_bonus ore_drops: `count * (max(0, nextInt(level+2) - 1) + 1)`;
//        uniform_bonus_count: `count + nextInt(level * bonusMultiplier + 1)`.
// ---------------------------------------------------------------------------

/// `evy`/`daj` number provider: a constant, a uniform range, or a
/// level-based value (`linear`, `clamped`, `fraction`, `levels_squared`)
/// evaluated at an enchantment level.
#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
enum LootNum {
    Constant(f64),
    Uniform(f64, f64),
    Linear { base: f64, per_level: f64 },
    Clamped(Box<LootNum>, f64, f64),
    Fraction(Box<LootNum>, Box<LootNum>),
    LevelsSquared(f64),
    /// An unrecognized provider — fail closed as zero.
    Unsupported,
}

impl LootNum {
    fn f32(&self, ctx: &mut LootEvalContext, level: i32) -> f32 {
        match self {
            LootNum::Constant(v) => *v as f32,
            LootNum::Uniform(lo, hi) => (*lo + (*hi - *lo) * ctx.rng.next_f32() as f64) as f32,
            LootNum::Linear { base, per_level } => {
                (*base + *per_level * (level - 1) as f64) as f32
            }
            LootNum::Clamped(inner, lo, hi) => {
                inner.f32(ctx, level).clamp(*lo as f32, *hi as f32)
            }
            LootNum::Fraction(num, den) => {
                let d = den.f32(ctx, level);
                if d == 0.0 {
                    0.0
                } else {
                    num.f32(ctx, level) / d
                }
            }
            LootNum::LevelsSquared(added) => (level * level) as f32 + *added as f32,
            LootNum::Unsupported => 0.0,
        }
    }

    /// `evy.a(LootContext)`: integral uniform bounds use the
    /// `nextInt(max - min + 1) + min` draw; fractional bounds floor a float.
    fn i32(&self, ctx: &mut LootEvalContext, level: i32) -> i32 {
        match self {
            LootNum::Uniform(lo, hi) if lo.fract() == 0.0 && hi.fract() == 0.0 => {
                let (lo, hi) = (*lo as i64, *hi as i64);
                if lo >= hi {
                    lo as i32
                } else {
                    (ctx.rng.next_bound((hi - lo + 1).min(i32::MAX as i64) as i32) as i64 + lo)
                        as i32
                }
            }
            _ => self.f32(ctx, level).floor() as i32,
        }
    }
}

/// Number provider parse: scalar, `{"type":"uniform","min","max"}`,
/// `{"type":"constant","value"}`, `{"type":"linear",...}`, `clamped`,
/// `fraction`, `levels_squared`, or a `{min,max}` shorthand for uniform.
fn loot_num(j: Option<&J>, default: LootNum) -> LootNum {
    let Some(j) = j else { return default };
    match j {
        J::Num(n) => LootNum::Constant(*n),
        J::Obj(_) => {
            let bare = |k: &str| j.get(k).map(|v| v.num()).unwrap_or(0.0);
            match j
                .get("type")
                .map(|v| v.str().trim_start_matches("minecraft:").to_owned())
                .unwrap_or_default()
                .as_str()
            {
                "uniform" => LootNum::Uniform(bare("min"), bare("max")),
                "constant" => LootNum::Constant(bare("value")),
                "linear" => LootNum::Linear {
                    base: bare("base"),
                    per_level: bare("per_level_above_first"),
                },
                "clamped" => LootNum::Clamped(
                    Box::new(loot_num(j.get("value"), LootNum::Unsupported)),
                    bare("min"),
                    bare("max"),
                ),
                "fraction" => LootNum::Fraction(
                    Box::new(loot_num(j.get("numerator"), LootNum::Unsupported)),
                    Box::new(loot_num(j.get("denominator"), LootNum::Unsupported)),
                ),
                "levels_squared" => LootNum::LevelsSquared(bare("added")),
                _ if j.get("min").is_some() || j.get("max").is_some() => {
                    LootNum::Uniform(bare("min"), bare("max"))
                }
                _ => LootNum::Unsupported,
            }
        }
        _ => LootNum::Unsupported,
    }
}

/// `true`/`1` JSON boolean.
fn loot_jbool(j: &J, key: &str) -> bool {
    match j.get(key) {
        Some(J::Num(n)) => *n != 0.0,
        Some(J::Str(s)) => s == "true",
        _ => false,
    }
}

/// One `match_tool` `items` element: a plain id or a `#tag`.
#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
enum LootItemMatcher {
    Id(String),
    Tag(String),
}

/// One `match_tool` enchantment predicate: ids set + optional level bounds.
#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct MatchToolEnchant {
    ids: Vec<String>,
    min: Option<i32>,
    max: Option<i32>,
}

/// The implemented `minecraft:` loot conditions (`evc` registry subset).
/// `Unsupported` is the fail-closed bucket: an unknown condition type can
/// never turn a drop on.
#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
enum LootCondition {
    SurvivesExplosion,
    RandomChance(LootNum),
    RandomChanceEnchantedBonus {
        enchantment: String,
        unenchanted: f32,
        enchanted: LootNum,
    },
    KilledByPlayer,
    Inverted(Box<LootCondition>),
    AnyOf(Vec<LootCondition>),
    AllOf(Vec<LootCondition>),
    MatchTool {
        items: Option<Vec<LootItemMatcher>>,
        enchantments: Vec<MatchToolEnchant>,
    },
    Unsupported,
}

/// `HolderSet` spelling: `"id"`, `"#tag"`, or a list of ids (tags allowed).
fn loot_item_matchers(j: Option<&J>) -> Option<Vec<LootItemMatcher>> {
    let j = j?;
    let push = |v: &J, out: &mut Vec<LootItemMatcher>| {
        let s = v.str();
        if let Some(t) = s.strip_prefix('#') {
            out.push(LootItemMatcher::Tag(t.to_owned()));
        } else if !s.is_empty() {
            out.push(LootItemMatcher::Id(s.to_owned()));
        }
    };
    let mut out = Vec::new();
    match j {
        J::Str(_) => push(j, &mut out),
        J::Arr(a) => {
            for v in a {
                push(v, &mut out);
            }
        }
        _ => return None,
    }
    Some(out)
}

/// `{"enchantments": id|[ids], "levels": n|{"min","max"}}` entries.
fn loot_match_tool_enchants(j: Option<&J>) -> Vec<MatchToolEnchant> {
    let mut out = Vec::new();
    let Some(list) = j.and_then(J::as_arr) else {
        return out;
    };
    for e in list {
        let mut ids = Vec::new();
        match e.get("enchantments").or_else(|| e.get("enchantment")) {
            Some(J::Str(s)) => ids.push(s.clone()),
            Some(J::Arr(a)) => {
                ids.extend(a.iter().map(|v| v.str().to_owned()))
            }
            _ => {}
        }
        if ids.is_empty() {
            continue;
        }
        let (mut min, mut max) = (None, None);
        match e.get("levels") {
            Some(J::Num(n)) => {
                let n = *n as i32;
                min = Some(n);
                max = Some(n);
            }
            Some(J::Obj(_)) => {
                let lv = e.get("levels").unwrap();
                min = lv.get("min").map(|v| v.num() as i32);
                max = lv.get("max").map(|v| v.num() as i32);
            }
            _ => {}
        }
        out.push(MatchToolEnchant { ids, min, max });
    }
    out
}

fn loot_condition(j: &J) -> LootCondition {
    let id = j
        .get("condition")
        .map(|v| v.str().trim_start_matches("minecraft:").to_owned())
        .unwrap_or_default();
    match id.as_str() {
        "survives_explosion" => LootCondition::SurvivesExplosion,
        "random_chance" => {
            LootCondition::RandomChance(loot_num(j.get("chance"), LootNum::Unsupported))
        }
        "random_chance_with_enchanted_bonus" => LootCondition::RandomChanceEnchantedBonus {
            enchantment: j
                .get("enchantment")
                .map(|v| v.str().to_owned())
                .unwrap_or_default(),
            unenchanted: j
                .get("unenchanted_chance")
                .or_else(|| j.get("base_chance"))
                .map(|v| v.num() as f32)
                .unwrap_or(0.0),
            enchanted: loot_num(j.get("enchanted_chance"), LootNum::Unsupported),
        },
        "killed_by_player" => LootCondition::KilledByPlayer,
        "inverted" => j
            .get("term")
            .map(|t| LootCondition::Inverted(Box::new(loot_condition(t))))
            .unwrap_or(LootCondition::Unsupported),
        "any_of" => j
            .get("terms")
            .and_then(J::as_arr)
            .map(|ts| LootCondition::AnyOf(ts.iter().map(loot_condition).collect()))
            .unwrap_or(LootCondition::Unsupported),
        "all_of" => j
            .get("terms")
            .and_then(J::as_arr)
            .map(|ts| LootCondition::AllOf(ts.iter().map(loot_condition).collect()))
            .unwrap_or(LootCondition::Unsupported),
        "match_tool" => {
            let pred = j.get("predicate").cloned().unwrap_or(J::Obj(Vec::new()));
            LootCondition::MatchTool {
                items: loot_item_matchers(
                    pred.get("items").or_else(|| pred.get("item")),
                ),
                enchantments: loot_match_tool_enchants(pred.get("enchantments")),
            }
        }
        _ => LootCondition::Unsupported,
    }
}

fn loot_conditions(j: Option<&J>) -> Vec<LootCondition> {
    j.and_then(J::as_arr)
        .map(|list| list.iter().map(loot_condition).collect())
        .unwrap_or_default()
}

/// `apply_bonus` formula id + parameters.
#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
enum ApplyBonusFormula {
    OreDrops,
    UniformBonusCount { bonus_multiplier: i32 },
    BinomialWithBonusCount { extra: i32, probability: f32 },
}

/// The implemented `minecraft:` loot functions (`eth` registry subset).
/// `Unsupported` functions are parse-time kept but evaluation no-ops.
#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
enum LootFunction {
    SetCount { provider: LootNum, add: bool },
    ApplyBonus { enchantment: String, formula: ApplyBonusFormula },
    ExplosionDecay,
    FurnaceSmelt,
    LimitCount { min: Option<LootNum>, max: Option<LootNum> },
    SetDamage { provider: LootNum, add: bool },
    /// `minecraft:sequence` — runs each child function in order.
    Sequence(Vec<LootFuncNode>),
    Unsupported,
}

/// A function plus the `conditions` array gating it.
#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct LootFuncNode {
    func: LootFunction,
    conditions: Vec<LootCondition>,
}

fn loot_function(j: &J) -> LootFunction {
    let id = j
        .get("function")
        .map(|v| v.str().trim_start_matches("minecraft:").to_owned())
        .unwrap_or_default();
    match id.as_str() {
        "set_count" => LootFunction::SetCount {
            provider: loot_num(j.get("count"), LootNum::Constant(0.0)),
            add: loot_jbool(j, "add"),
        },
        "apply_bonus" => {
            let params = j.get("parameters");
            let formula = match j
                .get("formula")
                .map(|v| v.str().trim_start_matches("minecraft:").to_owned())
                .unwrap_or_default()
                .as_str()
            {
                "ore_drops" => ApplyBonusFormula::OreDrops,
                "uniform_bonus_count" => ApplyBonusFormula::UniformBonusCount {
                    bonus_multiplier: params
                        .and_then(|p| p.get("bonusMultiplier"))
                        .map(|v| v.num() as i32)
                        .unwrap_or(0),
                },
                "binomial_with_bonus_count" => ApplyBonusFormula::BinomialWithBonusCount {
                    extra: params
                        .and_then(|p| p.get("extra"))
                        .map(|v| v.num() as i32)
                        .unwrap_or(0),
                    probability: params
                        .and_then(|p| p.get("probability"))
                        .map(|v| v.num() as f32)
                        .unwrap_or(0.0),
                },
                _ => return LootFunction::Unsupported,
            };
            LootFunction::ApplyBonus {
                enchantment: j
                    .get("enchantment")
                    .map(|v| v.str().to_owned())
                    .unwrap_or_default(),
                formula,
            }
        }
        "explosion_decay" => LootFunction::ExplosionDecay,
        "furnace_smelt" => LootFunction::FurnaceSmelt,
        "limit_count" => {
            let (mut min, mut max) = (None, None);
            match j.get("limit") {
                Some(J::Num(_)) => {
                    let v = loot_num(j.get("limit"), LootNum::Unsupported);
                    min = Some(v.clone());
                    max = Some(v);
                }
                Some(J::Obj(_)) => {
                    let limit = j.get("limit").unwrap();
                    min = limit.get("min").map(|v| loot_num(Some(v), LootNum::Unsupported));
                    max = limit.get("max").map(|v| loot_num(Some(v), LootNum::Unsupported));
                }
                _ => {}
            }
            LootFunction::LimitCount { min, max }
        }
        "set_damage" => LootFunction::SetDamage {
            provider: loot_num(j.get("damage"), LootNum::Constant(0.0)),
            add: loot_jbool(j, "add"),
        },
        "sequence" => LootFunction::Sequence(loot_func_nodes(j.get("functions"))),
        _ => LootFunction::Unsupported,
    }
}

fn loot_func_nodes(j: Option<&J>) -> Vec<LootFuncNode> {
    j.and_then(J::as_arr)
        .map(|list| {
            list.iter()
                .map(|f| LootFuncNode {
                    func: loot_function(f),
                    conditions: loot_conditions(f.get("conditions")),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `esf` LootPoolEntries: every kind a pool or composite child can be.
#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
enum LootEntryKind {
    Item,
    /// `expand`: the vanilla two-mode tag entry (see `esm`).
    Tag { expand: bool },
    /// `minecraft:loot_table` — NestedLootTable, resolved through
    /// `loot_table_json` with the context's visited-set cycle guard.
    Table,
    Alternatives,
    Group,
    Sequence,
    Empty,
}

/// One parsed pool/composite entry.
#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct LootEntryNode {
    kind: LootEntryKind,
    /// Item id, tag id, or nested table id depending on `kind`.
    name: String,
    weight: i32,
    quality: i32,
    conditions: Vec<LootCondition>,
    functions: Vec<LootFuncNode>,
    children: Vec<LootEntryNode>,
}

fn loot_entry_node(j: &J) -> LootEntryNode {
    let ty = j
        .get("type")
        .map(|v| v.str().trim_start_matches("minecraft:").to_owned())
        .unwrap_or_default();
    let kind = match ty.as_str() {
        "item" => LootEntryKind::Item,
        "tag" => LootEntryKind::Tag {
            expand: loot_jbool(j, "expand"),
        },
        "loot_table" => LootEntryKind::Table,
        "alternatives" => LootEntryKind::Alternatives,
        "group" => LootEntryKind::Group,
        "sequence" => LootEntryKind::Sequence,
        // `empty`, `dynamic`, `slot` and unknown kinds: never emit.
        _ => LootEntryKind::Empty,
    };
    LootEntryNode {
        kind,
        name: j
            .get("name")
            .map(|v| v.str().to_owned())
            .unwrap_or_default(),
        weight: j.get("weight").map(|v| v.num() as i32).unwrap_or(1),
        quality: j.get("quality").map(|v| v.num() as i32).unwrap_or(0),
        conditions: loot_conditions(j.get("conditions")),
        functions: loot_func_nodes(j.get("functions")),
        children: j
            .get("children")
            .and_then(J::as_arr)
            .map(|list| list.iter().map(loot_entry_node).collect())
            .unwrap_or_default(),
    }
}

struct LootPoolDef {
    rolls: LootNum,
    bonus_rolls: LootNum,
    conditions: Vec<LootCondition>,
    entries: Vec<LootEntryNode>,
    functions: Vec<LootFuncNode>,
}

struct LootTableDef {
    pools: Vec<LootPoolDef>,
    functions: Vec<LootFuncNode>,
}

fn loot_table_def(j: &J) -> LootTableDef {
    LootTableDef {
        pools: j
            .get("pools")
            .and_then(J::as_arr)
            .map(|list| {
                list.iter()
                    .map(|p| LootPoolDef {
                        rolls: loot_num(p.get("rolls"), LootNum::Constant(1.0)),
                        bonus_rolls: loot_num(p.get("bonus_rolls"), LootNum::Constant(0.0)),
                        conditions: loot_conditions(p.get("conditions")),
                        entries: p
                            .get("entries")
                            .and_then(J::as_arr)
                            .map(|list| list.iter().map(loot_entry_node).collect())
                            .unwrap_or_default(),
                        functions: loot_func_nodes(p.get("functions")),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        functions: loot_func_nodes(j.get("functions")),
    }
}

/// One stack produced by the evaluator.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct LootDrop {
    /// Namespaced item id (`minecraft:diamond`).
    item: String,
    count: i32,
    /// Damage value; `0` is undamaged.
    damage: i32,
}

/// The tool the roll attributes `match_tool` / `enchantment` lookups to
/// (vanilla's `TOOL` loot-context parameter).
#[derive(Clone, Debug, Default)]
#[cfg_attr(not(test), allow(dead_code))]
struct LootTool {
    /// Item id, any namespace spelling.
    item: String,
    /// Enchantment id → level; both `silk_touch` and `minecraft:silk_touch`
    /// spellings resolve.
    enchantments: std::collections::BTreeMap<String, i32>,
}

/// Non-parser state for one `evaluate_loot_table*` call. `rng` is Java's
/// `java.util.Random` stream (`crate::generation_math::Lcg`), so the same
/// seed replays the same draws.
#[cfg_attr(not(test), allow(dead_code))]
struct LootEvalContext {
    rng: crate::generation_math::Lcg,
    /// `luck` attribute (entity/generic.luck) feeding `quality` and
    /// `bonus_rolls`.
    luck: f32,
    killed_by_player: bool,
    tool: Option<LootTool>,
    /// `EXPLOSION_RADIUS` loot-context parameter; `None` means no explosion.
    explosion_radius: Option<f32>,
    depth: usize,
    visiting: HashSet<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl LootEvalContext {
    fn seeded(seed: u64) -> Self {
        LootEvalContext {
            rng: crate::generation_math::Lcg::new(seed),
            luck: 0.0,
            killed_by_player: false,
            tool: None,
            explosion_radius: None,
            depth: 0,
            visiting: HashSet::new(),
        }
    }

    /// Enchantment level on the tool, both `id` and `minecraft:id` spellings.
    fn tool_level(&self, id: &str) -> i32 {
        let Some(tool) = &self.tool else {
            return 0;
        };
        let bare = id.strip_prefix("minecraft:").unwrap_or(id);
        tool.enchantments
            .get(id)
            .or_else(|| tool.enchantments.get(bare))
            .copied()
            .unwrap_or(0)
    }
}

/// `ns:name` normalization: a bare path reads as `minecraft:`.
fn loot_norm_id(id: &str) -> String {
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

/// Expanded member list of an item tag via the datapack registry, normalized
/// to namespaced ids.
fn loot_item_tag_members(tag: &str) -> Vec<String> {
    datapack_tag_members("item", &loot_norm_id(tag))
        .into_iter()
        .map(|m| loot_norm_id(&m))
        .collect()
}

/// `#tag` membership test that accepts both the builtin bare members and the
/// datapack-loaded namespaced ones.
fn loot_item_in_tag(tag: &str, item: &str) -> bool {
    let tag = loot_norm_id(tag);
    let item = loot_norm_id(item);
    datapack_tag_contains("item", &tag, &item)
        || item
            .strip_prefix("minecraft:")
            .is_some_and(|bare| datapack_tag_contains("item", &tag, bare))
}

fn loot_match_tool(items: &Option<Vec<LootItemMatcher>>, enchants: &[MatchToolEnchant], ctx: &LootEvalContext) -> bool {
    // Vanilla `evj`: no TOOL parameter means the condition fails even with an
    // empty predicate.
    let Some(tool) = &ctx.tool else {
        return false;
    };
    if let Some(matchers) = items {
        let tool_item = loot_norm_id(&tool.item);
        if !matchers.iter().any(|m| match m {
            LootItemMatcher::Id(id) => loot_norm_id(id) == tool_item,
            LootItemMatcher::Tag(tag) => loot_item_in_tag(tag, &tool_item),
        }) {
            return false;
        }
    }
    enchants.iter().all(|e| {
        let lvl = e
            .ids
            .iter()
            .map(|id| ctx.tool_level(id))
            .max()
            .unwrap_or(0);
        lvl > 0 && e.min.is_none_or(|m| lvl >= m) && e.max.is_none_or(|m| lvl <= m)
    })
}

fn loot_condition_holds(c: &LootCondition, ctx: &mut LootEvalContext) -> bool {
    match c {
        LootCondition::SurvivesExplosion => match ctx.explosion_radius {
            Some(r) => ctx.rng.next_f32() <= 1.0 / r.max(1.0e-6),
            None => true,
        },
        LootCondition::RandomChance(provider) => {
            let chance = provider.f32(ctx, 0);
            ctx.rng.next_f32() < chance
        }
        LootCondition::RandomChanceEnchantedBonus {
            enchantment,
            unenchanted,
            enchanted,
        } => {
            let level = ctx.tool_level(enchantment);
            let chance = if level > 0 {
                enchanted.f32(ctx, level)
            } else {
                *unenchanted
            };
            ctx.rng.next_f32() < chance
        }
        LootCondition::KilledByPlayer => ctx.killed_by_player,
        LootCondition::Inverted(term) => !loot_condition_holds(term, ctx),
        LootCondition::AnyOf(terms) => terms.iter().any(|t| loot_condition_holds(t, ctx)),
        LootCondition::AllOf(terms) => terms.iter().all(|t| loot_condition_holds(t, ctx)),
        LootCondition::MatchTool { items, enchantments } => {
            loot_match_tool(items, enchantments, ctx)
        }
        LootCondition::Unsupported => false,
    }
}

fn loot_conditions_pass(conditions: &[LootCondition], ctx: &mut LootEvalContext) -> bool {
    conditions.iter().all(|c| loot_condition_holds(c, ctx))
}

/// A weighted candidate emitted by `loot_expand_entry` (`esg` LootPoolEntry).
/// `member` is set only for `expand: true` tag members — those emit their
/// item directly and skip the entry's functions (the `esm$1` quirk).
struct LootCandidate<'a> {
    node: &'a LootEntryNode,
    member: Option<String>,
}

/// `esh.expand`: conditions first, then emit candidates through the consumer.
/// Returns whether the container expanded (what alternatives/sequence read).
fn loot_expand_entry<'a>(
    node: &'a LootEntryNode,
    ctx: &mut LootEvalContext,
    out: &mut Vec<LootCandidate<'a>>,
) -> bool {
    if !loot_conditions_pass(&node.conditions, ctx) {
        return false;
    }
    match &node.kind {
        LootEntryKind::Empty | LootEntryKind::Item | LootEntryKind::Table => {
            out.push(LootCandidate {
                node,
                member: None,
            });
            true
        }
        LootEntryKind::Tag { expand: false } => {
            out.push(LootCandidate {
                node,
                member: None,
            });
            true
        }
        LootEntryKind::Tag { expand: true } => {
            for member in loot_item_tag_members(&node.name) {
                out.push(LootCandidate {
                    node,
                    member: Some(member),
                });
            }
            true
        }
        LootEntryKind::Alternatives => node
            .children
            .iter()
            .any(|c| loot_expand_entry(c, ctx, out)),
        LootEntryKind::Group => {
            for c in &node.children {
                loot_expand_entry(c, ctx, out);
            }
            true
        }
        LootEntryKind::Sequence => node
            .children
            .iter()
            .all(|c| loot_expand_entry(c, ctx, out)),
    }
}

/// `esj$c.a(F)`: effective weight `max(0, floor(weight + quality * luck))`.
fn loot_eff_weight(node: &LootEntryNode, luck: f32) -> i32 {
    (node.weight as f32 + node.quality as f32 * luck)
        .floor()
        .max(0.0) as i32
}

/// `furnace_smelt`: the first smelting recipe whose ingredient matches.
fn loot_smelt_result(item: &str) -> Option<String> {
    let bare = item.strip_prefix("minecraft:").unwrap_or(item);
    let item_id = name_to_item(bare)?;
    json_smelts().lock().unwrap().iter().find_map(|(ingr, out, _, _)| {
        let hit = match ingr {
            Ingr::Item(id) => *id == item_id,
            Ingr::Tag(tag) => loot_item_in_tag(tag, item),
        };
        hit.then(|| item_to_name(*out).map(|n| loot_norm_id(n)))
            .flatten()
    })
}

fn loot_apply_func_nodes(drop: &mut LootDrop, nodes: &[LootFuncNode], ctx: &mut LootEvalContext) {
    for node in nodes {
        if loot_conditions_pass(&node.conditions, ctx) {
            loot_apply_function(drop, &node.func, ctx);
        }
    }
}

fn loot_apply_function(drop: &mut LootDrop, func: &LootFunction, ctx: &mut LootEvalContext) {
    match func {
        LootFunction::SetCount { provider, add } => {
            let v = provider.i32(ctx, 0);
            drop.count = if *add { drop.count + v } else { v };
        }
        LootFunction::ApplyBonus { enchantment, formula } => {
            let level = ctx.tool_level(enchantment).clamp(0, 1 << 20);
            drop.count = match formula {
                ApplyBonusFormula::OreDrops => {
                    // Draws even at level 0 (vanilla calls nextInt unconditionally).
                    let roll = ctx.rng.next_bound(level + 2) - 1;
                    drop.count * (roll.max(0) + 1)
                }
                ApplyBonusFormula::UniformBonusCount { bonus_multiplier } => {
                    drop.count + ctx.rng.next_bound(level * bonus_multiplier + 1)
                }
                ApplyBonusFormula::BinomialWithBonusCount { extra, probability } => {
                    let mut count = drop.count;
                    for _ in 0..(level + extra).max(0) {
                        if ctx.rng.next_f32() < *probability {
                            count += 1;
                        }
                    }
                    count
                }
            };
        }
        LootFunction::ExplosionDecay => {
            if let Some(radius) = ctx.explosion_radius {
                let p = 1.0 / radius.max(1.0e-6);
                let mut keep = 0;
                for _ in 0..drop.count.max(0) {
                    if ctx.rng.next_f32() <= p {
                        keep += 1;
                    }
                }
                drop.count = keep;
            }
        }
        LootFunction::FurnaceSmelt => {
            if let Some(item) = loot_smelt_result(&drop.item) {
                drop.item = item;
            }
        }
        LootFunction::LimitCount { min, max } => {
            let lo = min
                .as_ref()
                .map(|p| p.i32(ctx, 0))
                .unwrap_or(i32::MIN);
            let hi = max
                .as_ref()
                .map(|p| p.i32(ctx, 0))
                .unwrap_or(i32::MAX);
            drop.count = drop.count.clamp(lo, hi);
        }
        LootFunction::SetDamage { provider, add } => {
            let Some(id) = name_to_item(&drop.item) else {
                return;
            };
            let Some(max_damage) = tool_max_damage(id).or_else(|| armor_max_damage(id))
            else {
                return;
            };
            if max_damage <= 0 {
                return;
            }
            let base = if *add {
                1.0 - drop.damage as f32 / max_damage as f32
            } else {
                0.0
            };
            let requested = provider.f32(ctx, 0) + base;
            drop.damage = ((1.0 - requested.clamp(0.0, 1.0)) * max_damage as f32).floor() as i32;
        }
        LootFunction::Sequence(children) => loot_apply_func_nodes(drop, children, ctx),
        LootFunction::Unsupported => {}
    }
}

/// `esj$1.a`: the picked candidate emits stacks through the entry's composed
/// functions (except expanded tag members, which skip them).
fn loot_candidate_generate(
    candidate: &LootCandidate,
    ctx: &mut LootEvalContext,
    out: &mut Vec<LootDrop>,
) {
    if let Some(member) = &candidate.member {
        out.push(LootDrop {
            item: member.clone(),
            count: 1,
            damage: 0,
        });
        return;
    }
    let node = candidate.node;
    match &node.kind {
        LootEntryKind::Item => {
            let mut drop = LootDrop {
                item: loot_norm_id(&node.name),
                count: 1,
                damage: 0,
            };
            loot_apply_func_nodes(&mut drop, &node.functions, ctx);
            if drop.count > 0 {
                out.push(drop);
            }
        }
        LootEntryKind::Tag { expand: false } => {
            for member in loot_item_tag_members(&node.name) {
                let mut drop = LootDrop {
                    item: member,
                    count: 1,
                    damage: 0,
                };
                loot_apply_func_nodes(&mut drop, &node.functions, ctx);
                if drop.count > 0 {
                    out.push(drop);
                }
            }
        }
        LootEntryKind::Table => {
            for drop in evaluate_loot_table(&node.name, ctx) {
                let mut drop = drop;
                loot_apply_func_nodes(&mut drop, &node.functions, ctx);
                if drop.count > 0 {
                    out.push(drop);
                }
            }
        }
        _ => {}
    }
}

/// `erv.b` — ONE roll: expand every entry (conditions evaluated now), keep
/// positive-weight candidates, weighted pick.
fn loot_pool_roll<'a>(pool: &'a LootPoolDef, ctx: &mut LootEvalContext, out: &mut Vec<LootDrop>) {
    let mut candidates: Vec<LootCandidate<'a>> = Vec::new();
    for entry in &pool.entries {
        loot_expand_entry(entry, ctx, &mut candidates);
    }
    candidates.retain(|c| loot_eff_weight(c.node, ctx.luck) > 0);
    if candidates.is_empty() {
        return;
    }
    if candidates.len() == 1 {
        loot_candidate_generate(&candidates[0], ctx, out);
        return;
    }
    let total: i32 = candidates
        .iter()
        .map(|c| loot_eff_weight(c.node, ctx.luck))
        .sum();
    let mut pick = ctx.rng.next_bound(total);
    for candidate in &candidates {
        pick -= loot_eff_weight(candidate.node, ctx.luck);
        if pick < 0 {
            loot_candidate_generate(candidate, ctx, out);
            return;
        }
    }
}

fn loot_eval_table_def(def: &LootTableDef, ctx: &mut LootEvalContext) -> Vec<LootDrop> {
    let mut out = Vec::new();
    for pool in &def.pools {
        // `erv.a`: pool conditions gate the whole pool, then
        // `rolls + floor(bonus_rolls * luck)` independent rolls.
        if !loot_conditions_pass(&pool.conditions, ctx) {
            continue;
        }
        let rolls = pool.rolls.i32(ctx, 0)
            + (pool.bonus_rolls.f32(ctx, 0) * ctx.luck).floor() as i32;
        let mut produced = Vec::new();
        for _ in 0..rolls.max(0) {
            loot_pool_roll(pool, ctx, &mut produced);
        }
        for mut drop in produced {
            loot_apply_func_nodes(&mut drop, &pool.functions, ctx);
            if drop.count > 0 {
                out.push(drop);
            }
        }
    }
    for drop in out.iter_mut() {
        loot_apply_func_nodes(drop, &def.functions, ctx);
    }
    out.retain(|d| d.count > 0);
    out
}

/// The nesting/cycle guard for `minecraft:loot_table` entries: the context's
/// visited set mirrors vanilla's "Detected infinite loop" skip, and the depth
/// cap stops non-cyclic runaway nesting.
const LOOT_EVAL_MAX_DEPTH: usize = 64;

/// Evaluate the loot table `id` (a datapack `loot_table` JSON looked up via
/// `loot_table_json`; a bare path reads as `minecraft:`). Unknown tables and
/// cyclic references evaluate to nothing — never panics.
#[allow(dead_code)]
fn evaluate_loot_table(id: &str, ctx: &mut LootEvalContext) -> Vec<LootDrop> {
    let id = loot_norm_id(id);
    if ctx.depth >= LOOT_EVAL_MAX_DEPTH || !ctx.visiting.insert(id.clone()) {
        return Vec::new();
    }
    ctx.depth += 1;
    let out = loot_table_json(&id)
        .and_then(|body| jparse(body.as_bytes()))
        .map(|j| loot_eval_table_def(&loot_table_def(&j), ctx))
        .unwrap_or_default();
    ctx.depth -= 1;
    ctx.visiting.remove(&id);
    out
}

/// Evaluate a loot-table JSON body directly (same pipeline as
/// `evaluate_loot_table`, but the table need not come from a datapack).
#[allow(dead_code)]
fn evaluate_loot_table_json(json_str: &str, ctx: &mut LootEvalContext) -> Vec<LootDrop> {
    jparse(json_str.as_bytes())
        .map(|j| loot_eval_table_def(&loot_table_def(&j), ctx))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Lane P5g / issue #198 — data-driven dynamic registries.
//
// Scans `data/<ns>/<registry>/*.json` for the seven 1.21.1 datapack registries
// (enchantment, damage_type, chat_type, wolf_variant, painting_variant,
// banner_pattern, jukebox_song). BTreeMap storage gives the deterministic
// `ns:path` ordering a configuration `0x07` RegistryData sync needs, and
// later packs override earlier ones like every other datapack map here.
// ---------------------------------------------------------------------------

/// `data/<ns>/<dir>` → the registry name, accepting both singular and the
/// pre-1.21 plural spellings datapacks carry.
fn dyn_registry_kind(dir: &str) -> Option<&'static str> {
    Some(match dir {
        "enchantment" | "enchantments" => "enchantment",
        "damage_type" | "damage_types" => "damage_type",
        "chat_type" | "chat_types" => "chat_type",
        "wolf_variant" | "wolf_variants" => "wolf_variant",
        "painting_variant" | "painting_variants" => "painting_variant",
        "banner_pattern" | "banner_patterns" => "banner_pattern",
        "jukebox_song" | "jukebox_songs" => "jukebox_song",
        _ => return None,
    })
}

/// A text component: `{text}` / `{translate}` object or a bare string.
fn dyn_jtext(j: &J, key: &str) -> String {
    match j.get(key) {
        Some(J::Obj(_)) => j
            .get(key)
            .and_then(|d| {
                d.get("text")
                    .or_else(|| d.get("translate"))
                    .map(|v| v.str().to_owned())
            })
            .unwrap_or_default(),
        Some(v) => v.str().to_owned(),
        None => String::new(),
    }
}

/// A field that is a bare string or a list of strings (objects contribute
/// their `id`, the tag-entry `required:false` form datapacks reuse here).
fn dyn_jstr_list(j: &J, key: &str) -> Vec<String> {
    match j.get(key) {
        Some(J::Str(s)) => vec![s.clone()],
        Some(J::Arr(a)) => a
            .iter()
            .map(|v| match v {
                J::Str(s) => s.clone(),
                J::Obj(_) => v
                    .get("id")
                    .map(|i| i.str().to_owned())
                    .unwrap_or_default(),
                _ => String::new(),
            })
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// A `chat_type` decoration object (`{translation_key, parameters}`).
#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct ChatDecoration {
    translation_key: String,
    parameters: Vec<String>,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct DatapackEnchantment {
    raw: String,
    description: String,
    supported_items: Vec<String>,
    weight: i64,
    max_level: i64,
    anvil_cost: i64,
    slots: Vec<String>,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct DatapackDamageType {
    raw: String,
    message_id: String,
    exhaustion: f64,
    scaling: String,
    effects: Option<String>,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct DatapackChatType {
    raw: String,
    chat: Option<ChatDecoration>,
    narration: Option<ChatDecoration>,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct DatapackWolfVariant {
    raw: String,
    wild_texture: String,
    tame_texture: String,
    angry_texture: String,
    biomes: Vec<String>,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct DatapackPaintingVariant {
    raw: String,
    asset_id: String,
    width: i64,
    height: i64,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct DatapackBannerPattern {
    raw: String,
    asset_id: String,
    translation_key: String,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct DatapackJukeboxSong {
    raw: String,
    sound_event: String,
    description: String,
    length_in_seconds: f64,
    comparator_output: i64,
}

/// The seven data-driven registries, each keyed `ns:path` in sorted order.
#[derive(Default)]
#[cfg_attr(not(test), allow(dead_code))]
struct DynamicDataRegistries {
    enchantment: std::collections::BTreeMap<String, DatapackEnchantment>,
    damage_type: std::collections::BTreeMap<String, DatapackDamageType>,
    chat_type: std::collections::BTreeMap<String, DatapackChatType>,
    wolf_variant: std::collections::BTreeMap<String, DatapackWolfVariant>,
    painting_variant: std::collections::BTreeMap<String, DatapackPaintingVariant>,
    banner_pattern: std::collections::BTreeMap<String, DatapackBannerPattern>,
    jukebox_song: std::collections::BTreeMap<String, DatapackJukeboxSong>,
}

impl DynamicDataRegistries {
    /// Parse one `data/<ns>/<registry>/<path>.json` body into the right map.
    /// Files that do not parse as objects are dropped (fail closed), like the
    /// tag/recipe readers.
    #[cfg_attr(not(test), allow(dead_code))]
    fn read_entry(&mut self, path: &str, body: &str) {
        let Some(("data", path)) = path.split_once('/') else {
            return;
        };
        let Some((ns, path)) = path.split_once('/') else {
            return;
        };
        let Some((kind, rel)) = path.split_once('/') else {
            return;
        };
        let Some(kind) = dyn_registry_kind(kind) else {
            return;
        };
        let Some(rel) = rel.strip_suffix(".json") else {
            return;
        };
        if rel.is_empty() {
            return;
        }
        let id = format!("{ns}:{rel}");
        let Some(j) = jparse(body.as_bytes()) else {
            return;
        };
        match kind {
            "enchantment" => {
                self.enchantment.insert(
                    id,
                    DatapackEnchantment {
                        raw: body.to_owned(),
                        description: dyn_jtext(&j, "description"),
                        supported_items: dyn_jstr_list(&j, "supported_items"),
                        weight: j.get("weight").map(|v| v.num() as i64).unwrap_or(0),
                        max_level: j.get("max_level").map(|v| v.num() as i64).unwrap_or(0),
                        anvil_cost: j.get("anvil_cost").map(|v| v.num() as i64).unwrap_or(0),
                        slots: dyn_jstr_list(&j, "slots"),
                    },
                );
            }
            "damage_type" => {
                self.damage_type.insert(
                    id,
                    DatapackDamageType {
                        raw: body.to_owned(),
                        message_id: j
                            .get("message_id")
                            .map(|v| v.str().to_owned())
                            .unwrap_or_default(),
                        exhaustion: j.get("exhaustion").map(|v| v.num()).unwrap_or(0.0),
                        scaling: j
                            .get("scaling")
                            .map(|v| v.str().to_owned())
                            .unwrap_or_else(|| "when_caused_by_living_non_player".to_owned()),
                        effects: j.get("effects").map(|v| v.str().to_owned()),
                    },
                );
            }
            "chat_type" => {
                let deco = |key: &str| {
                    j.get(key).map(|d| ChatDecoration {
                        translation_key: d
                            .get("translation_key")
                            .map(|v| v.str().to_owned())
                            .unwrap_or_default(),
                        parameters: dyn_jstr_list(d, "parameters"),
                    })
                };
                self.chat_type.insert(
                    id,
                    DatapackChatType {
                        raw: body.to_owned(),
                        chat: deco("chat"),
                        narration: deco("narration"),
                    },
                );
            }
            "wolf_variant" => {
                let tex = |key: &str| {
                    j.get(key)
                        .map(|v| v.str().to_owned())
                        .unwrap_or_default()
                };
                self.wolf_variant.insert(
                    id,
                    DatapackWolfVariant {
                        raw: body.to_owned(),
                        wild_texture: tex("wild_texture"),
                        tame_texture: tex("tame_texture"),
                        angry_texture: tex("angry_texture"),
                        biomes: dyn_jstr_list(&j, "biomes"),
                    },
                );
            }
            "painting_variant" => {
                self.painting_variant.insert(
                    id,
                    DatapackPaintingVariant {
                        raw: body.to_owned(),
                        asset_id: j
                            .get("asset_id")
                            .map(|v| v.str().to_owned())
                            .unwrap_or_default(),
                        width: j.get("width").map(|v| v.num() as i64).unwrap_or(0),
                        height: j.get("height").map(|v| v.num() as i64).unwrap_or(0),
                    },
                );
            }
            "banner_pattern" => {
                self.banner_pattern.insert(
                    id,
                    DatapackBannerPattern {
                        raw: body.to_owned(),
                        asset_id: j
                            .get("asset_id")
                            .map(|v| v.str().to_owned())
                            .unwrap_or_default(),
                        translation_key: j
                            .get("translation_key")
                            .map(|v| v.str().to_owned())
                            .unwrap_or_default(),
                    },
                );
            }
            "jukebox_song" => {
                self.jukebox_song.insert(
                    id,
                    DatapackJukeboxSong {
                        raw: body.to_owned(),
                        sound_event: j
                            .get("sound_event")
                            .map(|v| v.str().to_owned())
                            .unwrap_or_default(),
                        description: dyn_jtext(&j, "description"),
                        length_in_seconds: j
                            .get("length_in_seconds")
                            .map(|v| v.num())
                            .unwrap_or(0.0),
                        comparator_output: j
                            .get("comparator_output")
                            .map(|v| v.num() as i64)
                            .unwrap_or(0),
                    },
                );
            }
            _ => {}
        }
    }
}

/// Scan `base` (the `datapacks/` directory) the same way `reload_datapacks`
/// does — alphabetical sources, disabled packs skipped, `.zip` packs through
/// the bounded ZIP reader — and merge every recognized registry entry,
/// later packs winning.
#[cfg_attr(not(test), allow(dead_code))]
fn load_dynamic_registries(base: &std::path::Path) -> DynamicDataRegistries {
    let mut regs = DynamicDataRegistries::default();
    let disabled = disabled_datapacks().lock().unwrap().clone();
    for (name, path, zip) in datapack_sources(base) {
        if disabled.contains(&name) {
            continue;
        }
        if zip {
            let mut bytes = Vec::new();
            let entries = std::fs::File::open(&path)
                .and_then(|file| {
                    file.take(ZIP_MAX_BYTES as u64 + 1).read_to_end(&mut bytes)
                })
                .ok()
                .filter(|&len| len <= ZIP_MAX_BYTES)
                .and_then(|_| zip_datapack_entries(&bytes));
            let Some(entries) = entries else {
                continue;
            };
            for (path, body) in entries {
                if let Ok(body) = std::str::from_utf8(&body) {
                    regs.read_entry(&path, body);
                }
            }
        } else {
            let mut dirs = vec![path.join("data")];
            let mut files = Vec::new();
            while let Some(dir) = dirs.pop() {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    let path = entry.path();
                    if kind.is_dir() {
                        dirs.push(path);
                    } else if kind.is_file()
                        && path.extension().is_some_and(|ext| ext == "json")
                    {
                        files.push(path);
                    }
                }
            }
            files.sort_unstable();
            for file in files {
                let Ok(path) = file.strip_prefix(&path) else {
                    continue;
                };
                if let (Some(path), Ok(body)) =
                    (path.to_str(), std::fs::read_to_string(&file))
                {
                    regs.read_entry(&path.replace('\\', "/"), &body);
                }
            }
        }
    }
    regs
}

/// Published dynamic-registry snapshot for later configuration-sync lanes,
/// refreshed from `datapacks/` on demand.
fn dynamic_registries() -> &'static Mutex<DynamicDataRegistries> {
    static R: OnceLock<Mutex<DynamicDataRegistries>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(DynamicDataRegistries::default()))
}

/// Reload the published snapshot from the `datapacks/` directory.
#[allow(dead_code)]
fn refresh_dynamic_registries() {
    let regs = load_dynamic_registries(std::path::Path::new(DATAPACK_DIRECTORY));
    *dynamic_registries().lock().unwrap() = regs;
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

// ------------------------------------------------------------ p5g_parity ---
#[cfg(test)]
mod p5g_datapack_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Same fresh-cwd fixture as `P3Directory`, kept module-local so the two
    /// suites cannot collide on a directory name.
    struct P5gDirectory {
        path: PathBuf,
        previous: PathBuf,
    }

    impl P5gDirectory {
        fn enter() -> Self {
            let path = std::env::temp_dir()
                .join(format!("spine-p5g-datapack-{}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            let previous = std::env::current_dir().unwrap();
            std::env::set_current_dir(&path).unwrap();
            Self { path, previous }
        }
    }

    impl std::ops::Drop for P5gDirectory {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.previous).unwrap();
            std::fs::remove_dir_all(&self.path).unwrap();
        }
    }

    fn write_file(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn verdict(json: &str, fmt: u32) -> PackFormatVerdict {
        parse_and_check_pack_mcmeta(json, fmt, (0, 0)).unwrap()
    }

    fn drop_at(item: &str, count: i32) -> LootDrop {
        LootDrop {
            item: item.to_owned(),
            count,
            damage: 0,
        }
    }

    // ------------------------------------------------------------- #194 ---

    #[test]
    fn p5g_parity_pack_mcmeta_scalar_pack_format() {
        let mcmeta = r#"{"pack":{"pack_format":48,"description":"d"}}"#;
        assert_eq!(verdict(mcmeta, 48), PackFormatVerdict::Compatible);
        assert_eq!(
            verdict(mcmeta, 47),
            PackFormatVerdict::TooNew {
                min_supported: PackVersion(48, 0)
            }
        );
        assert_eq!(
            verdict(mcmeta, 49),
            PackFormatVerdict::TooOld {
                max_supported: PackVersion(48, 0)
            }
        );
    }

    #[test]
    fn p5g_parity_pack_mcmeta_supported_formats_array_and_object() {
        // `[41, 48]` widens the declared 48 into the inclusive 41..=48 range.
        let array = r#"{"pack":{"pack_format":48,"supported_formats":[41,48]}}"#;
        assert_eq!(verdict(array, 41), PackFormatVerdict::Compatible);
        assert_eq!(verdict(array, 45), PackFormatVerdict::Compatible);
        assert_eq!(verdict(array, 48), PackFormatVerdict::Compatible);
        assert_eq!(
            verdict(array, 40),
            PackFormatVerdict::TooNew {
                min_supported: PackVersion(41, 0)
            }
        );
        assert_eq!(
            verdict(array, 49),
            PackFormatVerdict::TooOld {
                max_supported: PackVersion(48, 0)
            }
        );
        // `{min_inclusive, max_inclusive}` object spelling resolves identically.
        let object = r#"{"pack":{"pack_format":48,"supported_formats":{"min_inclusive":41,"max_inclusive":48}}}"#;
        assert_eq!(verdict(object, 44), PackFormatVerdict::Compatible);
        assert_eq!(
            verdict(object, 50),
            PackFormatVerdict::TooOld {
                max_supported: PackVersion(48, 0)
            }
        );
        // A scalar `supported_formats` reads as a single-point range.
        let scalar = r#"{"pack":{"pack_format":48,"supported_formats":46}}"#;
        assert_eq!(verdict(scalar, 46), PackFormatVerdict::Compatible);
        assert_eq!(
            verdict(scalar, 45),
            PackFormatVerdict::TooNew {
                min_supported: PackVersion(46, 0)
            }
        );
        assert_eq!(
            verdict(scalar, 47),
            PackFormatVerdict::TooOld {
                max_supported: PackVersion(46, 0)
            }
        );
    }

    #[test]
    fn p5g_parity_pack_mcmeta_26_3_tuple_verdicts() {
        let tuple = r#"{"pack":{"min_format":[121,0],"max_format":[121,3]}}"#;
        let check = |t| parse_and_check_pack_mcmeta(tuple, 0, t).unwrap();
        assert_eq!(check((121, 0)), PackFormatVerdict::Compatible);
        assert_eq!(check((121, 2)), PackFormatVerdict::Compatible);
        assert_eq!(check((121, 3)), PackFormatVerdict::Compatible);
        assert_eq!(
            check((120, 9)),
            PackFormatVerdict::TooNew {
                min_supported: PackVersion(121, 0)
            }
        );
        assert_eq!(
            check((122, 0)),
            PackFormatVerdict::TooOld {
                max_supported: PackVersion(121, 3)
            }
        );
        assert_eq!(
            check((121, 4)),
            PackFormatVerdict::TooOld {
                max_supported: PackVersion(121, 3)
            }
        );
        // The tuple scheme wins when a pack carries both spellings.
        let both = r#"{"pack":{"pack_format":48,"min_format":[121,0],"max_format":[121,3]}}"#;
        assert_eq!(
            parse_and_check_pack_mcmeta(both, 48, (121, 1)).unwrap(),
            PackFormatVerdict::Compatible
        );
        // One half alone is malformed; so is a non-2-element tuple.
        for broken in [
            r#"{"pack":{"min_format":[121,0]}}"#,
            r#"{"pack":{"max_format":[121,3]}}"#,
            r#"{"pack":{"min_format":[121],"max_format":[121,3]}}"#,
            r#"{"pack":{"min_format":[121,0],"max_format":"122"}}"#,
        ] {
            assert_eq!(
                parse_and_check_pack_mcmeta(broken, 0, (121, 0)).unwrap(),
                PackFormatVerdict::Invalid,
                "{broken}"
            );
        }
    }

    #[test]
    fn p5g_parity_pack_mcmeta_fail_closed() {
        // Not JSON at all, or not a JSON object: a hard parse error.
        for bad in ["nope", "{", "\"pack\"", "[]", "42"] {
            assert!(parse_and_check_pack_mcmeta(bad, 48, (0, 0)).is_err(), "{bad}");
        }
        // Well-formed JSON without usable pack metadata: Invalid, never panic.
        for invalid in [
            r#"{"pack":{"description":"missing format"}}"#,
            r#"{"pack":{"pack_format":"48"}}"#,
            r#"{"pack":{"pack_format":48.5}}"#,
            r#"{"pack":{"pack_format":48,"supported_formats":"wide"}}"#,
            r#"{"pack":{"pack_format":48,"supported_formats":[]}}"#,
            r#"{"pack":{"pack_format":48,"supported_formats":{"min_inclusive":41}}}"#,
            r#"{"pack":{"pack_format":48,"supported_formats":[41,"48"]}}"#,
            r#"{"pack":null}"#,
        ] {
            assert_eq!(verdict(invalid, 48), PackFormatVerdict::Invalid, "{invalid}");
        }
        // A `pack` that isn't an object also fails closed.
        assert_eq!(verdict(r#"{"pack":48}"#, 48), PackFormatVerdict::Invalid);
    }

    #[test]
    fn p5g_parity_pack_mcmeta_filter_block() {
        let m = PackMcmeta::parse(
            r#"{"pack":{"pack_format":48},
               "filter":{"block":[
                   {"namespace":"evilns","path":"data/.*"},
                   {"path":"forbidden"},
                   {"namespace":"literal\\.json"}
               ]}}"#,
        )
        .unwrap();
        assert!(m.valid);
        // Rule 1: namespace literal + path regex.
        assert!(m.blocks("evilns", "data/foo"));
        assert!(!m.blocks("evilns", "other/foo"));
        assert!(!m.blocks("okns", "data/foo"));
        // Rule 2: absent namespace defaults to `.*` — any ns, literal path.
        assert!(m.blocks("anything", "forbidden"));
        assert!(!m.blocks("anything", "forbidden/x"));
        // Rule 3: escaped `\.` in the namespace must not treat `.` as wildcard.
        assert!(m.blocks("literal.json", "any/path"));
        assert!(!m.blocks("literal_json", "any/path"));
        // A malformed rule invalidates the mcmeta but does not panic.
        let broken = PackMcmeta::parse(
            r#"{"pack":{"pack_format":48},"filter":{"block":[{"path":"["}]}}"#,
        )
        .unwrap();
        assert!(!broken.valid);
        assert_eq!(verdict(r#"{"pack":{"pack_format":48},"filter":{"block":[{"path":"["}]}}"#, 48), PackFormatVerdict::Invalid);
    }

    #[test]
    fn p5g_parity_pack_mcmeta_real_datapacks() {
        // Every pack.mcmeta shipped with the repo declares format 48 and must
        // classify Compatible for the 1.21.1 target.
        for entry in [
            "datapacks/vanilla/pack.mcmeta",
            "datapacks/modtest/pack.mcmeta",
            "datapacks/rectest/pack.mcmeta",
            "datapacks/spinetest/pack.mcmeta",
            "datapacks/tagtest/pack.mcmeta",
            "datapacks/testpack/pack.mcmeta",
        ] {
            let path = Path::new(entry);
            let Ok(body) = std::fs::read_to_string(path) else {
                continue;
            };
            assert_eq!(
                parse_and_check_pack_mcmeta(&body, 48, (0, 0)).unwrap(),
                PackFormatVerdict::Compatible,
                "{entry}"
            );
        }
    }

    // ------------------------------------------------------------- #197 ---

    /// The ore-drop pool shape from `blocks/diamond_ore.json`: Silk Touch
    /// wins the alternatives arm, Fortune multiplies the gem arm.
    const ORE_TABLE: &str = r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:alternatives","children":[
        {"type":"minecraft:item","name":"minecraft:diamond_ore","conditions":[{"condition":"minecraft:match_tool","predicate":{"enchantments":[{"enchantments":"minecraft:silk_touch","levels":{"min":1}}]}}]},
        {"type":"minecraft:item","name":"minecraft:diamond","conditions":[{"condition":"minecraft:inverted","term":{"condition":"minecraft:match_tool","predicate":{"enchantments":[{"enchantments":"minecraft:silk_touch"}]}}}],"functions":[{"function":"minecraft:apply_bonus","enchantment":"minecraft:fortune","formula":"minecraft:ore_drops"}]}
    ]}]}]}"#;

    fn tool(item: &str, enchants: &[(&str, i32)]) -> LootTool {
        LootTool {
            item: item.to_owned(),
            enchantments: enchants
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect(),
        }
    }

    #[test]
    fn p5g_parity_loot_alternatives_silk_touch_vs_fortune() {
        // Silk Touch I pick: the alternatives arm matches first — the raw ore
        // block drops, never the bonus-multiplied gem.
        let mut ctx = LootEvalContext::seeded(7);
        ctx.tool = Some(tool("minecraft:diamond_pickaxe", &[("silk_touch", 1)]));
        assert_eq!(
            evaluate_loot_table_json(ORE_TABLE, &mut ctx),
            vec![drop_at("minecraft:diamond_ore", 1)]
        );
        // Fortune III: the silk arm fails, the gem arm multiplies.
        let mut ctx = LootEvalContext::seeded(7);
        ctx.tool = Some(tool(
            "minecraft:diamond_pickaxe",
            &[("minecraft:fortune", 3)],
        ));
        let drops = evaluate_loot_table_json(ORE_TABLE, &mut ctx);
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].item, "minecraft:diamond");
        // ore_drops at level 3: count * (max(0, nextInt(5) - 1) + 1) ∈ 1..=4.
        assert!((1..=4).contains(&drops[0].count), "{drops:?}");
        // Same seed replays the same draw.
        let mut again = LootEvalContext::seeded(7);
        again.tool = Some(tool(
            "minecraft:diamond_pickaxe",
            &[("minecraft:fortune", 3)],
        ));
        assert_eq!(evaluate_loot_table_json(ORE_TABLE, &mut again), drops);
        // Unenchanted tool: ore_drops still draws nextInt(2) but can only
        // produce multiplier 1 — the gem count stays 1.
        let mut ctx = LootEvalContext::seeded(9);
        ctx.tool = Some(tool("minecraft:iron_pickaxe", &[]));
        assert_eq!(
            evaluate_loot_table_json(ORE_TABLE, &mut ctx),
            vec![drop_at("minecraft:diamond", 1)]
        );
    }

    #[test]
    fn p5g_parity_loot_group_and_sequence_expansion() {
        let mut ctx = LootEvalContext::seeded(1);
        // group emits every child whose conditions pass.
        let group = loot_entry_node(
            &jparse(
                br#"{"type":"minecraft:group","children":[
                    {"type":"minecraft:item","name":"minecraft:a"},
                    {"type":"minecraft:item","name":"minecraft:b","conditions":[{"condition":"minecraft:random_chance","chance":0.0}]},
                    {"type":"minecraft:item","name":"minecraft:c"}
                ]}"#,
            )
            .unwrap(),
        );
        let mut out = Vec::new();
        assert!(loot_expand_entry(&group, &mut ctx, &mut out));
        assert_eq!(
            out.iter().map(|c| c.node.name.as_str()).collect::<Vec<_>>(),
            ["minecraft:a", "minecraft:c"]
        );
        // sequence emits until the first condition failure — the children
        // already emitted before that failure stay in the candidate list,
        // matching vanilla's `a.expand && b.expand` combinator.
        let seq = loot_entry_node(
            &jparse(
                br#"{"type":"minecraft:sequence","children":[
                    {"type":"minecraft:item","name":"minecraft:a"},
                    {"type":"minecraft:item","name":"minecraft:b","conditions":[{"condition":"minecraft:random_chance","chance":0.0}]},
                    {"type":"minecraft:item","name":"minecraft:c"}
                ]}"#,
            )
            .unwrap(),
        );
        let mut out = Vec::new();
        assert!(!loot_expand_entry(&seq, &mut ctx, &mut out));
        assert_eq!(
            out.iter().map(|c| c.node.name.as_str()).collect::<Vec<_>>(),
            ["minecraft:a"]
        );
        // An all-passing sequence expands true and emits every child.
        let seq_all = loot_entry_node(
            &jparse(
                br#"{"type":"minecraft:sequence","children":[
                    {"type":"minecraft:item","name":"minecraft:a"},
                    {"type":"minecraft:item","name":"minecraft:b"}
                ]}"#,
            )
            .unwrap(),
        );
        let mut out = Vec::new();
        assert!(loot_expand_entry(&seq_all, &mut ctx, &mut out));
        assert_eq!(out.len(), 2);
        // An alternatives entry with every child failing expands false.
        let alt = loot_entry_node(
            &jparse(
                br#"{"type":"minecraft:alternatives","children":[
                    {"type":"minecraft:item","name":"minecraft:a","conditions":[{"condition":"minecraft:random_chance","chance":0.0}]}
                ]}"#,
            )
            .unwrap(),
        );
        assert!(!loot_expand_entry(&alt, &mut ctx, &mut Vec::new()));
    }

    #[test]
    fn p5g_parity_loot_conditions() {
        let mut ctx = LootEvalContext::seeded(5);
        let parse = |s: &str| loot_condition(&jparse(s.as_bytes()).unwrap());
        // inverted(killed_by_player).
        let inv = parse(
            r#"{"condition":"minecraft:inverted","term":{"condition":"minecraft:killed_by_player"}}"#,
        );
        assert!(loot_condition_holds(&inv, &mut ctx));
        ctx.killed_by_player = true;
        assert!(!loot_condition_holds(&inv, &mut ctx));
        ctx.killed_by_player = false;
        // any_of short-circuits true; all_of needs every term.
        let any = parse(
            r#"{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:killed_by_player"},{"condition":"minecraft:random_chance","chance":1.0}]}"#,
        );
        assert!(loot_condition_holds(&any, &mut ctx));
        let all = parse(
            r#"{"condition":"minecraft:all_of","terms":[{"condition":"minecraft:killed_by_player"},{"condition":"minecraft:random_chance","chance":1.0}]}"#,
        );
        assert!(!loot_condition_holds(&all, &mut ctx));
        ctx.killed_by_player = true;
        assert!(loot_condition_holds(&all, &mut ctx));
        ctx.killed_by_player = false;
        // random_chance 0 / 1 are deterministic endpoints.
        assert!(!loot_condition_holds(
            &parse(r#"{"condition":"minecraft:random_chance","chance":0.0}"#),
            &mut ctx
        ));
        assert!(loot_condition_holds(
            &parse(r#"{"condition":"minecraft:random_chance","chance":1.0}"#),
            &mut ctx
        ));
        // survives_explosion: no radius always passes; radius 4 keeps ~1/4.
        let surv = parse(r#"{"condition":"minecraft:survives_explosion"}"#);
        assert!(loot_condition_holds(&surv, &mut ctx));
        ctx.explosion_radius = Some(4.0);
        let passes = (0..40)
            .filter(|_| loot_condition_holds(&surv, &mut ctx))
            .count();
        assert!((3..=25).contains(&passes), "survives passes: {passes}");
        ctx.explosion_radius = None;
        // match_tool: absent tool fails; id and enchant predicates gate.
        let mt_pick = parse(
            r#"{"condition":"minecraft:match_tool","predicate":{"items":"minecraft:diamond_pickaxe"}}"#,
        );
        assert!(!loot_condition_holds(&mt_pick, &mut ctx));
        ctx.tool = Some(tool("minecraft:diamond_pickaxe", &[]));
        assert!(loot_condition_holds(&mt_pick, &mut ctx));
        ctx.tool = Some(tool("minecraft:iron_pickaxe", &[]));
        assert!(!loot_condition_holds(&mt_pick, &mut ctx));
        // random_chance_with_enchanted_bonus uses the tool's enchant level.
        let bonus = parse(
            r#"{"condition":"minecraft:random_chance_with_enchanted_bonus","enchantment":"minecraft:fortune","unenchanted_chance":0.0,"enchanted_chance":{"type":"minecraft:linear","base":0.5,"per_level_above_first":0.5}}"#,
        );
        ctx.tool = Some(tool("minecraft:diamond_pickaxe", &[]));
        assert!(!loot_condition_holds(&bonus, &mut ctx));
        ctx.tool = Some(tool("minecraft:diamond_pickaxe", &[("fortune", 2)]));
        // linear: 0.5 + 0.5 * (2 - 1) = 1.0 → always passes.
        assert!(loot_condition_holds(&bonus, &mut ctx));
        // An unknown condition type fails closed.
        assert!(!loot_condition_holds(
            &parse(r#"{"condition":"minecraft:entity_scores"}"#),
            &mut ctx
        ));
    }

    #[test]
    fn p5g_parity_loot_functions_set_count_limit_explosion() {
        // set_count uniform {2,5} then limit_count max 3 → count ∈ 2..=3.
        let table = r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:coal","functions":[
            {"function":"minecraft:set_count","count":{"min":2,"max":5}},
            {"function":"minecraft:limit_count","limit":{"max":3}}
        ]}]}]}"#;
        let mut seen_min = i32::MAX;
        let mut seen_max = 0;
        // Seeds are golden-ratio strided: `Lcg::new` is a Java `Random`, whose
        // first draws correlate across consecutive seeds.
        for i in 0..30u64 {
            let mut ctx = LootEvalContext::seeded(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let drops = evaluate_loot_table_json(table, &mut ctx);
            assert_eq!(drops.len(), 1);
            assert!((2..=3).contains(&drops[0].count), "{drops:?}");
            seen_min = seen_min.min(drops[0].count);
            seen_max = seen_max.max(drops[0].count);
        }
        assert_eq!((seen_min, seen_max), (2, 3), "both clamp endpoints reached");
        // set_count "add": true accumulates onto the rolled 1.
        let add = r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:coal","functions":[
            {"function":"minecraft:set_count","count":2,"add":true}
        ]}]}]}"#;
        let mut ctx = LootEvalContext::seeded(1);
        assert_eq!(
            evaluate_loot_table_json(add, &mut ctx),
            vec![drop_at("minecraft:coal", 3)]
        );
        // explosion_decay keeps each item with 1/radius — a 64-stack under
        // radius 8.0 loses most of its count but stays deterministic per seed.
        let decay = r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:tnt","functions":[
            {"function":"minecraft:set_count","count":64},
            {"function":"minecraft:explosion_decay"}
        ]}]}]}"#;
        let mut ctx = LootEvalContext::seeded(3);
        ctx.explosion_radius = Some(8.0);
        let drops = evaluate_loot_table_json(decay, &mut ctx);
        assert_eq!(drops[0].item, "minecraft:tnt");
        assert!((1..=24).contains(&drops[0].count), "{drops:?}");
        let mut ctx2 = LootEvalContext::seeded(3);
        ctx2.explosion_radius = Some(8.0);
        assert_eq!(evaluate_loot_table_json(decay, &mut ctx2), drops);
        // Without an explosion radius the function is a no-op.
        let mut ctx = LootEvalContext::seeded(3);
        assert_eq!(
            evaluate_loot_table_json(decay, &mut ctx),
            vec![drop_at("minecraft:tnt", 64)]
        );
        // A stack that decays to zero drops out entirely.
        let mut ctx = LootEvalContext::seeded(3);
        ctx.explosion_radius = Some(1.0e9);
        assert!(evaluate_loot_table_json(decay, &mut ctx).is_empty());
    }

    #[test]
    fn p5g_parity_loot_functions_set_damage_and_bonus() {
        // set_damage 0.5 (no add): damage = floor((1 - 0.5) * max_damage).
        let mut ctx = LootEvalContext::seeded(1);
        let mut drop = drop_at("minecraft:diamond_pickaxe", 1);
        let func = loot_function(
            &jparse(br#"{"function":"minecraft:set_damage","damage":0.5}"#).unwrap(),
        );
        let max = tool_max_damage(name_to_item("diamond_pickaxe").unwrap())
            .expect("diamond pickaxe is damageable");
        loot_apply_function(&mut drop, &func, &mut ctx);
        assert_eq!(
            drop.damage,
            ((1.0 - 0.5f32) * max as f32).floor() as i32
        );
        assert_eq!(max, 1561, "vanilla diamond pickaxe durability");
        // add:true starts from the current wear fraction.
        let func = loot_function(
            &jparse(br#"{"function":"minecraft:set_damage","damage":0.0,"add":true}"#).unwrap(),
        );
        drop.damage = (max / 4) as i32;
        loot_apply_function(&mut drop, &func, &mut ctx);
        assert_eq!(drop.damage, (max / 4) as i32, "add keeps existing wear");
        // uniform_bonus_count: count + nextInt(level * bonusMultiplier + 1).
        let table = r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:coal","functions":[
            {"function":"minecraft:apply_bonus","enchantment":"minecraft:fortune","formula":"minecraft:uniform_bonus_count","parameters":{"bonusMultiplier":2}}
        ]}]}]}"#;
        let mut counts = std::collections::BTreeSet::new();
        for i in 0..40u64 {
            let mut ctx = LootEvalContext::seeded(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            ctx.tool = Some(tool("minecraft:diamond_pickaxe", &[("fortune", 3)]));
            let drops = evaluate_loot_table_json(table, &mut ctx);
            assert_eq!(drops.len(), 1);
            assert!((1..=7).contains(&drops[0].count), "{drops:?}");
            counts.insert(drops[0].count);
        }
        assert!(counts.len() >= 3, "multiplier actually varies: {counts:?}");
        // binomial_with_bonus_count: level + extra trials at p.
        let table = r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:coal","functions":[
            {"function":"minecraft:apply_bonus","enchantment":"minecraft:fortune","formula":"minecraft:binomial_with_bonus_count","parameters":{"extra":2,"probability":0.0}}
        ]}]}]}"#;
        let mut ctx = LootEvalContext::seeded(1);
        ctx.tool = Some(tool("minecraft:diamond_pickaxe", &[("fortune", 3)]));
        // p = 0 → the count never grows past the base 1.
        assert_eq!(
            evaluate_loot_table_json(table, &mut ctx),
            vec![drop_at("minecraft:coal", 1)]
        );
    }

    /// The reload-dependent half of #197: `minecraft:tag` entries expand via
    /// the datapack item-tag registry, `minecraft:loot_table` resolves through
    /// `loot_table_json`, and `furnace_smelt` consults the smelt registry.
    #[test]
    fn p5g_parity_loot_tag_nested_cycle_and_smelt() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        let _dir = P5gDirectory::enter();
        let root = Path::new("datapacks");
        write_file(
            &root.join("a/pack.mcmeta"),
            r#"{"pack":{"pack_format":48}}"#,
        );
        write_file(
            &root.join("a/data/test/tags/item/ores.json"),
            r#"{"values":["minecraft:coal_ore","minecraft:iron_ore"]}"#,
        );
        write_file(
            &root.join("a/data/test/recipe/smooth_stone.json"),
            r#"{"type":"minecraft:smelting","ingredient":"minecraft:stone","result":{"id":"minecraft:smooth_stone","count":1}}"#,
        );
        write_file(
            &root.join("a/data/test/loot_table/leaf.json"),
            r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:dirt"}]}]}"#,
        );
        write_file(
            &root.join("a/data/test/loot_table/outer.json"),
            r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:loot_table","name":"test:leaf"}]}]}"#,
        );
        // Direct self-reference: cycle guard must cut it.
        write_file(
            &root.join("a/data/test/loot_table/loop.json"),
            r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:loot_table","name":"test:loop"}]}]}"#,
        );
        // Two-hop cycle: cycle_a -> cycle_b -> cycle_a; cycle_b also emits.
        write_file(
            &root.join("a/data/test/loot_table/cycle_a.json"),
            r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:loot_table","name":"test:cycle_b"}]}]}"#,
        );
        write_file(
            &root.join("a/data/test/loot_table/cycle_b.json"),
            r#"{"pools":[
                {"rolls":1,"entries":[{"type":"minecraft:loot_table","name":"test:cycle_a"}]},
                {"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:coal"}]}
            ]}"#,
        );
        write_file(
            &root.join("a/data/test/loot_table/tagmode.json"),
            r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:tag","name":"test:ores","expand":false}]}]}"#,
        );
        write_file(
            &root.join("a/data/test/loot_table/tagexpand.json"),
            r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:tag","name":"test:ores","expand":true}]}]}"#,
        );
        write_file(
            &root.join("a/data/test/loot_table/smelt.json"),
            r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:stone","functions":[{"function":"minecraft:furnace_smelt"}]}]}]}"#,
        );
        reload_datapacks(root, None, false).expect("synthetic pack loads");
        let mut ctx = LootEvalContext::seeded(4);
        // expand:false — one candidate producing every tag member.
        assert_eq!(
            evaluate_loot_table("test:tagmode", &mut ctx),
            vec![
                drop_at("minecraft:coal_ore", 1),
                drop_at("minecraft:iron_ore", 1)
            ]
        );
        // expand:true — each member is an equal-weight sibling candidate;
        // the single roll picks exactly one.
        let expanded = evaluate_loot_table("test:tagexpand", &mut ctx);
        assert_eq!(expanded.len(), 1);
        assert!(
            ["minecraft:coal_ore", "minecraft:iron_ore"].contains(&expanded[0].item.as_str()),
            "{expanded:?}"
        );
        // Nested loot_table delegates to the referenced table.
        assert_eq!(
            evaluate_loot_table("test:outer", &mut ctx),
            vec![drop_at("minecraft:dirt", 1)]
        );
        // The visited-table set cuts direct and two-hop cycles (would hang or
        // overflow without the guard) and leaves valid output intact.
        assert!(evaluate_loot_table("test:loop", &mut ctx).is_empty());
        assert_eq!(
            evaluate_loot_table("test:cycle_a", &mut ctx),
            vec![drop_at("minecraft:coal", 1)]
        );
        // furnace_smelt transmutes through the loaded smelting recipe.
        assert_eq!(
            evaluate_loot_table("test:smelt", &mut ctx),
            vec![drop_at("minecraft:smooth_stone", 1)]
        );
    }

    // ------------------------------------------------------------- #198 ---

    #[test]
    fn p5g_parity_dynamic_registries_all_seven_sorted() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        let _dir = P5gDirectory::enter();
        let root = Path::new("datapacks");
        let a = root.join("a/data/test");
        write_file(&root.join("a/pack.mcmeta"), r#"{"pack":{"pack_format":48}}"#);
        write_file(
            &a.join("enchantment/zap.json"),
            r##"{"description":{"text":"Zap"},"supported_items":"#minecraft:swords","weight":5,"max_level":3,"anvil_cost":4,"slots":["mainhand","offhand"]}"##,
        );
        write_file(
            &a.join("damage_type/zap.json"),
            r#"{"message_id":"zap.death","exhaustion":0.5,"scaling":"always","effects":"hurt"}"#,
        );
        write_file(
            &a.join("chat_type/team.json"),
            r#"{"chat":{"translation_key":"chat.type.team","parameters":["sender","content"]},"narration":{"translation_key":"chat.type.team.narrate","parameters":["sender","content"]}}"#,
        );
        write_file(
            &a.join("wolf_variant/pale.json"),
            r##"{"wild_texture":"minecraft:entity/wolf/wolf","tame_texture":"minecraft:entity/wolf/wolf_tame","angry_texture":"minecraft:entity/wolf/wolf_angry","biomes":["minecraft:taiga","#minecraft:is_forest"]}"##,
        );
        write_file(
            &a.join("painting_variant/sunset.json"),
            r#"{"asset_id":"test:sunset","width":2,"height":1}"#,
        );
        write_file(
            &a.join("banner_pattern/diamond.json"),
            r#"{"asset_id":"test:diamond","translation_key":"block.test.diamond"}"#,
        );
        write_file(
            &a.join("jukebox_song/track.json"),
            r#"{"sound_event":"test:music.track","description":{"text":"Track"},"length_in_seconds":95.5,"comparator_output":7}"#,
        );
        // A second pack whose entry sorts first alphabetically proves the
        // deterministic ResourceLocation ordering, and a garbage JSON file
        // proves malformed entries drop rather than panic.
        write_file(&root.join("b/pack.mcmeta"), r#"{"pack":{"pack_format":48}}"#);
        write_file(
            &root.join("b/data/test/enchantment/aaa.json"),
            r#"{"description":{"text":"A"},"supported_items":[],"weight":1,"max_level":1,"anvil_cost":1,"slots":[]}"#,
        );
        write_file(&root.join("b/data/test/enchantment/broken.json"), "{not json");
        let regs = load_dynamic_registries(root);

        assert_eq!(
            regs.enchantment.keys().cloned().collect::<Vec<_>>(),
            ["test:aaa", "test:zap"],
            "enchantment ids iterate in sorted ns:path order"
        );
        let ench = &regs.enchantment["test:zap"];
        assert_eq!(ench.description, "Zap");
        assert_eq!(ench.supported_items, ["#minecraft:swords"]);
        assert_eq!(ench.weight, 5);
        assert_eq!(ench.max_level, 3);
        assert_eq!(ench.anvil_cost, 4);
        assert_eq!(ench.slots, ["mainhand", "offhand"]);

        let damage = &regs.damage_type["test:zap"];
        assert_eq!(damage.message_id, "zap.death");
        assert_eq!(damage.exhaustion, 0.5);
        assert_eq!(damage.scaling, "always");
        assert_eq!(damage.effects.as_deref(), Some("hurt"));

        let chat = &regs.chat_type["test:team"];
        assert_eq!(chat.chat.as_ref().unwrap().translation_key, "chat.type.team");
        assert_eq!(chat.chat.as_ref().unwrap().parameters, ["sender", "content"]);
        assert_eq!(
            chat.narration.as_ref().unwrap().translation_key,
            "chat.type.team.narrate"
        );

        let wolf = &regs.wolf_variant["test:pale"];
        assert_eq!(wolf.wild_texture, "minecraft:entity/wolf/wolf");
        assert_eq!(wolf.tame_texture, "minecraft:entity/wolf/wolf_tame");
        assert_eq!(wolf.angry_texture, "minecraft:entity/wolf/wolf_angry");
        assert_eq!(wolf.biomes, ["minecraft:taiga", "#minecraft:is_forest"]);

        let painting = &regs.painting_variant["test:sunset"];
        assert_eq!(painting.asset_id, "test:sunset");
        assert_eq!(painting.width, 2);
        assert_eq!(painting.height, 1);

        let banner = &regs.banner_pattern["test:diamond"];
        assert_eq!(banner.asset_id, "test:diamond");
        assert_eq!(banner.translation_key, "block.test.diamond");

        let song = &regs.jukebox_song["test:track"];
        assert_eq!(song.sound_event, "test:music.track");
        assert_eq!(song.description, "Track");
        assert_eq!(song.length_in_seconds, 95.5);
        assert_eq!(song.comparator_output, 7);

        // `raw` keeps the file body verbatim so a later RegistryData-sync lane
        // can emit byte-faithful entries instead of reserializing typed fields.
        for raw in [
            ench.raw.as_str(),
            damage.raw.as_str(),
            chat.raw.as_str(),
            wolf.raw.as_str(),
            painting.raw.as_str(),
            banner.raw.as_str(),
            song.raw.as_str(),
        ] {
            assert!(raw.starts_with('{') && raw.ends_with('}'), "{raw}");
            assert!(jparse(raw.as_bytes()).is_some(), "{raw}");
        }
        assert!(ench.raw.contains("\"Zap\""));
    }

    /// The real `datapacks/vanilla` tree (when present) must scan without
    /// panicking, and every registry id must come out sorted.
    #[test]
    fn p5g_parity_dynamic_registries_real_vanilla_pack() {
        if !Path::new("datapacks/vanilla").is_dir() {
            return;
        }
        let regs = load_dynamic_registries(Path::new("datapacks"));
        for (name, map) in [
            ("enchantment", regs.enchantment.keys().cloned().collect::<Vec<_>>()),
            ("damage_type", regs.damage_type.keys().cloned().collect::<Vec<_>>()),
            ("chat_type", regs.chat_type.keys().cloned().collect::<Vec<_>>()),
            (
                "wolf_variant",
                regs.wolf_variant.keys().cloned().collect::<Vec<_>>(),
            ),
            (
                "painting_variant",
                regs.painting_variant.keys().cloned().collect::<Vec<_>>(),
            ),
            (
                "banner_pattern",
                regs.banner_pattern.keys().cloned().collect::<Vec<_>>(),
            ),
            (
                "jukebox_song",
                regs.jukebox_song.keys().cloned().collect::<Vec<_>>(),
            ),
        ] {
            let mut sorted = map.clone();
            sorted.sort();
            assert_eq!(map, sorted, "{name} ids iterate sorted");
        }
        eprintln!(
            "real pack registry counts: enchantment={} damage_type={} chat_type={} wolf={} painting={} banner={} jukebox={}",
            regs.enchantment.len(),
            regs.damage_type.len(),
            regs.chat_type.len(),
            regs.wolf_variant.len(),
            regs.painting_variant.len(),
            regs.banner_pattern.len(),
            regs.jukebox_song.len(),
        );
    }
}
