// SPDX-License-Identifier: LGPL-3.0-or-later

//! Deterministic crop-archetype classifier for procedural 3D crop meshes.
//!
//! [`classify`] decides which silhouette family a crop block — vanilla or
//! modded — should use for the procedural 3D crop upgrade in
//! [`crate::vanilla_plus_3d`]: [`CropArchetype::SheafStalk`] (wheat/rice/corn
//! style stalk bundles with grain heads), [`CropArchetype::BulbLeaf`]
//! (carrot/potato/beetroot style ground bulbs with leaf rosettes), or
//! [`CropArchetype::VineFruit`] (tomato/berry/pepper style stems with
//! isolated fruit pods). There is **no per-mod or per-block name list**
//! anywhere in this module: identity comes from registry tags, the `age`
//! blockstate, the resolved parent-model chain, and integer texture
//! features of the block's 16x16 sprite.
//!
//! Pipeline:
//!
//! 1. **Crop gate** — a block counts as a crop when a crop-family tag says
//!    so (`minecraft:crops`, `c:crops`, `forge:crops`, or any `*:crop(s)`
//!    tag), when an `age` property meets a `block/crop`, `block/cross` or
//!    `block/tinted_cross` parent in the model chain, or when a supporting
//!    tag (`maintains_farmland`, `bee_growables`) is corroborated by either
//!    signal. Flowers, saplings, grass, mushrooms and vine blocks carry none
//!    of these and fail closed to `is_crop = false`.
//! 2. **Archetype** — the alpha mask of the sprite is reduced to pure
//!    integer features: alpha coverage, bounding-box aspect and fill,
//!    vertical mass profile, column-run statistics, 4-connected component
//!    statistics, and dominant-hue clusters via integer HSV hue bins. Three
//!    evidence scores (one per archetype) are compared; **no floats touch
//!    the decision path**, so results are bit-identical on every CPU.
//! 3. **Confidence** — the winner must clear a minimum raw score *and* a
//!    minimum lead over the runner-up. Anything weaker returns
//!    `archetype = None` so the caller falls back cleanly to plain cross
//!    extrusion.
//!
//! Everything is pure `std`, reads no wall clock, uses no randomness, and
//! iterates only fixed-size arrays — [`classify`] is a pure function of its
//! input and [`decision_fingerprint`] pins the outcome.

use crate::vanilla_plus_3d::Crop3dArchetype;

// ---------------------------------------------------------------------------
// Limits and constants
// ---------------------------------------------------------------------------

/// Edge length of the sprite the classifier analyses, in pixels.
pub const SPRITE_DIM: usize = 16;

/// Byte length of one 16x16 RGBA8 sprite (`16 * 16 * 4`).
pub const SPRITE_BYTES: usize = SPRITE_DIM * SPRITE_DIM * 4;

/// Alpha at/above which a texel counts as opaque — the same cutout threshold
/// the ore classifier uses.
const ALPHA_CUTOFF: u8 = 128;

/// Fewer opaque texels than this cannot carry archetype evidence (a lone
/// sprout or salt-and-pepper noise).
const MIN_OPAQUE_PX: u32 = 12;

/// Sprites denser than this many opaque texels — or whose bounding box is
/// more than ~80% ink — are solid slabs, not cutout plants.
const MAX_OPAQUE_PX: u32 = 192;
const FILL_NUM: u32 = 4;
const FILL_DEN: u32 = 5;

/// A vertical column run at least this long counts as a stalk/column.
const TALL_RUN_MIN: u32 = 9;
/// A full-height stalk run.
const SHEAF_FULL_RUN_MIN: u32 = 12;
/// SheafStalk needs a *bundle* of tall columns — one or two is a vine stem.
const SHEAF_TALL_COLS_MIN: u32 = 4;
/// Fewer tall columns than this cannot be a sheaf at all: a lone stalk is
/// some other upright plant and fails closed.
const SHEAF_MIN_BUNDLE: u32 = 2;
const SHEAF_TALL_COL_WEIGHT: u32 = 10;
const SHEAF_TALL_COL_CAP: u32 = 6;

/// Rows `[0, UPPER_BAND)` form the top band, `[LOWER_BAND, 16)` the bottom.
const UPPER_BAND: usize = 6;
const LOWER_BAND: usize = 10;
/// A top-band row at least this wide, clearly wider than the bottom band,
/// reads as a grain head / leaf crown.
const SHEAF_TOP_ROW_MIN: u32 = 8;
const SHEAF_TOP_ROW_MARGIN: u32 = 2;

/// A connected component that is a small *compact blob* — bounded width and
/// height, mostly filled, centred above row 10 — is an isolated dot: the
/// fruit-pod signature of VineFruit. Elongated strokes (leaf blades) and
/// sparse crosses never qualify.
const DOT_MAX_PX: u32 = 12;
const DOT_MAX_DIM: u32 = 5;
const DOT_MAX_CENTRE_SUM: u32 = 20; // `min_y + max_y` keeps the centre above row 10
const DOT_MIN_DIM: u32 = 2;
const DOT_MIN_FILL_NUM: u32 = 2;
const DOT_MIN_FILL_DEN: u32 = 3;
const VINE_MIN_DOTS: u32 = 2;
const VINE_DOT_WEIGHT: u32 = 15;
const VINE_DOT_CAP: u32 = 4;
/// The vine body (largest component mass and height) must clear these
/// floors, or scattered dots are just noise.
const VINE_BODY_MIN_PX: u32 = 20;
const VINE_BODY_MIN_H: u32 = 10;
/// A single stem produces at most this many tall columns; more means a
/// sheaf-like stalk bundle.
const VINE_MAX_TALL_COLS: u32 = SHEAF_TALL_COLS_MIN - 1;
/// No wide head/crown rows on a vine.
const VINE_MAX_WIDEST_ROW: u32 = 9;

/// BulbLeaf: the solid ground mass makes the widest bottom-band row at
/// least this wide.
const BULB_BASE_ROW_MIN: u32 = 7;

/// Hue analysis: 12 chromatic 30-degree bins plus one achromatic bin.
const HUE_BINS: usize = 13;
const HUE_NEUTRAL: usize = 12;
/// "Foliage" hue family: bins 2..=5 cover hue 60..180 degrees
/// (yellow-green through cyan-green). Everything else chromatic is
/// candidate "produce" colour — gold heads, orange bulbs, red fruit.
const GREEN_HUE_LO: usize = 2;
const GREEN_HUE_HI: usize = 5;
/// The produce cluster must cover at least 1/16 of opaque texels, and at
/// least this many texels must remain outside it, for a two-tone read.
const PRODUCE_MIN_SHARE_DEN: u32 = 16;
const PRODUCE_MIN_OTHER_PX: u32 = 8;

/// Minimum raw evidence for an archetype to be considered at all.
const MIN_ARCHETYPE_SCORE: u32 = 45;
/// Minimum lead of the winner over the runner-up.
const MIN_ARCHETYPE_MARGIN: u32 = 10;

// ---------------------------------------------------------------------------
// Reason bits
// ---------------------------------------------------------------------------

/// The block carries a crop-family tag (`*:crops`, `*:crop`).
pub const REASON_CROP_TAG: u32 = 1 << 0;
/// The block carries a supporting tag (`maintains_farmland`, `bee_growables`).
pub const REASON_SUPPORT_TAG: u32 = 1 << 1;
/// The blockstate carries an `age` property.
pub const REASON_AGE_STATE: u32 = 1 << 2;
/// The model chain contains a crop/cross parent.
pub const REASON_CROSS_PARENT: u32 = 1 << 3;
/// A usable 16x16 sprite was analysed.
pub const REASON_TEXTURE: u32 = 1 << 4;
/// No sprite (or a short buffer) was supplied.
pub const REASON_NO_TEXTURE: u32 = 1 << 5;
/// The sprite was empty, solid, or too sparse/dense to analyse.
pub const REASON_TEXTURE_DEGENERATE: u32 = 1 << 6;
/// The sprite shows a real second colour cluster (two-tone foliage+produce).
pub const REASON_TWO_TONE: u32 = 1 << 7;
/// The produce colour concentrates in the upper half (heads/fruit).
pub const REASON_TOP_PRODUCE: u32 = 1 << 8;
/// The produce colour concentrates in the lower half (bulb/root).
pub const REASON_BOTTOM_PRODUCE: u32 = 1 << 9;
/// Several full-height column runs (a stalk bundle).
pub const REASON_TALL_COLUMNS: u32 = 1 << 10;
/// Isolated dot components in the upper region (fruit pods).
pub const REASON_DOT_ISOLATES: u32 = 1 << 11;
/// The widest row sits in the top band (crown/heads).
pub const REASON_WIDE_TOP: u32 = 1 << 12;
/// The widest row sits in the bottom band (ground mass).
pub const REASON_WIDE_BASE: u32 = 1 << 13;
/// The crop gate passed but no archetype cleared the confidence bar.
pub const REASON_LOW_CONFIDENCE: u32 = 1 << 14;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The three procedural crop silhouette families. Mirrors
/// [`Crop3dArchetype`] one-to-one so a decision maps straight onto the mesh
/// builder via [`From<CropArchetype> for Crop3dArchetype`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CropArchetype {
    /// Wheat/rice/corn style: a bundle of tall stalks crowned by grain
    /// heads in the upper rows.
    SheafStalk,
    /// Carrot/potato/beetroot style: a ground-level bulb or root mass plus
    /// a leaf rosette above it.
    BulbLeaf,
    /// Tomato/berry/pepper style: a supporting vine stem with foliage and
    /// small isolated fruit pods in the upper region.
    VineFruit,
}

impl CropArchetype {
    /// Stable machine-readable name for logs and pack UI badges.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SheafStalk => "sheaf_stalk",
            Self::BulbLeaf => "bulb_leaf",
            Self::VineFruit => "vine_fruit",
        }
    }
}

impl From<CropArchetype> for Crop3dArchetype {
    fn from(a: CropArchetype) -> Self {
        match a {
            CropArchetype::SheafStalk => Crop3dArchetype::SheafStalk,
            CropArchetype::BulbLeaf => Crop3dArchetype::BulbLeaf,
            CropArchetype::VineFruit => Crop3dArchetype::VineFruit,
        }
    }
}

/// Crop-family tag flags the caller resolved from the registry. Each flag
/// is also honoured when the same tag appears in [`CropInput::tags`]; the
/// classifier unions both sources so callers may supply either.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CropTagFlags {
    /// `minecraft:crops` is present.
    pub minecraft_crops: bool,
    /// `c:crops` (Forge/NeoForge convention) is present.
    pub c_crops: bool,
    /// `minecraft:maintains_farmland` is present.
    pub maintains_farmland: bool,
    /// `minecraft:bee_growables` is present.
    pub bee_growables: bool,
}

/// Caller-supplied facts about one candidate crop block. Plain borrowed
/// data — no globals, no I/O.
#[derive(Clone, Copy, Debug)]
pub struct CropInput<'a> {
    /// Namespaced block id, e.g. `"minecraft:wheat"` or `"mymod:ghost_crop"`.
    /// Retained for caller context; the decision itself is namespace- and
    /// name-agnostic by design.
    pub block_id: &'a str,
    /// Sorted `(key, value)` blockstate pairs, e.g.
    /// `[("age", "7"), ("facing", "north")]`. The growth property is `age`;
    /// callers that know the property's range may add a companion
    /// `("max_age", "<max>")` (alias `"age_max"`) pair so the decision can
    /// report `max_stage`.
    pub states: &'a [(&'a str, &'a str)],
    /// Registry block tags the caller resolved, e.g.
    /// `&["minecraft:crops"]`. Order does not matter — membership checks
    /// only, so shuffled input is bit-identical.
    pub tags: &'a [&'a str],
    /// Pre-resolved crop tag flags; unioned with [`CropInput::tags`].
    pub tag_flags: CropTagFlags,
    /// Resolved parent-model chain ids for the block's model, e.g.
    /// `&["minecraft:block/crop", "minecraft:block/block"]`. Any
    /// `block/crop`, `block/cross` or `block/tinted_cross` entry (any
    /// namespace) counts as a crop/cross parent.
    pub parents: &'a [&'a str],
    /// Optional 16x16 RGBA8 sprite — the current stage's texture, or the
    /// final stage's when the caller only resolved that frame (young-stage
    /// sprites are subsets of the mature silhouette). `None` or a short
    /// buffer simply yields `archetype = None`.
    pub rgba: Option<&'a [u8]>,
}

/// What the classifier decided for one [`CropInput`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CropDecision {
    /// Whether the crop gate passed (crop tags, `age` + crop/cross parent,
    /// or a corroborated support tag).
    pub is_crop: bool,
    /// The winning archetype, or `None` when evidence was missing or
    /// ambiguous — the caller should fall back to plain extrusion.
    pub archetype: Option<CropArchetype>,
    /// Evidence strength `0..=100`. When `archetype` is `Some` this is the
    /// confidence in that pick; when `None` it reports the strongest
    /// sub-threshold evidence seen (or `0` for non-crops / missing
    /// textures) so near-misses stay visible in logs.
    pub confidence: u8,
    /// Current growth stage parsed from the `age` blockstate (`0` when
    /// absent or unparsable).
    pub stage: u8,
    /// Growth maximum parsed from `max_age`/`age_max`, or `0` when the
    /// caller did not supply it — `0` means "unknown", matching the
    /// fail-closed `max_stage == 0` convention of [`Crop3dSpec`].
    ///
    /// [`Crop3dSpec`]: crate::vanilla_plus_3d::Crop3dSpec
    pub max_stage: u8,
    /// Bitmask of [`REASON_*`] constants explaining the decision.
    pub reasons: u32,
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// `(key, value)` lookup over the sorted blockstate pairs. Case-insensitive
/// on the key; first match wins (blockstate keys are unique).
fn state_value<'a>(states: &'a [(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    states
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| *v)
}

/// Tag path after the last `:` — `"minecraft:crops"`, `"c:crops"` and a bare
/// `"crops"` all reduce to `"crops"`, matching the cross-loader convention
/// used across this crate.
fn tag_path(tag: &str) -> &str {
    let t = tag.trim().trim_start_matches('#');
    t.rsplit(':').next().unwrap_or(t)
}

/// Strong crop-family tag: any namespace's `crop`/`crops`.
fn is_crop_tag_path(path: &str) -> bool {
    path.eq_ignore_ascii_case("crop") || path.eq_ignore_ascii_case("crops")
}

/// Supporting tag: farmland maintenance / bee-growable. Corroborating
/// evidence only — never sufficient alone.
fn is_support_tag_path(path: &str) -> bool {
    path.eq_ignore_ascii_case("maintains_farmland") || path.eq_ignore_ascii_case("bee_growables")
}

/// Whether a resolved parent-model id is one of the cross-plane crop
/// parents, in any namespace.
fn is_crop_parent(model: &str) -> bool {
    let p = model.rsplit(':').next().unwrap_or(model);
    matches!(p, "block/crop" | "block/cross" | "block/tinted_cross")
}

// ---------------------------------------------------------------------------
// Texture feature extraction (integer only)
// ---------------------------------------------------------------------------

/// Integer hue bin for one RGB texel: `0..=11` are 30-degree chromatic bins,
/// [`HUE_NEUTRAL`] is the achromatic bin (near-gray, near-black, near-white).
/// Same integer hexcone math as the ore classifier — no floats.
fn hue_bin(r: u8, g: u8, b: u8) -> usize {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let d = mx - mn;
    if d < 24 {
        return HUE_NEUTRAL;
    }
    let h60 = if mx == r {
        60 * (g - b) / d
    } else if mx == g {
        60 * (b - r) / d + 120
    } else {
        60 * (r - g) / d + 240
    };
    let h = h60.rem_euclid(360) as usize;
    (h / 30).min(11)
}

/// Whether a hue bin belongs to the foliage-green family (hue 60..180).
fn is_green_bin(bin: usize) -> bool {
    (GREEN_HUE_LO..=GREEN_HUE_HI).contains(&bin)
}

/// Integer-only summary of one 16x16 alpha mask. Every field is a count or
/// a comparison-ready integer — the decision path below contains no floats.
struct TexFeatures {
    /// Opaque texel count.
    count: u32,
    /// Bounding-box width/height/area in texels.
    bbox_w: u32,
    bbox_h: u32,
    bbox_area: u32,
    /// Widest single row overall, in the top band (rows 0..6) and bottom
    /// band (rows 10..16).
    widest_row: u32,
    widest_upper: u32,
    widest_lower: u32,
    /// Opaque mass in rows 0..8 vs 8..16.
    top_mass: u32,
    bot_mass: u32,
    /// Columns whose longest vertical opaque run reaches [`TALL_RUN_MIN`],
    /// plus the single longest run seen anywhere.
    tall_cols: u32,
    longest_run: u32,
    /// Connected-component statistics: largest component size, tallest
    /// component height, and the number of small upper-region isolated
    /// dots.
    largest_px: u32,
    tallest_h: u32,
    upper_dots: u32,
    /// Whether a real second ("produce") colour cluster exists.
    two_tone: bool,
    /// Produce-colour texels in the upper vs lower half (meaningful only
    /// when `two_tone`).
    produce_upper: u32,
    produce_lower: u32,
}

/// Reduces a 16x16 RGBA8 buffer to [`TexFeatures`]. Returns `None` when the
/// buffer is shorter than [`SPRITE_BYTES`]; callers treat that as "no
/// texture". All math is integer — identical results on every CPU.
fn analyze_texture(rgba: &[u8]) -> Option<TexFeatures> {
    let buf = rgba.get(..SPRITE_BYTES)?;

    let mut mask = [0u8; SPRITE_DIM * SPRITE_DIM];
    let mut count = 0u32;
    for (i, px) in buf.as_chunks::<4>().0.iter().enumerate() {
        if px[3] >= ALPHA_CUTOFF {
            mask[i] = 1;
            count += 1;
        }
    }

    // Bounding box + row profile.
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (16usize, 0usize, 16usize, 0usize);
    let mut row = [0u32; SPRITE_DIM];
    for y in 0..SPRITE_DIM {
        for x in 0..SPRITE_DIM {
            if mask[y * SPRITE_DIM + x] == 1 {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
                row[y] += 1;
            }
        }
    }
    let (bbox_w, bbox_h, bbox_area) = if count == 0 {
        (0, 0, 1)
    } else {
        (
            (max_x - min_x + 1) as u32,
            (max_y - min_y + 1) as u32,
            ((max_x - min_x + 1) * (max_y - min_y + 1)) as u32,
        )
    };
    let widest_row = row.iter().copied().max().unwrap_or(0);
    let widest_upper = row[..UPPER_BAND].iter().copied().max().unwrap_or(0);
    let widest_lower = row[LOWER_BAND..].iter().copied().max().unwrap_or(0);
    let top_mass = row[..8].iter().sum();
    let bot_mass = row[8..].iter().sum();

    // Column-run statistics.
    let (mut tall_cols, mut longest_run) = (0u32, 0u32);
    for x in 0..SPRITE_DIM {
        let (mut run, mut best) = (0u32, 0u32);
        for y in 0..SPRITE_DIM {
            if mask[y * SPRITE_DIM + x] == 1 {
                run += 1;
                best = best.max(run);
            } else {
                run = 0;
            }
        }
        longest_run = longest_run.max(best);
        if best >= TALL_RUN_MIN {
            tall_cols += 1;
        }
    }

    // 4-connected component statistics via flood fill.
    let mut seen = [0u8; SPRITE_DIM * SPRITE_DIM];
    let mut stack = [0u16; SPRITE_DIM * SPRITE_DIM];
    let (mut largest_px, mut tallest_h, mut upper_dots) = (0u32, 0u32, 0u32);
    for start in 0..SPRITE_DIM * SPRITE_DIM {
        if mask[start] == 0 || seen[start] == 1 {
            continue;
        }
        let mut sp = 0usize;
        stack[sp] = start as u16;
        sp += 1;
        seen[start] = 1;
        let (mut size, mut cmin_x, mut cmax_x, mut cmin_y, mut cmax_y) =
            (0u32, SPRITE_DIM, 0usize, SPRITE_DIM, 0usize);
        while sp > 0 {
            sp -= 1;
            let i = stack[sp] as usize;
            let (x, y) = (i % SPRITE_DIM, i / SPRITE_DIM);
            size += 1;
            cmin_x = cmin_x.min(x);
            cmax_x = cmax_x.max(x);
            cmin_y = cmin_y.min(y);
            cmax_y = cmax_y.max(y);
            // 4-neighbours with bounds checks.
            if x > 0 {
                let n = i - 1;
                if mask[n] == 1 && seen[n] == 0 {
                    seen[n] = 1;
                    stack[sp] = n as u16;
                    sp += 1;
                }
            }
            if x + 1 < SPRITE_DIM {
                let n = i + 1;
                if mask[n] == 1 && seen[n] == 0 {
                    seen[n] = 1;
                    stack[sp] = n as u16;
                    sp += 1;
                }
            }
            if y > 0 {
                let n = i - SPRITE_DIM;
                if mask[n] == 1 && seen[n] == 0 {
                    seen[n] = 1;
                    stack[sp] = n as u16;
                    sp += 1;
                }
            }
            if y + 1 < SPRITE_DIM {
                let n = i + SPRITE_DIM;
                if mask[n] == 1 && seen[n] == 0 {
                    seen[n] = 1;
                    stack[sp] = n as u16;
                    sp += 1;
                }
            }
        }
        largest_px = largest_px.max(size);
        tallest_h = tallest_h.max((cmax_y - cmin_y + 1) as u32);
        let (cw, ch) = ((cmax_x - cmin_x + 1) as u32, (cmax_y - cmin_y + 1) as u32);
        if size <= DOT_MAX_PX
            && cw >= DOT_MIN_DIM
            && ch >= DOT_MIN_DIM
            && cw <= DOT_MAX_DIM
            && ch <= DOT_MAX_DIM
            && size * DOT_MIN_FILL_DEN >= cw * ch * DOT_MIN_FILL_NUM
            && (cmin_y + cmax_y) < DOT_MAX_CENTRE_SUM as usize
        {
            upper_dots += 1;
        }
    }

    // Hue clusters: bin every opaque texel, then take the strongest
    // non-foliage ("produce") chromatic cluster as the accent.
    let mut bins = [0u8; SPRITE_DIM * SPRITE_DIM];
    let mut hist = [0u32; HUE_BINS];
    for (i, m) in mask.iter().enumerate() {
        if *m == 1 {
            let px = &buf[i * 4..i * 4 + 4];
            let b = hue_bin(px[0], px[1], px[2]);
            bins[i] = b as u8;
            hist[b] += 1;
        }
    }
    let mut produce_bin = usize::MAX;
    let mut produce_count = 0u32;
    for (b, &c) in hist.iter().enumerate() {
        if is_green_bin(b) || b == HUE_NEUTRAL {
            continue;
        }
        if c > produce_count {
            produce_count = c;
            produce_bin = b;
        }
    }
    let other_count = count.saturating_sub(produce_count);
    let two_tone = produce_bin != usize::MAX
        && produce_count * PRODUCE_MIN_SHARE_DEN >= count
        && other_count >= PRODUCE_MIN_OTHER_PX;
    let (mut produce_upper, mut produce_lower) = (0u32, 0u32);
    if two_tone {
        for (i, m) in mask.iter().enumerate() {
            if *m == 1 && bins[i] as usize == produce_bin {
                if i / SPRITE_DIM < 8 {
                    produce_upper += 1;
                } else {
                    produce_lower += 1;
                }
            }
        }
    }

    Some(TexFeatures {
        count,
        bbox_w,
        bbox_h,
        bbox_area,
        widest_row,
        widest_upper,
        widest_lower,
        top_mass,
        bot_mass,
        tall_cols,
        longest_run,
        largest_px,
        tallest_h,
        upper_dots,
        two_tone,
        produce_upper,
        produce_lower,
    })
}

/// Whether the analysed sprite carries enough structured alpha to score.
/// Empty, solid, near-solid and salt-and-pepper sprites all fail closed.
fn texture_usable(f: &TexFeatures) -> bool {
    f.count >= MIN_OPAQUE_PX
        && f.count <= MAX_OPAQUE_PX
        && f.count * FILL_DEN <= f.bbox_area * FILL_NUM
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Evidence scores `[sheaf, bulb, vine]` — every term is an integer
/// comparison against a named threshold, so the argmax is total and
/// deterministic (ties resolve to the lowest archetype index).
fn score_archetypes(f: &TexFeatures) -> [u32; 3] {
    // SheafStalk: a bundle of tall stalk columns crowned by wide head rows,
    // with the produce colour (ripe heads) above the foliage. Fewer than
    // [`SHEAF_MIN_BUNDLE`] tall columns is a lone stalk — not a sheaf.
    let mut sheaf = 0;
    if f.tall_cols >= SHEAF_MIN_BUNDLE {
        sheaf = SHEAF_TALL_COL_WEIGHT * f.tall_cols.min(SHEAF_TALL_COL_CAP);
        if f.longest_run >= SHEAF_FULL_RUN_MIN {
            sheaf += 5;
        }
        if f.widest_upper >= SHEAF_TOP_ROW_MIN
            && f.widest_upper > f.widest_lower + SHEAF_TOP_ROW_MARGIN
        {
            sheaf += 25;
        }
        if f.two_tone && f.produce_upper > f.produce_lower {
            sheaf += 20;
        }
        if f.bbox_h > f.bbox_w {
            sheaf += 10;
        }
    }

    // BulbLeaf: produce colour (the bulb/root) below the foliage rosette,
    // a solid wide base, no stalk bundle.
    let mut bulb = 0;
    if f.two_tone && f.produce_lower > f.produce_upper {
        bulb += 35;
    }
    if f.widest_lower >= BULB_BASE_ROW_MIN {
        bulb += 15;
    }
    if f.tall_cols <= VINE_MAX_TALL_COLS {
        bulb += 15;
    }
    if f.top_mass >= f.bot_mass {
        bulb += 10;
    }
    if f.widest_upper <= f.widest_lower {
        bulb += 10;
    }

    // VineFruit: gated on a real foliage/stem body *and* at least
    // [`VINE_MIN_DOTS`] isolated upper dots — the fruit pods are the
    // archetype's discriminating signature, so a dotless stem fails closed
    // rather than guessing at every upright plant.
    let mut vine = 0;
    let body_ok = f.largest_px >= VINE_BODY_MIN_PX && f.tallest_h >= VINE_BODY_MIN_H;
    if body_ok && f.upper_dots >= VINE_MIN_DOTS {
        vine += 25;
        vine += VINE_DOT_WEIGHT * f.upper_dots.min(VINE_DOT_CAP);
        if f.tall_cols <= VINE_MAX_TALL_COLS {
            vine += 10;
        }
        if f.widest_row <= VINE_MAX_WIDEST_ROW {
            vine += 10;
        }
        if f.two_tone && f.produce_upper >= f.produce_lower {
            vine += 10;
        }
        if f.tall_cols >= SHEAF_TALL_COLS_MIN {
            vine = vine.saturating_sub(30);
        }
    }

    [sheaf, bulb, vine]
}

const ARCHETYPES: [CropArchetype; 3] = [
    CropArchetype::SheafStalk,
    CropArchetype::BulbLeaf,
    CropArchetype::VineFruit,
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Classifies one crop block candidate. Pure function of `input` — same
/// input, same [`CropDecision`], on every platform and every run.
pub fn classify(input: &CropInput) -> CropDecision {
    // Stage pass-through: `age` is the growth property; `max_age`/`age_max`
    // carry its range when the caller knows it. Absent max reports 0
    // ("unknown"), matching the fail-closed `max_stage == 0` convention.
    let stage = state_value(input.states, "age")
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(0);
    let max_stage = ["max_age", "age_max"]
        .iter()
        .find_map(|k| state_value(input.states, k))
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(0);

    // --- Step 1: crop gate -------------------------------------------------
    let strong_tag = input.tag_flags.minecraft_crops
        || input.tag_flags.c_crops
        || input.tags.iter().any(|t| is_crop_tag_path(tag_path(t)));
    let support_tag = input.tag_flags.maintains_farmland
        || input.tag_flags.bee_growables
        || input.tags.iter().any(|t| is_support_tag_path(tag_path(t)));
    let has_age = input
        .states
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("age"));
    let has_parent = input.parents.iter().any(|p| is_crop_parent(p));

    let is_crop = strong_tag || (has_age && has_parent) || (support_tag && (has_age || has_parent));

    let mut reasons = 0u32;
    if strong_tag {
        reasons |= REASON_CROP_TAG;
    }
    if support_tag {
        reasons |= REASON_SUPPORT_TAG;
    }
    if has_age {
        reasons |= REASON_AGE_STATE;
    }
    if has_parent {
        reasons |= REASON_CROSS_PARENT;
    }

    // --- Step 2+3: archetype + confidence ---------------------------------
    let (archetype, confidence) = if !is_crop {
        (None, 0)
    } else {
        match input.rgba.and_then(analyze_texture) {
            Some(f) if texture_usable(&f) => {
                reasons |= REASON_TEXTURE;
                if f.two_tone {
                    reasons |= REASON_TWO_TONE;
                    if f.produce_upper > f.produce_lower {
                        reasons |= REASON_TOP_PRODUCE;
                    } else if f.produce_lower > f.produce_upper {
                        reasons |= REASON_BOTTOM_PRODUCE;
                    }
                }
                if f.tall_cols >= SHEAF_TALL_COLS_MIN {
                    reasons |= REASON_TALL_COLUMNS;
                }
                if f.upper_dots >= VINE_MIN_DOTS {
                    reasons |= REASON_DOT_ISOLATES;
                }
                if f.widest_upper >= SHEAF_TOP_ROW_MIN
                    && f.widest_upper > f.widest_lower + SHEAF_TOP_ROW_MARGIN
                {
                    reasons |= REASON_WIDE_TOP;
                }
                if f.widest_lower >= BULB_BASE_ROW_MIN {
                    reasons |= REASON_WIDE_BASE;
                }

                let scores = score_archetypes(&f);
                let mut best_i = 0usize;
                let mut best = scores[0];
                for (i, &s) in scores.iter().enumerate().skip(1) {
                    if s > best {
                        best = s;
                        best_i = i;
                    }
                }
                let second = scores
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != best_i)
                    .map(|(_, s)| *s)
                    .max()
                    .unwrap_or(0);
                let margin = best - second;
                let confidence = (best + margin / 2).min(100) as u8;
                if best >= MIN_ARCHETYPE_SCORE && margin >= MIN_ARCHETYPE_MARGIN {
                    (Some(ARCHETYPES[best_i]), confidence)
                } else {
                    reasons |= REASON_LOW_CONFIDENCE;
                    (None, confidence)
                }
            }
            Some(_) => {
                reasons |= REASON_TEXTURE_DEGENERATE;
                (None, 0)
            }
            None => {
                reasons |= REASON_NO_TEXTURE;
                (None, 0)
            }
        }
    };

    CropDecision {
        is_crop,
        archetype,
        confidence,
        stage,
        max_stage,
        reasons,
    }
}

/// FNV-1a 64-bit fingerprint of a [`CropDecision`] — feeds every field in a
/// fixed order so identical decisions hash identically and any drift in
/// gate, archetype, confidence, stage or reasons flips the value.
pub fn decision_fingerprint(d: &CropDecision) -> u64 {
    let mut buf = [0u8; 9];
    buf[..4].copy_from_slice(&[d.is_crop as u8, d.confidence, d.stage, d.max_stage]);
    buf[4] = d.archetype.map_or(0, |a| a as u8 + 1);
    buf[5..].copy_from_slice(&d.reasons.to_le_bytes());
    megu_core::perf_kit::fnv1a64(&buf)
}
