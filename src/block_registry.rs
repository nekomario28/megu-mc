// block_registry.rs — block-state ids, properties, tags and the per-state
// values (hardness, drops, light, behaviour) read from assets/blocks_1211.json
// (per block) and assets/block_states_1211.json (per state). Terrain and feature
// generation read it; it knows nothing about terrain, placement or features.
//
// Both assets are produced from the same pinned 1.21.1 server jar by
// tools/vanilla_data/gen_vanilla_data.sh and share one state-id numbering, so a
// per-state value is read by id and no name/property pairing is involved.

use crate::json::{jarr, jnum, jparse, js, jstr, J};
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Item-id base for dynamically registered items, shared by every loader.
///
/// One number owns the dynamic item space: the legacy single-adapter (`create:*`) table
/// below is a thin alias of this base and indexes `DYNAMIC_MOD_ITEM_BASE + i`, while
/// [`register_dynamic_mod_item`] starts a loader's first item at
/// `DYNAMIC_MOD_ITEM_BASE + ADAPTER_ITEM_RANGE` so the two ranges cannot collide.
/// `block_registry` never depends upward on `main` / `live_mod_bridge` for it.
pub(crate) const DYNAMIC_MOD_ITEM_BASE: i64 = 1_000_000;

/// Width of the single-adapter item range reserved inside [`DYNAMIC_MOD_ITEM_BASE`]'s
/// block. The adapter tables are small (tens of entries) and this is the same order of
/// magnitude as a whole mod's item list, so a loader's ids start above any adapter's.
const ADAPTER_ITEM_RANGE: i64 = 1 << 16;

/// Item-id base for the `create:*` BlockItem/wrench table, kept as the name the
/// `mod-suite` adapter lane spells. It is the loader-agnostic base, aliased.
#[cfg(feature = "mod-suite")]
pub(crate) const LIVE_CREATE_ITEM_BASE: i64 = DYNAMIC_MOD_ITEM_BASE;

/// Test override for [`live_create_enabled`]: 0 = follow the environment,
/// 1 = force on, 2 = force off. Compiled into test builds only.
#[cfg(all(test, feature = "mod-suite"))]
pub(crate) static LIVE_CREATE_TEST_GATE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

/// Whether the Create adapter is armed for this process: `MEGU_ENABLE_CREATE`
/// truthy, or `MEGU_MODS_DIR` holding a `*.jar` whose filename mentions
/// "create". Evaluated once — the runtime decision must not drift mid-run.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_create_enabled() -> bool {
    #[cfg(test)]
    match LIVE_CREATE_TEST_GATE.load(Ordering::Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        let flag = std::env::var("MEGU_ENABLE_CREATE")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        if flag {
            return true;
        }
        let Some(dir) = std::env::var_os("MEGU_MODS_DIR") else {
            return false;
        };
        let Ok(rd) = std::fs::read_dir(dir) else {
            return false;
        };
        rd.filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
            })
            .any(|path| jar_manifest_declares_create(&path))
    })
}

/// The manifest-gated Create probe: a JAR arms the adapter only when its own
/// mod metadata declares the mod id the flagship adapter catalog handles
/// (`create`). The filename is operator-controlled noise — `recreate-maps.jar`
/// is not Create and `create-0.5.1.jar` renamed to `bin.jar` still is — so the
/// archived manifest, not the file name, decides. Fabric/Quilt declare
/// `fabric.mod.json`'s `id`; Forge/NeoForge declare `modId` inside a
/// `[[mods]]` table of the TOML under `META-INF`.
#[cfg(feature = "mod-suite")]
fn jar_manifest_declares_create(path: &Path) -> bool {
    let Ok(index) = megu_core::zip_archive::LazyZipIndex::open_all(path) else {
        return false;
    };
    if let Ok(json) = index.read_entry("fabric.mod.json", 1 << 20) {
        if let Some(parsed) = jparse(&json) {
            if parsed.get("id").is_some_and(|id| id.str() == "create") {
                return true;
            }
        }
    }
    for name in ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"] {
        if let Ok(toml) = index.read_entry(name, 1 << 20) {
            if toml_declares_create_mod_id(&toml) {
                return true;
            }
        }
    }
    false
}

/// Scan a Forge/NeoForge `mods.toml` for a `[[mods]]` table whose `modId` is
/// `create`. Dependency tables also name `modId="create"` (as a requirement,
/// not the mod itself), so only the first `modId` after a `[[mods]]` header
/// counts as the declaration.
#[cfg(feature = "mod-suite")]
fn toml_declares_create_mod_id(toml: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(toml) else {
        return false;
    };
    let mut in_mods = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_mods = line == "[[mods]]";
            continue;
        }
        if in_mods && line.starts_with("modId") {
            let Some((_, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            return value == "create";
        }
    }
    false
}

/// The `create:*` item table: `(item_name, Option<block_id>)` sorted by name
/// so item ids are deterministic across runs. `None` marks the wrench —
/// an item with no corresponding block.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_create_item_table() -> &'static [(String, Option<String>)] {
    static TABLE: std::sync::OnceLock<Vec<(String, Option<String>)>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut items: Vec<(String, Option<String>)> =
            create_adapter::blockstates::core_create_block_defs()
                .into_iter()
                .map(|def| {
                    let id = def.id.clone();
                    (id.clone(), Some(id))
                })
                .collect();
        items.push((
            "create:mechanical_belt".to_string(),
            Some("create:belt".to_string()),
        ));
        items.push(("create:wrench".to_string(), None));
        // Processed material items: press/mixer outputs and mixing inputs.
        // `None` like the wrench — they never place a block.
        for item in [
            "create:andesite_alloy",
            "create:brass_ingot",
            "create:brass_sheet",
            "create:copper_sheet",
            "create:gold_sheet",
            "create:iron_sheet",
            "create:zinc_ingot",
        ] {
            items.push((item.to_string(), None));
        }
        items.sort();
        items
    })
}

/// Item id a block drop resolves to for `BReg::block_drop`; `-1` = no drop.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_create_block_item_id(block_id: &str) -> i64 {
    live_create_item_table()
        .iter()
        .position(|(n, _)| n == block_id)
        .map(|i| LIVE_CREATE_ITEM_BASE + i as i64)
        .unwrap_or(-1)
}

// ---------- version-pack asset paths ----------
//
// One block-registry pack exists per Minecraft version: the bundled 1.21.1
// pack spells the tag on every filename (`blocks_1211.json`), and a future
// pack directory uses version-neutral names (`blocks.json`). The
// `MEGU_VERSION_DIR` environment override points at such a directory;
// when it is unset the bundled `assets/` pack resolves to exactly the paths
// this module always used. This is the single place the `1211` tag appears.

/// The bundled pack's version tag.
const VERSION_ASSET_TAG: &str = "1211";

/// Resolved filesystem paths for one version's block-registry asset set.
/// `resolve()` picks the pack, `from_dir()` maps the filenames inside it.
pub(crate) struct VersionAssetPaths {
    /// `blocks*.json`: per-block metadata — names, state-id ranges, properties,
    /// hardness, harvest tools, drops and stack sizes.
    pub(crate) blocks: PathBuf,
    /// `block_states*.json`: per-state flags, collision shape indices and the
    /// deduplicated shape table.
    pub(crate) block_states: PathBuf,
    /// `block_predicates*.bin`: heightmap predicate bits, the double-plant flag
    /// and the water/source-water pair indexed by state id.
    pub(crate) block_predicates: PathBuf,
    /// `block_state_facts*.bin`: the pinned six-bytes-per-state physical-facts
    /// table (`STATE_FACT_BYTES` columns).
    pub(crate) block_state_facts: PathBuf,
    /// `mineshaft_support*.bin`: support-face bytes; the chain-anchor bit is
    /// the one fact in it the pinned table does not carry.
    pub(crate) mineshaft_support: PathBuf,
    /// `multiface_support*.bin`: `MultifaceBlock.canAttachTo` face masks.
    pub(crate) multiface_support: PathBuf,
    /// `template_solid*.bin`: solid-template eligibility per state.
    pub(crate) template_solid: PathBuf,
}

/// The seven files that make up one pack: (field-neutral-name) pairs are
/// spelled once so `from_dir` and `default_pack` cannot disagree.
const VERSION_ASSET_FILES: &[&str] = &[
    "blocks.json",
    "block_states.json",
    "block_predicates.bin",
    "block_state_facts.bin",
    "mineshaft_support.bin",
    "multiface_support.bin",
    "template_solid.bin",
];

impl VersionAssetPaths {
    /// The pack for this run: the directory `MEGU_VERSION_DIR` names when
    /// set, else the bundled 1.21.1 `assets/` pack — byte-identical paths to
    /// what the hardcoded reads used, so the default run cannot observe the
    /// indirection.
    pub(crate) fn resolve() -> VersionAssetPaths {
        match std::env::var_os("MEGU_VERSION_DIR") {
            Some(dir) if !dir.is_empty() => Self::from_dir(Path::new(&dir)),
            _ => Self::default_pack(),
        }
    }

    /// One pack directory: for each asset the version-neutral filename wins;
    /// when it is absent the `*_TAG` spelling of the bundled pack is the
    /// fallback, so an existing `assets/` copy also serves as a version dir.
    /// When neither exists the neutral path is still reported, so the load
    /// error names the file the pack should have provided.
    pub(crate) fn from_dir(dir: &Path) -> VersionAssetPaths {
        let pick = |neutral: &str| -> PathBuf {
            let n = dir.join(neutral);
            if n.is_file() {
                return n;
            }
            let (stem, ext) = neutral
                .split_once('.')
                .expect("version asset names carry one extension");
            let tagged = dir.join(format!("{stem}_{VERSION_ASSET_TAG}.{ext}"));
            if tagged.is_file() {
                tagged
            } else {
                n
            }
        };
        let mut it = VERSION_ASSET_FILES.iter();
        let mut next = || pick(it.next().expect("seven pack files"));
        VersionAssetPaths {
            blocks: next(),
            block_states: next(),
            block_predicates: next(),
            block_state_facts: next(),
            mineshaft_support: next(),
            multiface_support: next(),
            template_solid: next(),
        }
    }

    /// The bundled pack: `assets/<name>_TAG.<ext>` for every file.
    fn default_pack() -> VersionAssetPaths {
        let dir = Path::new("assets");
        let mut it = VERSION_ASSET_FILES.iter();
        let mut next = || {
            let (stem, ext) = it
                .next()
                .expect("seven pack files")
                .split_once('.')
                .expect("version asset names carry one extension");
            dir.join(format!("{stem}_{VERSION_ASSET_TAG}.{ext}"))
        };
        VersionAssetPaths {
            blocks: next(),
            block_states: next(),
            block_predicates: next(),
            block_state_facts: next(),
            mineshaft_support: next(),
            multiface_support: next(),
            template_solid: next(),
        }
    }
}

// ---------- block-state JSON helpers ----------

pub(crate) fn ns_name(s: &str) -> String {
    if s.contains(':') {
        s.to_string()
    } else {
        format!("minecraft:{}", s)
    }
}
// blockstate json (string or {id,properties}) -> (name, props)
pub(crate) fn bs_parts(j: &J) -> Option<(String, Option<&J>)> {
    match j {
        J::Str(s) => Some((ns_name(s), None)),
        // 1.21.1 writes a serialised block state as {"Name": ..., "Properties": ...};
        // 1.21.2+ (and this repository's copy of the data) as {"id": ...,
        // "properties": ...}. Both appear in the bundled data, so read either.
        J::Obj(_) => Some((
            ns_name(&jstr(j, "id").or_else(|| jstr(j, "Name"))?),
            js(j, "properties").or_else(|| js(j, "Properties")),
        )),
        _ => None,
    }
}

// ---------- block registry ----------

struct Prop {
    name: String,
    vals: Vec<String>,
}

/// What a block does when a random tick lands on it. Zero is the common case and the
/// registry stores it for every block, so the dispatch is a compare against zero.
pub(crate) const B_NONE: u8 = 0;
pub(crate) const B_SUGAR_CANE: u8 = 1;
/// Blocks affected by gravity; scheduling and physics belong to the falling loop.
pub(crate) const B_FALLING: u8 = 2;

/// Native Heightmap.Types: WG variants share predicates, not update lifetimes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum HeightmapKind {
    WorldSurfaceWg,
    WorldSurface,
    OceanFloorWg,
    OceanFloor,
    MotionBlocking,
    MotionBlockingNoLeaves,
}
impl HeightmapKind {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "WORLD_SURFACE_WG" => Self::WorldSurfaceWg,
            "WORLD_SURFACE" => Self::WorldSurface,
            "OCEAN_FLOOR_WG" => Self::OceanFloorWg,
            "OCEAN_FLOOR" => Self::OceanFloor,
            "MOTION_BLOCKING" => Self::MotionBlocking,
            "MOTION_BLOCKING_NO_LEAVES" => Self::MotionBlockingNoLeaves,
            _ => return None,
        })
    }
    pub(crate) fn is_wg(self) -> bool {
        matches!(self, Self::WorldSurfaceWg | Self::OceanFloorWg)
    }
    fn mask(self) -> u8 {
        match self {
            Self::WorldSurfaceWg | Self::WorldSurface => 2,
            Self::OceanFloorWg | Self::OceanFloor => 4,
            Self::MotionBlocking => 8,
            Self::MotionBlockingNoLeaves => 16,
        }
    }
}

pub(crate) struct BReg {
    bname: Vec<String>,
    idx: HashMap<String, u32>,
    props: Vec<Box<[Prop]>>,
    dfl: Vec<u32>,
    st_lo: Vec<u32>,
    n_states: Vec<u32>,
    st_blk: Vec<u32>,      // state id -> block idx
    st_fl: Vec<u16>,       // per-state flag word from block_states_1211.json
    hardness: Vec<f32>,    // state id -> vanilla destroy time (-1 unbreakable)
    htools: Vec<Vec<i64>>, // block idx -> tool item ids that can harvest it (empty = any)
    tool_kind: Vec<i8>,    // block idx -> tool kind mined at full speed (-1 = none)
    /// Target-version states that could not obtain complete metadata by property value.
    /// The registry gate requires this to be empty, not merely the block names to match.
    pub(crate) unpaired: Vec<String>,
    drops: Vec<i64>, // state id -> item dropped with no enchantment (-1 = nothing)
    /// A few blocks drop twice: the potted plants drop the pot and the plant. Kept beside
    /// `drops` rather than folded into it so the single-drop path stays as it was.
    drops_second: Vec<i64>,
    stack_size: Vec<i64>, // state id -> max stack of the item, from the asset
    behaviour: Vec<u8>,   // block idx -> random-tick behaviour id
    shapes: Vec<([f64; 3], [f64; 3])>,
    col_range: Vec<(u32, u8)>, // state id -> (start in col_idx, len)
    col_idx: Vec<u16>,
    /// Packed per-state predicate byte (`STATE_FACTS` byte 0), copied at load so
    /// `is_air`/`blocks_motion`/`is_solid_render` and the rest are one indexed
    /// byte load — Lithium-style precomputed block-state flags.
    pred: Vec<u8>,
    /// Packed per-state light byte (`STATE_FACTS` byte 1): emission << 4 | opacity.
    light: Vec<u8>,
    /// `BlockStateBase.hasPostProcess` per state (`STATE_FACTS` byte 5).
    postproc: Vec<u8>,
    /// Packed `SupportType` face planes per state: three 6-bit planes (Full,
    /// Center, Rigid) assembled once at load instead of per `is_face_sturdy` call.
    faces: Vec<u32>,
    /// Dense tag id -> canonical (namespaced) tag name, for the memoized
    /// `in_tag` name resolution.
    tag_names: Vec<Box<str>>,
    /// Dense tag id -> packed block-index bitset. `in_tag` is one bit test on
    /// the hot path; the name half is memoized in `tag_cache`.
    tag_bits: Vec<Box<[u64]>>,
    /// Canonical tag name -> dense id. Lookups happen once per distinct name.
    tag_ids: HashMap<String, u32, BuildHasherDefault<TagHasher>>,
    /// Direct-mapped memo of tag name -> dense id, keyed on the string's
    /// pointer bits. Verified against `tag_names`, so a slot collision or a
    /// recycled allocation can only cost a miss, never a wrong answer.
    tag_cache: [AtomicU64; TAG_CACHE_SLOTS],
    /// tag name -> block idx list in the pack's own `values` order, nested tags expanded in
    /// place. `getRandomElement(tag, random)` indexes into that order, so a set would return
    /// a different member for the same draw.
    tag_order: HashMap<String, Vec<u32>>,
    /// The pack's `block_state_facts` bytes, indexed by `state_fact`.
    facts: Box<[u8]>,
    /// The pack's `block_predicates` bytes, read by `native_predicates`.
    predicates: Box<[u8]>,
    /// The pack's `mineshaft_support` bytes: the chain-anchor bit the pinned
    /// table does not carry.
    mineshaft: Box<[u8]>,
    /// The pack's `multiface_support` bytes.
    multiface: Box<[u8]>,
    /// The pack's `template_solid` bytes.
    template_solid: Box<[u8]>,
    /// Dynamic mod blockstate extension (`mod-suite`): Create's dynamic
    /// registry is appended after the vanilla states so `state_of("create:x")`,
    /// `name_of`, `with_prop`, `canonical_state` and friends all answer for
    /// ids >= `DEFAULT_BASE_STATE_ID` through the same dense columns.
    #[cfg(feature = "mod-suite")]
    create_reg: Option<create_adapter::blockstates::CreateBlockRegistry>,
}

/// Slots in the `in_tag` name-resolution memo. Tag names at call sites are
/// string literals, so a few dozen distinct names carry essentially every call.
const TAG_CACHE_SLOTS: usize = 256;

/// Multiply-shift mixer for the tag-name map and the memo slot: tag resolution
/// is once-per-name, so this beats SipHash without being a cryptographic hash.
#[derive(Clone, Copy, Debug, Default)]
struct TagHasher(u64);

type TagBuild = BuildHasherDefault<TagHasher>;

impl TagHasher {
    #[inline]
    fn add(hash: u64, word: u64) -> u64 {
        (hash.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95)
    }
}

impl Hasher for TagHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut hash = self.0;
        let (chunks, rem) = bytes.as_chunks::<8>();
        for c in chunks {
            hash = Self::add(hash, u64::from_ne_bytes(*c));
        }
        if !rem.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rem.len()].copy_from_slice(rem);
            hash = Self::add(hash, u64::from_ne_bytes(buf));
        }
        self.0 = hash;
    }
}

/// The asset's `flag_bits.replaceable` bit, `BlockState.isReplaceable()`. The other bits of the
/// same word (`air`, `liquid`, `solid`, `fluid`) are recorded but have a single owner in the
/// pinned fact table below, which answers them from the same jar; this bit is the only one the
/// registry reads out of the asset, and `is_air` is asked of the table even here so the two
/// cannot disagree about air.
const FL_REPLACEABLE: u16 = 2;

/// The asset's `flag_bits.solid` bit, `BlockState.isSolid()` (the legacy solid
/// heuristic). Kept distinct from `isSolidRender`: standing signs and banners
/// still ask the legacy predicate in 1.21.1.
const FL_LEGACY_SOLID: u16 = 8;

/// The pinned per-state physical facts: six bytes per state id, generated by
/// `tools/block_state_facts_from_jar.py` from the 1.21.1 server jar.
///
/// This table is the only owner of these predicates. `assets/block_predicates_1211.bin`'s
/// solid-render bit, `assets/collision_shape_full_1211.bin` and the face bits of
/// `assets/mineshaft_support_1211.bin` are the same native calls recorded by other oracles;
/// `generation_contract_tests::pinned_fact_tables_agree_with_the_older_oracle_assets` reads them
/// as a cross-check on the state-id alignment rather than as a second definition.
///
/// Byte 5 is not a physical fact but `BlockStateBase.hasPostProcess(BlockGetter, BlockPos)`, the
/// `WorldGenRegion.setBlock` gate for `markPosForPostprocessing`; it lives here because it is a
/// cached `StatePredicate` on the state, and the oracle derives the three names from
/// `Blocks.<clinit>` instead of a Rust constant.
const FACT_IS_AIR: u8 = 1;
const FACT_LIQUID: u8 = 1 << 1;
const FACT_BLOCKS_MOTION: u8 = 1 << 2;
const FACT_CAN_OCCLUDE: u8 = 1 << 3;
const FACT_SOLID_RENDER: u8 = 1 << 4;
const FACT_COLLISION_EMPTY: u8 = 1 << 5;
const FACT_COLLISION_FULL: u8 = 1 << 6;
const FACT_FLUID_EMPTY: u8 = 1 << 7;
/// Byte 5: `BlockStateBase.hasPostProcess` (`dtb$a.q`). A whole byte per state, not a bit, so a
/// future column cannot silently alias it.
const FACT_POSTPROCESS: u8 = 1;

/// Rows of the pinned `block_state_facts` table: `STATE_FACT_BYTES` per state
/// id, loaded from the version pack alongside the two JSONs it shares the
/// state-id numbering with.
const STATE_FACT_BYTES: usize = 6;

/// The pinned `SupportType`: which part of a face has to be covered for it to count as
/// sturdy. `Full` is vanilla's three-argument `isFaceSturdy`; `Center` is what
/// `canSupportCenter` (torch, plant and sign support) asks for, and it accepts a fence post or
/// a piston where `Full` does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Support {
    Full,
    Center,
    // `RIGID` is the frame-like face-support plane repeaters, comparators,
    // rails and pressure plates ask. It is not `isRedstoneConductor`; that is
    // a separate `BlockState` predicate exposed as `is_redstone_conductor`.
    Rigid,
}

/// The pinned `block_state_facts` bytes for `st`. An id outside the table answers every
/// predicate false and every level zero - "nothing here" - rather than panicking
/// mid-generation.
fn state_fact(facts: &[u8], st: u32) -> [u8; STATE_FACT_BYTES] {
    let at = st as usize * STATE_FACT_BYTES;
    match facts.get(at..at + STATE_FACT_BYTES) {
        Some(bytes) => [bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]],
        None => [0; STATE_FACT_BYTES],
    }
}

// The pinned MIT table has dense wire ids and property values in wire order.
// Check these before allocating synthesized columns or decoding a state id.
fn validate_synthesized_blocks(blocks: &[J]) -> Option<()> {
    let mut next = 0;
    let mut names = HashSet::new();
    for block in blocks {
        let name = jstr(block, "name")?;
        if (next == 0 && name != "air") || !names.insert(name) {
            return None;
        }
        let id = |key| jnum(block, key).filter(|n| (0.0..65536.0).contains(n) && n.fract() == 0.0);
        let lo = id("minStateId")? as u32;
        let hi = id("maxStateId")? as u32;
        let default = if block.get("defaultState").is_some() {
            id("defaultState")? as u32
        } else {
            lo
        };
        if lo != next || !(lo..=hi).contains(&default) {
            return None;
        }
        next = hi + 1;
    }
    (next > 0).then_some(())
}

type CollisionBox = ([f64; 3], [f64; 3]);
// Integer sixteenths of a block: exact dyadic box coordinates and cheap face masks.
type GridBox = ([u8; 3], [u8; 3]);
const FULL_BOX: GridBox = ([0; 3], [16; 3]);
const CARDINAL: [&str; 4] = ["north", "east", "south", "west"];

struct SynthBlock<'a> {
    name: &'a str,
    full_bounds: bool,
    transparent: bool,
    emission: u8,
    opacity: u8,
}

impl<'a> SynthBlock<'a> {
    fn new(name: &'a str, block: &J) -> Self {
        let full_bounds = jstr(block, "boundingBox").as_deref() == Some("block");
        let transparent = jnum(block, "transparent") == Some(1.0);
        Self {
            name,
            full_bounds,
            transparent,
            emission: jnum(block, "emitLight").unwrap_or(0.0).clamp(0.0, 15.0) as u8,
            opacity: jnum(block, "filterLight")
                .unwrap_or(if full_bounds && !transparent {
                    15.0
                } else {
                    0.0
                })
                .clamp(0.0, 15.0) as u8,
        }
    }

    fn state(&self, props: &[Prop], index: usize, reg: &mut BReg, st: usize) {
        let property = |key: &str| {
            let mut i = index;
            for prop in props.iter().rev() {
                let radix = prop.vals.len();
                if prop.name == key {
                    return prop.vals[i % radix].as_str();
                }
                i /= radix;
            }
            ""
        };
        let name = self.name;
        let air = matches!(name, "air" | "cave_air" | "void_air");
        let liquid = matches!(name, "water" | "lava" | "bubble_column");
        let water = property("waterlogged") == "true"
            || matches!(
                name,
                "water" | "kelp" | "kelp_plant" | "seagrass" | "tall_seagrass" | "bubble_column"
            );
        let fluid = water || name == "lava";
        let source =
            fluid && (!matches!(name, "water" | "lava") || matches!(property("level"), "0" | ""));
        let boxes = self.collision(&property, air || liquid);
        let full = boxes.as_slice() == [FULL_BOX];
        let partial_motion_exception = matches!(
            name,
            "ladder"
                | "lily_pad"
                | "snow"
                | "bamboo"
                | "sea_pickle"
                | "scaffolding"
                | "azalea"
                | "flowering_azalea"
        ) || name.ends_with("carpet")
            || name.ends_with("_sign");
        let motion = (!boxes.is_empty() && !partial_motion_exception)
            || name.ends_with("_fence_gate")
            || name.ends_with("_wall")
            || name.ends_with("_pressure_plate");
        let occlude = !self.transparent && (self.full_bounds || full) && !air && !liquid;
        let solid_render = occlude && (full || name == "soul_sand");
        let double_plant = matches!(
            name,
            "sunflower"
                | "lilac"
                | "rose_bush"
                | "peony"
                | "tall_grass"
                | "large_fern"
                | "tall_seagrass"
                | "pitcher_plant"
        );
        let replaceable = air
            || liquid
            || matches!(
                name,
                "short_grass"
                    | "tall_grass"
                    | "fern"
                    | "large_fern"
                    | "dead_bush"
                    | "seagrass"
                    | "tall_seagrass"
                    | "fire"
                    | "soul_fire"
                    | "snow"
                    | "vine"
                    | "glow_lichen"
                    | "light"
                    | "structure_void"
                    | "warped_roots"
                    | "crimson_roots"
                    | "nether_sprouts"
                    | "hanging_roots"
            );
        let flag = |set, mask| if set { mask } else { 0u8 };
        let mut opacity = self.opacity;
        if solid_render && name.ends_with("_slab") {
            opacity = 15;
        }
        if water {
            opacity = opacity.max(1);
        }
        let faces = if name.ends_with("_leaves") {
            0
        } else {
            grid_support(&boxes)
        };
        let packed_faces = faces.to_le_bytes();
        let fact = [
            flag(air, FACT_IS_AIR)
                | flag(liquid, FACT_LIQUID)
                | flag(motion, FACT_BLOCKS_MOTION)
                | flag(occlude, FACT_CAN_OCCLUDE)
                | flag(solid_render, FACT_SOLID_RENDER)
                | flag(boxes.is_empty(), FACT_COLLISION_EMPTY)
                | flag(full, FACT_COLLISION_FULL)
                | flag(!fluid, FACT_FLUID_EMPTY),
            self.luminance(&property, water) << 4 | opacity,
            packed_faces[0],
            packed_faces[1],
            packed_faces[2],
            flag(
                matches!(name, "brown_mushroom" | "red_mushroom" | "magma_block"),
                FACT_POSTPROCESS,
            ),
        ];
        let at = st * STATE_FACT_BYTES;
        reg.facts[at..at + STATE_FACT_BYTES].copy_from_slice(&fact);
        reg.predicates[st] = flag(solid_render, 1)
            | flag(!air, 2)
            | flag(motion, 4)
            | flag(motion || fluid, 8)
            | flag((motion || fluid) && !name.ends_with("_leaves"), 16)
            | flag(double_plant, 32)
            | flag(water, 64)
            | flag(source, 128);
        let full_faces = faces as u8 & 63;
        reg.mineshaft[st] = full_faces
            | flag(
                full_faces & 1 != 0 || (name == "chain" && property("axis") == "y"),
                64,
            );
        let collision_faces = if full { 63 } else { full_faces };
        // Attachment directions ask for the opposite face of the support block.
        reg.multiface[st] = ((collision_faces & 0x15) << 1) | ((collision_faces & 0x2a) >> 1);
        reg.template_solid[st] = u8::from(full);
        reg.st_fl[st] = u16::from(
            flag(air, 1)
                | flag(replaceable, FL_REPLACEABLE as u8)
                | flag(liquid, 4)
                | flag(motion, FL_LEGACY_SOLID as u8)
                | flag(fluid, 16),
        );
        reg.col_range[st] = (reg.col_idx.len() as u32, boxes.len() as u8);
        for (lo, hi) in boxes {
            let shape = (
                lo.map(|x| f64::from(x) / 16.0),
                hi.map(|x| f64::from(x) / 16.0),
            );
            let id = reg
                .shapes
                .iter()
                .position(|s| *s == shape)
                .unwrap_or_else(|| {
                    reg.shapes.push(shape);
                    reg.shapes.len() - 1
                });
            reg.col_idx.push(id as u16);
        }
    }

    fn luminance<'p>(&self, property: &impl Fn(&str) -> &'p str, water: bool) -> u8 {
        let lit = property("lit");
        match self.name {
            "light" => property("level")
                .parse::<u8>()
                .unwrap_or(self.emission)
                .min(15),
            "furnace" | "blast_furnace" | "smoker" if lit == "true" => 13,
            "redstone_ore" | "deepslate_redstone_ore" if lit == "true" => 9,
            "redstone_lamp" if lit == "true" => 15,
            "redstone_torch" | "redstone_wall_torch" if lit == "true" => 7,
            "cave_vines" | "cave_vines_plant" if property("berries") == "true" => 14,
            "sea_pickle" if water => {
                3 * (1 + property("pickles").parse::<u8>().unwrap_or(1).min(4))
            }
            "sea_pickle" => 0,
            n if lit == "true" && n.ends_with("candle") => {
                3 * property("candles").parse::<u8>().unwrap_or(1).min(4)
            }
            n if lit == "true" && n.ends_with("candle_cake") => 3,
            _ if lit == "false" => 0,
            _ => self.emission,
        }
    }

    /// The MIT `boundingBox` field only distinguishes empty from block. Use
    /// property-driven shapes for common survival blocks before that coarse
    /// fallback; this is not a substitute for the context-sensitive oracle.
    fn collision<'p>(&self, property: &impl Fn(&str) -> &'p str, empty: bool) -> Vec<GridBox> {
        if empty {
            return Vec::new();
        }
        let name = self.name;
        let facing = CARDINAL
            .iter()
            .position(|&d| d == property("facing"))
            .unwrap_or(0);
        let top = property("half") == "top";
        let open = property("open") == "true";
        if name.ends_with("_slab") {
            return vec![match property("type") {
                "double" => FULL_BOX,
                "top" => ([0, 8, 0], [16, 16, 16]),
                _ => ([0, 0, 0], [16, 8, 16]),
            }];
        }
        if name.ends_with("_stairs") {
            return grid_stairs(facing, top, property("shape"));
        }
        if name.ends_with("_door") {
            let turn = if !open {
                0
            } else if property("hinge") == "right" {
                3
            } else {
                1
            };
            return vec![rotate_grid_box(
                ([0, 0, 13], [16, 16, 16]),
                (facing + turn) % 4,
            )];
        }
        if name.ends_with("_trapdoor") {
            return vec![if open {
                rotate_grid_box(([0, 0, 13], [16, 16, 16]), facing)
            } else if top {
                ([0, 13, 0], [16, 16, 16])
            } else {
                ([0, 0, 0], [16, 3, 16])
            }];
        }
        if name.ends_with("_fence_gate") {
            return if open {
                Vec::new()
            } else {
                vec![rotate_grid_box(([0, 0, 6], [16, 24, 10]), facing)]
            };
        }
        if name.ends_with("_fence")
            || name.ends_with("_wall")
            || name.ends_with("_pane")
            || name == "iron_bars"
        {
            return grid_connected(name, property);
        }
        if name.ends_with("_sign")
            || name.ends_with("_banner")
            || name.ends_with("_rail")
            || name == "rail"
            || name.ends_with("_button")
            || name.ends_with("_pressure_plate")
        {
            return Vec::new();
        }
        let height = |y| ([0, 0, 0], [16, y, 16]);
        let centered = |inset, y| ([inset, 0, inset], [16 - inset, y, 16 - inset]);
        let shape = match name {
            "ladder" => rotate_grid_box(([0, 0, 13], [16, 16, 16]), facing),
            "chest" | "trapped_chest" | "ender_chest" => {
                let mut shape = centered(1, 14);
                match property("type") {
                    "left" => shape.1[0] = 16,
                    "right" => shape.0[0] = 0,
                    _ => {}
                }
                rotate_grid_box(shape, facing)
            }
            "farmland" | "dirt_path" => height(15),
            "soul_sand" => height(14),
            "cactus" | "honey_block" => centered(1, 15),
            "lily_pad" => centered(1, 2),
            "repeater" | "comparator" => height(2),
            "snow" => {
                let layers = property("layers").parse::<u8>().unwrap_or(1).clamp(1, 8);
                if layers == 1 {
                    return Vec::new();
                }
                height(2 * (layers - 1))
            }
            "enchanting_table" => height(12),
            "end_portal_frame" => height(13),
            "daylight_detector" => height(6),
            "mud" => height(14),
            "bamboo" => centered(7, 16),
            "chain" => match property("axis") {
                "x" => ([0, 6, 6], [16, 10, 10]),
                "z" => ([6, 6, 0], [10, 10, 16]),
                _ => centered(6, 16),
            },
            "sea_pickle" => centered(
                match property("pickles") {
                    "2" => 3,
                    "3" | "4" => 2,
                    _ => 6,
                },
                6,
            ),
            "flower_pot" => centered(5, 6),
            "decorated_pot" => centered(1, 16),
            "azalea" | "flowering_azalea" => {
                return vec![centered(6, 8), ([0, 8, 0], [16, 16, 16])];
            }
            "hopper" => {
                return vec![
                    height(10),
                    ([0, 10, 0], [16, 16, 2]),
                    ([0, 10, 14], [16, 16, 16]),
                    ([0, 10, 2], [2, 16, 14]),
                    ([14, 10, 2], [16, 16, 14]),
                ];
            }
            n if n.ends_with("cauldron") => {
                return vec![
                    height(5),
                    ([0, 5, 0], [16, 16, 2]),
                    ([0, 5, 14], [16, 16, 16]),
                    ([0, 5, 2], [2, 16, 14]),
                    ([14, 5, 2], [16, 16, 14]),
                ];
            }
            "lantern" | "soul_lantern" => {
                let y = if property("hanging") == "true" { 1 } else { 0 };
                return vec![
                    ([5, y, 5], [11, y + 7, 11]),
                    ([6, y + 7, 6], [10, y + 9, 10]),
                ];
            }
            n if n.ends_with("_bed") => height(9),
            n if n.ends_with("carpet") => height(1),
            n if n.ends_with("campfire") => height(7),
            n if n.starts_with("potted_") => centered(5, 6),
            _ if self.full_bounds => FULL_BOX,
            _ => return Vec::new(),
        };
        vec![shape]
    }
}

fn rotate_grid_box((mut lo, mut hi): GridBox, turns: usize) -> GridBox {
    for _ in 0..turns {
        (lo, hi) = ([16 - hi[2], lo[1], lo[0]], [16 - lo[2], hi[1], hi[0]]);
    }
    (lo, hi)
}

fn grid_stairs(facing: usize, top: bool, shape: &str) -> Vec<GridBox> {
    let (base, rise) = if top { (8, 0) } else { (0, 8) };
    let quarter = |x, z| rotate_grid_box(([x, rise, z], [x + 8, rise + 8, z + 8]), facing);
    let mut boxes = vec![([0, base, 0], [16, base + 8, 16])];
    match shape {
        "outer_left" => boxes.push(quarter(0, 0)),
        "outer_right" => boxes.push(quarter(8, 0)),
        _ => {
            boxes.push(rotate_grid_box(([0, rise, 0], [16, rise + 8, 8]), facing));
            match shape {
                "inner_left" => boxes.push(quarter(0, 8)),
                "inner_right" => boxes.push(quarter(8, 8)),
                _ => {}
            }
        }
    }
    boxes
}

fn grid_connected<'a>(name: &str, property: &impl Fn(&str) -> &'a str) -> Vec<GridBox> {
    let wall = name.ends_with("_wall");
    let fence = name.ends_with("_fence");
    let height = if wall || fence { 24 } else { 16 };
    let (post, arm) = if wall {
        (4, 3)
    } else if fence {
        (2, 2)
    } else {
        (1, 1)
    };
    let mut boxes = Vec::new();
    if !wall || property("up") == "true" {
        boxes.push(([8 - post, 0, 8 - post], [8 + post, height, 8 + post]));
    }
    for (turn, dir) in CARDINAL.into_iter().enumerate() {
        if matches!(property(dir), "true" | "low" | "tall") {
            boxes.push(rotate_grid_box(
                ([8 - arm, 0, 0], [8 + arm, height, 8]),
                turn,
            ));
        }
    }
    boxes
}

/// Union the rectangles touching each face; adjacent stair boxes may together
/// cover a full face. Center is the middle 2×2; rigid asks for the border.
fn grid_support(boxes: &[GridBox]) -> u32 {
    if boxes == [FULL_BOX] {
        return (1 << 18) - 1;
    }
    let mut mask = 0;
    for (face, (axis, high)) in [
        (1, false),
        (1, true),
        (2, false),
        (2, true),
        (0, false),
        (0, true),
    ]
    .into_iter()
    .enumerate()
    {
        let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
        let mut rows = [0u32; 16];
        for (lo, hi) in boxes {
            if (high && hi[axis] < 16) || (!high && lo[axis] != 0) {
                continue;
            }
            let cols = ((1 << hi[u].min(16)) - 1) ^ ((1 << lo[u].min(16)) - 1);
            for row in &mut rows[usize::from(lo[v].min(16))..usize::from(hi[v].min(16))] {
                *row |= cols;
            }
        }
        if rows.iter().all(|&row| row == 0xffff) {
            mask |= 1 << face;
        }
        if rows[7..9].iter().all(|row| row & 0x180 == 0x180) {
            mask |= 1 << (face + 6);
        }
        if rows[..2]
            .iter()
            .chain(&rows[14..])
            .all(|&row| row == 0xffff)
            && rows[2..14].iter().all(|row| row & 0xc003 == 0xc003)
        {
            mask |= 1 << (face + 12);
        }
    }
    mask
}

#[derive(Default)]
struct BlockFactTables {
    shapes: Vec<CollisionBox>,
    facts: Box<[u8]>,
    predicates: Box<[u8]>,
    mineshaft: Box<[u8]>,
    multiface: Box<[u8]>,
    template_solid: Box<[u8]>,
}

fn load_oracle_tables(paths: &VersionAssetPaths, states: &J) -> Option<BlockFactTables> {
    let mut tables = BlockFactTables {
        facts: std::fs::read(&paths.block_state_facts).ok()?.into(),
        predicates: std::fs::read(&paths.block_predicates).ok()?.into(),
        mineshaft: std::fs::read(&paths.mineshaft_support).ok()?.into(),
        multiface: std::fs::read(&paths.multiface_support).ok()?.into(),
        template_solid: std::fs::read(&paths.template_solid).ok()?.into(),
        shapes: Vec::new(),
    };
    for sh in states.get("shapes").and_then(J::as_arr)? {
        let min_a = sh.get("min").and_then(J::as_arr)?;
        let max_a = sh.get("max").and_then(J::as_arr)?;
        if min_a.len() != 3 || max_a.len() != 3 {
            return None;
        }
        tables.shapes.push((
            [min_a[0].num(), min_a[1].num(), min_a[2].num()],
            [max_a[0].num(), max_a[1].num(), max_a[2].num()],
        ));
    }
    Some(tables)
}

impl BReg {
    fn load() -> Option<BReg> {
        Self::load_paths(&VersionAssetPaths::resolve())
    }

    fn load_paths(paths: &VersionAssetPaths) -> Option<BReg> {
        let md = jparse(&std::fs::read(&paths.blocks).ok()?)?;
        let md = md.as_arr()?;
        // An incomplete pack uses one synthesized source for all physical
        // facts, never a mix of oracle and derived columns. A present but
        // unreadable complete pack still fails as before.
        let mut synthesize = false;
        for path in [
            &paths.block_states,
            &paths.block_state_facts,
            &paths.block_predicates,
            &paths.mineshaft_support,
            &paths.multiface_support,
            &paths.template_solid,
        ] {
            synthesize |= !path.try_exists().ok()?;
        }
        let states = if synthesize {
            validate_synthesized_blocks(md)?;
            None
        } else {
            Some(jparse(&std::fs::read(&paths.block_states).ok()?)?)
        };
        let oracle_states = match &states {
            Some(states) => Some((
                states.get("flags").and_then(J::as_arr)?,
                states.get("collision_shapes").and_then(J::as_arr)?,
            )),
            None => None,
        };
        let state_count = md
            .iter()
            .filter_map(|b| jnum(b, "maxStateId"))
            .fold(0.0, f64::max) as usize
            + 1;
        let BlockFactTables {
            shapes,
            facts,
            predicates,
            mineshaft,
            multiface,
            template_solid,
        } = if let Some(states) = &states {
            load_oracle_tables(paths, states)?
        } else {
            BlockFactTables {
                facts: vec![0; state_count * STATE_FACT_BYTES].into_boxed_slice(),
                predicates: vec![0; state_count].into_boxed_slice(),
                mineshaft: vec![0; state_count].into_boxed_slice(),
                multiface: vec![0; state_count].into_boxed_slice(),
                template_solid: vec![0; state_count].into_boxed_slice(),
                ..BlockFactTables::default()
            }
        };
        let mut reg = BReg {
            bname: vec![],
            idx: HashMap::new(),
            props: vec![],
            dfl: vec![],
            st_lo: vec![],
            n_states: vec![],
            st_blk: vec![0; state_count],
            st_fl: vec![0; state_count],
            hardness: vec![0.6; state_count],
            htools: vec![],
            tool_kind: vec![],
            unpaired: Vec::new(),
            drops: vec![-1; state_count],
            drops_second: vec![-1; state_count],
            stack_size: vec![64; state_count],
            behaviour: vec![],
            shapes,
            col_range: vec![(0, 0); state_count],
            col_idx: Vec::new(),
            pred: vec![0; state_count],
            light: vec![0; state_count],
            postproc: vec![0; state_count],
            faces: vec![0; state_count],
            tag_names: Vec::new(),
            tag_bits: Vec::new(),
            tag_ids: HashMap::default(),
            tag_cache: std::array::from_fn(|_| AtomicU64::new(0)),
            tag_order: HashMap::new(),
            facts,
            predicates,
            mineshaft,
            multiface,
            template_solid,
            #[cfg(feature = "mod-suite")]
            create_reg: None,
        };
        for b in md {
            let path = jstr(b, "name")?;
            let name = format!("minecraft:{path}");
            let bi = reg.bname.len() as u32;
            reg.idx.insert(name.clone(), bi);
            reg.bname.push(name.clone());
            let lo = jnum(b, "minStateId")? as u32;
            let hi = jnum(b, "maxStateId")? as u32;
            reg.st_lo.push(lo);
            reg.dfl
                .push(jnum(b, "defaultState").unwrap_or(lo as f64) as u32);
            reg.n_states.push(hi - lo + 1);
            let mut props = Vec::new();
            if let Some(ps) = jarr(b, "states") {
                for p in ps {
                    let pn = jstr(p, "name")?;
                    let ty = jstr(p, "type").unwrap_or_default();
                    let vals: Vec<String> = match jarr(p, "values") {
                        Some(vs) => vs.iter().map(|v| v.str().to_string()).collect(),
                        None if ty == "bool" => vec!["true".into(), "false".into()],
                        None => vec![],
                    };
                    props.push(Prop { name: pn, vals });
                }
            }
            if synthesize {
                let count = props
                    .iter()
                    .try_fold(1usize, |n, p| n.checked_mul(p.vals.len()))?;
                if count != (hi - lo + 1) as usize {
                    return None;
                }
            }
            let synthesized = synthesize.then(|| SynthBlock::new(&path, b));
            // Destroy time is a property of the block, so it is read here once and
            // held per state rather than re-derived by a hand table. A hand table
            // covered 356 of the 26684 states, which meant mining time was wrong for
            // almost every block in the world.
            let hardness = jnum(b, "hardness").unwrap_or(0.6) as f32;
            // Stack size is a block property in the asset (1 for tools and potions,
            // 16 for signs and buckets, 64 otherwise) and is read here once per state
            // rather than re-derived from item id ranges.
            let stack_size = jnum(b, "stackSize").unwrap_or(64.0) as i64;
            // What the block drops. The asset lists the default drop, with no
            // enchantment and the correct tool assumed, which is the question this
            // answers; silk touch and fortune are handled where the block is broken.
            // 34 blocks list two drops (the potted plants drop the pot and the plant). Both are
            // read: modelling only the first lost one item per broken pot.
            let (drop0, drop1) = match b.get("drops").and_then(|d| d.as_arr()) {
                Some(a) => (
                    a.first().map(|j| j.num() as i64).unwrap_or(-1),
                    a.get(1).map(|j| j.num() as i64).unwrap_or(-1),
                ),
                None => (-1, -1),
            };
            // Vanilla answers "can this tool harvest this block?" from the block's
            // harvestTools set: empty means any tool works, otherwise the held item
            // must be in it. Re-deriving that as (needs a pickaxe) AND (tier high
            // enough) got the multiplier wrong for every block that needs no tool.
            let ht: Vec<i64> = match b.get("harvestTools") {
                Some(J::Obj(entries)) => entries
                    .iter()
                    .filter_map(|(k, _)| k.parse::<i64>().ok())
                    .collect(),
                _ => Vec::new(),
            };
            // Which tool type mines this block at full speed. The asset states it as a
            // mineable/<kind> tag, or - for blocks gated by tier rather than by tool
            // type, like obsidian - only through the harvestTools set. A hand table of
            // bare id ranges answered this for 16% of the states.
            let from_material = match b.get("material") {
                Some(m) => m.str(),
                None => "",
            }
            .split(';')
            .find_map(|part| match part {
                "mineable/shovel" => Some(1i8),
                "mineable/pickaxe" => Some(2),
                "mineable/axe" => Some(3),
                "mineable/hoe" => Some(4),
                "mineable/sword" => Some(0),
                _ => None,
            });
            let from_tools = {
                let mut k: Option<i8> = None;
                let mut disagree = false;
                for t in &ht {
                    if (818..=847).contains(t) {
                        let kd = ((t - 818) % 5) as i8;
                        match k {
                            None => k = Some(kd),
                            Some(prev) if prev != kd => disagree = true,
                            _ => {}
                        }
                    }
                }
                if disagree {
                    None
                } else {
                    k
                }
            };
            // The asset has no behaviour field, so this is the one hand-written table in
            // the registry. It lives here, next to the data it accompanies, rather than at
            // the call site, so the dispatch is an integer compare and a behaviour is added
            // by one line instead of a new branch in the tick.
            reg.behaviour.push(match name.as_str() {
                "minecraft:sugar_cane" => B_SUGAR_CANE,
                "minecraft:sand" | "minecraft:red_sand" | "minecraft:gravel" => B_FALLING,
                n if n.ends_with("_concrete_powder") => B_FALLING,
                _ => B_NONE,
            });
            reg.tool_kind
                .push(from_material.or(from_tools).unwrap_or(-1));
            reg.htools.push(ht);
            let n = (hi - lo + 1) as usize;
            for i in 0..n {
                let st = (lo + i as u32) as usize;
                if st >= state_count {
                    break;
                }
                reg.st_blk[st] = bi;
                reg.hardness[st] = hardness;
                reg.drops[st] = drop0;
                reg.drops_second[st] = drop1;
                reg.stack_size[st] = stack_size;
                let Some((state_flags, collision)) = oracle_states else {
                    synthesized.as_ref()?.state(&props, i, &mut reg, st);
                    continue;
                };
                // The collision boxes are indices into the deduplicated `shapes` table the
                // asset carries; both come from the same state, so no name or property
                // translation stands between the game's answer and the registry's copy.
                let start = reg.col_idx.len() as u32;
                let mut len = 0u8;
                let mut boxes_ok = true;
                match collision.get(st).and_then(J::as_arr) {
                    Some(cols) => {
                        for sh in cols {
                            let idx = sh.num() as usize;
                            if !matches!(sh, J::Num(_)) || idx >= reg.shapes.len() || len == u8::MAX {
                                boxes_ok = false;
                                continue;
                            }
                            reg.col_idx.push(idx as u16);
                            len += 1;
                        }
                    }
                    None => boxes_ok = false,
                }
                reg.col_range[st] = (start, len);
                match state_flags.get(st) {
                    Some(J::Num(f)) if boxes_ok => reg.st_fl[st] = *f as u16,
                    _ => reg.unpaired.push(format!("{name} state {st}")),
                }
            }
            reg.props.push(props.into_boxed_slice());
        }
        // Copy the per-state fact bytes into dense tables once, so the hot
        // predicates are a single indexed load regardless of runtime mode.
        for st in 0..state_count {
            let fact = state_fact(&reg.facts, st as u32);
            reg.pred[st] = fact[0];
            reg.light[st] = fact[1];
            reg.postproc[st] = fact[5];
            reg.faces[st] = u32::from_le_bytes([fact[2], fact[3], fact[4], 0]);
        }
        if synthesize {
            eprintln!("[breg] synthesized block-state facts from blocks_1211.json (zero-Java runtime mode)");
        }
        // block tags: eagerly expand nested #tag refs
        let mut raw: HashMap<String, Vec<String>> = HashMap::new();
        for (name, bytes) in dp_tag_files("block") {
            if let Some(j) = jparse(&bytes) {
                if let Some(vals) = jarr(&j, "values") {
                    let mut members = Vec::new();
                    for v in vals {
                        match v {
                            J::Str(s) => members.push(ns_name(s)),
                            J::Obj(_) => {
                                if let Some(id) = jstr(v, "id") {
                                    members.push(ns_name(&id));
                                }
                            }
                            _ => {}
                        }
                    }
                    raw.insert(name, members);
                }
            }
        }
        fn expand(
            name: &str,
            raw: &HashMap<String, Vec<String>>,
            idx: &HashMap<String, u32>,
            done: &mut HashMap<String, HashSet<u32>>,
            stack: &mut HashSet<String>,
        ) -> HashSet<u32> {
            if let Some(s) = done.get(name) {
                return s.clone();
            }
            if stack.contains(name) {
                return HashSet::new();
            }
            stack.insert(name.to_string());
            let mut out = HashSet::new();
            if let Some(members) = raw.get(name) {
                for m in members {
                    if let Some(inner) = m.strip_prefix('#') {
                        for b in expand(&ns_name(inner), raw, idx, done, stack) {
                            out.insert(b);
                        }
                    } else if let Some(&bi) = idx.get(m) {
                        out.insert(bi);
                    }
                }
            }
            stack.remove(name);
            done.insert(name.to_string(), out.clone());
            out
        }
        fn expand_ordered(
            name: &str,
            raw: &HashMap<String, Vec<String>>,
            idx: &HashMap<String, u32>,
            done: &mut HashMap<String, Vec<u32>>,
            stack: &mut HashSet<String>,
        ) -> Vec<u32> {
            if let Some(v) = done.get(name) {
                return v.clone();
            }
            if stack.contains(name) {
                return Vec::new();
            }
            stack.insert(name.to_string());
            let mut out = Vec::new();
            if let Some(members) = raw.get(name) {
                for m in members {
                    if let Some(inner) = m.strip_prefix('#') {
                        out.extend(expand_ordered(&ns_name(inner), raw, idx, done, stack));
                    } else if let Some(&bi) = idx.get(m) {
                        out.push(bi);
                    }
                }
            }
            stack.remove(name);
            done.insert(name.to_string(), out.clone());
            out
        }
        let names: Vec<String> = raw.keys().cloned().collect();
        // `expand` memoizes every tag it touches — top-level names and nested
        // `#tag` members alike — so the finished map is the same coverage the
        // old `tag` field had.
        let mut done_sets: HashMap<String, HashSet<u32>> = HashMap::new();
        for n in names {
            let mut stack = HashSet::new();
            let _ = expand(&n, &raw, &reg.idx, &mut done_sets, &mut stack);
            let mut stack = HashSet::new();
            let ordered = expand_ordered(&n, &raw, &reg.idx, &mut reg.tag_order, &mut stack);
            reg.tag_order.insert(n, ordered);
        }
        // Each expanded set becomes a packed block-index bitset: `in_tag` then
        // costs one bit test, with no HashSet probe or string work per call.
        let words = reg.bname.len().div_ceil(64);
        let mut tag_ids = HashMap::with_capacity_and_hasher(done_sets.len(), TagBuild::default());
        reg.tag_names.reserve(done_sets.len());
        reg.tag_bits.reserve(done_sets.len());
        for (name, set) in done_sets {
            let id = reg.tag_names.len() as u32;
            tag_ids.insert(name.clone(), id);
            let mut bits = vec![0u64; words].into_boxed_slice();
            for bi in set {
                let u = bi as usize;
                if u >> 6 < words {
                    bits[u >> 6] |= 1u64 << (u & 63);
                }
            }
            reg.tag_names.push(name.into_boxed_str());
            reg.tag_bits.push(bits);
        }
        reg.tag_ids = tag_ids;
        #[cfg(feature = "mod-suite")]
        reg.extend_mod_blocks();
        // Loader-agnostic dynamic blocks: appended after the adapter registry so the id
        // spaces stay contiguous, and a no-op when no loader contributed any.
        reg.extend_dynamic_blocks();
        Some(reg)
    }
    /// Dynamic mod blockstate extension (`mod-suite`): when Create is armed
    /// (`MEGU_ENABLE_CREATE=1` or a `create` JAR under `MEGU_MODS_DIR`), the
    /// adapter's `core_create_registry` table is appended after the vanilla
    /// states at `DEFAULT_BASE_STATE_ID`, so `state_of("create:shaft")`,
    /// `name_of`, `with_prop`, `properties_of`, `canonical_state`,
    /// `is_solid_render`, drops, and hardness all answer through the same
    /// dense columns as vanilla ids — chunk-section palettes and the `0x09`
    /// block-update varints then carry mod states naturally.
    #[cfg(feature = "mod-suite")]
    fn extend_mod_blocks(&mut self) {
        if !live_create_enabled() {
            return;
        }
        let create = create_adapter::blockstates::core_create_registry(
            create_adapter::blockstates::DEFAULT_BASE_STATE_ID,
        );
        self.extend_create_registry(create);
    }
    /// The work of [`BReg::extend_mod_blocks`] once the armed Create
    /// registry is built: appends its blocks and expanded states to the
    /// dense columns, derives each state's physical facts from the block id
    /// and property pairs via [`derive_mod_block_facts`], then grows the tag
    /// bitsets so `in_tag` covers the modded block indices.
    #[cfg(feature = "mod-suite")]
    fn extend_create_registry(
        &mut self,
        create: create_adapter::blockstates::CreateBlockRegistry,
    ) {
        let base = create.base_state_id() as usize;
        if self.st_blk.len() > base {
            // A vanilla table that already reaches into the mod range would
            // collide; refuse rather than corrupting both id spaces.
            return;
        }
        let need = base + create.state_count();
        self.st_blk.resize(need, 0);
        self.st_fl.resize(need, 0);
        self.hardness.resize(need, 0.6);
        self.drops.resize(need, -1);
        self.drops_second.resize(need, -1);
        self.stack_size.resize(need, 64);
        self.col_range.resize(need, (0, 0));
        self.pred.resize(need, 0);
        self.light.resize(need, 0);
        self.postproc.resize(need, 0);
        self.faces.resize(need, 0);
        // The state-id-indexed byte tables are boxed slices; grow them too so
        // the derived rows land instead of reading as absent-table defaults.
        let mut predicates = std::mem::take(&mut self.predicates).into_vec();
        predicates.resize(need, 0);
        self.predicates = predicates.into_boxed_slice();
        let mut template_solid = std::mem::take(&mut self.template_solid).into_vec();
        template_solid.resize(need, 0);
        self.template_solid = template_solid.into_boxed_slice();
        let mut full_cube: Option<u16> = None;
        let mut mod_block_ids = Vec::with_capacity(create.blocks().len());
        for (ci, def) in create.blocks().iter().enumerate() {
            let bi = self.bname.len() as u32;
            mod_block_ids.push(bi);
            self.idx.insert(def.id.clone(), bi);
            self.bname.push(def.id.clone());
            self.props.push(
                def.properties
                    .iter()
                    .map(|p| Prop {
                        name: p.name.clone(),
                        vals: p.values.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            );
            let first = create.default_state(&def.id).unwrap_or(0);
            self.dfl.push(first);
            self.st_lo.push(first);
            // The flag LUT classifies modded ids by vanilla vocabulary (the
            // namespace is ignored, so `mymod:sand` still reads as sand): its
            // replaceable/gravity verdicts feed the `st_fl` and `behaviour`
            // columns the pinned table never shipped for these ids.
            let lut_word = megu_core::block_flags_lut::classify_name(&def.id);
            let mut n = 0u32;
            for entry in create.states().iter().filter(|e| e.block_index == ci) {
                let st = entry.state_id as usize;
                self.st_blk[st] = bi;
                self.hardness[st] = 1.0;
                self.drops[st] = live_create_block_item_id(&def.id);
                self.stack_size[st] = 64;
                if lut_word & megu_core::block_flags_lut::FLAG_REPLACEABLE != 0 {
                    self.st_fl[st] |= FL_REPLACEABLE;
                }
                let facts = derive_mod_block_facts(&def.id, &entry.properties);
                self.pred[st] = facts.pred;
                self.light[st] = facts.light;
                self.faces[st] = facts.faces;
                self.predicates[st] = facts.native;
                if facts.pred & FACT_COLLISION_FULL != 0 {
                    // A full-collision modded block is a template solid and
                    // collides with the unit cube, so entities and
                    // fence/pane/wall connection updates treat it like any
                    // other opaque cube.
                    self.template_solid[st] = 1;
                    let shape = *full_cube.get_or_insert_with(|| {
                        self.shapes.push(([0.0; 3], [1.0; 3]));
                        (self.shapes.len() - 1) as u16
                    });
                    let start = self.col_idx.len() as u32;
                    self.col_idx.push(shape);
                    self.col_range[st] = (start, 1);
                }
                n += 1;
            }
            self.n_states.push(n);
            self.htools.push(Vec::new());
            self.tool_kind.push(2); // pickaxe family
            self.behaviour.push(
                if lut_word & megu_core::block_flags_lut::FLAG_GRAVITY_AFFECTED != 0 {
                    B_FALLING
                } else {
                    B_NONE
                },
            );
        }
        // The tag bitsets were sized for the vanilla block list before this
        // extension ran; grow each to cover the appended indices, then tag
        // every modded block under the vanilla names its id implies
        // (`*_planks` -> `minecraft:planks`, a solid `*_casing` ->
        // `minecraft:mineable/pickaxe`), so `in_tag` answers for modded
        // states instead of reading past the bitsets' end.
        let words = self.bname.len().div_ceil(64);
        for bits in self.tag_bits.iter_mut() {
            let mut grown = std::mem::take(bits).into_vec();
            grown.resize(words, 0);
            *bits = grown.into_boxed_slice();
        }
        for &bi in &mod_block_ids {
            let id = self.bname[bi as usize].clone();
            let path = id.rsplit(':').next().unwrap_or(&id);
            for tag in mod_block_tag_names(path, mod_block_shape(path)) {
                let tid = self.tag_id_or_insert(tag, words);
                self.tag_bits[tid as usize][(bi >> 6) as usize] |= 1u64 << (bi & 63);
                self.tag_order.entry(tag.to_string()).or_default().push(bi);
            }
        }
        self.create_reg = Some(create);
    }
    /// The dense id of `tag`, creating the tag's name row, empty bitset and
    /// lookup entry when the pack did not declare it — modded-only tags
    /// such as `minecraft:mineable/pickaxe` for a stripped pack.
    #[cfg(feature = "mod-suite")]
    fn tag_id_or_insert(&mut self, tag: &'static str, words: usize) -> u32 {
        if let Some(&id) = self.tag_ids.get(tag) {
            return id;
        }
        let id = self.tag_names.len() as u32;
        self.tag_names.push(tag.into());
        self.tag_bits.push(vec![0u64; words].into_boxed_slice());
        self.tag_ids.insert(tag.to_string(), id);
        id
    }
    /// The armed Create registry, when the mod-suite extension ran.
    #[cfg(feature = "mod-suite")]
    pub(crate) fn create_registry(
        &self,
    ) -> Option<&create_adapter::blockstates::CreateBlockRegistry> {
        self.create_reg.as_ref()
    }
    /// Loader-agnostic dynamic block extension: appends every entry registered through
    /// [`register_dynamic_mod_blocks`] after the states this registry already carries.
    ///
    /// Each entry's light emission, light opacity, hardness, solidity and replaceability are
    /// written verbatim into the dense columns. The neutral path has no name vocabulary — a
    /// loader states what its block is — so none of the `*_leaves`/`*_planks` heuristics the
    /// adapter path applies can run here, and a block's metadata is exactly what it declared.
    /// A block whose name also registered a dynamic item drops that item; any other block
    /// drops nothing, which is a real answer and not an inferred one.
    ///
    /// Returns the number of blocks appended, i.e. the entries that were new and whose
    /// property table expanded inside [`MAX_DYNAMIC_BLOCK_STATES`].
    fn extend_dynamic_blocks(&mut self) -> usize {
        let entries = dynamic_mod_blocks_snapshot();
        let mut appended = 0;
        for entry in &entries {
            if self.idx.contains_key(&entry.name) {
                continue;
            }
            let states = dynamic_block_state_count(&entry.properties);
            if states == 0 {
                continue;
            }
            let base = self.st_blk.len();
            let need = base + states;
            self.st_blk.resize(need, 0);
            self.st_fl.resize(need, 0);
            self.hardness.resize(need, 0.6);
            self.drops.resize(need, -1);
            self.drops_second.resize(need, -1);
            self.stack_size.resize(need, 64);
            self.col_range.resize(need, (0, 0));
            self.pred.resize(need, 0);
            self.light.resize(need, 0);
            self.postproc.resize(need, 0);
            self.faces.resize(need, 0);
            let mut predicates = std::mem::take(&mut self.predicates).into_vec();
            predicates.resize(need, 0);
            self.predicates = predicates.into_boxed_slice();
            let mut template_solid = std::mem::take(&mut self.template_solid).into_vec();
            template_solid.resize(need, 0);
            self.template_solid = template_solid.into_boxed_slice();

            let bi = self.bname.len() as u32;
            let light = (entry.light_emission.min(15) << 4) | entry.light_opacity.min(15);
            // A solid entry is a full cube: it blocks motion, renders solid, collides with
            // the unit cube and supports a full face on every side. A non-solid entry keeps
            // the empty-shape facts a decorative block needs and no support.
            let pred = if entry.is_solid {
                FACT_BLOCKS_MOTION | FACT_SOLID_RENDER | FACT_COLLISION_FULL | FACT_FLUID_EMPTY
            } else {
                // Not air, and nothing else claimed: a block the loader did not call solid does
                // not block motion, does not render solid and does not fill its cube. Its
                // collision shape is *not* reported empty either, so a fluid cannot pass
                // through it on this registry's word alone.
                FACT_FLUID_EMPTY
            };
            let faces: u32 = if entry.is_solid { 0x003f_3f3f } else { 0 };
            // `native_predicates`: `WORLD_SURFACE` counts every non-air state, and the
            // heightmap/fluid bits follow a collider.
            let native: u8 = if entry.is_solid { 2 | 4 | 8 | 16 } else { 2 };
            let mut flags = 0u16;
            if entry.is_replaceable {
                flags |= FL_REPLACEABLE;
            }
            if entry.is_solid {
                flags |= FL_LEGACY_SOLID;
            }
            let full_cube = if entry.is_solid {
                let existing = self
                    .shapes
                    .iter()
                    .position(|shape| *shape == ([0.0; 3], [1.0; 3]));
                Some(match existing {
                    Some(index) => index as u16,
                    None => {
                        self.shapes.push(([0.0; 3], [1.0; 3]));
                        (self.shapes.len() - 1) as u16
                    }
                })
            } else {
                None
            };
            let item = dynamic_mod_item_facts(&entry.name);
            for offset in 0..states {
                let st = base + offset;
                self.st_blk[st] = bi;
                self.st_fl[st] = flags;
                self.hardness[st] = entry.hardness;
                self.drops[st] = item.map_or(-1, |(id, _)| id);
                self.stack_size[st] = item.map_or(64, |(_, max_stack)| i64::from(max_stack));
                self.pred[st] = pred;
                self.light[st] = light;
                self.faces[st] = faces;
                self.predicates[st] = native;
                self.template_solid[st] = u8::from(entry.is_solid);
                self.col_range[st] = match full_cube {
                    Some(shape) => {
                        let start = self.col_idx.len() as u32;
                        self.col_idx.push(shape);
                        (start, 1)
                    }
                    None => (self.col_idx.len() as u32, 0),
                };
            }
            self.idx.insert(entry.name.clone(), bi);
            self.bname.push(entry.name.clone());
            self.props.push(
                entry
                    .properties
                    .iter()
                    .map(|(name, values)| Prop {
                        name: name.clone(),
                        vals: values.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            );
            self.dfl.push(base as u32);
            self.st_lo.push(base as u32);
            self.n_states.push(states as u32);
            self.htools.push(Vec::new());
            // No tool is faster than bare hands on a block whose loader declared none.
            self.tool_kind.push(-1);
            self.behaviour.push(B_NONE);
            appended += 1;
        }
        if appended == 0 {
            return 0;
        }
        // The tag bitsets were sized for the block list before this extension ran; grow each
        // so `in_tag` covers the appended indices. A neutral block claims no tag: its loader
        // stated its properties, not a vanilla family it belongs to.
        let words = self.bname.len().div_ceil(64);
        for bits in self.tag_bits.iter_mut() {
            let mut grown = std::mem::take(bits).into_vec();
            grown.resize(words, 0);
            *bits = grown.into_boxed_slice();
        }
        appended
    }
    /// resolve (name, props) to a 1.21.1 state id
    pub(crate) fn state_of(&self, name: &str, props: Option<&J>) -> u32 {
        let name = ns_name(name);
        let bi = match self.idx.get(&name) {
            Some(&b) => b,
            None => return 0,
        };
        let base = match props {
            Some(J::Obj(m)) => m,
            _ => return self.dfl[bi as usize],
        };
        let ps = &self.props[bi as usize];
        let mut off = 0u64;
        for (i, p) in ps.iter().enumerate() {
            let want = base
                .iter()
                .find(|(kk, _)| kk == &p.name)
                .map(|(_, v)| v.str().to_string());
            let mult: u64 = ps[i + 1..]
                .iter()
                .map(|q| q.vals.len().max(1) as u64)
                .product();
            if let Some(w) = want {
                match p.vals.iter().position(|v| *v == w) {
                    Some(d) => off += d as u64 * mult,
                    None => return self.dfl[bi as usize], // bad value -> default
                }
            }
        }
        let st = self.st_lo[bi as usize] + off as u32;
        if off >= self.n_states[bi as usize] as u64 {
            return self.dfl[bi as usize];
        }
        st
    }
    /// The block registry's raw id (`Registries.BLOCK`), which is what the wire
    /// carries where a packet names a *block* rather than a state - for example
    /// `ClientboundBlockEventPacket`'s trailing VarInt. It is not a state id.
    pub(crate) fn block_id_of(&self, st: u32) -> Option<i32> {
        self.st_blk.get(st as usize).map(|block| *block as i32)
    }
    pub(crate) fn name_of(&self, st: u32) -> &str {
        self.st_blk
            .get(st as usize)
            .and_then(|&b| self.bname.get(b as usize))
            .map(|s| s.as_str())
            .unwrap_or("minecraft:air")
    }

    /// Structure palettes start from the block's default state. Invalid names
    /// or properties are errors for the template loader, not air substitutes.
    pub(crate) fn checked_state(&self, name: &str, properties: &[(String, String)]) -> Option<u32> {
        let bi = *self.idx.get(&ns_name(name))? as usize;
        let mut state = self.dfl[bi];
        for (key, value) in properties {
            let property = self.props[bi].iter().find(|p| p.name == *key)?;
            if !property.vals.contains(value) {
                return None;
            }
            state = self.with_prop(state, key, value);
        }
        Some(state)
    }

    pub(crate) fn properties_of(&self, state: u32) -> Option<Vec<(String, String)>> {
        let bi = *self.st_blk.get(state as usize)? as usize;
        self.props[bi]
            .iter()
            .map(|p| Some((p.name.clone(), self.get_prop(state, &p.name)?)))
            .collect()
    }

    pub(crate) fn is_template_solid(&self, state: u32) -> Option<bool> {
        self.template_solid
            .get(state as usize)
            .map(|v| *v != 0)
    }
    /// Number of states the registry knows. Callers that walk the state space need
    /// this rather than a literal, so the extent has one owner.
    pub(crate) fn state_count(&self) -> usize {
        self.st_blk.len()
    }

    /// Vanilla `canHarvest`: whether this tool gets the block's drop, and whether
    /// mining uses the fast multiplier. A block with no harvestTools entry can be
    /// harvested by anything.
    pub(crate) fn can_harvest(&self, st: u32, item: i64) -> bool {
        let Some(&bi) = self.st_blk.get(st as usize) else {
            return true;
        };
        match self.htools.get(bi as usize) {
            Some(ht) if !ht.is_empty() => ht.contains(&item),
            _ => true,
        }
    }

    /// The packed predicate byte for `st`, or 0 outside the table — the same
    /// answer `state_fact`'s zero fill gave, at one indexed load.
    #[inline]
    fn pred_of(&self, st: u32) -> u8 {
        self.pred.get(st as usize).copied().unwrap_or(0)
    }

    /// The packed light byte for `st` (`emission << 4 | opacity`), or 0 outside
    /// the table.
    #[inline]
    fn light_of_byte(&self, st: u32) -> u8 {
        self.light.get(st as usize).copied().unwrap_or(0)
    }

    /// Light this state emits, 0..15, from the pinned `getLightEmission()`.
    pub(crate) fn luminance_of(&self, st: u32) -> u8 {
        self.light_of_byte(st) >> 4
    }

    /// How much light this state stops, 0..15, from the pinned `getLightBlock(level, pos)`.
    /// Zero is fully transparent.
    pub(crate) fn opacity_of(&self, st: u32) -> u8 {
        self.light_of_byte(st) & 0xf
    }

    /// `BlockStateBase.hasPostProcess(BlockGetter, BlockPos)` (`dtb$a.q`): the predicate
    /// `WorldGenRegion.setBlock` reads to decide whether the written cell is filed with
    /// `ChunkAccess.markPosForPostprocessing`. Constant true for exactly three blocks in 1.21.1
    /// (`brown_mushroom`, `red_mushroom`, `magma_block`) and false for every other state. The
    /// table column behind this is re-derived from `Blocks.<clinit>` by
    /// `tools/block_state_facts_from_jar.py`, so no block name is typed in here.
    pub(crate) fn has_post_process(&self, st: u32) -> bool {
        self.postproc
            .get(st as usize)
            .is_some_and(|b| b & FACT_POSTPROCESS != 0)
    }

    /// The random-tick behaviour of this block, or B_NONE.
    pub(crate) fn behaviour_of(&self, st: u32) -> u8 {
        let Some(&bi) = self.st_blk.get(st as usize) else {
            return B_NONE;
        };
        self.behaviour.get(bi as usize).copied().unwrap_or(B_NONE)
    }

    /// How many of this item fit in one slot, as the asset states it.
    pub(crate) fn stack_size_of(&self, st: u32) -> i64 {
        self.stack_size.get(st as usize).copied().unwrap_or(64)
    }

    /// The item this block drops when broken with no enchantment, or None when it
    /// drops nothing (glass, ice, tall grass) - which is a real answer, not a missing
    /// one, and the reason this cannot be inferred from the block's own item.
    pub(crate) fn block_drop(&self, st: u32) -> Option<i64> {
        match self.drops.get(st as usize).copied() {
            Some(d) if d >= 0 => Some(d),
            _ => None,
        }
    }

    /// The second item a block drops, when the asset lists one.
    pub(crate) fn block_drop_second(&self, st: u32) -> Option<i64> {
        match self.drops_second.get(st as usize).copied() {
            Some(d) if d >= 0 => Some(d),
            _ => None,
        }
    }

    /// Which tool type (0 sword, 1 shovel, 2 pickaxe, 3 axe, 4 hoe) mines this block
    /// at full speed, or -1 when no tool is faster than bare hands.
    pub(crate) fn tool_kind_of(&self, st: u32) -> i8 {
        let Some(&bi) = self.st_blk.get(st as usize) else {
            return -1;
        };
        self.tool_kind.get(bi as usize).copied().unwrap_or(-1)
    }

    /// Vanilla destroy time in seconds. Negative means unbreakable.
    pub(crate) fn hardness_of(&self, st: u32) -> f32 {
        self.hardness.get(st as usize).copied().unwrap_or(0.6)
    }

    /// Pinned `isAir()`. Three states answer true (air, cave air, void air); the old
    /// `state_flags` bit happened to agree on all 26,684, and the table removes the guess.
    pub(crate) fn is_air(&self, st: u32) -> bool {
        self.pred_of(st) & FACT_IS_AIR != 0
    }
    /// Pinned `blocksMotion()`: whether the state's collision shape stops an entity. Named for
    /// the native method; `is_solid` remains the name the generation callers use.
    pub(crate) fn blocks_motion(&self, st: u32) -> bool {
        self.pred_of(st) & FACT_BLOCKS_MOTION != 0
    }
    /// The name the generation callers use for `blocksMotion`, kept because forty call sites
    /// read better as "is this solid"; the value is the native predicate, not a second one.
    pub(crate) fn is_solid(&self, st: u32) -> bool {
        self.blocks_motion(st)
    }
    /// Pinned `canOcclude()`: the state's occlusion shape is a full cube. A different question
    /// from `isSolidRender`, and the looser of the two: every solid render occludes, while 14,747
    /// states (pistons, walls, dirt paths) occlude as a full cube without being one. Exported for
    /// the audit; no generation caller asks it yet, which is why it is allowed to be idle rather
    /// than plumbed through a call site that does not want it.
    pub(crate) fn can_occlude(&self, st: u32) -> bool {
        self.pred_of(st) & FACT_CAN_OCCLUDE != 0
    }
    /// The state's collision boxes, from the same `getCollisionShape(EmptyBlockView, ORIGIN)`
    /// answer the fact table records as `COLLISION_EMPTY`/`COLLISION_FULL`: `collision_boxes`
    /// and those two bits agree on all 26,684 states. This is the geometry; the table owns the
    /// predicates, so a caller that only asks "does it collide" reads the table.
    pub(crate) fn collision_boxes(
        &self,
        st: u32,
    ) -> impl Iterator<Item = ([f64; 3], [f64; 3])> + '_ {
        let (start, len) = self.col_range.get(st as usize).copied().unwrap_or((0, 0));
        let slice = &self.col_idx[start as usize..start as usize + len as usize];
        slice.iter().map(|&idx| self.shapes[idx as usize])
    }
    /// Pinned BlockState.isSolidRender, not isSolid or light opacity: e.g.
    /// tinted glass blocks light but does not orient a dungeon chest.
    pub(crate) fn is_solid_render(&self, st: u32) -> bool {
        self.pred_of(st) & FACT_SOLID_RENDER != 0
    }
    /// Pinned legacy `BlockState.isSolid()` (`dtb$a.e`), the shape-heuristic
    /// predicate standing signs and banners still ask. This is not
    /// `isSolidRender(level, pos)`: the two are separate native methods.
    pub(crate) fn is_legacy_solid(&self, st: u32) -> bool {
        self.st_fl
            .get(st as usize)
            .map(|f| f & FL_LEGACY_SOLID != 0)
            .unwrap_or(false)
    }
    /// Pinned `getCollisionShape(level, pos).isEmpty()`. `collision_boxes` is the geometry of the
    /// same call and agrees with this bit on all 26,684 states; a caller that needs the boxes
    /// rather than the predicate reads that, and the two cannot drift because one asset exports
    /// both from one invocation.
    pub(crate) fn is_collision_empty(&self, st: u32) -> bool {
        self.pred_of(st) & FACT_COLLISION_EMPTY != 0
    }
    /// Pinned `BlockStateBase.isCollisionShapeFullBlock`; distinct from render, heightmap,
    /// and sturdy-face predicates.
    pub(crate) fn is_collision_shape_full_block(&self, st: u32) -> bool {
        self.pred_of(st) & FACT_COLLISION_FULL != 0
    }
    /// Pinned `BlockStateBase.isRedstoneConductor(level, pos)` (`dtb$a.g`):
    /// default in `BlockBehaviour.Properties` is `blocksMotion && isCollisionShapeFullBlock`,
    /// overridden in `Blocks.<clinit>` to `always` for `mud` and `soul_sand`, and `never` for
    /// glass, stained_glass, tinted_glass, ice, frosted_ice, beacon, leaves, copper grates,
    /// copper bulbs, glowstone, sea_lantern, redstone_block, tnt, observer, piston, and
    /// sticky_piston. Verified across all 26,684 states of the pinned 1.21.1 server jar.
    pub(crate) fn is_redstone_conductor(&self, st: u32) -> bool {
        let name = self.name_of(st);
        if matches!(name, "minecraft:mud" | "minecraft:soul_sand") {
            return true;
        }
        if matches!(
            name,
            "minecraft:glass"
                | "minecraft:tinted_glass"
                | "minecraft:ice"
                | "minecraft:frosted_ice"
                | "minecraft:beacon"
                | "minecraft:glowstone"
                | "minecraft:sea_lantern"
                | "minecraft:redstone_block"
                | "minecraft:tnt"
                | "minecraft:observer"
                | "minecraft:piston"
                | "minecraft:sticky_piston"
        ) || name.ends_with("_stained_glass")
            || name.ends_with("_leaves")
            || name.ends_with("copper_grate")
            || name.ends_with("copper_bulb")
        {
            return false;
        }
        self.blocks_motion(st) && self.is_collision_shape_full_block(st)
    }
    /// Pinned `isFaceSturdy(level, pos, direction, supportType)`. `direction` uses the native
    /// DOWN/UP/NORTH/SOUTH/WEST/EAST indices. `Support::Center` is the `canSupportCenter`
    /// question (torches, plants, signs) and differs from `Full` on 10,621 states for one
    /// direction alone: a fence post, a piston or a wall supports a centre but covers no
    /// whole face.
    pub(crate) fn is_face_sturdy(&self, st: u32, direction: u8, support: Support) -> bool {
        if direction >= 6 {
            return false;
        }
        let faces = self.faces.get(st as usize).copied().unwrap_or(0);
        (faces >> (support as usize * 6)) & 0x3f & (1 << direction) != 0
    }
    /// The pinned FULL face, which is what vanilla's three-argument
    /// `BlockStateBase.isFaceSturdy(level, pos, dir)` answers and what every caller of this
    /// older name asks.
    pub(crate) fn side_solid(&self, st: u32, dir: usize) -> bool {
        dir < 6 && self.is_face_sturdy(st, dir as u8, Support::Full)
    }
    /// The remaining bits of `assets/block_predicates_1211.bin`: the four heightmap predicate
    /// bits, the double-plant flag and the water/source-water pair. The solid-render bit that
    /// table also carries is read from [`BReg::is_solid_render`] instead, from the pinned facts.
    fn native_predicates(&self, st: u32) -> u8 {
        self.predicates.get(st as usize).copied().unwrap_or(0)
    }
    /// Native FULL support face for the mineshaft floor and chain anchors.
    pub(crate) fn mineshaft_sturdy_face(&self, st: u32, face: u8) -> bool {
        face < 6 && self.is_face_sturdy(st, face, Support::Full)
    }
    /// The chain anchor also asks whether the block is an instance of the chain class, which is
    /// the one fact `assets/mineshaft_support_1211.bin` records that the pinned table does not.
    pub(crate) fn mineshaft_chain_anchor(&self, st: u32) -> bool {
        self.mineshaft
            .get(st as usize)
            .is_some_and(|v| v & 192 == 64)
            && !self.in_tag(st, "minecraft:unstable_bottom_center")
    }
    /// Pinned `Heightmap.Types` predicate bits, from `assets/block_predicates_1211.bin`.
    pub(crate) fn heightmap_counts(&self, st: u32, kind: HeightmapKind) -> bool {
        self.native_predicates(st) & kind.mask() != 0
    }
    pub(crate) fn is_double_plant(&self, st: u32) -> bool {
        self.native_predicates(st) & 32 != 0
    }
    pub(crate) fn has_water(&self, st: u32) -> bool {
        self.native_predicates(st) & 64 != 0
    }
    /// Native MultifaceBlock.canAttachTo on this state, seen in the attachment
    /// direction from the growth cell (the tested support face is opposite).
    pub(crate) fn supports_multiface(&self, st: u32, direction: u8) -> bool {
        self.multiface
            .get(st as usize)
            .is_some_and(|faces| faces & (1 << direction) != 0)
    }
    /// The actual FluidState type, not the block name (waterlogged plants and
    /// solids contain source water; liquid block levels may contain flowing fluid).
    pub(crate) fn fluid_name(&self, st: u32) -> &'static str {
        let source = self.native_predicates(st) & 128 != 0;
        if self.has_water(st) {
            if source {
                "minecraft:water"
            } else {
                "minecraft:flowing_water"
            }
        } else if self.name_of(st) == "minecraft:lava" {
            if source {
                "minecraft:lava"
            } else {
                "minecraft:flowing_lava"
            }
        } else {
            "minecraft:empty"
        }
    }
    /// Pinned `!getFluidState().isEmpty()`: the state carries fluid, which includes waterlogged
    /// plants and corals. The narrower native `liquid()` - only water, lava and bubble columns -
    /// is [`BReg::is_liquid_block`].
    pub(crate) fn is_liquid(&self, st: u32) -> bool {
        self.pred_of(st) & FACT_FLUID_EMPTY == 0
    }
    /// Pinned `BlockStateBase.liquid()`. Exported for the audit; generation asks
    /// [`BReg::is_liquid`], which is the fluid-state question the call sites mean.
    pub(crate) fn is_liquid_block(&self, st: u32) -> bool {
        self.pred_of(st) & FACT_LIQUID != 0
    }
    /// The pinned `SupportType` face planes as a six-bit mask, for the audit and for the
    /// cross-check against the older single-plane assets.
    #[cfg(all(test, megu_vanilla_data))]
    pub(crate) fn face_plane(&self, st: u32, support: Support) -> u8 {
        let faces = self.faces.get(st as usize).copied().unwrap_or(0);
        ((faces >> (support as usize * 6)) & 0x3f) as u8
    }
    pub(crate) fn is_replaceable(&self, st: u32) -> bool {
        self.is_air(st)
            || self
                .st_fl
                .get(st as usize)
                .map(|f| f & FL_REPLACEABLE != 0)
                .unwrap_or(false)
    }
    /// Bounded shape update when an exposed template face supplies a newly
    /// placed ordinary full cube to an existing horizontal fence. The direction
    /// points FROM the fence TOWARDS the new neighbour, using the native
    /// DOWN/UP/NORTH/SOUTH/WEST/EAST indices.
    ///
    /// This is deliberately not a general BlockState.updateShape implementation.
    /// Fences also connect to eligible gates/fences and some non-full faces,
    /// and other block families have different rules. The solid-template path
    /// only adds full-cube neighbours, so it can prove this positive transition
    /// without fabricating negative/replacement transitions or runtime behavior.
    pub(crate) fn fence_after_full_neighbor(
        &self,
        state: u32,
        neighbor: u32,
        toward_neighbor: u8,
    ) -> Option<u32> {
        if !self.name_of(state).ends_with("_fence") {
            return None;
        }
        let key = match toward_neighbor {
            2 => "north",
            3 => "south",
            4 => "west",
            5 => "east",
            _ => return None,
        };
        // A fully colliding cube is not necessarily a valid fence support:
        // non-occluding blocks and vanilla's connection exceptions are excluded.
        if self.is_template_solid(neighbor) != Some(true)
            || !self.is_collision_shape_full_block(neighbor)
            || !self.is_solid_render(neighbor)
            || matches!(
                self.name_of(neighbor),
                "minecraft:barrier"
                    | "minecraft:carved_pumpkin"
                    | "minecraft:jack_o_lantern"
                    | "minecraft:melon"
                    | "minecraft:pumpkin"
            )
        {
            return None;
        }
        (self.get_prop(state, key).as_deref() == Some("false"))
            .then(|| self.with_prop(state, key, "true"))
    }

    /// The solid-template placer admits only ordinary full cube outputs.
    /// Adjacent panes connect to that face (including iron bars, whose name
    /// does not end in "_pane"). Other, non-full neighbors need their own
    /// native updateShape contract.
    pub(crate) fn pane_after_full_neighbor(
        &self,
        state: u32,
        neighbor: u32,
        toward_neighbor: u8,
    ) -> Option<u32> {
        let name = self.name_of(state);
        if !name.ends_with("_pane") && name != "minecraft:iron_bars" {
            return None;
        }
        let key = match toward_neighbor {
            2 => "north",
            3 => "south",
            4 => "west",
            5 => "east",
            _ => return None,
        };
        if self.is_template_solid(neighbor) != Some(true)
            || !self.is_collision_shape_full_block(neighbor)
            || !self.is_solid_render(neighbor)
        {
            return None;
        }
        (self.get_prop(state, key).as_deref() == Some("false"))
            .then(|| self.with_prop(state, key, "true"))
    }

    /// Recompute LOW/NONE wall sides for horizontal changes that are ordinary
    /// opaque full cubes, other walls, or air. With air above, a straight
    /// opposite two-sided wall suppresses its central post; corners, endpoints
    /// and T junctions retain it. Native TALL/covered walls need shape coverage
    /// checks and are intentionally not rewritten by this bounded rule.
    pub(crate) fn wall_after_full_neighbor(
        &self,
        state: u32,
        neighbor: u32,
        above: u32,
        toward_neighbor: u8,
    ) -> Option<u32> {
        if !self.name_of(state).ends_with("_wall") || !self.is_air(above) {
            return None;
        }
        let key = match toward_neighbor {
            2 => "north",
            3 => "south",
            4 => "west",
            5 => "east",
            _ => return None,
        };
        let other_name = self.name_of(neighbor);
        let cube = self.is_template_solid(neighbor) == Some(true)
            && self.is_collision_shape_full_block(neighbor)
            && self.is_solid_render(neighbor);
        let wanted = if cube || other_name.ends_with("_wall") {
            "low"
        } else if self.is_air(neighbor) {
            "none"
        } else {
            return None;
        };
        if ["north", "south", "west", "east"]
            .iter()
            .any(|side| self.get_prop(state, side).as_deref() == Some("tall"))
        {
            return None;
        }
        let next = self.with_prop(state, key, wanted);
        let north = self.get_prop(next, "north").as_deref() == Some("low");
        let south = self.get_prop(next, "south").as_deref() == Some("low");
        let east = self.get_prop(next, "east").as_deref() == Some("low");
        let west = self.get_prop(next, "west").as_deref() == Some("low");
        let straight = (north && south && !east && !west)
            || (east && west && !north && !south);
        let result = self.with_prop(next, "up", if straight { "false" } else { "true" });
        (result != state).then_some(result)
    }

    /// Bounded runtime counterpart for immediate air removal / full-cube
    /// replacement. This can disconnect a previously connected side. For
    /// non-full neighbors, other connection types (gates, fences, walls and
    /// panes) require independent native classification and are not guessed.
    pub(crate) fn connection_after_neighbor(
        &self,
        state: u32,
        neighbor: u32,
        above: u32,
        direction: u8,
    ) -> Option<u32> {
        if let Some(connected) = self.fence_after_full_neighbor(state, neighbor, direction) {
            return Some(connected);
        }
        if let Some(connected) = self.pane_after_full_neighbor(state, neighbor, direction) {
            return Some(connected);
        }
        if let Some(connected) = self.wall_after_full_neighbor(state, neighbor, above, direction) {
            return Some(connected);
        }
        let key = match direction {
            2 => "north",
            3 => "south",
            4 => "west",
            5 => "east",
            _ => return None,
        };
        let name = self.name_of(state);
        let other = self.name_of(neighbor);
        let fence = name.ends_with("_fence");
        let other_fence = other.ends_with("_fence");
        let fence_connect = fence && other_fence
            && ((name == "minecraft:nether_brick_fence")
                == (other == "minecraft:nether_brick_fence"));
        let pane = name.ends_with("_pane") || name == "minecraft:iron_bars";
        let other_pane = other.ends_with("_pane") || other == "minecraft:iron_bars";
        if fence_connect || (pane && other_pane) {
            return (self.get_prop(state, key).as_deref() != Some("true"))
                .then(|| self.with_prop(state, key, "true"));
        }
        if self.is_air(neighbor) && (fence || pane) {
            return (self.get_prop(state, key).as_deref() == Some("true"))
                .then(|| self.with_prop(state, key, "false"));
        }
        // The isolated post remains raised in this case. Do not silently
        // overwrite tall/multi-sided wall shape geometry with a guessed post.
        // This cleanup exists only for neighbors `wall_after_full_neighbor`
        // cannot classify; cubes, walls and air are already decided there, and
        // overriding their "low"/"none" verdicts here makes two adjacent walls
        // alternately disconnect and reconnect each other forever.
        let cube = self.is_template_solid(neighbor) == Some(true)
            && self.is_collision_shape_full_block(neighbor)
            && self.is_solid_render(neighbor);
        let unclassified = !cube && !other.ends_with("_wall") && !self.is_air(neighbor);
        if name.ends_with("_wall")
            && unclassified
            && self.is_air(above)
            && self.get_prop(state, "up").as_deref() == Some("true")
            && self.get_prop(state, key).as_deref() == Some("low")
            && ["north", "south", "west", "east"].iter().all(|other| {
                *other == key || self.get_prop(state, other).as_deref() == Some("none")
            })
        {
            return Some(self.with_prop(state, key, "none"));
        }
        None
    }

    /// Stair shape selection is a function of surrounding STAIR orientations,
    /// not whether the changed neighbor is a solid cube. After template writes,
    /// the caller supplies a write-through world view. The result is the state
    /// change only; waterlogged, half and facing properties are preserved.
    pub(crate) fn stair_after_neighbors(
        &self,
        state: u32,
        pos: (i32, i32, i32),
        mut get: impl FnMut((i32, i32, i32)) -> u32,
    ) -> Option<u32> {
        if !self.name_of(state).ends_with("_stairs") {
            return None;
        }
        let facing = self.get_prop(state, "facing")?;
        let half = self.get_prop(state, "half")?;
        let offset = |direction: &str| -> Option<(i32, i32, i32)> {
            Some(match direction {
                "north" => (0, 0, -1),
                "south" => (0, 0, 1),
                "west" => (-1, 0, 0),
                "east" => (1, 0, 0),
                _ => return None,
            })
        };
        let at = |(dx, dy, dz): (i32, i32, i32)| {
            (pos.0.wrapping_add(dx), pos.1.wrapping_add(dy), pos.2.wrapping_add(dz))
        };
        let (dx, _, dz) = offset(&facing)?;
        let front = get(at((dx, 0, dz)));
        let stair_face = |other: u32| -> Option<String> {
            (self.name_of(other).ends_with("_stairs")
                && self.get_prop(other, "half").as_deref() == Some(half.as_str()))
                .then(|| self.get_prop(other, "facing"))
                .flatten()
        };
        let perpendicular = |direction: &str| match facing.as_str() {
            "north" | "south" => matches!(direction, "east" | "west"),
            "east" | "west" => matches!(direction, "north" | "south"),
            _ => false,
        };
        let ccw = match facing.as_str() {
            "north" => "west",
            "south" => "east",
            "west" => "south",
            "east" => "north",
            _ => return None,
        };
        // Vanilla's canTakeShape checks the flank opposite the front stair's
        // turn, or the same direction as the back stair's turn. Without that
        // check a third aligned stair becomes an incorrect inside corner.
        let can_take = |side: &str, get: &mut dyn FnMut((i32, i32, i32)) -> u32| -> bool {
            let Some(o) = offset(side) else { return false };
            let s = get(at(o));
            !self.name_of(s).ends_with("_stairs")
                || self.get_prop(s, "half").as_deref() != Some(half.as_str())
                || self.get_prop(s, "facing").as_deref() != Some(facing.as_str())
        };
        let mut shape = "straight";
        if let Some(dir) = stair_face(front) {
            if perpendicular(&dir) {
                let opposite = match dir.as_str() {
                    "north" => "south",
                    "south" => "north",
                    "west" => "east",
                    "east" => "west",
                    _ => return None,
                };
                if can_take(opposite, &mut get) {
                    shape = if dir == ccw { "outer_left" } else { "outer_right" };
                }
            }
        }
        if shape == "straight" {
            // Native only reads the rear stair if the front did not already
            // establish an outer corner; keep scripted-world reads bounded.
            let back = get(at((-dx, 0, -dz)));
            if let Some(dir) = stair_face(back) {
                if perpendicular(&dir) && can_take(&dir, &mut get) {
                    shape = if dir == ccw { "inner_left" } else { "inner_right" };
                }
            }
        }
        (self.get_prop(state, "shape").as_deref() != Some(shape))
            .then(|| self.with_prop(state, "shape", shape))
    }

    /// `StructurePiece.SHAPE_CHECK_BLOCKS`: the blocks whose `placeBlock` marks the
    /// cell with `ChunkAccess.markPosForPostprocessing`, so the owning chunk's
    /// `LevelChunk.postProcessGeneration` recomputes the connection shape when it is
    /// promoted. Exactly eleven `Blocks` identities in the pinned version — the set
    /// is by block, so the other fence woods, glass panes and the soul/redstone torch
    /// family are deliberately absent.
    pub(crate) fn needs_postprocess_shape(&self, st: u32) -> bool {
        matches!(
            self.name_of(st),
            "minecraft:nether_brick_fence"
                | "minecraft:torch"
                | "minecraft:wall_torch"
                | "minecraft:oak_fence"
                | "minecraft:spruce_fence"
                | "minecraft:birch_fence"
                | "minecraft:jungle_fence"
                | "minecraft:acacia_fence"
                | "minecraft:dark_oak_fence"
                | "minecraft:ladder"
                | "minecraft:iron_bars"
        )
    }

    /// The side a torch/lantern family member reads as its support and the
    /// `Support` plane vanilla asks on it:
    ///
    /// - floor torches and floor lanterns: `canSupportCenter` of the block
    ///   below (`TorchBlock.canSurvive` / `LanternBlock.canSurvive`)
    /// - hanging lanterns: `canSupportCenter` of the block above
    /// - wall torches: the three-argument `isFaceSturdy` (FULL) of the block
    ///   behind (`WallTorchBlock.canSurvive` -> `WallTorchBlock.canAttach`)
    ///
    /// Returns `(direction_from_the_block_toward_its_support, plane)` using the
    /// native direction indices (`down/up/north/south/west/east` = `0..=5`), or
    /// `None` for states outside the family. `updateShape` re-asks this face
    /// whenever a change lands on the support side, so a support replaced by a
    /// non-sturdy block (not only air/liquid) also drops the attached block.
    pub(crate) fn attached_support(&self, state: u32) -> Option<(u8, Support)> {
        let name = self.name_of(state);
        match name {
            "minecraft:torch" | "minecraft:soul_torch" | "minecraft:redstone_torch" => {
                Some((0, Support::Center))
            }
            "minecraft:wall_torch"
            | "minecraft:soul_wall_torch"
            | "minecraft:redstone_wall_torch" => {
                let face = self.get_prop(state, "facing");
                let behind = match face.as_deref() {
                    Some("north") => 3,
                    Some("south") => 2,
                    Some("west") => 5,
                    Some("east") => 4,
                    _ => return None,
                };
                Some((behind, Support::Full))
            }
            "minecraft:lantern" | "minecraft:soul_lantern" => {
                let hanging = self.get_prop(state, "hanging");
                Some((
                    if hanging.as_deref() == Some("true") { 1 } else { 0 },
                    Support::Center,
                ))
            }
            _ => None,
        }
    }

    /// Re-ask a torch/lantern family's support rule after the direct support
    /// cell changes. The direction points FROM the fragile block TOWARDS its
    /// support, in the six native direction indices. Vanilla `updateShape`
    /// does not merely test air/liquid: replacing stone with leaves, a fence,
    /// or another non-supporting block breaks the attachment too.
    ///
    /// Rails, buttons, plates, plants and conditional multi-block shapes are
    /// separate contracts; `attached_support` owns this shared attachment
    /// table for the families whose support rule is a single face.
    pub(crate) fn fragile_loses_support(
        &self,
        state: u32,
        toward_support: u8,
        new_support: u32,
    ) -> bool {
        self.attached_support(state)
            .is_some_and(|(attach, plane)| {
                attach == toward_support
                    && !self.is_face_sturdy(new_support, toward_support ^ 1, plane)
            })
    }

    /// set a property by name on a resolved state; missing -> unchanged
    pub(crate) fn with_prop(&self, st: u32, prop: &str, val: &str) -> u32 {
        let bi = match self.st_blk.get(st as usize) {
            Some(&b) => b,
            None => return st,
        };
        let ps = &self.props[bi as usize];
        let off = st.saturating_sub(self.st_lo[bi as usize]);
        for (i, p) in ps.iter().enumerate() {
            if p.name != prop {
                continue;
            }
            let mult: u64 = ps[i + 1..]
                .iter()
                .map(|q| q.vals.len().max(1) as u64)
                .product();
            let len = p.vals.len().max(1) as u64;
            let digit = (off as u64).checked_div(mult).map_or(0, |q| q % len);
            if let Some(nd) = p.vals.iter().position(|v| v == val) {
                // Move this property's digit to `nd` by the difference, NOT by
                // (nd + len - digit) % len. The offset already carries digit*mult, so
                // the wrapping form adds a positive delta where the answer needed a
                // negative one: setting age 15 back to 0 landed one state past the end
                // of the block and produced a jukebox.
                let o = off as i64 + (nd as i64 - digit as i64) * mult as i64;
                if o < 0 {
                    return st;
                }
                return self.st_lo[bi as usize] + o as u32;
            }
            return st;
        }
        st
    }
    /// LeavesBlock.getOptionalDistance: LOGS are zero; otherwise require
    /// the integer `distance` property with domain 1..=7, not scaffolding's 0..=7.
    pub(crate) fn distance_from_log(&self, st: u32) -> Option<u8> {
        if self.in_tag(st, "minecraft:logs") {
            return Some(0);
        }
        let bi = *self.st_blk.get(st as usize)? as usize;
        let p = self.props[bi].iter().find(|p| p.name == "distance")?;
        if p.vals.len() != 7
            || !p
                .vals
                .iter()
                .zip(1..=7)
                .all(|(v, d)| v.parse::<u8>().ok() == Some(d))
        {
            return None;
        }
        self.get_prop(st, "distance")?.parse().ok()
    }

    /// `minecraft:name[a=1,b=2]` with the property names sorted, the same spelling the
    /// region reader emits for a saved chunk. Whole-chunk comparison needs this: a name-only
    /// diff counts a stair facing the wrong way as equal.
    pub(crate) fn canonical_state(&self, st: u32) -> String {
        let Some(blk) = self.st_blk.get(st as usize).map(|b| *b as usize) else {
            return format!("<state {st} outside the registry>");
        };
        let name = self.bname.get(blk).cloned().unwrap_or_default();
        let mut parts: Vec<String> = self
            .props
            .get(blk)
            .map(|props| {
                props
                    .iter()
                    .filter_map(|p| self.get_prop(st, &p.name).map(|v| format!("{}={}", p.name, v)))
                    .collect()
            })
            .unwrap_or_default();
        parts.sort();
        if parts.is_empty() {
            name
        } else {
            format!("{}[{}]", name, parts.join(","))
        }
    }

    pub(crate) fn get_prop(&self, st: u32, prop: &str) -> Option<String> {
        let bi = *self.st_blk.get(st as usize)?;
        let ps = &self.props[bi as usize];
        let off = st.saturating_sub(self.st_lo[bi as usize]);
        for (i, p) in ps.iter().enumerate() {
            if p.name != prop {
                continue;
            }
            let mult: u64 = ps[i + 1..]
                .iter()
                .map(|q| q.vals.len().max(1) as u64)
                .product();
            let len = p.vals.len().max(1) as u64;
            let d = (off as u64).checked_div(mult).map_or(0, |q| q % len);
            return p.vals.get(d as usize).cloned();
        }
        None
    }
    pub(crate) fn has_prop(&self, name: &str, prop: &str) -> bool {
        self.idx
            .get(&ns_name(name))
            .map(|&bi| self.props[bi as usize].iter().any(|p| p.name == prop))
            .unwrap_or(false)
    }
    /// Ordered members of a block tag, as `getRandomElement(tag, random)` sees them.
    pub(crate) fn tag_members(&self, tag: &str) -> &[u32] {
        self.tag_order
            .get(&ns_name(tag))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// A block index (a tag member) as its default state id.
    pub(crate) fn default_state(&self, block_idx: u32) -> u32 {
        self.dfl.get(block_idx as usize).copied().unwrap_or(0)
    }

    /// Packed tag membership for `st`. The name resolves through a memoized
    /// slot and the state test is one bit AND — no hashing or set probing per
    /// candidate cell.
    pub(crate) fn in_tag(&self, st: u32, tag: &str) -> bool {
        let Some(&bi) = self.st_blk.get(st as usize) else {
            return false;
        };
        let Some(id) = self.tag_id(tag) else {
            return false;
        };
        let bits = &self.tag_bits[id as usize];
        let u = bi as usize;
        u >> 6 < bits.len() && (bits[u >> 6] & (1u64 << (u & 63))) != 0
    }

    /// Resolve a tag name to its dense id. Call-site names are string literals,
    /// so the same `&str` recurs for thousands of cells; a direct-mapped slot
    /// keyed on the pointer's bits memoizes the resolution. The stored name is
    /// always compared before trusting the slot, so a collision — or a
    /// recycled allocation — costs a miss, never a wrong answer.
    #[inline]
    fn tag_id(&self, tag: &str) -> Option<u32> {
        let slot = (tag.as_ptr() as usize).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            >> (usize::BITS - TAG_CACHE_SLOTS.trailing_zeros());
        let entry = self.tag_cache[slot].load(Ordering::Relaxed);
        if entry != 0 {
            let id = (entry - 1) as usize;
            if self.tag_name_eq(id, tag) {
                return Some(id as u32);
            }
        }
        let id = self.resolve_tag(tag)?;
        self.tag_cache[slot].store(id as u64 + 1, Ordering::Relaxed);
        Some(id)
    }

    /// Whether dense tag `id`'s canonical name is what `tag` resolves to —
    /// either verbatim or through the `minecraft:` prefix `ns_name` adds.
    #[inline]
    fn tag_name_eq(&self, id: usize, tag: &str) -> bool {
        let name = self.tag_names[id].as_ref();
        name == tag
            || (!tag.contains(':') && name.strip_prefix("minecraft:") == Some(tag))
    }

    /// The cold half of `in_tag`: canonical-name lookup in `tag_ids`, with the
    /// `minecraft:` prefix fallback `ns_name` documents. Runs once per distinct
    /// name thanks to `tag_cache`.
    fn resolve_tag(&self, tag: &str) -> Option<u32> {
        if let Some(&id) = self.tag_ids.get(tag) {
            return Some(id);
        }
        if !tag.contains(':') {
            let qualified = format!("minecraft:{tag}");
            return self.tag_ids.get(qualified.as_str()).copied();
        }
        None
    }
}

// ---------- dynamic mod block facts (`mod-suite`) ----------
//
// A dynamically registered block arrives with no generated fact-table row, so
// `extend_create_registry` derives per-state physical facts from the block id
// and this state's property pairs. The derivation is namespace-agnostic and
// generic — name and property vocabulary only, never a mod id — so any mod
// block registered through this path gets vanilla-like opacity, luminance,
// collision, face sturdiness and tag membership without a per-mod table.

/// Shape family a mod block's id implies. First match wins, in the order
/// [`mod_block_shape`] checks them.
#[cfg(feature = "mod-suite")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModShape {
    /// Torches, rails, plants and wire: no collision, no opacity.
    Empty,
    /// Glassy fittings and thin mechanical mounts block motion without a
    /// full-cube collision and pass light.
    Thin,
    /// Slabs, stairs, fences and friends cover part of the cube and dim
    /// light by one level.
    Partial,
    /// `*leaves*`: a full-cube body that still passes most light.
    Leaves,
    /// Fluid blocks by name (`*_water`, `*_lava`, `*_fluid`, `*_liquid`).
    Liquid,
    /// The default: an opaque full cube.
    Full,
}

/// Classify a mod block by its path (the part after `namespace:`). The
/// keyword lists are deliberately generic; an unrecognised block answers
/// `Full`, the safe opaque-cube default.
#[cfg(feature = "mod-suite")]
fn mod_block_shape(path: &str) -> ModShape {
    // `_block` names are cubes first: `grass_block` must not read as `grass`.
    if path == "block" || path.ends_with("_block") {
        return ModShape::Full;
    }
    // Suffixes, not substrings: `fluid_pipe` and `water_wheel` are mounts,
    // not fluids.
    if ["water", "lava", "fluid", "liquid"]
        .iter()
        .any(|k| path.ends_with(k))
    {
        return ModShape::Liquid;
    }
    if path.contains("leaves") {
        return ModShape::Leaves;
    }
    // Empty-collision fittings: they stop neither light nor entities.
    const EMPTY: &[&str] = &[
        "torch", "rail", "flower", "sapling", "crop", "vine", "wire", "button",
        "lever", "sign", "banner", "candle", "mushroom", "fern", "coral",
        "reed", "lichen", "grass", "plant", "seed",
    ];
    if EMPTY.iter().any(|k| path.contains(k)) {
        return ModShape::Empty;
    }
    // Thin mounts and glassy fittings block motion — usually carrying the
    // machine's working parts — without a full-cube collision.
    const THIN: &[&str] = &[
        "glass", "pane", "grate", "bars", "chain", "lantern", "pipe", "shaft",
        "belt", "cogwheel", "wheel", "funnel", "chute", "spout", "depot",
        "basin", "pump", "bearing", "ladder", "scaffold", "frame",
        "pressure_plate", "door", "trapdoor", "hopper", "cauldron", "anvil",
        "conduit",
    ];
    if THIN.iter().any(|k| path.contains(k)) {
        return ModShape::Thin;
    }
    // Partial occluders cover part of the cube; `bed` is a suffix so
    // `bedrock` still reads as the full cube it is.
    const PARTIAL: &[&str] = &[
        "slab", "stair", "fence", "wall", "carpet", "snow", "path", "farmland",
    ];
    if PARTIAL.iter().any(|k| path.contains(k)) || path.ends_with("_bed") {
        return ModShape::Partial;
    }
    ModShape::Full
}

/// Emission a mod block state's name implies, before properties adjust it:
/// `magma` is a dull 7, the rest of the emitter vocabulary a torch-like 14.
/// `lit`/`powered`/`blaze` properties then raise or clear it per state.
#[cfg(feature = "mod-suite")]
fn mod_state_luminance(path: &str, prop_pairs: &[(String, String)]) -> u8 {
    let base = if path.contains("magma") {
        7
    } else if [
        "lamp",
        "lantern",
        "torch",
        "glow",
        "blaze_burner",
        "light",
        "crystal",
        "luminous",
        "forge",
    ]
    .iter()
    .any(|k| path.contains(k))
    {
        14
    } else {
        0
    };
    let mut luminance = base;
    for (key, value) in prop_pairs {
        match (key.as_str(), value.as_str()) {
            ("lit", "true") => luminance = 15,
            ("lit", _) => luminance = 0,
            ("blaze", level) => {
                luminance = match level {
                    "seething" => 15,
                    "kindled" => 11,
                    "smouldering" => 7,
                    _ => 0,
                }
            }
            // `powered` only lights blocks that read as lamps or burners.
            ("powered", "true") if base > 0 => luminance = 15,
            _ => {}
        }
    }
    luminance.min(15)
}

/// The resolved per-state facts `extend_create_registry` writes into the
/// dense columns.
#[cfg(feature = "mod-suite")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModBlockFacts {
    /// The packed predicate byte (`pred` column).
    pub(crate) pred: u8,
    /// The packed light byte, `emission << 4 | opacity`.
    pub(crate) light: u8,
    /// The three `Support` face planes (`faces` column).
    pub(crate) faces: u32,
    /// The `native_predicates` byte: heightmap bits plus the water pair.
    pub(crate) native: u8,
}

/// Derive one modded state's physical facts from its block id and resolved
/// property pairs. Full solids answer like vanilla stone: opacity 15,
/// `BLOCKS_MOTION | SOLID_RENDER | COLLISION_FULL | FLUID_EMPTY`, all six
/// faces sturdy on every support plane, so sunlight is blocked, entities
/// collide, redstone conducts and torches/rails/buttons attach. Decorative
/// shapes answer the transparent/partial facts their family implies, and
/// `waterlogged=true` always clears the empty-fluid bit.
#[cfg(feature = "mod-suite")]
pub(crate) fn derive_mod_block_facts(
    block_id: &str,
    prop_pairs: &[(String, String)],
) -> ModBlockFacts {
    let path = block_id.rsplit(':').next().unwrap_or(block_id);
    let shape = mod_block_shape(path);
    let waterlogged = prop_pairs
        .iter()
        .any(|(k, v)| k == "waterlogged" && v == "true");
    let mut opacity = match shape {
        ModShape::Empty | ModShape::Thin => 0,
        ModShape::Partial | ModShape::Leaves | ModShape::Liquid => 1,
        ModShape::Full => 15,
    };
    if waterlogged {
        opacity = opacity.max(1);
    }
    let mut pred = match shape {
        ModShape::Empty => FACT_COLLISION_EMPTY | FACT_FLUID_EMPTY,
        ModShape::Thin | ModShape::Partial => FACT_BLOCKS_MOTION | FACT_FLUID_EMPTY,
        ModShape::Leaves => {
            FACT_BLOCKS_MOTION | FACT_SOLID_RENDER | FACT_COLLISION_FULL
                | FACT_FLUID_EMPTY
        }
        ModShape::Liquid => FACT_LIQUID | FACT_COLLISION_EMPTY,
        ModShape::Full => {
            FACT_BLOCKS_MOTION | FACT_SOLID_RENDER | FACT_FLUID_EMPTY
                | FACT_COLLISION_FULL
        }
    };
    if waterlogged {
        pred &= !FACT_FLUID_EMPTY;
    }
    let faces = match shape {
        ModShape::Full | ModShape::Leaves => 0x003f_3f3f,
        _ => 0,
    };
    // `native_predicates`: WORLD_SURFACE counts every non-air state,
    // OCEAN_FLOOR and MOTION_BLOCKING count colliders and fluids with the
    // no-leaves variant skipping Leaves, and bits 64/128 mark water
    // carriers (a waterlogged mount or a `*_water` fluid).
    let mut native = 2;
    if pred & FACT_BLOCKS_MOTION != 0 || pred & FACT_LIQUID != 0 {
        native |= 4 | 8;
        if shape != ModShape::Leaves {
            native |= 16;
        }
    }
    if waterlogged || path.ends_with("water") {
        native |= 64 | 128;
    }
    ModBlockFacts {
        pred,
        light: (mod_state_luminance(path, prop_pairs) << 4) | (opacity & 0x0f),
        faces,
        native,
    }
}

/// The vanilla block tags a modded block's id implies, namespace-agnostic:
/// `*_planks` -> `minecraft:planks`, `*_leaves` -> `minecraft:leaves`,
/// `*_ore` and solid blocks -> `minecraft:mineable/pickaxe`, wood-family
/// solids -> `minecraft:mineable/axe`.
#[cfg(feature = "mod-suite")]
fn mod_block_tag_names(path: &str, shape: ModShape) -> Vec<&'static str> {
    let mut tags = Vec::new();
    if path.contains("leaves") {
        tags.push("minecraft:leaves");
        tags.push("minecraft:mineable/hoe");
    }
    if path.ends_with("_bed") {
        tags.push("minecraft:beds");
    }
    if path.ends_with("_planks") {
        tags.push("minecraft:planks");
    }
    if path.ends_with("_log") || path.ends_with("_logs") {
        tags.push("minecraft:logs");
    }
    if path.ends_with("_slab") {
        tags.push("minecraft:slabs");
    }
    if path.ends_with("_stairs") {
        tags.push("minecraft:stairs");
    }
    if path.ends_with("_fence") {
        tags.push("minecraft:fences");
    }
    if path.ends_with("_fence_gate") {
        tags.push("minecraft:fence_gates");
    }
    if path.ends_with("_wall") {
        tags.push("minecraft:walls");
    }
    if path.ends_with("_door") {
        tags.push("minecraft:doors");
    }
    if path.ends_with("_trapdoor") {
        tags.push("minecraft:trapdoors");
    }
    if path.ends_with("_rail") {
        tags.push("minecraft:rails");
    }
    // The flag LUT's climbable table (ladders, vines, scaffolding) names the
    // modded ids that belong on the vanilla climbable tag.
    if megu_core::block_flags_lut::classify_name(path)
        & megu_core::block_flags_lut::FLAG_CLIMBABLE
        != 0
    {
        tags.push("minecraft:climbable");
    }
    let woody =
        path.contains("plank") || path.ends_with("_log") || path.contains("wood");
    if path.ends_with("_ore") || matches!(shape, ModShape::Full | ModShape::Partial) {
        tags.push(if woody {
            "minecraft:mineable/axe"
        } else {
            "minecraft:mineable/pickaxe"
        });
    }
    tags
}

fn dp_tag_files(sub: &str) -> HashMap<String, Vec<u8>> {
    let mut m = HashMap::new();
    let base = std::path::Path::new("datapacks");
    if let Ok(packs) = std::fs::read_dir(base) {
        for p in packs.flatten() {
            let root = p.path().join("data");
            if let Ok(nss) = std::fs::read_dir(&root) {
                for ns in nss.flatten() {
                    let dir = ns.path().join(format!("tags/{}", sub));
                    let mut stack = vec![dir.clone()];
                    while let Some(d) = stack.pop() {
                        if let Ok(rd) = std::fs::read_dir(&d) {
                            for e in rd.flatten() {
                                let p = e.path();
                                if p.is_dir() {
                                    stack.push(p);
                                } else if p.extension().map(|x| x == "json").unwrap_or(false) {
                                    if let Ok(rel) = p.strip_prefix(&dir) {
                                        let key = format!(
                                            "{}:{}",
                                            ns.file_name().to_string_lossy(),
                                            rel.with_extension("").to_string_lossy()
                                        );
                                        if let Ok(b) = std::fs::read(&p) {
                                            m.entry(key).or_insert(b);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    m
}

pub(crate) fn reg_props_axis(name: &str) -> Option<()> {
    breg().and_then(|r| r.has_prop(name, "axis").then_some(()))
}

// ---------- loader-agnostic dynamic blocks and items ----------
//
// A loader — Fabric, NeoForge, the WASM runtime, a datapack — states what a block or an item
// is; this section is the neutral vocabulary it states it in. Nothing here reads a name's
// spelling to decide a property: light emission, opacity, hardness, solidity and
// replaceability come from the entry, and a block's drop is the item its loader registered
// under the same name. The Create adapter keeps its own path (`BReg::extend_mod_blocks`), so
// nothing about that lane moves.

/// One loader-supplied block: the vocabulary every loader maps its own block properties into.
#[derive(Clone, Debug)]
pub(crate) struct DynamicBlockEntry {
    /// The canonical `namespace:path` the block registers under, e.g. `mymod:glow_crystal`.
    pub(crate) name: String,
    /// `getLightEmission()`, clamped into `0..=15` when the columns are built.
    pub(crate) light_emission: u8,
    /// `getLightBlock()`, clamped into `0..=15` when the columns are built.
    pub(crate) light_opacity: u8,
    /// `destroyTime`: negative means unbreakable, exactly as in vanilla.
    pub(crate) hardness: f32,
    /// `blocksMotion()` with a full collision cube: what an opaque block answers.
    pub(crate) is_solid: bool,
    /// `isReplaceable()`: whether a placement may overwrite the block.
    pub(crate) is_replaceable: bool,
    /// The block's own property table in `state_of` order — the last property varies fastest,
    /// exactly like a vanilla block's.
    pub(crate) properties: Vec<(String, Vec<String>)>,
}

impl DynamicBlockEntry {
    /// An opaque, unreplaceable block at `hardness`, with no light and no properties: the shape
    /// most loaders' plain blocks take. The other fields stay writable, so a loader states the
    /// rest by assignment instead of through a builder that would guess at it.
    pub(crate) fn new(name: &str, hardness: f32) -> Self {
        Self {
            name: name.to_string(),
            light_emission: 0,
            light_opacity: 15,
            hardness,
            is_solid: true,
            is_replaceable: false,
            properties: Vec::new(),
        }
    }
}

/// How many loader-supplied blocks one process may register: the bound that keeps a hostile or
/// broken pack from growing the table without limit.
pub(crate) const MAX_DYNAMIC_MOD_BLOCKS: usize = 4096;
/// How many loader-supplied items one process may register, for the same reason.
pub(crate) const MAX_DYNAMIC_MOD_ITEMS: usize = 4096;
/// Cartesian states one loader-supplied block may expand to. A property table past this is
/// refused rather than truncated: a state id the loader cannot address is a loader bug, and
/// keeping a subset would hide it behind wrong state ids.
pub(crate) const MAX_DYNAMIC_BLOCK_STATES: usize = 4096;

/// One loader-supplied item: its canonical name and the stack size it declared.
struct DynamicModItem {
    name: String,
    max_stack: u8,
}

/// What loaders have contributed, in registration order.
#[derive(Default)]
struct DynamicModRegistry {
    blocks: Vec<DynamicBlockEntry>,
    /// Canonical block name -> index in `blocks`.
    block_by_name: HashMap<String, usize>,
    items: Vec<DynamicModItem>,
    /// Canonical item name -> index in `items`, which is also its id offset.
    item_by_name: HashMap<String, usize>,
}

static DYNAMIC_MOD: std::sync::OnceLock<std::sync::RwLock<DynamicModRegistry>> =
    std::sync::OnceLock::new();

/// How many blocks and items are registered, mirrored outside the lock: a world no loader
/// touched answers a lookup without taking it.
static DYNAMIC_MOD_BLOCK_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static DYNAMIC_MOD_ITEM_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn dynamic_mod_registry() -> &'static std::sync::RwLock<DynamicModRegistry> {
    DYNAMIC_MOD.get_or_init(|| std::sync::RwLock::new(DynamicModRegistry::default()))
}

/// The id of the item in `index`-th registration slot. The adapter range sits between
/// [`DYNAMIC_MOD_ITEM_BASE`] and it, so the two tables cannot overlap.
fn dynamic_mod_item_id_at(index: usize) -> i64 {
    DYNAMIC_MOD_ITEM_BASE + ADAPTER_ITEM_RANGE + index as i64
}

/// Register loader-supplied blocks and answer how many of `entries` are now registered.
///
/// A name that is not a valid [`megu_core::id::ResourceLocation`] is refused, as is anything
/// past [`MAX_DYNAMIC_MOD_BLOCKS`]. Registering a name twice keeps one block with the metadata
/// of its **first** declaration — the state ids a loaded world already carries were built from
/// it — so a loader that needs a different value clears the table and registers again.
///
/// The blocks reach the dense registry columns through [`BReg::extend_dynamic_blocks`], which
/// runs when the registry loads; registering after that answers metadata through
/// [`dynamic_mod_block`] but appends no states, because a state id already handed out cannot
/// change meaning.
pub(crate) fn register_dynamic_mod_blocks(entries: &[DynamicBlockEntry]) -> usize {
    let mut registry = dynamic_mod_registry()
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let mut accepted = 0;
    for entry in entries {
        let Ok(name) = megu_core::id::ResourceLocation::parse(entry.name.trim()) else {
            continue;
        };
        if registry.block_by_name.contains_key(name.as_str()) {
            accepted += 1;
            continue;
        }
        if registry.blocks.len() >= MAX_DYNAMIC_MOD_BLOCKS {
            continue;
        }
        let mut stored = entry.clone();
        stored.name = name.as_str().to_string();
        let idx = registry.blocks.len();
        registry.block_by_name.insert(stored.name.clone(), idx);
        registry.blocks.push(stored);
        accepted += 1;
    }
    DYNAMIC_MOD_BLOCK_COUNT.store(registry.blocks.len(), Ordering::Release);
    accepted
}

/// Register a loader-supplied item and answer its id, or the id its name already owns.
///
/// Ids are deterministic and bounded: the first item a process registers is
/// `DYNAMIC_MOD_ITEM_BASE + ADAPTER_ITEM_RANGE` and each later one takes the next slot, so the
/// answer depends only on the loader's registration order. `None` means the name is not a valid
/// resource location or the item table is already at [`MAX_DYNAMIC_MOD_ITEMS`]. `max_stack` is
/// clamped into `1..=64`.
pub(crate) fn register_dynamic_mod_item(name: &str, max_stack: u8) -> Option<i64> {
    let name = megu_core::id::ResourceLocation::parse(name.trim()).ok()?;
    let max_stack = max_stack.clamp(1, 64);
    let mut registry = dynamic_mod_registry()
        .write()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&index) = registry.item_by_name.get(name.as_str()) {
        return Some(dynamic_mod_item_id_at(index));
    }
    if registry.items.len() >= MAX_DYNAMIC_MOD_ITEMS {
        return None;
    }
    let index = registry.items.len();
    registry
        .item_by_name
        .insert(name.as_str().to_string(), index);
    registry.items.push(DynamicModItem {
        name: name.as_str().to_string(),
        max_stack,
    });
    DYNAMIC_MOD_ITEM_COUNT.store(registry.items.len(), Ordering::Release);
    Some(dynamic_mod_item_id_at(index))
}

/// A registered block's entry, or `None` for a name nothing registered.
pub(crate) fn dynamic_mod_block(name: &str) -> Option<DynamicBlockEntry> {
    if DYNAMIC_MOD_BLOCK_COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    let name = megu_core::id::ResourceLocation::parse(name.trim()).ok()?;
    let registry = dynamic_mod_registry()
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let index = *registry.block_by_name.get(name.as_str())?;
    registry.blocks.get(index).cloned()
}

/// The id of a registered item, by name.
pub(crate) fn dynamic_mod_item_id(name: &str) -> Option<i64> {
    dynamic_mod_item_facts(name).map(|(id, _)| id)
}

/// A registered item's canonical name and stack size, by id.
pub(crate) fn dynamic_mod_item(id: i64) -> Option<(String, u8)> {
    let index = usize::try_from(
        id.checked_sub(DYNAMIC_MOD_ITEM_BASE)?
            .checked_sub(ADAPTER_ITEM_RANGE)?,
    )
    .ok()?;
    if DYNAMIC_MOD_ITEM_COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    let registry = dynamic_mod_registry()
        .read()
        .unwrap_or_else(|e| e.into_inner());
    registry
        .items
        .get(index)
        .map(|item| (item.name.clone(), item.max_stack))
}

/// A registered item's id and stack size in one read: what a block's drop and stack columns
/// need, so the registry lock is taken once per block instead of once per value.
fn dynamic_mod_item_facts(name: &str) -> Option<(i64, u8)> {
    if DYNAMIC_MOD_ITEM_COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    let registry = dynamic_mod_registry()
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let index = *registry.item_by_name.get(name)?;
    Some((
        dynamic_mod_item_id_at(index),
        registry.items[index].max_stack,
    ))
}

/// Every registered block, sorted by canonical name so the state ids they take depend on the
/// set a loader contributed and not on the order it registered them in.
fn dynamic_mod_blocks_snapshot() -> Vec<DynamicBlockEntry> {
    if DYNAMIC_MOD_BLOCK_COUNT.load(Ordering::Acquire) == 0 {
        return Vec::new();
    }
    let mut blocks = dynamic_mod_registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .blocks
        .clone();
    blocks.sort_by(|a, b| a.name.cmp(&b.name));
    blocks
}

/// The states a block's property table expands to — the Cartesian product of the value lists —
/// or `0` when a list is empty or the product passes [`MAX_DYNAMIC_BLOCK_STATES`].
fn dynamic_block_state_count(properties: &[(String, Vec<String>)]) -> usize {
    let mut states = 1usize;
    for (_, values) in properties {
        if values.is_empty() {
            return 0;
        }
        let Some(next) = states.checked_mul(values.len()) else {
            return 0;
        };
        if next > MAX_DYNAMIC_BLOCK_STATES {
            return 0;
        }
        states = next;
    }
    states
}

/// Drop every loader-supplied block and item: the reload path a loader runs before it
/// contributes its registry again.
pub(crate) fn clear_dynamic_mod_registry() {
    {
        let mut registry = dynamic_mod_registry()
            .write()
            .unwrap_or_else(|e| e.into_inner());
        registry.blocks.clear();
        registry.block_by_name.clear();
        registry.items.clear();
        registry.item_by_name.clear();
    }
    DYNAMIC_MOD_BLOCK_COUNT.store(0, Ordering::Release);
    DYNAMIC_MOD_ITEM_COUNT.store(0, Ordering::Release);
}

// Loader-only entry points: one pin per signature. Registration happens in a loader, which
// this crate does not contain, so without these the compiler would report every one of them as
// dead code in a build that does not compile the tests.
const _: fn(&[DynamicBlockEntry]) -> usize = register_dynamic_mod_blocks;
const _: fn(&str, u8) -> Option<i64> = register_dynamic_mod_item;
const _: fn(&str) -> Option<i64> = dynamic_mod_item_id;
const _: fn(i64) -> Option<(String, u8)> = dynamic_mod_item;
const _: fn(&str) -> Option<DynamicBlockEntry> = dynamic_mod_block;
const _: fn() = clear_dynamic_mod_registry;
const _: fn(&str, f32) -> DynamicBlockEntry = DynamicBlockEntry::new;

static BREG: std::sync::OnceLock<Option<BReg>> = std::sync::OnceLock::new();
pub(crate) fn breg() -> Option<&'static BReg> {
    BREG.get_or_init(BReg::load).as_ref()
}

pub(crate) fn mkprops(pairs: &[(&str, &str)]) -> J {
    J::Obj(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), J::Str(v.to_string())))
            .collect(),
    )
}

#[cfg(test)]
mod tag_tests {
    /// `ore_*` features target `#minecraft:base_stone_overworld`; if the tag lookup
    /// also matched `gravel`/`dirt`, a later ore would overwrite the gravel and dirt
    /// an earlier one placed (measured: this port's gravel set was a strict subset of
    /// vanilla's, 121 cells missing, while its stone-variant sets were supersets).
    #[test]
    #[cfg(megu_vanilla_data)]
    fn base_stone_overworld_excludes_gravel_and_dirt() {
        let reg = super::breg().expect("registry");
        for name in [
            "stone",
            "granite",
            "diorite",
            "andesite",
            "tuff",
            "deepslate",
        ] {
            let st = reg.state_of(name, None);
            assert!(
                reg.in_tag(st, "minecraft:base_stone_overworld"),
                "{name} should match"
            );
        }
        for name in [
            "gravel",
            "dirt",
            "grass_block",
            "sand",
            "clay",
            "andesite_slab",
        ] {
            let st = reg.state_of(name, None);
            assert!(
                !reg.in_tag(st, "minecraft:base_stone_overworld"),
                "{name} must not match base_stone_overworld"
            );
        }
    }

    /// Support-loss for flowers/saplings re-reads `#minecraft:flowers`, which is a
    /// nested tag (`#small_flowers` + `#tall_flowers` + direct members). The
    /// expansion must reach members two levels deep or flowers never pop.
    #[test]
    #[cfg(megu_vanilla_data)]
    fn nested_flowers_tag_reaches_small_flower_members() {
        let reg = super::breg().expect("registry");
        for name in ["poppy", "dandelion", "torchflower", "sunflower"] {
            let st = reg.state_of(name, None);
            assert!(
                reg.in_tag(st, "minecraft:flowers"),
                "{name} must be in #minecraft:flowers"
            );
        }
        assert!(reg.in_tag(reg.state_of("grass_block", None), "minecraft:dirt"));
        assert!(!reg.in_tag(reg.state_of("stone", None), "minecraft:dirt"));
    }
}

#[cfg(test)]
mod version_asset_tests {
    use super::VersionAssetPaths;
    #[cfg(megu_vanilla_data)]
    use std::path::Path;

    /// With no `MEGU_VERSION_DIR` override the resolver must produce the
    /// same `assets/*_1211.*` paths the hardcoded reads used — the zero
    /// behavior change contract for default 1.21.1 runs.
    #[test]
    #[cfg(megu_vanilla_data)]
    fn default_pack_resolves_the_bundled_1211_paths() {
        // A set override is an operator choice; this test only proves the
        // default branch, so it declines rather than fight the environment.
        if std::env::var_os("MEGU_VERSION_DIR").is_some() {
            return;
        }
        let dir = Path::new("assets");
        let paths = VersionAssetPaths::resolve();
        assert_eq!(paths.blocks, dir.join("blocks_1211.json"));
        assert_eq!(paths.block_states, dir.join("block_states_1211.json"));
        assert_eq!(paths.block_predicates, dir.join("block_predicates_1211.bin"));
        assert_eq!(paths.block_state_facts, dir.join("block_state_facts_1211.bin"));
        assert_eq!(
            paths.mineshaft_support,
            dir.join("mineshaft_support_1211.bin")
        );
        assert_eq!(
            paths.multiface_support,
            dir.join("multiface_support_1211.bin")
        );
        assert_eq!(paths.template_solid, dir.join("template_solid_1211.bin"));
        // The pack the default run reads is the pack the tests ship.
        for path in [
            &paths.blocks,
            &paths.block_states,
            &paths.block_predicates,
            &paths.block_state_facts,
            &paths.mineshaft_support,
            &paths.multiface_support,
            &paths.template_solid,
        ] {
            assert!(path.is_file(), "bundled pack missing {}", path.display());
        }
    }

    /// A custom version dir resolves version-neutral names first, falls back to
    /// the `_1211` spelling inside the same dir, and still names the neutral
    /// file when neither exists so the read error points at what is missing.
    #[test]
    fn version_dir_prefers_neutral_and_falls_back_to_the_tagged_names() {
        let dir = std::env::temp_dir().join(format!("megu-vpack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Neutral + tagged both present: the neutral file is the pack's own.
        std::fs::write(dir.join("blocks.json"), b"[]").unwrap();
        std::fs::write(dir.join("blocks_1211.json"), b"[]").unwrap();
        // Only the tagged file present: fallback picks it up unrenamed.
        std::fs::write(dir.join("block_states_1211.json"), b"{}").unwrap();
        let paths = VersionAssetPaths::from_dir(&dir);
        assert_eq!(paths.blocks, dir.join("blocks.json"));
        assert_eq!(paths.block_states, dir.join("block_states_1211.json"));
        // Neither spelling: the neutral name is what the error should report.
        assert_eq!(paths.block_predicates, dir.join("block_predicates.bin"));
        assert_eq!(paths.template_solid, dir.join("template_solid.bin"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod synthesized_block_registry_tests {
    use super::*;
    use std::fs;

    const BLOCKS: &[u8] = br#"[
      {"name":"air","minStateId":0,"maxStateId":0,"boundingBox":"empty","transparent":true},
      {"name":"stone","minStateId":1,"maxStateId":1,"boundingBox":"block","hardness":1.5,"filterLight":15},
      {"name":"water","minStateId":2,"maxStateId":3,"boundingBox":"empty","transparent":true,"filterLight":1,
       "states":[{"name":"level","type":"int","values":["0","1"]}]},
      {"name":"lava","minStateId":4,"maxStateId":5,"boundingBox":"empty","transparent":true,"filterLight":1,"emitLight":15,
       "states":[{"name":"level","type":"int","values":["0","1"]}]},
      {"name":"torch","minStateId":6,"maxStateId":6,"boundingBox":"empty","transparent":true,"emitLight":14},
      {"name":"oak_slab","minStateId":7,"maxStateId":12,"defaultState":10,"boundingBox":"block","filterLight":0,
       "states":[{"name":"type","type":"enum","values":["top","bottom","double"]},{"name":"waterlogged","type":"bool"}]},
      {"name":"cave_air","minStateId":13,"maxStateId":13,"boundingBox":"empty","transparent":true},
      {"name":"void_air","minStateId":14,"maxStateId":14,"boundingBox":"empty","transparent":true},
      {"name":"kelp","minStateId":15,"maxStateId":15,"boundingBox":"empty","transparent":true,"filterLight":1},
      {"name":"seagrass","minStateId":16,"maxStateId":16,"boundingBox":"empty","transparent":true,"filterLight":1},
      {"name":"bubble_column","minStateId":17,"maxStateId":17,"boundingBox":"empty","transparent":true,"filterLight":1},
      {"name":"glass","minStateId":18,"maxStateId":18,"boundingBox":"block","transparent":true,"filterLight":0}
    ]"#;

    fn local_pack(label: &str, bytes: &[u8]) -> (PathBuf, VersionAssetPaths) {
        let dir = std::env::temp_dir().join(format!("megu-breg-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("blocks_1211.json"), bytes).unwrap();
        let paths = VersionAssetPaths::from_dir(&dir);
        (dir, paths)
    }

    #[test]
    fn blocks_only_derives_survival_facts() {
        let (dir, paths) = local_pack("blocks-only", BLOCKS);
        // Partial files must not leak into synthesized facts.
        fs::write(&paths.block_state_facts, [0; 108]).unwrap();
        let reg = BReg::load_paths(&paths).unwrap();
        assert!(reg.unpaired.is_empty());
        for air in ["air", "cave_air", "void_air"] {
            let st = reg.state_of(air, None);
            assert!(reg.is_air(st) && reg.is_replaceable(st));
            assert!(!reg.blocks_motion(st) && !reg.can_occlude(st) && !reg.is_liquid(st));
            assert_eq!(reg.fluid_name(st), "minecraft:empty");
            assert!(!reg.heightmap_counts(st, HeightmapKind::WorldSurface));
            assert!(reg.is_collision_empty(st));
            assert_eq!(reg.collision_boxes(st).count(), 0);
        }
        for (name, level, fluid, liquid_block) in [
            ("water", "0", "minecraft:water", true),
            ("water", "1", "minecraft:flowing_water", true),
            ("lava", "0", "minecraft:lava", true),
            ("lava", "1", "minecraft:flowing_lava", true),
            ("kelp", "0", "minecraft:water", false),
            ("seagrass", "0", "minecraft:water", false),
            ("bubble_column", "0", "minecraft:water", true),
        ] {
            let st = reg.state_of(name, Some(&mkprops(&[("level", level)])));
            assert_eq!(reg.fluid_name(st), fluid);
            assert_eq!(reg.is_liquid_block(st), liquid_block);
            assert!(reg.is_liquid(st) && reg.is_collision_empty(st));
            assert!(reg.heightmap_counts(st, HeightmapKind::MotionBlocking));
            assert!(!reg.heightmap_counts(st, HeightmapKind::OceanFloor));
            assert_eq!(reg.opacity_of(st), 1);
        }
        let stone = reg.state_of("stone", None);
        assert_eq!(
            reg.collision_boxes(stone).collect::<Vec<_>>(),
            [([0.0; 3], [1.0; 3])]
        );
        assert_eq!(reg.hardness_of(stone), 1.5);
        assert!(reg.is_collision_shape_full_block(stone));
        assert!(reg.blocks_motion(stone) && reg.can_occlude(stone) && reg.is_solid_render(stone));
        assert_eq!(reg.opacity_of(stone), 15);
        assert_eq!(reg.is_template_solid(stone), Some(true));
        assert!(reg.mineshaft_chain_anchor(stone));
        for face in 0..6 {
            assert!(reg.is_face_sturdy(stone, face, Support::Full));
            assert!(reg.is_face_sturdy(stone, face, Support::Center));
            assert!(reg.is_face_sturdy(stone, face, Support::Rigid));
            assert!(reg.supports_multiface(stone, face));
        }
        for kind in [
            HeightmapKind::WorldSurface,
            HeightmapKind::OceanFloor,
            HeightmapKind::MotionBlocking,
            HeightmapKind::MotionBlockingNoLeaves,
        ] {
            assert!(reg.heightmap_counts(stone, kind));
        }
        let torch = reg.state_of("torch", None);
        assert!(reg.is_collision_empty(torch));
        assert!(!reg.blocks_motion(torch) && !reg.can_occlude(torch));
        assert_eq!((reg.luminance_of(torch), reg.opacity_of(torch)), (14, 0));
        assert_eq!((reg.luminance_of(4), reg.opacity_of(4)), (15, 1));
        let glass = reg.state_of("glass", None);
        assert!(reg.is_collision_shape_full_block(glass) && !reg.is_solid_render(glass));
        assert_eq!(reg.opacity_of(glass), 0);

        let slab = reg.state_of("oak_slab", None);
        assert_eq!(
            reg.collision_boxes(slab).collect::<Vec<_>>(),
            [([0.0; 3], [1.0, 0.5, 1.0])]
        );
        assert!(reg.blocks_motion(slab) && !reg.is_solid_render(slab));
        assert!(!reg.is_liquid(slab));
        assert_eq!(reg.is_template_solid(slab), Some(false));
        assert!(reg.supports_multiface(slab, 1)); // bottom face, viewed from below
        assert!(!reg.supports_multiface(slab, 0));
        let wet_top = reg.state_of(
            "oak_slab",
            Some(&mkprops(&[("type", "top"), ("waterlogged", "true")])),
        );
        assert_eq!(
            reg.collision_boxes(wet_top).collect::<Vec<_>>(),
            [([0.0, 0.5, 0.0], [1.0; 3])]
        );
        assert_eq!(reg.fluid_name(wet_top), "minecraft:water");
        assert!(!reg.is_liquid_block(wet_top));
        assert_eq!(reg.opacity_of(wet_top), 1);
        let double = reg.with_prop(slab, "type", "double");
        assert!(reg.is_collision_shape_full_block(double) && reg.is_solid_render(double));
        assert_eq!(reg.opacity_of(double), 15);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn complete_oracle_tables_take_precedence() {
        let (dir, paths) = local_pack("full-oracle", BLOCKS);
        let state_count = 19;
        let json = format!(
            r#"{{"shapes":[],"flags":{:?},"collision_shapes":{:?}}}"#,
            vec![0; state_count],
            vec![Vec::<u8>::new(); state_count]
        );
        fs::write(&paths.block_states, json).unwrap();
        let mut facts = vec![0; state_count * STATE_FACT_BYTES];
        facts[6 * STATE_FACT_BYTES + 1] = 0x72;
        fs::write(&paths.block_state_facts, &facts).unwrap();
        for path in [
            &paths.block_predicates,
            &paths.mineshaft_support,
            &paths.multiface_support,
            &paths.template_solid,
        ] {
            fs::write(path, vec![0; state_count]).unwrap();
        }
        let reg = BReg::load_paths(&paths).unwrap();
        assert_eq!(&*reg.facts, facts);
        assert_eq!((reg.luminance_of(6), reg.opacity_of(6)), (7, 2));
        fs::write(&paths.block_states, b"invalid JSON").unwrap();
        assert!(
            BReg::load_paths(&paths).is_none(),
            "do not mask broken full packs"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn pinned_mit_metadata_decodes_every_state_without_oracle_tables() {
        let bytes = fs::read(VersionAssetPaths::default_pack().blocks).unwrap();
        let (dir, paths) = local_pack("pinned-mit", &bytes);
        let reg = BReg::load_paths(&paths).unwrap();
        assert!(reg.state_count() >= 26_684);
        assert!(reg.unpaired.is_empty());
        for name in [
            "short_grass",
            "redstone_wire",
            "poppy",
            "rail",
            "oak_sign",
            "oak_button",
            "oak_pressure_plate",
            "vine",
        ] {
            assert!(
                !reg.is_air(reg.state_of(name, None)),
                "{name} resolved to air"
            );
            assert!(
                reg.is_collision_empty(reg.state_of(name, None)),
                "{name} obstructed movement"
            );
        }
        for (name, y) in [
            ("oak_stairs", 1.0),
            ("oak_fence", 1.5),
            ("cobblestone_wall", 1.5),
            ("red_bed", 0.5625),
            ("chest", 0.875),
            ("white_carpet", 0.0625),
        ] {
            let st = reg.state_of(name, None);
            assert!(
                reg.collision_boxes(st).any(|(_, hi)| hi[1] == y),
                "{name} lacks its support height"
            );
            assert!(!reg.is_collision_shape_full_block(st));
        }
        let ladder = reg.state_of("ladder", None);
        assert!(!reg.blocks_motion(ladder) && !reg.is_collision_empty(ladder));
        let door = reg.state_of("oak_door", None);
        let opened = reg.with_prop(door, "open", "true");
        assert_ne!(
            reg.collision_boxes(door).next(),
            reg.collision_boxes(opened).next()
        );
        let gate = reg.state_of("oak_fence_gate", None);
        assert!(!reg.is_collision_empty(gate));
        assert!(reg.is_collision_empty(reg.with_prop(gate, "open", "true")));
        let furnace = reg.state_of("furnace", None);
        assert_eq!(reg.luminance_of(furnace), 0);
        assert_eq!(reg.luminance_of(reg.with_prop(furnace, "lit", "true")), 13);
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod dynamic_extension_tests {
    use super::*;

    /// The neutral extension registers what a loader declares — light, opacity, hardness,
    /// solidity, replaceability, properties — under a canonical name, and answers it back
    /// without reading the name's spelling. Items get a bounded, deterministic id above the
    /// adapter range and resolve both ways.
    #[test]
    fn dynamic_blocks_and_items_register_neutral_metadata() {
        let _guard = crate::world_guard();
        clear_dynamic_mod_registry();
        assert!(dynamic_mod_block("mymod:glow_crystal").is_none());
        assert!(dynamic_mod_item(0).is_none());
        assert_eq!(dynamic_mod_item_id("mymod:wrench"), None);

        let mut crystal = DynamicBlockEntry::new("mymod:glow_crystal", 3.5);
        crystal.light_emission = 14;
        crystal.light_opacity = 12;
        crystal.properties.push((
            "lit".to_string(),
            vec!["false".to_string(), "true".to_string()],
        ));
        let mut scaffolding = DynamicBlockEntry::new("mymod:scaffolding", 0.4);
        scaffolding.is_replaceable = true;
        scaffolding.light_opacity = 0;
        assert_eq!(
            register_dynamic_mod_blocks(&[crystal.clone(), scaffolding]),
            2
        );
        // One name, one block: a second declaration counts once and keeps the first metadata.
        crystal.light_emission = 0;
        assert_eq!(register_dynamic_mod_blocks(&[crystal]), 1);

        let stored = dynamic_mod_block("mymod:glow_crystal").expect("registered");
        assert_eq!(stored.name, "mymod:glow_crystal");
        assert_eq!(stored.light_emission, 14, "the first declaration wins");
        assert_eq!(stored.light_opacity, 12);
        assert_eq!(stored.hardness, 3.5);
        assert!(stored.is_solid);
        assert!(!stored.is_replaceable);
        assert_eq!(stored.properties.len(), 1);
        let scaffolding = dynamic_mod_block("mymod:scaffolding").expect("registered");
        assert!(scaffolding.is_replaceable);
        assert_eq!(scaffolding.light_opacity, 0);

        // A name that is not a resource location is refused, not repaired.
        assert_eq!(
            register_dynamic_mod_blocks(&[DynamicBlockEntry::new("Not A Block", 1.0)]),
            0
        );
        assert!(dynamic_mod_block("Not A Block").is_none());

        // The state count is the Cartesian product of the property table, refused past the cap.
        assert_eq!(dynamic_block_state_count(&[]), 1);
        assert_eq!(dynamic_block_state_count(&stored.properties), 2);
        assert_eq!(
            dynamic_block_state_count(&[("a".to_string(), Vec::new())]),
            0
        );
        assert_eq!(
            dynamic_block_state_count(&[(
                "a".to_string(),
                vec!["0".to_string(); MAX_DYNAMIC_BLOCK_STATES + 1]
            )]),
            0
        );

        // Items: the first id sits above the adapter range, so no adapter table can collide
        // with it, and the same name answers the same id.
        let wrench = register_dynamic_mod_item("mymod:wrench", 1).expect("registered");
        assert_eq!(wrench, DYNAMIC_MOD_ITEM_BASE + ADAPTER_ITEM_RANGE);
        assert!(wrench > DYNAMIC_MOD_ITEM_BASE);
        assert_eq!(register_dynamic_mod_item("mymod:wrench", 1), Some(wrench));
        assert_eq!(dynamic_mod_item_id("mymod:wrench"), Some(wrench));
        let (name, max_stack) = dynamic_mod_item(wrench).expect("registered");
        assert_eq!(name, "mymod:wrench");
        assert_eq!(max_stack, 1);
        // A stack size outside `1..=64` is clamped rather than stored.
        let torch = register_dynamic_mod_item("mymod:torch", 0).expect("registered");
        assert_eq!(
            dynamic_mod_item(torch),
            Some(("mymod:torch".to_string(), 1))
        );
        assert_eq!(register_dynamic_mod_item("not a name", 1), None);
        assert!(dynamic_mod_item(wrench + 1_000).is_none());

        // A block drops the item its loader registered under the same name, and a block with
        // no such item drops nothing.
        assert_eq!(dynamic_mod_item_facts("mymod:wrench"), Some((wrench, 1)));
        assert_eq!(dynamic_mod_item_facts("mymod:glow_crystal"), None);

        clear_dynamic_mod_registry();
        assert!(dynamic_mod_block("mymod:glow_crystal").is_none());
        assert!(dynamic_mod_item(wrench).is_none());
    }
}

/// Modded-state fact derivation and the `extend_create_registry` wiring.
/// The fixtures build a bare `BReg` — no vanilla pack needed — so they run
/// on strict-public exports too.
#[cfg(all(test, feature = "mod-suite"))]
mod mod_block_facts_tests {
    use super::*;
    use create_adapter::blockstates::{core_create_registry, DEFAULT_BASE_STATE_ID};

    fn empty_breg() -> BReg {
        BReg {
            bname: vec![],
            idx: HashMap::new(),
            props: vec![],
            dfl: vec![],
            st_lo: vec![],
            n_states: vec![],
            st_blk: vec![],
            st_fl: vec![],
            hardness: vec![],
            htools: vec![],
            tool_kind: vec![],
            unpaired: vec![],
            drops: vec![],
            drops_second: vec![],
            stack_size: vec![],
            behaviour: vec![],
            shapes: vec![],
            col_range: vec![],
            col_idx: vec![],
            pred: vec![],
            light: vec![],
            postproc: vec![],
            faces: vec![],
            tag_names: vec![],
            tag_bits: vec![],
            tag_ids: HashMap::default(),
            tag_cache: std::array::from_fn(|_| AtomicU64::new(0)),
            tag_order: HashMap::new(),
            facts: Vec::new().into_boxed_slice(),
            predicates: Vec::new().into_boxed_slice(),
            mineshaft: Vec::new().into_boxed_slice(),
            multiface: Vec::new().into_boxed_slice(),
            template_solid: Vec::new().into_boxed_slice(),
            create_reg: None,
        }
    }

    fn create_breg() -> BReg {
        let mut reg = empty_breg();
        reg.extend_create_registry(core_create_registry(DEFAULT_BASE_STATE_ID));
        reg
    }

    /// A full solid modded block must behave like vanilla stone: opaque,
    /// full collision, all faces sturdy, redstone-conducting, and inside
    /// the pickaxe-mineable tag — the facts the zero-filled extension lost.
    #[test]
    fn modded_solid_blocks_are_opaque_sturdy_conducting_and_tagged() {
        let reg = create_breg();
        let casing = reg.state_of("create:andesite_casing", None);
        assert!(casing >= DEFAULT_BASE_STATE_ID);
        assert_eq!(reg.opacity_of(casing), 15);
        assert_eq!(reg.luminance_of(casing), 0);
        assert!(reg.is_solid_render(casing));
        assert!(reg.is_collision_shape_full_block(casing));
        assert!(!reg.is_collision_empty(casing));
        assert!(reg.is_redstone_conductor(casing));
        for direction in 0..6 {
            assert!(
                reg.is_face_sturdy(casing, direction, Support::Full),
                "direction {direction} must support torches and rails"
            );
            assert!(reg.side_solid(casing, direction as usize));
        }
        // A full-collision modded block collides with the unit cube and is
        // a template solid for structure connection updates.
        assert_eq!(reg.collision_boxes(casing).count(), 1);
        assert_eq!(reg.is_template_solid(casing), Some(true));
        assert!(reg.in_tag(casing, "minecraft:mineable/pickaxe"));
        assert!(reg.tag_members("minecraft:mineable/pickaxe").contains(
            &(reg.st_blk[casing as usize])
        ));
    }

    /// Decorative mounts keep their partial facts: a shaft passes light,
    /// has no full-cube collision, conducts no redstone and is not sturdy,
    /// while `waterlogged=true` still reports its carried fluid.
    #[test]
    fn modded_thin_blocks_stay_transparent_and_non_conducting() {
        let reg = create_breg();
        let shaft = reg.state_of(
            "create:shaft",
            Some(&mkprops(&[("axis", "y"), ("waterlogged", "false")])),
        );
        assert!(shaft >= DEFAULT_BASE_STATE_ID);
        assert_eq!(reg.opacity_of(shaft), 0);
        assert_eq!(reg.luminance_of(shaft), 0);
        assert!(reg.blocks_motion(shaft));
        assert!(!reg.is_solid_render(shaft));
        assert!(!reg.is_collision_shape_full_block(shaft));
        assert!(!reg.is_redstone_conductor(shaft));
        for direction in 0..6 {
            assert!(!reg.is_face_sturdy(shaft, direction, Support::Full));
        }
        assert!(!reg.in_tag(shaft, "minecraft:mineable/pickaxe"));
        let wet = reg.state_of(
            "create:hand_crank",
            Some(&mkprops(&[("facing", "north"), ("waterlogged", "true")])),
        );
        assert!(reg.is_liquid(wet));
        assert_eq!(reg.fluid_name(wet), "minecraft:water");
    }

    /// The derivation itself: emitters light up, `lit`/`blaze`/`powered`
    /// adjust per state, glass passes light, partial shapes dim it by one,
    /// and `*_block`/unknown names stay opaque cubes.
    #[test]
    fn derived_facts_follow_names_and_properties_generically() {
        let lum = |id: &str, props: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = props
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            derive_mod_block_facts(id, &pairs).light >> 4
        };
        assert_eq!(lum("mymod:rose_lamp", &[]), 14);
        assert_eq!(lum("mymod:glow_panel", &[]), 14);
        assert_eq!(lum("mymod:magma_vent", &[]), 7);
        assert_eq!(lum("mymod:arc_lamp", &[("lit", "true")]), 15);
        assert_eq!(lum("mymod:arc_lamp", &[("lit", "false")]), 0);
        assert_eq!(lum("mymod:arc_lamp", &[("powered", "true")]), 15);
        assert_eq!(lum("mymod:plain_box", &[("powered", "true")]), 0);
        assert_eq!(
            lum("create:blaze_burner", &[("blaze", "kindled")]),
            11
        );
        assert_eq!(lum("create:blaze_burner", &[("blaze", "none")]), 0);

        let glass = derive_mod_block_facts("mymod:framed_glass", &[]);
        assert_eq!(glass.light & 0x0f, 0);
        assert_ne!(glass.pred & FACT_BLOCKS_MOTION, 0);
        assert_eq!(glass.pred & FACT_COLLISION_FULL, 0);
        assert_eq!(glass.faces, 0);
        let slab = derive_mod_block_facts("mymod:basalt_slab", &[]);
        assert_eq!(slab.light & 0x0f, 1);
        assert_eq!(slab.pred & FACT_COLLISION_FULL, 0);
        let ore = derive_mod_block_facts("mymod:tin_ore", &[]);
        assert_eq!(ore.light & 0x0f, 15);
        assert_eq!(ore.faces, 0x003f_3f3f);
        // `grass_block`/`bedrock` must not read as `grass`/`bed`.
        assert_eq!(mod_block_shape("grass_block"), ModShape::Full);
        assert_eq!(mod_block_shape("bedrock"), ModShape::Full);
        assert_eq!(mod_block_shape("fluid_pipe"), ModShape::Thin);
        assert_eq!(mod_block_shape("water_wheel"), ModShape::Thin);
        assert_eq!(mod_block_shape("sump_water"), ModShape::Liquid);
        // Tag vocabulary is namespace-agnostic.
        assert!(mod_block_tag_names("oak_planks", ModShape::Full)
            .contains(&"minecraft:planks"));
        assert!(mod_block_tag_names("palm_leaves", ModShape::Leaves)
            .contains(&"minecraft:leaves"));
        assert!(mod_block_tag_names("tin_ore", ModShape::Full)
            .contains(&"minecraft:mineable/pickaxe"));
        assert!(mod_block_tag_names("cedar_planks", ModShape::Full)
            .contains(&"minecraft:mineable/axe"));
    }

    /// A vanilla table already reaching the mod range refuses the
    /// extension rather than corrupting both id spaces.
    #[test]
    fn extension_refuses_a_colliding_base() {
        let mut reg = empty_breg();
        reg.st_blk = vec![0; DEFAULT_BASE_STATE_ID as usize + 1];
        reg.extend_create_registry(core_create_registry(DEFAULT_BASE_STATE_ID));
        assert!(reg.create_reg.is_none());
        assert_eq!(reg.state_count(), DEFAULT_BASE_STATE_ID as usize + 1);
    }

    /// The loader-agnostic extension writes its columns from what the entry declared, with no
    /// name heuristic anywhere: light emission, light opacity, hardness, solidity and the drop
    /// come from the entry (and from the item registered under the same name), so a block whose
    /// name ends in `_leaves` carries exactly what its loader stated and nothing else.
    #[test]
    fn dynamic_blocks_reach_the_dense_columns_without_name_heuristics() {
        let _guard = crate::world_guard();
        super::clear_dynamic_mod_registry();
        let mut crystal = DynamicBlockEntry::new("mymod:glow_crystal", 3.5);
        crystal.light_emission = 14;
        crystal.light_opacity = 12;
        crystal.properties.push((
            "lit".to_string(),
            vec!["false".to_string(), "true".to_string()],
        ));
        let mut pale_leaves = DynamicBlockEntry::new("mymod:pale_leaves", 0.2);
        pale_leaves.is_solid = false;
        pale_leaves.light_opacity = 1;
        assert_eq!(
            super::register_dynamic_mod_blocks(&[crystal, pale_leaves]),
            2
        );
        let item = super::register_dynamic_mod_item("mymod:glow_crystal", 1);
        assert_eq!(item, Some(DYNAMIC_MOD_ITEM_BASE + ADAPTER_ITEM_RANGE));
        let item = item.unwrap_or(-1);
        let mut reg = empty_breg();
        assert_eq!(reg.extend_dynamic_blocks(), 2);

        // Sorted by canonical name, so the ids depend on the set and not the load order.
        let base = reg.state_count() - 3;
        assert_eq!(reg.state_of("mymod:glow_crystal", None), base as u32);
        assert_eq!(
            reg.state_of("mymod:glow_crystal", Some(&mkprops(&[("lit", "true")]))),
            base as u32 + 1,
            "the property table expands in `state_of` order"
        );
        let crystal_state = reg.state_of("mymod:glow_crystal", None);
        assert_eq!(reg.luminance_of(crystal_state), 14);
        assert_eq!(reg.opacity_of(crystal_state), 12);
        assert_eq!(reg.hardness_of(crystal_state), 3.5);
        assert!(reg.is_solid(crystal_state));
        assert!(reg.is_collision_shape_full_block(crystal_state));
        assert!(reg.is_face_sturdy(crystal_state, 0, Support::Full));
        assert_eq!(reg.block_drop(crystal_state), Some(item));
        assert_eq!(reg.stack_size_of(crystal_state), 1);

        // `*_leaves` is not a rule on this path: the entry is the only input.
        let leaves_state = reg.state_of("mymod:pale_leaves", None);
        assert_eq!(reg.opacity_of(leaves_state), 1);
        assert!(!reg.is_solid(leaves_state));
        assert!(!reg.is_collision_shape_full_block(leaves_state));
        assert_eq!(reg.block_drop(leaves_state), None);

        // A second extension appends nothing: these names already own states.
        assert_eq!(reg.extend_dynamic_blocks(), 0);
        super::clear_dynamic_mod_registry();
    }
}
