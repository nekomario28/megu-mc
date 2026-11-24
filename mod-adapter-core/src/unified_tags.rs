// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Unified cross-mod tag index (`XMOD1-CORE`): one merged item/block/fluid
//! tag table across every loaded mod JAR — including bounded Jar-in-Jar
//! inner JARs — datapacks, and the Vanilla 1.21.1 conventional baseline.
//!
//! - Three domains ([`TagDomain::Item`], [`TagDomain::Block`],
//!   [`TagDomain::Fluid`]) classified from
//!   `data/<ns>/tags/{item,items,block,blocks,fluid,fluids}/**.json`.
//! - Recursive nested-tag expansion (`#c:ingots` containing
//!   `#c:ingots/iron`) with deterministic sorted resolution, a strict
//!   [`MAX_UNIFIED_TAG_DEPTH`] bound, and fail-closed cycle detection —
//!   cyclic graphs error instead of looping, and [`matches`](Self::matches)
//!   reports `false`.
//! - Bidirectional NeoForge 1.21.1 `#c:*` ↔ legacy `#forge:*` /
//!   `#neoforge:*` common-tag aliasing: legacy spellings canonicalize onto
//!   `c:` at both merge and query time, so a mod that only registered
//!   `forge:plates/brass` still answers `#c:plates/brass` queries and
//!   vice-versa. Arbitrary paths are covered — not only the ten
//!   conventional families.
//! - `"replace": true` and `{"id": ..., "required": false}` entries follow
//!   the Vanilla/NeoForge tag spec, shared with
//!   [`crate::loader_extensions::parse_tag_file_json`].
//! - Interop helpers export a flattened
//!   [`crate::culinary_adapter::TagIndex`] and ingest the flat
//!   `auto_adapter` tag table so existing APIs keep working while gaining
//!   nested expansion and `c:`/`forge:` aliasing.

use std::collections::{BTreeMap, BTreeSet};

use crate::loader_extensions::{
    parse_tag_file_json, ConventionalTagRegistry, TagFile, TagFileEntry, TagRef,
    MAX_JAR_IN_JAR_DEPTH, MAX_JAR_IN_JAR_NESTED_JARS, MAX_JAR_IN_JAR_TOTAL_BYTES,
};
use crate::modpack;

/// Deepest `#tag` reference chain expanded during resolution. NeoForge's
/// own recursion guard is far looser; 16 covers every real datapack chain
/// while keeping adversarial cyclic graphs cheap to reject.
pub const MAX_UNIFIED_TAG_DEPTH: usize = 16;

/// Cumulative decompressed-byte budget for one [`ingest_jar_bytes`] scan —
/// the same policy the culinary and mixin scanners apply so a hostile
/// archive cannot OOM the host.
///
/// [`ingest_jar_bytes`]: UnifiedTagIndex::ingest_jar_bytes
const MAX_UNIFIED_TAG_SCAN_BYTES: u64 = 256 * 1024 * 1024;

/// Tag registry domains the index keeps separate: items, blocks, and
/// fluids live in distinct Minecraft registries, so `c:ores` the block tag
/// and `c:ores` the item tag never alias each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TagDomain {
    /// `data/<ns>/tags/item(s)/*.json`
    Item,
    /// `data/<ns>/tags/block(s)/*.json`
    Block,
    /// `data/<ns>/tags/fluid(s)/*.json`
    Fluid,
}

impl TagDomain {
    /// `data/<ns>/tags/` subdirectory spellings feeding this domain.
    pub fn dirs(self) -> &'static [&'static str] {
        match self {
            Self::Item => &["item", "items"],
            Self::Block => &["block", "blocks"],
            Self::Fluid => &["fluid", "fluids"],
        }
    }

    /// Singular registry name (`"item"`, `"block"`, `"fluid"`).
    pub fn registry_name(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Block => "block",
            Self::Fluid => "fluid",
        }
    }
}

/// Failure of [`UnifiedTagIndex`] parsing, ingestion, or resolution.
/// Fail-closed throughout: a malformed file, over-deep nesting, or a
/// cyclic reference aborts the operation rather than producing a
/// half-resolved tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnifiedTagError {
    /// The JAR itself is not a readable archive, or a Jar-in-Jar bound was
    /// exceeded.
    Archive(String),
    /// A `*.json` member or tag document did not parse.
    Json(String),
    /// A *required* nested `#tag` reference points at a tag no loaded file
    /// defined.
    MissingTag(String),
    /// Nested `#tag` expansion hit a cycle (fail-closed, no infinite loop).
    Cycle(String),
    /// Nested `#tag` expansion exceeded [`MAX_UNIFIED_TAG_DEPTH`].
    DepthExceeded(String),
}

impl std::fmt::Display for UnifiedTagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Archive(m)
            | Self::Json(m)
            | Self::MissingTag(m)
            | Self::Cycle(m)
            | Self::DepthExceeded(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for UnifiedTagError {}

impl From<modpack::ModpackError> for UnifiedTagError {
    fn from(e: modpack::ModpackError) -> Self {
        Self::Archive(e.reason().to_string())
    }
}

impl From<crate::loader_extensions::LoaderExtensionError> for UnifiedTagError {
    fn from(e: crate::loader_extensions::LoaderExtensionError) -> Self {
        Self::Json(e.reason().to_string())
    }
}

/// Prepend `minecraft:` to a bare path id; namespaced ids pass through.
fn normalize_id(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains(':') {
        raw.to_string()
    } else {
        format!("minecraft:{raw}")
    }
}

/// Canonicalize a common-tag id onto the NeoForge 1.21.1 `c:` namespace:
/// `#forge:<path>` and `#neoforge:<path>` (with or without the leading
/// `#`) map onto `c:<path>` for **any** path — `forge:ingots/iron` →
/// `c:ingots/iron`, `forge:plates/brass` → `c:plates/brass`,
/// `forge:chests` → `c:chests`. Every other namespace is returned
/// unchanged, so `minecraft:*` and arbitrary mod namespaces are never
/// rewritten.
///
/// Canonicalization runs at merge time **and** at query time, which makes
/// the `c:` ↔ `forge:`/`neoforge:` aliasing bidirectional for free: a
/// file loaded under `data/forge/tags/...` and a file loaded under
/// `data/c/tags/...` land in the same merged entry, and either spelling
/// resolves it.
pub fn canonical_common_tag_id(id: &str) -> String {
    let id = id.strip_prefix('#').unwrap_or(id);
    match id.split_once(':') {
        Some(("forge", path)) | Some(("neoforge", path)) => format!("c:{path}"),
        _ => id.to_string(),
    }
}

/// `(domain, tag_id)` carried by a `data/<ns>/tags/<kind>/<path>.json`
/// member, or `None` when the member is not an item/block/fluid tag file
/// (entity-type, game-event, and other tag kinds stay out of this index).
pub fn tag_id_for_member_path(rel_path: &str) -> Option<(TagDomain, String)> {
    let rest = rel_path.strip_prefix("data/")?;
    let (ns, rest) = rest.split_once('/')?;
    let rest = rest.strip_prefix("tags/")?;
    for domain in [TagDomain::Item, TagDomain::Block, TagDomain::Fluid] {
        for dir in domain.dirs() {
            let prefix = format!("{dir}/");
            if let Some(stem) = rest
                .strip_prefix(prefix.as_str())
                .and_then(|s| s.strip_suffix(".json"))
            {
                if ns.is_empty() || stem.is_empty() {
                    return None;
                }
                return Some((domain, format!("{ns}:{stem}")));
            }
        }
    }
    None
}

/// Per-domain merged tag table, keyed by canonical (`c:`-normalized) id.
#[derive(Clone, Debug, Default)]
struct DomainTable {
    files: BTreeMap<String, TagFile>,
}

/// Per-JAR ingest accounting returned by
/// [`UnifiedTagIndex::ingest_jar_bytes`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TagJarStats {
    /// `data/*/tags/{item,items,block,blocks,fluid,fluids}/**.json`
    /// members merged (including from nested JARs).
    pub tag_files: usize,
    /// `META-INF/jarjar/*.jar` members recursively ingested.
    pub nested_jars: usize,
    /// Decompressed member bytes scanned (bounded by
    /// [`MAX_UNIFIED_TAG_SCAN_BYTES`]).
    pub bytes_scanned: u64,
}

/// Merged item/block/fluid tag index across all loaded mods, datapacks,
/// JiJ inner JARs, and (optionally) the Vanilla 1.21.1 conventional
/// baseline.
#[derive(Clone, Debug, Default)]
pub struct UnifiedTagIndex {
    tables: [DomainTable; 3],
}

impl UnifiedTagIndex {
    /// Empty index — no baseline members.
    pub fn new() -> Self {
        Self::default()
    }

    /// Index seeded with the exact Vanilla 1.21.1 `c:` conventional
    /// baseline member sets (item domain) from
    /// [`ConventionalTagRegistry::vanilla_1_21_1_baseline`].
    pub fn vanilla_1_21_1_baseline() -> Self {
        let mut out = Self::new();
        let baseline = ConventionalTagRegistry::vanilla_1_21_1_baseline();
        for tag_id in baseline.families() {
            if let Some(members) = baseline.members(&tag_id) {
                out.merge_values(TagDomain::Item, &tag_id, members.iter().map(String::as_str));
            }
        }
        out
    }

    fn table(&self, domain: TagDomain) -> &DomainTable {
        &self.tables[domain as usize]
    }

    fn table_mut(&mut self, domain: TagDomain) -> &mut DomainTable {
        &mut self.tables[domain as usize]
    }

    /// Merge a parsed tag file under `tag_id` into `domain`. The id is
    /// canonicalized (`forge:`/`neoforge:` → `c:`); `replace: true`
    /// discards earlier data for that tag (datapack load-order semantics).
    pub fn merge_tag_file(&mut self, domain: TagDomain, tag_id: &str, file: &TagFile) {
        let canon = canonical_common_tag_id(tag_id);
        let entry = self.table_mut(domain).files.entry(canon).or_default();
        if file.replace {
            entry.entries.clear();
            entry.replace = true;
        }
        entry.entries.extend(file.entries.iter().cloned());
    }

    /// Merge raw `values`-style strings under `tag_id`: entries prefixed
    /// `#` become required nested references, everything else a required
    /// literal id.
    pub fn merge_values<I, S>(&mut self, domain: TagDomain, tag_id: &str, values: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let file = TagFile {
            replace: false,
            entries: values
                .into_iter()
                .map(|v| {
                    let text = v.as_ref();
                    match text.strip_prefix('#') {
                        Some(nested) => {
                            TagFileEntry::Required(TagRef::NestedTag(nested.to_string()))
                        }
                        None => TagFileEntry::Required(TagRef::Id(text.to_string())),
                    }
                })
                .collect(),
        };
        self.merge_tag_file(domain, tag_id, &file);
    }

    /// Merge an existing [`crate::culinary_adapter::TagIndex`]
    /// (`tag_id -> {"id"|"#tag"}` members) into `domain`. Nested `#`
    /// members become required references — the same semantics
    /// `culinary_adapter::tag_contains` gives them.
    pub fn merge_tag_index(&mut self, domain: TagDomain, tags: &crate::culinary_adapter::TagIndex) {
        for (tag_id, members) in tags {
            self.merge_values(domain, tag_id, members.iter().map(String::as_str));
        }
    }

    /// Merge the flat `auto_adapter` tag table
    /// (`BTreeMap<"ns:<kind>/<path>", Vec<values>>`, where `<kind>` is the
    /// `tags/` subdirectory such as `items`, `blocks`, or `fluids`).
    /// Entries whose kind is not item/block/fluid — or that carry no
    /// `<kind>/<path>` split — are skipped. Returns the number of tag ids
    /// merged.
    pub fn merge_auto_adapter_tags(&mut self, tags: &BTreeMap<String, Vec<String>>) -> usize {
        let mut merged = 0usize;
        for (id, values) in tags {
            let Some((ns, path)) = id.split_once(':') else {
                continue;
            };
            let Some((kind, stem)) = path.split_once('/') else {
                continue;
            };
            let domain = [TagDomain::Item, TagDomain::Block, TagDomain::Fluid]
                .into_iter()
                .find(|d| d.dirs().contains(&kind));
            let Some(domain) = domain else {
                continue;
            };
            if ns.is_empty() || stem.is_empty() {
                continue;
            }
            self.merge_values(domain, &format!("{ns}:{stem}"), values.iter());
            merged += 1;
        }
        merged
    }

    /// Classify `rel_path` as `data/<ns>/tags/<kind>/<path>.json` and, when
    /// it names an item/block/fluid tag, parse + merge it. Returns
    /// `Ok(Some(domain))` when consumed, `Ok(None)` for other members.
    pub fn ingest_member(
        &mut self,
        rel_path: &str,
        bytes: &[u8],
    ) -> Result<Option<TagDomain>, UnifiedTagError> {
        let Some((domain, tag_id)) = tag_id_for_member_path(rel_path) else {
            return Ok(None);
        };
        let file = parse_tag_file_json(bytes, rel_path)?;
        self.merge_tag_file(domain, &tag_id, &file);
        Ok(Some(domain))
    }

    /// Scan a whole mod/datapack JAR: every item/block/fluid tag member is
    /// merged, and `META-INF/jarjar/*.jar` inner JARs are recursively
    /// ingested under the shared Jar-in-Jar bounds
    /// ([`MAX_JAR_IN_JAR_DEPTH`], [`MAX_JAR_IN_JAR_NESTED_JARS`],
    /// [`MAX_JAR_IN_JAR_TOTAL_BYTES`], and the decompressed scan budget).
    /// Fail-closed: a corrupt archive, over-deep nesting, or a malformed
    /// tag document aborts the ingest.
    pub fn ingest_jar_bytes(
        &mut self,
        jar_bytes: &[u8],
        display: &str,
    ) -> Result<TagJarStats, UnifiedTagError> {
        let mut stats = TagJarStats::default();
        self.ingest_jar_inner(jar_bytes, display, 0, &mut stats)?;
        Ok(stats)
    }

    fn ingest_jar_inner(
        &mut self,
        jar_bytes: &[u8],
        display: &str,
        depth: usize,
        stats: &mut TagJarStats,
    ) -> Result<(), UnifiedTagError> {
        let entries = modpack::zip_member_list(jar_bytes, display)?;
        let mut nested: Vec<(String, Vec<u8>)> = Vec::new();
        for entry in &entries {
            if entry.name.starts_with("META-INF/jarjar/") && entry.name.ends_with(".jar") {
                if depth >= MAX_JAR_IN_JAR_DEPTH {
                    return Err(UnifiedTagError::Archive(format!(
                        "{display}: jarjar member {} nested deeper than {MAX_JAR_IN_JAR_DEPTH} levels",
                        entry.name
                    )));
                }
                let bytes = modpack::read_zip_member(jar_bytes, entry, &entry.name)?;
                stats.bytes_scanned = stats
                    .bytes_scanned
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| {
                        UnifiedTagError::Archive(format!("{display}: scan byte budget overflow"))
                    })?;
                nested.push((entry.name.clone(), bytes));
                continue;
            }
            let Some((domain, _)) = tag_id_for_member_path(&entry.name) else {
                continue;
            };
            let bytes = modpack::read_zip_member(jar_bytes, entry, &entry.name)?;
            stats.bytes_scanned = stats
                .bytes_scanned
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| {
                    UnifiedTagError::Archive(format!("{display}: scan byte budget overflow"))
                })?;
            if stats.bytes_scanned > MAX_UNIFIED_TAG_SCAN_BYTES {
                return Err(UnifiedTagError::Archive(format!(
                    "{display}: tag scan byte budget exceeded"
                )));
            }
            self.ingest_member(&entry.name, &bytes)?;
            stats.tag_files += 1;
            let _ = domain;
        }
        for (name, bytes) in nested {
            if stats.nested_jars >= MAX_JAR_IN_JAR_NESTED_JARS
                || stats.bytes_scanned > MAX_JAR_IN_JAR_TOTAL_BYTES
            {
                return Err(UnifiedTagError::Archive(format!(
                    "{display}: jarjar bound exceeded ({name})"
                )));
            }
            stats.nested_jars += 1;
            self.ingest_jar_inner(&bytes, &name, depth + 1, stats)?;
        }
        Ok(())
    }

    /// Fully resolved member set for `tag_id` in `domain` — literal ids
    /// plus transitively expanded `#` references through `c:`/`forge:`
    /// aliases, sorted and deduplicated. `required: false` literal ids are
    /// skipped unless the live registry reports them — supply a presence
    /// predicate via [`resolve_with`](Self::resolve_with). A tag no file
    /// defined resolves to the empty set.
    pub fn resolve(
        &self,
        domain: TagDomain,
        tag_id: &str,
    ) -> Result<BTreeSet<String>, UnifiedTagError> {
        self.resolve_with(domain, tag_id, &|_| false)
    }

    /// [`resolve`](Self::resolve) consulting `known(id)` for
    /// `required: false` literal ids.
    pub fn resolve_with(
        &self,
        domain: TagDomain,
        tag_id: &str,
        known: &dyn Fn(&str) -> bool,
    ) -> Result<BTreeSet<String>, UnifiedTagError> {
        let mut out = BTreeSet::new();
        let mut visiting = BTreeSet::new();
        self.resolve_into(
            domain,
            &canonical_common_tag_id(tag_id),
            0,
            &mut visiting,
            &mut out,
            known,
        )?;
        Ok(out)
    }

    fn resolve_into(
        &self,
        domain: TagDomain,
        tag_id: &str,
        depth: usize,
        visiting: &mut BTreeSet<String>,
        out: &mut BTreeSet<String>,
        known: &dyn Fn(&str) -> bool,
    ) -> Result<(), UnifiedTagError> {
        if depth > MAX_UNIFIED_TAG_DEPTH {
            return Err(UnifiedTagError::DepthExceeded(format!(
                "{} tag '#{tag_id}': nested reference depth exceeds {MAX_UNIFIED_TAG_DEPTH}",
                domain.registry_name()
            )));
        }
        if !visiting.insert(tag_id.to_string()) {
            return Err(UnifiedTagError::Cycle(format!(
                "{} tag '#{tag_id}': cyclic nested tag reference",
                domain.registry_name()
            )));
        }
        if let Some(file) = self.table(domain).files.get(tag_id) {
            for entry in &file.entries {
                match entry {
                    TagFileEntry::Required(TagRef::Id(id)) => {
                        out.insert(normalize_id(id));
                    }
                    TagFileEntry::Optional(TagRef::Id(id)) => {
                        let id = normalize_id(id);
                        if known(&id) {
                            out.insert(id);
                        }
                    }
                    TagFileEntry::Required(TagRef::NestedTag(nested)) => {
                        let nested = canonical_common_tag_id(nested);
                        if !self.table(domain).files.contains_key(&nested) {
                            return Err(UnifiedTagError::MissingTag(format!(
                                "{} tag '#{tag_id}': required nested tag '#{nested}' is absent",
                                domain.registry_name()
                            )));
                        }
                        self.resolve_into(domain, &nested, depth + 1, visiting, out, known)?;
                    }
                    TagFileEntry::Optional(TagRef::NestedTag(nested)) => {
                        let nested = canonical_common_tag_id(nested);
                        if self.table(domain).files.contains_key(&nested) {
                            self.resolve_into(domain, &nested, depth + 1, visiting, out, known)?;
                        }
                    }
                }
            }
        }
        visiting.remove(tag_id);
        Ok(())
    }

    /// Is `object_id` a member of `tag_id` in `domain`? Fail-closed:
    /// unresolved required references, cycles, and over-deep chains all
    /// report `false`. `tag_id` may carry a leading `#` and either the
    /// `c:` or legacy `forge:`/`neoforge:` spelling; `object_id` may be a
    /// bare path (normalized onto `minecraft:`).
    pub fn matches(&self, domain: TagDomain, tag_id: &str, object_id: &str) -> bool {
        let Ok(members) = self.resolve(domain, tag_id) else {
            return false;
        };
        members.contains(object_id) || members.contains(&normalize_id(object_id))
    }

    /// [`matches`](Self::matches) consulting `known(id)` for
    /// `required: false` literal members.
    pub fn matches_with(
        &self,
        domain: TagDomain,
        tag_id: &str,
        object_id: &str,
        known: &dyn Fn(&str) -> bool,
    ) -> bool {
        let Ok(members) = self.resolve_with(domain, tag_id, known) else {
            return false;
        };
        members.contains(object_id) || members.contains(&normalize_id(object_id))
    }

    /// Loaded canonical tag ids in `domain`, sorted.
    pub fn tag_ids(&self, domain: TagDomain) -> Vec<String> {
        self.table(domain).files.keys().cloned().collect()
    }

    /// Flatten `domain` into a [`crate::culinary_adapter::TagIndex`]:
    /// every canonical tag resolves to its full member set, and `c:` tags
    /// are also exported under their `forge:` and `neoforge:` spellings so
    /// lookups through `culinary_adapter::tag_contains` hit on either
    /// convention. Tags whose resolution fails (cycle / missing required
    /// reference / depth) degrade to their direct literal members —
    /// fail-closed, never a panic.
    pub fn export_flat(&self, domain: TagDomain) -> crate::culinary_adapter::TagIndex {
        let mut out: crate::culinary_adapter::TagIndex = BTreeMap::new();
        for tag_id in self.table(domain).files.keys() {
            let resolved = self.resolve(domain, tag_id).unwrap_or_else(|_| {
                let mut direct = BTreeSet::new();
                if let Some(file) = self.table(domain).files.get(tag_id) {
                    for entry in &file.entries {
                        if let TagFileEntry::Required(TagRef::Id(id))
                        | TagFileEntry::Optional(TagRef::Id(id)) = entry
                        {
                            direct.insert(normalize_id(id));
                        }
                    }
                }
                direct
            });
            if let Some(path) = tag_id.strip_prefix("c:") {
                out.insert(format!("forge:{path}"), resolved.clone());
                out.insert(format!("neoforge:{path}"), resolved.clone());
            }
            out.insert(tag_id.clone(), resolved);
        }
        out
    }
}
