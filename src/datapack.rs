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
// Shared JSON accessors — the tag, recipe, dynamic-registry, loot and
// pack.mcmeta readers all read the same handful of `J` field shapes and all
// need the same resource-location normalization.
// ---------------------------------------------------------------------------

/// `ns:path` normalization: a bare path reads as `minecraft:<path>`.
fn dp_norm_id(id: &str) -> String {
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

/// An id with its `minecraft:` namespace stripped; other namespaces stay.
fn dp_bare_id(id: &str) -> &str {
    id.strip_prefix("minecraft:").unwrap_or(id)
}

/// `key` as a string, or `default` when absent/non-string.
fn dpj_str_or(j: &J, key: &str, default: &str) -> String {
    j.get(key)
        .map_or_else(|| default.to_owned(), |v| v.str().to_owned())
}

/// `key` as a string (`""` when absent/non-string).
fn dpj_str(j: &J, key: &str) -> String {
    dpj_str_or(j, key, "")
}

/// `key` as a namespace-stripped id string (`""` when absent).
#[cfg(test)]
fn dpj_ns_id(j: &J, key: &str) -> String {
    dp_bare_id(&dpj_str(j, key)).to_owned()
}

/// Numeric `key` (`0.0` when absent/non-numeric, matching `J::num`).
fn dpj_f64(j: &J, key: &str) -> f64 {
    j.get(key).map_or(0.0, J::num)
}

/// Integral `key`.
fn dpj_i64(j: &J, key: &str) -> i64 {
    dpj_f64(j, key) as i64
}

/// 32-bit integral `key`.
fn dpj_i32(j: &J, key: &str) -> i32 {
    dpj_f64(j, key) as i32
}

/// The array at `key` mapped through `f` (`[]` when absent or not an array).
#[cfg(test)]
fn dpj_map<T>(j: &J, key: &str, f: impl FnMut(&J) -> T) -> Vec<T> {
    j.get(key)
        .and_then(J::as_arr)
        .map_or(&[][..], |a| a.as_slice())
        .iter()
        .map(f)
        .collect()
}

/// The optional node as an array (`[]` when absent or not an array).
#[cfg(test)]
fn dpj_opt_arr(j: Option<&J>) -> &[J] {
    j.and_then(J::as_arr).map_or(&[], |a| a.as_slice())
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
            let member = match id.strip_prefix('#') {
                Some(reference) => format!("#{}", dp_norm_id(reference.trim())),
                None => dp_norm_id(id),
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
    let bare = dp_bare_id(member);
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
fn datapack_tag_members(category: &str, id: &str) -> Vec<String> {
    let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
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
    let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
    registry
        .tags
        .get(category)
        .is_some_and(|map| tag_contains_rec(map, id, member, &mut HashSet::new()))
}

/// `true` when `#<id>` is a loaded tag in `category`. A tag the packs never
/// define is not an empty tag: callers fall back to the pinned 1.21.1 table.
fn datapack_tag_known(category: &str, id: &str) -> bool {
    let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
    registry
        .tags
        .get(category)
        .is_some_and(|map| map.contains_key(id))
}

/// The `damage_type` tags `damage_type_flags` resolves, in slot order. The
/// order is the contract between this resolver and the flag construction.
const DAMAGE_TYPE_TAG_ORDER: [&str; 8] = [
    "bypasses_armor",
    "bypasses_invulnerability",
    "is_fire",
    "is_explosion",
    "is_projectile",
    "is_fall",
    "witch_resistant_to",
    "bypasses_shield",
];

/// Membership of one registry damage type in every tag of
/// `DAMAGE_TYPE_TAG_ORDER`, resolved under a single registry lock. `None` in a
/// slot means that tag id is not loaded, so the caller uses the pinned 1.21.1
/// membership table for it.
fn loaded_damage_type_tag_flags(member: &str) -> [Option<bool>; 8] {
    let member = dp_norm_id(member);
    let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
    let mut out = [None; 8];
    let Some(map) = registry.tags.get("damage_type") else {
        return out;
    };
    for (slot, tag) in out.iter_mut().zip(DAMAGE_TYPE_TAG_ORDER) {
        let id = format!("minecraft:{tag}");
        if map.contains_key(&id) {
            *slot = Some(tag_contains_rec(map, &id, &member, &mut HashSet::new()));
        }
    }
    out
}

/// `true` when a `#tag` id is registered under the `biome` or `worldgen/biome`
/// tag category (both spellings appear in 1.21.1 datapacks).
fn biome_tag_known(tag: &str) -> bool {
    datapack_tag_known("biome", tag) || datapack_tag_known("worldgen/biome", tag)
}

// ---------------------------------------------------------------------------
// Extended recipe registry.
// ---------------------------------------------------------------------------

/// `minecraft:stonecutting`: ingredient alternatives, result item, count.
/// Fields are read by the tests and by later recipe consumers (P1 lane).
struct StonecuttingRecipe {
    ingredients: Vec<Ingr>,
    result: i64,
    count: u8,
}

/// `minecraft:smithing_transform` (template+base+addition → result) and
/// `minecraft:smithing_trim` (same slots; no result — vanilla keeps the base
/// item and applies the trim, so `result` is `None` there).
/// Fields are read by the tests and by later recipe consumers (P1 lane).
struct SmithingRecipe {
    template: Vec<Ingr>,
    base: Vec<Ingr>,
    addition: Vec<Ingr>,
    result: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum CookingKind {
    Smelting,
    Blasting,
    Smoking,
    Campfire,
}

impl CookingKind {
    fn from_recipe_type(id: &str) -> Option<Self> {
        match dp_bare_id(id) {
            "smelting" => Some(Self::Smelting),
            "blasting" => Some(Self::Blasting),
            "smoking" => Some(Self::Smoking),
            "campfire_cooking" => Some(Self::Campfire),
            _ => None,
        }
    }

    /// Default for a missing `cookingtime`, as registered by the 1.21.1
    /// RecipeSerializer (`cze`). Vanilla campfire JSON explicitly uses 600.
    fn default_ticks(self) -> u32 {
        match self {
            Self::Smelting => 200,
            Self::Blasting | Self::Smoking | Self::Campfire => 100,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CookingResult {
    item: i64,
    count: u8,
    game_ticks: u32,
    experience: f32,
}

struct CookingRecipe {
    kind: CookingKind,
    ingredients: Vec<Ingr>,
    result: CookingResult,
}

#[derive(Default)]
struct CookingRecipes {
    by_id: std::collections::BTreeMap<String, (u64, CookingRecipe)>,
    next_priority: u64,
}

impl CookingRecipes {
    fn resolve(
        &self,
        kind: CookingKind,
        item: i64,
        tags: Option<&HashMap<String, Vec<String>>>,
    ) -> Option<CookingResult> {
        self.by_id
            .values()
            .filter(|(_, recipe)| {
                recipe.kind == kind
                    && recipe.ingredients.iter().any(|ingredient| match ingredient {
                        Ingr::Item(id) => *id == item,
                        Ingr::Tag(tag) => {
                            tags.is_some_and(|tags| dp_item_tag_match(tags, tag, item))
                        }
                    })
            })
            .max_by_key(|(priority, _)| *priority)
            .map(|(_, recipe)| recipe.result)
    }
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
    cooking: CookingRecipes,
}

fn datapack_registry() -> &'static Mutex<DatapackRegistry> {
    static R: OnceLock<Mutex<DatapackRegistry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(DatapackRegistry::default()))
}

// DPC1 hashes the complete enabled input tree, including files the current
// loader does not parse. That makes metadata and new resource types part of
// invalidation as well. The snapshot reader below never trusts cache lengths.

const DPC1_MAGIC: &[u8; 4] = b"DPC1";
const DPC1_VERSION: u32 = 2;
const DPC1_HEADER_LEN: usize = 4 + 4 + 32 + 8 + 4;
const DPC1_MAX_PAYLOAD: usize = 64 * 1024 * 1024;
const DPC1_MAX_ENTRIES: usize = 1_000_000;
const DPC1_MAX_STRING: usize = 8 * 1024 * 1024;
static DPC1_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

// SHA-256 (FIPS 180-4), streaming to avoid retaining a second copy of
// every datapack file while hashing. No additional dependency is needed.
struct Dpc1Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    buffered: usize,
    bits: u64,
}

impl Dpc1Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            buffered: 0,
            bits: 0,
        }
    }

    fn compress(&mut self) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut words = [0u32; 64];
        for (i, chunk) in self.block.as_chunks::<4>().0.iter().enumerate() {
            words[i] = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = words[i - 15].rotate_right(7)
                ^ words[i - 15].rotate_right(18)
                ^ (words[i - 15] >> 3);
            let s1 = words[i - 2].rotate_right(17)
                ^ words[i - 2].rotate_right(19)
                ^ (words[i - 2] >> 10);
            words[i] = words[i - 16]
                .wrapping_add(s0)
                .wrapping_add(words[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[i])
                .wrapping_add(words[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (state, add) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(add);
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.bits = self.bits.wrapping_add((data.len() as u64).wrapping_mul(8));
        while !data.is_empty() {
            let n = (64 - self.buffered).min(data.len());
            self.block[self.buffered..self.buffered + n].copy_from_slice(&data[..n]);
            self.buffered += n;
            data = &data[n..];
            if self.buffered == 64 {
                self.compress();
                self.buffered = 0;
            }
        }
    }

    fn finish(mut self) -> [u8; 32] {
        let bits = self.bits;
        self.update(&[0x80]);
        while self.buffered != 56 {
            self.update(&[0]);
        }
        // update() above adds padding to the length counter, but the
        // encoded length must be that of the original message.
        self.block[56..64].copy_from_slice(&bits.to_be_bytes());
        self.compress();
        let mut digest = [0; 32];
        for (out, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            out.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }
}

fn dpc1_frame(hash: &mut Dpc1Sha256, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

/// Hash each enabled pack in its specified priority order. Directory
/// entries use UTF-8 relative paths sorted lexicographically. ZIP packs
/// use their complete archive bytes so even unused archive entries count.
/// Reject symlinks, special files and unreadable input rather than risk
/// a cache hit based on an incomplete walk.
fn compute_datapack_tree_digest(
    root: &std::path::Path,
    enabled_order: &[String],
) -> Result<[u8; 32], String> {
    if !std::fs::symlink_metadata(root)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_dir()
    {
        return Err("datapack root is not a directory".into());
    }
    let sources = datapack_sources(root);
    let mut hash = Dpc1Sha256::new();
    hash.update(b"megu DPC1 tree v1");
    hash.update(&(enabled_order.len() as u64).to_le_bytes());
    let mut seen = std::collections::HashSet::new();
    for name in enabled_order {
        if !seen.insert(name) {
            return Err(format!("duplicate datapack {name}"));
        }
        let (_, path, zip) = sources
            .iter()
            .find(|(n, _, _)| n == name)
            .ok_or_else(|| format!("missing datapack {name}"))?;
        dpc1_frame(&mut hash, name.as_bytes());
        if *zip {
            hash.update(b"zip");
            dpc1_hash_file(&mut hash, path)?;
            continue;
        }
        hash.update(b"directory");
        let mut pending = vec![path.to_path_buf()];
        let mut files: Vec<(String, std::path::PathBuf)> = Vec::new();
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let ty = entry.file_type().map_err(|e| e.to_string())?;
                let item = entry.path();
                if ty.is_symlink() {
                    return Err(format!("datapack symlink rejected: {}", item.display()));
                }
                if ty.is_dir() {
                    pending.push(item);
                } else if ty.is_file() {
                    let relative = item
                        .strip_prefix(path)
                        .map_err(|e| e.to_string())?
                        .to_str()
                        .ok_or("non-UTF-8 datapack path")?
                        .replace('\\', "/");
                    files.push((relative, item));
                } else {
                    return Err(format!("unsupported datapack entry: {}", item.display()));
                }
            }
        }
        files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        hash.update(&(files.len() as u64).to_le_bytes());
        for (relative, path) in files {
            dpc1_frame(&mut hash, relative.as_bytes());
            dpc1_hash_file(&mut hash, &path)?;
        }
    }
    Ok(hash.finish())
}

fn dpc1_hash_file(hash: &mut Dpc1Sha256, path: &std::path::Path) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err(format!("not a regular datapack file: {}", path.display()));
    }
    hash.update(&meta.len().to_le_bytes());
    let mut buf = [0u8; 32 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .ok_or("datapack file too large")?;
        hash.update(&buf[..n]);
    }
    if total != meta.len() {
        return Err(format!(
            "datapack changed while hashing: {}",
            path.display()
        ));
    }
    Ok(())
}

fn dpc1_checksum(body: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(body);
    crc.sum()
}

fn dpc1_cache_path(cache_dir: &std::path::Path) -> std::path::PathBuf {
    cache_dir.join("datapacks.dpc1")
}

fn dpc1_read_payload(path: &std::path::Path, expected: &[u8; 32]) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > (DPC1_HEADER_LEN + DPC1_MAX_PAYLOAD) as u64 {
        return None;
    }
    file.take((DPC1_HEADER_LEN + DPC1_MAX_PAYLOAD + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() < DPC1_HEADER_LEN
        || &bytes[..4] != DPC1_MAGIC
        || bytes[4..8] != DPC1_VERSION.to_le_bytes()
        || &bytes[8..40] != expected
    {
        return None;
    }
    let length = u64::from_le_bytes(bytes[40..48].try_into().ok()?);
    if length > DPC1_MAX_PAYLOAD as u64 || bytes.len() != DPC1_HEADER_LEN + length as usize {
        return None;
    }
    let crc = u32::from_le_bytes(bytes[48..52].try_into().ok()?);
    (dpc1_checksum(&bytes[DPC1_HEADER_LEN..]) == crc).then(|| bytes.split_off(DPC1_HEADER_LEN))
}

fn dpc1_write_payload(
    path: &std::path::Path,
    digest: &[u8; 32],
    payload: &[u8],
) -> Result<(), String> {
    if payload.len() > DPC1_MAX_PAYLOAD {
        return Err("datapack cache payload too large".into());
    }
    let parent = path.parent().ok_or("cache path has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let sequence = DPC1_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".datapacks-{}-{sequence}.tmp", std::process::id()));
    let write = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(DPC1_MAGIC)?;
        file.write_all(&DPC1_VERSION.to_le_bytes())?;
        file.write_all(digest)?;
        file.write_all(&(payload.len() as u64).to_le_bytes())?;
        file.write_all(&dpc1_checksum(payload).to_le_bytes())?;
        file.write_all(payload)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write.map_err(|e| e.to_string())
}

/// The persisted subset is the complete extended recipe/tag registry,
/// pack-format verdicts, lossless loot JSON, all seven parsed dynamic
/// registry categories, and the retained non-vanilla recipes. The core loader
/// owns additional independent live stores (functions, crafted recipes,
/// advancement state, etc.).
#[derive(Default)]
struct Dpc1Snapshot {
    pub registry: DatapackRegistry,
    pub pack_verdicts: std::collections::BTreeMap<String, PackFormatVerdict>,
    pub loot_json: std::collections::BTreeMap<String, String>,
    pub dynamic: DynamicDataRegistries,
    /// Recipe files whose type the cache covers by path, so the warm reload never
    /// re-reads them. Cached here so a warm boot keeps a mod's recipe types.
    pub custom_recipes: Vec<CustomRecipeEntry>,
}

struct Dpc1Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Dpc1Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.offset.checked_add(len)?;
        let data = self.bytes.get(self.offset..end)?;
        self.offset = end;
        Some(data)
    }

    fn byte(&mut self) -> Option<u8> {
        Some(*self.take(1)?.first()?)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn len(&mut self) -> Option<usize> {
        let n = self.u32()? as usize;
        (n <= DPC1_MAX_ENTRIES).then_some(n)
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        if n > DPC1_MAX_STRING {
            return None;
        }
        Some(std::str::from_utf8(self.take(n)?).ok()?.to_owned())
    }
    fn strings(&mut self) -> Option<Vec<String>> {
        let n = self.len()?;
        (0..n).map(|_| self.string()).collect()
    }
    fn ingredients(&mut self) -> Option<Vec<Ingr>> {
        let n = self.len()?;
        (0..n)
            .map(|_| match self.byte()? {
                0 => Some(Ingr::Item(self.i64()?)),
                1 => Some(Ingr::Tag(self.string()?)),
                _ => None,
            })
            .collect()
    }
    fn smithing(&mut self) -> Option<SmithingRecipe> {
        let template = self.ingredients()?;
        let base = self.ingredients()?;
        let addition = self.ingredients()?;
        let result = match self.byte()? {
            0 => None,
            1 => Some(self.i64()?),
            _ => return None,
        };
        Some(SmithingRecipe {
            template,
            base,
            addition,
            result,
        })
    }
    fn map(&mut self) -> Option<std::collections::BTreeMap<String, String>> {
        let n = self.len()?;
        let mut map = std::collections::BTreeMap::new();
        for _ in 0..n {
            if map.insert(self.string()?, self.string()?).is_some() {
                return None;
            }
        }
        Some(map)
    }
}

fn dpc1_u32_into(out: &mut Vec<u8>, v: usize) {
    out.extend_from_slice(&(v as u32).to_le_bytes());
}

fn dpc1_string_into(out: &mut Vec<u8>, value: &str) {
    dpc1_u32_into(out, value.len());
    out.extend_from_slice(value.as_bytes());
}

fn dpc1_strings_into(out: &mut Vec<u8>, list: &[String]) {
    dpc1_u32_into(out, list.len());
    for entry in list {
        dpc1_string_into(out, entry);
    }
}

fn dpc1_ingredients_into(out: &mut Vec<u8>, list: &[Ingr]) {
    dpc1_u32_into(out, list.len());
    for ing in list {
        match ing {
            Ingr::Item(item) => {
                out.push(0);
                out.extend_from_slice(&item.to_le_bytes());
            }
            Ingr::Tag(tag) => {
                out.push(1);
                dpc1_string_into(out, tag);
            }
        }
    }
}

fn dpc1_smithing_into(out: &mut Vec<u8>, recipe: &SmithingRecipe) {
    dpc1_ingredients_into(out, &recipe.template);
    dpc1_ingredients_into(out, &recipe.base);
    dpc1_ingredients_into(out, &recipe.addition);
    match recipe.result {
        None => out.push(0),
        Some(item) => {
            out.push(1);
            out.extend_from_slice(&item.to_le_bytes());
        }
    }
}

fn dpc1_map_into(out: &mut Vec<u8>, map: &std::collections::BTreeMap<String, String>) {
    dpc1_u32_into(out, map.len());
    for (key, value) in map {
        dpc1_string_into(out, key);
        dpc1_string_into(out, value);
    }
}

fn dpc1_encode(snapshot: &Dpc1Snapshot) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut categories: Vec<_> = snapshot.registry.tags.iter().collect();
    categories.sort_unstable_by(|a, b| a.0.cmp(b.0));
    dpc1_u32_into(&mut out, categories.len());
    for (category, tags) in categories {
        dpc1_string_into(&mut out, category);
        let mut sorted: Vec<_> = tags.iter().collect();
        sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
        dpc1_u32_into(&mut out, sorted.len());
        for (name, members) in sorted {
            dpc1_string_into(&mut out, name);
            dpc1_strings_into(&mut out, members);
        }
    }
    dpc1_u32_into(&mut out, snapshot.registry.stonecutting.len());
    for item in &snapshot.registry.stonecutting {
        dpc1_ingredients_into(&mut out, &item.ingredients);
        out.extend_from_slice(&item.result.to_le_bytes());
        out.push(item.count);
    }
    for recipes in [
        &snapshot.registry.smithing_transform,
        &snapshot.registry.smithing_trim,
    ] {
        dpc1_u32_into(&mut out, recipes.len());
        for recipe in recipes {
            dpc1_smithing_into(&mut out, recipe);
        }
    }
    dpc1_map_into(&mut out, &snapshot.registry.special_recipes);
    let cooking = &snapshot.registry.cooking.by_id;
    dpc1_u32_into(&mut out, cooking.len());
    for (id, (priority, recipe)) in cooking {
        dpc1_string_into(&mut out, id);
        out.extend_from_slice(&priority.to_le_bytes());
        out.push(recipe.kind as u8);
        dpc1_ingredients_into(&mut out, &recipe.ingredients);
        out.extend_from_slice(&recipe.result.item.to_le_bytes());
        out.push(recipe.result.count);
        out.extend_from_slice(&recipe.result.game_ticks.to_le_bytes());
        out.extend_from_slice(&recipe.result.experience.to_bits().to_le_bytes());
    }
    dpc1_u32_into(&mut out, snapshot.pack_verdicts.len());
    for (name, verdict) in &snapshot.pack_verdicts {
        dpc1_string_into(&mut out, name);
        match verdict {
            PackFormatVerdict::Compatible => out.push(0),
            PackFormatVerdict::TooOld {
                max_supported: PackVersion(major, minor),
            } => {
                out.push(1);
                out.extend_from_slice(&major.to_le_bytes());
                out.extend_from_slice(&minor.to_le_bytes());
            }
            PackFormatVerdict::TooNew {
                min_supported: PackVersion(major, minor),
            } => {
                out.push(2);
                out.extend_from_slice(&major.to_le_bytes());
                out.extend_from_slice(&minor.to_le_bytes());
            }
            PackFormatVerdict::Invalid => out.push(3),
        }
    }
    dpc1_map_into(&mut out, &snapshot.loot_json);
    // The raw source for each dynamic entry is retained by the reader.
    // Reusing read_entry keeps derived fields consistent with source JSON.
    let mut dynamic = std::collections::BTreeMap::new();
    macro_rules! add_dynamic {
        ($field:ident, $kind:literal) => {
            for (name, value) in &snapshot.dynamic.$field {
                dynamic.insert(format!("{}/{}", $kind, name), value.raw.clone());
            }
        };
    }
    add_dynamic!(enchantment, "enchantment");
    add_dynamic!(damage_type, "damage_type");
    add_dynamic!(chat_type, "chat_type");
    add_dynamic!(wolf_variant, "wolf_variant");
    add_dynamic!(painting_variant, "painting_variant");
    add_dynamic!(banner_pattern, "banner_pattern");
    add_dynamic!(jukebox_song, "jukebox_song");
    dpc1_map_into(&mut out, &dynamic);
    // Appended last: a payload written before this section existed decodes to `None`
    // and falls back to a full parse, so there is no version byte to bump.
    dpc1_u32_into(&mut out, snapshot.custom_recipes.len());
    for entry in &snapshot.custom_recipes {
        dpc1_string_into(&mut out, &entry.id);
        dpc1_string_into(&mut out, &entry.recipe_type);
        dpc1_string_into(&mut out, &entry.raw_json);
    }
    (out.len() <= DPC1_MAX_PAYLOAD).then_some(out)
}

fn dpc1_decode(bytes: &[u8]) -> Option<Dpc1Snapshot> {
    let mut reader = Dpc1Reader { bytes, offset: 0 };
    let mut snapshot = Dpc1Snapshot::default();
    for _ in 0..reader.len()? {
        let category = reader.string()?;
        let mut tags = HashMap::new();
        for _ in 0..reader.len()? {
            if tags.insert(reader.string()?, reader.strings()?).is_some() {
                return None;
            }
        }
        if snapshot.registry.tags.insert(category, tags).is_some() {
            return None;
        }
    }
    for _ in 0..reader.len()? {
        let ingredients = reader.ingredients()?;
        let result = reader.i64()?;
        let count = reader.byte()?;
        if count == 0 || count > 64 {
            return None;
        }
        snapshot.registry.stonecutting.push(StonecuttingRecipe {
            ingredients,
            result,
            count,
        });
    }
    for recipes in [
        &mut snapshot.registry.smithing_transform,
        &mut snapshot.registry.smithing_trim,
    ] {
        for _ in 0..reader.len()? {
            recipes.push(reader.smithing()?);
        }
    }
    snapshot.registry.special_recipes = reader.map()?;
    let cooking = &mut snapshot.registry.cooking;
    let mut priorities = HashSet::new();
    for _ in 0..reader.len()? {
        let id = reader.string()?;
        let priority = reader.u64()?;
        if !priorities.insert(priority) {
            return None;
        }
        cooking.next_priority = cooking.next_priority.max(priority.checked_add(1)?);
        let kind = match reader.byte()? {
            0 => CookingKind::Smelting,
            1 => CookingKind::Blasting,
            2 => CookingKind::Smoking,
            3 => CookingKind::Campfire,
            _ => return None,
        };
        let ingredients = reader.ingredients()?;
        let result = CookingResult {
            item: reader.i64()?,
            count: reader.byte()?,
            game_ticks: reader.u32()?,
            experience: f32::from_bits(reader.u32()?),
        };
        if ingredients.is_empty()
            || !(1..=64).contains(&result.count)
            || result.game_ticks > i32::MAX as u32
            || !result.experience.is_finite()
            || result.experience < 0.0
        {
            return None;
        }
        let recipe = CookingRecipe { kind, ingredients, result };
        if cooking.by_id.insert(id, (priority, recipe)).is_some() {
            return None;
        }
    }
    for _ in 0..reader.len()? {
        let name = reader.string()?;
        let verdict = match reader.byte()? {
            0 => PackFormatVerdict::Compatible,
            1 => PackFormatVerdict::TooOld {
                max_supported: PackVersion(reader.u32()?, reader.u32()?),
            },
            2 => PackFormatVerdict::TooNew {
                min_supported: PackVersion(reader.u32()?, reader.u32()?),
            },
            3 => PackFormatVerdict::Invalid,
            _ => return None,
        };
        if snapshot.pack_verdicts.insert(name, verdict).is_some() {
            return None;
        }
    }
    snapshot.loot_json = reader.map()?;
    for (kind_and_id, body) in reader.map()? {
        let (kind, id) = kind_and_id.split_once('/')?;
        let (namespace, resource) = id.split_once(':')?;
        if namespace.is_empty() || resource.is_empty() {
            return None;
        }
        let path = format!("data/{namespace}/{kind}/{resource}.json");
        snapshot.dynamic.read_entry(&path, &body);
        let accepted = match kind {
            "enchantment" => snapshot.dynamic.enchantment.contains_key(id),
            "damage_type" => snapshot.dynamic.damage_type.contains_key(id),
            "chat_type" => snapshot.dynamic.chat_type.contains_key(id),
            "wolf_variant" => snapshot.dynamic.wolf_variant.contains_key(id),
            "painting_variant" => snapshot.dynamic.painting_variant.contains_key(id),
            "banner_pattern" => snapshot.dynamic.banner_pattern.contains_key(id),
            "jukebox_song" => snapshot.dynamic.jukebox_song.contains_key(id),
            _ => false,
        };
        if !accepted {
            return None;
        }
    }
    for _ in 0..reader.len()? {
        let id = reader.string()?;
        let recipe_type = reader.string()?;
        let raw_json = reader.string()?;
        snapshot.custom_recipes.push(CustomRecipeEntry {
            id,
            recipe_type,
            raw_json,
        });
    }
    (reader.offset == bytes.len()).then_some(snapshot)
}

/// Optional integration boundary. Cache failure (including an unreadable
/// input tree) always falls back to building from the original data.
fn dpc1_load_or_build<F>(
    root: &std::path::Path,
    enabled_order: &[String],
    cache_dir: Option<&std::path::Path>,
    build: F,
) -> Result<(Dpc1Snapshot, bool), String>
where
    F: FnOnce() -> Result<Dpc1Snapshot, String>,
{
    let digest = cache_dir.and_then(|_| compute_datapack_tree_digest(root, enabled_order).ok());
    if let (Some(directory), Some(digest)) = (cache_dir, digest) {
        let path = dpc1_cache_path(directory);
        if let Some(snapshot) = dpc1_read_payload(&path, &digest).and_then(|b| dpc1_decode(&b)) {
            return Ok((snapshot, true));
        }
    }
    let snapshot = build()?;
    if let (Some(directory), Some(initial)) = (cache_dir, digest) {
        // A source edit during parsing must not seed a stale hit.
        if compute_datapack_tree_digest(root, enabled_order).ok() == Some(initial) {
            if let Some(encoded) = dpc1_encode(&snapshot) {
                let _ = dpc1_write_payload(&dpc1_cache_path(directory), &initial, &encoded);
            }
        }
    }
    Ok((snapshot, false))
}

fn dpc1_configured_cache_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("MEGU_DATAPACK_CACHE_DIR")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}

#[cfg(test)]
mod bot_boot_cache_tests {
    use super::*;

    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "spine-bot-boot-cache-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn put(&self, rel: &str, value: &[u8]) {
            let file = self.dir.join(rel);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, value).unwrap();
        }

        fn root(&self) -> std::path::PathBuf {
            self.dir.join("datapacks")
        }
        fn cache(&self) -> std::path::PathBuf {
            self.dir.join("cache")
        }
    }

    impl std::ops::Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).unwrap();
        }
    }

    fn order(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn build_from_json(root: &std::path::Path, packs: &[String]) -> Result<Dpc1Snapshot, String> {
        let mut snapshot = Dpc1Snapshot::default();
        let sources = datapack_sources(root);
        let mut content = DatapackContent::default();
        for name in packs {
            let (_, path, zip) = sources
                .iter()
                .find(|(n, _, _)| n == name)
                .ok_or_else(|| format!("missing {name}"))?;
            if *zip {
                return Err("zip test fixture unsupported".into());
            }
            let tags = path.join("data/test/tags/item/probe.json");
            if tags.exists() {
                let body = std::fs::read_to_string(tags).map_err(|e| e.to_string())?;
                content.read_tag_entry("item", "test:probe", &body);
            }
            let recipe = path.join("data/test/recipe/stone.json");
            if recipe.exists() {
                let body = std::fs::read_to_string(recipe).map_err(|e| e.to_string())?;
                content.read_extended_recipe("test:stone", &body);
            }
        }
        snapshot.registry = content.registry;
        snapshot.pack_verdicts.insert(
            "a".into(),
            PackFormatVerdict::TooNew {
                min_supported: PackVersion(121, 1),
            },
        );
        snapshot
            .loot_json
            .insert("test:loot".into(), "{\"pools\":[]}".into());
        snapshot.dynamic.read_entry(
            "data/test/damage_type/probe.json",
            "{\"message_id\":\"boom\",\"exhaustion\":0.2}",
        );
        Ok(snapshot)
    }

    fn populate(f: &Fixture) {
        f.put("datapacks/a/pack.mcmeta", br#"{"pack":{"pack_format":48}}"#);
        f.put(
            "datapacks/a/data/test/tags/item/probe.json",
            br#"{"replace":true,"values":["minecraft:stone"]}"#,
        );
        f.put("datapacks/a/data/test/recipe/stone.json", br#"{"type":"minecraft:stonecutting","ingredient":{"item":"minecraft:stone"},"result":{"id":"minecraft:diamond","count":2}}"#);
    }

    #[test]
    fn bot_boot_cache_sha256_vectors() {
        for (input, expected) in [
            (
                "",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                "abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
        ] {
            let mut h = Dpc1Sha256::new();
            h.update(input.as_bytes());
            let hex: String = h.finish().iter().map(|v| format!("{v:02x}")).collect();
            assert_eq!(hex, expected);
        }
        let mut hash = Dpc1Sha256::new();
        for chunk in vec![b'a'; 1_000_000].chunks(997) {
            hash.update(chunk);
        }
        let hex: String = hash.finish().iter().map(|v| format!("{v:02x}")).collect();
        assert_eq!(
            hex,
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
        assert_eq!(dpc1_checksum(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn bot_boot_cache_cold_warm_identical_registry_and_auxiliary_data() {
        let f = Fixture::new();
        populate(&f);
        let names = order(&["a"]);
        let root = f.root();
        let (cold, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
            build_from_json(&root, &names)
        })
        .unwrap();
        assert!(!hit);
        let cache_bytes = std::fs::read(f.cache().join("datapacks.dpc1")).unwrap();
        assert!(cache_bytes.starts_with(DPC1_MAGIC));
        let (warm, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
            panic!("cache hit must skip JSON parsing")
        })
        .unwrap();
        assert!(hit);
        assert_eq!(dpc1_encode(&cold), dpc1_encode(&warm));
        assert_eq!(warm.registry.stonecutting.len(), 1);
        assert_eq!(warm.registry.stonecutting[0].count, 2);
        assert_eq!(
            warm.registry.tags["item"]["test:probe"],
            ["minecraft:stone"]
        );
        assert_eq!(warm.pack_verdicts["a"], cold.pack_verdicts["a"]);
        assert_eq!(warm.loot_json, cold.loot_json);
        assert_eq!(warm.dynamic.damage_type["test:probe"].message_id, "boom");
    }

    #[test]
    fn bot_boot_cache_disabled_uses_builder_every_time() {
        let f = Fixture::new();
        populate(&f);
        let root = f.root();
        let names = order(&["a"]);
        for _ in 0..2 {
            let (_, hit) =
                dpc1_load_or_build(&root, &names, None, || build_from_json(&root, &names)).unwrap();
            assert!(!hit);
        }
        assert!(!f.cache().exists());
    }

    #[test]
    fn bot_boot_cache_snapshot_cold_warm_timing() {
        let f = Fixture::new();
        f.put("datapacks/a/pack.mcmeta", br#"{"pack":{"pack_format":48}}"#);
        for index in 0..500 {
            f.put(
                &format!("datapacks/a/data/test/tags/item/tag_{index}.json"),
                r#"{"values":["minecraft:stone","minecraft:dirt","minecraft:diamond","minecraft:coal","minecraft:iron_ingot","minecraft:gold_ingot","minecraft:emerald","minecraft:oak_log","minecraft:birch_log","minecraft:acacia_log","minecraft:netherite_ingot","minecraft:lapis_lazuli"]}"#.as_bytes(),
            );
        }
        let root = f.root();
        let names = order(&["a"]);
        let build = || {
            let mut snapshot = Dpc1Snapshot::default();
            for index in 0..500 {
                let file = root.join(format!("a/data/test/tags/item/tag_{index}.json"));
                let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
                let mut content = DatapackContent::default();
                content.read_tag_entry("item", &format!("test:tag_{index}"), &text);
                snapshot
                    .registry
                    .tags
                    .entry("item".into())
                    .or_default()
                    .extend(content.registry.tags.remove("item").unwrap_or_default());
            }
            Ok(snapshot)
        };
        let start = std::time::Instant::now();
        let (cold, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), build).unwrap();
        let cold_time = start.elapsed();
        assert!(!hit);
        let start = std::time::Instant::now();
        let (warm, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
            panic!("unexpected cold build")
        })
        .unwrap();
        let warm_time = start.elapsed();
        assert!(hit);
        assert_eq!(dpc1_encode(&cold), dpc1_encode(&warm));
        println!("DPC1 snapshot path (500 JSON files): cold={cold_time:?}, warm={warm_time:?}");
    }

    #[test]
    fn bot_boot_cache_single_byte_edit_add_remove_and_metadata() {
        let f = Fixture::new();
        populate(&f);
        let names = order(&["a"]);
        let root = f.root();
        let digest = compute_datapack_tree_digest(&root, &names).unwrap();
        let (_, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
            build_from_json(&root, &names)
        })
        .unwrap();
        assert!(!hit);
        f.put(
            "datapacks/a/data/test/tags/item/probe.json",
            br#"{"replace":true,"values":["minecraft:stove"]}"#,
        );
        assert_ne!(compute_datapack_tree_digest(&root, &names).unwrap(), digest);
        let (new, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
            build_from_json(&root, &names)
        })
        .unwrap();
        assert!(!hit);
        assert_eq!(new.registry.tags["item"]["test:probe"], ["minecraft:stove"]);
        let edit_digest = compute_datapack_tree_digest(&root, &names).unwrap();
        f.put("datapacks/a/pack.mcmeta", br#"{"pack":{"pack_format":49}}"#);
        assert_ne!(
            compute_datapack_tree_digest(&root, &names).unwrap(),
            edit_digest
        );
        let (_, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
            build_from_json(&root, &names)
        })
        .unwrap();
        assert!(!hit, "pack.mcmeta edit must invalidate cache");
        f.put("datapacks/a/data/test/new.txt", b"new");
        let with_extra = compute_datapack_tree_digest(&root, &names).unwrap();
        let (_, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
            build_from_json(&root, &names)
        })
        .unwrap();
        assert!(!hit, "added datapack file must invalidate cache");
        std::fs::remove_file(f.root().join("a/data/test/new.txt")).unwrap();
        assert_ne!(
            compute_datapack_tree_digest(&root, &names).unwrap(),
            with_extra
        );
        let (_, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
            build_from_json(&root, &names)
        })
        .unwrap();
        assert!(!hit, "removed datapack file must invalidate cache");
    }

    #[test]
    fn bot_boot_cache_zip_raw_bytes_invalidation() {
        let f = Fixture::new();
        populate(&f);
        f.put("datapacks/extra.zip", b"raw archive A");
        let names = order(&["a", "extra.zip"]);
        let before = compute_datapack_tree_digest(&f.root(), &names).unwrap();
        f.put("datapacks/extra.zip", b"raw archive B");
        let after = compute_datapack_tree_digest(&f.root(), &names).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn bot_boot_cache_order_change_and_disabled_pack() {
        let f = Fixture::new();
        populate(&f);
        f.put(
            "datapacks/b/data/test/tags/item/probe.json",
            br#"{"replace":true,"values":["minecraft:dirt"]}"#,
        );
        let root = f.root();
        let ab = order(&["a", "b"]);
        let ba = order(&["b", "a"]);
        let (first, hit) =
            dpc1_load_or_build(&root, &ab, Some(&f.cache()), || build_from_json(&root, &ab))
                .unwrap();
        assert!(!hit);
        assert_eq!(
            first.registry.tags["item"]["test:probe"],
            ["minecraft:dirt"]
        );
        assert_ne!(
            compute_datapack_tree_digest(&root, &ab).unwrap(),
            compute_datapack_tree_digest(&root, &ba).unwrap()
        );
        let (second, hit) =
            dpc1_load_or_build(&root, &ba, Some(&f.cache()), || build_from_json(&root, &ba))
                .unwrap();
        assert!(!hit);
        assert_eq!(
            second.registry.tags["item"]["test:probe"],
            ["minecraft:stone"]
        );
        assert_ne!(
            compute_datapack_tree_digest(&root, &ba).unwrap(),
            compute_datapack_tree_digest(&root, &order(&["a"])).unwrap()
        );
    }

    #[test]
    fn bot_boot_cache_every_byte_corruption_and_truncation_falls_back() {
        let f = Fixture::new();
        populate(&f);
        let names = order(&["a"]);
        let root = f.root();
        let path = f.cache().join("datapacks.dpc1");
        dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
            build_from_json(&root, &names)
        })
        .unwrap();
        let valid = std::fs::read(&path).unwrap();
        for index in 0..valid.len() {
            let mut changed = valid.clone();
            changed[index] ^= 1;
            std::fs::write(&path, changed).unwrap();
            let (_, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
                build_from_json(&root, &names)
            })
            .unwrap();
            assert!(!hit, "flipped byte at {index} passed validation");
        }
        for len in 0..valid.len() {
            std::fs::write(&path, &valid[..len]).unwrap();
            let (_, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
                build_from_json(&root, &names)
            })
            .unwrap();
            assert!(!hit, "truncated cache length {len} passed validation");
        }
        let mut wrong_schema = valid.clone();
        wrong_schema[4..8].copy_from_slice(&(DPC1_VERSION + 1).to_le_bytes());
        std::fs::write(&path, wrong_schema).unwrap();
        let (_, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
            build_from_json(&root, &names)
        })
        .unwrap();
        assert!(!hit, "schema version mismatch must invalidate cache");
        std::fs::write(&path, valid).unwrap();
        let (_, hit) = dpc1_load_or_build(&root, &names, Some(&f.cache()), || {
            panic!("valid snapshot missed")
        })
        .unwrap();
        assert!(hit);
    }

    #[cfg(unix)]
    #[test]
    fn bot_boot_cache_symlink_escape_fails_closed() {
        let f = Fixture::new();
        populate(&f);
        std::os::unix::fs::symlink("/etc/passwd", f.root().join("a/data/test/escape.json"))
            .unwrap();
        assert!(compute_datapack_tree_digest(&f.root(), &order(&["a"])).is_err());
        let (_, hit) = dpc1_load_or_build(&f.root(), &order(&["a"]), Some(&f.cache()), || {
            Ok(Dpc1Snapshot::default())
        })
        .unwrap();
        assert!(!hit);
        assert!(!f.cache().join("datapacks.dpc1").exists());
    }
}

/// One `{"item"|"tag": ...}` slot or a bare `"ns:id"` member.
fn dp_ingredient(spec: &str) -> Option<Ingr> {
    if let Some(t) = jstr(spec, "tag") {
        return Some(Ingr::Tag(dp_norm_id(t.trim_start_matches('#'))));
    }
    let bare = jstr(spec, "item").unwrap_or_else(|| spec.trim().trim_matches('"'));
    if bare.is_empty() || bare.starts_with(['{', '[', '}', ']']) {
        return None;
    }
    name_to_item(dp_bare_id(bare)).map(Ingr::Item)
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
            .and_then(|s| name_to_item(dp_bare_id(s)).map(Ingr::Item))
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
    name.and_then(|n| name_to_item(dp_bare_id(n)))
}

fn dp_cooking(kind: CookingKind, json: &J) -> Option<CookingRecipe> {
    fn ingredient(json: &J) -> Option<Ingr> {
        if let Some(tag) = json.get("tag").map(J::str).or_else(|| json.str().strip_prefix('#')) {
            return (!tag.is_empty()).then(|| Ingr::Tag(dp_norm_id(tag)));
        }
        let item = json.get("item").map_or_else(|| json.str(), J::str);
        name_to_item(dp_bare_id(item)).map(Ingr::Item)
    }
    fn integer(json: Option<&J>, default: u32, max: u32) -> Option<u32> {
        match json {
            Some(J::Num(value)) if value.fract() == 0.0 && (0.0..=f64::from(max)).contains(value) => {
                Some(*value as u32)
            }
            None => Some(default),
            _ => None,
        }
    }
    let ingredients = match json.get("ingredient")? {
        J::Arr(values) => values.iter().map(ingredient).collect::<Option<Vec<_>>>()?,
        single => vec![ingredient(single)?],
    };
    if ingredients.is_empty() {
        return None;
    }
    let result = json.get("result")?;
    let name = result.get("id").or_else(|| result.get("item")).unwrap_or(result).str();
    let count = integer(result.get("count").or_else(|| json.get("count")), 1, 64)? as u8;
    if count == 0 {
        return None;
    }
    let experience = match json.get("experience") {
        Some(J::Num(value)) if *value >= 0.0 && *value <= f64::from(f32::MAX) => *value as f32,
        None => 0.0,
        _ => return None,
    };
    Some(CookingRecipe {
        kind,
        ingredients,
        result: CookingResult {
            item: name_to_item(dp_bare_id(name))?,
            count,
            game_ticks: integer(json.get("cookingtime"), kind.default_ticks(), i32::MAX as u32)?,
            experience,
        },
    })
}

// ---------------------------------------------------------------------------
// Non-vanilla recipe types — retained verbatim instead of dropped.
// ---------------------------------------------------------------------------

/// Bound on the retained non-vanilla recipes. A pack tree that declares more is
/// trimmed with a diagnostic: the store exists so a mod's recipe type survives
/// loading, not to keep an unbounded second copy of the datapack tree.
const MAX_CUSTOM_RECIPES: usize = 16_384;

/// One recipe whose `"type"` this port does not model. `raw_json` is the file
/// body exactly as the pack shipped it, so a consumer that does understand the
/// type (`mymod:crushing`) reads what the pack declared instead of a rewrite the
/// generic shaped/shapeless parser guessed at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CustomRecipeEntry {
    pub(crate) id: String,
    /// The authored `type`, namespace included — never the bare short form.
    pub(crate) recipe_type: String,
    pub(crate) raw_json: String,
}

/// The published non-vanilla recipes. `DatapackContent::publish` swaps the whole
/// list, so the newest reload wins and a removed pack's types leave with it.
static CUSTOM_RECIPES: OnceLock<Mutex<Vec<CustomRecipeEntry>>> = OnceLock::new();
fn custom_recipes() -> &'static Mutex<Vec<CustomRecipeEntry>> {
    CUSTOM_RECIPES.get_or_init(|| Mutex::new(Vec::new()))
}

/// The retained non-vanilla recipe types and how many recipes each carries,
/// sorted by type — what the reload diagnostic reports.
pub(crate) fn custom_recipe_types() -> Vec<(String, usize)> {
    let mut types: Vec<(String, usize)> = Vec::new();
    for entry in custom_recipes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        match types.iter_mut().find(|(name, _)| *name == entry.recipe_type) {
            Some((_, count)) => *count += 1,
            None => types.push((entry.recipe_type.clone(), 1)),
        }
    }
    types.sort();
    types
}

/// Every retained recipe of one authored `type` (`mymod:crushing`), in load order.
pub(crate) fn custom_recipes_of_type(recipe_type: &str) -> Vec<CustomRecipeEntry> {
    custom_recipes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|entry| entry.recipe_type == recipe_type)
        .cloned()
        .collect()
}

// A consumer of a mod recipe type lives outside this crate, so nothing here calls
// the per-type lookup. Pinning its signature keeps the surface type-checked.
const _: fn(&str) -> Vec<CustomRecipeEntry> = custom_recipes_of_type;

impl DatapackContent {
    /// Keep one non-vanilla recipe verbatim. A higher-priority pack replaces the
    /// lower copy by id, and the store never grows past `MAX_CUSTOM_RECIPES`.
    fn remember_custom_recipe(&mut self, id: &str, recipe_type: &str, raw_json: &str) {
        let entry = CustomRecipeEntry {
            id: id.to_owned(),
            recipe_type: recipe_type.to_owned(),
            raw_json: raw_json.to_owned(),
        };
        match self.custom_recipes.iter().rposition(|held| held.id == id) {
            Some(index) => self.custom_recipes[index] = entry,
            None if self.custom_recipes.len() >= MAX_CUSTOM_RECIPES => {
                eprintln!("[spine] custom recipe store is full; dropping {id}");
            }
            None => self.custom_recipes.push(entry),
        }
    }
}

impl DatapackContent {
    /// Returns `true` for a consumed type, even if its file was malformed.
    /// A higher-priority recipe of any type replaces the lower copy by ID.
    fn read_extended_recipe(&mut self, key: &str, body: &str) -> bool {
        let cooking = &mut self.registry.cooking;
        cooking.by_id.remove(key);
        let Some(json) = jparse(body.as_bytes()) else {
            eprintln!("[spine] skipping invalid recipe JSON {key}");
            return true;
        };
        let rtype = dp_bare_id(json.get("type").map_or("", J::str));
        if let Some(kind) = CookingKind::from_recipe_type(rtype) {
            if let Some(recipe) = dp_cooking(kind, &json) {
                // The legacy reload test observes this store, but live cooking
                // consumes the typed registry below.
                #[cfg(test)]
                if kind == CookingKind::Smelting {
                    let CookingResult { item, count, experience, .. } = recipe.result;
                    self.smelts.extend(recipe.ingredients.iter().cloned().map(|ingredient| {
                        (ingredient, item, count, experience)
                    }));
                }
                cooking.by_id.insert(key.to_owned(), (cooking.next_priority, recipe));
                cooking.next_priority += 1;
            } else {
                eprintln!("[spine] skipping unresolved or invalid {rtype} recipe {key}");
            }
            return true;
        }
        match rtype {
            "stonecutting" => self.registry.stonecutting.extend(dp_stonecutting(body)),
            "smithing_transform" | "smithing" => {
                self.registry.smithing_transform.extend(dp_smithing(body, true));
            }
            "smithing_trim" => {
                self.registry.smithing_trim.extend(dp_smithing(body, false));
            }
            t if t.starts_with("crafting_special_") || t == "crafting_decorated_pot" => {
                self.registry
                    .special_recipes
                    .insert(key.to_owned(), rtype.to_owned());
            }
            _ => {
                // A type this port does not model is a mod's recipe
                // (`mymod:crushing`): keep the file verbatim instead of dropping
                // it. `false` still hands the body to the legacy loose parser, so
                // a file that used to load through `read_recipe` keeps loading.
                let authored = json.get("type").map_or("", J::str).trim();
                self.remember_custom_recipe(key, authored, body);
                return false;
            }
        }
        true
    }
}

/// One `minecraft:stonecutting` recipe. Files with no resolvable result item or
/// an empty ingredient list are dropped (fail closed), which is why this is
/// fallible rather than a partially-filled recipe.
fn dp_stonecutting(body: &str) -> Option<StonecuttingRecipe> {
    let ingredients = dp_ingredients(body, "ingredient");
    let result = dp_result_item(body)?;
    if ingredients.is_empty() {
        return None;
    }
    Some(StonecuttingRecipe {
        ingredients,
        result,
        count: jblock(body, "result")
            .and_then(|b| jnum(b, "count"))
            .or_else(|| jnum(body, "count"))
            .unwrap_or(1)
            .clamp(1, 64) as u8,
    })
}

/// One `smithing_transform` / `smithing_trim` recipe: template + base +
/// addition slots, dropped when the base slot is unresolvable. Only
/// `smithing_transform` carries a result item (`smithing_trim` keeps the base).
fn dp_smithing(body: &str, with_result: bool) -> Option<SmithingRecipe> {
    let (template, base, addition) = (
        dp_ingredients(body, "template"),
        dp_ingredients(body, "base"),
        dp_ingredients(body, "addition"),
    );
    if base.is_empty() {
        return None;
    }
    Some(SmithingRecipe {
        template,
        base,
        addition,
        result: if with_result {
            dp_result_item(body)
        } else {
            None
        },
    })
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
type BiomeOverrideMap = HashMap<DimId, HashMap<(i32, i32, i32), String>>;

fn biome_overrides() -> &'static Mutex<BiomeOverrideMap> {
    static O: OnceLock<Mutex<BiomeOverrideMap>> = OnceLock::new();
    O.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The biome a quart cell reports now: a `/fillbiome` override wins over the
/// generator's natural `worldgen::biome_at` answer.
fn effective_biome_name(dim: DimId, qx: i32, qy: i32, qz: i32) -> Option<String> {
    if let Some(name) = biome_overrides()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
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
/// logged here; configured features, structures and jigsaw pools cannot run
/// without a chunk generation context (there is no live `FC` seam — `FC`
/// borrows `&mut Gen` and the Gen lock is private to `worldgen.rs`), so their
/// accepted dispatch is the recorded effect. Fields are the record; only tests
/// inspect them today.
#[derive(Clone)]
struct PlaceDispatch {
    dim: DimId,
    pos: (i32, i32, i32),
    id: String,
    kind: PlaceKind,
}

impl std::fmt::Debug for PlaceDispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaceDispatch")
            .field("dim", &self.dim)
            .field("pos", &self.pos)
            .field("id", &self.id)
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaceKind {
    Feature,
    Template,
    Structure,
    Jigsaw,
}

fn place_dispatches() -> &'static Mutex<Vec<PlaceDispatch>> {
    static D: OnceLock<Mutex<Vec<PlaceDispatch>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(Vec::new()))
}

/// `/place template`'s optional `[rotation] [mirror] [integrity] [seed]`
/// arguments — the `StructurePlaceSettings` subset Vanilla's command exposes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TemplatePlacement {
    pub(crate) rotation: crate::structure_transform::Rotation,
    pub(crate) mirror: crate::structure_transform::Mirror,
    /// `BlockRotProcessor(integrity)`: every palette block survives the draw
    /// with this chance; `1.0` keeps all of them.
    pub(crate) integrity: f32,
    /// `RandomSource.create(seed)` — drives the `BlockRotProcessor` draw.
    pub(crate) seed: i64,
}

impl TemplatePlacement {
    /// No arguments: no rotation, no mirror, integrity 1.0, seed 0.
    pub(crate) const DEFAULT: TemplatePlacement = TemplatePlacement {
        rotation: crate::structure_transform::Rotation::None,
        mirror: crate::structure_transform::Mirror::None,
        integrity: 1.0,
        seed: 0,
    };

    /// `BlockRotProcessor.processBlock`: a block survives when
    /// `nextFloat() <= integrity`. The draw is position-derived
    /// (`structure_template::positional_from_world_seed`) so one seed rots one
    /// template the same way on every run.
    fn keeps(&self, pos: (i32, i32, i32)) -> bool {
        self.integrity >= 1.0
            || crate::structure_template::positional_from_world_seed(self.seed as u64, pos)
                .next_f32()
                <= self.integrity
    }
}

/// `/place template` live stamp: load `structures/<id>.nbt`, refuse
/// entity-bearing templates (same fail-closed contract as `place_blocks`),
/// and write palette-0 block states (structure voids skipped, matching
/// vanilla `StructureTemplate.placeInWorld` semantics) after applying the
/// requested rotation, mirror and integrity.
fn place_template_live(
    dim: DimId,
    anchor: (i32, i32, i32),
    id: &str,
    placement: TemplatePlacement,
) -> Result<usize, String> {
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
    if template.palettes.first().is_none_or(Vec::is_empty) {
        return Ok(0);
    }
    let blocks = template
        .transformed_blocks(0, placement.mirror, placement.rotation, (0, 0, 0), (0, 0, 0))
        .map_err(|e| format!("Failed to load template '{id}': {e}"))?;
    let mut placed = 0usize;
    with_save_guard(|| {
        for block in blocks {
            if !placement.keeps(block.pos) {
                continue;
            }
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
struct PackVersion(u32, u32);

/// Compatibility of a `pack.mcmeta` against the server's target format.
#[derive(Clone, Debug, PartialEq, Eq)]
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
                let ns = dpj_str_or(rule, "namespace", ".*");
                let path = dpj_str_or(rule, "path", ".*");
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
    #[cfg(test)]
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
fn parse_and_check_pack_mcmeta(
    json_str: &str,
    target_pack_format: u32,
    target_tuple: (u32, u32),
) -> Result<PackFormatVerdict, String> {
    let m = PackMcmeta::parse(json_str)?;
    if !m.valid {
        return Ok(PackFormatVerdict::Invalid);
    }
    // The 26.3 tuple scheme wins when the pack declares it: both halves are
    // mandatory, so one alone is malformed. Otherwise the scalar range is
    // checked, where a bare `pack_format` is its own single-version range.
    let (target, min, max) = if m.min_format.is_some() || m.max_format.is_some() {
        let (Some(min), Some(max)) = (m.min_format, m.max_format) else {
            return Ok(PackFormatVerdict::Invalid);
        };
        (PackVersion(target_tuple.0, target_tuple.1), min, max)
    } else {
        let Some(pack_format) = m.pack_format else {
            return Ok(PackFormatVerdict::Invalid);
        };
        let (lo, hi) = m.supported.unwrap_or((pack_format, pack_format));
        (
            PackVersion(target_pack_format, 0),
            PackVersion(lo, 0),
            PackVersion(hi, 0),
        )
    };
    Ok(if target > max {
        PackFormatVerdict::TooOld { max_supported: max }
    } else if target < min {
        PackFormatVerdict::TooNew { min_supported: min }
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
const MC_RE_MAX_DEPTH: usize = 32;

struct McReParser<'a> {
    c: &'a [char],
    i: usize,
    depth: usize,
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
            arms.swap_remove(0)
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
            nodes.swap_remove(0)
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
                if self.depth >= MC_RE_MAX_DEPTH {
                    return None;
                }
                self.depth += 1;
                // `(?:...)` non-capturing group prefix is accepted silently.
                if self.peek() == Some('?') {
                    self.i += 1;
                    if self.peek() == Some(':') {
                        self.i += 1;
                    }
                }
                let n = self.alt();
                self.depth -= 1;
                let n = n?;
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
    if pattern.len() > MC_RE_MAX_INSTS {
        return None;
    }
    let chars: Vec<char> = pattern.chars().collect();
    let mut parser = McReParser {
        c: &chars,
        i: 0,
        depth: 0,
    };
    let ast = parser.alt()?;
    if parser.i != chars.len() {
        return None;
    }
    let mut prog = Vec::new();
    mc_re_emit(&ast, &mut prog)?;
    prog.push(McInst::Match);
    mc_re_program_valid(&prog).then_some(prog)
}

fn mc_re_program_valid(prog: &[McInst]) -> bool {
    !prog.is_empty()
        && matches!(prog.last(), Some(McInst::Match))
        && prog.iter().all(|inst| match inst {
            McInst::Char(c) => {
                let _ = c;
                true
            }
            McInst::Any | McInst::Match => true,
            McInst::Assert(start) => {
                let _ = start;
                true
            }
            McInst::Class {
                neg,
                ranges,
                predef,
            } => {
                let _ = neg;
                *predef <= 0b111 && ranges.iter().all(|(lo, hi)| lo <= hi)
            }
            McInst::Split(a, b) => *a < prog.len() && *b < prog.len(),
            McInst::Jmp(target) => *target < prog.len(),
        })
}

#[cfg(test)]
fn mc_re_predef(predef: u8, c: char) -> bool {
    (predef & 1 != 0 && c.is_ascii_digit())
        || (predef & 2 != 0 && (c.is_ascii_alphanumeric() || c == '_'))
        || (predef & 4 != 0 && c.is_whitespace())
}

/// Epsilon-closure: mark every consuming state reachable from `pc` at text
/// position `pos`. `Assert` states consume nothing — they gate the path
/// through to `pc + 1`.
#[cfg(test)]
fn mc_re_add(prog: &[McInst], states: &mut [bool], pc: usize, pos: usize, len: usize) {
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
#[cfg(test)]
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
#[cfg(test)]
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
#[cfg(test)]
#[derive(Clone, Debug)]
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

#[cfg(test)]
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
#[cfg(test)]
fn loot_num(j: Option<&J>, default: LootNum) -> LootNum {
    let Some(j) = j else { return default };
    match j {
        J::Num(n) => LootNum::Constant(*n),
        J::Obj(_) => {
            let bare = |k: &str| dpj_f64(j, k);
            match dpj_ns_id(j, "type").as_str() {
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
#[cfg(test)]
fn loot_jbool(j: &J, key: &str) -> bool {
    match j.get(key) {
        Some(J::Num(n)) => *n != 0.0,
        Some(J::Str(s)) => s == "true",
        _ => false,
    }
}

/// One `match_tool` `items` element: a plain id or a `#tag`.
#[cfg(test)]
#[derive(Clone, Debug)]
enum LootItemMatcher {
    Id(String),
    Tag(String),
}

/// One `match_tool` enchantment predicate: ids set + optional level bounds.
#[cfg(test)]
#[derive(Clone, Debug)]
struct MatchToolEnchant {
    ids: Vec<String>,
    min: Option<i32>,
    max: Option<i32>,
}

/// The implemented `minecraft:` loot conditions (`evc` registry subset).
/// `Unsupported` is the fail-closed bucket: an unknown condition type can
/// never turn a drop on.
#[cfg(test)]
#[derive(Clone, Debug)]
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
#[cfg(test)]
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
#[cfg(test)]
fn loot_match_tool_enchants(j: Option<&J>) -> Vec<MatchToolEnchant> {
    let mut out = Vec::new();
    for e in dpj_opt_arr(j) {
        // Exact parity with the original reader: only bare strings count, and
        // an array element that is not a string contributes `""` (kept, so a
        // non-empty list still yields an entry).
        let ids: Vec<String> = match e.get("enchantments").or_else(|| e.get("enchantment")) {
            Some(J::Str(s)) => vec![s.clone()],
            Some(J::Arr(a)) => a.iter().map(|v| v.str().to_owned()).collect(),
            _ => Vec::new(),
        };
        if ids.is_empty() {
            continue;
        }
        let (mut min, mut max) = (None, None);
        match e.get("levels") {
            Some(J::Num(n)) => {
                min = Some(*n as i32);
                max = Some(*n as i32);
            }
            Some(levels) => {
                min = levels.get("min").map(|v| v.num() as i32);
                max = levels.get("max").map(|v| v.num() as i32);
            }
            None => {}
        }
        out.push(MatchToolEnchant { ids, min, max });
    }
    out
}

#[cfg(test)]
fn loot_condition(j: &J) -> LootCondition {
    let id = dpj_ns_id(j, "condition");
    match id.as_str() {
        "survives_explosion" => LootCondition::SurvivesExplosion,
        "random_chance" => {
            LootCondition::RandomChance(loot_num(j.get("chance"), LootNum::Unsupported))
        }
        "random_chance_with_enchanted_bonus" => LootCondition::RandomChanceEnchantedBonus {
            enchantment: dpj_str(j, "enchantment"),
            unenchanted: j
                .get("unenchanted_chance")
                .or_else(|| j.get("base_chance"))
                .map_or(0.0, J::num) as f32,
            enchanted: loot_num(j.get("enchanted_chance"), LootNum::Unsupported),
        },
        "killed_by_player" => LootCondition::KilledByPlayer,
        "inverted" => j.get("term").map_or(LootCondition::Unsupported, |t| {
            LootCondition::Inverted(Box::new(loot_condition(t)))
        }),
        "any_of" | "all_of" => {
            let Some(terms) = j.get("terms").and_then(J::as_arr) else {
                return LootCondition::Unsupported;
            };
            let terms = terms.iter().map(loot_condition).collect::<Vec<_>>();
            if id == "any_of" {
                LootCondition::AnyOf(terms)
            } else {
                LootCondition::AllOf(terms)
            }
        }
        "match_tool" => {
            let pred = j.get("predicate").cloned().unwrap_or(J::Obj(Vec::new()));
            LootCondition::MatchTool {
                items: loot_item_matchers(pred.get("items").or_else(|| pred.get("item"))),
                enchantments: loot_match_tool_enchants(pred.get("enchantments")),
            }
        }
        _ => LootCondition::Unsupported,
    }
}

#[cfg(test)]
fn loot_conditions(j: Option<&J>) -> Vec<LootCondition> {
    dpj_opt_arr(j).iter().map(loot_condition).collect()
}

/// `apply_bonus` formula id + parameters.
#[cfg(test)]
#[derive(Clone, Debug)]
enum ApplyBonusFormula {
    OreDrops,
    UniformBonusCount { bonus_multiplier: i32 },
    BinomialWithBonusCount { extra: i32, probability: f32 },
}

/// The implemented `minecraft:` loot functions (`eth` registry subset).
/// `Unsupported` functions are parse-time kept but evaluation no-ops.
#[cfg(test)]
#[derive(Clone, Debug)]
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
#[cfg(test)]
#[derive(Clone, Debug)]
struct LootFuncNode {
    func: LootFunction,
    conditions: Vec<LootCondition>,
}

#[cfg(test)]
fn loot_function(j: &J) -> LootFunction {
    let id = dpj_ns_id(j, "function");
    match id.as_str() {
        "set_count" => LootFunction::SetCount {
            provider: loot_num(j.get("count"), LootNum::Constant(0.0)),
            add: loot_jbool(j, "add"),
        },
        "apply_bonus" => {
            let params = j.get("parameters");
            let formula = match dpj_ns_id(j, "formula").as_str() {
                "ore_drops" => ApplyBonusFormula::OreDrops,
                "uniform_bonus_count" => ApplyBonusFormula::UniformBonusCount {
                    bonus_multiplier: params.map_or(0, |p| dpj_i32(p, "bonusMultiplier")),
                },
                "binomial_with_bonus_count" => ApplyBonusFormula::BinomialWithBonusCount {
                    extra: params.map_or(0, |p| dpj_i32(p, "extra")),
                    probability: params.map_or(0.0, |p| dpj_f64(p, "probability")) as f32,
                },
                _ => return LootFunction::Unsupported,
            };
            LootFunction::ApplyBonus {
                enchantment: dpj_str(j, "enchantment"),
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
                Some(limit) => {
                    min = limit
                        .get("min")
                        .map(|v| loot_num(Some(v), LootNum::Unsupported));
                    max = limit
                        .get("max")
                        .map(|v| loot_num(Some(v), LootNum::Unsupported));
                }
                None => {}
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

#[cfg(test)]
fn loot_func_nodes(j: Option<&J>) -> Vec<LootFuncNode> {
    dpj_opt_arr(j)
        .iter()
        .map(|f| LootFuncNode {
            func: loot_function(f),
            conditions: loot_conditions(f.get("conditions")),
        })
        .collect()
}

/// `esf` LootPoolEntries: every kind a pool or composite child can be.
#[cfg(test)]
#[derive(Clone, Debug)]
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
#[cfg(test)]
#[derive(Clone, Debug)]
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

#[cfg(test)]
fn loot_entry_node(j: &J) -> LootEntryNode {
    let kind = match dpj_ns_id(j, "type").as_str() {
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
        name: dpj_str(j, "name"),
        weight: j.get("weight").map_or(1, |v| v.num() as i32),
        quality: dpj_i32(j, "quality"),
        conditions: loot_conditions(j.get("conditions")),
        functions: loot_func_nodes(j.get("functions")),
        children: dpj_map(j, "children", loot_entry_node),
    }
}

#[cfg(test)]
struct LootPoolDef {
    rolls: LootNum,
    bonus_rolls: LootNum,
    conditions: Vec<LootCondition>,
    entries: Vec<LootEntryNode>,
    functions: Vec<LootFuncNode>,
}

#[cfg(test)]
struct LootTableDef {
    pools: Vec<LootPoolDef>,
    functions: Vec<LootFuncNode>,
}

#[cfg(test)]
fn loot_table_def(j: &J) -> LootTableDef {
    LootTableDef {
        pools: dpj_map(j, "pools", |p| LootPoolDef {
            rolls: loot_num(p.get("rolls"), LootNum::Constant(1.0)),
            bonus_rolls: loot_num(p.get("bonus_rolls"), LootNum::Constant(0.0)),
            conditions: loot_conditions(p.get("conditions")),
            entries: dpj_map(p, "entries", loot_entry_node),
            functions: loot_func_nodes(p.get("functions")),
        }),
        functions: loot_func_nodes(j.get("functions")),
    }
}

/// One stack produced by the evaluator.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct LootDrop {
    /// Namespaced item id (`minecraft:diamond`).
    item: String,
    count: i32,
    /// Damage value; `0` is undamaged.
    damage: i32,
}

/// The tool the roll attributes `match_tool` / `enchantment` lookups to
/// (vanilla's `TOOL` loot-context parameter).
#[cfg(test)]
#[derive(Clone, Debug, Default)]
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
#[cfg(test)]
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

#[cfg(test)]
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
        let bare = dp_bare_id(id);
        tool.enchantments
            .get(id)
            .or_else(|| tool.enchantments.get(bare))
            .copied()
            .unwrap_or(0)
    }
}

/// Expanded member list of an item tag via the datapack registry, normalized
/// to namespaced ids.
#[cfg(test)]
fn loot_item_tag_members(tag: &str) -> Vec<String> {
    datapack_tag_members("item", &dp_norm_id(tag))
        .into_iter()
        .map(|m| dp_norm_id(&m))
        .collect()
}

/// `#tag` membership test that accepts both the builtin bare members and the
/// datapack-loaded namespaced ones.
#[cfg(test)]
fn loot_item_in_tag(tag: &str, item: &str) -> bool {
    let tag = dp_norm_id(tag);
    let item = dp_norm_id(item);
    datapack_tag_contains("item", &tag, &item)
        || datapack_tag_contains("item", &tag, dp_bare_id(&item))
}

#[cfg(test)]
fn loot_match_tool(items: &Option<Vec<LootItemMatcher>>, enchants: &[MatchToolEnchant], ctx: &LootEvalContext) -> bool {
    // Vanilla `evj`: no TOOL parameter means the condition fails even with an
    // empty predicate.
    let Some(tool) = &ctx.tool else {
        return false;
    };
    if let Some(matchers) = items {
        let tool_item = dp_norm_id(&tool.item);
        if !matchers.iter().any(|m| match m {
            LootItemMatcher::Id(id) => dp_norm_id(id) == tool_item,
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

#[cfg(test)]
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

#[cfg(test)]
fn loot_conditions_pass(conditions: &[LootCondition], ctx: &mut LootEvalContext) -> bool {
    conditions.iter().all(|c| loot_condition_holds(c, ctx))
}

/// A weighted candidate emitted by `loot_expand_entry` (`esg` LootPoolEntry).
/// `member` is set only for `expand: true` tag members — those emit their
/// item directly and skip the entry's functions (the `esm$1` quirk).
#[cfg(test)]
struct LootCandidate<'a> {
    node: &'a LootEntryNode,
    member: Option<String>,
}

/// `esh.expand`: conditions first, then emit candidates through the consumer.
/// Returns whether the container expanded (what alternatives/sequence read).
#[cfg(test)]
fn loot_expand_entry<'a>(
    node: &'a LootEntryNode,
    ctx: &mut LootEvalContext,
    out: &mut Vec<LootCandidate<'a>>,
) -> bool {
    if !loot_conditions_pass(&node.conditions, ctx) {
        return false;
    }
    match &node.kind {
        LootEntryKind::Empty
        | LootEntryKind::Item
        | LootEntryKind::Table
        | LootEntryKind::Tag { expand: false } => {
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
#[cfg(test)]
fn loot_eff_weight(node: &LootEntryNode, luck: f32) -> i32 {
    (node.weight as f32 + node.quality as f32 * luck)
        .floor()
        .max(0.0) as i32
}

/// `furnace_smelt`: the first smelting recipe whose ingredient matches.
#[cfg(test)]
fn loot_smelt_result(item: &str) -> Option<String> {
    let item_id = name_to_item(dp_bare_id(item))?;
    smelt_out(item_id).and_then(|(output, _)| item_to_name(output)).map(dp_norm_id)
}

#[cfg(test)]
fn loot_apply_func_nodes(drop: &mut LootDrop, nodes: &[LootFuncNode], ctx: &mut LootEvalContext) {
    for node in nodes {
        if loot_conditions_pass(&node.conditions, ctx) {
            loot_apply_function(drop, &node.func, ctx);
        }
    }
}

#[cfg(test)]
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

/// `esj$1`: one emitted stack — the entry's composed functions run on it and a
/// non-positive count drops it. Shared by every candidate source below.
#[cfg(test)]
fn loot_emit(
    mut drop: LootDrop,
    node: &LootEntryNode,
    ctx: &mut LootEvalContext,
    out: &mut Vec<LootDrop>,
) {
    loot_apply_func_nodes(&mut drop, &node.functions, ctx);
    if drop.count > 0 {
        out.push(drop);
    }
}

/// `esj$c.a(F)`: the picked candidate emits stacks through the entry's composed
/// functions (except expanded tag members, which skip them).
#[cfg(test)]
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
    let stack = |item: String| LootDrop {
        item,
        count: 1,
        damage: 0,
    };
    match &node.kind {
        LootEntryKind::Item => loot_emit(stack(dp_norm_id(&node.name)), node, ctx, out),
        LootEntryKind::Tag { expand: false } => {
            for member in loot_item_tag_members(&node.name) {
                loot_emit(stack(member), node, ctx, out);
            }
        }
        LootEntryKind::Table => {
            for drop in evaluate_loot_table(&node.name, ctx) {
                loot_emit(drop, node, ctx, out);
            }
        }
        _ => {}
    }
}

/// `erv.b` — ONE roll: expand every entry (conditions evaluated now), keep
/// positive-weight candidates, weighted pick.
#[cfg(test)]
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

#[cfg(test)]
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
#[cfg(test)]
const LOOT_EVAL_MAX_DEPTH: usize = 64;

/// Evaluate the loot table `id` (a datapack `loot_table` JSON looked up via
/// `loot_table_json`; a bare path reads as `minecraft:`). Unknown tables and
/// cyclic references evaluate to nothing — never panics.
#[cfg(test)]
fn evaluate_loot_table(id: &str, ctx: &mut LootEvalContext) -> Vec<LootDrop> {
    let id = dp_norm_id(id);
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
#[cfg(test)]
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

/// `data/<ns>/<registry>/<path>.json` → (`registry`, `ns:path`) for a file the
/// dynamic-registry reader owns; `None` for anything else (fail closed).
fn dyn_entry_id(path: &str) -> Option<(&'static str, String)> {
    let (ns, rest) = path.strip_prefix("data/")?.split_once('/')?;
    let (dir, rel) = rest.split_once('/')?;
    let rel = rel.strip_suffix(".json")?;
    if rel.is_empty() {
        return None;
    }
    Some((dyn_registry_kind(dir)?, format!("{ns}:{rel}")))
}

/// A text component: `{text}` / `{translate}` object or a bare string.
#[cfg(test)]
fn dyn_jtext(j: &J, key: &str) -> String {
    match j.get(key) {
        Some(d @ J::Obj(_)) => {
            d.get("text")
                .or_else(|| d.get("translate"))
                .map_or_else(String::new, |v| v.str().to_owned())
        }
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
                J::Obj(_) => dpj_str(v, "id"),
                other => other.str().to_owned(),
            })
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// A `chat_type` decoration object (`{translation_key, parameters}`).
#[derive(Clone, Debug)]
struct ChatDecoration {
    translation_key: String,
    parameters: Vec<String>,
}

#[derive(Clone, Debug)]
struct DatapackEnchantment {
    raw: String,
    #[cfg(test)]
    description: String,
    supported_items: Vec<String>,
    exclusive_set: Vec<String>,
    weight: i64,
    max_level: i64,
    anvil_cost: i64,
    slots: Vec<String>,
}

#[derive(Clone, Debug)]
struct DatapackDamageType {
    raw: String,
    message_id: String,
    exhaustion: f64,
    scaling: String,
    effects: Option<String>,
}

/// Vanilla `DamageType.DamageScaling` — the three registry spellings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DamageScaling {
    Never,
    WhenCausedByLivingNonPlayer,
    Always,
}

impl DamageScaling {
    /// The `scaling` field. Vanilla's codec rejects anything else, so an
    /// unknown spelling keeps the documented default instead of guessing.
    fn parse(value: &str) -> Self {
        match value {
            "never" => Self::Never,
            "always" => Self::Always,
            _ => Self::WhenCausedByLivingNonPlayer,
        }
    }

    /// `DamageSource.scalesWithDifficulty`: `never` and `always` ignore the
    /// cause, the middle spelling needs a living non-player cause.
    pub(crate) fn scales_with_difficulty(self, caused_by_living_non_player: bool) -> bool {
        match self {
            Self::Never => false,
            Self::WhenCausedByLivingNonPlayer => caused_by_living_non_player,
            Self::Always => true,
        }
    }
}

/// The runtime view of one loaded `damage_type` registry entry. `/damage`
/// reads difficulty scaling, food exhaustion, the hurt-sound family and the
/// `death.attack.*` message key from the same parsed entry instead of a second
/// hand-maintained table.
#[derive(Clone, Debug)]
pub(crate) struct DamageTypeProfile {
    pub(crate) scaling: DamageScaling,
    pub(crate) exhaustion: f64,
    /// `DamageEffects` member (`burning`, `drowning`, `freezing`, `poking`,
    /// `thorns`); absent means the plain `hurt` family.
    pub(crate) effects: Option<String>,
    /// Vanilla `message_id`, the `death.attack.<id>` translation key suffix.
    pub(crate) message_id: String,
}

/// A loaded `damage_type` entry (`ns:path` or a bare path, `minecraft:`
/// implied). `None` when the registry has no such entry — a datapack-only id
/// before its pack is enabled, or a bare test tree.
pub(crate) fn datapack_damage_type(id: &str) -> Option<DamageTypeProfile> {
    let registry = dynamic_registries().lock().unwrap_or_else(|e| e.into_inner());
    let entry = registry.damage_type.get(&dp_norm_id(id))?;
    Some(DamageTypeProfile {
        scaling: DamageScaling::parse(&entry.scaling),
        exhaustion: entry.exhaustion,
        effects: entry.effects.clone(),
        message_id: entry.message_id.clone(),
    })
}

#[derive(Clone, Debug)]
struct DatapackChatType {
    raw: String,
    chat: Option<ChatDecoration>,
    narration: Option<ChatDecoration>,
}

#[derive(Clone, Debug)]
struct DatapackWolfVariant {
    raw: String,
    wild_texture: String,
    tame_texture: String,
    angry_texture: String,
    biomes: Vec<String>,
}

#[derive(Clone, Debug)]
struct DatapackPaintingVariant {
    raw: String,
    asset_id: String,
    width: i64,
    height: i64,
}

#[derive(Clone, Debug)]
struct DatapackBannerPattern {
    raw: String,
    asset_id: String,
    translation_key: String,
}

#[derive(Clone, Debug)]
struct DatapackJukeboxSong {
    raw: String,
    sound_event: String,
    #[cfg(test)]
    description: String,
    length_in_seconds: f64,
    comparator_output: i64,
}

/// The seven data-driven registries, each keyed `ns:path` in sorted order.
#[derive(Default)]
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
    /// Files outside a known registry or that do not parse as objects are
    /// dropped (fail closed), like the tag/recipe readers.
    fn read_entry(&mut self, path: &str, body: &str) {
        let Some((kind, id)) = dyn_entry_id(path) else {
            return;
        };
        let Some(j) = jparse(body.as_bytes()) else {
            return;
        };
        match kind {
            "enchantment" => {
                self.enchantment.insert(id, dyn_enchantment(body, &j));
            }
            "damage_type" => {
                let entry = dyn_damage_type(body, &j);
                if dyn_damage_type_usable(&entry) {
                    self.damage_type.insert(id, entry);
                }
            }
            "chat_type" => {
                let entry = dyn_chat_type(body, &j);
                if dyn_chat_type_usable(&entry) {
                    self.chat_type.insert(id, entry);
                }
            }
            "wolf_variant" => {
                let entry = dyn_wolf_variant(body, &j);
                if dyn_wolf_variant_usable(&entry) {
                    self.wolf_variant.insert(id, entry);
                }
            }
            "painting_variant" => {
                let entry = dyn_painting_variant(body, &j);
                if dyn_painting_variant_usable(&entry) {
                    self.painting_variant.insert(id, entry);
                }
            }
            "banner_pattern" => {
                self.banner_pattern.insert(id, dyn_banner_pattern(body, &j));
            }
            "jukebox_song" => {
                let entry = dyn_jukebox_song(body, &j);
                if dyn_jukebox_song_usable(&entry, &j) {
                    self.jukebox_song.insert(id, entry);
                }
            }
            _ => {}
        }
    }
}

/// The per-registry field readers. Every one keeps the file body verbatim in
/// `raw` so a later RegistryData-sync lane can emit byte-faithful entries
/// instead of reserializing typed fields. Missing fields keep the same
/// zero/empty defaults the inline reader used.
fn dyn_enchantment(body: &str, j: &J) -> DatapackEnchantment {
    DatapackEnchantment {
        raw: body.to_owned(),
        #[cfg(test)]
        description: dyn_jtext(j, "description"),
        supported_items: dyn_jstr_list(j, "supported_items"),
        exclusive_set: dyn_jstr_list(j, "exclusive_set"),
        weight: dpj_i64(j, "weight"),
        max_level: dpj_i64(j, "max_level"),
        anvil_cost: dpj_i64(j, "anvil_cost"),
        slots: dyn_jstr_list(j, "slots"),
    }
}

fn dyn_damage_type(body: &str, j: &J) -> DatapackDamageType {
    DatapackDamageType {
        raw: body.to_owned(),
        message_id: dpj_str(j, "message_id"),
        exhaustion: dpj_f64(j, "exhaustion"),
        scaling: dpj_str_or(j, "scaling", "when_caused_by_living_non_player"),
        effects: j.get("effects").map(|v| v.str().to_owned()),
    }
}

/// Codec floor for a `damage_type` entry, mirroring the enum/string fields
/// the vanilla `DamageType` codec requires: `message_id` and `exhaustion`
/// are mandatory, `scaling` only accepts the three `DamageScaling` values,
/// and `effects` — when present — must be a `DamageEffects` member. Entries
/// the codec would reject are dropped here like the other fail-closed
/// readers.
fn dyn_damage_type_usable(entry: &DatapackDamageType) -> bool {
    const SCALING: &[&str] = &["never", "always", "when_caused_by_living_non_player"];
    const EFFECTS: &[&str] = &["hurt", "thorns", "drowning", "burning", "poking", "freezing"];
    !entry.message_id.is_empty()
        && entry.exhaustion.is_finite()
        && SCALING.contains(&entry.scaling.as_str())
        && entry
            .effects
            .as_deref()
            .is_none_or(|effect| EFFECTS.contains(&effect))
}

fn dyn_decoration(j: &J, key: &str) -> Option<ChatDecoration> {
    j.get(key).map(|d| ChatDecoration {
        translation_key: dpj_str(d, "translation_key"),
        parameters: dyn_jstr_list(d, "parameters"),
    })
}

/// Codec floor for `chat_type`: every decoration present must carry its
/// required `translation_key`, and parameter names stay nonempty.
fn dyn_chat_type_usable(entry: &DatapackChatType) -> bool {
    entry
        .chat
        .iter()
        .chain(entry.narration.iter())
        .all(|d| !d.translation_key.is_empty() && d.parameters.iter().all(|p| !p.is_empty()))
}

fn dyn_chat_type(body: &str, j: &J) -> DatapackChatType {
    DatapackChatType {
        raw: body.to_owned(),
        chat: dyn_decoration(j, "chat"),
        narration: dyn_decoration(j, "narration"),
    }
}

fn dyn_wolf_variant(body: &str, j: &J) -> DatapackWolfVariant {
    DatapackWolfVariant {
        raw: body.to_owned(),
        wild_texture: dpj_str(j, "wild_texture"),
        tame_texture: dpj_str(j, "tame_texture"),
        angry_texture: dpj_str(j, "angry_texture"),
        biomes: dyn_jstr_list(j, "biomes"),
    }
}

/// Codec floor for `wolf_variant`: the three texture ids are required
/// resource locations and every biome selector stays a nonempty id/tag.
fn dyn_wolf_variant_usable(entry: &DatapackWolfVariant) -> bool {
    !entry.wild_texture.is_empty()
        && !entry.tame_texture.is_empty()
        && !entry.angry_texture.is_empty()
        && entry.biomes.iter().all(|b| !b.is_empty())
}

fn dyn_painting_variant(body: &str, j: &J) -> DatapackPaintingVariant {
    DatapackPaintingVariant {
        raw: body.to_owned(),
        asset_id: dpj_str(j, "asset_id"),
        width: dpj_i64(j, "width"),
        height: dpj_i64(j, "height"),
    }
}

/// Codec floor for `painting_variant`: `asset_id` is a required resource
/// location and the canvas must have a positive extent to place at all.
fn dyn_painting_variant_usable(entry: &DatapackPaintingVariant) -> bool {
    !entry.asset_id.is_empty() && entry.width > 0 && entry.height > 0
}

fn dyn_banner_pattern(body: &str, j: &J) -> DatapackBannerPattern {
    DatapackBannerPattern {
        raw: body.to_owned(),
        asset_id: dpj_str(j, "asset_id"),
        translation_key: dpj_str(j, "translation_key"),
    }
}

fn dyn_jukebox_song(body: &str, j: &J) -> DatapackJukeboxSong {
    DatapackJukeboxSong {
        raw: body.to_owned(),
        sound_event: dpj_str(j, "sound_event"),
        #[cfg(test)]
        description: dyn_jtext(j, "description"),
        length_in_seconds: dpj_f64(j, "length_in_seconds"),
        comparator_output: dpj_i64(j, "comparator_output"),
    }
}

/// Codec floor for `jukebox_song`: `sound_event` is required and may be a
/// bare id or an inline sound-event object (only the string form lands in
/// the typed field), `length_in_seconds` must be positive, and
/// `comparator_output` is the codec's 0–15 redstone range.
fn dyn_jukebox_song_usable(entry: &DatapackJukeboxSong, j: &J) -> bool {
    let sound_ok = match j.get("sound_event") {
        Some(J::Str(_)) => !entry.sound_event.is_empty(),
        Some(J::Obj(_)) => true,
        _ => false,
    };
    sound_ok
        && entry.length_in_seconds.is_finite()
        && entry.length_in_seconds > 0.0
        && (0..=15).contains(&entry.comparator_output)
}

/// Scan `base` (the `datapacks/` directory) the same way `reload_datapacks`
/// does — alphabetical sources, disabled packs skipped, `.zip` packs through
/// the bounded ZIP reader — and merge every recognized registry entry,
/// later packs winning.
#[cfg(test)]
fn load_dynamic_registries(base: &std::path::Path) -> DynamicDataRegistries {
    let mut regs = DynamicDataRegistries::default();
    let disabled = disabled_datapacks().lock().unwrap_or_else(|e| e.into_inner()).clone();
    for (name, path, zip) in datapack_sources(base) {
        if disabled.contains(&name) {
            continue;
        }
        let entries = if zip {
            dyn_zip_entries(&path)
        } else {
            dyn_dir_entries(&path)
        };
        for (path, body) in entries {
            regs.read_entry(&path, &body);
        }
    }
    regs
}

/// One enabled `.zip` pack's `(path, body)` entries, bounded by
/// `ZIP_MAX_BYTES`. An unreadable or oversized archive contributes nothing.
#[cfg(test)]
fn dyn_zip_entries(path: &std::path::Path) -> Vec<(String, String)> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(ZIP_MAX_BYTES as u64 + 1).read_to_end(&mut bytes))
        .ok()
        .filter(|&len| len <= ZIP_MAX_BYTES)
        .and_then(|_| zip_datapack_entries(&bytes))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(path, body)| {
            let body = std::str::from_utf8(&body).ok()?.to_owned();
            Some((path, body))
        })
        .collect()
}

/// One enabled directory pack's `(path, body)` entries under its `data/`
/// subtree, sorted so the merge order is path-deterministic.
#[cfg(test)]
fn dyn_dir_entries(pack: &std::path::Path) -> Vec<(String, String)> {
    let mut dirs = vec![pack.join("data")];
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
            } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "json") {
                files.push(path);
            }
        }
    }
    files.sort_unstable();
    files
        .into_iter()
        .filter_map(|file| {
            let path = file.strip_prefix(pack).ok()?.to_str()?;
            let body = std::fs::read_to_string(&file).ok()?;
            Some((path.replace('\\', "/"), body))
        })
        .collect()
}

/// Published dynamic-registry snapshot for later configuration-sync lanes,
/// refreshed from `datapacks/` on demand.
fn dynamic_registries() -> &'static Mutex<DynamicDataRegistries> {
    static R: OnceLock<Mutex<DynamicDataRegistries>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(DynamicDataRegistries::default()))
}

// ------------------------------------------------------------ p3_parity ---

#[cfg(all(test, megu_vanilla_data))]
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
            let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
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
        assert!(block_tags().lock().unwrap_or_else(|e| e.into_inner()).contains_key("minecraft:logs"));
        assert!(block_tags().lock().unwrap_or_else(|e| e.into_inner()).contains_key("test:probe"));
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

        let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
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
        let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
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

    // ------------------------------------------------------------- P1-6 ---

    /// `reload_datapacks` over a synthetic pack, then drop it and re-publish so
    /// the globals other tests read are back to the pack-free state.
    fn with_pack<T>(pack: &[(String, String)], run: impl FnOnce() -> T) -> T {
        let _dir = P5gDirectory::enter();
        let root = Path::new("datapacks");
        for (path, body) in pack {
            write_file(&root.join(path), body);
        }
        reload_datapacks(root, None, false).expect("synthetic pack loads");
        let out = run();
        std::fs::remove_dir_all(root).expect("fixture packs removed");
        reload_datapacks(root, None, false).expect("pack-free reload");
        out
    }

    #[test]
    fn p16_damage_type_entry_feeds_scaling_exhaustion_message_and_effects() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        with_pack(
            &[(
                "a/data/test/damage_type/zap.json".to_owned(),
                r#"{"message_id":"zap.death","exhaustion":0.25,"scaling":"never","effects":"burning"}"#
                    .to_owned(),
            )],
            || {
                // The loaded entry is the runtime source of truth: the fields
                // used to live behind `#[cfg(test)]`, so nothing could read the
                // difficulty scaling, exhaustion, effects or message id.
                let profile = datapack_damage_type("test:zap").expect("loaded entry");
                assert_eq!(profile.scaling, DamageScaling::Never);
                assert_eq!(profile.exhaustion, 0.25);
                assert_eq!(profile.message_id, "zap.death");
                assert_eq!(profile.effects.as_deref(), Some("burning"));
                // A bare path normalises to `minecraft:`; an unknown id is None.
                assert!(datapack_damage_type("test:missing").is_none());
                assert!(datapack_damage_type("not_a_type").is_none());
            },
        );
        // `DamageSource.scalesWithDifficulty` semantics.
        assert!(!DamageScaling::Never.scales_with_difficulty(true));
        assert!(DamageScaling::Always.scales_with_difficulty(false));
        assert!(DamageScaling::WhenCausedByLivingNonPlayer.scales_with_difficulty(true));
        assert!(!DamageScaling::WhenCausedByLivingNonPlayer.scales_with_difficulty(false));
        // An entry with no `scaling` field reads as Vanilla's codec default.
        with_pack(
            &[(
                "a/data/test/damage_type/plain.json".to_owned(),
                r#"{"message_id":"plain","exhaustion":0.0}"#.to_owned(),
            )],
            || {
                assert_eq!(
                    datapack_damage_type("test:plain").expect("loaded entry").scaling,
                    DamageScaling::WhenCausedByLivingNonPlayer
                );
            },
        );
    }

    // ------------------------------------------------------------- P1-7 ---

    #[test]
    fn p17_damage_type_tags_prefer_the_loaded_pack() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        with_pack(
            &[
                (
                    "a/data/minecraft/tags/damage_type/bypasses_shield.json".to_owned(),
                    r#"{"replace":true,"values":["minecraft:falling_anvil"]}"#.to_owned(),
                ),
                (
                    "a/data/minecraft/tags/damage_type/is_fall.json".to_owned(),
                    r#"{"replace":true,"values":["minecraft:falling_block"]}"#.to_owned(),
                ),
            ],
            || {
                // The pack's `bypasses_shield` replaces the pinned table, so a
                // falling stalactite (a table member) no longer bypasses while
                // the pack's own member does.
                assert!(crate::damage_type_flags("falling_anvil").bypasses_shield);
                assert!(!crate::damage_type_flags("falling_stalactite").bypasses_shield);
                assert!(!crate::damage_type_flags("on_fire").bypasses_shield);
                // A tag the pack does not define still uses the pinned 1.21.1
                // table (armor bypass is untouched here).
                assert!(crate::damage_type_flags("on_fire").bypasses_armor);
                assert!(!crate::damage_type_flags("falling_stalactite").bypasses_armor);
                // `is_fall` comes from the pack too.
                assert!(crate::damage_type_flags("falling_block").is_fall);
                assert!(!crate::damage_type_flags("fall").is_fall);
            },
        );
        // With no damage_type tags loaded at all, the pinned table answers.
        assert!(crate::damage_type_flags("falling_stalactite").bypasses_shield);
        assert!(crate::damage_type_flags("fall").is_fall);
    }
}

/// A recipe type this port does not model: retained verbatim, still queryable.
#[cfg(test)]
mod mod_recipe_type_tests {
    use super::*;

    #[test]
    fn custom_recipe_types_are_retained_verbatim() {
        let mut content = DatapackContent::default();
        let body = r#"{"type":"mymod:crushing","ingredient":{"item":"minecraft:iron_ore"},"result":{"id":"mymod:iron_dust","count":2},"energy":400}"#;
        // `false` keeps the legacy loose parser in play; the file is retained either way.
        assert!(!content.read_extended_recipe("mymod:iron_dust", body));
        assert_eq!(content.custom_recipes.len(), 1);
        assert_eq!(content.custom_recipes[0].recipe_type, "mymod:crushing");
        assert_eq!(content.custom_recipes[0].raw_json, body);

        // A higher-priority pack replaces the lower copy by id instead of stacking.
        let higher = r#"{"type":"mymod:crushing","ingredient":{"item":"minecraft:raw_iron"},"result":{"id":"mymod:iron_dust","count":3},"energy":200}"#;
        assert!(!content.read_extended_recipe("mymod:iron_dust", higher));
        assert_eq!(content.custom_recipes.len(), 1);
        assert_eq!(content.custom_recipes[0].raw_json, higher);

        // A vanilla type never enters the custom store.
        let stonecutting = r#"{"type":"minecraft:stonecutting","ingredient":{"item":"minecraft:stone"},"result":{"id":"minecraft:slab","count":2}}"#;
        assert!(content.read_extended_recipe("minecraft:slab_from_stone", stonecutting));
        assert_eq!(content.custom_recipes.len(), 1);

        // The published store answers a per-type lookup.
        *custom_recipes().lock().unwrap_or_else(|e| e.into_inner()) =
            content.custom_recipes.clone();
        let found = custom_recipes_of_type("mymod:crushing");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "mymod:iron_dust");
        assert!(custom_recipes_of_type("mymod:pressing").is_empty());
        assert_eq!(
            custom_recipe_types(),
            vec![("mymod:crushing".to_string(), 1)]
        );
        custom_recipes()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}
