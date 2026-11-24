// SPDX-License-Identifier: LGPL-3.0-or-later
//! Deterministic, dependency-free GLSL declaration reflection.
//!
//! This is a lexical resource inventory for preprocessed shader sources, not
//! a GLSL compiler. It ignores comments, directives, and function bodies.
//! Conditional branches must be resolved by the pack preprocessor first.
//! Binding plans describe resources; actual WGSL code and buffer layouts
//! require translation and shader validation by the renderer.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UniformCategory {
    Camera,
    Time,
    World,
    HandLighting,
    DistantHorizons,
    Custom,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReflectedUniform {
    pub name: String,
    pub glsl_type: String,
    pub array_size: Option<u32>,
    pub category: UniformCategory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SamplerSemantic {
    Color(u8),
    Depth(u8),
    ShadowDepth(u8),
    ShadowColor(u8),
    DistantHorizonsDepth(u8),
    Noise,
    Custom,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReflectedSampler {
    pub name: String,
    pub glsl_type: String,
    pub array_size: Option<u32>,
    pub declared_binding: Option<u32>,
    pub semantic: SamplerSemantic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReflectedStorageBuffer {
    pub block_name: String,
    pub instance_name: Option<String>,
    pub declared_binding: Option<u32>,
    pub readonly: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReflectedUniformBlock {
    pub block_name: String,
    pub instance_name: Option<String>,
    pub declared_binding: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageAccess {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReflectedStorageImage {
    pub name: String,
    pub glsl_type: String,
    pub array_size: Option<u32>,
    pub declared_binding: Option<u32>,
    pub format: Option<String>,
    pub access: ImageAccess,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReflectedProgramBindings {
    pub glsl_version: Option<u32>,
    pub uniforms: Vec<ReflectedUniform>,
    pub samplers: Vec<ReflectedSampler>,
    pub uniform_blocks: Vec<ReflectedUniformBlock>,
    pub storage_buffers: Vec<ReflectedStorageBuffer>,
    pub storage_images: Vec<ReflectedStorageImage>,
    pub workgroup_size: Option<[u32; 3]>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttachmentUsagePlan {
    pub color: BTreeSet<u8>,
    pub depth: BTreeSet<u8>,
    pub shadow_depth: BTreeSet<u8>,
    pub shadow_color: BTreeSet<u8>,
    pub dh_depth: BTreeSet<u8>,
    pub uses_noise: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WgslResourceKind {
    Uniform,
    UniformBlock,
    SampledTexture,
    Sampler,
    StorageBuffer,
    StorageImage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslBinding {
    pub name: String,
    pub group: u8,
    pub binding: u32,
    pub kind: WgslResourceKind,
    pub glsl_binding: Option<u32>,
}

impl WgslBinding {
    /// Attribute pair for a WGSL resource declaration.
    pub fn wgsl_attributes(&self) -> String {
        format!("@group({}) @binding({})", self.group, self.binding)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WgslBindingTable {
    pub entries: Vec<WgslBinding>,
    pub attachments: AttachmentUsagePlan,
}

fn ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn ident_continue(b: u8) -> bool {
    ident_start(b) || b.is_ascii_digit()
}

// Tokenize ASCII GLSL punctuation without changing non-ASCII source offsets.
// A directive and its continuation lines are omitted in their entirety.
fn tokenize(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut line_start = true;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            if bytes[i] == b'\n' {
                line_start = true;
            }
            i += 1;
        } else if bytes[i] == b'#' && line_start {
            loop {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                let continued = bytes[start..i]
                    .iter()
                    .rev()
                    .find(|b| !b.is_ascii_whitespace())
                    == Some(&b'\\');
                if i < bytes.len() {
                    i += 1;
                }
                if !continued || i >= bytes.len() {
                    break;
                }
            }
            line_start = true;
        } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && &bytes[i..i + 2] != b"*/" {
                if bytes[i] == b'\n' {
                    line_start = true;
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(bytes.len());
            line_start = false;
        } else {
            let start = i;
            if ident_continue(bytes[i]) {
                i += 1;
                while i < bytes.len() && ident_continue(bytes[i]) {
                    i += 1;
                }
            } else {
                i += 1;
            }
            // Each token consists of ASCII, so the slice is on UTF-8 boundaries.
            if let Some(token) = source.get(start..i) {
                tokens.push(token);
            }
            line_start = false;
        }
    }
    tokens
}

fn version(source: &str) -> Option<u32> {
    // Strip comments using the same tokenizer behavior for directives; only
    // consider a #version directive at the start of a logical source line.
    let mut in_block = false;
    for line in source.lines() {
        let mut visible = String::new();
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if in_block {
                if ch == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    in_block = false;
                }
            } else if ch == '/' && chars.peek() == Some(&'*') {
                chars.next();
                in_block = true;
            } else if ch == '/' && chars.peek() == Some(&'/') {
                break;
            } else {
                visible.push(ch);
            }
        }
        let trimmed = visible.trim_start();
        if let Some(rest) = trimmed.strip_prefix("#version") {
            if rest.starts_with(char::is_whitespace) {
                return rest.split_whitespace().next()?.parse().ok();
            }
        }
    }
    None
}

fn uniform_category(name: &str) -> UniformCategory {
    if name.starts_with("dh") && name.chars().nth(2).is_some_and(char::is_uppercase) {
        return UniformCategory::DistantHorizons;
    }
    if matches!(
        name,
        "heldItemId" | "heldBlockLightValue" | "heldItemId2" | "heldBlockLightValue2"
    ) {
        return UniformCategory::HandLighting;
    }
    if matches!(
        name,
        "viewWidth"
            | "viewHeight"
            | "aspectRatio"
            | "near"
            | "far"
            | "cameraPosition"
            | "previousCameraPosition"
            | "gbufferModelView"
            | "gbufferModelViewInverse"
            | "gbufferPreviousModelView"
            | "gbufferProjection"
            | "gbufferProjectionInverse"
            | "gbufferPreviousProjection"
            | "shadowModelView"
            | "shadowModelViewInverse"
            | "shadowProjection"
            | "shadowProjectionInverse"
    ) {
        return UniformCategory::Camera;
    }
    if matches!(
        name,
        "worldTime"
            | "worldDay"
            | "frameTime"
            | "frameTimeCounter"
            | "frameCounter"
            | "sunAngle"
            | "moonPhase"
    ) {
        return UniformCategory::Time;
    }
    if matches!(
        name,
        "rainStrength"
            | "wetness"
            | "eyeAltitude"
            | "isEyeInWater"
            | "skyColor"
            | "fogColor"
            | "sunPosition"
            | "moonPosition"
            | "upPosition"
            | "ambientLight"
            | "blindness"
            | "nightVision"
            | "screenBrightness"
            | "centerDepthSmooth"
    ) {
        return UniformCategory::World;
    }
    UniformCategory::Custom
}

fn index_suffix(name: &str, prefix: &str, max: u8) -> Option<u8> {
    let value = name.strip_prefix(prefix)?.parse::<u8>().ok()?;
    (value <= max).then_some(value)
}

fn sampler_semantic(name: &str) -> SamplerSemantic {
    if let Some(index) = index_suffix(name, "colortex", 15) {
        return SamplerSemantic::Color(index);
    }
    if let Some(index) = index_suffix(name, "depthtex", 2) {
        return SamplerSemantic::Depth(index);
    }
    if let Some(index) = index_suffix(name, "shadowtex", 1) {
        return SamplerSemantic::ShadowDepth(index);
    }
    if let Some(index) = index_suffix(name, "shadowcolor", 1) {
        return SamplerSemantic::ShadowColor(index);
    }
    if let Some(index) = index_suffix(name, "dhDepthTex", 1) {
        return SamplerSemantic::DistantHorizonsDepth(index);
    }
    match name {
        "gcolor" => SamplerSemantic::Color(0),
        "gdepth" => SamplerSemantic::Color(1),
        "gnormal" => SamplerSemantic::Color(2),
        "composite" => SamplerSemantic::Color(3),
        "gaux1" => SamplerSemantic::Color(4),
        "gaux2" => SamplerSemantic::Color(5),
        "gaux3" => SamplerSemantic::Color(6),
        "gaux4" => SamplerSemantic::Color(7),
        "shadow" | "watershadow" => SamplerSemantic::ShadowDepth(0),
        "noisetex" => SamplerSemantic::Noise,
        _ => SamplerSemantic::Custom,
    }
}

fn is_sampler(ty: &str) -> bool {
    ty.starts_with("sampler") || ty.starts_with("isampler") || ty.starts_with("usampler")
}

fn is_image(ty: &str) -> bool {
    ty.starts_with("image") || ty.starts_with("iimage") || ty.starts_with("uimage")
}

fn qualifier(token: &str) -> bool {
    matches!(
        token,
        "readonly"
            | "writeonly"
            | "coherent"
            | "volatile"
            | "restrict"
            | "highp"
            | "mediump"
            | "lowp"
            | "precise"
            | "invariant"
    )
}

fn layout_args(tokens: &[&str], start: usize) -> (BTreeMap<String, String>, usize) {
    let mut args = BTreeMap::new();
    let mut i = start;
    if tokens.get(i) != Some(&"(") {
        return (args, i);
    }
    i += 1;
    while i < tokens.len() && tokens[i] != ")" {
        if tokens[i] == "," {
            i += 1;
            continue;
        }
        let key = tokens[i].to_owned();
        i += 1;
        if tokens.get(i) == Some(&"=") {
            i += 1;
            if let Some(value) = tokens.get(i) {
                args.insert(key, (*value).to_owned());
                i += 1;
            }
        } else {
            args.insert(key, String::new());
        }
    }
    (args, (i + 1).min(tokens.len()))
}

fn array_size(tokens: &[&str], i: &mut usize) -> Option<u32> {
    if tokens.get(*i) != Some(&"[") {
        return None;
    }
    *i += 1;
    let size = tokens.get(*i).and_then(|v| v.parse::<u32>().ok());
    while *i < tokens.len() && tokens[*i] != "]" {
        *i += 1;
    }
    *i = (*i + 1).min(tokens.len());
    size
}

fn skip_braces(tokens: &[&str], start: usize) -> usize {
    let mut depth = 0u32;
    let mut i = start;
    while i < tokens.len() {
        match tokens[i] {
            "{" => depth += 1,
            "}" => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    i
}

/// Reflect top-level GLSL 120–460 resource declarations. Invalid or
/// unresolved declarations are ignored safely. Returned lists are sorted by
/// identifier, irrespective of declaration order.
pub fn reflect_glsl_program(source: &str) -> ReflectedProgramBindings {
    let tokens = tokenize(source);
    let mut out = ReflectedProgramBindings {
        glsl_version: version(source),
        ..ReflectedProgramBindings::default()
    };
    let mut i = 0;
    while i < tokens.len() {
        let mut layout = BTreeMap::new();
        while tokens.get(i) == Some(&"layout") {
            let (args, next) = layout_args(&tokens, i + 1);
            layout.extend(args);
            i = next;
        }
        let mut readonly = false;
        let mut writeonly = false;
        while tokens.get(i).is_some_and(|token| qualifier(token)) {
            readonly |= tokens[i] == "readonly";
            writeonly |= tokens[i] == "writeonly";
            i += 1;
        }
        let binding = layout.get("binding").and_then(|v| v.parse::<u32>().ok());
        match tokens.get(i).copied() {
            Some("in") if !layout.is_empty() => {
                let mut group = [1; 3];
                let mut specified = false;
                for (pos, axis) in ["local_size_x", "local_size_y", "local_size_z"]
                    .iter()
                    .enumerate()
                {
                    if let Some(value) = layout.get(*axis).and_then(|v| v.parse::<u32>().ok()) {
                        if value > 0 {
                            group[pos] = value;
                            specified = true;
                        }
                    }
                }
                if specified {
                    out.workgroup_size = Some(group);
                }
                i += 1;
            }
            Some("buffer" | "uniform") => {
                let storage = tokens[i] == "buffer";
                i += 1;
                while tokens.get(i).is_some_and(|token| qualifier(token)) {
                    readonly |= tokens[i] == "readonly";
                    writeonly |= tokens[i] == "writeonly";
                    i += 1;
                }
                let Some(ty) = tokens.get(i).copied() else {
                    break;
                };
                i += 1;
                if tokens.get(i) == Some(&"{") {
                    i = skip_braces(&tokens, i);
                    let instance = tokens.get(i).filter(|t| ident_start(t.as_bytes()[0]));
                    let instance_name = instance.map(|s| (*s).to_owned());
                    if instance.is_some() {
                        i += 1;
                        array_size(&tokens, &mut i);
                    }
                    if storage {
                        out.storage_buffers.push(ReflectedStorageBuffer {
                            block_name: ty.to_owned(),
                            instance_name,
                            declared_binding: binding,
                            readonly,
                        });
                    } else {
                        out.uniform_blocks.push(ReflectedUniformBlock {
                            block_name: ty.to_owned(),
                            instance_name,
                            declared_binding: binding,
                        });
                    }
                } else if !storage {
                    // Multiple variables may share one uniform declaration.
                    while let Some(&name) = tokens.get(i) {
                        if !name.as_bytes().first().is_some_and(|b| ident_start(*b)) {
                            break;
                        }
                        i += 1;
                        let count = array_size(&tokens, &mut i);
                        if is_sampler(ty) {
                            out.samplers.push(ReflectedSampler {
                                name: name.to_owned(),
                                glsl_type: ty.to_owned(),
                                array_size: count,
                                declared_binding: binding,
                                semantic: sampler_semantic(name),
                            });
                        } else if is_image(ty) {
                            out.storage_images.push(ReflectedStorageImage {
                                name: name.to_owned(),
                                glsl_type: ty.to_owned(),
                                array_size: count,
                                declared_binding: binding,
                                format: layout
                                    .keys()
                                    .find(|key| {
                                        key.starts_with("rgba")
                                            || key.starts_with("rg")
                                            || (key.starts_with('r')
                                                && key
                                                    .as_bytes()
                                                    .get(1)
                                                    .is_some_and(u8::is_ascii_digit))
                                    })
                                    .cloned(),
                                access: if readonly {
                                    ImageAccess::ReadOnly
                                } else if writeonly {
                                    ImageAccess::WriteOnly
                                } else {
                                    ImageAccess::ReadWrite
                                },
                            });
                        } else {
                            out.uniforms.push(ReflectedUniform {
                                name: name.to_owned(),
                                glsl_type: ty.to_owned(),
                                array_size: count,
                                category: uniform_category(name),
                            });
                        }
                        // Avoid interpreting initializer expressions as additional names.
                        if tokens.get(i) == Some(&"=") {
                            while i < tokens.len() && !matches!(tokens[i], "," | ";") {
                                i += 1;
                            }
                        }
                        if tokens.get(i) != Some(&",") {
                            break;
                        }
                        i += 1;
                    }
                }
                while i < tokens.len() && tokens[i] != ";" {
                    if tokens[i] == "{" {
                        i = skip_braces(&tokens, i);
                    } else {
                        i += 1;
                    }
                }
                i = (i + 1).min(tokens.len());
            }
            Some("{") => i = skip_braces(&tokens, i),
            Some(_) => i += 1,
            None => break,
        }
    }
    out.uniforms.sort_by(|a, b| a.name.cmp(&b.name));
    out.uniforms.dedup_by(|a, b| a.name == b.name);
    out.samplers.sort_by(|a, b| a.name.cmp(&b.name));
    out.samplers.dedup_by(|a, b| a.name == b.name);
    out.uniform_blocks
        .sort_by(|a, b| a.block_name.cmp(&b.block_name));
    out.uniform_blocks
        .dedup_by(|a, b| a.block_name == b.block_name);
    out.storage_buffers
        .sort_by(|a, b| a.block_name.cmp(&b.block_name));
    out.storage_buffers
        .dedup_by(|a, b| a.block_name == b.block_name);
    out.storage_images.sort_by(|a, b| a.name.cmp(&b.name));
    out.storage_images.dedup_by(|a, b| a.name == b.name);
    out
}

// An explicit GLSL slot is preferred if it is unused in the resource's WGSL
// group. Collisions and implicit slots use the lowest available index.
fn add_binding(
    table: &mut WgslBindingTable,
    used: &mut [BTreeSet<u32>; 4],
    name: &str,
    group: u8,
    kind: WgslResourceKind,
    preferred: Option<u32>,
) {
    let occupied = &mut used[group as usize];
    let binding = preferred
        .filter(|slot| !occupied.contains(slot))
        .unwrap_or_else(|| {
            let mut slot = 0;
            while occupied.contains(&slot) {
                slot += 1;
            }
            slot
        });
    occupied.insert(binding);
    table.entries.push(WgslBinding {
        name: name.to_owned(),
        group,
        binding,
        kind,
        glsl_binding: preferred,
    });
}

/// Allocate deterministic WGSL slots across four resource groups.
///
/// Group 0 holds scalar/matrix uniforms, group 1 sampled textures and their
/// separate samplers, group 2 uniform/storage blocks, and group 3 storage
/// images. The planner does not assume that a GLSL binding number is globally
/// unique across different resource classes.
pub fn plan_wgsl_binding_table(reflected: &ReflectedProgramBindings) -> WgslBindingTable {
    let mut table = WgslBindingTable::default();
    let mut used: [BTreeSet<u32>; 4] = std::array::from_fn(|_| BTreeSet::new());
    let mut uniforms = reflected.uniforms.iter().collect::<Vec<_>>();
    uniforms.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in uniforms {
        add_binding(
            &mut table,
            &mut used,
            &entry.name,
            0,
            WgslResourceKind::Uniform,
            None,
        );
    }
    let mut samplers = reflected.samplers.iter().collect::<Vec<_>>();
    // Explicit GLSL slots take priority over implicit allocations.
    samplers.sort_by_key(|e| (e.declared_binding.is_none(), e.declared_binding, &e.name));
    for entry in &samplers {
        add_binding(
            &mut table,
            &mut used,
            &entry.name,
            1,
            WgslResourceKind::SampledTexture,
            entry.declared_binding,
        );
        match entry.semantic {
            SamplerSemantic::Color(i) => {
                table.attachments.color.insert(i);
            }
            SamplerSemantic::Depth(i) => {
                table.attachments.depth.insert(i);
            }
            SamplerSemantic::ShadowDepth(i) => {
                table.attachments.shadow_depth.insert(i);
            }
            SamplerSemantic::ShadowColor(i) => {
                table.attachments.shadow_color.insert(i);
            }
            SamplerSemantic::DistantHorizonsDepth(i) => {
                table.attachments.dh_depth.insert(i);
            }
            SamplerSemantic::Noise => table.attachments.uses_noise = true,
            SamplerSemantic::Custom => {}
        }
    }
    for entry in samplers {
        add_binding(
            &mut table,
            &mut used,
            &format!("{}__sampler", entry.name),
            1,
            WgslResourceKind::Sampler,
            None,
        );
    }
    let mut blocks = Vec::new();
    for entry in &reflected.uniform_blocks {
        blocks.push((
            entry.block_name.as_str(),
            WgslResourceKind::UniformBlock,
            entry.declared_binding,
        ));
    }
    for entry in &reflected.storage_buffers {
        blocks.push((
            entry.block_name.as_str(),
            WgslResourceKind::StorageBuffer,
            entry.declared_binding,
        ));
    }
    blocks.sort_by_key(|(name, _, slot)| (slot.is_none(), *slot, *name));
    for (name, kind, slot) in blocks {
        add_binding(&mut table, &mut used, name, 2, kind, slot);
    }
    let mut images = reflected.storage_images.iter().collect::<Vec<_>>();
    images.sort_by_key(|e| (e.declared_binding.is_none(), e.declared_binding, &e.name));
    for entry in images {
        add_binding(
            &mut table,
            &mut used,
            &entry.name,
            3,
            WgslResourceKind::StorageImage,
            entry.declared_binding,
        );
    }
    table
        .entries
        .sort_by_key(|e| (e.group, e.binding, e.name.clone()));
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_glsl120_aliases_and_comments() {
        let shader = r#"
            #version 120
            uniform sampler2D gcolor, gdepth;
            uniform sampler2D gnormal;
            uniform float viewWidth, viewHeight;
            // uniform sampler2D colortex15;
            /* uniform int heldItemId2; */
            void main() { vec4 c = texture2D(gcolor, vec2(0.5)); }
        "#;
        let result = reflect_glsl_program(shader);
        assert_eq!(result.glsl_version, Some(120));
        assert_eq!(result.uniforms.len(), 2);
        assert_eq!(result.samplers.len(), 3);
        assert_eq!(result.samplers[0].semantic, SamplerSemantic::Color(0));
        let bindings = plan_wgsl_binding_table(&result);
        assert_eq!(bindings.attachments.color, [0, 1, 2].into());
        assert_eq!(bindings.entries.iter().filter(|e| e.group == 1).count(), 6);
    }

    #[test]
    fn glsl330_colortex_and_stable_ordering() {
        let shader = r#"
            #version 330 core
            uniform sampler2D colortex4;
            layout(binding = 2) uniform sampler2D colortex2;
            uniform sampler2D colortex1, colortex0, colortex3;
            uniform sampler2D depthtex0, shadowtex1, shadowcolor0, noisetex;
            uniform samplerCube skyTexture;
        "#;
        let reflected = reflect_glsl_program(shader);
        assert_eq!(reflected.samplers.len(), 10);
        let plan = plan_wgsl_binding_table(&reflected);
        assert_eq!(plan.attachments.color, [0, 1, 2, 3, 4].into());
        assert_eq!(plan.attachments.depth, [0].into());
        assert_eq!(plan.attachments.shadow_depth, [1].into());
        assert_eq!(plan.attachments.shadow_color, [0].into());
        assert!(plan.attachments.uses_noise);
        assert_eq!(
            plan.entries
                .iter()
                .find(|e| e.name == "colortex2")
                .unwrap()
                .glsl_binding,
            Some(2)
        );
        assert_eq!(plan, plan_wgsl_binding_table(&reflected));
    }

    #[test]
    fn iris460_compute_ssbo_storage_images_dual_hand_and_dh() {
        let shader = r#"
            #version 460 core
            layout(local_size_y = 8, local_size_x = 16, local_size_z = 2) in;
            layout(std430, binding = 3) readonly buffer Particles {
                vec4 positions[];
            } particleData;
            layout(rgba16f, binding = 7) uniform readonly image2D historyImage;
            layout(rgba8, binding = 8) writeonly uniform image3D volume;
            layout(std140, binding = 0) uniform Matrices {
                mat4 projection;
            } matrices;
            uniform int heldItemId, heldItemId2;
            uniform int heldBlockLightValue, heldBlockLightValue2;
            uniform float dhNearPlane, dhFarPlane, frameTimeCounter;
            uniform vec3 cameraPosition;
            uniform mat4 gbufferProjection, shadowModelView;
            uniform sampler2D dhDepthTex0, dhDepthTex1, colortex15;
        "#;
        let reflected = reflect_glsl_program(shader);
        assert_eq!(reflected.glsl_version, Some(460));
        assert_eq!(reflected.workgroup_size, Some([16, 8, 2]));
        assert_eq!(reflected.storage_buffers.len(), 1);
        assert_eq!(reflected.storage_buffers[0].declared_binding, Some(3));
        assert!(reflected.storage_buffers[0].readonly);
        assert_eq!(reflected.uniform_blocks.len(), 1);
        assert_eq!(reflected.storage_images.len(), 2);
        assert_eq!(
            reflected.storage_images[0].format.as_deref(),
            Some("rgba16f")
        );
        assert_eq!(reflected.storage_images[0].access, ImageAccess::ReadOnly);
        assert_eq!(reflected.storage_images[1].access, ImageAccess::WriteOnly);
        assert_eq!(
            reflected
                .uniforms
                .iter()
                .find(|u| u.name == "heldItemId2")
                .unwrap()
                .category,
            UniformCategory::HandLighting
        );
        assert_eq!(
            reflected
                .uniforms
                .iter()
                .find(|u| u.name == "dhNearPlane")
                .unwrap()
                .category,
            UniformCategory::DistantHorizons
        );
        let plan = plan_wgsl_binding_table(&reflected);
        assert_eq!(plan.attachments.dh_depth, [0, 1].into());
        assert_eq!(plan.attachments.color, [15].into());
        assert!(plan.entries.iter().any(|e| e.group == 2 && e.binding == 3));
        assert!(plan.entries.iter().any(|e| e.group == 3 && e.binding == 7));
        assert!(plan.entries.iter().any(|e| e.group == 3 && e.binding == 8));
        let slots = plan
            .entries
            .iter()
            .map(|e| (e.group, e.binding))
            .collect::<BTreeSet<_>>();
        assert_eq!(slots.len(), plan.entries.len());
    }

    #[test]
    fn ignores_comment_directives_and_function_scope_resources() {
        let shader = r#"
            // #version 999
            #version 430
            #define UNUSED uniform sampler2D fake; \
                uniform image2D alsoFake;
            /* multiline
                uniform sampler2D invisible;
            */
            void helper() {
                uniform sampler2D impossible;
            }
            layout(local_size_x = 32) in;
            uniform sampler2D realTexture[4];
        "#;
        let reflected = reflect_glsl_program(shader);
        assert_eq!(reflected.glsl_version, Some(430));
        assert_eq!(reflected.workgroup_size, Some([32, 1, 1]));
        assert_eq!(reflected.samplers.len(), 1);
        assert_eq!(reflected.samplers[0].name, "realTexture");
        assert_eq!(reflected.samplers[0].array_size, Some(4));
    }

    #[test]
    fn explicit_bindings_take_priority_over_implicit_slots() {
        let reflected = reflect_glsl_program(
            r#"
                #version 460
                uniform sampler2D aImplicit;
                layout(binding = 0) uniform sampler2D zExplicit;
                layout(std140) uniform AImplicitBlock { vec4 a; } aBlock;
                layout(std430, binding = 0) buffer ZExplicitBlock { vec4 z; } zBlock;
                uniform image2D aImplicitImage;
                layout(r32f, binding = 0) uniform image2D zExplicitImage;
            "#,
        );
        let bindings = plan_wgsl_binding_table(&reflected);
        for name in ["zExplicit", "ZExplicitBlock", "zExplicitImage"] {
            let entry = bindings.entries.iter().find(|e| e.name == name).unwrap();
            assert_eq!(entry.binding, 0, "{name}");
            assert_eq!(
                entry.wgsl_attributes(),
                format!("@group({}) @binding(0)", entry.group)
            );
        }
        assert_eq!(
            reflected
                .storage_images
                .iter()
                .find(|e| e.name == "zExplicitImage")
                .unwrap()
                .format
                .as_deref(),
            Some("r32f")
        );
        let slots: BTreeSet<_> = bindings
            .entries
            .iter()
            .map(|e| (e.group, e.binding))
            .collect();
        assert_eq!(slots.len(), bindings.entries.len());
    }
}
