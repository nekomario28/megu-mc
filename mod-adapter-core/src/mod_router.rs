// SPDX-License-Identifier: LGPL-3.0-or-later
#![deny(unsafe_code)]

//! Bounded, zero-JVM security inspection and structural capability routing.
//! All class files are inspected before a catalog match can skip an archive.

use std::collections::BTreeSet;

use crate::fallback::BUILTIN_SUPERSEDED_PERF_MODS;
use crate::modpack::{read_zip_member, zip_member_list};

/// Production mod JARs routinely carry thousands of classes and tens of
/// thousands of members (a current NeoForge `create` release ships ~2,700
/// classes across ~12,500 members), so the bounds below sit well past the
/// largest real packs while still rejecting member-count bombs and
/// archives whose decompressed footprint is unreasonably large.
const MAX_CLASSES: usize = 16_384;
const MAX_MEMBERS: usize = 32_768;
const MAX_SCANNED_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecurityThreatKind {
    EmbeddedNativeOrExecutable,
    ProcessExecution,
    NativeLibraryLoad,
    ForeignLinkerAccess,
    UnsafeMemoryAccess,
    DirectNetworkAccess,
    DynamicCodeLoading,
    JndiLookup,
}

impl SecurityThreatKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EmbeddedNativeOrExecutable => "embedded_native_or_executable",
            Self::ProcessExecution => "process_execution",
            Self::NativeLibraryLoad => "native_library_load",
            Self::ForeignLinkerAccess => "foreign_linker_access",
            Self::UnsafeMemoryAccess => "unsafe_memory_access",
            Self::DirectNetworkAccess => "direct_network_access",
            Self::DynamicCodeLoading => "dynamic_code_loading",
            Self::JndiLookup => "jndi_lookup",
        }
    }

    /// Returns `true` when this finding must hard-quarantine the entire JAR.
    /// `DirectNetworkAccess` (update checkers, patreon credits, shaded Lucene
    /// sockets) and `ForeignLinkerAccess` (`java.lang.foreign.Linker` /
    /// `SymbolLookup`, which JDK 21+ blocks by default unless
    /// `--enable-native-access` is granted — used by shaded Apache Lucene
    /// `PosixNativeAccess` in JEI and Ars Nouveau) are surfaced as
    /// sandbox-enforced advisories rather than hard-quarantining benign mods.
    pub fn is_hard_quarantine(self) -> bool {
        !matches!(self, Self::DirectNetworkAccess | Self::ForeignLinkerAccess)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecurityThreat {
    pub kind: SecurityThreatKind,
    pub member: String,
    pub indicator: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSubsystem {
    WgpuRenderBackend,
    LightEngine,
    ServerChunkAndNetEngine,
    CompactRegistryAndMemory,
}

impl NativeSubsystem {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WgpuRenderBackend => "wgpu_render_backend",
            Self::LightEngine => "light_engine",
            Self::ServerChunkAndNetEngine => "native_server_engine",
            Self::CompactRegistryAndMemory => "compact_registry_storage",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteMatchKind {
    KnownCatalogId,
    BehavioralCapability,
}

impl RouteMatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KnownCatalogId => "known_catalog_id",
            Self::BehavioralCapability => "behavioral_capability",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilitySignals {
    pub render_engine_hooks: Vec<String>,
    pub light_engine_hooks: Vec<String>,
    pub server_engine_hooks: Vec<String>,
    pub has_gameplay_content: bool,
    pub class_files_inspected: usize,
    /// Classes whose constant pool carried at least one render-engine
    /// hook. The behavioral route only fires when these dominate the
    /// archive — a handful of client classes importing `RenderSystem` or
    /// `com/mojang/blaze3d/` is normal for any mod that draws a HUD icon
    /// or screen and is not an engine-replacement signature.
    pub render_hook_classes: usize,
    /// Same accounting for the light-engine hook family.
    pub light_hook_classes: usize,
    /// Same accounting for the server chunk/network-engine hook family.
    pub server_hook_classes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModRouteDecision {
    SecurityQuarantine {
        threats: Vec<SecurityThreat>,
    },
    RouteToNativeSubsystem {
        subsystem: NativeSubsystem,
        matched_by: RouteMatchKind,
        evidence: Vec<String>,
    },
    HybridSplit {
        client_render_fallback: NativeSubsystem,
        evidence: Vec<String>,
    },
    StandardPipeline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JarRoutingReport {
    pub decision: ModRouteDecision,
    pub signals: CapabilitySignals,
    pub security_advisories: Vec<SecurityThreat>,
}

/// Check archive members and every class constant pool without executing Java.
/// An unreadable archive, malformed class, or exceeded scan budget is an error;
/// the caller must not fall back to executing its contents.
pub fn inspect_jar_security_and_capabilities(
    jar_bytes: &[u8],
    display: &str,
) -> Result<JarRoutingReport, String> {
    let entries = zip_member_list(jar_bytes, display).map_err(|e| e.reason().to_string())?;
    if entries.len() > MAX_MEMBERS {
        return Err(format!(
            "{display}: security scan member count exceeds {MAX_MEMBERS}"
        ));
    }
    let mut signals = CapabilitySignals::default();
    let mut threats = Vec::new();
    let mut render_hooks = BTreeSet::new();
    let mut light_hooks = BTreeSet::new();
    let mut server_hooks = BTreeSet::new();
    let mut inspected_bytes = 0usize;
    let mut hook_classes = HookClassCounts::default();

    for entry in &entries {
        let name = entry.name.as_str();
        validate_member_name(name).map_err(|why| format!("{display}: {why}: {name}"))?;
        if name.ends_with('/') {
            continue;
        }
        // Validate every member, including resources in a known superseded
        // mod, so encrypted members and CRC failures cannot bypass the scan.
        let member_bytes =
            read_zip_member(jar_bytes, entry, display).map_err(|e| e.reason().to_string())?;
        inspected_bytes = inspected_bytes
            .checked_add(member_bytes.len())
            .ok_or_else(|| format!("{display}: security scan size overflow"))?;
        if inspected_bytes > MAX_SCANNED_BYTES {
            return Err(format!(
                "{display}: security scan exceeds {} MiB of archive data",
                MAX_SCANNED_BYTES / (1024 * 1024)
            ));
        }
        let lower = name.to_ascii_lowercase();
        if [
            ".so", ".dll", ".dylib", ".jnilib", ".exe", ".bat", ".cmd", ".ps1", ".sh", ".msi",
            ".vbs", ".scr",
        ]
        .iter()
        .any(|ext| lower.ends_with(ext))
        {
            threats.push(SecurityThreat {
                kind: SecurityThreatKind::EmbeddedNativeOrExecutable,
                member: name.to_string(),
                indicator: "embedded native library or executable member".to_string(),
            });
        }
        if name == "megu-bundle.json" {
            signals.has_gameplay_content = true;
        }
        if let Some(rest) = name.strip_prefix("data/") {
            if let Some((namespace, category_path)) = rest.split_once('/') {
                if !namespace.is_empty() && lower.ends_with(".json") {
                    if let Some(category) = category_path.split('/').next() {
                        if matches!(
                            category,
                            "recipe"
                                | "recipes"
                                | "tags"
                                | "tag"
                                | "dimension"
                                | "dimension_type"
                                | "worldgen"
                                | "loot_table"
                                | "loot_tables"
                                | "loot_modifiers"
                                | "advancement"
                                | "advancements"
                                | "data_maps"
                                | "neoforge"
                                | "forge"
                        ) && category_path.contains('/')
                        {
                            signals.has_gameplay_content = true;
                        }
                    }
                }
            }
        }
        if !lower.ends_with(".class") {
            continue;
        }
        if signals.class_files_inspected >= MAX_CLASSES {
            return Err(format!(
                "{display}: security scan class count exceeds {MAX_CLASSES}"
            ));
        }
        signals.class_files_inspected += 1;
        let pool = constant_pool_report(&member_bytes)
            .map_err(|reason| format!("{display}: malformed class {name}: {reason}"))?;
        detect_security_threats(&pool, name, &mut threats);
        let mut class_hits = HookHits::default();
        for text in &pool.utf8 {
            collect_signals(
                text,
                &mut render_hooks,
                &mut light_hooks,
                &mut server_hooks,
                &mut class_hits,
            );
        }
        if class_hits.render {
            hook_classes.render += 1;
        }
        if class_hits.light {
            hook_classes.light += 1;
        }
        if class_hits.server {
            hook_classes.server += 1;
        }
    }

    signals.render_engine_hooks = render_hooks.into_iter().collect();
    signals.light_engine_hooks = light_hooks.into_iter().collect();
    signals.server_engine_hooks = server_hooks.into_iter().collect();
    signals.render_hook_classes = hook_classes.render;
    signals.light_hook_classes = hook_classes.light;
    signals.server_hook_classes = hook_classes.server;
    // A behavioral capability route fires only when hook-bearing classes
    // dominate the archive — the JAR is then, by structure, mostly that
    // engine's code (the way sodium/starlight/C2ME-class replacements
    // look). A gameplay, library, or HUD mod that merely touches
    // `RenderSystem`, `blaze3d`, `ChunkMap`, or `ServerChunkCache` in a
    // few of its classes stays in the standard pipeline.
    let render_dominant = hook_classes.render * 2 > signals.class_files_inspected;
    let light_dominant = hook_classes.light * 2 > signals.class_files_inspected;
    let server_dominant = hook_classes.server * 2 > signals.class_files_inspected;
    let (hard_threats, security_advisories): (Vec<SecurityThreat>, Vec<SecurityThreat>) =
        threats.into_iter().partition(|t| t.kind.is_hard_quarantine());
    let decision = if !hard_threats.is_empty() {
        ModRouteDecision::SecurityQuarantine {
            threats: hard_threats,
        }
    } else if signals.has_gameplay_content && !signals.render_engine_hooks.is_empty() {
        ModRouteDecision::HybridSplit {
            client_render_fallback: NativeSubsystem::WgpuRenderBackend,
            evidence: signals.render_engine_hooks.clone(),
        }
    } else if signals.has_gameplay_content {
        ModRouteDecision::StandardPipeline
    } else if render_dominant {
        capability_route(
            NativeSubsystem::WgpuRenderBackend,
            &signals.render_engine_hooks,
        )
    } else if light_dominant {
        capability_route(NativeSubsystem::LightEngine, &signals.light_engine_hooks)
    } else if server_dominant {
        capability_route(
            NativeSubsystem::ServerChunkAndNetEngine,
            &signals.server_engine_hooks,
        )
    } else {
        ModRouteDecision::StandardPipeline
    };
    Ok(JarRoutingReport {
        decision,
        signals,
        security_advisories,
    })
}

/// Known IDs are considered only after the full static security scan —
/// the archive is always fail-closed validated first — but a verified
/// superseded id resolves its route before the *bytecode* quarantine
/// verdict: `megu` never executes a single byte of these
/// engine-replacement mods, so their constant pools can never run.
/// Embedded native/executable *members* still quarantine regardless: a
/// hostile archive renamed after a known mod must stay reported.
pub fn route_mod_jar(
    jar_bytes: &[u8],
    display: &str,
    declared_or_inferred_id: Option<&str>,
) -> Result<JarRoutingReport, String> {
    let mut report = inspect_jar_security_and_capabilities(jar_bytes, display)?;
    if let Some(id) = declared_or_inferred_id {
        if let Some(known) = BUILTIN_SUPERSEDED_PERF_MODS
            .iter()
            .find(|known| canonical_id(known) == canonical_id(id))
        {
            let hostile_archive = matches!(
                &report.decision,
                ModRouteDecision::SecurityQuarantine { threats }
                    if threats
                        .iter()
                        .any(|t| t.kind == SecurityThreatKind::EmbeddedNativeOrExecutable)
            );
            if !hostile_archive {
                let subsystem = match *known {
                    "starlight" | "scalablelux" | "phosphor" => NativeSubsystem::LightEngine,
                    "c2me" | "krypton" | "servercore" => {
                        NativeSubsystem::ServerChunkAndNetEngine
                    }
                    "ferritecore" | "modernfix" | "memoryleakfix" => {
                        NativeSubsystem::CompactRegistryAndMemory
                    }
                    _ => NativeSubsystem::WgpuRenderBackend,
                };
                report.decision = ModRouteDecision::RouteToNativeSubsystem {
                    subsystem,
                    matched_by: RouteMatchKind::KnownCatalogId,
                    evidence: vec![format!("BUILTIN_SUPERSEDED_PERF_MODS:{known}")],
                };
            }
        }
    }
    Ok(report)
}

fn canonical_id(id: &str) -> String {
    id.chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn capability_route(subsystem: NativeSubsystem, evidence: &[String]) -> ModRouteDecision {
    ModRouteDecision::RouteToNativeSubsystem {
        subsystem,
        matched_by: RouteMatchKind::BehavioralCapability,
        evidence: evidence.to_vec(),
    }
}

pub(crate) fn validate_member_name(name: &str) -> Result<(), &'static str> {
    let path = name.trim_end_matches('/');
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', '\0', '\u{fffd}'])
        || path.as_bytes().get(1) == Some(&b':')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err("unsafe ZIP member path");
    }
    Ok(())
}

struct ClassReader<'a> {
    bytes: &'a [u8],
}

impl<'a> ClassReader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], &'static str> {
        let part = self.bytes.get(..len).ok_or("truncated class file")?;
        self.bytes = &self.bytes[len..];
        Ok(part)
    }

    fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, &'static str> {
        let raw = self.take(2)?;
        Ok(u16::from_be_bytes([raw[0], raw[1]]))
    }

    fn u32(&mut self) -> Result<u32, &'static str> {
        let raw = self.take(4)?;
        Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    fn attributes(&mut self) -> Result<(), &'static str> {
        let count = self.u16()?;
        for _ in 0..count {
            self.u16()?;
            let length = usize::try_from(self.u32()?).map_err(|_| "attribute too large")?;
            self.take(length)?;
        }
        Ok(())
    }
}

/// The constant-pool structures one `.class` resolved for the scan.
/// [`ConstantPoolReport::utf8`] feeds the capability-signal substring
/// scan; security matching uses only structurally resolved entries —
/// [`ConstantPoolReport::class_names`] (real `CONSTANT_Class` names) and
/// [`ConstantPoolReport::method_pairs`] (owner + method name from real
/// `CONSTANT_Methodref`/`CONSTANT_InterfaceMethodref` entries) — so a
/// string literal, type descriptor, or unrelated method name can never
/// fabricate a threat reference.
#[derive(Default)]
struct ConstantPoolReport {
    /// Every `CONSTANT_Utf8` payload, raw bytes.
    utf8: Vec<Vec<u8>>,
    /// Internal names resolved from `CONSTANT_Class` entries.
    class_names: Vec<Vec<u8>>,
    /// `(owner, name)` resolved from `CONSTANT_Methodref` (tag 10) and
    /// `CONSTANT_InterfaceMethodref` (tag 11). `invokedynamic`/`ldc`
    /// method handles are covered too: a `CONSTANT_MethodHandle`'s
    /// `reference_index` always points at one of these entries.
    method_pairs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// One parsed constant-pool slot, keeping the indexes later entries
/// resolve through.
enum CpEntry {
    /// `CONSTANT_Utf8` raw bytes.
    Utf8(Vec<u8>),
    /// `CONSTANT_Class` — `name_index` of a `CONSTANT_Utf8`.
    Class(u16),
    /// `CONSTANT_NameAndType` — `name_index` of a `CONSTANT_Utf8`.
    NameAndType(u16),
    /// `CONSTANT_Methodref`/`CONSTANT_InterfaceMethodref` —
    /// `class_index` + `name_and_type_index`.
    MethodRef(u16, u16),
    /// Every other constant kind (numbers, strings, handles, dynamics).
    Other,
}

/// Parse JVMS 4.4 constant-pool structures, including two-slot constants,
/// then resolve `CONSTANT_Class` names and method-reference owner/name
/// pairs. UTF8 values remain raw bytes so legal modified-UTF8
/// (NUL/surrogates) does not hide ASCII references or get mistaken for
/// invalid UTF-8.
fn constant_pool_report(class: &[u8]) -> Result<ConstantPoolReport, &'static str> {
    let mut reader = ClassReader { bytes: class };
    if reader.u32()? != 0xcafe_babe {
        return Err("invalid class magic");
    }
    reader.u16()?; // minor
    reader.u16()?; // major
    let count = reader.u16()?;
    if count == 0 {
        return Err("invalid constant-pool count");
    }
    let mut entries: Vec<CpEntry> = Vec::with_capacity(usize::from(count));
    entries.push(CpEntry::Other); // pool indexes are 1-based
    let mut index = 1u32;
    while index < u32::from(count) {
        let tag = reader.u8()?;
        let entry = match tag {
            1 => {
                let len = reader.u16()? as usize;
                CpEntry::Utf8(reader.take(len)?.to_vec())
            }
            3 | 4 => {
                reader.take(4)?;
                CpEntry::Other
            }
            5 | 6 => {
                if index + 1 >= u32::from(count) {
                    return Err("invalid two-slot constant-pool entry");
                }
                reader.take(8)?;
                index += 1;
                CpEntry::Other
            }
            7 => CpEntry::Class(reader.u16()?),
            8 | 16 | 19 | 20 => {
                reader.take(2)?;
                CpEntry::Other
            }
            9 => {
                reader.take(4)?;
                CpEntry::Other
            }
            10 | 11 => {
                let class_index = reader.u16()?;
                let name_and_type_index = reader.u16()?;
                CpEntry::MethodRef(class_index, name_and_type_index)
            }
            12 => {
                let name_index = reader.u16()?;
                reader.u16()?; // descriptor_index
                CpEntry::NameAndType(name_index)
            }
            15 => {
                reader.take(3)?;
                CpEntry::Other
            }
            17 | 18 => {
                reader.take(4)?;
                CpEntry::Other
            }
            _ => return Err("unknown constant-pool tag"),
        };
        entries.push(entry);
        if matches!(tag, 5 | 6) {
            entries.push(CpEntry::Other); // long/double occupy two slots
        }
        index += 1;
    }
    reader.take(6)?; // access_flags, this_class, super_class
    let interfaces = reader.u16()? as usize;
    reader.take(
        interfaces
            .checked_mul(2)
            .ok_or("interface count overflow")?,
    )?;
    for _ in 0..2 {
        let members = reader.u16()?;
        for _ in 0..members {
            reader.take(6)?; // flags, name, descriptor
            reader.attributes()?;
        }
    }
    reader.attributes()?;
    if !reader.bytes.is_empty() {
        return Err("trailing bytes after class file");
    }
    resolve_constant_pool(&entries)
}

/// Resolve `CONSTANT_Class` names and method-reference owner/name pairs
/// through the parsed pool. A dangling or mistyped index means a
/// malformed class and fails closed like every other structural defect.
fn resolve_constant_pool(entries: &[CpEntry]) -> Result<ConstantPoolReport, &'static str> {
    let utf8_at = |index: u16| -> Result<&[u8], &'static str> {
        match entries.get(usize::from(index)) {
            Some(CpEntry::Utf8(bytes)) => Ok(bytes.as_slice()),
            _ => Err("invalid constant-pool UTF-8 reference"),
        }
    };
    let mut report = ConstantPoolReport::default();
    for entry in entries {
        match entry {
            CpEntry::Utf8(bytes) => report.utf8.push(bytes.clone()),
            CpEntry::Class(name_index) => {
                report.class_names.push(utf8_at(*name_index)?.to_vec());
            }
            CpEntry::MethodRef(class_index, name_and_type_index) => {
                let owner = match entries.get(usize::from(*class_index)) {
                    Some(CpEntry::Class(name_index)) => utf8_at(*name_index)?,
                    _ => return Err("invalid constant-pool method reference"),
                };
                let name = match entries.get(usize::from(*name_and_type_index)) {
                    Some(CpEntry::NameAndType(name_index)) => utf8_at(*name_index)?,
                    _ => return Err("invalid constant-pool method reference"),
                };
                report.method_pairs.push((owner.to_vec(), name.to_vec()));
            }
            _ => {}
        }
    }
    Ok(report)
}

fn contains(text: &[u8], needle: &str) -> bool {
    text.windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// Whether a real `CONSTANT_Class` internal name matches a class
/// indicator: exactly, as a nested class (`Indicator$Inner`), or — for
/// `/`-terminated indicators — as a package prefix (`com/sun/jna/`).
/// String literals, descriptors, and annotations never reach this
/// matcher; array-type class constants (`[Ljava/net/Socket;`) do not
/// collide with the plain-name indicators either.
fn class_name_indicates(class_name: &[u8], indicator: &str) -> bool {
    let needle = indicator.as_bytes();
    if needle.ends_with(b"/") {
        return class_name.starts_with(needle);
    }
    class_name == needle || class_name.strip_prefix(needle).is_some_and(|rest| rest.starts_with(b"$"))
}

fn add_threat(
    threats: &mut Vec<SecurityThreat>,
    kind: SecurityThreatKind,
    member: &str,
    indicator: &str,
) {
    threats.push(SecurityThreat {
        kind,
        member: member.to_string(),
        indicator: indicator.to_string(),
    });
}

fn detect_security_threats(
    pool: &ConstantPoolReport,
    member: &str,
    threats: &mut Vec<SecurityThreat>,
) {
    let singles = [
        (
            "java/lang/ProcessBuilder",
            SecurityThreatKind::ProcessExecution,
        ),
        ("com/sun/jna/", SecurityThreatKind::NativeLibraryLoad),
        (
            "java/lang/foreign/Linker",
            SecurityThreatKind::ForeignLinkerAccess,
        ),
        (
            "java/lang/foreign/SymbolLookup",
            SecurityThreatKind::ForeignLinkerAccess,
        ),
        ("sun/misc/Unsafe", SecurityThreatKind::UnsafeMemoryAccess),
        (
            "jdk/internal/misc/Unsafe",
            SecurityThreatKind::UnsafeMemoryAccess,
        ),
        ("java/net/Socket", SecurityThreatKind::DirectNetworkAccess),
        (
            "java/net/ServerSocket",
            SecurityThreatKind::DirectNetworkAccess,
        ),
        (
            "java/net/DatagramSocket",
            SecurityThreatKind::DirectNetworkAccess,
        ),
        (
            "java/net/http/HttpClient",
            SecurityThreatKind::DirectNetworkAccess,
        ),
        (
            "java/net/URLConnection",
            SecurityThreatKind::DirectNetworkAccess,
        ),
        (
            "java/net/HttpURLConnection",
            SecurityThreatKind::DirectNetworkAccess,
        ),
        (
            "java/net/URLClassLoader",
            SecurityThreatKind::DynamicCodeLoading,
        ),
        (
            "javax/script/ScriptEngine",
            SecurityThreatKind::DynamicCodeLoading,
        ),
        (
            "javax/naming/InitialContext",
            SecurityThreatKind::JndiLookup,
        ),
        (
            "javax/naming/directory/InitialDirContext",
            SecurityThreatKind::JndiLookup,
        ),
    ];
    for (indicator, kind) in singles {
        if pool
            .class_names
            .iter()
            .any(|name| class_name_indicates(name, indicator))
        {
            add_threat(threats, kind, member, indicator);
        }
    }
    let combined = [
        (
            "java/lang/Runtime",
            "exec",
            SecurityThreatKind::ProcessExecution,
        ),
        (
            "java/lang/System",
            "loadLibrary",
            SecurityThreatKind::NativeLibraryLoad,
        ),
        (
            "java/lang/System",
            "load",
            SecurityThreatKind::NativeLibraryLoad,
        ),
        (
            "java/lang/invoke/MethodHandles$Lookup",
            "defineClass",
            SecurityThreatKind::DynamicCodeLoading,
        ),
        (
            "java/lang/ClassLoader",
            "defineClass",
            SecurityThreatKind::DynamicCodeLoading,
        ),
    ];
    for (owner, method, kind) in combined {
        if pool.method_pairs.iter().any(|(ref_owner, ref_name)| {
            ref_owner.as_slice() == owner.as_bytes() && ref_name.as_slice() == method.as_bytes()
        }) {
            add_threat(threats, kind, member, &format!("{owner}#{method}"));
        }
    }
}

/// Whether one constant-pool string matched each capability family —
/// aggregated per class so [`CapabilitySignals::render_hook_classes`] &
/// co. count hook-bearing classes, not matching strings.
#[derive(Default)]
struct HookHits {
    render: bool,
    light: bool,
    server: bool,
}

/// Per-family count of classes carrying at least one hook.
#[derive(Default)]
struct HookClassCounts {
    render: usize,
    light: usize,
    server: usize,
}

fn collect_signals(
    text: &[u8],
    render: &mut BTreeSet<String>,
    light: &mut BTreeSet<String>,
    server: &mut BTreeSet<String>,
    hits: &mut HookHits,
) {
    const RENDER: &[&str] = &[
        "org/lwjgl/vulkan/",
        "org/lwjgl/opengl/",
        "org/lwjgl/glfw/",
        "com/mojang/blaze3d/",
        "net/minecraft/client/renderer/LevelRenderer",
        "ChunkBuilder",
        "SectionRenderDispatcher",
        "RenderSystem",
        "ShaderInstance",
        "VertexBuffer",
        "GlStateManager",
        "BufferBuilder",
        "GameRenderer",
        "sodium",
        "vulkanmod",
        "iris",
    ];
    const LIGHT: &[&str] = &[
        "LevelLightEngine",
        "LayerLightEngine",
        "ChunkLightProvider",
        "BlockLightEngine",
        "SkyLightEngine",
    ];
    const SERVER: &[&str] = &[
        "ChunkMap",
        "ServerChunkCache",
        "CompressionEncoder",
        "CompressionDecoder",
        "PalettedContainer",
    ];
    for hook in RENDER {
        if contains(text, hook) {
            hits.render = true;
            render.insert((*hook).to_string());
        }
    }
    for hook in LIGHT {
        if contains(text, hook) {
            hits.light = true;
            light.insert((*hook).to_string());
        }
    }
    for hook in SERVER {
        if contains(text, hook) {
            hits.server = true;
            server.insert((*hook).to_string());
        }
    }
}
