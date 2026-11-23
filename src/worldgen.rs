// worldgen.rs — datapack-driven vanilla worldgen slice for megu.
// Loads vanilla worldgen JSONs (noise_settings / density_function / noise)
// from datapacks/<pack>/data/minecraft/worldgen/ and evaluates the vanilla
// density-function pipeline: per-chunk cell grid + trilinear interp.
// Algorithm referenced from vanilla/Pumpkin; implementation is independent.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::block_registry::{breg, bs_parts, BReg};
use crate::json::{jbool, jparse, J};

use crate::generation_math::*;
use crate::trace_env::{env_flag, env_str};

// ---------- worldgen datapack parse diagnostics ----------

/// One worldgen datapack entry that failed to parse: the datapack directory
/// family (`configured_feature`, `placed_feature`, `configured_carver` or
/// `surface_rule`), the resource id, and why the loader refused it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldgenParseError {
    pub kind: &'static str,
    pub id: String,
    pub reason: String,
}

impl std::fmt::Display for WorldgenParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.kind, self.id, self.reason)
    }
}

impl std::error::Error for WorldgenParseError {}

/// The worldgen datapack parse failures one load saw, in the order they
/// happened. A clean report means nothing was dropped.
#[derive(Clone, Debug, Default)]
pub struct WorldgenParseReport {
    pub errors: Vec<WorldgenParseError>,
}

impl WorldgenParseReport {
    /// No recorded failures.
    #[cfg_attr(not(test), allow(dead_code))]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }

    /// The recorded failures of one datapack family, in load order.
    #[cfg_attr(not(test), allow(dead_code))]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub fn of_kind<'a>(
        &'a self,
        kind: &'a str,
    ) -> impl Iterator<Item = &'a WorldgenParseError> + 'a {
        self.errors.iter().filter(move |e| e.kind == kind)
    }

    /// Record one failure: a one-line warning on stderr, kept for queries.
    pub(crate) fn fail(
        &mut self,
        kind: &'static str,
        id: impl Into<String>,
        reason: impl Into<String>,
    ) -> WorldgenParseError {
        let e = WorldgenParseError {
            kind,
            id: id.into(),
            reason: reason.into(),
        };
        eprintln!("worldgen parse error: {e}");
        self.errors.push(e.clone());
        e
    }
}

static WG_PARSE_REPORT: Mutex<WorldgenParseReport> =
    Mutex::new(WorldgenParseReport { errors: Vec::new() });

/// The process-wide worldgen parse diagnostics: every failure any registry or
/// generator load has recorded, in the order it happened.
#[cfg_attr(not(test), allow(dead_code))]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub fn last_worldgen_parse_report() -> WorldgenParseReport {
    WG_PARSE_REPORT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Merge one loader's findings into the process-wide report.
pub(crate) fn publish_worldgen_parse_report(rep: &WorldgenParseReport) {
    if rep.errors.is_empty() {
        return;
    }
    WG_PARSE_REPORT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .errors
        .extend(rep.errors.iter().cloned());
}

/// Record one failure straight into the process-wide report, for loaders that
/// do not carry a local report.
pub(crate) fn record_worldgen_parse_error(
    kind: &'static str,
    id: impl Into<String>,
    reason: impl Into<String>,
) -> WorldgenParseError {
    let mut rep = WorldgenParseReport::default();
    let e = rep.fail(kind, id, reason);
    publish_worldgen_parse_report(&rep);
    e
}

/// `MEGU_STRICT_WORLDGEN=1` opts a load into fail-closed parsing: the first
/// failure in any of the four worldgen datapack families aborts the load
/// (`Err` / refusal) instead of continuing with the entry dropped. Read per
/// call, never cached, so a test can toggle it between loads.
pub(crate) fn strict_worldgen_parse() -> bool {
    std::env::var("MEGU_STRICT_WORLDGEN").as_deref() == Ok("1")
}

// ---------- density functions ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NoisePreset {
    Overworld,
    Nether,
    End,
}
impl NoisePreset {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "overworld" => Some(Self::Overworld),
            "nether" => Some(Self::Nether),
            "end" => Some(Self::End),
            _ => None,
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Overworld => "overworld",
            Self::Nether => "nether",
            Self::End => "end",
        }
    }
    /// The dimension this preset's noise settings belong to. One preset, one dimension
    /// here: the overworld settings also cover its caves (`overworld_caves` is a separate
    /// `dimension_type` this port does not load), and the Nether and the End each have
    /// exactly one.
    pub(crate) fn dim(self) -> DimId {
        match self {
            Self::Overworld => DimId::Overworld,
            Self::Nether => DimId::Nether,
            Self::End => DimId::End,
        }
    }
}

/// Which dimension a `Gen` answers for: the key of the per-dimension registry.
///
/// A dimension is not a preset spelled differently. The **preset** selects the noise
/// settings — terrain, density, aquifers, carvers, surface rules, the biome quart sweep — and
/// the **dimension** selects the `dimension_type` — the world window and whether there is a
/// sky light layer. They agree one-to-one here only because this port loads one dimension per
/// preset. The registry exists so a streaming caller can *name* the generator it wants
/// instead of reading whichever one `init` installed last.
/// A dimension registry slot. Slots 0..2 are pinned to the vanilla dimensions — the
/// binary saves, the protocol wire IDs and the enum-era call sites all depend on them —
/// and slots >= 3 are datapack-registered custom dimensions in registration order.
///
/// The constants keep `DimId::Overworld` / `DimId::Nether` / `DimId::End` valid in
/// expressions AND match patterns, so the enum conversion touches no call-site syntax.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct DimId(u32);

impl DimId {
    /// Vanilla slot 0.
    #[allow(non_upper_case_globals)]
    pub(crate) const Overworld: DimId = DimId(0);
    /// Vanilla slot 1.
    #[allow(non_upper_case_globals)]
    pub(crate) const Nether: DimId = DimId(1);
    /// Vanilla slot 2.
    #[allow(non_upper_case_globals)]
    pub(crate) const End: DimId = DimId(2);

    /// The pinned vanilla slots, in order.
    pub(crate) const ALL: [DimId; 3] = [DimId::Overworld, DimId::Nether, DimId::End];
    /// Same set, under the name a reader reaches for next to `all_registered`.
    #[allow(dead_code)]
    pub(crate) const VANILLA: [DimId; 3] = Self::ALL;

    /// A handle for a raw slot, validated or not. `from_index` is the checked entry.
    #[allow(dead_code)]
    pub(crate) const fn from_raw(raw: u32) -> DimId {
        DimId(raw)
    }

    /// The raw slot value: registry index and the legacy one-byte save encoding.
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }

    /// Slots 0..2 are the pinned vanilla dimensions; higher slots are datapack ones.
    pub(crate) const fn is_vanilla(self) -> bool {
        self.0 < Self::ALL.len() as u32
    }

    /// The slot this dimension owns in the registry arrays: `ALL[i].index() == i`.
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }

    /// Slot `index`'s dimension when it is registered now. Vanilla slots always
    /// resolve; a custom slot resolves only while a datapack holds it — which is
    /// exactly why the binary saves name custom dimensions by key instead.
    pub(crate) fn from_index(index: usize) -> Option<Self> {
        (index < registered_dimension_count()).then_some(DimId(index as u32))
    }

    /// Every registered dimension: the three vanilla slots, then custom dimensions in
    /// registration order.
    pub(crate) fn all_registered() -> Vec<DimId> {
        (0..registered_dimension_count())
            .map(|index| DimId(index as u32))
            .collect()
    }

    /// The noise preset a vanilla dimension's generator is built from. A custom
    /// `Noise` spec keeps its own preset; other custom generators report `Overworld`
    /// because there is no noise preset to name.
    pub(crate) fn preset(self) -> NoisePreset {
        match self.0 {
            0 => NoisePreset::Overworld,
            1 => NoisePreset::Nether,
            2 => NoisePreset::End,
            _ => match dimension_spec(self).map(|spec| spec.generator) {
                Some(CustomGeneratorKind::Noise { preset }) => preset,
                _ => NoisePreset::Overworld,
            },
        }
    }

    /// The `dimension_type` file name under `data/minecraft/dimension_type/`, for the
    /// vanilla dimensions. Two of the three are not the preset name: `the_nether` and
    /// `the_end`. `None` for a custom dimension, whose type name is its `type_key`.
    pub(crate) fn dimension_file(self) -> Option<&'static str> {
        match self.0 {
            0 => Some("overworld"),
            1 => Some("the_nether"),
            2 => Some("the_end"),
            _ => None,
        }
    }

    /// The canonical `"namespace:path"` id a datapack (or the wire) names this
    /// dimension by. The empty string answers for an unregistered slot, which the
    /// wire paths must never emit.
    pub(crate) fn dimension_id(self) -> &'static str {
        match self.0 {
            0 => "minecraft:overworld",
            1 => "minecraft:the_nether",
            2 => "minecraft:the_end",
            _ => dimension_key(self).unwrap_or(""),
        }
    }

    /// Resolve a canonical `"namespace:path"` dimension id. A bare `path` names
    /// `minecraft:path`, matching vanilla ResourceLocation defaults.
    pub(crate) fn parse_dimension_id(name: &str) -> Option<Self> {
        match name {
            "minecraft:overworld" | "overworld" => return Some(Self::Overworld),
            "minecraft:the_nether" | "the_nether" => return Some(Self::Nether),
            "minecraft:the_end" | "the_end" => return Some(Self::End),
            _ => {}
        }
        let canonical: std::borrow::Cow<'_, str> = if name.contains(':') {
            name.into()
        } else {
            format!("minecraft:{name}").into()
        };
        DIMENSION_REGISTRY
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .by_key
            .get(canonical.as_ref())
            .map(|&raw| DimId(raw))
    }

    /// `DimensionType.hasSkyLight()`: only the Overworld among the vanilla three, plus
    /// whatever a custom `dimension_type` declares.
    pub(crate) fn has_skylight(self) -> bool {
        match self.0 {
            0 => true,
            1 | 2 => false,
            _ => dimension_spec(self).is_some_and(|spec| spec.has_skylight),
        }
    }

    /// The `dimension_type.min_y`: -64 for the Overworld, 0 for the Nether and the End,
    /// the declared value for a custom dimension.
    pub(crate) fn min_y(self) -> i32 {
        match self.0 {
            0 => -64,
            1 | 2 => 0,
            _ => dimension_spec(self).map_or(0, |spec| spec.min_y),
        }
    }

    /// The `dimension_type.height`: 384 for the Overworld, 256 for the Nether and the
    /// End, the declared value for a custom dimension.
    pub(crate) fn height(self) -> i32 {
        match self.0 {
            0 => 384,
            1 | 2 => 256,
            _ => dimension_spec(self).map_or(256, |spec| spec.height),
        }
    }

    /// `dimension_type.coordinate_scale`: 1 for the Overworld and the End, 8 for the
    /// Nether, the declared value for a custom dimension.
    pub(crate) fn coordinate_scale(self) -> f64 {
        match self.0 {
            0 | 2 => 1.0,
            1 => 8.0,
            _ => dimension_spec(self).map_or(1.0, |spec| spec.coordinate_scale),
        }
    }

    /// `height / 16`: 24 block sections for the Overworld, 16 for the Nether and the End.
    pub(crate) fn sections(self) -> i32 {
        self.height() / 16
    }

    /// `LevelLightEngine.getLightSectionCount() = getSectionsCount() + 2`: 26 for the
    /// Overworld, 18 for the Nether and the End.
    pub(crate) fn light_sections(self) -> usize {
        (self.sections() + 2) as usize
    }

    /// `getMinLightSection() = min_y - 16`: -80 for the Overworld, -16 for the Nether and
    /// the End. Light section `i` covers `[light_min_y + 16*i, light_min_y + 16*i + 16)`.
    pub(crate) fn light_min_y(self) -> i32 {
        self.min_y() - 16
    }

    /// The all-ones mask over this dimension's light sections, saturated into one i64:
    /// `0x3ff_ffff` for the Overworld (26 bits), `0x3_ffff` for the Nether and the End
    /// (18 bits). Dimensions with more than 63 light sections need the word list in
    /// [`DimId::light_mask_longs`]; this saturates to `i64::MAX` rather than shifting
    /// past the word.
    pub(crate) fn light_mask(self) -> i64 {
        (1i64 << self.light_sections().min(63)) - 1
    }

    /// The all-ones mask as the little-endian u64 word list the protocol bitset
    /// carries: one word for every dimension the runtime supports today (up to 63
    /// light sections), more for a taller custom `dimension_type`.
    #[allow(dead_code)]
    pub(crate) fn light_mask_longs(self) -> Vec<u64> {
        let light = self.light_sections();
        let mut words = vec![u64::MAX; light / 64];
        let rest = light % 64;
        if rest != 0 {
            words.push((1u64 << rest) - 1);
        }
        words
    }
}

impl From<NoisePreset> for DimId {
    fn from(preset: NoisePreset) -> Self {
        preset.dim()
    }
}

impl From<DimId> for NoisePreset {
    fn from(dimension: DimId) -> Self {
        dimension.preset()
    }
}

// ---------- custom dimension registry ----------

/// The `generator.type` of a datapack `dimension/` file, reduced to what this engine
/// can generate.
#[derive(Clone, Debug)]
pub(crate) enum CustomGeneratorKind {
    /// `minecraft:noise` over one of the three vanilla noise settings, windowed by the
    /// custom `dimension_type` (the generator's own noise window stays the preset's;
    /// cells outside it answer air, exactly like the Nether's 0..255 world over its
    /// 0..127 generator).
    Noise { preset: NoisePreset },
    /// `minecraft:flat`: `(block state id, layer height)` stacked upward from
    /// `min_y`, every cell above the last layer air, every biome quart `biome_id`.
    Flat { layers: Vec<(u16, u32)>, biome_id: i64 },
    /// `minecraft:debug`, an unknown generator type, or a noise/flat settings object
    /// this engine cannot load: every cell air, every biome quart `biome_id`.
    Void { biome_id: i64 },
}

/// A custom dimension's `dimension_type` plus its generator choice — everything the
/// runtime needs that vanilla's three hard-coded answers used to provide inline.
#[derive(Clone, Debug)]
pub(crate) struct DimensionSpec {
    /// The registry slot this dimension owns (>= 3).
    pub id: DimId,
    /// Canonical `"namespace:path"` of the `dimension/` file.
    pub key: &'static str,
    /// Canonical `"namespace:path"` of the `dimension_type` it names.
    pub type_key: &'static str,
    pub min_y: i32,
    pub height: i32,
    pub logical_height: i32,
    pub has_skylight: bool,
    pub has_ceiling: bool,
    pub ultrawarm: bool,
    pub natural: bool,
    pub coordinate_scale: f64,
    pub ambient_light: f32,
    pub fixed_time: Option<i64>,
    pub piglin_safe: bool,
    pub has_raids: bool,
    pub bed_works: bool,
    pub respawn_anchor_works: bool,
    pub effects: String,
    pub infiniburn: String,
    pub monster_spawn_light_level: i32,
    pub monster_spawn_block_light_limit: i32,
    pub generator: CustomGeneratorKind,
}

/// The registered custom dimensions. `specs[i]` owns slot `3 + i`; `by_key` maps a
/// canonical name to its raw slot. Registration is append-only so a slot never
/// changes hands inside a process.
#[derive(Default)]
struct DimensionRegistry {
    specs: Vec<DimensionSpec>,
    by_key: std::collections::BTreeMap<String, u32>,
}

static DIMENSION_REGISTRY: std::sync::RwLock<DimensionRegistry> =
    std::sync::RwLock::new(DimensionRegistry {
        specs: Vec::new(),
        by_key: std::collections::BTreeMap::new(),
    });

/// The legacy one-byte dimension encoding plus the java-compat u8 slot bound the
/// registry to 255 slots: indices 0..=254 exist, 255 stays unresolvable forever.
const MAX_DIMENSIONS: usize = 255;

/// Total registered slots: the pinned vanilla three plus every custom dimension.
pub(crate) fn registered_dimension_count() -> usize {
    DimId::ALL.len() + DIMENSION_REGISTRY.read().unwrap_or_else(|e| e.into_inner()).specs.len()
}

/// The spec a custom dimension registered under, cloned. Vanilla dimensions and
/// unregistered slots answer `None` — their facts live in `GenFacts`.
pub(crate) fn dimension_spec(dimension: DimId) -> Option<DimensionSpec> {
    let index = dimension.index().checked_sub(DimId::ALL.len())?;
    DIMENSION_REGISTRY.read().unwrap_or_else(|e| e.into_inner()).specs.get(index).cloned()
}

/// Every custom dimension, in registration (and therefore slot) order.
pub(crate) fn custom_dimension_specs() -> Vec<DimensionSpec> {
    DIMENSION_REGISTRY.read().unwrap_or_else(|e| e.into_inner()).specs.clone()
}

/// A custom dimension's canonical key without cloning its spec.
fn dimension_key(dimension: DimId) -> Option<&'static str> {
    let index = dimension.index().checked_sub(DimId::ALL.len())?;
    DIMENSION_REGISTRY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .specs
        .get(index)
        .map(|spec| spec.key)
}

/// Install `spec` under its canonical key. Re-registering an existing key updates it
/// in place so slots stay stable across a datapack rescan.
fn register_dimension_spec(mut spec: DimensionSpec) -> Result<DimId, String> {
    let mut registry = DIMENSION_REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    if let Some(&raw) = registry.by_key.get(spec.key) {
        spec.id = DimId(raw);
        registry.specs[raw as usize - DimId::ALL.len()] = spec;
        return Ok(DimId(raw));
    }
    let total = DimId::ALL.len() + registry.specs.len();
    if total >= MAX_DIMENSIONS {
        return Err(format!(
            "dimension registry is full ({MAX_DIMENSIONS} slots): {}",
            spec.key
        ));
    }
    let raw = total as u32;
    spec.id = DimId(raw);
    registry.by_key.insert(spec.key.to_string(), raw);
    registry.specs.push(spec);
    Ok(DimId(raw))
}

/// Drop every custom registration and generator slot. Registration is append-only at
/// runtime; tests reset between fixtures.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn reset_dimension_registry() {
    let mut registry = DIMENSION_REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    registry.specs.clear();
    registry.by_key.clear();
    drop(registry);
    PER_DIM.gens.lock().unwrap_or_else(|e| e.into_inner()).truncate(DimId::ALL.len());
    PER_DIM.data.lock().unwrap_or_else(|e| e.into_inner()).truncate(DimId::ALL.len());
    PER_DIM.facts.lock().unwrap_or_else(|e| e.into_inner()).truncate(DimId::ALL.len());
}

/// `j.num()` as an integer, or `None` when the field is absent or not a whole number.
fn jint(j: &J, key: &str) -> Option<i64> {
    let value = j.get(key)?.num();
    (value.fract() == 0.0 && value >= f64::from(i32::MIN) && value <= f64::from(i32::MAX))
        .then_some(value as i64)
}

/// `j.str()` for a string field that must be present.
fn jstring<'a>(j: &'a J, key: &str) -> Option<&'a str> {
    match j.get(key)? {
        J::Str(value) => Some(value),
        _ => None,
    }
}

/// A canonical `"namespace:path"` from a ResourceLocation string; a bare path names
/// the `minecraft` namespace.
fn canonical_key(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let (namespace, path) = match name.split_once(':') {
        Some((ns, path)) => (ns, path),
        None => ("minecraft", name),
    };
    let valid = |part: &str, colon: bool| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-./".contains(c) || (colon && c == ':'))
    };
    (valid(namespace, false) && valid(path, true))
        .then(|| format!("{namespace}:{path}"))
}

/// The `dimension_type` values the four vanilla registry names carry. Custom
/// dimensions may name them; `minecraft:overworld_caves` maps onto the Overworld
/// window because this port does not distinguish the caves variant.
fn vanilla_dimension_type(type_key: &str) -> Option<DimensionSpec> {
    let (min_y, height, skylight, ceiling, ultrawarm, scale, ambient, effects, infiniburn, raids, bed, anchor) =
        match type_key {
            "minecraft:overworld" | "minecraft:overworld_caves" => (
                -64, 384, true, false, false, 1.0, 0.0, "minecraft:overworld",
                "#minecraft:infiniburn_overworld", true, true, false,
            ),
            "minecraft:the_nether" => (
                0, 256, false, true, true, 8.0, 0.1, "minecraft:the_nether",
                "#minecraft:infiniburn_nether", false, false, true,
            ),
            "minecraft:the_end" => (
                0, 256, false, false, false, 1.0, 0.0, "minecraft:the_end",
                "#minecraft:infiniburn_end", false, false, false,
            ),
            _ => return None,
        };
    Some(DimensionSpec {
        id: DimId::Overworld,
        key: "",
        type_key: "",
        min_y,
        height,
        logical_height: height,
        has_skylight: skylight,
        has_ceiling: ceiling,
        ultrawarm,
        natural: true,
        coordinate_scale: scale,
        ambient_light: ambient,
        fixed_time: None,
        piglin_safe: false,
        has_raids: raids,
        bed_works: bed,
        respawn_anchor_works: anchor,
        effects: effects.into(),
        infiniburn: infiniburn.into(),
        monster_spawn_light_level: 0,
        monster_spawn_block_light_limit: 0,
        generator: CustomGeneratorKind::Void { biome_id: 39 },
    })
}

/// Parse one `dimension_type/*.json` body into the fields a `DimensionSpec` carries,
/// with vanilla's defaults for the optional fields and hard validation on the window.
fn parse_dimension_type(key: &str, j: &J) -> Result<DimensionSpec, String> {
    let bad = |why: &str| format!("dimension_type {key}: {why}");
    let min_y = jint(j, "min_y").ok_or_else(|| bad("missing integer min_y"))? as i32;
    let height = jint(j, "height").ok_or_else(|| bad("missing integer height"))? as i32;
    if min_y % 16 != 0 {
        return Err(bad("min_y must be a multiple of 16"));
    }
    if height <= 0 || height % 16 != 0 || height > 4096 {
        return Err(bad("height must be a positive multiple of 16 up to 4096"));
    }
    let coordinate_scale = j.get("coordinate_scale").map(J::num).unwrap_or(1.0);
    if !(coordinate_scale > 0.0 && coordinate_scale.is_finite()) {
        return Err(bad("coordinate_scale must be positive and finite"));
    }
    let ambient_light = j.get("ambient_light").map(J::num).unwrap_or(0.0) as f32;
    let natural = jbool(j, "natural", true);
    Ok(DimensionSpec {
        id: DimId::Overworld,
        key: "",
        type_key: "",
        min_y,
        height,
        logical_height: jint(j, "logical_height").map_or(height, |v| v as i32),
        has_skylight: jbool(j, "has_skylight", true),
        has_ceiling: jbool(j, "has_ceiling", false),
        ultrawarm: jbool(j, "ultrawarm", false),
        natural,
        coordinate_scale,
        ambient_light,
        fixed_time: jint(j, "fixed_time"),
        piglin_safe: jbool(j, "piglin_safe", false),
        has_raids: jbool(j, "has_raids", natural),
        bed_works: jbool(j, "bed_works", true),
        respawn_anchor_works: jbool(j, "respawn_anchor_works", false),
        effects: jstring(j, "effects").unwrap_or("minecraft:overworld").into(),
        infiniburn: jstring(j, "infiniburn")
            .unwrap_or("#minecraft:infiniburn_overworld")
            .into(),
        monster_spawn_light_level: j
            .get("monster_spawn_light_level")
            .and_then(|v| jint(&J::Obj(vec![("v".into(), v.clone())]), "v"))
            .unwrap_or(0) as i32,
        monster_spawn_block_light_limit: jint(j, "monster_spawn_block_light_limit")
            .unwrap_or(0) as i32,
        generator: CustomGeneratorKind::Void { biome_id: 39 },
    })
}

/// The `settings.biome` of a flat/unknown generator, or the engine's fallback.
fn generator_biome_id(generator: &J) -> i64 {
    generator
        .get("biome")
        .map(J::str)
        .map(biome_id)
        .unwrap_or(39) // plains
}

/// Parse the `generator` object of a `dimension/*.json` into a generator kind.
fn parse_dimension_generator(
    key: &str,
    j: &J,
    res: fn(&str) -> Option<u32>,
) -> Result<CustomGeneratorKind, String> {
    let bad = |why: &str| format!("dimension {key}: {why}");
    let generator = j.get("generator").ok_or_else(|| bad("missing generator"))?;
    match generator.get("type").map(J::str) {
        Some("minecraft:noise") => {
            let preset = generator
                .get("settings")
                .map(J::str)
                .and_then(|name| NoisePreset::parse(name.rsplit(':').next().unwrap_or(name)));
            Ok(match preset {
                Some(preset) => CustomGeneratorKind::Noise { preset },
                // Noise settings beyond the three vanilla presets cannot be loaded:
                // fall back to a void world rather than refusing the dimension.
                None => CustomGeneratorKind::Void {
                    biome_id: generator_biome_id(generator),
                },
            })
        }
        Some("minecraft:flat") => {
            let settings = generator
                .get("settings")
                .ok_or_else(|| bad("flat generator without settings"))?;
            let mut layers = Vec::new();
            if let Some(list) = settings.get("layers").and_then(J::as_arr) {
                for layer in list {
                    let name = layer.get("block").map(J::str).unwrap_or("");
                    let height = layer.get("height").map(J::num).unwrap_or(0.0);
                    if !(height > 0.0 && height.fract() == 0.0 && height <= 4096.0) {
                        return Err(bad("flat layer height must be a positive integer"));
                    }
                    let Some(state) = res(name).filter(|&id| id <= u32::from(u16::MAX))
                    else {
                        return Err(bad(&format!("flat layer block '{name}' is unknown")));
                    };
                    layers.push((state as u16, height as u32));
                }
            }
            Ok(CustomGeneratorKind::Flat {
                layers,
                biome_id: generator_biome_id(settings),
            })
        }
        // `minecraft:debug` and every generator type this engine does not know:
        // a safe void world is the honest answer.
        _ => Ok(CustomGeneratorKind::Void {
            biome_id: generator_biome_id(generator),
        }),
    }
}

/// One datapack's raw materials for the dimension scan: a directory root whose
/// `data/` tree is walked, or a zip's already-extracted `(entry path, body)` pairs.
pub(crate) enum DatapackDimensionSource {
    Directory(std::path::PathBuf),
    Entries(Vec<(String, String)>),
}

/// Scan `data/<ns>/dimension_type/*.json` and `data/<ns>/dimension/*.json` across the
/// packs, in the order `datapack_sources` declared them (later packs override earlier
/// ones, matching `read_entry`'s own merge), and register every valid custom
/// dimension. Custom slots are assigned in `(namespace, path)` order across the whole
/// merge, so the wire ids are deterministic. The three vanilla dimension names are
/// pinned and never re-registered. Returns the number of distinct custom dimensions
/// now registered.
///
/// An invalid file is skipped with a diagnostic rather than aborting the scan —
/// vanilla ignores a bad pack file the same way.
pub(crate) fn register_datapack_dimensions(
    sources: &[DatapackDimensionSource],
    res: fn(&str) -> Option<u32>,
) -> Result<usize, String> {
    *BLOCK_LOOKUP.lock().unwrap_or_else(|e| e.into_inner()) = Some(res);
    let mut types: std::collections::BTreeMap<(String, String), J> = Default::default();
    let mut dims: std::collections::BTreeMap<(String, String), J> = Default::default();
    for source in sources {
        let files: Vec<(String, String)> = match source {
            DatapackDimensionSource::Entries(entries) => entries
                .iter()
                .filter(|(path, _)| path.ends_with(".json"))
                .map(|(path, body)| (path.clone(), body.clone()))
                .collect(),
            DatapackDimensionSource::Directory(root) => {
                let mut dirs = vec![root.join("data")];
                let mut files = Vec::new();
                while let Some(dir) = dirs.pop() {
                    let Ok(entries) = std::fs::read_dir(&dir) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        let Ok(file_kind) = entry.file_type() else {
                            continue;
                        };
                        let path = entry.path();
                        if file_kind.is_dir() {
                            dirs.push(path);
                        } else if file_kind.is_file()
                            && path.extension().is_some_and(|ext| ext == "json")
                        {
                            let rel = path
                                .strip_prefix(root)
                                .ok()
                                .and_then(|p| p.to_str())
                                .map(|p| p.replace('\\', "/"));
                            if let (Some(rel), Ok(body)) = (rel, std::fs::read_to_string(&path))
                            {
                                files.push((rel, body));
                            }
                        }
                    }
                }
                files
            }
        };
        for (rel, body) in files {
            // `data/<ns>/<kind>/<path>.json` — anything else is not a dimension file.
            let mut parts = rel.splitn(3, '/');
            let (Some("data"), Some(ns), Some(rest)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Some((kind, name)) = rest.split_once('/') else {
                continue;
            };
            let Some(name) = name.strip_suffix(".json") else {
                continue;
            };
            let map = match kind {
                "dimension_type" => &mut types,
                "dimension" => &mut dims,
                _ => continue,
            };
            match jparse(body.as_bytes()) {
                Some(j) => {
                    map.insert((ns.to_string(), name.to_string()), j);
                }
                None => eprintln!("[spine] invalid {kind} JSON {rel}"),
            }
        }
    }
    register_dimension_maps(&types, &dims, res)
}

/// The registration half of [`register_datapack_dimensions`], separated so the file
/// walk and the (ns, path)-sorted registration order stay testable apart.
fn register_dimension_maps(
    types: &std::collections::BTreeMap<(String, String), J>,
    dims: &std::collections::BTreeMap<(String, String), J>,
    res: fn(&str) -> Option<u32>,
) -> Result<usize, String> {
    let mut registered = 0usize;
    for ((ns, path), dim_j) in dims {
        let key = format!("{ns}:{path}");
        // The three vanilla names are pinned: a datapack must not move a vanilla
        // slot or its wire id.
        if matches!(
            key.as_str(),
            "minecraft:overworld" | "minecraft:the_nether" | "minecraft:the_end"
        ) {
            continue;
        }
        let Some(type_key) = canonical_key(dim_j.get("type").map(J::str).unwrap_or(""))
        else {
            eprintln!("[spine] dimension {key}: missing or invalid type");
            continue;
        };
        let type_spec = match dim_j.get("type") {
            // An inline type object parses directly; the dimension's own key names it.
            Some(J::Obj(_)) => {
                match parse_dimension_type(&key, dim_j.get("type").unwrap()) {
                    Ok(spec) => Ok(spec),
                    Err(error) => Err(error),
                }
            }
            _ => {
                let (tns, tpath) = type_key.split_once(':').unwrap();
                match types.get(&(tns.to_string(), tpath.to_string())) {
                    Some(tj) => parse_dimension_type(&type_key, tj),
                    None => match vanilla_dimension_type(&type_key) {
                        Some(spec) => Ok(spec),
                        None => Err(format!(
                            "dimension {key}: unknown dimension_type '{type_key}'"
                        )),
                    },
                }
            }
        };
        let mut spec = match type_spec {
            Ok(spec) => spec,
            Err(error) => {
                eprintln!("[spine] {error}");
                continue;
            }
        };
        spec.generator = match parse_dimension_generator(&key, dim_j, res) {
            Ok(generator) => generator,
            Err(error) => {
                eprintln!("[spine] {error}");
                continue;
            }
        };
        spec.key = Box::leak(key.clone().into_boxed_str());
        spec.type_key = match dim_j.get("type") {
            Some(J::Obj(_)) => spec.key,
            _ => Box::leak(type_key.into_boxed_str()),
        };
        match register_dimension_spec(spec) {
            Ok(_) => registered += 1,
            Err(error) => eprintln!("[spine] {error}"),
        }
    }
    Ok(registered)
}

/// The generated state for one cell of a `Flat`/`Void` custom dimension, without a
/// generator. `Noise` specs, vanilla dimensions and unregistered slots answer `None`:
/// their `Gen` owns the answer.
pub(crate) fn custom_dim_state(dimension: DimId, _x: i32, y: i32, _z: i32) -> Option<u32> {
    let spec = dimension_spec(dimension)?;
    match &spec.generator {
        CustomGeneratorKind::Void { .. } => Some(0),
        CustomGeneratorKind::Flat { layers, .. } => {
            let mut top = i64::from(spec.min_y);
            for &(state, height) in layers {
                top += i64::from(height);
                if i64::from(y) < top {
                    return Some(u32::from(state));
                }
            }
            Some(0)
        }
        CustomGeneratorKind::Noise { .. } => None,
    }
}

/// The uniform biome id a `Flat`/`Void` custom dimension reports, or `None`.
fn custom_biome_id(dimension: DimId) -> Option<i64> {
    match dimension_spec(dimension)?.generator {
        CustomGeneratorKind::Flat { biome_id, .. } | CustomGeneratorKind::Void { biome_id } => {
            Some(biome_id)
        }
        CustomGeneratorKind::Noise { .. } => None,
    }
}

/// One 16³ section of a `Flat`/`Void` custom dimension, or `None` when the dimension
/// has a real generator (vanilla or custom `Noise`) that must answer instead.
fn custom_terrain_section(dimension: DimId, sy: i32) -> Option<Box<[u32; 4096]>> {
    let spec = dimension_spec(dimension)?;
    match &spec.generator {
        CustomGeneratorKind::Noise { .. } => None,
        CustomGeneratorKind::Void { .. } => Some(Box::new([0; 4096])),
        CustomGeneratorKind::Flat { layers, .. } => {
            let mut arr = Box::new([0u32; 4096]);
            let mut top = i64::from(spec.min_y);
            for &(state, height) in layers {
                let bottom = top;
                top += i64::from(height);
                for ly in 0..16i64 {
                    let y = i64::from(sy) + ly;
                    if y >= bottom && y < top {
                        let row = ly as usize * 256;
                        arr[row..row + 256].fill(u32::from(state));
                    }
                }
            }
            Some(arr)
        }
    }
}

/// The column surface a `Flat`/`Void` custom dimension reports for the light and
/// height paths: the top of its layer stack (`min_y` for a void). `None` for anything
/// with a real generator.
pub(crate) fn custom_surface_height(dimension: DimId) -> Option<i32> {
    let spec = dimension_spec(dimension)?;
    match &spec.generator {
        CustomGeneratorKind::Flat { layers, .. } => Some(
            spec.min_y
                .saturating_add(layers.iter().map(|(_, height)| *height as i32).sum::<i32>()),
        ),
        CustomGeneratorKind::Void { .. } => Some(spec.min_y),
        CustomGeneratorKind::Noise { .. } => None,
    }
}

pub(crate) fn parse_world_seed(value: &str) -> Result<u64, std::num::ParseIntError> {
    value
        .parse::<i64>()
        .map(|seed| seed as u64)
        .or_else(|_| value.parse())
}

/// The `dimension_type` file a preset's chunks belong to. Note the two names that are
/// not the preset: the Nether and the End are `the_nether` / `the_end` on disk.
///
/// One preset, one dimension: the overworld noise settings also cover its caves
/// (`overworld_caves` is a separate dimension_type this port does not load), and it
/// declares the same -64/384 window.
fn dimension_type_file(preset: NoisePreset) -> &'static str {
    preset.dim().dimension_file().unwrap_or("overworld")
}

/// The world window one `dimension_type` object declares, or `None` when it is not a
/// window this generator can live in.
///
/// Three refusals, each of which would otherwise silently cut terrain or produce a dump
/// whose length is not a whole number of sections:
/// * `min_y`/`height` missing, non-positive, or not multiples of 16;
/// * a window that starts above the generator's own `min_y`;
/// * a window that ends below the generator's top.
///
/// `None` means "keep `world_* = gen_*`" and the loader traces the reason.
fn dimension_window(j: &J, gen_min_y: i32, gen_height: i32) -> Option<(i32, i32)> {
    let min_y = j.get("min_y").map(J::num)? as i32;
    let height = j.get("height").map(J::num)? as i32;
    if height <= 0 || height % 16 != 0 || min_y % 16 != 0 {
        return None;
    }
    if min_y > gen_min_y || min_y + height < gen_min_y + gen_height {
        return None;
    }
    Some((min_y, height))
}

// ---------- beardifier (vanilla `Beardifier`) ----------
//
// The vanilla `beardifier` density-function node is not in the datapack: `NoiseChunk`
// wraps the router with `add(final_density, BeardifierMarker)` and substitutes the
// chunk's `Beardifier` for the marker (`NoiseChunk.wrap`), so this module inserts the
// same node (`load_preset`) and evaluates it against the chunk's beard data.
//
// Bytecode authority (pinned jar, `tools/bytecode_probe.sh`):
// `Beardifier.compute(dyr$b)`, `Beardifier.a(IIII)` (kernel lookup),
// `Beardifier.a(IDI)` (kernel value = `pow(E, -(x^2+y^2+z^2)/16)`),
// `Beardifier.a(DDD)` = `clampedMap(length(x,y,z), 0, 6, 1, 0)` (the burying value),
// `Mth.fastInvSqrt(double)` (the bit-hack, not `1/sqrt`).
// `TerrainAdjustment` ordinals: NONE, BURY, BEARD_THIN, BEARD_BOX, ENCAPSULATE.

/// `Beardifier.TerrainAdjustment` — the structure's `terrain_adaptation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BeardAdjustment {
    None,
    Bury,
    BeardThin,
    BeardBox,
    Encapsulate,
}

impl BeardAdjustment {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::None),
            "bury" => Some(Self::Bury),
            "beard_thin" => Some(Self::BeardThin),
            "beard_box" => Some(Self::BeardBox),
            "encapsulate" => Some(Self::Encapsulate),
            _ => None,
        }
    }
}

/// `Beardifier.Rigid`: one piece's bounding box, the structure's terrain
/// adjustment and the piece's ground-level delta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BeardRigid {
    pub(crate) min: [i32; 3],
    pub(crate) max: [i32; 3],
    pub(crate) adjustment: BeardAdjustment,
    pub(crate) ground_level_delta: i32,
}

/// `JigsawJunction`: a jigsaw connection point. `rigid` records the source
/// piece's projection (RIGID vs TERRAIN_MATCHING); `compute` does not read it,
/// but the collection filter and diagnostics do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BeardJunction {
    pub(crate) source_x: i32,
    pub(crate) source_ground_y: i32,
    pub(crate) source_z: i32,
    pub(crate) delta_y: i32,
    pub(crate) rigid: bool,
}

/// `Beardifier.forStructuresInChunk`: the rigids and junctions of every structure
/// piece that is close to one chunk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BeardChunkData {
    pub(crate) rigids: Vec<BeardRigid>,
    pub(crate) junctions: Vec<BeardJunction>,
}

impl BeardChunkData {
    pub(crate) fn is_empty(&self) -> bool {
        self.rigids.is_empty() && self.junctions.is_empty()
    }
}

/// `Beardifier.BEARD_KERNEL_RADIUS`.
const BEARD_KERNEL_RADIUS: i32 = 12;
/// `Beardifier.BEARD_KERNEL_SIZE`.
const BEARD_KERNEL_SIZE: usize = 24;

/// `Beardifier.BEARD_KERNEL`: 24^3 f32 values, `BEARD_KERNEL[z*576 + x*24 + y]`
/// = `kernel(x, y, z)` for offsets relative to the kernel centre.
fn beard_kernel() -> &'static [f32] {
    static KERNEL: std::sync::OnceLock<Box<[f32]>> = std::sync::OnceLock::new();
    KERNEL.get_or_init(|| {
        let mut v = vec![0f32; BEARD_KERNEL_SIZE * BEARD_KERNEL_SIZE * BEARD_KERNEL_SIZE];
        for i in 0..BEARD_KERNEL_SIZE {
            for j in 0..BEARD_KERNEL_SIZE {
                for k in 0..BEARD_KERNEL_SIZE {
                    v[i * 576 + j * 24 + k] = beard_kernel_value(
                        j as i32 - BEARD_KERNEL_RADIUS,
                        k as i32 - BEARD_KERNEL_RADIUS,
                        i as i32 - BEARD_KERNEL_RADIUS,
                    );
                }
            }
        }
        v.into_boxed_slice()
    })
}

/// `Beardifier.a(int, int, int)` → `Beardifier.a(int, double, int)`: the table
/// value `pow(E, -(x^2 + (y+0.5)^2 + z^2)/16)`.
fn beard_kernel_value(x: i32, y: i32, z: i32) -> f32 {
    let y = f64::from(y) + 0.5;
    let xs = f64::from(x);
    let zs = f64::from(z);
    let squared = xs * xs + y * y + zs * zs;
    // Java `Math.pow(Math.E, -squared / 16)`: the value is stored as f32 in the table.
    std::f64::consts::E.powf(-squared / 16.0) as f32
}

/// `Mth.fastInvSqrt(double)`: the seeded bit-hack, *not* `1.0 / sqrt(d)`.
fn fast_inv_sqrt(d: f64) -> f64 {
    let half = 0.5 * d;
    let bits = 6_910_469_410_427_058_090u64.wrapping_sub((d.to_bits() as i64 >> 1) as u64);
    let d = f64::from_bits(bits);
    d * (1.5 - half * d * d)
}

/// `Beardifier.a(int, int, int, int)`: the exp kernel times the ground-plane term.
/// `ground_level_delta` is the y distance whose half-plane the kernel is centred on.
fn beard_contribution(x: i32, y: i32, z: i32, ground_level_delta: i32) -> f64 {
    let (ix, iy, iz) = (x + 12, y + 12, z + 12);
    if !(0..BEARD_KERNEL_SIZE as i32).contains(&ix)
        || !(0..BEARD_KERNEL_SIZE as i32).contains(&iy)
        || !(0..BEARD_KERNEL_SIZE as i32).contains(&iz)
    {
        return 0.0;
    }
    let d = f64::from(ground_level_delta) + 0.5;
    let (xs, zs) = (f64::from(x), f64::from(z));
    let squared = xs * xs + d * d + zs * zs;
    let f = -d * fast_inv_sqrt(squared / 2.0) / 2.0;
    let kernel = beard_kernel()[iz as usize * 576 + ix as usize * 24 + iy as usize];
    f * f64::from(kernel)
}

/// `Beardifier.a(double, double, double)` — `clampedMap(length(x, y, z), 0, 6, 1, 0)`.
fn beard_burying_value(x: f64, y: f64, z: f64) -> f64 {
    let len = (x * x + y * y + z * z).sqrt();
    clamped_map(len, 0.0, 6.0, 1.0, 0.0)
}

/// `Mth.clampedMap(value, min, max, newMin, newMax)`.
fn clamped_map(value: f64, min: f64, max: f64, new_min: f64, new_max: f64) -> f64 {
    let delta = (value - min) / (max - min);
    let t = delta.clamp(0.0, 1.0);
    new_min + t * (new_max - new_min)
}

/// `Beardifier.compute(FunctionContext)`: the density this chunk's structures add to
/// the terrain density at one block position.
pub(crate) fn compute_beardifier(data: &BeardChunkData, x: i32, y: i32, z: i32) -> f64 {
    let mut density = 0.0f64;
    for rigid in &data.rigids {
        let dx = (rigid.min[0] - x).max(x - rigid.max[0]).max(0);
        let dz = (rigid.min[2] - z).max(z - rigid.max[2]).max(0);
        let ground = rigid.min[1] + rigid.ground_level_delta;
        let dy = y - ground;
        let boxed = match rigid.adjustment {
            BeardAdjustment::None => 0,
            BeardAdjustment::Bury | BeardAdjustment::BeardThin => dy,
            BeardAdjustment::BeardBox => (ground - y).max(y - rigid.max[1]).max(0),
            BeardAdjustment::Encapsulate => (rigid.min[1] - y).max(y - rigid.max[1]).max(0),
        };
        density += match rigid.adjustment {
            BeardAdjustment::None => 0.0,
            BeardAdjustment::Bury => {
                beard_burying_value(f64::from(dx), f64::from(boxed) / 2.0, f64::from(dz))
            }
            BeardAdjustment::BeardThin | BeardAdjustment::BeardBox => {
                beard_contribution(dx, boxed, dz, dy) * 0.8
            }
            BeardAdjustment::Encapsulate => {
                beard_burying_value(
                    f64::from(dx) / 2.0,
                    f64::from(boxed) / 2.0,
                    f64::from(dz) / 2.0,
                ) * 0.8
            }
        };
    }
    for junction in &data.junctions {
        let q = x - junction.source_x;
        let r = y - junction.source_ground_y;
        let s = z - junction.source_z;
        density += beard_contribution(q, r, s, r) * 0.4;
    }
    density
}

/// Resolves one chunk's beardifier input. `worldgen` may not import the structure
/// pipeline (`feature` sits above it in `tools/layers_check.sh`), so the pipeline
/// registers this function instead.
pub(crate) type BeardResolver = fn(&mut Gen, i32, i32) -> BeardChunkData;

static GLOBAL_BEARD_RESOLVER: Mutex<Option<BeardResolver>> = Mutex::new(None);

/// Register the process-wide beardifier resolver (`main.rs` and the structure tests).
pub(crate) fn register_beard_resolver(resolver: BeardResolver) {
    *GLOBAL_BEARD_RESOLVER.lock().unwrap_or_else(|e| e.into_inner()) = Some(resolver);
}

fn global_beard_resolver() -> Option<BeardResolver> {
    *GLOBAL_BEARD_RESOLVER.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
enum SV {
    V(f32),
    S(Box<Spl>),
}
#[derive(Clone)]
struct Spl {
    coord: usize,
    pts: Vec<(f32, SV, f32)>,
}

/// `dys$z$a`: the rarity mapper of a `weird_scaled_sampler`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RarityValueMapper {
    Type1,
    Type2,
}

impl RarityValueMapper {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "type_1" => Some(Self::Type1),
            "type_2" => Some(Self::Type2),
            _ => None,
        }
    }

    /// `dzf$a.b` (TYPE1) / `dzf$a.a` (TYPE2). Strict comparisons: a value equal
    /// to a threshold takes the next branch, and NaN falls through to the last.
    fn rarity(self, input: f64) -> f64 {
        match self {
            Self::Type1 => {
                if input < -0.5 {
                    0.75
                } else if input < 0.0 {
                    1.0
                } else if input < 0.5 {
                    1.5
                } else {
                    2.0
                }
            }
            Self::Type2 => {
                if input < -0.75 {
                    0.5
                } else if input < -0.5 {
                    0.75
                } else if input < 0.5 {
                    1.0
                } else if input < 0.75 {
                    2.0
                } else {
                    3.0
                }
            }
        }
    }
}

#[derive(Clone)]
enum DF {
    C(f64),
    EndIslands(Box<EndIslands>),
    Add(usize, usize),
    Sub(usize, usize),
    Mul(usize, usize),
    Min(usize, usize),
    Max(usize, usize),
    Div(usize, usize),
    Neg(usize),
    Abs(usize),
    Sq(usize),
    Cube(usize),
    HNeg(usize),
    QNeg(usize),
    Squeeze(usize),
    Clamp {
        i: usize,
        lo: f64,
        hi: f64,
    },
    Lerp {
        a: usize,
        f: usize,
        s: usize,
    },
    Grad {
        fc: f64,
        fv: f64,
        tc: f64,
        tv: f64,
    }, // axis always y here
    Cache(usize),
    // vanilla `interpolated`: sampled at the preset's cell corners, trilinearly
    // interpolated at the query position (a per-chunk grid, see ChunkCols).
    Interp(usize),
    Noise {
        ni: usize,
        sx: usize,
        sy: usize,
        sz: usize,
        xzs: f64,
        ys: f64,
    },
    ShiftA(usize),
    ShiftB(usize),
    WeirdScaledSampler {
        input: usize,
        ni: usize,
        mapper: RarityValueMapper,
    },
    RangeChoice {
        i: usize,
        mi: f64,
        ma: f64,
        wr: usize,
        wo: usize,
    },
    IntervalSelect {
        i: usize,
        th: Vec<f64>,
        fns: Vec<usize>,
    },
    Spline(Box<Spl>),
    Blended(Box<BlendedNoise>),
    BlendD(usize),
    Zero,
    /// Vanilla `BeardifierOrMarker`: the chunk's `BeardChunkData` decides the value.
    Beardifier,
    FindTopSurface {
        density: usize,
        lower: i32,
        upper: usize,
        cell_h: i32,
    },
}

/// Density-function ids a node reads. Shared by the grid reachability walk and by
/// [`compute_is_2d`]; nested spline points are flattened into the same `out`.
fn df_kids(df: &DF, out: &mut Vec<usize>) {
    fn spl_kids(s: &Spl, out: &mut Vec<usize>) {
        out.push(s.coord);
        for (_, v, _) in &s.pts {
            if let SV::S(inner) = v {
                spl_kids(inner, out);
            }
        }
    }
    match df {
        DF::C(_)
        | DF::EndIslands(_)
        | DF::Zero
        | DF::Beardifier
        | DF::Grad { .. }
        | DF::ShiftA(_)
        | DF::ShiftB(_)
        | DF::Blended(_) => {}
        DF::WeirdScaledSampler { input, .. } => out.push(*input),
        DF::Add(a, b)
        | DF::Sub(a, b)
        | DF::Mul(a, b)
        | DF::Min(a, b)
        | DF::Max(a, b)
        | DF::Div(a, b) => {
            out.push(*a);
            out.push(*b);
        }
        DF::Neg(a)
        | DF::Abs(a)
        | DF::Sq(a)
        | DF::Cube(a)
        | DF::HNeg(a)
        | DF::QNeg(a)
        | DF::Squeeze(a)
        | DF::Cache(a)
        | DF::Interp(a)
        | DF::BlendD(a) => out.push(*a),
        DF::Clamp { i, .. } => out.push(*i),
        DF::Lerp { a, f, s } => {
            out.push(*a);
            out.push(*f);
            out.push(*s);
        }
        DF::Noise { sx, sy, sz, .. } => {
            out.push(*sx);
            out.push(*sy);
            out.push(*sz);
        }
        DF::RangeChoice { i, wr, wo, .. } => {
            out.push(*i);
            out.push(*wr);
            out.push(*wo);
        }
        DF::IntervalSelect { i, fns, .. } => {
            out.push(*i);
            out.extend(fns.iter().copied());
        }
        DF::Spline(s) => spl_kids(s, out),
        DF::FindTopSurface { density, upper, .. } => {
            out.push(*density);
            out.push(*upper);
        }
    }
}

/// Classify every node as a pure function of `(x, z)` or not, with a memoized post-order
/// DFS over the DAG (the id table is not in topological order — references can point
/// forward — so a single forward pass would read uninitialised entries).
/// `FindTopSurface` is 2D even though its inner scan touches many `y` values: its *result*
/// depends only on `(x, z)`. A `Noise` node is 2D only when `ys == 0.0` and all three of its
/// scale inputs are 2D; the `y as f64 * 0.0 + ysv` sign-of-zero difference that can arise for
/// negative `y` is erased by `wrap(±0.0) + yo` (the noise's `yo` is finite and non-negative),
/// which the bit-identity test checks across `-64..320`.
fn compute_is_2d(dfs: &[DF]) -> Vec<bool> {
    fn visit(id: usize, dfs: &[DF], is2: &mut [bool], state: &mut [u8]) -> bool {
        if state[id] == 2 {
            return is2[id];
        }
        if state[id] == 1 {
            // A cycle is not a valid density DAG; be conservative rather than loop.
            return false;
        }
        state[id] = 1;
        let mut kids = Vec::new();
        df_kids(&dfs[id], &mut kids);
        let mut all_kids_2d = true;
        for k in kids {
            if !visit(k, dfs, is2, state) {
                all_kids_2d = false;
            }
        }
        let v = match &dfs[id] {
            DF::C(_)
            | DF::Zero
            | DF::EndIslands(_)
            | DF::ShiftA(_)
            | DF::ShiftB(_)
            | DF::FindTopSurface { .. } => true,
            DF::Grad { .. } | DF::Beardifier | DF::Blended(_) | DF::WeirdScaledSampler { .. } => {
                false
            }
            DF::Noise { ys, .. } => *ys == 0.0 && all_kids_2d,
            _ => all_kids_2d,
        };
        is2[id] = v;
        state[id] = 2;
        v
    }
    let n = dfs.len();
    let mut is2 = vec![false; n];
    let mut state = vec![0u8; n];
    for id in 0..n {
        visit(id, dfs, &mut is2, &mut state);
    }
    is2
}

/// Exact post-order constant folding over the density-function id table.
///
/// [`DfCompiler::const_value`] already knows which nodes are position
/// independent; this pass applies the same IEEE-754 operations once at load so
/// the folded `DF::C` answers the interpreter, the flat-memo evaluator, the
/// vein/aquifer fast paths and the compiled program identically while doing
/// less work per block.
///
/// Only `Const op Const` shapes fold — no `x + 0.0` identities and no
/// `0.0 * x` (both are wrong for `-0.0`/`NaN`/`Inf`), and no reassociation:
/// `(a + b) + c` never becomes `a + (b + c)`. `Cache` and `BlendD` are pure
/// pass-throughs and fold with their input. `Interp` folds only when the
/// constant survives the corner-grid trilinear: `lerp3` of eight equal finite,
/// non-negative-zero corners returns the corner itself, while a `-0.0` or
/// non-finite constant would come back `+0.0`/NaN through the grid. `Zero` is
/// never rewritten: the vein and aquifer fast paths recognise the variant
/// itself (`matches!(.., DF::Zero)`), not just its value.
///
/// Returns the number of nodes rewritten to `DF::C`.
fn fold_constant_dfs(dfs: &mut Vec<DF>) -> usize {
    fn const_at(dfs: &[DF], id: usize, vals: &mut [Option<f64>], state: &mut [u8]) -> Option<f64> {
        if state[id] == 2 {
            return vals[id];
        }
        if state[id] == 1 {
            // A cycle is not a valid density DAG; leave it unfolded.
            return None;
        }
        state[id] = 1;
        let v = match &dfs[id] {
            DF::C(v) => Some(*v),
            DF::Zero => Some(0.0),
            DF::Add(a, b) => {
                match (const_at(dfs, *a, vals, state), const_at(dfs, *b, vals, state)) {
                    (Some(a), Some(b)) => Some(a + b),
                    _ => None,
                }
            }
            DF::Sub(a, b) => {
                match (const_at(dfs, *a, vals, state), const_at(dfs, *b, vals, state)) {
                    (Some(a), Some(b)) => Some(a - b),
                    _ => None,
                }
            }
            DF::Mul(a, b) => {
                match (const_at(dfs, *a, vals, state), const_at(dfs, *b, vals, state)) {
                    (Some(a), Some(b)) => Some(a * b),
                    _ => None,
                }
            }
            DF::Min(a, b) => {
                match (const_at(dfs, *a, vals, state), const_at(dfs, *b, vals, state)) {
                    (Some(a), Some(b)) => Some(df_min(a, b)),
                    _ => None,
                }
            }
            DF::Max(a, b) => {
                match (const_at(dfs, *a, vals, state), const_at(dfs, *b, vals, state)) {
                    (Some(a), Some(b)) => Some(df_max(a, b)),
                    _ => None,
                }
            }
            DF::Div(a, b) => match const_at(dfs, *b, vals, state) {
                Some(0.0) => Some(0.0),
                Some(b) => const_at(dfs, *a, vals, state).map(|a| a / b),
                None => None,
            },
            DF::Neg(a) => const_at(dfs, *a, vals, state).map(|v| -v),
            DF::Abs(a) => const_at(dfs, *a, vals, state).map(|v| v.abs()),
            DF::Sq(a) => const_at(dfs, *a, vals, state).map(|v| v * v),
            DF::Cube(a) => const_at(dfs, *a, vals, state).map(|v| v * v * v),
            DF::HNeg(a) => const_at(dfs, *a, vals, state)
                .map(|v| if v > 0.0 { v } else { v * 0.5 }),
            DF::QNeg(a) => const_at(dfs, *a, vals, state)
                .map(|v| if v > 0.0 { v } else { v * 0.25 }),
            DF::Squeeze(a) => const_at(dfs, *a, vals, state).map(|v| {
                let v = v.clamp(-1.0, 1.0);
                v / 2.0 - v * v * v / 24.0
            }),
            DF::Clamp { i, lo, hi } => {
                // `f64::clamp` panics on `lo > hi` or a NaN bound; the
                // interpreter would panic at evaluation time, so an out-of-order
                // clamp is left unfolded to keep that behaviour identical.
                if *lo <= *hi {
                    const_at(dfs, *i, vals, state).map(|v| v.clamp(*lo, *hi))
                } else {
                    None
                }
            }
            DF::Lerp { a, f, s } => {
                match (
                    const_at(dfs, *a, vals, state),
                    const_at(dfs, *f, vals, state),
                    const_at(dfs, *s, vals, state),
                ) {
                    (Some(a), Some(f), Some(s)) => Some(f + a * (s - f)),
                    _ => None,
                }
            }
            DF::Cache(a) | DF::BlendD(a) => const_at(dfs, *a, vals, state),
            DF::Interp(a) => const_at(dfs, *a, vals, state)
                .filter(|v| v.is_finite() && !(*v == 0.0 && v.is_sign_negative())),
            _ => None,
        };
        vals[id] = v;
        state[id] = 2;
        v
    }
    let n = dfs.len();
    let mut vals = vec![None; n];
    let mut state = vec![0u8; n];
    let mut folded = 0usize;
    for id in 0..n {
        if let Some(v) = const_at(dfs.as_slice(), id, &mut vals, &mut state) {
            if !matches!(dfs[id], DF::C(_) | DF::Zero) {
                dfs[id] = DF::C(v);
                folded += 1;
            }
        }
    }
    folded
}

/// Bytes `clear()` + `shrink_to_fit()` hands back for a load-only
/// `HashMap<String, usize>`: every key's heap buffer plus the bucket table
/// (`capacity` slots of `(K, V)` plus one control byte each — hashbrown's
/// usable capacity is a little under the bucket count, so this is a slight
/// underestimate).
fn str_map_trim_bytes(m: &mut HashMap<String, usize>) -> usize {
    let heap: usize = m.keys().map(|k| k.capacity()).sum();
    let table = m.capacity() * (std::mem::size_of::<(String, usize)>() + 1);
    m.clear();
    m.shrink_to_fit();
    heap + table
}

/// Bytes a `Vec`'s spare capacity held before `shrink_to_fit`.
fn vec_trim_bytes<T>(v: &mut Vec<T>) -> usize {
    let before = v.capacity();
    v.shrink_to_fit();
    before.saturating_sub(v.capacity()) * std::mem::size_of::<T>()
}

// ---------- vanilla surface (material_rule) ----------

#[derive(Clone, Copy)]
enum YA {
    Abs(i32),
    AB(i32),
    BT(i32),
}
impl YA {
    fn get(&self, min_y: i32, top: i32) -> i32 {
        match *self {
            YA::Abs(v) => v,
            YA::AB(v) => min_y + v,
            YA::BT(v) => top - v,
        }
    }
}

#[derive(Clone, Copy)]
enum Cond {
    False,
    // Bitmask over biome leaf ids: up to 128 distinct leaves are supported (a
    // `minecraft:biome` condition naming more, or one with an out-of-range leaf,
    // simply leaves its bit unset and matches nothing).
    Biome(u128),
    NoiseTh {
        ni: usize,
        is3d: bool,
        lo: f64,
        hi: f64,
    },
    VGrad {
        legacy_seed: u64,
        #[allow(dead_code)]
        lo_w: u64,
        #[allow(dead_code)]
        hi_w: u64,
        sp: Split,
        t: YA,
        f: YA,
    },
    YAbove {
        a: YA,
        mult: i32,
        sd: bool,
    },
    Water {
        off: i32,
        mult: i32,
        sd: bool,
    },
    Steep,
    Temperature,
    Not(usize),
    Hole,
    Prelim,
    StoneD {
        off: i32,
        sdep: bool,
        sec: i32,
        ceil: bool,
    },
}

// Adapted from Horizon's worldgen/biome_temperature.rs (0564ab0), but 1.21.1
// fixes the altitude boundary at 80, not the newer sea_level + 17. Keep its
// PerlinSimplex intermediates in f64 and narrow only where Biome's bytecode does.
#[derive(Clone, Copy)]
struct BiomeWeather {
    temperature: f32,
    frozen: bool,
}
impl BiomeWeather {
    fn parse(j: &J) -> Option<Self> {
        let J::Num(temperature) = j.get("temperature")? else {
            return None;
        };
        let frozen = match j.get("temperature_modifier") {
            None => false,
            Some(J::Str(s)) if s == "none" => false,
            Some(J::Str(s)) if s == "frozen" => true,
            _ => return None,
        };
        Some(Self {
            temperature: *temperature as f32,
            frozen,
        })
    }

    fn at(self, x: i32, y: i32, z: i32) -> f32 {
        static NOISES: std::sync::OnceLock<(OSimp, OSimp, OSimp)> = std::sync::OnceLock::new();
        let mut base = self.temperature;
        if self.frozen || y > 80 {
            let (height, frozen, info) = NOISES.get_or_init(|| {
                (
                    OSimp::new_l(&mut Lcg::new(1234), &[0]),
                    OSimp::new_l(&mut Lcg::new(3456), &[-2, -1, 0]),
                    OSimp::new_l(&mut Lcg::new(2345), &[0]),
                )
            });
            let (dx, dz) = (x as f64, z as f64);
            if self.frozen
                && frozen.sample(dx * 0.05, dz * 0.05) * 7.0 + info.sample(dx * 0.2, dz * 0.2) < 0.3
                && info.sample(dx * 0.09, dz * 0.09) < 0.8
            {
                base = 0.2;
            }
            if y > 80 {
                let variation =
                    (height.sample((x as f32 / 8.0) as f64, (z as f32 / 8.0) as f64) * 8.0) as f32;
                base -= (variation + y as f32 - 80.0) * 0.05 / 40.0;
            }
        }
        base
    }

    fn cold(self, x: i32, y: i32, z: i32) -> bool {
        let warm = self.at(x, y, z) >= 0.15;
        !warm
    }
}

#[derive(Clone)]
enum Rule {
    Seq(Box<[usize]>),
    Cond(usize, usize),
    Block(u32),
    Bandlands,
    OreVein {
        density: usize,
        rich: usize,
        gap: usize,
        ore: u32,
        raw: u32,
        fill: u32,
        rawc: f32,
        ymin: i32,
        ymax: i32,
    },
}

// per-column surface-rule result: dense y->state map over [lo..lo+len)
struct SurfMat {
    lo: i16,
    vals: Box<[u16]>, // u16::MAX = no override -> caller uses default
}

/// One terrain cell as the surface scan sees it before the rule runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceCell {
    Air,
    Fluid,
    Solid,
}

/// Classify one pre-surface cell. Vanilla scans the chunk's actual block states;
/// an aquifer barrier (`None`) is solid, and a non-fluid aquifer state is solid
/// even when the density is non-positive. Such cells can still receive a
/// surface material and must stay in the surface window.
/// The four-block material window below the actual pre-surface top. A surface
/// material may be applied to an aquifer/default solid above the density top,
/// but not to deep solids far below the saved heightmap.
fn surface_material_applies(y: i32, surface_top: i32) -> bool {
    y >= surface_top.saturating_sub(4)
}

fn classify_surface_cell(
    density: f64,
    aquifer: Option<u32>,
    aquifer_air: bool,
    aquifer_fluid: bool,
) -> SurfaceCell {
    if density > 0.0 || aquifer.is_none() {
        SurfaceCell::Solid
    } else if aquifer_air {
        SurfaceCell::Air
    } else if aquifer_fluid {
        SurfaceCell::Fluid
    } else {
        SurfaceCell::Solid
    }
}

// scan context for material rules (vanilla MaterialRuleContext)
struct SC {
    x: i32,
    y: i32,
    z: i32,
    run_depth: i32,
    sec: f32,
    sec_ok: bool,
    steep: Option<bool>,
    sda: i32,
    sdb: i32,
    fluid: i32,
    est_min: i32,
    biome: Option<((i32, i32, i32), u16)>,
    nt: Vec<u8>, // per-column memo for 2d noise_threshold (0=unset, 1=false, 2=true)
    memo: HashMap<(usize, i32, i32, i32), f64>,
}
impl SC {
    fn new() -> SC {
        SC {
            x: 0,
            y: 0,
            z: 0,
            run_depth: 0,
            sec: 0.0,
            sec_ok: false,
            steep: None,
            sda: 0,
            sdb: 0,
            fluid: i32::MIN,
            est_min: i32::MAX,
            biome: None,
            nt: Vec::new(),
            memo: HashMap::new(),
        }
    }
    #[inline]
    fn clear_nt(&mut self) {
        self.nt.fill(0);
    }
}

// bound on the shared quart -> biome cache
const BIO_CAP: usize = 1 << 18;

/// One router entry: either a resource name or an inline density-function object.
fn router_df(g: &mut Gen, files: &HashMap<String, Vec<u8>>, j: &J) -> Option<usize> {
    let name = j.str();
    if !name.is_empty() {
        if let Some(i) = g.resolve_ref(files, name) {
            return Some(i);
        }
    }
    g.node(j, files)
}

/// The three vein functions plus the six block states `OreVeinSampler` uses.
#[derive(Clone, Copy)]
struct VeinCfg {
    toggle: usize,
    ridged: usize,
    gap: usize,
    copper_ore: u32,
    copper_raw: u32,
    copper_fill: u32,
    iron_ore: u32,
    iron_raw: u32,
    iron_fill: u32,
}

#[derive(Clone, Copy)]
enum InterpKind {
    General(usize),
    YNoise {
        y_grad: Option<(f64, f64, f64, f64, f64, f64, f64)>,
        ni: usize,
        xzs: f64,
        ys: f64,
        mul: Option<(f64, bool)>,
        add: Option<(f64, bool)>,
    },
}

fn classify_interp_inner(dfs: &[DF], inner: usize) -> InterpKind {
    let strip_cache = |mut id: usize| -> usize {
        while let Some(DF::Cache(a)) = dfs.get(id) {
            id = *a;
        }
        id
    };
    let const_val = |id: usize| -> Option<f64> {
        match dfs.get(strip_cache(id))? {
            DF::C(v) => Some(*v),
            DF::Zero => Some(0.0),
            _ => None,
        }
    };
    type AffineMatch = (usize, f64, f64, Option<(f64, bool)>, Option<(f64, bool)>);
    let match_affine_noise = |id: usize| -> Option<AffineMatch> {
        let mut cur = strip_cache(id);
        let mut add = None;
        if let DF::Add(a, b) = dfs.get(cur)? {
            if let Some(c) = const_val(*a) {
                add = Some((c, true));
                cur = strip_cache(*b);
            } else if let Some(c) = const_val(*b) {
                add = Some((c, false));
                cur = strip_cache(*a);
            }
        }
        let mut mul = None;
        if let DF::Mul(a, b) = dfs.get(cur)? {
            if let Some(c) = const_val(*a) {
                mul = Some((c, true));
                cur = strip_cache(*b);
            } else if let Some(c) = const_val(*b) {
                mul = Some((c, false));
                cur = strip_cache(*a);
            }
        }
        if let DF::Noise {
            ni,
            sx,
            sy,
            sz,
            xzs,
            ys,
        } = dfs.get(cur)?
        {
            if const_val(*sx) == Some(0.0)
                && const_val(*sy) == Some(0.0)
                && const_val(*sz) == Some(0.0)
            {
                return Some((*ni, *xzs, *ys, mul, add));
            }
        }
        None
    };
    let root = strip_cache(inner);
    if let Some((ni, xzs, ys, mul, add)) = match_affine_noise(root) {
        return InterpKind::YNoise {
            y_grad: None,
            ni,
            xzs,
            ys,
            mul,
            add,
        };
    }
    if let Some(DF::RangeChoice { i, mi, ma, wr, wo }) = dfs.get(root) {
        if let (Some(DF::Grad { fc, fv, tc, tv }), Some(out_val)) =
            (dfs.get(strip_cache(*i)), const_val(*wo))
        {
            if let Some((ni, xzs, ys, mul, add)) = match_affine_noise(*wr) {
                return InterpKind::YNoise {
                    y_grad: Some((*fc, *fv, *tc, *tv, *mi, *ma, out_val)),
                    ni,
                    xzs,
                    ys,
                    mul,
                    add,
                };
            }
        }
    }
    InterpKind::General(inner)
}

/// `(grid slot, inner)` for every `InterpKind::General` entry: the roots
/// `DfCols::compile` lowers into the corner-grid column programs. `YNoise`
/// grids belong to the specialised `fill_ynoise_grid` path and are not roots.
fn general_roots(nodes: &[(usize, InterpKind)]) -> Vec<(usize, usize)> {
    nodes
        .iter()
        .filter_map(|&(iid, kind)| match kind {
            InterpKind::General(inner) => Some((iid, inner)),
            InterpKind::YNoise { .. } => None,
        })
        .collect()
}

fn detect_upper_bound_interp(dfs: &[DF], final_density: usize) -> Option<usize> {
    let base = match dfs.get(final_density)? {
        DF::Add(a, b) if matches!(dfs.get(*b), Some(DF::Beardifier)) => *a,
        _ => return None,
    };
    let check_squeeze = |id: usize| -> Option<usize> {
        let mut cur = id;
        while let Some(DF::Cache(inner)) = dfs.get(cur) {
            cur = *inner;
        }
        let sq_arg = match dfs.get(cur)? {
            DF::Squeeze(a) => *a,
            _ => return None,
        };
        let mut inner = sq_arg;
        while let Some(DF::Cache(a)) = dfs.get(inner) {
            inner = *a;
        }
        let (a, b) = match dfs.get(inner)? {
            DF::Mul(a, b) => (*a, *b),
            _ => return None,
        };
        let is_pos_c =
            |n: usize| matches!(dfs.get(n), Some(DF::C(v)) if *v > 0.0 && v.is_finite());
        let interp_id = if is_pos_c(a) {
            b
        } else if is_pos_c(b) {
            a
        } else {
            return None;
        };
        if matches!(dfs.get(interp_id), Some(DF::Interp(_))) {
            Some(interp_id)
        } else {
            None
        }
    };
    if let Some(id) = check_squeeze(base) {
        return Some(id);
    }
    if let DF::Min(a, b) = dfs.get(base)? {
        if let Some(id) = check_squeeze(*a) {
            return Some(id);
        }
        if let Some(id) = check_squeeze(*b) {
            return Some(id);
        }
    }
    None
}

fn beard_max_y(beard: Option<&BeardChunkData>) -> i32 {
    let Some(b) = beard else {
        return i32::MIN;
    };
    if b.is_empty() {
        return i32::MIN;
    }
    let mut hi = i32::MIN;
    for r in &b.rigids {
        let top = r.max[1].max(r.min[1] + r.ground_level_delta) + 12;
        if top > hi {
            hi = top;
        }
    }
    for j in &b.junctions {
        let top = j.source_ground_y + 12;
        if top > hi {
            hi = top;
        }
    }
    hi
}

#[derive(Clone, Copy)]
enum DeepStep {
    Block(u32),
    VGrad {
        ta: i32,
        fa: i32,
        legacy_seed: u64,
        sp: Split,
        st: u32,
    },
}

fn compile_deep_fast(
    rules: &[Rule],
    conds: &[Cond],
    root: usize,
    min_y: i32,
    height: i32,
) -> Option<Vec<DeepStep>> {
    let top = min_y + height - 1;
    let mut steps = Vec::new();
    fn visit(
        rid: usize,
        rules: &[Rule],
        conds: &[Cond],
        min_y: i32,
        top: i32,
        out: &mut Vec<DeepStep>,
    ) -> bool {
        match rules.get(rid) {
            Some(Rule::Block(st)) => {
                out.push(DeepStep::Block(*st));
                true
            }
            Some(Rule::Seq(seq)) => seq.iter().all(|&sub| visit(sub, rules, conds, min_y, top, out)),
            Some(Rule::Cond(ci, ri)) => match conds.get(*ci) {
                Some(Cond::False | Cond::Prelim) => true,
                Some(Cond::VGrad {
                    legacy_seed,
                    sp,
                    t,
                    f,
                    ..
                }) => {
                    if let Some(Rule::Block(st)) = rules.get(*ri) {
                        out.push(DeepStep::VGrad {
                            ta: t.get(min_y, top),
                            fa: f.get(min_y, top),
                            legacy_seed: *legacy_seed,
                            sp: *sp,
                            st: *st,
                        });
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            },
            _ => false,
        }
    }
    if visit(root, rules, conds, min_y, top, &mut steps) {
        Some(steps)
    } else {
        None
    }
}

/// The immutable half of the generator: everything read from the datapack and the
/// noise settings at `init`, plus the resolved block states and the seed.
///
/// `Clone` exists only so `Arc::make_mut` keeps the construction-time writes
/// working; nothing mutates it after `load_preset` returns. A worker gets an
/// `Arc<GenData>`/`&GenData`, which is the point of the split in
/// `specs/chunk-parallelism`: the read-only inputs need no world lock.
#[derive(Clone)]
pub struct GenData {
    pub(crate) preset: NoisePreset,
    dfs: Vec<DF>,
    names: HashMap<String, usize>,
    noises: Vec<Normal>,
    noise_names: HashMap<String, usize>,
    factory: Split,
    base: String,
    pub final_density: usize,
    // router ids: [temperature, vegetation, continentalness, erosion, depth, ridges]
    climate: Option<[usize; 6]>,
    // vanilla depth = grad(y) + offset(x,z) — (fc,fv,tc,tv, offset_df)
    depth_split: Option<(f64, f64, f64, f64, usize)>,
    // 1.21.1 ore veins: `ore_veins_enabled` plus the router's vein functions
    vein: Option<VeinCfg>,
    /// One `bool` per node id: is this density function a pure function of `(x, z)`?
    /// Computed once when the id table is final (post-order over the DAG). The corner-grid
    /// fill memoizes every such node per `(x, z)` instead of re-evaluating it at all `ny`
    /// vertical levels, which removes ~49x redundant work on the y-independent spline /
    /// continentalness / erosion subtrees (Layer A2-a).
    is_2d: Vec<bool>,
    /// Density-function nodes [`fold_constant_dfs`] rewrote to `DF::C` during
    /// load (0 for the unfolded test loader). Diagnostics only.
    df_folded: usize,
    /// Capacity-based estimate of the bytes the post-load trim handed back to
    /// the allocator (cleared load-only maps plus every finalized Vec's spare
    /// capacity). Diagnostics only.
    ram_trimmed: usize,
    biome_zoom: i64,
    btree: Option<BT>,
    leaf_ps: Vec<[[i32; 2]; 7]>,
    biome_names: Vec<String>,
    biome_ids: Vec<i64>,
    biome_weather: Vec<BiomeWeather>,
    pub sea: i32,
    /// The **generator** window from `worldgen/noise_settings/<preset>.json`'s `noise`
    /// object. Terrain, density, aquifers, carvers, surface rules and the biome quart
    /// sweep are defined over exactly this range.
    pub min_y: i32,
    pub height: i32,
    /// The **world** window from `dimension_type/<dimension>.json`. Chunk sections,
    /// heightmaps, the offline dump length and structure/furniture bounds are defined
    /// over this range. 1.21.1 makes them different: the Nether and the End generate
    /// 0..127 but store 0..255 (see `dim_height_256_design.md`). It equals
    /// `min_y`/`height` whenever the pack has no usable `dimension_type` object, so a
    /// fixture that predates this split keeps its exact meaning.
    pub world_min_y: i32,
    pub world_height: i32,
    // NoiseSettings.getCellHeight() = size_vertical * 4
    noise_cell_h: i32,
    noise_cell_w: i32,
    legacy_random_source: bool,
    rules: Vec<Rule>,
    conds: Vec<Cond>,
    rule_ids: HashMap<String, usize>,
    cond_ids: HashMap<String, usize>,
    bleaf: HashMap<String, u16>,
    surface_rule: Option<usize>,
    deep_fast: Option<Vec<DeepStep>>,
    prelim_df: Option<usize>,
    surf_ni: Option<usize>,
    sec_ni: Option<usize>,
    band_off_ni: Option<usize>,
    bands: Option<Box<[u32; 192]>>,
    ore_split: Option<Split>,
    pub def_st: u32,
    pub def_fl: u32,
    // resolved once from the caller's state lookup; a generation module must not
    // reach the crate root for a block state (see tools/layers_check.sh).
    bedrock_st: u32,
    // aquifer (vanilla NoiseBasedAquifer): DF ids + fluid states + caches
    aq: Option<Aqd>,
    world_seed: u64,
    /// Interpolated nodes and their inputs, in the corner-grid evaluation order.
    interp_nodes: Vec<(usize, usize)>,
    density_interp_nodes: Vec<(usize, InterpKind)>,
    vein_interp_nodes: Vec<(usize, InterpKind)>,
    upper_bound_interp: Option<usize>,
    fallback_share: std::sync::Arc<TerrainShare>,
    /// `final_density` compiled once into a flat post-order program (see
    /// `DfProg`). `None` keeps the interpreted evaluator (`MEGU_DF_INTERP`).
    dfprog: Option<Box<DfProg>>,
    /// The `interpolated` inputs of the density path, compiled for the
    /// corner-grid column sweep (`DfCols`): a hoisted 2D prefix per `(x, z)`
    /// column plus a 3D program per grid. `None` keeps `eval_flat`
    /// (`MEGU_DF_INTERP` or `MEGU_DFCOL=0`).
    dfcols: Option<Box<DfCols>>,
    /// The same for the vein-path `interpolated` inputs (`vein_interp_nodes`).
    vfcols: Option<Box<DfCols>>,
}
struct AqCell12 {
    valid: bool,
    cx: i32,
    cz: i32,
    sx: i32,
    sy: i32,
    sz: i32,
    slot: usize,
    px: [i32; 12],
    py: [i32; 12],
    pz: [i32; 12],
    idx: [usize; 12],
}

impl AqCell12 {
    fn empty() -> Self {
        Self {
            valid: false,
            cx: 0,
            cz: 0,
            sx: 0,
            sy: 0,
            sz: 0,
            slot: 0,
            px: [0; 12],
            py: [0; 12],
            pz: [0; 12],
            idx: [0; 12],
        }
    }
}

struct BaseColLazy {
    xz: (i32, i32),
    cc: ChunkCols,
    memos: Box<[FlatCornerMemo; 4]>,
    ub_filled: Vec<bool>,
    other_filled: Vec<bool>,
    corners: [(usize, usize, i32, i32); 4],
    n_corners: usize,
    xi: usize,
    zi: usize,
    fx_zero: bool,
    fz_zero: bool,
}

type ShapeMarkMap = HashMap<(i32, i32), Vec<(i32, i32, i32)>>;
type ShapeMarkBuckets = Vec<((i32, i32), Vec<(i32, i32, i32)>)>;
type FlatMemo = HashMap<(usize, i32, i32, i32), f64>;

pub struct Gen {
    /// The immutable inputs. `Deref` keeps every existing field name working;
    /// `DerefMut` exists only for construction (see below).
    pub(crate) data: std::sync::Arc<GenData>,
    // scratch memo for the vein sampler's density evaluations
    memo: FlatMemo,
    corner_memo: Option<FlatCornerMemo>,
    /// Scratch shared by `DfCols` evaluations (stack + hoisted 2D slots), taken
    /// by a corner-grid fill and put back when it ends so the next chunk reuses
    /// the same buffers.
    dfcol_scratch: Option<DfColScratch>,
    /// Chunk-local aquifer grids (see [`AqChunkCache`]). A small MRU ring rather than a
    /// single slot: carvers and features read neighbour chunks while a chunk is being
    /// filled, and a one-slot cache would recompute the same 12-cell neighbourhood.
    aq_chunk: [Option<AqChunkCache>; AQ_CHUNK_SLOTS],
    aq_chunk_use: [u64; AQ_CHUNK_SLOTS],
    aq_chunk_tick: u64,
    aq_cell12: AqCell12,
    prelim_c: HashMap<(i32, i32), i32>,
    /// per chunk: the distinct biomes of its section palettes, i.e. what vanilla's
    /// `applyBiomeDecoration` unions over the 3x3 before deciding which features run
    chunk_biomes: HashMap<(i32, i32), std::sync::Arc<Vec<String>>>,
    chunk_biomes_ord: std::collections::VecDeque<(i32, i32)>,
    prelim_ord: std::collections::VecDeque<(i32, i32)>,
    clim_c: HashMap<(i32, i32), ([i64; 5], f64)>,
    clim_ord: std::collections::VecDeque<(i32, i32)>,
    // Paletted biome IDs by quart position for surface rules.
    bioq: HashMap<(i32, i32, i32), u16>,
    bioq_ord: std::collections::VecDeque<(i32, i32, i32)>,
    // Cached `biome_quart(qx, qy, qz, None)` answers (pure when `last` is None).
    bioq_none: HashMap<(i32, i32, i32), u16>,
    bioq_none_ord: std::collections::VecDeque<(i32, i32, i32)>,
    // Lazy single-column base heightmap cache `(x, z) -> (surf, wtop_or_i32_min)` plus
    // reusable scratch buffers so `top_ws`/`top_oc` during beardifier resolution never
    // have to compute a full 25-column × 49-level chunk grid for a single column query.
    base_hm_c: HashMap<(i32, i32), (i32, i32)>,
    base_hm_cc: Option<Box<ChunkCols>>,
    base_hm_memos: Option<Box<[FlatCornerMemo; 4]>>,
    base_col_lazy: Option<Box<BaseColLazy>>,
    col_c: HashMap<(i32, i32), std::sync::Arc<ChunkCols>>,
    col_ord: std::collections::VecDeque<(i32, i32)>,
    /// The parallel terrain round this generator belongs to, if any: selects that round's
    /// shared structure-start cache instead of the thread-local one. `Gen` cannot name the
    /// cache's type (it lives above this layer), so the round is a number here; see
    /// `feature::starts_round_begin`.
    pub(crate) starts_round: Option<u64>,
    /// Memoized [`Gen::round_share`] answer for the current round. The lookup takes the
    /// process-wide `terrain_shares` mutex and this generator calls it per `cols` /
    /// `base_cols` / `resolve_beard` query, so at `jobs=8` it is the one lock every
    /// worker's column/beard path contends on. The slot is keyed by the round id:
    /// `starts_round` can be reassigned on a live generator (a new round, or `None`),
    /// and a stale entry must never answer for a different round.
    share_slot: Option<(u64, Option<std::sync::Arc<TerrainShare>>)>,
    /// `ChunkGenerator.getBaseColumn` columns: the noise-only answer a structure's
    /// `findGenerationPoint` scans. Keyed like `col_c` but *not* interchangeable with
    /// it: a base column has no beardifier and no surface rule, so it is pure in
    /// (seed, preset, chunk) and may be cached even while a beard is resolving.
    base_c: HashMap<(i32, i32), std::sync::Arc<ChunkCols>>,
    base_ord: std::collections::VecDeque<(i32, i32)>,
    /// Per-chunk beardifier input, keyed like `col_c`. Pure in (seed, cx, cz): the
    /// resolver reads structure starts, which are functions of the seed only.
    beard_c: HashMap<(i32, i32), std::sync::Arc<BeardChunkData>>,
    beard_ord: std::collections::VecDeque<(i32, i32)>,
    /// Per-generator resolver override (tests); the world registers a global one.
    beard_resolver: Option<BeardResolver>,
    /// Non-zero while a beardifier resolution is in progress: a nested `cols` (a
    /// structure's heightmap query) must not resolve again, or it would describe
    /// terrain shaped by the structure it is trying to place.
    beard_depth: u32,
    /// Base (feature-free) section rows a worker already built, keyed like a section
    /// `(cx, sy, cz)` and consumed by `terrain_section`, which then applies the feature
    /// layer serially. This is the landing site for cross-chunk parallel terrain
    /// (`specs/chunk-parallelism`, lever 2): the rows are a pure function of the
    /// immutable data and the seed, so a scratch generator produces the same array the
    /// world generator would. Bounded by `TERRAIN_STORE_BOUND`.
    terr_c: HashMap<(i32, i32, i32), std::sync::Arc<[u32; 4096]>>,
    terr_ord: std::collections::VecDeque<(i32, i32, i32)>,
    /// Sections installed into [`Gen::terr_c`] and not yet consumed by `terrain_section_at`,
    /// for the M3 P1a M4 counter only. Lives on the generator (not in a process-wide set)
    /// so "was this row evicted before its consumer read it" is a property of one store:
    /// the answer is what the `TERRAIN_STORE_BOUND` A/B is asking. Untouched unless
    /// `MEGU_M3_PROFILE`/`FEAT_PROFILE` is on.
    terr_pending: std::collections::BTreeSet<(i32, i32, i32)>,
    // feature layer: chunk -> {(y-min)<<8|z<<4|x -> state}
    feat_c: HashMap<(i32, i32), HashMap<u32, u32>>,
    /// Container contents staged by template placement during feature population.
    /// Generation-owned: they are moved into the runtime container store as soon as
    /// this generator's lock is released (see `block_state`), never kept as a second
    /// live store.
    staged_payloads: Vec<((i32, i32, i32), crate::generation_payload::GenerationPayload)>,
    feat_ord: std::collections::VecDeque<(i32, i32)>,
    deep_c: HashMap<(i32, i32, i32), u32>,
    deep_ord: std::collections::VecDeque<(i32, i32, i32)>,
    pub(crate) pop_done: HashSet<(i32, i32)>,
    /// The window a recorded population plan covers (`None` outside a planned run): the
    /// chunks the input decorates. Native decorated three rings around the request and the
    /// plan keeps one (`parse_population_order`), so a chunk outside this box is one native
    /// decorated and this model did not — see [`Gen::promotion_window_ready`].
    pop_plan_window: Option<(i32, i32, i32, i32)>,
    /// `ChunkAccess.postProcessing` seen across chunks: the positions the mark sources of
    /// 1.21.1 filed, keyed by the chunk that owns them and kept in mark order. A chunk's
    /// bucket is consumed by `LevelChunk.postProcessGeneration` once the chunk is promoted,
    /// which native only allows after the whole 3x3 around it is `FULL`.
    ///
    /// The owner is `getChunk(pos)`, not the chunk being generated: a carver paints up to
    /// eight chunks away, and its mark follows the position.
    shape_marks: ShapeMarkMap,
    /// Freed [`Gen::shape_marks`] buckets kept for reuse. A promoted chunk's bucket is
    /// consumed once; parking it here turns the next `store_shape_mark` into a move
    /// instead of a fresh allocation. Bounded small — a bucket is a few marks.
    mark_pool: Vec<Vec<(i32, i32, i32)>>,
    /// Which writer filed each mark, for `MARK_TRACE` only. Kept apart from
    /// [`Gen::shape_marks`] because the buckets are the answer and this is the audit of it;
    /// a position filed twice keeps its last writer.
    mark_src: HashMap<(i32, i32, i32), MarkSrc>,
    /// `Aquifer.shouldScheduleFluidUpdate()` as of the last `computeSubstance` call on this
    /// generator, i.e. the port's `NoiseBasedAquifer.shouldScheduleFluidUpdate`. Vanilla keeps
    /// it as a mutable field on the aquifer object and three mark sites read it right after the
    /// call that filled it (see [`Gen::aq_schedule_fluid_update`]); it is a field and not a
    /// return value for the same reason there — `computeSubstance` has one return channel, the
    /// substance, and the flag is a separate observable of the same call.
    aq_sched: bool,
    /// How many aquifer calls this generator has answered. `build_cols_body` snapshots it so a
    /// column shared through a [`TerrainShare`] knows whether the build that produced it wrote
    /// `aq_sched` at all: a build whose surface scan never reached below sea level leaves the
    /// flag the *caller's* history wrote, which is each generator's own and must not travel.
    aq_calls: u64,
    /// Sections whose terrain-stage marks are already in [`Gen::shape_marks`]. Keyed by
    /// section because that is the granularity `base_section_rows` decides and
    /// `terrain_section` can be asked for twice.
    terrain_marked: HashSet<(i32, i32, i32)>,
    /// The chunk whose FEATURES pass is running (`None` outside one): the centre of the
    /// step's `blockStateWriteRadius` window. See `feature_write_allowed`.
    pub(crate) pop_active: Option<(i32, i32)>,
    // Sources reconstructed after loading must recreate terrain but not enqueue
    // the generation ticks a second time.
    restored_tick_sources: HashSet<(i32, i32)>,
    pub(crate) suppress_generated_ticks: bool,
    /// `ChunkAccess`'s carve stage: one record per chunk, holding the latch `begin_carve`
    /// reads and both outputs the stage writes (see `CarveStage`). **Nothing else may
    /// hold a carve claim or a post-carve WG value** — three containers with three
    /// lifetimes let a claim outlive the output it stood for.
    carve_c: HashMap<(i32, i32), CarveStage>,
    /// The `carve_c` records that carry a WG value, oldest first: the retention bound
    /// lives in exactly one place (`set_wg_after_carve`).
    carve_ord: std::collections::VecDeque<(i32, i32)>,
    /// Generation metadata persists independently of evictable block caches.
    pub(crate) mineshaft_spawners: HashSet<(i32, i32, usize)>,
    pub(crate) payload_sources: HashSet<(i32, i32)>,
    /// Decoration region whose lexicographic scan order to follow (`MEGU_POP_REGION`).
    pop_region: Option<(i32, i32, i32, i32)>,
    /// A3-a dispatch hand-off. `populate_in_order` records the pending chunks here when a
    /// multi-chunk feature run can be parallelised; the first `feature::populate` call takes
    /// the batch and drives it. `worldgen` cannot import `feature` (layer policy), so the
    /// higher layer pulls the work instead of being called from here.
    pub(crate) a3a_dispatch: Option<Vec<(i32, i32)>>,
    // per column: (highest non-air + 1, highest solid + 1) written by the feature layer
    feat_hm: HashMap<(i32, i32), (i32, i32)>,
    // Each generator owns its pending ticks; only the active world's handles are published.
    generated_ticks: GeneratedTickQueues,
    /// The most precise `(id, reason)` a failed surface-rule parse recorded on
    /// the way out, committed to [`WorldgenParseReport`] only when the load
    /// actually gives up. Parser probes that never reach the load site leave it
    /// set but unreported.
    rule_diag: Option<(String, String)>,
}

/// Generation-stamped, zero-allocation scratch memo for the corner-grid fill.
///
/// `val_2d[id]` holds the value of a y-independent node for the current `(x, z)`;
/// `val_3d[id]` holds the value of a `Cache` node for the current `(x, y, z)`. The
/// generation counters are bumped once per `(x, z)` and once per `(x, y, z)`
/// respectively, so a stale value is rejected by one integer compare instead of a
/// SipHash lookup into a `HashMap<(usize, i32, i32, i32), f64>`.
struct FlatCornerMemo {
    val_2d: Vec<f64>,
    gen_2d: Vec<u32>,
    cur_2d: u32,
    val_3d: Vec<f64>,
    gen_3d: Vec<u32>,
    cur_3d: u32,
}

impl FlatCornerMemo {
    fn new(n: usize) -> Self {
        Self {
            val_2d: vec![0.0; n],
            gen_2d: vec![0; n],
            cur_2d: 0,
            val_3d: vec![0.0; n],
            gen_3d: vec![0; n],
            cur_3d: 0,
        }
    }
    #[inline]
    fn next_2d(&mut self) {
        self.cur_2d = self.cur_2d.wrapping_add(1);
        if self.cur_2d == 0 {
            self.gen_2d.fill(0);
            self.cur_2d = 1;
        }
    }
    #[inline]
    fn next_3d(&mut self) {
        self.cur_3d = self.cur_3d.wrapping_add(1);
        if self.cur_3d == 0 {
            self.gen_3d.fill(0);
            self.cur_3d = 1;
        }
    }
}

fn wg_count() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WG_COUNT").is_some())
}

impl std::ops::Deref for Gen {
    type Target = GenData;
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl std::ops::DerefMut for Gen {
    /// Construction-time only: `load_preset` fills the immutable fields after the
    /// literal. `Arc::make_mut` clones when a snapshot is shared, so a write after
    /// the generator is published would be a silent copy rather than an error —
    /// keep mutation out of this path.
    fn deref_mut(&mut self) -> &mut Self::Target {
        std::sync::Arc::make_mut(&mut self.data)
    }
}

#[derive(Clone)]
struct Aqd {
    barrier: usize,
    flood: usize,
    spread: usize,
    lava: usize,
    split: Split,
    water: u32,
    lava_st: u32,
}

/// Chunks whose post-carve WG heightmaps stay resident. A value is a deterministic scan
/// of the carved terrain, so re-running the carver pass reproduces it, but it is *not*
/// re-readable from a dropped block bucket: that is why this bound belongs to the record
/// and not to `feat_c`. Approved retention for the first chunk-state repair — do not
/// shrink it to the feature bucket's bound.
const CARVE_WG_BOUND: usize = 4096;

/// Chunks whose FEATURES-layer bucket (`feat_c` + `pop_done`) stays resident. Unchanged
/// by the chunk-state repair: it is a cache of block writes, not of stage identity.
const FEATURE_BUCKET_BOUND: usize = 384;

/// Base terrain sections (`terr_c`) retained for chunks a worker has already filled:
/// 512 x 16 KiB = 8 MiB. One prefetched group is at most `2 * jobs` chunks (24 sections
/// each), so the store does not have to evict a row its consumer is about to read; the
/// bound exists so a caller that prefetches without consuming cannot grow it.
///
/// The A/B switch is `MEGU_TERRAIN_STORE_BOUND` (see [`terrain_store_bound`]): retention
/// is the only thing the bound decides, so the two arms must produce the same bytes and the
/// same `terr_c` hit/miss counts for every section a caller actually asks for.
const TERRAIN_STORE_BOUND: usize = 512;

/// The retention bound in force, `MEGU_TERRAIN_STORE_BOUND` or [`TERRAIN_STORE_BOUND`].
pub fn terrain_store_bound() -> usize {
    static BOUND: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BOUND.get_or_init(|| parse_terrain_store_bound(env_str!("MEGU_TERRAIN_STORE_BOUND")))
}

/// [`terrain_store_bound`] without the environment, so the parse rule is testable in a
/// process that has already cached the answer.
pub(crate) fn parse_terrain_store_bound(value: Option<&str>) -> usize {
    value
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(TERRAIN_STORE_BOUND)
}

// ---------- M3 Slice P1b: round-scoped single-flight caches ----------
//
// `specs/chunk-parallelism` P1b. P1a measured the shape of the `full` CPU penalty: it grows
// with the worker count (+63% at five, +122% at eight) and all of it is in the structure
// layer, counted by M1 - `chunk_starts` 281 serial against 454-462 at five workers and
// 376-571 at eight, `cols built` 481 against 650-657 and 565-760, while a prefetch worker's
// own `col_c` answered **zero** hits at every count. A round's workers share one structure
// cache, but each of them computed a chunk's starts *before* taking the lock and only then
// re-checked for an existing entry, so two workers could compute the same admission scan;
// and every column a worker built nested inside a beard resolution was private to that
// worker's `Gen`.
//
// The two answers here are the two halves of that measurement:
//
// * [`FlightCache`] - one thread computes a key, everybody else waits for the publication.
//   Nobody computes a value twice while it is resident, and a key that is *being* computed
//   is not computed a second time either. The single-flight discipline is what a
//   compute-then-insert cache cannot give: it only deduplicates after the fact.
// * [`TerrainShare`] - the three bounded caches a round shares between its workers and its
//   owner (`bases`, `beards`, `cols`), keyed the way `Gen` keys its own.
//
// Both are **round-scoped**, never process-wide: a cache of starts, beards or columns is a
// function of the round's generator, and sharing one across rounds is how the Nether seed
// `-1` fixture broke (`(-994,89,538)`: 19460 cells against 5849). The round opens them
// (`terrain_share_begin`, driven by `feature::starts_round_begin`) and drops them when it
// ends.

/// Where a round's shared value lives: still being computed, or published.
pub(crate) enum RoundSlot<T> {
    /// One thread is computing the value; the flight is where it publishes it.
    InFlight(std::sync::Arc<RoundFlight<T>>),
    /// Published. Lookups take the value and never touch the flight.
    Ready(T),
}

enum FlightOutcome<T> {
    Pending,
    Done(T),
    /// The leader returned without publishing (its build panicked, or produced nothing to
    /// publish). A waiter computes inline instead of waiting for a value that will not come.
    Abandoned,
}

/// One key's publication channel.
pub(crate) struct RoundFlight<T> {
    outcome: std::sync::Mutex<FlightOutcome<T>>,
    cv: std::sync::Condvar,
}

impl<T: Clone> RoundFlight<T> {
    fn new() -> Self {
        RoundFlight {
            outcome: std::sync::Mutex::new(FlightOutcome::Pending),
            cv: std::sync::Condvar::new(),
        }
    }

    /// The published value, if it is already there. Never waits.
    fn value(&self) -> Option<T> {
        match &*self.outcome.lock().unwrap_or_else(|e| e.into_inner()) {
            FlightOutcome::Done(v) => Some(v.clone()),
            FlightOutcome::Pending | FlightOutcome::Abandoned => None,
        }
    }

    /// Block until the leader publishes, or until it abandons the flight.
    fn wait(&self) -> Option<T> {
        let mut outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match &*outcome {
                FlightOutcome::Pending => {
                    outcome = self.cv.wait(outcome).unwrap_or_else(|e| e.into_inner());
                }
                FlightOutcome::Done(v) => return Some(v.clone()),
                FlightOutcome::Abandoned => return None,
            }
        }
    }

    fn publish(&self, value: T) {
        let mut outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*outcome, FlightOutcome::Pending) {
            *outcome = FlightOutcome::Done(value);
        }
        drop(outcome);
        self.cv.notify_all();
    }

    fn abandon(&self) {
        let mut outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*outcome, FlightOutcome::Pending) {
            *outcome = FlightOutcome::Abandoned;
        }
        drop(outcome);
        self.cv.notify_all();
    }
}

thread_local! {
    /// Keys whose flight **this thread** is leading, innermost last. A structure resolution
    /// can ask for a key the same thread is already computing (a heightmap query inside the
    /// cache's own build); claiming that flight again would deadlock on the publication only
    /// this thread can make, so the re-entry computes inline instead.
    static FLIGHT_STACK: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn flight_stack_contains(token: u64) -> bool {
    FLIGHT_STACK.with(|s| s.borrow().contains(&token))
}

/// Identity of one flight, for the re-entry check. Two different keys may collide (the hash
/// is 64 bits and the stack is a handful deep): a collision only makes the re-entrant call
/// compute inline, which is always safe, and never suppresses a publication.
pub(crate) fn flight_token<K: std::hash::Hash>(domain: u64, key: &K) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    domain.hash(&mut h);
    key.hash(&mut h);
    h.finish()
}

struct FlightToken(u64);

impl FlightToken {
    fn push(token: u64) -> Self {
        FLIGHT_STACK.with(|s| s.borrow_mut().push(token));
        FlightToken(token)
    }
}

impl Drop for FlightToken {
    fn drop(&mut self) {
        FLIGHT_STACK.with(|s| {
            let mut stack = s.borrow_mut();
            if let Some(pos) = stack.iter().rposition(|t| *t == self.0) {
                stack.remove(pos);
            }
        });
    }
}

/// A bounded cache with single-flight fills.
///
/// Reads are one mutex acquisition. A miss makes this thread the key's leader and runs the
/// build **without the lock** (a column build resolves structures, which would otherwise
/// serialise the whole round); every other asker waits on that flight's condvar, also
/// without the cache's lock held.
pub(crate) struct FlightCache<K, V> {
    /// FIFO retention over published keys, or `None` for a cache that lives as long as its
    /// round. Same eviction discipline as `Gen`'s own caches so the two cannot disagree about
    /// what "resident" means.
    bound: Option<usize>,
    inner: Mutex<FlightCacheInner<K, V>>,
    /// Test probe: leaders that actually built a value, and askers that took one without
    /// building. A single-flight claim is exactly the difference between the two, and it is
    /// read by `m3_p1b_single_flight_deduplicates_chunk_starts_and_cols`.
    #[cfg(test)]
    built: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    shared: std::sync::atomic::AtomicU64,
}

struct FlightCacheInner<K, V> {
    map: HashMap<K, RoundSlot<V>>,
    /// Published keys, oldest first. An in-flight key is deliberately absent: a leader is
    /// never evicted out from under the waiters that already hold its flight.
    ord: std::collections::VecDeque<K>,
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> FlightCache<K, V> {
    pub(crate) fn new(bound: Option<usize>) -> Self {
        FlightCache {
            bound,
            inner: Mutex::new(FlightCacheInner {
                map: HashMap::new(),
                ord: std::collections::VecDeque::new(),
            }),
            #[cfg(test)]
            built: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            shared: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Test probe: `(values this cache built, values it served from a flight or a slot)`.
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn flight_counts(&self) -> (u64, u64) {
        (
            self.built.load(std::sync::atomic::Ordering::Relaxed),
            self.shared.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// The value for `key`, if it is resident or already published. Never builds, never waits.
    pub(crate) fn peek(&self, key: &K) -> Option<V> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match inner.map.get(key) {
            Some(RoundSlot::Ready(v)) => Some(v.clone()),
            Some(RoundSlot::InFlight(f)) => f.value(),
            None => None,
        }
    }

    /// `key`'s value: resident, else published by whoever leads it, else built here.
    ///
    /// `token` comes from [`flight_token`] and is the key's re-entry identity (see
    /// [`FLIGHT_STACK`]).
    pub(crate) fn get_or_build(&self, key: K, token: u64, build: impl FnOnce() -> V) -> V {
        // This thread is already computing this key (a nested ask from inside the build):
        // waiting for this thread's own publication would deadlock, so compute inline.
        if flight_stack_contains(token) {
            return build();
        }
        match self.claim(&key) {
            Claim::Ready(v) => {
                #[cfg(test)]
                self.shared
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                v
            }
            Claim::Wait(flight) => match flight.wait() {
                Some(v) => {
                    #[cfg(test)]
                    self.shared
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    v
                }
                // The leader panicked or gave up: this thread computes its own answer, and
                // pushes the token so a nested ask inside that build behaves the same way.
                None => {
                    #[cfg(test)]
                    self.built.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let _token = FlightToken::push(token);
                    build()
                }
            },
            Claim::Lead(flight) => {
                #[cfg(test)]
                self.built.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut leader = FlightLeader {
                    cache: self,
                    key,
                    flight,
                    token: FlightToken::push(token),
                    published: false,
                };
                let value = build();
                leader.publish(value.clone());
                value
            }
        }
    }

    fn claim(&self, key: &K) -> Claim<V> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match inner.map.get(key) {
            Some(RoundSlot::Ready(v)) => Claim::Ready(v.clone()),
            Some(RoundSlot::InFlight(f)) => Claim::Wait(f.clone()),
            None => {
                let flight = std::sync::Arc::new(RoundFlight::new());
                inner
                    .map
                    .insert(key.clone(), RoundSlot::InFlight(flight.clone()));
                Claim::Lead(flight)
            }
        }
    }
}

enum Claim<V> {
    Ready(V),
    Wait(std::sync::Arc<RoundFlight<V>>),
    Lead(std::sync::Arc<RoundFlight<V>>),
}

/// The leader's obligation: publish, or release the slot so the next asker can lead.
///
/// The `Drop` is the panic guard. Without it a build that unwinds would leave the slot in
/// `InFlight` forever: every other worker asking for that chunk would wait on a publication
/// that can never happen (a hang, not a wrong byte). Dropping it marks the flight abandoned
/// and removes the slot, so waiters compute inline and later askers become leaders.
struct FlightLeader<'a, K: std::hash::Hash + Eq + Clone, V: Clone> {
    cache: &'a FlightCache<K, V>,
    key: K,
    flight: std::sync::Arc<RoundFlight<V>>,
    // Retain the re-entry token until after the leader's Drop cleanup finishes.
    #[allow(dead_code)]
    token: FlightToken,
    published: bool,
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> FlightLeader<'_, K, V> {
    fn publish(&mut self, value: V) {
        // Publish first: a waiter that grabbed the flight before the promotion below must
        // find the value, not a pending flight nothing will ever fill.
        self.flight.publish(value.clone());
        let mut inner = self.cache.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(RoundSlot::InFlight(f)) = inner.map.get(&self.key) {
            if std::sync::Arc::ptr_eq(f, &self.flight) {
                inner
                    .map
                    .insert(self.key.clone(), RoundSlot::Ready(value));
                inner.ord.push_back(self.key.clone());
                if let Some(bound) = self.cache.bound {
                    while inner.ord.len() > bound {
                        if let Some(old) = inner.ord.pop_front() {
                            inner.map.remove(&old);
                        }
                    }
                }
            }
        }
        self.published = true;
    }
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> Drop for FlightLeader<'_, K, V> {
    fn drop(&mut self) {
        // `self.token` is dropped after this body: the re-entry stack must still hold the key
        // while the cleanup below runs, so a nested ask during cleanup cannot re-claim it.
        if self.published {
            return;
        }
        self.flight.abandon();
        let mut inner = self.cache.inner.lock().unwrap_or_else(|e| e.into_inner());
        let ours = matches!(
            inner.map.get(&self.key),
            Some(RoundSlot::InFlight(f)) if std::sync::Arc::ptr_eq(f, &self.flight)
        );
        if ours {
            inner.map.remove(&self.key);
        }
    }
}

/// One terrain round's shared terrain caches: what its workers and its owner would otherwise
/// each compute privately.
///
/// Three caches, all keyed `(world_seed, preset, chunk)` plus the two things that change what
/// a column contains (`MEGU_NOFEAT`, and which beardifier resolver is in force):
///
/// * `bases` (64) - `base_cols`, pure even while a beard resolves;
/// * `beards` (128) - `resolve_beard`, only filed by an outermost resolution;
/// * `cols` (128) - the **final** columns, only filed at `beard_depth == 0`.
///
/// The last rule is the Nether seed `-1` lesson: a column built while `beard_depth > 0` is the
/// preliminary, structure-free answer on purpose (`Gen::cols` already refuses to put one in
/// `col_c`), and putting it in a cache another worker reads would hand that chunk the wrong
/// terrain - measured on the fixture row `(-994,89,538)`: 19460 cells against 5849.
pub(crate) struct TerrainShare {
    /// `Option` because `Gen`'s own builders answer one: a chunk with no columns is an answer
    /// (and a pure one), so the share publishes it like any other instead of making every
    /// waiter recompute the same absence.
    bases: FlightCache<TerrainKey, Option<std::sync::Arc<ChunkCols>>>,
    beards: FlightCache<TerrainKey, Option<std::sync::Arc<BeardChunkData>>>,
    cols: FlightCache<TerrainKey, Option<std::sync::Arc<ChunkCols>>>,
    base_surf: Mutex<HashMap<(u64, u8, i32, i32), i32>>,
}

/// `(world_seed, preset, cx, cz, flags)` with the flags that change a column's bytes: the
/// `MEGU_NOFEAT` switch, which the beard/column path reads on every call.
type TerrainKey = (u64, u8, i32, i32, u8, usize);

/// Which cache a [`flight_token`] belongs to. The token only has to distinguish a key from the
/// other keys *this thread* is leading at the same moment, so one number per cache plus the
/// key is enough.
const SHARE_DOMAIN_BASES: u64 = 0x1_0000_0001;
const SHARE_DOMAIN_BEARDS: u64 = 0x1_0000_0002;
const SHARE_DOMAIN_COLS: u64 = 0x1_0000_0003;

/// `MEGU_NOFEAT=1` is the terrain-only switch: it skips the whole feature layer, and a
/// structure-shaped terrain is part of that layer. Cached once per process (the flag cannot
/// change under a run) because the share keys on it.
fn share_nofeat() -> bool {
    static NOFEAT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NOFEAT.get_or_init(|| env_flag!("MEGU_NOFEAT"))
}

const BASE_CACHE_CAP: usize = 384;
const BEARD_CACHE_CAP: usize = 512;
const COL_CACHE_CAP: usize = 512;

impl TerrainShare {
    fn new() -> Self {
        TerrainShare {
            bases: FlightCache::new(Some(BASE_CACHE_CAP)),
            beards: FlightCache::new(Some(BEARD_CACHE_CAP)),
            cols: FlightCache::new(Some(COL_CACHE_CAP)),
            base_surf: Mutex::new(HashMap::new()),
        }
    }

    /// Test probe: `((built, shared) for bases, beards, cols)`.
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn flight_counts(&self) -> [(u64, u64); 3] {
        [
            self.bases.flight_counts(),
            self.beards.flight_counts(),
            self.cols.flight_counts(),
        ]
    }
}

static TERRAIN_SHARES: std::sync::OnceLock<Mutex<HashMap<u64, std::sync::Arc<TerrainShare>>>> =
    std::sync::OnceLock::new();

fn terrain_shares() -> &'static Mutex<HashMap<u64, std::sync::Arc<TerrainShare>>> {
    TERRAIN_SHARES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Open a round's shared terrain caches under the id its generators carry in
/// `Gen::starts_round`. Called by `feature::starts_round_begin`, so the two caches a round
/// owns are opened and dropped together.
pub(crate) fn terrain_share_begin(round: u64) {
    terrain_shares()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(round, std::sync::Arc::new(TerrainShare::new()));
}

/// Drop a round's shared terrain caches; called after the round's workers have joined. A
/// straggler that still asks finds no share and builds its own columns, which is the serial
/// answer, never a stale one.
pub(crate) fn terrain_share_end(round: u64) {
    terrain_shares()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&round);
}

/// The share a round opened, if it is still open. A caller that asks after the round ended
/// (or for an id no round opened) gets `None` and builds its own columns.
pub(crate) fn terrain_share(round: u64) -> Option<std::sync::Arc<TerrainShare>> {
    terrain_shares()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&round)
        .cloned()
}

// ---------- M3 Slice P1a: load-independent counters (M1-M5) ----------
//
// `specs/chunk-parallelism` P1a. Terrain Slice 1 (`3921aa5`/`1667aa3`) bought 3.7-4.7x wall
// clock on the terrain stage and paid for it in CPU: `full` went 8.32 s -> 15.64 s at five
// workers while `full - NOFEAT` went the wrong way by 156%. A stopwatch cannot say which
// part of that is repeated work, because every wall-clock and CPU number moves with the load
// on the host. These counters count *work* - chunk starts, column builds, store hits,
// feature-side fallbacks, sections filled - so `jobs=1` against `jobs=5` and `512` against
// `1536` are comparable, and they change no decision: nothing reads them.
//
// The switch is `MEGU_M3_PROFILE=1` (or the older `FEAT_PROFILE=1`). With neither set,
// every site pays one cached `OnceLock` load and a branch that is predicted not-taken, and
// nothing else: no environment reads, no allocation, no atomics.
//
// The meter is **per thread** and merged into a process aggregate on demand. That is not
// decoration: a worker builds its own scratch generator, so its counts have to be flushed
// when its thread ends (`prefetch_terrain`), while a test needs to read the counters of the
// work *it* just did without every other test thread's contribution. `m3_local` is that
// per-thread answer; `m3_aggregate` is the run's.
//
// Why this lives in `worldgen` instead of a module of its own: `evidence/foundation/
// module-layers/policy.json` pins the module set and the allowed edges, and the three
// modules that need these counters (`worldgen`, `feature`, `generation_context`) already
// depend on this one.

/// Number of M3 counters. Keep `M3_NAMES` in step with the indices below.
pub(crate) const M3_N: usize = 43;

// M1 - chunk starts, column builds and beardifier resolutions, owner against worker.
pub(crate) const M3_CHUNK_STARTS_OWNER: usize = 0;
pub(crate) const M3_CHUNK_STARTS_WORKER: usize = 1;
pub(crate) const M3_COLS_BUILT_OWNER: usize = 2;
pub(crate) const M3_COLS_BUILT_WORKER: usize = 3;
pub(crate) const M3_COLS_HIT_OWNER: usize = 4;
pub(crate) const M3_COLS_HIT_WORKER: usize = 5;
pub(crate) const M3_BEARD_RESOLVED_OWNER: usize = 6;
pub(crate) const M3_BEARD_RESOLVED_WORKER: usize = 7;
// M2 - the `terr_c` store.
pub(crate) const M3_TERR_HIT: usize = 8;
pub(crate) const M3_TERR_MISS: usize = 9;
pub(crate) const M3_TERR_EVICT: usize = 10;
pub(crate) const M3_TERR_INSTALL: usize = 11;
pub(crate) const M3_TERR_INSTALL_DUP: usize = 12;
// M3 - what answered a feature-side terrain read.
pub(crate) const M3_RC_HIT: usize = 13;
pub(crate) const M3_PREFETCH_HIT: usize = 14;
pub(crate) const M3_PREFETCH_MISS: usize = 15;
pub(crate) const M3_FALLBACK_DENSITY: usize = 16;
pub(crate) const M3_FALLBACK_DEEP: usize = 17;
pub(crate) const M3_FALLBACK_AQUIFER: usize = 18;
pub(crate) const M3_FALLBACK_SURFACE: usize = 19;
pub(crate) const M3_DEEP_HIT: usize = 20;
pub(crate) const M3_DEEP_MISS: usize = 21;
pub(crate) const M3_AQLV_HIT: usize = 22;
pub(crate) const M3_AQLV_MISS: usize = 23;
pub(crate) const M3_PRELIM_HIT: usize = 24;
pub(crate) const M3_PRELIM_MISS: usize = 25;
// M4 - section work: off-owner against on-owner.
pub(crate) const M3_WORKER_SECTIONS: usize = 26;
pub(crate) const M3_OWNER_SECTIONS: usize = 27;
pub(crate) const M3_EVICTED_UNCONSUMED: usize = 28;
// M5 - stage breakdown. `*_NS` pairs with the `*_N` next to it.
pub(crate) const M3_COLS_NS: usize = 29;
pub(crate) const M3_COLS_N: usize = 30;
pub(crate) const M3_BEARD_NS: usize = 31;
pub(crate) const M3_BEARD_N: usize = 32;
pub(crate) const M3_FILL_NS: usize = 33;
pub(crate) const M3_FILL_N: usize = 34;
pub(crate) const M3_AQUIFER_CELLS: usize = 35;
pub(crate) const M3_SURFACE_CELLS: usize = 36;
pub(crate) const M3_CARVER_NS: usize = 37;
pub(crate) const M3_CARVER_N: usize = 38;
pub(crate) const M3_FEATURE_NS: usize = 39;
pub(crate) const M3_FEATURE_N: usize = 40;
// M6 - the four-lane density evaluator: groups it evaluated, and groups a conditional jump
// found divergent (finished on the one-lane path).
pub(crate) const M3_DF4_VEC: usize = 41;
pub(crate) const M3_DF4_BAIL: usize = 42;

/// Counter names, in index order. The determinism test reports them, so this is a test
/// helper: the run report groups them by metric instead of dumping the flat list.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) const M3_NAMES: [&str; M3_N] = [
    "m1_chunk_starts_owner",
    "m1_chunk_starts_worker",
    "m1_cols_built_owner",
    "m1_cols_built_worker",
    "m1_cols_hit_owner",
    "m1_cols_hit_worker",
    "m1_beard_resolved_owner",
    "m1_beard_resolved_worker",
    "m2_terr_hit",
    "m2_terr_miss",
    "m2_terr_evict",
    "m2_terr_install",
    "m2_terr_install_dup",
    "m3_rc_hit",
    "m3_prefetch_hit",
    "m3_prefetch_miss",
    "m3_fallback_density",
    "m3_fallback_deep",
    "m3_fallback_aquifer",
    "m3_fallback_surface",
    "m3_deep_hit",
    "m3_deep_miss",
    "m3_aqlv_hit",
    "m3_aqlv_miss",
    "m3_prelim_hit",
    "m3_prelim_miss",
    "m4_worker_sections",
    "m4_owner_sections",
    "m4_evicted_unconsumed",
    "m5_cols_ns",
    "m5_cols_n",
    "m5_beard_ns",
    "m5_beard_n",
    "m5_fill_ns",
    "m5_fill_n",
    "m5_aquifer_cells",
    "m5_surface_cells",
    "m5_carver_ns",
    "m5_carver_n",
    "m5_feature_ns",
    "m5_feature_n",
    "m6_df4_vec",
    "m6_df4_bail",
];

/// The counters that are durations rather than counts. Everything else counts deterministic
/// work and is repeatable across runs; these move with the host's load by construction, which
/// is why the P1a conclusions are drawn from the counts and these only split the remainder.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) const M3_DURATION_COUNTERS: [usize; 5] = [
    M3_COLS_NS,
    M3_BEARD_NS,
    M3_FILL_NS,
    M3_CARVER_NS,
    M3_FEATURE_NS,
];

/// One thread's counter block. Plain `u64`s: a counter is read only after the thread that
/// owns it has finished with it, so nothing here needs to be atomic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct M3Meter(pub [u64; M3_N]);

impl Default for M3Meter {
    fn default() -> Self {
        M3Meter([0; M3_N])
    }
}

impl M3Meter {
    pub(crate) fn get(&self, k: usize) -> u64 {
        self.0[k]
    }

    /// Add `other` into `self`, counter by counter.
    pub(crate) fn merge(&mut self, other: &M3Meter) {
        for (dst, src) in self.0.iter_mut().zip(other.0.iter()) {
            *dst += *src;
        }
    }
}

static M3_ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
#[cfg(test)]
static M3_TEST_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Is the counter layer on? One cached load on every guarded site.
#[inline]
pub(crate) fn m3_on() -> bool {
    #[cfg(test)]
    if M3_TEST_ON.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    *M3_ON.get_or_init(|| env_flag!("MEGU_M3_PROFILE") || env_flag!("FEAT_PROFILE"))
}

#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn m3_test_force(on: bool) {
    M3_TEST_ON.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Held by every test that forces the counters on. The force is process-wide, so two such
/// tests running at once would each switch it off under the other's measurement - measured:
/// the determinism test went red about once in five runs until this existed.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) static M3_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    static M3_LOCAL: std::cell::RefCell<M3Meter> = std::cell::RefCell::new(M3Meter::default());
    /// Set on a thread that is filling one chunk's terrain for another thread's consumer.
    static M3_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Depth of `FC::get`'s cold fallback, so the memo counters below can tell a
    /// feature-side read from the terrain fill that warms the same memos.
    static M3_FEATURE_READ: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

static M3_AGG: [std::sync::atomic::AtomicU64; M3_N] =
    [const { std::sync::atomic::AtomicU64::new(0) }; M3_N];

/// One counter.
#[inline]
pub(crate) fn m3_inc(k: usize) {
    if !m3_on() {
        return;
    }
    M3_LOCAL.with(|m| m.borrow_mut().0[k] += 1);
}

/// One counter, decided by a flag the caller already computed (`m3_on() && …`).
#[inline]
fn inc_if(flag: bool, k: usize) {
    if flag {
        M3_LOCAL.with(|m| m.borrow_mut().0[k] += 1);
    }
}

/// One counter, attributed to the owner thread or to a prefetch worker.
#[inline]
pub(crate) fn m3_inc_split(owner: usize, worker: usize) {
    if !m3_on() {
        return;
    }
    let k = if m3_is_worker() { worker } else { owner };
    M3_LOCAL.with(|m| m.borrow_mut().0[k] += 1);
}

/// Start a timed stage. `None` when the counters are off, so the stage costs a branch.
#[inline]
pub(crate) fn m3_start() -> Option<std::time::Instant> {
    m3_on().then(std::time::Instant::now)
}

/// Close a stage opened by [`m3_start`] into its `(ns, n)` counter pair.
#[inline]
pub(crate) fn m3_elapsed(k_ns: usize, k_n: usize, t0: Option<std::time::Instant>) {
    if let Some(t0) = t0 {
        let ns = t0.elapsed().as_nanos() as u64;
        M3_LOCAL.with(|m| {
            let mut m = m.borrow_mut();
            m.0[k_ns] += ns;
            m.0[k_n] += 1;
        });
    }
}

#[inline]
pub(crate) fn m3_is_worker() -> bool {
    M3_WORKER.with(|c| c.get())
}

/// Mark this thread as a terrain prefetch worker. Set once per spawned worker.
pub(crate) fn m3_set_worker(on: bool) {
    M3_WORKER.with(|c| c.set(on));
}

/// Scope guard for [`m3_set_worker`], entered at the top of a scoped-batch job.
/// A `std::thread::scope` worker died holding the flag, but a resident pool
/// worker — or the scope's own caller when it help-steals a job — keeps its
/// thread-locals afterwards, so the flag must be restored on drop or later
/// work on that thread attributes to the worker bucket.
pub(crate) struct M3WorkerScope;

impl M3WorkerScope {
    pub(crate) fn enter() -> Self {
        m3_set_worker(true);
        M3WorkerScope
    }
}

impl Drop for M3WorkerScope {
    fn drop(&mut self) {
        m3_set_worker(false);
    }
}

#[inline]
pub(crate) fn m3_in_feature_read() -> bool {
    M3_FEATURE_READ.with(|c| c.get() > 0)
}

/// Scope guard: while it lives, the terrain memos (`deep_c`, `aq_lv`, `prelim_c`) are being
/// read on behalf of the feature layer rather than by the terrain fill.
pub(crate) struct M3FeatureRead(bool);

impl M3FeatureRead {
    pub(crate) fn enter() -> Self {
        if !m3_on() {
            return M3FeatureRead(false);
        }
        M3_FEATURE_READ.with(|c| c.set(c.get() + 1));
        M3FeatureRead(true)
    }
}

impl Drop for M3FeatureRead {
    fn drop(&mut self) {
        if self.0 {
            M3_FEATURE_READ.with(|c| c.set(c.get() - 1));
        }
    }
}

/// Merge this thread's counters into the process aggregate and clear them.
///
/// A prefetch worker calls this before its thread ends: its meter would otherwise die with
/// the thread and the run's own report would under-count exactly the work P1a is about.
pub(crate) fn m3_flush() {
    if !m3_on() {
        return;
    }
    let local = M3_LOCAL.with(|m| *m.borrow());
    for (agg, v) in M3_AGG.iter().zip(local.0.iter()) {
        if *v != 0 {
            agg.fetch_add(*v, std::sync::atomic::Ordering::Relaxed);
        }
    }
    M3_LOCAL.with(|m| *m.borrow_mut() = M3Meter::default());
}

/// This thread's counters (not the process aggregate).
pub(crate) fn m3_local() -> M3Meter {
    M3_LOCAL.with(|m| *m.borrow())
}

/// Drop this thread's counters without touching the aggregate (test helper: a test reads
/// [`m3_local`] for the work it just did, so it has to start from a known state).
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn m3_reset_local() {
    M3_LOCAL.with(|m| *m.borrow_mut() = M3Meter::default());
}

/// The process aggregate: every thread that has flushed, including this one's unflushed
/// counters so a caller does not have to remember to flush first.
pub(crate) fn m3_aggregate() -> M3Meter {
    let mut out = M3Meter::default();
    for (dst, agg) in out.0.iter_mut().zip(M3_AGG.iter()) {
        *dst = agg.load(std::sync::atomic::Ordering::Relaxed);
    }
    out.merge(&m3_local());
    out
}

/// The M1-M5 report, one line per metric group. Empty when the counters are off.
pub(crate) fn m3_report() -> String {
    if !m3_on() {
        return String::new();
    }
    let m = m3_aggregate();
    let g = |k: usize| m.get(k);
    let ms = |ns: usize| g(ns) as f64 / 1e6;
    let mut s = String::new();
    s.push_str(&format!(
        "[m3] terrain_store_bound={} jobs={}\n",
        terrain_store_bound(),
        terrain_jobs()
    ));
    s.push_str(&format!(
        "[m3] M1 chunk_starts owner={} worker={} | cols built owner={} worker={} hit owner={} worker={} | beard resolved owner={} worker={}\n",
        g(M3_CHUNK_STARTS_OWNER),
        g(M3_CHUNK_STARTS_WORKER),
        g(M3_COLS_BUILT_OWNER),
        g(M3_COLS_BUILT_WORKER),
        g(M3_COLS_HIT_OWNER),
        g(M3_COLS_HIT_WORKER),
        g(M3_BEARD_RESOLVED_OWNER),
        g(M3_BEARD_RESOLVED_WORKER)
    ));
    s.push_str(&format!(
        "[m3] M2 terr_c hit={} miss={} evict={} install={} install_dup={} bound={}\n",
        g(M3_TERR_HIT),
        g(M3_TERR_MISS),
        g(M3_TERR_EVICT),
        g(M3_TERR_INSTALL),
        g(M3_TERR_INSTALL_DUP),
        terrain_store_bound()
    ));
    s.push_str(&format!(
        "[m3] M3 feature reads rc_hit={} prefetch_hit={} prefetch_miss={} | fallback density={} deep={} aquifer={} surface={} | deep_memo hit={} miss={} aqlv hit={} miss={} prelim hit={} miss={}\n",
        g(M3_RC_HIT),
        g(M3_PREFETCH_HIT),
        g(M3_PREFETCH_MISS),
        g(M3_FALLBACK_DENSITY),
        g(M3_FALLBACK_DEEP),
        g(M3_FALLBACK_AQUIFER),
        g(M3_FALLBACK_SURFACE),
        g(M3_DEEP_HIT),
        g(M3_DEEP_MISS),
        g(M3_AQLV_HIT),
        g(M3_AQLV_MISS),
        g(M3_PRELIM_HIT),
        g(M3_PRELIM_MISS)
    ));
    s.push_str(&format!(
        "[m3] M4 sections worker={} owner={} evicted_unconsumed={}\n",
        g(M3_WORKER_SECTIONS),
        g(M3_OWNER_SECTIONS),
        g(M3_EVICTED_UNCONSUMED)
    ));
    s.push_str(&format!(
        "[m3] M6 df4 vec={} bail={}\n",
        g(M3_DF4_VEC),
        g(M3_DF4_BAIL)
    ));
    s.push_str(&format!(
        "[m3] M5 cols={:.1}ms/{} beard={:.1}ms/{} fill={:.1}ms/{} aquifer_cells={} surface_cells={} carver={:.1}ms/{} feature={:.1}ms/{}\n",
        ms(M3_COLS_NS),
        g(M3_COLS_N),
        ms(M3_BEARD_NS),
        g(M3_BEARD_N),
        ms(M3_FILL_NS),
        g(M3_FILL_N),
        g(M3_AQUIFER_CELLS),
        g(M3_SURFACE_CELLS),
        ms(M3_CARVER_NS),
        g(M3_CARVER_N),
        ms(M3_FEATURE_NS),
        g(M3_FEATURE_N)
    ));
    s
}

// ---------- end M3 P1a counters ----------

/// One chunk's carve stage — the record that replaces the three containers that used to
/// describe it (`carved`, `wg_after_carve`, `wg_after_carve_ord`).
///
/// `blocks_live` is the carve claim; there is no separate "this chunk is carved" set.
/// A record can retain a WG value after its block bucket and claim have been dropped.
/// The two outputs deliberately have different lifetimes:
///
/// * `wg` — `ChunkAccess`'s WORLD_SURFACE_WG `[0..256)` / OCEAN_FLOOR_WG `[256..512)` as
///   of the end of the carver pass, index `lz * 16 + lx`, `topSolid + 1`. Vanilla creates
///   a chunk's WG maps while it still carries the SURFACE status and updates them per
///   write, so the carver pass lowers them and the FEATURES status afterwards updates
///   only the POST_FEATURES maps. Retained up to `CARVE_WG_BOUND` chunks.
/// * `blocks_live` — whether the pass' block writes are still materialised in `feat_c`.
///   That map is a `FEATURE_BUCKET_BOUND`-chunk LRU shared with the feature layer, so it
///   drops carve writes while `wg` may still be resident; the claim has to fall with the
///   writes or the next FEATURES pass reads uncarved stone (`apply_carvers` would return
///   at `begin_carve`) against a post-carve WG value.
///
/// `wg` owns its own lifetime precisely so the 384-chunk feature bound cannot silently
/// truncate the 4096-chunk WG retention, and `blocks_live` owns the claim precisely so
/// the reverse cannot happen either. Neither field is written by anything but the
/// lifecycle methods below.
pub(crate) struct CarveStage {
    wg: Option<Box<[i32; 512]>>,
    blocks_live: bool,
}

/// `AQ_TRACE="<cx>,<cz>"` prints the aquifer's decision terms for that chunk.
/// Warm cost is one `OnceLock` atomic load; callers hoist it out of the hot path
/// so each `aq_state` call pays it once rather than at every return branch.
fn aq_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("AQ_TRACE").is_some() || std::env::var_os("AQ_POS").is_some())
}

fn aq_trace_at(x: i32, z: i32) -> bool {
    if !aq_trace_enabled() {
        return false;
    }
    use std::sync::OnceLock;
    static SPEC: OnceLock<Option<(i32, i32)>> = std::sync::OnceLock::new();
    let spec = SPEC.get_or_init(|| {
        std::env::var("AQ_TRACE").ok().and_then(|v| {
            let (a, b) = v.split_once(',')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        })
    });
    matches!(spec, Some((cx, cz)) if *cx == x >> 4 && *cz == z >> 4)
}

/// Diagnostics: `WG_COUNT=1` counts **interpreter** node visits
/// (`Gen::eval_c` calls). It is not a work counter any more: with a compiled
/// `final_density` program the per-block spine runs as instructions and is not
/// counted, so the number covers only the corner samples that fill the
/// `interpolated` grids plus the `Op::Call` subtrees (splines,
/// `find_top_surface`, `old_blended_noise`, `end_islands`). Measured
/// 2026-11-18: 1,751,278 per chunk interpreted, 387,576 with the program
/// (see evidence/foundation/performance/README.md, "Counter semantics").
pub(crate) static EVAL_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Diagnostics: `WG_COUNT=1` also counts visits per density-function node id.
/// Sized once per generator (see `Gen::df_hits_init`); the counters are what say
/// *which* nodes the per-chunk time goes to, which is what a compiled evaluator
/// has to beat.
pub(crate) static DF_HITS: std::sync::OnceLock<Vec<std::sync::atomic::AtomicU64>> =
    std::sync::OnceLock::new();

impl Gen {
    /// Allocate the per-node visit counters for this generator's `dfs` (diagnostic only).
    fn df_hits_init(&self) {
        let _ = DF_HITS.set(
            (0..self.dfs.len())
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
        );
    }

    fn df_files() -> HashMap<String, Vec<u8>> {
        Self::dp_files("density_function")
    }
    // datapacks/*/data/<ns>/worldgen/<subdir>/**.json -> "ns:relpath" (no .json)
    /// The packs under `datapacks/` in *precedence* order: `vanilla` first (the
    /// lowest priority, like the built-in data a datapack overrides), then the rest by
    /// name so the order is deterministic. Iterating `read_dir` instead made
    /// precedence arbitrary - a control pack meant to isolate one feature could be
    /// silently ignored (measured: a ctl pack that zeroed every ore except
    /// `ore_gravel` was ignored, and the port still ran 241 ore placements where the
    /// server ran 126).
    pub(crate) fn packs() -> Vec<std::path::PathBuf> {
        let base = std::path::Path::new("datapacks");
        let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(base)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        dirs.sort_by_key(|p| {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            (name != "vanilla", name)
        });
        dirs
    }

    /// Read one resource with the same later-pack-wins ordering as JSON inventories.
    pub(crate) fn dp_resource(kind: &str, id: &str, extension: &str) -> Option<Vec<u8>> {
        Self::dp_resource_in(&Self::packs(), kind, id, extension)
    }
    fn dp_resource_in(
        packs: &[std::path::PathBuf],
        kind: &str,
        id: &str,
        extension: &str,
    ) -> Option<Vec<u8>> {
        let (namespace, path) = id.split_once(':').unwrap_or(("minecraft", id));
        let valid = |s: &str, slash: bool| {
            !s.is_empty()
                && s.bytes().all(|b| {
                    b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b"._-".contains(&b)
                        || (slash && b == b'/')
                })
        };
        if !valid(namespace, false)
            || matches!(namespace, "." | "..")
            || !valid(path, true)
            || std::path::Path::new(path)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        for pack in packs.iter().rev() {
            let file = pack
                .join("data")
                .join(namespace)
                .join(kind)
                .join(format!("{path}.{extension}"));
            match std::fs::read(file) {
                Ok(bytes) => return Some(bytes),
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        || e.kind() == std::io::ErrorKind::NotADirectory => {}
                Err(_) => return None,
            }
        }
        None
    }

    pub(crate) fn dp_files(subdir: &str) -> HashMap<String, Vec<u8>> {
        Self::dp_files_any(&[subdir])
    }

    /// Merge several candidate directory names, later *names* winning within a pack.
    /// The reference build's own data uses 1.21.1's layout (`worldgen/configured_feature`)
    /// while a converted pack may use the newer `worldgen/feature`; both have to load,
    /// because the data is the reference and the conversion is not.
    pub(crate) fn dp_files_any(subdirs: &[&str]) -> HashMap<String, Vec<u8>> {
        let mut m = HashMap::new();
        for subdir in subdirs {
            for (k, v) in Self::dp_files_one(subdir) {
                m.insert(k, v);
            }
        }
        m
    }

    fn dp_files_one(subdir: &str) -> HashMap<String, Vec<u8>> {
        let mut m = HashMap::new();
        {
            for p in Gen::packs() {
                let root = p.join("data");
                if let Ok(nss) = std::fs::read_dir(&root) {
                    for ns in nss.flatten() {
                        let dfr = ns.path().join(format!("worldgen/{}", subdir));
                        let mut stack = vec![dfr.clone()];
                        while let Some(d) = stack.pop() {
                            if let Ok(rd) = std::fs::read_dir(&d) {
                                for e in rd.flatten() {
                                    let p = e.path();
                                    if p.is_dir() {
                                        stack.push(p);
                                    } else if p.extension().map(|e| e == "json").unwrap_or(false) {
                                        if let Ok(rel) = p.strip_prefix(&dfr) {
                                            let key = format!(
                                                "{}:{}",
                                                ns.file_name().to_string_lossy(),
                                                rel.with_extension("").to_string_lossy()
                                            );
                                            if let Ok(b) = std::fs::read(&p) {
                                                // later packs override earlier ones, and
                                                // `packs()` puts `vanilla` first (lowest
                                                // priority). `or_insert` made the first
                                                // pack win, so a control pack could never
                                                // override the bundled data.
                                                m.insert(key, b);
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

    /// `LOAD_TRACE=1` names the step at which loading gave up. Without it a missing
    /// piece of the worldgen data looks exactly like a wrong world (CV2).
    fn load_trace(why: &str) {
        if std::env::var("LOAD_TRACE").is_ok() {
            eprintln!("[load] {why}");
        }
    }

    pub fn load(seed: u64, res: fn(&str) -> Option<u32>) -> Option<Gen> {
        Self::load_preset(seed, res, NoisePreset::Overworld)
    }

    pub(crate) fn load_preset(
        seed: u64,
        res: fn(&str) -> Option<u32>,
        preset: NoisePreset,
    ) -> Option<Gen> {
        #[cfg(test)]
        {
            if std::env::var_os("LOAD_TRACE").is_none()
                && std::env::var_os("MEGU_VEINDBG").is_none()
            {
                use std::sync::{Mutex, OnceLock};
                type Key = (u64, u8, usize, bool, Vec<std::path::PathBuf>);
                static CACHE: OnceLock<Mutex<HashMap<Key, Option<std::sync::Arc<GenData>>>>> =
                    OnceLock::new();
                let preset_id = match preset {
                    NoisePreset::Overworld => 0,
                    NoisePreset::Nether => 1,
                    NoisePreset::End => 2,
                };
                let noaq = std::env::var_os("MEGU_NOAQ").is_some();
                let key = (seed, preset_id, res as usize, noaq, Self::packs());
                let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
                if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
                    return hit
                        .as_ref()
                        .map(|d| Self::fresh_scratch(std::sync::Arc::clone(d)));
                }
                let built = Self::load_preset_uncached(seed, res, preset);
                let entry = built.as_ref().map(|g| std::sync::Arc::clone(&g.data));
                cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, entry);
                return built;
            }
        }
        Self::load_preset_uncached(seed, res, preset)
    }

    fn load_preset_uncached(
        seed: u64,
        res: fn(&str) -> Option<u32>,
        preset: NoisePreset,
    ) -> Option<Gen> {
        Self::load_preset_inner(seed, res, preset, true)
    }

    /// The unfolded twin of [`Gen::load_preset_uncached`], for the
    /// `perf_df_constant_fold_*` A/B: identical inputs with
    /// [`fold_constant_dfs`] skipped, so the raw and folded id tables can be
    /// built and timed side by side. Test-only; production always folds.
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn load_preset_uncached_unfolded(
        seed: u64,
        res: fn(&str) -> Option<u32>,
        preset: NoisePreset,
    ) -> Option<Gen> {
        Self::load_preset_inner(seed, res, preset, false)
    }

    fn load_preset_inner(
        seed: u64,
        res: fn(&str) -> Option<u32>,
        preset: NoisePreset,
        fold_dfs: bool,
    ) -> Option<Gen> {
        let files = Self::df_files();
        let mut g = Gen {
            data: std::sync::Arc::new(GenData {
                preset,
                dfs: vec![],
                names: HashMap::new(),
                noises: vec![],
                noise_names: HashMap::new(),
                factory: Xor::seed(seed).splitter(),
                base: String::new(),
                final_density: 0,
                climate: None,
                depth_split: None,
                biome_zoom: crate::generation_math::biome_zoom_seed(seed),
                btree: None,
                leaf_ps: Vec::new(),
                biome_names: Vec::new(),
                biome_ids: Vec::new(),
                biome_weather: Vec::new(),
                sea: 63,
                min_y: -64,
                height: 384,
                world_min_y: -64,
                world_height: 384,
                noise_cell_h: 8,
                noise_cell_w: 4,
                legacy_random_source: false,
                rules: vec![],
                conds: vec![],
                rule_ids: HashMap::new(),
                cond_ids: HashMap::new(),
                bleaf: HashMap::new(),
                surface_rule: None,
                deep_fast: None,
                prelim_df: None,
                surf_ni: None,
                sec_ni: None,
                band_off_ni: None,
                bands: None,
                ore_split: None,
                def_st: 1,
                def_fl: 0,
                bedrock_st: res("minecraft:bedrock").unwrap_or(1),
                aq: None,
                vein: None,
                is_2d: Vec::new(),
                df_folded: 0,
                ram_trimmed: 0,
                world_seed: seed,
                interp_nodes: Vec::new(),
                density_interp_nodes: Vec::new(),
                vein_interp_nodes: Vec::new(),
                upper_bound_interp: None,
                fallback_share: std::sync::Arc::new(TerrainShare::new()),
                dfprog: None,
                dfcols: None,
                vfcols: None,
            }),
            aq_chunk: std::array::from_fn(|_| None),
            aq_chunk_use: [0; AQ_CHUNK_SLOTS],
            aq_chunk_tick: 0,
            aq_cell12: AqCell12::empty(),
            prelim_c: HashMap::new(),
            chunk_biomes: HashMap::new(),
            chunk_biomes_ord: std::collections::VecDeque::new(),
            prelim_ord: std::collections::VecDeque::new(),
            clim_c: HashMap::new(),
            clim_ord: std::collections::VecDeque::new(),
            bioq: HashMap::new(),
            bioq_ord: std::collections::VecDeque::new(),
            bioq_none: HashMap::new(),
            bioq_none_ord: std::collections::VecDeque::new(),
            base_hm_c: HashMap::new(),
            base_hm_cc: None,
            base_hm_memos: None,
            base_col_lazy: None,
            col_c: HashMap::new(),
            col_ord: std::collections::VecDeque::new(),
            starts_round: None,
            share_slot: None,
            base_c: HashMap::new(),
            base_ord: std::collections::VecDeque::new(),
            beard_c: HashMap::new(),
            beard_ord: std::collections::VecDeque::new(),
            beard_resolver: None,
            beard_depth: 0,
            terr_c: HashMap::new(),
            terr_ord: std::collections::VecDeque::new(),
            terr_pending: std::collections::BTreeSet::new(),
            feat_c: HashMap::new(),
            staged_payloads: Vec::new(),
            feat_ord: std::collections::VecDeque::new(),
            deep_c: HashMap::new(),
            deep_ord: std::collections::VecDeque::new(),
            pop_done: HashSet::new(),
            pop_plan_window: None,
            shape_marks: HashMap::new(),
            mark_pool: Vec::new(),
            mark_src: HashMap::new(),
            aq_sched: false,
            aq_calls: 0,
            terrain_marked: HashSet::new(),
            pop_active: None,
            restored_tick_sources: HashSet::new(),
            suppress_generated_ticks: false,
            carve_c: HashMap::new(),
            carve_ord: std::collections::VecDeque::new(),
            pop_region: std::env::var("MEGU_POP_REGION").ok().and_then(|v| {
                let p: Vec<i32> = v.split(',').filter_map(|n| n.parse().ok()).collect();
                if p.len() == 4 {
                    Some((p[0], p[1], p[2], p[3]))
                } else {
                    None
                }
            }),
            a3a_dispatch: None,
            memo: HashMap::new(),
            corner_memo: None,
            dfcol_scratch: None,
            mineshaft_spawners: HashSet::new(),
            payload_sources: HashSet::new(),
            feat_hm: HashMap::new(),
            rule_diag: None,
            generated_ticks: GeneratedTickQueues::default(),
        };
        // noise settings (sea level, bounds, router names)
        let mut ns_json: Option<J> = None;
        'outer: for p in Gen::packs() {
            let f = p.join(format!(
                "data/minecraft/worldgen/noise_settings/{}.json",
                preset.name()
            ));
            if let Ok(b) = std::fs::read(&f) {
                ns_json = jparse(&b);
                g.base = p.to_string_lossy().to_string();
                break 'outer;
            }
        }
        let ns = match ns_json {
            Some(j) => j,
            None => {
                Self::load_trace(&format!(
                    "noise_settings/{}.json was not found or is not JSON",
                    preset.name()
                ));
                return None;
            }
        };
        g.sea = ns.get("sea_level").map(J::num).unwrap_or(63.0) as i32;
        g.legacy_random_source = jbool(&ns, "legacy_random_source", false);
        if let Some(n) = ns.get("noise") {
            g.min_y = n.get("min_y").map(J::num).unwrap_or(-64.0) as i32;
            g.height = n.get("height").map(J::num).unwrap_or(384.0) as i32;
            g.noise_cell_h = (n.get("size_vertical").map(J::num).unwrap_or(2.0) as i32 * 4).max(1);
            g.noise_cell_w = n.get("size_horizontal").map(J::num).unwrap_or(1.0) as i32 * 4;
            if g.noise_cell_w <= 0
                || g.height <= 0
                || 16 % g.noise_cell_w != 0
                || g.height % g.noise_cell_h != 0
            {
                Self::load_trace("noise cell sizes do not divide the chunk dimensions");
                return None;
            }
        }
        let router = ns.get("noise_router");
        // The **world** window: `dimension_type/<dimension>.json`'s `min_y`/`height`.
        // It is a different fact from the noise settings above and 1.21.1 differs by 2x
        // for both non-overworld dimensions (noise 0/128, dimension 0/256). Only the
        // storage-side readers use it - chunk sections, heightmaps, the offline dump
        // length, structure/furniture bounds - never the density/aquifer/carver/surface
        // path, which is generator height in vanilla too. Absent or unusable, the world
        // window *is* the generator window, which keeps every pre-split fixture exact.
        g.world_min_y = g.min_y;
        g.world_height = g.height;
        {
            let file = dimension_type_file(preset);
            let mut candidates: Vec<std::path::PathBuf> = Vec::new();
            if !g.base.is_empty() {
                // the pack the noise settings came from answers first, so one pack cannot
                // contribute a generator and a different pack its world
                candidates.push(std::path::PathBuf::from(&g.base));
            }
            candidates.extend(Gen::packs());
            for p in candidates {
                let f = p.join(format!("data/minecraft/dimension_type/{file}.json"));
                let Ok(b) = std::fs::read(&f) else { continue };
                let Some(j) = jparse(&b) else {
                    Self::load_trace(&format!(
                        "dimension_type/{file}.json is not JSON; the world window stays {}/{}",
                        g.min_y, g.height
                    ));
                    break;
                };
                match dimension_window(&j, g.min_y, g.height) {
                    Some((min_y, height)) => {
                        g.world_min_y = min_y;
                        g.world_height = height;
                    }
                    // Fail closed: a window that is not whole sections or that does not
                    // contain the terrain the generator makes would truncate it.
                    None => Self::load_trace(&format!(
                        "dimension_type/{file}.json is not a window containing the generator's \
                         {}/{}",
                        g.min_y, g.height
                    )),
                }
                break;
            }
        }
        // 1.21.1 writes the router entries as inline density functions (the newer
        // dialect writes references), so this has to go through `router_df`, which
        // accepts either.
        let Some(fd) = router.and_then(|r| r.get("final_density")) else {
            Self::load_trace("noise_settings has no noise_router.final_density");
            return None;
        };
        let Some(idx) = router_df(&mut g, &files, fd) else {
            Self::load_trace("noise_router.final_density did not resolve");
            return None;
        };
        // Vanilla's chunk pipeline never reads `final_density` bare: `NoiseChunk`
        // builds `cacheAllInCell(add(router.finalDensity, BeardifierMarker))` and
        // `wrap` substitutes the chunk's `Beardifier` for the marker. The datapack has
        // no such node, so the same edge is added here once, at load.
        let base = idx;
        let beard = g.dfs.len();
        g.dfs.push(DF::Beardifier);
        let sum = g.dfs.len();
        g.dfs.push(DF::Add(base, beard));
        g.final_density = sum;
        // 1.21.1 generates ore veins in the noise chunk (`ore_veins_enabled`)
        // from three router functions; 1.21.2+ moved them into a material rule.
        if jbool(&ns, "ore_veins_enabled", false) {
            let get = |k: &str| router.and_then(|r| r.get(k)).cloned();
            if let (Some(a), Some(b), Some(c)) =
                (get("vein_toggle"), get("vein_ridged"), get("vein_gap"))
            {
                // the vein functions are *inline* objects in 1.21.1, not names
                let (ta, tb, tc) = (
                    router_df(&mut g, &files, &a),
                    router_df(&mut g, &files, &b),
                    router_df(&mut g, &files, &c),
                );
                if std::env::var("MEGU_VEINDBG").is_ok() {
                    eprintln!("VEINCFG refs a={:?} b={:?} c={:?}", ta, tb, tc);
                }
                if let (Some(ta), Some(tb), Some(tc)) = (ta, tb, tc) {
                    // the resolver `Gen::load` was handed, not the root binding: a
                    // generation module may not reach `crate::` for names.
                    let st = |n: &str| res(n).unwrap_or(0);
                    g.vein = Some(VeinCfg {
                        toggle: ta,
                        ridged: tb,
                        gap: tc,
                        copper_ore: st("minecraft:copper_ore"),
                        copper_raw: st("minecraft:raw_copper_block"),
                        copper_fill: st("minecraft:granite"),
                        iron_ore: st("minecraft:deepslate_iron_ore"),
                        iron_raw: st("minecraft:raw_iron_block"),
                        iron_fill: st("minecraft:tuff"),
                    });
                }
            }
        }
        // climate router ids for biome multi-noise sampling
        {
            let keys = [
                "temperature",
                "vegetation",
                "continents",
                "erosion",
                "depth",
                "ridges",
            ];
            let mut ids = [0usize; 6];
            let mut ok = true;
            for (i, k) in keys.iter().enumerate() {
                match router
                    .and_then(|r| r.get(k))
                    .and_then(|j| router_df(&mut g, &files, j))
                {
                    Some(v) => ids[i] = v,
                    None => {
                        Self::load_trace(&format!("climate router field {k:?} did not resolve"));
                        ok = false;
                    }
                }
            }
            if ok {
                g.climate = Some(ids);
                // vanilla depth = grad(y) + offset(x,z): split it so the
                // expensive offset term can be cached per column.
                if let DF::Add(a, b) = g.dfs[ids[4]] {
                    let (gi, oi) = if matches!(g.dfs[a], DF::Grad { .. }) {
                        (a, b)
                    } else if matches!(g.dfs[b], DF::Grad { .. }) {
                        (b, a)
                    } else {
                        (usize::MAX, usize::MAX)
                    };
                    if gi != usize::MAX {
                        if let DF::Grad { fc, fv, tc, tv } = g.dfs[gi] {
                            g.depth_split = Some((fc, fv, tc, tv, oi));
                        }
                    }
                }
            }
        }
        // Vanilla's built-in climate tree, exported by tools/biome_tree_from_jar.py.
        // The preset JSON only names the builder; it does not contain its intervals.
        if preset == NoisePreset::End {
            for name in [
                "minecraft:the_end",
                "minecraft:end_highlands",
                "minecraft:end_midlands",
                "minecraft:small_end_islands",
                "minecraft:end_barrens",
            ] {
                let leaf = g.biome_names.len() as u16;
                g.biome_names.push(name.to_string());
                g.bleaf.insert(name.to_string(), leaf);
            }
        } else {
            let tp = std::path::Path::new(&g.base).join("multi_noise_biome_tree.json");
            if let Ok(b) = std::fs::read(&tp) {
                if let Some(j) = jparse(&b) {
                    if let Some(ow) = j.get(preset.name()) {
                        let d = std::sync::Arc::make_mut(&mut g.data);
                        d.btree = bt_parse(ow, &mut d.biome_names, &mut d.bleaf, &mut d.leaf_ps);
                    }
                }
            }
            if g.btree.is_none() {
                Self::load_trace(&format!(
                    "no valid {} biome tree at {}",
                    preset.name(),
                    tp.display()
                ));
            }
        }
        // SurfaceRules.Temperature reads biome weather, not the noise-router
        // temperature coordinate. Bind the settings to the same leaf identities.
        let biome_files = Self::dp_files("biome");
        for name in g.biome_names.clone() {
            g.biome_ids.push(biome_id(&name));
            let Some(weather) = biome_files
                .get(&name)
                .and_then(|b| jparse(b))
                .and_then(|j| BiomeWeather::parse(&j))
            else {
                Self::load_trace(&format!("biome weather did not parse: {name}"));
                return None;
            };
            g.biome_weather.push(weather);
        }
        // vanilla surface rules (material_rule) — needs biome tree loaded first
        // (biome conditions resolve to leaf ids).
        {
            let rfiles = Self::dp_files("material_rule");
            let cfiles = Self::dp_files("material_condition");
            g.surf_ni = g.noise_id("minecraft:surface");
            g.sec_ni = g.noise_id("minecraft:surface_secondary");
            g.band_off_ni = g.noise_id("minecraft:clay_bands_offset");
            g.ore_split = Some(g.factory.of_str("minecraft:ore").splitter());
            g.def_st = Self::block_state(ns.get("default_block")?, res)?;
            g.def_fl = Self::block_state(ns.get("default_fluid")?, res)?;
            g.bands = create_bands(&mut g.factory.of_str("minecraft:clay_bands"), res);
            // Vanilla NoiseChunk computes the preliminary surface from
            // `noise_router.initial_density_without_jaggedness` in code (threshold
            // 0.390625, quart-aligned column, cell-height steps). Older converted
            // packs carry an explicit `aquifers.surface_level` find_top_surface or a
            // `chunk_surface_level` router entry; keep those as legacy fallbacks.
            let prelim = router
                .and_then(|r| r.get("initial_density_without_jaggedness"))
                .or_else(|| ns.get("aquifers").and_then(|a| a.get("surface_level")))
                .or_else(|| router.and_then(|r| r.get("chunk_surface_level")));
            if let Some(j) = prelim {
                g.prelim_df = router_df(&mut g, &files, j);
                if g.prelim_df.is_none() {
                    Self::load_trace("preliminary surface density did not resolve");
                }
            }
            if let Some(rule) = ns.get("surface_rule").or_else(|| ns.get("material_rule")) {
                g.surface_rule = g.rule_node(rule, &files, &rfiles, &cfiles, res);
                if g.surface_rule.is_none() {
                    let e = g.surface_rule_load_failure(preset);
                    Self::load_trace(&format!(
                        "noise_settings surface_rule/material_rule did not resolve: {e}"
                    ));
                    return None;
                }
                if let Some(root) = g.surface_rule {
                    g.deep_fast = compile_deep_fast(&g.rules, &g.conds, root, g.min_y, g.height);
                }
            }
        }
        // aquifer config: noise DF ids + positional deriver (minecraft:aquifer)
        let noaq = std::env::var("MEGU_NOAQ").is_ok() || !jbool(&ns, "aquifers_enabled", true);
        // 1.21.1 has no top-level `aquifers`: the same four functions sit in
        // `noise_router` under their own names. 1.21.1's deep-aquifer test reads
        // `erosion` and `depth` (OverworldBiomeBuilder.isDeepDark; NoiseRouter's
        // h()/i() accessors), not an `exclusion` node; a converted pack's
        // `exclusion` is not applied.
        let aq = ns.get("aquifers");
        if !noaq && (aq.is_some() || router.is_some()) {
            let get = |g: &mut Gen, k: &str| -> Option<usize> {
                let j = match aq
                    .and_then(|a| a.get(k))
                    .or_else(|| router.and_then(|r| r.get(k)))
                {
                    Some(j) => j,
                    None => {
                        Self::load_trace(&format!(
                            "no aquifer function {k:?} in aquifers or noise_router"
                        ));
                        return None;
                    }
                };
                router_df(g, &files, j)
            };
            let (b, f, s, l) = (
                get(&mut g, "barrier"),
                get(&mut g, "fluid_level_floodedness"),
                get(&mut g, "fluid_level_spread"),
                get(&mut g, "lava"),
            );
            if let (Some(barrier), Some(flood), Some(spread), Some(lava)) = (b, f, s, l) {
                g.aq = Some(Aqd {
                    barrier,
                    flood,
                    spread,
                    lava,
                    split: g.factory.of_str("minecraft:aquifer").splitter(),
                    water: res("minecraft:water").unwrap_or(80),
                    lava_st: res("minecraft:lava").unwrap_or(1),
                });
            }
        }
        // Exact constant folding over the finished id table, before `is_2d`,
        // the interp classification and the compiled program read it: only
        // position-independent subtrees collapse (see `fold_constant_dfs`), so
        // every evaluator sees the same values with less work per block.
        g.df_folded = if fold_dfs {
            fold_constant_dfs(&mut g.dfs)
        } else {
            0
        };
        // Compile `final_density` once, now that the id table is final.
        g.is_2d = compute_is_2d(&g.dfs);
        let density_ids = g.interp_ids_from(&[g.final_density]);
        let density_set: HashSet<usize> = density_ids.iter().copied().collect();
        g.interp_nodes = g
            .interp_ids()
            .into_iter()
            .filter_map(|id| match g.dfs[id] {
                DF::Interp(inner) => Some((id, inner)),
                _ => None,
            })
            .collect();
        let mut density_interp_nodes = Vec::new();
        let mut vein_interp_nodes = Vec::new();
        for &(id, inner) in &g.interp_nodes {
            let kind = classify_interp_inner(&g.dfs, inner);
            if density_set.contains(&id) {
                density_interp_nodes.push((id, kind));
            } else {
                vein_interp_nodes.push((id, kind));
            }
        }
        g.density_interp_nodes = density_interp_nodes;
        g.vein_interp_nodes = vein_interp_nodes;
        g.upper_bound_interp = detect_upper_bound_interp(&g.dfs, g.final_density);
        g.dfprog = DfProg::compile(&g);
        // BOT-DF-BATCH: the `interpolated` inputs compiled for the corner-grid
        // column sweep (a hoisted 2D prefix per column plus a 3D tail per level).
        g.dfcols = DfCols::compile(&g, &general_roots(&g.density_interp_nodes));
        g.vfcols = DfCols::compile(&g, &general_roots(&g.vein_interp_nodes));
        // Diagnostic counters sized to this generator's node table (WG_COUNT only).
        if std::env::var_os("WG_COUNT").is_some() {
            g.df_hits_init();
        }
        // Post-load RAM trim. `names`, `noise_names`, `rule_ids` and
        // `cond_ids` exist for JSON loading; their only post-load readers are
        // the debug/test resolvers (`resolve_ref`/`noise_id`), which re-parse
        // and re-insert on demand. `bleaf` stays: `biome_leaf_of_name` is a
        // runtime lookup. Every Vec below is final here, so its spare capacity
        // is reclaimed once.
        let mut trimmed = 0usize;
        trimmed += str_map_trim_bytes(&mut g.names);
        trimmed += str_map_trim_bytes(&mut g.noise_names);
        trimmed += str_map_trim_bytes(&mut g.rule_ids);
        trimmed += str_map_trim_bytes(&mut g.cond_ids);
        trimmed += vec_trim_bytes(&mut g.dfs);
        trimmed += vec_trim_bytes(&mut g.noises);
        trimmed += vec_trim_bytes(&mut g.is_2d);
        trimmed += vec_trim_bytes(&mut g.rules);
        trimmed += vec_trim_bytes(&mut g.conds);
        trimmed += vec_trim_bytes(&mut g.leaf_ps);
        trimmed += vec_trim_bytes(&mut g.biome_names);
        trimmed += vec_trim_bytes(&mut g.biome_ids);
        trimmed += vec_trim_bytes(&mut g.biome_weather);
        trimmed += vec_trim_bytes(&mut g.interp_nodes);
        trimmed += vec_trim_bytes(&mut g.density_interp_nodes);
        trimmed += vec_trim_bytes(&mut g.vein_interp_nodes);
        g.ram_trimmed = trimmed;
        Some(g)
    }

    // ---- material rule parsing ----

    fn block_state(j: &J, res: fn(&str) -> Option<u32>) -> Option<u32> {
        let Some((name, props)) = bs_parts(j) else {
            Self::load_trace(&format!("invalid surface block state {j:?}"));
            return None;
        };
        let Some(default) = res(&name) else {
            Self::load_trace(&format!("unknown surface block {name:?}"));
            return None;
        };
        // Keep state/property decoding with the registry, rather than a second
        // surface-only list of state offsets. The callback still resolves names.
        match props {
            Some(props) => Some(breg()?.state_of(&name, Some(props))),
            None => Some(default),
        }
    }

    /// The `(id, reason)` pair a failed surface-rule parse leaves for the load
    /// site: the innermost failure is the most precise, so the first writer
    /// wins and later `?` propagations only keep it.
    fn note_rule_diag(&mut self, id: String, reason: String) {
        if self.rule_diag.is_none() {
            self.rule_diag = Some((id, reason));
        }
    }

    /// Commit the pending surface-rule diagnostic into the process-wide report
    /// and return the error; called at the one place a failed rule load gives
    /// up. The surface rule is the generator's only surface pipeline, so the
    /// load already refuses: this makes the refusal name what it lost.
    fn surface_rule_load_failure(&mut self, preset: NoisePreset) -> WorldgenParseError {
        let (id, reason) = self.rule_diag.take().unwrap_or_else(|| {
            (
                format!("minecraft:{}", preset.name()),
                "surface_rule/material_rule did not resolve".to_string(),
            )
        });
        record_worldgen_parse_error("surface_rule", id, reason)
    }

    fn resolve_rule(
        &mut self,
        name: &str,
        files: &HashMap<String, Vec<u8>>,
        rf: &HashMap<String, Vec<u8>>,
        cf: &HashMap<String, Vec<u8>>,
        res: fn(&str) -> Option<u32>,
    ) -> Option<usize> {
        if let Some(&i) = self.rule_ids.get(name) {
            return Some(i);
        }
        let Some(j) = rf.get(name).and_then(|b| jparse(b)) else {
            Self::load_trace(&format!(
                "surface rule reference {name:?}: missing or invalid JSON"
            ));
            self.note_rule_diag(name.to_string(), "missing or invalid JSON".to_string());
            return None;
        };
        let idx = self.rules.len();
        self.rule_ids.insert(name.to_string(), idx);
        self.rules.push(Rule::Block(0)); // placeholder for recursion
        let Some(r) = self.parse_rule(&j, files, rf, cf, res) else {
            self.rule_ids.remove(name);
            Self::load_trace(&format!("surface rule reference {name:?}: did not parse"));
            self.note_rule_diag(name.to_string(), "did not parse".to_string());
            return None;
        };
        self.rules[idx] = r;
        Some(idx)
    }

    fn rule_node(
        &mut self,
        j: &J,
        files: &HashMap<String, Vec<u8>>,
        rf: &HashMap<String, Vec<u8>>,
        cf: &HashMap<String, Vec<u8>>,
        res: fn(&str) -> Option<u32>,
    ) -> Option<usize> {
        match j {
            J::Str(s) => self.resolve_rule(s, files, rf, cf, res),
            J::Obj(_) => {
                let Some(r) = self.parse_rule(j, files, rf, cf, res) else {
                    Self::load_trace(&format!("surface rule did not parse: {j:?}"));
                    self.note_rule_diag(
                        format!("minecraft:{}", self.preset.name()),
                        "surface rule node did not parse".to_string(),
                    );
                    return None;
                };
                self.rules.push(r);
                Some(self.rules.len() - 1)
            }
            _ => {
                self.note_rule_diag(
                    format!("minecraft:{}", self.preset.name()),
                    "surface rule node is not a name or object".to_string(),
                );
                None
            }
        }
    }

    fn parse_rule(
        &mut self,
        j: &J,
        files: &HashMap<String, Vec<u8>>,
        rf: &HashMap<String, Vec<u8>>,
        cf: &HashMap<String, Vec<u8>>,
        res: fn(&str) -> Option<u32>,
    ) -> Option<Rule> {
        let ty = j.get("type")?.str().strip_prefix("minecraft:")?.to_string();
        Some(match ty.as_str() {
            "sequence" => Rule::Seq(
                j.get("sequence")?
                    .as_arr()?
                    .iter()
                    .map(|c| self.rule_node(c, files, rf, cf, res))
                    .collect::<Option<_>>()?,
            ),
            "condition" => Rule::Cond(
                self.cond_node(j.get("if_true")?, cf)?,
                self.rule_node(j.get("then_run")?, files, rf, cf, res)?,
            ),
            "block" => Rule::Block(Self::block_state(j.get("result_state")?, res)?),
            "bandlands" => Rule::Bandlands,
            "ore_vein" => {
                // 1.21.1 has no `material_rule`; when the 1.21.1 sampler is
                // active (ore_veins_enabled), the 1.21.2+ rule would place a
                // second vein on top of it.
                if self.vein.is_some() {
                    return Some(Rule::Seq(Box::new([])));
                }
                let density = self.node(j.get("density")?, files)?;
                // bounds check fast-path: density root is range_choice on minecraft:y
                let (mut ymin, mut ymax) = (i32::MIN, i32::MAX);
                let yid = self.names.get("minecraft:y").copied();
                if let (Some(yid), DF::RangeChoice { i, mi, ma, .. }) = (yid, &self.dfs[density]) {
                    if *i == yid {
                        ymin = *mi as i32;
                        ymax = *ma as i32;
                    }
                }
                Rule::OreVein {
                    density,
                    rich: self.node(j.get("richness")?, files)?,
                    gap: self.node(j.get("filler_gap")?, files)?,
                    ore: res(j.get("ore_block")?.str()).unwrap_or(self.def_st),
                    raw: res(j.get("raw_ore_block")?.str()).unwrap_or(self.def_st),
                    fill: res(j.get("filler_block")?.str()).unwrap_or(self.def_st),
                    rawc: j.get("raw_ore_chance")?.num() as f32,
                    ymin,
                    ymax,
                }
            }
            _ => {
                Self::load_trace(&format!("unknown surface rule type {ty:?}"));
                self.note_rule_diag(
                    format!("minecraft:{}", self.preset.name()),
                    format!("unknown surface rule type {ty:?}"),
                );
                return None;
            }
        })
    }

    fn resolve_cond(&mut self, name: &str, cf: &HashMap<String, Vec<u8>>) -> Option<usize> {
        // `minecraft:not_underwater` is a built-in ConditionSource, not a datapack
        // file. Its condition (`SurfaceRules$WaterConditionSource$1WaterCondition`)
        // is already "not underwater": waterHeight == MIN, or blockY + stoneDepth
        // above the water surface. Negating it put grass on submerged floors.
        if name == "minecraft:not_underwater" || name == "not_underwater" {
            self.conds.push(Cond::Water {
                off: 0,
                mult: 0,
                sd: false,
            });
            return Some(self.conds.len() - 1);
        }
        if let Some(&i) = self.cond_ids.get(name) {
            return Some(i);
        }
        let Some(j) = cf.get(name).and_then(|b| jparse(b)) else {
            Self::load_trace(&format!(
                "surface condition reference {name:?}: missing or invalid JSON"
            ));
            self.note_rule_diag(
                name.to_string(),
                "surface condition: missing or invalid JSON".to_string(),
            );
            return None;
        };
        let idx = self.conds.len();
        self.cond_ids.insert(name.to_string(), idx);
        self.conds.push(Cond::False);
        let Some(c) = self.parse_cond(&j, cf) else {
            self.cond_ids.remove(name);
            Self::load_trace(&format!(
                "surface condition reference {name:?}: did not parse"
            ));
            self.note_rule_diag(name.to_string(), "surface condition did not parse".to_string());
            return None;
        };
        self.conds[idx] = c;
        Some(idx)
    }

    fn cond_node(&mut self, j: &J, cf: &HashMap<String, Vec<u8>>) -> Option<usize> {
        match j {
            J::Str(s) => self.resolve_cond(s, cf),
            J::Obj(_) => {
                let c = self.parse_cond(j, cf)?;
                self.conds.push(c);
                Some(self.conds.len() - 1)
            }
            _ => {
                self.note_rule_diag(
                    format!("minecraft:{}", self.preset.name()),
                    "surface condition is not a name or object".to_string(),
                );
                None
            }
        }
    }

    fn parse_cond(&mut self, j: &J, cf: &HashMap<String, Vec<u8>>) -> Option<Cond> {
        let ty = j.get("type")?.str().strip_prefix("minecraft:")?.to_string();
        let n = |k: &str| j.get(k).map(J::num).unwrap_or(0.0);
        let bj = |k: &str| matches!(j.get(k), Some(J::Num(v)) if *v != 0.0);
        let ya = |k: &str| -> Option<YA> {
            let v = j.get(k)?;
            if let Some(x) = v.get("absolute") {
                Some(YA::Abs(x.num() as i32))
            } else if let Some(x) = v.get("above_bottom") {
                Some(YA::AB(x.num() as i32))
            } else {
                v.get("below_top").map(|x| YA::BT(x.num() as i32))
            }
        };
        Some(match ty.as_str() {
            "biome" => {
                let mut mask = 0u128;
                let mut one = |s: &str| {
                    // Unknown biomes and leaves >= 128 leave their bit unset: the
                    // condition then matches nothing, and nothing panics on a shift
                    // that would overflow the old `u64` mask.
                    if let Some(&l) = self.bleaf.get(s) {
                        if usize::from(l) < 128 {
                            mask |= 1u128 << usize::from(l);
                        }
                    }
                };
                match j.get("biome_is")? {
                    J::Str(s) => one(s),
                    J::Arr(a) => {
                        for e in a {
                            one(e.str());
                        }
                    }
                    _ => {}
                }
                if mask == 0 {
                    Cond::False
                } else {
                    Cond::Biome(mask)
                }
            }
            "noise_threshold" => Cond::NoiseTh {
                ni: self.noise_id(j.get("noise")?.str())?,
                is3d: bj("is_3d"),
                lo: n("min_threshold"),
                hi: n("max_threshold"),
            },
            "vertical_gradient" => {
                // Guava Longs.fromBytes (vanilla StringHash.fromString) is
                // big-endian; `Split::of_str` already reads it that way.
                let h = md5(j.get("random_name")?.str().as_bytes());
                let lo_w = u64::from_be_bytes(h[0..8].try_into().unwrap());
                let hi_w = u64::from_be_bytes(h[8..16].try_into().unwrap());
                let mut xr = Xor::raw(lo_w ^ self.factory.lo, hi_w ^ self.factory.hi);
                let sp = xr.splitter();
                Cond::VGrad {
                    legacy_seed: legacy_named_factory_seed(
                        self.world_seed,
                        j.get("random_name")?.str(),
                    ),
                    lo_w,
                    hi_w,
                    sp,
                    t: ya("true_at_and_below")?,
                    f: ya("false_at_and_above")?,
                }
            }
            "y_above" => Cond::YAbove {
                a: ya("anchor")?,
                mult: n("surface_depth_multiplier") as i32,
                sd: bj("add_stone_depth"),
            },
            "water" => Cond::Water {
                off: n("offset") as i32,
                mult: n("surface_depth_multiplier") as i32,
                sd: bj("add_stone_depth"),
            },
            "stone_depth" => Cond::StoneD {
                off: n("offset") as i32,
                sdep: bj("add_surface_depth"),
                sec: n("secondary_depth_range") as i32,
                ceil: j.get("surface_type").map(J::str) == Some("ceiling"),
            },
            // The condition is already "not underwater" (see resolve_cond).
            "not_underwater" => Cond::Water {
                off: 0,
                mult: 0,
                sd: false,
            },
            "not" => Cond::Not(self.cond_node(j.get("invert")?, cf)?),
            "hole" => Cond::Hole,
            "above_preliminary_surface" => Cond::Prelim,
            "steep" => Cond::Steep,
            "temperature" => Cond::Temperature,
            _ => {
                Self::load_trace(&format!("unknown surface condition type {ty:?}"));
                self.note_rule_diag(
                    format!("minecraft:{}", self.preset.name()),
                    format!("unknown surface condition type {ty:?}"),
                );
                return None;
            }
        })
    }

    fn resolve_ref(&mut self, files: &HashMap<String, Vec<u8>>, name: &str) -> Option<usize> {
        if let Some(&i) = self.names.get(name) {
            return Some(i);
        }
        let trace = |why: &str| {
            if std::env::var("LOAD_TRACE").is_ok() {
                eprintln!("[load] density reference {name}: {why}");
            }
        };
        let b = match files.get(name) {
            Some(b) => b,
            None => {
                trace("no such file in the pack");
                return None;
            }
        };
        let j = match jparse(b) {
            Some(j) => j,
            None => {
                trace("the file is not valid JSON");
                return None;
            }
        };
        let idx = self.dfs.len();
        self.names.insert(name.to_string(), idx);
        self.dfs.push(DF::Zero); // placeholder for recursion safety
        let d = match self.parse_df(&j, files) {
            Some(d) => d,
            None => {
                // A failed parse must not leave this name pointing at the
                // recursion placeholder: a retry has to reach `parse_df` again
                // instead of silently resolving to `DF::Zero`.
                self.names.remove(name);
                trace("its own definition did not parse");
                return None;
            }
        };
        self.dfs[idx] = d;
        Some(idx)
    }

    fn node(&mut self, j: &J, files: &HashMap<String, Vec<u8>>) -> Option<usize> {
        match j {
            J::Num(n) => {
                self.dfs.push(DF::C(*n));
                Some(self.dfs.len() - 1)
            }
            J::Str(s) => self.resolve_ref(files, s),
            J::Obj(_) => {
                let d = match self.parse_df(j, files) {
                    Some(d) => d,
                    None => {
                        Self::load_trace(&format!(
                            "density node type {:?} did not parse",
                            j.get("type").map(|t| t.str()).unwrap_or("<none>")
                        ));
                        return None;
                    }
                };
                self.dfs.push(d);
                Some(self.dfs.len() - 1)
            }
            _ => None,
        }
    }

    fn noise_id(&mut self, name: &str) -> Option<usize> {
        if let Some(&i) = self.noise_names.get(name) {
            return Some(i);
        }
        if self.legacy_random_source {
            let noise = match name {
                "minecraft:temperature" => Some(normal_legacy_nether(self.world_seed)),
                "minecraft:vegetation" => {
                    Some(normal_legacy_nether(self.world_seed.wrapping_add(1)))
                }
                "minecraft:offset" => Some(normal_new_amps(&mut Xor::seed(0), 0, &[0.0])),
                _ => None,
            };
            if let Some(noise) = noise {
                let id = self.noises.len();
                self.noises.push(noise);
                self.noise_names.insert(name.to_owned(), id);
                return Some(id);
            }
        }
        let path = format!(
            "{}/data/minecraft/worldgen/noise/{}.json",
            self.base,
            name.rsplit(':').next().unwrap_or(name)
        );
        let b = std::fs::read(&path).ok()?;
        let j = jparse(&b)?;
        // 1.21.1 noise files: `firstOctave` + `amplitudes`. The converted pack re-encoded
        // the same octaves as `base_octave`/`base_amplitude`/`octave_count`; both load,
        // and both construct the noise the modern way (see `normal_new_amps`).
        if let Some(amps) = j.get("amplitudes").and_then(|a| a.as_arr()) {
            let first = j
                .get("firstOctave")
                .or_else(|| j.get("first_octave"))
                .map(|v| v.num())
                .unwrap_or(-4.0) as i32;
            let amps: Vec<f64> = amps.iter().map(|v| v.num()).collect();
            let mut rng = self.factory.of_str(name);
            let n = if self.legacy_random_source {
                normal_legacy_named(self.world_seed, name, first, &amps)
            } else {
                crate::generation_math::normal_new_amps(&mut rng, first, &amps)
            };
            self.noises.push(n);
            let id = self.noises.len() - 1;
            self.noise_names.insert(name.to_string(), id);
            return Some(id);
        }
        let base_octave = j.get("base_octave")?.num() as i32;
        let base_amplitude = j.get("base_amplitude")?.num();
        // absent modifiers = single implicit octave (vanilla amplitudes [1.0])
        let mods: Vec<f64> = match j.get("amplitude_modifiers") {
            Some(J::Arr(a)) => a.iter().map(J::num).collect(),
            _ => vec![1.0; j.get("octave_count").map(J::num).unwrap_or(1.0).max(1.0) as usize],
        };
        let mut rng = self.factory.of_str(name);
        let n = normal_new(&mut rng, base_octave, base_amplitude, &mods);
        self.noises.push(n);
        let id = self.noises.len() - 1;
        self.noise_names.insert(name.to_string(), id);
        Some(id)
    }

    fn parse_spline(&mut self, j: &J, files: &HashMap<String, Vec<u8>>) -> Option<Spl> {
        let coord = self.node(j.get("coordinate")?, files)?;
        let mut pts = Vec::new();
        for p in j.get("points")?.as_arr()? {
            let loc = p.get("location")?.num() as f32;
            let der = p.get("derivative").map(J::num).unwrap_or(0.0) as f32;
            let val = match p.get("value")? {
                J::Num(n) => SV::V(*n as f32),
                x @ J::Obj(_) => SV::S(Box::new(self.parse_spline(x, files)?)),
                _ => return None,
            };
            pts.push((loc, val, der));
        }
        Some(Spl { coord, pts })
    }

    fn parse_df(&mut self, j: &J, files: &HashMap<String, Vec<u8>>) -> Option<DF> {
        let ty = j.get("type")?.str().strip_prefix("minecraft:")?.to_string();
        // The loader silently dropping a node looks exactly like a wrong world, so
        // `LOAD_TRACE=1` names the type it could not build (CV2: print what was skipped).
        let trace = |ty: &str, ok: bool| {
            if !ok && std::env::var("LOAD_TRACE").is_ok() {
                eprintln!("[load] density function type not built: {ty}");
            }
        };
        let n = |k: &str| j.get(k).map(J::num).unwrap_or(0.0);
        let sub = |g: &mut Gen, k: &str| -> Option<usize> { g.node(j.get(k)?, files) };
        // 1.21.1 names the two operands `argument1`/`argument2`, the newer dialect
        // `left`/`right`. Same operation, same order, so accept either.
        let either = |g: &mut Gen, k: &str, alt: &str| -> Option<usize> {
            g.node(j.get(k).or_else(|| j.get(alt))?, files)
        };
        let out = Some(match ty.as_str() {
            "add" => DF::Add(
                either(self, "left", "argument1")?,
                either(self, "right", "argument2")?,
            ),
            "sub" => DF::Sub(
                either(self, "left", "argument1")?,
                either(self, "right", "argument2")?,
            ),
            "mul" => DF::Mul(
                either(self, "left", "argument1")?,
                either(self, "right", "argument2")?,
            ),
            "min" => DF::Min(
                either(self, "left", "argument1")?,
                either(self, "right", "argument2")?,
            ),
            "max" => DF::Max(
                either(self, "left", "argument1")?,
                either(self, "right", "argument2")?,
            ),
            "div" => DF::Div(
                either(self, "left", "argument1")?,
                either(self, "right", "argument2")?,
            ),
            "negate" => DF::Neg(either(self, "input", "argument")?),
            "abs" => DF::Abs(either(self, "input", "argument")?),
            "square" => DF::Sq(either(self, "input", "argument")?),
            "cube" => DF::Cube(either(self, "input", "argument")?),
            "half_negative" => DF::HNeg(either(self, "input", "argument")?),
            "quarter_negative" => DF::QNeg(either(self, "input", "argument")?),
            "squeeze" => DF::Squeeze(either(self, "input", "argument")?),
            "clamp" => DF::Clamp {
                i: either(self, "input", "argument")?,
                lo: n("min"),
                hi: n("max"),
            },
            "lerp" => DF::Lerp {
                a: sub(self, "alpha")?,
                f: sub(self, "first")?,
                s: sub(self, "second")?,
            },
            "gradient" => DF::Grad {
                fc: n("from_coordinate"),
                fv: n("from_value"),
                tc: n("to_coordinate"),
                tv: n("to_value"),
            },
            // 1.21.1's name for the y gradient, with `from_y`/`to_y`. Both clamp.
            "y_clamped_gradient" => DF::Grad {
                fc: n("from_y"),
                fv: n("from_value"),
                tc: n("to_y"),
                tv: n("to_value"),
            },
            "interpolated" => DF::Interp(either(self, "input", "argument")?),
            "cache" | "flat_cache" | "cache_2d" | "cache_once" | "cache_all_in_cell" => {
                DF::Cache(either(self, "input", "argument")?)
            }
            "noise" => {
                let zero = {
                    self.dfs.push(DF::C(0.0));
                    self.dfs.len() - 1
                };
                // For this dialect, omitted shifts mean zero, but a present
                // unparseable shift is not equivalent to an omitted one.
                let shift = |g: &mut Gen, k: &str| -> Option<usize> {
                    match j.get(k) {
                        Some(value) => g.node(value, files),
                        None => Some(zero),
                    }
                };
                DF::Noise {
                    ni: self.noise_id(j.get("noise")?.str())?,
                    sx: shift(self, "shift_x")?,
                    sy: shift(self, "shift_y")?,
                    sz: shift(self, "shift_z")?,
                    xzs: n("xz_scale"),
                    ys: n("y_scale"),
                }
            }
            // Native 1.21.1 shifted_noise specifies all three density shifts.
            // Reading shift_y as constant zero was only correct by coincidence
            // for the currently pinned vanilla data (whose shift_y is zero).
            "shifted_noise" => DF::Noise {
                ni: self.noise_id(j.get("noise")?.str())?,
                sx: self.node(j.get("shift_x")?, files)?,
                sy: self.node(j.get("shift_y")?, files)?,
                sz: self.node(j.get("shift_z")?, files)?,
                xzs: n("xz_scale"),
                ys: n("y_scale"),
            },
            // 1.21.1 names it in `argument`, the newer dialect in `noise`; either way it
            // is a *noise* name (`minecraft:offset`), not a density-function reference.
            "shift_a" | "shift_x" => {
                DF::ShiftA(self.noise_id(j.get("noise").or_else(|| j.get("argument"))?.str())?)
            }
            "shift_b" | "shift_z" => {
                DF::ShiftB(self.noise_id(j.get("noise").or_else(|| j.get("argument"))?.str())?)
            }
            "weird_scaled_sampler" => {
                let name = j
                    .get("rarity_value_mapper")
                    .map(J::str)
                    .unwrap_or("<missing>");
                let mapper = RarityValueMapper::parse(name).or_else(|| {
                    Self::load_trace(&format!(
                        "weird_scaled_sampler invalid rarity_value_mapper {name:?}"
                    ));
                    None
                })?;
                let name = j.get("noise").map(J::str).unwrap_or("<missing>");
                let ni = self.noise_id(name).or_else(|| {
                    Self::load_trace(&format!("weird_scaled_sampler unresolved noise {name:?}"));
                    None
                })?;
                DF::WeirdScaledSampler {
                    input: either(self, "input", "argument")?,
                    ni,
                    mapper,
                }
            }
            "range_choice" => DF::RangeChoice {
                i: either(self, "input", "argument")?,
                mi: n("min_inclusive"),
                ma: n("max_exclusive"),
                wr: sub(self, "when_in_range")?,
                wo: sub(self, "when_out_of_range")?,
            },
            "interval_select" => {
                let mut th = vec![];
                let mut fns = vec![];
                for f in j.get("functions")?.as_arr()? {
                    fns.push(self.node(f, files)?);
                }
                for t in j.get("thresholds")?.as_arr()? {
                    th.push(t.num());
                }
                DF::IntervalSelect {
                    i: either(self, "input", "argument")?,
                    th,
                    fns,
                }
            }
            "spline" => DF::Spline(Box::new(self.parse_spline(j.get("spline")?, files)?)),
            "old_blended_noise" => {
                let xzs = n("xz_scale");
                let ys = n("y_scale");
                let xzf = n("xz_factor");
                let yf = n("y_factor");
                let smear = n("smear_scale_multiplier");
                DF::Blended(Box::new(if self.legacy_random_source {
                    BlendedNoise::legacy(self.world_seed, xzs, ys, xzf, yf, smear)
                } else {
                    let mut rng = self.factory.of_str("minecraft:terrain");
                    BlendedNoise::modern(&mut rng, xzs, ys, xzf, yf, smear)
                }))
            }
            "blend_density" => DF::BlendD(either(self, "input", "argument")?),
            "blend_alpha" => DF::C(1.0),
            "end_islands" => DF::EndIslands(Box::new(EndIslands::new(self.world_seed))),
            "beardifier" => DF::Beardifier,
            "blend_offset" | "end_outer_islands" | "distance_to_point" | "slice" => DF::Zero,
            "find_top_surface" => DF::FindTopSurface {
                density: sub(self, "density")?,
                lower: n("lower_bound") as i32,
                upper: sub(self, "upper_bound")?,
                cell_h: n("cell_height") as i32,
            },
            _ => {
                // An unrecognized required density function is not a constant zero.
                // Returning None propagates through node/resolve_ref into the
                // noise-router loader instead of silently changing the terrain.
                Self::load_trace(&format!("unsupported density function type {ty:?}"));
                return None;
            }
        });
        trace(&ty, out.is_some());
        out
    }

    pub(crate) fn eval(
        &self,
        id: usize,
        x: i32,
        y: i32,
        z: i32,
        memo: &mut HashMap<(usize, i32, i32, i32), f64>,
    ) -> f64 {
        self.eval_c(id, x, y, z, memo, None)
    }

    /// Evaluate a density function. `cc` supplies the per-chunk interpolation
    /// grids: with it, an `interpolated` node returns the trilinear value of its
    /// grid instead of evaluating its input at block resolution (vanilla
    /// NoiseChunk). While the grids are being filled `cc` is None, which is a
    /// corner sample.
    fn eval_c(
        &self,
        id: usize,
        x: i32,
        y: i32,
        z: i32,
        memo: &mut HashMap<(usize, i32, i32, i32), f64>,
        cc: Option<&ChunkCols>,
    ) -> f64 {
        if wg_count() {
            EVAL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(h) = DF_HITS.get().and_then(|hits| hits.get(id)) {
                h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Some(cc) = cc {
            if let DF::Interp(inner) = self.dfs[id] {
                if let Some(v) = cc.interp(id, x, y, z) {
                    return v;
                }
                return self.eval_c(inner, x, y, z, memo, Some(cc));
            }
        }
        if let DF::Cache(_) = self.dfs[id] {
            if let Some(&v) = memo.get(&(id, x, y, z)) {
                return v;
            }
        }
        let v = self.eval0_c(id, x, y, z, memo, cc);
        if let DF::Cache(_) = self.dfs[id] {
            memo.insert((id, x, y, z), v);
        }
        v
    }

    /// Terrain density at a block position, with `interpolated` resolved from the
    /// chunk's corner grids.
    pub(crate) fn block_density(&self, cc: &ChunkCols, x: i32, y: i32, z: i32) -> f64 {
        if let Some(prog) = self.dfprog.as_ref() {
            return prog.eval(self, Some(cc), x, y, z);
        }
        let mut memo = HashMap::new();
        self.eval_c(self.final_density, x, y, z, &mut memo, Some(cc))
    }

    /// [`Gen::block_density`] at the four consecutive columns `x..x+4`, all at one `(y, z)`.
    ///
    /// The four values are the four `block_density` values bit for bit (see `DF4`); the
    /// vector arm exists because the terrain stage asks for the same row of a section 16
    /// blocks at a time. A generator that kept the interpreter
    /// (`MEGU_DF_INTERP=1`) answers the four positions through it, so the A/B switch
    /// keeps meaning what it meant.
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn block_density_x4(&self, cc: &ChunkCols, x: i32, y: i32, z: i32) -> [f64; DF4] {
        if let Some(prog) = self.dfprog.as_ref() {
            return prog.eval_x4(self, Some(cc), x, y, z);
        }
        let mut out = [0.0f64; DF4];
        for (k, o) in out.iter_mut().enumerate() {
            let mut memo = HashMap::new();
            *o = self.eval_c(self.final_density, x + k as i32, y, z, &mut memo, Some(cc));
        }
        out
    }

    /// [`Gen::block_density`] at the eight consecutive columns `x..x+8` (layer A2-c). The
    /// values are eight `block_density` values bit for bit; the vector arm exists because the
    /// terrain stage asks for a whole 16-wide row of a section. A generator that kept the
    /// interpreter (`MEGU_DF_INTERP=1`) answers the eight positions through it, so the A/B
    /// switch keeps meaning what it meant.
    pub(crate) fn block_density_x8(&self, cc: &ChunkCols, x: i32, y: i32, z: i32) -> [f64; DF8] {
        if let Some(prog) = self.dfprog.as_ref() {
            return prog.eval_x8(self, Some(cc), x, y, z);
        }
        let mut out = [0.0f64; DF8];
        for (k, o) in out.iter_mut().enumerate() {
            let mut memo = HashMap::new();
            *o = self.eval_c(self.final_density, x + k as i32, y, z, &mut memo, Some(cc));
        }
        out
    }

    /// Density-function ids a node reads, for the grid reachability walk.
    fn push_kids(&self, id: usize, out: &mut Vec<usize>) {
        df_kids(&self.dfs[id], out);
    }

    /// `interpolated` nodes reachable from `roots`.
    fn interp_ids_from(&self, roots: &[usize]) -> Vec<usize> {
        let mut seen = vec![false; self.dfs.len()];
        let mut out = Vec::new();
        let mut kids = Vec::new();
        let mut stack = roots.to_vec();
        while let Some(id) = stack.pop() {
            if seen[id] {
                continue;
            }
            seen[id] = true;
            if matches!(self.dfs[id], DF::Interp(_)) {
                out.push(id);
            }
            kids.clear();
            self.push_kids(id, &mut kids);
            for &k in &kids {
                if !seen[k] {
                    stack.push(k);
                }
            }
        }
        out
    }

    /// `interpolated` nodes reachable from final_density and veins.
    fn interp_ids(&self) -> Vec<usize> {
        let mut roots = vec![self.final_density];
        if let Some(v) = self.vein {
            roots.extend([v.toggle, v.ridged, v.gap]);
        }
        self.interp_ids_from(&roots)
    }

    fn eval_spline_c(
        &self,
        s: &Spl,
        x: i32,
        y: i32,
        z: i32,
        memo: &mut HashMap<(usize, i32, i32, i32), f64>,
        cc: Option<&ChunkCols>,
    ) -> f64 {
        // CubicSpline stores locations/derivatives/constants as f32 and casts
        // the coordinate to f32; interpolation is float arithmetic (Mth.lerp).
        let loc = self.eval_c(s.coord, x, y, z, memo, cc) as f32;
        let n = s.pts.len();
        if n == 0 {
            return 0.0;
        }
        let mut sv = |sv: &SV| -> f32 {
            match sv {
                SV::V(v) => *v,
                SV::S(sp) => self.eval_spline_c(sp, x, y, z, memo, cc) as f32,
            }
        };
        let idx = s.pts.partition_point(|p| loc >= p.0) as isize - 1;
        if idx < 0 || idx == n as isize - 1 {
            let p = if idx < 0 { &s.pts[0] } else { &s.pts[n - 1] };
            let v = sv(&p.1);
            let out = if p.2 == 0.0 { v } else { v + p.2 * (loc - p.0) };
            return out as f64;
        }
        let i = idx as usize;
        let (x0, v0, d0) = (s.pts[i].0, sv(&s.pts[i].1), s.pts[i].2);
        let (x1, v1, d1) = (s.pts[i + 1].0, sv(&s.pts[i + 1].1), s.pts[i + 1].2);
        let t = (loc - x0) / (x1 - x0);
        let dx = x1 - x0;
        let d0t = d0 * dx - (v1 - v0);
        let d1t = -d1 * dx + (v1 - v0);
        let lerp = |delta: f32, start: f32, end: f32| start + delta * (end - start);
        (lerp(t, v0, v1) + t * (1.0 - t) * lerp(t, d0t, d1t)) as f64
    }

    fn eval0_c(
        &self,
        id: usize,
        x: i32,
        y: i32,
        z: i32,
        memo: &mut HashMap<(usize, i32, i32, i32), f64>,
        cc: Option<&ChunkCols>,
    ) -> f64 {
        match &self.dfs[id] {
            DF::C(v) => *v,
            DF::EndIslands(field) => field.density(x, z),
            DF::Zero => 0.0,
            // Vanilla substitutes the chunk's `Beardifier` for the marker in
            // `NoiseChunk.wrap`. Without columns (the `interpolated` corner samples,
            // and `getBaseHeight`/preliminary-surface queries) the marker itself is
            // what vanilla evaluates, and it is zero.
            DF::Beardifier => match cc.and_then(|cc| cc.beard.as_deref()) {
                Some(beard) => compute_beardifier(beard, x, y, z),
                None => 0.0,
            },
            DF::Add(a, b) => {
                self.eval_c(*a, x, y, z, memo, cc) + self.eval_c(*b, x, y, z, memo, cc)
            }
            DF::Sub(a, b) => {
                self.eval_c(*a, x, y, z, memo, cc) - self.eval_c(*b, x, y, z, memo, cc)
            }
            DF::Mul(a, b) => {
                self.eval_c(*a, x, y, z, memo, cc) * self.eval_c(*b, x, y, z, memo, cc)
            }
            DF::Min(a, b) => df_min(
                self.eval_c(*a, x, y, z, memo, cc),
                self.eval_c(*b, x, y, z, memo, cc),
            ),
            DF::Max(a, b) => df_max(
                self.eval_c(*a, x, y, z, memo, cc),
                self.eval_c(*b, x, y, z, memo, cc),
            ),
            DF::Div(a, b) => {
                let d = self.eval_c(*b, x, y, z, memo, cc);
                if d == 0.0 {
                    0.0
                } else {
                    self.eval_c(*a, x, y, z, memo, cc) / d
                }
            }
            DF::Neg(a) => -self.eval_c(*a, x, y, z, memo, cc),
            DF::Abs(a) => self.eval_c(*a, x, y, z, memo, cc).abs(),
            DF::Sq(a) => {
                let v = self.eval_c(*a, x, y, z, memo, cc);
                v * v
            }
            DF::Cube(a) => {
                let v = self.eval_c(*a, x, y, z, memo, cc);
                v * v * v
            }
            DF::HNeg(a) => {
                let v = self.eval_c(*a, x, y, z, memo, cc);
                if v > 0.0 {
                    v
                } else {
                    v * 0.5
                }
            }
            DF::QNeg(a) => {
                let v = self.eval_c(*a, x, y, z, memo, cc);
                if v > 0.0 {
                    v
                } else {
                    v * 0.25
                }
            }
            DF::Squeeze(a) => {
                let v = self.eval_c(*a, x, y, z, memo, cc).clamp(-1.0, 1.0);
                v / 2.0 - v * v * v / 24.0
            }
            DF::Clamp { i, lo, hi } => self.eval_c(*i, x, y, z, memo, cc).clamp(*lo, *hi),
            DF::Lerp { a, f, s } => {
                let alpha = self.eval_c(*a, x, y, z, memo, cc);
                let fv = self.eval_c(*f, x, y, z, memo, cc);
                let sv = self.eval_c(*s, x, y, z, memo, cc);
                fv + alpha * (sv - fv)
            }
            DF::Grad { fc, fv, tc, tv } => {
                let t = ((y as f64) - fc) / (tc - fc);
                fv + t.clamp(0.0, 1.0) * (tv - fv)
            }
            DF::Cache(a) => self.eval_c(*a, x, y, z, memo, cc),
            DF::Interp(a) => self.eval_c(*a, x, y, z, memo, cc),
            DF::Noise {
                ni,
                sx,
                sy,
                sz,
                xzs,
                ys,
            } => {
                let xs = self.eval_c(*sx, x, y, z, memo, cc);
                let ysv = self.eval_c(*sy, x, y, z, memo, cc);
                let zs = self.eval_c(*sz, x, y, z, memo, cc);
                self.noises[*ni].get(
                    x as f64 * xzs + xs,
                    y as f64 * ys + ysv,
                    z as f64 * xzs + zs,
                )
            }
            DF::ShiftA(ni) => self.noises[*ni].get(x as f64 * 0.25, 0.0, z as f64 * 0.25) * 4.0,
            DF::ShiftB(ni) => self.noises[*ni].get(z as f64 * 0.25, x as f64 * 0.25, 0.0) * 4.0,
            DF::WeirdScaledSampler { input, ni, mapper } => {
                let rarity = mapper.rarity(self.eval_c(*input, x, y, z, memo, cc));
                let xd = x as f64 / rarity;
                let yd = y as f64 / rarity;
                let zd = z as f64 / rarity;
                rarity * self.noises[*ni].get(xd, yd, zd).abs()
            }
            DF::RangeChoice { i, mi, ma, wr, wo } => {
                let v = self.eval_c(*i, x, y, z, memo, cc);
                self.eval_c(
                    if v >= *mi && v < *ma { *wr } else { *wo },
                    x,
                    y,
                    z,
                    memo,
                    cc,
                )
            }
            DF::IntervalSelect { i, th, fns } => {
                let v = self.eval_c(*i, x, y, z, memo, cc);
                let mut pick = fns.len() - 1;
                for (k, t) in th.iter().enumerate() {
                    if v < *t {
                        pick = k;
                        break;
                    }
                }
                self.eval_c(fns[pick.min(fns.len() - 1)], x, y, z, memo, cc)
            }
            DF::Spline(s) => self.eval_spline_c(s, x, y, z, memo, cc),
            DF::Blended(noise) => noise.get(x, y, z),
            DF::BlendD(a) => self.eval_c(*a, x, y, z, memo, cc),
            DF::FindTopSurface {
                density,
                lower,
                upper,
                cell_h,
            } => {
                let ub = self.eval_c(*upper, x, 0, z, memo, cc) as i32;
                let mut yy = ub.div_euclid(*cell_h) * *cell_h;
                while yy >= *lower {
                    if self.eval_c(*density, x, yy, z, memo, cc) > 0.0 {
                        return yy as f64;
                    }
                    yy -= cell_h;
                }
                *lower as f64
            }
        }
    }

    /// The flat-memo counterpart of [`Gen::eval`]: bit-identical to `eval_c(_, _, _, _, None)`
    /// but memoizes every y-independent node per `(x, z)` and every `Cache` node per
    /// `(x, y, z)` in a generation-stamped array instead of a `HashMap`.
    ///
    /// Used for grid corners and preliminary surface scans, where `cc` is `None`, so the
    /// `Interp`/`cc` special case in `eval_c` never applies. Diagnostics are deliberately
    /// absent here: when `WG_COUNT` is set `build_cols_body` keeps the old `eval_c` path so
    /// the counters describe the same visits as before.
    fn eval_flat(&self, id: usize, x: i32, y: i32, z: i32, memo: &mut FlatCornerMemo) -> f64 {
        if self.is_2d[id] {
            if memo.gen_2d[id] == memo.cur_2d {
                return memo.val_2d[id];
            }
            let v = self.eval0_flat(id, x, y, z, memo);
            memo.gen_2d[id] = memo.cur_2d;
            memo.val_2d[id] = v;
            return v;
        }
        // Only `Cache` nodes are memoized in 3D: a plain shared subtree is re-evaluated
        // by the reference path too, so memoizing it here would only waste memory.
        if matches!(self.dfs[id], DF::Cache(_)) {
            if memo.gen_3d[id] == memo.cur_3d {
                return memo.val_3d[id];
            }
            let v = self.eval0_flat(id, x, y, z, memo);
            memo.gen_3d[id] = memo.cur_3d;
            memo.val_3d[id] = v;
            return v;
        }
        self.eval0_flat(id, x, y, z, memo)
    }

    /// [`Gen::eval0_c`] with recursive calls routed to [`Gen::eval_flat`].
    ///
    /// The arithmetic is copied verbatim so every operation keeps its IEEE-754 order.
    /// The one deliberate departure is `FindTopSurface`: it is classified 2D, but its
    /// inner scan samples `density` at many `y` values. Those samples go through the
    /// scalar `eval_c` with a fresh `HashMap` so they can never collide with the current
    /// corner's `cur_3d` generation; the node's own result is still memoized in `val_2d`.
    fn eval0_flat(&self, id: usize, x: i32, y: i32, z: i32, memo: &mut FlatCornerMemo) -> f64 {
        match &self.dfs[id] {
            DF::C(v) => *v,
            DF::EndIslands(field) => field.density(x, z),
            DF::Zero => 0.0,
            DF::Beardifier => 0.0, // corner samples have no `cc` (vanilla marker)
            DF::Add(a, b) => self.eval_flat(*a, x, y, z, memo) + self.eval_flat(*b, x, y, z, memo),
            DF::Sub(a, b) => self.eval_flat(*a, x, y, z, memo) - self.eval_flat(*b, x, y, z, memo),
            DF::Mul(a, b) => self.eval_flat(*a, x, y, z, memo) * self.eval_flat(*b, x, y, z, memo),
            DF::Min(a, b) => df_min(
                self.eval_flat(*a, x, y, z, memo),
                self.eval_flat(*b, x, y, z, memo),
            ),
            DF::Max(a, b) => df_max(
                self.eval_flat(*a, x, y, z, memo),
                self.eval_flat(*b, x, y, z, memo),
            ),
            DF::Div(a, b) => {
                let d = self.eval_flat(*b, x, y, z, memo);
                if d == 0.0 {
                    0.0
                } else {
                    self.eval_flat(*a, x, y, z, memo) / d
                }
            }
            DF::Neg(a) => -self.eval_flat(*a, x, y, z, memo),
            DF::Abs(a) => self.eval_flat(*a, x, y, z, memo).abs(),
            DF::Sq(a) => {
                let v = self.eval_flat(*a, x, y, z, memo);
                v * v
            }
            DF::Cube(a) => {
                let v = self.eval_flat(*a, x, y, z, memo);
                v * v * v
            }
            DF::HNeg(a) => {
                let v = self.eval_flat(*a, x, y, z, memo);
                if v > 0.0 {
                    v
                } else {
                    v * 0.5
                }
            }
            DF::QNeg(a) => {
                let v = self.eval_flat(*a, x, y, z, memo);
                if v > 0.0 {
                    v
                } else {
                    v * 0.25
                }
            }
            DF::Squeeze(a) => {
                let v = self.eval_flat(*a, x, y, z, memo).clamp(-1.0, 1.0);
                v / 2.0 - v * v * v / 24.0
            }
            DF::Clamp { i, lo, hi } => self.eval_flat(*i, x, y, z, memo).clamp(*lo, *hi),
            DF::Lerp { a, f, s } => {
                let alpha = self.eval_flat(*a, x, y, z, memo);
                let fv = self.eval_flat(*f, x, y, z, memo);
                let sv = self.eval_flat(*s, x, y, z, memo);
                fv + alpha * (sv - fv)
            }
            DF::Grad { fc, fv, tc, tv } => {
                let t = ((y as f64) - fc) / (tc - fc);
                fv + t.clamp(0.0, 1.0) * (tv - fv)
            }
            DF::Cache(a) => self.eval_flat(*a, x, y, z, memo),
            DF::Interp(a) => self.eval_flat(*a, x, y, z, memo),
            DF::Noise {
                ni,
                sx,
                sy,
                sz,
                xzs,
                ys,
            } => {
                let xs = self.eval_flat(*sx, x, y, z, memo);
                let ysv = self.eval_flat(*sy, x, y, z, memo);
                let zs = self.eval_flat(*sz, x, y, z, memo);
                self.noises[*ni].get(
                    x as f64 * xzs + xs,
                    y as f64 * ys + ysv,
                    z as f64 * xzs + zs,
                )
            }
            DF::ShiftA(ni) => self.noises[*ni].get(x as f64 * 0.25, 0.0, z as f64 * 0.25) * 4.0,
            DF::ShiftB(ni) => self.noises[*ni].get(z as f64 * 0.25, x as f64 * 0.25, 0.0) * 4.0,
            DF::WeirdScaledSampler { input, ni, mapper } => {
                let rarity = mapper.rarity(self.eval_flat(*input, x, y, z, memo));
                let xd = x as f64 / rarity;
                let yd = y as f64 / rarity;
                let zd = z as f64 / rarity;
                rarity * self.noises[*ni].get(xd, yd, zd).abs()
            }
            DF::RangeChoice { i, mi, ma, wr, wo } => {
                let v = self.eval_flat(*i, x, y, z, memo);
                self.eval_flat(if v >= *mi && v < *ma { *wr } else { *wo }, x, y, z, memo)
            }
            DF::IntervalSelect { i, th, fns } => {
                let v = self.eval_flat(*i, x, y, z, memo);
                let mut pick = fns.len() - 1;
                for (k, t) in th.iter().enumerate() {
                    if v < *t {
                        pick = k;
                        break;
                    }
                }
                self.eval_flat(fns[pick.min(fns.len() - 1)], x, y, z, memo)
            }
            DF::Spline(s) => self.eval_spline_flat(s, x, y, z, memo),
            DF::Blended(noise) => noise.get(x, y, z),
            DF::BlendD(a) => self.eval_flat(*a, x, y, z, memo),
            DF::FindTopSurface {
                density,
                lower,
                upper,
                cell_h,
            } => {
                // The inner scan evaluates `density` at other `yy` values; route it
                // through the scalar interpreter with its own memo so those 3D `Cache`
                // lookups cannot be mistaken for this corner's generation.
                let mut sm = HashMap::new();
                let ub = self.eval_c(*upper, x, 0, z, &mut sm, None) as i32;
                let mut yy = ub.div_euclid(*cell_h) * *cell_h;
                while yy >= *lower {
                    if self.eval_c(*density, x, yy, z, &mut sm, None) > 0.0 {
                        return yy as f64;
                    }
                    yy -= cell_h;
                }
                *lower as f64
            }
        }
    }

    /// [`Gen::eval_spline_c`] with recursive calls routed to [`Gen::eval_flat`].
    fn eval_spline_flat(&self, s: &Spl, x: i32, y: i32, z: i32, memo: &mut FlatCornerMemo) -> f64 {
        let loc = self.eval_flat(s.coord, x, y, z, memo) as f32;
        let n = s.pts.len();
        if n == 0 {
            return 0.0;
        }
        let mut sv = |sv: &SV| -> f32 {
            match sv {
                SV::V(v) => *v,
                SV::S(sp) => self.eval_spline_flat(sp, x, y, z, memo) as f32,
            }
        };
        let idx = s.pts.partition_point(|p| loc >= p.0) as isize - 1;
        if idx < 0 || idx == n as isize - 1 {
            let p = if idx < 0 { &s.pts[0] } else { &s.pts[n - 1] };
            let v = sv(&p.1);
            let out = if p.2 == 0.0 { v } else { v + p.2 * (loc - p.0) };
            return out as f64;
        }
        let i = idx as usize;
        let (x0, v0, d0) = (s.pts[i].0, sv(&s.pts[i].1), s.pts[i].2);
        let (x1, v1, d1) = (s.pts[i + 1].0, sv(&s.pts[i + 1].1), s.pts[i + 1].2);
        let t = (loc - x0) / (x1 - x0);
        let dx = x1 - x0;
        let d0t = d0 * dx - (v1 - v0);
        let d1t = -d1 * dx + (v1 - v0);
        let lerp = |delta: f32, start: f32, end: f32| start + delta * (end - start);
        (lerp(t, v0, v1) + t * (1.0 - t) * lerp(t, d0t, d1t)) as f64
    }
}

// ---------- compiled density evaluator ----------
//
// `final_density` is a DAG of ~300 nodes and `block_density` walks it once per
// block: ~1.8M `eval_c` visits per chunk, ~18 per block
// (evidence/foundation/performance/README.md). Almost all of those visits are
// cheap arithmetic behind a full dispatch (an `EVAL_COUNT` gate, two enum
// matches for the `interpolated`/`cache` special cases, then the `eval0_c`
// match and a recursive call), so the cost is the per-node overhead, not the
// arithmetic.
//
// The compiled form linearises the DAG once, per generator, into a post-order
// instruction list evaluated by a stack machine: no per-node `match`, no node
// id to index, no recursion, and constants folded into the instruction stream.
// That is the shape the Java JIT optimises (vanilla interprets a
// `DensityFunction` tree) and the shape hyperchunk's `df_compile.c` walks.
//
// What it must preserve, and why each is not "close enough":
//
// * **Operand order per node.** `Div` evaluates its divisor first and returns
//   `0.0` *without* evaluating the dividend when it is zero; `Lerp` evaluates
//   `alpha`, then `f`, then `s`. IEEE arithmetic is order-sensitive for `sub`
//   and `div`, and the short circuit is what the interpreter does.
// * **Laziness.** `range_choice` and `interval_select` evaluate one branch.
//   The program therefore jumps: a flat post-order list that evaluated both
//   sides would produce the same value here (the branches are pure) but it
//   would no longer be a compilation of the same function, and any future
//   impurity under a branch would silently change the world.
// * **`interpolated`'s two behaviours.** With a chunk's corner grids it is the
//   trilinear value of `grids[id]`; without them (corner sampling) it is its
//   input evaluated at the position. `Op::Interp` is "trilinear if the slot is
//   present, else run the input's program", which is the same two-branch rule.
// * **`f32` in splines.** Spline locations, derivatives and point values are
//   `f32` and the coordinate is cast to `f32`, so a spline is *not* a `f64`
//   subtree. Splines stay on the interpreter (`Op::Call`) rather than being
//   re-expressed in the compiled arithmetic - the `f32` round-trips are the
//   whole semantics, and 0.3% of the visits do not need rewriting.
//
// `cache` nodes compile to their input (one instruction, no memo). The memo
// `block_density` passes is created fresh for every block and every node
// reachable from `final_density` is a deterministic function of
// `(id, x, y, z, chunk columns)`, so a memo hit inside one block can only
// return the value a re-evaluation would compute. Dropping the map removes a
// hash lookup per cached visit and a `HashMap` insert per block; it cannot
// change a value, and `compiled_density_matches_interpreter` holds that.

/// `MEGU_DF_INTERP=1` keeps `final_density` interpreted: the program is not
/// even built, so the compiled path then costs one `None` test per block. This
/// is the A/B arm for the performance page and the reference arm for the
/// identity tests.
fn df_compile_disabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MEGU_DF_INTERP").is_some())
}

/// One instruction of a compiled density program.
///
/// Every instruction pops a fixed number of stack slots and leaves exactly one
/// value, except `Halt` (ends the program, root value on the stack), `Drop`
/// (used only by `Div`'s zero branch) and the jumps, whose stack effect is
/// documented at the variant.
#[derive(Clone, Copy, Debug)]
enum Op {
    /// Push a constant. Constant subtrees are folded at compile time, so this is
    /// also what a folded subtree becomes.
    Push(f64),
    Add,
    Sub,
    Mul,
    Min,
    Max,
    /// `a / b`, popped in that order. The zero-divisor short circuit is a jump;
    /// see `DfCompiler::emit_div`.
    Div,
    Neg,
    Abs,
    Sq,
    Cube,
    HNeg,
    QNeg,
    Squeeze,
    Clamp {
        lo: f64,
        hi: f64,
    },
    /// `f + a * (s - f)`: pops `s`, `f`, `a` in that order.
    Lerp,
    /// `a + c` / `a * c` / `df_min(a, c)` / `df_max(a, c)` for a compile-time `c`: one
    /// slot instead of a push plus a two-slot op. `+` and `*` are exact under
    /// exchange, and [`df_min`]/[`df_max`] return the non-NaN operand and the same
    /// zero in either order, so the constant may sit on either side of the node.
    ///
    /// These are codegen forms of *this repository's* `DF::Add`/`DF::Mul`, which
    /// are plain arithmetic. Vanilla's codec instead builds a separate
    /// `MulOrAdd` node when one side is a constant, and that node has no
    /// `arg1 == 0.0` short circuit, so `mul(0.0, negative)` is `-0.0` there and
    /// `+0.0` through `Ap2.MUL`. That distinction is not modelled here (it is
    /// the interpreter that the fixtures pin), and an immediate operand cannot
    /// introduce it: exchange is exact, so `0.0 * -2.0` and `-2.0 * 0.0` are the
    /// same bits.
    AddC(f64),
    MulC(f64),
    MinC(f64),
    MaxC(f64),
    /// `y_clamped_gradient`: `fv + t.clamp(0,1) * (tv - fv)`, `t = (y-fc)/(tc-fc)`.
    Grad {
        fc: f64,
        fv: f64,
        tc: f64,
        tv: f64,
    },
    /// Pops the three scale arguments (in the interpreter's x, y, z reading
    /// order) and samples the noise.
    Noise {
        ni: u32,
        xzs: f64,
        ys: f64,
    },
    ShiftA(u32),
    ShiftB(u32),
    /// Pops the rarity input.
    Weird {
        ni: u32,
        mapper: RarityValueMapper,
    },
    Beardifier,
    /// `interpolated`: if the chunk column set carries this slot's grid, push its
    /// trilinear value and jump to `join`; otherwise fall through and run the
    /// input's program, which ends at `join`.
    Interp { slot: u32, join: u32 },
    /// Evaluate a subtree with the interpreter (splines, `find_top_surface`,
    /// `old_blended_noise`, `end_islands`). The memo handed over is empty: a
    /// `cache` node outside this subtree needs none, and one inside it only ever
    /// sees the single position being evaluated (see the section comment).
    Call(u32),
    /// Peeks at the top slot and jumps when it is outside `[mi, ma)`; otherwise
    /// pops it. `range_choice`.
    BranchOut { mi: f64, ma: f64, target: u32 },
    /// Pops one value and jumps when it is `< t` (`interval_select`).
    BranchLt { t: f64, target: u32 },
    /// Peeks at the top slot and jumps when it is `0.0` (leaving it in place for
    /// the zero branch to drop), else falls through with it still on top.
    BranchZero { target: u32 },
    Jump(u32),
    /// Discard the top slot.
    Drop,
    /// Column evaluator (`DfCols`) only: push `v2[k]`, the value the 2D prefix
    /// computed once for this `(x, z)` column. `k < n2` is a compile-time
    /// invariant (see `DfCompiler::emit_hoisted`), which is why the interpreter
    /// indexes `v2` unchecked.
    Push2(u32),
    /// Column evaluator only: pop the top slot into `v2[k]`; terminates one
    /// hoisted subtree inside `ops2`. Same `k < n2` invariant as `Push2`.
    Store2(u32),
    /// End of the `ops2` prefix list: a program that leaves its values in `v2`,
    /// not on the stack, so it cannot share `Halt`'s "one value on top" rule.
    Done,
    Halt,
}

/// `f64::min` with the answer pinned instead of left to the lowering.
///
/// Rust's `f64::min` is IEEE-754 minNum, except for two cases it documents as unspecified:
/// two zeros of different signs, and two NaNs. The lowerings disagree about the first - a
/// branchy comparison sequence returns the first operand where `minsd` returns the second -
/// which was measured here: the same `f64::min(-0.0, 0.0)` answered `-0.0` from one build of
/// this tree and `0.0` from another. A vector arm cannot be checked against "whatever the
/// scalar op did" while that is true, so the density evaluator's three implementations (the
/// interpreter, the compiled program and its four-lane form) all call this sequence:
///
/// * `a < b` -> `a`, `b < a` -> `b`;
/// * equal -> the bitwise OR of the two, i.e. `-0.0` for the two zeros and the value itself
///   otherwise (minNum's own rule for the zeros);
/// * otherwise one is NaN: the other operand (and `b` when both are, which is what both
///   lowerings do).
///
/// It is the sequence `Op::Min`'s vector form runs as `minv!`, so the two agree by
/// construction. Nothing about the density's *value* changes: the sign of a zero and a NaN's
/// payload are invisible to every consumer (all of them are comparisons, and the corner grid
/// the bytes come from is filled on the one-lane path) - the 64-chunk dumps are byte for byte
/// what they were before this.
#[inline]
fn df_min(a: f64, b: f64) -> f64 {
    if a < b {
        a
    } else if b < a {
        b
    } else if a == b {
        f64::from_bits(a.to_bits() | b.to_bits())
    } else if a.is_nan() {
        b
    } else {
        a
    }
}

/// [`df_min`]'s maximum: `+0.0` for the two zeros (the bitwise AND), the other operand when
/// one is NaN, `b` when both are.
#[inline]
fn df_max(a: f64, b: f64) -> f64 {
    if a > b {
        a
    } else if b > a {
        b
    } else if a == b {
        f64::from_bits(a.to_bits() & b.to_bits())
    } else if a.is_nan() {
        b
    } else {
        a
    }
}

/// A compiled `final_density` program.
#[derive(Clone)]
struct DfProg {
    ops: Vec<Op>,
    /// Peak stack depth, so one scratch buffer serves the whole program.
    slots: usize,
}

thread_local! {
    /// Per-thread evaluation stack. `block_density` runs once per block, so the
    /// buffer is reused rather than allocated (a fresh `Vec` per block would be
    /// 98k allocations a chunk).
    static DF_STACK: std::cell::RefCell<Vec<f64>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl DfProg {
    /// Compile `final_density`. `None` means "keep interpreting": either the
    /// switch is off, or the DAG collapsed to a handful of instructions (the
    /// Nether and End roots are a single `old_blended_noise` / `end_islands`
    /// call), where a program would only add indirection.
    fn compile(d: &GenData) -> Option<Box<DfProg>> {
        if df_compile_disabled() {
            return None;
        }
        Self::compile_inner(d)
    }

    /// The compiler itself, independent of `MEGU_DF_INTERP`: the identity
    /// tests ask it for a program the switch may have suppressed, so that a tree
    /// with the A/B switch exported does not silently stop testing anything.
    fn compile_inner(d: &GenData) -> Option<Box<DfProg>> {
        let n = d.dfs.len();
        let mut c = DfCompiler {
            d,
            ops: Vec::new(),
            depth: 0,
            max_depth: 0,
            consts: vec![None; n],
            consts_known: vec![false; n],
            col: None,
        };
        c.emit(d.final_density);
        if c.ops.len() < 8 {
            return None;
        }
        c.ops.push(Op::Halt);
        Some(Box::new(DfProg {
            ops: c.ops,
            slots: c.max_depth.max(1),
        }))
    }

    /// `block_density`'s body when a program exists.
    fn eval(&self, g: &Gen, cc: Option<&ChunkCols>, x: i32, y: i32, z: i32) -> f64 {
        let reuse = DF_STACK.with(|slot| {
            let mut buf = slot.try_borrow_mut().ok()?;
            if buf.len() < self.slots {
                buf.resize(self.slots, 0.0);
            }
            let (head, _) = buf.split_at_mut(self.slots);
            Some(self.run(g, cc, x, y, z, head))
        });
        match reuse {
            Some(v) => v,
            // Re-entered from inside a `Call` (which cannot happen today, since
            // the interpreter never calls back into the program): pay for a local
            // buffer rather than panicking on the borrow.
            None => {
                let mut local = vec![0.0; self.slots];
                self.run(g, cc, x, y, z, &mut local)
            }
        }
    }

    fn run(
        &self,
        g: &Gen,
        cc: Option<&ChunkCols>,
        x: i32,
        y: i32,
        z: i32,
        st: &mut [f64],
    ) -> f64 {
        df_run(&self.ops, g, cc, x, y, z, st, &mut [], None)
    }
}

/// The stack-machine interpreter shared by `DfProg` (the whole `ops` list, an
/// empty `v2`) and `DfCols` (the `ops2` prefix, then one 3D program per grid):
/// one instruction set and one dispatch. `Push2`/`Store2`/`Done` appear only in
/// `DfCols` lists, where every slot index was proved `< n2` at compile time.
#[expect(clippy::too_many_arguments)]
fn df_run(
    ops: &[Op],
    g: &Gen,
    cc: Option<&ChunkCols>,
    x: i32,
    y: i32,
    z: i32,
    st: &mut [f64],
    v2: &mut [f64],
    mut cm: Option<&mut FlatMemo>,
) -> f64 {
    {
        let mut sp = 0usize;
        let mut pc = 0usize;
        macro_rules! bin {
            ($e:expr) => {{
                sp -= 1;
                st[sp - 1] = $e;
                pc += 1;
            }};
        }
        macro_rules! un {
            ($e:expr) => {{
                st[sp - 1] = $e;
                pc += 1;
            }};
        }
        loop {
            match ops[pc] {
                Op::Push(v) => {
                    st[sp] = v;
                    sp += 1;
                    pc += 1;
                }
                Op::Add => bin!(st[sp - 1] + st[sp]),
                Op::Sub => bin!(st[sp - 1] - st[sp]),
                Op::Mul => bin!(st[sp - 1] * st[sp]),
                Op::Min => bin!(df_min(st[sp - 1], st[sp])),
                Op::Max => bin!(df_max(st[sp - 1], st[sp])),
                Op::Div => bin!(st[sp - 1] / st[sp]),
                Op::Neg => un!(-st[sp - 1]),
                Op::Abs => un!(st[sp - 1].abs()),
                Op::Sq => un!(st[sp - 1] * st[sp - 1]),
                Op::Cube => {
                    let v = st[sp - 1];
                    un!(v * v * v)
                }
                Op::HNeg => {
                    let v = st[sp - 1];
                    un!(if v > 0.0 { v } else { v * 0.5 })
                }
                Op::QNeg => {
                    let v = st[sp - 1];
                    un!(if v > 0.0 { v } else { v * 0.25 })
                }
                Op::Squeeze => {
                    let v = st[sp - 1].clamp(-1.0, 1.0);
                    un!(v / 2.0 - v * v * v / 24.0)
                }
                Op::Clamp { lo, hi } => un!(st[sp - 1].clamp(lo, hi)),
                Op::Lerp => {
                    let s = st[sp - 1];
                    let f = st[sp - 2];
                    let a = st[sp - 3];
                    sp -= 2;
                    st[sp - 1] = f + a * (s - f);
                    pc += 1;
                }
                Op::AddC(c) => un!(st[sp - 1] + c),
                Op::MulC(c) => un!(st[sp - 1] * c),
                Op::MinC(c) => un!(df_min(st[sp - 1], c)),
                Op::MaxC(c) => un!(df_max(st[sp - 1], c)),
                Op::Grad { fc, fv, tc, tv } => {
                    // A producer, like `Push`: it consumes nothing, so it writes at
                    // `sp` and grows the stack. Writing it at `sp - 1` leaves every
                    // later operand pair one slot low, which is silently right for
                    // some programs and wrong for others.
                    let t = ((y as f64) - fc) / (tc - fc);
                    st[sp] = fv + t.clamp(0.0, 1.0) * (tv - fv);
                    sp += 1;
                    pc += 1;
                }
                Op::Noise { ni, xzs, ys } => {
                    let zs = st[sp - 1];
                    let ysv = st[sp - 2];
                    let xs = st[sp - 3];
                    sp -= 2;
                    st[sp - 1] = g.noises[ni as usize].get(
                        x as f64 * xzs + xs,
                        y as f64 * ys + ysv,
                        z as f64 * xzs + zs,
                    );
                    pc += 1;
                }
                Op::ShiftA(ni) => {
                    st[sp] = g.noises[ni as usize].get(x as f64 * 0.25, 0.0, z as f64 * 0.25) * 4.0;
                    sp += 1;
                    pc += 1;
                }
                Op::ShiftB(ni) => {
                    st[sp] = g.noises[ni as usize].get(z as f64 * 0.25, x as f64 * 0.25, 0.0) * 4.0;
                    sp += 1;
                    pc += 1;
                }
                Op::Weird { ni, mapper } => {
                    let rarity = mapper.rarity(st[sp - 1]);
                    st[sp - 1] = rarity
                        * g.noises[ni as usize]
                            .get(x as f64 / rarity, y as f64 / rarity, z as f64 / rarity)
                            .abs();
                    pc += 1;
                }
                Op::Beardifier => {
                    st[sp] = match cc.and_then(|cc| cc.beard.as_deref()) {
                        Some(beard) => compute_beardifier(beard, x, y, z),
                        None => 0.0,
                    };
                    sp += 1;
                    pc += 1;
                }
                Op::Interp { slot, join } => match cc.and_then(|cc| cc.interp_slot(slot as usize, x, y, z)) {
                    Some(v) => {
                        st[sp] = v;
                        sp += 1;
                        pc = join as usize;
                    }
                    None => pc += 1,
                },
                Op::Call(id) => {
                    st[sp] = if let Some(m) = cm.as_deref_mut() {
                        // The memo's scope is one `Call`; clearing first makes
                        // the reuse bit-identical to the fresh `HashMap` the
                        // interpreter arm would build.
                        m.clear();
                        g.eval_c(id as usize, x, y, z, m, cc)
                    } else {
                        let mut memo = HashMap::new();
                        g.eval_c(id as usize, x, y, z, &mut memo, cc)
                    };
                    sp += 1;
                    pc += 1;
                }
                Op::BranchOut { mi, ma, target } => {
                    if st[sp - 1] >= mi && st[sp - 1] < ma {
                        sp -= 1;
                        pc += 1;
                    } else {
                        sp -= 1;
                        pc = target as usize;
                    }
                }
                Op::BranchLt { t, target } => {
                    sp -= 1;
                    pc = if st[sp] < t { target as usize } else { pc + 1 };
                }
                Op::BranchZero { target } => {
                    pc = if st[sp - 1] == 0.0 {
                        target as usize
                    } else {
                        pc + 1
                    };
                }
                Op::Jump(target) => pc = target as usize,
                Op::Drop => {
                    sp -= 1;
                    pc += 1;
                }
                Op::Push2(k) => {
                    // SAFETY: `k < n2` is invariant under `DfCols` compilation
                    // (`emit_hoisted` hands out dense slots), and the column
                    // callers all run with `v2.len() >= n2`. `DfProg` programs
                    // never carry this op.
                    debug_assert!((k as usize) < v2.len());
                    st[sp] = unsafe { *v2.get_unchecked(k as usize) };
                    sp += 1;
                    pc += 1;
                }
                Op::Store2(k) => {
                    sp -= 1;
                    // SAFETY: as `Op::Push2`.
                    debug_assert!((k as usize) < v2.len());
                    unsafe { *v2.get_unchecked_mut(k as usize) = st[sp] };
                    pc += 1;
                }
                Op::Done => {
                    // Every `Store2` returns the stack to depth zero, so the
                    // prefix must end empty; the value is never read.
                    debug_assert_eq!(sp, 0, "2D prefix left {sp} values, not 0");
                    return 0.0;
                }
                Op::Halt => {
                    // The program leaves exactly one value. Compile-time depth and
                    // runtime depth disagreeing is how a producer written as an
                    // in-place update hid until a value identity test ran a branch
                    // it never took.
                    debug_assert_eq!(sp, 1, "compiled program left {sp} values, not 1");
                    return st[0];
                }
            }
        }
    }
}

// ---------- BOT-DF-BATCH: the corner-grid column evaluator ----------
//
// The corner-grid fill walks `for yi in 0..ny` per `(x, z)` column and used to
// answer every level through `eval_flat`: a recursive interpreter that checks
// `is_2d` and the `Cache` generation on every node visit. The flat memo already
// keeps each y-independent node to one evaluation per column; what remains per
// level is the dispatch itself. `DfCols` compiles the same split into
// instructions: every `is_2d` subtree is emitted once into `ops2` (run once per
// column, its result held in `v2[k]`), and the per-level program keeps only the
// 3D tail, reading the hoisted values through `Op::Push2`. The instruction set,
// the operand order and the `Op::Call` fallback are `DfProg`'s, so a cell is
// the `eval_flat` value bit for bit.
//
// The two pieces share the shape the reference evaluator already proves:
//
// * **`ops2` runs where `FlatCornerMemo::next_2d` fired.** A y-independent
//   node is evaluated once per `(x, z)`, by slot index rather than by a
//   generation-stamped array read. Nested is_2d nodes emit inline (see
//   `emit_hoisted`), so the prefix is a straight run.
// * **The 3D tail runs per `iy`.** `Op::Push2(k)` stands where the subtree's
//   instructions stood; branches still read the value off the same stack slot.
//   Laziness only ever skips work: a hoisted node that sat under an untaken
//   branch is computed once per column anyway, which cannot change a value.

/// The compiled form of one interp-grid family (the density set, or the vein
/// set): one shared 2D prefix plus one program per grid.
#[derive(Clone)]
struct DfCols {
    /// Hoisted 2D subtrees, each terminated by `Op::Store2`; the list itself
    /// ends at `Op::Done`. Runs once per `(x, z)` column.
    ops2: Box<[Op]>,
    /// One post-order program per `interpolated` grid, `Op::Push2` where the
    /// interpreter re-evaluated a y-independent subtree. Each ends at `Op::Halt`.
    progs: Box<[Box<[Op]>]>,
    /// `interpolated` node id (the `ChunkCols::grids` index) -> `progs` index,
    /// `u32::MAX` for a node this set did not compile.
    pi_of: Box<[u32]>,
    /// Peak stack depth across `ops2` and every program; one scratch serves all.
    slots: usize,
    /// `v2` length: the hoisted 2D value count.
    n2: usize,
}

/// Scratch shared by every `DfCols` evaluation on one generator: the stack and
/// the hoisted 2D values, allocated once and reused across all `ny` levels of a
/// `(cx, cz)` column and across chunks. `v2` is fully rewritten by `ops2`
/// before any `Op::Push2` reads it, so no zeroing is needed between columns.
#[derive(Default)]
struct DfColScratch {
    st: Vec<f64>,
    v2: Vec<f64>,
    /// The memo `Op::Call` would otherwise allocate fresh on every dispatch;
    /// it is cleared per call, so the reuse changes nothing but the
    /// allocation.
    cm: HashMap<(usize, i32, i32, i32), f64>,
}

impl DfCols {
    /// Compile one program per `(grid, inner)` pair, sharing the hoisted 2D
    /// prefix so a node two inputs read is still evaluated once per column.
    /// `None` when the switch keeps the interpreter (`MEGU_DF_INTERP`) or
    /// the set has no `InterpKind::General` inputs.
    fn compile(d: &GenData, roots: &[(usize, usize)]) -> Option<Box<DfCols>> {
        if roots.is_empty() || df_compile_disabled() {
            return None;
        }
        debug_assert_eq!(
            d.is_2d.len(),
            d.dfs.len(),
            "is_2d must cover the node table before column compilation"
        );
        let n = d.dfs.len();
        let mut c = DfCompiler {
            d,
            ops: Vec::new(),
            depth: 0,
            max_depth: 0,
            consts: vec![None; n],
            consts_known: vec![false; n],
            col: Some(ColEmit {
                ops2: Vec::new(),
                slot_of: vec![u32::MAX; n],
                n2: 0,
            }),
        };
        let mut pi_of = vec![u32::MAX; n];
        let mut progs = Vec::with_capacity(roots.len());
        for &(slot, inner) in roots {
            pi_of[slot] = progs.len() as u32;
            c.emit(inner);
            c.ops.push(Op::Halt);
            c.depth = 0;
            progs.push(std::mem::take(&mut c.ops).into_boxed_slice());
        }
        let col = c.col.take().expect("column emission state");
        let mut ops2 = col.ops2;
        ops2.push(Op::Done);
        let n2 = col.n2 as usize;
        // The invariant `v2` indexing relies on: every slot index emitted is
        // `< n2`, because `emit_hoisted` hands out dense slots. Debug-check it
        // here, at compile time, rather than per access.
        debug_assert!(ops2
            .iter()
            .chain(progs.iter().flat_map(|p| p.iter()))
            .all(|op| !matches!(op, Op::Push2(k) | Op::Store2(k) if (*k as usize) >= n2)));
        Some(Box::new(DfCols {
            ops2: ops2.into_boxed_slice(),
            progs: progs.into_boxed_slice(),
            pi_of: pi_of.into_boxed_slice(),
            slots: c.max_depth.max(1),
            n2,
        }))
    }

    /// `Op::Push2` for one grid's program, or `None` when the set did not
    /// compile this `interpolated` node (it is a `YNoise` grid the specialised
    /// fill owns).
    fn prog(&self, slot: usize) -> Option<&[Op]> {
        self.pi_of
            .get(slot)
            .and_then(|&pi| (pi != u32::MAX).then_some(pi))
            .map(|pi| &*self.progs[pi as usize])
    }

    /// The 2D prefix for one `(x, z)` column: every `Op::Push2` the level
    /// programs dispatch reads a slot this fills. `y` is immaterial — every op
    /// in `ops2` is y-independent, the same invariant `FlatCornerMemo` holds
    /// under `cur_2d`.
    fn begin(&self, g: &Gen, x: i32, z: i32, sc: &mut DfColScratch) {
        #[cfg(test)]
        DFCOL_BEGINS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        df_run(&self.ops2, g, None, x, 0, z, &mut sc.st, &mut sc.v2, Some(&mut sc.cm));
    }

    /// One corner's density: `eval_flat(inner, x, y, z)` compiled.
    fn eval(&self, ops: &[Op], g: &Gen, x: i32, y: i32, z: i32, sc: &mut DfColScratch) -> f64 {
        df_run(ops, g, None, x, y, z, &mut sc.st, &mut sc.v2, Some(&mut sc.cm))
    }

    /// Four levels of one column's tail program — `y0, y0+ystep, y0+2*ystep,
    /// y0+3*ystep` — as one `run4y_avx2` pass. `None` from the vector arm (a
    /// conditional that found the lanes disagreeing) or a host without AVX2
    /// falls back to four scalar `eval`s, so the result is always the four
    /// `eval` results bit for bit. `df4_disabled` gates the arm the same way
    /// `DfProg::eval_x4`'s is gated.
    #[expect(clippy::too_many_arguments)]
    fn eval_y4(
        &self,
        ops: &[Op],
        g: &Gen,
        x: i32,
        y0: i32,
        ystep: i32,
        z: i32,
        sc: &mut DfColScratch,
    ) -> [f64; DF4] {
        #[cfg(target_arch = "x86_64")]
        {
            if self.slots <= DF4_SLOTS && !df4_disabled() && df4_avx2() {
                // SAFETY: `df4_avx2` proved the feature is present at runtime,
                // and `slots <= DF4_SLOTS` bounds the vector stack.
                if let Some(v) = unsafe { run4y_avx2(ops, &sc.v2, g, None, x, y0, ystep, z) }
                {
                    return v;
                }
            }
        }
        std::array::from_fn(|k| self.eval(ops, g, x, y0 + (k as i32) * ystep, z, sc))
    }

    /// Eight levels of the tail: `run8y_avx512` where the CPU and the program
    /// allow it, two `eval_y4` passes where it does not — the same composition
    /// `trilinear_x8` and `DfProg::eval_x8` use. `df8_disabled` gates the arm
    /// the same way `DfProg::eval_x8`'s is gated.
    #[expect(clippy::too_many_arguments)]
    fn eval_y8(
        &self,
        ops: &[Op],
        g: &Gen,
        x: i32,
        y0: i32,
        ystep: i32,
        z: i32,
        sc: &mut DfColScratch,
    ) -> [f64; DF8] {
        #[cfg(target_arch = "x86_64")]
        {
            if self.slots <= DF4_SLOTS && !df8_disabled() && df8_avx512() {
                // SAFETY: `df8_avx512` proved both features are present at
                // runtime, and `slots <= DF4_SLOTS` bounds the vector stack.
                if let Some(v) = unsafe { run8y_avx512(ops, &sc.v2, g, None, x, y0, ystep, z) }
                {
                    return v;
                }
            }
        }
        let a = self.eval_y4(ops, g, x, y0, ystep, z, sc);
        let b = self.eval_y4(ops, g, x, y0 + (DF4 as i32) * ystep, ystep, z, sc);
        std::array::from_fn(|k| if k < DF4 { a[k] } else { b[k - DF4] })
    }
}

/// The A/B switch for the compiled column fill, mirroring `df4_disabled`:
/// `MEGU_DFCOL=0` keeps the flat-memo evaluator, and the test toggle runs
/// the same binary through both arms.
fn dfcol_flat() -> bool {
    #[cfg(test)]
    if DFCOL_TEST_FLAT.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    matches!(env_str!("MEGU_DFCOL").as_deref(), Some("0"))
}

/// The in-process form of `MEGU_DFCOL=0`, so the parity/bench test holds
/// the two fill paths to the same bytes without a second binary.
#[cfg(test)]
static DFCOL_TEST_FLAT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn dfcol_test_force_flat(on: bool) {
    DFCOL_TEST_FLAT.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// How many `(x, z)` columns ran the compiled 2D prefix since the counter was
/// reset: the proof that the parity run exercised `DfCols` and not the
/// reference it is compared against.
#[cfg(test)]
static DFCOL_BEGINS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn dfcol_test_begins() -> u64 {
    DFCOL_BEGINS.swap(0, std::sync::atomic::Ordering::Relaxed)
}

// ---------- M3 P4: the four-lane density evaluator ----------
//
// `final_density` is evaluated once per block, and that per-block call is where the terrain
// stage spends its density time: on a 64-chunk dump the `surf` scans make 15.4M calls, the
// surface window 7.8M and the section fill 6.3M, at 84 ns each (measured 2026-11-19, M5).
// The spine around the `interpolated` grids is pure `f64` arithmetic on four adjacent
// columns, so four columns evaluate as four lanes of one `__m256d`.
//
// The contract is bit identity with the one-lane evaluator, not "the same value to within an
// ulp": density feeds `> 0.0` and the trilinear cells, so a last-bit difference is a
// different block somewhere in the world. That rules out `mul_add`/FMA (a fused multiply-add
// rounds once where the scalar program rounds twice), so every operation below is the plain
// IEEE-754 operation the scalar program performs. The ops whose Rust semantics are not the
// hardware's - `f64::min`/`max` and their immediate forms - are compare/blend sequences
// written to reproduce the scalar method's four cases, because `_mm256_min_pd` answers
// differently on the two zeros (`-0.0` against `+0.0`) and on NaN.
//
// The `interpolated` grid is what makes this pay. On this preset the per-block spine is 29
// arithmetic ops and *no* noise samples: the grid lookup jumps over the whole
// `blend_density` subtree, so the four lanes share eight corner values
// (`trilinear_x4`) and the rest is arithmetic. When the same bench was run with the
// scalar `min`/`max` (four libcalls per op) and four separate `trilinear` calls (three
// `floor` calls each), the group cost four evaluations, i.e. the vector arm bought nothing:
// the ops *around* the density were the density's cost, not the density itself.
//
// A conditional jump is the one thing four lanes cannot share: `range_choice` reads a value
// the four positions compute separately. The evaluator takes the branch when all four lanes
// agree and otherwise returns `None`, and the caller finishes those four positions on the
// one-lane evaluator. `M3_DF4_VEC`/`M3_DF4_BAIL` count both outcomes, so "the vector arm is
// used" is measured rather than assumed.

/// Lanes in the vector evaluator: one `__m256d`.
const DF4: usize = 4;

/// Vector stack slots. `DfProg::slots` is the program's peak depth (12 on the Overworld
/// `final_density`); a deeper program keeps the one-lane path rather than sizing a buffer for
/// a shape no preset produces.
const DF4_SLOTS: usize = 16;

/// The A/B switch: `MEGU_DF4=0` disables the vector arm and leaves the same binary's
/// one-lane path.
fn df4_disabled() -> bool {
    #[cfg(test)]
    if DF4_TEST_SCALAR.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    // `env_str!` caches the read itself.
    matches!(env_str!("MEGU_DF4").as_deref(), Some("0"))
}

/// The in-process form of that switch, so one test can build the same chunk with the vector
/// arm and without it and compare the two (the env read is cached per process, and an
/// in-process A/B is what proves the batching, not the environment).
#[cfg(test)]
static DF4_TEST_SCALAR: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn df4_test_force_scalar(on: bool) {
    DF4_TEST_SCALAR.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Does this CPU have the vector arm? Asked once; a host without AVX2 keeps the scalar path.
#[cfg(target_arch = "x86_64")]
fn df4_avx2() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::arch::is_x86_feature_detected!("avx2"))
}

// ---------- Layer A2-c: the eight-lane density evaluator ----------
//
// Same contract as the four-lane arm above, on one AVX-512 `__m512d`: bit identity with
// `DfProg::run`, no FMA, the same compare/blend sequences for min/max/clamp and the same
// lane-uniform branch rule. A conditional that splits the eight lanes returns `None` and the
// caller finishes the group through two `eval_x4` calls (so a four-lane subset that agrees
// still gets AVX2). `MEGU_DF8=0` keeps the same binary on the 4-lane path for A/B.

/// Lanes in the eight-lane evaluator: one `__m512d`.
pub(crate) const DF8: usize = 8;

/// The A/B switch for the eight-lane arm: `MEGU_DF8=0` leaves the 4-lane path (and
/// `MEGU_DF4=0` / `DF4_TEST_SCALAR` still disable every vector arm).
fn df8_disabled() -> bool {
    #[cfg(test)]
    if DF4_TEST_SCALAR.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    df4_disabled() || matches!(env_str!("MEGU_DF8").as_deref(), Some("0"))
}

/// Does this CPU have AVX-512F + AVX-512DQ? Asked once; `dq` is required for the
/// floating-point-domain logical ops (`or_pd`/`and_pd`/`xor_pd`) the min/max/neg/abs
/// sequences use. A host without either feature keeps the 4-lane arm.
#[cfg(target_arch = "x86_64")]
fn df8_avx512() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512dq")
    })
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn v4_to_arr(v: std::arch::x86_64::__m256d) -> [f64; DF4] {
    // SAFETY: `__m256d` is four `f64` lanes and nothing else.
    unsafe { std::mem::transmute(v) }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn v4_from_arr(a: [f64; DF4]) -> std::arch::x86_64::__m256d {
    // SAFETY: as above; `__m256d` has no padding.
    unsafe { std::mem::transmute(a) }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn v8_to_arr(v: std::arch::x86_64::__m512d) -> [f64; DF8] {
    // SAFETY: `__m512d` is eight `f64` lanes and nothing else.
    unsafe { std::mem::transmute(v) }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn v8_from_arr(a: [f64; DF8]) -> std::arch::x86_64::__m512d {
    // SAFETY: as above; `__m512d` has no padding.
    unsafe { std::mem::transmute(a) }
}

impl DfProg {
    /// Density at the four consecutive columns `x..x+4`, all at one `(y, z)`.
    ///
    /// Called only when a compiled program exists (the callers keep the interpreter for
    /// `MEGU_DF_INTERP=1`), so both arms here are programs: the vector one where the CPU
    /// and the program allow it, and otherwise the same four positions one at a time.
    fn eval_x4(&self, g: &Gen, cc: Option<&ChunkCols>, x: i32, y: i32, z: i32) -> [f64; DF4] {
        #[cfg(target_arch = "x86_64")]
        {
            if self.slots <= DF4_SLOTS && !df4_disabled() && df4_avx2() {
                // SAFETY: `df4_avx2` proved the feature is present at runtime.
                if let Some(v) = unsafe { run4_avx2(self, g, cc, x, y, z) } {
                    if m3_on() {
                        m3_inc(M3_DF4_VEC);
                    }
                    return v;
                }
                if m3_on() {
                    m3_inc(M3_DF4_BAIL);
                }
            }
        }
        let mut out = [0.0f64; DF4];
        for (k, o) in out.iter_mut().enumerate() {
            *o = self.eval(g, cc, x + k as i32, y, z);
        }
        out
    }

    /// Density at the eight consecutive columns `x..x+8`, all at one `(y, z)` (layer A2-c).
    /// The AVX-512 arm is tried first; a lane-divergent conditional (or a host without
    /// AVX-512) falls back to two `eval_x4` calls, so each agreeing four-lane half still
    /// takes the AVX2 arm. The values are always eight `eval` results bit for bit.
    fn eval_x8(&self, g: &Gen, cc: Option<&ChunkCols>, x: i32, y: i32, z: i32) -> [f64; DF8] {
        #[cfg(target_arch = "x86_64")]
        {
            if self.slots <= DF4_SLOTS && !df8_disabled() && df8_avx512() {
                // SAFETY: `df8_avx512` proved both features are present at runtime.
                if let Some(v) = unsafe { run8_avx512(self, g, cc, x, y, z) } {
                    if m3_on() {
                        m3_inc(M3_DF4_VEC);
                    }
                    return v;
                }
                if m3_on() {
                    m3_inc(M3_DF4_BAIL);
                }
            }
        }
        let a = self.eval_x4(g, cc, x, y, z);
        let b = self.eval_x4(g, cc, x + DF4 as i32, y, z);
        std::array::from_fn(|k| if k < DF4 { a[k] } else { b[k - DF4] })
    }
}

/// [`trilinear`] for four consecutive columns at one `(y, z)`, one lane each.
///
/// The four lanes are four consecutive `x`, which puts them inside one horizontal grid cell
/// (checked below), so all four interpolate between the *same* eight corner values and only
/// the `x` fraction differs. That is what makes this cheap: eight scalar loads broadcast to
/// eight vectors, then `lerp3`'s three stages with `fx` per lane and `fy`/`fz` shared.
///
/// Every scalar in here is the expression `trilinear` writes - the same divisions, the same
/// `floor` and `min` clamp, the same `lerp3` order - so the four results are the four
/// `trilinear` results bit for bit. `None` means the group straddles a cell boundary (a
/// `cell_w` below 4, or an `x` that is not the start of a cell-aligned run), where the
/// caller has to interpolate the four positions one at a time.
///
/// # Safety
/// The caller must have checked `is_x86_feature_detected!("avx2")`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn trilinear_x4(
    cells: &[f64],
    columns: &ChunkCols,
    x: i32,
    y: i32,
    z: i32,
) -> Option<[f64; DF4]> {
    use std::arch::x86_64::*;
    let (ny, min_y) = (columns.ny, columns.min_y);
    let (cell_w, cell_h) = (columns.cell_w, columns.cell_h);
    let lx0 = x & 15;
    if cell_w < DF4 as i32 || 16 % cell_w != 0 || lx0 % cell_w + DF4 as i32 > cell_w {
        return None;
    }
    let nx = (16 / cell_w + 1) as usize;
    let (width, height) = (f64::from(cell_w), f64::from(cell_h));
    let lz = (z & 15) as f64;
    let ly = (y - min_y) as f64;
    let xi = (lx0 as f64 / width).floor().min((nx - 2) as f64) as usize;
    let zi = (lz / width).floor().min((nx - 2) as f64) as usize;
    let yi = (ly / height).floor().min((ny - 2) as f64) as usize;
    let fz = lz / width - zi as f64;
    let fy = ly / height - yi as f64;
    let g = |xi: usize, yi: usize, zi: usize| cells[(zi * nx + xi) * ny + yi];
    let c000 = _mm256_set1_pd(g(xi, yi, zi));
    let c100 = _mm256_set1_pd(g(xi + 1, yi, zi));
    let c010 = _mm256_set1_pd(g(xi, yi + 1, zi));
    let c110 = _mm256_set1_pd(g(xi + 1, yi + 1, zi));
    let c001 = _mm256_set1_pd(g(xi, yi, zi + 1));
    let c101 = _mm256_set1_pd(g(xi + 1, yi, zi + 1));
    let c011 = _mm256_set1_pd(g(xi, yi + 1, zi + 1));
    let c111 = _mm256_set1_pd(g(xi + 1, yi + 1, zi + 1));
    let mut fx = [0.0f64; DF4];
    for (k, f) in fx.iter_mut().enumerate() {
        let lx = (((lx0 + k as i32) & 15) as f64) / width;
        *f = lx - xi as f64;
    }
    let a = _mm256_setr_pd(fx[0], fx[1], fx[2], fx[3]);
    let b = _mm256_set1_pd(fy);
    let c = _mm256_set1_pd(fz);
    let l0 = _mm256_add_pd(c000, _mm256_mul_pd(a, _mm256_sub_pd(c100, c000)));
    let l1 = _mm256_add_pd(c010, _mm256_mul_pd(a, _mm256_sub_pd(c110, c010)));
    let l2 = _mm256_add_pd(c001, _mm256_mul_pd(a, _mm256_sub_pd(c101, c001)));
    let l3 = _mm256_add_pd(c011, _mm256_mul_pd(a, _mm256_sub_pd(c111, c011)));
    let m0 = _mm256_add_pd(l0, _mm256_mul_pd(b, _mm256_sub_pd(l1, l0)));
    let m1 = _mm256_add_pd(l2, _mm256_mul_pd(b, _mm256_sub_pd(l3, l2)));
    Some(v4_to_arr(_mm256_add_pd(
        m0,
        _mm256_mul_pd(c, _mm256_sub_pd(m1, m0)),
    )))
}

/// [`trilinear`] for eight consecutive columns at one `(y, z)`, as two `trilinear_x4`
/// halves (layer A2-c). `None` when the group cannot be split into two cell-aligned
/// four-lane runs (`cell_w != 4`, or an `x` that is not 8-aligned), so the caller answers
/// the eight positions one at a time - the same fallback `trilinear_x4` has.
fn trilinear_x8(cells: &[f64], cc: &ChunkCols, x: i32, y: i32, z: i32) -> Option<[f64; DF8]> {
    let lx0 = x & 15;
    if lx0 % DF8 as i32 != 0 || cc.cell_w != DF4 as i32 || 16 % cc.cell_w != 0 {
        return None;
    }
    #[cfg(target_arch = "x86_64")]
    {
        if df4_avx2() {
            // SAFETY: `df4_avx2` proved AVX2. `lx0` is 8-aligned and `cell_w` is 4, so
            // both halves start at a cell-aligned run that `trilinear_x4` accepts.
            let a = unsafe { trilinear_x4(cells, cc, x, y, z)? };
            let b = unsafe { trilinear_x4(cells, cc, x + DF4 as i32, y, z)? };
            return Some(std::array::from_fn(|k| if k < DF4 { a[k] } else { b[k - DF4] }));
        }
    }
    Some(std::array::from_fn(|k| trilinear(cells, cc, x + k as i32, y, z)))
}

/// The four-lane interpreter: `DfProg::run` with `__m256d` slots. `None` means a conditional
/// jump found the lanes disagreeing, and the caller has to finish them one at a time.
///
/// # Safety
/// The caller must have checked `is_x86_feature_detected!("avx2")`, and `prog.slots` must be
/// at most `DF4_SLOTS` (checked by the caller too: a deeper program would overrun `st`).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn run4_avx2(
    prog: &DfProg,
    g: &Gen,
    cc: Option<&ChunkCols>,
    x: i32,
    y: i32,
    z: i32,
) -> Option<[f64; DF4]> {
    use std::arch::x86_64::*;
    unsafe {
        // Written before it is read: the program's compile-time depth and its runtime depth
        // agree (`DfProg::run` relies on the same invariant), so slot `sp` always holds a
        // value pushed earlier in the same evaluation.
        let mut st: [std::mem::MaybeUninit<__m256d>; DF4_SLOTS] =
            [std::mem::MaybeUninit::uninit(); DF4_SLOTS];
        macro_rules! rd {
            ($i:expr) => {
                st[$i].assume_init()
            };
        }
        macro_rules! wr {
            ($i:expr, $v:expr) => {
                st[$i] = std::mem::MaybeUninit::new($v)
            };
        }
        let lane_x = [
            x as f64,
            (x + 1) as f64,
            (x + 2) as f64,
            (x + 3) as f64,
        ];
        let yv = _mm256_set1_pd(y as f64);
        let mut sp = 0usize;
        let mut pc = 0usize;
        /// Two lane operands through one floating-point instruction.
        macro_rules! binop {
            ($f:ident) => {{
                let b = rd!(sp - 1);
                let a = rd!(sp - 2);
                sp -= 1;
                wr!(sp - 1, $f(a, b));
                pc += 1;
            }};
        }
        /// `f64::clamp`'s own two comparisons, so `NaN` and the sign of zero answer what the
        /// scalar method answers.
        macro_rules! clampv {
            ($v:expr, $lo:expr, $hi:expr) => {{
                let v = $v;
                let lo = _mm256_set1_pd($lo);
                let hi = _mm256_set1_pd($hi);
                let r = _mm256_blendv_pd(v, lo, _mm256_cmp_pd(v, lo, _CMP_LT_OQ));
                _mm256_blendv_pd(r, hi, _mm256_cmp_pd(r, hi, _CMP_GT_OQ))
            }};
        }
        /// [`df_min`] on four lanes: its four cases (`a < b`, `b < a`, equal, and one NaN),
        /// which is why this is not `_mm256_min_pd` - that op answers differently on the two
        /// zeros and on NaN.
        macro_rules! minv {
            ($a:expr, $b:expr) => {{
                let a = $a;
                let b = $b;
                let r = _mm256_blendv_pd(b, a, _mm256_cmp_pd(a, b, _CMP_LT_OQ));
                let r = _mm256_blendv_pd(r, _mm256_or_pd(a, b), _mm256_cmp_pd(a, b, _CMP_EQ_OQ));
                let r = _mm256_blendv_pd(r, a, _mm256_cmp_pd(b, b, _CMP_UNORD_Q));
                _mm256_blendv_pd(r, b, _mm256_cmp_pd(a, a, _CMP_UNORD_Q))
            }};
        }
        /// [`df_max`] on four lanes, the same four cases (`+0.0` for the two zeros).
        macro_rules! maxv {
            ($a:expr, $b:expr) => {{
                let a = $a;
                let b = $b;
                let r = _mm256_blendv_pd(b, a, _mm256_cmp_pd(a, b, _CMP_GT_OQ));
                let r =
                    _mm256_blendv_pd(r, _mm256_and_pd(a, b), _mm256_cmp_pd(a, b, _CMP_EQ_OQ));
                let r = _mm256_blendv_pd(r, a, _mm256_cmp_pd(b, b, _CMP_UNORD_Q));
                _mm256_blendv_pd(r, b, _mm256_cmp_pd(a, a, _CMP_UNORD_Q))
            }};
        }
        /// Eight `f64`s out of (or into) the stack, for the ops that have to be scalar.
        macro_rules! lanes3 {
            () => {{
                let c = v4_to_arr(rd!(sp - 1));
                let b = v4_to_arr(rd!(sp - 2));
                let a = v4_to_arr(rd!(sp - 3));
                (a, b, c)
            }};
        }
        loop {
            match prog.ops[pc] {
                Op::Push(v) => {
                    wr!(sp, _mm256_set1_pd(v));
                    sp += 1;
                    pc += 1;
                }
                Op::Add => binop!(_mm256_add_pd),
                Op::Sub => binop!(_mm256_sub_pd),
                Op::Mul => binop!(_mm256_mul_pd),
                Op::Div => binop!(_mm256_div_pd),
                Op::Min => {
                    let b = rd!(sp - 1);
                    let a = rd!(sp - 2);
                    sp -= 1;
                    wr!(sp - 1, minv!(a, b));
                    pc += 1;
                }
                Op::Max => {
                    let b = rd!(sp - 1);
                    let a = rd!(sp - 2);
                    sp -= 1;
                    wr!(sp - 1, maxv!(a, b));
                    pc += 1;
                }
                Op::Neg => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm256_xor_pd(a, _mm256_set1_pd(-0.0)));
                    pc += 1;
                }
                Op::Abs => {
                    let a = rd!(sp - 1);
                    let mask = _mm256_castsi256_pd(_mm256_set1_epi64x(0x7fff_ffff_ffff_ffffu64 as i64));
                    wr!(sp - 1, _mm256_and_pd(a, mask));
                    pc += 1;
                }
                Op::Sq => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm256_mul_pd(a, a));
                    pc += 1;
                }
                Op::Cube => {
                    let a = rd!(sp - 1);
                    let sq = _mm256_mul_pd(a, a);
                    wr!(sp - 1, _mm256_mul_pd(sq, a));
                    pc += 1;
                }
                Op::HNeg => {
                    let a = rd!(sp - 1);
                    let half = _mm256_mul_pd(a, _mm256_set1_pd(0.5));
                    let pos = _mm256_cmp_pd(a, _mm256_setzero_pd(), _CMP_GT_OQ);
                    wr!(sp - 1, _mm256_blendv_pd(half, a, pos));
                    pc += 1;
                }
                Op::QNeg => {
                    let a = rd!(sp - 1);
                    let quarter = _mm256_mul_pd(a, _mm256_set1_pd(0.25));
                    let pos = _mm256_cmp_pd(a, _mm256_setzero_pd(), _CMP_GT_OQ);
                    wr!(sp - 1, _mm256_blendv_pd(quarter, a, pos));
                    pc += 1;
                }
                Op::Squeeze => {
                    let a = rd!(sp - 1);
                    let v = clampv!(a, -1.0, 1.0);
                    let cube = _mm256_mul_pd(_mm256_mul_pd(v, v), v);
                    let half = _mm256_div_pd(v, _mm256_set1_pd(2.0));
                    let over = _mm256_div_pd(cube, _mm256_set1_pd(24.0));
                    wr!(sp - 1, _mm256_sub_pd(half, over));
                    pc += 1;
                }
                Op::Clamp { lo, hi } => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, clampv!(a, lo, hi));
                    pc += 1;
                }
                Op::Lerp => {
                    let s = rd!(sp - 1);
                    let f = rd!(sp - 2);
                    let a = rd!(sp - 3);
                    sp -= 2;
                    wr!(sp - 1, _mm256_add_pd(f, _mm256_mul_pd(a, _mm256_sub_pd(s, f))));
                    pc += 1;
                }
                Op::AddC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm256_add_pd(a, _mm256_set1_pd(c)));
                    pc += 1;
                }
                Op::MulC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm256_mul_pd(a, _mm256_set1_pd(c)));
                    pc += 1;
                }
                Op::MinC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, minv!(a, _mm256_set1_pd(c)));
                    pc += 1;
                }
                Op::MaxC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, maxv!(a, _mm256_set1_pd(c)));
                    pc += 1;
                }
                Op::Grad { fc, fv, tc, tv } => {
                    let denom = _mm256_set1_pd(tc - fc);
                    let t = _mm256_div_pd(
                        _mm256_sub_pd(yv, _mm256_set1_pd(fc)),
                        denom,
                    );
                    let t = clampv!(t, 0.0, 1.0);
                    wr!(
                        sp,
                        _mm256_add_pd(
                            _mm256_set1_pd(fv),
                            _mm256_mul_pd(t, _mm256_set1_pd(tv - fv))
                        )
                    );
                    sp += 1;
                    pc += 1;
                }
                Op::Noise { ni, xzs, ys } => {
                    let (xs, ysv, zs) = lanes3!();
                    sp -= 2;
                    let noise = &g.noises[ni as usize];
                    // The z coordinate is `z as f64 * xzs + zs[k]`, NOT another copy of
                    // `lane_x[k] * xzs`; the four lanes differ in x only, so using the
                    // lane's x for z silently sampled the wrong column whenever x != z.
                    let nx: [f64; DF4] = std::array::from_fn(|k| lane_x[k] * xzs + xs[k]);
                    let ny: [f64; DF4] = std::array::from_fn(|k| y as f64 * ys + ysv[k]);
                    let nz: [f64; DF4] = std::array::from_fn(|k| z as f64 * xzs + zs[k]);
                    let r = noise.get_4(nx, ny, nz);
                    wr!(sp - 1, v4_from_arr(r));
                    pc += 1;
                }
                Op::ShiftA(ni) => {
                    let noise = &g.noises[ni as usize];
                    let nx: [f64; DF4] = std::array::from_fn(|k| lane_x[k] * 0.25);
                    let nz = [z as f64 * 0.25; DF4];
                    let r = noise.get_4(nx, [0.0; DF4], nz).map(|v| v * 4.0);
                    wr!(sp, v4_from_arr(r));
                    sp += 1;
                    pc += 1;
                }
                Op::ShiftB(ni) => {
                    let noise = &g.noises[ni as usize];
                    let nx = [z as f64 * 0.25; DF4];
                    let ny: [f64; DF4] = std::array::from_fn(|k| lane_x[k] * 0.25);
                    let r = noise.get_4(nx, ny, [0.0; DF4]).map(|v| v * 4.0);
                    wr!(sp, v4_from_arr(r));
                    sp += 1;
                    pc += 1;
                }
                Op::Weird { ni, mapper } => {
                    let rarity_in = v4_to_arr(rd!(sp - 1));
                    let noise = &g.noises[ni as usize];
                    let rarity: [f64; DF4] = std::array::from_fn(|k| mapper.rarity(rarity_in[k]));
                    let nx: [f64; DF4] = std::array::from_fn(|k| lane_x[k] / rarity[k]);
                    let ny: [f64; DF4] = std::array::from_fn(|k| y as f64 / rarity[k]);
                    let nz: [f64; DF4] = std::array::from_fn(|k| z as f64 / rarity[k]);
                    let nv = noise.get_4(nx, ny, nz);
                    let r: [f64; DF4] = std::array::from_fn(|k| rarity[k] * nv[k].abs());
                    wr!(sp - 1, v4_from_arr(r));
                    pc += 1;
                }
                Op::Beardifier => {
                    let beard = cc.and_then(|cc| cc.beard.as_deref());
                    // The common case is no beardifier at all (or an empty one): the
                    // marker evaluates to zero at every lane, so push one vector rather
                    // than four scalar `compute_beardifier` calls.
                    if beard.is_none_or(|b| b.is_empty()) {
                        wr!(sp, _mm256_setzero_pd());
                    } else {
                        let beard = beard.unwrap();
                        let mut r = [0.0f64; DF4];
                        for (k, o) in r.iter_mut().enumerate() {
                            *o = compute_beardifier(beard, x + k as i32, y, z);
                        }
                        wr!(sp, v4_from_arr(r));
                    }
                    sp += 1;
                    pc += 1;
                }
                // The grid's presence is a property of the columns, not of the position, so
                // this jump is lane-uniform: either all four lanes read a trilinear cell or
                // all four fall through to the node's own program.
                Op::Interp { slot, join } => {
                    let cells = cc
                        .and_then(|c| c.grids.get(slot as usize))
                        .and_then(|g| g.as_deref());
                    match (cells, cc) {
                        (Some(cells), Some(columns)) => {
                            let r = match trilinear_x4(cells, columns, x, y, z) {
                                Some(r) => r,
                                None => {
                                    let mut r = [0.0f64; DF4];
                                    for (k, o) in r.iter_mut().enumerate() {
                                        *o = trilinear(cells, columns, x + k as i32, y, z);
                                    }
                                    r
                                }
                            };
                            wr!(sp, v4_from_arr(r));
                            sp += 1;
                            pc = join as usize;
                        }
                        _ => pc += 1,
                    }
                }
                Op::Call(id) => {
                    let mut r = [0.0f64; DF4];
                    for (k, o) in r.iter_mut().enumerate() {
                        let mut memo = HashMap::new();
                        *o = g.eval_c(id as usize, x + k as i32, y, z, &mut memo, cc);
                    }
                    wr!(sp, v4_from_arr(r));
                    sp += 1;
                    pc += 1;
                }
                Op::BranchOut { mi, ma, target } => {
                    // `t >= mi && t < ma` with ordered-quiet compares: NaN answers
                    // false in both, exactly as the scalar `&&` does.
                    let v = rd!(sp - 1);
                    let lo = _mm256_cmp_pd(v, _mm256_set1_pd(mi), _CMP_GE_OQ);
                    let hi = _mm256_cmp_pd(v, _mm256_set1_pd(ma), _CMP_LT_OQ);
                    match _mm256_movemask_pd(_mm256_and_pd(lo, hi)) {
                        0xF => {
                            sp -= 1;
                            pc += 1;
                        }
                        0 => {
                            sp -= 1;
                            pc = target as usize;
                        }
                        _ => return None, // lanes disagree: caller finishes scalar
                    }
                }
                Op::BranchLt { t, target } => {
                    let v = rd!(sp - 1);
                    let mask = _mm256_movemask_pd(_mm256_cmp_pd(v, _mm256_set1_pd(t), _CMP_LT_OQ));
                    sp -= 1;
                    if mask == 0xF {
                        pc = target as usize;
                    } else if mask == 0 {
                        pc += 1;
                    } else {
                        return None;
                    }
                }
                Op::BranchZero { target } => {
                    let v = rd!(sp - 1);
                    // `== 0.0` is true for both zeros and false for NaN, which is
                    // what the ordered-equal compare answers too.
                    let mask =
                        _mm256_movemask_pd(_mm256_cmp_pd(v, _mm256_setzero_pd(), _CMP_EQ_OQ));
                    if mask == 0xF {
                        pc = target as usize;
                    } else if mask == 0 {
                        pc += 1;
                    } else {
                        return None;
                    }
                }
                Op::Jump(target) => pc = target as usize,
                Op::Drop => {
                    sp -= 1;
                    pc += 1;
                }
                // `DfCols` ops; a `DfProg` never contains them, so treat one as a
                // bail-out rather than a lane op.
                Op::Push2(_) | Op::Store2(_) | Op::Done => return None,
                Op::Halt => {
                    debug_assert_eq!(sp, 1, "compiled program left {sp} values, not 1");
                    return Some(v4_to_arr(rd!(0)));
                }
            }
        }
    }
}

/// The eight-lane interpreter: `DfProg::run` with `__m512d` slots (layer A2-c). `None`
/// means a conditional jump found the lanes disagreeing, and the caller finishes the group
/// through two `eval_x4` calls. Every arithmetic op is the plain IEEE-754 operation (NO FMA),
/// and the compare/blend sequences mirror `run4_avx2` exactly, so the eight results are the
/// eight `DfProg::run` results bit for bit - including the two zeros and NaN.
///
/// # Safety
/// The caller must have checked `is_x86_feature_detected!("avx512f")` and
/// `is_x86_feature_detected!("avx512dq")`, and `prog.slots` must be at most `DF4_SLOTS`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512dq")]
unsafe fn run8_avx512(
    prog: &DfProg,
    g: &Gen,
    cc: Option<&ChunkCols>,
    x: i32,
    y: i32,
    z: i32,
) -> Option<[f64; DF8]> {
    use std::arch::x86_64::*;
    unsafe {
        // Written before it is read: the program's compile-time depth and its runtime depth
        // agree (`DfProg::run` relies on the same invariant), so slot `sp` always holds a
        // value pushed earlier in the same evaluation.
        let mut st: [std::mem::MaybeUninit<__m512d>; DF4_SLOTS] =
            [std::mem::MaybeUninit::uninit(); DF4_SLOTS];
        macro_rules! rd {
            ($i:expr) => {
                st[$i].assume_init()
            };
        }
        macro_rules! wr {
            ($i:expr, $v:expr) => {
                st[$i] = std::mem::MaybeUninit::new($v)
            };
        }
        let lane_x: [f64; DF8] = std::array::from_fn(|k| (x + k as i32) as f64);
        let yv = _mm512_set1_pd(y as f64);
        let zero = _mm512_setzero_pd();
        let negzero = _mm512_set1_pd(-0.0);
        let abs_mask = _mm512_castsi512_pd(_mm512_set1_epi64(0x7fff_ffff_ffff_ffffu64 as i64));
        let mut sp = 0usize;
        let mut pc = 0usize;
        /// Two lane operands through one floating-point instruction.
        macro_rules! binop {
            ($f:ident) => {{
                let b = rd!(sp - 1);
                let a = rd!(sp - 2);
                sp -= 1;
                wr!(sp - 1, $f(a, b));
                pc += 1;
            }};
        }
        /// `f64::clamp`'s own two comparisons, so `NaN` and the sign of zero answer what the
        /// scalar method answers (`_mm512_mask_blend_pd(k, a, b)` picks `b` where `k` is set).
        macro_rules! clampv {
            ($v:expr, $lo:expr, $hi:expr) => {{
                let v = $v;
                let lo = _mm512_set1_pd($lo);
                let hi = _mm512_set1_pd($hi);
                let r = _mm512_mask_blend_pd(_mm512_cmp_pd_mask(v, lo, _CMP_LT_OQ), v, lo);
                _mm512_mask_blend_pd(_mm512_cmp_pd_mask(r, hi, _CMP_GT_OQ), r, hi)
            }};
        }
        /// [`df_min`] on eight lanes: the same four cases (`a < b`, `b < a`, equal, one NaN)
        /// and the same operand order as `run4_avx2`'s `minv!`.
        macro_rules! minv {
            ($a:expr, $b:expr) => {{
                let a = $a;
                let b = $b;
                let r = _mm512_mask_blend_pd(_mm512_cmp_pd_mask(a, b, _CMP_LT_OQ), b, a);
                let r = _mm512_mask_blend_pd(
                    _mm512_cmp_pd_mask(a, b, _CMP_EQ_OQ),
                    r,
                    _mm512_or_pd(a, b),
                );
                let r = _mm512_mask_blend_pd(_mm512_cmp_pd_mask(b, b, _CMP_UNORD_Q), r, a);
                _mm512_mask_blend_pd(_mm512_cmp_pd_mask(a, a, _CMP_UNORD_Q), r, b)
            }};
        }
        /// [`df_max`] on eight lanes, the same four cases (`+0.0` for the two zeros).
        macro_rules! maxv {
            ($a:expr, $b:expr) => {{
                let a = $a;
                let b = $b;
                let r = _mm512_mask_blend_pd(_mm512_cmp_pd_mask(a, b, _CMP_GT_OQ), b, a);
                let r = _mm512_mask_blend_pd(
                    _mm512_cmp_pd_mask(a, b, _CMP_EQ_OQ),
                    r,
                    _mm512_and_pd(a, b),
                );
                let r = _mm512_mask_blend_pd(_mm512_cmp_pd_mask(b, b, _CMP_UNORD_Q), r, a);
                _mm512_mask_blend_pd(_mm512_cmp_pd_mask(a, a, _CMP_UNORD_Q), r, b)
            }};
        }
        /// Eight `f64`s out of the top three stack slots, for the ops that must be scalar.
        macro_rules! lanes3 {
            () => {{
                let c = v8_to_arr(rd!(sp - 1));
                let b = v8_to_arr(rd!(sp - 2));
                let a = v8_to_arr(rd!(sp - 3));
                (a, b, c)
            }};
        }
        loop {
            match prog.ops[pc] {
                Op::Push(v) => {
                    wr!(sp, _mm512_set1_pd(v));
                    sp += 1;
                    pc += 1;
                }
                Op::Add => binop!(_mm512_add_pd),
                Op::Sub => binop!(_mm512_sub_pd),
                Op::Mul => binop!(_mm512_mul_pd),
                Op::Div => binop!(_mm512_div_pd),
                Op::Min => {
                    let b = rd!(sp - 1);
                    let a = rd!(sp - 2);
                    sp -= 1;
                    wr!(sp - 1, minv!(a, b));
                    pc += 1;
                }
                Op::Max => {
                    let b = rd!(sp - 1);
                    let a = rd!(sp - 2);
                    sp -= 1;
                    wr!(sp - 1, maxv!(a, b));
                    pc += 1;
                }
                Op::Neg => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm512_xor_pd(a, negzero));
                    pc += 1;
                }
                Op::Abs => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm512_and_pd(a, abs_mask));
                    pc += 1;
                }
                Op::Sq => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm512_mul_pd(a, a));
                    pc += 1;
                }
                Op::Cube => {
                    let a = rd!(sp - 1);
                    let sq = _mm512_mul_pd(a, a);
                    wr!(sp - 1, _mm512_mul_pd(sq, a));
                    pc += 1;
                }
                Op::HNeg => {
                    let a = rd!(sp - 1);
                    let half = _mm512_mul_pd(a, _mm512_set1_pd(0.5));
                    let pos = _mm512_cmp_pd_mask(a, zero, _CMP_GT_OQ);
                    wr!(sp - 1, _mm512_mask_blend_pd(pos, half, a));
                    pc += 1;
                }
                Op::QNeg => {
                    let a = rd!(sp - 1);
                    let quarter = _mm512_mul_pd(a, _mm512_set1_pd(0.25));
                    let pos = _mm512_cmp_pd_mask(a, zero, _CMP_GT_OQ);
                    wr!(sp - 1, _mm512_mask_blend_pd(pos, quarter, a));
                    pc += 1;
                }
                Op::Squeeze => {
                    let a = rd!(sp - 1);
                    let v = clampv!(a, -1.0, 1.0);
                    let cube = _mm512_mul_pd(_mm512_mul_pd(v, v), v);
                    let half = _mm512_div_pd(v, _mm512_set1_pd(2.0));
                    let over = _mm512_div_pd(cube, _mm512_set1_pd(24.0));
                    wr!(sp - 1, _mm512_sub_pd(half, over));
                    pc += 1;
                }
                Op::Clamp { lo, hi } => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, clampv!(a, lo, hi));
                    pc += 1;
                }
                Op::Lerp => {
                    let s = rd!(sp - 1);
                    let f = rd!(sp - 2);
                    let a = rd!(sp - 3);
                    sp -= 2;
                    wr!(sp - 1, _mm512_add_pd(f, _mm512_mul_pd(a, _mm512_sub_pd(s, f))));
                    pc += 1;
                }
                Op::AddC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm512_add_pd(a, _mm512_set1_pd(c)));
                    pc += 1;
                }
                Op::MulC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm512_mul_pd(a, _mm512_set1_pd(c)));
                    pc += 1;
                }
                Op::MinC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, minv!(a, _mm512_set1_pd(c)));
                    pc += 1;
                }
                Op::MaxC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, maxv!(a, _mm512_set1_pd(c)));
                    pc += 1;
                }
                Op::Grad { fc, fv, tc, tv } => {
                    let denom = _mm512_set1_pd(tc - fc);
                    let t = _mm512_div_pd(
                        _mm512_sub_pd(yv, _mm512_set1_pd(fc)),
                        denom,
                    );
                    let t = clampv!(t, 0.0, 1.0);
                    wr!(
                        sp,
                        _mm512_add_pd(
                            _mm512_set1_pd(fv),
                            _mm512_mul_pd(t, _mm512_set1_pd(tv - fv))
                        )
                    );
                    sp += 1;
                    pc += 1;
                }
                Op::Noise { ni, xzs, ys } => {
                    let (xs, ysv, zs) = lanes3!();
                    sp -= 2;
                    let noise = &g.noises[ni as usize];
                    // The z coordinate is `z as f64 * xzs + zs[k]`, NOT another copy of
                    // `lane_x[k] * xzs`: the lanes differ in x only.
                    let nx: [f64; DF8] = std::array::from_fn(|k| lane_x[k] * xzs + xs[k]);
                    let ny: [f64; DF8] = std::array::from_fn(|k| y as f64 * ys + ysv[k]);
                    let nz: [f64; DF8] = std::array::from_fn(|k| z as f64 * xzs + zs[k]);
                    let r = noise.get_8(nx, ny, nz);
                    wr!(sp - 1, v8_from_arr(r));
                    pc += 1;
                }
                Op::ShiftA(ni) => {
                    let noise = &g.noises[ni as usize];
                    let nx: [f64; DF8] = std::array::from_fn(|k| lane_x[k] * 0.25);
                    let nz = [z as f64 * 0.25; DF8];
                    let r = noise.get_8(nx, [0.0; DF8], nz).map(|v| v * 4.0);
                    wr!(sp, v8_from_arr(r));
                    sp += 1;
                    pc += 1;
                }
                Op::ShiftB(ni) => {
                    let noise = &g.noises[ni as usize];
                    let nx = [z as f64 * 0.25; DF8];
                    let ny: [f64; DF8] = std::array::from_fn(|k| lane_x[k] * 0.25);
                    let r = noise.get_8(nx, ny, [0.0; DF8]).map(|v| v * 4.0);
                    wr!(sp, v8_from_arr(r));
                    sp += 1;
                    pc += 1;
                }
                Op::Weird { ni, mapper } => {
                    let rarity_in = v8_to_arr(rd!(sp - 1));
                    let noise = &g.noises[ni as usize];
                    let rarity: [f64; DF8] = std::array::from_fn(|k| mapper.rarity(rarity_in[k]));
                    let nx: [f64; DF8] = std::array::from_fn(|k| lane_x[k] / rarity[k]);
                    let ny: [f64; DF8] = std::array::from_fn(|k| y as f64 / rarity[k]);
                    let nz: [f64; DF8] = std::array::from_fn(|k| z as f64 / rarity[k]);
                    let nv = noise.get_8(nx, ny, nz);
                    let r: [f64; DF8] = std::array::from_fn(|k| rarity[k] * nv[k].abs());
                    wr!(sp - 1, v8_from_arr(r));
                    pc += 1;
                }
                Op::Beardifier => {
                    let beard = cc.and_then(|cc| cc.beard.as_deref());
                    // The common case is no beardifier at all (or an empty one): the
                    // marker evaluates to zero at every lane, so push one vector rather
                    // than eight scalar `compute_beardifier` calls.
                    if beard.is_none_or(|b| b.is_empty()) {
                        wr!(sp, _mm512_setzero_pd());
                    } else {
                        let beard = beard.unwrap();
                        let r: [f64; DF8] = std::array::from_fn(|k| {
                            compute_beardifier(beard, x + k as i32, y, z)
                        });
                        wr!(sp, v8_from_arr(r));
                    }
                    sp += 1;
                    pc += 1;
                }
                // The grid's presence is a property of the columns, not of the position,
                // so this jump is lane-uniform: either all eight lanes read a trilinear
                // cell or all eight fall through to the node's own program.
                Op::Interp { slot, join } => {
                    let cells = cc
                        .and_then(|c| c.grids.get(slot as usize))
                        .and_then(|g| g.as_deref());
                    match (cells, cc) {
                        (Some(cells), Some(columns)) => {
                            let r = match trilinear_x8(cells, columns, x, y, z) {
                                Some(r) => r,
                                None => std::array::from_fn(|k| {
                                    trilinear(cells, columns, x + k as i32, y, z)
                                }),
                            };
                            wr!(sp, v8_from_arr(r));
                            sp += 1;
                            pc = join as usize;
                        }
                        _ => pc += 1,
                    }
                }
                Op::Call(id) => {
                    let r: [f64; DF8] = std::array::from_fn(|k| {
                        let mut memo = HashMap::new();
                        g.eval_c(id as usize, x + k as i32, y, z, &mut memo, cc)
                    });
                    wr!(sp, v8_from_arr(r));
                    sp += 1;
                    pc += 1;
                }
                Op::BranchOut { mi, ma, target } => {
                    // `t >= mi && t < ma` with ordered-quiet compares, exactly as in
                    // `run4_avx2` and the scalar `run`: NaN answers false in both, so it
                    // takes the "out of range" branch. The eight lanes must all agree.
                    let v = rd!(sp - 1);
                    let lo = _mm512_cmp_pd_mask(v, _mm512_set1_pd(mi), _CMP_GE_OQ);
                    let hi = _mm512_cmp_pd_mask(v, _mm512_set1_pd(ma), _CMP_LT_OQ);
                    match lo & hi {
                        0xFF => {
                            sp -= 1;
                            pc += 1;
                        }
                        0 => {
                            sp -= 1;
                            pc = target as usize;
                        }
                        _ => return None, // lanes disagree: caller finishes on eval_x4
                    }
                }
                Op::BranchLt { t, target } => {
                    let v = rd!(sp - 1);
                    let mask = _mm512_cmp_pd_mask(v, _mm512_set1_pd(t), _CMP_LT_OQ);
                    sp -= 1;
                    if mask == 0xFF {
                        pc = target as usize;
                    } else if mask == 0 {
                        pc += 1;
                    } else {
                        return None;
                    }
                }
                Op::BranchZero { target } => {
                    let v = rd!(sp - 1);
                    // `== 0.0` is true for both zeros and false for NaN, which is what
                    // the ordered-equal compare answers too.
                    let mask = _mm512_cmp_pd_mask(v, zero, _CMP_EQ_OQ);
                    if mask == 0xFF {
                        pc = target as usize;
                    } else if mask == 0 {
                        pc += 1;
                    } else {
                        return None;
                    }
                }
                Op::Jump(target) => pc = target as usize,
                Op::Drop => {
                    sp -= 1;
                    pc += 1;
                }
                // `DfCols` ops; a `DfProg` never contains them (see `run4_avx2`).
                Op::Push2(_) | Op::Store2(_) | Op::Done => return None,
                Op::Halt => {
                    debug_assert_eq!(sp, 1, "compiled program left {sp} values, not 1");
                    return Some(v8_to_arr(rd!(0)));
                }
            }
        }
    }
}

/// The column-fill four-lane interpreter: `run4_avx2` with the lanes turned
/// 90° — `y0, y0+ystep, y0+2*ystep, y0+3*ystep` of one corner column instead of
/// four `x` positions at one `y`. One column means `x`, `z` and every hoisted
/// `Op::Push2` value are lane-uniform, which is what `begin` left in `v2`.
/// `None` means a conditional jump found the lanes disagreeing, and the caller
/// finishes the group one level at a time.
///
/// Every arithmetic op is `run4_avx2`'s own sequence — the plain IEEE-754
/// operation (NO FMA), `df_min`/`df_max`'s compare/blend ladder, `f64::clamp`'s
/// two compares — so lane `k`'s result is the scalar `df_run` result at
/// `(x, y0 + k*ystep, z)` bit for bit.
///
/// # Safety
/// The caller must have checked `is_x86_feature_detected!("avx2")`, and the
/// program's compile-time stack depth must be at most `DF4_SLOTS` (the caller
/// checks both).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[expect(clippy::too_many_arguments)]
unsafe fn run4y_avx2(
    ops: &[Op],
    v2: &[f64],
    g: &Gen,
    cc: Option<&ChunkCols>,
    x: i32,
    y0: i32,
    ystep: i32,
    z: i32,
) -> Option<[f64; DF4]> {
    use std::arch::x86_64::*;
    unsafe {
        // Written before it is read: the program's compile-time depth and its
        // runtime depth agree (`df_run` relies on the same invariant), so slot
        // `sp` always holds a value pushed earlier in the same evaluation.
        let mut st: [std::mem::MaybeUninit<__m256d>; DF4_SLOTS] =
            [std::mem::MaybeUninit::uninit(); DF4_SLOTS];
        macro_rules! rd {
            ($i:expr) => {
                st[$i].assume_init()
            };
        }
        macro_rules! wr {
            ($i:expr, $v:expr) => {
                st[$i] = std::mem::MaybeUninit::new($v)
            };
        }
        // The one position input that varies across lanes.
        let lane_y: [f64; DF4] = std::array::from_fn(|k| (y0 + (k as i32) * ystep) as f64);
        let yv = v4_from_arr(lane_y);
        let xf = x as f64;
        let zf = z as f64;
        let mut sp = 0usize;
        let mut pc = 0usize;
        /// Two lane operands through one floating-point instruction.
        macro_rules! binop {
            ($f:ident) => {{
                let b = rd!(sp - 1);
                let a = rd!(sp - 2);
                sp -= 1;
                wr!(sp - 1, $f(a, b));
                pc += 1;
            }};
        }
        /// `f64::clamp`'s own two comparisons, so `NaN` and the sign of zero answer what the
        /// scalar method answers.
        macro_rules! clampv {
            ($v:expr, $lo:expr, $hi:expr) => {{
                let v = $v;
                let lo = _mm256_set1_pd($lo);
                let hi = _mm256_set1_pd($hi);
                let r = _mm256_blendv_pd(v, lo, _mm256_cmp_pd(v, lo, _CMP_LT_OQ));
                _mm256_blendv_pd(r, hi, _mm256_cmp_pd(r, hi, _CMP_GT_OQ))
            }};
        }
        /// [`df_min`] on four lanes: its four cases (`a < b`, `b < a`, equal, and one NaN),
        /// which is why this is not `_mm256_min_pd` - that op answers differently on the two
        /// zeros and on NaN.
        macro_rules! minv {
            ($a:expr, $b:expr) => {{
                let a = $a;
                let b = $b;
                let r = _mm256_blendv_pd(b, a, _mm256_cmp_pd(a, b, _CMP_LT_OQ));
                let r = _mm256_blendv_pd(r, _mm256_or_pd(a, b), _mm256_cmp_pd(a, b, _CMP_EQ_OQ));
                let r = _mm256_blendv_pd(r, a, _mm256_cmp_pd(b, b, _CMP_UNORD_Q));
                _mm256_blendv_pd(r, b, _mm256_cmp_pd(a, a, _CMP_UNORD_Q))
            }};
        }
        /// [`df_max`] on four lanes, the same four cases (`+0.0` for the two zeros).
        macro_rules! maxv {
            ($a:expr, $b:expr) => {{
                let a = $a;
                let b = $b;
                let r = _mm256_blendv_pd(b, a, _mm256_cmp_pd(a, b, _CMP_GT_OQ));
                let r =
                    _mm256_blendv_pd(r, _mm256_and_pd(a, b), _mm256_cmp_pd(a, b, _CMP_EQ_OQ));
                let r = _mm256_blendv_pd(r, b, _mm256_cmp_pd(a, b, _CMP_UNORD_Q));
                _mm256_blendv_pd(r, b, _mm256_cmp_pd(a, a, _CMP_UNORD_Q))
            }};
        }
        /// Eight `f64`s out of (or into) the stack, for the ops that have to be scalar.
        macro_rules! lanes3 {
            () => {{
                let c = v4_to_arr(rd!(sp - 1));
                let b = v4_to_arr(rd!(sp - 2));
                let a = v4_to_arr(rd!(sp - 3));
                (a, b, c)
            }};
        }
        loop {
            match ops[pc] {
                Op::Push(v) => {
                    wr!(sp, _mm256_set1_pd(v));
                    sp += 1;
                    pc += 1;
                }
                Op::Push2(k) => {
                    // One column, one hoisted value: the same slot feeds all
                    // four lanes.
                    debug_assert!((k as usize) < v2.len());
                    wr!(sp, _mm256_set1_pd(*v2.get_unchecked(k as usize)));
                    sp += 1;
                    pc += 1;
                }
                Op::Add => binop!(_mm256_add_pd),
                Op::Sub => binop!(_mm256_sub_pd),
                Op::Mul => binop!(_mm256_mul_pd),
                Op::Div => binop!(_mm256_div_pd),
                Op::Min => {
                    let b = rd!(sp - 1);
                    let a = rd!(sp - 2);
                    sp -= 1;
                    wr!(sp - 1, minv!(a, b));
                    pc += 1;
                }
                Op::Max => {
                    let b = rd!(sp - 1);
                    let a = rd!(sp - 2);
                    sp -= 1;
                    wr!(sp - 1, maxv!(a, b));
                    pc += 1;
                }
                Op::Neg => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm256_xor_pd(a, _mm256_set1_pd(-0.0)));
                    pc += 1;
                }
                Op::Abs => {
                    let a = rd!(sp - 1);
                    let mask = _mm256_castsi256_pd(_mm256_set1_epi64x(0x7fff_ffff_ffff_ffffu64 as i64));
                    wr!(sp - 1, _mm256_and_pd(a, mask));
                    pc += 1;
                }
                Op::Sq => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm256_mul_pd(a, a));
                    pc += 1;
                }
                Op::Cube => {
                    let a = rd!(sp - 1);
                    let sq = _mm256_mul_pd(a, a);
                    wr!(sp - 1, _mm256_mul_pd(sq, a));
                    pc += 1;
                }
                Op::HNeg => {
                    let a = rd!(sp - 1);
                    let half = _mm256_mul_pd(a, _mm256_set1_pd(0.5));
                    let pos = _mm256_cmp_pd(a, _mm256_setzero_pd(), _CMP_GT_OQ);
                    wr!(sp - 1, _mm256_blendv_pd(half, a, pos));
                    pc += 1;
                }
                Op::QNeg => {
                    let a = rd!(sp - 1);
                    let quarter = _mm256_mul_pd(a, _mm256_set1_pd(0.25));
                    let pos = _mm256_cmp_pd(a, _mm256_setzero_pd(), _CMP_GT_OQ);
                    wr!(sp - 1, _mm256_blendv_pd(quarter, a, pos));
                    pc += 1;
                }
                Op::Squeeze => {
                    let a = rd!(sp - 1);
                    let v = clampv!(a, -1.0, 1.0);
                    let cube = _mm256_mul_pd(_mm256_mul_pd(v, v), v);
                    let half = _mm256_div_pd(v, _mm256_set1_pd(2.0));
                    let over = _mm256_div_pd(cube, _mm256_set1_pd(24.0));
                    wr!(sp - 1, _mm256_sub_pd(half, over));
                    pc += 1;
                }
                Op::Clamp { lo, hi } => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, clampv!(a, lo, hi));
                    pc += 1;
                }
                Op::Lerp => {
                    let s = rd!(sp - 1);
                    let f = rd!(sp - 2);
                    let a = rd!(sp - 3);
                    sp -= 2;
                    wr!(sp - 1, _mm256_add_pd(f, _mm256_mul_pd(a, _mm256_sub_pd(s, f))));
                    pc += 1;
                }
                Op::AddC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm256_add_pd(a, _mm256_set1_pd(c)));
                    pc += 1;
                }
                Op::MulC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm256_mul_pd(a, _mm256_set1_pd(c)));
                    pc += 1;
                }
                Op::MinC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, minv!(a, _mm256_set1_pd(c)));
                    pc += 1;
                }
                Op::MaxC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, maxv!(a, _mm256_set1_pd(c)));
                    pc += 1;
                }
                Op::Grad { fc, fv, tc, tv } => {
                    let denom = _mm256_set1_pd(tc - fc);
                    let t = _mm256_div_pd(
                        _mm256_sub_pd(yv, _mm256_set1_pd(fc)),
                        denom,
                    );
                    let t = clampv!(t, 0.0, 1.0);
                    wr!(
                        sp,
                        _mm256_add_pd(
                            _mm256_set1_pd(fv),
                            _mm256_mul_pd(t, _mm256_set1_pd(tv - fv))
                        )
                    );
                    sp += 1;
                    pc += 1;
                }
                Op::Noise { ni, xzs, ys } => {
                    let (xs, ysv, zs) = lanes3!();
                    sp -= 2;
                    let noise = &g.noises[ni as usize];
                    // The lanes differ in y only: `x` and `z` are the column's
                    // scalars, `y` is the lane.
                    let nx: [f64; DF4] = std::array::from_fn(|k| xf * xzs + xs[k]);
                    let ny: [f64; DF4] = std::array::from_fn(|k| lane_y[k] * ys + ysv[k]);
                    let nz: [f64; DF4] = std::array::from_fn(|k| zf * xzs + zs[k]);
                    let r = noise.get_4(nx, ny, nz);
                    wr!(sp - 1, v4_from_arr(r));
                    pc += 1;
                }
                Op::ShiftA(ni) => {
                    let noise = &g.noises[ni as usize];
                    let nx = [xf * 0.25; DF4];
                    let nz = [zf * 0.25; DF4];
                    let r = noise.get_4(nx, [0.0; DF4], nz).map(|v| v * 4.0);
                    wr!(sp, v4_from_arr(r));
                    sp += 1;
                    pc += 1;
                }
                Op::ShiftB(ni) => {
                    let noise = &g.noises[ni as usize];
                    let nx = [zf * 0.25; DF4];
                    let ny = [xf * 0.25; DF4];
                    let r = noise.get_4(nx, ny, [0.0; DF4]).map(|v| v * 4.0);
                    wr!(sp, v4_from_arr(r));
                    sp += 1;
                    pc += 1;
                }
                Op::Weird { ni, mapper } => {
                    let rarity_in = v4_to_arr(rd!(sp - 1));
                    let noise = &g.noises[ni as usize];
                    let rarity: [f64; DF4] = std::array::from_fn(|k| mapper.rarity(rarity_in[k]));
                    let nx: [f64; DF4] = std::array::from_fn(|k| xf / rarity[k]);
                    let ny: [f64; DF4] = std::array::from_fn(|k| lane_y[k] / rarity[k]);
                    let nz: [f64; DF4] = std::array::from_fn(|k| zf / rarity[k]);
                    let nv = noise.get_4(nx, ny, nz);
                    let r: [f64; DF4] = std::array::from_fn(|k| rarity[k] * nv[k].abs());
                    wr!(sp - 1, v4_from_arr(r));
                    pc += 1;
                }
                Op::Beardifier => {
                    let beard = cc.and_then(|cc| cc.beard.as_deref());
                    if beard.is_none_or(|b| b.is_empty()) {
                        wr!(sp, _mm256_setzero_pd());
                    } else {
                        let beard = beard.unwrap();
                        let mut r = [0.0f64; DF4];
                        for (k, o) in r.iter_mut().enumerate() {
                            *o = compute_beardifier(beard, x, y0 + (k as i32) * ystep, z);
                        }
                        wr!(sp, v4_from_arr(r));
                    }
                    sp += 1;
                    pc += 1;
                }
                Op::Interp { slot, join } => {
                    let cells = cc
                        .and_then(|c| c.grids.get(slot as usize))
                        .and_then(|g| g.as_deref());
                    match (cells, cc) {
                        (Some(cells), Some(columns)) => {
                            let mut r = [0.0f64; DF4];
                            for (k, o) in r.iter_mut().enumerate() {
                                *o = trilinear(
                                    cells,
                                    columns,
                                    x,
                                    y0 + (k as i32) * ystep,
                                    z,
                                );
                            }
                            wr!(sp, v4_from_arr(r));
                            sp += 1;
                            pc = join as usize;
                        }
                        _ => pc += 1,
                    }
                }
                Op::Call(id) => {
                    let mut r = [0.0f64; DF4];
                    for (k, o) in r.iter_mut().enumerate() {
                        let mut memo = HashMap::new();
                        *o = g.eval_c(
                            id as usize,
                            x,
                            y0 + (k as i32) * ystep,
                            z,
                            &mut memo,
                            cc,
                        );
                    }
                    wr!(sp, v4_from_arr(r));
                    sp += 1;
                    pc += 1;
                }
                Op::BranchOut { mi, ma, target } => {
                    // `t >= mi && t < ma` with ordered-quiet compares: NaN answers
                    // false in both, exactly as the scalar `&&` does.
                    let v = rd!(sp - 1);
                    let lo = _mm256_cmp_pd(v, _mm256_set1_pd(mi), _CMP_GE_OQ);
                    let hi = _mm256_cmp_pd(v, _mm256_set1_pd(ma), _CMP_LT_OQ);
                    match _mm256_movemask_pd(_mm256_and_pd(lo, hi)) {
                        0xF => {
                            sp -= 1;
                            pc += 1;
                        }
                        0 => {
                            sp -= 1;
                            pc = target as usize;
                        }
                        _ => return None, // lanes disagree: caller finishes scalar
                    }
                }
                Op::BranchLt { t, target } => {
                    let v = rd!(sp - 1);
                    let mask = _mm256_movemask_pd(_mm256_cmp_pd(v, _mm256_set1_pd(t), _CMP_LT_OQ));
                    sp -= 1;
                    if mask == 0xF {
                        pc = target as usize;
                    } else if mask == 0 {
                        pc += 1;
                    } else {
                        return None;
                    }
                }
                Op::BranchZero { target } => {
                    let v = rd!(sp - 1);
                    // `== 0.0` is true for both zeros and false for NaN, which is
                    // what the ordered-equal compare answers too.
                    let mask =
                        _mm256_movemask_pd(_mm256_cmp_pd(v, _mm256_setzero_pd(), _CMP_EQ_OQ));
                    if mask == 0xF {
                        pc = target as usize;
                    } else if mask == 0 {
                        pc += 1;
                    } else {
                        return None;
                    }
                }
                Op::Jump(target) => pc = target as usize,
                Op::Drop => {
                    sp -= 1;
                    pc += 1;
                }
                // `ops2` ops never reach the per-level programs; treat one as a
                // bail-out rather than a lane op.
                Op::Store2(_) | Op::Done => return None,
                Op::Halt => {
                    debug_assert_eq!(sp, 1, "compiled program left {sp} values, not 1");
                    return Some(v4_to_arr(rd!(0)));
                }
            }
        }
    }
}

/// `run4y_avx2`'s eight-lane form: `y0 + k*ystep` for `k` in `0..8` of one
/// corner column on one `__m512d`, mirroring `run8_avx512`'s op-for-op
/// sequences (so each lane is the scalar `df_run` result bit for bit). `None`
/// means a conditional jump found the lanes disagreeing; the caller finishes
/// the group through two `run4y_avx2`-backed `eval_y4` calls.
///
/// # Safety
/// The caller must have checked `is_x86_feature_detected!("avx512f")` and
/// `is_x86_feature_detected!("avx512dq")`, and the program's compile-time stack
/// depth must be at most `DF4_SLOTS` (the caller checks both).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512dq")]
#[expect(clippy::too_many_arguments)]
unsafe fn run8y_avx512(
    ops: &[Op],
    v2: &[f64],
    g: &Gen,
    cc: Option<&ChunkCols>,
    x: i32,
    y0: i32,
    ystep: i32,
    z: i32,
) -> Option<[f64; DF8]> {
    use std::arch::x86_64::*;
    unsafe {
        // Written before it is read: the program's compile-time depth and its
        // runtime depth agree (`df_run` relies on the same invariant), so slot
        // `sp` always holds a value pushed earlier in the same evaluation.
        let mut st: [std::mem::MaybeUninit<__m512d>; DF4_SLOTS] =
            [std::mem::MaybeUninit::uninit(); DF4_SLOTS];
        macro_rules! rd {
            ($i:expr) => {
                st[$i].assume_init()
            };
        }
        macro_rules! wr {
            ($i:expr, $v:expr) => {
                st[$i] = std::mem::MaybeUninit::new($v)
            };
        }
        // The one position input that varies across lanes.
        let lane_y: [f64; DF8] = std::array::from_fn(|k| (y0 + (k as i32) * ystep) as f64);
        let yv = v8_from_arr(lane_y);
        let xf = x as f64;
        let zf = z as f64;
        let zero = _mm512_setzero_pd();
        let negzero = _mm512_set1_pd(-0.0);
        let abs_mask = _mm512_castsi512_pd(_mm512_set1_epi64(0x7fff_ffff_ffff_ffffu64 as i64));
        let mut sp = 0usize;
        let mut pc = 0usize;
        /// Two lane operands through one floating-point instruction.
        macro_rules! binop {
            ($f:ident) => {{
                let b = rd!(sp - 1);
                let a = rd!(sp - 2);
                sp -= 1;
                wr!(sp - 1, $f(a, b));
                pc += 1;
            }};
        }
        /// `f64::clamp`'s own two comparisons, so `NaN` and the sign of zero answer what the
        /// scalar method answers (`_mm512_mask_blend_pd(k, a, b)` picks `b` where `k` is set).
        macro_rules! clampv {
            ($v:expr, $lo:expr, $hi:expr) => {{
                let v = $v;
                let lo = _mm512_set1_pd($lo);
                let hi = _mm512_set1_pd($hi);
                let r = _mm512_mask_blend_pd(_mm512_cmp_pd_mask(v, lo, _CMP_LT_OQ), v, lo);
                _mm512_mask_blend_pd(_mm512_cmp_pd_mask(r, hi, _CMP_GT_OQ), r, hi)
            }};
        }
        /// [`df_min`] on eight lanes: the same four cases (`a < b`, `b < a`, equal, one NaN)
        /// and the same operand order as `run4_avx2`'s `minv!`.
        macro_rules! minv {
            ($a:expr, $b:expr) => {{
                let a = $a;
                let b = $b;
                let r = _mm512_mask_blend_pd(_mm512_cmp_pd_mask(a, b, _CMP_LT_OQ), b, a);
                let r = _mm512_mask_blend_pd(
                    _mm512_cmp_pd_mask(a, b, _CMP_EQ_OQ),
                    r,
                    _mm512_or_pd(a, b),
                );
                let r = _mm512_mask_blend_pd(_mm512_cmp_pd_mask(b, b, _CMP_UNORD_Q), r, a);
                _mm512_mask_blend_pd(_mm512_cmp_pd_mask(a, a, _CMP_UNORD_Q), r, b)
            }};
        }
        /// [`df_max`] on eight lanes, the same four cases (`+0.0` for the two zeros).
        macro_rules! maxv {
            ($a:expr, $b:expr) => {{
                let a = $a;
                let b = $b;
                let r = _mm512_mask_blend_pd(_mm512_cmp_pd_mask(a, b, _CMP_GT_OQ), b, a);
                let r = _mm512_mask_blend_pd(
                    _mm512_cmp_pd_mask(a, b, _CMP_EQ_OQ),
                    r,
                    _mm512_and_pd(a, b),
                );
                let r = _mm512_mask_blend_pd(_mm512_cmp_pd_mask(b, b, _CMP_UNORD_Q), r, a);
                _mm512_mask_blend_pd(_mm512_cmp_pd_mask(a, a, _CMP_UNORD_Q), r, b)
            }};
        }
        /// Eight `f64`s out of the top three stack slots, for the ops that must be scalar.
        macro_rules! lanes3 {
            () => {{
                let c = v8_to_arr(rd!(sp - 1));
                let b = v8_to_arr(rd!(sp - 2));
                let a = v8_to_arr(rd!(sp - 3));
                (a, b, c)
            }};
        }
        loop {
            match ops[pc] {
                Op::Push(v) => {
                    wr!(sp, _mm512_set1_pd(v));
                    sp += 1;
                    pc += 1;
                }
                Op::Push2(k) => {
                    // One column, one hoisted value: the same slot feeds all
                    // eight lanes.
                    debug_assert!((k as usize) < v2.len());
                    wr!(sp, _mm512_set1_pd(*v2.get_unchecked(k as usize)));
                    sp += 1;
                    pc += 1;
                }
                Op::Add => binop!(_mm512_add_pd),
                Op::Sub => binop!(_mm512_sub_pd),
                Op::Mul => binop!(_mm512_mul_pd),
                Op::Div => binop!(_mm512_div_pd),
                Op::Min => {
                    let b = rd!(sp - 1);
                    let a = rd!(sp - 2);
                    sp -= 1;
                    wr!(sp - 1, minv!(a, b));
                    pc += 1;
                }
                Op::Max => {
                    let b = rd!(sp - 1);
                    let a = rd!(sp - 2);
                    sp -= 1;
                    wr!(sp - 1, maxv!(a, b));
                    pc += 1;
                }
                Op::Neg => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm512_xor_pd(a, negzero));
                    pc += 1;
                }
                Op::Abs => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm512_and_pd(a, abs_mask));
                    pc += 1;
                }
                Op::Sq => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm512_mul_pd(a, a));
                    pc += 1;
                }
                Op::Cube => {
                    let a = rd!(sp - 1);
                    let sq = _mm512_mul_pd(a, a);
                    wr!(sp - 1, _mm512_mul_pd(sq, a));
                    pc += 1;
                }
                Op::HNeg => {
                    let a = rd!(sp - 1);
                    let half = _mm512_mul_pd(a, _mm512_set1_pd(0.5));
                    let pos = _mm512_cmp_pd_mask(a, zero, _CMP_GT_OQ);
                    wr!(sp - 1, _mm512_mask_blend_pd(pos, half, a));
                    pc += 1;
                }
                Op::QNeg => {
                    let a = rd!(sp - 1);
                    let quarter = _mm512_mul_pd(a, _mm512_set1_pd(0.25));
                    let pos = _mm512_cmp_pd_mask(a, zero, _CMP_GT_OQ);
                    wr!(sp - 1, _mm512_mask_blend_pd(pos, quarter, a));
                    pc += 1;
                }
                Op::Squeeze => {
                    let a = rd!(sp - 1);
                    let v = clampv!(a, -1.0, 1.0);
                    let cube = _mm512_mul_pd(_mm512_mul_pd(v, v), v);
                    let half = _mm512_div_pd(v, _mm512_set1_pd(2.0));
                    let over = _mm512_div_pd(cube, _mm512_set1_pd(24.0));
                    wr!(sp - 1, _mm512_sub_pd(half, over));
                    pc += 1;
                }
                Op::Clamp { lo, hi } => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, clampv!(a, lo, hi));
                    pc += 1;
                }
                Op::Lerp => {
                    let s = rd!(sp - 1);
                    let f = rd!(sp - 2);
                    let a = rd!(sp - 3);
                    sp -= 2;
                    wr!(sp - 1, _mm512_add_pd(f, _mm512_mul_pd(a, _mm512_sub_pd(s, f))));
                    pc += 1;
                }
                Op::AddC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm512_add_pd(a, _mm512_set1_pd(c)));
                    pc += 1;
                }
                Op::MulC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, _mm512_mul_pd(a, _mm512_set1_pd(c)));
                    pc += 1;
                }
                Op::MinC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, minv!(a, _mm512_set1_pd(c)));
                    pc += 1;
                }
                Op::MaxC(c) => {
                    let a = rd!(sp - 1);
                    wr!(sp - 1, maxv!(a, _mm512_set1_pd(c)));
                    pc += 1;
                }
                Op::Grad { fc, fv, tc, tv } => {
                    let denom = _mm512_set1_pd(tc - fc);
                    let t = _mm512_div_pd(
                        _mm512_sub_pd(yv, _mm512_set1_pd(fc)),
                        denom,
                    );
                    let t = clampv!(t, 0.0, 1.0);
                    wr!(
                        sp,
                        _mm512_add_pd(
                            _mm512_set1_pd(fv),
                            _mm512_mul_pd(t, _mm512_set1_pd(tv - fv))
                        )
                    );
                    sp += 1;
                    pc += 1;
                }
                Op::Noise { ni, xzs, ys } => {
                    let (xs, ysv, zs) = lanes3!();
                    sp -= 2;
                    let noise = &g.noises[ni as usize];
                    // The lanes differ in y only: `x` and `z` are the column's
                    // scalars, `y` is the lane.
                    let nx: [f64; DF8] = std::array::from_fn(|k| xf * xzs + xs[k]);
                    let ny: [f64; DF8] = std::array::from_fn(|k| lane_y[k] * ys + ysv[k]);
                    let nz: [f64; DF8] = std::array::from_fn(|k| zf * xzs + zs[k]);
                    let r = noise.get_8(nx, ny, nz);
                    wr!(sp - 1, v8_from_arr(r));
                    pc += 1;
                }
                Op::ShiftA(ni) => {
                    let noise = &g.noises[ni as usize];
                    let nx = [xf * 0.25; DF8];
                    let nz = [zf * 0.25; DF8];
                    let r = noise.get_8(nx, [0.0; DF8], nz).map(|v| v * 4.0);
                    wr!(sp, v8_from_arr(r));
                    sp += 1;
                    pc += 1;
                }
                Op::ShiftB(ni) => {
                    let noise = &g.noises[ni as usize];
                    let nx = [zf * 0.25; DF8];
                    let ny = [xf * 0.25; DF8];
                    let r = noise.get_8(nx, ny, [0.0; DF8]).map(|v| v * 4.0);
                    wr!(sp, v8_from_arr(r));
                    sp += 1;
                    pc += 1;
                }
                Op::Weird { ni, mapper } => {
                    let rarity_in = v8_to_arr(rd!(sp - 1));
                    let noise = &g.noises[ni as usize];
                    let rarity: [f64; DF8] = std::array::from_fn(|k| mapper.rarity(rarity_in[k]));
                    let nx: [f64; DF8] = std::array::from_fn(|k| xf / rarity[k]);
                    let ny: [f64; DF8] = std::array::from_fn(|k| lane_y[k] / rarity[k]);
                    let nz: [f64; DF8] = std::array::from_fn(|k| zf / rarity[k]);
                    let nv = noise.get_8(nx, ny, nz);
                    let r: [f64; DF8] = std::array::from_fn(|k| rarity[k] * nv[k].abs());
                    wr!(sp - 1, v8_from_arr(r));
                    pc += 1;
                }
                Op::Beardifier => {
                    let beard = cc.and_then(|cc| cc.beard.as_deref());
                    if beard.is_none_or(|b| b.is_empty()) {
                        wr!(sp, _mm512_setzero_pd());
                    } else {
                        let beard = beard.unwrap();
                        let r: [f64; DF8] = std::array::from_fn(|k| {
                            compute_beardifier(beard, x, y0 + (k as i32) * ystep, z)
                        });
                        wr!(sp, v8_from_arr(r));
                    }
                    sp += 1;
                    pc += 1;
                }
                Op::Interp { slot, join } => {
                    let cells = cc
                        .and_then(|c| c.grids.get(slot as usize))
                        .and_then(|g| g.as_deref());
                    match (cells, cc) {
                        (Some(cells), Some(columns)) => {
                            let r: [f64; DF8] = std::array::from_fn(|k| {
                                trilinear(cells, columns, x, y0 + (k as i32) * ystep, z)
                            });
                            wr!(sp, v8_from_arr(r));
                            sp += 1;
                            pc = join as usize;
                        }
                        _ => pc += 1,
                    }
                }
                Op::Call(id) => {
                    let r: [f64; DF8] = std::array::from_fn(|k| {
                        let mut memo = HashMap::new();
                        g.eval_c(
                            id as usize,
                            x,
                            y0 + (k as i32) * ystep,
                            z,
                            &mut memo,
                            cc,
                        )
                    });
                    wr!(sp, v8_from_arr(r));
                    sp += 1;
                    pc += 1;
                }
                Op::BranchOut { mi, ma, target } => {
                    // `t >= mi && t < ma` with ordered-quiet compares, exactly as in
                    // `run4y_avx2` and the scalar `run`: NaN answers false in both, so
                    // it takes the "out of range" branch. The lanes must all agree.
                    let v = rd!(sp - 1);
                    let lo = _mm512_cmp_pd_mask(v, _mm512_set1_pd(mi), _CMP_GE_OQ);
                    let hi = _mm512_cmp_pd_mask(v, _mm512_set1_pd(ma), _CMP_LT_OQ);
                    match lo & hi {
                        0xFF => {
                            sp -= 1;
                            pc += 1;
                        }
                        0 => {
                            sp -= 1;
                            pc = target as usize;
                        }
                        _ => return None,
                    }
                }
                Op::BranchLt { t, target } => {
                    let v = rd!(sp - 1);
                    let mask = _mm512_cmp_pd_mask(v, _mm512_set1_pd(t), _CMP_LT_OQ);
                    sp -= 1;
                    if mask == 0xFF {
                        pc = target as usize;
                    } else if mask == 0 {
                        pc += 1;
                    } else {
                        return None;
                    }
                }
                Op::BranchZero { target } => {
                    let v = rd!(sp - 1);
                    // `== 0.0` is true for both zeros and false for NaN, which is
                    // what the ordered-equal compare answers too.
                    let mask = _mm512_cmp_pd_mask(v, zero, _CMP_EQ_OQ);
                    if mask == 0xFF {
                        pc = target as usize;
                    } else if mask == 0 {
                        pc += 1;
                    } else {
                        return None;
                    }
                }
                Op::Jump(target) => pc = target as usize,
                Op::Drop => {
                    sp -= 1;
                    pc += 1;
                }
                // `ops2` ops never reach the per-level programs; treat one as a
                // bail-out rather than a lane op.
                Op::Store2(_) | Op::Done => return None,
                Op::Halt => {
                    debug_assert_eq!(sp, 1, "compiled program left {sp} values, not 1");
                    return Some(v8_to_arr(rd!(0)));
                }
            }
        }
    }
}

struct DfCompiler<'a> {
    d: &'a GenData,
    ops: Vec<Op>,
    /// Compile-time stack depth: every `emit` leaves exactly one value.
    depth: usize,
    max_depth: usize,
    /// Folded constant per node id, filled on demand (`consts_known`).
    consts: Vec<Option<f64>>,
    consts_known: Vec<bool>,
    /// Column-compilation state: `Some` only while `DfCols::compile` emits. When
    /// set, a y-independent subtree leaves the main stream for `ops2` and the
    /// main stream pushes its slot (`Op::Push2`) — the same separation
    /// `FlatCornerMemo`'s `val_2d`/`cur_2d` generations perform at runtime.
    col: Option<ColEmit>,
}

/// `DfCols` emission: the hoisted 2D prefix and the node-to-slot map.
struct ColEmit {
    /// Every hoisted subtree's instructions, each run ending in `Op::Store2`.
    ops2: Vec<Op>,
    /// Node id -> hoisted slot (`u32::MAX` = not hoisted yet).
    slot_of: Vec<u32>,
    /// Slots handed out so far; also the `v2` length the evaluator needs.
    n2: u32,
}

impl<'a> DfCompiler<'a> {
    /// The node's value when it does not depend on the position at all. Anything
    /// that reads `x`/`y`/`z`, a noise, a spline, a chunk grid or the Beardifier
    /// is not a constant.
    fn const_value(&mut self, id: usize) -> Option<f64> {
        if self.consts_known[id] {
            return self.consts[id];
        }
        let node = &self.d.dfs[id];
        let v = match node {
            DF::C(v) => Some(*v),
            DF::Zero => Some(0.0),
            DF::Add(a, b) => match (self.const_value(*a), self.const_value(*b)) {
                (Some(a), Some(b)) => Some(a + b),
                _ => None,
            },
            DF::Sub(a, b) => match (self.const_value(*a), self.const_value(*b)) {
                (Some(a), Some(b)) => Some(a - b),
                _ => None,
            },
            DF::Mul(a, b) => match (self.const_value(*a), self.const_value(*b)) {
                (Some(a), Some(b)) => Some(a * b),
                _ => None,
            },
            DF::Min(a, b) => match (self.const_value(*a), self.const_value(*b)) {
                (Some(a), Some(b)) => Some(df_min(a, b)),
                _ => None,
            },
            DF::Max(a, b) => match (self.const_value(*a), self.const_value(*b)) {
                (Some(a), Some(b)) => Some(df_max(a, b)),
                _ => None,
            },
            DF::Div(a, b) => match self.const_value(*b) {
                Some(0.0) => Some(0.0),
                Some(b) => self.const_value(*a).map(|a| a / b),
                None => None,
            },
            DF::Neg(a) => self.const_value(*a).map(|v| -v),
            DF::Abs(a) => self.const_value(*a).map(|v| v.abs()),
            DF::Sq(a) => self.const_value(*a).map(|v| v * v),
            DF::Cube(a) => self.const_value(*a).map(|v| v * v * v),
            DF::HNeg(a) => self.const_value(*a).map(|v| if v > 0.0 { v } else { v * 0.5 }),
            DF::QNeg(a) => self.const_value(*a).map(|v| if v > 0.0 { v } else { v * 0.25 }),
            DF::Squeeze(a) => self.const_value(*a).map(|v| {
                let v = v.clamp(-1.0, 1.0);
                v / 2.0 - v * v * v / 24.0
            }),
            DF::Clamp { i, lo, hi } => self.const_value(*i).map(|v| v.clamp(*lo, *hi)),
            DF::Lerp { a, f, s } => match (
                self.const_value(*a),
                self.const_value(*f),
                self.const_value(*s),
            ) {
                (Some(a), Some(f), Some(s)) => Some(f + a * (s - f)),
                _ => None,
            },
            DF::Cache(a) | DF::BlendD(a) => self.const_value(*a),
            _ => None,
        };
        self.consts[id] = v;
        self.consts_known[id] = true;
        v
    }

    /// Emit one node. On entry the stack holds `depth` values; on exit it holds
    /// `depth + 1` with the node's value on top.
    fn emit(&mut self, id: usize) {
        if let Some(v) = self.const_value(id) {
            self.ops.push(Op::Push(v));
            self.stack_push(1);
            return;
        }
        // Column compilation: a y-independent subtree is emitted into `ops2`
        // once per `(x, z)` column and the main stream pushes its slot. The ops
        // are the same ones the subtree would have emitted inline - `Push2` is
        // a value-transparent indirection, so nothing about the result can
        // differ - it just no longer runs `ny` times per column.
        if self.col.is_some() && self.d.is_2d.get(id).copied().unwrap_or(false) {
            self.emit_hoisted(id);
            return;
        }
        let node = &self.d.dfs[id];
        match node {
            DF::Add(a, b) => self.bin_op(*a, *b, Op::Add),
            DF::Sub(a, b) => self.bin_op(*a, *b, Op::Sub),
            DF::Mul(a, b) => self.bin_op(*a, *b, Op::Mul),
            DF::Min(a, b) => self.bin_op(*a, *b, Op::Min),
            DF::Max(a, b) => self.bin_op(*a, *b, Op::Max),
            DF::Div(a, b) => self.emit_div(*a, *b),
            DF::Neg(a) => self.un_op(*a, Op::Neg),
            DF::Abs(a) => self.un_op(*a, Op::Abs),
            DF::Sq(a) => self.un_op(*a, Op::Sq),
            DF::Cube(a) => self.un_op(*a, Op::Cube),
            DF::HNeg(a) => self.un_op(*a, Op::HNeg),
            DF::QNeg(a) => self.un_op(*a, Op::QNeg),
            DF::Squeeze(a) => self.un_op(*a, Op::Squeeze),
            DF::Clamp { i, lo, hi } => self.un_op(*i, Op::Clamp { lo: *lo, hi: *hi }),
            DF::Lerp { a, f, s } => {
                self.emit(*a);
                self.emit(*f);
                self.emit(*s);
                self.stack_pop(2);
                self.ops.push(Op::Lerp);
            }
            DF::Grad { fc, fv, tc, tv } => {
                self.ops.push(Op::Grad {
                    fc: *fc,
                    fv: *fv,
                    tc: *tc,
                    tv: *tv,
                });
                self.stack_push(1);
            }
            // A cache node's memo lives exactly one block (see the section
            // comment), so it compiles to its input.
            DF::Cache(a) | DF::BlendD(a) => self.emit(*a),
            DF::Interp(inner) => {
                let slot = id as u32;
                let at = self.ops.len();
                self.ops.push(Op::Interp { slot, join: 0 });
                self.emit(*inner);
                let join = self.ops.len() as u32;
                self.ops[at] = Op::Interp { slot, join };
            }
            DF::Noise {
                ni,
                sx,
                sy,
                sz,
                xzs,
                ys,
            } => {
                self.emit(*sx);
                self.emit(*sy);
                self.emit(*sz);
                self.stack_pop(2);
                self.ops.push(Op::Noise {
                    ni: *ni as u32,
                    xzs: *xzs,
                    ys: *ys,
                });
            }
            DF::ShiftA(ni) => {
                self.ops.push(Op::ShiftA(*ni as u32));
                self.stack_push(1);
            }
            DF::ShiftB(ni) => {
                self.ops.push(Op::ShiftB(*ni as u32));
                self.stack_push(1);
            }
            DF::WeirdScaledSampler { input, ni, mapper } => {
                self.emit(*input);
                self.ops.push(Op::Weird {
                    ni: *ni as u32,
                    mapper: *mapper,
                });
            }
            DF::Beardifier => {
                self.ops.push(Op::Beardifier);
                self.stack_push(1);
            }
            DF::RangeChoice { i, mi, ma, wr, wo } => {
                self.emit(*i);
                self.stack_pop(1);
                let at = self.ops.len();
                self.ops.push(Op::BranchOut {
                    mi: *mi,
                    ma: *ma,
                    target: 0,
                });
                self.emit(*wr);
                let to_end = self.ops.len();
                self.ops.push(Op::Jump(0));
                let out_of_range = self.ops.len() as u32;
                self.ops[at] = Op::BranchOut {
                    mi: *mi,
                    ma: *ma,
                    target: out_of_range,
                };
                self.emit(*wo);
                let end = self.ops.len() as u32;
                self.ops[to_end] = Op::Jump(end);
            }
            DF::IntervalSelect { i, th, fns } => self.emit_interval_select(*i, th, fns),
            // Cold or non-f64 subtrees (see the section comment): keep the
            // interpreter for them.
            DF::Spline(_) | DF::FindTopSurface { .. } | DF::Blended(_) | DF::EndIslands(_) => {
                self.ops.push(Op::Call(id as u32));
                self.stack_push(1);
            }
            // Everything else was constant-folded above, `C`/`Zero` included.
            DF::C(_) | DF::Zero => unreachable!("constant node missed by const_value"),
        }
    }

    fn emit_interval_select(&mut self, i: usize, th: &[f64], fns: &[usize]) {
        // The interpreter's rule: `pick` is the first `k` with `v < th[k]`,
        // otherwise `fns.len() - 1`, then clamped to `fns.len() - 1`. That needs
        // at least one bucket and more buckets than thresholds; anything else
        // falls back to the interpreter rather than inventing a shape.
        if fns.is_empty() || th.is_empty() || th.len() >= fns.len() {
            self.ops.push(Op::Call(i as u32));
            self.stack_push(1);
            return;
        }
        self.emit(i);
        self.stack_pop(1);
        let mut tests = Vec::with_capacity(th.len());
        for _ in 0..th.len() {
            tests.push(self.ops.len());
            self.ops.push(Op::BranchLt { t: 0.0, target: 0 });
        }
        let miss = self.ops.len();
        self.ops.push(Op::Jump(0));
        let mut ends = Vec::with_capacity(th.len());
        for (k, fn_id) in fns.iter().enumerate().take(th.len()) {
            let at = self.ops.len() as u32;
            self.ops[tests[k]] = Op::BranchLt { t: th[k], target: at };
            self.emit(*fn_id);
            ends.push(self.ops.len());
            self.ops.push(Op::Jump(0));
        }
        let last = fns.len() - 1;
        self.ops[miss] = Op::Jump(self.ops.len() as u32);
        self.emit(fns[last]);
        let end = self.ops.len() as u32;
        for e in ends {
            self.ops[e] = Op::Jump(end);
        }
    }

    fn emit_div(&mut self, a: usize, b: usize) {
        // `Div` evaluates the divisor first and returns 0.0 without evaluating
        // the dividend when it is zero.
        self.emit(b);
        let at = self.ops.len();
        self.ops.push(Op::BranchZero { target: 0 });
        self.emit(a);
        self.stack_pop(1);
        self.ops.push(Op::Div);
        let to_end = self.ops.len();
        self.ops.push(Op::Jump(0));
        let zero = self.ops.len() as u32;
        self.ops[at] = Op::BranchZero { target: zero };
        self.ops.push(Op::Drop);
        self.ops.push(Op::Push(0.0));
        let end = self.ops.len() as u32;
        self.ops[to_end] = Op::Jump(end);
    }

    /// Column compilation of a y-independent node: emit the subtree into
    /// `ops2` (terminated by `Op::Store2`) and leave `Op::Push2` in the main
    /// stream. The first caller pays the emission; every later reference to the
    /// same node reuses its slot, matching `FlatCornerMemo`'s one-per-column
    /// `val_2d` write.
    fn emit_hoisted(&mut self, id: usize) {
        let existing = self.col.as_ref().unwrap().slot_of[id];
        if existing != u32::MAX {
            self.ops.push(Op::Push2(existing));
            self.stack_push(1);
            return;
        }
        let k = {
            let col = self.col.as_mut().unwrap();
            let k = col.n2;
            col.n2 += 1;
            col.slot_of[id] = k;
            k
        };
        // `self.ops` IS `ops2` while the subtree emits, so every jump target it
        // resolves lands in the list the prefix interpreter walks. `col` is
        // dropped for the duration, so a nested is_2d node inside the subtree
        // emits inline rather than taking a second slot for the same column's
        // work.
        let mut col = self.col.take().unwrap();
        std::mem::swap(&mut self.ops, &mut col.ops2);
        let depth = self.depth;
        self.depth = 0;
        self.emit(id);
        self.ops.push(Op::Store2(k));
        self.depth = depth;
        std::mem::swap(&mut self.ops, &mut col.ops2);
        self.col = Some(col);
        self.ops.push(Op::Push2(k));
        self.stack_push(1);
    }

    fn bin_op(&mut self, a: usize, b: usize, op: Op) {
        // A constant operand becomes an immediate form for the four ops where
        // that is exact (see `Op::AddC`); the rest keep a plain `Push`.
        let imm = matches!(op, Op::Add | Op::Mul | Op::Min | Op::Max);
        if imm {
            let (dyn_id, c) = match (self.const_value(a), self.const_value(b)) {
                (Some(c), None) => (b, c),
                (None, Some(c)) => (a, c),
                _ => {
                    self.emit(a);
                    self.emit(b);
                    self.stack_pop(1);
                    self.ops.push(op);
                    return;
                }
            };
            self.emit(dyn_id);
            self.ops.push(match op {
                Op::Add => Op::AddC(c),
                Op::Mul => Op::MulC(c),
                Op::Min => Op::MinC(c),
                _ => Op::MaxC(c),
            });
            return;
        }
        self.emit(a);
        self.emit(b);
        self.stack_pop(1);
        self.ops.push(op);
    }

    fn un_op(&mut self, a: usize, op: Op) {
        self.emit(a);
        self.ops.push(op);
    }

    fn stack_push(&mut self, n: usize) {
        self.depth += n;
        if self.depth > self.max_depth {
            self.max_depth = self.depth;
        }
    }

    fn stack_pop(&mut self, n: usize) {
        self.depth -= n;
    }
}

// ---------- material rule evaluation ----------

fn create_bands(r: &mut Xor, res: fn(&str) -> Option<u32>) -> Option<Box<[u32; 192]>> {
    let t = res("minecraft:terracotta")?;
    let o = res("minecraft:orange_terracotta")?;
    let y = res("minecraft:yellow_terracotta")?;
    let br = res("minecraft:brown_terracotta")?;
    let rd = res("minecraft:red_terracotta")?;
    let w = res("minecraft:white_terracotta")?;
    let lg = res("minecraft:light_gray_terracotta")?;
    let mut b = [t; 192];
    let mut i = 0i32;
    while i < 192 {
        i += r.next_bound(5) + 1;
        if i >= 192 {
            break;
        }
        b[i as usize] = o;
        i += 1;
    }
    fn add_bands(r: &mut Xor, b: &mut [u32; 192], mn: i32, st: u32) {
        let n = r.next_inbetween(6, 15);
        for _ in 0..n {
            let wd = mn + r.next_bound(3);
            let s = r.next_bound(192);
            let mut m = 0;
            while m < wd && s + m < 192 {
                b[(s + m) as usize] = st;
                m += 1;
            }
        }
    }
    add_bands(r, &mut b, 1, y);
    add_bands(r, &mut b, 2, br);
    add_bands(r, &mut b, 1, rd);
    let n = r.next_inbetween(9, 15);
    let (mut cur, mut idx) = (0, 0i32);
    while cur < n && idx < 192 {
        b[idx as usize] = w;
        if idx > 1 && r.next_bool() {
            b[(idx - 1) as usize] = lg;
        }
        if idx + 1 < 192 && r.next_bool() {
            b[(idx + 1) as usize] = lg;
        }
        idx += r.next_bound(16) + 4;
        cur += 1;
    }
    Some(Box::new(b))
}

// per-16-block-corner preliminary surface estimates (global, keyed by quart coord)

impl Gen {
    // surface noise -> vanilla "surface depth" (run depth), per column
    fn run_depth(&self, x: i32, z: i32) -> i32 {
        let n = self
            .surf_ni
            .map(|ni| self.noises[ni].get(x as f64, 0.0, z as f64))
            .unwrap_or(0.0);
        let mut r = self.factory.pos(x, 0, z);
        let random = if self.legacy_random_source {
            Lcg::new(legacy_world_factory_seed(self.world_seed) ^ hash_block_pos(x, 0, z) as u64)
                .next_f64()
        } else {
            r.next_f64()
        };
        (n * 2.75 + 3.0 + random * 0.25) as i32
    }

    fn sec_depth(&self, c: &mut SC) -> f32 {
        if !c.sec_ok {
            c.sec = self
                .sec_ni
                .map(|ni| self.noises[ni].get(c.x as f64, 0.0, c.z as f64))
                .unwrap_or(0.0) as f32;
            c.sec_ok = true;
        }
        c.sec
    }

    fn leaf_at(&mut self, c: &mut SC) -> u16 {
        // Several predicates can query the same cell during one rule evaluation.
        let pos = (c.x, c.y, c.z);
        if let Some((last, leaf)) = c.biome {
            if last == pos {
                return leaf;
            }
        }
        // SurfaceRules.Context uses BiomeManager to select a quart from the
        // already filled biome palettes, including those in adjacent chunks.
        let (qx, qy, qz) = self.fiddled_biome_quart(c.x, c.y, c.z);
        let qy = qy.clamp(self.world_min_y >> 2, (self.world_min_y + self.world_height - 1) >> 2);
        let key = (qx, qy, qz);
        if let Some(&leaf) = self.bioq.get(&key) {
            c.biome = Some((pos, leaf));
            return leaf;
        }
        let (qx0, qy0, qz0) = (key.0 & !3, key.1 & !3, key.2 & !3);
        let mut last = None;
        // LevelChunkSection.fillBiomesFromNoise walks X, then Y, then Z.
        // Retain the preceding R-tree entry and its bounds across this section.
        // The 64 quarts share only 16 (qx, qz) climate columns; keep them in a 4x4
        // table so the inner walk hits memory instead of the `clim_c` map for the
        // other 48 quarts.
        type ClimCol = Option<([i64; 5], f64)>;
        let mut col4: [[ClimCol; 4]; 4] = [[None; 4]; 4];
        for qx in qx0..qx0 + 4 {
            for qy in qy0..qy0 + 4 {
                for qz in qz0..qz0 + 4 {
                    let pos = (qx, qy, qz);
                    let leaf = if self.preset == NoisePreset::End {
                        self.biome_quart(qx, qy, qz, None).map(|(_, leaf)| leaf)
                    } else if let Some(target) = {
                        let (cx4, cz4) = ((qx - qx0) as usize, (qz - qz0) as usize);
                        let col = match col4[cx4][cz4] {
                            v @ Some(_) => v,
                            None => {
                                col4[cx4][cz4] = self.climate_col(qx, qz);
                                col4[cx4][cz4]
                            }
                        };
                        col.and_then(|(c5, off)| self.target7_with_col(c5, off, qx, qy, qz))
                    } {
                        self.btree.as_ref().map(|tree| {
                            let mut best = last.as_ref().map_or((i64::MAX, 0), |(id, bounds)| {
                                (ps_dist(bounds, &target), *id)
                            });
                            if let Some(bounds) = tree.search(&target, &mut best) {
                                last = Some((best.1, *bounds));
                            }
                            best.1
                        })
                    } else {
                        None
                    };
                    self.bioq.insert(pos, leaf.unwrap_or(u16::MAX));
                    self.bioq_ord.push_back(pos);
                }
            }
        }
        while self.bioq_ord.len() > BIO_CAP {
            if let Some(old) = self.bioq_ord.pop_front() {
                self.bioq.remove(&old);
            }
        }
        let leaf = self.bioq[&key];
        c.biome = Some((pos, leaf));
        leaf
    }

    // find_top_surface of the preliminary_surface_level DF, cached per quart column
    pub(crate) fn prelim_est(&mut self, x: i32, z: i32) -> i32 {
        if let Some(spec) = env_str!("PRELIM_DUMP") {
            static DONE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            let v = self.prelim_est_raw(x, z);
            if DONE.get().is_none() && spec == format!("{x},{z}") {
                let _ = DONE.set(());
                for dz in 0..16 {
                    for dx in 0..16 {
                        let n = self.prelim_est_raw(x + dx, z + dz);
                        eprintln!("PRELIM {} {} {}", x + dx, z + dz, n);
                    }
                }
            }
            return v;
        }
        self.prelim_est_raw(x, z)
    }
    fn prelim_est_raw(&mut self, x: i32, z: i32) -> i32 {
        // NoiseChunk.preliminarySurfaceLevel keys by QuartPos (x >> 2, z >> 2).
        let k = (x >> 2, z >> 2);
        let m3_read = m3_on() && m3_in_feature_read();
        if let Some(&v) = self.prelim_c.get(&k) {
            inc_if(m3_read, M3_PRELIM_HIT);
            return v;
        }
        inc_if(m3_read, M3_PRELIM_MISS);
        let v = match self.prelim_df {
            Some(id) => {
                let top = self.min_y + self.height;
                let (density, start, step, threshold) = match self.dfs.get(id) {
                    // Converted dialect only: its node carries explicit bounds.
                    Some(DF::FindTopSurface {
                        density, cell_h, ..
                    }) => {
                        let step = (*cell_h).max(1);
                        (*density, (top / step) * step, step, 0.0)
                    }
                    // 1.21.1: scan initial_density_without_jaggedness from
                    // minY + height down to minY by the noise cell height and
                    // take the first value above 0.390625. The column is a
                    // QuartPos column, not a section column.
                    _ => (id, top, self.noise_cell_h.max(1), 0.390625),
                };
                let (ax, az) = (k.0 << 2, k.1 << 2);
                let mut levels = (self.min_y..=start).rev().step_by(step as usize);
                let found = if wg_count() {
                    levels.find(|&y| {
                        self.eval(density, ax, y, az, &mut HashMap::new()) > threshold
                    })
                } else {
                    // The corner evaluator already separates values fixed for this
                    // column from values that must be refreshed at every Y sample.
                    let mut memo = self.take_corner_memo();
                    memo.next_2d();
                    let found = levels.find(|&y| {
                        memo.next_3d();
                        self.eval_flat(density, ax, y, az, &mut memo) > threshold
                    });
                    self.corner_memo = Some(memo);
                    found
                };
                found.unwrap_or(i32::MAX)
            }
            None => i32::MAX,
        };
        self.prelim_c.insert(k, v);
        self.prelim_ord.push_back(k);
        if self.prelim_ord.len() > 32768 {
            let old = self.prelim_ord.pop_front().unwrap();
            self.prelim_c.remove(&old);
        }
        v
    }

    /// Vanilla's preliminary surface level for one column: the bilinear
    /// interpolation of the four corner heights, plus the surface noise, minus
    /// the 8 block window (`SurfaceRules.Context`'s preliminary surface level).
    /// `above_preliminary_surface` is `y >= this`.
    pub(crate) fn prelim_min(&mut self, x: i32, z: i32) -> i32 {
        let (cx, cz) = (x >> 4, z >> 4);
        let h00 = self.prelim_est(cx * 16, cz * 16);
        let h10 = self.prelim_est(cx * 16 + 16, cz * 16);
        let h01 = self.prelim_est(cx * 16, cz * 16 + 16);
        let h11 = self.prelim_est(cx * 16 + 16, cz * 16 + 16);
        let est = lerp2(
            (x & 15) as f64 / 16.0,
            (z & 15) as f64 / 16.0,
            h00 as f64,
            h10 as f64,
            h01 as f64,
            h11 as f64,
        )
        .floor() as i32;
        est.wrapping_add(self.run_depth(x, z)).wrapping_sub(8)
    }

    fn test_cond(&mut self, cid: usize, c: &mut SC, heightmap: &[i16; 256]) -> bool {
        let cd = self.conds[cid];
        match cd {
            Cond::False => false,
            Cond::Temperature => {
                let leaf = self.leaf_at(c);
                self.biome_weather
                    .get(leaf as usize)
                    .is_some_and(|weather| weather.cold(c.x, c.y, c.z))
            }
            Cond::Biome(mask) => {
                let l = self.leaf_at(c);
                // `leaf_at` answers `u16::MAX` for a missing biome; every out-of-range
                // leaf is excluded by the same bound. The shift itself is short-circuited
                // for any `l >= 128`.
                usize::from(l) < 128 && (mask >> usize::from(l)) & 1 == 1
            }
            Cond::NoiseTh { ni, is3d, lo, hi } => {
                if !is3d {
                    if let Some(&v) = c.nt.get(cid) {
                        if v != 0 {
                            return v == 2;
                        }
                    }
                }
                let v = if is3d {
                    self.noises[ni].get(c.x as f64, c.y as f64, c.z as f64)
                } else {
                    self.noises[ni].get(c.x as f64, 0.0, c.z as f64)
                };
                let ok = v >= lo && v <= hi;
                if !is3d {
                    if c.nt.len() <= cid {
                        c.nt.resize(self.conds.len(), 0);
                    }
                    c.nt[cid] = if ok { 2 } else { 1 };
                }
                ok
            }
            Cond::VGrad {
                legacy_seed,
                sp,
                t,
                f,
                ..
            } => {
                let top = self.min_y + self.height - 1;
                let ta = t.get(self.min_y, top);
                let fa = f.get(self.min_y, top);
                if c.y <= ta {
                    return true;
                }
                if c.y >= fa {
                    return false;
                }
                let m = (c.y as f32 - ta as f32) / (fa as f32 - ta as f32) * (0.0 - 1.0) + 1.0;
                if self.legacy_random_source {
                    Lcg::new(legacy_seed ^ hash_block_pos(c.x, c.y, c.z) as u64).next_f32() < m
                } else {
                    sp.pos(c.x, c.y, c.z).next_f32() < m
                }
            }
            Cond::YAbove { a, mult, sd } => {
                let top = self.min_y + self.height - 1;
                c.y + if sd { c.sda } else { 0 } >= a.get(self.min_y, top) + c.run_depth * mult
            }
            Cond::Water { off, mult, sd } => {
                c.fluid == i32::MIN
                    || c.y + if sd { c.sda } else { 0 } >= c.fluid + off + c.run_depth * mult
            }
            Cond::Steep => {
                if let Some(s) = c.steep {
                    return s;
                }
                let lx = c.x & 15;
                let lz = c.z & 15;
                let t = |lx: i32, lz: i32| -> i32 {
                    heightmap[(lz.clamp(0, 15) as usize) * 16 + (lx.clamp(0, 15) as usize)] as i32
                };
                // pumpkin steep: north-facing (z+1 >= z-1 + 4) or west-facing (x-1 >= x+1 + 4)
                let s = t(lx, lz + 1) >= t(lx, lz - 1) + 4 || t(lx - 1, lz) >= t(lx + 1, lz) + 4;
                c.steep = Some(s);
                s
            }
            Cond::Not(i) => !self.test_cond(i, c, heightmap),
            Cond::Hole => c.run_depth <= 0,
            Cond::Prelim => c.y >= c.est_min,
            Cond::StoneD {
                off,
                sdep,
                sec,
                ceil,
            } => {
                let sd = if ceil { c.sdb } else { c.sda };
                let depth = if sdep { c.run_depth } else { 0 };
                let range = if sec == 0 {
                    0
                } else {
                    lmap(self.sec_depth(c) as f64, -1.0, 1.0, 0.0, sec as f64) as i32
                };
                sd <= 1 + off + depth + range
            }
        }
    }

    fn eval_rule(&mut self, rid: usize, c: &mut SC, heightmap: &[i16; 256]) -> Option<u32> {
        match &self.rules[rid] {
            Rule::Block(st) => Some(*st),
            Rule::Bandlands => {
                let bands = self.bands.as_ref()?;
                let ni = self.band_off_ni?;
                let off = (self.noises[ni].get(c.x as f64, 0.0, c.z as f64) * 4.0).round() as i32;
                Some(bands[((c.y + off).rem_euclid(192)) as usize])
            }
            Rule::Seq(v) => {
                let len = v.len();
                for i in 0..len {
                    // Release the rules borrow before recursive evaluation.
                    let Rule::Seq(seq) = &self.rules[rid] else {
                        unreachable!();
                    };
                    let sub = seq[i];
                    if let Some(st) = self.eval_rule(sub, c, heightmap) {
                        return Some(st);
                    }
                }
                None
            }
            Rule::Cond(ci, ri) => {
                let (ci, ri) = (*ci, *ri);
                if self.test_cond(ci, c, heightmap) {
                    self.eval_rule(ri, c, heightmap)
                } else {
                    None
                }
            }
            Rule::OreVein {
                density,
                rich,
                gap,
                ore,
                raw,
                fill,
                rawc,
                ymin,
                ymax,
            } => {
                let (density, rich, gap, ore, raw, fill, rawc) =
                    (*density, *rich, *gap, *ore, *raw, *fill, *rawc);
                let (ymin, ymax) = (*ymin, *ymax);
                if c.y < ymin || c.y >= ymax {
                    return None;
                }
                let d = self.eval(density, c.x, c.y, c.z, &mut c.memo);
                if d <= 0.0 {
                    return None;
                }
                let sp = self.ore_split.as_ref()?;
                let mut r = sp.pos(c.x, c.y, c.z);
                // `OreVeinSampler` (dzi in 1.21.1) keeps a cell with probability
                // VEIN_SOLIDNESS = 0.7; the density value is *not* the probability
                // (it decides the edge roundoff and the richness band instead).
                if r.next_f32() > 0.7 {
                    return None;
                }
                let richness = self.eval(rich, c.x, c.y, c.z, &mut c.memo) as f32;
                let gapv = self.eval(gap, c.x, c.y, c.z, &mut c.memo);
                if r.next_f32() < richness && gapv > -0.3 {
                    Some(if r.next_f32() < rawc { raw } else { ore })
                } else {
                    Some(fill)
                }
            }
        }
    }
}

// ---------- chunk column generation ----------

pub(crate) struct ChunkCols {
    // Preset-sized horizontal/vertical corner grid per `interpolated` node reachable from
    // final_density, indexed by density-function id. Vanilla interpolates only
    // the input of `interpolated`; squeeze/min/add stay per block.
    grids: Vec<Option<Box<[f64]>>>,
    surf: [i16; 256],
    ny: usize, // vertical corner count, including both edges
    cell_w: i32,
    cell_h: i32,
    min_y: i32,
    /// Per-column material-rule results (surface window). Layer A2-b builds these on the
    /// first material read, not in `build_cols_body`: a neighbour column used only as a
    /// heightmap source pays none of the 256-column surface-rule scan. `FromIterator` is
    /// implemented so synthetic `ChunkCols` literals in tests keep their original shape.
    mats: SurfMatsCell,
    /// Per-column run_depth (surface noise). Kept as a field for the same synthetic-column
    /// literals; production reads the lazy value through [`ChunkCols::surface_mats`].
    #[allow(dead_code)]
    rd: [i16; 256],
    wtop: [i16; 256],           // per-column top of solid-or-fluid (heightmap basis)
    /// The beardifier input of the chunk that owns these columns, resolved by
    /// `Gen::resolve_beard` before `surf`, `wtop` and `mats` are built (vanilla's
    /// `NoiseChunk.fillFromNoise` applies `Beardifier` before
    /// `SurfaceSystem.buildSurface`). `None` during nested structure-heightmap
    /// queries (`beard_depth > 0`) and for chunks with no structures.
    beard: Option<std::sync::Arc<BeardChunkData>>,
    /// The `aq_sched` flag this chunk's build left, when the build asked the aquifer at
    /// all — the state vanilla's per-chunk `Aquifer` is in when `applyCarvers` starts on
    /// the same `NoiseChunk` that `fillFromNoise` filled. Columns served from a share did
    /// not run the build on the asking generator, so a carve restores this instead of
    /// reading whatever its own last aquifer call left. `None` for base columns and
    /// synthetic test columns (no fill ran) and for a build that made no aquifer call:
    /// then the flag the build left is the caller's own history, not the chunk's.
    pub(crate) carve_sched: Option<bool>,
}

/// The lazily-built half of a [`ChunkCols`]: surface material windows and the per-column
/// `run_depth` the deep rules read. Split out so a neighbour column that is only used as a
/// heightmap source pays none of this cost.
pub(crate) struct SurfaceMats {
    mats: Vec<Option<SurfMat>>,
    rd: [i16; 256],
}

/// Interior-mutable container for the lazy [`SurfaceMats`]. A finished column carries
/// `grids`/`surf`/`wtop`/`beard` already, so the reading generator can reproduce the exact
/// density/aquifer/surface-rule answers the old eager in-pipeline scan produced. `OnceLock`
/// is `Send + Sync`, so a `ChunkCols` shared through `TerrainShare::cols` initializes its
/// materials exactly once across threads.
struct SurfMatsCell {
    cell: std::sync::OnceLock<SurfaceMats>,
    base_surf: [std::sync::atomic::AtomicI16; 256],
    base_wtop: [std::sync::atomic::AtomicI16; 256],
    cx: i32,
    cz: i32,
}

impl SurfMatsCell {
    fn new(cx: i32, cz: i32) -> Self {
        Self {
            cell: std::sync::OnceLock::new(),
            base_surf: std::array::from_fn(|_| std::sync::atomic::AtomicI16::new(i16::MIN)),
            base_wtop: std::array::from_fn(|_| std::sync::atomic::AtomicI16::new(i16::MIN)),
            cx,
            cz,
        }
    }

    /// Compute with the *reading* generator, not a fresh scratch: the aquifer and the
    /// preliminary-surface memos it warmed while scanning `surf`/`wtop` are the ones the
    /// eager in-pipeline scan used, so the deferred scan costs the same as it always did
    /// and produces the same bytes.
    fn get_or_compute(&self, g: &mut Gen, cc: &ChunkCols) -> &SurfaceMats {
        self.cell
            .get_or_init(|| g.build_surface_mats(self.cx, self.cz, cc))
    }
}

impl FromIterator<Option<SurfMat>> for SurfMatsCell {
    fn from_iter<T: IntoIterator<Item = Option<SurfMat>>>(_iter: T) -> Self {
        // A synthetic column built by a test has no owning chunk; its (unused) material
        // answer is the empty eager default, computed with the test's own generator.
        Self::new(0, 0)
    }
}

pub(crate) type GeneratedTick = ((i32, i32, i32), u32);
type PendingGeneratedTicks = (Vec<GeneratedTick>, HashSet<GeneratedTick>);

/// Per-world pending ticks. Sharing these handles with runtime avoids taking the generator
/// lock to drain, without mixing independent generators or reinitialized worlds.
#[derive(Clone, Default)]
struct GeneratedTickQueues {
    leaves: std::sync::Arc<Mutex<PendingGeneratedTicks>>,
    fluids: std::sync::Arc<Mutex<PendingGeneratedTicks>>,
}

static GEN_TICK_QUEUES: Mutex<Option<GeneratedTickQueues>> = Mutex::new(None);

fn drain_generated_ticks(queue: &Mutex<PendingGeneratedTicks>) -> Vec<GeneratedTick> {
    let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
    q.1.clear();
    std::mem::take(&mut q.0)
}

/// Drain the active world's leaf queue without waiting for the worldgen lock.
pub fn take_generated_leaf_ticks() -> Vec<GeneratedTick> {
    let queues = GEN_TICK_QUEUES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    queues.map_or_else(Vec::new, |q| drain_generated_ticks(&q.leaves))
}

pub fn take_generated_fluid_ticks() -> Vec<GeneratedTick> {
    let queues = GEN_TICK_QUEUES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    queues.map_or_else(Vec::new, |q| drain_generated_ticks(&q.fluids))
}

/// The per-dimension generator registry: one `Gen` per dimension, every one built from the
/// same world seed and its own preset's noise settings.
///
/// Before this the process had exactly one generator (`GEN`), and "which dimension" was a
/// property of `init`'s last call — which is why the Nether and the End could only ever be an
/// offline diagnostic (`MEGU_WG_PRESET`). The Overworld keeps its old meaning exactly:
/// the dim-less accessors ([`with_gen`], [`terrain_section`], [`facts`], [`data_snapshot`])
/// answer for the [`active_dim`] slot, and `init` always makes that the Overworld.
///
/// The vectors index by [`DimId::index`]: slots 0..2 are the pinned vanilla dimensions
/// and custom dimensions extend them in registration order. A vector reads as "no
/// generator" for a slot that has not been installed, so a `Flat`/`Void` custom
/// dimension — which has no `Gen` — just never fills its slot.
pub(crate) struct PerDimGen {
    /// The generators, indexed by [`DimId::index`]. One lock for all of them, because a
    /// `Gen` is not `Send`-shareable work in progress: generation holds its dimension's
    /// generator exclusively, exactly as it held `GEN`.
    gens: Mutex<Vec<Option<Gen>>>,
    /// The immutable half of each generator, published at init. A worker clones the `Arc`
    /// once and then reads a dimension's inputs without touching the generator lock.
    data: Mutex<Vec<Option<std::sync::Arc<GenData>>>>,
    /// The dimension facts of each generator, published at init: the lock-free answer a
    /// reader wants for the build height, the sea level or the world window.
    facts: Mutex<Vec<Option<GenFacts>>>,
}

static PER_DIM: PerDimGen = PerDimGen {
    gens: Mutex::new(Vec::new()),
    data: Mutex::new(Vec::new()),
    facts: Mutex::new(Vec::new()),
};

/// The seed `init` installed, kept so a custom `Noise` dimension can lazily build its
/// generator on first use.
static GEN_SEED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The block-state lookup `init` installed, kept for the same lazy install.
type BlockLookupFn = fn(&str) -> Option<u32>;
static BLOCK_LOOKUP: Mutex<Option<BlockLookupFn>> = Mutex::new(None);

/// Which slot the dim-less accessors answer for.
///
/// `init_preset` publishes here the generator it installed, so every offline diagnostic that
/// selects a dimension with `MEGU_WG_PRESET` keeps meaning what it meant (its
/// `with_gen`, its `world_min_y()`, its dump length). `init` — the runtime's only call —
/// always installs the Overworld, so a server's dim-less path is the Overworld.
static ACTIVE_DIM: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(DimId::Overworld.raw() as u8);

/// The dimension the dim-less accessors answer for (the Overworld unless an offline
/// diagnostic selected another one). The streaming path never asks this: it names its
/// dimension in [`with_gen_dim`] / [`terrain_section_dim`].
pub(crate) fn active_dim() -> DimId {
    DimId::from_index(ACTIVE_DIM.load(std::sync::atomic::Ordering::Relaxed) as usize)
        .unwrap_or(DimId::Overworld)
}

/// Bumped by every `init`. The caches above the generator (`main.rs`'s state band and
/// section store) describe *this* generator, so they must not answer for a replacement:
/// a band left over from a previous `init` fed the anti-xray exposure test states from
/// another world, which changed one chunk's delivered bytes between two sweeps in the same
/// process (seed 60007, chunk 2,-1 — found by the digest guard).
static GEN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn generation_seq() -> u64 {
    GEN_SEQ.load(std::sync::atomic::Ordering::Relaxed)
}

/// Serializes tests that mutate the process-global generator, including ones in other
/// test modules (`worldgen::wg_tests`' own `lock_gen` still uses it too).
#[cfg(test)]
static GEN_TEST_LOCK: Mutex<()> = Mutex::new(());

/// A test-only rendezvous the terrain workers pass through, so a test can *prove* two
/// chunk fills overlapped instead of inferring it from elapsed time.
///
/// `prefetch_terrain` calls [`parallel_probe::arrive`] at the start of every chunk claim.
/// With `expect = 2`, the first worker to arrive blocks until a second one is inside the
/// same window; if the dispatch were serial the wait times out and the probe records that
/// failure rather than hanging the suite.
#[cfg(test)]
pub(crate) mod parallel_probe {
    use std::sync::{Condvar, Mutex};

    struct Rendezvous {
        expect: usize,
        arrived: usize,
        timed_out: bool,
    }

    static RENDEZVOUS: Mutex<Option<Rendezvous>> = Mutex::new(None);
    static CV: Condvar = Condvar::new();
    /// How long the first arrival waits for the second before recording a timeout.
    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn arm(expect: usize) {
        *RENDEZVOUS.lock().unwrap_or_else(|e| e.into_inner()) = Some(Rendezvous {
            expect,
            arrived: 0,
            timed_out: false,
        });
    }

    /// Called by a worker at the start of a chunk fill. Blocks until `expect` workers are
    /// inside this window; a timeout records the failure instead of hanging.
    pub(crate) fn arrive() {
        let mut guard = RENDEZVOUS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = guard.as_mut() else { return };
        r.arrived += 1;
        CV.notify_all();
        if r.arrived >= r.expect {
            return;
        }
        let (mut guard, timed_out) = CV.wait_timeout(guard, WAIT).unwrap();
        if timed_out.timed_out() {
            if let Some(r) = guard.as_mut() {
                r.timed_out = true;
            }
            CV.notify_all();
        }
    }

    /// Disarm and report `(arrived, timed_out)`.
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn disarm() -> (usize, bool) {
        let mut guard = RENDEZVOUS.lock().unwrap_or_else(|e| e.into_inner());
        let out = guard
            .as_ref()
            .map(|r| (r.arrived, r.timed_out))
            .unwrap_or((0, false));
        *guard = None;
        CV.notify_all();
        out
    }
}

/// The generator lock, timing how long a caller waited for it and how long it held it.
///
/// The parallelization decision needs this split: `FEAT_PROFILE` can say how much
/// work a populate does, but not how much of a server's wall time is spent queued
/// behind the one world lock. Off unless `FEAT_PROFILE` is set, in which case the
/// two `Instant` calls are paid on every acquisition.
///
/// The guard is over the registry's whole slot array, and the derefs hand out the one slot
/// the lock was taken for — so `gen_lock().as_mut()` still means "the Overworld generator",
/// unchanged, and `gen_lock_dim(dim)` means the same thing for another dimension.
pub(crate) struct GenLock {
    guard: std::sync::MutexGuard<'static, Vec<Option<Gen>>>,
    dim: DimId,
    /// `Some` when profiling: the time spent inside `PER_DIM.gens.lock()`.
    waited_ns: Option<u64>,
    held_since: Option<std::time::Instant>,
}
static GEN_WAIT_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static GEN_HOLD_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static GEN_LOCK_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Drop for GenLock {
    fn drop(&mut self) {
        let (Some(waited), Some(since)) = (self.waited_ns, self.held_since) else {
            return;
        };
        use std::sync::atomic::Ordering;
        GEN_HOLD_NS.fetch_add(since.elapsed().as_nanos() as u64, Ordering::Relaxed);
        GEN_WAIT_NS.fetch_add(waited, Ordering::Relaxed);
        GEN_LOCK_N.fetch_add(1, Ordering::Relaxed);
    }
}

impl std::ops::Deref for GenLock {
    type Target = Option<Gen>;
    fn deref(&self) -> &Self::Target {
        &self.guard[self.dim.index()]
    }
}
impl std::ops::DerefMut for GenLock {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard[self.dim.index()]
    }
}

/// The part of `Gen` that never changes after `init`: the dimension facts every reader
/// needs (`sea`, build height, the default block states).
///
/// Published separately from the generator so a reader does not queue behind a 13 ms generation
/// hold to learn the build height. It is the first piece of the `GenData`/`Gen` split in
/// `specs/chunk-parallelism`: immutable inputs readable without the world lock. Anything
/// added here must be immutable after construction — the snapshot is taken once, and a
/// later writer would silently diverge from the generator.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GenFacts {
    #[allow(dead_code)]
    pub preset: NoisePreset,
    pub sea: i32,
    /// generator window (`noise_settings`)
    pub min_y: i32,
    pub height: i32,
    /// world window (`dimension_type`); see `GenData::world_min_y`
    pub world_min_y: i32,
    pub world_height: i32,
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub def_st: u32,
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub def_fl: u32,
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub bedrock_st: u32,
}

/// `Arc<GenData>` of the active dimension, without waiting for a generation step to finish.
/// A job can keep the snapshot for its lifetime; the caches it must not share live on `Gen`,
/// not here. The snapshot itself is [`PerDimGen::data`]: a worker takes a clone of the `Arc`
/// once and then reads that dimension's inputs without the generator lock, which is held for a
/// clone and not for a generation step.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub fn data_snapshot() -> Option<std::sync::Arc<GenData>> {
    data_snapshot_dim(active_dim())
}

/// One dimension's immutable half, published at init.
pub(crate) fn data_snapshot_dim(dimension: DimId) -> Option<std::sync::Arc<GenData>> {
    PER_DIM
        .data
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(dimension.index())
        .cloned()
        .flatten()
}

/// The active dimension's immutable facts, without the generator lock. `None` before `init`.
pub(crate) fn facts() -> Option<GenFacts> {
    facts_dim(active_dim())
}

/// One dimension's immutable facts, without the generator lock. `None` until that dimension
/// is installed.
pub(crate) fn facts_dim(dimension: DimId) -> Option<GenFacts> {
    PER_DIM
        .facts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(dimension.index())
        .copied()
        .flatten()
}

/// `DimensionType.hasSkyLight()` for one dimension, answered from the published facts (no
/// generator lock); the Overworld's answer before any dimension is installed, which is the
/// frame every pre-registry default came from.
pub(crate) fn has_skylight(dimension: DimId) -> bool {
    // `DimId::has_skylight` is authoritative: the vanilla match answers slots 0..2 and
    // a custom dimension's `dimension_type` answers the rest.
    dimension.has_skylight()
}

fn gen_lock() -> GenLock {
    gen_lock_dim(active_dim())
}

/// One dimension's generator lock.
fn gen_lock_dim(dimension: DimId) -> GenLock {
    // Either switch turns the timing on: `FEAT_PROFILE` for generation-only runs,
    // `MEGU_TICK_PROFILE` for a live server, where the tick thread is the caller
    // that queues behind a section pass.
    let prof = env_flag!("FEAT_PROFILE") || env_flag!("MEGU_TICK_PROFILE");
    let started = prof.then(std::time::Instant::now);
    let mut guard = PER_DIM.gens.lock().unwrap_or_else(|e| e.into_inner());
    // The caller's slot always exists: a custom dimension registers before it can be
    // named, and this grow is what turns "slot beyond the vanilla three" from a panic
    // into a `None` generator.
    let need = dimension.index() + 1;
    if guard.len() < need {
        guard.resize_with(need, || None);
    }
    GenLock {
        guard,
        dim: dimension,
        waited_ns: started.map(|t| t.elapsed().as_nanos() as u64),
        held_since: started,
    }
}

/// `GENLOCK wait=<ms> hold=<ms> n=<acquisitions>` under `FEAT_PROFILE`.
///
/// `wait / (wait + hold)` is the share of lock-attributed time spent queued; a
/// worker-per-chunk design removes the queue, not the hold.
pub fn report_gen_lock() {
    if !env_flag!("FEAT_PROFILE") && !env_flag!("MEGU_TICK_PROFILE") {
        return;
    }
    use std::sync::atomic::Ordering;
    let wait = GEN_WAIT_NS.swap(0, Ordering::Relaxed) as f64 / 1e6;
    let hold = GEN_HOLD_NS.swap(0, Ordering::Relaxed) as f64 / 1e6;
    let n = GEN_LOCK_N.swap(0, Ordering::Relaxed);
    eprintln!(
        "GENLOCK wait={wait:.1}ms hold={hold:.1}ms n={n} threads={}",
        gen_threads_probe()
    );
}

fn gen_threads_probe() -> usize {
    std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
}

/// The one instruction for a tree that has no game data committed next to it. Both the
/// startup fallbacks and the tests that read the data print it, so a missing file names the
/// command that writes it instead of the `Option` that came back empty.
pub(crate) const VANILLA_DATA_MISSING: &str =
    "Vanilla data missing. Regenerate it from your own 1.21.1 jar: tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/server.jar --fetch-fabric-mappings --accept-eula";

pub fn init(seed: u64, res: fn(&str) -> Option<u32>) -> bool {
    init_preset(seed, res, NoisePreset::Overworld)
}

/// Install one dimension's generator for `seed`, leaving the active dimension alone.
///
/// This is what a server calls for the Nether and the End after [`init`]: the same world
/// seed, the same block-state lookup, and the generator that dimension's terrain, surfaces
/// and world window are defined against. It publishes that dimension's facts and data and
/// nothing else — in particular it does not touch `GEN_SEQ` or the generated-tick handles,
/// which describe the active world.
pub(crate) fn init_dim(dimension: DimId, seed: u64, res: fn(&str) -> Option<u32>) -> bool {
    install(dimension, seed, res, false)
}

/// Install `preset`'s generator and make its dimension the active one.
///
/// The runtime reaches this through [`init`], i.e. with the Overworld. The offline
/// diagnostics (`MEGU_WG_PRESET=nether|end`) reach it with another preset, and because
/// the installed generator also becomes the active one, every dim-less accessor keeps
/// answering for it exactly as it did before the registry existed.
pub(crate) fn init_preset(seed: u64, res: fn(&str) -> Option<u32>, preset: NoisePreset) -> bool {
    let dimension: DimId = preset.into();
    install(dimension, seed, res, true)
}

fn install(dimension: DimId, seed: u64, res: fn(&str) -> Option<u32>, activate: bool) -> bool {
    let preset = dimension.preset();
    let generator = match preset {
        NoisePreset::Overworld => Gen::load(seed, res),
        NoisePreset::End | NoisePreset::Nether => Gen::load_preset(seed, res, preset),
    };
    let Some(mut g) = generator else { return false };
    // Published before the generator, in the order a reader needs them: once a reader can see
    // this dimension as active, its facts, its data snapshot and its generator are all there.
    GEN_SEED.store(seed, std::sync::atomic::Ordering::Relaxed);
    *BLOCK_LOOKUP.lock().unwrap_or_else(|e| e.into_inner()) = Some(res);
    // A custom `Noise` dimension keeps its preset's generator window but its own
    // declared world window — the same split the Nether's 0..255 world over a 0..127
    // generator already exercises. `block_state_at` answers air outside the generator
    // window and `feat_write` stays inside the world window, both unchanged.
    if let Some(spec) = dimension_spec(dimension)
        .filter(|spec| matches!(spec.generator, CustomGeneratorKind::Noise { .. }))
    {
        g.world_min_y = spec.min_y;
        g.world_height = spec.height;
    }
    let (world_min_y, world_height) = (g.world_min_y, g.world_height);
    let slot = dimension.index();
    {
        let mut facts = PER_DIM.facts.lock().unwrap_or_else(|e| e.into_inner());
        if facts.len() <= slot {
            facts.resize(slot + 1, None);
        }
        facts[slot] = Some(GenFacts {
            preset: g.preset,
            sea: g.sea,
            min_y: g.min_y,
            height: g.height,
            world_min_y,
            world_height,
            #[cfg(test)]
            def_st: g.def_st,
            #[cfg(test)]
            def_fl: g.def_fl,
            #[cfg(test)]
            bedrock_st: g.bedrock_st,
        });
    }
    {
        let mut data = PER_DIM.data.lock().unwrap_or_else(|e| e.into_inner());
        if data.len() <= slot {
            data.resize(slot + 1, None);
        }
        data[slot] = Some(g.data.clone());
    }
    let ticks = g.generated_ticks.clone();
    *gen_lock_dim(dimension) = Some(g);
    if activate {
        ACTIVE_DIM.store(dimension.index() as u8, std::sync::atomic::Ordering::Relaxed);
        // The runtime drains the *active* world's generated ticks through these handles, and
        // the tick thread must not queue behind generation to get them.
        *GEN_TICK_QUEUES.lock().unwrap_or_else(|e| e.into_inner()) = Some(ticks);
        GEN_READY.store(true, std::sync::atomic::Ordering::Relaxed);
        GEN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    true
}
/// `MEGU_DFDUMP=x,y,z` diagnostic: print the vein-sampler inputs.
pub fn dfdump(spec: &str) {
    let p: Vec<i32> = spec.split(',').filter_map(|n| n.parse().ok()).collect();
    if p.len() != 3 {
        return;
    }
    if let Some(g) = gen_lock().as_mut() {
        g.dump_dfs(p[0], p[1], p[2]);
    }
}
/// Is a generator loaded?
///
/// An atomic rather than `gen_lock().is_some()`: this is asked once per cell read on the
/// light and block-read paths, and taking the worldgen lock there made every read queue
/// behind whatever generation is in flight. `init_preset` is the only writer, and the
/// generator is never dropped once loaded (tests that re-init only replace it).
static GEN_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn enabled() -> bool {
    GEN_READY.load(std::sync::atomic::Ordering::Relaxed)
}

/// `ChunkLevel.byStatus(FullChunkStatus.ENTITY_TICKING)`: a `forceload` request enters the
/// scheduler as a ticket at level 31 (`ChunkMap.c`), not as a position in a list.
pub(crate) const POP_REQUEST_LEVEL: i32 = 31;
/// The status whose task decorates a chunk (`ChunkStatus.FEATURES`) answers to level 34 in this
/// build (`ChunkLevel.getStatusForLevel`), so a request set is mirrored by three rings of
/// *demanders* and the decorated set is the request set plus three rings (`FEATURES_RING`).
pub(crate) const POP_DECORATION_LEVEL: i32 = 34;
/// `ChunkStep.blockStateWriteRadius` for the `FEATURES` step: one chunk, so a decoration walks
/// its own 3x3 and everything it writes outside that square is dropped (`arb.f_`).
pub(crate) const POP_WRITE_RADIUS: i32 = 1;

/// One pop of the population priority discipline: the demander that reached the head of the
/// queue, the level it was queued at, and the chunks its `FEATURES` step decorated, in the order
/// the step walked them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PopBatch {
    pub level: i32,
    pub demander: (i32, i32),
    pub decorated: Vec<(i32, i32)>,
}

/// The request set in the box shape `MEGU_POP_REGION` already uses, walked x-then-z.
///
/// This walk is the *declared* half of the default order and the reason the whole default is
/// named: vanilla fixes the priority discipline and leaves the within-level sequence to the wall
/// clock (two identical-setting native runs agree on 9 of 49 chunks), so any single default is one
/// member of that family. Here the member is: the first demander is the box's min corner, and a
/// demander's window is walked x-outer / z-inner.
pub(crate) fn request_box(x0: i32, z0: i32, x1: i32, z1: i32) -> Vec<(i32, i32)> {
    let (x0, x1) = (x0.min(x1), x0.max(x1));
    let (z0, z1) = (z0.min(z1), z0.max(z1));
    (x0..=x1)
        .flat_map(|x| (z0..=z1).map(move |z| (x, z)))
        .collect()
}

/// The default population order: **the serialized demand scan**, a named member of vanilla's
/// order family.
///
/// Vanilla's population order is a priority discipline, not a sequence:
///
/// * a request is a ticket at level 31 (`POP_REQUEST_LEVEL`), and `DistanceManager` propagates it
///   at +1 per Chebyshev step, so ring *d* sits at level 31+d;
/// * `ChunkTaskPriorityQueue` holds one insertion-ordered FIFO bucket per level and pops the
///   lowest non-empty level (`aqd`), with the task's priority being the demander's queue level;
/// * a popped task walks statuses upward and, at `FEATURES`, walks its `2r+1` window x-outer /
///   z-inner, running each demanded chunk's step inline (`apy`, `dwc`);
/// * `FEATURES` answers to level 34, so demanders exist at levels 31..=33 and the decorated set
///   is the request set plus three rings (measured on seven native captures: 1 chunk -> 7x7,
///   3x3 -> 9x9).
///
/// The rule is fixed; the within-level order is wall clock, and identical-setting native runs
/// differ (9/49 common prefix). This function therefore implements *one named member* and does not
/// claim bit-parity with any capture — for a comparison against a recorded order the order is an
/// input (`MEGU_POP_ORDER`), which is what the finite acceptance set compares with.
///
/// The member, exactly:
///
/// 1. request chunks enter level 31's bucket in `request_box` order;
/// 2. pop the head of the lowest non-empty level;
/// 3. walk that demander's 3x3 window x-outer / z-inner, skipping chunks already decorated, and
///    append every newly decorated chunk that is itself a demander (level < 34) to the tail of
///    its own level's bucket — that inline demand is what makes the scan serial;
/// 4. repeat until every bucket is empty.
///
/// Steps 2-3 give the two measured shape properties for free: the first batch is the x-then-z 3x3
/// at the request box's min corner, and batches pop in non-decreasing level (the decorated list is
/// not level-monotone — a batch's window reaches one ring further — exactly as measured).
pub(crate) fn population_discipline(requests: &[(i32, i32)]) -> Vec<PopBatch> {
    if requests.is_empty() {
        return Vec::new();
    }
    let level_of = |c: (i32, i32)| {
        let ring = requests
            .iter()
            .map(|r| (c.0 - r.0).abs().max((c.1 - r.1).abs()))
            .min()
            .unwrap_or(0);
        POP_REQUEST_LEVEL + ring
    };
    // One FIFO bucket per level, and the pop order is by level: a `BTreeMap` is the whole queue.
    let mut buckets: std::collections::BTreeMap<i32, std::collections::VecDeque<(i32, i32)>> =
        std::collections::BTreeMap::new();
    let mut queued: HashSet<(i32, i32)> = HashSet::new();
    let mut decorated: HashSet<(i32, i32)> = HashSet::new();
    for &request in requests {
        if queued.insert(request) {
            buckets
                .entry(level_of(request))
                .or_default()
                .push_back(request);
        }
    }
    let mut batches = Vec::new();
    while let Some(level) = buckets
        .iter()
        .find(|(_, bucket)| !bucket.is_empty())
        .map(|(level, _)| *level)
    {
        let demander = buckets
            .get_mut(&level)
            .and_then(|bucket| bucket.pop_front())
            .expect("the level was found non-empty");
        let mut window = Vec::with_capacity(9);
        for dx in -POP_WRITE_RADIUS..=POP_WRITE_RADIUS {
            for dz in -POP_WRITE_RADIUS..=POP_WRITE_RADIUS {
                let chunk = (demander.0 + dx, demander.1 + dz);
                if !decorated.insert(chunk) {
                    continue;
                }
                window.push(chunk);
                let chunk_level = level_of(chunk);
                // A chunk whose own target *is* `FEATURES` has a demand radius of 0 for it, so
                // ring 3 is decorated but never demands: that is where the set stops.
                if chunk_level < POP_DECORATION_LEVEL && queued.insert(chunk) {
                    buckets.entry(chunk_level).or_default().push_back(chunk);
                }
            }
        }
        batches.push(PopBatch {
            level,
            demander,
            decorated: window,
        });
    }
    batches
}

/// The decorated order of [`population_discipline`], flattened: the execution order.
pub(crate) fn population_discipline_order(requests: &[(i32, i32)]) -> Vec<(i32, i32)> {
    population_discipline(requests)
        .into_iter()
        .flat_map(|batch| batch.decorated)
        .collect()
}

/// A recorded FEATURES plan: the order its sources ran in, and the window of chunks the
/// fixture covers.
///
/// The window is `parse_population_order`'s required box, the dump's `(n+2)²` halo. Native's
/// decorated set is three rings around the request and its `FULL` (promotion) region is two,
/// so the recorded plan is a *narrowed input*: it names the passes that matter for the dump,
/// not every pass native ran. [`Gen::promotion_window_ready`] reads the window for exactly
/// that reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PopulationPlan {
    pub(crate) order: Vec<(i32, i32)>,
    /// Inclusive `(x0, z0, x1, z1)` of the chunks the plan is required to cover.
    pub(crate) window: (i32, i32, i32, i32),
}

/// Validate an explicit FEATURES plan covering a dump's one-chunk decoration halo.
/// Recorded plans may include additional observed sources; exact plans may not.
/// Neither mode silently drops or reorders entries.
pub(crate) fn parse_population_order(
    text: &str,
    ox: i32,
    oz: i32,
    n: i32,
    allow_extra: bool,
) -> Result<PopulationPlan, String> {
    if n <= 0 {
        return Err("dump size must be positive".into());
    }
    let width = n.checked_add(2).ok_or("population footprint overflow")? as usize;
    let expected = width
        .checked_mul(width)
        .ok_or("population footprint overflow")?;
    let x0 = ox.checked_sub(1).ok_or("population coordinate overflow")?;
    let z0 = oz.checked_sub(1).ok_or("population coordinate overflow")?;
    let x1 = ox.checked_add(n).ok_or("population coordinate overflow")?;
    let z1 = oz.checked_add(n).ok_or("population coordinate overflow")?;
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    for (i, line) in text.lines().enumerate() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 2 {
            return Err(format!("population line {} needs x z: {line:?}", i + 1));
        }
        let x = fields[0]
            .parse::<i32>()
            .map_err(|_| format!("invalid population x on line {}: {line:?}", i + 1))?;
        let z = fields[1]
            .parse::<i32>()
            .map_err(|_| format!("invalid population z on line {}: {line:?}", i + 1))?;
        if !allow_extra && (x < x0 || x > x1 || z < z0 || z > z1) {
            return Err(format!(
                "population chunk ({x},{z}) is outside ({x0},{z0})..({x1},{z1})"
            ));
        }
        if !seen.insert((x, z)) {
            return Err(format!(
                "duplicate population chunk ({x},{z}) on line {}",
                i + 1
            ));
        }
        order.push((x, z));
    }
    let covered = seen
        .iter()
        .filter(|&&(x, z)| x0 <= x && x <= x1 && z0 <= z && z <= z1)
        .count();
    if covered != expected {
        return Err(format!("incomplete population footprint: {covered} of {expected} source chunks in ({x0},{z0})..({x1},{z1})"));
    }
    Ok(PopulationPlan {
        order,
        window: (x0, z0, x1, z1),
    })
}

/// Run a diagnostic against one dimension's generator.
///
/// This is the streaming seam: a caller that serves the Nether asks for the Nether's
/// generator instead of whatever `init` installed last. A dimension with no generator
/// installed answers `None`, exactly as an uninitialised world answered before.
pub fn with_gen_dim<R>(dimension: DimId, f: impl FnOnce(&mut Gen) -> R) -> Option<R> {
    ensure_dim_generator(dimension);
    gen_lock_dim(dimension).as_mut().map(f)
}

/// Install a custom `Noise` dimension's generator on first use, with the seed and
/// block lookup `init` published. `Flat`/`Void` dimensions and vanilla slots answer
/// early — their `None`/`Some` slots need nothing installed.
fn ensure_dim_generator(dimension: DimId) {
    let spec = match dimension_spec(dimension) {
        Some(spec) => spec,
        None => return,
    };
    if !matches!(spec.generator, CustomGeneratorKind::Noise { .. }) {
        return;
    }
    let slot_ready = PER_DIM
        .gens
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(dimension.index())
        .is_some_and(|slot| slot.is_some());
    if slot_ready {
        return;
    }
    let Some(res) = *BLOCK_LOOKUP.lock().unwrap_or_else(|e| e.into_inner()) else {
        return;
    };
    install(dimension, GEN_SEED.load(std::sync::atomic::Ordering::Relaxed), res, false);
}

/// Run a diagnostic against the active dimension's generator — the Overworld for every
/// runtime caller (`init` is the only install the server does); an offline
/// `MEGU_WG_PRESET` diagnostic is what can make it another dimension. Feature-facing
/// accessors stay in the caller's layer to avoid worldgen depending on generation_context.
pub fn with_gen<R>(f: impl FnOnce(&mut Gen) -> R) -> Option<R> {
    with_gen_dim(active_dim(), f)
}

/// A stable fingerprint of one chunk's generated columns, for determinism tests.
/// Every field is part of the terrain answer, including the interpolated grids.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn cols_fingerprint(g: &mut Gen, cc: &ChunkCols) -> u64 {
    fn mix(h: &mut u64, v: u64) {
        *h ^= v;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &s in &cc.surf {
        mix(&mut h, s as u64);
    }
    for &s in &cc.wtop {
        mix(&mut h, s as u64);
    }
    let sm = cc.surface_mats(g);
    for &s in &sm.rd {
        mix(&mut h, s as u64);
    }
    for m in &sm.mats {
        match m {
            None => mix(&mut h, u64::MAX),
            Some(m) => {
                mix(&mut h, m.lo as u64);
                for v in &m.vals {
                    mix(&mut h, *v as u64);
                }
            }
        }
    }
    for grid in &cc.grids {
        match grid {
            None => mix(&mut h, u64::MAX - 1),
            Some(cells) => {
                for v in cells.iter() {
                    mix(&mut h, v.to_bits());
                }
            }
        }
    }
    mix(&mut h, cc.ny as u64);
    mix(&mut h, cc.cell_w as u64);
    mix(&mut h, cc.cell_h as u64);
    mix(&mut h, cc.min_y as u64);
    h
}

/// Generate one chunk's columns on a scratch generator, without the world lock.
///
/// Stage 2A of `specs/chunk-parallelism`: terrain is a pure function of the immutable
/// data and the seed — `cols` reads no feature write — so a worker can compute it while
/// the owner keeps the generator. Features stay sequential: they read other chunks' writes, and
/// their order is pinned to the native capture.
pub fn chunk_cols_scratch(
    data: &std::sync::Arc<GenData>,
    cx: i32,
    cz: i32,
) -> Option<std::sync::Arc<ChunkCols>> {
    Gen::fresh_scratch(data.clone()).cols(cx, cz)
}

#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
type PrefetchedCols = Vec<((i32, i32), Option<std::sync::Arc<ChunkCols>>)>;

/// The resident background task pool every scoped worldgen/feature batch shares
/// under `mod-suite` (§1.1/§1.2 of
/// `audit/full-multithreading-server-client-parallel-architecture-20261123.md`).
/// `std::thread::scope` pays an OS `clone`/`join` pair per call — several per
/// generation round — while these workers spawn once at first use and
/// `scope_blocking` only queues jobs, the caller help-stealing instead of
/// sleeping. Lane B (Background) is where the audit routes chunk generation
/// and feature placement.
#[cfg(feature = "mod-suite")]
pub(crate) fn bg_task_pool() -> &'static megu_core::task_pool::MeguTaskPool {
    static POOL: std::sync::OnceLock<megu_core::task_pool::MeguTaskPool> =
        std::sync::OnceLock::new();
    POOL.get_or_init(megu_core::task_pool::MeguTaskPool::for_host)
}

/// Run `f` in a scoped parallel batch: the resident background task pool under
/// `mod-suite`, per-call OS threads otherwise. Both arms hand `f` a scope whose
/// `spawn` accepts `FnOnce() + Send + 'scope`, so call sites read exactly like
/// `std::thread::scope` and spawn identically-shaped jobs on either executor.
macro_rules! bg_scope {
    ($f:expr) => {{
        #[cfg(feature = "mod-suite")]
        let r = $crate::worldgen::bg_task_pool()
            .scope_blocking(megu_core::task_pool::TaskLane::Background, $f);
        #[cfg(not(feature = "mod-suite"))]
        let r = ::std::thread::scope($f);
        r
    }};
}

pub(crate) use bg_scope;

/// Compute `keys` with up to `workers` scratch generators, returned in the caller's order.
/// `workers <= 1` runs inline, which is the reference path; every worker count must give
/// the same `ChunkCols` because `cols` is pure (the determinism test pins that).
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub fn prefetch_cols(
    data: &std::sync::Arc<GenData>,
    keys: &[(i32, i32)],
    workers: usize,
) -> PrefetchedCols {
    keys.iter()
        .copied()
        .zip(prefetch_cols_with(data, None, keys, workers, None))
        .collect()
}

/// [`prefetch_cols`] with the owner's per-generator beardifier resolver, for a world whose
/// columns must resolve structures the way the owner generator would.
pub(crate) fn prefetch_cols_with(
    data: &std::sync::Arc<GenData>,
    resolver: Option<BeardResolver>,
    keys: &[(i32, i32)],
    workers: usize,
    starts_round: Option<u64>,
) -> Vec<Option<std::sync::Arc<ChunkCols>>> {
    let mut out: Vec<Option<std::sync::Arc<ChunkCols>>> = keys.iter().map(|_| None).collect();
    if keys.is_empty() {
        return out;
    }
    if workers <= 1 || keys.len() <= 1 {
        match resolver {
            Some(resolver) => {
                let mut g = Gen::fresh_scratch(data.clone());
                g.starts_round = starts_round;
                g.set_beard_resolver(resolver);
                for (slot, &(cx, cz)) in out.iter_mut().zip(keys) {
                    *slot = g.cols(cx, cz);
                }
            }
            None => {
                for (slot, &(cx, cz)) in out.iter_mut().zip(keys) {
                    *slot = chunk_cols_scratch(data, cx, cz);
                }
            }
        }
        return out;
    }
    let per = keys.len().div_ceil(workers);
    bg_scope!(|s| {
        for (kslice, slice) in keys.chunks(per).zip(out.chunks_mut(per)) {
            s.spawn(move || {
                // M3 P1a M1: a column built here is off-owner work, and at `jobs=1` the same
                // build happens on the caller's thread instead (`workers <= 1` above).
                let _m3w = M3WorkerScope::enter();
                let mut g = Gen::fresh_scratch(data.clone());
                g.starts_round = starts_round;
                if let Some(resolver) = resolver {
                    g.set_beard_resolver(resolver);
                }
                for (slot, &(cx, cz)) in slice.iter_mut().zip(kslice) {
                    *slot = g.cols(cx, cz);
                }
                m3_flush();
            });
        }
    });
    out
}

/// One chunk's base terrain, built by a worker: its columns plus the section rows the
/// consumer will read. `cols` is what the serial path would have had in `col_c`, so the
/// feature pass on the owner does not rebuild it per chunk.
#[derive(Clone)]
pub(crate) struct PrefetchedTerrain {
    pub key: (i32, i32),
    pub cols: std::sync::Arc<ChunkCols>,
    pub rows: Vec<BaseRow>,
}

/// One section of base terrain, plus the positions the same pass files with
/// `ChunkAccess.markPosForPostprocessing`, already in vanilla's filing order for that
/// section (see `Gen::base_section_rows`).
///
/// The marks travel with the rows because the cell decisions are made on the worker: a
/// worker that reported cells without their marks would make the mark set depend on which
/// thread built the section, which is exactly what `MEGU_WG_GEN_JOBS=1` vs `8` has to
/// stay identical on.
#[derive(Clone)]
pub(crate) struct BaseRow {
    pub key: (i32, i32, i32),
    pub arr: std::sync::Arc<[u32; 4096]>,
    pub marks: Vec<(u8, i32, i32, i32)>,
}

/// Which 1.21.1 writer filed a terrain mark (bit 0 is set for the fill's, bit 1 for the
/// surface updater's). One position can carry both bits: the fill's mark is filed first and
/// the surface's state can be written over the same cell, and vanilla keeps both entries in
/// `postProcessing` (duplicates are preserved), so the second pass over the cell re-derives
/// it again. Keeping the bit lets the ordering stay per-writer without deduplicating.
pub(crate) const MARK_FILL: u8 = 1;
pub(crate) const MARK_SURFACE: u8 = 2;

/// The 1.21.1 `ChunkAccess.markPosForPostprocessing` call sites this port files, by owner.
///
/// A `duy.e(Ljd;)V` reference scan of the pinned jar finds nine owners: `arb` (WorldGenRegion),
/// `dkz$b` (MultifaceSpreader), `dzb` (NoiseBasedChunkGenerator's fill), `dzq$1`
/// (SurfaceSystem's column updater), `eaa` (Blender), `ebe` (WorldCarver), `ece`
/// (`Feature.markAboveForPostProcessing`), `ecw` (MultifaceGrowthFeature) and `ejv`
/// (StructurePiece). The enum records sources actually emitted by this implementation;
/// Blender needs blending data this world never writes, and the StructurePiece source
/// is not currently emitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MarkSrc {
    /// `WorldGenRegion.setBlock` -> `state.hasPostProcess`, i.e. the three lit-engine blocks.
    Write,
    /// `NoiseBasedChunkGenerator`'s fill.
    Fill,
    /// `SurfaceSystem`'s `BlockUpdater`.
    Surface,
    /// `WorldCarver.getCarveState` at either of its two sites.
    Carver,
    /// `MultifaceGrowthFeature` / `MultifaceSpreader`: the growth cell each places.
    Multiface,
    /// `Feature.markAboveForPostProcessing` (`DiskFeature`, `LakeFeature`).
    FeatureAbove,
}

impl MarkSrc {
    pub(crate) fn name(self) -> &'static str {
        match self {
            MarkSrc::Write => "write",
            MarkSrc::Fill => "fill",
            MarkSrc::Surface => "surface",
            MarkSrc::Carver => "carver",
            MarkSrc::Multiface => "multiface",
            MarkSrc::FeatureAbove => "above",
        }
    }
}

/// Build the base terrain rows of `keys` on `jobs` scratch generators, one chunk per job
/// claim, without the world lock.
///
/// Lever 2 of `evidence/foundation/performance/README.md`: terrain is a pure function of
/// the immutable data and the seed, so chunks are independent and can be filled while the
/// owner keeps the generator. This is exactly the half vanilla runs on its background executor —
/// `NoiseBasedChunkGenerator.fillFromNoise` is `CompletableFuture.supplyAsync(…, background
/// executor)` per chunk, gated by `ChunkStep`'s neighbour-status requirements
/// (`apis`, `ChunkGenerationTask`, in `dwc.a`/`apy.a`), while `FEATURES` and `CARVERS`
/// complete synchronously on the thread that popped the task (`dwb.h`/`dwb.g` return an
/// already-completed future). The feature layer therefore stays on the serial path: this
/// function produces base terrain only.
///
/// `cols` is the already-built column set per key (from [`prefetch_cols_with`]), so the
/// rows reuse it instead of computing it a second time.
///
/// A worker that panics leaves its keys as `None` (the consumer builds those chunks
/// inline) and gets a fresh generator; it never poisons the owner's.
#[expect(clippy::too_many_arguments)]
pub(crate) fn prefetch_terrain(
    data: &std::sync::Arc<GenData>,
    resolver: Option<BeardResolver>,
    keys: &[(i32, i32)],
    cols: &[Option<std::sync::Arc<ChunkCols>>],
    lo: i32,
    hi: i32,
    jobs: usize,
    starts_round: Option<u64>,
) -> Vec<Option<PrefetchedTerrain>> {
    let mut out: Vec<Option<PrefetchedTerrain>> = keys.iter().map(|_| None).collect();
    if keys.is_empty() {
        return out;
    }
    let started = std::time::Instant::now();
    let jobs = jobs.max(1).min(keys.len());
    // Each worker claims one contiguous slice, so a chunk's columns and its result slot are
    // addressed by the same index and nothing is shared.
    let per = keys.len().div_ceil(jobs).max(1);
    bg_scope!(|s| {
        for (i, oslice) in out.chunks_mut(per).enumerate() {
            let start = i * per;
            let end = (start + per).min(keys.len());
            let kslice = &keys[start..end];
            let cslice = &cols[start..end];
            s.spawn(move || {
                // M3 P1a M1/M4: everything this thread does is off-owner terrain work.
                let _m3w = M3WorkerScope::enter();
                let mut g = Gen::fresh_scratch(data.clone());
                g.starts_round = starts_round;
                if let Some(resolver) = resolver {
                    g.set_beard_resolver(resolver);
                }
                for ((&key, cols), slot) in kslice.iter().zip(cslice).zip(oslice.iter_mut()) {
                    #[cfg(test)]
                    parallel_probe::arrive();
                    let filled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        g.build_terrain_rows(key.0, key.1, lo, hi, cols.clone())
                    }));
                    match filled {
                        Ok(Some((cc, rows))) => {
                            *slot = Some(PrefetchedTerrain {
                                key,
                                cols: cc,
                                rows,
                            })
                        }
                        // No columns for this chunk: the owner answers 0 for it today, so
                        // leave the slot empty and let the serial path do the same.
                        Ok(None) => {}
                        Err(_) => {
                            // The generator may hold a half-updated cache after a panic;
                            // discard it rather than hand it the next chunk.
                            g = Gen::fresh_scratch(data.clone());
                            if let Some(resolver) = resolver {
                                g.set_beard_resolver(resolver);
                            }
                        }
                    }
                }
                // The meter is per thread, so the worker's own counts die with this thread
                // unless they are merged first: `MEGU_M3_PROFILE` would then under-count
                // exactly the work P1a exists to attribute.
                m3_flush();
            });
        }
    });
    // `MEGU_GEN_TRACE=1`: one line per round, so a run can show the parallel path
    // actually executed instead of assuming it did (a bit-identical dump is produced by
    // the serial fallback too).
    if env_flag!("MEGU_GEN_TRACE") {
        let filled = out.iter().filter(|c| c.is_some()).count();
        eprintln!(
            "[terrain] round chunks={} filled={} jobs={} sections={} fill_ms={:.1}",
            keys.len(),
            filled,
            jobs,
            out.iter()
                .filter_map(|c| c.as_ref().map(|c| c.rows.len()))
                .sum::<usize>(),
            started.elapsed().as_secs_f64() * 1e3
        );
    }
    out
}

/// The parallel terrain worker count.
///
/// `MEGU_GEN_JOBS` (0 or 1 = run inline) wins; the older name `MEGU_GEN_WORKERS`
/// still selects a count. The default is `min(available_parallelism, TERRAIN_MAX_JOBS)`:
/// terrain is what the whole generation costs (§"Baseline" in the performance README) and
/// the win is wall clock, not CPU — one chunk per worker is what vanilla does, so the
/// parallel arm is the default and the environment variable is the escape hatch.
pub fn terrain_jobs() -> usize {
    if let Some(v) = env_str!("MEGU_GEN_JOBS") {
        if let Ok(n) = v.trim().parse::<usize>() {
            return n;
        }
    }
    if let Some(v) = env_str!("MEGU_GEN_WORKERS") {
        if let Ok(n) = v.trim().parse::<usize>() {
            return n;
        }
    }
    std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
        .min(TERRAIN_MAX_JOBS)
}

/// Ceiling on the default worker count. Beyond this the group's serial tail (the feature
/// layer, overlays and encoding) is what the batch waits on, and each worker costs one
/// `Gen`'s caches of RSS.
pub const TERRAIN_MAX_JOBS: usize = 8;

/// Is this chunk's `ChunkCols` already in the generator's cache? Diagnostic only: it
/// tells whether a caller that asks for columns also pays for their generation.
/// Does this chunk's column set carry beardifier data? Diagnostic for the cache rule
/// that keeps preliminary (structure-free) columns out of `col_c`.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn cols_have_beard(cc: &ChunkCols) -> bool {
    cc.beard.is_some()
}

// Used by the server's band profiler, which is absent from auxiliary check targets.
#[allow(dead_code)]
pub fn cols_cached(cx: i32, cz: i32) -> bool {
    gen_lock()
        .as_ref()
        .is_some_and(|g| g.col_c.contains_key(&(cx, cz)))
}

pub fn chunk_cols(cx: i32, cz: i32) -> Option<std::sync::Arc<ChunkCols>> {
    chunk_cols_dim(active_dim(), cx, cz)
}

/// [`chunk_cols`] for one named dimension. A streaming caller must ask the generator that
/// owns the chunk, not whichever one `init` installed last: the Nether's column heights are
/// not the Overworld's.
pub fn chunk_cols_dim(dimension: DimId, cx: i32, cz: i32) -> Option<std::sync::Arc<ChunkCols>> {
    ensure_dim_generator(dimension);
    gen_lock_dim(dimension).as_mut()?.cols(cx, cz)
}

/// Tri-state `chunk_cols_dim` for latency-critical readers: a built column
/// comes back `Ready`, an uncontended cache miss is `Absent` (the caller may
/// build it synchronously), and a contended generator lock is `Contended`,
/// where the caller should defer instead of queueing behind a section pass.
pub(crate) enum PeekCols {
    Ready(std::sync::Arc<ChunkCols>),
    Absent,
    Contended,
}

pub(crate) fn peek_cols_dim(dimension: DimId, cx: i32, cz: i32) -> PeekCols {
    let Some(guard) = PER_DIM.gens.try_lock().ok() else {
        return PeekCols::Contended;
    };
    match guard
        .get(dimension.index())
        .and_then(|g| g.as_ref())
        .and_then(|g| g.col_c.get(&(cx, cz)).cloned())
    {
        Some(cols) => PeekCols::Ready(cols),
        None => PeekCols::Absent,
    }
}

type ChunkBiomeEntries = Vec<((i32, i32), std::sync::Arc<Vec<String>>)>;

impl Gen {
    /// Queue a leaf block tick.
    ///
    /// Each generator owns its queue, while runtime shares the active world's handle.
    /// The separate lock lets the tick thread drain without waiting for generation;
    /// process-global contents would mix independent generators and their tick order.
    pub(crate) fn schedule_generated_leaf(&mut self, pos: (i32, i32, i32), block: u32) {
        if self.suppress_generated_ticks {
            return;
        }
        let mut q = self.generated_ticks.leaves.lock().unwrap_or_else(|e| e.into_inner());
        if q.1.insert((pos, block)) {
            q.0.push((pos, block));
        }
    }

    pub(crate) fn generated_leaf_ticks(&self) -> Vec<((i32, i32, i32), u32)> {
        self.generated_ticks.leaves.lock().unwrap_or_else(|e| e.into_inner()).0.clone()
    }

    /// Queue a generated fluid tick: same reasoning as the leaf queue above — the tick
    /// thread drains it every tick, so it must not be behind the worldgen lock.
    pub(crate) fn schedule_generated_fluid(&mut self, pos: (i32, i32, i32), fluid: u32) {
        if self.suppress_generated_ticks {
            return;
        }
        let mut q = self.generated_ticks.fluids.lock().unwrap_or_else(|e| e.into_inner());
        if q.1.insert((pos, fluid)) {
            q.0.push((pos, fluid));
        }
    }

    /// Snapshot pending fluid ticks without transferring ownership, for saves.
    pub(crate) fn generated_fluid_ticks(&self) -> Vec<GeneratedTick> {
        self.generated_ticks.fluids.lock().unwrap_or_else(|e| e.into_inner()).0.clone()
    }

    pub(crate) fn tick_sources(&self) -> HashSet<(i32, i32)> {
        self.pop_done
            .union(&self.restored_tick_sources)
            .copied()
            .collect()
    }

    pub(crate) fn restore_tick_sources(&mut self, sources: HashSet<(i32, i32)>) {
        self.restored_tick_sources = sources;
    }

    pub(crate) fn restore_generated_ticks(
        &mut self,
        blocks: Vec<((i32, i32, i32), u32)>,
        fluids: Vec<((i32, i32, i32), u32)>,
    ) {
        // Restore only this world's queues, never another generator's pending work.
        {
            let mut q = self.generated_ticks.leaves.lock().unwrap_or_else(|e| e.into_inner());
            q.1 = blocks.iter().copied().collect();
            q.0 = blocks;
        }
        let mut fq = self.generated_ticks.fluids.lock().unwrap_or_else(|e| e.into_inner());
        fq.1 = fluids.iter().copied().collect();
        fq.0 = fluids;
    }

    /// Take this generator's pending feature ticks. A wave worker fills its own queues; the
    /// owner appends them in population order at the wave commit (A3-a), which keeps the tick
    /// order identical to the serial loop instead of interleaving workers arbitrarily.
    pub(crate) fn take_generated_ticks(&mut self) -> (Vec<GeneratedTick>, Vec<GeneratedTick>) {
        (
            drain_generated_ticks(&self.generated_ticks.leaves),
            drain_generated_ticks(&self.generated_ticks.fluids),
        )
    }

    /// Append ticks a wave worker produced, preserving their order and the dedup set.
    pub(crate) fn append_generated_ticks(
        &mut self,
        leaves: Vec<GeneratedTick>,
        fluids: Vec<GeneratedTick>,
    ) {
        for (queue, ticks) in [
            (&self.generated_ticks.leaves, leaves),
            (&self.generated_ticks.fluids, fluids),
        ] {
            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
            for tick in ticks {
                if q.1.insert(tick) {
                    q.0.push(tick);
                }
            }
        }
    }

    /// Per-Gen cached chunk column builder (terrain layer only — no features).
    /// A generator over the same immutable inputs with empty caches: the per-job scratch
    /// for terrain work (`specs/chunk-parallelism`, stage 2A). Every cache on `Gen` is a
    /// memoization of a pure function of `GenData` and the seed, so a cold copy produces
    /// the same `ChunkCols`; it only re-does work the shared generator may already have
    /// done. Nothing in `cols` reads another chunk's feature writes.
    pub(crate) fn fresh_scratch(data: std::sync::Arc<GenData>) -> Gen {
        Gen {
            data,
            memo: HashMap::new(),
            corner_memo: None,
            dfcol_scratch: None,
            aq_chunk: std::array::from_fn(|_| None),
            aq_chunk_use: [0; AQ_CHUNK_SLOTS],
            aq_chunk_tick: 0,
            aq_cell12: AqCell12::empty(),
            prelim_c: HashMap::new(),
            chunk_biomes: HashMap::new(),
            chunk_biomes_ord: std::collections::VecDeque::new(),
            prelim_ord: std::collections::VecDeque::new(),
            clim_c: HashMap::new(),
            clim_ord: std::collections::VecDeque::new(),
            bioq: HashMap::new(),
            bioq_ord: std::collections::VecDeque::new(),
            bioq_none: HashMap::new(),
            bioq_none_ord: std::collections::VecDeque::new(),
            base_hm_c: HashMap::new(),
            base_hm_cc: None,
            base_hm_memos: None,
            base_col_lazy: None,
            col_c: HashMap::new(),
            col_ord: std::collections::VecDeque::new(),
            starts_round: None,
            share_slot: None,
            base_c: HashMap::new(),
            base_ord: std::collections::VecDeque::new(),
            beard_c: HashMap::new(),
            beard_ord: std::collections::VecDeque::new(),
            beard_resolver: None,
            beard_depth: 0,
            terr_c: HashMap::new(),
            terr_ord: std::collections::VecDeque::new(),
            terr_pending: std::collections::BTreeSet::new(),
            feat_c: HashMap::new(),
            staged_payloads: Vec::new(),
            feat_ord: std::collections::VecDeque::new(),
            deep_c: HashMap::new(),
            deep_ord: std::collections::VecDeque::new(),
            pop_done: HashSet::new(),
            pop_plan_window: None,
            shape_marks: HashMap::new(),
            mark_pool: Vec::new(),
            mark_src: HashMap::new(),
            aq_sched: false,
            aq_calls: 0,
            terrain_marked: HashSet::new(),
            pop_active: None,
            restored_tick_sources: HashSet::new(),
            suppress_generated_ticks: false,
            carve_c: HashMap::new(),
            carve_ord: std::collections::VecDeque::new(),
            pop_region: std::env::var("MEGU_POP_REGION").ok().and_then(|v| {
                let p: Vec<i32> = v.split(',').filter_map(|n| n.parse().ok()).collect();
                if p.len() == 4 {
                    Some((p[0], p[1], p[2], p[3]))
                } else {
                    None
                }
            }),
            a3a_dispatch: None,
            mineshaft_spawners: HashSet::new(),
            payload_sources: HashSet::new(),
            feat_hm: HashMap::new(),
            generated_ticks: GeneratedTickQueues::default(),
            rule_diag: None,
        }
    }

    /// Install prefetched columns with the same FIFO eviction `cols` uses, so a caller
    /// never grows `col_c` past its bound behind the cache's back.
    pub(crate) fn install_cols(&mut self, cols: Vec<((i32, i32), std::sync::Arc<ChunkCols>)>) {
        for ((cx, cz), arc) in cols {
            if self.col_c.contains_key(&(cx, cz)) {
                continue;
            }
            self.col_ord.push_back((cx, cz));
            if self.col_ord.len() > COL_CACHE_CAP {
                let old = self.col_ord.pop_front().unwrap();
                self.col_c.remove(&old);
            }
            self.col_c.insert((cx, cz), arc);
        }
    }

    /// Install a wave worker's biome unions onto the owner (A3-a): `populate_worker`
    /// computes the union of each chunk's 3x3 (`chunk_biome_set`), so a chunk a later
    /// wave places inside that 3x3 reads the union from the owner instead of rebuilding
    /// it on the scratch, and `promote_full_chunks` answers from the same resident sets.
    /// Idempotent: unions already resident are left alone; the eviction bound matches
    /// `chunk_biome_set`.
    pub(crate) fn merge_chunk_biomes(
        &mut self,
        entries: ChunkBiomeEntries,
    ) {
        for (key, arc) in entries {
            if self.chunk_biomes.contains_key(&key) {
                continue;
            }
            self.chunk_biomes.insert(key, arc);
            self.chunk_biomes_ord.push_back(key);
            while self.chunk_biomes.len() > 384 {
                if let Some(old) = self.chunk_biomes_ord.pop_front() {
                    self.chunk_biomes.remove(&old);
                } else {
                    break;
                }
            }
        }
    }

    /// This generator's own beardifier resolver, for a worker that has to build the same
    /// columns the owner would (a test's per-generator override, not the global one).
    pub(crate) fn beard_resolver(&self) -> Option<BeardResolver> {
        self.beard_resolver
    }

    fn aq_section_all_air(&mut self, cx: i32, sy: i32, cz: i32) -> bool {
        if self.aq.is_none() {
            return sy >= self.sea && sy >= -54;
        }
        if sy < self.sea || sy < -53 {
            return false;
        }
        let slot = self.aq_chunk_slot(cx, cz);
        let min_gx = lxz(cx * 16 - 5);
        let max_gx = lxz(cx * 16 + 15 - 5) + 1;
        let min_gz = lxz(cz * 16 - 5);
        let max_gz = lxz(cz * 16 + 15 - 5) + 1;
        let min_gy = ly12(sy + 1) - 1;
        let max_gy = ly12(sy + 16) + 1;
        if !(self.aq_cache(slot).contains(min_gx, min_gy, min_gz)
            && self.aq_cache(slot).contains(max_gx, max_gy, max_gz))
        {
            return false;
        }
        let mut common: Option<(i32, u8)> = None;
        for gy in min_gy..=max_gy {
            for gz in min_gz..=max_gz {
                for gx in min_gx..=max_gx {
                    let idx = self.aq_cache(slot).idx(gx, gy, gz);
                    let _ = self.aq_cell_rpos(slot, idx, gx, gy, gz);
                    let lvl = self.aq_cell_level(slot, idx, false);
                    if sy < lvl.0 {
                        return false;
                    }
                    match common {
                        None => common = Some(lvl),
                        Some(prev) if prev == lvl => {}
                        Some(_) => return false,
                    }
                }
            }
        }
        common.is_some()
    }

    /// Base (feature-free) rows for one section: the terrain half of `block_state_at`,
    /// with the columns already fetched.
    fn base_section_rows(
        &mut self,
        cc: &ChunkCols,
        cx: i32,
        sy: i32,
        cz: i32,
        arr: &mut [u32; 4096],
    ) -> Vec<(u8, i32, i32, i32)> {
        // M3 P1a M4/M5: which thread filled this section, and what the fill cost. A worker
        // and the owner run this same function, so the split is decided by the thread the
        // caller marked (`m3_set_worker`), not by looking at the generator.
        m3_inc_split(M3_OWNER_SECTIONS, M3_WORKER_SECTIONS);
        let t0 = m3_start();
        let ly_top = self.min_y + self.height;
        if sy + 15 < self.min_y || sy >= ly_top {
            m3_elapsed(M3_FILL_NS, M3_FILL_N, t0);
            return Vec::new();
        }
        if !aq_trace_enabled() && sy >= self.min_y && sy + 16 <= ly_top {
            let max_surf = cc
                .surf
                .iter()
                .copied()
                .max()
                .unwrap_or((self.min_y - 1) as i16) as i32;
            if sy > max_surf && self.aq_section_all_air(cx, sy, cz) {
                self.aq_sched = false;
                m3_elapsed(M3_FILL_NS, M3_FILL_N, t0);
                return Vec::new();
            }
        }
        let mut marks: Vec<(u8, i32, i32, i32)> = Vec::new();
        // M3 P4 / A2-c: the section's densities are asked for eight columns at a time
        // (`Gen::block_density_x8`, falling back to two `x4` calls where the 8-lane arm
        // cannot run) and the cell rules then run per cell in the order they always ran:
        // `ly`, `lz`, `lx`. Only the density call is batched, and the values are the
        // individual `block_density` values bit for bit, so the order the aquifer, the
        // material rules and the deep rules see is the order they saw before.
        // Hoisted per-section invariants: the feature-read scope flag, the block
        // registry, and (lazily, on the first material read) the mats cell. Resolving
        // them once keeps each of the section's 4096 cells on the array write and the
        // aquifer/density decision alone.
        let m3_read = m3_on() && m3_in_feature_read();
        let reg = breg();
        let mut mats: Option<&SurfaceMats> = None;
        for ly in 0..16i32 {
            let y = sy + ly;
            if y < self.min_y || y >= ly_top {
                continue;
            }
            for lz in 0..16i32 {
                let z = cz * 16 + lz;
                // A z-row's 16 cells are contiguous in the section array: write through
                // the slice once per z rather than re-deriving `(ly|lz|lx)` per cell.
                let row = &mut arr[((ly << 8) | (lz << 4)) as usize
                    ..((ly << 8) | (lz << 4) | 15) as usize + 1];
                let mut lx = 0i32;
                while lx < 16 {
                    let x = cx * 16 + lx;
                    let d = self.block_density_x8(cc, x, y, z);
                    for (k, &lane_v) in d.iter().enumerate() {
                        let (state, marked) = self.terrain_cell_density_m(
                            cc,
                            &mut mats,
                            m3_read,
                            reg,
                            x + k as i32,
                            y,
                            z,
                            lane_v,
                        );
                        let lx = lx + k as i32;
                        row[lx as usize] = state;
                        // One entry per writer: vanilla preserves duplicates, and a cell the
                        // fill marked and the surface then rewrote with another fluid is
                        // visited twice by `postProcessGeneration`.
                        if marked != 0 {
                            for bit in [MARK_FILL, MARK_SURFACE] {
                                if marked & bit != 0 {
                                    marks.push((bit, x + k as i32, y, z));
                                }
                            }
                        }
                    }
                    lx += DF8 as i32;
                }
            }
        }
        // `ChunkAccess.postProcessing` keeps the order the writers filed in, and the
        // promotion pass consumes a section's bucket in that order, so the intra-section
        // order is part of the answer. The two writers have different traversals and
        // vanilla runs them in stage order, so the bucket is `fill marks (fill order)`
        // then `surface marks (surface order)`:
        //
        // * fill (`dzb.doFill`): xQuart, zQuart, y descending, xIn, zIn;
        // * surface (`dzq$1.setBlock`): column by column (x, then z), top down.
        //
        // Sorting here rather than at the consumer keeps the ordering decision next to the
        // two loops that produce it.
        // Each key includes the writer and all three coordinates, so there are no ties.
        marks.sort_unstable_by_key(|&(kind, x, y, z)| {
            if kind & MARK_FILL != 0 {
                (0u8, (x >> 2), (z >> 2), -y, (x & 3), (z & 3))
            } else {
                (1, x, z, -y, 0, 0)
            }
        });
        m3_elapsed(M3_FILL_NS, M3_FILL_N, t0);
        marks
    }

    /// Fill every section of `[lo, hi]` for one chunk on this (scratch) generator.
    /// `cols` lets a caller that already prefetched the chunk's columns reuse them; a
    /// missing column set is built here, which is the serial reference path.
    pub(crate) fn build_terrain_rows(
        &mut self,
        cx: i32,
        cz: i32,
        lo: i32,
        hi: i32,
        cols: Option<std::sync::Arc<ChunkCols>>,
    ) -> Option<(
        std::sync::Arc<ChunkCols>,
        Vec<BaseRow>,
    )> {
        let cc = match cols {
            Some(c) => c,
            None => self.cols(cx, cz)?,
        };
        let mut rows = Vec::with_capacity(if lo <= hi {
            ((i64::from(hi) - i64::from(lo)) / 16 + 1) as usize
        } else {
            0
        });
        let mut sy = lo;
        while sy <= hi {
            let mut arr = std::sync::Arc::new([0; 4096]);
            let marks =
                self.base_section_rows(&cc, cx, sy, cz, std::sync::Arc::get_mut(&mut arr).unwrap());
            rows.push(BaseRow {
                key: (cx, sy, cz),
                arr,
                marks,
            });
            sy += 16;
        }
        Some((cc, rows))
    }

    /// Land a worker's base rows in the store `terrain_section` reads, with the same FIFO
    /// eviction the other caches use. A section already in the store is left alone: both
    /// copies are the same pure result.
    /// Install terrain rows with the retention bound passed in rather than read from
    /// the environment: the `TERRAIN_STORE_BOUND` A/B has to be reproducible inside one
    /// process, and an env read is cached per call site.
    ///
    /// Retention is the only thing the bound decides. A row it evicts early is rebuilt from
    /// the (pure) columns when its consumer asks, so both arms have to return the same bytes.
    fn install_terrain_rows_bounded(&mut self, rows: Vec<BaseRow>, bound: usize) {
        for row in rows {
            let BaseRow { key, arr, marks } = row;
            // The marks are filed even when the rows are already present: a section whose
            // rows a previous round installed is the same section, but only the round that
            // built it decided its marks, and a second round would file them twice.
            self.file_terrain_marks(&key, &marks);
            if self.terr_c.contains_key(&key) {
                // M3 P1a M2: a duplicate install is worker terrain work whose consumer
                // already held the answer.
                m3_inc(M3_TERR_INSTALL_DUP);
                continue;
            }
            self.terr_ord.push_back(key);
            while self.terr_ord.len() > bound {
                if let Some(old) = self.terr_ord.pop_front() {
                    self.terr_c.remove(&old);
                    m3_inc(M3_TERR_EVICT);
                    // M4: the bound dropped a row before its consumer asked for it. That is
                    // the one thing raising `TERRAIN_STORE_BOUND` can fix, so it is counted
                    // separately from evictions a fully-drained store makes anyway.
                    if m3_on() && self.terr_pending.remove(&old) {
                        m3_inc(M3_EVICTED_UNCONSUMED);
                    }
                }
            }
            if m3_on() {
                self.terr_pending.insert(key);
            }
            m3_inc(M3_TERR_INSTALL);
            self.terr_c.insert(key, arr);
        }
    }

    /// `markPosForPostprocessing` for the positions one section's terrain stage filed, in
    /// the order that stage filed them. Guarded per section: `terrain_section` can be asked
    /// for the same section twice (a prefetch round and then a lazy read), and vanilla files
    /// each cell once.
    fn file_terrain_marks(&mut self, key: &(i32, i32, i32), marks: &[(u8, i32, i32, i32)]) {
        if marks.is_empty() || !self.terrain_marked.insert(*key) {
            return;
        }
        for &(kind, x, y, z) in marks {
            let src = if kind & MARK_FILL != 0 {
                MarkSrc::Fill
            } else {
                MarkSrc::Surface
            };
            self.store_shape_mark_from(x, y, z, src);
        }
    }

    /// The prefetched base terrain state of one cell, if a worker already built its section.
    ///
    /// The feature stage reads terrain cells that the terrain pass used to have warmed
    /// (`deep_c`, `aq_lv`, `prelim_c`) on the same generator. When the pass runs on a
    /// worker, those memos live on the worker, so a feature read would re-run the material
    /// and aquifer rules against a cold cache: measured on the 64-chunk full dump,
    /// `full - nofeat` (the feature stage) went 3.97 s serial -> 19.7 s at 5 terrain
    /// workers. The landed row is that same cell's base state, so answering from it keeps
    /// the feature stage's cost where it was.
    pub(crate) fn prefetched_cell(&self, x: i32, y: i32, z: i32) -> Option<u32> {
        // An empty store answers `None` without the tuple-hash probe; the miss
        // count still has to move or the report under-counts exactly the stage
        // being measured.
        if self.terr_c.is_empty() {
            m3_inc(M3_PREFETCH_MISS);
            return None;
        }
        let rows = match self.terr_c.get(&(x >> 4, y & !15, z >> 4)) {
            Some(rows) => rows,
            None => {
                m3_inc(M3_PREFETCH_MISS);
                return None;
            }
        };
        let st = rows
            .get((((y & 15) << 8) | ((z & 15) << 4) | (x & 15)) as usize)
            .copied();
        if st.is_some() {
            m3_inc(M3_PREFETCH_HIT);
        }
        st
    }

    /// Install a prefetch round: the workers' base rows, and the columns they built, so
    /// the serial feature/carver pass reads `col_c` instead of rebuilding them per chunk.
    pub(crate) fn install_prefetched(&mut self, chunks: Vec<Option<PrefetchedTerrain>>) {
        self.install_prefetched_bounded(chunks, terrain_store_bound());
    }

    /// [`Gen::install_prefetched`] with the retention bound passed in (see
    /// [`Gen::install_terrain_rows_bounded`]).
    pub(crate) fn install_prefetched_bounded(
        &mut self,
        chunks: Vec<Option<PrefetchedTerrain>>,
        bound: usize,
    ) {
        let mut cols = Vec::new();
        for chunk in chunks.into_iter().flatten() {
            cols.push((chunk.key, chunk.cols));
            self.install_terrain_rows_bounded(chunk.rows, bound);
        }
        self.install_cols(cols);
    }

    /// Set this generator's beardifier resolver.
    pub(crate) fn set_beard_resolver(&mut self, resolver: BeardResolver) {
        self.beard_resolver = Some(resolver);
    }

    /// The chunk's beardifier input, resolving (and caching) it on first use.
    ///
    /// `None` while another resolution is in progress: a structure's own heightmap
    /// query must describe the terrain without that structure, which is exactly why
    /// vanilla's `getBaseHeight` samples the BeardifierMarker and not the start.
    pub(crate) fn resolve_beard(
        &mut self,
        cx: i32,
        cz: i32,
    ) -> Option<std::sync::Arc<BeardChunkData>> {
        if let Some(v) = self.beard_c.get(&(cx, cz)) {
            return Some(v.clone());
        }
        if self.beard_depth > 0 {
            return None;
        }
        // `MEGU_NOFEAT=1` is the terrain-only comparison switch: it skips the whole
        // feature layer (`feature::populate`), and a structure-shaped terrain is part of
        // that layer. Applying structures to a terrain fixture would compare the port's
        // beardified terrain with a native capture that had none.
        if share_nofeat() {
            return None;
        }
        let resolver = self.beard_resolver.or_else(global_beard_resolver)?;
        // M3 P1b: the round's other workers and its owner want the same chunk's beardifier
        // input (P1a measured 234 resolutions serially against 64 per worker on top of 148-164
        // on the owner: the same chunk resolved twice). Resolving is only entered at
        // `beard_depth == 0`, so the share is asked here and nowhere nested.
        let share = self.round_share();
        let arc = match share {
            Some(share) => {
                let key = self.terrain_share_key(cx, cz);
                if let Some(v) = share.beards.peek(&key) {
                    v
                } else {
                    let token = flight_token(SHARE_DOMAIN_BEARDS, &key);
                    share
                        .beards
                        .get_or_build(key, token, || self.resolve_beard_uncached(cx, cz, resolver))
                }
            }
            None => self.resolve_beard_uncached(cx, cz, resolver),
        }?;
        // A structure generation may have populated the cache for this chunk already
        // (nested resolution is refused, so only the outermost call inserts).
        if let Some(v) = self.beard_c.get(&(cx, cz)) {
            return Some(v.clone());
        }
        self.beard_ord.push_back((cx, cz));
        if self.beard_ord.len() > BEARD_CACHE_CAP {
            if let Some(old) = self.beard_ord.pop_front() {
                self.beard_c.remove(&old);
            }
        }
        self.beard_c.insert((cx, cz), arc.clone());
        Some(arc)
    }

    /// [`Gen::resolve_beard`]'s body on this generator: the resolver call, its M1/M5 counters
    /// and the recursion refusal. Split out so the round's share can wrap it without either
    /// half owning the cache rule.
    fn resolve_beard_uncached(
        &mut self,
        cx: i32,
        cz: i32,
        resolver: BeardResolver,
    ) -> Option<std::sync::Arc<BeardChunkData>> {
        if self.beard_depth > 0 {
            return None;
        }
        self.beard_depth += 1;
        // M3 P1a M1/M5: a beardifier resolution is the 17x17 structure-start scan, the most
        // expensive per-chunk unit in `cols`. A flat count across worker counts means the
        // owner and the workers resolve disjoint chunks; a growing one means they repeat each
        // other (which is what the split is there to answer), so the count and the time are
        // both kept.
        let t0 = m3_start();
        let data = resolver(self, cx, cz);
        m3_elapsed(M3_BEARD_NS, M3_BEARD_N, t0);
        m3_inc_split(M3_BEARD_RESOLVED_OWNER, M3_BEARD_RESOLVED_WORKER);
        self.beard_depth -= 1;
        Some(std::sync::Arc::new(data))
    }

    /// This generator's round-scoped terrain share, or the per-world fallback share when no
    /// explicit round is open (and `WG_COUNT` is off).
    ///
    /// The round lookup (`terrain_share`) takes the process-wide mutex and this function is
    /// reached per `cols` / `base_cols` / `resolve_beard` call on every worker of a round,
    /// so the `(round, share)` pair is memoized in `share_slot`. Round ids are unique
    /// (`STARTS_NEXT_ROUND.fetch_add`), which makes the keyed memo safe across
    /// `starts_round` reassignments: a different round refreshes the slot, and `None`
    /// recomputes the fallback arm - an `Arc` clone that never touches the mutex anyway.
    /// A straggler that asks after its round ended now still finds its share (the `Arc`
    /// keeps it alive); the share's contents are the same keyed pure values it would have
    /// built privately, so the answer is unchanged.
    fn round_share(&mut self) -> Option<std::sync::Arc<TerrainShare>> {
        if let Some(round) = self.starts_round {
            if self.share_slot.as_ref().map(|(r, _)| *r) != Some(round) {
                self.share_slot = Some((round, terrain_share(round)));
            }
            self.share_slot.as_ref().unwrap().1.clone()
        } else if !wg_count() {
            Some(self.data.fallback_share.clone())
        } else {
            None
        }
    }

    /// The share key for one chunk. Two things beyond the chunk's identity change what a
    /// shared column contains and are therefore part of the key: the `MEGU_NOFEAT` switch
    /// (a terrain-only run has no beardifier at all) and which beardifier resolver is in force
    /// (a test's per-generator override is not the world's). Without them a terrain-only arm
    /// could take a bearded column from a neighbouring run's cache.
    fn terrain_share_key(&self, cx: i32, cz: i32) -> TerrainKey {
        let resolver = self.beard_resolver.or_else(global_beard_resolver);
        (
            self.world_seed,
            self.preset.dim().index() as u8,
            cx,
            cz,
            u8::from(share_nofeat()),
            resolver.map(|r| r as usize).unwrap_or(0),
        )
    }

    pub(crate) fn cols(&mut self, cx: i32, cz: i32) -> Option<std::sync::Arc<ChunkCols>> {
        if let Some(v) = self.col_c.get(&(cx, cz)) {
            m3_inc_split(M3_COLS_HIT_OWNER, M3_COLS_HIT_WORKER);
            return Some(v.clone());
        }
        // M3 P1b: a **final** column (built at `beard_depth == 0`) is what the round's other
        // threads would build again on their own scratch generator. A nested build
        // (`beard_depth > 0`) is the preliminary, structure-free answer and must never enter
        // this cache - see `TerrainShare`.
        let share = if self.beard_depth == 0 {
            self.round_share()
        } else {
            None
        };
        let arc = match share {
            Some(share) => {
                let key = self.terrain_share_key(cx, cz);
                if let Some(v) = share.cols.peek(&key) {
                    // The column another thread of this round already built: this is the
                    // `cols hit worker = 0` row of the P1a table, answered.
                    m3_inc_split(M3_COLS_HIT_OWNER, M3_COLS_HIT_WORKER);
                    v
                } else {
                    let token = flight_token(SHARE_DOMAIN_COLS, &key);
                    share.cols.get_or_build(key, token, || {
                        self.build_cols(cx, cz, true).map(std::sync::Arc::new)
                    })
                }
            }
            None => self.build_cols(cx, cz, true).map(std::sync::Arc::new),
        }?;
        // Columns built while a beardifier resolution was in flight are the preliminary
        // (structure-free) answer on purpose, and must not be cached: the same chunk's
        // final columns need the structure data this build could not ask for.
        if self.beard_depth == 0 {
            self.col_ord.push_back((cx, cz));
            if self.col_ord.len() > COL_CACHE_CAP {
                let old = self.col_ord.pop_front().unwrap();
                self.col_c.remove(&old);
            }
            self.col_c.insert((cx, cz), arc.clone());
        }
        Some(arc)
    }

    /// `ChunkGenerator.getBaseColumn(x, z, level, randomState)`: the column a
    /// structure's `findGenerationPoint` may scan, with no beardifier and no surface
    /// rule. Vanilla builds a fresh 1x1 `NoiseChunk` here, so the answer must not
    /// depend on the carvers, the features, or the chunk's own beardifier: a
    /// `nether_fossil` placed from the live column lands on top of the mound its own
    /// beard raises instead of inside the terrain the noise says is there.
    ///
    /// Cached separately from `col_c` because it is pure in (seed, preset, chunk);
    /// unlike `cols`, it is safe to cache while `beard_depth > 0`.
    ///
    /// M3 P1b: this is the one column the structure layer builds *nested*, so it is the one
    /// a round's workers repeat for each other. P1a measured the landing of that: the nested
    /// builds inside a beard resolution were per-worker work, because the only shared cache
    /// (`col_c`) refuses to file a `beard_depth > 0` column - correctly, since that column is
    /// the preliminary answer. A base column has no such rule (it is the noise answer alone),
    /// so it is shared with the round whenever one is open.
    pub(crate) fn base_cols(&mut self, cx: i32, cz: i32) -> Option<std::sync::Arc<ChunkCols>> {
        if let Some(v) = self.base_c.get(&(cx, cz)) {
            return Some(v.clone());
        }
        let share = self.round_share();
        let arc = match share {
            Some(share) => {
                let key = self.terrain_share_key(cx, cz);
                if let Some(v) = share.bases.peek(&key) {
                    v
                } else {
                    let token = flight_token(SHARE_DOMAIN_BASES, &key);
                    share.bases.get_or_build(key, token, || {
                        self.build_cols(cx, cz, false).map(std::sync::Arc::new)
                    })
                }
            }
            None => self.build_cols(cx, cz, false).map(std::sync::Arc::new),
        }?;
        self.base_ord.push_back((cx, cz));
        if self.base_ord.len() > BASE_CACHE_CAP {
            if let Some(old) = self.base_ord.pop_front() {
                self.base_c.remove(&old);
            }
        }
        self.base_c.insert((cx, cz), arc.clone());
        Some(arc)
    }

    /// One cell of `ChunkGenerator.getBaseColumn`: `NoiseChunk.getBlockState` with the
    /// beardifier node answering zero, i.e. density -> default block, otherwise the
    /// aquifer's substance, falling back to the default block where the aquifer
    /// refuses the cell (vanilla's `$$27 == null ? settings.defaultBlock() : $$27`).
    ///
    /// `None` outside the noise window, where vanilla's `NoiseColumn` has no cell.
    pub(crate) fn base_column_state(&mut self, x: i32, y: i32, z: i32) -> Option<u32> {
        if y < self.min_y || y >= self.min_y + self.height {
            return None;
        }
        let cx = x >> 4;
        let cz = z >> 4;
        if let Some(cc) = self.base_c.get(&(cx, cz)).cloned() {
            let d = self.block_density(&cc, x, y, z);
            if d > 0.0 {
                return Some(self.def_st);
            }
            return Some(self.aq_state(x, y, z, d).unwrap_or(self.def_st));
        }
        if let Some(st) = self.base_column_state_lazy(x, y, z) {
            return Some(st);
        }
        let cc = self.base_cols(cx, cz)?;
        let d = self.block_density(&cc, x, y, z);
        if d > 0.0 {
            return Some(self.def_st);
        }
        Some(self.aq_state(x, y, z, d).unwrap_or(self.def_st))
    }

    fn base_column_state_lazy(&mut self, x: i32, y: i32, z: i32) -> Option<u32> {
        if wg_count() || self.dfprog.is_none() || self.noise_cell_w <= 0 || self.noise_cell_h <= 0
        {
            return None;
        }
        let ub_slot = self.upper_bound_interp?;
        let ub_inner = match self.dfs[ub_slot] {
            DF::Interp(inner) => inner,
            _ => return None,
        };
        let cell_w = self.noise_cell_w;
        let cell_h = self.noise_cell_h;
        let min_y = self.min_y;
        let nx = (16 / cell_w + 1) as usize;
        let ny = ((self.height / cell_h) + 1) as usize;
        if nx < 2 || ny < 2 {
            return None;
        }
        let dfs_len = self.dfs.len();
        let grid_len = nx * nx * ny;
        let mut lazy = self.base_col_lazy.take().unwrap_or_else(|| {
            let mut grids: Vec<Option<Box<[f64]>>> = (0..dfs_len).map(|_| None).collect();
            for &(iid, _) in &self.density_interp_nodes {
                grids[iid] = Some(vec![0.0; grid_len].into_boxed_slice());
            }
            Box::new(BaseColLazy {
                xz: (i32::MAX, i32::MAX),
                cc: ChunkCols {
                    grids,
                    surf: [0i16; 256],
                    ny,
                    cell_w,
                    cell_h,
                    min_y,
                    mats: SurfMatsCell::new(0, 0),
                    rd: [0i16; 256],
                    wtop: [0i16; 256],
                    beard: None,
                    carve_sched: None,
                },
                memos: Box::new(std::array::from_fn(|_| FlatCornerMemo::new(dfs_len))),
                ub_filled: vec![false; ny],
                other_filled: vec![false; ny],
                corners: [(0, 0, 0, 0); 4],
                n_corners: 0,
                xi: 0,
                zi: 0,
                fx_zero: false,
                fz_zero: false,
            })
        });

        if lazy.xz != (x, z) {
            let cx = x >> 4;
            let cz = z >> 4;
            let lx = x & 15;
            let lz = z & 15;
            let xi = ((lx / cell_w) as usize).min(nx - 2);
            let zi = ((lz / cell_w) as usize).min(nx - 2);
            let fx_zero = lx == (xi as i32) * cell_w;
            let fz_zero = lz == (zi as i32) * cell_w;
            let mut n_corners = 0usize;
            let z_end = if fz_zero { 0 } else { 1 };
            let x_end = if fx_zero { 0 } else { 1 };
            for dzi in 0..=z_end {
                let bz = cz * 16 + ((zi + dzi) as i32) * cell_w;
                for dxi in 0..=x_end {
                    let bx = cx * 16 + ((xi + dxi) as i32) * cell_w;
                    lazy.corners[n_corners] = (dxi, dzi, bx, bz);
                    n_corners += 1;
                }
            }
            lazy.xz = (x, z);
            lazy.xi = xi;
            lazy.zi = zi;
            lazy.fx_zero = fx_zero;
            lazy.fz_zero = fz_zero;
            lazy.n_corners = n_corners;
            lazy.ub_filled.fill(false);
            lazy.other_filled.fill(false);
            for memo in lazy.memos.iter_mut().take(n_corners) {
                memo.next_2d();
            }
        }

        let yi = (((y - min_y) as f64 / f64::from(cell_h))
            .floor()
            .min((ny - 2) as f64)) as usize;
        let xi = lazy.xi;
        let zi = lazy.zi;
        let fx_zero = lazy.fx_zero;
        let fz_zero = lazy.fz_zero;
        let n_corners = lazy.n_corners;

        let write_corner = |cells: &mut [f64], dxi: usize, dzi: usize, lvl: usize, v: f64| {
            let set = |cells: &mut [f64], cxi: usize, czi: usize| {
                cells[(czi * nx + cxi) * ny + lvl] = v;
            };
            set(cells, xi + dxi, zi + dzi);
            if fx_zero {
                set(cells, xi + 1, zi + dzi);
            }
            if fz_zero {
                set(cells, xi + dxi, zi + 1);
                if fx_zero {
                    set(cells, xi + 1, zi + 1);
                }
            }
        };

        // Ensure `ub_slot` is evaluated at `yi` and `yi + 1`.
        for lvl in [yi, yi + 1] {
            if !lazy.ub_filled[lvl] {
                lazy.ub_filled[lvl] = true;
                let by = min_y + (lvl as i32) * cell_h;
                let ub_cells = lazy.cc.grids[ub_slot].as_mut().unwrap();
                for c in 0..n_corners {
                    let (dxi, dzi, bx, bz) = lazy.corners[c];
                    lazy.memos[c].next_3d();
                    let v = self.eval_flat(ub_inner, bx, by, bz, &mut lazy.memos[c]);
                    write_corner(ub_cells, dxi, dzi, lvl, v);
                }
            }
        }

        // Check if all active corners of `ub_slot` at [yi, yi+1] are <= 0.0.
        let mut any_pos = false;
        {
            let ub_cells = lazy.cc.grids[ub_slot].as_deref().unwrap();
            for &(dxi, dzi, _, _) in &lazy.corners[..n_corners] {
                let base = ((zi + dzi) * nx + (xi + dxi)) * ny + yi;
                if ub_cells[base] > 0.0 || ub_cells[base + 1] > 0.0 {
                    any_pos = true;
                    break;
                }
            }
        }

        if !any_pos {
            if let Some(aq_res) = self.aq_state_if_nonpos_independent(x, y, z) {
                self.base_col_lazy = Some(lazy);
                return Some(aq_res.unwrap_or(self.def_st));
            }
        }

        // Need exact `block_density`: fill any remaining density interp grids at [yi, yi+1].
        for lvl in [yi, yi + 1] {
            if !lazy.other_filled[lvl] {
                lazy.other_filled[lvl] = true;
                let by_lvl = min_y + (lvl as i32) * cell_h;
                for &(iid, kind) in &self.density_interp_nodes {
                    if iid == ub_slot {
                        continue;
                    }
                    let cells = lazy.cc.grids[iid].as_mut().unwrap();
                    match kind {
                        InterpKind::YNoise {
                            y_grad,
                            ni,
                            xzs,
                            ys,
                            mul,
                            add,
                        } => {
                            let out = y_grad.and_then(|(fc, fv, tc, tv, mi, ma, ov)| {
                                let t = ((by_lvl as f64) - fc) / (tc - fc);
                                let gv = fv + t.clamp(0.0, 1.0) * (tv - fv);
                                if gv >= mi && gv < ma { None } else { Some(ov) }
                            });
                            for &(dxi, dzi, bx, bz) in &lazy.corners[..n_corners] {
                                let v = match out {
                                    Some(ov) => ov,
                                    None => {
                                        let mut nv = self.noises[ni].get_uncached(
                                            bx as f64 * xzs,
                                            by_lvl as f64 * ys,
                                            bz as f64 * xzs,
                                        );
                                        if let Some((mc, left)) = mul {
                                            let (lhs, rhs) = if left { (mc, nv) } else { (nv, mc) };
                                            nv = lhs * rhs;
                                        }
                                        if let Some((ac, left)) = add {
                                            let (lhs, rhs) = if left { (ac, nv) } else { (nv, ac) };
                                            nv = lhs + rhs;
                                        }
                                        nv
                                    }
                                };
                                write_corner(cells, dxi, dzi, lvl, v);
                            }
                        }
                        InterpKind::General(inner) => {
                            for c in 0..n_corners {
                                let (dxi, dzi, bx, bz) = lazy.corners[c];
                                lazy.memos[c].next_3d();
                                let v = self.eval_flat(inner, bx, by_lvl, bz, &mut lazy.memos[c]);
                                write_corner(cells, dxi, dzi, lvl, v);
                            }
                        }
                    }
                }
            }
        }

        let d = self.block_density(&lazy.cc, x, y, z);
        self.base_col_lazy = Some(lazy);
        if d > 0.0 {
            return Some(self.def_st);
        }
        Some(self.aq_state(x, y, z, d).unwrap_or(self.def_st))
    }

    /// `cols` (`final_columns`) or `base_cols` (`!final_columns`): one chunk's columns.
    ///
    /// The body is [`Gen::build_cols_body`] so the M3 P1a counters can time every exit
    /// (the base-column path returns early, before the surface and heightmap passes).
    fn build_cols(&mut self, cx: i32, cz: i32, final_columns: bool) -> Option<ChunkCols> {
        m3_inc_split(M3_COLS_BUILT_OWNER, M3_COLS_BUILT_WORKER);
        let t0 = m3_start();
        let out = self.build_cols_body(cx, cz, final_columns);
        m3_elapsed(M3_COLS_NS, M3_COLS_N, t0);
        out
    }

    fn take_corner_memo(&mut self) -> FlatCornerMemo {
        self.corner_memo
            .take()
            .unwrap_or_else(|| FlatCornerMemo::new(self.dfs.len()))
    }

    /// The column evaluator's shared scratch, grown to `cols`' demand. Buffers
    /// keep their capacity across chunks; `v2` needs no clearing because `ops2`
    /// rewrites every slot before any `Op::Push2` reads it.
    fn take_dfcol_scratch(&mut self, cols: &DfCols) -> DfColScratch {
        let mut sc = self.dfcol_scratch.take().unwrap_or_default();
        if sc.st.len() < cols.slots {
            sc.st.resize(cols.slots, 0.0);
        }
        if sc.v2.len() < cols.n2 {
            sc.v2.resize(cols.n2, 0.0);
        }
        sc
    }

    fn fill_ynoise_grid(
        &self,
        cells: &mut [f64],
        cx: i32,
        cz: i32,
        nx: usize,
        ny: usize,
        kind: InterpKind,
    ) {
        let InterpKind::YNoise {
            y_grad,
            ni,
            xzs,
            ys,
            mul,
            add,
        } = kind
        else {
            return;
        };
        let noise = &self.noises[ni];
        let mut active_yi: Vec<usize> = Vec::with_capacity(ny);
        let mut active_y: Vec<f64> = Vec::with_capacity(ny);
        let mut out_val = 0.0;
        let has_range = y_grad.is_some();
        if let Some((fc, fv, tc, tv, mi, ma, ov)) = y_grad {
            out_val = ov;
            let inv = tc - fc;
            for yi in 0..ny {
                let by = self.min_y + (yi as i32) * self.noise_cell_h;
                let t = ((by as f64) - fc) / inv;
                let v = fv + t.clamp(0.0, 1.0) * (tv - fv);
                if v >= mi && v < ma {
                    active_yi.push(yi);
                    active_y.push((by as f64) * ys);
                }
            }
        } else {
            for yi in 0..ny {
                let by = self.min_y + (yi as i32) * self.noise_cell_h;
                active_yi.push(yi);
                active_y.push((by as f64) * ys);
            }
        }
        if has_range && active_yi.len() < ny {
            cells.fill(out_val);
        }
        if active_yi.is_empty() {
            return;
        }
        let apply = |mut v: f64| -> f64 {
            if let Some((c, left)) = mul {
                let (lhs, rhs) = if left { (c, v) } else { (v, c) };
                v = lhs * rhs;
            }
            if let Some((c, left)) = add {
                let (lhs, rhs) = if left { (c, v) } else { (v, c) };
                v = lhs + rhs;
            }
            v
        };
        let n_act = active_yi.len();
        for zi in 0..nx {
            let bz = (cz * 16 + (zi as i32) * self.noise_cell_w) as f64 * xzs;
            let vz8 = [bz; 8];
            let vz4 = [bz; 4];
            for xi in 0..nx {
                let bx = (cx * 16 + (xi as i32) * self.noise_cell_w) as f64 * xzs;
                let vx8 = [bx; 8];
                let vx4 = [bx; 4];
                let col_base = (zi * nx + xi) * ny;
                let mut i = 0usize;
                while i + 8 <= n_act {
                    let vy8 = [
                        active_y[i],
                        active_y[i + 1],
                        active_y[i + 2],
                        active_y[i + 3],
                        active_y[i + 4],
                        active_y[i + 5],
                        active_y[i + 6],
                        active_y[i + 7],
                    ];
                    let nv = noise.eval_x8_uncached(vx8, vy8, vz8);
                    for k in 0..8 {
                        cells[col_base + active_yi[i + k]] = apply(nv[k]);
                    }
                    i += 8;
                }
                while i + 4 <= n_act {
                    let vy4 = [
                        active_y[i],
                        active_y[i + 1],
                        active_y[i + 2],
                        active_y[i + 3],
                    ];
                    let nv = noise.eval_x4_uncached(vx4, vy4, vz4);
                    for k in 0..4 {
                        cells[col_base + active_yi[i + k]] = apply(nv[k]);
                    }
                    i += 4;
                }
                while i < n_act {
                    let nv = noise.get_uncached(bx, active_y[i], bz);
                    cells[col_base + active_yi[i]] = apply(nv);
                    i += 1;
                }
            }
        }
    }

    fn upper_bound_start_y(
        &self,
        cc: &ChunkCols,
        lx_start: usize,
        lx_end_excl: usize,
        lz: usize,
        beard_top: i32,
        top: i32,
    ) -> i32 {
        let Some(slot) = self.upper_bound_interp else {
            return top;
        };
        let Some(cells) = cc.grids.get(slot).and_then(|g| g.as_deref()) else {
            return top;
        };
        let cell_w = cc.cell_w as usize;
        let cell_h = cc.cell_h;
        let nx = 16 / cell_w + 1;
        let ny = cc.ny;
        if ny < 2 {
            return top;
        }
        let xi_lo = (lx_start / cell_w).min(nx - 2);
        let xi_hi = ((lx_end_excl - 1) / cell_w).min(nx - 2);
        let zi = (lz / cell_w).min(nx - 2);
        let mut yi = ny - 2;
        loop {
            let cell_lo_y = cc.min_y + (yi as i32) * cell_h;
            if cell_lo_y <= beard_top {
                return (cell_lo_y + cell_h - 1).min(top);
            }
            let mut any_pos = false;
            'corners: for dz in 0..=1 {
                let z_base = (zi + dz) * nx;
                for xi in xi_lo..=(xi_hi + 1) {
                    let base = (z_base + xi) * ny + yi;
                    if cells[base] > 0.0 || cells[base + 1] > 0.0 {
                        any_pos = true;
                        break 'corners;
                    }
                }
            }
            if any_pos {
                return (cell_lo_y + cell_h - 1).min(top);
            }
            if yi == 0 {
                return cc.min_y - 1;
            }
            yi -= 1;
        }
    }

    fn build_cols_body(&mut self, cx: i32, cz: i32, final_columns: bool) -> Option<ChunkCols> {
        let g = self;
        // Whether this build asks the aquifer decides what `carve_sched` means: `Some`
        // when a call wrote `aq_sched` during the build (the flag the chunk's own aquifer
        // holds), `None` when the build left the flag the caller already had.
        let aq0 = g.aq_calls;
        let ny = ((g.height / g.noise_cell_h) + 1) as usize;
        let nx = (16 / g.noise_cell_w + 1) as usize;
        // Resolve the chunk's beardifier before `surf`, `wtop` and `mats`: vanilla's
        // `fillFromNoise` evaluates `final_density` with `Beardifier` active and only
        // then calls `SurfaceSystem.buildSurface`. `preliminarySurfaceLevel` is the
        // unsmoothed estimate and is not affected; a nested heightmap query during
        // resolution runs with `beard_depth > 0`, gets `None` here, and is therefore
        // excluded from `col_c` (see the cache rule at the end of this function).
        let beard = if final_columns && !wg_count() {
            g.resolve_beard(cx, cz)
        } else {
            None
        };
        // One corner grid per `interpolated` node on the final_density path. The
        // grid holds the node's input; the enclosing operators are evaluated per
        // block in block_density(). Interpolating final_density itself would
        // interpolate squeeze(), which is cubic and not vanilla.
        let mut grids: Vec<Option<Box<[f64]>>>;
        // `WG_COUNT` counts visits through `eval_c`, so keep the original HashMap path
        // when diagnostics are on; the flat path has no per-visit counter to update.
        if wg_count() {
            grids = (0..g.dfs.len()).map(|_| None).collect();
            let mut memo: HashMap<(usize, i32, i32, i32), f64> = HashMap::new();
            for &(iid, inner) in &g.interp_nodes {
                let mut cells = vec![0.0f64; nx * nx * ny];
                for zi in 0..nx {
                    for xi in 0..nx {
                        for yi in 0..ny {
                            let bx = cx * 16 + (xi as i32) * g.noise_cell_w;
                            let by = g.min_y + (yi as i32) * g.noise_cell_h;
                            let bz = cz * 16 + (zi as i32) * g.noise_cell_w;
                            cells[(zi * nx + xi) * ny + yi] = g.eval(inner, bx, by, bz, &mut memo);
                        }
                    }
                }
                grids[iid] = Some(cells.into_boxed_slice());
            }
        } else {
            let mut reused = g
                .base_c
                .get(&(cx, cz))
                .or_else(|| g.col_c.get(&(cx, cz)))
                .map(|c| c.grids.clone())
                .or_else(|| {
                    let share = g.round_share()?;
                    let key = g.terrain_share_key(cx, cz);
                    share
                        .bases
                        .peek(&key)
                        .flatten()
                        .or_else(|| share.cols.peek(&key).flatten())
                        .map(|c| c.grids.clone())
                });
            let have_density = reused.is_some();
            grids = reused
                .take()
                .unwrap_or_else(|| (0..g.dfs.len()).map(|_| None).collect());
            if !have_density {
                let mut general: Vec<(usize, usize)> =
                    Vec::with_capacity(g.density_interp_nodes.len());
                for &(iid, kind) in &g.density_interp_nodes {
                    match kind {
                        InterpKind::General(inner) => {
                            grids[iid] = Some(vec![0.0; nx * nx * ny].into_boxed_slice());
                            general.push((iid, inner));
                        }
                        InterpKind::YNoise { .. } => {
                            let mut cells = vec![0.0; nx * nx * ny].into_boxed_slice();
                            g.fill_ynoise_grid(&mut cells, cx, cz, nx, ny, kind);
                            grids[iid] = Some(cells);
                        }
                    }
                }
                if !general.is_empty() {
                    let data = g.data.clone();
                    if let Some(cols) = data.dfcols.as_deref().filter(|_| !dfcol_flat()) {
                        let mut sc = g.take_dfcol_scratch(cols);
                        // Each grid's program resolves once; the column then pays
                        // the 2D prefix once and `ny` runs of each 3D tail, one
                        // contiguous run of cells per grid.
                        let mut resolved: Vec<(usize, &[Op])> =
                            Vec::with_capacity(general.len());
                        resolved.extend(
                            general
                                .iter()
                                .map(|&(iid, _)| (iid, cols.prog(iid).unwrap())),
                        );
                        for zi in 0..nx {
                            for xi in 0..nx {
                                let bx = cx * 16 + (xi as i32) * g.noise_cell_w;
                                let bz = cz * 16 + (zi as i32) * g.noise_cell_w;
                                cols.begin(g, bx, bz, &mut sc);
                                for &(iid, ops) in &resolved {
                                    let cells = grids[iid].as_mut().unwrap();
                                    let base = (zi * nx + xi) * ny;
                                    let mut yi = 0usize;
                                    // Four (or eight) levels of the tail at a
                                    // time on the AVX2 arm; the scalar tail
                                    // covers the `ny` remainder.
                                    while yi + DF8 <= ny {
                                        let by =
                                            g.min_y + (yi as i32) * g.noise_cell_h;
                                        let r = cols.eval_y8(
                                            ops,
                                            g,
                                            bx,
                                            by,
                                            g.noise_cell_h,
                                            bz,
                                            &mut sc,
                                        );
                                        cells[base + yi..base + yi + DF8]
                                            .copy_from_slice(&r);
                                        yi += DF8;
                                    }
                                    while yi + DF4 <= ny {
                                        let by =
                                            g.min_y + (yi as i32) * g.noise_cell_h;
                                        let r = cols.eval_y4(
                                            ops,
                                            g,
                                            bx,
                                            by,
                                            g.noise_cell_h,
                                            bz,
                                            &mut sc,
                                        );
                                        cells[base + yi..base + yi + DF4]
                                            .copy_from_slice(&r);
                                        yi += DF4;
                                    }
                                    while yi < ny {
                                        let by =
                                            g.min_y + (yi as i32) * g.noise_cell_h;
                                        cells[base + yi] =
                                            cols.eval(ops, g, bx, by, bz, &mut sc);
                                        yi += 1;
                                    }
                                }
                            }
                        }
                        g.dfcol_scratch = Some(sc);
                    } else {
                        let mut flat = g.take_corner_memo();
                        for zi in 0..nx {
                            for xi in 0..nx {
                                flat.next_2d();
                                let bx = cx * 16 + (xi as i32) * g.noise_cell_w;
                                let bz = cz * 16 + (zi as i32) * g.noise_cell_w;
                                for yi in 0..ny {
                                    flat.next_3d();
                                    let by = g.min_y + (yi as i32) * g.noise_cell_h;
                                    for &(iid, inner) in &general {
                                        grids[iid].as_mut().unwrap()
                                            [(zi * nx + xi) * ny + yi] =
                                            g.eval_flat(inner, bx, by, bz, &mut flat);
                                    }
                                }
                            }
                        }
                        g.corner_memo = Some(flat);
                    }
                }
            }
            if final_columns && !g.vein_interp_nodes.is_empty() {
                let mut general_vein: Vec<(usize, usize)> = Vec::new();
                for &(iid, kind) in &g.vein_interp_nodes {
                    if grids[iid].is_some() {
                        continue;
                    }
                    match kind {
                        InterpKind::General(inner) => {
                            grids[iid] = Some(vec![0.0; nx * nx * ny].into_boxed_slice());
                            general_vein.push((iid, inner));
                        }
                        InterpKind::YNoise { .. } => {
                            let mut cells = vec![0.0; nx * nx * ny].into_boxed_slice();
                            g.fill_ynoise_grid(&mut cells, cx, cz, nx, ny, kind);
                            grids[iid] = Some(cells);
                        }
                    }
                }
                if !general_vein.is_empty() {
                    let data = g.data.clone();
                    if let Some(cols) = data.vfcols.as_deref().filter(|_| !dfcol_flat()) {
                        let mut sc = g.take_dfcol_scratch(cols);
                        let resolved: Vec<(usize, &[Op])> = general_vein
                            .iter()
                            .map(|&(iid, _)| (iid, cols.prog(iid).unwrap()))
                            .collect();
                        for zi in 0..nx {
                            for xi in 0..nx {
                                let bx = cx * 16 + (xi as i32) * g.noise_cell_w;
                                let bz = cz * 16 + (zi as i32) * g.noise_cell_w;
                                cols.begin(g, bx, bz, &mut sc);
                                for &(iid, ops) in &resolved {
                                    let cells = grids[iid].as_mut().unwrap();
                                    let base = (zi * nx + xi) * ny;
                                    let mut yi = 0usize;
                                    while yi + DF8 <= ny {
                                        let by =
                                            g.min_y + (yi as i32) * g.noise_cell_h;
                                        let r = cols.eval_y8(
                                            ops,
                                            g,
                                            bx,
                                            by,
                                            g.noise_cell_h,
                                            bz,
                                            &mut sc,
                                        );
                                        cells[base + yi..base + yi + DF8]
                                            .copy_from_slice(&r);
                                        yi += DF8;
                                    }
                                    while yi + DF4 <= ny {
                                        let by =
                                            g.min_y + (yi as i32) * g.noise_cell_h;
                                        let r = cols.eval_y4(
                                            ops,
                                            g,
                                            bx,
                                            by,
                                            g.noise_cell_h,
                                            bz,
                                            &mut sc,
                                        );
                                        cells[base + yi..base + yi + DF4]
                                            .copy_from_slice(&r);
                                        yi += DF4;
                                    }
                                    while yi < ny {
                                        let by =
                                            g.min_y + (yi as i32) * g.noise_cell_h;
                                        cells[base + yi] =
                                            cols.eval(ops, g, bx, by, bz, &mut sc);
                                        yi += 1;
                                    }
                                }
                            }
                        }
                        g.dfcol_scratch = Some(sc);
                    } else {
                        let mut flat = g.take_corner_memo();
                        for zi in 0..nx {
                            for xi in 0..nx {
                                flat.next_2d();
                                let bx = cx * 16 + (xi as i32) * g.noise_cell_w;
                                let bz = cz * 16 + (zi as i32) * g.noise_cell_w;
                                for yi in 0..ny {
                                    flat.next_3d();
                                    let by = g.min_y + (yi as i32) * g.noise_cell_h;
                                    for &(iid, inner) in &general_vein {
                                        grids[iid].as_mut().unwrap()
                                            [(zi * nx + xi) * ny + yi] =
                                            g.eval_flat(inner, bx, by, bz, &mut flat);
                                    }
                                }
                            }
                        }
                        g.corner_memo = Some(flat);
                    }
                }
            }
        }
        let beard = if final_columns && wg_count() {
            g.resolve_beard(cx, cz)
        } else {
            beard
        };
        let mut cc = ChunkCols {
            grids,
            surf: [0i16; 256],
            ny,
            cell_w: g.noise_cell_w,
            cell_h: g.noise_cell_h,
            min_y: g.min_y,
            mats: SurfMatsCell::new(cx, cz),
            rd: [0i16; 256],
            wtop: [0i16; 256],
            beard,
            carve_sched: None,
        };
        // A base column is the noise answer only: vanilla's `getBaseColumn` runs neither
        // `SurfaceSystem.buildSurface` nor the heightmap pass, and `block_density` never
        // reads `surf`/`wtop`/`mats`, so skip all three instead of paying for them.
        if !final_columns {
            return Some(cc);
        }
        let top = g.min_y + g.height - 1;
        let beard_top = beard_max_y(cc.beard.as_deref());
        // M3 P4 / A2-c: eight columns of one `z` row per pass. Each lane keeps its own answer
        // - the first `y` from the top whose density is positive - so the searches share the
        // pass and the step down, and the lanes that have already found their surface are
        // simply no longer updated. Evaluating a lane past its own surface changes nothing
        // (density is pure in the position and the columns), which is what makes the lockstep
        // scan exactly the independent scans.
        for zi in 0..16usize {
            let wz = cz * 16 + zi as i32;
            let mut xb = 0usize;
            while xb < 16 {
                let wx = cx * 16 + xb as i32;
                let mut sy = [g.min_y - 1; DF8];
                let mut live = DF8;
                let mut yy = g.upper_bound_start_y(&cc, xb, xb + DF8, zi, beard_top, top);
                while yy >= g.min_y && live > 0 {
                    let d = g.block_density_x8(&cc, wx, yy, wz);
                    for (k, lane) in d.iter().enumerate() {
                        if sy[k] == g.min_y - 1 && *lane > 0.0 {
                            sy[k] = yy;
                            live -= 1;
                        }
                    }
                    yy -= 1;
                }
                for (k, lane) in sy.iter().enumerate() {
                    cc.surf[zi * 16 + xb + k] = *lane as i16;
                }
                xb += DF8;
            }
        }

        // heightmap basis: topmost solid-or-fluid per column. Columns whose
        // solid top sits below sea level probe the aquifer for water so
        // OCEAN_FLOOR/WORLD_SURFACE_WG land on the water surface.
        let water_st = g.def_fl;
        for lz in 0..16usize {
            for lx in 0..16usize {
                let sy = cc.surf[lz * 16 + lx] as i32;
                let mut wt = sy;
                if sy < g.sea {
                    let wx = cx * 16 + lx as i32;
                    let wz = cz * 16 + lz as i32;
                    let mut yy = g.sea;
                    while yy > sy {
                        if g.aq_state(wx, yy, wz, -1.0) == Some(water_st) {
                            wt = yy;
                            break;
                        }
                        yy -= 1;
                    }
                }
                cc.wtop[lz * 16 + lx] = wt as i16;
            }
        }
        // `aq_calls != aq0` means this build asked the aquifer (the `wtop` probe below sea
        // level, or a structure heightmap inside `resolve_beard`), so `aq_sched` is the flag
        // this chunk's aquifer holds at carve time — not the caller's leftover.
        cc.carve_sched = (g.aq_calls != aq0).then_some(g.aq_sched);
        // Layer A2-b: `mats`/`rd` are built on first material read (`ChunkCols::surface_mats`),
        // not here. Most columns in a worldgen round are neighbour/heightmap sources and never
        // read them; the exact in-pipeline scan now lives in `Gen::build_surface_mats`.
        Some(cc)
    }

    /// The eager `build_cols_body` surface-material scan, now the body of the lazy
    /// `ChunkCols::surface_mats` cell. Eight columns share a density buffer. The aquifer
    /// and material rules still visit each column and each cell in their original order.
    fn build_surface_mats(&mut self, cx: i32, cz: i32, cc: &ChunkCols) -> SurfaceMats {
        let g = self;
        // vanilla surface material rules: per-column top-down scan over the
        // surface window [est_min .. max(surf, sea)] tracking vanilla's
        // stone_depth_above/below and fluid_height.
        let mut mats: Vec<Option<SurfMat>> = (0..256).map(|_| None).collect();
        let mut rd = [0i16; 256];
        if let Some(root) = g.surface_rule {
            let mut c = SC::new();
            c.nt.resize(g.conds.len(), 0);
            let is_nether = g.preset == NoisePreset::Nether;
            let (h00, h10, h01, h11) = if is_nether {
                (0.0, 0.0, 0.0, 0.0)
            } else {
                (
                    g.prelim_est(cx * 16, cz * 16) as f64,
                    g.prelim_est(cx * 16 + 16, cz * 16) as f64,
                    g.prelim_est(cx * 16, cz * 16 + 16) as f64,
                    g.prelim_est(cx * 16 + 16, cz * 16 + 16) as f64,
                )
            };
            // Batch the densities in row order; the columns are still walked one
            // at a time, in the order they were walked before, so the aquifer, the surface
            // predicates and the per-column caches see exactly the sequence they saw.
            let mut dbuf: Vec<[f64; DF8]> = Vec::new();
            // A2-a: one reusable buffer for the 256 per-column surface-cell scans; the
            // previous `collect()` allocated a fresh Vec for every column.
            let mut cells: Vec<SurfaceCell> = Vec::new();
            let mut rds = [0i32; DF8];
            let mut est_mins = [0i32; DF8];
            let mut scan_tops = [0i32; DF8];
            for lz in 0..16usize {
                let z = cz * 16 + lz as i32;
                let fz = lz as f64 / 16.0;
                let mut xb = 0usize;
                while xb < 16 {
                    let mut min_lo = i32::MAX;
                    let mut max_top = i32::MIN;
                    for k in 0..DF8 {
                        let lx = xb + k;
                        let x = cx * 16 + lx as i32;
                        let rdv = g.run_depth(x, z);
                        rds[k] = rdv;
                        rd[lz * 16 + lx] = rdv as i16;
                        let scan_top = (cc.surf[lz * 16 + lx] as i32).max(g.sea);
                        scan_tops[k] = scan_top;
                        let est_min = if is_nether {
                            g.min_y
                        } else {
                            let est = lerp2(lx as f64 / 16.0, fz, h00, h10, h01, h11).floor() as i32;
                            est.wrapping_add(rdv).wrapping_sub(8)
                        };
                        est_mins[k] = est_min;
                        if scan_top >= est_min {
                            let lo = est_min.max(g.min_y);
                            if lo < min_lo {
                                min_lo = lo;
                            }
                            if scan_top > max_top {
                                max_top = scan_top;
                            }
                        }
                    }
                    dbuf.clear();
                    if min_lo <= max_top {
                        let gx = cx * 16 + xb as i32;
                        let span = (max_top - min_lo + 1) as usize;
                        dbuf.extend((0..span).map(|yi| {
                            let y = min_lo + yi as i32;
                            g.block_density_x8(cc, gx, y, z)
                        }));
                    }
                    for k in 0..DF8 {
                        let lx = xb + k;
                        let x = cx * 16 + lx as i32;
                        let scan_top = scan_tops[k];
                        let est_min = est_mins[k];
                        if scan_top < est_min {
                            continue;
                        }
                        c.x = x;
                        c.z = z;
                        c.run_depth = rds[k];
                        c.sec_ok = false;
                        c.steep = None;
                        c.clear_nt();
                        c.est_min = est_min;
                        let lo = est_min.max(g.min_y);
                        cells.clear();
                        cells.extend((lo..=scan_top).map(|y| {
                            g.surface_cell_density(dbuf[(y - min_lo) as usize][k], x, y, z)
                        }));
                        let column =
                            g.scan_surface_column_lazy(cc, root, &mut c, est_min, lo, &cells, &cc.wtop);
                        mats[lz * 16 + lx] = column;
                    }
                    xb += DF8;
                }
            }
        }
        SurfaceMats { mats, rd }
    }
}

impl Gen {
    /// The surface cell rule with the cell's density already in hand, so the surface window
    /// can ask for four columns' densities at once and still run vanilla's per-cell aquifer
    /// rules. `d` must be this position's own `block_density`.
    fn surface_cell_density(&mut self, d: f64, x: i32, y: i32, z: i32) -> SurfaceCell {
        if d > 0.0 {
            return SurfaceCell::Solid;
        }
        let aq = self.aq_state(x, y, z, d);
        let air = aq.is_some_and(|st| st == 0 || breg().is_some_and(|r| r.is_air(st)));
        let fluid = aq.is_some_and(|st| self.is_fluid_state(st));
        classify_surface_cell(d, aq, air, fluid)
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_surface_column_lazy(
        &mut self,
        cc: &ChunkCols,
        root: usize,
        c: &mut SC,
        est_min: i32,
        lo: i32,
        cells_above: &[SurfaceCell],
        heightmap: &[i16; 256],
    ) -> Option<SurfMat> {
        let min_y = self.min_y;
        let scan_top = lo + cells_above.len() as i32 - 1;
        if scan_top < est_min {
            return None;
        }
        let mut vals = vec![u16::MAX; (scan_top - est_min + 1) as usize];
        let mut min = i32::MAX;
        let mut sda = 0i32;
        let mut fluid = i32::MIN;
        for y in (est_min..=scan_top).rev() {
            let cell = if y >= lo {
                cells_above[(y - lo) as usize]
            } else {
                let d = self.block_density(cc, c.x, y, c.z);
                self.surface_cell_density(d, c.x, y, c.z)
            };
            match cell {
                SurfaceCell::Air => {
                    sda = 0;
                    fluid = i32::MIN;
                }
                SurfaceCell::Fluid => {
                    if fluid == i32::MIN {
                        fluid = y + 1;
                    }
                }
                SurfaceCell::Solid => {
                    if min >= y {
                        min = min_y;
                        let mut found = false;
                        if y > lo {
                            for sy in (lo..y).rev() {
                                if cells_above[(sy - lo) as usize] != SurfaceCell::Solid {
                                    min = sy + 1;
                                    found = true;
                                    break;
                                }
                            }
                        }
                        if !found {
                            let below_top = y.min(lo);
                            for sy in (min_y..below_top).rev() {
                                let d = self.block_density(cc, c.x, sy, c.z);
                                if d <= 0.0
                                    && self.surface_cell_density(d, c.x, sy, c.z)
                                        != SurfaceCell::Solid
                                {
                                    min = sy + 1;
                                    break;
                                }
                            }
                        }
                    }
                    sda += 1;
                    c.y = y;
                    c.sda = sda;
                    c.sdb = y - min + 1;
                    c.fluid = fluid;
                    if let Some(st) = self.eval_rule(root, c, heightmap) {
                        vals[(y - est_min) as usize] = st.min(u16::MAX as u32 - 1) as u16;
                    }
                }
            }
        }
        Some(SurfMat {
            lo: est_min as i16,
            vals: vals.into_boxed_slice(),
        })
    }

    /// The top-down surface scan, separated from terrain sampling so a synthetic
    /// column can be compared with the actual `SurfaceSystem.buildSurface`.
    /// `cells` is indexed by `y - min_y`; the scan starts at its top cell.
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn scan_surface_column(
        &mut self,
        root: usize,
        c: &mut SC,
        est_min: i32,
        cells: &[SurfaceCell],
        heightmap: &[i16; 256],
    ) -> Option<SurfMat> {
        let min_y = self.min_y;
        let scan_top = min_y + cells.len() as i32 - 1;
        if scan_top < est_min {
            return None;
        }
        let mut vals = vec![u16::MAX; (scan_top - est_min + 1) as usize];
        let mut min = i32::MAX;
        let mut sda = 0i32;
        let mut fluid = i32::MIN;
        for y in (est_min..=scan_top).rev() {
            match cells[(y - min_y) as usize] {
                SurfaceCell::Air => {
                    sda = 0;
                    fluid = i32::MIN;
                }
                SurfaceCell::Fluid => {
                    if fluid == i32::MIN {
                        fluid = y + 1;
                    }
                }
                SurfaceCell::Solid => {
                    if min >= y {
                        min = min_y;
                        for sy in (min_y..y).rev() {
                            if cells[(sy - min_y) as usize] != SurfaceCell::Solid {
                                min = sy + 1;
                                break;
                            }
                        }
                    }
                    sda += 1;
                    c.y = y;
                    c.sda = sda;
                    c.sdb = y - min + 1;
                    c.fluid = fluid;
                    if let Some(st) = self.eval_rule(root, c, heightmap) {
                        vals[(y - est_min) as usize] = st.min(u16::MAX as u32 - 1) as u16;
                    }
                }
            }
        }
        Some(SurfMat {
            lo: est_min as i16,
            vals: vals.into_boxed_slice(),
        })
    }

    /// Full block decision (feature layer, material window, aquifer, deep rules)
    /// without taking the generator lock. `terrain_section` and the free `block_state`
    /// use it so a chunk fill takes one lock instead of five per block.
    pub(crate) fn block_state_at(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        populate: impl FnMut(&mut Gen, i32, i32),
    ) -> u32 {
        if let Some(st) = self.feat_state(x, y, z, populate) {
            return st;
        }
        // The generator is defined over its own window: a cell above (or below) it is
        // not terrain, it is an empty section, and vanilla answers air there
        // (`NoiseChunk` has no cell, `ChunkAccess` has no palette entry). Without this
        // the density function is *evaluated* outside the window it was built for and
        // invents blocks: measured on the Nether preset, all 32 768 cells of y 128..255
        // in chunk (0,0) came back non-air. `feat_state` above is consulted first
        // because a feature may legitimately write there (the End City does).
        if y < self.min_y || y >= self.min_y + self.height {
            return 0;
        }
        let cc = match self.cols(x >> 4, z >> 4) {
            Some(c) => c,
            None => return 0,
        };
        let d = self.block_density(&cc, x, y, z);
        if d > 0.0 {
            // `NoiseChunk.blockState` asks the aquifer for every cell, and its `density > 0`
            // arm clears `shouldScheduleFluidUpdate` before returning null. This path skips the
            // question, so it has to clear the flag itself: otherwise it answers with the
            // previous cell's flag and a mark gate reading it is decided by a position it never
            // asked about (measured: `--worldgen-cell` reported `mark=true` for solid
            // `grass_block` surface cells).
            self.aq_sched = false;
            if let Some(st) = cc.mat(self, (x & 15) as usize, (z & 15) as usize, y) {
                let st = if st == u32::MAX { self.def_st } else { st };
                return self.vein_override(&cc, x, y, z, st);
            }
            let st = self.deep_state_cached(x, y, z, &cc);
            return self.vein_override(&cc, x, y, z, st);
        }
        let aq = self.aq_state(x, y, z, d);
        if let Some(st) = aq {
            if st == 0 || breg().is_some_and(|r| r.is_air(st)) {
                return 0;
            }
            return st;
        }
        let lx = (x & 15) as usize;
        let lz = (z & 15) as usize;
        let surface_top = cc.wtop[lz * 16 + lx] as i32;
        if surface_material_applies(y, surface_top) {
            if let Some(st) = cc.mat(self, lx, lz, y) {
                if st != u32::MAX {
                    return self.vein_override(&cc, x, y, z, st);
                }
            }
        }
        match aq {
            Some(st) => st,
            None => self.deep_state_cached(x, y, z, &cc),
        }
    }

    /// `MEGU_DFDUMP=x,y,z`: print the four density functions the vein
    /// sampler's caller passes, for comparison with the live server.
    pub(crate) fn dump_dfs(&mut self, x: i32, y: i32, z: i32) {
        let mut memo = std::mem::take(&mut self.memo);
        let cc = self.cols(x >> 4, z >> 4);
        let ccr = cc.as_deref();
        let fd = self.eval_c(self.final_density, x, y, z, &mut memo, ccr);
        let v = self.vein.map(|v| {
            (
                self.eval_c(v.toggle, x, y, z, &mut memo, ccr),
                self.eval_c(v.ridged, x, y, z, &mut memo, ccr),
                self.eval_c(v.gap, x, y, z, &mut memo, ccr),
            )
        });
        self.memo = memo;
        eprintln!("DFDUMP {x} {y} {z} final={fd:.6} vein={v:?}");
    }

    /// 1.21.1 `OreVeinSampler` (`dzi` in the obfuscated jar). The terrain state
    /// is overridden where a vein is present:
    ///
    /// ```text
    /// t = vein_toggle(ctx);  type = t > 0 ? COPPER : IRON
    /// if y outside [min, max) of the type                     -> no vein
    /// if |t| + map(min(up, down), 0, 20, -0.2, 0) < 0.4       -> no vein
    /// r = random at (x,y,z); if r.nextFloat() > 0.7           -> no vein
    /// if vein_ridged(ctx) >= 0                                -> no vein
    /// richness = map(|t|, 0.4, 0.6, 0.1, 0.3)
    /// if r.nextFloat() < richness && vein_gap(ctx) > -0.3
    ///     -> r.nextFloat() < 0.02 ? raw ore : ore
    /// else -> filler
    /// ```
    ///
    /// COPPER is y 0..50 with copper_ore / raw_copper_block / granite, IRON is
    /// y -60..-8 with deepslate_iron_ore / raw_iron_block / tuff.
    #[inline]
    fn eval_vein_df(&self, id: usize, cc: &ChunkCols, x: i32, y: i32, z: i32) -> Option<f64> {
        match &self.dfs[id] {
            DF::C(v) => Some(*v),
            DF::Zero => Some(0.0),
            DF::Interp(_) => cc
                .grids
                .get(id)
                .and_then(|g| g.as_deref())
                .map(|cells| trilinear(cells, cc, x, y, z)),
            DF::Cache(a) | DF::BlendD(a) => self.eval_vein_df(*a, cc, x, y, z),
            DF::Abs(a) => Some(self.eval_vein_df(*a, cc, x, y, z)?.abs()),
            DF::Add(a, b) => {
                Some(self.eval_vein_df(*a, cc, x, y, z)? + self.eval_vein_df(*b, cc, x, y, z)?)
            }
            DF::Max(a, b) => Some(df_max(
                self.eval_vein_df(*a, cc, x, y, z)?,
                self.eval_vein_df(*b, cc, x, y, z)?,
            )),
            DF::Noise {
                ni,
                sx,
                sy,
                sz,
                xzs,
                ys,
            } => {
                if matches!(self.dfs[*sx], DF::Zero)
                    && matches!(self.dfs[*sy], DF::Zero)
                    && matches!(self.dfs[*sz], DF::Zero)
                {
                    Some(self.noises[*ni].get(x as f64 * xzs, y as f64 * ys, z as f64 * xzs))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn vein_override(&mut self, cc: &ChunkCols, x: i32, y: i32, z: i32, st: u32) -> u32 {
        let v = match self.vein {
            Some(v) => v,
            None => {
                static ONCE: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(true);
                if env_flag!("MEGU_VEINDBG")
                    && ONCE.swap(false, std::sync::atomic::Ordering::Relaxed)
                {
                    eprintln!("VEINCFG sampler active but vein=None");
                }
                return st;
            }
        };
        if st == 0 {
            return st;
        }
        let map = |v: f64, lo: f64, hi: f64, from: f64, to: f64| {
            if v <= lo {
                from
            } else if v >= hi {
                to
            } else {
                (v - lo) / (hi - lo) * (to - from) + from
            }
        };
        // `MEGU_VEINTRACE="x,y,z"`: parsed once.
        static VEIN_TRACE_AT: std::sync::OnceLock<Option<[i32; 3]>> = std::sync::OnceLock::new();
        let dbg = *VEIN_TRACE_AT.get_or_init(|| {
            let v = std::env::var("MEGU_VEINTRACE").ok()?;
            let p: Vec<i32> = v.split(',').filter_map(|n| n.parse().ok()).collect();
            <[i32; 3]>::try_from(p).ok()
        }) == Some([x, y, z]);
        // Outside both vertical ranges, either toggle sign rejects the cell
        // before the sampler makes its first positional draw.
        if !dbg && !wg_count() && !((-60..=-8).contains(&y) || (0..=50).contains(&y)) {
            return st;
        }
        if !dbg && !wg_count() {
            if let Some(t) = self.eval_vein_df(v.toggle, cc, x, y, z) {
                let (min_y, max_y, ore, raw, fill) = if t > 0.0 {
                    (0i32, 50i32, v.copper_ore, v.copper_raw, v.copper_fill)
                } else {
                    (-60i32, -8i32, v.iron_ore, v.iron_raw, v.iron_fill)
                };
                let (up, down) = (max_y - y, y - min_y);
                if up < 0 || down < 0 {
                    return st;
                }
                let at = t.abs();
                let roundoff = map(up.min(down) as f64, 0.0, 20.0, -0.2, 0.0);
                if at + roundoff < 0.4 {
                    return st;
                }
                let mut r = match self.ore_split.as_ref() {
                    Some(f) => f.pos(x, y, z),
                    None => self.factory.pos(x, y, z),
                };
                if r.next_f32() > 0.7 {
                    return st;
                }
                let ridged = match self.eval_vein_df(v.ridged, cc, x, y, z) {
                    Some(rv) => rv,
                    None => {
                        let mut memo = std::mem::take(&mut self.memo);
                        let rv = self.eval_c(v.ridged, x, y, z, &mut memo, Some(cc));
                        self.memo = memo;
                        rv
                    }
                };
                if ridged >= 0.0 {
                    return st;
                }
                let richness = map(at, 0.4, 0.6, 0.1, 0.3);
                let r2 = r.next_f32();
                let gv = match self.eval_vein_df(v.gap, cc, x, y, z) {
                    Some(g) => g,
                    None => {
                        let mut memo = std::mem::take(&mut self.memo);
                        let g = self.eval_c(v.gap, x, y, z, &mut memo, Some(cc));
                        self.memo = memo;
                        g
                    }
                };
                let r3 = r.next_f32();
                return if (r2 as f64) < richness && gv > -0.3 {
                    if r3 < 0.02 { raw } else { ore }
                } else {
                    fill
                };
            }
        }
        let mut memo = std::mem::take(&mut self.memo);
        let t = self.eval_c(v.toggle, x, y, z, &mut memo, Some(cc));
        let (min_y, max_y, ore, raw, fill) = if t > 0.0 {
            (0i32, 50i32, v.copper_ore, v.copper_raw, v.copper_fill)
        } else {
            (-60i32, -8i32, v.iron_ore, v.iron_raw, v.iron_fill)
        };
        if dbg {
            let inner = match self.dfs[v.toggle] {
                DF::Interp(i) => i,
                _ => v.toggle,
            };
            let direct = self.eval_c(inner, x, y, z, &mut memo, None);
            let nodirect = self.eval_c(v.toggle, x, y, z, &mut memo, None);
            eprintln!(
                "VEIN {x} {y} {z} t={t:.4} direct={direct:.4} interp_no_cc={nodirect:.4} st={st}"
            );
        }
        let (up, down) = (max_y - y, y - min_y);
        let out = if up < 0 || down < 0 {
            None
        } else {
            let at = t.abs();
            let roundoff = map(up.min(down) as f64, 0.0, 20.0, -0.2, 0.0);
            if at + roundoff < 0.4 {
                None
            } else {
                // `RandomState` builds `this.g = this.a.fromHashOf("ore")
                // .forkPositional()` and hands *that* to OreVeinifier, not the
                // base factory (decompiled dzm: `this.g = this.a.a(akr.b("ore")).e()`).
                let mut r = match self.ore_split.as_ref() {
                    Some(f) => f.pos(x, y, z),
                    None => self.factory.pos(x, y, z),
                };
                let r1 = r.next_f32();
                if dbg {
                    eprintln!(
                        "VEIN   up={up} down={down} at={at:.4} round={roundoff:.4} r1={r1:.4}"
                    );
                }
                if r1 > 0.7 {
                    None
                } else if self.eval_c(v.ridged, x, y, z, &mut memo, Some(cc)) >= 0.0 {
                    if dbg {
                        eprintln!("VEIN   ridged reject");
                    }
                    None
                } else {
                    let richness = map(at, 0.4, 0.6, 0.1, 0.3);
                    let r2 = r.next_f32();
                    let gv = self.eval_c(v.gap, x, y, z, &mut memo, Some(cc));
                    let r3 = r.next_f32();
                    if dbg {
                        eprintln!("VEIN   rich={richness:.4} r2={r2:.4} gap={gv:.4} r3={r3:.4}");
                    }
                    if (r2 as f64) < richness && gv > -0.3 {
                        Some(if r3 < 0.02 { raw } else { ore })
                    } else {
                        Some(fill)
                    }
                }
            }
        };
        self.memo = memo;
        out.unwrap_or(st)
    }

    /// Terrain-layer block state at a world position (no feature layer).
    /// Mirrors wg2_block: material window -> deep rules -> aquifer.
    ///
    /// Only the generation probes and the contract tests read through this: the
    /// generator itself goes to the material rules directly.
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn terrain_state(&mut self, x: i32, y: i32, z: i32) -> u32 {
        let cc = match self.cols(x >> 4, z >> 4) {
            Some(c) => c,
            None => return 0,
        };
        self.terrain_cell(&cc, x, y, z)
    }

    /// `terrain_state` with the chunk columns already fetched. Feature placement
    /// reads thousands of cells from one chunk; fetching `cols()` (a HashMap
    /// lookup plus an Arc clone) per cell was ~1.2 us of the 1.8 us read.
    pub(crate) fn terrain_cell(&mut self, cc: &ChunkCols, x: i32, y: i32, z: i32) -> u32 {
        self.terrain_cell_marked(cc, x, y, z).0
    }

    /// [`Gen::terrain_cell`] plus the `markPosForPostprocessing` decision the same pass makes.
    ///
    /// `MARK_FILL` is `NoiseBasedChunkGenerator.doFill`'s gate — the cell it wrote holds a fluid
    /// and `Aquifer.shouldScheduleFluidUpdate()` is true — evaluated on the state the **fill**
    /// wrote, which is the aquifer's substance and not the material rule's answer: when the
    /// surface stage overwrites the cell afterwards, vanilla's mark survives and its state does
    /// not (the measured 106 RUNTIME lines contain 18 `dirt_path` and 7 `grass_block` cells
    /// whose mark was filed long before those states were written).
    ///
    /// `MARK_SURFACE` is `SurfaceSystem`'s `BlockUpdater.setBlock`: a write that carries a fluid.
    /// There the state has to be the one this function returns, because that *is* the surface
    /// write — the two writers are distinguished by which stage produced the returned state.
    pub(crate) fn terrain_cell_marked(
        &mut self,
        cc: &ChunkCols,
        x: i32,
        y: i32,
        z: i32,
    ) -> (u32, u8) {
        if y < self.min_y || y >= self.min_y + self.height {
            return (0, 0);
        }
        let d = self.block_density(cc, x, y, z);
        self.terrain_cell_density(cc, x, y, z, d)
    }

    /// [`Gen::terrain_cell_marked`]'s body with the density already in hand.
    ///
    /// The only thing split off is the `block_density` call: the caller must have checked the
    /// generator's window (a cell outside it is air, not a density evaluation) and must pass
    /// that cell's own density. That is what lets the section fill ask for eight columns'
    /// densities at once (`Gen::block_density_x8`) and still run vanilla's per-cell aquifer,
    /// material and deep rules in the order it ran them.
    pub(crate) fn terrain_cell_density(
        &mut self,
        cc: &ChunkCols,
        x: i32,
        y: i32,
        z: i32,
        d: f64,
    ) -> (u32, u8) {
        let mut mats: Option<&SurfaceMats> = None;
        self.terrain_cell_density_m(
            cc,
            &mut mats,
            m3_on() && m3_in_feature_read(),
            breg(),
            x,
            y,
            z,
            d,
        )
    }

    /// `terrain_cell_density` with the per-call invariants passed in: the fill loop
    /// resolves `m3_read`, the block registry, and the section's `surface_mats` cell
    /// once instead of inside each of the 4096 cells of a section.
    #[allow(clippy::too_many_arguments)]
    fn terrain_cell_density_m<'a>(
        &mut self,
        cc: &'a ChunkCols,
        mats: &mut Option<&'a SurfaceMats>,
        m3_read: bool,
        reg: Option<&BReg>,
        x: i32,
        y: i32,
        z: i32,
        d: f64,
    ) -> (u32, u8) {
        // M3 P1a M3: which layer answered a *feature-side* cold read. The scope is set by
        // `FC::get`'s fallback, so the terrain fill (which warms the same memos on purpose)
        // stays out of these counts. `m3_on() &&` keeps the off path at one cached load.
        if d > 0.0 {
            // `NoiseChunk.blockState` asks the aquifer for every cell, and its `density > 0`
            // arm clears `shouldScheduleFluidUpdate` before returning null. This path skips the
            // question, so it has to clear the flag itself: otherwise it answers with the
            // previous cell's flag and a mark gate reading it is decided by a position it never
            // asked about (measured: `--worldgen-cell` reported `mark=true` for solid
            // `grass_block` surface cells).
            self.aq_sched = false;
            if let Some(st) = mat_in(
                mats.get_or_insert_with(|| cc.surface_mats(self)),
                (x & 15) as usize,
                (z & 15) as usize,
                y,
            ) {
                let st = if st == u32::MAX { self.def_st } else { st };
                let st = self.vein_override(cc, x, y, z, st);
                inc_if(m3_read, M3_FALLBACK_DENSITY);
                return (st, terrain_mark(reg, 0, st));
            }
            let st = self.deep_state_cached(x, y, z, cc);
            let st = self.vein_override(cc, x, y, z, st);
            inc_if(m3_read, M3_FALLBACK_DEEP);
            return (st, 0);
        }
        let aq = self.aq_state(x, y, z, d);
        let fill_mark = if self.aq_sched {
            MARK_FILL
        } else {
            0
        };
        if let Some(st) = aq {
            if st == 0 || reg.is_some_and(|r| r.is_air(st)) {
                return (0, 0);
            }
            // The fill wrote this cell, so the gate is the one vanilla puts on the fill.
            inc_if(m3_read, M3_FALLBACK_AQUIFER);
            return (st, terrain_mark(reg, fill_mark, st));
        }
        let lx = (x & 15) as usize;
        let lz = (z & 15) as usize;
        let surface_top = cc.wtop[lz * 16 + lx] as i32;
        if surface_material_applies(y, surface_top) {
            if let Some(st) = mat_in(
                mats.get_or_insert_with(|| cc.surface_mats(self)),
                lx,
                lz,
                y,
            ) {
                if st != u32::MAX {
                    let st = self.vein_override(cc, x, y, z, st);
                    // The aquifer answered a barrier here, so this state is the surface
                    // stage's write and only its own fluid test applies.
                    let _ = fill_mark;
                    m3_inc(M3_SURFACE_CELLS);
                    inc_if(m3_read, M3_FALLBACK_SURFACE);
                    return (st, terrain_mark(reg, MARK_SURFACE, st));
                }
            }
        }
        match aq {
            Some(st) => {
                inc_if(m3_read, M3_FALLBACK_AQUIFER);
                (st, terrain_mark(reg, fill_mark, st))
            }
            None => {
                let st = self.deep_state_cached(x, y, z, cc);
                inc_if(m3_read, M3_FALLBACK_DEEP);
                (st, 0)
            }
        }
    }
    /// `DimensionType.hasSkyLight()` for this preset. Only the Overworld has a sky
    /// light layer.
    pub(crate) fn has_skylight(&self) -> bool {
        self.preset.dim().has_skylight()
    }
    /// Single-column `surf` scan over an already-built chunk, top-down. This is the
    /// one-column form of `build_cols_body`'s eight-lane scan: the first `y` from the top
    /// whose density is positive, or `min_y - 1` when the column has no solid cell.
    /// `block_density` at one column equals that lane's `block_density_x8` value bit for
    /// bit, so the answer is the same cell the full column build would have filed.
    fn single_col_surf(&self, cc: &ChunkCols, x: i32, z: i32) -> i32 {
        let lx = (x & 15) as usize;
        let lz = (z & 15) as usize;
        let idx = lz * 16 + lx;
        let cached = cc.mats.base_surf[idx].load(std::sync::atomic::Ordering::Relaxed);
        if cached != i16::MIN {
            return cached as i32;
        }
        let top = self.min_y + self.height - 1;
        let beard_top = beard_max_y(cc.beard.as_deref());
        let mut yy = self.upper_bound_start_y(cc, lx, lx + 1, lz, beard_top, top);
        while yy >= self.min_y {
            if self.block_density(cc, x, yy, z) > 0.0 {
                cc.mats.base_surf[idx].store(yy as i16, std::sync::atomic::Ordering::Relaxed);
                return yy;
            }
            yy -= 1;
        }
        let res = self.min_y - 1;
        cc.mats.base_surf[idx].store(res as i16, std::sync::atomic::Ordering::Relaxed);
        res
    }

    /// The matching single-column water-surface scan: a solid top below sea level probes
    /// the aquifer downward for the default fluid, exactly as `build_cols_body` does for
    /// every column of a final chunk.
    fn single_col_wtop(&mut self, cc: &ChunkCols, x: i32, z: i32, surf: i32) -> i32 {
        let idx = ((z & 15) as usize) * 16 + (x & 15) as usize;
        let cached = cc.mats.base_wtop[idx].load(std::sync::atomic::Ordering::Relaxed);
        if cached != i16::MIN {
            return cached as i32;
        }
        let mut wt = surf;
        if surf < self.sea {
            let mut yy = self.sea;
            while yy > surf {
                if self.aq_state(x, yy, z, -1.0) == Some(self.def_fl) {
                    wt = yy;
                    break;
                }
                yy -= 1;
            }
        }
        cc.mats.base_wtop[idx].store(wt as i16, std::sync::atomic::Ordering::Relaxed);
        wt
    }

    /// Lazy single-column base surface scan for `top_ws`/`top_oc` while `beard_depth > 0`.
    /// Evaluates only the 1..4 horizontal corners needed for `(x, z)` top-down, skipping
    /// empty sky cells and stopping at the first positive density cell without building
    /// a full 25-column chunk grid.
    fn compute_base_col_surf_lazy(&mut self, x: i32, z: i32) -> Option<i32> {
        if wg_count() || self.dfprog.is_none() || self.noise_cell_w <= 0 || self.noise_cell_h <= 0
        {
            return None;
        }
        let ub_slot = self.upper_bound_interp?;
        let ub_inner = match self.dfs[ub_slot] {
            DF::Interp(inner) => inner,
            _ => return None,
        };
        let cx = x >> 4;
        let cz = z >> 4;
        let lx = x & 15;
        let lz = z & 15;
        let cell_w = self.noise_cell_w;
        let cell_h = self.noise_cell_h;
        let min_y = self.min_y;
        let top = min_y + self.height - 1;
        let nx = (16 / cell_w + 1) as usize;
        let ny = ((self.height / cell_h) + 1) as usize;
        if nx < 2 || ny < 2 {
            return None;
        }
        let xi = ((lx / cell_w) as usize).min(nx - 2);
        let zi = ((lz / cell_w) as usize).min(nx - 2);
        let fx_zero = lx == (xi as i32) * cell_w;
        let fz_zero = lz == (zi as i32) * cell_w;

        let mut corners = [(0usize, 0usize, 0i32, 0i32); 4];
        let mut n_corners = 0usize;
        let z_end = if fz_zero { 0 } else { 1 };
        let x_end = if fx_zero { 0 } else { 1 };
        for dzi in 0..=z_end {
            let bz = cz * 16 + ((zi + dzi) as i32) * cell_w;
            for dxi in 0..=x_end {
                let bx = cx * 16 + ((xi + dxi) as i32) * cell_w;
                corners[n_corners] = (dxi, dzi, bx, bz);
                n_corners += 1;
            }
        }

        let dfs_len = self.dfs.len();
        let grid_len = nx * nx * ny;
        let mut scratch_cc = self.base_hm_cc.take().unwrap_or_else(|| {
            let mut grids: Vec<Option<Box<[f64]>>> = (0..dfs_len).map(|_| None).collect();
            for &(iid, _) in &self.density_interp_nodes {
                grids[iid] = Some(vec![0.0; grid_len].into_boxed_slice());
            }
            Box::new(ChunkCols {
                grids,
                surf: [0i16; 256],
                ny,
                cell_w,
                cell_h,
                min_y,
                mats: SurfMatsCell::new(cx, cz),
                rd: [0i16; 256],
                wtop: [0i16; 256],
                beard: None,
                carve_sched: None,
            })
        });
        let mut memos = self.base_hm_memos.take().unwrap_or_else(|| {
            Box::new(std::array::from_fn(|_| FlatCornerMemo::new(dfs_len)))
        });
        for memo in memos.iter_mut().take(n_corners) {
            memo.next_2d();
        }

        let write_corner = |cells: &mut [f64], dxi: usize, dzi: usize, yi: usize, v: f64| {
            let set = |cells: &mut [f64], cxi: usize, czi: usize| {
                cells[(czi * nx + cxi) * ny + yi] = v;
            };
            set(cells, xi + dxi, zi + dzi);
            if fx_zero {
                set(cells, xi + 1, zi + dzi);
            }
            if fz_zero {
                set(cells, xi + dxi, zi + 1);
                if fx_zero {
                    set(cells, xi + 1, zi + 1);
                }
            }
        };

        // Evaluate top vertical grid level `ny - 1` for `ub_slot`.
        let by_top = min_y + ((ny - 1) as i32) * cell_h;
        let mut prev_pos = false;
        {
            let ub_cells = scratch_cc.grids[ub_slot].as_mut().unwrap();
            for c in 0..n_corners {
                let (dxi, dzi, bx, bz) = corners[c];
                memos[c].next_3d();
                let v = self.eval_flat(ub_inner, bx, by_top, bz, &mut memos[c]);
                write_corner(ub_cells, dxi, dzi, ny - 1, v);
                if v > 0.0 {
                    prev_pos = true;
                }
            }
        }

        let mut other_filled_stack = [false; 128];
        let mut other_filled_heap;
        let other_filled: &mut [bool] = if ny <= 128 {
            &mut other_filled_stack[..ny]
        } else {
            other_filled_heap = vec![false; ny];
            &mut other_filled_heap
        };

        let mut found = min_y - 1;
        for yi in (0..ny - 1).rev() {
            let by = min_y + (yi as i32) * cell_h;
            let mut cur_pos = false;
            {
                let ub_cells = scratch_cc.grids[ub_slot].as_mut().unwrap();
                for c in 0..n_corners {
                    let (dxi, dzi, bx, bz) = corners[c];
                    memos[c].next_3d();
                    let v = self.eval_flat(ub_inner, bx, by, bz, &mut memos[c]);
                    write_corner(ub_cells, dxi, dzi, yi, v);
                    if v > 0.0 {
                        cur_pos = true;
                    }
                }
            }
            if prev_pos || cur_pos {
                for lvl in [yi + 1, yi] {
                    if !other_filled[lvl] {
                        other_filled[lvl] = true;
                        let by_lvl = min_y + (lvl as i32) * cell_h;
                        for &(iid, kind) in &self.density_interp_nodes {
                            if iid == ub_slot {
                                continue;
                            }
                            let cells = scratch_cc.grids[iid].as_mut().unwrap();
                            match kind {
                                InterpKind::YNoise {
                                    y_grad,
                                    ni,
                                    xzs,
                                    ys,
                                    mul,
                                    add,
                                } => {
                                    let out = y_grad.and_then(|(fc, fv, tc, tv, mi, ma, ov)| {
                                        let t = ((by_lvl as f64) - fc) / (tc - fc);
                                        let gv = fv + t.clamp(0.0, 1.0) * (tv - fv);
                                        if gv >= mi && gv < ma { None } else { Some(ov) }
                                    });
                                    for &(dxi, dzi, bx, bz) in &corners[..n_corners] {
                                        let v = match out {
                                            Some(ov) => ov,
                                            None => {
                                                let mut nv = self.noises[ni].get_uncached(
                                                    bx as f64 * xzs,
                                                    by_lvl as f64 * ys,
                                                    bz as f64 * xzs,
                                                );
                                                if let Some((mc, left)) = mul {
                                                    let (lhs, rhs) = if left { (mc, nv) } else { (nv, mc) };
                                                    nv = lhs * rhs;
                                                }
                                                if let Some((ac, left)) = add {
                                                    let (lhs, rhs) = if left { (ac, nv) } else { (nv, ac) };
                                                    nv = lhs + rhs;
                                                }
                                                nv
                                            }
                                        };
                                        write_corner(cells, dxi, dzi, lvl, v);
                                    }
                                }
                                InterpKind::General(inner) => {
                                    for c in 0..n_corners {
                                        let (dxi, dzi, bx, bz) = corners[c];
                                        memos[c].next_3d();
                                        let v = self.eval_flat(inner, bx, by_lvl, bz, &mut memos[c]);
                                        write_corner(cells, dxi, dzi, lvl, v);
                                    }
                                }
                            }
                        }
                    }
                }
                let y_hi = (min_y + (yi as i32 + 1) * cell_h - 1).min(top);
                let y_lo = min_y + (yi as i32) * cell_h;
                let mut yy = y_hi;
                while yy >= y_lo {
                    if self.block_density(&scratch_cc, x, yy, z) > 0.0 {
                        found = yy;
                        break;
                    }
                    yy -= 1;
                }
                if found != min_y - 1 {
                    break;
                }
            }
            prev_pos = cur_pos;
        }

        self.base_hm_cc = Some(scratch_cc);
        self.base_hm_memos = Some(memos);
        Some(found)
    }

    fn base_col_surf_cached(&mut self, x: i32, z: i32) -> i32 {
        if let Some(&(surf, _)) = self.base_hm_c.get(&(x, z)) {
            return surf;
        }
        let cx = x >> 4;
        let cz = z >> 4;
        if let Some(cc) = self.base_c.get(&(cx, cz)).cloned() {
            return self.single_col_surf(&cc, x, z);
        }
        let share_key = (self.world_seed, self.preset.dim().index() as u8, x, z);
        if let Some(share) = self.round_share() {
            if let Some(&surf) = share
                .base_surf
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&share_key)
            {
                if self.base_hm_c.len() >= 16384 {
                    self.base_hm_c.clear();
                }
                self.base_hm_c.insert((x, z), (surf, i32::MIN));
                return surf;
            }
        }
        let surf = match self.compute_base_col_surf_lazy(x, z) {
            Some(s) => s,
            None => match self.base_cols(cx, cz) {
                Some(cc) => self.single_col_surf(&cc, x, z),
                None => self.min_y - 1,
            },
        };
        if self.base_hm_c.len() >= 16384 {
            self.base_hm_c.clear();
        }
        self.base_hm_c.insert((x, z), (surf, i32::MIN));
        if let Some(share) = self.round_share() {
            let mut m = share.base_surf.lock().unwrap_or_else(|e| e.into_inner());
            if m.len() >= 32768 {
                m.clear();
            }
            m.insert(share_key, surf);
        }
        surf
    }

    fn base_col_wtop_cached(&mut self, x: i32, z: i32) -> i32 {
        if let Some(&(_, wtop)) = self.base_hm_c.get(&(x, z)) {
            if wtop != i32::MIN {
                return wtop;
            }
        }
        let cx = x >> 4;
        let cz = z >> 4;
        if let Some(cc) = self.base_c.get(&(cx, cz)).cloned() {
            let surf = self.single_col_surf(&cc, x, z);
            return self.single_col_wtop(&cc, x, z, surf);
        }
        let surf = self.base_col_surf_cached(x, z);
        let mut wt = surf;
        if surf < self.sea {
            let mut yy = self.sea;
            while yy > surf {
                if self.aq_state(x, yy, z, -1.0) == Some(self.def_fl) {
                    wt = yy;
                    break;
                }
                yy -= 1;
            }
        }
        self.base_hm_c.insert((x, z), (surf, wt));
        wt
    }

    /// WORLD_SURFACE_WG-style height (first non-air + 1) at a column.
    ///
    /// Layer A2-b: while a beardifier resolution is in flight, a structure start queries
    /// the same chunk's heightmap several times (`JigsawPlacement`,
    /// `min_corner_occupied_y`). The old body went through `cols`, which at
    /// `beard_depth > 0` builds all 256 columns (surf, wtop AND the surface materials)
    /// and then refuses to cache them because they are the preliminary answer. Here the
    /// *cached* base column supplies the identical corner grids (both builds have
    /// `beard = None` unless a final column is already in `col_c`, which is checked
    /// first), and only the one asked-for column is scanned.
    pub(crate) fn top_ws(&mut self, x: i32, z: i32) -> i32 {
        let cx = x >> 4;
        let cz = z >> 4;
        if self.beard_depth > 0 {
            // Same first hit as `cols`: a final column already in `col_c` wins, so a
            // bearded answer for a chunk built earlier in this round is preserved.
            if let Some(c) = self.col_c.get(&(cx, cz)) {
                m3_inc_split(M3_COLS_HIT_OWNER, M3_COLS_HIT_WORKER);
                return c.wtop((x & 15) as usize, (z & 15) as usize) + 1;
            }
            return self.base_col_wtop_cached(x, z) + 1;
        }
        match self.cols(cx, cz) {
            Some(c) => c.wtop((x & 15) as usize, (z & 15) as usize) + 1,
            None => self.min_y,
        }
    }
    /// OCEAN_FLOOR_WG-style height (first solid + 1). See [`Gen::top_ws`] for the
    /// `beard_depth > 0` single-column path.
    pub(crate) fn top_oc(&mut self, x: i32, z: i32) -> i32 {
        let cx = x >> 4;
        let cz = z >> 4;
        if self.beard_depth > 0 {
            if let Some(c) = self.col_c.get(&(cx, cz)) {
                m3_inc_split(M3_COLS_HIT_OWNER, M3_COLS_HIT_WORKER);
                return c.surface((x & 15) as usize, (z & 15) as usize) + 1;
            }
            return self.base_col_surf_cached(x, z) + 1;
        }
        match self.cols(cx, cz) {
            Some(c) => c.surface((x & 15) as usize, (z & 15) as usize) + 1,
            None => self.min_y,
        }
    }
    /// biome registry name at a block position (quart-sampled).
    pub(crate) fn biome_name_at(&mut self, x: i32, y: i32, z: i32) -> Option<String> {
        let (_, leaf) = self.biome_quart(x >> 2, y >> 2, z >> 2, None)?;
        self.biome_name_of_leaf(leaf)
    }
    pub(crate) fn biome_name_of_leaf(&self, leaf: u16) -> Option<String> {
        self.biome_names.get(leaf as usize).cloned()
    }
    pub(crate) fn biome_leaf_of_name(&self, name: &str) -> Option<u16> {
        self.bleaf.get(name).copied()
    }
    pub(crate) fn biome_cold_at_leaf(&self, leaf: u16, x: i32, y: i32, z: i32) -> bool {
        self.biome_weather
            .get(leaf as usize)
            .is_some_and(|weather| weather.cold(x, y, z))
    }
    /// Feature/carver override already recorded for this cell, without populating.
    /// Features run after the carver pass and must see those air/fluid writes.
    pub(crate) fn feature_override(&self, x: i32, y: i32, z: i32) -> Option<u32> {
        let k = ((y - self.min_y) as u32) << 8 | (((z & 15) as u32) << 4) | (x & 15) as u32;
        self.feat_c.get(&(x >> 4, z >> 4))?.get(&k).copied()
    }

    /// Every FEATURES-layer write this generator still holds for one chunk, decoded back to
    /// positions and states: `(pos, state)` for each entry of the chunk's bucket, in the
    /// bucket's own order.
    ///
    /// The promotion-boundary light sample (`generation_context::FC::begin_promotion_pass`) is
    /// the one caller. It needs the emitters a chunk can see, and the write layers are the only
    /// place they can be enumerated without a full terrain scan; the cell *reads* during the
    /// flood still go through `FC::get`, so the terrain is not skipped, only not scanned.
    /// A chunk with no bucket yields nothing, which is the right answer for a chunk nothing
    /// wrote to.
    pub(crate) fn feature_bucket_writes(
        &self,
        cx: i32,
        cz: i32,
    ) -> impl Iterator<Item = ((i32, i32, i32), u32)> + '_ {
        let min_y = self.min_y;
        self.feat_c
            .get(&(cx, cz))
            .into_iter()
            .flat_map(move |bucket| {
                bucket.iter().map(move |(&k, &st)| {
                    let y = min_y + (k >> 8) as i32;
                    let z = cz * 16 + ((k >> 4) & 15) as i32;
                    let x = cx * 16 + (k & 15) as i32;
                    ((x, y, z), st)
                })
            })
    }

    /// Read-only view of a resident column. A3-a wave workers use this to answer terrain
    /// reads from the owner's already-built columns without rebuilding them on the owner or
    /// taking a lock; the `Arc` is immutable.
    pub(crate) fn col_c_get(&self, cx: i32, cz: i32) -> Option<std::sync::Arc<ChunkCols>> {
        self.col_c.get(&(cx, cz)).cloned()
    }

    /// This generator's resident columns as a snapshot: `(chunk, Arc)` pairs, for the
    /// owner to install after a wave (`Gen::install_cols`). `Arc` clones only.
    pub(crate) fn col_c_snapshot(&self) -> Vec<((i32, i32), std::sync::Arc<ChunkCols>)> {
        self.col_c.iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// This generator's resident biome unions as a snapshot: `(chunk, Arc)` pairs, for
    /// the owner to merge after a wave (`Gen::merge_chunk_biomes`). `Arc` clones only.
    pub(crate) fn chunk_biomes_snapshot(
        &self,
    ) -> ChunkBiomeEntries {
        self.chunk_biomes.iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// feature-layer lookup: runs the default population dispatch on demand, then
    /// returns the override state for (x,y,z) if a feature wrote one.
    pub(crate) fn feat_state(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        mut populate: impl FnMut(&mut Gen, i32, i32),
    ) -> Option<u32> {
        let (cx, cz) = (x >> 4, z >> 4);
        // Order is an input: vanilla's ticket scheduler is not a coordinate
        // sort. POP_REGION is a legacy lexicographic scan; the default is the
        // named member of vanilla's discipline (`population_discipline`), never
        // a claim of bit-parity with a capture. Offline callers can prepopulate
        // an observed plan through the same execution owner, and a chunk the plan
        // already covers needs no dispatch at all.
        if let Some((px0, pz0, px1, pz1)) = self.pop_region {
            for sx in px0..=px1 {
                for sz in pz0..=pz1 {
                    if (sx, sz) > (cx, cz) {
                        break;
                    }
                    self.populate_in_order([(sx, sz)], &mut populate);
                }
            }
            self.populate_in_order([(cx, cz)], &mut populate);
        } else {
            let nbr = env_str!("MEGU_FEAT_NBR")
                .map(|v| v != "0")
                .unwrap_or(true);
            if !self.pop_done.contains(&(cx, cz)) {
                if nbr {
                    self.populate_request_box(cx, cz, cx, cz, &mut populate);
                } else {
                    self.populate_in_order([(cx, cz)], &mut populate);
                }
            }
        }
        // bound the override maps (LRU over chunks)
        while self.feat_ord.len() > FEATURE_BUCKET_BOUND {
            let old = self.feat_ord.pop_front().unwrap();
            self.forget_feature_bucket(old);
        }
        let k = ((y - self.min_y) as u32) << 8 | (((z & 15) as u32) << 4) | (x & 15) as u32;
        self.feat_c.get(&(cx, cz))?.get(&k).copied()
    }
    /// Drop a chunk's FEATURES-layer bucket: its override map, its `pop_done` mark and
    /// the `feat_ord` slot that lists it. Both bounds go through here, so the queue and
    /// the map cannot disagree (a stale slot would make the next `feat_write` push a
    /// duplicate and grow the queue without a bound).
    ///
    /// A bucket also holds the carver's writes for that chunk, so the carve claim that
    /// stood for them falls here too — otherwise `apply_carvers` returns at `begin_carve`
    /// and the next FEATURES pass reads uncarved stone. `CarveStage::wg` is *not* touched:
    /// it is a scan of the carved terrain with its own 4096-chunk retention.
    fn forget_feature_bucket(&mut self, key: (i32, i32)) {
        self.feat_c.remove(&key);
        self.pop_done.remove(&key);
        if let Some(i) = self.feat_ord.iter().position(|k| *k == key) {
            self.feat_ord.remove(i);
        }
        if let Some(rec) = self.carve_c.get_mut(&key) {
            rec.blocks_live = false;
        }
        if env_flag!("CHUNK_STAGE_TRACE") {
            eprintln!("[stage] evict bucket {key:?}");
        }
    }
    /// The one place a carve record dies. Dropping a WG value also invalidates the
    /// claim and block bucket, so the next generation can re-derive the whole stage.
    /// Unlike feature-bucket eviction, this removes both retained outputs.
    fn evict_carve_record(&mut self, key: (i32, i32)) {
        self.carve_c.remove(&key);
        self.forget_feature_bucket(key);
        if env_flag!("CHUNK_STAGE_TRACE") {
            eprintln!("[stage] evict record {key:?}");
        }
    }
    /// The feature layer's heightmap: for each column, the highest non-air and the
    /// highest solid block a *feature* has written. Vanilla keeps this per chunk and
    /// updates it on every `setBlock`, so a tree placed for one chunk raises the
    /// heightmap a neighbouring chunk's placements read - `heightmap(OCEAN_FLOOR)`
    /// counts canopy leaves, and a later candidate then fails `would_survive` on the
    /// leaf above it. Keeping the tally inside one chunk's placement pass loses that
    /// (measured: vanilla's candidate (0,70,17) against this port's (0,64,17)).
    ///
    /// The caller classifies the state: the block knowledge lives below this layer
    /// and a generation module may not reach for it (see module-layers/policy.json).
    /// The live feature layer's per-column maxima: highest non-air (`solid` 0) and
    /// highest solid (`solid` 1) block a *feature* has written, +1.
    ///
    /// These are scan **bounds**, not heightmap answers. `FC::heightmap_uncached` starts a
    /// column scan here and walks down with the native predicate
    /// (`BReg::heightmap_counts`) because a later writer can remove the cell these maxima
    /// name, and because the marks are max-only: they never record a removal, they miss
    /// every state the predicate rejects (a leaf above the floor), and they know nothing
    /// about carver writes (`FC::feature_override` is the carver's channel, not this
    /// tally). `MEGU_HM_DUMP` used to print `max(top_oc, tally)` and that is exactly
    /// the bug this comment exists to stop: the printed answer was the bound.
    pub(crate) fn note_feature_write(&mut self, x: i32, y: i32, z: i32, solid: bool, air: bool) {
        let e = self.feat_hm.entry((x, z)).or_insert((i32::MIN, i32::MIN));
        if !air {
            e.0 = e.0.max(y + 1);
        }
        if solid {
            e.1 = e.1.max(y + 1);
        }
    }

    /// The distinct biomes of a chunk's sections, cached per chunk: vanilla unions
    /// these over the 3x3 (and intersects with the generator's possible biomes)
    /// before running a feature, so a *neighbour's* river makes `trees_water` run for
    /// this chunk. Sampling a few columns at y=0 instead reads the cave biomes and
    /// silently skipped it.
    pub(crate) fn chunk_biome_set(&mut self, cx: i32, cz: i32) -> std::sync::Arc<Vec<String>> {
        if let Some(v) = self.chunk_biomes.get(&(cx, cz)) {
            return v.clone();
        }
        let mut seen: Vec<String> = vec![];
        let mut seen_mask = 0u128;
        let mut seen_extra: HashSet<u16> = HashSet::new();
        let (qx0, qz0) = (cx * 4, cz * 4);
        // The quart y index is the *block* y divided by four, so the sweep has to start at
        // `min_y / 4` (-16 in the overworld), not at 0. Starting at 0 covered y = 0..383 and
        // silently omitted every biome that lives only below y = 0: the chunk's biome set had
        // no `deep_dark`, so `sculk_patch_deep_dark` and `sculk_vein` never ran for a deep-dark
        // chunk (measured: 0 of the 397 sculk cells vanilla places), and the same hole applies
        // to any cave biome confined to the negative range.
        let qy_min = self.min_y / 4;
        let qy_max = (self.min_y + self.height) / 4;
        for qy in qy_min..qy_max {
            for lx in 0..4 {
                for lz in 0..4 {
                    if let Some((_, leaf)) = self.biome_quart(qx0 + lx, qy, qz0 + lz, None) {
                        let is_new = if leaf < 128 {
                            let bit = 1u128 << leaf;
                            if seen_mask & bit == 0 {
                                seen_mask |= bit;
                                true
                            } else {
                                false
                            }
                        } else {
                            seen_extra.insert(leaf)
                        };
                        if is_new {
                            if let Some(name) = self.biome_names.get(leaf as usize) {
                                seen.push(name.clone());
                            }
                        }
                    }
                }
            }
        }
        let arc = std::sync::Arc::new(seen);
        self.chunk_biomes_ord.push_back((cx, cz));
        while self.chunk_biomes.len() > 384 {
            if let Some(old) = self.chunk_biomes_ord.pop_front() {
                self.chunk_biomes.remove(&old);
            } else {
                break;
            }
        }
        self.chunk_biomes.insert((cx, cz), arc.clone());
        arc
    }

    /// Read-only view of one resident biome-union cache entry: the answer
    /// [`Gen::chunk_biome_set`] would return, without the build, the LRU push or the
    /// mutation. A wave worker (`populate_worker`) takes this first so a chunk whose
    /// owner already built the union of its 3x3 reads that instead of rebuilding the
    /// ~2304 `biome_quart` visits per chunk on the scratch, across waves.
    pub(crate) fn chunk_biome_set_readonly(
        &self,
        cx: i32,
        cz: i32,
    ) -> Option<std::sync::Arc<Vec<String>>> {
        self.chunk_biomes.get(&(cx, cz)).cloned()
    }

    /// Claim before executing each source chunk, exactly once in caller order.
    ///
    /// Layer A3-a keeps this the single execution owner. With more than one pending chunk the
    /// carver stage of the whole demand is pre-carved in one batch, and (for a large enough
    /// window) the feature placement loops run in dependency-DAG waves on worker generators and
    /// are committed here, in caller order. Every diagnostic/trace mode and every small or
    /// explicitly ordered run takes the original serial arm, which is also the reference the
    /// parallel arm is measured against.
    pub(crate) fn populate_in_order(
        &mut self,
        order: impl IntoIterator<Item = (i32, i32)>,
        mut populate: impl FnMut(&mut Gen, i32, i32),
    ) -> usize {
        let list: Vec<(i32, i32)> = order.into_iter().collect();
        // The return value is the number of distinct chunks this call decorated: duplicates
        // in `order` claim once (as in the original loop), whether the serial or the A3-a
        // arm ran them.
        let mut distinct: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
        for &(x, z) in &list {
            if !self.pop_done.contains(&(x, z)) {
                distinct.insert((x, z));
            }
        }
        let executed = distinct.len();
        // A populate pass the tick itself executes is the stall the streamed-
        // generation queue exists to remove; counting it makes the offload
        // measurable instead of asserted.
        if executed > 0 && crate::on_tick_thread() {
            crate::stream_gen::TICK_GENERATED_CHUNKS
                .fetch_add(executed as u64, std::sync::atomic::Ordering::Relaxed);
        }
        let jobs = terrain_jobs();
        // A generic `FnMut` callback cannot be analysed or sent to a worker. The one driver
        // this layer is allowed to split is `feature::populate`; every other caller (unit
        // fixtures counting callbacks, `POP_REGION` replay) stays on the original serial arm.
        // The type name is a dispatch gate only: a false negative costs a optimisation, never
        // correctness, and a false positive is ruled out by the explicit call sites.
        let is_feature_driver = std::any::type_name_of_val(&populate).contains("feature::populate");
        // A3-a scratch generators copy `owner.starts_round`. With parallel terrain jobs but no
        // shared round cache (the delivery default: `terrain_jobs()` is 8 while
        // `delivery_prefetch_jobs()` is 1), every worker rebuilds structure starts and terrain
        // answers on its own thread-local cache; E4-12 measured ~58-63 s for the first 81-chunk
        // window. Only dispatch in parallel when a round is open. Batch worldgen / `perf_bench`
        // open one for `--jobs 5/8`, so those stay parallel; the round-less delivery path runs
        // the serial arm and keeps its tick-latency contract.
        let gate = jobs > 1
            && self.starts_round.is_some()
            && self.pop_region.is_none()
            && is_feature_driver;

        if gate && list.len() >= 2 {
            let mut pending: Vec<(i32, i32)> = Vec::with_capacity(list.len());
            let mut seen = std::collections::HashSet::new();
            for &(x, z) in &list {
                if !self.pop_done.contains(&(x, z)) && seen.insert((x, z)) {
                    pending.push((x, z));
                }
            }
            if pending.len() >= 2 {
                // `feature::populate` (the driver this call site was given) takes this batch
                // on its first invocation and runs the pre-carve / wave schedule itself.
                self.a3a_dispatch = Some(pending);
            }
        }

        for (x, z) in list {
            if self.pop_done.insert((x, z)) {
                self.suppress_generated_ticks = self.restored_tick_sources.contains(&(x, z));
                populate(self, x, z);
                self.suppress_generated_ticks = false;
            }
        }
        executed
    }

    /// Execute a recorded [`PopulationPlan`]: install its window, then run its order.
    ///
    /// The window is what makes the plan's boundary readable: see
    /// [`Gen::promotion_window_ready`].
    pub(crate) fn populate_plan(
        &mut self,
        plan: PopulationPlan,
        populate: impl FnMut(&mut Gen, i32, i32),
    ) -> usize {
        self.pop_plan_window = Some(plan.window);
        self.populate_in_order(plan.order, populate)
    }

    /// `ChunkAccess.markPosForPostprocessing`, filed under the chunk that owns the cell
    /// and kept in mark order.
    pub(crate) fn store_shape_mark(&mut self, x: i32, y: i32, z: i32) {
        match self.shape_marks.entry((x >> 4, z >> 4)) {
            std::collections::hash_map::Entry::Occupied(e) => {
                e.into_mut().push((x, y, z));
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(self.mark_pool.pop().unwrap_or_default())
                    .push((x, y, z));
            }
        }
    }

    /// Hand a consumed mark bucket back for [`Gen::store_shape_mark`] to reuse. The
    /// caller has already drained it; a bucket that grew past the idle bound is
    /// dropped rather than parked (the pool itself stays tiny).
    pub(crate) fn recycle_mark_bucket(&mut self, mut bucket: Vec<(i32, i32, i32)>) {
        bucket.clear();
        if bucket.capacity() > (256 * 1024) / std::mem::size_of::<(i32, i32, i32)>() {
            bucket = Vec::new();
        }
        if self.mark_pool.len() < 64 {
            self.mark_pool.push(bucket);
        }
    }

    /// [`Gen::store_shape_mark`] with the writer recorded for `MARK_TRACE`.
    pub(crate) fn store_shape_mark_from(&mut self, x: i32, y: i32, z: i32, src: MarkSrc) {
        if crate::trace_env::env_str!("MARK_TRACE").is_some() {
            self.mark_src.insert((x, y, z), src);
        }
        self.store_shape_mark(x, y, z);
    }

    /// The writer that filed a mark, for `MARK_TRACE`.
    pub(crate) fn mark_source_of(&self, x: i32, y: i32, z: i32) -> &'static str {
        self.mark_src
            .get(&(x, y, z))
            .map(|s| s.name())
            .unwrap_or("unknown")
    }

    /// Record which writer filed a mark whose position a caller already pushed (the
    /// `WorldGenRegion.setBlock` path files through `FC::shape_marks` first).
    pub(crate) fn note_mark_source(&mut self, x: i32, y: i32, z: i32, src: MarkSrc) {
        if crate::trace_env::env_str!("MARK_TRACE").is_some() {
            self.mark_src.insert((x, y, z), src);
        }
    }

    /// `ChunkMap`'s promotion gate, over the chunks this model has: a chunk's
    /// `postProcessGeneration` runs only once the whole 3x3 around it is at
    /// `ChunkStatus.FULL`.
    ///
    /// The rule from the pinned jar: `ChunkMap` (`aqb`) promotes a chunk on the load path,
    /// `getChunkRangeFuture(pos, 1, $ -> ChunkStatus.FULL)` followed by
    /// `LevelChunk.postProcessGeneration` (`dvi.H`, the jar's only call site of it), so the
    /// chunk is promoted after its whole 3x3 has reached `FULL`. `FULL`'s accumulated
    /// dependency array (`ChunkPyramid.GENERATION_PYRAMID`, `dvy.a`) works out to
    /// `[light@0, initialize_light@1, carvers@2, biomes@3, structure_starts@4..10]`: a chunk
    /// needs *its own* `FEATURES` (through `light@0`) but only *earlier* stages of its
    /// neighbours. The decorated region is therefore the request plus three rings
    /// (`POP_DECORATION_LEVEL`) and the `FULL` region the request plus two, which is why a
    /// promotion window never leaves the decorated region.
    ///
    /// `pop_done` is the port's `FULL` table for the chunks it models. A 3x3 neighbour
    /// outside the plan window is *not* a chunk without a `FEATURES` pass — the recorded plan
    /// is a narrowed input, and native's decorated region contains every promotion window —
    /// so it counts as `FULL`. Inside the window the answer is `pop_done`, which keeps the
    /// consumption time native's: a mark filed after the promotion is never applied.
    pub(crate) fn promotion_window_ready(&self, cx: i32, cz: i32) -> bool {
        let in_window = |c: (i32, i32)| match self.pop_plan_window {
            Some((x0, z0, x1, z1)) => x0 <= c.0 && c.0 <= x1 && z0 <= c.1 && c.1 <= z1,
            None => true,
        };
        (-1..=1).all(|dx| {
            (-1..=1).all(|dz| {
                let c = (cx + dx, cz + dz);
                !in_window(c) || self.pop_done.contains(&c)
            })
        })
    }

    /// Whether `(cx, cz)` is in the `FULL` demand region: the plan window plus one ring,
    /// `(n+4)²` for the plan's `(n+2)²` window.
    ///
    /// The window is the request plus one ring and native's `FULL` region is the request plus
    /// two, so the plan window expanded once is exactly the region native promoted (a 3x3
    /// dump at `(4,-32)` populates `(3..7) x (-33..-29)` and its capture holds 49
    /// `minecraft:full` chunks in `(2..8) x (-34..-28)`). Without a plan there is no such
    /// region to state and every candidate stands: the default discipline decorates native's
    /// whole decorated region, so `pop_done` alone answers the question.
    pub(crate) fn promotion_region_contains(&self, cx: i32, cz: i32) -> bool {
        match self.pop_plan_window {
            Some((x0, z0, x1, z1)) => {
                x0 - 1 <= cx && cx <= x1 + 1 && z0 - 1 <= cz && cz <= z1 + 1
            }
            None => true,
        }
    }

    /// The marks of a promoted chunk, in mark order. Consumed once.
    pub(crate) fn take_shape_marks(&mut self, cx: i32, cz: i32) -> Vec<(i32, i32, i32)> {
        self.shape_marks.remove(&(cx, cz)).unwrap_or_default()
    }

    /// Drain every shape-mark bucket a wave worker filed directly on its generator (the
    /// `markAboveForPostProcessing` and piece-writer paths file there rather than in `FC`).
    /// A target bucket is written by at most one worker of a wave (the wave spacing keeps the
    /// 3x3 write regions disjoint), so appending per bucket in the worker's own mark order is
    /// the serial order.
    pub(crate) fn take_all_shape_marks(
        &mut self,
    ) -> ShapeMarkBuckets {
        std::mem::take(&mut self.shape_marks).into_iter().collect()
    }

    /// Land a worker's direct shape marks on the owner, keeping each bucket's mark order.
    pub(crate) fn merge_shape_marks(
        &mut self,
        marks: ShapeMarkBuckets,
    ) {
        for (chunk, bucket) in marks {
            self.shape_marks.entry(chunk).or_default().extend(bucket);
        }
    }

    /// Merge a wave worker's feature-height maxima into the owner's. Max is associative, so
    /// the order in which disjoint wave windows are merged cannot change the bound.
    pub(crate) fn merge_feat_hm(
        &mut self,
        entries: HashMap<(i32, i32), (i32, i32)>,
    ) {
        for (key, (non_air, solid)) in entries {
            let e = self.feat_hm.entry(key).or_insert((i32::MIN, i32::MIN));
            e.0 = e.0.max(non_air);
            e.1 = e.1.max(solid);
        }
    }

    /// Whether this owner suppresses generated ticks for a restored chunk; a wave worker
    /// inherits the flag before it places anything.
    pub(crate) fn restores_ticks_for(&self, chunk: (i32, i32)) -> bool {
        self.restored_tick_sources.contains(&chunk)
    }

    /// Take a wave worker's feature-height maxima for the owner to merge at commit.
    pub(crate) fn feat_hm_for_worker(&mut self) -> HashMap<(i32, i32), (i32, i32)> {
        std::mem::take(&mut self.feat_hm)
    }

    /// Execute the default population order for a request box: the named member of vanilla's
    /// order family documented on [`population_discipline`], over the box shape `MEGU_POP_REGION`
    /// already uses. `.0` is the number of chunks this call decorated.
    pub(crate) fn populate_request_box(
        &mut self,
        x0: i32,
        z0: i32,
        x1: i32,
        z1: i32,
        populate: impl FnMut(&mut Gen, i32, i32),
    ) -> usize {
        let order = population_discipline_order(&request_box(x0, z0, x1, z1));
        self.populate_in_order(order, populate)
    }

    /// Populate the request a diagnostic makes, so it reads the feature-layer state a generation
    /// loop would hold for that chunk. The driver is passed in, as at every other call site here:
    /// the choice of placement implementation belongs to the caller's boundary, not to worldgen.
    ///
    /// A chunk that is already decorated needs nothing: everything that can write into its cells
    /// either decorated them or was decorated itself when the demand that reached it was served
    /// (a writer sits within `POP_WRITE_RADIUS` of it, so within the same three rings).
    pub(crate) fn ensure_populated(
        &mut self,
        cx: i32,
        cz: i32,
        mut populate: impl FnMut(&mut Gen, i32, i32),
    ) {
        if self.pop_done.contains(&(cx, cz)) {
            return;
        }
        self.populate_request_box(cx, cz, cx, cz, &mut populate);
    }

    /// Highest non-air (`0`) / solid (`1`) feature-written block + 1 in a column.
    pub(crate) fn feature_height(&self, x: i32, z: i32, solid: usize) -> i32 {
        match self.feat_hm.get(&(x, z)) {
            Some(e) => {
                if solid == 0 {
                    e.0
                } else {
                    e.1
                }
            }
            None => i32::MIN,
        }
    }

    /// Whether a feature write may land in the chunk that owns `(x, z)`.
    ///
    /// This models `WorldGenRegion.ensureCanWrite` (`arb.f_`), which rejects a chunk
    /// further than the FEATURES step's `blockStateWriteRadius` from the decorated
    /// chunk: the 1.21.1 `ChunkPyramid` (`dvy`) builds that step as
    /// `features -> carvers@1 + struct_starts@8 + .a(1)`, and `arb.f_` compares the
    /// chunk offset with that radius (Chebyshev, in chunks). Writes outside it are
    /// dropped with only a log line, so the feature sees its own cell unchanged.
    ///
    /// The other vanilla refusal is *not* modelled, deliberately. Once a chunk is
    /// promoted to a `LevelChunk`, `ChunkMap` installs
    /// `new ImposterProtoChunk(levelChunk, false)` into every proto-status slot and
    /// `ImposterProtoChunk.setBlockState` (`dvh.a(jd,dtc,Z)`) then returns `null`
    /// without touching the chunk's blocks or heightmaps - while `arb.a(...)` still
    /// returns true, which is why a `FCELL ... wrote=true` trace line proves nothing
    /// about persistence (see `tools/carve-agent/README.md`). That flag flips at
    /// promotion, not when the chunk's own FEATURES pass returns: measured on the
    /// pinned Nether fixture (`/tmp/nat-4242-nether-fossil/world`, grid (-25,19)+3x3
    /// with the recorded pop order), treating "its pass finished" as read-only moved
    /// 34 diffs to 1183, because native accepted a later pass' writes into an
    /// already-decorated neighbour that was still a writable proto chunk (blackstone
    /// at (-385,10,304) from pop (-24,19)). The offline generator has no promotion
    /// stage, so modeling that half would refuse writes vanilla kept.
    pub(crate) fn feature_write_allowed(&self, x: i32, z: i32) -> bool {
        let Some((px, pz)) = self.pop_active else {
            // Not inside a FEATURES pass (unit tests, template post-processing):
            // there is no region and therefore no window to enforce.
            return true;
        };
        let (cx, cz) = (x >> 4, z >> 4);
        (cx - px).abs() <= 1 && (cz - pz).abs() <= 1
    }

    pub(crate) fn feat_write(&mut self, x: i32, y: i32, z: i32, st: u32) {
        // The FEATURES-layer storage gate is the **world** window: 1.21.1's End City
        // builds above the End's generator top (measured: 273 cells at y 128..143 in
        // chunk (65,45) of the pinned capture), and vanilla's
        // `WorldGenRegion.ensureCanWrite` bound is the level's build height.
        if y < self.world_min_y || y >= self.world_min_y + self.world_height {
            return;
        }
        let key = (x >> 4, z >> 4);
        let k = ((y - self.min_y) as u32) << 8 | (((z & 15) as u32) << 4) | (x & 15) as u32;
        if !self.feat_c.contains_key(&key) {
            self.feat_ord.push_back(key);
        }
        self.feat_c.entry(key).or_default().insert(k, st);
    }
    pub(crate) fn stage_payloads(
        &mut self,
        source: (i32, i32),
        items: &mut Vec<((i32, i32, i32), crate::generation_payload::GenerationPayload)>,
    ) {
        if self.payload_sources.insert(source) {
            self.staged_payloads.append(items);
        } else {
            // Recomputing evicted blocks must not reset NBT or recreate entities.
            items.clear();
        }
    }

    /// Hand the staged contents to the caller. Taken by value so a second call cannot
    /// double-apply them.
    pub(crate) fn take_staged_payloads(
        &mut self,
    ) -> Vec<((i32, i32, i32), crate::generation_payload::GenerationPayload)> {
        std::mem::take(&mut self.staged_payloads)
    }

    pub(crate) fn wseed(&self) -> u64 {
        self.world_seed
    }

    /// Claim the one carver pass for a chunk. Vanilla writes CARVERS before
    /// FEATURES for every chunk in the decoration area; claiming here lets the
    /// feature context pre-carve its neighbours without carving them twice.
    pub(crate) fn begin_carve(&mut self, cx: i32, cz: i32) -> bool {
        let key = (cx, cz);
        if let Some(rec) = self.carve_c.get_mut(&key) {
            if rec.blocks_live {
                return false;
            }
            // The bucket that held this pass' writes is gone (`forget_feature_bucket`),
            // so re-running is the only way to get them back. `wg` stays: it is a scan of
            // the carved terrain, not a read of `feat_c`, and the re-run reproduces it.
            if env_flag!("CHUNK_STAGE_TRACE") {
                eprintln!("[stage] reclaim carve {cx},{cz} (block output evicted)");
            }
            rec.blocks_live = true;
            return true;
        }
        self.carve_c.insert(
            key,
            CarveStage {
                wg: None,
                blocks_live: true,
            },
        );
        true
    }

    /// Record a chunk's WG heightmaps as the carver pass leaves them (see
    /// `CarveStage::wg` for why the carver, and only the carver, moves them).
    pub(crate) fn set_wg_after_carve(&mut self, cx: i32, cz: i32, tops: Box<[i32; 512]>) {
        let key = (cx, cz);
        // A caller that hands over a carved scan without claiming the pass first (the
        // `wg_heightmap_keeps_the_carved_top` fixture) records the value and no block
        // output: the same state `forget_feature_bucket` leaves behind, so the next
        // `begin_carve` claims and re-runs instead of suppressing the pass.
        let had_wg = {
            let rec = self.carve_c.entry(key).or_insert(CarveStage {
                wg: None,
                blocks_live: false,
            });
            rec.wg.replace(tops).is_some()
        };
        if !had_wg {
            self.carve_ord.push_back(key);
        }
        // Decoration reaches the 17x17 region plus the structure reach, so a value must
        // outlive the pass that wrote it; `CARVE_WG_BOUND` chunks is the approved bound
        // and it is *this* record's bound — not the 384-chunk `feat_c` one. Dropping the
        // record drops its block output with it (`evict_carve_record`), so the chunk
        // returns to the not-yet-carved stage and the next generation re-derives both;
        // an evicted value never leaves a claim behind that could not recover it.
        while self.carve_ord.len() > CARVE_WG_BOUND {
            let Some(old) = self.carve_ord.pop_front() else {
                break;
            };
            if old == key {
                // The record being written is the oldest one: keep it, evict the next.
                self.carve_ord.push_back(old);
                continue;
            }
            self.evict_carve_record(old);
        }
    }

    /// The stored post-carve WG height for a column, first-available form
    /// (`topSolid + 1`), or `None` when this generator holds no post-carve value for that
    /// chunk. `None` means no WG value is retained; a newly claimed pass also has no
    /// value until `set_wg_after_carve` hands over its scan. Evicting a completed WG
    /// value invalidates the claim too, allowing the next generation to re-derive it.
    pub(crate) fn wg_after_carve_height(&self, ocean_floor: bool, x: i32, z: i32) -> Option<i32> {
        let tops = self.carve_c.get(&(x >> 4, z >> 4))?.wg.as_ref()?;
        let base = if ocean_floor { 256 } else { 0 };
        Some(tops[base + (z & 15) as usize * 16 + (x & 15) as usize])
    }

    /// feature overrides for a whole chunk (populate flushes here)
    #[allow(dead_code)]
    pub(crate) fn feat_map(&self, cx: i32, cz: i32) -> Option<&HashMap<u32, u32>> {
        self.feat_c.get(&(cx, cz))
    }
}

impl ChunkCols {
    pub fn surface(&self, lx: usize, lz: usize) -> i32 {
        self.surf[lz * 16 + lx] as i32
    }
    /// topmost solid-or-fluid y (WORLD_SURFACE_WG / OCEAN_FLOOR_WG basis)
    pub fn wtop(&self, lx: usize, lz: usize) -> i32 {
        self.wtop[lz * 16 + lx] as i32
    }
    /// The surface materials, built on first material read and then shared. Heightmap,
    /// `surf`/`wtop`, `grids` and `beard` readers never enter `get_or_init`, so neighbour
    /// chunks that are only height sources stay material-free (Layer A2-b).
    pub(crate) fn surface_mats(&self, g: &mut Gen) -> &SurfaceMats {
        self.mats.get_or_compute(g, self)
    }
    /// Diagnostic used by the lazy-material unit test.
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn surface_mats_computed(&self) -> bool {
        self.mats.cell.get().is_some()
    }
    /// material-rule result inside the surface window.
    /// Some(u32::MAX) = scanned but no rule matched -> default block.
    /// None = outside the scanned window -> caller runs deep_state.
    pub fn mat(&self, g: &mut Gen, lx: usize, lz: usize, y: i32) -> Option<u32> {
        mat_in(self.surface_mats(g), lx, lz, y)
    }
    /// Per-column `run_depth`, used by the deep material rule.
    pub(crate) fn run_depth(&self, g: &mut Gen, lx: usize, lz: usize) -> i16 {
        self.surface_mats(g).rd[lz * 16 + lx]
    }
    /// Trilinear value of this chunk's grid for `interpolated` node `id`, or
    /// `None` when that node has no grid (not reachable from final_density).
    #[inline]
    fn interp(&self, id: usize, x: i32, y: i32, z: i32) -> Option<f64> {
        self.interp_slot(id, x, y, z)
    }

    /// The same lookup addressed by the grid slot the compiled `Op::Interp`
    /// carries (a slot is a density-function id; see `interp_ids`), so the
    /// compiled and interpreted paths share one `trilinear` call.
    fn interp_slot(&self, slot: usize, x: i32, y: i32, z: i32) -> Option<f64> {
        let cells = self.grids.get(slot)?.as_deref()?;
        Some(trilinear(cells, self, x, y, z))
    }
}

/// The surface-material array lookup [`ChunkCols::mat`] performs once the mats
/// cell is resolved, exposed separately so the fill loop can hoist the
/// `surface_mats` resolution out of its 4096-per-section calls.
/// `Some(u32::MAX)` = scanned but no rule matched -> default block;
/// `None` = outside the scanned window -> caller runs deep_state.
#[inline]
fn mat_in(mats: &SurfaceMats, lx: usize, lz: usize, y: i32) -> Option<u32> {
    let m = mats.mats[lz * 16 + lx].as_ref()?;
    let i = y - m.lo as i32;
    if i < 0 || i as usize >= m.vals.len() {
        return None;
    }
    let v = m.vals[i as usize];
    Some(if v == u16::MAX { u32::MAX } else { v as u32 })
}

#[inline]
fn trilinear(cells: &[f64], columns: &ChunkCols, x: i32, y: i32, z: i32) -> f64 {
    let (ny, min_y) = (columns.ny, columns.min_y);
    let nx = (16 / columns.cell_w + 1) as usize;
    let (width, height) = (f64::from(columns.cell_w), f64::from(columns.cell_h));
    let lx = (x & 15) as f64;
    let lz = (z & 15) as f64;
    let ly = (y - min_y) as f64;
    let xi = (lx / width).floor().min((nx - 2) as f64) as usize;
    let zi = (lz / width).floor().min((nx - 2) as f64) as usize;
    let yi = (ly / height).floor().min((ny - 2) as f64) as usize;
    let fx = lx / width - xi as f64;
    let fz = lz / width - zi as f64;
    let fy = ly / height - yi as f64;
    let g = |xi: usize, yi: usize, zi: usize| cells[(zi * nx + xi) * ny + yi];
    let c000 = g(xi, yi, zi);
    let c100 = g(xi + 1, yi, zi);
    let c010 = g(xi, yi + 1, zi);
    let c110 = g(xi + 1, yi + 1, zi);
    let c001 = g(xi, yi, zi + 1);
    let c101 = g(xi + 1, yi, zi + 1);
    let c011 = g(xi, yi + 1, zi + 1);
    let c111 = g(xi + 1, yi + 1, zi + 1);
    lerp3(fx, fy, fz, c000, c100, c010, c110, c001, c101, c011, c111)
}

// public API -------------------------------------------------------------

/// Density at a block position (trilerped from the per-chunk cell grid).
pub fn density_at(x: i32, y: i32, z: i32) -> Option<f64> {
    let cc = chunk_cols(x >> 4, z >> 4)?;
    Some(density_in(&cc, x, y, z))
}

/// Block density for a column already fetched with `chunk_cols`.
pub fn density_in(cc: &ChunkCols, x: i32, y: i32, z: i32) -> f64 {
    match gen_lock().as_ref() {
        Some(g) => g.block_density(cc, x, y, z),
        None => 0.0,
    }
}

pub fn debug_eval(name: &str, x: i32, y: i32, z: i32) -> Option<f64> {
    let mut g = gen_lock();
    let g = g.as_mut()?;
    let mut m = HashMap::new();
    if let Some(nn) = name.strip_prefix("noise:") {
        let ni = g.noise_id(&format!("minecraft:{}", nn))?;
        return Some(g.noises[ni].get(x as f64, y as f64, z as f64));
    }
    let files = Gen::df_files();
    let id = g.resolve_ref(&files, name)?;
    Some(g.eval(id, x, y, z, &mut m))
}

/// Diagnostics: `WG_COUNT=1 MEGU_DFSTATS=1` prints where the density-node
/// visits went (per kind and per node), so an evaluator change can be attributed.
pub fn df_stats() -> String {
    let hits = match DF_HITS.get() {
        Some(h) => h,
        None => return String::new(),
    };
    let g = gen_lock();
    let g = match g.as_ref() {
        Some(g) => g,
        None => return String::new(),
    };
    let mut per_kind: HashMap<&'static str, u64> = HashMap::new();
    let mut rows: Vec<(u64, usize, &'static str)> = Vec::new();
    let mut total = 0u64;
    for (id, c) in hits.iter().enumerate() {
        let n = c.load(std::sync::atomic::Ordering::Relaxed);
        if n == 0 {
            continue;
        }
        total += n;
        let k = df_kind(&g.dfs[id]);
        *per_kind.entry(k).or_insert(0) += n;
        rows.push((n, id, k));
    }
    rows.sort_unstable_by_key(|r| std::cmp::Reverse(r.0));
    let mut out = String::new();
    out.push_str(&format!(
        "[dfstats] nodes={} reachable_visited={} visits={total}\n",
        g.dfs.len(),
        rows.len()
    ));
    let mut kinds: Vec<_> = per_kind.into_iter().collect();
    kinds.sort_unstable_by_key(|k| std::cmp::Reverse(k.1));
    for (k, n) in kinds {
        out.push_str(&format!(
            "[dfstats] kind {k:<20} {n:>12} ({:.1}%)\n",
            100.0 * n as f64 / total.max(1) as f64
        ));
    }
    for (n, id, k) in rows.iter().take(24) {
        out.push_str(&format!(
            "[dfstats]   id={id:<6} {k:<20} {n:>12} ({:.1}%) {}\n",
            100.0 * *n as f64 / total.max(1) as f64,
            df_detail(&g.dfs[*id])
        ));
    }
    out
}

fn df_kind(d: &DF) -> &'static str {
    match d {
        DF::C(_) => "constant",
        DF::EndIslands(_) => "end_islands",
        DF::Zero => "zero",
        DF::Beardifier => "beardifier",
        DF::Add(..) => "add",
        DF::Sub(..) => "sub",
        DF::Mul(..) => "mul",
        DF::Min(..) => "min",
        DF::Max(..) => "max",
        DF::Div(..) => "div",
        DF::Neg(_) => "neg",
        DF::Abs(_) => "abs",
        DF::Sq(_) => "square",
        DF::Cube(_) => "cube",
        DF::HNeg(_) => "half_negative",
        DF::QNeg(_) => "quarter_negative",
        DF::Squeeze(_) => "squeeze",
        DF::Clamp { .. } => "clamp",
        DF::Lerp { .. } => "lerp",
        DF::Grad { .. } => "y_gradient",
        DF::Cache(_) => "cache",
        DF::Interp(_) => "interpolated",
        DF::Noise { .. } => "noise",
        DF::ShiftA(_) => "shift_a",
        DF::ShiftB(_) => "shift_b",
        DF::WeirdScaledSampler { .. } => "weird_sampler",
        DF::RangeChoice { .. } => "range_choice",
        DF::IntervalSelect { .. } => "interval_select",
        DF::Spline(_) => "spline",
        DF::Blended(_) => "old_blended_noise",
        DF::BlendD(_) => "blend_density",
        DF::FindTopSurface { .. } => "find_top_surface",
    }
}

fn df_detail(d: &DF) -> String {
    match d {
        DF::C(v) => format!("{v}"),
        DF::Noise { ni, xzs, ys, .. } => format!("ni={ni} xzs={xzs} ys={ys}"),
        DF::Spline(s) => format!("coord={} pts={}", s.coord, s.pts.len()),
        DF::Cache(a) | DF::Interp(a) | DF::BlendD(a) => format!("arg={a}"),
        DF::Add(a, b) | DF::Sub(a, b) | DF::Mul(a, b) | DF::Min(a, b) | DF::Max(a, b) | DF::Div(a, b) => {
            format!("{a},{b}")
        }
        DF::Clamp { i, lo, hi } => format!("{i} [{lo},{hi}]"),
        DF::Grad { fc, fv, tc, tv } => format!("y {fc}->{tc} {fv}->{tv}"),
        DF::RangeChoice { i, mi, ma, wr, wo } => format!("{i} [{mi},{ma}) wr={wr} wo={wo}"),
        _ => String::new(),
    }
}

pub fn sea_level() -> i32 {
    facts().map(|f| f.sea).unwrap_or(63)
}

/// material-rule result for blocks below the surface window (bedrock,
/// ore veins, deepslate gradient, underground fallback -> default block).
/// stone_depth/fluid context is pinned "deep" — vanilla underground rules
/// do not read them.
#[cfg(test)]
pub fn deep_state(x: i32, y: i32, z: i32, cc: &ChunkCols) -> u32 {
    let mut gg = gen_lock();
    match gg.as_mut() {
        Some(g) => g.deep_state_cached(x, y, z, cc),
        None => 1,
    }
}

/// `MEGU_BEDROCK=flat` replaces the per-cell vanilla bedrock gradient with a solid
/// five-block floor. It is a deliberate parity/cheapness option for players who prefer the
/// flat floor; measured at ~8us a chunk (about 0.14% of the terrain layer), so the vanilla
/// per-cell pattern stays the default.
pub fn flat_bedrock() -> bool {
    static FLAT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAT.get_or_init(|| std::env::var("MEGU_BEDROCK").ok().as_deref() == Some("flat"))
}
impl Gen {
    /// in-lock deep rule evaluation (feature layer / tests use this; the free
    /// `deep_state` wrapper locks the generator).
    /// The material rule below the surface window. Running the rule is the most expensive
    /// terrain work the feature pass does, and the same cell is read again by the send pass
    /// (`wg2_block`) immediately after, so the result is memoized here rather than recomputed.
    #[inline]
    fn deep_state_fast(&self, x: i32, y: i32, z: i32) -> Option<u32> {
        let steps = self.deep_fast.as_deref()?;
        if flat_bedrock() && y >= self.min_y && y < self.min_y + 5 {
            return Some(self.bedrock_st);
        }
        for step in steps {
            match *step {
                DeepStep::Block(st) => return Some(st),
                DeepStep::VGrad {
                    ta,
                    fa,
                    legacy_seed,
                    sp,
                    st,
                } => {
                    if y <= ta {
                        return Some(st);
                    }
                    if y < fa {
                        let m =
                            (y as f32 - ta as f32) / (fa as f32 - ta as f32) * (0.0 - 1.0) + 1.0;
                        let hit = if self.legacy_random_source {
                            Lcg::new(legacy_seed ^ hash_block_pos(x, y, z) as u64).next_f32() < m
                        } else {
                            sp.pos(x, y, z).next_f32() < m
                        };
                        if hit {
                            return Some(st);
                        }
                    }
                }
            }
        }
        Some(self.def_st)
    }

    pub(crate) fn deep_state_cached(&mut self, x: i32, y: i32, z: i32, cc: &ChunkCols) -> u32 {
        if let Some(v) = self.deep_state_fast(x, y, z) {
            return v;
        }
        // M3 P1a M3: the deep rule is the most expensive terrain work a feature read does,
        // and its memo lives on whichever generator ran the terrain pass. A cold memo here is
        // the concrete form of "the feature layer reads terrain the terrain pass used to warm".
        let m3_read = m3_on() && m3_in_feature_read();
        if let Some(&v) = self.deep_c.get(&(x, y, z)) {
            inc_if(m3_read, M3_DEEP_HIT);
            return v;
        }
        inc_if(m3_read, M3_DEEP_MISS);
        let v = self.deep_state_i(x, y, z, cc);
        self.deep_c.insert((x, y, z), v);
        self.deep_ord.push_back((x, y, z));
        while self.deep_ord.len() > 500_000 {
            if let Some(old) = self.deep_ord.pop_front() {
                self.deep_c.remove(&old);
            }
        }
        v
    }
    /// Water or lava, as the aquifer decided it for this cell.
    pub(crate) fn is_fluid_state(&self, st: u32) -> bool {
        if st == self.def_fl {
            return true;
        }
        self.aq.as_ref().is_some_and(|a| st == a.lava_st)
    }

    /// Vanilla `WorldCarver.getCarveState`: at or below the lava level the cell
    /// becomes lava, otherwise the aquifer's substance at density 0. `None` is
    /// the aquifer's barrier (the material rule keeps the cell solid) and means
    /// "do not carve here".
    pub(crate) fn carve_state(&mut self, x: i32, y: i32, z: i32, lava_level: i32) -> Option<u32> {
        if y <= lava_level {
            return Some(self.aq.as_ref().map(|a| a.lava_st).unwrap_or(0));
        }
        self.aq_state(x, y, z, 0.0)
    }

    /// Vanilla `CarvingContext.topMaterial` / `SurfaceSystem.buildSurface` for a
    /// single position: the surface rule evaluated with one block of stone above
    /// and below, and the fluid height the carved state implies. A carver calls
    /// this to restore the top material under a cave mouth it opened.
    pub(crate) fn top_material_at(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        is_fluid: bool,
    ) -> Option<u32> {
        let root = self.surface_rule?;
        // Vanilla's `topMaterial` never asks the aquifer, but this `cols` lookup can build
        // columns on a cold generator — or answer from a share on one that did not — and the
        // `aq_sched` flag it would leave is this generator's own history leaking into the
        // carve's mark gate. Keep the flag across the lookup either way.
        let sched = self.aq_sched;
        let cc = self.cols(x >> 4, z >> 4);
        self.aq_sched = sched;
        let cc = cc?;
        let mut c = SC::new();
        c.x = x;
        c.z = z;
        c.y = y;
        c.run_depth = self.run_depth(x, z);
        c.est_min = self.prelim_min(x, z);
        c.sda = 1;
        c.sdb = 1;
        c.fluid = if is_fluid { y + 1 } else { i32::MIN };
        self.eval_rule(root, &mut c, &cc.wtop)
    }

    pub(crate) fn deep_state_i(&mut self, x: i32, y: i32, z: i32, cc: &ChunkCols) -> u32 {
        if let Some(v) = self.deep_state_fast(x, y, z) {
            return v;
        }
        // Opt-in flat floor: the vanilla bedrock band is y in [min_y, min_y+4].
        if flat_bedrock() && y >= self.min_y && y < self.min_y + 5 {
            return self.bedrock_st;
        }
        let root = match self.surface_rule {
            Some(r) => r,
            None => return self.def_st,
        };
        let mut c = SC::new();
        c.x = x;
        c.z = z;
        c.y = y;
        c.run_depth = cc.run_depth(self, (x & 15) as usize, (z & 15) as usize) as i32;
        c.est_min = i32::MAX; // above_preliminary_surface -> false
        c.sda = 32000;
        c.sdb = 32000;
        c.fluid = i32::MIN;
        self.eval_rule(root, &mut c, &cc.wtop)
            .unwrap_or(self.def_st)
    }
}
pub fn min_y() -> i32 {
    facts().map(|f| f.min_y).unwrap_or(-64)
}
pub fn max_y() -> i32 {
    facts().map(|f| f.min_y + f.height - 1).unwrap_or(319)
}
/// The **world** window's floor (`dimension_type` `min_y`): the bottom of the chunk a
/// player stands in, which is not necessarily the bottom of the terrain.
///
/// `min_y()`/`max_y()` above are the generator's window and stay that way: the density
/// lattice, the surface scans and the biome quart sweep are defined over it, so moving
/// them would rewrite terrain. Use these for storage and bounds: chunk sections,
/// heightmaps, the offline dump length, feature/structure write bounds and structure
/// placement boxes.
pub fn world_min_y() -> i32 {
    facts().map(|f| f.world_min_y).unwrap_or(-64)
}
pub fn world_height() -> i32 {
    facts().map(|f| f.world_height).unwrap_or(384)
}
pub fn world_max_y() -> i32 {
    world_min_y() + world_height() - 1
}

/// First solid (density>0) y when scanning down from max_y. -min_y-1 if none.
pub fn surface_of(x: i32, z: i32) -> i32 {
    let mut y = max_y();
    while y >= min_y() {
        if density_at(x, y, z).unwrap_or(0.0) > 0.0 {
            return y;
        }
        y -= 1;
    }
    min_y() - 1
}

// ---------- aquifer (vanilla NoiseBasedAquifer port) ----------

const AQ_MIN: i32 = -32512; // vanilla MIN_HEIGHT_CELL (sentinel magnitude affects calc_density)
const AQ_OFF: [(i32, i32); 13] = [
    (0, 0),
    (-2, -1),
    (-1, -1),
    (0, -1),
    (1, -1),
    (-3, 0),
    (-2, 0),
    (-1, 0),
    (1, 0),
    (-2, 1),
    (-1, 1),
    (0, 1),
    (1, 1),
];
#[inline]
fn lxz(v: i32) -> i32 {
    v.div_euclid(16)
}
#[inline]
fn ly12(v: i32) -> i32 {
    v.div_euclid(12)
}

/// Chunk-local aquifer grids kept on a generator. A terrain fill walks one chunk
/// densely while carvers/features probe its neighbours, so a small ring avoids both
/// a single-slot thrash and a global hash lookup on every cell.
const AQ_CHUNK_SLOTS: usize = 16;

/// The whole 12-cell neighbourhood of every block in a chunk fits in a flat grid:
/// `sx = lxz(x-5)` is `cx-1..=cx` and `sz = lxz(z-5)` is `cz-1..=cz`, so the sampled
/// cells span `gx ∈ [cx-1, cx+1]`, `gz ∈ [cz-1, cz+1]`; `sy = ly12(y+1)` spans
/// `ly12(min_y+1)..=ly12(min_y+height)`, so `gy = sy + dy` spans
/// `ly12(min_y+1)-1..=ly12(min_y+height)+1`. That is 3×35×3 cells in the Overworld
/// (~4 KiB of positions, ~3 KiB of levels — both L1-resident).
///
/// `aq_rpos(gx,gy,gz)` returns `(gx*16 + r_x, gy*12 + r_y, gz*16 + r_z)` with
/// `r_x, r_z < 16` and `r_y < 12`, so `rx.div_euclid(16) == gx` (likewise Y/Z):
/// the fluid level of a cell's own sample position belongs at that cell's flat
/// index. Both arrays are therefore indexed by the same cell index, populated
/// lazily: sparse neighbour probes do not pay for all 315 sample positions, while
/// a dense fill pays one bounds check and a direct array index per cell.
struct AqChunkCache {
    cx: i32,
    cz: i32,
    min_gx: i32,
    min_gy: i32,
    min_gz: i32,
    nx: usize,
    ny: usize,
    nz: usize,
    /// `aq_rpos` for each cell, filled on first probe.
    rpos: Vec<Option<(i32, i32, i32)>>,
    /// `aq_fluid_level_at` for the same cell's sample position, filled on first read.
    levels: Vec<Option<(i32, u8)>>,
}

impl AqChunkCache {
    fn new(cx: i32, cz: i32, min_y: i32, height: i32) -> Self {
        let min_gx = lxz(cx * 16 - 5);
        let max_gx = lxz(cx * 16 + 15 - 5) + 1;
        let min_gz = lxz(cz * 16 - 5);
        let max_gz = lxz(cz * 16 + 15 - 5) + 1;
        let min_gy = ly12(min_y + 1) - 1;
        let max_gy = ly12(min_y + height) + 1;
        let nx = (max_gx - min_gx + 1) as usize;
        let ny = (max_gy - min_gy + 1) as usize;
        let nz = (max_gz - min_gz + 1) as usize;
        let n = nx * ny * nz;
        Self {
            cx,
            cz,
            min_gx,
            min_gy,
            min_gz,
            nx,
            ny,
            nz,
            rpos: vec![None; n],
            levels: vec![None; n],
        }
    }

    fn reset(&mut self, cx: i32, cz: i32, min_y: i32, height: i32) {
        let min_gx = lxz(cx * 16 - 5);
        let max_gx = lxz(cx * 16 + 15 - 5) + 1;
        let min_gz = lxz(cz * 16 - 5);
        let max_gz = lxz(cz * 16 + 15 - 5) + 1;
        let min_gy = ly12(min_y + 1) - 1;
        let max_gy = ly12(min_y + height) + 1;
        let nx = (max_gx - min_gx + 1) as usize;
        let ny = (max_gy - min_gy + 1) as usize;
        let nz = (max_gz - min_gz + 1) as usize;
        let n = nx * ny * nz;
        self.cx = cx;
        self.cz = cz;
        self.min_gx = min_gx;
        self.min_gy = min_gy;
        self.min_gz = min_gz;
        self.nx = nx;
        self.ny = ny;
        self.nz = nz;
        self.rpos.clear();
        self.rpos.resize(n, None);
        self.levels.clear();
        self.levels.resize(n, None);
    }

    #[inline]
    fn contains(&self, gx: i32, gy: i32, gz: i32) -> bool {
        gx >= self.min_gx
            && gx < self.min_gx + self.nx as i32
            && gy >= self.min_gy
            && gy < self.min_gy + self.ny as i32
            && gz >= self.min_gz
            && gz < self.min_gz + self.nz as i32
    }

    #[inline]
    fn idx(&self, gx: i32, gy: i32, gz: i32) -> usize {
        let i = (gx - self.min_gx) as usize;
        let j = (gy - self.min_gy) as usize;
        let k = (gz - self.min_gz) as usize;
        ((j * self.nz) + k) * self.nx + i
    }
}

/// The three nearest aquifer nodes of a query block: `(position, squared distance,
/// flat cell index)`. The vanilla selection keeps four slots, but slot 4 is never
/// read: the final `>=` branch only writes it and no caller of `aq_near4` observes
/// it. Keeping three produces the exact same `near[0..3]`.
type AqNear = [((i32, i32, i32), i64, usize); 3];

// fluid level = (max_y exclusive, kind); kind: 1=water, 2=lava
impl Gen {
    #[inline]
    fn aq_pick(&self, y: i32) -> (i32, u8) {
        if y < -54 {
            (-54, 2)
        } else {
            (self.sea, 1)
        }
    }
    #[inline]
    fn lvl_block(max_y: i32, kind: u8, y: i32) -> u8 {
        if y < max_y {
            kind
        } else {
            0
        }
    }
    // per-cell random sample position (16x12x16 grid, minecraft:aquifer deriver)
    /// `AQ_POS="x,y,z"` prints the 12 sample positions for that query block,
    /// for diffing against `tools/vanilla-probe/AquiferPos.java`.
    fn aq_pos_trace(&mut self, x: i32, y: i32, z: i32) {
        if !aq_trace_enabled() {
            return;
        }
        use std::sync::OnceLock;
        static SPEC: OnceLock<Option<String>> = OnceLock::new();
        let Some(spec) = SPEC.get_or_init(|| std::env::var("AQ_POS").ok()) else {
            return;
        };
        if *spec != format!("{x},{y},{z}") {
            return;
        }
        let (sx, sy, sz) = (lxz(x - 5), ly12(y + 1), lxz(z - 5));
        for dx in 0..=1 {
            for dy in -1..=1 {
                for dz in 0..=1 {
                    let (px, py, pz) = self.aq_rpos(sx + dx, sy + dy, sz + dz);
                    eprintln!(
                        "APOS {} {} {} {} {} {}",
                        sx + dx,
                        sy + dy,
                        sz + dz,
                        px,
                        py,
                        pz
                    );
                }
            }
        }
    }
    fn aq_rpos(&self, gx: i32, gy: i32, gz: i32) -> (i32, i32, i32) {
        let mut r = self.aq.as_ref().unwrap().split.pos(gx, gy, gz);
        (
            gx * 16 + r.next_bound(10),
            gy * 12 + r.next_bound(9),
            gz * 16 + r.next_bound(10),
        )
    }
    #[inline]
    fn aq_cache(&self, slot: usize) -> &AqChunkCache {
        self.aq_chunk[slot].as_ref().unwrap()
    }
    #[inline]
    fn aq_cache_mut(&mut self, slot: usize) -> &mut AqChunkCache {
        self.aq_chunk[slot].as_mut().unwrap()
    }
    /// MRU slot holding `(cx, cz)`'s chunk-local grid. The reuse order is a pure
    /// function of the access sequence, so both bytes and counters stay repeatable.
    fn aq_chunk_slot(&mut self, cx: i32, cz: i32) -> usize {
        let tick = self.aq_chunk_tick.wrapping_add(1);
        self.aq_chunk_tick = tick;
        for (i, c) in self.aq_chunk.iter().enumerate() {
            if c.as_ref().is_some_and(|c| c.cx == cx && c.cz == cz) {
                self.aq_chunk_use[i] = tick;
                return i;
            }
        }
        let mut victim = 0;
        for i in 1..AQ_CHUNK_SLOTS {
            if self.aq_chunk[i].is_none() {
                victim = i;
                break;
            }
            if self.aq_chunk_use[i] < self.aq_chunk_use[victim] {
                victim = i;
            }
        }
        if self.aq_cell12.valid && self.aq_cell12.slot == victim {
            self.aq_cell12.valid = false;
        }
        let (min_y, height) = (self.data.min_y, self.data.height);
        match &mut self.aq_chunk[victim] {
            Some(existing) => existing.reset(cx, cz, min_y, height),
            slot_opt => *slot_opt = Some(AqChunkCache::new(cx, cz, min_y, height)),
        }
        self.aq_chunk_use[victim] = tick;
        victim
    }
    /// One cell's sample position, computed once per cell per chunk residency.
    fn aq_cell_rpos(
        &mut self,
        slot: usize,
        idx: usize,
        gx: i32,
        gy: i32,
        gz: i32,
    ) -> (i32, i32, i32) {
        if let Some(p) = self.aq_cache(slot).rpos[idx] {
            return p;
        }
        let p = self.aq_rpos(gx, gy, gz);
        self.aq_cache_mut(slot).rpos[idx] = Some(p);
        p
    }
    /// Fluid level at a cell's own sample position, stored at that cell's flat
    /// index. Keeps the M3 feature-read HIT/MISS counters on the same path as the
    /// hash cache it replaces.
    fn aq_cell_level(&mut self, slot: usize, idx: usize, m3_read: bool) -> (i32, u8) {
        if let Some(v) = self.aq_cache(slot).levels[idx] {
            inc_if(m3_read, M3_AQLV_HIT);
            return v;
        }
        inc_if(m3_read, M3_AQLV_MISS);
        let (rx, ry, rz) = self.aq_cache(slot).rpos[idx].expect("aq_cell_rpos fills before level");
        let v = self.aq_fluid_level_at(rx, ry, rz);
        self.aq_cache_mut(slot).levels[idx] = Some(v);
        v
    }
    /// Level for one of the three nearest nodes; `None` is the out-of-Y-window
    /// fallback, which computes directly without populating a chunk grid.
    fn aq_near_level(
        &mut self,
        near: &AqNear,
        i: usize,
        slot: Option<usize>,
        m3_read: bool,
    ) -> (i32, u8) {
        let (p, _, idx) = near[i];
        match slot {
            Some(slot) => self.aq_cell_level(slot, idx, m3_read),
            None => {
                inc_if(m3_read, M3_AQLV_MISS);
                self.aq_fluid_level_at(p.0, p.1, p.2)
            }
        }
    }
    /// The three nearest sample positions to `(x,y,z)` in exact bytecode order
    /// (`dx 0..1`, `dy -1..1`, `dz 0..1`, `>=` inserts in front). `None` means the
    /// block is outside the generator's Y window, where the caller computes the
    /// same selection directly instead.
    fn aq_near3(&mut self, x: i32, y: i32, z: i32) -> Option<(AqNear, usize)> {
        let (sx, sy, sz) = (lxz(x - 5), ly12(y + 1), lxz(z - 5));
        let (cx, cz) = (x.div_euclid(16), z.div_euclid(16));
        if self.aq_cell12.valid
            && self.aq_cell12.sx == sx
            && self.aq_cell12.sy == sy
            && self.aq_cell12.sz == sz
            && self.aq_cell12.cx == cx
            && self.aq_cell12.cz == cz
        {
            let slot = self.aq_cell12.slot;
            let tick = self.aq_chunk_tick.wrapping_add(1);
            self.aq_chunk_tick = tick;
            self.aq_chunk_use[slot] = tick;
            let mut near: AqNear = [((0, 0, 0), i64::MAX, 0); 3];
            let px = &self.aq_cell12.px;
            let py = &self.aq_cell12.py;
            let pz = &self.aq_cell12.pz;
            let idxs = &self.aq_cell12.idx;
            for i in 0..12 {
                let ex = px[i] - x;
                let ey = py[i] - y;
                let ez = pz[i] - z;
                let h = (ex * ex + ey * ey + ez * ez) as i64;
                if near[2].1 >= h {
                    let p = ((px[i], py[i], pz[i]), h, idxs[i]);
                    if near[0].1 >= h {
                        near[2] = near[1];
                        near[1] = near[0];
                        near[0] = p;
                    } else if near[1].1 >= h {
                        near[2] = near[1];
                        near[1] = p;
                    } else {
                        near[2] = p;
                    }
                }
            }
            return Some((near, slot));
        }
        let slot = self.aq_chunk_slot(cx, cz);
        if !(self.aq_cache(slot).contains(sx, sy - 1, sz)
            && self.aq_cache(slot).contains(sx + 1, sy + 1, sz + 1))
        {
            return None;
        }
        let mut near: AqNear = [((0, 0, 0), i64::MAX, 0); 3];
        let mut cpx = [0i32; 12];
        let mut cpy = [0i32; 12];
        let mut cpz = [0i32; 12];
        let mut cidx = [0usize; 12];
        let mut n = 0usize;
        for dx in 0..=1 {
            for dy in -1..=1 {
                for dz in 0..=1 {
                    let (gx, gy, gz) = (sx + dx, sy + dy, sz + dz);
                    let idx = self.aq_cache(slot).idx(gx, gy, gz);
                    let (px, py, pz) = self.aq_cell_rpos(slot, idx, gx, gy, gz);
                    cpx[n] = px;
                    cpy[n] = py;
                    cpz[n] = pz;
                    cidx[n] = idx;
                    n += 1;
                    let (ex, ey, ez) = (px - x, py - y, pz - z);
                    let h = ex as i64 * ex as i64 + ey as i64 * ey as i64 + ez as i64 * ez as i64;
                    if near[2].1 >= h {
                        let p = ((px, py, pz), h, idx);
                        if near[0].1 >= h {
                            near[2] = near[1];
                            near[1] = near[0];
                            near[0] = p;
                        } else if near[1].1 >= h {
                            near[2] = near[1];
                            near[1] = p;
                        } else {
                            near[2] = p;
                        }
                    }
                }
            }
        }
        self.aq_cell12 = AqCell12 {
            valid: true,
            cx,
            cz,
            sx,
            sy,
            sz,
            slot,
            px: cpx,
            py: cpy,
            pz: cpz,
            idx: cidx,
        };
        Some((near, slot))
    }
    /// Uncached selection for the out-of-window fallback: same 12 cells, same `>=`
    /// tie rule, but sample positions computed directly (the flat index is unused).
    fn aq_near3_uncached(&self, x: i32, y: i32, z: i32) -> AqNear {
        let (sx, sy, sz) = (lxz(x - 5), ly12(y + 1), lxz(z - 5));
        let mut near: AqNear = [((0, 0, 0), i64::MAX, 0); 3];
        for dx in 0..=1 {
            for dy in -1..=1 {
                for dz in 0..=1 {
                    let (gx, gy, gz) = (sx + dx, sy + dy, sz + dz);
                    let (px, py, pz) = self.aq_rpos(gx, gy, gz);
                    let (ex, ey, ez) = (px - x, py - y, pz - z);
                    let h = ex as i64 * ex as i64 + ey as i64 * ey as i64 + ez as i64 * ez as i64;
                    let p = ((px, py, pz), h, 0);
                    if near[0].1 >= h {
                        near[2] = near[1];
                        near[1] = near[0];
                        near[0] = p;
                    } else if near[1].1 >= h {
                        near[2] = near[1];
                        near[1] = p;
                    } else if near[2].1 >= h {
                        near[2] = p;
                    }
                }
            }
        }
        near
    }
    fn aq_fluid_level_at(&mut self, rx: i32, ry: i32, rz: i32) -> (i32, u8) {
        let def = self.aq_pick(ry);
        let (j, k) = (ry + 12, ry - 12);
        let mut bl = false;
        let mut min_est = i32::MAX;
        for (ox, oz) in AQ_OFF {
            let (xx, zz) = (rx + ox * 16, rz + oz * 16);
            let n = self.prelim_est(xx, zz);
            let o = n.wrapping_add(8);
            let center = ox == 0 && oz == 0;
            if center && k > o {
                return def;
            }
            if j > o || center {
                let fl = self.aq_pick(o);
                if Self::lvl_block(fl.0, fl.1, o) != 0 {
                    if center {
                        bl = true;
                    }
                    if j > o {
                        return fl;
                    }
                }
            }
            if n < min_est {
                min_est = n;
            }
        }
        let p = self.aq_fluid_y(rx, ry, rz, def, min_est, bl);
        let kind = self.aq_fluid_kind(rx, ry, rz, def, p);
        (p, kind)
    }
    #[inline]
    fn eval_aq_df(&mut self, id: usize, x: i32, y: i32, z: i32) -> f64 {
        if !wg_count() {
            match &self.dfs[id] {
                DF::C(v) => return *v,
                DF::Zero => return 0.0,
                DF::Noise {
                    ni,
                    sx,
                    sy,
                    sz,
                    xzs,
                    ys,
                } if matches!(self.dfs[*sx], DF::Zero)
                    && matches!(self.dfs[*sy], DF::Zero)
                    && matches!(self.dfs[*sz], DF::Zero) =>
                {
                    return self.noises[*ni].get(x as f64 * xzs, y as f64 * ys, z as f64 * xzs);
                }
                _ => {
                    let mut flat = self.take_corner_memo();
                    flat.next_2d();
                    flat.next_3d();
                    let v = self.eval_flat(id, x, y, z, &mut flat);
                    self.corner_memo = Some(flat);
                    return v;
                }
            }
        }
        self.eval(id, x, y, z, &mut HashMap::new())
    }

    fn aq_fluid_y(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        def: (i32, u8),
        min_est: i32,
        map_y: bool,
    ) -> i32 {
        // OverworldBiomeBuilder.isDeepDark: erosion < -0.225F && depth > 0.9F.
        // NoiseRouter.h()/i() are erosion/depth accessors (the fields are named
        // continents/erosion/depth); keep the promoted f32 literals.
        let deep = match self.climate {
            Some(c) => {
                if wg_count() {
                    let mut memo = HashMap::new();
                    self.eval(c[3], x, y, z, &mut memo) < -0.225f32 as f64
                        && self.eval(c[4], x, y, z, &mut memo) > 0.9f32 as f64
                } else {
                    let mut flat = self.take_corner_memo();
                    flat.next_2d();
                    flat.next_3d();
                    let is_deep = self.eval_flat(c[3], x, y, z, &mut flat) < -0.225f32 as f64
                        && self.eval_flat(c[4], x, y, z, &mut flat) > 0.9f32 as f64;
                    self.corner_memo = Some(flat);
                    is_deep
                }
            }
            None => false,
        };
        let (d, e) = if deep {
            (-1.0, -1.0)
        } else {
            let top = min_est.wrapping_add(8).wrapping_sub(y);
            let f = if map_y {
                1.0 - (top as f64).clamp(0.0, 64.0) / 64.0
            } else {
                0.0
            };
            let flood_id = self.aq.as_ref().unwrap().flood;
            let g = self.eval_aq_df(flood_id, x, y, z).clamp(-1.0, 1.0);
            (
                g - lmap(f, 1.0, 0.0, -0.8, 0.4),
                g - lmap(f, 1.0, 0.0, -0.3, 0.8),
            )
        };
        if e > 0.0 {
            def.0
        } else if d > 0.0 {
            self.aq_noise_level(x, y, z, min_est)
        } else {
            AQ_MIN
        }
    }
    fn aq_noise_level(&mut self, x: i32, y: i32, z: i32, min_est: i32) -> i32 {
        let (gx, gy, gz) = (x.div_euclid(16), y.div_euclid(40), z.div_euclid(16));
        let spread_id = self.aq.as_ref().unwrap().spread;
        let s = self.eval_aq_df(spread_id, gx, gy, gz) * 10.0;
        min_est.min(((s / 3.0).floor() as i32) * 3 + gy * 40 + 20)
    }
    fn aq_fluid_kind(&mut self, x: i32, y: i32, z: i32, def: (i32, u8), level: i32) -> u8 {
        if level <= -10 && level != AQ_MIN && def.1 != 2 {
            let lava_id = self.aq.as_ref().unwrap().lava;
            let s = self.eval_aq_df(
                lava_id,
                x.div_euclid(64),
                y.div_euclid(40),
                z.div_euclid(64),
            );
            if s.abs() > 0.3 {
                return 2;
            }
        }
        def.1
    }
    fn aq_calc_density(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        l1: (i32, u8),
        l2: (i32, u8),
        barrier: &mut Option<f64>,
    ) -> f64 {
        let b1 = Self::lvl_block(l1.0, l1.1, y);
        let b2 = Self::lvl_block(l2.0, l2.1, y);
        if (b1 == 2 && b2 == 1) || (b1 == 1 && b2 == 2) {
            return 2.0;
        }
        let diff = (l1.0 - l2.0).abs();
        if diff == 0 {
            return 0.0;
        }
        let avg = 0.5 * (l1.0 + l2.0) as f64;
        let sc = y as f64 + 0.5 - avg;
        let hd = diff as f64 / 2.0;
        let o = hd - sc.abs();
        let q = if sc > 0.0 {
            if o > 0.0 {
                o / 1.5
            } else {
                o / 2.5
            }
        } else {
            let p = 3.0 + o;
            if p > 0.0 {
                p / 3.0
            } else {
                p / 10.0
            }
        };
        // Vanilla: !(q < -2.0) && !(q > 2.0), which is true for NaN.
        let r = if q.is_nan() || (-2.0..=2.0).contains(&q) {
            match *barrier {
                Some(v) => v,
                None => {
                    let barrier_id = self.aq.as_ref().unwrap().barrier;
                    let v = self.eval_aq_df(barrier_id, x, y, z);
                    *barrier = Some(v);
                    v
                }
            }
        } else {
            0.0
        };
        2.0 * (r + q)
    }
}

/// Vanilla aquifer: fluid state at a non-solid position.
/// Some(state) = air/water/lava; None = barrier pushed it solid (material rules).
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub fn aquifer_state(x: i32, y: i32, z: i32, density: f64) -> Option<u32> {
    gen_lock().as_mut()?.aq_state(x, y, z, density)
}

/// The block state at a world position, taking the generator lock once.
/// One cell. Payloads stay generator-owned until a safe runtime boundary or
/// save drains them under save -> generator -> runtime lock order. A cell read
/// can hold a mob/player lock, so it must never acquire the save lock or create
/// a take-before-save gap between a source claim and its payload.
pub fn block_state(
    x: i32,
    y: i32,
    z: i32,
    populate: impl FnMut(&mut Gen, i32, i32),
) -> Option<u32> {
    let mut guard = gen_lock();
    Some(guard.as_mut()?.block_state_at(x, y, z, populate))
}

/// Base terrain states for one 16x16x16 section, in the send layout
/// `(ly << 8) | (lz << 4) | lx`, with one generator lock for the whole section.
///
/// The section is built in two halves, and only the first one is ever parallel:
///
/// 1. **terrain** — density, material rules, aquifers, surface/deep rules. A pure
///    function of the immutable data and the seed, so a worker's scratch generator
///    gives the same array (see [`prefetch_terrain`]); a row a worker already built is
///    taken from `terr_c` instead of recomputed here.
/// 2. **the feature layer** — `feat_state` first, exactly as `block_state_at` did, so a
///    feature write still wins over terrain, the population dispatch still happens at the
///    first cell of the section in the same order, and the marks it files are unchanged.
///    A base cell is computed even where a feature later overrides it; that is work the
///    parallel half absorbs, and it is why the two halves can be separated at all.
pub fn terrain_section(
    cx: i32,
    sy: i32,
    cz: i32,
    populate: impl FnMut(&mut Gen, i32, i32),
) -> Option<Box<[u32; 4096]>> {
    terrain_section_dim(active_dim(), cx, sy, cz, populate)
}

/// [`terrain_section`] for one dimension: the streaming seam.
///
/// The section a caller serves belongs to a dimension, and that dimension's generator is the
/// one whose surface rules, aquifers, carvers, features and world window produced it. Asking
/// for it here is the whole difference from the dim-less form; the two bodies are the same so
/// a test can compare a dimension's arm with the active arm on the same generator.
pub fn terrain_section_dim(
    dimension: DimId,
    cx: i32,
    sy: i32,
    cz: i32,
    populate: impl FnMut(&mut Gen, i32, i32),
) -> Option<Box<[u32; 4096]>> {
    if let Some(section) = custom_terrain_section(dimension, sy) {
        return Some(section);
    }
    ensure_dim_generator(dimension);
    let mut gg = gen_lock_dim(dimension);
    gg.as_mut()?
        .terrain_section_at(cx, sy, cz, populate)
        .into()
}

impl Gen {
    /// [`terrain_section`] for one generator, without taking the registry's lock. The two
    /// halves are described there; this is the same body so a test can compare a prefetched
    /// arm and a serial arm on two independent generators.
    pub(crate) fn terrain_section_at(
        &mut self,
        cx: i32,
        sy: i32,
        cz: i32,
        mut populate: impl FnMut(&mut Gen, i32, i32),
    ) -> Box<[u32; 4096]> {
        let mut arr: Box<[u32; 4096]> = match self.terr_c.get(&(cx, sy, cz)) {
            Some(rows) => {
                // M3 P1a M2/M4: the store answered, so this section's fill is not repeated and
                // the row is no longer "waiting for its consumer".
                m3_inc(M3_TERR_HIT);
                if m3_on() {
                    self.terr_pending.remove(&(cx, sy, cz));
                }
                // P1b: the store, the worker that built the row and any other consumer share
                // the array; the feature pass below writes into it, so this is the one copy
                // out of the `Arc` (16 KiB per served section, against a fresh fill).
                Box::new(*rows.as_ref())
            }
            None => {
                // M4: the owner filled a section the store did not have. At `jobs=1` a prefetch
                // round always installs, so this counts genuinely-missed sections only.
                m3_inc(M3_TERR_MISS);
                match self.cols(cx, cz) {
                    Some(cc) => {
                        let mut arr = Box::new([0; 4096]);
                        let marks = self.base_section_rows(&cc, cx, sy, cz, &mut arr);
                        self.file_terrain_marks(&(cx, sy, cz), &marks);
                        arr
                    }
                    None => Box::new([0u32; 4096]),
                }
            }
        };
        // Once population is settled, reading the section neither dispatches nor
        // evicts anything. Apply the chunk's overrides without the per-cell lookups
        // of the population latch and outer chunk map.
        let key = (cx, cz);
        if self.pop_region.is_none()
            && self.pop_done.contains(&key)
            && self.feat_ord.len() <= FEATURE_BUCKET_BOUND
        {
            if let Some(bucket) = self.feat_c.get(&key) {
                let first = ((sy - self.min_y) as u32) << 8;
                if bucket.capacity() < arr.len() {
                    for (&k, &st) in bucket {
                        if let Some(cell) = arr.get_mut(k.wrapping_sub(first) as usize) {
                            *cell = st;
                        }
                    }
                } else {
                    for (i, cell) in arr.iter_mut().enumerate() {
                        if let Some(&st) = bucket.get(&first.wrapping_add(i as u32)) {
                            *cell = st;
                        }
                    }
                }
            }
            return arr;
        }
        // The first `feat_state` call settles this chunk's population - its own pass
        // plus the 3x3 writers `populate_request_box` covers, or the whole
        // `pop_region` order - and every cell of the section belongs to that same
        // chunk. Once settled, the bucket cannot change during this walk (only
        // another `feat_state` call could trigger a populate, and no later one is
        // reached), so the remaining cells read the bucket directly instead of
        // repeating the region scan / `pop_done` / bound / double-map chain 4095
        // more times. The settle itself can evict the bucket it just filled when
        // `feat_ord` is past its bound: re-settle the way the next cell's
        // `feat_state` would until `pop_done` sticks.
        let (x0, y0, z0) = (cx * 16, sy, cz * 16);
        loop {
            // The first cell's own answer is discarded on purpose: the bucket pass
            // below writes the same value at the same index.
            let _ = self.feat_state(x0, y0, z0, &mut populate);
            if self.pop_done.contains(&key) {
                break;
            }
        }
        if let Some(bucket) = self.feat_c.get(&key) {
            let first = ((sy - self.min_y) as u32) << 8;
            if bucket.capacity() < arr.len() {
                for (&k, &st) in bucket {
                    if let Some(cell) = arr.get_mut(k.wrapping_sub(first) as usize) {
                        *cell = st;
                    }
                }
            } else {
                for (i, cell) in arr.iter_mut().enumerate() {
                    if let Some(&st) = bucket.get(&first.wrapping_add(i as u32)) {
                        *cell = st;
                    }
                }
            }
        }
        arr
    }
}

/// feature-layer override lookup (populates 3x3 chunks lazily on first touch)
pub fn feature_state(
    x: i32,
    y: i32,
    z: i32,
    populate: impl FnMut(&mut Gen, i32, i32),
) -> Option<u32> {
    gen_lock().as_mut()?.feat_state(x, y, z, populate)
}

/// The mark a writer's state carries: a state whose `FluidState` is not empty. Vanilla's
/// fill and surface updater both ask `!state.getFluidState().isEmpty()`, which is not the
/// same question as "the state is water or lava" (a waterlogged stair answers yes; `magma_block`
/// answers no), so it goes through the exported fluid identity rather than a name test.
fn terrain_mark(reg: Option<&BReg>, gate: u8, state: u32) -> u8 {
    if gate == 0 {
        return 0;
    }
    match reg {
        Some(reg) if reg.fluid_name(state) != "minecraft:empty" => gate,
        _ => 0,
    }
}

/// `Aquifer.FLOWING_UPDATE_SIMULARITY` (`dyk$c.k`): `similarity(square(10), square(12))` =
/// `1 - |144 - 100| / 25` = `-0.76`. It is a floor on the similarity of the two nearest aquifer
/// nodes and it decides whether a fluid cell the aquifer produced is *settled*: a cell whose
/// nearest node dominates (similarity at or above this) gets a `markPosForPostprocessing` from
/// the fill and from the carvers, a cell inside a blend boundary does not.
const AQ_FLOWING_UPDATE_SIMULARITY: f64 = 1.0 - 44.0 / 25.0;

impl Gen {
    /// `Aquifer.shouldScheduleFluidUpdate()` — the port's `NoiseBasedAquifer.w`, as of the last
    /// [`Gen::aq_state`] call.
    ///
    /// Two 1.21.1 mark sites gate on it: the fill (`dzb`) and the carvers (`ebe`, through
    /// `getCarveState`, which is [`Gen::carve_state`]). It cannot be replaced by "the state has
    /// a fluid": a fluid inside a blend is a fluid vanilla does not mark.
    pub(crate) fn aq_schedule_fluid_update(&self) -> bool {
        self.aq_sched
    }

    /// Install the flag a shared column build recorded in [`ChunkCols::carve_sched`]. A carve
    /// on a generator that did not run the build restores it instead of answering the mark
    /// gate with whatever its own last aquifer call left.
    pub(crate) fn set_aq_schedule_fluid_update(&mut self, sched: bool) {
        self.aq_sched = sched;
    }

    /// Fast aquifer probe when the caller already proved `density <= 0.0`.
    /// Returns `Some(res)` when `aq_state(x, y, z, d)` returns `res` identically for every
    /// `d <= 0.0`, avoiding unnecessary `noodle` density evaluation. Returns `None` when
    /// a non-zero aquifer barrier blend could depend on the exact magnitude of `d`.
    fn aq_state_if_nonpos_independent(&mut self, x: i32, y: i32, z: i32) -> Option<Option<u32>> {
        if aq_trace_enabled() {
            return None;
        }
        // Every return below writes `aq_sched` exactly as `aq_state` would for this cell,
        // so it is an aquifer call for `build_cols_body`'s touch accounting too.
        self.aq_calls = self.aq_calls.wrapping_add(1);
        if self.aq.is_none() {
            m3_inc(M3_AQUIFER_CELLS);
            self.aq_sched = false;
            return Some(Some(if y < -54 {
                1
            } else if y < self.sea {
                self.def_fl
            } else {
                0
            }));
        }
        let (water, lava) = {
            let a = self.aq.as_ref().unwrap();
            (a.water, a.lava_st)
        };
        let def = self.aq_pick(y);
        if Gen::lvl_block(def.0, def.1, y) == 2 {
            m3_inc(M3_AQUIFER_CELLS);
            self.aq_sched = false;
            return Some(Some(lava));
        }
        let (near, slot) = match self.aq_near3(x, y, z) {
            Some((near, slot)) => (near, Some(slot)),
            None => (self.aq_near3_uncached(x, y, z), None),
        };
        let m3_read = m3_on() && m3_in_feature_read();
        let (d0, d1) = (near[0].1, near[1].1);
        let l2 = self.aq_near_level(&near, 0, slot, m3_read);
        let blk = Gen::lvl_block(l2.0, l2.1, y);
        let sim12 = 1.0 - (d1 - d0).abs() as f64 / 25.0;
        if sim12 <= 0.0 {
            m3_inc(M3_AQUIFER_CELLS);
            self.aq_sched = sim12 >= AQ_FLOWING_UPDATE_SIMULARITY;
            return Some(Some(match blk {
                1 => water,
                2 => lava,
                _ => 0,
            }));
        }
        let pb = self.aq_pick(y - 1);
        if blk == 1 && Gen::lvl_block(pb.0, pb.1, y - 1) == 2 {
            m3_inc(M3_AQUIFER_CELLS);
            self.aq_sched = true;
            return Some(Some(water));
        }
        let l3 = self.aq_near_level(&near, 1, slot, m3_read);
        let l4 = self.aq_near_level(&near, 2, slot, m3_read);
        if l2 == l3 && l2 == l4 {
            m3_inc(M3_AQUIFER_CELLS);
            self.aq_sched = true;
            return Some(Some(match blk {
                1 => water,
                2 => lava,
                _ => 0,
            }));
        }
        None
    }

    pub(crate) fn aq_state(&mut self, x: i32, y: i32, z: i32, density: f64) -> Option<u32> {
        // M3 P1a M5: how many cells ask the aquifer at all. The count is the stage's share of
        // the 4096 cells a section fills; the aquifer's own cost sits behind it.
        m3_inc(M3_AQUIFER_CELLS);
        self.aq_calls = self.aq_calls.wrapping_add(1);
        let g = self;
        // Every return below is one of `Aquifer$NoiseBasedAquifer.computeSubstance`'s, and each
        // carries its own `this.w = …` assignment:
        //   density > 0          -> w = false (the body's first statement)
        //   disabled aquifer     -> `createDisabled().a()` is constant false
        //   the fluid is lava    -> w = false
        //   sim12 <= 0           -> w = sim12 >= FLOWING_UPDATE_SIMULARITY
        //   water over lava      -> w = true
        //   a blend wins (null)  -> w = false
        //   the fluid survives   -> w = true
        if density > 0.0 {
            g.aq_sched = false;
            return None;
        }
        if g.aq.is_none() {
            g.aq_sched = false;
            return Some(if y < -54 {
                1
            } else if y < g.sea {
                g.def_fl
            } else {
                0
            });
        }
        // One OnceLock load per cell, not one per return branch: tracing is almost
        // always off, and this is the hottest function in the terrain fill.
        let trace = aq_trace_at(x, z);
        let (water, lava) = {
            let a = g.aq.as_ref().unwrap();
            (a.water, a.lava_st)
        };
        let def = g.aq_pick(y);
        let def_blk = Gen::lvl_block(def.0, def.1, y);
        if def_blk == 2 {
            g.aq_sched = false;
            if trace {
                eprintln!("Aqd {x} {y} {z} d={density} lava");
            }
            return Some(lava);
        }
        g.aq_pos_trace(x, y, z);
        let (near, slot) = match g.aq_near3(x, y, z) {
            Some((near, slot)) => (near, Some(slot)),
            None => (g.aq_near3_uncached(x, y, z), None),
        };
        let m3_read = m3_on() && m3_in_feature_read();
        // Distances come straight out of the nearest-three selection — no second
        // `(p - query)` recomputation, and no hash lookup for the levels: each
        // node's level lives at its own chunk-local flat index.
        let (d0, d1, d2) = (near[0].1, near[1].1, near[2].1);
        let l2 = g.aq_near_level(&near, 0, slot, m3_read);
        let blk = Gen::lvl_block(l2.0, l2.1, y);
        let sim12 = 1.0 - (d1 - d0).abs() as f64 / 25.0;
        if trace {
            eprintln!(
            "AQ x={x} y={y} z={z} sim12={sim12:.4} near0={:?} lvl0={:?} near1={:?} lvl1={:?} d0={d0} d1={d1}",
            near[0].0, l2, near[1].0, g.aq_near_level(&near, 1, slot, m3_read)
        );
        }
        if sim12 <= 0.0 {
            g.aq_sched = sim12 >= AQ_FLOWING_UPDATE_SIMULARITY;
            if trace {
                eprintln!("Aqd {x} {y} {z} d={density} sim {blk} sim12={sim12:.4} l0={l2:?}");
            }
            return Some(match blk {
                1 => water,
                2 => lava,
                _ => 0,
            });
        }
        // water directly above a lava level stays water (vanilla schedules a tick)
        let pb = g.aq_pick(y - 1);
        if blk == 1 && Gen::lvl_block(pb.0, pb.1, y - 1) == 2 {
            g.aq_sched = true;
            if trace {
                eprintln!("Aqd {x} {y} {z} d={density} lavaunder {blk} sim12={sim12:.4} l0={l2:?}");
            }
            return Some(water);
        }
        let mut barrier = None;
        let l3 = g.aq_near_level(&near, 1, slot, m3_read);
        // Two identical levels make `aq_calc_density` return 0.0 at its `diff == 0`
        // early-out, so that pair's blend can never reject the fluid and its
        // barrier-noise sample is never needed. Under a uniform water table or in
        // dry air all three levels are equal and no `aq_calc_density` runs at all -
        // that is the common case, so it is checked before any pair density.
        let c12 = if l2 == l3 {
            0.0
        } else {
            g.aq_calc_density(x, y, z, l2, l3, &mut barrier)
        };
        let (mut c13, mut c23) = (0.0f64, 0.0f64);
        if density + sim12 * c12 > 0.0 {
            g.aq_sched = false;
            if trace {
                eprintln!("Aqd {x} {y} {z} d={density} null 12 sim={sim12:.4} c12={c12:.4} c13=0 c23=0 l0={:?} l1={:?} l2={:?}", l2, l3, (0, 0));
            }
            return None;
        }
        let l4 = g.aq_near_level(&near, 2, slot, m3_read);
        if l2 == l3 && l3 == l4 {
            // c13 and c23 are provably 0.0 as well: every blend is a no-op and the
            // cell is the nearest level's fluid (or air), exactly the answer the
            // full evaluation's tail produces.
            g.aq_sched = true;
            if trace {
                eprintln!("Aqd {x} {y} {z} d={density} state {blk} sim={sim12:.4} c12=0 c13=0 c23=0 l0={:?} l1={:?} l2={:?}", l2, l3, l4);
            }
            return Some(match blk {
                1 => water,
                2 => lava,
                _ => 0,
            });
        }
        let sim13 = 1.0 - (d2 - d0).abs() as f64 / 25.0;
        if sim13 > 0.0 {
            c13 = g.aq_calc_density(x, y, z, l2, l4, &mut barrier);
            let b13 = sim12 * sim13 * c13;
            if density + b13 > 0.0 {
                g.aq_sched = false;
                if trace {
                    eprintln!("Aqd {x} {y} {z} d={density} null 13 sim={sim12:.4} c12={c12:.4} s13={sim13:.4} c13={c13:.4} l0={:?} l1={:?} l2={:?}", l2, l3, l4);
                }
                return None;
            }
        }
        let sim23 = 1.0 - (d2 - d1).abs() as f64 / 25.0;
        if sim23 > 0.0 {
            c23 = g.aq_calc_density(x, y, z, l3, l4, &mut barrier);
            let b23 = sim12 * sim23 * c23;
            if density + b23 > 0.0 {
                g.aq_sched = false;
                if trace {
                    eprintln!("Aqd {x} {y} {z} d={density} null 23 sim={sim12:.4} s23={sim23:.4} c23={c23:.4} l0={:?} l1={:?} l2={:?}", l2, l3, l4);
                }
                return None;
            }
        }
        g.aq_sched = true;
        if trace {
            eprintln!("Aqd {x} {y} {z} d={density} state {blk} sim={sim12:.4} c12={c12:.4} c13={c13:.4} c23={c23:.4} l0={:?} l1={:?} l2={:?}", l2, l3, l4);
        }
        Some(match blk {
            1 => water,
            2 => lava,
            _ => 0,
        })
    }
}

// ---------- biome multi-noise tree ----------

// vanilla 1.21.1 overworld biome registry order — matches the captured
// registry_data sent to clients in config phase (alphabetical, 64 entries).
const BIOME64: [&str; 64] = [
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

fn biome_id(name: &str) -> i64 {
    let n = name.rsplit(':').next().unwrap_or(name);
    BIOME64
        .iter()
        .position(|&b| b == n)
        .map(|i| i as i64)
        .unwrap_or(39) // plains
}

#[derive(Debug, Clone)]
struct BTNode {
    bounds: [[i32; 2]; 7],
    /// When `child_len == 0`, this is a leaf with biome id `child_start as u16`.
    /// Otherwise children live contiguously at `nodes[child_start .. child_start + child_len]`.
    child_start: u32,
    child_len: u16,
    /// Precomputed squared distance for dimension 6 when `t[6] == 0` (always true in `target7`).
    off6_sq: i64,
}

#[derive(Debug, Clone)]
struct BT {
    nodes: Vec<BTNode>,
}

enum RawBT {
    Leaf([[i32; 2]; 7], u16),
    Sub([[i32; 2]; 7], Vec<RawBT>),
}

fn bt_params(j: &J) -> [[i32; 2]; 7] {
    let mut p = [[0i32; 2]; 7];
    if let Some(a) = j.get("parameters").and_then(J::as_arr) {
        for (i, e) in a.iter().take(7).enumerate() {
            p[i][0] = e.get("min").map(J::num).unwrap_or(0.0) as i32;
            p[i][1] = e.get("max").map(J::num).unwrap_or(0.0) as i32;
        }
    }
    p
}

fn bt_parse_raw(
    j: &J,
    names: &mut Vec<String>,
    ids: &mut HashMap<String, u16>,
    leaf_ps: &mut Vec<[[i32; 2]; 7]>,
) -> Option<RawBT> {
    let ps = bt_params(j);
    match j.get("_type").map(J::str) {
        Some("leaf") => {
            let b = j.get("biome").map(J::str)?.to_string();
            let id = match ids.get(&b) {
                Some(&i) => i,
                None => {
                    let i = names.len() as u16;
                    names.push(b.clone());
                    ids.insert(b, i);
                    leaf_ps.push(ps);
                    i
                }
            };
            Some(RawBT::Leaf(ps, id))
        }
        Some("branch") => {
            let ch = j
                .get("subTree")
                .and_then(J::as_arr)?
                .iter()
                .filter_map(|c| bt_parse_raw(c, names, ids, leaf_ps))
                .collect();
            Some(RawBT::Sub(ps, ch))
        }
        _ => None,
    }
}

fn bt_parse(
    j: &J,
    names: &mut Vec<String>,
    ids: &mut HashMap<String, u16>,
    leaf_ps: &mut Vec<[[i32; 2]; 7]>,
) -> Option<BT> {
    let root = bt_parse_raw(j, names, ids, leaf_ps)?;
    let mut nodes = Vec::new();
    fn off6(ps: &[[i32; 2]; 7]) -> i64 {
        let (mn, mx) = (ps[6][0] as i64, ps[6][1] as i64);
        let dd = if 0 < mn {
            mn
        } else if 0 > mx {
            -mx
        } else {
            0
        };
        dd * dd
    }
    fn flatten(node: &RawBT, out: &mut Vec<BTNode>) -> usize {
        let idx = out.len();
        match node {
            RawBT::Leaf(ps, id) => {
                out.push(BTNode {
                    bounds: *ps,
                    child_start: u32::from(*id),
                    child_len: 0,
                    off6_sq: off6(ps),
                });
            }
            RawBT::Sub(ps, children) => {
                out.push(BTNode {
                    bounds: *ps,
                    child_start: 0,
                    child_len: children.len() as u16,
                    off6_sq: off6(ps),
                });
                let start = out.len() as u32;
                out[idx].child_start = start;
                // Reserve all direct children contiguously so a branch scans one flat slice.
                for ch in children {
                    match ch {
                        RawBT::Leaf(cps, id) => out.push(BTNode {
                            bounds: *cps,
                            child_start: u32::from(*id),
                            child_len: 0,
                            off6_sq: off6(cps),
                        }),
                        RawBT::Sub(cps, grand) => out.push(BTNode {
                            bounds: *cps,
                            child_start: 0,
                            child_len: grand.len() as u16,
                            off6_sq: off6(cps),
                        }),
                    }
                }
                for (i, ch) in children.iter().enumerate() {
                    if let RawBT::Sub(_, grand) = ch {
                        fill_children(start as usize + i, grand, out);
                    }
                }
            }
        }
        idx
    }
    fn fill_children(parent_idx: usize, children: &[RawBT], out: &mut Vec<BTNode>) {
        let start = out.len() as u32;
        out[parent_idx].child_start = start;
        for ch in children {
            match ch {
                RawBT::Leaf(cps, id) => out.push(BTNode {
                    bounds: *cps,
                    child_start: u32::from(*id),
                    child_len: 0,
                    off6_sq: off6(cps),
                }),
                RawBT::Sub(cps, grand) => out.push(BTNode {
                    bounds: *cps,
                    child_start: 0,
                    child_len: grand.len() as u16,
                    off6_sq: off6(cps),
                }),
            }
        }
        for (i, ch) in children.iter().enumerate() {
            if let RawBT::Sub(_, grand) = ch {
                fill_children(start as usize + i, grand, out);
            }
        }
    }
    flatten(&root, &mut nodes);
    Some(BT { nodes })
}

#[inline(always)]
fn dim_dist_sq(bounds: [i32; 2], v: i64) -> i64 {
    let d1 = bounds[0] as i64 - v;
    let d2 = v - bounds[1] as i64;
    let d = d1.max(d2).max(0);
    d * d
}

#[inline(always)]
fn ps_dist(ps: &[[i32; 2]; 7], t: &[i64; 7]) -> i64 {
    dim_dist_sq(ps[0], t[0])
        + dim_dist_sq(ps[1], t[1])
        + dim_dist_sq(ps[2], t[2])
        + dim_dist_sq(ps[3], t[3])
        + dim_dist_sq(ps[4], t[4])
        + dim_dist_sq(ps[5], t[5])
        + dim_dist_sq(ps[6], t[6])
}

impl BTNode {
    /// The exact distance when it is below `bound`, else `bound`. Every term is
    /// a non-negative squared distance, so once the running sum reaches `bound`
    /// the full sum cannot be below it — and the search only ever asks
    /// `d < best`, so a node that reached the bound is rejected either way.
    /// Same ordering decision the unbounded sum gives; the terms are just not
    /// all paid for.
    #[inline(always)]
    fn dist_bounded(&self, t: &[i64; 7], bound: i64) -> i64 {
        let mut sum = dim_dist_sq(self.bounds[0], t[0]);
        if sum >= bound {
            return bound;
        }
        sum += dim_dist_sq(self.bounds[1], t[1]);
        if sum >= bound {
            return bound;
        }
        sum += dim_dist_sq(self.bounds[2], t[2]);
        if sum >= bound {
            return bound;
        }
        sum += dim_dist_sq(self.bounds[3], t[3]);
        if sum >= bound {
            return bound;
        }
        sum += dim_dist_sq(self.bounds[4], t[4]);
        if sum >= bound {
            return bound;
        }
        sum += dim_dist_sq(self.bounds[5], t[5]);
        if sum >= bound {
            return bound;
        }
        sum + if t[6] == 0 {
            self.off6_sq
        } else {
            dim_dist_sq(self.bounds[6], t[6])
        }
    }
}

impl BT {
    /// `SearchTree.TreeNode.getResultingNode`. `best` is the incumbent (distance, leaf): the
    /// previous result when the caller has one, else `(i64::MAX, _)`. A branch scans its
    /// children **in array order**, descends into a child only when the child's box is
    /// strictly nearer than the incumbent (`if (l > m)`), and a leaf replaces the incumbent
    /// only when strictly nearer (`if (l > n)`). So among equally near leaves the incumbent
    /// wins, then the one reached first in array order - which is why the order must not be
    /// changed (sorting the children by distance reaches a different leaf on ties, and also
    /// prunes differently).
    fn search(&self, t: &[i64; 7], best: &mut (i64, u16)) -> Option<&[[i32; 2]; 7]> {
        if self.nodes.is_empty() || best.0 == 0 {
            return None;
        }
        self.search_node(0, t, best)
    }

    fn search_node(
        &self,
        idx: usize,
        t: &[i64; 7],
        best: &mut (i64, u16),
    ) -> Option<&[[i32; 2]; 7]> {
        let node = &self.nodes[idx];
        if node.child_len == 0 {
            let d = node.dist_bounded(t, best.0);
            if d < best.0 {
                *best = (d, node.child_start as u16);
                return Some(&node.bounds);
            }
            return None;
        }
        let start = node.child_start as usize;
        let end = start + node.child_len as usize;
        let mut selected = None;
        for ci in start..end {
            let child = &self.nodes[ci];
            // Bounded distance: a child already at the incumbent is rejected
            // without paying for its remaining dimensions (see `dist_bounded`).
            let d = child.dist_bounded(t, best.0);
            if d < best.0 {
                if child.child_len == 0 {
                    *best = (d, child.child_start as u16);
                    selected = Some(&child.bounds);
                    if d == 0 {
                        return selected;
                    }
                } else {
                    selected = self.search_node(ci, t, best).or(selected);
                    if best.0 == 0 {
                        return selected;
                    }
                }
            }
        }
        selected
    }
}

// 5 y-independent climate params cached per quart column:
// [temperature, vegetation, continentalness, erosion, weirdness]

impl Gen {
    fn climate_col(&mut self, qx: i32, qz: i32) -> Option<([i64; 5], f64)> {
        if let Some(&v) = self.clim_c.get(&(qx, qz)) {
            return Some(v);
        }
        let cl = self.climate?;
        let (bx, bz) = (qx << 2, qz << 2);
        let mut t = [0i64; 5];
        let off;
        if wg_count() {
            let mut memo = HashMap::new();
            for (i, &id) in [cl[0], cl[1], cl[2], cl[3], cl[5]].iter().enumerate() {
                t[i] = (self.eval(id, bx, 0, bz, &mut memo) * 10000.0) as i64;
            }
            // depth offset term (y-independent part of depth) when splittable
            off = match self.depth_split {
                Some((_, _, _, _, oid)) => self.eval(oid, bx, 0, bz, &mut memo),
                None => 0.0,
            };
        } else {
            let mut flat = self.take_corner_memo();
            flat.next_2d();
            flat.next_3d();
            for (i, &id) in [cl[0], cl[1], cl[2], cl[3], cl[5]].iter().enumerate() {
                t[i] = (self.eval_flat(id, bx, 0, bz, &mut flat) * 10000.0) as i64;
            }
            off = match self.depth_split {
                Some((_, _, _, _, oid)) => self.eval_flat(oid, bx, 0, bz, &mut flat),
                None => 0.0,
            };
            self.corner_memo = Some(flat);
        }
        self.clim_c.insert((qx, qz), (t, off));
        self.clim_ord.push_back((qx, qz));
        if self.clim_ord.len() > 16384 {
            let old = self.clim_ord.pop_front().unwrap();
            self.clim_c.remove(&old);
        }
        Some((t, off))
    }

    /// 7-param biome target at quart coords
    fn target7(&mut self, qx: i32, qy: i32, qz: i32) -> Option<[i64; 7]> {
        let (c5, off) = self.climate_col(qx, qz)?;
        self.target7_with_col(c5, off, qx, qy, qz)
    }

    /// `target7` with the column pair already in hand: the 4x4x4 quart scan visits
    /// each of its 16 (qx, qz) columns four times, so the caller keeps them in a
    /// table and only the depth term (which depends on `qy`) is recomputed per quart.
    fn target7_with_col(
        &mut self,
        c5: [i64; 5],
        off: f64,
        qx: i32,
        qy: i32,
        qz: i32,
    ) -> Option<[i64; 7]> {
        let dq = match self.depth_split {
            Some((fc, fv, tc, tv, _)) => {
                let y = (qy << 2) as f64;
                let tt = ((y - fc) / (tc - fc)).clamp(0.0, 1.0);
                ((fv + tt * (tv - fv) + off) * 10000.0) as i64
            }
            None => {
                (self.eval(
                    self.climate?[4],
                    qx << 2,
                    qy << 2,
                    qz << 2,
                    &mut HashMap::new(),
                ) * 10000.0) as i64
            }
        };
        Some([c5[0], c5[1], c5[2], c5[3], dq, c5[4], 0])
    }

    /// `BiomeManager.getBiome(BlockPos)` — the *blended* biome vanilla's
    /// `LevelReader.getBiome` returns, which is what `BiomeFilter` (and anything
    /// else reading the level at a position) sees. It is not the chunk's biome
    /// container: the eight neighbouring noise samples are ranked by
    /// `getFiddledDistance`, so at a biome boundary the two disagree.
    pub(crate) fn biome_blended(&mut self, x: i32, y: i32, z: i32) -> Option<(i64, u16)> {
        let (qx, qy, qz) = self.fiddled_biome_quart(x, y, z);
        self.biome_quart(qx, qy, qz, None)
    }

    fn fiddled_biome_quart(&self, x: i32, y: i32, z: i32) -> (i32, i32, i32) {
        let zoom = self.biome_zoom;
        let (x, y, z) = (x - 2, y - 2, z - 2);
        let (bx, by, bz) = (x >> 2, y >> 2, z >> 2);
        let (fx, fy, fz) = (
            (x & 3) as f64 / 4.0,
            (y & 3) as f64 / 4.0,
            (z & 3) as f64 / 4.0,
        );
        let mut best = (0usize, f64::INFINITY);
        for i in 0..8usize {
            let (px, py, pz) = ((i & 4) == 0, (i & 2) == 0, (i & 1) == 0);
            let (qx, qy, qz) = (
                if px { bx } else { bx + 1 },
                if py { by } else { by + 1 },
                if pz { bz } else { bz + 1 },
            );
            let (ox, oy, oz) = (
                if px { fx } else { fx - 1.0 },
                if py { fy } else { fy - 1.0 },
                if pz { fz } else { fz - 1.0 },
            );
            let d = crate::generation_math::fiddled_distance(zoom, qx, qy, qz, ox, oy, oz);
            if d < best.1 {
                best = (i, d);
            }
        }
        let i = best.0;
        let (px, py, pz) = ((i & 4) == 0, (i & 2) == 0, (i & 1) == 0);
        (bx + i32::from(!px), by + i32::from(!py), bz + i32::from(!pz))
    }

    pub(crate) fn biome_quart(
        &mut self,
        qx: i32,
        qy: i32,
        qz: i32,
        last: Option<u16>,
    ) -> Option<(i64, u16)> {
        if self.preset == NoisePreset::End {
            let (cx, cz) = (qx.wrapping_mul(4) >> 4, qz.wrapping_mul(4) >> 4);
            let leaf = if i64::from(cx) * i64::from(cx) + i64::from(cz) * i64::from(cz) <= 4096 {
                0
            } else {
                let x = cx.wrapping_mul(2).wrapping_add(1).wrapping_mul(8);
                let z = cz.wrapping_mul(2).wrapping_add(1).wrapping_mul(8);
                let erosion = self.eval(
                    self.climate?[3],
                    x,
                    qy.wrapping_mul(4),
                    z,
                    &mut HashMap::new(),
                );
                if erosion > 0.25 {
                    1
                } else if erosion >= -0.0625 {
                    2
                } else if erosion < -0.21875 {
                    3
                } else {
                    4
                }
            };
            return Some((*self.biome_ids.get(leaf)?, leaf as u16));
        }
        if last.is_none() {
            if let Some(&leaf) = self.bioq_none.get(&(qx, qy, qz)) {
                return Some((*self.biome_ids.get(leaf as usize)?, leaf));
            }
        }
        let t = self.target7(qx, qy, qz)?;
        // vanilla seeds the search with the previous result
        let mut best = (i64::MAX, 0u16);
        if let Some(l) = last {
            if let Some(ps) = self.leaf_ps.get(l as usize) {
                best = (ps_dist(ps, &t), l);
            }
        }
        self.btree.as_ref()?.search(&t, &mut best);
        if last.is_none() {
            self.bioq_none.insert((qx, qy, qz), best.1);
            self.bioq_none_ord.push_back((qx, qy, qz));
            if self.bioq_none_ord.len() > 65536 {
                if let Some(old) = self.bioq_none_ord.pop_front() {
                    self.bioq_none.remove(&old);
                }
            }
        }
        let id = *self.biome_ids.get(best.1 as usize)?;
        Some((id, best.1))
    }
}

/// debug: (biome_id, [t,h,c,e,d,w,0]) at quart coords
pub fn biome_debug(qx: i32, qy: i32, qz: i32) -> Option<(i64, [i64; 7])> {
    let mut gg = gen_lock();
    let g = gg.as_mut()?;
    let t = g.target7(qx, qy, qz)?;
    let mut best = (i64::MAX, 0u16);
    g.btree.as_ref()?.search(&t, &mut best);
    Some((biome_id(&g.biome_names[best.1 as usize].clone()), t))
}

/// Block-space biome name through the same quart lookup as the feature layer.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub fn biome_name_at_block(x: i32, y: i32, z: i32) -> Option<String> {
    gen_lock().as_mut()?.biome_name_at(x, y, z)
}

/// quart-space biome id (public for debug/tests)
pub fn biome_at(qx: i32, qy: i32, qz: i32) -> Option<i64> {
    let mut gg = gen_lock();
    gg.as_mut()?.biome_quart(qx, qy, qz, None).map(|(b, _)| b)
}

/// 4x4x4 biome ids for one chunk section, quart-space index (y<<4|z<<2|x).
/// `MEGU_HM_DUMP=cx,cz` lives in `generation_context::hm_dump`: the live maps are
/// what a feature reads (`FC::hm_oc`/`hm_ws`), and `worldgen` may not reach for the
/// generation context (module layers).
///
/// `MEGU_BIOME_AT=x,y,z;x,y,z;...`: print both biome notions at each position.
/// The container (what a saved chunk holds) and the blended lookup that
/// `LevelReader.getBiome` returns are different answers; a placement filter reads
/// the second, so seeing both at once is how a boundary disagreement is read.
pub fn biome_at_dump(spec: &str) {
    let mut gg = gen_lock();
    let Some(g) = gg.as_mut() else { return };
    for item in spec.split(';') {
        let p: Vec<i32> = item.split(',').filter_map(|n| n.parse().ok()).collect();
        if p.len() != 3 {
            continue;
        }
        let (x, y, z) = (p[0], p[1], p[2]);
        let blended = g
            .biome_blended(x, y, z)
            .and_then(|(_, l)| g.biome_name_of_leaf(l));
        let container = match g.biome_quart(x >> 2, y >> 2, z >> 2, None) {
            Some((_, l)) => g.biome_name_of_leaf(l),
            None => None,
        };
        eprintln!(
            "BIOMEAT {x} {y} {z} blended={} container={}",
            blended.unwrap_or_else(|| "?".into()),
            container.unwrap_or_else(|| "?".into())
        );
    }
}

/// `MEGU_BIOME_DUMP=cx,sy,cz`: print the quart-sampled biome name for every
/// 4x4x4 cell of one section, to compare against a saved vanilla chunk.
pub fn biome_dump(spec: &str) {
    let p: Vec<i32> = spec.split(',').filter_map(|n| n.parse().ok()).collect();
    if p.len() != 3 {
        return;
    }
    let mut gg = gen_lock();
    let Some(g) = gg.as_mut() else { return };
    let (cx, sy, cz) = (p[0], p[1], p[2]);
    let (qx0, qy0, qz0) = (cx * 4, sy >> 2, cz * 4);
    let mut last: Option<u16> = None;
    for ly in 0..4 {
        for lz in 0..4 {
            for lx in 0..4 {
                if let Some((_, lid)) = g.biome_quart(qx0 + lx, qy0 + ly, qz0 + lz, last) {
                    last = Some(lid);
                    let name = g
                        .biome_names
                        .get(lid as usize)
                        .cloned()
                        .unwrap_or_else(|| format!("#{lid}"));
                    eprintln!(
                        "BDUMP {} {} {} {}",
                        (qx0 + lx) * 4,
                        (qy0 + ly) * 4,
                        (qz0 + lz) * 4,
                        name
                    );
                }
            }
        }
    }
}

pub fn biome_section(cx: i32, sy: i32, cz: i32) -> Option<Box<[i64; 64]>> {
    biome_section_dim(active_dim(), cx, sy, cz)
}

/// [`biome_section`] for one named dimension. An unset quart keeps the same default the
/// dim-less form used: 39 (plains) is the protocol-767 default biome and is what a caller
/// writes when the generator has no answer for that cell.
pub fn biome_section_dim(
    dimension: DimId,
    cx: i32,
    sy: i32,
    cz: i32,
) -> Option<Box<[i64; 64]>> {
    if let Some(biome) = custom_biome_id(dimension) {
        return Some(Box::new([biome; 64]));
    }
    ensure_dim_generator(dimension);
    let mut gg = gen_lock_dim(dimension);
    let g = gg.as_mut()?;
    let mut out = Box::new([39i64; 64]); // plains
    let (qx0, qy0, qz0) = (cx * 4, sy >> 2, cz * 4);
    let mut last: Option<u16> = None;
    for ly in 0..4 {
        for lz in 0..4 {
            for lx in 0..4 {
                if let Some((b, lid)) = g.biome_quart(qx0 + lx, qy0 + ly, qz0 + lz, last) {
                    out[((ly << 4) | (lz << 2) | lx) as usize] = b;
                    last = Some(lid);
                }
            }
        }
    }
    Some(out)
}

/// The generator lock, for tests outside `wg_tests` that drive `feature::populate`.
///
/// `wg_tests` is private, so the reachable path is this wrapper: a parent module can see a
/// child's private items, and the lock has to be the *same* mutex the generator tests use.
#[cfg(test)]
pub(crate) fn lock_gen_for_tests() -> std::sync::MutexGuard<'static, ()> {
    GEN_TEST_LOCK.lock().unwrap_or_else(|poison| {
        // This mutex only serializes tests; it protects no recoverable data.
        GEN_TEST_LOCK.clear_poison();
        poison.into_inner()
    })
}

#[cfg(all(test, megu_vanilla_data))]
mod wg_tests {
    use super::*;

    /// Issue #199: a surface_rule that fails to parse used to give up with only
    /// a `LOAD_TRACE`-gated line. Now the load records the exact
    /// `{kind, id, reason}` in the queryable `WorldgenParseReport` — and the
    /// report must name the file (or rule type) that failed, not just "it
    /// failed".
    #[test]
    fn p8b1_issue199_surface_rule_failures_are_recorded_not_dropped() {
        crate::test_support::require_vanilla_data();
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack");
        let empty = HashMap::new();
        let before = last_worldgen_parse_report().errors.len();

        // An unknown rule type: the deepest failure names the type.
        let rule = jparse(br#"{"type":"minecraft:bogus_rule"}"#).unwrap();
        assert!(
            g.rule_node(&rule, &empty, &empty, &empty, crate::name_to_state)
                .is_none()
        );
        let e = g.surface_rule_load_failure(NoisePreset::Overworld);
        assert_eq!(e.kind, "surface_rule");
        assert_eq!(e.id, "minecraft:overworld");
        assert!(
            e.reason.contains("unknown surface rule type") && e.reason.contains("bogus_rule"),
            "the reason must name the refused type: {e}"
        );
        let after = last_worldgen_parse_report();
        assert_eq!(after.errors.len(), before + 1);
        assert_eq!(after.errors[before], e, "the report is queryable");

        // A dangling material_rule reference names the missing file.
        let rule = J::Str("test:missing_rule".to_string());
        assert!(
            g.rule_node(&rule, &empty, &empty, &empty, crate::name_to_state)
                .is_none()
        );
        let e = g.surface_rule_load_failure(NoisePreset::Overworld);
        assert_eq!(e.id, "test:missing_rule");
        assert_eq!(e.reason, "missing or invalid JSON");

        // A node that is neither name nor object is malformed, not skipped.
        let rule = J::Num(1.0);
        assert!(
            g.rule_node(&rule, &empty, &empty, &empty, crate::name_to_state)
                .is_none()
        );
        let e = g.surface_rule_load_failure(NoisePreset::Overworld);
        assert_eq!(e.reason, "surface rule node is not a name or object");
    }

    /// The pinned datapack's noise_settings must resolve its surface_rule
    /// without a single recorded failure.
    #[test]
    fn p8b1_issue199_vanilla_gen_load_has_clean_surface_rules() {
        crate::test_support::require_vanilla_data();
        let before = last_worldgen_parse_report().of_kind("surface_rule").count();
        let g = Gen::load(0, crate::name_to_state).expect("pinned pack loads");
        assert!(g.surface_rule.is_some());
        assert!(g.rule_diag.is_none(), "a clean load leaves no pending diag");
        let after = last_worldgen_parse_report().of_kind("surface_rule").count();
        assert_eq!(after, before, "a clean vanilla load records nothing");
    }

    #[test]
    fn rule_seq_preserves_order_fallback_and_nested_conditions() {
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let false_cond = g.conds.len();
        g.conds.push(Cond::False);
        let hole_cond = g.conds.len();
        g.conds.push(Cond::Hole);
        let prelim_cond = g.conds.len();
        g.conds.push(Cond::Prelim);

        let first = g.rules.len();
        g.rules.push(Rule::Block(11));
        let second = g.rules.len();
        g.rules.push(Rule::Block(22));
        let fallback = g.rules.len();
        g.rules.push(Rule::Block(33));
        let never = g.rules.len();
        g.rules.push(Rule::Cond(false_cond, first));
        let maybe = g.rules.len();
        g.rules.push(Rule::Cond(hole_cond, second));
        let inner = g.rules.len();
        g.rules.push(Rule::Seq(vec![never, maybe].into_boxed_slice()));
        let guarded = g.rules.len();
        g.rules.push(Rule::Cond(prelim_cond, inner));
        let outer = g.rules.len();
        g.rules.push(Rule::Seq(vec![guarded, fallback].into_boxed_slice()));
        let direct = g.rules.len();
        g.rules.push(Rule::Seq(vec![first, second].into_boxed_slice()));
        let empty = g.rules.len();
        g.rules.push(Rule::Seq(Box::new([])));

        let mut c = SC::new();
        c.y = 64;
        c.est_min = 0;
        let heightmap = [0i16; 256];
        assert_eq!(g.eval_rule(direct, &mut c, &heightmap), Some(11));
        assert_eq!(g.eval_rule(outer, &mut c, &heightmap), Some(22));
        c.run_depth = 1;
        assert_eq!(g.eval_rule(outer, &mut c, &heightmap), Some(33));
        c.run_depth = 0;
        c.est_min = 100;
        assert_eq!(g.eval_rule(outer, &mut c, &heightmap), Some(33));
        assert_eq!(g.eval_rule(inner, &mut c, &heightmap), Some(22));
        assert_eq!(g.eval_rule(never, &mut c, &heightmap), None);
        assert_eq!(g.eval_rule(empty, &mut c, &heightmap), None);
    }

    #[test]
    fn datapack_resources_respect_priority_and_confine_id_paths() {
        let root = std::env::temp_dir().join(format!("mc-resource-paths-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let low = root.join("low");
        let high = root.join("high");
        for pack in [&low, &high] {
            std::fs::create_dir_all(pack.join("data/minecraft/structure/fossil")).unwrap();
        }
        std::fs::write(low.join("data/minecraft/structure/fossil/test.nbt"), b"low").unwrap();
        std::fs::write(
            high.join("data/minecraft/structure/fossil/test.nbt"),
            b"high",
        )
        .unwrap();
        let packs = [low, high.clone()];
        assert_eq!(
            Gen::dp_resource_in(&packs, "structure", "minecraft:fossil/test", "nbt"),
            Some(b"high".to_vec())
        );
        let readme = root.join("README.txt");
        std::fs::write(&readme, b"not a pack directory").unwrap();
        let entries = [packs[0].clone(), packs[1].clone(), readme];
        assert_eq!(
            Gen::dp_resource_in(&entries, "structure", "minecraft:fossil/test", "nbt"),
            Some(b"high".to_vec()),
            "ordinary datapacks/ entries must not mask real resources"
        );
        std::fs::create_dir_all(packs[0].join("data/minecraft/structure/nested")).unwrap();
        std::fs::write(
            packs[0].join("data/minecraft/structure/nested/test.nbt"),
            b"nested-low",
        )
        .unwrap();
        std::fs::write(
            high.join("data/minecraft/structure/nested"),
            b"not a directory",
        )
        .unwrap();
        assert_eq!(
            Gen::dp_resource_in(&packs, "structure", "minecraft:nested/test", "nbt"),
            Some(b"nested-low".to_vec()),
            "a non-directory prefix is an absent resource, not a shadowing resource"
        );
        std::fs::create_dir_all(high.join("structure")).unwrap();
        std::fs::write(high.join("structure/escape.nbt"), b"outside namespace").unwrap();
        std::fs::write(
            high.join("data/minecraft/escape.nbt"),
            b"outside resource kind",
        )
        .unwrap();
        // The targets EXIST: removing either path guard must really expose a file.
        for id in ["..:escape", "minecraft:../escape"] {
            assert!(
                Gen::dp_resource_in(&packs, "structure", id, "nbt").is_none(),
                "unsafe resource id resolved: {id}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_ticks_belong_to_their_generator() {
        let mut first = Gen::load(0, crate::name_to_state).unwrap();
        let mut second = Gen::load(1, crate::name_to_state).unwrap();
        let leaf = ((1, 80, 2), 264);
        first.schedule_generated_leaf(leaf.0, leaf.1);
        assert!(
            second.generated_leaf_ticks().is_empty(),
            "another generator's ticks leaked"
        );
        second.restore_generated_ticks(Vec::new(), Vec::new());
        assert_eq!(
            first.generated_leaf_ticks(),
            vec![leaf],
            "restore cleared another world"
        );
    }

    #[test]
    fn runtime_drains_only_published_ticks_without_the_gen_lock() {
        let _guard = lock_gen();
        assert!(init(0, crate::name_to_state));
        let _ = take_generated_leaf_ticks();
        let _ = take_generated_fluid_ticks();
        let leaf = ((1, 80, 2), 264);
        let fluid = ((3, 60, 4), 1);
        with_gen(|g| {
            g.schedule_generated_leaf(leaf.0, leaf.1);
            g.schedule_generated_leaf(leaf.0, leaf.1);
            g.schedule_generated_fluid(fluid.0, fluid.1);
        })
        .unwrap();
        let mut local = Gen::load(42, crate::name_to_state).unwrap();
        local.schedule_generated_leaf((9, 80, 9), 264);
        local.schedule_generated_fluid((9, 60, 9), 1);
        // Hold the generator while another thread drains: reintroducing an acquisition
        // here must fail promptly rather than deadlock the test process.
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = with_gen(|_| {
            let worker = std::thread::spawn(move || {
                tx.send((take_generated_leaf_ticks(), take_generated_fluid_ticks()))
                    .unwrap();
            });
            let got = rx.recv_timeout(std::time::Duration::from_secs(2));
            (worker, got)
        })
        .unwrap();
        worker.0.join().unwrap();
        let (leaves, fluids) = worker.1.expect("runtime drain waited on the generator");
        assert_eq!(
            leaves,
            vec![leaf],
            "unpublished generator leaked into runtime"
        );
        assert_eq!(
            fluids,
            vec![fluid],
            "unpublished generator leaked into runtime"
        );
        assert_eq!(local.generated_leaf_ticks(), vec![((9, 80, 9), 264)]);
        with_gen(|g| g.schedule_generated_leaf(leaf.0, leaf.1)).unwrap();
        assert_eq!(
            take_generated_leaf_ticks(),
            vec![leaf],
            "drain retained the dedup set"
        );
        with_gen(|g| g.restore_generated_ticks(vec![leaf], vec![fluid])).unwrap();
        local.restore_generated_ticks(Vec::new(), Vec::new());
        assert!(init(1, crate::name_to_state));
        assert!(take_generated_leaf_ticks().is_empty());
        assert!(take_generated_fluid_ticks().is_empty());
    }

    #[test]
    fn interpolation_uses_noise_preset_cell_dimensions() {
        for (cell_w, cell_h, min_y, world_height) in
            [(4, 8, -64, 384), (8, 4, 0, 128), (4, 8, 0, 128)]
        {
            let nx = (16 / cell_w + 1) as usize;
            let ny = (world_height / cell_h + 1) as usize;
            let columns = ChunkCols {
                grids: Vec::new(),
                surf: [0; 256],
                ny,
                cell_w,
                cell_h,
                min_y,
                mats: (0..256).map(|_| None).collect(),
                rd: [0; 256],
                wtop: [0; 256],
                beard: None,
                carve_sched: None,
            };
            let (cx, cz) = (-2, 3);
            let field = |x, y, z| f64::from(x) * 3.0 + f64::from(y) * 0.5 + f64::from(z) * 7.0;
            let mut grid = vec![0.0; nx * nx * ny];
            for zi in 0..nx {
                for xi in 0..nx {
                    for yi in 0..ny {
                        grid[(zi * nx + xi) * ny + yi] = field(
                            cx * 16 + xi as i32 * cell_w,
                            min_y + yi as i32 * cell_h,
                            cz * 16 + zi as i32 * cell_w,
                        );
                    }
                }
            }
            for z in cz * 16..(cz + 1) * 16 {
                for x in cx * 16..(cx + 1) * 16 {
                    for y in min_y..min_y + world_height {
                        assert_eq!(
                            trilinear(&grid, &columns, x, y, z).to_bits(),
                            field(x, y, z).to_bits(),
                            "cell {cell_w}x{cell_h}: {x},{y},{z}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn end_features_match_native_full_feature_chunks() {
        compare_end_features(
            -116,
            1262,
            "minecraft:end_stone",
            include_bytes!("../tests/data/end_small_features_1211.bin.gz"),
            include_str!("../tests/data/end_small_features_1211.order.tsv"),
        );
        compare_end_features(
            128,
            -128,
            "minecraft:chorus_plant",
            include_bytes!("../tests/data/end_highlands_features_1211.bin.gz"),
            include_str!("../tests/data/end_highlands_features_1211.order.tsv"),
        );
    }

    fn compare_end_features(
        cx0: i32,
        cz0: i32,
        positive_name: &str,
        compressed: &[u8],
        order: &str,
    ) {
        use std::io::Read;
        let mut expected = Vec::new();
        flate2::read::GzDecoder::new(compressed)
            .read_to_end(&mut expected)
            .unwrap();
        assert_eq!(expected.len(), 9 * 32768 * 4);
        let mut g = Gen::load_preset(0, crate::name_to_state, NoisePreset::End).unwrap();
        let order = order.lines().map(|line| {
            let (x, z) = line.split_once('\t').unwrap();
            (x.parse().unwrap(), z.parse().unwrap())
        });
        assert_eq!(g.populate_in_order(order, crate::feature::populate), 25);
        let mut states = expected.as_chunks::<4>().0.iter();
        let mut positive_features = 0;
        for cz in cz0..cz0 + 3 {
            for cx in cx0..cx0 + 3 {
                for y in 0..128 {
                    for z in cz * 16..(cz + 1) * 16 {
                        for x in cx * 16..(cx + 1) * 16 {
                            let want = u32::from_le_bytes(*states.next().unwrap());
                            let got = g.block_state_at(x, y, z, |_, _, _| {});
                            assert_eq!(got, want, "native End features: {x},{y},{z}");
                            positive_features += usize::from(
                                g.feature_override(x, y, z)
                                    .is_some_and(|s| breg().unwrap().name_of(s) == positive_name),
                            );
                        }
                    }
                }
            }
        }
        assert!(states.next().is_none());
        assert!(positive_features > 0, "positive decorated cells required");
    }

    #[test]
    fn nether_carvers_match_all_native_fixture_states() {
        use std::io::Read;
        let mut expected = Vec::new();
        flate2::read::GzDecoder::new(
            include_bytes!("../tests/data/nether_carvers_1211.bin.gz").as_slice(),
        )
        .read_to_end(&mut expected)
        .unwrap();
        assert_eq!(expected.len(), 9 * 32768 * 4);
        let mut g = Gen::load_preset(0, crate::name_to_state, NoisePreset::Nether).unwrap();
        for cz in 0..3 {
            for cx in 0..3 {
                crate::feature::apply_carvers(&mut g, cx, cz);
            }
        }
        let mut states = expected.as_chunks::<4>().0.iter();
        let mut cave_air = 0;
        for cz in 0..3 {
            for cx in 0..3 {
                for y in 0..128 {
                    for z in cz * 16..(cz + 1) * 16 {
                        for x in cx * 16..(cx + 1) * 16 {
                            let want = u32::from_le_bytes(*states.next().unwrap());
                            let got = g.block_state_at(x, y, z, |_, _, _| {});
                            assert_eq!(got, want, "native Nether carvers: {x},{y},{z}");
                            cave_air +=
                                usize::from(breg().unwrap().name_of(got) == "minecraft:cave_air");
                        }
                    }
                }
            }
        }
        assert!(states.next().is_none());
        assert!(cave_air > 0, "positive carved cells required");
    }

    #[test]
    fn dimension_terrain_matches_all_native_fixture_states() {
        use std::io::Read;
        type TerrainCase = (NoisePreset, u64, i32, i32, i32, &'static [u8]);
        let cases: &[TerrainCase] = &[
            (
                NoisePreset::End,
                0,
                0,
                0,
                1,
                include_bytes!("../tests/data/dimension_terrain_1211/0-end.bin.gz"),
            ),
            (
                NoisePreset::End,
                u64::MAX,
                100,
                -103,
                3,
                include_bytes!("../tests/data/dimension_terrain_1211/1-end.bin.gz"),
            ),
            (
                NoisePreset::Nether,
                0,
                0,
                0,
                1,
                include_bytes!("../tests/data/dimension_terrain_1211/2-nether.bin.gz"),
            ),
            (
                NoisePreset::Nether,
                u64::MAX,
                -65,
                32,
                3,
                include_bytes!("../tests/data/dimension_terrain_1211/3-nether.bin.gz"),
            ),
        ];
        let mut compared = 0;
        for &(preset, seed, cx, cz, n, bytes) in cases {
            let mut expected = Vec::new();
            flate2::read::GzDecoder::new(bytes)
                .read_to_end(&mut expected)
                .unwrap();
            assert_eq!(expected.len(), (n * n * 128 * 256 * 4) as usize);
            let mut g = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            assert_eq!((g.min_y, g.height), (0, 128));
            let mut states = expected.as_chunks::<4>().0.iter();
            for dz in 0..n {
                for dx in 0..n {
                    let columns = g.cols(cx + dx, cz + dz).unwrap();
                    for y in 0..128 {
                        for z in (cz + dz) * 16..(cz + dz + 1) * 16 {
                            for x in (cx + dx) * 16..(cx + dx + 1) * 16 {
                                let want = u32::from_le_bytes(*states.next().unwrap());
                                assert_eq!(
                                    g.terrain_cell(&columns, x, y, z),
                                    want,
                                    "{preset:?} seed={seed} at {x},{y},{z}"
                                );
                                compared += 1;
                            }
                        }
                    }
                }
            }
            assert!(states.next().is_none());
        }
        assert_eq!(compared, 655_360);
    }

    /// The whole Overworld chunk the pinned server produced, at canonical-state level.
    ///
    /// The capture is a real 1.21.1 server run (`tests/data/overworld_chunk_0_0_1211.tsv.gz`,
    /// seed 0, chunk 0,0, features on, no players). Two runs of that input differed in four
    /// cells, so the port is compared against the run it matches; the four noisy cells are
    /// named in the provenance rather than tolerated here.
    #[test]
    fn overworld_chunk_matches_native_capture_state_for_state() {
        use std::io::Read;
        let mut expected_text = String::new();
        flate2::read::GzDecoder::new(&include_bytes!(
            "../tests/data/overworld_chunk_0_0_1211.tsv.gz"
        )[..])
        .read_to_string(&mut expected_text)
        .unwrap();
        let reg = crate::block_registry::breg().unwrap();
        let mut want: std::collections::HashMap<(i32, i32, i32), String> =
            std::collections::HashMap::new();
        for line in expected_text.lines() {
            if line.starts_with('#') {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 4, "{line}");
            want.insert(
                (
                    f[0].parse().unwrap(),
                    f[1].parse().unwrap(),
                    f[2].parse().unwrap(),
                ),
                f[3].to_owned(),
            );
        }
        assert_eq!(want.len(), 32_518, "the capture carries its full cell count");

        // `terrain_section` reads the process-global generator, so hold its lock (the
        // parallel rounds run this beside other world tests) and initialise seed 0.
        let _guard = lock_gen();
        assert!(init(0, crate::name_to_state), "generator for the captured seed");
        let (lo, hi) = (min_y(), max_y());
        let mut got: std::collections::HashMap<(i32, i32, i32), String> =
            std::collections::HashMap::new();
        let mut sy = lo;
        while sy <= hi {
            let arr = terrain_section(0, sy, 0, crate::feature::populate).unwrap();
            for ly in 0..16 {
                for lz in 0..16 {
                    for lx in 0..16 {
                        let st = arr[((ly << 8) | (lz << 4) | lx) as usize];
                        if st == 0 {
                            continue;
                        }
                        got.insert(
                            (lx, sy + ly, lz),
                            reg.canonical_state(st),
                        );
                    }
                }
            }
            sy += 16;
        }
        type CellDiff<'a> = ((i32, i32, i32), Option<&'a String>, Option<&'a String>);
        let mut diffs: Vec<CellDiff> = Vec::new();
        for key in want.keys().chain(got.keys()) {
            if want.get(key) != got.get(key) && !diffs.iter().any(|d| d.0 == *key) {
                diffs.push((*key, want.get(key), got.get(key)));
            }
        }
        assert!(
            diffs.is_empty(),
            "{} cells differ from the native capture, first few: {:?}",
            diffs.len(),
            &diffs[..diffs.len().min(5)]
        );
    }

    /// The state-for-state gate for a chunk whose content is a *structure*, not terrain.
    ///
    /// Seed 4242 chunk (-96,35) carries a mineshaft corridor, a stronghold and a trial
    /// chamber. Four of its cells are the residual this fixes: the `oak_fence` pair at
    /// x = -1536, z = 565 keeps `east+north+west` (two `oak_log` pillars and a fence on
    /// its west side) and the pair at z = 562 keeps *no* connection bit, because native's
    /// `StructurePiece.placeBlock` marks those cells with `markPosForPostprocessing` and
    /// `LevelChunk.postProcessGeneration` re-derives them from the world as it stands when
    /// the chunk is promoted — which `ChunkMap.protoChunkToFullChunk` only allows once the
    /// whole 3x3 around it is `FULL`.
    ///
    /// It also pins the other half of that pass: the two `glow_lichen` cells at
    /// (-1521,-30,573..574) must be absent. The trial chamber template writes them with
    /// `east=true`, their support cell lies outside the placement box, and
    /// `MultifaceBlock.updateShape` clears the face and then the cell.
    #[test]
    fn structure_chunk_matches_native_capture_state_for_state() {
        use std::io::Read;
        let mut expected_text = String::new();
        flate2::read::GzDecoder::new(
            &include_bytes!("../tests/data/stronghold_chunk_-96_35_1211.tsv.gz")[..],
        )
        .read_to_string(&mut expected_text)
        .unwrap();
        let reg = crate::block_registry::breg().unwrap();
        let mut want: std::collections::HashMap<(i32, i32, i32), String> =
            std::collections::HashMap::new();
        for line in expected_text.lines() {
            if line.starts_with('#') {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 4, "{line}");
            want.insert(
                (
                    f[0].parse().unwrap(),
                    f[1].parse().unwrap(),
                    f[2].parse().unwrap(),
                ),
                f[3].to_owned(),
            );
        }
        assert_eq!(want.len(), 31_721, "the capture carries its full cell count");

        let _guard = lock_gen();
        // The generator reaches the structure pipeline's beardifier through a
        // registered function pointer (`worldgen` sits below `feature`), and the
        // process entry point registers it before any dump. Without it the terrain is
        // unbearded, which moves the trial chamber's `encapsulate` shell and every block
        // the surface then sees.
        crate::feature::register_beard_resolver();
        assert!(init(4242, crate::name_to_state), "generator for the captured seed");
        let (lo, hi) = (min_y(), max_y());
        let mut got: std::collections::HashMap<(i32, i32, i32), String> =
            std::collections::HashMap::new();
        let mut sy = lo;
        while sy <= hi {
            let arr = terrain_section(-96, sy, 35, crate::feature::populate).unwrap();
            for ly in 0..16 {
                for lz in 0..16 {
                    for lx in 0..16 {
                        let st = arr[((ly << 8) | (lz << 4) | lx) as usize];
                        if st == 0 {
                            continue;
                        }
                        got.insert(
                            (-96 * 16 + lx, sy + ly, 35 * 16 + lz),
                            reg.canonical_state(st),
                        );
                    }
                }
            }
            sy += 16;
        }
        type CellDiff<'a> = ((i32, i32, i32), Option<&'a String>, Option<&'a String>);
        let mut diffs: Vec<CellDiff> = Vec::new();
        for key in want.keys().chain(got.keys()) {
            if want.get(key) != got.get(key) && !diffs.iter().any(|d| d.0 == *key) {
                diffs.push((*key, want.get(key), got.get(key)));
            }
        }
        assert!(
            diffs.is_empty(),
            "{} cells differ from the native capture, first few: {:?}",
            diffs.len(),
            &diffs[..diffs.len().min(5)]
        );
    }

    #[test]
    fn nether_cave_surfaces_and_lava_match_native_negative_seed() {
        let mut g = Gen::load_preset(u64::MAX, crate::name_to_state, NoisePreset::Nether).unwrap();
        assert_eq!(g.def_fl, crate::name_to_state("minecraft:lava").unwrap());
        assert!(g.aq.is_none());
        for (y, name) in [(27, "minecraft:soul_sand"), (30, "minecraft:lava")] {
            assert_eq!(
                g.terrain_state(-995, y, 512),
                crate::name_to_state(name).unwrap(),
                "native cell -995,{y},512"
            );
        }
    }

    #[test]
    fn nether_surface_depth_and_named_random_match_native() {
        let mut current = None;
        let mut generator = None;
        for row in include_str!("../tests/data/nether_surface_1211.tsv").lines() {
            let p: Vec<_> = row.split('\t').collect();
            let seed = parse_world_seed(p[0]).unwrap();
            if current != Some(seed) {
                generator = Some(
                    Gen::load_preset(seed, crate::name_to_state, NoisePreset::Nether).unwrap(),
                );
                current = Some(seed);
            }
            let x = p[1].parse().unwrap();
            let z = p[2].parse().unwrap();
            assert_eq!(
                generator.as_ref().unwrap().run_depth(x, z),
                p[3].parse::<i32>().unwrap(),
                "{row}"
            );
            let factory = legacy_named_factory_seed(seed, p[4]);
            let y = p[5].parse().unwrap();
            let value = Lcg::new(factory ^ hash_block_pos(x, y, z) as u64).next_f32();
            assert_eq!(
                value.to_bits(),
                u32::from_str_radix(p[6], 16).unwrap(),
                "{row}"
            );
        }
    }

    #[test]
    fn nether_biome_source_matches_native_nether_random_state() {
        let mut current = None;
        let mut generator = None;
        let mut names = std::collections::BTreeSet::new();
        for row in include_str!("../tests/data/nether_biome_1211.tsv").lines() {
            let p: Vec<_> = row.split('\t').collect();
            let seed = parse_world_seed(p[0]).unwrap();
            if current != Some(seed) {
                generator = Some(
                    Gen::load_preset(seed, crate::name_to_state, NoisePreset::Nether)
                        .expect("Nether datapack"),
                );
                current = Some(seed);
            }
            let g = generator.as_mut().unwrap();
            let (_, leaf) = g
                .biome_quart(
                    p[1].parse().unwrap(),
                    p[2].parse().unwrap(),
                    p[3].parse().unwrap(),
                    None,
                )
                .expect("Nether biome");
            let name = &g.biome_names[usize::from(leaf)];
            assert_eq!(name, p[4], "{row}");
            names.insert(name.clone());
        }
        assert_eq!(names.len(), 5);
    }

    #[test]
    fn signed_world_seed_preserves_java_long_bits() {
        assert_eq!(parse_world_seed("-1").unwrap(), u64::MAX);
        assert_eq!(parse_world_seed("-9223372036854775808").unwrap(), 1 << 63);
        assert_eq!(parse_world_seed("18446744073709551615").unwrap(), u64::MAX);
        assert!(parse_world_seed("unknown").is_err());
    }

    #[test]
    fn end_biome_source_matches_native_end_random_state() {
        let mut current = None;
        let mut generator = None;
        let mut names = std::collections::BTreeSet::new();
        let mut count = 0;
        for row in include_str!("../tests/data/end_biome_1211.tsv").lines() {
            let f: Vec<_> = row.split('\t').collect();
            assert_eq!(f.len(), 5);
            let seed = f[0].parse::<i64>().unwrap() as u64;
            if current != Some(seed) {
                generator = Some(
                    Gen::load_preset(seed, crate::name_to_state, NoisePreset::End)
                        .expect("End datapack"),
                );
                current = Some(seed);
            }
            let g = generator.as_mut().unwrap();
            let (_, leaf) = g
                .biome_quart(
                    f[1].parse().unwrap(),
                    f[2].parse().unwrap(),
                    f[3].parse().unwrap(),
                    None,
                )
                .unwrap();
            assert_eq!(
                g.biome_name_of_leaf(leaf).as_deref(),
                Some(f[4]),
                "native End biome: {row}"
            );
            names.insert(f[4]);
            count += 1;
        }
        assert_eq!(count, 2352);
        assert_eq!(
            names,
            std::collections::BTreeSet::from([
                "minecraft:the_end",
                "minecraft:end_highlands",
                "minecraft:end_midlands",
                "minecraft:small_end_islands",
                "minecraft:end_barrens"
            ])
        );
    }

    #[test]
    fn end_island_density_node_uses_the_generators_seed() {
        for seed in [0u64, 42] {
            let mut g = Gen::load(seed, crate::name_to_state).expect("pinned datapack");
            let config = jparse(br#"{"type":"minecraft:end_islands"}"#).unwrap();
            let index = g.node(&config, &HashMap::new()).expect("End density node");
            let mut checked = 0;
            for row in include_str!("../tests/data/end_islands_1211.tsv").lines() {
                let f: Vec<_> = row.split('\t').collect();
                if f[0].parse::<i64>().unwrap() as u64 != seed {
                    continue;
                }
                let (x, z) = (f[1].parse::<i32>().unwrap(), f[2].parse::<i32>().unwrap());
                let actual = g.eval(index, x, 64, z, &mut HashMap::new());
                assert_eq!(
                    actual.to_bits(),
                    u64::from_str_radix(f[4], 16).unwrap(),
                    "production End density: {row}"
                );
                checked += 1;
            }
            assert_eq!(checked, 80);
        }
    }

    #[test]
    fn unsupported_density_types_fail_closed_at_every_reference_boundary() {
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned datapack");
        let empty = HashMap::new();

        // Unknown nodes must never masquerade as a legitimate zero function.
        let unknown = jparse(br#"{"type":"minecraft:unsupported_density"}"#).unwrap();
        assert!(g.node(&unknown, &empty).is_none());

        // A required child inside a valid operator must propagate the failure.
        let nested = jparse(
            br#"{"type":"minecraft:add","argument1":1,"argument2":{"type":"minecraft:unsupported_density"}}"#,
        )
        .unwrap();
        assert!(g.node(&nested, &empty).is_none());

        // A failing named definition must not leave the recursion placeholder
        // cached as a valid reference on subsequent loads.
        let mut files = HashMap::new();
        files.insert(
            "minecraft:broken".to_string(),
            br#"{"type":"minecraft:unsupported_density"}"#.to_vec(),
        );
        for _ in 0..2 {
            assert!(g.resolve_ref(&files, "minecraft:broken").is_none());
            assert!(!g.names.contains_key("minecraft:broken"));
        }

        // An intentional numeric zero remains valid and must still evaluate.
        let valid = jparse(br#"{"type":"minecraft:add","argument1":2,"argument2":0}"#).unwrap();
        let id = g.node(&valid, &empty).expect("known density operators");
        assert_eq!(g.eval(id, 0, 0, 0, &mut HashMap::new()), 2.0);
    }

    #[test]
    fn shifted_noise_preserves_y_and_rejects_invalid_required_shifts() {
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned datapack");
        let empty = HashMap::new();
        let good = jparse(
            br#"{"type":"minecraft:shifted_noise","noise":"minecraft:temperature","shift_x":0,"shift_y":3,"shift_z":0,"xz_scale":1,"y_scale":1}"#,
        )
        .unwrap();
        let id = g.node(&good, &empty).expect("valid shifted noise");
        let DF::Noise { sy, .. } = &g.dfs[id] else {
            panic!("shifted_noise did not use the noise evaluator");
        };
        match &g.dfs[*sy] {
            DF::C(y) => assert_eq!(*y, 3.0, "nonzero shift_y must not be dropped"),
            _ => panic!("expected literal y shift"),
        }

        // The native codec requires all three shifts. Missing or unparseable
        // children must not be silently replaced by a zero density.
        for invalid in [
            r#"{"type":"minecraft:shifted_noise","noise":"minecraft:temperature","shift_x":0,"shift_z":0,"xz_scale":1,"y_scale":1}"#,
            r#"{"type":"minecraft:shifted_noise","noise":"minecraft:temperature","shift_x":0,"shift_y":{"type":"minecraft:unsupported_density"},"shift_z":0,"xz_scale":1,"y_scale":1}"#,
            r#"{"type":"minecraft:noise","noise":"minecraft:temperature","shift_x":{"type":"minecraft:unsupported_density"},"xz_scale":1,"y_scale":1}"#,
        ] {
            assert!(g.node(&jparse(invalid.as_bytes()).unwrap(), &empty).is_none(), "{invalid}");
        }
    }

    #[test]
    fn the_surface_material_window_stops_at_deep_solids() {
        assert!(surface_material_applies(62, 62));
        assert!(surface_material_applies(59, 62));
        assert!(surface_material_applies(58, 62));
        assert!(!surface_material_applies(57, 62));
        assert!(!surface_material_applies(33, 62));
    }

    /// The terrain stage files a mark on the cell the fill wrote a *settled* fluid into, and
    /// nowhere else. Measured cell: seed 4242, `(98,19,-513)` — the source the village-taiga
    /// capture's promotion pass turns into `water[level=1]` at `(98,19,-512)`. Its neighbour
    /// `(98,20,-513)` is stone (no fluid, no mark), and `(98,19,-512)` is air the fill left
    /// alone (no fluid, no mark) with the aquifer's flag still set — the case that makes "the
    /// flag is enough" wrong.
    #[test]
    fn the_fill_marks_a_settled_aquifer_fluid_and_nothing_else() {
        let _guard = lock_gen();
        assert!(init(4242, crate::name_to_state));
        let Some(data) = data_snapshot() else {
            return;
        };
        let mut g = Gen::fresh_scratch(data);
        let cc = g.cols(6, -33).expect("columns for the measured chunk");
        let water = breg().expect("registry").state_of("minecraft:water", None);

        let (state, marked) = g.terrain_cell_marked(&cc, 98, 19, -513);
        assert_eq!(state, water, "the fill writes the aquifer's water here");
        assert!(g.aq_schedule_fluid_update(), "the aquifer calls this cell settled");
        assert_eq!(marked, MARK_FILL, "so the fill files one mark for it");

        // Below the fluid the fill wrote stone: no fluid, no mark, whatever the flag says.
        let (state, marked) = g.terrain_cell_marked(&cc, 98, 20, -513);
        assert!(!breg().expect("registry").is_air(state));
        assert_eq!(marked, 0, "a solid cell carries no fluid");
    }

    #[test]
    fn a_nonpositive_density_with_an_aquifer_barrier_is_still_solid() {
        assert_eq!(
            classify_surface_cell(-0.01, None, false, false),
            SurfaceCell::Solid
        );
        assert_eq!(
            classify_surface_cell(-0.01, Some(1), false, false),
            SurfaceCell::Solid
        );
        assert_eq!(
            classify_surface_cell(-0.01, Some(0), true, false),
            SurfaceCell::Air
        );
        assert_eq!(
            classify_surface_cell(-0.01, Some(2), false, true),
            SurfaceCell::Fluid
        );
        assert_eq!(
            classify_surface_cell(0.01, Some(0), true, false),
            SurfaceCell::Solid
        );
    }

    // ---- compiled density evaluator ----

    /// Presets to compare on, and the seeds; the overworld is the one that
    /// compiles to a real program, the other two prove the compiler either
    /// matches there too or declines.
    const COMPILE_CASES: [(NoisePreset, u64); 4] = [
        (NoisePreset::Overworld, 0),
        (NoisePreset::Overworld, 4242),
        (NoisePreset::Nether, 4242),
        (NoisePreset::End, 0),
    ];

    /// The compiled `final_density` program must reproduce the interpreter's
    /// value **bit for bit**. Density is compared against `> 0.0` and feeds the
    /// trilinear cell grid, so a last-bit difference is a different block
    /// somewhere in the world; "close" is not a property this change may have.
    ///
    /// The reference is the interpreter, which is the evaluator the fixtures pin
    /// against vanilla (`tests/data/dimension_terrain_1211`, the seed-0
    /// overworld gate). This test holds the two evaluators together; whether it
    /// can fail at all is `tools/mutation_check.sh`'s business, not this test's.
    #[test]
    fn compiled_density_matches_interpreter_per_block() {
        let _guard = lock_gen();
        let mut compared = 0usize;
        for (preset, seed) in COMPILE_CASES {
            let mut g = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            let Some(prog) = DfProg::compile_inner(&g) else {
                // Declined: the DAG is a single call (`old_blended_noise` /
                // `end_islands` root), so there is no program to compare. The
                // interpreter is still the only evaluator there, which is why
                // this is allowed rather than a failure.
                continue;
            };
            for (cx, cz) in [(0, 0), (3, -2), (-1, -1)] {
                let cc = g.cols(cx, cz).unwrap();
                let cc = cc.as_ref();
                let mut y = g.min_y;
                while y < g.min_y + g.height {
                    for lz in 0..16 {
                        for lx in 0..16 {
                            let (x, z) = (cx * 16 + lx, cz * 16 + lz);
                            let mut memo = HashMap::new();
                            let want =
                                g.eval_c(g.final_density, x, y, z, &mut memo, Some(cc));
                            let got = prog.eval(&g, Some(cc), x, y, z);
                            assert_eq!(
                                want.to_bits(),
                                got.to_bits(),
                                "{preset:?} seed {seed} at {x},{y},{z}: interpreter {want:?} program {got:?}"
                            );
                            compared += 1;
                        }
                    }
                    y += 7;
                }
            }
        }
        assert!(
            compared > 40_000,
            "only {compared} positions were compared; the comparison loop did not run"
        );
    }

    /// The same identity without a chunk's cell grids, which is the corner
    /// sampling the grid fill does: an `interpolated` node must fall through to
    /// its input, and the Beardifier marker must be the zero vanilla evaluates
    /// when there is no chunk to wrap.
    #[test]
    fn compiled_density_matches_interpreter_without_chunk_grids() {
        let _guard = lock_gen();
        let mut compared = 0usize;
        let mut compiled = 0usize;
        for (preset, seed) in COMPILE_CASES {
            let g = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            let Some(prog) = DfProg::compile_inner(&g) else {
                continue;
            };
            compiled += 1;
            for x in [-17, -1, 0, 7, 15, 16, 31] {
                for z in [-5, 0, 9] {
                    let mut y = g.min_y;
                    while y < g.min_y + g.height {
                        let mut memo = HashMap::new();
                        let want = g.eval_c(g.final_density, x, y, z, &mut memo, None);
                        let got = prog.eval(&g, None, x, y, z);
                        assert_eq!(
                            want.to_bits(),
                            got.to_bits(),
                            "{preset:?} seed {seed} at {x},{y},{z} without columns"
                        );
                        compared += 1;
                        y += 3;
                    }
                }
            }
        }
        assert!(
            compared > 3_000,
            "only {compared} positions were compared; the comparison loop did not run"
        );
        assert_eq!(
            compiled, 4,
            "every preset must compile here, or this test stops covering the compiler"
        );
    }

    // ---- BOT-DF-BATCH: the corner-grid column evaluator ----

    /// The `DfCols` fill must write the same corner-grid cells the `eval_flat`
    /// reference wrote, bit for bit. The comparison is `to_bits` for the same
    /// reason `compiled_density_matches_interpreter_per_block`'s is: a cell is
    /// a trilinear weight and a `> 0.0` input, so a last-bit difference is a
    /// different block somewhere in the world.
    ///
    /// Both arms run `build_cols` end to end — the reference under
    /// `dfcol_test_force_flat` and the compiled path as shipped — on separate
    /// generators so a cache populated under one arm cannot answer the other.
    /// The `DFCOL_BEGINS` count is the proof the compiled arm actually ran the
    /// hoisted prefix rather than comparing the reference against itself.
    #[test]
    fn bot_df_batch_parity_and_bench() {
        let _guard = lock_gen();
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                dfcol_test_force_flat(false);
                df4_test_force_scalar(false);
            }
        }
        let _restore = Restore;

        // Parity: four chunks on each preset that produced a column program,
        // once on the vector arms and once with `df4_test_force_scalar` so the
        // scalar `df_run` tail is held to the same bytes.
        let mut compared = 0usize;
        let mut total_begins = 0u64;
        for (preset, seed) in COMPILE_CASES {
            let mut g_ref = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            let mut g_new = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            if g_new.dfcols.is_none() && g_new.vfcols.is_none() {
                continue;
            }
            let nx = (16 / g_new.noise_cell_w + 1) as u64;
            for scalar in [false, true] {
                df4_test_force_scalar(scalar);
                for (cx, cz) in [(0, 0), (3, -2), (-7, 5), (12, 9)] {
                    dfcol_test_force_flat(true);
                    let want = g_ref.build_cols(cx, cz, true).expect("reference columns");
                    dfcol_test_force_flat(false);
                    let _ = dfcol_test_begins();
                    let got = g_new.build_cols(cx, cz, true).expect("columns");
                    total_begins += dfcol_test_begins();
                    assert_eq!(want.grids.len(), got.grids.len());
                    for (slot, (a, b)) in want.grids.iter().zip(&got.grids).enumerate() {
                        assert_eq!(a.is_some(), b.is_some(), "grid {slot} presence differs");
                        let (Some(a), Some(b)) = (a, b) else {
                            continue;
                        };
                        assert_eq!(a.len(), b.len());
                        for (cell, (&va, &vb)) in a.iter().zip(b.iter()).enumerate() {
                            assert_eq!(
                                va.to_bits(),
                                vb.to_bits(),
                                "{preset:?} seed {seed} chunk ({cx},{cz}) grid {slot} cell \
                                 {cell} (scalar={scalar}): reference {va:?} compiled {vb:?}"
                            );
                            compared += 1;
                        }
                    }
                }
            }
            df4_test_force_scalar(false);
            assert!(
                total_begins >= nx * nx,
                "{preset:?} seed {seed}: the compiled column fill ran {total_begins} prefixes, \
                 fewer than one column grid's {nx}x{nx}"
            );
        }
        assert!(
            compared > 10_000,
            "only {compared} cells were compared; the comparison loop did not run"
        );

        // Bench: `cols()` over 32 chunks on each arm of the Overworld generator.
        // `load_preset_uncached`, not `load_preset`: the test cache hands every
        // arm the same `Arc<GenData>`, and `cols()` answers misses through the
        // shared `fallback_share` `FlightCache` inside it — under the cache one
        // arm would be timing the other's hits.
        const N: i32 = 32;
        let bench = |flat: bool| -> f64 {
            let mut g = Gen::load_preset_uncached(0, crate::name_to_state, NoisePreset::Overworld)
                .expect("generator for the bench");
            dfcol_test_force_flat(flat);
            let t0 = std::time::Instant::now();
            for k in 0..N {
                std::hint::black_box(g.cols(31 + k, -101).expect("bench columns"));
            }
            dfcol_test_force_flat(false);
            t0.elapsed().as_secs_f64() * 1_000.0 / N as f64
        };
        let flat_ms = bench(true);
        let col_ms = bench(false);
        let g = Gen::load_preset_uncached(0, crate::name_to_state, NoisePreset::Overworld)
            .expect("generator for the stats");
        if let Some(d) = g.data.dfcols.as_deref() {
            println!(
                "bot_df_batch: ops2={} n2={} progs={:?}",
                d.ops2.len(),
                d.n2,
                d.progs.iter().map(|p| p.len()).collect::<Vec<_>>()
            );
        }
        println!(
            "bot_df_batch: eval_flat {flat_ms:.3} ms/chunk, dfcols {col_ms:.3} ms/chunk \
             ({:.2}x) over {N} chunks",
            flat_ms / col_ms
        );
    }

    // ---- PERF-WORLDGEN-SPEED: the 16-chunk digest + timing gate ----

    /// Every block state of a 4x4 chunk grid through `terrain_section_at` — the
    /// same function the streaming path serves — folded into one md5, so a
    /// "same timing, different bytes" optimization cannot pass. The grid sits
    /// on mixed terrain at seed 0 and the assertion that it spans more than
    /// one biome keeps the fixture honest: a chunk grid clipped to a single
    /// biome would not exercise the feature paths this lane touches.
    ///
    /// `feature::populate` runs inside the measured window the way it runs on
    /// the serving path: the first `feat_state` of a chunk settles its whole
    /// population (carvers included), so the per-chunk figure is the full
    /// generation cost, not the terrain fill alone. A fresh scratch generator
    /// keeps the measurement off the shared `GenData` flight caches.
    #[test]
    fn perf_worldgen_speed_16chunk_bit_identical_and_bench() {
        let _guard = lock_gen();
        let local = Gen::load_preset_uncached(0, crate::name_to_state, NoisePreset::Overworld)
            .expect("generator for the 16-chunk bench");
        let mut g = Gen::fresh_scratch(std::sync::Arc::clone(&local.data));
        drop(local);
        let lo = g.min_y;
        let hi = g.min_y + g.height - 16;
        m3_reset_local();
        let mut bytes: Vec<u8> = Vec::new();
        let mut cells = 0u64;
        let mut elapsed = std::time::Duration::ZERO;
        for cz in -2..2 {
            for cx in -2..2 {
                let mut sy = lo;
                while sy <= hi {
                    let s0 = std::time::Instant::now();
                    let arr = g.terrain_section_at(cx, sy, cz, crate::feature::populate);
                    elapsed += s0.elapsed();
                    for &st in arr.iter() {
                        bytes.extend_from_slice(&st.to_le_bytes());
                    }
                    cells += arr.len() as u64;
                    sy += 16;
                }
            }
        }
        let sections = (hi - lo) / 16 + 1;
        assert_eq!(
            cells,
            16 * sections as u64 * 4096,
            "the 16-chunk grid must have produced every cell"
        );
        // The grid must span more than one biome, or the feature-layer timings
        // it reports describe a corner of the input space this lane does not
        // optimise for.
        let mut biome_names: Vec<String> = Vec::new();
        for (cx, cz) in (-2..2).flat_map(|cz| (-2..2).map(move |cx| (cx, cz))) {
            if let Some(set) = g.chunk_biome_set_readonly(cx, cz) {
                for b in set.iter() {
                    if !biome_names.iter().any(|n| n == b) {
                        biome_names.push(b.clone());
                    }
                }
            }
        }
        assert!(
            biome_names.len() > 1,
            "the 16-chunk bench grid must span multiple biomes, got {biome_names:?}"
        );
        let digest: String = crate::generation_math::md5(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        println!(
            "perf_worldgen_speed: {} biomes, {} bytes, {:.1} us/chunk over 16 chunks",
            biome_names.len(),
            bytes.len(),
            elapsed.as_secs_f64() * 1e6 / 16.0
        );
        if m3_on() {
            eprintln!("{}", m3_report());
        }
        assert_eq!(
            digest, "e7020b651cd5bf136f7bbac0f3c844f5",
            "16-chunk worldgen digest changed"
        );
    }


    // ---- M3 P4: the four-lane density evaluator ----

    /// The values the four-lane ops are held to: the ones where an IEEE-754 operation on one
    /// lane can disagree with the hardware's vector form (the two zeros, the two infinities,
    /// quiet and signalling NaN, subnormals, `f64::MIN`/`MAX`) plus the constants the
    /// Overworld spine actually multiplies by.
    const DF4_EDGES: [f64; 22] = [
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        1.5625,
        -1.5625,
        24.0,
        -24.0,
        0.1171875,
        -0.078125,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        -f64::from_bits(1),
        f64::MAX,
        f64::MIN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        f64::from_bits(0xfff8_0000_0000_0001),
        1.0e300,
    ];

    /// A hand-built program for the op under test: `slots` is generous, and the vector arm
    /// refuses a program deeper than `DF4_SLOTS`, so these all take it.
    fn df4_prog(ops: Vec<Op>) -> DfProg {
        DfProg { ops, slots: 4 }
    }

    /// Every op the evaluator implements, over the edge values, one lane group at a time.
    ///
    /// The reference is `DfProg::run` - the one-lane interpreter the world's bytes are built
    /// from - compared with `to_bits`, which is the only comparison this can be: density is
    /// consumed by `> 0.0` and by the trilinear cells, so a last-bit difference is a
    /// different block somewhere.
    #[test]
    fn df4_ops_match_the_scalar_ops_over_edge_values() {
        let _guard = lock_gen();
        let _measurement = M3_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let g = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld)
            .expect("generator for the fixture seed");
        df4_test_force_scalar(false);
        m3_test_force(true);
        m3_reset_local();
        let mut compared = 0usize;
        let mut nan_results = 0usize;
        let mut check = |ops: Vec<Op>, what: &str, a: f64, b: f64| {
            let prog = df4_prog(ops);
            for x in [-8, 0, 5] {
                let want: [f64; DF4] = std::array::from_fn(|k| prog.eval(&g, None, x + k as i32, 7, 3));
                let got = prog.eval_x4(&g, None, x, 7, 3);
                for k in 0..DF4 {
                    // A NaN *result* is the one place the two lowerings are allowed to
                    // disagree: with two NaN operands neither the scalar op nor `vaddp*` is
                    // specified to keep which payload, so both answer "a NaN" with possibly
                    // different bits. Nothing reads a NaN's payload - every consumer of the
                    // density is a comparison (`> 0.0`, the aquifer's blends) and the corner
                    // grid the bytes come from is filled on the one-lane path - so the
                    // comparison below is "identical, or both NaN".
                    if want[k].is_nan() && got[k].is_nan() {
                        nan_results += 1;
                        continue;
                    }
                    assert_eq!(
                        want[k].to_bits(),
                        got[k].to_bits(),
                        "{what} at lane {k} of x={x}: {a:?} / {b:?} -> scalar {:?} vector {:?}",
                        want[k],
                        got[k]
                    );
                    compared += 1;
                }
            }
        };
        let unary: [(&str, Op); 8] = [
            ("neg", Op::Neg),
            ("abs", Op::Abs),
            ("sq", Op::Sq),
            ("cube", Op::Cube),
            ("hneg", Op::HNeg),
            ("qneg", Op::QNeg),
            ("squeeze", Op::Squeeze),
            ("clamp(-1,1)", Op::Clamp { lo: -1.0, hi: 1.0 }),
        ];
        for (name, op) in unary {
            for v in DF4_EDGES {
                check(vec![Op::Push(v), op, Op::Halt], name, v, v);
            }
        }
        let binary: [(&str, Op); 6] = [
            ("add", Op::Add),
            ("sub", Op::Sub),
            ("mul", Op::Mul),
            ("div", Op::Div),
            ("min", Op::Min),
            ("max", Op::Max),
        ];
        for (name, op) in binary {
            for a in DF4_EDGES {
                for b in DF4_EDGES {
                    check(vec![Op::Push(a), Op::Push(b), op, Op::Halt], name, a, b);
                }
            }
        }
        for c in DF4_EDGES {
            for v in DF4_EDGES {
                check(vec![Op::Push(v), Op::AddC(c), Op::Halt], "addC", v, c);
                check(vec![Op::Push(v), Op::MulC(c), Op::Halt], "mulC", v, c);
                check(vec![Op::Push(v), Op::MinC(c), Op::Halt], "minC", v, c);
                check(vec![Op::Push(v), Op::MaxC(c), Op::Halt], "maxC", v, c);
            }
        }
        for a in [0.0f64, -0.0, 1.0, -2.5, f64::NAN, f64::INFINITY] {
            for f in [0.0f64, 1.0, -1.0, f64::NAN] {
                for s in [0.5f64, -0.25, f64::NEG_INFINITY] {
                    check(
                        vec![Op::Push(a), Op::Push(f), Op::Push(s), Op::Lerp, Op::Halt],
                        "lerp",
                        a,
                        f,
                    );
                }
            }
        }
        // `Grad` is the one op that reads a coordinate, and the four lanes share `y` (the
        // batched columns differ in `x` only), so the groups below walk `y` across the two
        // clamps' edges and the four lanes must all answer the scalar value for that `y`.
        for (fc, fv, tc, tv) in [
            (-64.0, 0.0, -40.0, 1.0),
            (240.0, 1.0, 256.0, 0.0),
            (0.0, -0.0, 0.0, 1.0),
        ] {
            let prog = df4_prog(vec![Op::Grad { fc, fv, tc, tv }, Op::Halt]);
            for y in [g.min_y, -56, -40, 0, 240, 255, 256, g.min_y + g.height - 1] {
                let want = prog.eval(&g, None, 3, y, -2);
                let got = prog.eval_x4(&g, None, 3, y, -2);
                for (k, &lane_v) in got.iter().enumerate() {
                    assert_eq!(
                        want.to_bits(),
                        lane_v.to_bits(),
                        "grad fc={fc} y={y} lane {k}"
                    );
                    compared += 1;
                }
            }
        }
        let vec = m3_local().get(M3_DF4_VEC);
        m3_test_force(false);
        df4_test_force_scalar(false);
        assert!(
            compared > 5_000,
            "only {compared} lane values were compared; the loop did not run"
        );
        assert!(
            nan_results > 0,
            "no NaN result was reached; the edge values stopped covering the NaN case"
        );
        if df4_avx2() {
            assert!(
                vec > 0,
                "no group took the vector arm: this test would pass with the evaluator removed"
            );
        }
    }

    /// The four-lane density against four one-lane densities, on the presets that compile, at
    /// chunk origins inside and outside the origin cells, and at a seed other than 0.
    #[test]
    fn df4_density_matches_block_density_per_column() {
        let _guard = lock_gen();
        let _measurement = M3_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        df4_test_force_scalar(false);
        m3_test_force(true);
        m3_reset_local();
        let mut compared = 0usize;
        let mut programs = 0usize;
        for (preset, seed) in [
            (NoisePreset::Overworld, 0u64),
            (NoisePreset::Overworld, 12345),
            (NoisePreset::Nether, 0),
            (NoisePreset::End, 0),
        ] {
            let mut g = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            if DfProg::compile_inner(&g).is_none() {
                continue;
            }
            programs += 1;
            for (cx, cz) in [(0, 0), (-3, 2), (5, -7)] {
                let cc = g.cols(cx, cz).expect("columns");
                let mut ly = 0;
                while ly < 16 {
                    for lz in 0..16 {
                        for lx in (0..16).step_by(DF4) {
                            let y = g.min_y + ly;
                            let x = cx * 16 + lx;
                            let z = cz * 16 + lz;
                            let got = g.block_density_x4(&cc, x, y, z);
                            for (k, &lane_v) in got.iter().enumerate() {
                                let want = g.block_density(&cc, x + k as i32, y, z);
                                assert_eq!(
                                    want.to_bits(),
                                    lane_v.to_bits(),
                                    "{preset:?} seed {seed} chunk ({cx},{cz}) at {}, {y}, {z}",
                                    x + k as i32
                                );
                                compared += 1;
                            }
                        }
                    }
                    ly += 5;
                }
            }
        }
        let vec = m3_local().get(M3_DF4_VEC);
        m3_test_force(false);
        assert_eq!(programs, 4, "every preset must compile here");
        assert!(
            compared > 10_000,
            "only {compared} positions were compared; the loop did not run"
        );
        if df4_avx2() {
            assert!(vec > 0, "the vector arm did not run: {vec} groups");
        }
    }

    /// `Gen::cols` on one chunk with the vector arm and without it: the surface levels, the
    /// water heights, the run depths, the surface materials and the corner grids must be the
    /// same bytes. This is what pins the batched `surf` scan and the buffered surface window
    /// (both of which evaluate four columns per pass) to the one-column order.
    #[test]
    fn df4_columns_match_the_scalar_density_path() {
        let _guard = lock_gen();
        let mut compared = 0usize;
        for (preset, seed) in [(NoisePreset::Overworld, 0u64), (NoisePreset::Overworld, 4242)] {
            let mut g = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            if DfProg::compile_inner(&g).is_none() {
                continue;
            }
            for (cx, cz) in [(0, 0), (-2, 1)] {
                df4_test_force_scalar(false);
                g.col_c.clear();
                let vector = g.cols(cx, cz).expect("columns with the vector arm");
                // A2-b: force the lazy material scan while its own density arm is active.
                let vector_sm = vector.surface_mats(&mut g);
                df4_test_force_scalar(true);
                g.col_c.clear();
                let scalar = g.cols(cx, cz).expect("columns with the scalar arm");
                let scalar_sm = scalar.surface_mats(&mut g);
                df4_test_force_scalar(false);
                assert_eq!(vector.surf, scalar.surf, "{preset:?} surf at ({cx},{cz})");
                assert_eq!(vector.wtop, scalar.wtop, "{preset:?} wtop at ({cx},{cz})");
                assert_eq!(
                    &vector_sm.rd, &scalar_sm.rd, "{preset:?} rd at ({cx},{cz})"
                );
                for (i, (a, b)) in vector_sm.mats.iter().zip(scalar_sm.mats.iter()).enumerate() {
                    match (a, b) {
                        (None, None) => {}
                        (Some(a), Some(b)) => {
                            assert_eq!(a.lo, b.lo, "{preset:?} mat window at column {i}");
                            assert_eq!(
                                a.vals, b.vals,
                                "{preset:?} surface material at column {i} of ({cx},{cz})"
                            );
                            compared += a.vals.len();
                        }
                        _ => panic!("{preset:?} column {i}: one arm produced a window and the other did not"),
                    }
                }
                for (slot, (a, b)) in vector.grids.iter().zip(scalar.grids.iter()).enumerate() {
                    match (a, b) {
                        (None, None) => {}
                        (Some(a), Some(b)) => {
                            assert_eq!(a.len(), b.len(), "grid {slot} length");
                            for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
                                assert_eq!(
                                    x.to_bits(),
                                    y.to_bits(),
                                    "{preset:?} grid {slot} cell {i} of ({cx},{cz})"
                                );
                                compared += 1;
                            }
                        }
                        _ => panic!("{preset:?} grid {slot}: one arm built a grid and the other did not"),
                    }
                }
            }
        }
        assert!(
            compared > 50_000,
            "only {compared} values were compared; the loop did not run"
        );
    }

    /// The batched section fill against the one-cell rule: same states, same mark list (order
    /// included), for sections above, through and below the surface.
    #[test]
    fn df4_section_fill_matches_the_scalar_cell_rule() {
        let _guard = lock_gen();
        let mut g = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        if DfProg::compile_inner(&g).is_none() {
            return;
        }
        let (cx, cz) = (0, 0);
        let cc = g.cols(cx, cz).expect("columns");
        let mut marks_seen = 0usize;
        for sy in [-64, 0, 48, 96, 240, 304] {
            let mut arr = Box::new([0; 4096]);
            let marks = g.base_section_rows(&cc, cx, sy, cz, &mut arr);
            let mut want = Box::new([0u32; 4096]);
            let mut want_marks: Vec<(u8, i32, i32, i32)> = Vec::new();
            for ly in 0..16i32 {
                let y = sy + ly;
                for lz in 0..16i32 {
                    for lx in 0..16i32 {
                        let (state, marked) =
                            g.terrain_cell_marked(&cc, cx * 16 + lx, y, cz * 16 + lz);
                        want[((ly << 8) | (lz << 4) | lx) as usize] = state;
                        for bit in [MARK_FILL, MARK_SURFACE] {
                            if marked & bit != 0 {
                                want_marks.push((bit, cx * 16 + lx, y, cz * 16 + lz));
                            }
                        }
                    }
                }
            }
            // The fill files its marks in vanilla's stage order and then sorts that bucket into
            // the order `postProcessGeneration` consumes, so want the returned list already in
            // that order and the one-cell reference sorted into it: the comparison below is
            // then the multiset *and* the order it was filed in.
            let key = |&(kind, x, y, z): &(u8, i32, i32, i32)| {
                if kind & MARK_FILL != 0 {
                    (0u8, (x >> 2), (z >> 2), -y, (x & 3), (z & 3))
                } else {
                    (1, x, z, -y, 0, 0)
                }
            };
            let mut sorted = marks.clone();
            sorted.sort_by_key(key);
            assert_eq!(marks, sorted, "section sy={sy} did not file its marks in order");
            want_marks.sort_by_key(key);
            assert_eq!(
                arr.as_ref(),
                want.as_ref(),
                "section sy={sy} did not match the one-cell fill"
            );
            assert_eq!(marks, want_marks, "section sy={sy} marks differ");
            marks_seen += want_marks.len();
        }
        assert!(marks_seen > 0, "no section filed a mark; the fixture moved");
    }

    // These tests mutate the shared generator registry, so they must not run in parallel. The
    // lock is the module-level one, also used by tests in other modules (the delivery-path
    // bench) and visible to them: one lock for every test that touches the generator.
    pub(super) fn lock_gen() -> std::sync::MutexGuard<'static, ()> {
        super::GEN_TEST_LOCK.lock().unwrap_or_else(|poison| {
            // This mutex only serializes tests; it protects no recoverable data.
            super::GEN_TEST_LOCK.clear_poison();
            poison.into_inner()
        })
    }
    #[test]
    fn poisoned_test_lock_does_not_cascade() {
        let failed = std::panic::catch_unwind(|| {
            let _guard = lock_gen();
            panic!("intentional test failure");
        });
        assert!(failed.is_err());
        let _guard = lock_gen();
        assert!(!super::GEN_TEST_LOCK.is_poisoned());
    }
    #[test]
    fn pinned_biome_tree_uses_only_biomes_from_its_jar() {
        let g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
        let biomes = Gen::dp_files("biome");
        assert!(!g.biome_names.is_empty(), "empty biome tree");
        let unknown: Vec<_> = g
            .biome_names
            .iter()
            .filter(|name| !biomes.contains_key(*name))
            .collect();
        assert!(
            unknown.is_empty(),
            "biome tree contains foreign biomes: {unknown:?}"
        );
    }

    /// The chunk biome set must cover the whole build height, not y >= 0.
    ///
    /// `biome_quart` takes the *block* y divided by four, so a sweep of `0..height/4` reads
    /// y = 0..383 and drops every biome confined below y = 0. Measured consequence: a deep-dark
    /// chunk had no `deep_dark` in its set, so `sculk_patch_deep_dark` and `sculk_vein` never
    /// ran there (0 of the 397 sculk cells vanilla places), while `pointed_dripstone` ran
    /// because `dripstone_caves` also occurs above y = 0.
    #[test]
    fn chunk_biome_set_covers_the_negative_y_range() {
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
        let (cx, cz) = (-16, 112);
        let set = g.chunk_biome_set(cx, cz);
        assert!(
            set.iter().any(|b| b == "minecraft:deep_dark"),
            "deep-dark chunk lost its below-zero biome: {set:?}"
        );
        // and the set must be the union over *every* quart layer of the chunk
        let mut union = std::collections::HashSet::new();
        for qy in g.min_y / 4..(g.min_y + g.height) / 4 {
            for lx in 0..4 {
                for lz in 0..4 {
                    if let Some((_, leaf)) = g.biome_quart(cx * 4 + lx, qy, cz * 4 + lz, None) {
                        if let Some(name) = g.biome_name_of_leaf(leaf) {
                            union.insert(name);
                        }
                    }
                }
            }
        }
        let from_set: std::collections::HashSet<String> = set.iter().cloned().collect();
        assert_eq!(
            from_set, union,
            "chunk biome set is not the union of its own quart layers"
        );
    }

    /// Actual `SurfaceSystem.buildSurface` scan on synthetic stone/water columns.
    /// The Java oracle fills a ProtoChunk, runs the real buildSurface with a
    /// stone-depth rule, and records the gold cells. The test feeds the same
    /// column states through the production `scan_surface_column` path.
    #[test]
    fn surface_build_scan_matches_the_pinned_classes_on_synthetic_columns() {
        let fixture = jparse(include_bytes!(
            "../evidence/foundation/surface-context/buildsurface.json"
        ))
        .unwrap();
        let rows = fixture.get("columns").unwrap().as_arr().unwrap();
        let gold = crate::name_to_state("minecraft:gold_block").expect("gold block");
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut seen = 0usize;
        for row in rows {
            let offset = row.get("offset").unwrap().num() as i32;
            let x = row.get("x").unwrap().num() as i32;
            let z = row.get("z").unwrap().num() as i32;
            let top = row.get("top").unwrap().num() as i32;
            let sea = row.get("sea").unwrap().num() as i32;
            g.conds.push(Cond::StoneD {
                off: offset,
                sdep: false,
                sec: 0,
                ceil: false,
            });
            let cid = g.conds.len() - 1;
            g.rules.push(Rule::Block(gold));
            let block = g.rules.len() - 1;
            g.rules.push(Rule::Cond(cid, block));
            let rid = g.rules.len() - 1;
            let scan_top = top.max(sea);
            let cells: Vec<SurfaceCell> = (g.min_y..=scan_top)
                .map(|y| {
                    if y <= top {
                        SurfaceCell::Solid
                    } else if y <= sea {
                        SurfaceCell::Fluid
                    } else {
                        SurfaceCell::Air
                    }
                })
                .collect();
            let mut c = SC::new();
            c.x = x;
            c.z = z;
            c.run_depth = 0;
            c.sec_ok = true;
            c.est_min = g.min_y;
            let want: Vec<i32> = row
                .get("gold")
                .unwrap()
                .as_arr()
                .unwrap()
                .iter()
                .map(|v| v.num() as i32)
                .collect();
            let mat = g
                .scan_surface_column(rid, &mut c, g.min_y, &cells, &[0i16; 256])
                .expect("surface column");
            let got: Vec<i32> = mat
                .vals
                .iter()
                .enumerate()
                .filter(|(_, v)| **v != u16::MAX)
                .map(|(i, _)| mat.lo as i32 + i as i32)
                .collect();
            assert_eq!(got, want, "scan offset {offset} column {x},{z} top {top}");
            let _ = cid;
            seen += 1;
        }
        assert_eq!(seen, 768, "buildSurface synthetic coverage changed");
    }

    /// Actual `SurfaceRules` condition formulas on synthetic context fields:
    /// stone depth above/below, surface depth, water height, surface secondary,
    /// hole and the preliminary-surface comparison. The fixture is generated by
    /// `tools/surface_condition_oracle.py` from the pinned classes.
    #[test]
    fn surface_conditions_match_the_pinned_context_formulas() {
        let fixture = jparse(include_bytes!(
            "../evidence/foundation/surface-context/conditions.json"
        ))
        .unwrap();
        let rows = fixture.get("rows").unwrap().as_arr().unwrap();
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut seen = 0usize;
        for row in rows {
            let kind = row.get("kind").unwrap().str();
            let mut c = SC::new();
            c.x = 5;
            c.z = 7;
            c.y = row.get("y").map(J::num).unwrap_or(0.0) as i32;
            c.run_depth = row.get("surface_depth").map(J::num).unwrap_or(0.0) as i32;
            c.sda = row.get("stone_above").map(J::num).unwrap_or(0.0) as i32;
            c.sdb = row.get("stone_below").map(J::num).unwrap_or(0.0) as i32;
            c.fluid = row
                .get("water_height")
                .map(J::num)
                .unwrap_or(i32::MIN as f64) as i32;
            c.est_min = row.get("min_surface").map(J::num).unwrap_or(0.0) as i32;
            c.sec = row.get("surface_secondary").map(J::num).unwrap_or(0.0) as f32;
            c.sec_ok = true;
            let cond = match kind {
                "stone" => Cond::StoneD {
                    off: row.get("offset").unwrap().num() as i32,
                    sdep: row.get("add").unwrap().num() != 0.0,
                    sec: row.get("secondary").unwrap().num() as i32,
                    ceil: row.get("ceiling").unwrap().num() != 0.0,
                },
                "water" => Cond::Water {
                    off: row.get("offset").unwrap().num() as i32,
                    mult: row.get("mult").unwrap().num() as i32,
                    sd: row.get("start").unwrap().num() != 0.0,
                },
                "hole" => Cond::Hole,
                "prelim" => Cond::Prelim,
                other => panic!("unexpected condition kind {other}"),
            };
            g.conds.push(cond);
            let cid = g.conds.len() - 1;
            let want = row.get("expected").unwrap().num() != 0.0;
            assert_eq!(
                g.test_cond(cid, &mut c, &[0i16; 256]),
                want,
                "{} {}",
                row.get("id").unwrap().str(),
                kind
            );
            seen += 1;
        }
        assert_eq!(seen, 4037, "surface condition coverage changed");
    }

    /// `Context$SteepMaterialCondition` compares WORLD_SURFACE_WG heights at
    /// z+1/z-1 and x-1/x+1 with a four-block threshold. The scan must pass the
    /// surface/fluid heightmap, not the stone-only density top.
    #[test]
    fn surface_steep_reads_the_world_surface_heightmap() {
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        g.conds.push(Cond::Steep);
        let cid = g.conds.len() - 1;
        let mut c = SC::new();
        c.x = 5;
        c.z = 5;
        let mut hm = [64i16; 256];
        hm[6 * 16 + 5] = 68;
        assert!(g.test_cond(cid, &mut c, &hm), "z+1 slope");
        let mut c = SC::new();
        c.x = 5;
        c.z = 5;
        let mut hm = [64i16; 256];
        hm[6 * 16 + 5] = 67;
        assert!(
            !g.test_cond(cid, &mut c, &hm),
            "three-block slope is not steep"
        );
        let mut c = SC::new();
        c.x = 5;
        c.z = 5;
        let mut hm = [64i16; 256];
        hm[5 * 16 + 4] = 68;
        assert!(g.test_cond(cid, &mut c, &hm), "x-1 slope");
        let mut c = SC::new();
        c.x = 5;
        c.z = 5;
        assert!(!g.test_cond(cid, &mut c, &[64i16; 256]), "flat");
    }

    #[test]
    fn pinned_surface_rule_places_bedrock_and_deepslate() {
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
        let root = g.surface_rule.expect("inline surface_rule did not resolve");
        let surface = [64; 256];
        for (y, expected) in [(-64, "minecraft:bedrock"), (-8, "minecraft:deepslate")] {
            let mut c = SC::new();
            c.y = y;
            c.est_min = i32::MAX;
            c.sda = 32000;
            c.sdb = 32000;
            assert_eq!(
                g.eval_rule(root, &mut c, &surface),
                crate::name_to_state(expected),
                "pinned surface rule at y={y}"
            );
        }
    }

    #[test]
    fn pinned_surface_block_states_match_the_native_codec() {
        let fixture = jparse(include_bytes!(
            "../evidence/foundation/surface-loader/vanilla.json"
        ))
        .expect("surface reference JSON");
        assert_eq!(fixture.get("minecraft").unwrap().str(), "1.21.1");
        assert_eq!(
            fixture.get("server_sha1").unwrap().str(),
            "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
        );
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack");
        let empty = HashMap::new();
        let mut checked = 0;
        let mut ids = HashSet::new();
        for row in fixture.get("cases").unwrap().as_arr().unwrap() {
            assert!(ids.insert(row.get("id").unwrap().str()));
            if row.get("op").unwrap().str() != "state" {
                continue;
            }
            let state = row.get("state").unwrap();
            let rule = J::Obj(vec![
                ("type".into(), J::Str("minecraft:block".into())),
                ("result_state".into(), state.clone()),
            ]);
            let Some(Rule::Block(actual)) =
                g.parse_rule(&rule, &empty, &empty, &empty, crate::name_to_state)
            else {
                panic!(
                    "native state did not decode: {}",
                    row.get("id").unwrap().str()
                );
            };
            let expected = row.get("expected").unwrap().get("state_id").unwrap().num() as u32;
            assert_eq!(actual, expected, "{}", row.get("id").unwrap().str());
            // The retained converted dialect must still use the same registry.
            let (name, props) = bs_parts(state).unwrap();
            let mut converted = vec![("id".into(), J::Str(name))];
            if let Some(props) = props {
                converted.push(("properties".into(), props.clone()));
            }
            assert_eq!(
                Gen::block_state(&J::Obj(converted), crate::name_to_state),
                Some(expected)
            );
            checked += 1;
        }
        assert!(checked > 0, "no native state cases compared");
    }

    #[test]
    fn pinned_surface_temperature_matches_the_native_biome() {
        let fixture = jparse(include_bytes!(
            "../evidence/foundation/surface-loader/vanilla.json"
        ))
        .expect("surface reference JSON");
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack");
        let empty = HashMap::new();
        let condition = jparse(br#"{"type":"minecraft:temperature"}"#).unwrap();
        let cid = g
            .cond_node(&condition, &empty)
            .expect("temperature condition");
        let mut checked = 0;
        let mut cold = 0;
        let mut nonzero_noise = 0;
        for row in fixture.get("cases").unwrap().as_arr().unwrap() {
            if row.get("op").unwrap().str() == "simplex" {
                let (seed, octaves): (u64, &[i32]) = match row.get("source").unwrap().str() {
                    "height" => (1234, &[0]),
                    "frozen" => (3456, &[-2, -1, 0]),
                    "info" => (2345, &[0]),
                    other => panic!("unknown native Biome noise field {other}"),
                };
                let pos = row.get("pos").unwrap().as_arr().unwrap();
                let noise = OSimp::new_l(&mut Lcg::new(seed), octaves);
                let actual = noise.sample(pos[0].num(), pos[1].num());
                let bits = row
                    .get("expected")
                    .unwrap()
                    .get("value_bits")
                    .unwrap()
                    .str();
                assert_eq!(
                    actual.to_bits(),
                    u64::from_str_radix(bits, 16).unwrap(),
                    "native Biome noise: {}",
                    row.get("id").unwrap().str()
                );
                nonzero_noise += usize::from(actual != 0.0);
                continue;
            }
            if row.get("op").unwrap().str() != "temperature" {
                continue;
            }
            let p = row.get("pos").unwrap().as_arr().unwrap();
            let (x, y, z) = (p[0].num() as i32, p[1].num() as i32, p[2].num() as i32);
            let weather = BiomeWeather {
                temperature: row.get("base_temperature").unwrap().num() as f32,
                frozen: jbool(row, "frozen", false),
            };
            let expected = row.get("expected").unwrap();
            let id = row.get("id").unwrap().str();
            assert_eq!(
                weather.at(x, y, z).to_bits(),
                expected.get("temperature_bits").unwrap().num() as i32 as u32,
                "{id}"
            );
            let is_cold = jbool(expected, "cold", false);
            // Exercise the actual condition dispatch, with an explicit biome
            // leaf. This does not claim that the world biome lookup is correct.
            g.biome_weather[0] = weather;
            g.bioq.insert(g.fiddled_biome_quart(x, y, z), 0);
            let mut c = SC::new();
            (c.x, c.y, c.z) = (x, y, z);
            assert_eq!(g.test_cond(cid, &mut c, &[64; 256]), is_cold, "{id}");
            cold += usize::from(is_cold);
            checked += 1;
        }
        assert!(cold > 0 && cold < checked, "no cold/warm contrast");
        assert!(nonzero_noise > 0, "no nonzero native noise values compared");
    }

    #[test]
    fn surface_sequence_rejects_a_child_that_did_not_parse() {
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack");
        let empty = HashMap::new();
        for child in [
            r#"{"type":"minecraft:block"}"#,
            r#"{"type":"minecraft:unknown_rule"}"#,
            r#""minecraft:missing_rule""#,
            r#"{"type":"minecraft:condition","if_true":{"type":"minecraft:unknown_condition"},"then_run":{"type":"minecraft:block","result_state":"minecraft:stone"}}"#,
        ] {
            let rule = jparse(format!(
                r#"{{"type":"minecraft:sequence","sequence":[{{"type":"minecraft:block","result_state":"minecraft:dirt"}},{child}]}}"#
            ).as_bytes()).unwrap();
            assert!(
                g.rule_node(&rule, &empty, &empty, &empty, crate::name_to_state)
                    .is_none(),
                "sequence silently dropped {child}"
            );
        }
        let bad_name = "minecraft:bad_named_rule";
        let named = HashMap::from([(
            bad_name.to_string(),
            br#"{"type":"minecraft:block"}"#.to_vec(),
        )]);
        for _ in 0..2 {
            assert!(
                g.resolve_rule(bad_name, &empty, &named, &empty, crate::name_to_state)
                    .is_none(),
                "failed named rule was cached as a valid placeholder"
            );
        }
    }

    #[test]
    fn pinned_climate_router_resolves_inline_fields() {
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
        // The tree existed, but temperature/vegetation are inline objects in 1.21.1.
        // Treating them as reference strings silently disabled every biome query.
        assert!(g.climate.is_some(), "climate router did not resolve");
        for (qx, qy, qz) in [(0, 16, 0), (832, 16, -448), (-256, 16, 128)] {
            let (_, leaf) = g.biome_quart(qx, qy, qz, None).expect("biome query");
            assert!(!g.biome_names[leaf as usize].is_empty());
        }
    }

    #[test]
    fn biome_vectors() {
        let _guard = lock_gen();
        // pumpkin multi_noise_sampler tests: seed, quart pos, params
        fn stub(_: &str) -> Option<u32> {
            Some(1)
        }
        assert!(init(123, stub));
        let (b, t) = biome_debug(123, 123, 123).unwrap();
        assert_eq!(t, [-5727, 55, 4996, 2371, -19774, 4421, 0]);
        assert_eq!(b, 47); // snowy_taiga
        assert!(init(13579, stub));
        let (b, t) = biome_debug(123, 123, 123).unwrap();
        assert_eq!(t, [7489, 3502, -2168, -3511, -21237, -5222, 0]);
        assert_eq!(b, 57); // warm_ocean
    }

    /// The same lookup as the game's on the tie-heavy probe set
    /// `tests/data/biome_probe_ties_1211.tsv` (points on leaf range edges, almost all
    /// with two different biomes at the same minimum distance). `SearchTree` scans a
    /// branch's children in array order and replaces its best only on a strictly
    /// smaller distance, so equally near leaves resolve to the first one reached.
    #[test]
    fn biome_tree_breaks_ties_the_way_vanilla_scans_children() {
        let _guard = lock_gen();
        fn stub(_: &str) -> Option<u32> {
            Some(1)
        }
        assert!(init(0, stub));
        let mut gg = gen_lock();
        let g = gg.as_mut().unwrap();
        let tree = g.btree.as_ref().expect("biome tree must load");
        let (mut n, mut tied, mut wrong) = (0usize, 0usize, Vec::new());
        for file in ["tests/data/biome_probe_ties_1211.tsv"] {
            let probes = std::fs::read_to_string(file).expect("probe points must be readable");
            for line in probes.lines() {
                let f: Vec<&str> = line.split('\t').collect();
                assert_eq!(f.len(), 8, "bad probe line {line:?}");
                if !file.contains("ties") && f[7] != "1" {
                    continue; // unambiguous points are covered by the test above
                }
                let mut t = [0i64; 7];
                for i in 0..6 {
                    t[i] = f[i].parse().unwrap();
                }
                let mut best = (i64::MAX, 0u16);
                tree.search(&t, &mut best);
                let got = &g.biome_names[best.1 as usize];
                n += 1;
                tied += (f[7] == "1") as usize;
                if got != f[6] {
                    wrong.push(format!("{t:?}: game {} ours {got}", f[6]));
                }
            }
        }
        eprintln!(
            "tie-break points: {n} ({tied} with different biomes equally near), differing: {}",
            wrong.len()
        );
        assert!(tied >= 2000, "the tie set must be large, got {tied}");
        assert!(
            wrong.is_empty(),
            "{} of {n} points differ from vanilla, first: {:?}",
            wrong.len(),
            &wrong[..wrong.len().min(5)]
        );
    }

    #[test]
    fn material_rules_load() {
        let _guard = lock_gen();
        fn stub(_: &str) -> Option<u32> {
            Some(1)
        }
        assert!(init(0, stub));
        let mut gg = gen_lock();
        let g = gg.as_mut().unwrap();
        println!(
            "surface_rule={:?} conds={} rules={} mats_loaded",
            g.surface_rule,
            g.conds.len(),
            g.rules.len()
        );
        assert!(g.surface_rule.is_some(), "material_rule not resolved");
        assert!(!g.conds.is_empty() && g.rules.len() > 5);
        drop(gg);
        // real state ids: grass/dirt/stone or sand should appear in a
        // plains-ish spawn column; "no match" (u32::MAX->default) is stone.
        let cc = chunk_cols(0, 0).unwrap();
        let mut gg = gen_lock();
        let g = gg.as_mut().unwrap();
        let mut kinds: HashMap<u32, u32> = HashMap::new();
        let mut nmat = 0;
        for lz in 0..16 {
            for lx in 0..16 {
                if let Some(m) = &cc.surface_mats(g).mats[lz * 16 + lx] {
                    nmat += 1;
                    for &v in m.vals.iter() {
                        *kinds.entry(v as u32).or_default() += 1;
                    }
                }
            }
        }
        println!("cols_with_mats={} kinds={:?}", nmat, kinds);
        assert!(nmat > 0, "no material mats built");
    }

    #[test]
    fn material_rules_real_ids() {
        let _guard = lock_gen();
        assert!(init(0, crate::name_to_state));
        let cc = chunk_cols(0, 0).unwrap();
        let mut gg = gen_lock();
        let g = gg.as_mut().unwrap();
        let mut kinds: HashMap<u32, u32> = HashMap::new();
        for lz in 0..16 {
            for lx in 0..16 {
                if let Some(m) = &cc.surface_mats(g).mats[lz * 16 + lx] {
                    for &v in m.vals.iter() {
                        *kinds.entry(v as u32).or_default() += 1;
                    }
                }
            }
        }
        println!("real-id kinds={:?}", kinds);
        // at least one real surface block resolved (grass=9, dirt=10,
        // sand=112, gravel=118) or all-default (ocean column)
        let surfaced = kinds.keys().any(|&k| [9u32, 10, 112, 118].contains(&k));
        assert!(surfaced || kinds.len() <= 2, "no surface mats resolved");
    }
    // Pumpkin-derived internal-method vectors, retained as regression pins. They
    // include coordinates a vanilla chunk-scoped aquifer never queries and a donor
    // GlobalRandomConfig; native behaviour is pinned separately by
    // `aquifer_matches_the_pinned_chunk_through_its_public_api`.
    const AQ_NB: &[((i32, i32, i32), i32)] = &[
        ((-100, -100, -100), -103),
        ((-100, -100, -50), -103),
        ((-100, -100, 0), -103),
        ((-100, -100, 50), -103),
        ((-100, -100, 100), -103),
        ((-100, -50, -100), -63),
        ((-100, -50, -50), -63),
        ((-100, -50, 0), -63),
        ((-100, -50, 50), -63),
        ((-100, -50, 100), -63),
        ((-100, 0, -100), 17),
        ((-100, 0, -50), 17),
        ((-100, 0, 0), 17),
        ((-100, 0, 50), 17),
        ((-100, 0, 100), 17),
        ((-100, 50, -100), 57),
        ((-100, 50, -50), 57),
        ((-100, 50, 0), 57),
        ((-100, 50, 50), 57),
        ((-100, 50, 100), 57),
        ((-100, 100, -100), 97),
        ((-100, 100, -50), 97),
        ((-100, 100, 0), 97),
        ((-100, 100, 50), 97),
        ((-100, 100, 100), 97),
        ((-50, -100, -100), -103),
        ((-50, -100, -50), -103),
        ((-50, -100, 0), -103),
        ((-50, -100, 50), -103),
        ((-50, -100, 100), -100),
        ((-50, -50, -100), -63),
        ((-50, -50, -50), -63),
        ((-50, -50, 0), -63),
        ((-50, -50, 50), -63),
        ((-50, -50, 100), -60),
        ((-50, 0, -100), 17),
        ((-50, 0, -50), 17),
        ((-50, 0, 0), 17),
        ((-50, 0, 50), 17),
        ((-50, 0, 100), 20),
        ((-50, 50, -100), 57),
        ((-50, 50, -50), 57),
        ((-50, 50, 0), 57),
        ((-50, 50, 50), 57),
        ((-50, 50, 100), 60),
        ((-50, 100, -100), 97),
        ((-50, 100, -50), 97),
        ((-50, 100, 0), 97),
        ((-50, 100, 50), 97),
        ((-50, 100, 100), 100),
        ((0, -100, -100), -103),
        ((0, -100, -50), -103),
        ((0, -100, 0), -103),
        ((0, -100, 50), -100),
        ((0, -100, 100), -100),
        ((0, -50, -100), -63),
        ((0, -50, -50), -63),
        ((0, -50, 0), -63),
        ((0, -50, 50), -60),
        ((0, -50, 100), -60),
        ((0, 0, -100), 17),
        ((0, 0, -50), 17),
        ((0, 0, 0), 17),
        ((0, 0, 50), 20),
        ((0, 0, 100), 20),
        ((0, 50, -100), 57),
        ((0, 50, -50), 57),
        ((0, 50, 0), 57),
        ((0, 50, 50), 60),
        ((0, 50, 100), 60),
        ((0, 100, -100), 97),
        ((0, 100, -50), 97),
        ((0, 100, 0), 97),
        ((0, 100, 50), 100),
        ((0, 100, 100), 100),
        ((50, -100, -100), -103),
        ((50, -100, -50), -103),
        ((50, -100, 0), -103),
        ((50, -100, 50), -100),
        ((50, -100, 100), -100),
        ((50, -50, -100), -63),
        ((50, -50, -50), -63),
        ((50, -50, 0), -63),
        ((50, -50, 50), -60),
        ((50, -50, 100), -60),
        ((50, 0, -100), 17),
        ((50, 0, -50), 17),
        ((50, 0, 0), 17),
        ((50, 0, 50), 20),
        ((50, 0, 100), 20),
        ((50, 50, -100), 57),
        ((50, 50, -50), 57),
        ((50, 50, 0), 57),
        ((50, 50, 50), 60),
        ((50, 50, 100), 60),
        ((50, 100, -100), 97),
        ((50, 100, -50), 97),
        ((50, 100, 0), 97),
        ((50, 100, 50), 100),
        ((50, 100, 100), 100),
        ((100, -100, -100), -103),
        ((100, -100, -50), -103),
        ((100, -100, 0), -103),
        ((100, -100, 50), -103),
        ((100, -100, 100), -100),
        ((100, -50, -100), -63),
        ((100, -50, -50), -63),
        ((100, -50, 0), -63),
        ((100, -50, 50), -63),
        ((100, -50, 100), -60),
        ((100, 0, -100), 17),
        ((100, 0, -50), 17),
        ((100, 0, 0), 17),
        ((100, 0, 50), 17),
        ((100, 0, 100), 20),
        ((100, 50, -100), 57),
        ((100, 50, -50), 57),
        ((100, 50, 0), 57),
        ((100, 50, 50), 57),
        ((100, 50, 100), 60),
        ((100, 100, -100), 97),
        ((100, 100, -50), 97),
        ((100, 100, 0), 97),
        ((100, 100, 50), 97),
        ((100, 100, 100), 100),
    ];
    const AQ_FYT: &[((i32, i32, i32), i32)] = &[
        ((-100, -100, -100), -32512),
        ((-100, -100, -50), -32512),
        ((-100, -100, 0), -32512),
        ((-100, -100, 50), -32512),
        ((-100, -100, 100), -32512),
        ((-100, -50, -100), -32512),
        ((-100, -50, -50), -63),
        ((-100, -50, 0), -63),
        ((-100, -50, 50), -63),
        ((-100, -50, 100), -32512),
        ((-100, 0, -100), -32512),
        ((-100, 0, -50), -32512),
        ((-100, 0, 0), -32512),
        ((-100, 0, 50), -32512),
        ((-100, 0, 100), -32512),
        ((-100, 50, -100), 57),
        ((-100, 50, -50), 57),
        ((-100, 50, 0), 57),
        ((-100, 50, 50), 57),
        ((-100, 50, 100), 57),
        ((-100, 100, -100), 80),
        ((-100, 100, -50), 0),
        ((-100, 100, 0), 0),
        ((-100, 100, 50), 0),
        ((-100, 100, 100), 0),
        ((-50, -100, -100), -32512),
        ((-50, -100, -50), -32512),
        ((-50, -100, 0), -32512),
        ((-50, -100, 50), -32512),
        ((-50, -100, 100), -32512),
        ((-50, -50, -100), -32512),
        ((-50, -50, -50), -32512),
        ((-50, -50, 0), -32512),
        ((-50, -50, 50), -32512),
        ((-50, -50, 100), -32512),
        ((-50, 0, -100), -32512),
        ((-50, 0, -50), -32512),
        ((-50, 0, 0), -32512),
        ((-50, 0, 50), -32512),
        ((-50, 0, 100), -32512),
        ((-50, 50, -100), -32512),
        ((-50, 50, -50), -32512),
        ((-50, 50, 0), 57),
        ((-50, 50, 50), 57),
        ((-50, 50, 100), 60),
        ((-50, 100, -100), 80),
        ((-50, 100, -50), 0),
        ((-50, 100, 0), 0),
        ((-50, 100, 50), 0),
        ((-50, 100, 100), 0),
        ((0, -100, -100), -32512),
        ((0, -100, -50), -32512),
        ((0, -100, 0), -32512),
        ((0, -100, 50), -32512),
        ((0, -100, 100), -32512),
        ((0, -50, -100), -32512),
        ((0, -50, -50), -32512),
        ((0, -50, 0), -32512),
        ((0, -50, 50), -32512),
        ((0, -50, 100), -32512),
        ((0, 0, -100), -32512),
        ((0, 0, -50), -32512),
        ((0, 0, 0), -32512),
        ((0, 0, 50), -32512),
        ((0, 0, 100), -32512),
        ((0, 50, -100), -32512),
        ((0, 50, -50), -32512),
        ((0, 50, 0), 57),
        ((0, 50, 50), 60),
        ((0, 50, 100), 60),
        ((0, 100, -100), 0),
        ((0, 100, -50), 0),
        ((0, 100, 0), 0),
        ((0, 100, 50), 0),
        ((0, 100, 100), 0),
        ((50, -100, -100), -32512),
        ((50, -100, -50), -32512),
        ((50, -100, 0), -32512),
        ((50, -100, 50), -32512),
        ((50, -100, 100), -100),
        ((50, -50, -100), -32512),
        ((50, -50, -50), -32512),
        ((50, -50, 0), -32512),
        ((50, -50, 50), -32512),
        ((50, -50, 100), -60),
        ((50, 0, -100), -32512),
        ((50, 0, -50), -32512),
        ((50, 0, 0), -32512),
        ((50, 0, 50), -32512),
        ((50, 0, 100), -32512),
        ((50, 50, -100), -32512),
        ((50, 50, -50), -32512),
        ((50, 50, 0), 57),
        ((50, 50, 50), 60),
        ((50, 50, 100), 60),
        ((50, 100, -100), 0),
        ((50, 100, -50), 0),
        ((50, 100, 0), 0),
        ((50, 100, 50), 0),
        ((50, 100, 100), 0),
        ((100, -100, -100), -32512),
        ((100, -100, -50), -32512),
        ((100, -100, 0), -32512),
        ((100, -100, 50), -103),
        ((100, -100, 100), -100),
        ((100, -50, -100), -32512),
        ((100, -50, -50), -32512),
        ((100, -50, 0), -32512),
        ((100, -50, 50), -32512),
        ((100, -50, 100), -60),
        ((100, 0, -100), -32512),
        ((100, 0, -50), -32512),
        ((100, 0, 0), -32512),
        ((100, 0, 50), -32512),
        ((100, 0, 100), -32512),
        ((100, 50, -100), -32512),
        ((100, 50, -50), -32512),
        ((100, 50, 0), 57),
        ((100, 50, 50), 57),
        ((100, 50, 100), 60),
        ((100, 100, -100), 0),
        ((100, 100, -50), 0),
        ((100, 100, 0), 0),
        ((100, 100, 50), 0),
        ((100, 100, 100), 0),
    ];
    const AQ_FYF: &[((i32, i32, i32), i32)] = &[
        ((-100, -100, -100), -32512),
        ((-100, -100, -50), -32512),
        ((-100, -100, 0), -32512),
        ((-100, -100, 50), -32512),
        ((-100, -100, 100), -32512),
        ((-100, -50, -100), -32512),
        ((-100, -50, -50), -63),
        ((-100, -50, 0), -63),
        ((-100, -50, 50), -63),
        ((-100, -50, 100), -32512),
        ((-100, 0, -100), -32512),
        ((-100, 0, -50), -32512),
        ((-100, 0, 0), -32512),
        ((-100, 0, 50), -32512),
        ((-100, 0, 100), -32512),
        ((-100, 50, -100), -32512),
        ((-100, 50, -50), -32512),
        ((-100, 50, 0), -32512),
        ((-100, 50, 50), -32512),
        ((-100, 50, 100), -32512),
        ((-100, 100, -100), -32512),
        ((-100, 100, -50), -32512),
        ((-100, 100, 0), -32512),
        ((-100, 100, 50), -32512),
        ((-100, 100, 100), -32512),
        ((-50, -100, -100), -32512),
        ((-50, -100, -50), -32512),
        ((-50, -100, 0), -32512),
        ((-50, -100, 50), -32512),
        ((-50, -100, 100), -32512),
        ((-50, -50, -100), -32512),
        ((-50, -50, -50), -32512),
        ((-50, -50, 0), -32512),
        ((-50, -50, 50), -32512),
        ((-50, -50, 100), -32512),
        ((-50, 0, -100), -32512),
        ((-50, 0, -50), -32512),
        ((-50, 0, 0), -32512),
        ((-50, 0, 50), -32512),
        ((-50, 0, 100), -32512),
        ((-50, 50, -100), -32512),
        ((-50, 50, -50), -32512),
        ((-50, 50, 0), -32512),
        ((-50, 50, 50), -32512),
        ((-50, 50, 100), -32512),
        ((-50, 100, -100), -32512),
        ((-50, 100, -50), -32512),
        ((-50, 100, 0), 80),
        ((-50, 100, 50), -32512),
        ((-50, 100, 100), -32512),
        ((0, -100, -100), -32512),
        ((0, -100, -50), -32512),
        ((0, -100, 0), -32512),
        ((0, -100, 50), -32512),
        ((0, -100, 100), -32512),
        ((0, -50, -100), -32512),
        ((0, -50, -50), -32512),
        ((0, -50, 0), -32512),
        ((0, -50, 50), -32512),
        ((0, -50, 100), -32512),
        ((0, 0, -100), -32512),
        ((0, 0, -50), -32512),
        ((0, 0, 0), -32512),
        ((0, 0, 50), -32512),
        ((0, 0, 100), -32512),
        ((0, 50, -100), -32512),
        ((0, 50, -50), -32512),
        ((0, 50, 0), -32512),
        ((0, 50, 50), -32512),
        ((0, 50, 100), -32512),
        ((0, 100, -100), -32512),
        ((0, 100, -50), -32512),
        ((0, 100, 0), 80),
        ((0, 100, 50), -32512),
        ((0, 100, 100), -32512),
        ((50, -100, -100), -32512),
        ((50, -100, -50), -32512),
        ((50, -100, 0), -32512),
        ((50, -100, 50), -32512),
        ((50, -100, 100), -100),
        ((50, -50, -100), -32512),
        ((50, -50, -50), -32512),
        ((50, -50, 0), -32512),
        ((50, -50, 50), -32512),
        ((50, -50, 100), -60),
        ((50, 0, -100), -32512),
        ((50, 0, -50), -32512),
        ((50, 0, 0), -32512),
        ((50, 0, 50), -32512),
        ((50, 0, 100), -32512),
        ((50, 50, -100), -32512),
        ((50, 50, -50), -32512),
        ((50, 50, 0), -32512),
        ((50, 50, 50), -32512),
        ((50, 50, 100), -32512),
        ((50, 100, -100), -32512),
        ((50, 100, -50), -32512),
        ((50, 100, 0), 80),
        ((50, 100, 50), -32512),
        ((50, 100, 100), -32512),
        ((100, -100, -100), -32512),
        ((100, -100, -50), -32512),
        ((100, -100, 0), -32512),
        ((100, -100, 50), -103),
        ((100, -100, 100), -100),
        ((100, -50, -100), -32512),
        ((100, -50, -50), -32512),
        ((100, -50, 0), -32512),
        ((100, -50, 50), -32512),
        ((100, -50, 100), -60),
        ((100, 0, -100), -32512),
        ((100, 0, -50), -32512),
        ((100, 0, 0), -32512),
        ((100, 0, 50), -32512),
        ((100, 0, 100), -32512),
        ((100, 50, -100), -32512),
        ((100, 50, -50), -32512),
        ((100, 50, 0), -32512),
        ((100, 50, 50), -32512),
        ((100, 50, 100), -32512),
        ((100, 100, -100), -32512),
        ((100, 100, -50), -32512),
        ((100, 100, 0), 80),
        ((100, 100, 50), -32512),
        ((100, 100, 100), -32512),
    ];
    const AQ_FBS: &[((i32, i32, i32), u8)] = &[
        ((-100, -100, -100), 1),
        ((-100, -100, -50), 2),
        ((-100, -100, 0), 1),
        ((-100, -100, 50), 1),
        ((-100, -100, 100), 1),
        ((-100, -50, -100), 1),
        ((-100, -50, -50), 2),
        ((-100, -50, 0), 2),
        ((-100, -50, 50), 2),
        ((-100, -50, 100), 1),
        ((-100, 0, -100), 1),
        ((-100, 0, -50), 1),
        ((-100, 0, 0), 1),
        ((-100, 0, 50), 1),
        ((-100, 0, 100), 1),
        ((-100, 50, -100), 1),
        ((-100, 50, -50), 1),
        ((-100, 50, 0), 1),
        ((-100, 50, 50), 1),
        ((-100, 50, 100), 1),
        ((-100, 100, -100), 1),
        ((-100, 100, -50), 1),
        ((-100, 100, 0), 1),
        ((-100, 100, 50), 1),
        ((-100, 100, 100), 1),
        ((-50, -100, -100), 1),
        ((-50, -100, -50), 1),
        ((-50, -100, 0), 2),
        ((-50, -100, 50), 2),
        ((-50, -100, 100), 1),
        ((-50, -50, -100), 1),
        ((-50, -50, -50), 1),
        ((-50, -50, 0), 1),
        ((-50, -50, 50), 1),
        ((-50, -50, 100), 1),
        ((-50, 0, -100), 2),
        ((-50, 0, -50), 1),
        ((-50, 0, 0), 1),
        ((-50, 0, 50), 1),
        ((-50, 0, 100), 1),
        ((-50, 50, -100), 1),
        ((-50, 50, -50), 1),
        ((-50, 50, 0), 2),
        ((-50, 50, 50), 2),
        ((-50, 50, 100), 1),
        ((-50, 100, -100), 1),
        ((-50, 100, -50), 1),
        ((-50, 100, 0), 2),
        ((-50, 100, 50), 2),
        ((-50, 100, 100), 2),
        ((0, -100, -100), 1),
        ((0, -100, -50), 2),
        ((0, -100, 0), 2),
        ((0, -100, 50), 2),
        ((0, -100, 100), 1),
        ((0, -50, -100), 1),
        ((0, -50, -50), 1),
        ((0, -50, 0), 1),
        ((0, -50, 50), 1),
        ((0, -50, 100), 1),
        ((0, 0, -100), 2),
        ((0, 0, -50), 2),
        ((0, 0, 0), 1),
        ((0, 0, 50), 1),
        ((0, 0, 100), 1),
        ((0, 50, -100), 1),
        ((0, 50, -50), 1),
        ((0, 50, 0), 1),
        ((0, 50, 50), 1),
        ((0, 50, 100), 1),
        ((0, 100, -100), 1),
        ((0, 100, -50), 2),
        ((0, 100, 0), 1),
        ((0, 100, 50), 1),
        ((0, 100, 100), 1),
        ((50, -100, -100), 1),
        ((50, -100, -50), 2),
        ((50, -100, 0), 2),
        ((50, -100, 50), 2),
        ((50, -100, 100), 1),
        ((50, -50, -100), 1),
        ((50, -50, -50), 1),
        ((50, -50, 0), 1),
        ((50, -50, 50), 1),
        ((50, -50, 100), 1),
        ((50, 0, -100), 2),
        ((50, 0, -50), 2),
        ((50, 0, 0), 1),
        ((50, 0, 50), 1),
        ((50, 0, 100), 1),
        ((50, 50, -100), 1),
        ((50, 50, -50), 1),
        ((50, 50, 0), 1),
        ((50, 50, 50), 1),
        ((50, 50, 100), 1),
        ((50, 100, -100), 1),
        ((50, 100, -50), 2),
        ((50, 100, 0), 1),
        ((50, 100, 50), 1),
        ((50, 100, 100), 1),
        ((100, -100, -100), 1),
        ((100, -100, -50), 2),
        ((100, -100, 0), 1),
        ((100, -100, 50), 1),
        ((100, -100, 100), 1),
        ((100, -50, -100), 2),
        ((100, -50, -50), 2),
        ((100, -50, 0), 2),
        ((100, -50, 50), 2),
        ((100, -50, 100), 2),
        ((100, 0, -100), 1),
        ((100, 0, -50), 2),
        ((100, 0, 0), 1),
        ((100, 0, 50), 1),
        ((100, 0, 100), 2),
        ((100, 50, -100), 1),
        ((100, 50, -50), 1),
        ((100, 50, 0), 1),
        ((100, 50, 50), 1),
        ((100, 50, 100), 1),
        ((100, 100, -100), 2),
        ((100, 100, -50), 2),
        ((100, 100, 0), 1),
        ((100, 100, 50), 1),
        ((100, 100, 100), 1),
    ];
    type AqDensityKey = (i32, i32, i32, i32, i32);
    type AqPointKey = (i32, i32, i32, f64);
    const AQ_FL: &[(crate::BlockPos, (i32, u8))] = &[
        ((-100, -100, -100), (-32512, 2)),
        ((-100, -100, -50), (-32512, 2)),
        ((-100, -100, 0), (-32512, 2)),
        ((-100, -100, 50), (-32512, 2)),
        ((-100, -100, 100), (-32512, 2)),
        ((-100, -50, -100), (-32512, 1)),
        ((-100, -50, -50), (-63, 2)),
        ((-100, -50, 0), (-63, 2)),
        ((-100, -50, 50), (-63, 2)),
        ((-100, -50, 100), (-32512, 1)),
        ((-100, 0, -100), (-32512, 1)),
        ((-100, 0, -50), (-32512, 1)),
        ((-100, 0, 0), (-32512, 1)),
        ((-100, 0, 50), (-32512, 1)),
        ((-100, 0, 100), (-32512, 1)),
        ((-100, 50, -100), (-32512, 1)),
        ((-100, 50, -50), (63, 1)),
        ((-100, 50, 0), (63, 1)),
        ((-100, 50, 50), (-32512, 1)),
        ((-100, 50, 100), (63, 1)),
        ((-100, 100, -100), (63, 1)),
        ((-100, 100, -50), (63, 1)),
        ((-100, 100, 0), (63, 1)),
        ((-100, 100, 50), (63, 1)),
        ((-100, 100, 100), (63, 1)),
        ((-50, -100, -100), (-32512, 2)),
        ((-50, -100, -50), (-32512, 2)),
        ((-50, -100, 0), (-32512, 2)),
        ((-50, -100, 50), (-32512, 2)),
        ((-50, -100, 100), (-32512, 2)),
        ((-50, -50, -100), (-32512, 1)),
        ((-50, -50, -50), (-32512, 1)),
        ((-50, -50, 0), (-32512, 1)),
        ((-50, -50, 50), (-32512, 1)),
        ((-50, -50, 100), (-32512, 1)),
        ((-50, 0, -100), (-32512, 1)),
        ((-50, 0, -50), (-32512, 1)),
        ((-50, 0, 0), (-32512, 1)),
        ((-50, 0, 50), (-32512, 1)),
        ((-50, 0, 100), (-32512, 1)),
        ((-50, 50, -100), (-32512, 1)),
        ((-50, 50, -50), (63, 1)),
        ((-50, 50, 0), (63, 1)),
        ((-50, 50, 50), (-32512, 1)),
        ((-50, 50, 100), (63, 1)),
        ((-50, 100, -100), (63, 1)),
        ((-50, 100, -50), (63, 1)),
        ((-50, 100, 0), (63, 1)),
        ((-50, 100, 50), (63, 1)),
        ((-50, 100, 100), (63, 1)),
        ((0, -100, -100), (-32512, 2)),
        ((0, -100, -50), (-32512, 2)),
        ((0, -100, 0), (-32512, 2)),
        ((0, -100, 50), (-32512, 2)),
        ((0, -100, 100), (-32512, 2)),
        ((0, -50, -100), (-32512, 1)),
        ((0, -50, -50), (-32512, 1)),
        ((0, -50, 0), (-32512, 1)),
        ((0, -50, 50), (-32512, 1)),
        ((0, -50, 100), (-32512, 1)),
        ((0, 0, -100), (-32512, 1)),
        ((0, 0, -50), (-32512, 1)),
        ((0, 0, 0), (-32512, 1)),
        ((0, 0, 50), (-32512, 1)),
        ((0, 0, 100), (-32512, 1)),
        ((0, 50, -100), (-32512, 1)),
        ((0, 50, -50), (63, 1)),
        ((0, 50, 0), (63, 1)),
        ((0, 50, 50), (-32512, 1)),
        ((0, 50, 100), (-32512, 1)),
        ((0, 100, -100), (63, 1)),
        ((0, 100, -50), (63, 1)),
        ((0, 100, 0), (63, 1)),
        ((0, 100, 50), (63, 1)),
        ((0, 100, 100), (63, 1)),
        ((50, -100, -100), (-32512, 2)),
        ((50, -100, -50), (-32512, 2)),
        ((50, -100, 0), (-32512, 2)),
        ((50, -100, 50), (-32512, 2)),
        ((50, -100, 100), (-100, 2)),
        ((50, -50, -100), (-32512, 1)),
        ((50, -50, -50), (-32512, 1)),
        ((50, -50, 0), (-32512, 1)),
        ((50, -50, 50), (-32512, 1)),
        ((50, -50, 100), (-60, 1)),
        ((50, 0, -100), (-32512, 1)),
        ((50, 0, -50), (-32512, 1)),
        ((50, 0, 0), (-32512, 1)),
        ((50, 0, 50), (-32512, 1)),
        ((50, 0, 100), (-32512, 1)),
        ((50, 50, -100), (-32512, 1)),
        ((50, 50, -50), (63, 1)),
        ((50, 50, 0), (63, 1)),
        ((50, 50, 50), (63, 1)),
        ((50, 50, 100), (-32512, 1)),
        ((50, 100, -100), (-32512, 1)),
        ((50, 100, -50), (63, 1)),
        ((50, 100, 0), (63, 1)),
        ((50, 100, 50), (63, 1)),
        ((50, 100, 100), (63, 1)),
        ((100, -100, -100), (-32512, 2)),
        ((100, -100, -50), (-32512, 2)),
        ((100, -100, 0), (-32512, 2)),
        ((100, -100, 50), (-103, 2)),
        ((100, -100, 100), (-100, 2)),
        ((100, -50, -100), (-32512, 1)),
        ((100, -50, -50), (-32512, 1)),
        ((100, -50, 0), (-32512, 1)),
        ((100, -50, 50), (-32512, 1)),
        ((100, -50, 100), (-60, 2)),
        ((100, 0, -100), (-32512, 1)),
        ((100, 0, -50), (-32512, 1)),
        ((100, 0, 0), (-32512, 1)),
        ((100, 0, 50), (-32512, 1)),
        ((100, 0, 100), (-32512, 1)),
        ((100, 50, -100), (63, 1)),
        ((100, 50, -50), (63, 1)),
        ((100, 50, 0), (63, 1)),
        ((100, 50, 50), (63, 1)),
        ((100, 50, 100), (-32512, 1)),
        ((100, 100, -100), (63, 1)),
        ((100, 100, -50), (63, 1)),
        ((100, 100, 0), (63, 1)),
        ((100, 100, 50), (63, 1)),
        ((100, 100, 100), (63, 1)),
    ];
    // (x, y, z, first height, second height) -> the density the reference reported
    const AQ_CD: &[(AqDensityKey, f64)] = &[
        ((-100, -100, -100, 0, 0), 0.0),
        ((-100, -100, -50, 50, 0), -19.3),
        ((-100, -100, 0, 0, 0), 0.0),
        ((-100, -100, 50, 50, 0), -19.3),
        ((-100, -100, 100, 0, 0), 0.0),
        ((-100, -50, -100, 0, 0), 0.0),
        ((-100, -50, -50, 50, 0), -9.3),
        ((-100, -50, 0, 0, 0), 0.0),
        ((-100, -50, 50, 50, 0), -9.3),
        ((-100, -50, 100, 0, 0), 0.0),
        ((-100, 0, -100, 0, 0), 0.0),
        ((-100, 0, -50, -50, 0), 0.20838508),
        ((-100, 0, 0, 0, 0), 0.0),
        ((-100, 0, 50, 50, 0), 2.069189),
        ((-100, 0, 100, 0, 0), 0.0),
        ((-100, 50, -100, 0, 0), 0.0),
        ((-100, 50, -50, -50, 0), -40.4),
        ((-100, 50, 0, 0, 0), 0.0),
        ((-100, 50, 50, -50, 0), -40.4),
        ((-100, 50, 100, 0, 0), 0.0),
        ((-100, 100, -100, 0, 0), 0.0),
        ((-100, 100, -50, -50, 0), -80.4),
        ((-100, 100, 0, 0, 0), 0.0),
        ((-100, 100, 50, -50, 0), -80.4),
        ((-100, 100, 100, 0, 0), 0.0),
        ((-50, -100, -100, 0, 50), -19.3),
        ((-50, -100, -50, 50, 50), 0.0),
        ((-50, -100, 0, 0, -50), -9.3),
        ((-50, -100, 50, 50, -50), -9.3),
        ((-50, -100, 100, 0, -50), -9.3),
        ((-50, -50, -100, 0, 50), -9.3),
        ((-50, -50, -50, 50, 50), 0.0),
        ((-50, -50, 0, 0, -50), 2.2042949518442185),
        ((-50, -50, 50, 50, -50), 1.8767275908406176),
        ((-50, -50, 100, 0, -50), 2.3399656359995133),
        ((-50, 0, -100, 0, -50), -0.08405951),
        ((-50, 0, -50, -50, -50), 0.0),
        ((-50, 0, 0, 0, -50), 0.3902410585192353),
        ((-50, 0, 50, 50, -50), 66.0),
        ((-50, 0, 100, 0, -50), -0.7930165090675787),
        ((-50, 50, -100, 0, -50), -40.4),
        ((-50, 50, -50, -50, -50), 0.0),
        ((-50, 50, 0, 0, -50), -40.4),
        ((-50, 50, 50, -50, 50), -0.35570822400215646),
        ((-50, 50, 100, 0, 50), -0.16770224497207317),
        ((-50, 100, -100, 0, -50), -80.4),
        ((-50, 100, -50, -50, -50), 0.0),
        ((-50, 100, 0, 0, -50), -80.4),
        ((-50, 100, 50, -50, 50), -40.4),
        ((-50, 100, 100, 0, 50), -40.4),
        ((0, -100, -100, 0, 0), 0.0),
        ((0, -100, -50, -50, 0), -9.3),
        ((0, -100, 0, 0, 0), 0.0),
        ((0, -100, 50, 50, 0), -19.3),
        ((0, -100, 100, 0, 0), 0.0),
        ((0, -50, -100, 0, 0), 0.0),
        ((0, -50, -50, -50, 0), 2.857141340264507),
        ((0, -50, 0, 0, 0), 0.0),
        ((0, -50, 50, 50, 0), -9.3),
        ((0, -50, 100, 0, 0), 0.0),
        ((0, 0, -100, 0, 0), 0.0),
        ((0, 0, -50, -50, 0), -0.1361016501707068),
        ((0, 0, 0, 0, 0), 0.0),
        ((0, 0, 50, 50, 0), 1.9841279541408636),
        ((0, 0, 100, 0, 0), 0.0),
        ((0, 50, -100, 0, 0), 0.0),
        ((0, 50, -50, -50, 0), -40.4),
        ((0, 50, 0, 0, 0), 0.0),
        ((0, 50, 50, 50, 0), -0.36331007530382964),
        ((0, 50, 100, 0, 0), 0.0),
        ((0, 100, -100, 0, 0), 0.0),
        ((0, 100, -50, -50, 0), -80.4),
        ((0, 100, 0, 0, 0), 0.0),
        ((0, 100, 50, 50, 0), -40.4),
        ((0, 100, 100, 0, 0), 0.0),
        ((50, -100, -100, 0, 50), -19.3),
        ((50, -100, -50, -50, 50), -9.3),
        ((50, -100, 0, 0, 50), -19.3),
        ((50, -100, 50, -50, -50), 0.0),
        ((50, -100, 100, 0, -50), -9.3),
        ((50, -50, -100, 0, 50), -9.3),
        ((50, -50, -50, -50, 50), 1.619242225388449),
        ((50, -50, 0, 0, 50), -9.3),
        ((50, -50, 50, -50, -50), 0.0),
        ((50, -50, 100, 0, -50), 2.1561171703198188),
        ((50, 0, -100, 0, 50), 2.6298865590685954),
        ((50, 0, -50, -50, 50), 66.0),
        ((50, 0, 0, 0, 50), 2.572198917833846),
        ((50, 0, 50, 50, 50), 0.0),
        ((50, 0, 100, 0, 50), 2.082884998883258),
        ((50, 50, -100, 0, -50), -40.4),
        ((50, 50, -50, 50, -50), -0.1894344852785401),
        ((50, 50, 0, 0, 50), -0.7155260733519367),
        ((50, 50, 50, 50, 50), 0.0),
        ((50, 50, 100, 0, 50), -0.4132183490530098),
        ((50, 100, -100, 0, -50), -80.4),
        ((50, 100, -50, 50, -50), -40.4),
        ((50, 100, 0, 0, 50), -40.4),
        ((50, 100, 50, 50, 50), 0.0),
        ((50, 100, 100, 0, 50), -40.4),
        ((100, -100, -100, 0, 0), 0.0),
        ((100, -100, -50, -50, 0), -9.3),
        ((100, -100, 0, 0, 0), 0.0),
        ((100, -100, 50, -50, 0), -9.3),
        ((100, -100, 100, 0, 0), 0.0),
        ((100, -50, -100, 0, 0), 0.0),
        ((100, -50, -50, -50, 0), 1.6711026207576742),
        ((100, -50, 0, 0, 0), 0.0),
        ((100, -50, 50, -50, 0), 2.042353012197518),
        ((100, -50, 100, 0, 0), 0.0),
        ((100, 0, -100, 0, 0), 0.0),
        ((100, 0, -50, -50, 0), 0.3145492757856567),
        ((100, 0, 0, 0, 0), 0.0),
        ((100, 0, 50, 50, 0), 2.27260703684609),
        ((100, 0, 100, 0, 0), 0.0),
        ((100, 50, -100, 0, 0), 0.0),
        ((100, 50, -50, 50, 0), -0.16949328993376553),
        ((100, 50, 0, 0, 0), 0.0),
        ((100, 50, 50, 50, 0), 0.5196380801381327),
        ((100, 50, 100, 0, 0), 0.0),
        ((100, 100, -100, 0, 0), 0.0),
        ((100, 100, -50, 50, 0), -40.4),
        ((100, 100, 0, 0, 0), 0.0),
        ((100, 100, 50, 50, 0), -40.4),
        ((100, 100, 100, 0, 0), 0.0),
    ];
    // (x, y, z, density) -> which fluid the reference put there
    const AQ_AP: &[(AqPointKey, &str)] = &[
        ((112, -100, 64, 0.037482421875), "none"),
        ((112, -100, 66, 0.037482421875), "none"),
        ((112, -100, 68, 0.037482421875), "none"),
        ((112, -100, 70, 0.037482421875), "none"),
        ((112, -100, 72, 0.037482421875), "none"),
        ((112, -100, 74, 0.037482421875), "none"),
        ((112, -80, 64, 0.037482421875), "none"),
        ((112, -80, 66, 0.037482421875), "none"),
        ((112, -80, 68, 0.037482421875), "none"),
        ((112, -80, 70, 0.037482421875), "none"),
        ((112, -80, 72, 0.037482421875), "none"),
        ((112, -80, 74, 0.037482421875), "none"),
        ((112, -60, 64, 0.04861063447117113), "none"),
        ((112, -60, 66, 0.04924175767418443), "none"),
        ((112, -60, 68, 0.04989611436947345), "none"),
        ((112, -60, 70, 0.0505756649980066), "none"),
        ((112, -60, 72, 0.051280297037227154), "none"),
        ((112, -60, 74, 0.05200473277817172), "none"),
        ((112, -40, 64, 0.10768778489063564), "none"),
        ((112, -40, 66, 0.11167582884587932), "none"),
        ((112, -40, 68, 0.1150242394949102), "none"),
        ((112, -40, 70, 0.11846243756482717), "none"),
        ((112, -40, 72, 0.12198087391634192), "none"),
        ((112, -40, 74, 0.12558796524209848), "none"),
        ((112, -20, 64, 0.10797485850904144), "none"),
        ((112, -20, 66, 0.10725848123542484), "none"),
        ((112, -20, 68, 0.1061187765379769), "none"),
        ((112, -20, 70, 0.10630527141203541), "none"),
        ((112, -20, 72, 0.10955530075188523), "none"),
        ((112, -20, 74, 0.11296050502041746), "none"),
        ((112, 0, 64, -0.0016242211141160837), "none"),
        ((112, 0, 68, 0.0013878562706830444), "none"),
        ((112, 0, 70, 0.0030297756784707407), "none"),
        ((112, 0, 72, 0.004722310162617811), "none"),
        ((112, 0, 74, 0.0064699315064971315), "none"),
        ((112, 20, 64, 0.042774410930765096), "none"),
        ((112, 20, 66, 0.040191327400772414), "none"),
        ((112, 20, 68, 0.0375637494138927), "none"),
        ((112, 20, 70, 0.03485919313914573), "none"),
        ((112, 20, 72, 0.032058901444915404), "none"),
        ((112, 20, 74, 0.029170651813548235), "none"),
        ((112, 40, 64, 0.011596925999172775), "none"),
        ((112, 40, 66, 0.014957019593043965), "none"),
        ((112, 40, 68, 0.01827627643898028), "none"),
        ((112, 40, 70, 0.021477214738188897), "none"),
        ((112, 40, 72, 0.02448508577325224), "none"),
        ((112, 40, 74, 0.027262647782191486), "none"),
        ((112, 60, 64, 0.14338446306313316), "none"),
        ((112, 60, 66, 0.16772904485726645), "none"),
        ((112, 60, 68, 0.1756309873589998), "none"),
        ((112, 60, 70, 0.1782686032102433), "none"),
        ((112, 60, 72, 0.18822148746793055), "none"),
        ((112, 60, 74, 0.20387997189913717), "none"),
        ((112, 80, 64, -0.28931054817132484), "air"),
        ((112, 80, 66, -0.2808098154769529), "air"),
        ((112, 80, 68, -0.2806908647477032), "air"),
        ((112, 80, 70, -0.28068300576359284), "air"),
        ((112, 80, 72, -0.2805878392398348), "air"),
        ((112, 80, 74, -0.27824504138444317), "air"),
        ((112, 100, 64, -0.4583333333333333), "air"),
        ((112, 100, 66, -0.4583333333333333), "air"),
        ((112, 100, 68, -0.4583333333333333), "air"),
        ((112, 100, 70, -0.4583333333333333), "air"),
        ((112, 100, 72, -0.4583333333333333), "air"),
        ((112, 100, 74, -0.4583333333333333), "air"),
        ((114, -100, 64, 0.037482421875), "none"),
        ((114, -100, 66, 0.037482421875), "none"),
        ((114, -100, 68, 0.037482421875), "none"),
        ((114, -100, 70, 0.037482421875), "none"),
        ((114, -100, 72, 0.037482421875), "none"),
        ((114, -100, 74, 0.037482421875), "none"),
        ((114, -80, 64, 0.037482421875), "none"),
        ((114, -80, 66, 0.037482421875), "none"),
        ((114, -80, 68, 0.037482421875), "none"),
        ((114, -80, 70, 0.037482421875), "none"),
        ((114, -80, 72, 0.037482421875), "none"),
        ((114, -80, 74, 0.037482421875), "none"),
        ((114, -60, 64, 0.0491033911468506), "none"),
        ((114, -60, 66, 0.04974948234981454), "none"),
        ((114, -60, 68, 0.05041864489941496), "none"),
        ((114, -60, 70, 0.051112999802414315), "none"),
        ((114, -60, 72, 0.051832684146704056), "none"),
        ((114, -60, 74, 0.05257231446254198), "none"),
        ((114, -40, 64, 0.1122919611041932), "none"),
        ((114, -40, 66, 0.11636247433302638), "none"),
        ((114, -40, 68, 0.11979640774332366), "none"),
        ((114, -40, 70, 0.12330055500715384), "none"),
        ((114, -40, 72, 0.1268619579414046), "none"),
        ((114, -40, 74, 0.13048474950215), "none"),
        ((114, -20, 64, 0.10435296791344484), "none"),
        ((114, -20, 66, 0.10558672714675042), "none"),
        ((114, -20, 68, 0.10831868013423275), "none"),
        ((114, -20, 70, 0.11121282163190734), "none"),
        ((114, -20, 72, 0.11433776346079558), "none"),
        ((114, -20, 74, 0.11770444723497474), "none"),
        ((114, 0, 64, -0.0026209759846139574), "water"),
        ((114, 0, 66, -0.0011869543056835608), "water"),
        ((114, 0, 70, 0.002068623223791626), "none"),
        ((114, 0, 72, 0.0038193250297024243), "none"),
        ((114, 0, 74, 0.005646396361630968), "none"),
        ((114, 20, 64, 0.04183884597405475), "none"),
        ((114, 20, 66, 0.039334239558802865), "none"),
        ((114, 20, 68, 0.0367933735261107), "none"),
        ((114, 20, 70, 0.03417828296442246), "none"),
        ((114, 20, 72, 0.03146119496538579), "none"),
        ((114, 20, 74, 0.028640178813057193), "none"),
        ((114, 40, 64, 0.0011727557986376189), "none"),
        ((114, 40, 66, 0.004478197250912497), "none"),
        ((114, 40, 68, 0.007807656734373648), "none"),
        ((114, 40, 70, 0.0110982528501824), "none"),
        ((114, 40, 72, 0.014287968294347897), "none"),
        ((114, 40, 74, 0.017340060385442255), "none"),
        ((114, 60, 64, 0.057307692379898946), "none"),
        ((114, 60, 66, 0.07909878599088975), "none"),
        ((114, 60, 68, 0.09103769264897049), "none"),
        ((114, 60, 70, 0.10529675531043547), "none"),
        ((114, 60, 72, 0.1261191394093652), "none"),
        ((114, 60, 74, 0.15323465023530602), "none"),
        ((114, 80, 64, -0.3135251519473628), "air"),
        ((114, 80, 66, -0.3092766951165722), "air"),
        ((114, 80, 68, -0.3063751991759311), "air"),
        ((114, 80, 70, -0.3004342091280733), "air"),
        ((114, 80, 72, -0.29703745590700253), "air"),
        ((114, 80, 74, -0.2920638815250855), "air"),
        ((114, 100, 64, -0.4583333333333333), "air"),
        ((114, 100, 66, -0.4583333333333333), "air"),
        ((114, 100, 68, -0.4583333333333333), "air"),
        ((114, 100, 70, -0.4583333333333333), "air"),
        ((114, 100, 72, -0.4583333333333333), "air"),
        ((114, 100, 74, -0.4583333333333333), "air"),
        ((116, -100, 64, 0.037482421875), "none"),
        ((116, -100, 66, 0.037482421875), "none"),
        ((116, -100, 68, 0.037482421875), "none"),
        ((116, -100, 70, 0.037482421875), "none"),
        ((116, -100, 72, 0.037482421875), "none"),
        ((116, -100, 74, 0.037482421875), "none"),
        ((116, -80, 64, 0.037482421875), "none"),
        ((116, -80, 66, 0.037482421875), "none"),
        ((116, -80, 68, 0.037482421875), "none"),
        ((116, -80, 70, 0.037482421875), "none"),
        ((116, -80, 72, 0.037482421875), "none"),
        ((116, -80, 74, 0.037482421875), "none"),
        ((116, -60, 64, 0.049510031628008565), "none"),
        ((116, -60, 66, 0.0501688864114582), "none"),
        ((116, -60, 68, 0.05085081029508635), "none"),
        ((116, -60, 70, 0.051557962654552196), "none"),
        ((116, -60, 72, 0.05229073376331405), "none"),
        ((116, -60, 74, 0.05304386684600273), "none"),
        ((116, -40, 64, 0.11647696380354154), "none"),
        ((116, -40, 66, 0.12065670598690036), "none"),
        ((116, -40, 68, 0.124186198423662), "none"),
        ((116, -40, 70, 0.12776934001653206), "none"),
        ((116, -40, 72, 0.1313900411197126), "none"),
        ((116, -40, 74, 0.13504793394752057), "none"),
        ((116, -20, 64, 0.10797310440989095), "none"),
        ((116, -20, 66, 0.11052803071968675), "none"),
        ((116, -20, 68, 0.11313075983384659), "none"),
        ((116, -20, 70, 0.11589560860382501), "none"),
        ((116, -20, 72, 0.11889599517563405), "none"),
        ((116, -20, 74, 0.12214992807094607), "none"),
        ((116, 0, 64, -0.003764380972543319), "water"),
        ((116, 0, 66, -0.002339168705169207), "water"),
        ((116, 0, 72, 0.0027605740566328273), "none"),
        ((116, 0, 74, 0.0046712919320928475), "none"),
        ((116, 20, 64, 0.04027812111674997), "none"),
        ((116, 20, 66, 0.03846080745866824), "none"),
        ((116, 20, 68, 0.03602535830962611), "none"),
        ((116, 20, 70, 0.033477031827460015), "none"),
        ((116, 20, 72, 0.03082854239914883), "none"),
        ((116, 20, 74, 0.028967404819271153), "none"),
        ((116, 40, 64, -0.009355588931802767), "none"),
        ((116, 40, 66, -0.006094366713842806), "air"),
        ((116, 40, 68, -0.0027537988904787606), "air"),
        ((116, 40, 72, 0.00396682662711753), "none"),
        ((116, 40, 74, 0.007260950530417173), "none"),
        ((116, 60, 64, 0.022624582978071506), "none"),
        ((116, 60, 66, 0.023942871216082746), "none"),
        ((116, 60, 68, 0.037192323513527165), "none"),
        ((116, 60, 70, 0.053305618429865455), "none"),
        ((116, 60, 72, 0.06694547220363958), "none"),
        ((116, 60, 74, 0.08711813973093903), "none"),
        ((116, 80, 64, -0.3326652310213258), "air"),
        ((116, 80, 66, -0.32962834810938174), "air"),
        ((116, 80, 68, -0.32236370014057947), "air"),
        ((116, 80, 70, -0.31670491006554574), "air"),
        ((116, 80, 72, -0.3130639601887072), "air"),
        ((116, 80, 74, -0.3124769234268471), "air"),
        ((116, 100, 64, -0.4583333333333333), "air"),
        ((116, 100, 66, -0.4583333333333333), "air"),
        ((116, 100, 68, -0.4583333333333333), "air"),
        ((116, 100, 70, -0.4583333333333333), "air"),
        ((116, 100, 72, -0.4583333333333333), "air"),
        ((116, 100, 74, -0.4583333333333333), "air"),
        ((118, -100, 64, 0.037482421875), "none"),
        ((118, -100, 66, 0.037482421875), "none"),
        ((118, -100, 68, 0.037482421875), "none"),
        ((118, -100, 70, 0.037482421875), "none"),
        ((118, -100, 72, 0.037482421875), "none"),
        ((118, -100, 74, 0.037482421875), "none"),
        ((118, -80, 64, 0.037482421875), "none"),
        ((118, -80, 66, 0.037482421875), "none"),
        ((118, -80, 68, 0.037482421875), "none"),
        ((118, -80, 70, 0.037482421875), "none"),
        ((118, -80, 72, 0.037482421875), "none"),
        ((118, -80, 74, 0.037482421875), "none"),
        ((118, -60, 64, 0.04983026397203373), "none"),
        ((118, -60, 66, 0.05049926028532013), "none"),
        ((118, -60, 68, 0.05119129093033625), "none"),
        ((118, -60, 70, 0.05190836104462707), "none"),
        ((118, -60, 72, 0.0526510917415344), "none"),
        ((118, -60, 74, 0.05341462434689053), "none"),
        ((118, -40, 64, 0.12023506214440967), "none"),
        ((118, -40, 66, 0.12453642919562771), "none"),
        ((118, -40, 68, 0.1281709824736104), "none"),
        ((118, -40, 70, 0.13184553502467083), "none"),
        ((118, -40, 72, 0.13554113610267055), "none"),
        ((118, -40, 74, 0.13925273367632113), "none"),
        ((118, -20, 64, 0.11282016461369758), "none"),
        ((118, -20, 66, 0.11526093589883914), "none"),
        ((118, -20, 68, 0.11774539644635261), "none"),
        ((118, -20, 70, 0.12038440430256446), "none"),
        ((118, -20, 72, 0.12325317705242089), "none"),
        ((118, -20, 74, 0.12637678353248477), "none"),
        ((118, 0, 64, -0.00501589634392619), "water"),
        ((118, 0, 66, -0.003601631485605401), "water"),
        ((118, 0, 68, -0.0020166185756455924), "water"),
        ((118, 0, 72, 0.0015704124446037308), "none"),
        ((118, 0, 74, 0.0035605198020311826), "none"),
        ((118, 20, 64, 0.040232966220497414), "none"),
        ((118, 20, 66, 0.039486572716430204), "none"),
        ((118, 20, 68, 0.03885242305023035), "none"),
        ((118, 20, 70, 0.03837601204655802), "none"),
        ((118, 20, 72, 0.03770430407795084), "none"),
        ((118, 20, 74, 0.03707763999605109), "none"),
        ((118, 40, 64, -0.016298811685686653), "none"),
        ((118, 40, 66, -0.016656636719901533), "air"),
        ((118, 40, 68, -0.01330299024830442), "air"),
        ((118, 40, 70, -0.009864486324034218), "air"),
        ((118, 40, 72, -0.006380723268648157), "air"),
        ((118, 40, 74, -0.002886835272463701), "air"),
        ((118, 60, 64, 0.006086790713922152), "none"),
        ((118, 60, 66, 0.006014479808113486), "none"),
        ((118, 60, 68, 0.008953321476532182), "none"),
        ((118, 60, 70, 0.011915636473415587), "none"),
        ((118, 60, 72, 0.01001192490238903), "none"),
        ((118, 60, 74, 0.0075500927486281426), "none"),
        ((118, 80, 64, -0.3462118919745469), "air"),
        ((118, 80, 66, -0.34419241078645835), "air"),
        ((118, 80, 68, -0.33580861045450133), "air"),
        ((118, 80, 70, -0.33008534054566163), "air"),
        ((118, 80, 72, -0.333649815109498), "air"),
        ((118, 80, 74, -0.33771329428807284), "air"),
        ((118, 100, 64, -0.4583333333333333), "air"),
        ((118, 100, 66, -0.4583333333333333), "air"),
        ((118, 100, 68, -0.4583333333333333), "air"),
        ((118, 100, 70, -0.4583333333333333), "air"),
        ((118, 100, 72, -0.4583333333333333), "air"),
        ((118, 100, 74, -0.4583333333333333), "air"),
        ((120, -100, 64, 0.037482421875), "none"),
        ((120, -100, 66, 0.037482421875), "none"),
        ((120, -100, 68, 0.037482421875), "none"),
        ((120, -100, 70, 0.037482421875), "none"),
        ((120, -100, 72, 0.037482421875), "none"),
        ((120, -100, 74, 0.037482421875), "none"),
        ((120, -80, 64, 0.037482421875), "none"),
        ((120, -80, 66, 0.037482421875), "none"),
        ((120, -80, 68, 0.037482421875), "none"),
        ((120, -80, 70, 0.037482421875), "none"),
        ((120, -80, 72, 0.037482421875), "none"),
        ((120, -80, 74, 0.037482421875), "none"),
        ((120, -60, 64, 0.05006400032487905), "none"),
        ((120, -60, 66, 0.05074065517392799), "none"),
        ((120, -60, 68, 0.05144011744518559), "none"),
        ((120, -60, 70, 0.05216399757431933), "none"),
        ((120, -60, 72, 0.052913091382129525), "none"),
        ((120, -60, 74, 0.053683198493088384), "none"),
        ((120, -40, 64, 0.1235607292468892), "none"),
        ((120, -40, 66, 0.12798336289135304), "none"),
        ((120, -40, 68, 0.13173164094126777), "none"),
        ((120, -40, 70, 0.13550880522456066), "none"),
        ((120, -40, 72, 0.1392932761010718), "none"),
        ((120, -40, 74, 0.14307520910884483), "none"),
        ((120, -20, 64, 0.11746888767713162), "none"),
        ((120, -20, 66, 0.1198086187834423), "none"),
        ((120, -20, 68, 0.12218509691580316), "none"),
        ((120, -20, 70, 0.12469970986670785), "none"),
        ((120, -20, 72, 0.1274266324562779), "none"),
        ((120, -20, 74, 0.13039812171785095), "none"),
        ((120, 0, 64, -0.006329576321547214), "water"),
        ((120, 0, 66, -0.004930192503238298), "water"),
        ((120, 0, 68, -0.003355964670343278), "water"),
        ((120, 0, 70, -0.0016206542469077872), "water"),
        ((120, 0, 74, 0.002332419553726638), "none"),
        ((120, 20, 64, 0.04783863627983937), "none"),
        ((120, 20, 66, 0.04699098011102891), "none"),
        ((120, 20, 68, 0.046231913852781324), "none"),
        ((120, 20, 70, 0.04560004002476374), "none"),
        ((120, 20, 72, 0.04512419653798615), "none"),
        ((120, 20, 74, 0.04482086018104904), "none"),
        ((120, 40, 64, -0.017456523705167773), "none"),
        ((120, 40, 66, -0.020044623482270124), "air"),
        ((120, 40, 68, -0.022372181411266172), "air"),
        ((120, 40, 70, -0.020228945291907708), "air"),
        ((120, 40, 72, -0.01664436674077766), "air"),
        ((120, 40, 74, -0.013001583733654043), "air"),
        ((120, 60, 64, -0.010805185122555435), "water"),
        ((120, 60, 66, -0.011684313707812422), "water"),
        ((120, 60, 68, -0.007705484690135335), "water"),
        ((120, 60, 70, -0.012326309226980426), "water"),
        ((120, 60, 72, -0.019043795741958334), "water"),
        ((120, 60, 74, -0.023185441889689514), "water"),
        ((120, 80, 64, -0.3611328625547435), "air"),
        ((120, 80, 66, -0.3586517592327399), "air"),
        ((120, 80, 68, -0.3524534485283812), "air"),
        ((120, 80, 70, -0.35323218454039057), "air"),
        ((120, 80, 72, -0.36213549677301105), "air"),
        ((120, 80, 74, -0.3684474143996314), "air"),
        ((120, 100, 64, -0.4583333333333333), "air"),
        ((120, 100, 66, -0.4583333333333333), "air"),
        ((120, 100, 68, -0.4583333333333333), "air"),
        ((120, 100, 70, -0.4583333333333333), "air"),
        ((120, 100, 72, -0.4583333333333333), "air"),
        ((120, 100, 74, -0.4583333333333333), "air"),
        ((122, -100, 64, 0.037482421875), "none"),
        ((122, -100, 66, 0.037482421875), "none"),
        ((122, -100, 68, 0.037482421875), "none"),
        ((122, -100, 70, 0.037482421875), "none"),
        ((122, -100, 72, 0.037482421875), "none"),
        ((122, -100, 74, 0.037482421875), "none"),
        ((122, -80, 64, 0.037482421875), "none"),
        ((122, -80, 66, 0.037482421875), "none"),
        ((122, -80, 68, 0.037482421875), "none"),
        ((122, -80, 70, 0.037482421875), "none"),
        ((122, -80, 72, 0.037482421875), "none"),
        ((122, -80, 74, 0.037482421875), "none"),
        ((122, -60, 64, 0.050211152183993704), "none"),
        ((122, -60, 66, 0.05089355899866971), "none"),
        ((122, -60, 68, 0.05159826596516877), "none"),
        ((122, -60, 70, 0.05232622949491639), "none"),
        ((122, -60, 72, 0.053078329171471185), "none"),
        ((122, -60, 74, 0.05385122904837061), "none"),
        ((122, -40, 64, 0.12644943947294945), "none"),
        ((122, -40, 66, 0.13095960991588632), "none"),
        ((122, -40, 68, 0.13485451601704876), "none"),
        ((122, -40, 70, 0.13874405974643175), "none"),
        ((122, -40, 72, 0.1426292409194739), "none"),
        ((122, -40, 74, 0.14649538659711875), "none"),
        ((122, -20, 64, 0.12191638116406471), "none"),
        ((122, -20, 66, 0.12416901766463509), "none"),
        ((122, -20, 68, 0.1264480340172126), "none"),
        ((122, -20, 70, 0.12884021810202248), "none"),
        ((122, -20, 72, 0.13141636494496137), "none"),
        ((122, -20, 74, 0.13421609155559988), "none"),
        ((122, 0, 64, -0.007667256303541582), "water"),
        ((122, 0, 66, -0.006288822820341533), "water"),
        ((122, 0, 68, -0.004737470102527975), "water"),
        ((122, 0, 70, -0.0030099389619020873), "water"),
        ((122, 0, 72, -0.0010942861750551764), "water"),
        ((122, 0, 74, 0.001001992078975999), "none"),
        ((122, 20, 64, 0.05535892661135848), "none"),
        ((122, 20, 66, 0.05444413322973901), "none"),
        ((122, 20, 68, 0.05359719969130291), "none"),
        ((122, 20, 70, 0.05284624631887433), "none"),
        ((122, 20, 72, 0.052209127741325245), "none"),
        ((122, 20, 74, 0.05170112238940831), "none"),
        ((122, 40, 64, -0.013498316953033454), "none"),
        ((122, 40, 66, -0.016896390550754353), "air"),
        ((122, 40, 68, -0.01994683889106233), "air"),
        ((122, 40, 70, -0.022658183924480487), "air"),
        ((122, 40, 72, -0.02460705550987633), "air"),
        ((122, 40, 74, -0.02133677750482264), "none"),
        ((122, 60, 64, -0.02580014098083049), "water"),
        ((122, 60, 66, -0.027410062228040422), "water"),
        ((122, 60, 68, -0.02425659570836858), "water"),
        ((122, 60, 70, -0.03261718168256943), "water"),
        ((122, 60, 72, -0.04369665936638442), "water"),
        ((122, 60, 74, -0.04490159197647781), "water"),
        ((122, 80, 64, -0.37247525946547166), "air"),
        ((122, 80, 66, -0.3727266378002749), "air"),
        ((122, 80, 68, -0.36804742745663505), "air"),
        ((122, 80, 70, -0.3736723706537362), "air"),
        ((122, 80, 72, -0.3860951288334311), "air"),
        ((122, 80, 74, -0.3923721309133264), "air"),
        ((122, 100, 64, -0.4583333333333333), "air"),
        ((122, 100, 66, -0.4583333333333333), "air"),
        ((122, 100, 68, -0.4583333333333333), "air"),
        ((122, 100, 70, -0.4583333333333333), "air"),
        ((122, 100, 72, -0.4583333333333333), "air"),
        ((122, 100, 74, -0.4583333333333333), "air"),
        ((124, -100, 64, 0.037482421875), "none"),
        ((124, -100, 66, 0.037482421875), "none"),
        ((124, -100, 68, 0.037482421875), "none"),
        ((124, -100, 70, 0.037482421875), "none"),
        ((124, -100, 72, 0.037482421875), "none"),
        ((124, -100, 74, 0.037482421875), "none"),
        ((124, -80, 64, 0.037482421875), "none"),
        ((124, -80, 66, 0.037482421875), "none"),
        ((124, -80, 68, 0.037482421875), "none"),
        ((124, -80, 70, 0.037482421875), "none"),
        ((124, -80, 72, 0.037482421875), "none"),
        ((124, -80, 74, 0.037482421875), "none"),
        ((124, -60, 64, 0.05027131800090962), "none"),
        ((124, -60, 66, 0.0509584844305151), "none"),
        ((124, -60, 68, 0.05166719216129481), "none"),
        ((124, -60, 70, 0.05239749814810798), "none"),
        ((124, -60, 72, 0.053150241639753654), "none"),
        ((124, -60, 74, 0.053923063702221205), "none"),
        ((124, -40, 64, 0.1288961855391929), "none"),
        ((124, -40, 66, 0.13348604127036617), "none"),
        ((124, -40, 68, 0.13753360917037172), "none"),
        ((124, -40, 70, 0.1415440368478666), "none"),
        ((124, -40, 72, 0.1455395599824845), "none"),
        ((124, -40, 74, 0.1495006614184435), "none"),
        ((124, -20, 64, 0.12612367959767024), "none"),
        ((124, -20, 66, 0.12830319402402945), "none"),
        ((124, -20, 68, 0.1304960062341348), "none"),
        ((124, -20, 70, 0.1327707947453995), "none"),
        ((124, -20, 72, 0.13519345838131658), "none"),
        ((124, -20, 74, 0.13781110986582973), "none"),
        ((124, 0, 64, -0.009009809178163559), "water"),
        ((124, 0, 66, -0.007660237459532607), "water"),
        ((124, 0, 68, -0.0061446463166489424), "water"),
        ((124, 0, 70, -0.004442459854201204), "water"),
        ((124, 0, 72, -0.0025318494505197223), "water"),
        ((124, 20, 64, 0.06277697412858188), "none"),
        ((124, 20, 66, 0.06182400117210263), "none"),
        ((124, 20, 68, 0.060920470163534336), "none"),
        ((124, 20, 70, 0.06008166953195572), "none"),
        ((124, 20, 72, 0.05931163578048011), "none"),
        ((124, 20, 74, 0.05862261977236657), "none"),
        ((124, 40, 64, -0.004596793013348681), "none"),
        ((124, 40, 66, -0.007881293688553023), "none"),
        ((124, 40, 68, -0.010851313478373932), "none"),
        ((124, 40, 70, -0.013513605008223933), "water"),
        ((124, 40, 72, -0.015880866729427373), "water"),
        ((124, 40, 74, -0.017978317799117856), "water"),
        ((124, 60, 64, -0.033729060298063454), "water"),
        ((124, 60, 66, -0.04062740064249005), "water"),
        ((124, 60, 68, -0.03660634922712756), "water"),
        ((124, 60, 70, -0.04106936165998065), "water"),
        ((124, 60, 72, -0.048715160337165046), "water"),
        ((124, 60, 74, -0.053817378732386144), "water"),
        ((124, 80, 64, -0.378513110274629), "air"),
        ((124, 80, 66, -0.37887533037235366), "air"),
        ((124, 80, 68, -0.3755672366866089), "air"),
        ((124, 80, 70, -0.3806264904596738), "air"),
        ((124, 80, 72, -0.39139114552312176), "air"),
        ((124, 80, 74, -0.39905004304932734), "air"),
        ((124, 100, 64, -0.4583333333333333), "air"),
        ((124, 100, 66, -0.4583333333333333), "air"),
        ((124, 100, 68, -0.4583333333333333), "air"),
        ((124, 100, 70, -0.4583333333333333), "air"),
        ((124, 100, 72, -0.4583333333333333), "air"),
        ((124, 100, 74, -0.4583333333333333), "air"),
        ((126, -100, 64, 0.037482421875), "none"),
        ((126, -100, 66, 0.037482421875), "none"),
        ((126, -100, 68, 0.037482421875), "none"),
        ((126, -100, 70, 0.037482421875), "none"),
        ((126, -100, 72, 0.037482421875), "none"),
        ((126, -100, 74, 0.037482421875), "none"),
        ((126, -80, 64, 0.037482421875), "none"),
        ((126, -80, 66, 0.037482421875), "none"),
        ((126, -80, 68, 0.037482421875), "none"),
        ((126, -80, 70, 0.037482421875), "none"),
        ((126, -80, 72, 0.037482421875), "none"),
        ((126, -80, 74, 0.037482421875), "none"),
        ((126, -60, 64, 0.05024330978368198), "none"),
        ((126, -60, 66, 0.05093542123713047), "none"),
        ((126, -60, 68, 0.05164825745773993), "none"),
        ((126, -60, 70, 0.052380776473173525), "none"),
        ((126, -60, 72, 0.053133618951577574), "none"),
        ((126, -60, 74, 0.05390538149459787), "none"),
        ((126, -40, 64, 0.1308942633558083), "none"),
        ((126, -40, 66, 0.1355722846035891), "none"),
        ((126, -40, 68, 0.13977234300629068), "none"),
        ((126, -40, 70, 0.14391148475400958), "none"),
        ((126, -40, 72, 0.1480252139578508), "none"),
        ((126, -40, 74, 0.15208907413316627), "none"),
        ((126, -20, 64, 0.13001056656809973), "none"),
        ((126, -20, 66, 0.13213006749070327), "none"),
        ((126, -20, 68, 0.1342494739710899), "none"),
        ((126, -20, 70, 0.13641807828631622), "none"),
        ((126, -20, 72, 0.13869641277763253), "none"),
        ((126, -20, 74, 0.1411390651002113), "none"),
        ((126, 0, 64, -0.010355206926252296), "water"),
        ((126, 0, 66, -0.009043874021560911), "water"),
        ((126, 0, 68, -0.007576245457331987), "water"),
        ((126, 0, 70, -0.005915269354878528), "water"),
        ((126, 0, 72, -0.0040306175772153365), "water"),
        ((126, 0, 74, -0.0019328609472880898), "water"),
        ((126, 20, 64, 0.0700499260773044), "none"),
        ((126, 20, 66, 0.06908003164862005), "none"),
        ((126, 20, 68, 0.0681425614589177), "none"),
        ((126, 20, 70, 0.06723915989832648), "none"),
        ((126, 20, 72, 0.066359174495176), "none"),
        ((126, 20, 74, 0.06551116407146489), "none"),
        ((126, 40, 64, 0.004497597038868666), "none"),
        ((126, 40, 66, 0.0013502912778844962), "none"),
        ((126, 40, 68, -0.0015191728313191184), "water"),
        ((126, 40, 70, -0.0041188588354404134), "water"),
        ((126, 40, 72, -0.006463772144671846), "water"),
        ((126, 40, 74, -0.008581034519921562), "water"),
        ((126, 60, 64, -0.03471424652823008), "water"),
        ((126, 60, 66, -0.04732045558891548), "water"),
        ((126, 60, 68, -0.04568337003176991), "water"),
        ((126, 60, 70, -0.0428377824231183), "water"),
        ((126, 60, 72, -0.04738820166968918), "water"),
        ((126, 60, 74, -0.05663750895047857), "water"),
        ((126, 80, 64, -0.37931742687180287), "air"),
        ((126, 80, 66, -0.38265481838544235), "air"),
        ((126, 80, 68, -0.3808041835281554), "air"),
        ((126, 80, 70, -0.38160238129796925), "air"),
        ((126, 80, 72, -0.387746448733821), "air"),
        ((126, 80, 74, -0.3990668807989283), "air"),
        ((126, 100, 64, -0.4583333333333333), "air"),
        ((126, 100, 66, -0.4583333333333333), "air"),
        ((126, 100, 68, -0.4583333333333333), "air"),
        ((126, 100, 70, -0.4583333333333333), "air"),
        ((126, 100, 72, -0.4583333333333333), "air"),
        ((126, 100, 74, -0.4583333333333333), "air"),
    ];

    #[test]
    fn aquifer_vectors() {
        let _guard = lock_gen();
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut miss = 0u32;
        {
            let g = &mut g;
            for &((x, y, z), want) in AQ_NB {
                let got = g.aq_noise_level(x, y, z, 200);
                if got != want {
                    miss += 1;
                    eprintln!("nb {x},{y},{z}: {got} != {want}");
                }
            }
            for (tbl, my) in [(AQ_FYT, true), (AQ_FYF, false)] {
                for &((x, y, z), want) in tbl {
                    let got = g.aq_fluid_y(x, y, z, (0, 1), 80, my);
                    if got != want {
                        miss += 1;
                        eprintln!("fy[{my}] {x},{y},{z}: {got} != {want}");
                    }
                }
            }
            for &((x, y, z), want) in AQ_FBS {
                let got = g.aq_fluid_kind(x, y, z, (0, 1), -10);
                if got != want {
                    miss += 1;
                    eprintln!("fbs {x},{y},{z}: {got} != {want}");
                }
            }
            for &((x, y, z), (wy, wk)) in AQ_FL {
                let got = g.aq_fluid_level_at(x, y, z);
                if got != (wy, wk) {
                    miss += 1;
                    eprintln!("fl {x},{y},{z}: {got:?} != ({wy},{wk})");
                }
            }
            for &((x, y, z, h1, h2), want) in AQ_CD {
                let mut b = None;
                let got = g.aq_calc_density(x, y, z, (h1, 1), (h2, 1), &mut b);
                if (got - want).abs() > 1e-4 {
                    miss += 1;
                    eprintln!("cd {x},{y},{z},{h1},{h2}: {got} != {want}");
                }
            }
        }
        let water = crate::name_to_state("minecraft:water").unwrap();
        let lava = crate::name_to_state("minecraft:lava").unwrap();
        for &((x, y, z, d), blk) in AQ_AP {
            let got = g.aq_state(x, y, z, d);
            let want = match blk {
                "none" => None,
                "air" => Some(0),
                "water" => Some(water),
                "lava" => Some(lava),
                _ => unreachable!(),
            };
            if got != want {
                miss += 1;
                eprintln!("ap {x},{y},{z}: {got:?} != {want:?}");
            }
        }
        assert_eq!(miss, 0, "aquifer vector mismatches");
    }

    #[test]
    fn aqdist() {
        let _guard = lock_gen();
        if std::env::var("WGAQ").is_err() {
            return;
        }
        assert!(init(0, crate::name_to_state));
        let (mut air, mut wat, mut lav, mut sol, mut cave) = (0, 0, 0, 0, 0);
        for cx in 0..4 {
            for cz in 0..4 {
                let _cc = chunk_cols(cx, cz).unwrap();
                for lx in 0..16 {
                    for lz in 0..16 {
                        for y in (-60..60).step_by(2) {
                            let d = density_at(cx * 16 + lx, y, cz * 16 + lz).unwrap_or(0.0);
                            if d <= 0.0 {
                                cave += 1;
                                match aquifer_state(cx * 16 + lx, y, cz * 16 + lz, d) {
                                    Some(0) => air += 1,
                                    Some(80) => wat += 1,
                                    Some(96) => lav += 1,
                                    Some(_) => sol += 1,
                                    None => sol += 1,
                                }
                            }
                        }
                    }
                }
            }
        }
        eprintln!("AqdIST caves={cave} air={air} water={wat} lava={lav} solid={sol}");
    }

    #[test]
    fn perf_probe() {
        let _guard = lock_gen();
        if std::env::var("WGPERF").is_err() {
            return;
        }
        assert!(init(0, crate::name_to_state));
        let t = std::time::Instant::now();
        let mut n = 0;
        for cx in 0..8 {
            for cz in 0..8 {
                chunk_cols(cx, cz).unwrap();
                n += 1;
            }
        }
        let el = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        eprintln!("PERF chunk_cols: {el:.2} ms/chunk ({n} chunks)");
        // aquifer+surface cell evaluation cost via deep_state on a few chunks
        let t = std::time::Instant::now();
        let mut m = 0;
        for cx in 0..4 {
            for cz in 0..4 {
                let cc = chunk_cols(cx, cz).unwrap();
                for y in (-64..64).step_by(2) {
                    for lx in 0..16 {
                        for lz in 0..16 {
                            deep_state(cx * 16 + lx, y, cz * 16 + lz, &cc);
                            m += 1;
                        }
                    }
                }
            }
        }
        eprintln!(
            "PERF deep_state: {:.4} ms/call",
            t.elapsed().as_secs_f64() * 1000.0 / m as f64
        );
    }

    /// Lane PERF-DF: the load-time [`fold_constant_dfs`] pass must (a) fire on
    /// the pinned pack, (b) leave every evaluated density bit-identical, and
    /// (c) not regress chunk terrain generation. The unfolded twin loader is
    /// the "before" side of the A/B — same inputs, fold skipped — so no env
    /// switch or second fixture is needed. The post-load RAM trim is verified
    /// by the build-only maps being empty on both generators.
    #[test]
    fn perf_df_constant_fold_counts_and_benchmarks_chunk_terrain() {
        crate::test_support::require_vanilla_data();
        let mut folded = Gen::load_preset_uncached(0, crate::name_to_state, NoisePreset::Overworld)
            .expect("folded generator");
        let mut raw =
            Gen::load_preset_uncached_unfolded(0, crate::name_to_state, NoisePreset::Overworld)
                .expect("unfolded generator");
        assert!(
            folded.df_folded > 0,
            "the pinned pack exercises no constant fold"
        );
        assert_eq!(raw.df_folded, 0, "the unfolded loader must skip the fold");
        assert_eq!(
            folded.dfs.len(),
            raw.dfs.len(),
            "folding rewrites in place; the id table cannot grow or shrink"
        );
        for g in [&folded, &raw] {
            assert!(g.names.is_empty() && g.noise_names.is_empty());
            assert!(g.rule_ids.is_empty() && g.cond_ids.is_empty());
            assert!(!g.bleaf.is_empty(), "biome_leaf_of_name is a runtime lookup");
        }
        assert!(
            folded.ram_trimmed > 0,
            "the post-load trim must reclaim measurable slack"
        );
        eprintln!(
            "PERF_DF_RAM_TRIM reclaimed_bytes folded={} unfolded={}",
            folded.ram_trimmed, raw.ram_trimmed
        );
        let folded_ids: Vec<usize> = (0..raw.dfs.len())
            .filter(|&i| {
                matches!(folded.dfs[i], DF::C(_)) && !matches!(raw.dfs[i], DF::C(_))
            })
            .collect();
        assert_eq!(folded_ids.len(), folded.df_folded);
        eprintln!(
            "PERF_DF_CONSTANT_FOLD nodes: {:?}",
            folded_ids
                .iter()
                .map(|&i| (i, df_kind(&raw.dfs[i])))
                .collect::<Vec<_>>()
        );

        // Bit identity across the fold boundary: the interpreted evaluator and
        // the trilerped chunk path must agree bit for bit.
        for (x, y, z) in [(0, 0, 0), (3, -20, -7), (-129, 64, 255), (1000, 200, -333)] {
            let a = folded.eval(folded.final_density, x, y, z, &mut HashMap::new());
            let b = raw.eval(raw.final_density, x, y, z, &mut HashMap::new());
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "interpreted density differs at ({x},{y},{z})"
            );
        }
        for (cx, cz) in [(0, 0), (-3, 5)] {
            let fc = folded.cols(cx, cz).expect("folded cols");
            let rc = raw.cols(cx, cz).expect("raw cols");
            for (x, y, z) in [
                (cx * 16, -40, cz * 16),
                (cx * 16 + 7, 90, cz * 16 + 9),
                (cx * 16 + 3, 12, cz * 16 + 15),
            ] {
                let a = folded.block_density(&fc, x, y, z);
                let b = raw.block_density(&rc, x, y, z);
                assert_eq!(
                    a.to_bits(),
                    b.to_bits(),
                    "block density differs at ({x},{y},{z})"
                );
            }
        }

        // Chunk terrain generation before/after: `cols` builds the whole
        // column set (interp grids + density + surface), the same work the
        // terrain stage does per chunk.
        for (label, g) in [("unfolded", &mut raw), ("folded", &mut folded)] {
            let t = std::time::Instant::now();
            let mut built = 0u32;
            for cx in 0..4i32 {
                for cz in 0..4i32 {
                    if g.cols(cx, cz).is_some() {
                        built += 1;
                    }
                }
            }
            eprintln!(
                "PERF_DF_CONSTANT_FOLD {label}: folded_nodes={} chunks={built} \
                 elapsed={:?} per_chunk={:?}",
                g.df_folded,
                t.elapsed(),
                t.elapsed() / built.max(1)
            );
        }
    }

    /// The published immutable facts must equal the generator's own values: they are
    /// read without the world lock, so a divergence would be silent (and every
    /// dimension-aware reader would be wrong).
    #[test]
    fn published_gen_facts_match_the_generator() {
        let _guard = lock_gen();
        assert!(init_preset(0, crate::name_to_state, NoisePreset::End));
        let f = facts().expect("facts published at init");
        let (g_sea, g_min_y, g_height, g_def_st, g_def_fl, g_bedrock, preset) = with_gen(|g| {
            (
                g.sea,
                g.min_y,
                g.height,
                g.def_st,
                g.def_fl,
                g.bedrock_st,
                g.preset,
            )
        })
        .expect("generator");
        assert_eq!(
            (f.sea, f.min_y, f.height, f.def_st, f.def_fl, f.bedrock_st),
            (g_sea, g_min_y, g_height, g_def_st, g_def_fl, g_bedrock)
        );
        assert_eq!(f.preset, preset);
        assert_eq!(min_y(), f.min_y);
        assert_eq!(max_y(), f.min_y + f.height - 1);
        assert_eq!(sea_level(), f.sea);
        // leave the shared fixture in the Overworld for the other tests
        assert!(init(0, crate::name_to_state));
        assert_eq!(facts().expect("facts").preset, NoisePreset::Overworld);
    }

    /// There are **two** heights, from two files, and they are not interchangeable.
    ///
    /// `worldgen/noise_settings/<preset>.json` is the generator: terrain, density,
    /// aquifers, carvers, surface rules and the biome quart sweep all live in it, and
    /// 1.21.1 gives the Nether and the End `min_y 0 / height 128` there. `dimension_type/
    /// <dimension>.json` is the world: chunk sections, heightmaps, the dump length and
    /// structure/furniture bounds, and 1.21.1 gives the Nether and the End `0 / 256` there.
    /// The overworld agrees at -64/384, which is why one number looked sufficient.
    ///
    /// The numbers are asserted against the pack files themselves, not against a Rust
    /// constant: if the data changes the test follows it, and if the loader stops reading
    /// the file the two sides disagree.
    #[test]
    fn dimension_type_declares_a_world_window_containing_the_generator() {
        let _guard = lock_gen();
        for (preset, file, gen, world) in [
            (NoisePreset::Overworld, "overworld.json", (-64, 384), (-64, 384)),
            (NoisePreset::Nether, "the_nether.json", (0, 128), (0, 256)),
            (NoisePreset::End, "the_end.json", (0, 128), (0, 256)),
        ] {
            assert!(init_preset(0, crate::name_to_state, preset));
            let f = facts().expect("facts published at init");
            assert_eq!(
                (f.min_y, f.height),
                gen,
                "{preset:?}: the generator window comes from noise_settings"
            );
            assert_eq!(
                (f.world_min_y, f.world_height),
                world,
                "{preset:?}: the world window comes from dimension_type"
            );
            assert_eq!(
                (world_min_y(), world_height(), world_max_y()),
                (world.0, world.1, world.0 + world.1 - 1),
                "{preset:?}: the free functions must agree with the published facts"
            );
            // A world window that does not contain the generator's would cut terrain.
            assert!(f.world_min_y <= f.min_y, "{preset:?}");
            assert!(
                f.world_min_y + f.world_height >= f.min_y + f.height,
                "{preset:?}"
            );
            assert!(f.world_height >= f.height, "{preset:?}");
            // And the numbers are the pack's own, read the way the loader reads them.
            let path = std::path::Path::new("datapacks/vanilla/data/minecraft/dimension_type")
                .join(file);
            let raw = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let j = jparse(&raw).unwrap_or_else(|| panic!("{}: invalid JSON", path.display()));
            assert_eq!(
                (
                    j.get("min_y").map(J::num).map(|v| v as i32),
                    j.get("height").map(J::num).map(|v| v as i32)
                ),
                (Some(f.world_min_y), Some(f.world_height)),
                "{}: the loaded world window must be this file's min_y/height",
                path.display()
            );
        }
        // leave the shared fixture in the Overworld for the other tests
        assert!(init(0, crate::name_to_state));
    }

    /// P2-3-S1: one registry, three dimensions, one seed, three answers.
    ///
    /// Before the registry the process held a single generator and "which dimension" was a
    /// property of `init`'s last call, so a Nether answer could only be had by *replacing* the
    /// Overworld's. Here all three are installed from the same seed and each answers from its
    /// own `dimension_type` — 0/256 and no sky light for the Nether and the End, -64/384 and a
    /// sky layer for the Overworld — and asking one does not disturb another.
    #[test]
    fn p2_3_s1_three_dimensions_answer_independently() {
        let _guard = lock_gen();
        assert!(init(0, crate::name_to_state));
        assert!(init_dim(DimId::Nether, 0, crate::name_to_state));
        assert!(init_dim(DimId::End, 0, crate::name_to_state));

        // Through the generator each dimension owns: its world window, its sky layer and the
        // preset its noise settings came from.
        let answers = |dimension: DimId| {
            with_gen_dim(dimension, |g| {
                (g.world_min_y, g.world_height, g.has_skylight(), g.preset)
            })
            .expect("generator installed for this dimension")
        };
        for (dimension, preset) in [
            (DimId::Nether, NoisePreset::Nether),
            (DimId::End, NoisePreset::End),
        ] {
            assert_eq!(
                answers(dimension),
                (0, 256, false, preset),
                "{dimension:?}: the world window comes from its own dimension_type"
            );
            // The lock-free answers a packet or a save reads, which is what a streaming caller
            // asks instead of taking the generator lock.
            let f = facts_dim(dimension).expect("facts published at init");
            assert_eq!(
                (f.world_min_y, f.world_height, f.preset),
                (0, 256, preset),
                "{dimension:?}: published facts"
            );
            assert_eq!(
                data_snapshot_dim(dimension)
                    .expect("snapshot published at init")
                    .world_height,
                256,
                "{dimension:?}: data snapshot"
            );
            assert!(!has_skylight(dimension), "{dimension:?}: no sky light layer");
        }

        // The Overworld is untouched by either install: still the active dimension, still its
        // own window, still the sky layer.
        assert_eq!(active_dim(), DimId::Overworld);
        assert_eq!(
            answers(DimId::Overworld),
            (-64, 384, true, NoisePreset::Overworld)
        );
        assert_eq!((world_min_y(), world_height()), (-64, 384));
        assert!(has_skylight(DimId::Overworld));

        // A dimension is named the way a datapack names it, and the two conversions agree.
        assert_eq!(DimId::Nether.dimension_id(), "minecraft:the_nether");
        assert_eq!(DimId::End.dimension_id(), "minecraft:the_end");
        for (slot, dimension) in DimId::ALL.into_iter().enumerate() {
            assert_eq!(dimension.index(), slot);
            assert_eq!(DimId::from_index(slot), Some(dimension));
            assert_eq!(
                DimId::parse_dimension_id(dimension.dimension_id()),
                Some(dimension)
            );
            assert_eq!(dimension.preset().dim(), dimension);
            assert_eq!(NoisePreset::from(dimension), dimension.preset());
            assert_eq!(
                dimension.dimension_file(),
                Some(dimension_type_file(dimension.preset()))
            );
        }

        // leave the shared fixture in the Overworld for the other tests
        assert!(init(0, crate::name_to_state));
    }

    /// The fallback that keeps every existing fixture's meaning: a pack without a usable
    /// `dimension_type` object leaves `world_* = gen_*`. "Unusable" is not only "absent":
    /// a window that is not whole sections, or that does not contain what the generator
    /// produces, would truncate terrain - refuse it and say so (the loader traces it),
    /// rather than defining a world smaller than the ground.
    #[test]
    fn dimension_window_falls_back_to_the_generator_when_the_object_is_unusable() {
        let parse = |s: &str| jparse(s.as_bytes()).expect("json");
        // absent fields (the file exists but is not a dimension_type)
        assert_eq!(dimension_window(&parse("{}"), 0, 128), None);
        // not whole sections
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": 0, "height": 120}"#), 0, 128),
            None
        );
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": 5, "height": 256}"#), 0, 128),
            None
        );
        // shorter than the generator's own terrain
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": 0, "height": 96}"#), 0, 128),
            None
        );
        // starts above the generator
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": 16, "height": 256}"#), 0, 128),
            None
        );
        // the three shapes 1.21.1 actually ships
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": 0, "height": 256}"#), 0, 128),
            Some((0, 256))
        );
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": -64, "height": 384}"#), -64, 384),
            Some((-64, 384))
        );
        // and an exact match is still a valid world window
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": 0, "height": 128}"#), 0, 128),
            Some((0, 128))
        );
    }

    /// The dump, not only the fetch, follows the **world** window.
    ///
    /// `main.rs` dumps `worldgen::world_min_y()..=world_max_y()` in 16-block sections, and
    /// `chunk_triage` is told the same numbers, so "the window the port can produce" and
    /// "the window the comparison reads" cannot disagree. Above the generator top the
    /// window must hold nothing but air: that is what makes the Nether's 256 a storage
    /// property and the End's a content property.
    #[test]
    fn nether_and_end_dumps_cover_the_world_window_with_air_above_the_generator() {
        let _guard = lock_gen();
        let reg = crate::block_registry::breg().expect("block registry");
        for preset in [NoisePreset::Nether, NoisePreset::End] {
            assert!(init_preset(4242, crate::name_to_state, preset));
            let f = facts().expect("facts");
            let (lo, hi) = (world_min_y(), world_max_y());
            assert_eq!(lo, f.world_min_y);
            assert_eq!(hi, f.world_min_y + f.world_height - 1);
            // The dump's own loop: one 4096-cell section per 16 blocks.
            let sections = ((hi - lo + 1) / 16) as usize;
            assert_eq!(sections, f.world_height as usize / 16, "{preset:?}");
            assert_eq!(sections, 16, "{preset:?}: 1.21.1 stores 16 sections");
            let mut cells = 0usize;
            let mut above_gen = 0usize;
            let mut sy = lo;
            while sy <= hi {
                let arr = terrain_section(0, sy, 0, crate::feature::populate)
                    .expect("terrain section");
                cells += arr.len();
                if sy >= f.min_y + f.height {
                    above_gen += arr
                        .iter()
                        .filter(|st| **st != 0 && !reg.is_air(**st))
                        .count();
                }
                sy += 16;
            }
            assert_eq!(
                cells,
                sections * 4096,
                "{preset:?}: the dump length must be the declared window"
            );
            assert_eq!(
                above_gen, 0,
                "{preset:?}: the generator window's ceiling is the top of the terrain, so \
                 128..255 must be air in chunk (0,0)"
            );
        }
        // leave the shared fixture in the Overworld for the other tests
        assert!(init(0, crate::name_to_state));
    }

    /// Stage 2A: terrain is pure, so a scratch generator on the same `GenData` must
    /// reproduce the shared generator's columns exactly, for every worker count.
    /// If this fails, the "no feature write is read by `cols`" premise is wrong and
    /// terrain cannot be parallelised without changing the parity contract.
    #[test]
    fn scratch_terrain_matches_the_shared_generator_at_any_worker_count() {
        let _guard = lock_gen();
        assert!(init(0, crate::name_to_state));
        let data = data_snapshot().expect("data published");
        let keys: Vec<(i32, i32)> = (-1..=2)
            .flat_map(|x| (-1..=2).map(move |z| (x * 3, z * 3)))
            .collect();
        // reference: the shared generator's own cache
        let reference: Vec<u64> = keys
            .iter()
            .map(|&(cx, cz)| {
                let cc = chunk_cols(cx, cz).expect("cols");
                with_gen(|g| cols_fingerprint(g, &cc)).expect("the generator for the fingerprint")
            })
            .collect();
        let mut fp_gen = Gen::fresh_scratch(data.clone());
        for workers in [1usize, 2, 4, 8] {
            let got = prefetch_cols(&data, &keys, workers);
            assert_eq!(got.len(), keys.len());
            for (i, (key, cols)) in got.iter().enumerate() {
                assert_eq!(key, &keys[i]);
                let cc = cols.as_ref().expect("prefetched cols");
                assert_eq!(
                    cols_fingerprint(&mut fp_gen, cc),
                    reference[i],
                    "worker count {workers} diverged at {key:?}"
                );
            }
        }
        // installing them must not change what the shared cache answers
        let mut fresh = Gen::fresh_scratch(data.clone());
        let installed = prefetch_cols(&data, &keys, 4)
            .into_iter()
            .filter_map(|(k, c)| c.map(|c| (k, c)))
            .collect();
        fresh.install_cols(installed);
        for (i, &(cx, cz)) in keys.iter().enumerate() {
            let cc = fresh.cols(cx, cz).expect("cached cols");
            assert_eq!(cols_fingerprint(&mut fresh, &cc), reference[i]);
        }
    }

    /// Lever 2 (`evidence/foundation/performance/README.md`): a worker's base rows must
    /// be the serial generator's bytes, for every worker count. If this fails, the
    /// "terrain is pure and per-chunk independent" premise is wrong and the parallel arm
    /// cannot ship. The column slice is checked too: a worker that fills a chunk with a
    /// *different* chunk's columns would still produce rows, but not these bytes.
    #[test]
    fn prefetched_terrain_rows_match_the_serial_fill_at_any_worker_count() {
        // A local generator, not `init`: these tests only need `GenData`, and re-initialising
        // the process generator here (or holding its test lock) makes an unrelated
        // wall-clock tick test flake when the suite runs with `--test-threads=8`.
        let local = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld)
            .expect("generator for the fixture seed");
        let data = std::sync::Arc::clone(&local.data);
        let keys: Vec<(i32, i32)> = vec![(-2, -2), (0, 0), (2, 0), (0, 2), (2, 2), (-2, 2)];
        let lo = min_y();
        let hi = min_y() + (max_y() + 1 - min_y()) / 16 * 16 - 16;
        // reference: one scratch generator, built cell by cell, no prefetch anywhere
        let mut reference = Gen::fresh_scratch(data.clone());
        let mut want_cols = Vec::new();
        let mut want_rows = Vec::new();
        for &(cx, cz) in &keys {
            let (cc, rows) = reference
                .build_terrain_rows(cx, cz, lo, hi, None)
                .expect("reference rows");
            want_cols.push(cols_fingerprint(&mut reference, &cc));
            want_rows.push(rows);
        }
        let mut fp_gen = Gen::fresh_scratch(data.clone());
        for jobs in [1usize, 2, 8] {
            let cols = prefetch_cols_with(&data, None, &keys, jobs, None);
            let got = prefetch_terrain(&data, None, &keys, &cols, lo, hi, jobs, None);
            assert_eq!(got.len(), keys.len());
            for (i, chunk) in got.iter().enumerate() {
                let chunk = chunk
                    .as_ref()
                    .unwrap_or_else(|| panic!("jobs {jobs}: no rows for {:?}", keys[i]));
                assert_eq!(chunk.key, keys[i]);
                assert_eq!(
                    cols_fingerprint(&mut fp_gen, &chunk.cols),
                    want_cols[i],
                    "jobs {jobs}: columns diverged at {:?}",
                    keys[i]
                );
                assert_eq!(chunk.rows.len(), want_rows[i].len());
                for (row, want) in chunk.rows.iter().zip(&want_rows[i]) {
                    assert_eq!(row.key, want.key, "jobs {jobs}: section key");
                    assert_eq!(
                        row.arr.as_slice(),
                        want.arr.as_slice(),
                        "jobs {jobs}: bytes diverged at {:?} section {}",
                        keys[i],
                        row.key.1
                    );
                    // The terrain stage's mark decisions are part of the worker's output:
                    // a serial and a parallel round have to file the same cells in the same
                    // order, or the promotion pass sees a different mark set per worker count.
                    assert_eq!(
                        row.marks, want.marks,
                        "jobs {jobs}: terrain marks diverged at {:?} section {}",
                        keys[i], row.key.1
                    );
                }
            }
        }
    }

    /// Lane BETA-SERVER-MT-SCOPE: the scoped batch rides the resident background
    /// pool under `mod-suite` and per-call OS threads otherwise; either executor
    /// must return the serial arm's columns and rows bit-for-bit.
    #[test]
    fn worldgen_pool_batch_matches_serial_bit_for_bit() {
        crate::test_support::require_vanilla_data();
        // A local generator, not `init`: the fixture needs only `GenData`, and a
        // process-wide re-init here would flake unrelated tests (see the
        // prefetched-terrain test above for the measured reasoning).
        let local = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld)
            .expect("generator for the fixture seed");
        let data = std::sync::Arc::clone(&local.data);
        let keys: Vec<(i32, i32)> = vec![(-2, -2), (0, 0), (2, 0), (0, 2), (2, 2), (-2, 2)];
        let lo = min_y();
        let hi = min_y() + (max_y() + 1 - min_y()) / 16 * 16 - 16;
        // Reference: one scratch generator filling rows inline, no batch anywhere.
        let mut reference = Gen::fresh_scratch(data.clone());
        let mut want_cols = Vec::new();
        let mut want_rows = Vec::new();
        for &(cx, cz) in &keys {
            let (cc, rows) = reference
                .build_terrain_rows(cx, cz, lo, hi, None)
                .expect("reference rows");
            want_cols.push(cols_fingerprint(&mut reference, &cc));
            want_rows.push(rows);
        }
        // `jobs = 4` keeps the batch above the serial fallback on both arms.
        #[cfg(feature = "mod-suite")]
        let executed_before = bg_task_pool().tasks_executed();
        let cols = prefetch_cols_with(&data, None, &keys, 4, None);
        let got = prefetch_terrain(&data, None, &keys, &cols, lo, hi, 4, None);
        // Identical bytes are produced by the serial fallback too, so the pool
        // arm must prove it actually ran rather than silently taking the other
        // executor (rules CV1).
        #[cfg(feature = "mod-suite")]
        assert!(
            bg_task_pool().tasks_executed() > executed_before,
            "the scoped batch ran on the resident pool"
        );
        let mut fp_gen = Gen::fresh_scratch(data.clone());
        assert_eq!(got.len(), keys.len());
        for (i, chunk) in got.iter().enumerate() {
            let chunk = chunk
                .as_ref()
                .unwrap_or_else(|| panic!("no rows for {:?}", keys[i]));
            assert_eq!(chunk.key, keys[i]);
            assert_eq!(
                cols_fingerprint(&mut fp_gen, &chunk.cols),
                want_cols[i],
                "columns diverged at {:?}",
                keys[i]
            );
            assert_eq!(chunk.rows.len(), want_rows[i].len());
            for (row, want) in chunk.rows.iter().zip(&want_rows[i]) {
                assert_eq!(row.key, want.key, "section key");
                assert_eq!(
                    row.arr.as_slice(),
                    want.arr.as_slice(),
                    "section bytes diverged at {:?} section {}",
                    keys[i],
                    row.key.1
                );
                assert_eq!(
                    row.marks, want.marks,
                    "terrain marks diverged at {:?} section {}",
                    keys[i], row.key.1
                );
            }
        }
    }

    /// The composed boundary: `terrain_section` returns the same bytes whether a worker
    /// prefetched the base rows or the section was built inline, **including** the serial
    /// feature layer on top. Two independent generators, same seed and data.
    ///
    /// It deliberately does **not** register the beardifier resolver: the
    /// `dimension_terrain` fixtures below are unbearded captures and pass only while no
    /// earlier test in the process has registered one (`overworld_chunk_matches_native_
    /// capture_state_for_state` registers it, and sorts after them). Registering it here
    /// made that fixture fail on the Nether seed `-1` row (`(-994, 89, 538)`: 19460 vs
    /// 5849) — a pre-existing order dependence in the fixture, not this lane's business to
    /// fix, but this test must not trip it. The bearded path is covered by the identity
    /// dumps, which run in a process whose entry point registers the resolver. (The P1b
    /// tests below reach the bearded path the same way without touching the process global:
    /// they pass `feature::beard_resolver` as a **per-generator** override.)
    #[test]
    fn a_prefetched_section_is_identical_to_the_serial_section_with_features() {
        let local = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld)
            .expect("generator for the fixture seed");
        let data = std::sync::Arc::clone(&local.data);
        let keys = [(0, 0), (1, 0)];
        let lo = min_y();
        let hi = min_y() + (max_y() + 1 - min_y()) / 16 * 16 - 16;
        let cols = prefetch_cols_with(&data, None, &keys, 2, None);
        let prefetched = prefetch_terrain(&data, None, &keys, &cols, lo, hi, 2, None);
        let mut serial = Gen::fresh_scratch(data.clone());
        let mut parallel = Gen::fresh_scratch(data.clone());
        parallel.install_prefetched(prefetched);
        for &(cx, cz) in &keys {
            let mut sy = lo;
            while sy <= hi {
                let a = serial.terrain_section_at(cx, sy, cz, crate::feature::populate);
                let b = parallel.terrain_section_at(cx, sy, cz, crate::feature::populate);
                assert_eq!(
                    a.as_slice(),
                    b.as_slice(),
                    "chunk ({cx},{cz}) section {sy}: the prefetched arm differs from the serial one"
                );
                sy += 16;
            }
        }
    }

    /// M3 Slice P1b (`specs/chunk-parallelism`): opening a round's shared terrain caches changes
    /// no byte.
    ///
    /// The arms are the configurations the dump runs: the serial reference (no round opened at
    /// all, which is what `MEGU_GEN_JOBS=1` is), then the same block at 1, 5 and 8 workers
    /// inside one round - so the share is entered on one thread, on five and on eight, and every
    /// arm must produce the same columns and the same section rows chunk for chunk. The Nether
    /// fixture is in it on purpose: seed `-1` is where a cache that leaks across contexts shows
    /// up (`(-994,89,538)`: 19460 cells against 5849), and the one rule this slice must not break
    /// is that a preliminary (`beard_depth > 0`) column never reaches `cols`.
    ///
    /// The reference arm passes the same **per-generator** beardifier resolver as the round arms
    /// rather than registering the process-wide one: registering it here would change what
    /// `dimension_terrain_matches_all_native_fixture_states` sees (see the note on
    /// `a_prefetched_section_is_identical_to_the_serial_section_with_features`).
    #[test]
    fn m3_p1b_64chunk_dump_is_bit_identical_across_worker_counts() {
        let resolver = crate::feature::beard_resolver;
        let lo = min_y();
        let hi = min_y() + (max_y() + 1 - min_y()) / 16 * 16 - 16;
        // The overworld arm is the dump's own block (`MEGU_WG_DUMP_N=8`, seed 0); the Nether
        // arm is a smaller block of the same fixture seed, because it is here for its structures
        // and not for its size.
        for (preset, seed, n) in [
            (NoisePreset::Overworld, 0u64, 8i32),
            (NoisePreset::Nether, u64::MAX, 4i32),
        ] {
            let local = Gen::load_preset(seed, crate::name_to_state, preset)
                .expect("generator for the fixture seed");
            let data = std::sync::Arc::clone(&local.data);
            let keys: Vec<(i32, i32)> = (0..n)
                .flat_map(|dz| (0..n).map(move |dx| (dx, dz)))
                .collect();
            // Reference: no round, so neither the round's start cache nor its share is entered.
            let cols = prefetch_cols_with(&data, Some(resolver), &keys, 1, None);
            let reference = prefetch_terrain(&data, Some(resolver), &keys, &cols, lo, hi, 1, None);
            let want: Vec<_> = reference
                .into_iter()
                .zip(&keys)
                .map(|(chunk, &key)| {
                    (key, chunk.unwrap_or_else(|| panic!("{preset:?}: no reference rows for {key:?}")))
                })
                .collect();
            let mut fp_gen = Gen::fresh_scratch(data.clone());
            for jobs in [1usize, 5, 8] {
                let round = crate::feature::starts_round_begin();
                let cols = prefetch_cols_with(&data, Some(resolver), &keys, jobs, Some(round));
                let got =
                    prefetch_terrain(&data, Some(resolver), &keys, &cols, lo, hi, jobs, Some(round));
                crate::feature::starts_round_end(round);
                assert_eq!(got.len(), want.len());
                for (chunk, (key, want_chunk)) in got.into_iter().zip(&want) {
                    let key = *key;
                    let chunk = chunk
                        .unwrap_or_else(|| panic!("{preset:?} jobs={jobs}: no rows for {key:?}"));
                    assert_eq!(chunk.key, key, "{preset:?} jobs={jobs}: chunk order");
                    assert_eq!(
                        cols_fingerprint(&mut fp_gen, &chunk.cols),
                        cols_fingerprint(&mut fp_gen, &want_chunk.cols),
                        "{preset:?} jobs={jobs}: columns diverged at {key:?}; the round's share \
                         must not change what a column contains"
                    );
                    assert_eq!(chunk.rows.len(), want_chunk.rows.len());
                    for (row, want_row) in chunk.rows.iter().zip(&want_chunk.rows) {
                        assert_eq!(
                            row.key, want_row.key,
                            "{preset:?} jobs={jobs}: section key at {key:?}"
                        );
                        assert_eq!(
                            row.arr.as_slice(),
                            want_row.arr.as_slice(),
                            "{preset:?} jobs={jobs}: bytes diverged at {key:?} section {}",
                            row.key.1
                        );
                        assert_eq!(
                            row.marks, want_row.marks,
                            "{preset:?} jobs={jobs}: terrain marks diverged at {key:?} section {}",
                            row.key.1
                        );
                    }
                }
            }
        }
    }

    /// M3 Slice P1b: a round's workers must not do the same admission scan, or build the same
    /// chunk's columns, twice.
    ///
    /// Two claims, each with its own probe so neither can pass vacuously:
    ///
    /// * the structure-start scans a round runs do not grow with the worker count. The 3x3 block
    ///   is chosen for the overlap - every chunk's 17x17 admission window covers its neighbours,
    ///   which is where P1a measured 281 scans serial against 454-462 and 376-571 at five and
    ///   eight workers. The counter is per round (`feature::round_start_scans`), not the process
    ///   aggregate, so a concurrently running test cannot supply or satisfy it.
    /// * two threads of one round asking for the same chunk's columns build them once
    ///   (`FlightCache::flight_counts`). Without single flight both threads build - that is the
    ///   `M1 cols hit owner / worker = 118,530 / 0` row of the P1a table.
    #[test]
    fn m3_p1b_single_flight_deduplicates_chunk_starts_and_cols() {
        let resolver = crate::feature::beard_resolver;
        let local = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld)
            .expect("generator for the fixture seed");
        let data = std::sync::Arc::clone(&local.data);
        // Centered on (44, -24): a seed-0 region whose windows admit four adapting chunks
        // (two of them - a trail_ruins and a trial_chambers - generate starts and so read
        // base columns), and every admitted chunk sits inside six to nine of the nine
        // 17x17 windows. The overlapping asks are what exercise the round's single flight;
        // the spawn-adjacent 3x3 admits only one chunk, which generates no start.
        let keys: Vec<(i32, i32)> = (43..=45)
            .flat_map(|x| (-25..=-23).map(move |z| (x, z)))
            .collect();
        let arm = |jobs: usize| -> (u64, (u64, u64), Vec<u64>) {
            let round = crate::feature::starts_round_begin();
            let _ = prefetch_cols_with(&data, Some(resolver), &keys, jobs, Some(round));
            let scans = crate::feature::round_start_scans(round).expect("the round's start cache");
            let flights =
                crate::feature::round_adapting_flight_counts(round).expect("adapting flights");
            let counts = terrain_share(round).expect("the round's terrain share");
            let counts = counts.flight_counts().into_iter().flat_map(|(a, b)| [a, b]).collect();
            crate::feature::starts_round_end(round);
            (scans, flights, counts)
        };
        let (serial, serial_flights, serial_counts) = arm(1);
        let (parallel, parallel_flights, parallel_counts) = arm(5);
        // `--nocapture` shows the measurement, not just the verdict: this is the P1a row that
        // grew (`chunk_starts` 281 -> 454-462) and the share's own work, per worker count.
        eprintln!(
            "[m3 p1b] starts_round scans jobs=1 {} jobs=5 {} | adapting (built,shared) jobs=1 \
             {serial_flights:?} jobs=5 {parallel_flights:?} | share (bases,beards,cols) as \
             (built,shared) jobs=1 {serial_counts:?} jobs=5 {parallel_counts:?}",
            serial, parallel
        );
        assert!(
            serial >= 1,
            "the fixture region admits adapting chunks: a zero here would make the equality \
             below vacuous"
        );
        let serial_asks = serial_flights.0 + serial_flights.1;
        assert!(
            serial_asks > keys.len() as u64,
            "overlapping 17x17 windows must ask the adapting-starts cache for more values than \
             the block holds: {serial_asks} asks for {} chunks",
            keys.len()
        );
        assert!(
            serial_flights.1 > 0,
            "no adapting-starts ask was served from a shared publication: the windows did not \
             overlap, so single flight is unproven"
        );
        assert!(
            serial_counts[2] > 0 && serial_counts[4] > 0,
            "the round's beards/cols share was never used; the counters below would say nothing"
        );
        assert_eq!(
            parallel, serial,
            "five workers ran {parallel} admission scans against {serial} serially: a starts key \
             was computed twice inside one round"
        );
        assert_eq!(
            parallel_flights, serial_flights,
            "five workers split the same adapting-starts asks into a different built/shared mix \
             than one: {parallel_flights:?} against {serial_flights:?}"
        );
        assert_eq!(
            parallel_counts, serial_counts,
            "the round's share did different work at five workers than at one \
             ((bases, beards, cols) as (built, shared) each): {parallel_counts:?} against \
             {serial_counts:?}"
        );

        // The same-key race, forced: two scratch generators of one round ask for one chunk's
        // final columns at the same moment. One leads the flight, the other takes the
        // publication, and both must get the same bytes.
        let round = crate::feature::starts_round_begin();
        let barrier = std::sync::Barrier::new(2);
        let fingers = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..2 {
                let fingers = &fingers;
                let barrier = &barrier;
                let data = std::sync::Arc::clone(&data);
                s.spawn(move || {
                    let mut g = Gen::fresh_scratch(data);
                    g.starts_round = Some(round);
                    g.set_beard_resolver(resolver);
                    barrier.wait();
                    let cc = g.cols(7, 7).expect("columns for the raced chunk");
                    fingers.lock().unwrap_or_else(|e| e.into_inner()).push(cols_fingerprint(&mut g, &cc));
                });
            }
        });
        let counts = terrain_share(round).expect("the round's terrain share");
        let (built, shared) = counts.flight_counts()[2];
        crate::feature::starts_round_end(round);
        let fingers = fingers.into_inner().unwrap();
        assert_eq!(fingers.len(), 2);
        assert_eq!(fingers[0], fingers[1], "the two racers got different columns");
        assert_eq!(
            built, 1,
            "two threads of one round built the same chunk's columns ({built} builds)"
        );
        assert_eq!(
            shared, 1,
            "the second asker took the flight's publication ({shared} shared values)"
        );
    }

    /// M3 Slice P1a (`specs/chunk-parallelism`): `MEGU_TERRAIN_STORE_BOUND` decides
    /// retention and nothing else. Three arms of the same round — a bound small enough to
    /// evict rows the consumer has not read yet, and the two A/B values — must return the
    /// same section bytes, and each arm's own counters must say whether it dropped a row
    /// early. Without the small arm this would pin an invariant nothing could violate.
    #[test]
    fn terrain_store_bound_changes_retention_not_bytes() {
        let _measurement = M3_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let local = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld)
            .expect("generator for the fixture seed");
        let data = std::sync::Arc::clone(&local.data);
        let keys = [(0, 0), (1, 0)];
        let lo = min_y();
        let hi = min_y() + (max_y() + 1 - min_y()) / 16 * 16 - 16;
        let cols = prefetch_cols_with(&data, None, &keys, 2, None);
        let prefetched = prefetch_terrain(&data, None, &keys, &cols, lo, hi, 2, None);
        // The serial reference: no store at all, every section filled inline.
        let mut reference = Gen::fresh_scratch(data.clone());
        let mut want: Vec<Box<[u32; 4096]>> = Vec::new();
        for &(cx, cz) in &keys {
            let mut sy = lo;
            while sy <= hi {
                want.push(reference.terrain_section_at(cx, sy, cz, |_, _, _| {}));
                sy += 16;
            }
        }
        // `sections_per_chunk` sections are installed per chunk, so a bound of 1 keeps one
        // row of the chunk being installed and evicts the rest before its consumer asks.
        for (bound, expect_early_evict) in [(1usize, true), (512, false), (1536, false)] {
            m3_test_force(true);
            m3_reset_local();
            let mut g = Gen::fresh_scratch(data.clone());
            g.install_prefetched_bounded(prefetched.clone(), bound);
            let resident = m3_local().get(M3_TERR_INSTALL);
            let evicted = m3_local().get(M3_TERR_EVICT);
            let early = m3_local().get(M3_EVICTED_UNCONSUMED);
            assert_eq!(resident, want.len() as u64, "bound {bound}: installs");
            assert_eq!(
                early > 0,
                expect_early_evict,
                "bound {bound}: evicted_unconsumed={early} (evicted={evicted}); the counter \
                 has to be able to see a row dropped before its consumer read it"
            );
            let mut i = 0;
            for &(cx, cz) in &keys {
                let mut sy = lo;
                while sy <= hi {
                    let got = g.terrain_section_at(cx, sy, cz, |_, _, _| {});
                    assert_eq!(
                        got.as_slice(),
                        want[i].as_slice(),
                        "bound {bound}: chunk ({cx},{cz}) section {sy} differs from the serial \
                         fill; retention must not change what a section contains"
                    );
                    i += 1;
                    sy += 16;
                }
            }
            m3_test_force(false);
        }
    }

    /// M3 Slice P1a: every counter is a count of deterministic work, so two runs of the same
    /// workload have to agree counter for counter. The workload is run on this thread and the
    /// meter read with [`m3_local`], so a concurrently running test cannot satisfy or break it;
    /// the worker arm sets the flag instead of spawning, which keeps the `*_worker` counters
    /// in this thread's meter.
    #[test]
    fn m3_counters_are_deterministic_across_runs() {
        let _measurement = M3_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let local = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld)
            .expect("generator for the fixture seed");
        let data = std::sync::Arc::clone(&local.data);
        let keys = [(0, 0), (1, 0)];
        let lo = min_y();
        let hi = min_y() + (max_y() + 1 - min_y()) / 16 * 16 - 16;
        let cols = prefetch_cols_with(&data, None, &keys, 2, None);
        let prefetched = prefetch_terrain(&data, None, &keys, &cols, lo, hi, 2, None);
        let run = |worker: bool| -> M3Meter {
            m3_test_force(true);
            m3_set_worker(worker);
            m3_reset_local();
            let mut g = Gen::fresh_scratch(data.clone());
            g.install_prefetched_bounded(prefetched.clone(), 16);
            for &(cx, cz) in &keys {
                let mut sy = lo;
                while sy <= hi {
                    let _ = g.prefetched_cell(cx * 16, sy, cz * 16);
                    let _ = g.terrain_section_at(cx, sy, cz, |_, _, _| {});
                    sy += 16;
                }
            }
            let meter = m3_local();
            m3_set_worker(false);
            m3_test_force(false);
            meter
        };
        for worker in [false, true] {
            let a = run(worker);
            let b = run(worker);
            let drift: Vec<String> = M3_NAMES
                .iter()
                .enumerate()
                .filter(|(i, _)| !M3_DURATION_COUNTERS.contains(i))
                .filter(|(i, _)| a.get(*i) != b.get(*i))
                .map(|(i, name)| format!("{name}: {} vs {}", a.get(i), b.get(i)))
                .collect();
            assert!(
                drift.is_empty(),
                "worker={worker}: the counters are not repeatable: {drift:?}"
            );
            assert!(
                a.get(M3_TERR_INSTALL) > 0 && a.get(M3_FILL_N) > 0,
                "worker={worker}: the measured workload did not run through the store"
            );
            if worker {
                assert_eq!(
                    a.get(M3_WORKER_SECTIONS),
                    a.get(M3_FILL_N),
                    "a marked worker thread's sections must be attributed to the worker arm"
                );
                assert_eq!(a.get(M3_OWNER_SECTIONS), 0);
            } else {
                assert_eq!(a.get(M3_OWNER_SECTIONS), a.get(M3_FILL_N));
                assert_eq!(a.get(M3_WORKER_SECTIONS), 0);
            }
        }
    }

    /// The dispatch is parallel, not merely thread-shaped: two chunk fills are inside the
    /// worker window at the same time. The probe releases only once two workers have
    /// arrived, so a serial dispatch makes this test red (after its timeout) instead of
    /// hanging. `keys.len()` is deliberately not divisible by `jobs`: a work partition
    /// that rounds down would silently leave the last chunks to the serial path.
    #[test]
    fn terrain_prefetch_fills_more_than_one_chunk_at_a_time() {
        let local = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld)
            .expect("generator for the fixture seed");
        let data = std::sync::Arc::clone(&local.data);
        let keys = [(0, 0), (2, 0), (4, 0), (6, 0), (8, 0)];
        let jobs = 3usize;
        let cols = prefetch_cols_with(&data, None, &keys, jobs, None);
        parallel_probe::arm(2);
        let got = prefetch_terrain(&data, None, &keys, &cols, min_y(), min_y() + 15, jobs, None);
        let (arrived, timed_out) = parallel_probe::disarm();
        let missing = got.iter().filter(|c| c.is_none()).count();
        assert!(missing == 0, "{missing} of {} keys were left to the serial path", keys.len());
        assert!(arrived >= 2, "only {arrived} worker(s) entered the fill window");
        assert!(
            !timed_out,
            "the first worker waited for a second and none came: the dispatch is serial"
        );
    }

    /// The default arm is parallel and bounded; the escape hatch is documented on
    /// `terrain_jobs`. This pins the ceiling, because a default that opened one thread per
    /// core would put one `Gen`'s caches of RSS per core on a shared host.
    #[test]
    fn the_default_terrain_worker_count_is_bounded() {
        const _: () = assert!(TERRAIN_MAX_JOBS <= 8);
        assert!(terrain_jobs() <= TERRAIN_MAX_JOBS.max(1));
    }

    /// Terrain throughput by worker count. `FEAT_PROFILE`-gated, and not a server
    /// measurement: it isolates `cols` (no populate, no delivery, no lock).
    #[test]
    fn terrain_parallel_throughput() {
        let _guard = lock_gen();
        if !env_flag!("FEAT_PROFILE") {
            return;
        }
        assert!(init(0, crate::name_to_state));
        let data = data_snapshot().expect("data published");
        let keys: Vec<(i32, i32)> = (0..6)
            .flat_map(|x| (0..6).map(move |z| (x * 5 - 10, z * 5 - 10)))
            .collect();
        for workers in [1usize, 2, 4, 8] {
            let t = std::time::Instant::now();
            let got = prefetch_cols(&data, &keys, workers);
            let ms = t.elapsed().as_secs_f64() * 1e3;
            assert!(got.iter().all(|(_, c)| c.is_some()));
            eprintln!(
                "TERRAIN workers={workers} chunks={} {ms:.1}ms ({:.2} ms/chunk)",
                keys.len(),
                ms / keys.len() as f64
            );
        }
    }

    /// The split's purpose: a worker's inputs are readable while the world lock is
    /// held by a long generation step. The snapshot lock is taken for an `Arc` clone,
    /// so the read must not wait for the 300 ms hold below.
    #[test]
    fn immutable_inputs_do_not_queue_on_the_world_lock() {
        let _guard = lock_gen();
        assert!(init(0, crate::name_to_state));
        let snap = data_snapshot().expect("data published at init");
        let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::scope(|s| {
            let held2 = held.clone();
            let released2 = released.clone();
            s.spawn(move || {
                // take the world lock and keep it, as a 13 ms-or-longer generate does
                let g = gen_lock();
                held2.store(true, std::sync::atomic::Ordering::SeqCst);
                while !released2.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                drop(g);
            });
            while !held.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let t = std::time::Instant::now();
            let again = data_snapshot().expect("snapshot");
            let sea = again.sea;
            let elapsed = t.elapsed();
            released.store(true, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(sea, snap.sea);
            assert!(
                elapsed < std::time::Duration::from_millis(50),
                "snapshot waited {elapsed:?} behind the world lock"
            );
        });
    }

    /// Lock-wait split under contention: three threads, each doing the work a
    /// server thread does on the shared world lock (column generation, populate,
    /// and a terrain/feature read). `GENLOCK` reports wait vs hold, so the
    /// parallelization decision can separate "serialized work" from "queueing".
    ///
    /// Not a throughput claim: one host, one process, work chosen to hit the lock
    /// rather than to match `load_compare.sh`'s movement column.
    #[test]
    fn gen_lock_contention() {
        let _guard = lock_gen();
        if !env_flag!("FEAT_PROFILE") {
            return;
        }
        assert!(init(0, crate::name_to_state));
        report_gen_lock(); // clear whatever the fixtures above accumulated
        let t0 = std::time::Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                for cx in 0..6 {
                    for cz in 0..6 {
                        let _ = chunk_cols(cx, cz);
                    }
                }
            });
            s.spawn(|| {
                for cx in -1..=2 {
                    for cz in -1..=2 {
                        let _ =
                            feature_state(cx * 16 + 8, 64, cz * 16 + 8, crate::feature::populate);
                    }
                }
            });
            s.spawn(|| {
                for i in 0..200 {
                    let _ = biome_name_at_block(i % 64, 64, (i / 64) % 64);
                }
            });
        });
        eprintln!("GENLOCK wall={:.1}ms", t0.elapsed().as_secs_f64() * 1e3);
        report_gen_lock();
    }

    #[test]
    fn feat_populate_perf() {
        let _guard = lock_gen();
        let seed = 0u64;
        init(seed, crate::name_to_state);
        let t = std::time::Instant::now();
        // one query triggers populate of the 3x3 neighborhood
        feature_state(8, 64, 8, crate::feature::populate);
        eprintln!("PERF populate9: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        for cx in -1..=1 {
            for cz in -1..=1 {
                feature_state(cx * 16 + 8, 64, cz * 16 + 8, crate::feature::populate);
            }
        }
        eprintln!("PERF +8 centers (cached): {:?}", t.elapsed());
        let mut n = 0;
        for cx in -1..=1 {
            for cz in -1..=1 {
                if let Some(m) = gen_lock().as_ref().unwrap().feat_map(cx, cz) {
                    n += m.len();
                }
            }
        }
        eprintln!("PERF overrides/chunk-9x9: {}", n);
        crate::feature::feat_prof_dump();
    }

    /// The default dispatch is a *named member* of vanilla's order family, not a capture: the
    /// level-major priority queue with the serialized demand scan tie-break documented on
    /// [`population_discipline`]. This pinned list is that member for a one-chunk request set, and
    /// it is the whole contract of the default — vanilla fixes the rule, not the sequence, so no
    /// captured run is reproduced bit-for-bit here (`MEGU_POP_ORDER` is how a comparison
    /// against one recorded order is made).
    ///
    /// Before this discipline the default was a lazy x-then-z 3x3, so this list is 9 entries long
    /// and in a different order; the golden is what turns that into a failure instead of a story.
    #[test]
    fn default_dispatch_runs_the_named_population_order() {
        let _guard = lock_gen();
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut visited = Vec::new();
        let _ = g.feat_state(8, 64, 8, |_, x, z| visited.push((x, z)));
        assert_eq!(
            render_population_order(&visited),
            squash(GOLDEN_DEFAULT_ORDER_1X1),
            "the default dispatch must run the named member, not a coordinate sort"
        );
        // A chunk the run already decorated is served from the cache: the discipline is a queue,
        // not a re-scan.
        let mut again = Vec::new();
        let _ = g.feat_state(8 + 16, 64, 8, |_, x, z| again.push((x, z)));
        assert_eq!(again, Vec::new(), "an already decorated chunk re-populated");
    }

    /// The same order for a 3x3 request box: 81 decorated chunks (9x9), and the tie-break that
    /// makes the member reproducible — request chunks are queued at level 31 in box order, so the
    /// min corner's window is the first batch.
    #[test]
    fn named_population_order_scales_to_a_three_by_three_request_box() {
        let _guard = lock_gen();
        let order = population_discipline_order(&request_box(0, 0, 2, 2));
        assert_eq!(
            render_population_order(&order),
            squash(GOLDEN_DEFAULT_ORDER_3X3),
            "the named member's 3x3 request box order"
        );
    }

    /// Invariance 1, measured on all seven native captures: the decorated set is the request set
    /// plus three rings (1 chunk -> 7x7, 3x3 -> 9x9). The rule comes from the status the decorating
    /// step answers to: `FEATURES` is level 34, so demanders sit at levels 31..=33.
    #[test]
    fn population_discipline_decorates_the_request_set_plus_three_rings() {
        let _guard = lock_gen();
        for (x0, z0, x1, z1, want) in [(0, 0, 0, 0, 49), (-3, 5, -3, 5, 49), (0, 0, 2, 2, 81)] {
            let requests = request_box(x0, z0, x1, z1);
            let order = population_discipline_order(&requests);
            assert_eq!(order.len(), want, "decorated count for ({x0},{z0})..({x1},{z1})");
            let (min_x, max_x) = (order.iter().map(|c| c.0).min(), order.iter().map(|c| c.0).max());
            let (min_z, max_z) = (order.iter().map(|c| c.1).min(), order.iter().map(|c| c.1).max());
            assert_eq!(
                (min_x, max_x, min_z, max_z),
                (Some(x0 - 3), Some(x1 + 3), Some(z0 - 3), Some(z1 + 3)),
                "decorated span for ({x0},{z0})..({x1},{z1}) must be the request box + 3 rings"
            );
            let unique: HashSet<_> = order.iter().copied().collect();
            assert_eq!(unique.len(), order.len(), "a chunk was decorated twice");
        }
    }

    /// Invariance 2, measured on all seven native captures: the first decoration block is the
    /// x-then-z 3x3 at the request box's (min x, min z) corner. That block is the first demander's
    /// write window, which is why it is the only part identical across runs.
    #[test]
    fn population_discipline_first_block_is_the_min_corner_window() {
        let _guard = lock_gen();
        for (x0, z0, x1, z1) in [(0, 0, 0, 0), (7, -4, 9, -2), (-25, 19, -25, 19)] {
            let requests = request_box(x0, z0, x1, z1);
            let batches = population_discipline(&requests);
            let first = &batches[0];
            assert_eq!(first.level, POP_REQUEST_LEVEL, "the first batch is a request chunk");
            assert_eq!(first.demander, (x0, z0), "the first demander is the min corner");
            let want: Vec<(i32, i32)> = (x0 - 1..=x0 + 1)
                .flat_map(|x| (z0 - 1..=z0 + 1).map(move |z| (x, z)))
                .collect();
            assert_eq!(first.decorated, want, "first block is the x-then-z 3x3 window");
            assert_eq!(
                population_discipline_order(&requests)[..9].to_vec(),
                want,
                "the first nine decorated chunks are that window"
            );
        }
    }

    /// Invariance 3: the pops are level-major (lowest non-empty level first) and every chunk's
    /// level is the request set's Chebyshev distance + 31. The *decorated* sequence is not
    /// level-monotone — a batch's window reaches one ring further — which is exactly the
    /// staircase native shows (`[1,0,1,2,3]` runs over levels {0..3} for one chunk).
    #[test]
    fn population_discipline_pops_in_level_order() {
        let _guard = lock_gen();
        for (x0, z0, x1, z1) in [(0, 0, 0, 0), (0, 0, 2, 2), (-2, 6, 1, 9)] {
            let requests = request_box(x0, z0, x1, z1);
            let batches = population_discipline(&requests);
            let levels: Vec<i32> = batches.iter().map(|batch| batch.level).collect();
            assert!(
                levels.windows(2).all(|w| w[0] <= w[1]),
                "pops must be level-major: {levels:?}"
            );
            assert_eq!(*levels.last().unwrap(), POP_DECORATION_LEVEL - 1);
            for batch in &batches {
                let ring = requests
                    .iter()
                    .map(|r| (batch.demander.0 - r.0).abs().max((batch.demander.1 - r.1).abs()))
                    .min()
                    .unwrap();
                assert_eq!(
                    batch.level,
                    POP_REQUEST_LEVEL + ring,
                    "{:?} must sit at 31 + its Chebyshev ring",
                    batch.demander
                );
                assert!(
                    batch.level < POP_DECORATION_LEVEL,
                    "a chunk whose own target is FEATURES never demands"
                );
                assert!(!batch.decorated.is_empty(), "a demander decorates its window");
            }
            // Level-major does not mean the decorated list is level-sorted: the window of a
            // level-31 pop already reaches ring 1.
            let decorated_levels: Vec<i32> = population_discipline_order(&requests)
                .into_iter()
                .map(|c| {
                    POP_REQUEST_LEVEL
                        + requests
                            .iter()
                            .map(|r| (c.0 - r.0).abs().max((c.1 - r.1).abs()))
                            .min()
                            .unwrap()
                })
                .collect();
            assert!(
                decorated_levels.windows(2).any(|w| w[0] > w[1]),
                "the decorated sequence is a staircase, not a sorted list"
            );
        }
    }

    /// The named member for a one-chunk request set, rendered as `x,z` in decoration order.
    const GOLDEN_DEFAULT_ORDER_1X1: &str = "
        -1,-1 -1,0 -1,1 0,-1 0,0 0,1 1,-1 1,0
        1,1 -2,-2 -2,-1 -2,0 -1,-2 0,-2 -2,1 -2,2
        -1,2 0,2 1,-2 1,2 2,-2 2,-1 2,0 2,1
        2,2 -3,-3 -3,-2 -3,-1 -2,-3 -1,-3 -3,0 -3,1
        0,-3 1,-3 -3,2 -3,3 -2,3 -1,3 0,3 1,3
        2,-3 2,3 3,-3 3,-2 3,-1 3,0 3,1 3,2
        3,3
    ";

    /// The same member for the 3x3 request box `(0,0)..(2,2)` (81 decorated chunks).
    const GOLDEN_DEFAULT_ORDER_3X3: &str = "
        -1,-1 -1,0 -1,1 0,-1 0,0 0,1 1,-1 1,0
        1,1 -1,2 0,2 1,2 -1,3 0,3 1,3 2,-1
        2,0 2,1 2,2 2,3 3,-1 3,0 3,1 3,2
        3,3 -2,-2 -2,-1 -2,0 -1,-2 0,-2 -2,1 -2,2
        1,-2 2,-2 -2,3 -2,4 -1,4 0,4 1,4 2,4
        3,-2 3,4 4,-2 4,-1 4,0 4,1 4,2 4,3
        4,4 -3,-3 -3,-2 -3,-1 -2,-3 -1,-3 -3,0 -3,1
        0,-3 1,-3 -3,2 -3,3 2,-3 3,-3 -3,4 -3,5
        -2,5 -1,5 0,5 1,5 2,5 3,5 4,-3 4,5
        5,-3 5,-2 5,-1 5,0 5,1 5,2 5,3 5,4
        5,5
    ";

    fn render_population_order(order: &[(i32, i32)]) -> String {
        order
            .iter()
            .map(|(x, z)| format!("{x},{z}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Compare a golden against a render without caring where the literal broke its lines.
    fn squash(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn default_dispatch_keeps_discipline_order_and_cached_overrides() {
        let _guard = lock_gen();
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut visited = Vec::new();
        let mut populate = |g: &mut Gen, sx: i32, sz: i32| {
            visited.push((sx, sz));
            // Cross-chunk writes make invocation order and repeated population
            // observable in returned values, not just in the call trace.
            g.feat_write(sx * 16 + 16, 64, sz * 16, 100 + visited.len() as u32);
        };
        assert_eq!(g.feat_state(-16, 64, -16, &mut populate), Some(102));
        assert_eq!(g.feat_state(-16, 64, -16, &mut populate), Some(102));
        assert_eq!(g.feat_state(0, 64, 0, &mut populate), Some(106));
        // Chunk (-1,-1)'s own 3x3 is the first pop's window, so the values are the ones the lazy
        // scan produced; the difference is the 40 chunks the discipline also decorates.
        assert_eq!(visited, population_discipline_order(&request_box(-1, -1, -1, -1)));
        assert_eq!(visited.len(), 49);
    }

    #[test]
    fn lazy_population_preserves_direct_feature_outputs() {
        let _guard = lock_gen();
        // Preserve population results through the default dispatch, including writes
        // outside the queried chunk. This is a refactor control, not a vanilla oracle.
        //
        // The compared quantity is the *queried* chunk's bucket. A chunk's cells are written
        // only by its own 3x3 (writes outside `POP_WRITE_RADIUS` are dropped), that 3x3 is the
        // first pop's window in the discipline, and no later pop decorates those chunks again —
        // so the equal-value claim is about that chunk, not about the 40 extra ring-2/ring-3
        // buckets the discipline also fills.
        for (x, z) in [(-1i32, -1i32), (16, 16)] {
            let mut lazy = Gen::load(0, crate::name_to_state).expect("datapack");
            let mut direct = Gen::load(0, crate::name_to_state).expect("datapack");
            let first = lazy.feat_state(x, 64, z, crate::feature::populate);
            let (cx, cz) = (x.div_euclid(16), z.div_euclid(16));
            for sx in cx - 1..=cx + 1 {
                for sz in cz - 1..=cz + 1 {
                    crate::feature::populate(&mut direct, sx, sz);
                }
            }
            let want = direct.feat_map(cx, cz).cloned();
            assert!(
                want.as_ref().is_some_and(|bucket| !bucket.is_empty()),
                "fixture must exercise real feature writes"
            );
            assert_eq!(
                lazy.feat_map(cx, cz).cloned(),
                want,
                "feature values differ at the queried chunk ({cx},{cz})"
            );
            assert_eq!(lazy.feat_state(x, 64, z, crate::feature::populate), first);
            assert_eq!(
                lazy.feat_map(cx, cz).cloned(),
                want,
                "cached lookup changed feature values"
            );
        }
    }

    /// The full, sorted feature population output for a fixed seed and query
    /// sequence, as raw bytes. The digest below was taken after switching the
    /// feature order to vanilla's decompiled `FeatureSorter` algorithm (DFS
    /// post-order reversed, cross-step edges); later shared provider/predicate/
    /// context edits must reproduce it exactly.
    fn feature_population_capture(g: &mut Gen) -> Vec<u8> {
        let mut bytes = b"GEN-OUTPUT-1\0".to_vec();
        for (x, y, z) in [(-1, 64, -1), (0, 64, 0), (-17, 64, -1), (0, 80, 0)] {
            let result = g.feat_state(x, y, z, crate::feature::populate);
            for n in [x, y, z] {
                bytes.extend_from_slice(&n.to_le_bytes());
            }
            bytes.push(u8::from(result.is_some()));
            bytes.extend_from_slice(&result.unwrap_or(0).to_le_bytes());
            let mut rows: Vec<_> = g
                .feat_c
                .iter()
                .flat_map(|(&(cx, cz), map)| {
                    map.iter()
                        .map(move |(&index, &state)| (cx, cz, index, state))
                })
                .collect();
            rows.sort_unstable();
            assert!(!rows.is_empty(), "the fixture must exercise feature writes");
            bytes.extend_from_slice(&(rows.len() as u64).to_le_bytes());
            for (cx, cz, index, state) in rows {
                bytes.extend_from_slice(&cx.to_le_bytes());
                bytes.extend_from_slice(&cz.to_le_bytes());
                bytes.extend_from_slice(&index.to_le_bytes());
                bytes.extend_from_slice(&state.to_le_bytes());
            }
            let mut done: Vec<_> = g.pop_done.iter().copied().collect();
            done.sort_unstable();
            bytes.extend_from_slice(&(done.len() as u64).to_le_bytes());
            for (cx, cz) in done {
                bytes.extend_from_slice(&cx.to_le_bytes());
                bytes.extend_from_slice(&cz.to_le_bytes());
            }
        }
        bytes
    }

    #[test]
    fn vertical_gradient_seed_reads_the_name_hash_big_endian() {
        let _guard = lock_gen();
        // vanilla StringHash.fromString = Guava Longs.fromBytes, i.e. big-endian,
        // then PositionalRandomFactory.fromHashOf XORs those words with the
        // world's forkPositional seed. Reading the md5 words little-endian put
        // deepslate/bedrock in the wrong cells (1221 vs 194 block diffs against
        // a vanilla server with features disabled).
        let g = Gen::load(0, crate::name_to_state).expect("datapack");
        let h = crate::generation_math::md5(b"minecraft:deepslate");
        let want_lo = u64::from_be_bytes(h[0..8].try_into().unwrap());
        let want_hi = u64::from_be_bytes(h[8..16].try_into().unwrap());
        let be_pair = g.conds.iter().any(|c| {
            if let Cond::VGrad { lo_w, hi_w, .. } = c {
                *lo_w == want_lo && *hi_w == want_hi
            } else {
                false
            }
        });
        assert!(
            be_pair,
            "vertical_gradient must read random_name's md5 big-endian"
        );
    }

    #[test]
    fn not_underwater_is_grass_only_above_the_water_surface() {
        let _guard = lock_gen();
        // vanilla's WaterCondition (SurfaceRules$WaterConditionSource$1WaterCondition)
        // is already "not underwater": waterHeight == MIN (no fluid), or
        // blockY + stoneDepthAbove >= the water surface. The grass rule uses it
        // directly. Negating it put grass on the submerged floors of the spawn
        // ocean and dirt on dry ground.
        assert!(init(0, crate::name_to_state));
        let grass = crate::name_to_state("minecraft:grass_block").unwrap();
        let dirt = crate::name_to_state("minecraft:dirt").unwrap();
        let wet = chunk_cols(0, 0).unwrap();
        assert_eq!(wet.surface(2, 0), 54, "top solid of the spawn ocean column");
        assert_eq!(wet.wtop(2, 0), 62, "its water surface");
        assert_eq!(
            with_gen(|g| wet.mat(g, 2, 0, 54)),
            Some(Some(dirt)),
            "a submerged floor is dirt"
        );
        let dry = chunk_cols(0, 2).unwrap();
        assert_eq!(dry.surface(4, 0), 63, "top solid of a dry column");
        assert_eq!(
            with_gen(|g| dry.mat(g, 4, 0, 63)),
            Some(Some(grass)),
            "a dry surface is grass"
        );
    }

    #[test]
    fn surface_density_follows_vanillas_interpolated_order() {
        let _guard = lock_gen();
        // vanilla samples `interpolated` over its input at 4x8x4 cell corners and
        // interpolates that, then applies squeeze/min/add per block. Interpolating
        // final_density as a whole is lerp(squeeze(X)); squeeze is cubic, so the
        // surface of this column sat one block low (y=62 instead of 63).
        assert!(init(0, crate::name_to_state));
        let cc = chunk_cols(0, 1).unwrap();
        assert_eq!(cc.surface(4, 0), 63, "surface of the check column");
    }

    #[test]
    fn beardifier_applies_before_surface_rules() {
        let _guard = lock_gen();
        // vanilla 1.21.1 orders `NoiseChunk.fillFromNoise` (density *with* Beardifier)
        // before `SurfaceSystem.buildSurface`, and `preliminarySurfaceLevel` samples the
        // BeardifierMarker (density *without* it). So `surf`/`wtop`/`mats` must see the
        // beardified density, while `prelim_min` must not.
        fn no_beard(_: &mut Gen, _: i32, _: i32) -> BeardChunkData {
            BeardChunkData::default()
        }
        // A `beard_thin` rigid at ground y=68 on column (4, 32) adds density below its
        // ground plane (`compute_beardifier`: `dy < 0` is positive) and carves above it,
        // so the unbeardified surface of 63 is pushed up towards 68 (measured: 67).
        fn raise_beard(g: &mut Gen, cx: i32, cz: i32) -> BeardChunkData {
            if (cx, cz) != (0, 2) {
                return BeardChunkData::default();
            }
            // A nested heightmap query runs while the resolution is in flight: it must
            // describe the un-beardified terrain and must not poison `col_c`.
            assert_eq!(
                g.top_oc(4, 32),
                64,
                "nested top_oc during resolve_beard must be un-beardified"
            );
            BeardChunkData {
                rigids: vec![BeardRigid {
                    min: [2, 68, 30],
                    max: [6, 72, 34],
                    adjustment: BeardAdjustment::BeardThin,
                    ground_level_delta: 0,
                }],
                junctions: Vec::new(),
            }
        }
        // Ground plane y=59: the 63 surface is carved down to 58, which is below the
        // aquifer level, so the carved cells 59..=62 flood (measured `wtop` = 62).
        // `wtop > surf` is what makes the surface rules see a *submerged* floor, giving
        // `dirt` rather than `grass_block`. The same beard evaluated after the surface
        // scan leaves `surf` at 63 and the assertion below fails (63 vs 63).
        fn carve_beard(g: &mut Gen, cx: i32, cz: i32) -> BeardChunkData {
            if (cx, cz) != (0, 2) {
                return BeardChunkData::default();
            }
            assert_eq!(
                g.top_oc(4, 32),
                64,
                "nested top_oc during resolve_beard must be un-beardified"
            );
            BeardChunkData {
                rigids: vec![BeardRigid {
                    min: [2, 59, 30],
                    max: [6, 65, 34],
                    adjustment: BeardAdjustment::BeardThin,
                    ground_level_delta: 0,
                }],
                junctions: Vec::new(),
            }
        }

        let grass = crate::name_to_state("minecraft:grass_block").unwrap();
        let dirt = crate::name_to_state("minecraft:dirt").unwrap();

        let mut g_plain = Gen::load(0, crate::name_to_state).unwrap();
        g_plain.set_beard_resolver(no_beard);
        let plain_cc = g_plain.cols(0, 2).unwrap();
        let plain_surf = plain_cc.surface(4, 0);
        assert_eq!(plain_surf, 63, "unbeardified surface of the check column");
        assert_eq!(plain_cc.wtop(4, 0), 63, "unbeardified column is dry");
        assert_eq!(plain_cc.mat(&mut g_plain, 4, 0, 63), Some(grass));
        let plain_prelim = g_plain.prelim_min(4, 32);

        // Positive beard: the surface rises above y=63, and the *material rules* have to
        // follow it. Resolving the beard last would leave `mats` describing y=63, so the
        // raised column would read as bare stone.
        let mut g_raise = Gen::load(0, crate::name_to_state).unwrap();
        g_raise.set_beard_resolver(raise_beard);
        let raise_cc = g_raise.cols(0, 2).unwrap();
        assert!(cols_have_beard(&raise_cc));
        let raised_surf = raise_cc.surface(4, 0);
        assert!(
            raised_surf > plain_surf,
            "positive beard should raise the surface ({raised_surf} vs {plain_surf})"
        );
        assert_eq!(raise_cc.mat(&mut g_raise, 4, 0, raised_surf), Some(grass));
        assert_eq!(
            raise_cc.wtop(4, 0),
            raised_surf,
            "the raised column stays above the aquifer: no water on top of it"
        );
        assert_eq!(g_raise.terrain_state(4, raised_surf, 32), grass);
        assert_eq!(g_raise.terrain_state(4, raised_surf - 1, 32), dirt);
        assert_eq!(g_raise.terrain_state(4, raised_surf + 1, 32), 0);
        // `preliminarySurfaceLevel` samples the BeardifierMarker: unaffected.
        assert_eq!(g_raise.prelim_min(4, 32), plain_prelim);

        // Negative beard: the surface is carved below the aquifer level, the carved
        // cells become water/air, and `wtop > surf` makes the exposed floor a submerged
        // floor, so the rule that applies is `dirt`, not `grass_block`.
        let mut g_carve = Gen::load(0, crate::name_to_state).unwrap();
        g_carve.set_beard_resolver(carve_beard);
        let carve_cc = g_carve.cols(0, 2).unwrap();
        assert!(cols_have_beard(&carve_cc));
        let carved_surf = carve_cc.surface(4, 0);
        assert!(
            carved_surf < plain_surf,
            "negative beard should carve the surface ({carved_surf} vs {plain_surf})"
        );
        assert!(
            carve_cc.wtop(4, 0) > carved_surf,
            "a carved-below-water column floods above its floor (wtop {} vs surf {carved_surf})",
            carve_cc.wtop(4, 0)
        );
        assert_eq!(g_carve.terrain_state(4, carved_surf, 32), dirt);
        for y in carved_surf + 1..=plain_surf {
            let st = g_carve.terrain_state(4, y, 32);
            let solid = st != 0 && st != g_carve.def_fl;
            assert!(
                !solid,
                "carved cell y={y} must not stay solid (state {st})"
            );
        }
        assert_eq!(g_carve.prelim_min(4, 32), plain_prelim);
    }

    #[test]
    fn ordered_population_claims_once_without_reordering_callbacks() {
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        let mut seen = Vec::new();
        let count = g.populate_in_order([(2, 1), (-3, 4), (2, 1)], |g, x, z| {
            assert!(g.pop_done.contains(&(x, z)), "claim precedes callback");
            seen.push((x, z));
        });
        assert_eq!(seen, [(2, 1), (-3, 4)]);
        assert_eq!(count, 2);
        assert_eq!(
            g.populate_in_order([(2, 1), (-3, 4)], |_, _, _| panic!("already populated")),
            0
        );
    }

    /// `ChunkMap`'s load path promotes a chunk only once the whole 3x3 around it is
    /// `ChunkStatus.FULL`, and `LevelChunk.postProcessGeneration` consumes the chunk's shape
    /// marks right there. So a mark waits for every neighbour, and the list is consumed once —
    /// a mark that arrives afterwards is never applied, which is the stale connection bit
    /// native keeps on a fence written into an already-promoted chunk. Outside a recorded
    /// plan there is no window to narrow the question, so every neighbour is required.
    #[test]
    fn shape_marks_wait_for_the_whole_three_by_three_and_are_consumed_once() {
        let mut g = Gen::load(4242, crate::name_to_state).unwrap();
        g.store_shape_mark(-1521, -21, 560);
        assert_eq!(g.take_shape_marks(-97, 35), Vec::new(), "filed under its own chunk");
        assert!(!g.promotion_window_ready(-96, 35), "nothing has generated yet");
        let mut order: Vec<(i32, i32)> = Vec::new();
        for dx in -1..=1 {
            for dz in -1..=1 {
                if (dx, dz) != (0, 0) {
                    order.push((-96 + dx, 35 + dz));
                }
            }
        }
        for (i, chunk) in order.iter().copied().enumerate() {
            assert_eq!(g.populate_in_order([chunk], |_, _, _| {}), 1);
            assert!(
                !g.promotion_window_ready(-96, 35),
                "still waiting for a chunk after {} of 8",
                i + 1
            );
        }
        assert_eq!(g.populate_in_order([(-96, 35)], |_, _, _| {}), 1);
        assert!(g.promotion_window_ready(-96, 35), "the whole 3x3 has generated");
        assert_eq!(g.take_shape_marks(-96, 35), vec![(-1521, -21, 560)]);
        assert!(g.take_shape_marks(-96, 35).is_empty(), "consumed once");
    }

    /// A recorded plan is a *window* of native's decorated region, not the whole of it: native
    /// decorated the request plus three rings while `parse_population_order` keeps the request
    /// plus one. A promotion window that leaves the plan therefore proves nothing about the
    /// neighbour - native ran that pass - while a window inside the plan still waits for
    /// `pop_done`. The demand region is the plan window plus one ring, `(n+4)²` for the plan's
    /// `(n+2)²` (measured: native's `minecraft:full` region is 7x7 for a 3x3 request while the
    /// recorded plan covers 5x5).
    #[test]
    fn a_recorded_plan_window_answers_for_the_neighbours_it_does_not_cover() {
        let mut g = Gen::load(4242, crate::name_to_state).unwrap();
        let order: Vec<(i32, i32)> = (-1..=1)
            .flat_map(|x| (-1..=1).map(move |z| (x, z)))
            .collect();
        let text: String = order.iter().map(|(x, z)| format!("{x}\t{z}\n")).collect();
        let plan = parse_population_order(&text, 0, 0, 1, false).unwrap();
        assert_eq!(plan.window, (-1, -1, 1, 1), "the (n+2)² halo, not the request");
        // The corner of the plan's own 3x3: three of its nine window cells lie outside.
        assert!(!g.promotion_window_ready(-1, -1), "its own chunk is not decorated");
        assert_eq!(g.populate_plan(plan, |_, _, _| {}), 9);
        assert!(
            g.promotion_window_ready(-1, -1),
            "the out-of-plan half of the window is native's, the in-plan half is ours"
        );
        // `(-2,-2)` is the corner of the demand region: its window leaves the plan on four
        // cells and stays inside it on five, so it is promoted too.
        assert!(g.promotion_region_contains(-2, -2));
        assert!(g.promotion_window_ready(-2, -2));
        // Two rings out is outside native's FULL region and is never a candidate.
        assert!(!g.promotion_region_contains(-3, 0));
        assert!(!g.promotion_region_contains(3, 3));
        // Without a plan there is no window: every neighbour is required again.
        let mut plain = Gen::load(4242, crate::name_to_state).unwrap();
        plain.populate_in_order([(7, 7)], |_, _, _| {});
        assert!(!plain.promotion_window_ready(7, 7));
        assert!(plain.promotion_region_contains(99, 99));
    }

    #[test]
    fn population_plan_preserves_order_and_rejects_incomplete_or_dropped_inputs() {
        let wanted: Vec<_> = (-1..=1)
            .rev()
            .flat_map(|x| (-1..=1).rev().map(move |z| (x, z)))
            .collect();
        let text: String = wanted.iter().map(|(x, z)| format!("{x}\t{z}\n")).collect();
        assert_eq!(
            parse_population_order(&text, 0, 0, 1, false).unwrap().order,
            wanted
        );
        for input in ["", "0 0\n0 0\n", "0 0\n1\n", "unknown 0\n", "9 9\n"] {
            assert!(
                parse_population_order(input, 0, 0, 1, false).is_err(),
                "must reject {input:?}"
            );
        }
        assert!(parse_population_order(&text, 0, 0, 0, false).is_err());
        assert!(parse_population_order(&text, i32::MIN, 0, 1, false).is_err());
    }

    #[test]
    fn recorded_population_plan_preserves_extras_without_hiding_missing_sources() {
        let core: Vec<_> = (-1..=1)
            .flat_map(|x| (-1..=1).map(move |z| (x, z)))
            .collect();
        let mut wanted = vec![(9, 9)];
        wanted.extend_from_slice(&core);
        wanted.push((-20, 3));
        let text: String = wanted.iter().map(|(x, z)| format!("{x}\t{z}\n")).collect();
        assert_eq!(
            parse_population_order(&text, 0, 0, 1, true).unwrap().order,
            wanted
        );
        assert!(parse_population_order(&text, 0, 0, 1, false).is_err());
        // Nine entries is not nine required sources: an extra cannot replace a core cell.
        let missing: String = wanted[..core.len()]
            .iter()
            .map(|(x, z)| format!("{x} {z}\n"))
            .collect();
        assert!(parse_population_order(&missing, 0, 0, 1, true).is_err());
        assert!(parse_population_order(&(text.clone() + "9 9\n"), 0, 0, 1, true).is_err());
        assert!(parse_population_order(&(text + "bad extra\n"), 0, 0, 1, true).is_err());
    }

    #[test]
    fn feature_population_output_matches_the_pre_refactor_capture() {
        let _guard = lock_gen();
        // Unlike the lazy-vs-direct comparison, this pins the values against a
        // digest recorded before the shared contracts moved, so a change made on
        // both sides at once cannot hide behind a passing local comparison.
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let bytes = feature_population_capture(&mut g);
        let digest: String = crate::generation_math::md5(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        // 2026-10-20: the surface materials changed because the
        // vertical_gradient name hash is now read big-endian like vanilla's
        // Guava Longs.fromBytes (see vertical_gradient_seed_reads_the_name_hash_big_endian).
        // Vanilla block diff with features and carvers disabled on both sides
        // went 1221 -> 194 cells at seed 0 chunk (0,0); the deepslate and
        // bedrock bands match, so this capture records the corrected input.
        // 2026-10-20: the terrain now matches vanilla exactly, and the biome
        // feature lists were replaced with the 1.21.1 server jar's (the bundled
        // data had 1.21.5-ish lists: 48/64 biomes differed). With features and
        // carvers disabled the block diff is 0 in (0,0), (0,1), (-1,-1), (-2,0);
        // with features on it is 10939 -> 10874 after the list swap.
        // 2026-10-20: placement modifiers now run as vanilla's stream
        // (`flatMap` per position, feature placed before the next position is
        // pulled) instead of one modifier over all positions. Same seed, same
        // step index, different draw order: `ore_andesite_lower` in chunk (0,0)
        // used to place at y=15 and y=18, vanilla's order gives y=49 and y=18.
        // Full-chunk block diff for (0,0) against the replayed vanilla world:
        // 11017 -> 10302 cells (11.21% -> 10.48%).
        // 2026-10-21: `Lcg::next_i64` now sign-extends its low word like Java's
        // `nextLong` (see `world_rng`'s pinned vector), which is what every
        // population / large-feature seed is built from. Half of all draws were
        // 2^32 off. Measured cost in this layer: the 2x2-chunk feature-on diff
        // against a fresh 1.21.1 world went 44187 -> 45226 cells, so the feature
        // layer still has a compensating defect (R1a); the carver seeds need the
        // correct value, and two wrong seeds are not a calibration to preserve.
        // 2026-10-22: `minecraft:uniform` height providers draw once (`eia` ->
        // `Mth.randomBetweenInclusive`), not twice like the trapezoid `ehv` the
        // port had copied. Every cave tunnel start was shifted by one draw.
        // Cave-only and all-carver 2x2 fixtures at (0,0) went 617 / 617+341 ->
        // 0 differing cells against vanilla, so this capture records the
        // corrected (vanilla-matching) carving output.
        // 2026-10-22: feature placement now draws from a `WorldgenRandom`, which
        // wraps the Xoroshiro source but implements `dyn`'s *legacy* draws
        // (`nextInt(bound)` = multiply-shift on `next(31)`, `nextInt()` = the top
        // word, `nextDouble()` = two draws). The modern `RandomGenerator` forms
        // put every ore blob somewhere else: 242/242 origins differed. With the
        // legacy flavour the first origins match vanilla exactly and the 2x2
        // feature-on diff at (0,0) drops 40975 -> 34679.
        // 2026-10-22: the feature height providers now match vanilla's classes
        // (uniform `eia` one draw, trapezoid `ehz` plateau 0, biased `ehv`,
        // very-biased `eib`) and `below_top` resolves to minY + height - 1 - value.
        // With those fixed every one of the 22 ore placed features in chunk (0,0)
        // has an identical origin list to the live server, and the 2x2 feature-on
        // diff drops 33853 cells.
        // 2026-10-22: the decoration seed is now computed the way vanilla does it
        // (`WorldgenRandom.setDecorationSeed` on the Xoroshiro source, with XOR
        // instead of the LCG and addition the port had). Only chunk (0,0) was
        // unaffected, because its seed is `0 ^ 0 ^ seed`. With the fix the ore
        // origins of neighbour chunks match too and the 2x2 feature-on diff
        // drops 33853 -> 15820.
        // 2026-10-22: `underwater_magma` now runs `UnderwaterMagmaFeature`'s
        // Column.scan (the origin must be water) and enclosure test, so a land
        // chunk no longer receives magma at all. 2x2 feature-on diff: 3472 ->
        // 2188 cells.
        // 2026-10-22: the placed-feature definitions are now the 1.21.1 jar's
        // (the bundled set was 1.21.2+ and, for example, referenced
        // `trees_birch_and_oak_leaf_litter` while the 1.21.1 biomes reference
        // `trees_birch_and_oak`, so every forest tree was silently skipped).
        // 2x2 feature-on diff: 2188 -> 2062.
        // 2026-10-22: the feature-order tie-break list is now the biome list
        // `FeatureSorter.buildFeaturesPerStep` actually receives (53 overworld
        // biomes, measured from a live server) instead of a 64-entry registry
        // order that also contained the nether and the end. That shifted the
        // per-feature index - and therefore the RNG offset - of every step whose
        // biome lists differ, so trees and vegetation landed in the wrong places
        // while the ore step (identical lists in every biome) was unaffected.
        // 2026-10-22: the disk features now place. Their 1.21.1 state provider has
        // no "type" field and their fluid predicate names a single fluid, so both
        // parsed to nothing and the feature silently did no work.
        // 2x2 feature-on diff: 2510 -> 1856 (chunk (0,0): 540 -> 140).
        // 2026-10-22: a nested *placed* feature reference now runs the referenced
        // feature's own placement modifiers (the configured-feature contract holds
        // Holder<PlacedFeature>). Dropping them hid
        // block_predicate_filter(would_survive) on the random_selector children, so
        // every tree candidate grew a tree. 2x2 feature-on diff: 1856 -> 1035.
        // 2026-10-22: the biome the placement chain reads is now
        // `BiomeManager.getBiome` (the blended lookup that `LevelReader.getBiome`
        // returns) instead of the chunk's quart container. BiomeFilter uses it, so
        // the container path accepted candidates vanilla rejects at a biome
        // boundary. 2x2 feature-on diff: 1035 -> 940; trees 45 -> 42 (vanilla 38).
        // 2026-10-22: the feature-layer heightmap is now owned by `Gen` and updated
        // on every write, so a tree placed for one chunk raises the height a
        // neighbouring chunk's candidates read (vanilla updates the *owning* chunk's
        // heightmap on setBlock, and OCEAN_FLOOR counts canopy leaves). Keeping the
        // tally inside one placement pass made candidate (0,17) read y=64 instead of
        // vanilla's 70 and grow a tree vanilla rejects on the leaf above it.
        // 2x2 feature-on diff: 940 -> 704; trees 42 -> 39 (vanilla 38).
        // 2026-10-23: `random_patch` is implemented and `matching_blocks` accepts a
        // scalar `blocks`, so the vegetation that was inert now places. The capture
        // moves with it; the fixture to read next is `tools/ore_fixture.sh`
        // (938 -> 42939 cells over 8 chunks, short_grass 0 -> 362 against vanilla's 102,
        // i.e. an over-placement to explain).
        // 2026-10-23: `SimpleBlockFeature`'s `canSurvive` gate is applied, so a patch no
        // longer places its block on any air cell in the box (short_grass 362 -> 44
        // against vanilla's 102, with the draw streams identical - the next item is the
        // attempt/acceptance count, not the placement).
        // 2026-10-23: `seagrass` is implemented (`edk` + `SeagrassBlock.mayPlaceOn`), which
        // places 27 cells against vanilla's 28 in the isolated fixture - the capture
        // moves with it, and that feature's residue is now the size of the reference's
        // own noise floor.
        // 2026-10-23: `minecraft:flower` (the same patch config shape as random_patch, a
        // different type name) is implemented, so the flower patches in the pack place.
        // 2026-10-26: features now read carver writes from the feature layer.
        // Without that, an ore feature saw uncarved stone and placed lapis into
        // reference cave air; the isolated lapis fixture moved 28/23 -> 23/23.
        // The capture moves with the corrected stage order; the remaining forest
        // residual is still features/structures, not terrain or carvers.
        // 2026-10-26: `TreeFeature` now rejects a clipped tree without a
        // `min_clipped_height`, as the pinned class does. The one-feature forest
        // fixture moved from 47 port origins (26 shared) to 41/41 all shared, and
        // its feature draw stream matches the reference after the port's lazy
        // aquifer samples. The capture moves with that native stage order.
        // 2026-10-26: the tree parser reads `dirt_provider` (the pinned
        // TreeConfiguration field), so trunk placers set the base dirt; the
        // target chunk's last 5 cells (grass vs dirt) became 0.
        // 2026-10-26: fancy trunk attachments start at `start + (height - 3)`,
        // not `start + k`; the latter moved every fancy canopy up. The 3x3 diff
        // moved 728 -> 319 while the target chunk stayed at 0.
        // 2026-10-26: the centre fancy limb is `h2 * 0.5` (vanilla assigns h2
        // then returns half); the port returned h2, doubling the centre limb.
        // The 3x3 diff moved 72 -> 51; the remainder is leaf distance.
        // 2026-10-26: carvers for the 3x3 decoration area are claimed before
        // features, so a feature can read a neighbour's carver air. This is
        // vanilla's CARVERS -> FEATURES order; the dungeon that required a
        // hole in chunk z+1 now places, and the full-feature target moved
        // 382 -> 114.
        // OreFeature's discard=1 still places buried ore (no RNG draw before
        // checking neighbours). Native decision/RNG vectors pin the correction;
        // forest target no-tick differences fall 53 -> 30, all 23 changed cells
        // becoming matching diamond/lapis ore. See forest-r1/ore-air.md.
        // Frozen WG heights, normal noise, wrapped dungeon RNG, fluid predicates
        // and multiface spreading have independent native fixtures.
        // 2026-10-27: leaf distances now follow actual TreeFeature buckets,
        // including Java HashSet traversal, rather than global shortest paths.
        // 117 native leaf cases and 15 actual HashSet operation cases pin that
        // behavior. With explicit source order, the production dump matches all
        // 884736 cells of the paused native 3x3, without property exclusions.
        // Saved-world runtime differences remain separate: feature-contracts.md.
        // 2026-11-15: the WG heightmaps are now frozen at the end of the carver pass
        // instead of at the surface. `ProtoChunk.setBlockState` updates
        // `getStatus().heightmapsAfter()`, and the status is still SURFACE while the
        // carver runs (the new status is set when the stage body returns), so a carve
        // lowers WORLD_SURFACE_WG/OCEAN_FLOOR_WG and the FEATURES status afterwards
        // leaves them alone. Features and structures read that map
        // (`HeightmapFilter`/`world_surface_wg` placements, `WorldGenRegion.getHeight`),
        // so they now see the carved top. Seed 4242 chunk (6,7) against the native
        // chunk: `minecraft:ruined_portal`'s netherrack base read 110 at (107,122)
        // instead of vanilla's 100, which shifted the whole weighted base/column RNG
        // stream - 124 netherrack/dirt/stone cells of the portal differed (chunk total
        // 278 -> 154, the remainder the amethyst geode). The capture moves with the
        // corrected input.
        // 2026-11-16: a tree's trunk dirt now joins the tree's *log* list, and the three
        // decorator lists are handed over in `TreeDecorator.Context`'s order (HashSet
        // bucket order, then a stable sort by y). `TrunkPlacer.setDirtUnderneath` writes
        // through the same `BiConsumer` as `placeLog` (`ehm.a`), so vanilla's `logs` set
        // holds the dirt position too; `trunk_vine` draws four `nextInt(3)` per entry, so
        // the missing entry shortened the stream and shifted every later decorator. The
        // list order matters because `leave_vine` draws four `nextFloat` per leaf.
        // Measured on the pinned jungle fixture (`/tmp/nat-4242-jungle-temple/world`,
        // chunk (74,-41) against the recorded population order): 732 -> 489 differing
        // cells with the dirt entry, 489 -> 145 with the list order, and the live
        // OCEAN_FLOOR / WORLD_SURFACE columns against the saved chunk 77 -> 14 and
        // 89 -> 18. The visible blocks of this capture's own fixture are unchanged
        // ((-2,-2)+3x3 564 -> 564, (0,0)+2x2 11 -> 11, (-1,1)+2x2 0 -> 0), so the digest
        // moves with the captured *write set* in the chunks those windows do not cover.
        // 2026-11-18: the default population dispatch is now vanilla's order *discipline* — the
        // level-major priority queue with the serialized demand scan tie-break
        // (`population_discipline`) — instead of a lazy x-then-z 3x3. This capture digests every
        // `feat_c` bucket and the whole `pop_done` set, and the discipline decorates the request
        // set plus three rings (49 chunks for a one-chunk request instead of 9), so the digest
        // moves with the write set of the two extra rings. The *visible* blocks are unchanged:
        // a chunk's cells are written only by its own 3x3 (writes beyond `POP_WRITE_RADIUS` are
        // dropped), that square is the first pop's window in x-then-z — the same nine chunks the
        // lazy scan ran, in the same order — and no later pop decorates them again
        // (`lazy_population_preserves_direct_feature_outputs` and
        // `default_dispatch_keeps_discipline_order_and_cached_overrides` pin both halves).
        // 2026-11-18: `StructurePiece.placeBlock` (`SHAPE_CHECK_BLOCKS`, P2-1-T0),
        // `Feature.markAboveForPostProcessing` (`DiskFeature` and `LakeFeature`'s two sites),
        // and `LevelChunk.postProcessGeneration`'s `BushBlock`/`DoublePlantBlock` removal now
        // run during chunk promotion, moving promoted cells in the negative chunks of this
        // capture's `feat_c` map (`(-2..0, -2..0)` outer rings). The seed-0 `(0,0)` native
        // chunk fixture (`overworld_chunk_matches_native_capture_state_for_state`) and the
        // 64-chunk `N=8` dump (`670fdfe3...`) are unchanged.
        assert_eq!(
            digest, "bed420818fc3a0de1d2d26ebc227a4f0",
            "feature population output changed from the native feature-contract capture"
        );
    }

    #[test]
    fn bundled_carver_configs_are_the_1_21_1_shapes() {
        let _guard = lock_gen();
        // The four configured carvers the overworld's biomes reference. They were missing
        // entirely, so no cave could ever be carved; this keeps them present and shaped like
        // the 1.21.1 jar's files now that the carving code is the next piece.
        for (name, ty) in [
            ("cave", "minecraft:cave"),
            ("cave_extra_underground", "minecraft:cave"),
            ("canyon", "minecraft:canyon"),
            ("nether_cave", "minecraft:nether_cave"),
        ] {
            let raw = std::fs::read(format!(
                "datapacks/vanilla/data/minecraft/worldgen/configured_carver/{name}.json"
            ))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            let j = crate::json::jparse(&raw).unwrap_or_else(|| panic!("{name}: not JSON"));
            assert_eq!(
                crate::json::jstr(&j, "type").as_deref(),
                Some(ty),
                "{name}: type"
            );
            let cfg = crate::json::js(&j, "config").unwrap_or_else(|| panic!("{name}: no config"));
            for key in ["probability", "y", "yScale", "lava_level", "replaceable"] {
                assert!(
                    crate::json::js(cfg, key).is_some(),
                    "{name}: config lacks {key}"
                );
            }
        }
    }

    #[test]
    fn the_ore_vein_geometry_matches_vanillas_own_output() {
        let _guard = lock_gen();
        // Vanilla's OreFeature.place (called through tools/vanilla-probe) writes these 666
        // cells for seed 60007, position (7,49,13) and size 64. Pinning them means the Rust
        // generator is compared against the game's own output, not only against the Python
        // port in tools/ore_oracle.py.
        let text = include_str!("../evidence/gameplay/feature-parity/ore_andesite_666.txt");
        let mut want: Vec<(i32, i32, i32)> = text
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((
                    it.next()?.parse().ok()?,
                    it.next()?.parse().ok()?,
                    it.next()?.parse().ok()?,
                ))
            })
            .collect();
        want.sort();
        let got = crate::feature::ore_cells_for_test(60007, (7, 49, 13), 64);
        assert_eq!(
            got.len(),
            want.len(),
            "cell count differs from vanilla's 666"
        );
        assert_eq!(got, want, "vein geometry differs from vanilla's own output");
    }

    #[test]
    fn surface_biomes_follow_fuzzy_palettes_and_preserve_climate_ties() {
        let mut g = Gen::load(4242, crate::name_to_state).expect("datapack");
        let mut c = SC::new();
        // Native SurfaceRules.Context samples two biomes inside one raw quart.
        // The sand floor at each position uses the chosen biome's material rule.
        for (pos, expected) in [
            ((2016, 45, -2901), "minecraft:beach"),
            ((2016, 46, -2901), "minecraft:lukewarm_ocean"),
            // River and beach have equal climate distance. The preceding beach
            // leaf's own bounds let it retain the tie.
            ((2029, 51, -2912), "minecraft:beach"),
        ] {
            (c.x, c.y, c.z) = pos;
            for _ in 0..2 {
                let leaf = g.leaf_at(&mut c);
                assert_eq!(g.biome_names[leaf as usize], expected, "{pos:?}");
            }
        }
    }

    #[test]
    fn feat_determinism() {
        let _guard = lock_gen();
        // same seed -> identical override maps; and feature writes must cross
        // chunk boundaries (e.g. a tree rooted in a neighbor must appear here)
        let g1 = Gen::load(0, crate::name_to_state).expect("datapack");
        let g2 = Gen::load(0, crate::name_to_state).expect("datapack");
        let (mut g1, mut g2) = (g1, g2);
        crate::feature::populate(&mut g1, 0, 0);
        crate::feature::populate(&mut g2, 0, 0);
        let m1 = g1.feat_map(0, 0).cloned().unwrap_or_default();
        let m2 = g2.feat_map(0, 0).cloned().unwrap_or_default();
        if m1 != m2 {
            let mut diffs = 0;
            for (k, v) in &m1 {
                match m2.get(k) {
                    Some(v2) if v2 == v => {}
                    other => {
                        if diffs < 20 {
                            eprintln!(
                                "  diff key={} (y={} x={} z={}) {} vs {:?}",
                                k,
                                (k >> 8) as i32 - 64,
                                k & 15,
                                (k >> 4) & 15,
                                v,
                                other
                            );
                        }
                        diffs += 1;
                    }
                }
            }
            for (k, v) in &m2 {
                if !m1.contains_key(k) {
                    if diffs < 40 {
                        eprintln!("  m2-only key={} {}", k, v);
                    }
                    diffs += 1;
                }
            }
            panic!("populate is not deterministic: {} diffs", diffs);
        }
        // cross-chunk: collect writes from chunk (0,0) into neighbor maps
        let neigh: usize = [-1i32, 1]
            .iter()
            .flat_map(|&dx| [-1i32, 1].iter().map(move |&dz| (dx, dz)))
            .map(|(cx, cz)| g1.feat_map(cx, cz).map(|m| m.len()).unwrap_or(0))
            .sum();
        eprintln!(
            "feat_determinism: own={} neighbor-overrides={}",
            m1.len(),
            neigh
        );
        assert!(m1.len() + neigh > 0, "no features generated at all");
    }

    // ---- carve-stage lifecycle -----------------------------------------------------

    /// Everything the block layer holds for one chunk, in a stable order, so two runs of
    /// the same stage on the same inputs compare as data.
    fn chunk_override_digest(g: &Gen, cx: i32, cz: i32) -> (usize, u64) {
        let mut cells: Vec<(i32, i32, i32, u32)> = Vec::new();
        for lx in 0..16 {
            for lz in 0..16 {
                for y in g.min_y..g.min_y + g.height {
                    let (x, z) = (cx * 16 + lx, cz * 16 + lz);
                    if let Some(st) = g.feature_override(x, y, z) {
                        cells.push((x, y, z, st));
                    }
                }
            }
        }
        cells.sort();
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for (x, y, z, st) in &cells {
            for v in [*x as u64, *y as u64, *z as u64, *st as u64] {
                h ^= v;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        }
        (cells.len(), h)
    }

    /// Turn `feat_c`'s own bound far enough that an old bucket is dropped: `feat_write`
    /// is the entry both the carver and the feature layer flush through, and `feat_state`
    /// is where that bound runs. The driver is a no-op so this stays a cache test.
    fn evict_feature_buckets(g: &mut Gen, n: i32) {
        for i in 0..n {
            g.feat_write(1_000_000 + i * 16, 60, 1_000_000, 1);
        }
        let _ = g.feat_state(1_000_000, 60, 1_000_000, |_, _, _| {});
    }

    /// The carver's writes and the latch that says "this chunk is carved" are one stage.
    /// When the override cache drops the writes, the latch has to go with them, because
    /// the only other way to get them back is the pass the latch suppresses.
    #[test]
    fn carve_output_and_its_latch_share_one_lifetime() {
        let mut g = Gen::load(4242, crate::name_to_state).expect("datapack");
        // The real FEATURES entry point on the real schedule: chunk (6,7) alone. Its 3x3
        // is carved but not populated, exactly as `feature::populate` does it.
        assert_eq!(
            g.populate_in_order([(6, 7)], crate::feature::populate),
            1,
            "the first pass must run"
        );
        let first = chunk_override_digest(&g, 6, 7);
        let first_wg = g.wg_after_carve_height(false, 6 * 16, 7 * 16);
        assert!(first.0 > 0, "the first pass wrote no block output");
        evict_feature_buckets(&mut g, 400);
        assert_eq!(
            chunk_override_digest(&g, 6, 7),
            (0, 0xcbf2_9ce4_8422_2325),
            "the 384 bound really dropped this chunk's block output"
        );
        // Same entry point, same schedule, same inputs: the stage reproduces itself.
        let reran = g.populate_in_order([(6, 7)], crate::feature::populate);
        let second = chunk_override_digest(&g, 6, 7);
        let second_wg = g.wg_after_carve_height(false, 6 * 16, 7 * 16);
        eprintln!(
            "FACTS carve: first=(n={} h={:016x}) reran={reran} second=(n={} h={:016x}) \
             wg={first_wg:?}->{second_wg:?}",
            first.0, first.1, second.0, second.1
        );
        assert_eq!(reran, 1, "the chunk re-claimed its FEATURES pass");
        assert_eq!(
            second, first,
            "re-running the FEATURES stage under the same inputs and schedule must \
             reproduce it; the first pass's terrain included the carver's writes, so a \
             second pass that no longer sees them cannot"
        );
        assert_eq!(
            second_wg, first_wg,
            "the post-carve WG value is a scan of the same terrain, so the re-run must \
             derive the same heights"
        );
    }

    /// The approved retention is 4096 post-carve WG chunks and a 384-chunk feature
    /// cache. They are different outputs with different lifetimes: dropping the block
    /// bucket must not shrink the WG retention to 384 (the value is not re-readable from
    /// a dropped bucket), but it must drop the claim that stood for those blocks.
    #[test]
    fn post_carve_wg_outlives_the_feature_cache_but_not_its_claim() {
        let mut g = Gen::load(4242, crate::name_to_state).expect("datapack");
        assert!(g.begin_carve(0, 0), "the real order: claim, then record");
        let mut tops = Box::new([0i32; 512]);
        tops[0] = 71;
        g.set_wg_after_carve(0, 0, tops);
        // A carved cell in this chunk's own bucket, so the 384 bound has something of
        // this chunk's to drop.
        g.feat_write(0, 71, 0, 1);
        assert!(g.feature_override(0, 71, 0).is_some());
        evict_feature_buckets(&mut g, 400);
        assert_eq!(
            g.feature_override(0, 71, 0),
            None,
            "the 384 bound really dropped this chunk's block output"
        );
        assert_eq!(
            g.wg_after_carve_height(false, 0, 0),
            Some(71),
            "the 4096 WG retention must not shrink to the 384 feature bound"
        );
        assert!(
            g.begin_carve(0, 0),
            "a claim whose block output was evicted must not survive it: the next \
             generation has to re-run the pass"
        );
    }

    /// Past 4096 chunks the post-carve value is really evicted. The record holds the
    /// value and the claim, so the eviction takes both: an absent value never leaves a
    /// claim behind that could not re-derive it, and the stage can be established again.
    #[test]
    fn the_wg_retention_bound_drops_the_value_and_the_claim_together() {
        let mut g = Gen::load(4242, crate::name_to_state).expect("datapack");
        assert!(g.begin_carve(0, 0));
        let mut tops = Box::new([0i32; 512]);
        tops[0] = 71;
        g.set_wg_after_carve(0, 0, tops);
        g.feat_write(0, 71, 0, 1);
        assert_eq!(g.wg_after_carve_height(false, 0, 0), Some(71));
        for i in 1..=4100 {
            assert!(g.begin_carve(i, 0), "chunk {i} has its own pass");
            g.set_wg_after_carve(i, 0, Box::new([64i32; 512]));
        }
        assert_eq!(
            g.wg_after_carve_height(false, 0, 0),
            None,
            "the 4096 bound must actually evict, not grow"
        );
        assert!(
            g.begin_carve(0, 0),
            "an evicted value must not leave a claim behind: that claim is what made \
             the value unrecoverable"
        );
        assert_eq!(
            g.feature_override(0, 71, 0),
            None,
            "the record's block output goes with the record, not into a second owner"
        );
        // The stage can be re-established from the same inputs, which is the only
        // contract an evicted value leaves behind.
        let mut again = Box::new([0i32; 512]);
        again[0] = 71;
        g.set_wg_after_carve(0, 0, again);
        assert_eq!(g.wg_after_carve_height(false, 0, 0), Some(71));
    }

    /// Claiming, recording, evicting and re-inserting the same chunks must not grow a
    /// queue, duplicate a slot or let the queue and the map disagree.
    #[test]
    fn repeated_stage_updates_keep_one_slot_per_chunk() {
        let mut g = Gen::load(4242, crate::name_to_state).expect("datapack");
        for round in 0..600i32 {
            let (cx, cz) = (round % 7, round % 5);
            g.begin_carve(cx, cz);
            g.set_wg_after_carve(cx, cz, Box::new([60 + round % 3; 512]));
            g.feat_write(cx * 16 + 1, 60, cz * 16 + 1, 1);
            g.feat_write(1_000_000 + round * 16, 60, 1_000_000, 1);
            let _ = g.feat_state(1_000_000, 60, 1_000_000, |_, _, _| {});
            let mut ord: Vec<_> = g.feat_ord.iter().copied().collect();
            ord.sort();
            assert!(
                ord.windows(2).all(|w| w[0] != w[1]),
                "round {round}: duplicate feat_ord slot"
            );
            assert_eq!(
                ord.len(),
                g.feat_c.len(),
                "round {round}: feat_ord must list exactly the buckets feat_c holds"
            );
            assert!(g.feat_ord.len() <= 384, "round {round}: bucket bound");
        }
        // Every subject chunk was written in the last rounds, so all of them are still
        // resident and keep their claim.
        let subjects: Vec<(i32, i32)> = (0..7).flat_map(|cx| (0..5).map(move |cz| (cx, cz))).collect();
        for &(cx, cz) in &subjects {
            assert!(
                g.feature_override(cx * 16 + 1, 60, cz * 16 + 1).is_some(),
                "{cx},{cz}: still resident"
            );
            assert!(!g.begin_carve(cx, cz), "a live bucket keeps its claim");
        }
        // Now turn every one of them over. A claim may not outlive the bucket it speaks
        // for: the next generation has to re-run the pass instead of reading the
        // uncarved terrain.
        evict_feature_buckets(&mut g, 500);
        for &(cx, cz) in &subjects {
            assert_eq!(
                g.feature_override(cx * 16 + 1, 60, cz * 16 + 1),
                None,
                "{cx},{cz}: the bucket turned over"
            );
            assert!(g.begin_carve(cx, cz), "a dropped bucket loses its claim");
        }
    }

    /// Evicting block output is not a reason to lose runtime work: a staged generation
    /// payload belongs to the runtime store, not to the cache that is being dropped.
    #[test]
    fn evicting_block_output_keeps_the_staged_payload() {
        let mut g = Gen::load(4242, crate::name_to_state).expect("datapack");
        let mut items = vec![(
            (0, 60, 0),
            crate::generation_payload::GenerationPayload::Sign(std::array::from_fn(|_| String::new())),
        )];
        g.stage_payloads((0, 0), &mut items);
        assert!(items.is_empty(), "the staging call takes the payloads");
        g.feat_write(0, 60, 0, 1);
        evict_feature_buckets(&mut g, 400);
        let taken = g.take_staged_payloads();
        assert_eq!(
            taken.len(),
            1,
            "dropping evicted blocks must not drop the pending runtime payload"
        );
        assert_eq!(taken[0].0, (0, 60, 0));
        // Recomputing the evicted source must not stage it a second time.
        let mut again = vec![(
            (0, 60, 0),
            crate::generation_payload::GenerationPayload::Sign(std::array::from_fn(|_| String::new())),
        )];
        g.stage_payloads((0, 0), &mut again);
        assert!(again.is_empty(), "a regenerated chunk keeps its payload source");
    }

    // ---- Layer A1-a: chunk-local aquifer grid (a1a_aq_*) ----

    /// The pre-A1a answer, computed from the same *uncached* primitives the cache
    /// is meant to accelerate: `aq_rpos` for the 12 cells (exact `>=` four-slot
    /// insertion) and `aq_fluid_level_at` for each of the three levels. This is the
    /// oracle the cached `aq_state` is compared against; it does not touch either
    /// generator's chunk cache.
    fn aq_state_uncached(g: &mut Gen, x: i32, y: i32, z: i32, density: f64) -> (Option<u32>, bool) {
        if density > 0.0 {
            return (None, false);
        }
        if g.aq.is_none() {
            return (
                Some(if y < -54 {
                    1
                } else if y < g.sea {
                    g.def_fl
                } else {
                    0
                }),
                false,
            );
        }
        let (water, lava) = {
            let a = g.aq.as_ref().unwrap();
            (a.water, a.lava_st)
        };
        let def = g.aq_pick(y);
        if Gen::lvl_block(def.0, def.1, y) == 2 {
            return (Some(lava), false);
        }
        let (sx, sy, sz) = (lxz(x - 5), ly12(y + 1), lxz(z - 5));
        let mut near = [((0, 0, 0), i64::MAX); 4];
        for dx in 0..=1 {
            for dy in -1..=1 {
                for dz in 0..=1 {
                    let p = g.aq_rpos(sx + dx, sy + dy, sz + dz);
                    let (ex, ey, ez) = (p.0 - x, p.1 - y, p.2 - z);
                    let h = ex as i64 * ex as i64 + ey as i64 * ey as i64 + ez as i64 * ez as i64;
                    let e = (p, h);
                    if near[0].1 >= h {
                        near[3] = near[2];
                        near[2] = near[1];
                        near[1] = near[0];
                        near[0] = e;
                    } else if near[1].1 >= h {
                        near[3] = near[2];
                        near[2] = near[1];
                        near[1] = e;
                    } else if near[2].1 >= h {
                        near[3] = near[2];
                        near[2] = e;
                    } else if near[3].1 >= h {
                        near[3] = e;
                    }
                }
            }
        }
        let dist = |p: (i32, i32, i32)| -> i64 {
            let (dx, dy, dz) = (p.0 - x, p.1 - y, p.2 - z);
            dx as i64 * dx as i64 + dy as i64 * dy as i64 + dz as i64 * dz as i64
        };
        let (d0, d1) = (dist(near[0].0), dist(near[1].0));
        let lv = |g: &mut Gen, i: usize| {
            let p = near[i].0;
            g.aq_fluid_level_at(p.0, p.1, p.2)
        };
        let l2 = lv(g, 0);
        let blk = Gen::lvl_block(l2.0, l2.1, y);
        let sim12 = 1.0 - (d1 - d0).abs() as f64 / 25.0;
        if sim12 <= 0.0 {
            return (
                Some(match blk {
                    1 => water,
                    2 => lava,
                    _ => 0,
                }),
                sim12 >= AQ_FLOWING_UPDATE_SIMULARITY,
            );
        }
        let pb = g.aq_pick(y - 1);
        if blk == 1 && Gen::lvl_block(pb.0, pb.1, y - 1) == 2 {
            return (Some(water), true);
        }
        let mut barrier = None;
        let l3 = lv(g, 1);
        let c12 = g.aq_calc_density(x, y, z, l2, l3, &mut barrier);
        if density + sim12 * c12 > 0.0 {
            return (None, false);
        }
        let l4 = lv(g, 2);
        let d2 = dist(near[2].0);
        let sim13 = 1.0 - (d2 - d0).abs() as f64 / 25.0;
        if sim13 > 0.0 {
            let c13 = g.aq_calc_density(x, y, z, l2, l4, &mut barrier);
            if density + sim12 * sim13 * c13 > 0.0 {
                return (None, false);
            }
        }
        let sim23 = 1.0 - (d2 - d1).abs() as f64 / 25.0;
        if sim23 > 0.0 {
            let c23 = g.aq_calc_density(x, y, z, l3, l4, &mut barrier);
            if density + sim12 * sim23 * c23 > 0.0 {
                return (None, false);
            }
        }
        (
            Some(match blk {
                1 => water,
                2 => lava,
                _ => 0,
            }),
            true,
        )
    }

    #[test]
    fn a1a_aq_chunk_grid_geometry_and_flat_index_identity() {
        let _guard = lock_gen();
        let g = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        let (min_y, height) = (g.min_y, g.height);
        for (cx, cz) in [(-3i32, -2i32), (0, 0), (7, -5)] {
            let c = AqChunkCache::new(cx, cz, min_y, height);
            assert_eq!((c.nx, c.nz), (3, 3), "the X/Z span is [c-1, c+1]");
            assert_eq!(c.cx, cx);
            assert_eq!(c.cz, cz);
            assert_eq!(
                c.rpos.len(),
                c.nx * c.ny * c.nz,
                "positions and levels share one flat index space"
            );
            assert_eq!(c.levels.len(), c.rpos.len());
            // All 12 cells of every corner block and of the boundary-offset blocks
            // live in this chunk's grid, at distinct indices.
            for lx in [0, 5, 15] {
                for lz in [0, 5, 15] {
                    let (x, z) = (cx * 16 + lx, cz * 16 + lz);
                    let (sx, sz) = (lxz(x - 5), lxz(z - 5));
                    for y in [min_y, -54, 0, 63, min_y + height - 1] {
                        let sy = ly12(y + 1);
                        let mut idxs = std::collections::HashSet::new();
                        for dx in 0..=1 {
                            for dy in -1..=1 {
                                for dz in 0..=1 {
                                    let (gx, gy, gz) = (sx + dx, sy + dy, sz + dz);
                                    assert!(c.contains(gx, gy, gz), "({gx},{gy},{gz}) outside grid");
                                    assert!(idxs.insert(c.idx(gx, gy, gz)), "flat index collision");
                                }
                            }
                        }
                        assert_eq!(idxs.len(), 12);
                    }
                }
            }
            // The sample position of each cell rounds back to that very cell: this
            // is why the fluid level can be stored at the position's own index.
            for gx in (c.min_gx - 1)..=(c.min_gx + c.nx as i32) {
                for gy in (c.min_gy - 1)..=(c.min_gy + c.ny as i32) {
                    for gz in (c.min_gz - 1)..=(c.min_gz + c.nz as i32) {
                        let (rx, ry, rz) = g.aq_rpos(gx, gy, gz);
                        assert_eq!((rx.div_euclid(16), ry.div_euclid(12), rz.div_euclid(16)), (gx, gy, gz));
                    }
                }
            }
        }
    }

    #[test]
    fn a1a_aq_state_matches_uncached_overworld_and_nether() {
        let _guard = lock_gen();
        for (preset, seed, label) in [
            (NoisePreset::Overworld, 0, "overworld"),
            (NoisePreset::Nether, 4242, "nether"),
        ] {
            let local = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            let data = std::sync::Arc::clone(&local.data);
            let mut cached = Gen::fresh_scratch(data.clone());
            let mut direct = Gen::fresh_scratch(data);
            let (lo, hi) = (cached.data.min_y, cached.data.min_y + cached.data.height - 1);
            let sea = cached.sea;
            let ys = [lo, -54, 0, sea, hi];
            let mut checked = 0usize;
            // Negative chunk origins plus the chunk boundary offsets the neighbour
            // scan hinges on; interleaving the two generators also exercises the
            // MRU ring's eviction and reuse.
            for (cx, cz) in [(-2i32, -1i32), (0, 0), (3, -4)] {
                for lx in [0, 5, 15] {
                    for lz in [0, 5, 15] {
                        let (x, z) = (cx * 16 + lx, cz * 16 + lz);
                        for y in ys {
                            for density in [-0.5f64, 0.0, 0.25] {
                                let got = cached.aq_state(x, y, z, density);
                                let sched = cached.aq_schedule_fluid_update();
                                let want = aq_state_uncached(&mut direct, x, y, z, density);
                                assert_eq!(
                                    (got, sched),
                                    want,
                                    "{label} ({x},{y},{z}) d={density}: cached vs uncached"
                                );
                                checked += 1;
                            }
                        }
                    }
                }
            }
            assert_eq!(checked, 405, "{label}: dense sweep actually ran");
        }
    }

    #[test]
    fn a1a_aq_near3_tie_break_matches_four_slot_selection() {
        let _guard = lock_gen();
        let mut g = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        let mut ties = 0usize;
        let mut checked = 0usize;
        // Dense X/Z sweep at several Y bands: symmetric cells tie often, and the
        // `>=` insertion rule decides which of the tied pair lands in slot 0.
        for y in [-60, -20, 20, 63] {
            for x in -48..48 {
                for z in [-17, 0, 5, 16] {
                    let (near, slot) = g.aq_near3(x, y, z).expect("inside the Y window");
                    assert!(slot < AQ_CHUNK_SLOTS);
                    let (sx, sy, sz) = (lxz(x - 5), ly12(y + 1), lxz(z - 5));
                    let mut four = [((0, 0, 0), i64::MAX); 4];
                    for dx in 0..=1 {
                        for dy in -1..=1 {
                            for dz in 0..=1 {
                                let p = g.aq_rpos(sx + dx, sy + dy, sz + dz);
                                let (ex, ey, ez) = (p.0 - x, p.1 - y, p.2 - z);
                                let h = ex as i64 * ex as i64 + ey as i64 * ey as i64
                                    + ez as i64 * ez as i64;
                                let e = (p, h);
                                if four[0].1 >= h {
                                    four[3] = four[2];
                                    four[2] = four[1];
                                    four[1] = four[0];
                                    four[0] = e;
                                } else if four[1].1 >= h {
                                    four[3] = four[2];
                                    four[2] = four[1];
                                    four[1] = e;
                                } else if four[2].1 >= h {
                                    four[3] = four[2];
                                    four[2] = e;
                                } else if four[3].1 >= h {
                                    four[3] = e;
                                }
                            }
                        }
                    }
                    assert_eq!(
                        [near[0].0, near[1].0, near[2].0],
                        [four[0].0, four[1].0, four[2].0],
                        "({x},{y},{z}): nearest-three selection differs"
                    );
                    assert_eq!(
                        [near[0].1, near[1].1, near[2].1],
                        [four[0].1, four[1].1, four[2].1],
                        "({x},{y},{z}): reused distance differs"
                    );
                    if four[0].1 == four[1].1 || four[1].1 == four[2].1 {
                        ties += 1;
                    }
                    checked += 1;
                }
            }
        }
        assert!(checked > 1000 && ties > 0, "sweep must include observable ties: {ties}");
    }

    /// Layer A2-a: the flat, generation-stamped corner memo must produce exactly the
    /// same corner-grid bits as the original `eval_c` + `HashMap` walk, on all presets,
    /// at negative chunk origins, and across the full negative-to-positive `y` range.
    #[test]
    fn a2a_flat_corner_memo_matches_hashmap_bit_for_bit() {
        let _guard = lock_gen();
        for (preset, seed) in [
            (NoisePreset::Overworld, 0u64),
            (NoisePreset::Overworld, 12345),
            (NoisePreset::Nether, 4242),
            (NoisePreset::End, 0),
        ] {
            let mut g = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            let nx = (16 / g.noise_cell_w + 1) as usize;
            let ny = ((g.height / g.noise_cell_h) + 1) as usize;
            let is2 = g.is_2d.clone();
            for (cx, cz) in [(0i32, 0i32), (-17, -23), (5, -7)] {
                // `final_columns = true` fills every `interp_ids()` grid — density and
                // vein alike; a base column only carries the density-reachable ones.
                let cc = g.build_cols_body(cx, cz, true).expect("cols");
                let mut memo: HashMap<(usize, i32, i32, i32), f64> = HashMap::new();
                let mut grids = 0usize;
                for iid in g.interp_ids() {
                    let inner = match g.dfs[iid] {
                        DF::Interp(i) => i,
                        _ => continue,
                    };
                    let have = cc.grids[iid].as_ref().expect("grid");
                    assert_eq!(have.len(), nx * nx * ny, "grid length");
                    for zi in 0..nx {
                        for xi in 0..nx {
                            for yi in 0..ny {
                                let bx = cx * 16 + xi as i32 * g.noise_cell_w;
                                let by = g.min_y + yi as i32 * g.noise_cell_h;
                                let bz = cz * 16 + zi as i32 * g.noise_cell_w;
                                let want = g.eval(inner, bx, by, bz, &mut memo);
                                let got = have[(zi * nx + xi) * ny + yi];
                                assert_eq!(
                                    want.to_bits(),
                                    got.to_bits(),
                                    "{preset:?} seed {seed} chunk ({cx},{cz}) iid {iid} at ({bx},{by},{bz})"
                                );
                            }
                        }
                    }
                    grids += 1;
                }
                assert!(grids > 0, "no interpolated grids were compared");

                // Direct node-level check of the signed-zero hazard: a 2D node's value
                // at the bottom (negative y) must be bit-identical to `eval_c` at any
                // other y for the same (x, z). Both NaN results are treated as equal.
                let mut flat = FlatCornerMemo::new(g.dfs.len());
                let (bx, bz) = (cx * 16, cz * 16);
                flat.next_2d();
                for (id, &two_d) in is2.iter().enumerate() {
                    if !two_d {
                        continue;
                    }
                    flat.next_3d();
                    let v0 = g.eval_flat(id, bx, g.min_y, bz, &mut flat);
                    for by in [-64, -1, 0, 1, 63, 320] {
                        let mut sm = HashMap::new();
                        let want = g.eval_c(id, bx, by, bz, &mut sm, None);
                        if want.is_nan() && v0.is_nan() {
                            continue;
                        }
                        assert_eq!(
                            want.to_bits(),
                            v0.to_bits(),
                            "2D node {id} at ({bx},{by},{bz}): y-independence / signed zero"
                        );
                    }
                }
            }
        }
    }

    /// Layer A2-a: `run4_avx2`'s `Op::Noise` (with the fixed `z as f64 * xzs` coordinate),
    /// `BranchOut`, `BranchLt` and `BranchZero` movemask branches must match the scalar
    /// `DfProg::run` bit for bit, including NaN, `-0.0` and the branch boundaries.
    #[test]
    fn a2a_run4_avx2_shifted_noise_and_branches_match_scalar_bit_for_bit() {
        let _guard = lock_gen();
        let _measurement = M3_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let g = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        df4_test_force_scalar(false);
        m3_test_force(true);
        m3_reset_local();

        let check = |ops: Vec<Op>, what: &str, x: i32, y: i32, z: i32| {
            let prog = df4_prog(ops);
            let want: [f64; DF4] = std::array::from_fn(|k| prog.eval(&g, None, x + k as i32, y, z));
            let got = prog.eval_x4(&g, None, x, y, z);
            for k in 0..DF4 {
                assert_eq!(
                    want[k].to_bits(),
                    got[k].to_bits(),
                    "{what} lane {k} at x={} y={y} z={z}",
                    x + k as i32
                );
            }
        };

        // `Op::Noise` with x != z distinguishes the fixed z coordinate from the old
        // (buggy) `lane_x[k] * xzs + zs[k]`. Include negative and mixed-sign x/z.
        let mut noise_cases = 0usize;
        for (xzs, ys) in [(1.0, 0.0), (0.25, 0.125), (0.0, 1.0), (-0.5, 0.0)] {
            for (x, y, z) in [(3, 7, -2), (-9, -40, 11), (17, 63, -33)] {
                for (xs, ysv, zs) in [(0.0, 0.0, 0.0), (1.25, -0.5, 2.75)] {
                    check(
                        vec![
                            Op::Push(xs),
                            Op::Push(ysv),
                            Op::Push(zs),
                            Op::Noise { ni: 0, xzs, ys },
                            Op::Halt,
                        ],
                        "noise",
                        x,
                        y,
                        z,
                    );
                    noise_cases += 1;
                }
            }
        }
        assert!(noise_cases >= 24, "noise cases did not run");

        let mut branch_cases = 0usize;
        for v in [f64::NAN, -0.0, 0.0, 1.0, -1.0, 0.5, f64::INFINITY] {
            for (mi, ma) in [(-1.0, 1.0), (0.0, 0.0), (-0.0, 0.5)] {
                check(
                    vec![
                        Op::Push(v),
                        Op::BranchOut { mi, ma, target: 4 },
                        Op::Push(1.0),
                        Op::Halt,
                        Op::Push(0.0),
                        Op::Halt,
                    ],
                    "branch_out",
                    0,
                    7,
                    3,
                );
                branch_cases += 1;
            }
        }
        for v in [f64::NAN, -0.0, 0.0, -1.0, 1.0] {
            for t in [0.0, -0.0, 1.0] {
                check(
                    vec![
                        Op::Push(v),
                        Op::BranchLt { t, target: 4 },
                        Op::Push(1.0),
                        Op::Halt,
                        Op::Push(0.0),
                        Op::Halt,
                    ],
                    "branch_lt",
                    0,
                    7,
                    3,
                );
                branch_cases += 1;
            }
        }
        for v in [f64::NAN, -0.0, 0.0, 1.0, -1.0] {
            check(
                vec![
                    Op::Push(v),
                    Op::BranchZero { target: 5 },
                    Op::Drop,
                    Op::Push(1.0),
                    Op::Halt,
                    Op::Drop,
                    Op::Push(0.0),
                    Op::Halt,
                ],
                "branch_zero",
                0,
                7,
                3,
            );
            branch_cases += 1;
        }
        assert!(branch_cases >= 36, "branch cases did not run");

        let vec = m3_local().get(M3_DF4_VEC);
        m3_test_force(false);
        if df4_avx2() {
            assert!(vec > 0, "the vector arm never ran: {vec} groups");
        }
    }

    // ---- Layer A2-c: the eight-lane density evaluator ----

    /// `trilinear_x8` must be eight scalar `trilinear` calls bit for bit on every 8-aligned
    /// start, and refuse a start that cannot be split into two cell-aligned 4-lane halves.
    #[test]
    fn a2c_trilinear_x8_matches_scalar_bit_for_bit() {
        let (cell_w, cell_h, min_y, world_height) = (4, 8, -64, 128);
        let nx = (16 / cell_w + 1) as usize;
        let ny = (world_height / cell_h + 1) as usize;
        let columns = ChunkCols {
            grids: Vec::new(),
            surf: [0; 256],
            ny,
            cell_w,
            cell_h,
            min_y,
            mats: (0..256).map(|_| None).collect(),
            rd: [0; 256],
            wtop: [0; 256],
            beard: None,
            carve_sched: None,
        };
        // Distinct corner values with signed zeros and subnormals, so a wrong corner load
        // or a reordered lerp cannot hide behind a constant grid.
        let mut cells = vec![0.0f64; nx * nx * ny];
        for (i, c) in cells.iter_mut().enumerate() {
            let bits = 0x3ff0_0000_0000_0000u64.wrapping_add((i as u64).wrapping_mul(0x9e37_79b9));
            *c = f64::from_bits(bits) * (if i % 3 == 0 { -1.0 } else { 0.5 });
            if i % 11 == 0 {
                *c = -0.0;
            }
        }
        let mut compared = 0usize;
        for cz in [0, 3] {
            for cx in [0, -2] {
                for z in [cz * 16 + 1, cz * 16 + 6, cz * 16 + 15] {
                    for x in [cx * 16, cx * 16 + 8] {
                        for y in [min_y, min_y + 13, min_y + 121] {
                            let got = trilinear_x8(&cells, &columns, x, y, z)
                                .expect("8-aligned start with cell_w = 4");
                            for (k, &lane_v) in got.iter().enumerate() {
                                let want = trilinear(&cells, &columns, x + k as i32, y, z);
                                assert_eq!(
                                    want.to_bits(),
                                    lane_v.to_bits(),
                                    "trilinear lane {k} at {x}+{k}, {y}, {z}"
                                );
                                compared += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(compared >= 2 * 2 * 3 * 2 * 3 * DF8, "trilinear cases did not run");
        for x in [1, 2, 4, 6, 15] {
            assert!(
                trilinear_x8(&cells, &columns, x, min_y, 0).is_none(),
                "x={x} is not 8-aligned"
            );
        }
        let narrow = ChunkCols {
            grids: Vec::new(),
            surf: [0; 256],
            ny,
            cell_w: 2,
            cell_h,
            min_y,
            mats: (0..256).map(|_| None).collect(),
            rd: [0; 256],
            wtop: [0; 256],
            beard: None,
            carve_sched: None,
        };
        assert!(trilinear_x8(&cells, &narrow, 0, min_y, 0).is_none());
    }

    /// Every op the eight-lane evaluator implements, over the same edge values as the
    /// four-lane test (cycled across successive groups), held to `DfProg::run` bit for bit.
    /// `Push` is lane-uniform, so each group checks eight lanes of one operand pair; cycling
    /// the pair across all edge values covers the same IEEE-754 corner cases.
    #[test]
    fn a2c_run8_ops_match_scalar_over_edge_values() {
        let _guard = lock_gen();
        let _measurement = M3_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let g = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        df4_test_force_scalar(false);
        m3_test_force(true);
        m3_reset_local();
        let mut compared = 0usize;
        let mut nan_results = 0usize;
        let mut check = |ops: Vec<Op>, what: &str, a: f64, b: f64| {
            let prog = df4_prog(ops);
            for x in [-8, 0, 5] {
                let want: [f64; DF8] =
                    std::array::from_fn(|k| prog.eval(&g, None, x + k as i32, 7, 3));
                let got = prog.eval_x8(&g, None, x, 7, 3);
                for k in 0..DF8 {
                    // Two-NaN operands: as in the four-lane test, neither lowering promises
                    // which payload survives, so "both NaN" is the honest comparison.
                    if want[k].is_nan() && got[k].is_nan() {
                        nan_results += 1;
                        continue;
                    }
                    assert_eq!(
                        want[k].to_bits(),
                        got[k].to_bits(),
                        "{what} lane {k} of x={x}: {a:?} / {b:?} -> scalar {:?} vector {:?}",
                        want[k],
                        got[k]
                    );
                    compared += 1;
                }
            }
        };
        let unary: [(&str, Op); 8] = [
            ("neg", Op::Neg),
            ("abs", Op::Abs),
            ("sq", Op::Sq),
            ("cube", Op::Cube),
            ("hneg", Op::HNeg),
            ("qneg", Op::QNeg),
            ("squeeze", Op::Squeeze),
            ("clamp(-1,1)", Op::Clamp { lo: -1.0, hi: 1.0 }),
        ];
        for (name, op) in unary {
            for v in DF4_EDGES {
                check(vec![Op::Push(v), op, Op::Halt], name, v, v);
            }
        }
        let binary: [(&str, Op); 6] = [
            ("add", Op::Add),
            ("sub", Op::Sub),
            ("mul", Op::Mul),
            ("div", Op::Div),
            ("min", Op::Min),
            ("max", Op::Max),
        ];
        for (name, op) in binary {
            for a in DF4_EDGES {
                for b in DF4_EDGES {
                    check(vec![Op::Push(a), Op::Push(b), op, Op::Halt], name, a, b);
                }
            }
        }
        for c in DF4_EDGES {
            for v in DF4_EDGES {
                check(vec![Op::Push(v), Op::AddC(c), Op::Halt], "addC", v, c);
                check(vec![Op::Push(v), Op::MulC(c), Op::Halt], "mulC", v, c);
                check(vec![Op::Push(v), Op::MinC(c), Op::Halt], "minC", v, c);
                check(vec![Op::Push(v), Op::MaxC(c), Op::Halt], "maxC", v, c);
            }
        }
        for a in [0.0f64, -0.0, 1.0, -1.0, f64::NAN] {
            for f in [0.0f64, 1.0, -1.0, f64::NAN] {
                for s in [0.5f64, -0.25, f64::NEG_INFINITY] {
                    check(
                        vec![Op::Push(a), Op::Push(f), Op::Push(s), Op::Lerp, Op::Halt],
                        "lerp",
                        a,
                        f,
                    );
                }
            }
        }
        for (fc, fv, tc, tv) in [
            (-64.0, 0.0, -40.0, 1.0),
            (240.0, 1.0, 256.0, 0.0),
            (0.0, -0.0, 0.0, 1.0),
        ] {
            let prog = df4_prog(vec![Op::Grad { fc, fv, tc, tv }, Op::Halt]);
            for y in [g.min_y, -40, 0, 240, g.min_y + g.height - 1] {
                let want = prog.eval(&g, None, 3, y, -2);
                let got = prog.eval_x8(&g, None, 3, y, -2);
                for (k, &lane_v) in got.iter().enumerate() {
                    assert_eq!(want.to_bits(), lane_v.to_bits(), "grad {fc} y={y} lane {k}");
                    compared += 1;
                }
            }
        }
        let vec = m3_local().get(M3_DF4_VEC);
        m3_test_force(false);
        assert!(compared > 1_000, "only {compared} lane values were compared");
        assert!(nan_results > 0, "NaN cases stopped being covered");
        if df8_avx512() {
            assert!(vec > 0, "eight-lane vector arm never ran: {vec} groups");
        }
    }

    /// A lane-divergent conditional must bail out of the 8-lane arm; the two `eval_x4` halves
    /// it falls back to must still answer all eight positions bit for bit.
    #[test]
    fn a2c_run8_mixed_branches_fall_back_to_eval_x4_bit_for_bit() {
        let _guard = lock_gen();
        let _measurement = M3_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let g = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        df4_test_force_scalar(false);
        // A noise whose value varies with x so the eight lanes disagree at a chosen cut.
        let probe = df4_prog(vec![
            Op::Push(0.0),
            Op::Push(0.0),
            Op::Push(0.0),
            Op::Noise { ni: 0, xzs: 1.0, ys: 0.0 },
            Op::Halt,
        ]);
        let (x, y, z) = (3, 7, -2);
        let vals: [f64; DF8] =
            std::array::from_fn(|k| probe.eval(&g, None, x + k as i32, y, z));
        let lo = vals.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        assert!(lo < hi, "the probe noise did not vary across the eight lanes");
        let mid = lo + (hi - lo) * 0.5;
        let check = |ops: Vec<Op>, what: &str| {
            let prog = df4_prog(ops);
            let want: [f64; DF8] =
                std::array::from_fn(|k| prog.eval(&g, None, x + k as i32, y, z));
            let got = prog.eval_x8(&g, None, x, y, z);
            for k in 0..DF8 {
                assert_eq!(
                    want[k].to_bits(),
                    got[k].to_bits(),
                    "{what} fallback lane {k}"
                );
            }
            #[cfg(target_arch = "x86_64")]
            if df8_avx512() {
                let direct = unsafe { run8_avx512(&prog, &g, None, x, y, z) };
                assert!(direct.is_none(), "{what}: mixed lanes must bail out of run8");
            }
        };
        // BranchLt with a cut strictly between the lane values.
        check(
            vec![
                Op::Push(0.0),
                Op::Push(0.0),
                Op::Push(0.0),
                Op::Noise { ni: 0, xzs: 1.0, ys: 0.0 },
                Op::BranchLt { t: mid, target: 7 },
                Op::Push(1.0),
                Op::Halt,
                Op::Push(0.0),
                Op::Halt,
            ],
            "branch_lt",
        );
        // BranchOut with a lower bound strictly between the lane values.
        check(
            vec![
                Op::Push(0.0),
                Op::Push(0.0),
                Op::Push(0.0),
                Op::Noise { ni: 0, xzs: 1.0, ys: 0.0 },
                Op::BranchOut { mi: mid, ma: 1.0e9, target: 7 },
                Op::Push(1.0),
                Op::Halt,
                Op::Push(0.0),
                Op::Halt,
            ],
            "branch_out",
        );
        // BranchZero: subtract lane 0's value so exactly that lane becomes +0.0.
        check(
            vec![
                Op::Push(0.0),
                Op::Push(0.0),
                Op::Push(0.0),
                Op::Noise { ni: 0, xzs: 1.0, ys: 0.0 },
                Op::AddC(-vals[0]),
                Op::BranchZero { target: 9 },
                Op::Drop,
                Op::Push(1.0),
                Op::Halt,
                Op::Drop,
                Op::Push(0.0),
                Op::Halt,
            ],
            "branch_zero",
        );
        // A NaN is "out of range" for both ordered compares, so every lane takes the
        // branch - the same answer the scalar `>= mi && < ma` gives. This pins the
        // vector form against an inverted `< mi || >= ma` formulation (which would
        // answer false for NaN and fall through).
        let nan_branch = df4_prog(vec![
            Op::Push(f64::NAN),
            Op::BranchOut { mi: -1.0, ma: 1.0, target: 4 },
            Op::Push(1.0),
            Op::Halt,
            Op::Push(0.0),
            Op::Halt,
        ]);
        let want = nan_branch.eval(&g, None, x, y, z);
        let got = nan_branch.eval_x8(&g, None, x, y, z);
        for (k, &lane_v) in got.iter().enumerate() {
            assert_eq!(want.to_bits(), lane_v.to_bits(), "NaN branch_out lane {k}");
        }
    }

    /// The main A2-c identity: `block_density_x8` (full 8-lane arm or the 4-lane fallback)
    /// against eight scalar `block_density` calls, on Overworld, Nether and End programs
    /// including `Noise`, `Weird`, `Interp`, the three branches and signed-zero `Min`/`Max`.
    #[test]
    fn a2c_run8_avx512_matches_scalar_bit_for_bit() {
        let _guard = lock_gen();
        let _measurement = M3_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        df4_test_force_scalar(false);
        m3_test_force(true);
        m3_reset_local();
        let mut compared = 0usize;
        let mut programs = 0usize;
        for (preset, seed) in [
            (NoisePreset::Overworld, 0u64),
            (NoisePreset::Overworld, 12345),
            (NoisePreset::Nether, 4242),
            (NoisePreset::End, 0),
        ] {
            let mut g = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            if DfProg::compile_inner(&g).is_none() {
                continue;
            }
            programs += 1;
            for (cx, cz) in [(0, 0), (-3, 2)] {
                let cc = g.cols(cx, cz).expect("columns");
                for lz in (0..16).step_by(5) {
                    for lx in (0..16).step_by(DF8) {
                        for ly in (0..16).step_by(3) {
                            let x = cx * 16 + lx;
                            let y = g.min_y + ly;
                            let z = cz * 16 + lz;
                            let got = g.block_density_x8(&cc, x, y, z);
                            for (k, &lane_v) in got.iter().enumerate() {
                                let want = g.block_density(&cc, x + k as i32, y, z);
                                assert_eq!(
                                    want.to_bits(),
                                    lane_v.to_bits(),
                                    "{preset:?} seed {seed} chunk ({cx},{cz}) x={} y={y} z={z}",
                                    x + k as i32
                                );
                                compared += 1;
                            }
                        }
                    }
                }
            }
        }
        let vec = m3_local().get(M3_DF4_VEC);
        let bail = m3_local().get(M3_DF4_BAIL);
        m3_test_force(false);
        assert_eq!(programs, 4, "every preset must compile here");
        assert!(compared > 2_000, "only {compared} positions compared");
        if df8_avx512() {
            assert!(vec > 0, "the 8-lane vector arm never ran: {vec} groups");
        }
        // The counter is informational: which groups diverge depends on the preset. Keep the
        // variable used so a future reader sees the bailout is measured, not assumed.
        let _ = bail;
    }

    /// Layer A2-b: at `beard_depth > 0` a structure start asks the same chunk's heightmap
    /// several times. The single-column answer from a cached `base_cols` must equal the
    /// full `build_cols(cx, cz, true)` cell for cell. Both references are built at
    /// `beard_depth = 1` so neither can pick up a process-wide beardifier resolver and
    /// both see `beard = None`, which is the premise of the fast path.
    #[test]
    fn a2b_single_column_top_ws_and_top_oc_match_full_cols_bit_for_bit() {
        type TopColCase = (NoisePreset, u64, &'static [(i32, i32)]);
        let cases: [TopColCase; 4] = [
            (NoisePreset::Overworld, 0, &[(0, 0), (1, 0), (-1, 0), (0, -1)]),
            (NoisePreset::Overworld, 4242, &[(0, 0), (-2, 1)]),
            (NoisePreset::Nether, 4242, &[(0, 0), (-1, 1)]),
            (NoisePreset::End, 0, &[(0, 0), (-1, 0)]),
        ];
        let mut compared = 0usize;
        for (preset, seed, chunks) in cases {
            let mut full = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            let mut single = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            full.beard_depth = 1;
            single.beard_depth = 1;
            assert!(single.col_c.is_empty(), "a nested query must start with no final columns");
            for &(cx, cz) in chunks {
                let cc = full.build_cols(cx, cz, true).expect("full columns");
                for lz in 0..16usize {
                    for lx in 0..16usize {
                        let x = cx * 16 + lx as i32;
                        let z = cz * 16 + lz as i32;
                        let i = lz * 16 + lx;
                        assert_eq!(
                            single.top_oc(x, z),
                            cc.surf[i] as i32 + 1,
                            "{preset:?} seed {seed} OCEAN_FLOOR at ({x},{z})"
                        );
                        assert_eq!(
                            single.top_ws(x, z),
                            cc.wtop[i] as i32 + 1,
                            "{preset:?} seed {seed} WORLD_SURFACE at ({x},{z})"
                        );
                        compared += 2;
                    }
                }
            }
        }
        assert!(compared >= 5_000, "the identity check was too small: {compared}");
    }

    /// Layer A2-b: building a column and answering height queries must leave the
    /// surface-material cell empty; the first material read computes exactly what the
    /// old eager in-pipeline scan produced.
    #[test]
    fn a2b_lazy_surface_mats_stays_uncomputed_on_height_queries_and_matches_eager() {
        let mut g = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        let (cx, cz) = (0, 0);
        let cc = g.cols(cx, cz).unwrap();
        assert!(
            !cc.surface_mats_computed(),
            "build_cols must not compute surface materials eagerly"
        );
        // A nested height query on a different chunk goes through `base_cols`; its
        // lazy material cell must stay empty too.
        g.beard_depth = 1;
        let base = g.base_cols(1, 0).unwrap();
        let _ = g.top_ws(19, 5);
        let _ = g.top_oc(19, 5);
        assert!(
            !base.surface_mats_computed(),
            "a heightmap query must not populate the material cell"
        );
        assert!(
            !cc.surface_mats_computed(),
            "a heightmap query on a final column must not populate the material cell"
        );
        // The eager body and the lazy cell must agree field for field.
        let eager = g.build_surface_mats(cx, cz, &cc);
        let lazy = cc.surface_mats(&mut g);
        assert_eq!(lazy.mats.len(), eager.mats.len());
        for (i, (a, b)) in lazy.mats.iter().zip(eager.mats.iter()).enumerate() {
            match (a, b) {
                (None, None) => {}
                (Some(a), Some(b)) => {
                    assert_eq!(a.lo, b.lo, "material window at column {i}");
                    assert_eq!(a.vals, b.vals, "material result at column {i}");
                }
                _ => panic!("column {i}: one arm produced a window and the other did not"),
            }
        }
        assert_eq!(&lazy.rd, &eager.rd, "run_depth differs between the lazy and eager scans");
        assert!(cc.surface_mats_computed(), "the lazy cell was not published");
    }
}

#[cfg(test)]
mod bedrock_bench {
    /// Diagnostic: is a flat bedrock layer worth its parity cost?
    /// The bedrock band is ~7us per chunk against ~5.7ms for the terrain layer
    /// (about 0.12%), so flattening it does not make the generator materially
    /// lighter and the per-cell vanilla pattern is kept. Run with
    /// `WGBEDROCK=1 cargo test --release time_deep_state_band -- --nocapture`.
    /// Env-gated like the other probes: a parked ignored test counts against
    /// the debt ratchet, and this is a benchmark rather than a skipped assertion.
    #[test]
    fn time_deep_state_band() {
        if std::env::var("WGBEDROCK").is_err() {
            return;
        }
        let mut g = super::Gen::load(0, crate::name_to_state).expect("datapack");
        let cc = g.cols(0, 0).expect("cols");
        let start = std::time::Instant::now();
        let mut n = 0u64;
        let mut acc = 0u64;
        for _ in 0..20 {
            for y in -63..=-60i32 {
                for z in 0..16 {
                    for x in 0..16 {
                        acc = acc.wrapping_add(super::deep_state(x, y, z, &cc) as u64);
                        n += 1;
                    }
                }
            }
        }
        let elapsed = start.elapsed();
        eprintln!(
            "BEDROCK_BENCH cells={n} elapsed={elapsed:?} per_cell={:?} acc={acc}",
            elapsed / n as u32
        );
        // Whole-chunk context: how much is the bedrock band against full generation?
        let mut cg = super::Gen::load(0, crate::name_to_state).expect("datapack");
        let t = std::time::Instant::now();
        let mut chunks = 0u32;
        for cx in 0..8i32 {
            for cz in 0..8i32 {
                if cg.cols(cx, cz).is_some() {
                    chunks += 1;
                }
            }
        }
        eprintln!(
            "CHUNK_BENCH chunks={chunks} elapsed={:?} per_chunk={:?}",
            t.elapsed(),
            t.elapsed() / chunks
        );
    }
}

#[cfg(all(test, megu_vanilla_data))]
#[path = "weird_sampler_tests.rs"]
mod weird_sampler_tests;

#[cfg(all(test, megu_vanilla_data))]
#[path = "aquifer_native_tests.rs"]
mod aquifer_native_tests;

