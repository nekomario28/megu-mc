// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
#![deny(unsafe_code)]

//! Bounded, zero-JVM security inspection and structural capability routing.
//! All class files are inspected before a catalog match can skip an archive.

use std::collections::BTreeSet;

use crate::fallback::BUILTIN_SUPERSEDED_PERF_MODS;
use crate::modpack::{read_zip_member, zip_member_list};

const MAX_CLASSES: usize = 2048;
const MAX_MEMBERS: usize = 8192;
const MAX_SCANNED_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecurityThreatKind {
    EmbeddedNativeOrExecutable,
    ProcessExecution,
    NativeLibraryLoad,
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
            Self::UnsafeMemoryAccess => "unsafe_memory_access",
            Self::DirectNetworkAccess => "direct_network_access",
            Self::DynamicCodeLoading => "dynamic_code_loading",
            Self::JndiLookup => "jndi_lookup",
        }
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
                "{display}: security scan exceeds 32 MiB of archive data"
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
        if name == "mc-spine-bundle.json" {
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
                                | "dimension"
                                | "dimension_type"
                                | "worldgen"
                                | "loot_table"
                                | "loot_tables"
                                | "advancement"
                                | "advancements"
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
        let strings = constant_pool_utf8(&member_bytes)
            .map_err(|reason| format!("{display}: malformed class {name}: {reason}"))?;
        detect_security_threats(&strings, name, &mut threats);
        for text in &strings {
            collect_signals(text, &mut render_hooks, &mut light_hooks, &mut server_hooks);
        }
    }

    signals.render_engine_hooks = render_hooks.into_iter().collect();
    signals.light_engine_hooks = light_hooks.into_iter().collect();
    signals.server_engine_hooks = server_hooks.into_iter().collect();
    let decision = if !threats.is_empty() {
        ModRouteDecision::SecurityQuarantine { threats }
    } else if signals.has_gameplay_content && !signals.render_engine_hooks.is_empty() {
        ModRouteDecision::HybridSplit {
            client_render_fallback: NativeSubsystem::WgpuRenderBackend,
            evidence: signals.render_engine_hooks.clone(),
        }
    } else if signals.has_gameplay_content {
        ModRouteDecision::StandardPipeline
    } else if !signals.render_engine_hooks.is_empty() {
        capability_route(
            NativeSubsystem::WgpuRenderBackend,
            &signals.render_engine_hooks,
        )
    } else if !signals.light_engine_hooks.is_empty() {
        capability_route(NativeSubsystem::LightEngine, &signals.light_engine_hooks)
    } else if !signals.server_engine_hooks.is_empty() {
        capability_route(
            NativeSubsystem::ServerChunkAndNetEngine,
            &signals.server_engine_hooks,
        )
    } else {
        ModRouteDecision::StandardPipeline
    };
    Ok(JarRoutingReport { decision, signals })
}

/// Known IDs are considered only after the full static security scan.
pub fn route_mod_jar(
    jar_bytes: &[u8],
    display: &str,
    declared_or_inferred_id: Option<&str>,
) -> Result<JarRoutingReport, String> {
    let mut report = inspect_jar_security_and_capabilities(jar_bytes, display)?;
    if matches!(report.decision, ModRouteDecision::SecurityQuarantine { .. }) {
        return Ok(report);
    }
    if let Some(id) = declared_or_inferred_id {
        if let Some(known) = BUILTIN_SUPERSEDED_PERF_MODS
            .iter()
            .find(|known| canonical_id(known) == canonical_id(id))
        {
            let subsystem = match *known {
                "starlight" | "scalablelux" | "phosphor" => NativeSubsystem::LightEngine,
                "c2me" | "krypton" | "servercore" => NativeSubsystem::ServerChunkAndNetEngine,
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

/// Parse JVMS 4.4 constant-pool structures, including two-slot constants.
/// UTF8 values remain raw bytes so legal modified-UTF8 (NUL/surrogates) does
/// not hide ASCII references or get mistaken for invalid UTF-8.
fn constant_pool_utf8(class: &[u8]) -> Result<Vec<Vec<u8>>, &'static str> {
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
    let mut utf8 = Vec::new();
    let mut index = 1u32;
    while index < u32::from(count) {
        let tag = reader.u8()?;
        match tag {
            1 => {
                let len = reader.u16()? as usize;
                utf8.push(reader.take(len)?.to_vec());
            }
            3 | 4 => {
                reader.take(4)?;
            }
            5 | 6 => {
                if index + 1 >= u32::from(count) {
                    return Err("invalid two-slot constant-pool entry");
                }
                reader.take(8)?;
                index += 1;
            }
            7 | 8 | 16 | 19 | 20 => {
                reader.take(2)?;
            }
            9 | 10 | 11 | 12 | 17 | 18 => {
                reader.take(4)?;
            }
            15 => {
                reader.take(3)?;
            }
            _ => return Err("unknown constant-pool tag"),
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
    Ok(utf8)
}

fn contains(text: &[u8], needle: &str) -> bool {
    text.windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

fn has(strings: &[Vec<u8>], needle: &str) -> bool {
    strings.iter().any(|value| contains(value, needle))
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

fn detect_security_threats(strings: &[Vec<u8>], member: &str, threats: &mut Vec<SecurityThreat>) {
    let singles = [
        (
            "java/lang/ProcessBuilder",
            SecurityThreatKind::ProcessExecution,
        ),
        ("com/sun/jna/", SecurityThreatKind::NativeLibraryLoad),
        ("java/lang/foreign/", SecurityThreatKind::NativeLibraryLoad),
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
        if has(strings, indicator) {
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
        if has(strings, owner) && strings.iter().any(|s| s.as_slice() == method.as_bytes()) {
            add_threat(threats, kind, member, &format!("{owner}#{method}"));
        }
    }
}

fn collect_signals(
    text: &[u8],
    render: &mut BTreeSet<String>,
    light: &mut BTreeSet<String>,
    server: &mut BTreeSet<String>,
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
            render.insert((*hook).to_string());
        }
    }
    for hook in LIGHT {
        if contains(text, hook) {
            light.insert((*hook).to_string());
        }
    }
    for hook in SERVER {
        if contains(text, hook) {
            server.insert((*hook).to_string());
        }
    }
}
