// SPDX-License-Identifier: LGPL-3.0-or-later
// Bounded fallback reader for the vanilla 1.21.1 data resources in a game JAR.
// Included from commands_core.rs so the fallback can feed the existing
// datapack pipeline without adding a crate dependency or module-graph edge.
use std::path::{Path, PathBuf};

const VANILLA_JAR_MAX_BYTES: u64 = 256 * 1024 * 1024;
const VANILLA_JAR_MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
const VANILLA_JAR_MAX_DATA_BYTES: usize = 64 * 1024 * 1024;
const VANILLA_JAR_MAX_ENTRIES: usize = 100_000;
const BUNDLER_SERVER_JAR: &str = "META-INF/versions/1.21.1/server-1.21.1.jar";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct JarDataFallbackSummary {
    recipes: usize,
    tags: usize,
    loot_tables: usize,
    advancements: usize,
    worldgen: usize,
    registries: usize,
}

#[derive(Clone, Debug, Default)]
struct VanillaJarDataFallback {
    entries: std::collections::BTreeMap<String, Vec<u8>>,
}

impl VanillaJarDataFallback {
    fn load_from_jar(path: &Path) -> Result<Self, String> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "{} is not a regular non-symlink file",
                path.display()
            ));
        }
        if metadata.len() > VANILLA_JAR_MAX_BYTES {
            return Err(format!("{} exceeds the 256 MiB JAR limit", path.display()));
        }
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if bytes.len() as u64 > VANILLA_JAR_MAX_BYTES {
            return Err(format!("{} exceeds the 256 MiB JAR limit", path.display()));
        }
        Self::from_archive(&bytes)
    }

    fn from_archive(bytes: &[u8]) -> Result<Self, String> {
        let members = jar_selected_entries(bytes, |name| name.starts_with("data/minecraft/"))?;
        if !members.is_empty() {
            return Ok(Self {
                entries: members.into_iter().collect(),
            });
        }
        let nested = jar_selected_entries_limited(
            bytes,
            |name| name == BUNDLER_SERVER_JAR,
            VANILLA_JAR_MAX_BYTES as usize,
            VANILLA_JAR_MAX_BYTES as usize,
        )?
        .into_iter()
        .next()
        .ok_or_else(|| {
            "JAR has neither root data/minecraft entries nor the 1.21.1 server bundler member"
                .to_string()
        })?;
        let inner = nested.1;
        let entries = jar_selected_entries(&inner, |name| name.starts_with("data/minecraft/"))?;
        if entries.is_empty() {
            return Err("nested server JAR contains no data/minecraft entries".into());
        }
        Ok(Self {
            entries: entries.into_iter().collect(),
        })
    }

    fn read_data_entry(&self, relative_under_data: &str) -> Option<&[u8]> {
        let name = safe_data_relative(relative_under_data)?;
        self.entries.get(&format!("data/{name}")).map(Vec::as_slice)
    }

    fn list_prefix(&self, prefix: &str) -> Vec<&str> {
        let prefix = prefix.trim_start_matches('/');
        self.entries
            .keys()
            .filter_map(|name| name.strip_prefix("data/"))
            .filter(|name| name.starts_with(prefix))
            .collect()
    }

    fn summary(&self) -> JarDataFallbackSummary {
        let mut summary = JarDataFallbackSummary::default();
        for path in self
            .entries
            .keys()
            .filter_map(|p| p.strip_prefix("data/minecraft/"))
        {
            let kind = path.split('/').next().unwrap_or_default();
            match kind {
                "recipe" | "recipes" => summary.recipes += 1,
                "tags" => summary.tags += 1,
                "loot_table" | "loot_tables" => summary.loot_tables += 1,
                "advancement" | "advancements" => summary.advancements += 1,
                "worldgen" => summary.worldgen += 1,
                "dimension_type" | "damage_type" | "painting_variant" | "jukebox_song" => {
                    summary.registries += 1
                }
                _ => {}
            }
        }
        summary
    }
}

fn safe_data_relative(path: &str) -> Option<&str> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', ':', '\0'])
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return None;
    }
    Some(path)
}

fn usable_vanilla_jar(path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    (!metadata.file_type().is_symlink()
        && metadata.is_file()
        && metadata.len() <= VANILLA_JAR_MAX_BYTES)
        .then(|| path.to_path_buf())
}

fn discover_vanilla_jar() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("MEGU_VANILLA_JAR") {
        let path = PathBuf::from(configured);
        if let Some(path) = usable_vanilla_jar(&path) {
            return Some(path);
        }
    }
    for path in ["1.21.1.jar", "server.jar", "minecraft_server.1.21.1.jar"] {
        if let Some(path) = usable_vanilla_jar(Path::new(path)) {
            return Some(path);
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    let candidates = home
        .into_iter()
        .map(|h| h.join(".minecraft/versions/1.21.1/1.21.1.jar"))
        .chain(
            xdg.into_iter()
                .map(|x| x.join("megu/versions/1.21.1/1.21.1.jar")),
        );
    candidates
        .filter_map(|path| usable_vanilla_jar(&path))
        .next()
}

#[allow(dead_code)]
pub(crate) fn vanilla_jar_data_read(rel_path: &str) -> Option<Vec<u8>> {
    static FALLBACK: std::sync::OnceLock<Option<VanillaJarDataFallback>> =
        std::sync::OnceLock::new();
    FALLBACK
        .get_or_init(|| {
            let path = discover_vanilla_jar()?;
            VanillaJarDataFallback::load_from_jar(&path).ok()
        })
        .as_ref()?
        .read_data_entry(rel_path)
        .map(<[u8]>::to_vec)
}

fn jar_u16(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(bytes.get(at..at.checked_add(2)?)?.try_into().ok()?) as usize)
}

fn jar_u32(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?) as usize)
}

/// Scan the central directory and inflate only matching files. Unselected
/// resource/assets do not consume the data-entry budget or get decompressed.
fn jar_selected_entries(
    bytes: &[u8],
    wanted: impl Fn(&str) -> bool,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    jar_selected_entries_limited(
        bytes,
        wanted,
        VANILLA_JAR_MAX_ENTRY_BYTES,
        VANILLA_JAR_MAX_DATA_BYTES,
    )
}

fn jar_selected_entries_limited(
    bytes: &[u8],
    wanted: impl Fn(&str) -> bool,
    max_entry: usize,
    max_total: usize,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    if bytes.len() as u64 > VANILLA_JAR_MAX_BYTES {
        return Err("JAR exceeds the 256 MiB limit".into());
    }
    let end = bytes.len().checked_sub(22).ok_or("truncated ZIP archive")?;
    let min = end.saturating_sub(u16::MAX as usize);
    let eocd = (min..=end)
        .rev()
        .find(|&p| {
            bytes.get(p..p + 4) == Some(b"PK\x05\x06") && jar_u16(bytes, p + 20) == Some(end - p)
        })
        .ok_or("missing or malformed ZIP footer")?;
    let count = jar_u16(bytes, eocd + 10).ok_or("truncated ZIP footer")?;
    let start = jar_u32(bytes, eocd + 16).ok_or("truncated ZIP footer")?;
    if count > VANILLA_JAR_MAX_ENTRIES
        || jar_u32(bytes, eocd + 4) != Some(0)
        || jar_u16(bytes, eocd + 8) != Some(count)
        || start > eocd
        || jar_u32(bytes, eocd + 12) != Some(eocd - start)
    {
        return Err("unsupported ZIP directory shape".into());
    }
    let mut central = bytes
        .get(start..eocd)
        .ok_or("invalid ZIP directory bounds")?;
    let mut result = Vec::new();
    let mut total = 0usize;
    let mut seen = std::collections::HashSet::with_capacity(count);
    for _ in 0..count {
        let rec = central.get(..46).ok_or("truncated ZIP directory record")?;
        if rec.get(..4) != Some(b"PK\x01\x02") || jar_u16(rec, 34) != Some(0) {
            return Err("unsupported ZIP directory record".into());
        }
        let flags = jar_u16(rec, 8).ok_or("bad flags")?;
        let method = jar_u16(rec, 10).ok_or("bad method")?;
        let crc = jar_u32(rec, 16).ok_or("bad CRC")?;
        let compressed = jar_u32(rec, 20).ok_or("bad compressed size")?;
        let size = jar_u32(rec, 24).ok_or("bad expanded size")?;
        let name_end = 46usize
            .checked_add(jar_u16(rec, 28).ok_or("bad name length")?)
            .ok_or("bad record length")?;
        let name_bytes = central.get(46..name_end).ok_or("truncated ZIP filename")?;
        let next = name_end
            .checked_add(jar_u16(rec, 30).ok_or("bad extra length")?)
            .and_then(|n| n.checked_add(jar_u16(rec, 32)?))
            .ok_or("bad record length")?;
        central = central.get(next..).ok_or("truncated ZIP extra fields")?;
        if !matches!(method, 0 | 8) || flags & !(0x0808 | if method == 8 { 6 } else { 0 }) != 0 {
            return Err("unsupported ZIP compression or flags".into());
        }
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| "non-UTF8 ZIP path")?
            .replace('\\', "/");
        let plain = name.strip_suffix('/').unwrap_or(&name);
        if plain.is_empty()
            || plain.contains(['\0', ':'])
            || plain.split('/').any(|p| matches!(p, "" | "." | ".."))
            || !seen.insert(plain.to_owned())
        {
            return Err("unsafe or duplicate ZIP path".into());
        }
        if !wanted(&name) || name.ends_with('/') {
            continue;
        }
        if size > max_entry || size > max_total - total {
            return Err("selected JAR data exceeds size limits".into());
        }
        total += size;
        let offset = jar_u32(rec, 42).ok_or("bad local header offset")?;
        let local = bytes
            .get(offset..start)
            .ok_or("invalid local header offset")?;
        let hdr = local.get(..30).ok_or("truncated local header")?;
        let data_at = 30usize
            .checked_add(jar_u16(hdr, 26).ok_or("bad local name")?)
            .and_then(|n| n.checked_add(jar_u16(hdr, 28)?))
            .ok_or("bad local header length")?;
        if hdr.get(..4) != Some(b"PK\x03\x04")
            || jar_u16(hdr, 6) != Some(flags)
            || jar_u16(hdr, 8) != Some(method)
            || local.get(30..30 + jar_u16(hdr, 26).ok_or("bad local name")?) != Some(name_bytes)
        {
            return Err("local and central ZIP headers disagree".into());
        }
        let data = local
            .get(data_at..)
            .and_then(|d| d.get(..compressed))
            .ok_or("truncated ZIP data")?;
        let body = if method == 0 {
            if compressed != size {
                return Err("stored entry size mismatch".into());
            }
            data.to_vec()
        } else {
            let mut decoder = flate2::read::DeflateDecoder::new(data);
            let mut body = Vec::with_capacity(size);
            std::io::Read::take(&mut decoder, size as u64 + 1)
                .read_to_end(&mut body)
                .map_err(|e| format!("deflate error: {e}"))?;
            if body.len() != size || decoder.total_in() != data.len() as u64 {
                return Err("deflate size mismatch".into());
            }
            body
        };
        let mut checksum = flate2::Crc::new();
        checksum.update(&body);
        if checksum.sum() as usize != crc {
            return Err("ZIP CRC mismatch".into());
        }
        result.push((name, body));
    }
    if !central.is_empty() {
        return Err("unexpected ZIP directory tail".into());
    }
    result.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}

#[cfg(test)]
mod jar_datapack_tests {
    use super::*;
    use std::io::Write;

    fn zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, body, deflate) in entries {
            let name_bytes = name.as_bytes();
            let packed = if *deflate {
                let mut encoder =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(body).unwrap();
                encoder.finish().unwrap()
            } else {
                body.to_vec()
            };
            let method = if *deflate { 8u16 } else { 0 };
            let offset = out.len();
            let mut crc = flate2::Crc::new();
            crc.update(body);
            let crc = crc.sum();
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name_bytes);
            out.extend_from_slice(&packed);
            central.extend_from_slice(b"PK\x01\x02");
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0; 4]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 8]);
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(offset as u32).to_le_bytes());
            central.extend_from_slice(name_bytes);
        }
        let start = out.len();
        out.extend_from_slice(&central);
        let central_len = central.len();
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central_len as u32).to_le_bytes());
        out.extend_from_slice(&(start as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn fixture_entries() -> Vec<(&'static str, &'static [u8], bool)> {
        vec![
            (
                "data/minecraft/recipe/crafting_table.json",
                br#"{"type":"minecraft:crafting_shaped"}"#,
                true,
            ),
            (
                "data/minecraft/tags/item/test.json",
                br#"{"values":["minecraft:stone"]}"#,
                false,
            ),
            (
                "data/minecraft/loot_table/chests/simple_dungeon.json",
                br#"{"pools":[]}"#,
                true,
            ),
            (
                "data/minecraft/advancement/story/root.json",
                br#"{"criteria":{}}"#,
                false,
            ),
            (
                "data/minecraft/dimension_type/overworld.json",
                br#"{"min_y":-64}"#,
                true,
            ),
        ]
    }

    #[test]
    fn safe_relative_paths_reject_traversal() {
        assert!(safe_data_relative("minecraft/recipe/a.json").is_some());
        for path in [
            "../secret",
            "minecraft/../secret",
            "/data/x",
            "minecraft\\x",
            "minecraft:x",
        ] {
            assert!(safe_data_relative(path).is_none(), "{path}");
        }
    }

    #[test]
    fn entry_and_archive_size_caps_are_enforced() {
        assert!(
            jar_selected_entries(&vec![0; VANILLA_JAR_MAX_BYTES as usize + 1], |_| true).is_err()
        );
    }

    #[test]
    fn reads_direct_client_style_jar_and_summarizes_categories() {
        let source = fixture_entries();
        let archive = zip(&source
            .iter()
            .map(|(p, b, d)| (*p, *b, *d))
            .collect::<Vec<_>>());
        let data = VanillaJarDataFallback::from_archive(&archive).unwrap();
        assert_eq!(
            data.read_data_entry("minecraft/recipe/crafting_table.json"),
            Some(br#"{"type":"minecraft:crafting_shaped"}"#.as_slice())
        );
        assert_eq!(
            data.read_data_entry("minecraft/loot_table/chests/simple_dungeon.json"),
            Some(br#"{"pools":[]}"#.as_slice())
        );
        assert_eq!(
            data.list_prefix("minecraft/tags/item/"),
            vec!["minecraft/tags/item/test.json"]
        );
        let summary = data.summary();
        assert_eq!(summary.recipes, 1);
        assert_eq!(summary.tags, 1);
        assert_eq!(summary.loot_tables, 1);
        assert_eq!(summary.advancements, 1);
        assert_eq!(summary.registries, 1);
    }

    #[test]
    fn unwraps_official_bundler_server_jar() {
        let source = fixture_entries();
        let inner = zip(&source
            .iter()
            .map(|(p, b, d)| (*p, *b, *d))
            .collect::<Vec<_>>());
        let outer = zip(&[(BUNDLER_SERVER_JAR, inner.as_slice(), false)]);
        let data = VanillaJarDataFallback::from_archive(&outer).unwrap();
        assert_eq!(
            data.read_data_entry("minecraft/dimension_type/overworld.json"),
            Some(br#"{"min_y":-64}"#.as_slice())
        );
    }

    #[test]
    fn absent_directory_uses_jar_in_reload_and_existing_vanilla_directory_wins() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "megu-jar-datapack-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let base = root.join("datapacks");
        std::fs::create_dir_all(&base).unwrap();
        let source = fixture_entries();
        let archive = zip(&source
            .iter()
            .map(|(p, b, d)| (*p, *b, *d))
            .collect::<Vec<_>>());
        let jar = root.join("fallback.jar");
        std::fs::write(&jar, archive).unwrap();
        let previous = std::env::var_os("MEGU_VANILLA_JAR");
        std::env::set_var("MEGU_VANILLA_JAR", &jar);
        assert_eq!(reload_datapacks(&base, None, false).unwrap(), 1);
        assert!(packs()
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.name == "vanilla_jar"));
        let vanilla = base.join("vanilla");
        std::fs::create_dir_all(vanilla.join("data/minecraft/tags/item")).unwrap();
        std::fs::write(
            vanilla.join("pack.mcmeta"),
            r#"{"pack":{"pack_format":48,"description":"test"}}"#,
        )
        .unwrap();
        std::fs::write(
            vanilla.join("data/minecraft/tags/item/disk.json"),
            r#"{"values":["minecraft:dirt"]}"#,
        )
        .unwrap();
        assert_eq!(reload_datapacks(&base, None, false).unwrap(), 1);
        assert!(!packs()
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.name == "vanilla_jar"));
        assert!(datapack_registry()
            .lock()
            .unwrap()
            .tags
            .get("item")
            .is_some_and(|tags| tags.contains_key("minecraft:disk")));
        if let Some(value) = previous {
            std::env::set_var("MEGU_VANILLA_JAR", value);
        } else {
            std::env::remove_var("MEGU_VANILLA_JAR");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
