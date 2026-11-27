//! Local-Minecraft font discovery for the authentic look: reads
//! `assets/minecraft/textures/font/ascii.png` (the vanilla 8×8 pixel
//! font) and the Unifont `.hex` packs (`unifont_all_no_pua`,
//! `unifont_jp_patch`) out of an installed 1.21.1 — entirely in memory,
//! nothing extracted to disk, nothing Mojang-copyrighted bundled with
//! Megu. Every failure degrades to "no external font", so offline hosts
//! and CI never notice the path exists.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use megu_client_mesh::png_decode::decode_png_rgba;
use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};
use megu_ui::font_cjk::{GlyphAtlas, GlyphBitmap};

use crate::live_session::SessionPaths;
use crate::vanilla_bootstrap::{
    default_vanilla_cache_dir, read_asset_object_store, AssetPackChain, AssetPackSource,
    VanillaAssetResolver,
};
use crate::vanilla_jar::{read_bounded_file, read_selected_entries, AssetLayer};

const FONT_READY_ASCII: u8 = 1;
const FONT_READY_UNIFONT: u8 = 2;
static FONT_READY: AtomicU8 = AtomicU8::new(0);
static FONT_REVISION_COUNTER: AtomicU64 = AtomicU64::new(0);

fn record_font_component(component: u8) {
    FONT_READY.fetch_or(component, Ordering::AcqRel);
    FONT_REVISION_COUNTER.fetch_add(1, Ordering::AcqRel);
}

/// Monotonic revision of the external font set. Every completed ASCII or
/// Unifont load/update advances this counter, so mesh caches can compare one
/// integer instead of hashing glyph data.
pub fn font_revision() -> u64 {
    FONT_REVISION_COUNTER.load(Ordering::Acquire)
}

/// Whether any vanilla `ascii.png` table has completed loading in this process.
pub fn ascii_loaded() -> bool {
    FONT_READY.load(Ordering::Acquire) & FONT_READY_ASCII != 0
}

/// Whether any Unifont pack has completed loading in this process.
pub fn unifont_loaded() -> bool {
    FONT_READY.load(Ordering::Acquire) & FONT_READY_UNIFONT != 0
}

/// Detailed result of one local-font probe. ASCII and Unifont readiness are
/// deliberately independent: a JAR may provide `ascii.png` before the assets
/// index/objects for Unifont have finished downloading, and that partial load
/// must not masquerade as CJK readiness.
#[derive(Clone, Debug)]
pub struct VanillaFontLoad {
    /// Merged glyphs ready for `install_external_font`.
    pub atlas: GlyphAtlas,
    /// Vanilla `ascii.png` table loaded on this probe.
    pub ascii_loaded: bool,
    /// At least one Unifont `.hex` pack loaded on this probe.
    pub unifont_loaded: bool,
    /// Global font revision after this probe's component updates.
    pub font_revision: u64,
}

impl VanillaFontLoad {
    /// ASCII readiness for this probe (independent of Unifont).
    pub fn ascii_ready(&self) -> bool {
        self.ascii_loaded
    }

    /// Unifont/CJK readiness for this probe (independent of ASCII).
    pub fn unifont_ready(&self) -> bool {
        self.unifont_loaded
    }

    /// Revision observed after the probe recorded its component updates.
    pub fn revision(&self) -> u64 {
        self.font_revision
    }

    /// Move the merged glyph atlas out for installation.
    pub fn into_atlas(self) -> GlyphAtlas {
        self.atlas
    }
}

/// JAR member holding the vanilla 8×8 ASCII bitmap font.
const ASCII_PNG_ENTRY: &str = "assets/minecraft/textures/font/ascii.png";
/// Bound on an `indexes/*.json` asset index read — real indexes are a
/// few hundred KB; anything larger is not a font manifest.
const MAX_INDEX_JSON_BYTES: u64 = 32 * 1024 * 1024;
/// Bound on a single `.hex` object read — the real Unifont packs are
/// well under 8 MiB (which `GlyphAtlas::load_unifont_hex` re-checks).
const MAX_HEX_OBJECT_BYTES: usize = 16 * 1024 * 1024;
const MAX_INDEX_CANDIDATES: usize = 64;

const MAX_FONT_JSON_BYTES: usize = 256 * 1024;
const MAX_FONT_PROVIDER_DEPTH: usize = 8;
const FONT_JSON_LIMITS: JsonLimits = JsonLimits {
    max_depth: 24,
    max_nodes: 32_768,
    max_bytes: MAX_FONT_JSON_BYTES,
};

fn exact_i32(value: &JsonValue) -> Option<i32> {
    let n = value.as_f64()?;
    (n.is_finite() && n.fract() == 0.0 && n >= i32::MIN as f64 && n <= i32::MAX as f64)
        .then_some(n as i32)
}

fn exact_f32(value: &JsonValue) -> Option<f32> {
    let n = value.as_f64()?;
    (n.is_finite() && (-3.4e38..=3.4e38).contains(&n)).then_some(n as f32)
}

fn valid_resource_name(name: &str) -> bool {
    let (ns, path) = name.split_once(':').unwrap_or(("minecraft", name));
    !ns.is_empty()
        && !path.is_empty()
        && ns.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.'))
        && path.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.' | b'/'))
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

fn resource_parts(name: &str) -> Option<(&str, &str)> {
    valid_resource_name(name).then(|| name.split_once(':').unwrap_or(("minecraft", name)))
}

fn texture_logical_path(file: &str) -> Option<String> {
    let (ns, path) = resource_parts(file)?;
    Some(format!("{ns}/textures/{path}"))
}

fn direct_logical_path(file: &str) -> Option<String> {
    let (ns, path) = resource_parts(file)?;
    Some(format!("{ns}/{path}"))
}

fn font_reference_logical(id: &str) -> Option<String> {
    let (ns, path) = resource_parts(id)?;
    Some(format!("{ns}/font/{path}.json"))
}

/// One parsed `size_overrides` entry from a `unihex` provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnihexSizeOverride {
    pub from: u32,
    pub to: u32,
    pub left: i32,
    pub right: i32,
}

/// Vanilla font-provider definition after JSON parsing. `Reference` is
/// retained by the raw parser and flattened by [`load_vanilla_font_definition`].
#[derive(Clone, Debug, PartialEq)]
pub enum VanillaFontProviderKind {
    Bitmap { file: String, height: i32, ascent: i32, chars: Vec<String> },
    Space { advances: BTreeMap<u32, f32> },
    Unihex { hex_file: String, size_overrides: Vec<UnihexSizeOverride> },
    Reference { id: String },
}

/// One provider plus Vanilla's font-option filter map.
#[derive(Clone, Debug, PartialEq)]
pub struct VanillaFontProvider {
    pub kind: VanillaFontProviderKind,
    pub filter: BTreeMap<String, bool>,
}

/// Parsed `assets/<ns>/font/<name>.json` document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VanillaFontDefinition {
    pub providers: Vec<VanillaFontProvider>,
}

// Vanilla accepts trailing commas in font JSON. Remove commas only when the
// next non-whitespace byte closes a container and the comma is outside quotes.
fn normalize_font_json(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut quoted = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if quoted {
            if escaped { escaped = false; }
            else if b == b'\\' { escaped = true; }
            else if b == b'"' { quoted = false; }
        } else if b == b'"' {
            quoted = true;
        } else if b == b',' && matches!(bytes[i + 1..].iter().copied().find(|b| !b.is_ascii_whitespace()), Some(b']' | b'}')) {
            continue;
        }
        out.push(b);
    }
    Some(out)
}

fn parse_font_json(bytes: &[u8]) -> Result<JsonValue, String> {
    let normalized = normalize_font_json(bytes).ok_or_else(|| "font JSON normalization failed".to_owned())?;
    let text = std::str::from_utf8(&normalized).map_err(|_| "font JSON is not UTF-8".to_owned())?;
    parse_json_with_limits(text, FONT_JSON_LIMITS, true).map_err(|e| e.to_string())
}

fn parse_codepoint(value: &JsonValue) -> Option<u32> {
    if let Some(text) = value.as_str() {
        let mut chars = text.chars();
        let cp = chars.next()?;
        return (chars.next().is_none()).then_some(cp as u32);
    }
    let n = exact_i32(value)?;
    (0..=0x10ffff).contains(&n).then_some(n as u32)
}

fn parse_filter(provider: &JsonValue) -> Option<BTreeMap<String, bool>> {
    let mut filter = BTreeMap::new();
    if let Some(object) = provider.get("filter").and_then(JsonValue::as_object) {
        for (key, value) in object {
            filter.insert(key.clone(), value.as_bool()?);
        }
    }
    Some(filter)
}

fn parse_font_provider(value: &JsonValue) -> Option<VanillaFontProvider> {
    let ty = value.get("type").and_then(JsonValue::as_str)?;
    let filter = parse_filter(value)?;
    let kind = match ty {
        "bitmap" => {
            let file = value.get("file").and_then(JsonValue::as_str)?.to_owned();
            let height = value.get("height").and_then(exact_i32).unwrap_or(8);
            let ascent = value.get("ascent").and_then(exact_i32)?;
            if ascent > height { return None; }
            let chars = value.get("chars").and_then(JsonValue::as_array)?;
            let chars: Option<Vec<String>> = chars.iter().map(|row| row.as_str().map(str::to_owned)).collect();
            let chars = chars?;
            if chars.is_empty() || chars.iter().any(|row| row.chars().count() != chars[0].chars().count()) {
                return None;
            }
            VanillaFontProviderKind::Bitmap { file, height, ascent, chars }
        }
        "space" => {
            let mut advances = BTreeMap::new();
            for (key, value) in value.get("advances").and_then(JsonValue::as_object)? {
                let mut chars = key.chars();
                let cp = chars.next()? as u32;
                if chars.next().is_some() { return None; }
                advances.insert(cp, exact_f32(value)?);
            }
            VanillaFontProviderKind::Space { advances }
        }
        "unihex" => {
            let hex_file = value.get("hex_file").and_then(JsonValue::as_str)?.to_owned();
            let mut size_overrides = Vec::new();
            if let Some(items) = value.get("size_overrides").and_then(JsonValue::as_array) {
                for item in items {
                    let from = parse_codepoint(item.get("from")?)?;
                    let to = parse_codepoint(item.get("to")?)?;
                    if from >= to { return None; }
                    size_overrides.push(UnihexSizeOverride {
                        from,
                        to,
                        left: exact_i32(item.get("left")?)?,
                        right: exact_i32(item.get("right")?)?,
                    });
                }
            }
            VanillaFontProviderKind::Unihex { hex_file, size_overrides }
        }
        "reference" => VanillaFontProviderKind::Reference {
            id: value.get("id").and_then(JsonValue::as_str)?.to_owned(),
        },
        _ => return None,
    };
    Some(VanillaFontProvider { kind, filter })
}

/// Parse a vanilla font JSON document without resolving `reference` providers.
pub fn parse_vanilla_font_definition(bytes: &[u8]) -> Result<VanillaFontDefinition, String> {
    let root = parse_font_json(bytes)?;
    let providers = root.get("providers").and_then(JsonValue::as_array).ok_or_else(|| "font JSON has no providers array".to_owned())?;
    let mut out = Vec::new();
    for provider in providers {
        if let Some(provider) = parse_font_provider(provider) {
            out.push(provider);
        }
    }
    Ok(VanillaFontDefinition { providers: out })
}

fn font_document(chain: &AssetPackChain, logical_path: &str) -> Option<Vec<u8>> {
    let primary = chain.read_asset(logical_path, MAX_FONT_JSON_BYTES)?;
    let primary_empty = parse_vanilla_font_definition(&primary).is_ok_and(|def| def.providers.is_empty());
    if !primary_empty {
        return Some(primary);
    }
    // The official JAR carries an empty include/unifont.json placeholder while
    // the large downloadable object carries the real provider list. Only an
    // empty primary falls through; non-empty JAR/pack documents always win.
    for source in chain.sources_high_to_low() {
        if let AssetPackSource::AssetObjectStore { root, index_id } = source {
            if let Some(bytes) = read_asset_object_store(root, index_id, logical_path, MAX_FONT_JSON_BYTES) {
                if parse_vanilla_font_definition(&bytes).is_ok_and(|def| !def.providers.is_empty()) {
                    return Some(bytes);
                }
            }
        }
    }
    Some(primary)
}

/// Vanilla font-option filter evaluation. Only the two keys Vanilla knows
/// (`uniform` = Force Unicode Font, `jp` = Japanese glyph variants) can
/// match; a provider carrying an unknown key never activates, matching the
/// runtime option map's closed key set. The defaults are the vanilla
/// defaults: unicode-forcing off, Japanese variants off.
pub fn font_provider_enabled(
    filter: &BTreeMap<String, bool>,
    force_uniform: bool,
    japanese_variants: bool,
) -> bool {
    filter.iter().all(|(key, want)| match key.as_str() {
        "uniform" => force_uniform == *want,
        "jp" => japanese_variants == *want,
        _ => false,
    })
}

fn flatten_font_document(
    chain: &AssetPackChain,
    logical_path: &str,
    visiting: &mut Vec<String>,
    out: &mut Vec<VanillaFontProvider>,
) {
    if visiting.len() >= MAX_FONT_PROVIDER_DEPTH || visiting.iter().any(|p| p == logical_path) {
        return;
    }
    let Some(bytes) = font_document(chain, logical_path) else { return; };
    let Ok(definition) = parse_vanilla_font_definition(&bytes) else { return; };
    visiting.push(logical_path.to_owned());
    for provider in definition.providers {
        match &provider.kind {
            VanillaFontProviderKind::Reference { id } => {
                // The reference's own filter gates the entire document it
                // pulls in (vanilla uses this for the force-unicode switch).
                if font_provider_enabled(&provider.filter, false, false) {
                    if let Some(reference) = font_reference_logical(id) {
                        flatten_font_document(chain, &reference, visiting, out);
                    }
                }
            }
            // Vanilla default font options: force-unicode off, jp off.
            _ if font_provider_enabled(&provider.filter, false, false) => out.push(provider),
            _ => {}
        }
    }
    visiting.pop();
}

/// Load `minecraft/font/default.json` through the authoritative asset chain,
/// recursively flattening Vanilla `reference` providers with cycle/depth caps.
pub fn load_vanilla_font_definition(chain: &AssetPackChain) -> Option<VanillaFontDefinition> {
    let mut providers = Vec::new();
    flatten_font_document(chain, "minecraft/font/default.json", &mut Vec::new(), &mut providers);
    (!providers.is_empty()).then_some(VanillaFontDefinition { providers })
}

/// Which provider family supplied one glyph metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VanillaFontMetricProvider { Bitmap, Space, Unihex }

/// Vanilla-exact glyph metric. Bitmap advances implement the 1.21.1
/// `0.5 + ink_width * scale` truncation plus one pixel; unihex implements
/// `(right-left+1)/2+1`; space uses the provider's float verbatim.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VanillaGlyphMetric {
    pub codepoint: u32,
    pub advance: f32,
    pub ascent: Option<i32>,
    pub ink_box: Option<(i32, i32)>,
    pub provider: VanillaFontMetricProvider,
}

/// First-provider-wins glyph metric table for one flattened font definition.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VanillaFontMetrics {
    pub glyphs: BTreeMap<u32, VanillaGlyphMetric>,
}

impl VanillaFontMetrics {
    pub fn glyph(&self, c: char) -> Option<&VanillaGlyphMetric> { self.glyphs.get(&(c as u32)) }
    pub fn advance(&self, c: char) -> Option<f32> { self.glyph(c).map(|g| g.advance) }
}

fn bitmap_ink_width(rgba: &[u8], image_w: u32, cell_w: u32, cell_h: u32, col: u32, row: u32) -> u32 {
    for x in (0..cell_w).rev() {
        for y in 0..cell_h {
            let px = col * cell_w + x;
            let py = row * cell_h + y;
            let offset = (py * image_w + px) as usize * 4 + 3;
            if rgba.get(offset).copied().unwrap_or(0) != 0 {
                return x + 1;
            }
        }
    }
    0
}

fn insert_metric(metrics: &mut VanillaFontMetrics, metric: VanillaGlyphMetric) {
    metrics.glyphs.entry(metric.codepoint).or_insert(metric);
}

fn bitmap_metrics(
    metrics: &mut VanillaFontMetrics,
    file: &str,
    height: i32,
    ascent: i32,
    chars: &[String],
    png: &[u8],
) {
    let Some(logical) = texture_logical_path(file) else { return; };
    let _ = logical;
    let Ok((image_w, image_h, rgba)) = decode_png_rgba(png) else { return; };
    let rows = chars.len() as u32;
    let cols = chars[0].chars().count() as u32;
    if rows == 0 || cols == 0 { return; }
    let cell_w = image_w / cols;
    let cell_h = image_h / rows;
    if cell_w == 0 || cell_h == 0 { return; }
    let scale = height as f32 / cell_h as f32;
    for (row, text) in chars.iter().enumerate() {
        for (col, cp) in text.chars().enumerate() {
            let cp = cp as u32;
            if cp == 0 { continue; }
            let ink = bitmap_ink_width(&rgba, image_w, cell_w, cell_h, col as u32, row as u32);
            let advance = ((0.5f32 + ink as f32 * scale) as i32 + 1) as f32;
            insert_metric(metrics, VanillaGlyphMetric {
                codepoint: cp,
                advance,
                ascent: Some(ascent),
                ink_box: Some((0, ink as i32)),
                provider: VanillaFontMetricProvider::Bitmap,
            });
        }
    }
}

fn hex_nibble(b: u8) -> Option<u32> {
    match b {
        b'0'..=b'9' => Some(u32::from(b - b'0')),
        b'a'..=b'f' => Some(u32::from(b - b'a' + 10)),
        b'A'..=b'F' => Some(u32::from(b - b'A' + 10)),
        _ => None,
    }
}

fn parse_unihex_line(line: &str) -> Option<(u32, i32, i32)> {
    let (cp_text, bits) = line.trim().split_once(':')?;
    if cp_text.len() < 4 || cp_text.len() > 6 { return None; }
    let cp = u32::from_str_radix(cp_text, 16).ok()?;
    let width = match bits.len() { 32 => 8, 64 => 16, 96 => 24, 128 => 32, _ => return None };
    let digits_per_row = bits.len() / 16;
    let mut combined = 0u32;
    for row in 0..16 {
        let mut bits32 = 0u32;
        for index in 0..digits_per_row {
            bits32 = (bits32 << 4) | hex_nibble(bits.as_bytes()[row * digits_per_row + index])?;
        }
        combined |= bits32 << (32 - width);
    }
    let (left, right) = if combined == 0 {
        (0, width)
    } else {
        (combined.leading_zeros() as i32, (32 - combined.trailing_zeros() - 1) as i32)
    };
    Some((cp, left, right))
}

fn unihex_advance(left: i32, right: i32) -> f32 {
    ((right - left + 1) / 2 + 1) as f32
}

fn unihex_metrics(metrics: &mut VanillaFontMetrics, zip_bytes: &[u8], overrides: &[UnihexSizeOverride]) {
    let Ok(mut layer) = AssetLayer::from_bytes(zip_bytes, false, |name| name.ends_with(".hex")) else { return; };
    let names = layer.names().to_vec();
    let mut bounds = BTreeMap::<u32, (i32, i32)>::new();
    for name in names {
        let Ok(bytes) = layer.read(&name, MAX_HEX_OBJECT_BYTES) else { continue; };
        let Ok(text) = String::from_utf8(bytes) else { continue; };
        for line in text.lines() {
            if line.trim().is_empty() { continue; }
            if let Some((cp, left, right)) = parse_unihex_line(line) {
                bounds.insert(cp, (left, right));
            }
        }
    }
    for (cp, (mut left, mut right)) in bounds {
        if let Some(override_range) = overrides.iter().find(|r| cp >= r.from && cp <= r.to) {
            left = override_range.left;
            right = override_range.right;
        }
        insert_metric(metrics, VanillaGlyphMetric {
            codepoint: cp,
            advance: unihex_advance(left, right),
            ascent: None,
            ink_box: Some((left, right)),
            provider: VanillaFontMetricProvider::Unihex,
        });
    }
}

/// Compute the exact metric table for a flattened font definition through
/// the same asset chain. Earlier providers win per codepoint, like Vanilla.
pub fn compute_vanilla_font_metrics(definition: &VanillaFontDefinition, chain: &AssetPackChain) -> VanillaFontMetrics {
    let mut metrics = VanillaFontMetrics::default();
    for provider in &definition.providers {
        match &provider.kind {
            VanillaFontProviderKind::Bitmap { file, height, ascent, chars } => {
                if let Some(logical) = texture_logical_path(file) {
                    if let Some(bytes) = chain.read_asset(&logical, 32 * 1024 * 1024) {
                        bitmap_metrics(&mut metrics, file, *height, *ascent, chars, &bytes);
                    }
                }
            }
            VanillaFontProviderKind::Space { advances } => {
                for (&cp, &advance) in advances {
                    insert_metric(&mut metrics, VanillaGlyphMetric {
                        codepoint: cp,
                        advance,
                        ascent: None,
                        ink_box: None,
                        provider: VanillaFontMetricProvider::Space,
                    });
                }
            }
            VanillaFontProviderKind::Unihex { hex_file, size_overrides } => {
                if let Some(logical) = direct_logical_path(hex_file) {
                    if let Some(bytes) = chain.read_asset(&logical, 32 * 1024 * 1024) {
                        unihex_metrics(&mut metrics, &bytes, size_overrides);
                    }
                }
            }
            VanillaFontProviderKind::Reference { .. } => {}
        }
    }
    metrics
}

/// Vanilla's default text shadow displacement at scale 1: one logical pixel
/// down and right.
pub const VANILLA_TEXT_SHADOW_OFFSET: [i32; 2] = [1, 1];

/// Vanilla 25%-brightness shadow color, implemented with the exact packed
/// channel mask (`(rgb & 0xfcfcfc) >> 2`) rather than rounded division.
pub fn vanilla_text_shadow_color(rgb: [u8; 3]) -> [u8; 3] {
    [(rgb[0] & 0xfc) >> 2, (rgb[1] & 0xfc) >> 2, (rgb[2] & 0xfc) >> 2]
}

/// Mutable legacy-formatting state produced by `§` codes. Color resets all
/// styles; `§r` resets color and styles together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VanillaTextStyle {
    pub color: Option<[u8; 3]>,
    pub bold: bool,
    pub italic: bool,
    pub underlined: bool,
    pub strikethrough: bool,
    pub obfuscated: bool,
}

/// One plain-text span and the style active while rendering it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VanillaFormattedSpan {
    pub text: String,
    pub style: VanillaTextStyle,
}

const LEGACY_COLORS: [u32; 16] = [
    0x000000, 0x0000aa, 0x00aa00, 0x00aaaa, 0xaa0000, 0xaa00aa, 0xffaa00, 0xaaaaaa,
    0x555555, 0x5555ff, 0x55ff55, 0x55ffff, 0xff5555, 0xff55ff, 0xffff55, 0xffffff,
];

fn rgb24(rgb: u32) -> [u8; 3] { [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8] }

/// Parse Vanilla legacy `§` formatting into styled spans. Recognized color
/// codes clear every active style, style codes accumulate, `§r` resets, and
/// unrecognized pairs remain literal text.
pub fn parse_legacy_formatting(text: &str) -> Vec<VanillaFormattedSpan> {
    let mut spans = Vec::new();
    let mut style = VanillaTextStyle::default();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '§' {
            current.push(c);
            continue;
        }
        let Some(code) = chars.next().map(|c| c.to_ascii_lowercase()) else {
            current.push('§');
            break;
        };
        let mut next_style = style;
        let recognized = match code {
            '0'..='9' | 'a'..='f' => {
                let digit = code.to_digit(16).unwrap_or(0) as usize;
                next_style = VanillaTextStyle { color: Some(rgb24(LEGACY_COLORS[digit])), ..VanillaTextStyle::default() };
                true
            }
            'l' => { next_style.bold = true; true }
            'o' => { next_style.italic = true; true }
            'n' => { next_style.underlined = true; true }
            'm' => { next_style.strikethrough = true; true }
            'k' => { next_style.obfuscated = true; true }
            'r' => { next_style = VanillaTextStyle::default(); true }
            _ => false,
        };
        if recognized {
            if !current.is_empty() {
                spans.push(VanillaFormattedSpan { text: std::mem::take(&mut current), style });
            }
            style = next_style;
        } else {
            current.push('§');
            current.push(code);
        }
    }
    if !current.is_empty() {
        spans.push(VanillaFormattedSpan { text: current, style });
    }
    spans
}

fn safe_index_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.contains(['/', '\\', '\0'])
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Assets-store roots scanned for `indexes/` + `objects/`, in priority
/// order: the explicit `--assetsDir`, then the game dir's own `assets/`
/// (the `.minecraft` layout), then Megu's bootstrap cache.
fn candidate_assets_roots(paths: &SessionPaths) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = &paths.assets_dir {
        out.push(dir.clone());
    }
    if let Some(dir) = &paths.game_dir {
        out.push(dir.join("assets"));
    }
    if let Some(cache) = default_vanilla_cache_dir() {
        out.push(cache.join("assets"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        out.push(home.join(".minecraft").join("assets"));
    }
    out
}

/// Parse `(asset key, sha1 hash)` pairs for every `minecraft/font/…`
/// entry in an `indexes/*.json` blob. The index format is a flat
/// `{"objects": {"<path>": {"hash": "<sha1>", "size": n}}}` map — this
/// is a bounded substring scan, not a JSON parser, and it only accepts
/// keys that end `.hex` or `.zip` so a hostile index cannot make us
/// open arbitrary object names.
fn font_asset_hashes(json: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(off) = json[pos..]
        .windows(b"\"minecraft/font/".len())
        .position(|w| w == b"\"minecraft/font/")
    {
        let start = pos + off + 1;
        pos = start + 1;
        let Some(end) = json[start..].iter().position(|&b| b == b'"') else {
            break;
        };
        let key = &json[start..start + end];
        let Ok(key) = std::str::from_utf8(key) else {
            continue;
        };
        if !(key.starts_with("minecraft/font/") && key.contains("unifont"))
            || !(key.ends_with(".hex") || key.ends_with(".zip"))
        {
            continue;
        }
        // The `"hash"` member follows the key within its object literal;
        // bound the lookahead so a malformed index cannot skip ahead.
        let window_end = (start + end + 512).min(json.len());
        let window = &json[start + end..window_end];
        let Some(hash_off) = window
            .windows(b"\"hash\"".len())
            .position(|w| w == b"\"hash\"")
        else {
            continue;
        };
        let after = &window[hash_off + b"\"hash\"".len()..];
        let Some(colon) = after.iter().position(|&b| b == b':') else {
            continue;
        };
        let after = &after[colon + 1..];
        let Some(q0) = after.iter().position(|&b| b == b'"') else {
            continue;
        };
        let after = &after[q0 + 1..];
        let Some(q1) = after.iter().position(|&b| b == b'"') else {
            continue;
        };
        let hash = &after[..q1];
        if hash.len() == 40 && hash.iter().all(|b| b.is_ascii_hexdigit()) {
            out.push((key.to_owned(), String::from_utf8_lossy(hash).into_owned()));
        }
    }
    out
}

/// Asset indexes to scan, best first: the explicit `--assetIndex`
/// (e.g. `17` for 1.21.1), then every other `indexes/*.json` newest
/// numeric id first.
fn index_files(root: &Path, paths: &SessionPaths) -> Vec<PathBuf> {
    let dir = root.join("indexes");
    let mut out = Vec::new();
    if let Some(id) = &paths.asset_index {
        if safe_index_id(id) {
            let f = dir.join(format!("{id}.json"));
            if f.is_file() {
                out.push(f);
            }
        }
    }
    let mut rest: Vec<(u64, PathBuf)> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .map(|p| {
            let id = p
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            (id, p)
        })
        // Bound directory admission before sorting, then keep the newest
        // numeric candidates rather than whichever names readdir returned first.
        .take(MAX_INDEX_CANDIDATES * 16)
        .collect();
    rest.sort_by_key(|a| std::cmp::Reverse(a.0));
    rest.truncate(MAX_INDEX_CANDIDATES);
    for (_, p) in rest {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Read one Unifont `.hex` blob out of an `objects/<hh>/<hash>` store
/// entry — either directly (`.hex` key) or from the first `.hex` member
/// inside the pack zip, all in memory.
fn read_hex_objects(path: &Path, is_zip: bool) -> Vec<String> {
    if is_zip {
        let Ok((entries, _)) = read_selected_entries(path, |n| n.ends_with(".hex")) else {
            return Vec::new();
        };
        entries
            .into_iter()
            .filter_map(|entry| String::from_utf8(entry.bytes).ok())
            .collect()
    } else {
        read_bounded_file(path, MAX_HEX_OBJECT_BYTES)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .into_iter()
            .collect()
    }
}

/// Patch preference for `unifont_all_no_pua` (base coverage) vs
/// `unifont_jp_patch` (glyph overrides): the base absorbs first, patches
/// last, mirroring the vanilla font provider chain.
fn unifont_pack_rank(key: &str) -> u8 {
    if key.contains("all_no_pua") {
        0
    } else if key.contains("jp_patch") {
        1
    } else {
        2
    }
}

/// Hydrate Unifont glyphs from the given assets object-store roots.
fn load_unifont_packs(
    paths: &SessionPaths,
    assets_roots: &[PathBuf],
    atlas: &mut GlyphAtlas,
) -> bool {
    let mut loaded_any = false;
    for root in assets_roots {
        for index in index_files(root, paths) {
            let Ok(json) = read_bounded_file(&index, MAX_INDEX_JSON_BYTES as usize) else {
                continue;
            };
            let mut entries = font_asset_hashes(&json);
            if entries.is_empty() {
                continue;
            }
            entries.sort_by_key(|(key, _)| unifont_pack_rank(key));
            entries.dedup_by(|a, b| a.1 == b.1);
            for (key, hash) in entries {
                let obj = root.join("objects").join(&hash[..2]).join(&hash);
                for hex in read_hex_objects(&obj, key.ends_with(".zip")) {
                    if let Ok(pack) = GlyphAtlas::load_unifont_hex(&hex) {
                        atlas.absorb(pack);
                        loaded_any = true;
                    }
                }
            }
            if loaded_any {
                return true;
            }
        }
    }
    loaded_any
}

/// Max-pool one bitmap-provider grid cell into the renderer's 8×8 canvas.
/// Alpha bit zero/nonzero follows Vanilla's metric scan exactly; the scalar
/// render target remains the UI crate's 8-row representation.
fn bitmap_cell_to_glyph(
    image_w: u32,
    cell_w: u32,
    cell_h: u32,
    col: u32,
    row: u32,
    rgba: &[u8],
) -> GlyphBitmap {
    let mut tile = [0u8; 8];
    for dy in 0..8u32 {
        let y0 = row * cell_h + dy * cell_h / 8;
        let y1 = (row * cell_h + (dy + 1) * cell_h / 8).max(y0 + 1);
        let mut bits = 0u8;
        for dx in 0..8u32 {
            let x0 = col * cell_w + dx * cell_w / 8;
            let x1 = (col * cell_w + (dx + 1) * cell_w / 8).max(x0 + 1);
            let lit = (y0..y1).any(|y| {
                (x0..x1).any(|x| {
                    let offset = (y * image_w + x) as usize * 4 + 3;
                    rgba.get(offset).copied().unwrap_or(0) != 0
                })
            });
            if lit {
                bits |= 0x80 >> dx;
            }
        }
        tile[dy as usize] = bits;
    }
    GlyphBitmap { cells: 1, tiles: [tile, [0; 8]] }
}

/// Load every bitmap provider in the flattened vanilla default-font chain.
/// Returns `(glyphs_inserted, ascii_png_loaded)`. Bitmaps intentionally load
/// after Unifont data at the call site and overwrite it: in Vanilla's provider
/// order the bitmap providers precede Unihex, so their shared codepoints win.
fn load_vanilla_bitmap_fonts(chain: &AssetPackChain, atlas: &mut GlyphAtlas) -> (usize, bool) {
    let Some(definition) = load_vanilla_font_definition(chain) else {
        return (0, false);
    };
    let mut inserted = 0usize;
    let mut ascii_loaded = false;
    // First provider wins per codepoint, consistent with the metric table's
    // `or_insert` precedence and Vanilla's glyph-provider ordering.
    let mut bitmap_seen = BTreeSet::new();
    for provider in &definition.providers {
        let VanillaFontProviderKind::Bitmap { file, chars, .. } = &provider.kind else {
            continue;
        };
        let Some(logical) = texture_logical_path(file) else { continue; };
        let Some(bytes) = chain.read_asset(&logical, 32 * 1024 * 1024) else { continue; };
        let Ok((image_w, image_h, rgba)) = decode_png_rgba(&bytes) else { continue; };
        let rows = chars.len() as u32;
        let Some(cols) = chars.first().map(|row| row.chars().count() as u32) else { continue; };
        if rows == 0 || cols == 0 { continue; }
        let cell_w = image_w / cols;
        let cell_h = image_h / rows;
        if cell_w == 0 || cell_h == 0 { continue; }
        let mut provider_inserted = 0usize;
        for (row, text) in chars.iter().enumerate() {
            for (col, c) in text.chars().enumerate() {
                let cp = c as u32;
                if cp == 0 || !bitmap_seen.insert(cp) { continue; }
                let glyph = bitmap_cell_to_glyph(image_w, cell_w, cell_h, col as u32, row as u32, &rgba);
                atlas.insert(c, glyph);
                provider_inserted += 1;
            }
        }
        inserted += provider_inserted;
        ascii_loaded |= provider_inserted > 0 && file.ends_with("font/ascii.png");
    }
    (inserted, ascii_loaded)
}

/// Build the vanilla `ascii.png` cell table: a square png divided into
/// a 16×16 grid of glyph cells (vanilla ships 128×128 → 8×8 cells; HD
/// scales like 256×256 also load — each 8×8 dst pixel lights when any
/// covered source pixel is opaque).
fn ascii_cells_from_png(w: u32, h: u32, rgba: &[u8]) -> Option<Box<[[u8; 8]; 128]>> {
    if w == 0 || w != h || !w.is_multiple_of(16) {
        return None;
    }
    if rgba.len() != (w * h * 4) as usize {
        return None;
    }
    let cell = (w / 16) as usize;
    let mut out = Box::new([[0u8; 8]; 128]);
    for (cp, glyph) in out.iter_mut().enumerate() {
        let gx = (cp % 16) * cell;
        let gy = (cp / 16) * cell;
        for (dy, row) in glyph.iter_mut().enumerate() {
            let mut bits = 0u8;
            for dx in 0..8usize {
                let x0 = gx + dx * cell / 8;
                let x1 = gx + (dx + 1) * cell / 8;
                let y0 = gy + dy * cell / 8;
                let y1 = gy + (dy + 1) * cell / 8;
                let lit = (y0..y1).any(|y| {
                    (x0..x1).any(|x| {
                        let o = ((y as u32 * w + x as u32) * 4) as usize;
                        rgba[o + 3] >= 0x80
                    })
                });
                if lit {
                    bits |= 0x80 >> dx;
                }
            }
            *row = bits;
        }
    }
    Some(out)
}

/// Pull `ascii.png` out of one local 1.21.1 client JAR.
fn load_ascii_bitmap(jar: &Path) -> Option<Box<[[u8; 8]; 128]>> {
    let (entries, _) = read_selected_entries(jar, |n| n == ASCII_PNG_ENTRY).ok()?;
    let entry = entries.into_iter().find(|e| e.name == ASCII_PNG_ENTRY)?;
    let (w, h, rgba) = decode_png_rgba(&entry.bytes).ok()?;
    ascii_cells_from_png(w, h, &rgba)
}

/// Load fonts from explicit sources — the testable core of
/// [`try_load_local_minecraft_fonts_detailed`] with discovery already resolved.
fn try_load_fonts_from_detailed(
    paths: &SessionPaths,
    assets_roots: &[PathBuf],
    jar: Option<&Path>,
) -> Option<VanillaFontLoad> {
    let mut atlas = GlyphAtlas::default();
    let unifont_loaded = load_unifont_packs(paths, assets_roots, &mut atlas);
    if unifont_loaded {
        record_font_component(FONT_READY_UNIFONT);
    }
    let chain = AssetPackChain::for_session(paths);
    let (bitmap_glyphs, mut ascii_loaded) = load_vanilla_bitmap_fonts(&chain, &mut atlas);
    if bitmap_glyphs == 0 {
        // Compatibility fallback for a partial or pre-flattened synthetic JAR:
        // direct ascii.png loading preserves the historical zero-setup path.
        if let Some(cells) = jar.and_then(load_ascii_bitmap) {
            atlas.set_ascii_bitmap(cells);
            ascii_loaded = true;
        }
    }
    if ascii_loaded {
        record_font_component(FONT_READY_ASCII);
    }
    if bitmap_glyphs == 0 && !ascii_loaded && !unifont_loaded {
        return None;
    }
    Some(VanillaFontLoad {
        atlas,
        ascii_loaded,
        unifont_loaded,
        font_revision: font_revision(),
    })
}

/// Detailed best-effort probe: reports ASCII and Unifont readiness
/// independently and exposes the monotonic [`font_revision`] invalidation
/// counter alongside the merged atlas.
pub fn try_load_local_minecraft_fonts_detailed(paths: &SessionPaths) -> Option<VanillaFontLoad> {
    try_load_fonts_from_detailed(
        paths,
        &candidate_assets_roots(paths),
        VanillaAssetResolver::resolve_session(paths).as_deref(),
    )
}

/// Best-effort load of the local Minecraft 1.21.1 font set: the vanilla
/// `ascii.png` 8×8 ASCII table and the Unifont `.hex` packs, merged into
/// one [`GlyphAtlas`] ready for
/// [`megu_ui::font_cjk::install_external_font`]. Returns `None` when no
/// local Minecraft install yields anything — callers treat `None` as
/// "keep the built-in fonts", never an error.
pub fn try_load_local_minecraft_fonts(paths: &SessionPaths) -> Option<GlyphAtlas> {
    try_load_fonts_from_detailed(
        paths,
        &candidate_assets_roots(paths),
        VanillaAssetResolver::resolve_session(paths).as_deref(),
    )
    .map(VanillaFontLoad::into_atlas)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal ZIP writer for unit tests — stored entries only, real
    /// CRC32s so strict index readers accept the archive.
    fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let crc = megu_core::hash::crc32_ieee_update(0, data);
            let lh_off = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&lh_off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Build a raw RGBA buffer the PNG decoder accepts via a real PNG
    /// encoder on a tiny helper: reuses `zlib_compress_stored` like the
    /// other fixtures in this crate — no Mojang bytes anywhere.
    fn raw_rgba_png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
        fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let crc = megu_core::hash::crc32_ieee_update(
                megu_core::hash::crc32_ieee_update(0, kind),
                data,
            );
            out.extend_from_slice(&crc.to_be_bytes());
            out
        }
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut raw = Vec::with_capacity((w * h * 4 + h) as usize);
        for y in 0..h {
            raw.push(0u8); // filter: none
            let row = &rgba[(y * w * 4) as usize..((y + 1) * w * 4) as usize];
            raw.extend_from_slice(row);
        }
        let idat = megu_core::zip_archive::zlib_compress_stored(&raw);
        let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10];
        png.extend(chunk(b"IHDR", &ihdr));
        png.extend(chunk(b"IDAT", &idat));
        png.extend(chunk(b"IEND", &[]));
        png
    }

    /// A tempdir that cleans itself up.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "megu-vanilla-font-test-{}-{}",
                std::process::id(),
                tag
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fake_paths(root: &Path) -> SessionPaths {
        SessionPaths {
            game_dir: Some(root.to_path_buf()),
            assets_dir: Some(root.join("assets")),
            asset_index: Some("17".to_owned()),
            version_id: Some("1.21.1".to_owned()),
            resource_pack_dir: None,
            server_mods_dir: None,
        }
    }

    #[test]
    fn ascii_cells_map_16x16_grid_to_codepoints() {
        // 128×128 png: paint 'A' (cp 0x41) cell's first pixel row solid.
        let w = 128u32;
        let mut rgba = vec![0u8; (w * w * 4) as usize];
        let cp = 0x41usize;
        let gx = (cp % 16) * 8;
        let gy = (cp / 16) * 8;
        for x in gx..gx + 8 {
            let o = ((gy as u32 * w + x as u32) * 4) as usize;
            rgba[o + 3] = 0xff;
        }
        let cells = ascii_cells_from_png(w, w, &rgba).unwrap();
        assert_eq!(cells[0x41][0], 0xff);
        assert_eq!(cells[0x41][1], 0);
        assert_eq!(cells[0x42][0], 0, "neighbour cell must stay empty");
    }

    #[test]
    fn ascii_cells_accept_hd_scale_multiples() {
        // 256×256 → 16×16 px cells; light 'B' everywhere.
        let w = 256u32;
        let mut rgba = vec![0u8; (w * w * 4) as usize];
        let cp = 0x42usize;
        for y in (cp / 16) * 16..(cp / 16 + 1) * 16 {
            for x in (cp % 16) * 16..(cp % 16 + 1) * 16 {
                let o = ((y as u32 * w + x as u32) * 4) as usize;
                rgba[o + 3] = 0xff;
            }
        }
        let cells = ascii_cells_from_png(w, w, &rgba).unwrap();
        assert_eq!(cells[0x42], [0xff; 8]);
        assert!(ascii_cells_from_png(100, 100, &rgba).is_none());
        assert!(ascii_cells_from_png(0, 0, &[]).is_none());
    }

    #[test]
    fn font_asset_hashes_find_unifont_entries_only() {
        let json = br#"{"objects":{
            "minecraft/font/unifont_all_no_pua-15.1.05.zip":{"hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":1},
            "minecraft/font/unifont_jp_patch-15.1.05.hex":{"hash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size":1},
            "minecraft/font/ascii.png":{"hash":"cccccccccccccccccccccccccccccccccccccccc","size":1},
            "minecraft/sounds/x.ogg":{"hash":"dddddddddddddddddddddddddddddddddddddddd","size":1}
        }}"#;
        let found = font_asset_hashes(json);
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|(k, _)| k.contains("all_no_pua")));
        assert!(found.iter().any(|(k, _)| k.contains("jp_patch")));
    }

    #[test]
    fn loads_ascii_and_unifont_from_synthetic_install() {
        let tmp = TempDir::new("full");
        let root = &tmp.0;
        // Fake 1.21.1 client JAR with only ascii.png inside.
        let mut rgba = vec![0u8; 128 * 128 * 4];
        let cp = 0x30usize; // '0'
        let (gx, gy) = ((cp % 16) * 8, (cp / 16) * 8);
        for x in gx..gx + 8 {
            rgba[((gy * 128 + x) * 4) + 3] = 0xff;
        }
        let png = raw_rgba_png(128, 128, &rgba);
        let jar = stored_zip(&[(ASCII_PNG_ENTRY, &png)]);
        let jar_dir = root.join("versions").join("1.21.1");
        std::fs::create_dir_all(&jar_dir).unwrap();
        std::fs::write(jar_dir.join("1.21.1.jar"), &jar).unwrap();
        // Assets index + objects for one unifont .hex zip.
        let hex = "30AB:FFFF80018001800180018001800180018001800180018001800180018001FFFF\n";
        let hex_zip = stored_zip(&[("unifont_all_no_pua-0.0.hex", hex.as_bytes())]);
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let obj_dir = root.join("assets").join("objects").join(&hash[..2]);
        std::fs::create_dir_all(&obj_dir).unwrap();
        std::fs::write(obj_dir.join(hash), &hex_zip).unwrap();
        let index = format!(
            "{{\"objects\":{{\"minecraft/font/unifont_all_no_pua-0.0.zip\":{{\"hash\":\"{hash}\",\"size\":1}}}}}}"
        );
        let idx_dir = root.join("assets").join("indexes");
        std::fs::create_dir_all(&idx_dir).unwrap();
        std::fs::write(idx_dir.join("17.json"), index).unwrap();

        let atlas = try_load_fonts_from_detailed(
            &fake_paths(root),
            &[root.join("assets")],
            Some(&jar_dir.join("1.21.1.jar")),
        )
        .expect("synthetic install should produce an atlas")
        .into_atlas();
        assert!(atlas.has_ascii_bitmap());
        assert_eq!(atlas.ascii_glyph('0').unwrap()[0], 0xff);
        let g = atlas.unifont_glyph('\u{30ab}').expect("katakana glyph");
        assert_eq!(g.cells, 2);
        assert_eq!(g.rows[0], 0xffff);
    }

    #[test]
    fn missing_install_never_panics() {
        let tmp = TempDir::new("empty");
        // The unified resolver intentionally sees the host's own launcher
        // caches when present — that is the Prism/`.minecraft` zero-setup
        // path. The empty tempdir itself contributes nothing, so the result
        // is host-dependent; the contract is fail-closed silence and never
        // a panic either way.
        let _ = try_load_fonts_from_detailed(&fake_paths(&tmp.0), &[tmp.0.join("assets")], None);
    }
}
