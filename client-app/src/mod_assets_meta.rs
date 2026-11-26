// SPDX-License-Identifier: LGPL-3.0-or-later
//! Bounded, client-side mod translations, item icons, and dynamic registry IDs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_ARCHIVES: usize = 256;
const MAX_LANG_BYTES: usize = 1024 * 1024;
const MAX_ICON_BYTES: usize = 16 * 1024 * 1024;
const MAX_INDEXED_ASSETS: usize = 65_535;
const MAX_MRS_BYTES: usize = 8 * 1024 * 1024;
const MAX_MRS_ROWS: usize = 131_072;

/// Play S2C custom-payload channel carrying the `MRS1` dynamic registry frame.
pub const CHANNEL_MOD_REGISTRY_SYNC: &str = "megu:mod_registry_sync_v1";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModMetaReport {
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ModTranslationTable {
    locale: String,
    translated: BTreeMap<String, String>,
    english: BTreeMap<String, String>,
}

impl ModTranslationTable {
    pub fn load(game_dir: &Path, locale: &str, report: &mut ModMetaReport) -> Self {
        let locale = normalize_locale(locale);
        let mut table = Self {
            locale: locale.clone(),
            ..Self::default()
        };
        let jars = sorted_jars(game_dir, report);
        for jar in jars {
            let archive = match crate::zip_entries::ZipArchive::open(&jar) {
                Ok(archive) => archive,
                Err(error) => {
                    report.warnings.push(error);
                    continue;
                }
            };
            for name in archive
                .entry_names()
                .filter(|name| is_lang_path(name, &locale))
                .map(str::to_owned)
            {
                let bytes = match archive.read_entry(&name, MAX_LANG_BYTES) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => continue,
                    Err(error) => {
                        report.warnings.push(format!("{}: {error}", jar.display()));
                        continue;
                    }
                };
                let parsed = match parse_flat_json(&bytes) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        report
                            .warnings
                            .push(format!("{}:{name}: {error}", jar.display()));
                        continue;
                    }
                };
                let target = if name.ends_with("/en_us.json") {
                    &mut table.english
                } else {
                    &mut table.translated
                };
                for (key, value) in parsed {
                    target.entry(key).or_insert(value);
                }
            }
        }
        table
    }

    pub fn translate_block(&self, namespaced_id: &str) -> String {
        self.translate("block", namespaced_id)
    }

    pub fn translate_item(&self, namespaced_id: &str) -> String {
        self.translate("item", namespaced_id)
    }

    fn translate(&self, kind: &str, id: &str) -> String {
        let key = format!("{kind}.{}", id.replace(':', "."));
        self.translated
            .get(&key)
            .or_else(|| self.english.get(&key))
            .cloned()
            .unwrap_or_else(|| humanize_id(id))
    }

    pub fn locale(&self) -> &str {
        &self.locale
    }
}

fn normalize_locale(locale: &str) -> String {
    locale.trim().replace('-', "_").to_ascii_lowercase()
}

fn is_lang_path(name: &str, locale: &str) -> bool {
    let parts: Vec<_> = name.split('/').collect();
    parts.len() == 4
        && parts[0] == "assets"
        && parts[1].len() <= 64
        && parts[1]
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && parts[2] == "lang"
        && (parts[3] == "en_us.json" || parts[3] == format!("{locale}.json"))
}

fn sorted_jars(game_dir: &Path, report: &mut ModMetaReport) -> Vec<PathBuf> {
    let dir = game_dir.join("mods");
    let mut jars = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("jar")))
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            report.warnings.push(format!("{}: {error}", dir.display()));
            Vec::new()
        }
    };
    jars.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    if jars.len() > MAX_ARCHIVES {
        report.warnings.push(format!(
            "mod archive count exceeds {MAX_ARCHIVES}; excess archives ignored"
        ));
        jars.truncate(MAX_ARCHIVES);
    }
    jars
}

fn humanize_id(id: &str) -> String {
    let tail = id.split_once(':').map_or(id, |(_, tail)| tail);
    let words = tail.replace(['_', '-'], " ");
    let mut out = String::new();
    let mut start = true;
    for ch in words.chars() {
        if ch.is_whitespace() {
            start = true;
            out.push(ch);
        } else if start {
            out.extend(ch.to_uppercase());
            start = false;
        } else {
            out.push(ch);
        }
    }
    out
}

#[derive(Clone, Debug, Default)]
pub struct ModItemIconIndex {
    icons: BTreeMap<String, (PathBuf, String)>,
}

impl ModItemIconIndex {
    pub fn load(game_dir: &Path, report: &mut ModMetaReport) -> Self {
        let mut index = Self::default();
        for jar in sorted_jars(game_dir, report) {
            let archive = match crate::zip_entries::ZipArchive::open(&jar) {
                Ok(a) => a,
                Err(e) => {
                    report.warnings.push(e);
                    continue;
                }
            };
            for entry in archive.entry_names() {
                if let Some(id) = icon_id(entry) {
                    if index.icons.len() >= MAX_INDEXED_ASSETS {
                        report
                            .warnings
                            .push("mod icon index entry budget exceeded".into());
                        return index;
                    }
                    index
                        .icons
                        .entry(id)
                        .or_insert_with(|| (jar.clone(), entry.to_owned()));
                }
            }
        }
        index
    }
    pub fn icon_bytes(&self, namespaced_id: &str) -> Result<Option<Vec<u8>>, String> {
        let Some((jar, entry)) = self.icons.get(namespaced_id) else {
            return Ok(None);
        };
        crate::zip_entries::ZipArchive::open(jar)?.read_entry(entry, MAX_ICON_BYTES)
    }
    pub fn len(&self) -> usize {
        self.icons.len()
    }
    pub fn is_empty(&self) -> bool {
        self.icons.is_empty()
    }
}

fn icon_id(path: &str) -> Option<String> {
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() >= 5
        && parts[0] == "assets"
        && parts[2] == "textures"
        && parts[3] == "item"
        && parts.last()?.ends_with(".png")
    {
        let id = parts[4..].join("/");
        Some(format!("{}:{}", parts[1], id.trim_end_matches(".png")))
    } else {
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModBlockState {
    pub name: String,
    pub properties: BTreeMap<String, String>,
    /// Packed `(luminance << 4) | opacity` lighting facts carried by MRS1 v2,
    /// or derived through the server-compatible generic rules for MRS1 v1.
    pub facts_byte: u8,
}

// The following shape/luminance/opacity rules mirror
// `mod_adapter_core::live_registry_bridge` exactly. MRS1 v1 does not carry the
// byte on the wire, so mixed-version clients and servers must derive the same
// value rather than treating every modded block as opaque.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModShape {
    Empty,
    Thin,
    Partial,
    Leaves,
    Liquid,
    Full,
}

fn mod_block_shape(path: &str) -> ModShape {
    if path == "block" || path.ends_with("_block") {
        return ModShape::Full;
    }
    if ["water", "lava", "fluid", "liquid"]
        .iter()
        .any(|k| path.ends_with(k))
    {
        return ModShape::Liquid;
    }
    if path.contains("leaves") {
        return ModShape::Leaves;
    }
    const EMPTY: &[&str] = &[
        "torch", "rail", "flower", "sapling", "crop", "vine", "wire", "button", "lever", "sign",
        "banner", "candle", "mushroom", "fern", "coral", "reed", "lichen", "grass", "plant",
        "seed",
    ];
    if EMPTY.iter().any(|k| path.contains(k)) {
        return ModShape::Empty;
    }
    const THIN: &[&str] = &[
        "glass",
        "pane",
        "grate",
        "bars",
        "chain",
        "lantern",
        "pipe",
        "shaft",
        "belt",
        "cogwheel",
        "wheel",
        "funnel",
        "chute",
        "spout",
        "depot",
        "basin",
        "pump",
        "bearing",
        "ladder",
        "scaffold",
        "frame",
        "pressure_plate",
        "door",
        "trapdoor",
        "hopper",
        "cauldron",
        "anvil",
        "conduit",
    ];
    if THIN.iter().any(|k| path.contains(k)) {
        return ModShape::Thin;
    }
    const PARTIAL: &[&str] = &[
        "slab", "stair", "fence", "wall", "carpet", "snow", "path", "farmland",
    ];
    if PARTIAL.iter().any(|k| path.contains(k)) || path.ends_with("_bed") {
        return ModShape::Partial;
    }
    ModShape::Full
}

fn mod_state_luminance(path: &str, properties: &BTreeMap<String, String>) -> u8 {
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
    for (key, value) in properties {
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
            ("powered", "true") if base > 0 => luminance = 15,
            _ => {}
        }
    }
    luminance.min(15)
}

fn mod_state_opacity(path: &str, properties: &BTreeMap<String, String>) -> u8 {
    let mut opacity = match mod_block_shape(path) {
        ModShape::Empty | ModShape::Thin => 0,
        ModShape::Partial | ModShape::Leaves | ModShape::Liquid => 1,
        ModShape::Full => 15,
    };
    if opacity == 0
        && properties
            .iter()
            .any(|(k, v)| k == "waterlogged" && v == "true")
    {
        opacity = 1;
    }
    opacity
}

pub fn derive_block_facts_byte(block_id: &str, properties: &BTreeMap<String, String>) -> u8 {
    let path = block_id.rsplit(':').next().unwrap_or(block_id);
    (mod_state_luminance(path, properties) << 4) | mod_state_opacity(path, properties)
}

#[derive(Clone, Debug, Default)]
pub struct ClientModRegistry {
    pub base_block_state_id: u32,
    pub base_item_id: u32,
    blocks: BTreeMap<u32, ModBlockState>,
    items: BTreeMap<u32, String>,
}


impl ClientModRegistry {
    /// Decode MRS1 matching `mod_adapter_core::live_registry_bridge` byte-for-byte:
    /// magic (`MRS1`), version `u16` (1 or 2), `base_block_state_id` (`u32`),
    /// `base_item_id` (`u32`), block count (`u32`), then each block as monotonic
    /// `state_id` (`u32`), UTF-8 `block_id`, `u16` property count and UTF-8
    /// key/value pairs. Version 2 appends one lighting-facts byte per block
    /// state; version 1 derives the same byte client-side. Item count (`u32`)
    /// is followed by monotonic `item_id` (`u32`) and UTF-8 item name, then a
    /// trailing IEEE CRC32 (`u32`, big-endian). All integers are big-endian and
    /// strings use `u16` byte lengths.
    pub fn decode_mrs1_frame(frame: &[u8]) -> Result<Self, String> {
        if frame.len() > MAX_MRS_BYTES {
            return Err("MRS1 frame exceeds byte budget".into());
        }
        if frame.len() < 4 + 2 + 4 + 4 + 4 + 4 + 4 {
            return Err("truncated MRS1 frame".into());
        }
        let payload_len = frame.len() - 4;
        let expected_crc = u32::from_be_bytes(
            frame[payload_len..]
                .try_into()
                .map_err(|_| "truncated MRS1 crc32")?,
        );
        if megu_core::hash::crc32_ieee(&frame[..payload_len]) != expected_crc {
            return Err("MRS1 crc32 mismatch".into());
        }
        let mut r = Reader {
            bytes: &frame[..payload_len],
            pos: 0,
        };
        if r.take(4)? != b"MRS1" {
            return Err("invalid MRS1 magic".into());
        }
        let version = r.u16()?;
        if version != 1 && version != 2 {
            return Err("unsupported MRS1 version".into());
        }
        let has_wire_facts = version == 2;
        let base_block_state_id = r.u32()?;
        let base_item_id = r.u32()?;
        let block_count = r.count()?;
        let mut blocks = BTreeMap::new();
        let mut prev_state_id = None;
        for _ in 0..block_count {
            let state_id = r.u32()?;
            if state_id < base_block_state_id
                || prev_state_id.is_some_and(|prev| state_id <= prev)
            {
                return Err("non-monotonic MRS1 block state id".into());
            }
            prev_state_id = Some(state_id);
            let name = r.string()?;
            let property_count = usize::from(r.u16()?);
            if property_count > 4096 {
                return Err("MRS1 property budget exceeded".into());
            }
            let mut properties = BTreeMap::new();
            for _ in 0..property_count {
                let k = r.string()?;
                let v = r.string()?;
                if properties.insert(k, v).is_some() {
                    return Err("duplicate MRS1 property".into());
                }
            }
            let facts_byte = if has_wire_facts {
                r.take(1)?[0]
            } else {
                derive_block_facts_byte(&name, &properties)
            };
            blocks.insert(
                state_id,
                ModBlockState {
                    name,
                    properties,
                    facts_byte,
                },
            );
        }
        let item_count = r.count()?;
        let mut items = BTreeMap::new();
        let mut prev_item_id = None;
        for _ in 0..item_count {
            let item_id = r.u32()?;
            if item_id < base_item_id || prev_item_id.is_some_and(|prev| item_id <= prev) {
                return Err("non-monotonic MRS1 item id".into());
            }
            prev_item_id = Some(item_id);
            items.insert(item_id, r.string()?);
        }
        if r.pos != payload_len {
            return Err("trailing bytes in MRS1 frame".into());
        }
        Ok(Self {
            base_block_state_id,
            base_item_id,
            blocks,
            items,
        })
    }
    pub fn block_state_name(&self, id: u32) -> Option<&str> {
        self.blocks.get(&id).map(|b| b.name.as_str())
    }
    pub fn block_state(&self, id: u32) -> Option<&ModBlockState> {
        self.blocks.get(&id)
    }
    /// Iterate every decoded sparse block-state row in ascending global state-id
    /// order. The rows are already fully sparse: callers must consume this
    /// iterator directly rather than probing a dense span derived from
    /// `base_block_state_id`, since one MRS1 frame may contain both the Create
    /// band (`26_684..`) and the generic mod band (`1_000_000..`).
    pub fn block_states(&self) -> impl Iterator<Item = (u32, &ModBlockState)> {
        self.blocks.iter().map(|(&id, state)| (id, state))
    }
    pub fn item_name(&self, id: u32) -> Option<&str> {
        self.items.get(&id).map(String::as_str)
    }
}

pub fn decode_mrs1_frame(frame: &[u8]) -> Result<ClientModRegistry, String> {
    ClientModRegistry::decode_mrs1_frame(frame)
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).ok_or("MRS1 offset overflow")?;
        let out = self
            .bytes
            .get(self.pos..end)
            .ok_or("truncated MRS1 frame")?;
        self.pos = end;
        Ok(out)
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| "truncated u16")?,
        ))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| "truncated u32")?,
        ))
    }
    fn count(&mut self) -> Result<usize, String> {
        let n = self.u32()? as usize;
        if n > MAX_MRS_ROWS {
            return Err("MRS1 row budget exceeded".into());
        }
        Ok(n)
    }
    fn string(&mut self) -> Result<String, String> {
        let n = usize::from(self.u16()?);
        let s = std::str::from_utf8(self.take(n)?).map_err(|_| "invalid MRS1 UTF-8")?;
        if s.is_empty() || s.len() > 512 {
            return Err("invalid MRS1 string length".into());
        }
        Ok(s.to_owned())
    }
}

fn parse_flat_json(bytes: &[u8]) -> Result<BTreeMap<String, String>, &'static str> {
    let s = std::str::from_utf8(bytes).map_err(|_| "invalid UTF-8")?;
    let value = megu_core::json::parse_json(s).map_err(|_| "invalid JSON syntax")?;
    let megu_core::json::JsonValue::Object(entries) = value else {
        return Err("invalid JSON syntax");
    };
    let mut map = BTreeMap::new();
    for (key, value) in entries {
        let megu_core::json::JsonValue::String(value) = value else {
            return Err("invalid JSON syntax");
        };
        if key.is_empty() || value.is_empty() || map.insert(key, value).is_some() {
            return Err("invalid or duplicate translation");
        };
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn temporary_game_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "megu-mod-meta-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("mods")).expect("create temp mod dir");
        dir
    }

    fn write_jar(path: &Path, entries: &[(&str, &[u8])]) {
        let mut file = fs::File::create(path).expect("create jar");
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = file.stream_position().expect("position") as u32;
            let name_bytes = name.as_bytes();
            let crc = megu_core::hash::crc32_ieee(data);
            let mut local = Vec::new();
            local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            local.extend_from_slice(&20u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&[0; 4]);
            local.extend_from_slice(&crc.to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(name_bytes);
            local.extend_from_slice(data);
            file.write_all(&local).expect("write entry");
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&[0; 4]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name_bytes);
        }
        let cd_offset = file.stream_position().expect("position") as u32;
        file.write_all(&central).expect("write directory");
        let cd_size = central.len() as u32;
        let count = entries.len() as u16;
        let mut end = Vec::new();
        end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&cd_size.to_le_bytes());
        end.extend_from_slice(&cd_offset.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        file.write_all(&end).expect("write end record");
    }

    #[test]
    fn loads_locale_english_fallback_icons_and_skips_bad_sibling() {
        let dir = temporary_game_dir();
        write_jar(&dir.join("mods/01-create.jar"), &[
            ("assets/create/lang/ja_jp.json", r#"{"block.create.water_wheel":"水車","item.create.wrench":"レンチ"}"#.as_bytes()),
            ("assets/create/lang/en_us.json", br#"{"block.create.water_wheel":"Water Wheel","item.create.wrench":"Wrench","item.create.goggles":"Goggles"}"#),
            ("assets/create/textures/item/wrench.png", b"png-bytes"),
            ("../assets/create/lang/ja_jp.json", br#"{"item.create.bad":"Bad"}"#),
        ]);
        fs::write(dir.join("mods/02-broken.jar"), b"not a zip").expect("write broken sibling");
        let mut report = ModMetaReport::default();
        let translations = ModTranslationTable::load(&dir, "ja-JP", &mut report);
        assert_eq!(translations.translate_block("create:water_wheel"), "水車");
        assert_eq!(translations.translate_item("create:goggles"), "Goggles");
        assert_eq!(
            translations.translate_item("create:brass_ingot"),
            "Brass Ingot"
        );
        let icons = ModItemIconIndex::load(&dir, &mut report);
        assert_eq!(
            icons.icon_bytes("create:wrench").expect("icon read"),
            Some(b"png-bytes".to_vec())
        );
        assert!(!report.warnings.is_empty());
        fs::remove_dir_all(dir).expect("remove temp dir");
    }

    #[test]
    fn decodes_mrs1_and_rejects_bad_frames() {
        let mut frame = b"MRS1".to_vec();
        frame.extend_from_slice(&1u16.to_be_bytes());
        frame.extend_from_slice(&40u32.to_be_bytes());
        frame.extend_from_slice(&70u32.to_be_bytes());
        frame.extend_from_slice(&1u32.to_be_bytes());
        frame.extend_from_slice(&40u32.to_be_bytes());
        let block_name = "create:wheel";
        frame.extend_from_slice(&(block_name.len() as u16).to_be_bytes());
        frame.extend_from_slice(block_name.as_bytes());
        frame.extend_from_slice(&1u16.to_be_bytes());
        for s in ["axis", "y"] {
            frame.extend_from_slice(&(s.len() as u16).to_be_bytes());
            frame.extend_from_slice(s.as_bytes());
        }
        frame.extend_from_slice(&1u32.to_be_bytes());
        frame.extend_from_slice(&70u32.to_be_bytes());
        let item = "create:wrench";
        frame.extend_from_slice(&(item.len() as u16).to_be_bytes());
        frame.extend_from_slice(item.as_bytes());
        let crc = megu_core::hash::crc32_ieee(&frame);
        frame.extend_from_slice(&crc.to_be_bytes());
        let registry = decode_mrs1_frame(&frame).expect("valid frame");
        assert_eq!(registry.block_state_name(40), Some("create:wheel"));
        assert_eq!(
            registry
                .block_state(40)
                .and_then(|b| b.properties.get("axis"))
                .map(String::as_str),
            Some("y")
        );
        assert_eq!(registry.item_name(70), Some("create:wrench"));
        assert_eq!(registry.block_state(40).map(|b| b.facts_byte), Some(0x00));
        for bad in [&b"NOPE"[..], &frame[..frame.len() - 1]] {
            assert!(decode_mrs1_frame(bad).is_err());
        }
        let mut wrong_version = frame[..frame.len() - 4].to_vec();
        wrong_version[4] = 0;
        wrong_version[5] = 3;
        let bad_ver_crc = megu_core::hash::crc32_ieee(&wrong_version);
        wrong_version.extend_from_slice(&bad_ver_crc.to_be_bytes());
        assert!(decode_mrs1_frame(&wrong_version).is_err());
        // Corrupt CRC must fail closed.
        let mut bad_crc = frame;
        let last = bad_crc.len() - 1;
        bad_crc[last] ^= 0xFF;
        assert!(decode_mrs1_frame(&bad_crc).is_err());
    }

    #[test]
    fn decodes_mrs1_v2_lighting_facts_without_dropping_entries() {
        let mut frame = b"MRS1".to_vec();
        frame.extend_from_slice(&2u16.to_be_bytes());
        frame.extend_from_slice(&40u32.to_be_bytes());
        frame.extend_from_slice(&70u32.to_be_bytes());
        frame.extend_from_slice(&2u32.to_be_bytes());

        frame.extend_from_slice(&40u32.to_be_bytes());
        let block = "create:arc_lamp";
        frame.extend_from_slice(&(block.len() as u16).to_be_bytes());
        frame.extend_from_slice(block.as_bytes());
        frame.extend_from_slice(&1u16.to_be_bytes());
        for s in ["lit", "true"] {
            frame.extend_from_slice(&(s.len() as u16).to_be_bytes());
            frame.extend_from_slice(s.as_bytes());
        }
        frame.push(0xff);

        frame.extend_from_slice(&41u32.to_be_bytes());
        let block = "create:framed_glass";
        frame.extend_from_slice(&(block.len() as u16).to_be_bytes());
        frame.extend_from_slice(block.as_bytes());
        frame.extend_from_slice(&0u16.to_be_bytes());
        frame.push(0x00);

        frame.extend_from_slice(&1u32.to_be_bytes());
        frame.extend_from_slice(&70u32.to_be_bytes());
        let item = "create:wrench";
        frame.extend_from_slice(&(item.len() as u16).to_be_bytes());
        frame.extend_from_slice(item.as_bytes());
        let crc = megu_core::hash::crc32_ieee(&frame);
        frame.extend_from_slice(&crc.to_be_bytes());

        let registry = decode_mrs1_frame(&frame).expect("valid v2 frame");
        assert_eq!(registry.block_state_name(40), Some("create:arc_lamp"));
        assert_eq!(registry.block_state_name(41), Some("create:framed_glass"));
        assert_eq!(registry.item_name(70), Some("create:wrench"));
        assert_eq!(registry.block_state(40).map(|b| b.facts_byte), Some(0xff));
        assert_eq!(registry.block_state(41).map(|b| b.facts_byte), Some(0x00));
    }
}
