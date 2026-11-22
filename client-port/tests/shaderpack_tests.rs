// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Tests for the clean-room Iris 1.7+/OptiFine shaderpack loader. All pack
//! contents below are synthetic, written for this suite — no third-party
//! shaderpack code is used anywhere.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use megu_client_mesh::shaderpack::*;

// ---------------------------------------------------------------------------
// Synthetic pack fixtures
// ---------------------------------------------------------------------------

/// A clean-room Iris 1.7+ compute/SSBO/custom-image "voxel" pack exercising
/// every pipeline stage.
fn voxel_pack() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "shaders/shaders.properties",
            "\
bufferObject.0 = 2097152
bufferObject.1 = 65536
bufferObject.0.scale = 1.0
image.voxelMap = voxelMap rgba8 rgba8 ubyte true false 256 256 256
texture.gbuffers.gaux1 = textures/noise.png
iris.features.required = COMPUTE_SHADERS SSBO CUSTOM_IMAGES
iris.features.optional = ENTITY_TRANSLUCENT
shadow.enabled = true
shadowDistance = 192.0
shadowMapResolution = 2048
scale.composite = 1.0
size.buffer.voxelBuffer = 512 512
flip.composite.colortex0 = false
alphaTest.gbuffers_cutout = GREATER 0.1
blend.composite.colortex0 = src_alpha one_minus_src_alpha
profile.HIGH = program:composite VIEW_DIST:16
screen = VIEW_DIST FOG
sliders = VIEW_DIST
",
        ),
        (
            "shaders/block.properties",
            "\
block.1 = minecraft:stone
block.2 = minecraft:dirt minecraft:grass_block:snowy=true \\
          minecraft:podzol
",
        ),
        ("shaders/item.properties", "item.7 = minecraft:stick minecraft:torch\n"),
        (
            "shaders/entity.properties",
            "entity.9 = minecraft:zombie minecraft:skeleton\n",
        ),
        (
            "shaders/lib/common.glsl",
            "#define PACK_COMMON 1\nfloat pack_common() { return 1.0; }\n",
        ),
        (
            "shaders/begin.csh",
            "\
#version 460 core
#include \"/lib/common.glsl\"
const ivec3 workGroups = ivec3(8, 8, 8);
const vec2 workGroupsRender = vec2(0.5, 0.5);
void main() {}
",
        ),
        ("shaders/shadow.vsh", "#version 460 core\nvoid main() {}\n"),
        ("shaders/shadow.fsh", "#version 460 core\nvoid main() {}\n"),
        (
            "shaders/shadowcomp.csh",
            "#version 460 core\nconst ivec3 workGroups = ivec3(16, 16, 1);\nvoid main() {}\n",
        ),
        (
            "shaders/prepare.csh",
            "#version 460 core\nconst ivec3 workGroups = ivec3(4, 4, 4);\nvoid main() {}\n",
        ),
        (
            "shaders/gbuffers_terrain.vsh",
            "#version 460 core\n#include \"/lib/common.glsl\"\nvoid main() {}\n",
        ),
        (
            "shaders/gbuffers_terrain.fsh",
            "#version 460 core\n/* DRAWBUFFERS:0,1 */\nvoid main() {}\n",
        ),
        (
            "shaders/dh_terrain.vsh",
            "#version 460 core\nvoid main() {}\n",
        ),
        (
            "shaders/dh_terrain.fsh",
            "#version 460 core\nvoid main() {}\n",
        ),
        (
            "shaders/deferred.csh",
            "#version 460 core\nconst ivec3 workGroups = ivec3(8, 8, 1);\nvoid main() {}\n",
        ),
        ("shaders/composite.vsh", "#version 460 core\nvoid main() {}\n"),
        (
            "shaders/composite.fsh",
            "#version 460 core\n/* RENDERTARGETS:0,1 */\nvoid main() {}\n",
        ),
        ("shaders/final.vsh", "#version 460 core\nvoid main() {}\n"),
        ("shaders/final.fsh", "#version 460 core\nvoid main() {}\n"),
    ]
}

#[test]
fn iris_compute_ssbo_custom_image_pack_plans_all_stages() {
    let source = ShaderPackSource::from_memory(voxel_pack()).expect("pack loads");
    assert_eq!(source.origin(), PackOrigin::InMemory);

    let plan = plan_shader_pipeline(
        &source,
        PackDimension::Overworld,
        &GpuShaderCaps::modern_compute(),
    )
    .expect("plan succeeds");

    // Exact stage order: Begin -> Shadow(shadow, shadowcomp) -> Prepare ->
    // Gbuffers -> DistantHorizons -> Deferred -> Composite -> Final.
    let pass_order: Vec<(&str, PipelineStage, PassKind)> = plan
        .passes
        .iter()
        .map(|p| (p.name.as_str(), p.stage, p.kind))
        .collect();
    assert_eq!(
        pass_order,
        vec![
            ("begin", PipelineStage::Begin, PassKind::Compute),
            ("shadow", PipelineStage::Shadow, PassKind::Graphics),
            ("shadowcomp", PipelineStage::Shadow, PassKind::Compute),
            ("prepare", PipelineStage::Prepare, PassKind::Compute),
            ("gbuffers_terrain", PipelineStage::Gbuffers, PassKind::Graphics),
            ("dh_terrain", PipelineStage::DistantHorizons, PassKind::Graphics),
            ("deferred", PipelineStage::Deferred, PassKind::Compute),
            ("composite", PipelineStage::Composite, PassKind::Graphics),
            ("final", PipelineStage::Final, PassKind::Graphics),
        ]
    );

    // Compute workgroup extraction + include expansion through the planner.
    let begin = &plan.passes[0];
    assert_eq!(begin.work_groups, Some([8, 8, 8]));
    let begin_csh = &begin.shaders[&ShaderKind::Compute];
    assert_eq!(begin_csh.work_groups_render, Some([0.5, 0.5]));
    assert_eq!(begin_csh.glsl_version, Some(460));
    assert_eq!(begin_csh.version_profile.as_deref(), Some("core"));
    assert_eq!(begin_csh.included, vec!["lib/common.glsl".to_owned()]);
    assert!(begin_csh.source.contains("pack_common()"));
    assert_eq!(
        begin_csh.defines["PACK_COMMON"],
        ShaderOption {
            name: "PACK_COMMON".to_owned(),
            value: Some("1".to_owned()),
            enabled: true,
        }
    );

    // Render targets.
    assert_eq!(
        plan.passes[7].render_targets.as_deref(),
        Some(&["0".to_owned(), "1".to_owned()][..])
    );

    // Properties: SSBOs, custom image, textures, features.
    let props = plan.properties.as_ref().expect("shaders.properties parsed");
    assert_eq!(props.ssbos.len(), 2);
    assert_eq!(props.ssbos[&0].byte_size, 2_097_152);
    assert_eq!(props.ssbos[&0].scale, Some(1.0));
    assert_eq!(props.ssbos[&1].byte_size, 65_536);
    let image = &props.images["voxelMap"];
    assert_eq!(image.sampler, "voxelMap");
    assert_eq!(image.format, "rgba8");
    assert!(image.clear);
    assert!(!image.relative);
    assert_eq!(image.size, vec![256.0, 256.0, 256.0]);
    assert_eq!(
        props.textures[&("gbuffers".to_owned(), "gaux1".to_owned())].path,
        "textures/noise.png"
    );
    assert!(props.required_features.contains(&IrisFeature::ComputeShaders));
    assert!(props.required_features.contains(&IrisFeature::Ssbo));
    assert!(props.required_features.contains(&IrisFeature::CustomImages));
    assert!(props
        .optional_features
        .contains(&IrisFeature::EntityTranslucent));
    assert_eq!(props.shadow.enabled, Some(true));
    assert_eq!(props.shadow.distance, Some(192.0));
    assert_eq!(props.shadow.resolution, Some(2048));
    assert_eq!(props.buffer_sizes["voxelBuffer"], (512, 512));
    assert_eq!(props.pass_scales["composite"], vec![1.0]);
    assert_eq!(
        props.flips,
        vec![("composite".to_owned(), "colortex0".to_owned(), false)]
    );
    assert_eq!(
        props.alpha_tests["gbuffers_cutout"],
        AlphaTest::Func {
            func: "greater".to_owned(),
            value: 0.1
        }
    );
    assert_eq!(props.blends.len(), 1);
    assert_eq!(props.blends[0].pass, "composite");
    assert_eq!(props.blends[0].buffer, "colortex0");
    assert!(props.profiles.contains_key("HIGH"));
    assert_eq!(props.screens[""], vec!["VIEW_DIST", "FOG"]);
    assert_eq!(props.sliders, vec!["VIEW_DIST"]);

    // Material ID maps.
    let blocks = plan.block_ids.as_ref().expect("block.properties parsed");
    assert_eq!(blocks.lookup_name("minecraft:stone"), Some(1));
    assert_eq!(blocks.lookup_name("stone"), Some(1));
    assert_eq!(blocks.lookup_name("minecraft:podzol"), Some(2));
    let mut props_state = BTreeMap::new();
    props_state.insert("snowy".to_owned(), "true".to_owned());
    assert_eq!(
        blocks.lookup("minecraft:grass_block", &props_state),
        Some(2)
    );
    props_state.insert("snowy".to_owned(), "false".to_owned());
    assert_eq!(blocks.lookup("minecraft:grass_block", &props_state), None);
    let items = plan.item_ids.as_ref().expect("item.properties parsed");
    assert_eq!(items.lookup_name("minecraft:stick"), Some(7));
    let entities = plan.entity_ids.as_ref().expect("entity.properties parsed");
    assert_eq!(entities.lookup_name("minecraft:zombie"), Some(9));
    assert_eq!(entities.lookup_name("minecraft:creeper"), None);

    // Vulkan/wgpu-class profile: fully compatible.
    assert_eq!(
        plan.compatibility,
        ShaderCompatibilityReport::Compatible
    );
    assert!(plan.is_compatible());

    // Legacy OpenGL-4.1-class profile: fails closed with every missing
    // capability enumerated.
    let legacy = plan_shader_pipeline(
        &source,
        PackDimension::Overworld,
        &GpuShaderCaps::legacy_opengl(),
    )
    .expect("plan still builds");
    let missing = match &legacy.compatibility {
        ShaderCompatibilityReport::MissingCapabilities { missing } => missing.clone(),
        other => panic!("expected MissingCapabilities, got {other:?}"),
    };
    assert!(!legacy.is_compatible());
    for needle in [
        "compute shaders required",
        "shader storage buffers (SSBO) required",
        "custom image units required",
        "GLSL 460 required",
        "bufferObject.0 needs",
    ] {
        assert!(
            missing.iter().any(|m| m.contains(needle)),
            "missing reason {needle:?} not reported in {missing:?}"
        );
    }
}

#[test]
fn include_preprocessor_resolves_nested_and_rejects_cycles_and_traversal() {
    let files = vec![
        (
            "main.fsh",
            "\
#version 460 core
#include \"/lib/a.glsl\"
#include \"sibling.glsl\"
void main() {}
",
        ),
        (
            "sibling.glsl",
            "#define SIBLING_OK\nfloat sibling_fn() { return 0.0; }\n",
        ),
        ("lib/a.glsl", "#include \"b.glsl\"\nfloat a_fn() { return 1.0; }\n"),
        ("lib/b.glsl", "#define DEPTH_OK 1\nfloat b_fn() { return 2.0; }\n"),
        // Cycle: cycle_a -> cycle_b -> cycle_a.
        ("cycle_a.glsl", "#include \"cycle_b.glsl\"\n"),
        ("cycle_b.glsl", "#include \"cycle_a.glsl\"\n"),
        ("cyclic.fsh", "#include \"cycle_a.glsl\"\n"),
        // Traversal attempts.
        ("trav.fsh", "#include \"../escape.glsl\"\n"),
        ("trav2.fsh", "#include \"/../etc/passwd\"\n"),
        ("missing.fsh", "#include \"no/such/file.glsl\"\n"),
        // An #include inside a block comment must NOT be expanded.
        (
            "commented.fsh",
            "/*\n#include \"no/such/file.glsl\"\n*/\nvoid main() {}\n",
        ),
    ];
    let source = ShaderPackSource::from_memory(files).expect("pack loads");

    // Valid nested expansion: absolute + relative includes both resolve.
    let pp = preprocess_shader_source(&source, "main.fsh").expect("preprocess");
    assert_eq!(pp.glsl_version, Some(460));
    assert!(pp.source.contains("a_fn()"));
    assert!(pp.source.contains("b_fn()"));
    assert!(pp.source.contains("sibling_fn()"));
    assert_eq!(
        pp.included,
        vec![
            "lib/a.glsl".to_owned(),
            "lib/b.glsl".to_owned(),
            "sibling.glsl".to_owned()
        ]
    );
    assert!(pp.defines["DEPTH_OK"].enabled);
    assert!(pp.defines.contains_key("SIBLING_OK"));

    // Relative include inside lib/: a.glsl finds b.glsl next to itself.
    let lib_pp = preprocess_shader_source(&source, "lib/a.glsl").expect("preprocess lib");
    assert_eq!(lib_pp.included, vec!["lib/b.glsl".to_owned()]);

    // Cycle detection.
    let err = preprocess_shader_source(&source, "cyclic.fsh").unwrap_err();
    assert!(
        matches!(err, ShaderPackError::IncludeCycle { .. }),
        "expected IncludeCycle, got {err:?}"
    );

    // Traversal is rejected fail-closed.
    for f in ["trav.fsh", "trav2.fsh"] {
        let err = preprocess_shader_source(&source, f).unwrap_err();
        assert!(
            matches!(err, ShaderPackError::UnsafePath { .. }),
            "{f}: expected UnsafePath, got {err:?}"
        );
    }

    // Missing include fails closed.
    let err = preprocess_shader_source(&source, "missing.fsh").unwrap_err();
    assert!(
        matches!(err, ShaderPackError::IncludeNotFound { .. }),
        "expected IncludeNotFound, got {err:?}"
    );

    // Commented-out include is ignored entirely.
    preprocess_shader_source(&source, "commented.fsh").expect("commented include ignored");

    // Depth cap: chain deeper than MAX_INCLUDE_DEPTH fails closed.
    let mut deep: Vec<(String, String)> = Vec::new();
    for i in 0..=MAX_INCLUDE_DEPTH + 1 {
        deep.push((
            format!("d{i}.glsl"),
            format!("#include \"d{}.glsl\"\n", i + 1),
        ));
    }
    deep.push(("entry.fsh".to_owned(), "#include \"d0.glsl\"\n".to_owned()));
    let deep_source = ShaderPackSource::from_memory(deep).expect("deep pack");
    let err = preprocess_shader_source(&deep_source, "entry.fsh").unwrap_err();
    assert!(
        matches!(err, ShaderPackError::IncludeDepthExceeded { .. }),
        "expected IncludeDepthExceeded, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Zip construction helpers (stored + deflate entries)
// ---------------------------------------------------------------------------

fn u16(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}
fn u32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

/// Wrap `data` in a single stored-mode DEFLATE block (BFINAL=1, BTYPE=00).
fn deflate_stored_block(data: &[u8]) -> Vec<u8> {
    assert!(data.len() <= 0xFFFF);
    let mut out = vec![0x01];
    let len = data.len() as u16;
    out.extend_from_slice(&u16(len));
    out.extend_from_slice(&u16(!len));
    out.extend_from_slice(data);
    out
}

struct ZipEntry<'a> {
    name: &'a str,
    /// Compressed bytes as stored in the archive.
    data: Vec<u8>,
    /// Declared uncompressed size.
    uncompressed: usize,
    method: u16,
}

fn build_zip(entries: &[ZipEntry<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for e in entries {
        let lh_off = out.len() as u32;
        out.extend_from_slice(&u32(0x0403_4b50));
        out.extend_from_slice(&u16(20));
        out.extend_from_slice(&u16(0));
        out.extend_from_slice(&u16(e.method));
        out.extend_from_slice(&u16(0));
        out.extend_from_slice(&u16(0));
        out.extend_from_slice(&u32(0));
        out.extend_from_slice(&u32(e.data.len() as u32));
        out.extend_from_slice(&u32(e.uncompressed as u32));
        out.extend_from_slice(&u16(e.name.len() as u16));
        out.extend_from_slice(&u16(0));
        out.extend_from_slice(e.name.as_bytes());
        out.extend_from_slice(&e.data);

        central.extend_from_slice(&u32(0x0201_4b50));
        central.extend_from_slice(&u16(20));
        central.extend_from_slice(&u16(20));
        central.extend_from_slice(&u16(0));
        central.extend_from_slice(&u16(e.method));
        central.extend_from_slice(&u16(0));
        central.extend_from_slice(&u16(0));
        central.extend_from_slice(&u32(0));
        central.extend_from_slice(&u32(e.data.len() as u32));
        central.extend_from_slice(&u32(e.uncompressed as u32));
        central.extend_from_slice(&u16(e.name.len() as u16));
        central.extend_from_slice(&u16(0));
        central.extend_from_slice(&u16(0));
        central.extend_from_slice(&u16(0));
        central.extend_from_slice(&u16(0));
        central.extend_from_slice(&u32(0));
        central.extend_from_slice(&u32(lh_off));
        central.extend_from_slice(e.name.as_bytes());
    }
    let cd_off = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&u32(0x0605_4b50));
    out.extend_from_slice(&u16(0));
    out.extend_from_slice(&u16(0));
    out.extend_from_slice(&u16(entries.len() as u16));
    out.extend_from_slice(&u16(entries.len() as u16));
    out.extend_from_slice(&u32(cd_size));
    out.extend_from_slice(&u32(cd_off));
    out.extend_from_slice(&u16(0));
    out
}

/// A real compressed DEFLATE stream (dynamic Huffman, raw `wbits=-15`) for a
/// 1506-byte repetitive shader source, precomputed at authoring time.
const DEFLATE_STREAM: &[u8] = &[
    211, 215, 87, 40, 174, 204, 43, 201, 72, 45, 201, 76, 86, 72, 206, 73, 77, 204, 211, 45,
    202, 207, 207, 85, 40, 206, 72, 76, 73, 45, 82, 200, 201, 204, 75, 229, 210, 31, 85, 51,
    170, 102, 84, 205, 168, 154, 81, 53, 244, 87, 163, 165, 16, 228, 234, 231, 226, 26, 20,
    226, 24, 228, 238, 26, 18, 108, 101, 160, 99, 168, 160, 165, 207, 85, 150, 159, 153, 162,
    144, 155, 152, 153, 167, 161, 169, 80, 173, 144, 158, 19, 239, 86, 148, 152, 238, 156,
    159, 147, 95, 164, 96, 171, 80, 150, 154, 108, 162, 97, 168, 103, 160, 105, 173, 80, 203,
    5, 0,
];

fn deflated_payload() -> Vec<u8> {
    let mut data = Vec::new();
    for _ in 0..40 {
        data.extend_from_slice(b"// synthetic clean-room shader line\n");
    }
    data.extend_from_slice(b"/* RENDERTARGETS:0,1 */\nvoid main() { gl_FragColor = vec4(1.0); }\n");
    data
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "megu_shaderpack_{}_{}",
        tag,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn zip_and_dir_external_packs_load_and_reject_traversal() {
    let dir = temp_dir("zipdir");

    // --- a clean zip with stored + deflate entries -----------------------
    let shader_text = "#version 460 core\nvoid main() {}\n";
    let payload = deflated_payload();
    assert_eq!(payload.len(), 1506);
    let zip_bytes = build_zip(&[
        ZipEntry {
            name: "shaders/composite.fsh",
            data: shader_text.as_bytes().to_vec(),
            uncompressed: shader_text.len(),
            method: 0,
        },
        ZipEntry {
            name: "shaders/composite.vsh",
            data: deflate_stored_block(shader_text.as_bytes()),
            uncompressed: shader_text.len(),
            method: 8,
        },
        ZipEntry {
            name: "shaders/big.fsh",
            data: DEFLATE_STREAM.to_vec(),
            uncompressed: payload.len(),
            method: 8,
        },
        ZipEntry {
            name: "shaders/lib/",
            data: Vec::new(),
            uncompressed: 0,
            method: 0,
        },
        ZipEntry {
            name: "shaders/lib/util.glsl",
            data: b"float util() { return 0.0; }\n".to_vec(),
            uncompressed: b"float util() { return 0.0; }\n".len(),
            method: 0,
        },
    ]);
    let zip_path = dir.join("clean.zip");
    fs::write(&zip_path, &zip_bytes).expect("write zip");
    let src = ShaderPackSource::open(&zip_path).expect("zip opens");
    assert_eq!(src.origin(), PackOrigin::ZipArchive);
    assert_eq!(
        src.read("composite.fsh").map(|b| b.len()),
        Some(shader_text.len())
    );
    assert_eq!(src.read("composite.vsh"), Some(shader_text.as_bytes()));
    assert_eq!(src.read("big.fsh"), Some(payload.as_slice()));
    assert!(src.contains("lib/util.glsl"));

    // --- zip traversal entries are rejected fail-closed -------------------
    for name in [
        "shaders/../escape.glsl",
        "/abs.glsl",
        "shaders/back\\slash.glsl",
        "shaders/nul\0.glsl",
    ] {
        let evil = build_zip(&[ZipEntry {
            name,
            data: b"x".to_vec(),
            uncompressed: 1,
            method: 0,
        }]);
        let p = dir.join("evil.zip");
        fs::write(&p, &evil).expect("write evil zip");
        let err = ShaderPackSource::open_zip(&p).unwrap_err();
        assert!(
            matches!(err, ShaderPackError::UnsafePath { .. }),
            "{name:?}: expected UnsafePath, got {err:?}"
        );
    }

    // Corrupt archives fail closed.
    fs::write(dir.join("garbage.zip"), b"not a zip at all").expect("write");
    assert!(matches!(
        ShaderPackSource::open_zip(&dir.join("garbage.zip")).unwrap_err(),
        ShaderPackError::ZipMalformed { .. }
    ));

    // --- directory pack ---------------------------------------------------
    let pack_dir = dir.join("MyPack");
    fs::create_dir_all(pack_dir.join("shaders/lib")).expect("mkdir");
    fs::write(
        pack_dir.join("shaders/composite.fsh"),
        shader_text.as_bytes(),
    )
    .expect("write fsh");
    fs::write(pack_dir.join("shaders/composite.vsh"), shader_text.as_bytes())
        .expect("write vsh");
    fs::write(
        pack_dir.join("shaders/lib/util.glsl"),
        b"float util() { return 0.0; }\n",
    )
    .expect("write util");
    let src = ShaderPackSource::open(&pack_dir).expect("dir opens");
    assert_eq!(src.origin(), PackOrigin::Directory);
    assert_eq!(src.read("composite.fsh"), Some(shader_text.as_bytes()));
    assert!(src.contains("lib/util.glsl"));

    // Symlinks escaping the pack are rejected.
    #[cfg(unix)]
    {
        fs::write(dir.join("secret.glsl"), b"secret\n").expect("write secret");
        std::os::unix::fs::symlink(
            dir.join("secret.glsl"),
            pack_dir.join("shaders/evil.glsl"),
        )
        .expect("symlink");
        let err = ShaderPackSource::open_directory(&pack_dir).unwrap_err();
        assert!(
            matches!(err, ShaderPackError::UnsafePath { .. }),
            "expected UnsafePath for escaping symlink, got {err:?}"
        );
        fs::remove_file(pack_dir.join("shaders/evil.glsl")).expect("cleanup");
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn block_item_entity_properties_and_user_options_parse_deterministically() {
    let block_text = "\
# material ids
block.1 = minecraft:stone
block.2 = minecraft:dirt minecraft:grass_block:snowy=true \\
          minecraft:podzol minecraft:mycelium:snowy=false
block.3 = minecraft:sand minecraft:red_sand
item.7  = minecraft:stick minecraft:torch
entity.9 = minecraft:zombie minecraft:skeleton
";
    // Parse twice: deterministic byte-for-byte results.
    let a = IdMap::from_block_properties(block_text).expect("block.properties");
    let b = IdMap::from_block_properties(block_text).expect("block.properties");
    assert_eq!(a.entries(), b.entries());
    assert_eq!(a.entries().len(), 3);
    let id2_names: Vec<&str> = a.entries()[1]
        .1
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(
        id2_names,
        vec![
            "minecraft:dirt",
            "minecraft:grass_block",
            "minecraft:podzol",
            "minecraft:mycelium"
        ]
    );
    let mut snowy = BTreeMap::new();
    snowy.insert("snowy".to_owned(), "true".to_owned());
    assert_eq!(a.lookup("minecraft:grass_block", &snowy), Some(2));
    snowy.insert("snowy".to_owned(), "false".to_owned());
    assert_eq!(a.lookup("minecraft:mycelium", &snowy), Some(2));
    assert_eq!(a.lookup("minecraft:grass_block", &snowy), None);
    // Lowest id wins for a name declared under two ids.
    let dup = IdMap::from_block_properties(
        "block.9 = minecraft:stone\nblock.4 = minecraft:stone\n",
    )
    .expect("dup");
    assert_eq!(dup.lookup_name("minecraft:stone"), Some(4));

    let items =
        IdMap::from_item_properties("item.7 = minecraft:stick minecraft:torch\n")
            .expect("items");
    assert_eq!(items.lookup_name("minecraft:torch"), Some(7));
    let entities =
        IdMap::from_entity_properties("entity.9 = minecraft:zombie\n").expect("entities");
    assert_eq!(entities.lookup_name("minecraft:zombie"), Some(9));

    // Profiles, screens and sliders parse into ordered token lists.
    let props_text = "\
profile.FANCY = program:composite FOG:true BLOOM:true
profile.FAST = FOG:false BLOOM:false
screen = [<profile>] FOG BLOOM
screen.END = RENDER_CLOUDS
sliders = FOG_INTENSITY BLOOM_STRENGTH
";
    let props = ShaderProperties::parse(props_text).expect("profiles");
    assert_eq!(props.profiles.len(), 2);
    assert_eq!(
        props.profiles["FANCY"].tokens,
        vec!["program:composite", "FOG:true", "BLOOM:true"]
    );
    assert_eq!(props.screens[""], vec!["[<profile>]", "FOG", "BLOOM"]);
    assert_eq!(props.screens["END"], vec!["RENDER_CLOUDS"]);
    assert_eq!(props.sliders, vec!["FOG_INTENSITY", "BLOOM_STRENGTH"]);

    // #define and commented // #define options are collected; an enabled
    // define always wins over a commented one.
    let shader = "\
#version 460 core
#define FOG 0.5
// #define BLOOM
// #define FOG 0.9
#define NOISE_SCALE 16
void main() {}
";
    let files = vec![("x.fsh", shader)];
    let src = ShaderPackSource::from_memory(files).expect("src");
    let pp = preprocess_shader_source(&src, "x.fsh").expect("preprocess");
    assert_eq!(
        pp.defines["FOG"],
        ShaderOption {
            name: "FOG".to_owned(),
            value: Some("0.5".to_owned()),
            enabled: true
        }
    );
    assert_eq!(
        pp.defines["BLOOM"],
        ShaderOption {
            name: "BLOOM".to_owned(),
            value: None,
            enabled: false
        }
    );
    assert_eq!(
        pp.defines["NOISE_SCALE"].value.as_deref(),
        Some("16")
    );
}

#[test]
fn dimension_world_dirs_override_and_isolate() {
    let files = vec![
        ("final.vsh", "#version 460 core\nvoid main() {}\n".to_owned()),
        ("final.fsh", "#version 460 core\n// root\nvoid main() {}\n".to_owned()),
        (
            "world0/final.fsh",
            "#version 460 core\n// world0 override\nvoid main() {}\n".to_owned(),
        ),
        (
            "world-1/final.fsh",
            "#version 460 core\n// nether\nvoid main() {}\n".to_owned(),
        ),
    ];
    let source = ShaderPackSource::from_memory(files).expect("pack");
    assert_eq!(source.dimensions(), vec![-1, 0]);

    let overworld = plan_shader_pipeline(
        &source,
        PackDimension::Overworld,
        &GpuShaderCaps::modern_compute(),
    )
    .expect("plan");
    let fsh = &overworld.passes[0].shaders[&ShaderKind::Fragment];
    assert!(fsh.source.contains("world0 override"));
    assert_eq!(fsh.path, "world0/final.fsh");

    // Nether has only final.fsh in world-1; the root vsh still completes the
    // program (per-file override).
    let nether = plan_shader_pipeline(
        &source,
        PackDimension::Nether,
        &GpuShaderCaps::modern_compute(),
    )
    .expect("plan");
    let fsh = &nether.passes[0].shaders[&ShaderKind::Fragment];
    assert!(fsh.source.contains("nether"));
    // world0 files are isolated away.
    assert!(!fsh.source.contains("world0 override"));
}

#[test]
fn malformed_packs_fail_closed() {
    // Empty pack.
    let empty = ShaderPackSource::from_memory(Vec::<(&str, &str)>::new()).expect("empty");
    let err = plan_shader_pipeline(
        &empty,
        PackDimension::Overworld,
        &GpuShaderCaps::modern_compute(),
    )
    .unwrap_err();
    assert!(matches!(err, ShaderPackError::EmptyPack));

    // Graphics program missing its fragment stage.
    let half = ShaderPackSource::from_memory(vec![(
        "composite.vsh",
        "#version 460 core\nvoid main() {}\n",
    )])
    .expect("half pack");
    let err = plan_shader_pipeline(
        &half,
        PackDimension::Overworld,
        &GpuShaderCaps::modern_compute(),
    )
    .unwrap_err();
    assert!(matches!(err, ShaderPackError::IncompleteProgram { .. }));

    // Malformed property value.
    let bad = ShaderProperties::parse("bufferObject.x = notanumber\n");
    assert!(matches!(
        bad.unwrap_err(),
        ShaderPackError::MalformedProperties { .. }
    ));
}
