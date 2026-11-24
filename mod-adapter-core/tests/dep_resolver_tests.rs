// SPDX-License-Identifier: LGPL-3.0-or-later

//! Integration tests for `dep_resolver` (Lane UX-DEPS).
//!
//! Covers the Modrinth auto-fetch half — exact query URLs, real-shape
//! `/v2/project/{id}/version` JSON parsing, fail-closed URL/digest
//! validation, EN/JA recovery summaries, and consent-gated
//! content-addressed install — plus the restart-free hot-reload
//! classifier (datapack/bundle/subset-VM safe vs Mixin/Tier-3/native).
//!
//! Every fixture is real bytes: ZIP/JAR archives assembled member-by-member
//! (stored entries, real central directory + EOCD) and JVMS §4 `.class`
//! files — no `javac`, no external JARs, no crates, no clock.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use mod_adapter_core::bytecode_subset_vm::testkit::{ClassAssembler, MethodSpec};
use mod_adapter_core::dep_resolver::{
    assess_hot_reload_after_add, install_resolved_artifact, parse_modrinth_versions_json,
    plan_dep_recovery, resolve_for_query, DepRecoveryStatus, DepResolveError, MissingDepQuery,
    ReloadRequirement, ResolvedModrinthArtifact,
};
use mod_adapter_core::ecosystem_compat::{
    plan_dual_loader_mods_dir, MissingDependency, ModLoaderKind,
};
use mod_adapter_core::mod_sync::{self, ConsentToken, ContentAddressedModCache, MAX_MOD_JAR_BYTES};
use mod_adapter_core::version_compat::VersionRange;

// ---------------------------------------------------------------------------
// Scratch dirs (pid + counter — deterministic, never SystemTime)
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

fn fixture_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "megu-dep-resolver-{}-{}-{tag}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).expect("create fixture dir");
    #[cfg(unix)]
    let dir = fs::canonicalize(&dir).expect("canonicalize fixture dir");
    dir
}

// ---------------------------------------------------------------------------
// ZIP/JAR builder (stored members, real central directory + EOCD)
// ---------------------------------------------------------------------------

use megu_core::hash::crc32_ieee as crc32;

/// Pack `entries` (name → bytes) into a real ZIP archive with stored
/// (uncompressed) members.
fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let name = name.as_bytes();
        let crc = crc32(data);
        let size = data.len() as u32;
        let offset = out.len() as u32;

        out.extend(0x0403_4B50u32.to_le_bytes());
        out.extend(10u16.to_le_bytes()); // version needed
        out.extend(0u16.to_le_bytes()); // flags
        out.extend(0u16.to_le_bytes()); // method: stored
        out.extend(0u16.to_le_bytes()); // mod time
        out.extend(0u16.to_le_bytes()); // mod date
        out.extend(crc.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes()); // extra len
        out.extend(name);
        out.extend(*data);

        central.extend(0x0201_4B50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes()); // version made by
        central.extend(10u16.to_le_bytes()); // version needed
        central.extend(0u16.to_le_bytes()); // flags
        central.extend(0u16.to_le_bytes()); // method
        central.extend(0u16.to_le_bytes()); // time
        central.extend(0u16.to_le_bytes()); // date
        central.extend(crc.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend((name.len() as u16).to_le_bytes());
        central.extend(0u16.to_le_bytes()); // extra len
        central.extend(0u16.to_le_bytes()); // comment len
        central.extend(0u16.to_le_bytes()); // disk
        central.extend(0u16.to_be_bytes()); // internal attrs
        central.extend(0u32.to_le_bytes()); // external attrs
        central.extend(offset.to_le_bytes());
        central.extend(name);
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend(central);
    out.extend(0x0605_4B50u32.to_le_bytes());
    out.extend(0u16.to_le_bytes()); // disk
    out.extend(0u16.to_le_bytes()); // cd disk
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(cd_size.to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes()); // comment len
    out
}

// ---------------------------------------------------------------------------
// JVM .class builder with annotation attributes (Mixin fixtures)
// ---------------------------------------------------------------------------

/// One constant-pool item; `Wide` is the unusable second slot of a
/// `Long`/`Double` entry.
enum CpItem {
    Utf8(Vec<u8>),
    Class(u16),
    Int(i32),
    Long,
    Wide,
}

/// Constant-pool assembler with deduplication; `Long` entries consume two
/// indices so every built class exercises dual-slot skipping.
struct Cp {
    items: Vec<CpItem>,
    utf8: HashMap<Vec<u8>, u16>,
    class: HashMap<Vec<u8>, u16>,
    ints: HashMap<i32, u16>,
}

impl Cp {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            utf8: HashMap::new(),
            class: HashMap::new(),
            ints: HashMap::new(),
        }
    }

    fn push(&mut self, item: CpItem) -> u16 {
        let index = (self.items.len() + 1) as u16;
        self.items.push(item);
        index
    }

    fn utf8(&mut self, s: &str) -> u16 {
        self.utf8_bytes(s.as_bytes())
    }

    fn utf8_bytes(&mut self, bytes: &[u8]) -> u16 {
        if let Some(&i) = self.utf8.get(bytes) {
            return i;
        }
        let index = self.push(CpItem::Utf8(bytes.to_vec()));
        self.utf8.insert(bytes.to_vec(), index);
        index
    }

    fn utf8_idx(&self, s: &str) -> u16 {
        self.utf8[s.as_bytes()]
    }

    fn class(&mut self, internal_name: &str) -> u16 {
        if let Some(&i) = self.class.get(internal_name.as_bytes()) {
            return i;
        }
        let name_index = self.utf8(internal_name);
        let index = self.push(CpItem::Class(name_index));
        self.class.insert(internal_name.as_bytes().to_vec(), index);
        index
    }

    fn class_idx(&self, internal_name: &str) -> u16 {
        self.class[internal_name.as_bytes()]
    }

    fn int(&mut self, v: i32) -> u16 {
        if let Some(&i) = self.ints.get(&v) {
            return i;
        }
        let index = self.push(CpItem::Int(v));
        self.ints.insert(v, index);
        index
    }

    fn int_idx(&self, v: i32) -> u16 {
        self.ints[&v]
    }

    /// Insert an unreferenced `CONSTANT_Long` — two slots, matching the
    /// pool padding a real `javac` class full of constants carries.
    fn pad_long(&mut self) {
        self.push(CpItem::Long);
        self.push(CpItem::Wide);
    }

    /// Number of constant-pool indices (entries + wide placeholders).
    fn count(&self) -> u16 {
        (self.items.len() + 1) as u16
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for item in &self.items {
            match item {
                CpItem::Utf8(bytes) => {
                    out.push(1);
                    out.extend((bytes.len() as u16).to_be_bytes());
                    out.extend(bytes);
                }
                CpItem::Class(name_index) => {
                    out.push(7);
                    out.extend(name_index.to_be_bytes());
                }
                CpItem::Int(v) => {
                    out.push(3);
                    out.extend(v.to_be_bytes());
                }
                CpItem::Long => {
                    out.push(5);
                    out.extend(123_456_789i64.to_be_bytes());
                }
                CpItem::Wide => {}
            }
        }
        out
    }
}

/// One annotation `element_value` for the test builder.
enum EV {
    /// `s` string constant.
    S(String),
    /// `c` class literal (internal name).
    Cls(String),
    /// `@` nested annotation.
    Ann(Box<Ann>),
    /// `[` array.
    Arr(Vec<EV>),
}

/// One `annotation` structure plus the attribute it belongs in.
struct Ann {
    /// Type descriptor, e.g. `Lorg/spongepowered/asm/mixin/Mixin;`.
    desc: String,
    elems: Vec<(String, EV)>,
    /// `true` → `RuntimeInvisibleAnnotations` (Mixin's real CLASS
    /// retention), `false` → `RuntimeVisibleAnnotations`.
    invisible: bool,
}

fn ann(desc: &str, elems: Vec<(&str, EV)>, invisible: bool) -> Ann {
    Ann {
        desc: desc.to_string(),
        elems: elems
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        invisible,
    }
}

fn s(v: &str) -> EV {
    EV::S(v.to_string())
}

fn cls(internal: &str) -> EV {
    EV::Cls(internal.to_string())
}

fn write_ev(out: &mut Vec<u8>, cp: &mut Cp, ev: &EV) {
    match ev {
        EV::S(v) => {
            out.push(b's');
            out.extend(cp.utf8(v).to_be_bytes());
        }
        EV::Cls(name) => {
            out.push(b'c');
            out.extend(cp.class(name).to_be_bytes());
        }
        EV::Ann(nested) => {
            out.push(b'@');
            write_ann(out, cp, nested);
        }
        EV::Arr(items) => {
            out.push(b'[');
            out.extend((items.len() as u16).to_be_bytes());
            for item in items {
                write_ev(out, cp, item);
            }
        }
    }
}

fn write_ann(out: &mut Vec<u8>, cp: &mut Cp, ann: &Ann) {
    out.extend(cp.utf8(&ann.desc).to_be_bytes());
    out.extend((ann.elems.len() as u16).to_be_bytes());
    for (name, ev) in &ann.elems {
        out.extend(cp.utf8(name).to_be_bytes());
        write_ev(out, cp, ev);
    }
}

/// `Runtime{Visible,Invisible}Annotations` attribute payload for a group
/// of same-visibility annotations.
fn anns_payload(cp: &mut Cp, anns: &[&Ann]) -> Vec<u8> {
    let mut payload = (anns.len() as u16).to_be_bytes().to_vec();
    for ann in anns {
        write_ann(&mut payload, cp, ann);
    }
    payload
}

struct Meth {
    name: String,
    anns: Vec<Ann>,
}

fn meth(name: &str, anns: Vec<Ann>) -> Meth {
    Meth {
        name: name.to_string(),
        anns,
    }
}

/// Assemble a complete `.class` file: public class `internal` extending
/// `java/lang/Object`, one private `int dummy` field carrying a
/// `ConstantValue` attribute (field-attribute skipping), each method a
/// `()V` stub with a real `Code` attribute (non-annotation attribute
/// skipping) plus its annotation attributes, class-level annotations,
/// and a `SourceFile` attribute.
fn build_class(internal: &str, class_anns: Vec<Ann>, methods: Vec<Meth>) -> Vec<u8> {
    let mut cp = Cp::new();
    // Intern every name the body needs, then pad the pool with a dual-slot
    // Long to prove the parser advances two slots.
    cp.class(internal);
    cp.class("java/lang/Object");
    for s in [
        "Code",
        "ConstantValue",
        "SourceFile",
        "RuntimeVisibleAnnotations",
        "RuntimeInvisibleAnnotations",
        "dummy",
        "I",
        "()V",
        "Dummy.java",
    ] {
        cp.utf8(s);
    }
    for m in &methods {
        cp.utf8(&m.name);
    }
    cp.int(7); // ConstantValue payload

    // Encode annotation attribute payloads first (interning as we go) so
    // the pool is complete before it is serialized.
    let mut method_attrs: Vec<Vec<(u16, Vec<u8>)>> = Vec::new();
    for m in &methods {
        let mut attrs = Vec::new();
        // A real Code attribute ahead of the annotations.
        let code = {
            let mut p = Vec::new();
            p.extend(1u16.to_be_bytes()); // max_stack
            p.extend(1u16.to_be_bytes()); // max_locals
            p.extend(1u32.to_be_bytes()); // code_length
            p.push(0xB1); // return
            p.extend(0u16.to_be_bytes()); // exception_table_length
            p.extend(0u16.to_be_bytes()); // attributes_count
            p
        };
        attrs.push((cp.utf8("Code"), code));
        let (invisible, visible): (Vec<&Ann>, Vec<&Ann>) =
            m.anns.iter().partition(|a| a.invisible);
        for (name, group) in [
            ("RuntimeInvisibleAnnotations", invisible),
            ("RuntimeVisibleAnnotations", visible),
        ] {
            if !group.is_empty() {
                attrs.push((cp.utf8(name), anns_payload(&mut cp, &group)));
            }
        }
        method_attrs.push(attrs);
    }
    let (invisible, visible): (Vec<&Ann>, Vec<&Ann>) =
        class_anns.iter().partition(|a| a.invisible);
    let mut class_attrs: Vec<(u16, Vec<u8>)> = Vec::new();
    for (name, group) in [
        ("RuntimeInvisibleAnnotations", invisible),
        ("RuntimeVisibleAnnotations", visible),
    ] {
        if !group.is_empty() {
            class_attrs.push((cp.utf8(name), anns_payload(&mut cp, &group)));
        }
    }
    // SourceFile — one more non-annotation attribute to skip.
    class_attrs.push((cp.utf8("SourceFile"), cp.utf8("Dummy.java").to_be_bytes().to_vec()));
    cp.pad_long();

    // Serialize: header + pool, then the body.
    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes()); // minor
    out.extend(61u16.to_be_bytes()); // major (Java 17)
    out.extend(cp.count().to_be_bytes());
    out.extend(cp.encode());
    out.extend(0x0021u16.to_be_bytes()); // public + super
    out.extend(cp.class_idx(internal).to_be_bytes());
    out.extend(cp.class_idx("java/lang/Object").to_be_bytes());
    out.extend(0u16.to_be_bytes()); // interfaces_count

    out.extend(1u16.to_be_bytes()); // fields_count
    out.extend(0x0002u16.to_be_bytes()); // private
    out.extend(cp.utf8_idx("dummy").to_be_bytes());
    out.extend(cp.utf8_idx("I").to_be_bytes());
    out.extend(1u16.to_be_bytes()); // one attribute
    out.extend(cp.utf8_idx("ConstantValue").to_be_bytes());
    out.extend(2u32.to_be_bytes());
    out.extend(cp.int_idx(7).to_be_bytes());

    out.extend((methods.len() as u16).to_be_bytes());
    for (m, attrs) in methods.iter().zip(&method_attrs) {
        out.extend(0x0001u16.to_be_bytes()); // public
        out.extend(cp.utf8_idx(&m.name).to_be_bytes());
        out.extend(cp.utf8_idx("()V").to_be_bytes());
        out.extend((attrs.len() as u16).to_be_bytes());
        for (name_index, payload) in attrs {
            out.extend(name_index.to_be_bytes());
            out.extend((payload.len() as u32).to_be_bytes());
            out.extend(payload);
        }
    }

    out.extend((class_attrs.len() as u16).to_be_bytes());
    for (name_index, payload) in &class_attrs {
        out.extend(name_index.to_be_bytes());
        out.extend((payload.len() as u32).to_be_bytes());
        out.extend(payload);
    }
    out
}

// ---------------------------------------------------------------------------
// JAR fixtures
// ---------------------------------------------------------------------------

/// Fabric mod declaring two required deps (`mylibrary >=1.0`, `jei *`)
/// plus the runtime-provided `minecraft` dep that must be filtered out.
fn dep_mod_jar() -> Vec<u8> {
    let manifest = br#"{"schemaVersion":1,"id":"fabricharm","version":"1.0.0","name":"Fabric Charm","depends":{"mylibrary":">=1.0","jei":"*","minecraft":"~1.21"}}"#;
    jar(&[("fabric.mod.json", manifest.as_slice())])
}

/// Fabric mod whose only `.class` members verify under the bounded
/// subset VM (`return`, `iload`, `ireturn` — no host calls).
fn subset_safe_mod_jar() -> Vec<u8> {
    let manifest = br#"{"schemaVersion":1,"id":"safemod","version":"1.0.0","name":"Safe Mod","entrypoints":{"main":["com.safemod.Main"]}}"#;
    let a = ClassAssembler::new("com/safemod/Main");
    let bytes = a.finish(&[
        MethodSpec::new("init", "()V", 1, 1, vec![0xB1]),
        // Instance method `(I)I`: local0=this, local1=arg → iload_1;ireturn.
        MethodSpec::new("tick", "(I)I", 1, 2, vec![0x1B, 0xAC]),
    ]);
    jar(&[
        ("fabric.mod.json", manifest.as_slice()),
        ("com/safemod/Main.class", bytes.as_slice()),
    ])
}

/// Fabric mod with a `.class` using `new` (0xBB) — outside the verified
/// subset, i.e. deep Tier-3-style bytecode.
fn deep_tier3_mod_jar() -> Vec<u8> {
    let manifest = br#"{"schemaVersion":1,"id":"deepmod","version":"1.0.0","name":"Deep Mod","entrypoints":{"main":["com.deepmod.Main"]}}"#;
    let a = ClassAssembler::new("com/deepmod/Main");
    let bytes = a.finish(&[MethodSpec::new(
        "spawn",
        "()V",
        1,
        1,
        vec![0xBB, 0x00, 0x01, 0xB1], // new #1 ; return (decode fails at 0xBB)
    )]);
    jar(&[
        ("fabric.mod.json", manifest.as_slice()),
        ("com/deepmod/Main.class", bytes.as_slice()),
    ])
}

const MIXIN_DESC: &str = "Lorg/spongepowered/asm/mixin/Mixin;";
const REDIRECT_DESC: &str = "Lorg/spongepowered/asm/mixin/injection/Redirect;";
const AT_DESC: &str = "Lorg/spongepowered/asm/mixin/injection/At;";

/// Fabric mod declaring `evil.mixins.json` plus a real `@Mixin` +
/// `@Redirect(method="hurt", at=@At("INVOKE"))` class — an unliftable
/// mixin site, i.e. restart-required bytecode.
fn unliftable_mixin_jar() -> Vec<u8> {
    let manifest = br#"{"schemaVersion":1,"id":"mixinmod","version":"1.0.0","name":"Mixin Mod","mixins":["evil.mixins.json"],"depends":{"minecraft":"~1.21"}}"#;
    let mixin_config = br#"{"required":true,"package":"com.evilmod.mixin","compatibilityLevel":"JAVA_17","minVersion":"0.8","mixins":["EvilMixin"],"injectors":{"defaultRequire":1}}"#;
    let mixin_class = build_class(
        "com/evilmod/mixin/EvilMixin",
        vec![ann(
            MIXIN_DESC,
            vec![(
                "value",
                EV::Arr(vec![cls("net/minecraft/world/entity/LivingEntity")]),
            )],
            true,
        )],
        vec![meth(
            "evil$redirect",
            vec![ann(
                REDIRECT_DESC,
                vec![
                    ("method", s("hurt")),
                    (
                        "at",
                        EV::Ann(Box::new(ann(
                            AT_DESC,
                            vec![
                                ("value", s("INVOKE")),
                                ("target", s("Lnet/minecraft/world/entity/Entity;->hurt()V")),
                            ],
                            true,
                        ))),
                    ),
                ],
                true,
            )],
        )],
    );
    jar(&[
        ("fabric.mod.json", manifest.as_slice()),
        ("evil.mixins.json", mixin_config.as_slice()),
        ("com/evilmod/mixin/EvilMixin.class", mixin_class.as_slice()),
    ])
}

/// Fabric mod shipping a JNI native library — cannot hot-load.
fn native_lib_jar() -> Vec<u8> {
    let manifest = br#"{"schemaVersion":1,"id":"nativebad","version":"1.0.0","name":"Native Bad"}"#;
    jar(&[
        ("fabric.mod.json", manifest.as_slice()),
        (
            "natives/linux/libnativebad.so",
            &[0x7F, b'E', b'L', b'F', 2, 1, 1, 0][..],
        ),
    ])
}

/// Archive whose `fabric.mod.json` is not JSON — the fail-closed
/// manifest scan must force a restart verdict.
fn corrupt_manifest_jar() -> Vec<u8> {
    jar(&[("fabric.mod.json", b"{not json".as_slice())])
}

/// Pure datapack/culinary archive: `pack.mcmeta` + recipes + a
/// culinary-style cooking recipe + tags + loot tables.
fn datapack_jar() -> Vec<u8> {
    jar(&[
        (
            "pack.mcmeta",
            br#"{"pack":{"pack_format":48,"description":"dep test pack"}}"#.as_slice(),
        ),
        (
            "data/megu_test/recipes/test_stew.json",
            br#"{"type":"minecraft:crafting_shapeless","category":"misc","ingredients":[{"item":"minecraft:carrot"}],"result":{"id":"minecraft:stick","count":4}}"#.as_slice(),
        ),
        (
            "data/farmersdelight/cooking/beetroot_soup.json",
            br#"{"type":"farmersdelight:cooking","ingredient":["minecraft:beetroot"],"result":"minecraft:beetroot_soup","experience":0.35,"cookingtime":200}"#.as_slice(),
        ),
        (
            "data/megu_test/tags/blocks/rocks.json",
            br#"{"replace":false,"values":["minecraft:stone","minecraft:cobblestone"]}"#.as_slice(),
        ),
        (
            "data/megu_test/loot_tables/chests/test.json",
            br#"{"type":"minecraft:chest","pools":[]}"#.as_slice(),
        ),
    ])
}

/// A `megu-mod-bundle/1` document at the archive root.
fn bundle_jar() -> Vec<u8> {
    jar(&[(
        "bundle.json",
        br#"{"schema":"megu-mod-bundle/1","mod_id":"testbundle","mc_version_range":"*","portals":[],"progression_barriers":[],"biome_regions":[],"creatures":[],"custom_channels":[]}"#.as_slice(),
    )])
}

// ---------------------------------------------------------------------------
// Modrinth response fixtures (real `/v2/project/{id}/version` shape)
// ---------------------------------------------------------------------------

fn rep_hex(ch: char, n: usize) -> String {
    std::iter::repeat_n(ch, n).collect()
}

fn cdn_url(project: &str, version_id: &str, filename: &str) -> String {
    format!("https://cdn.modrinth.com/data/{project}/{version_id}/{filename}")
}

fn hashes(pairs: &[(&str, &str)]) -> String {
    let inner: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!(r#""{k}":"{v}""#))
        .collect();
    format!("{{{}}}", inner.join(","))
}

fn mr_file(
    filename: &str,
    url: &str,
    size: u64,
    hashes_json: &str,
    primary: bool,
    file_type: &str,
) -> String {
    format!(
        r#"{{"hashes":{hashes_json},"url":"{url}","filename":"{filename}","primary":{primary},"size":{size},"file_type":"{file_type}"}}"#
    )
}

/// Scalar fields of one Modrinth version entry; `files` is a pre-rendered
/// `files[]` JSON fragment.
struct MrVersionSpec<'a> {
    project_id: &'a str,
    id: &'a str,
    version_number: &'a str,
    version_type: &'a str,
    status: &'a str,
    game_versions: &'a str,
    loaders: &'a str,
    files: &'a str,
}

fn mr_version(spec: MrVersionSpec<'_>) -> String {
    format!(
        r#"{{"id":"{}","project_id":"{}","author_id":"auth","name":"{}","version_number":"{}","changelog":"","date_published":"2024-01-01T00:00:00.000Z","downloads":0,"version_type":"{}","status":"{}","requested_status":"approved","game_versions":{},"loaders":{},"files":[{}],"dependencies":[]}}"#,
        spec.id,
        spec.project_id,
        spec.id,
        spec.version_number,
        spec.version_type,
        spec.status,
        spec.game_versions,
        spec.loaders,
        spec.files,
    )
}

/// One-version response whose single `files[]` entry is `file_json` —
/// used to probe each fail-closed rejection independently.
fn single_candidate_json(file_json: &str) -> String {
    let v = mr_version(MrVersionSpec {
        project_id: "proj-x",
        id: "v-only",
        version_number: "1.0.0",
        version_type: "release",
        status: "listed",
        game_versions: r#"["1.21.1"]"#,
        loaders: r#"["fabric"]"#,
        files: file_json,
    });
    format!("[{v}]")
}

/// A realistic multi-version Modrinth response for `proj-mylibrary`:
/// ineligible entries (wrong MC, draft status, sha1-only digest, evil
/// URL) mixed with eligible releases/betas/alphas so the deterministic
/// ranking is observable.
fn versions_response() -> String {
    let h512a = hashes(&[("sha512", &rep_hex('a', 128))]);
    let h512b = hashes(&[("sha512", &rep_hex('b', 128))]);
    let h256512 = hashes(&[("sha256", &rep_hex('c', 64)), ("sha512", &rep_hex('d', 128))]);
    let h1only = hashes(&[("sha1", &rep_hex('e', 40))]);

    let ver = |id: &str,
               vn: &str,
               vt: &str,
               status: &str,
               gv: &str,
               loaders: &str,
               fname: &str,
               size: u64,
               h: &str,
               url_override: Option<&str>| {
        let url = url_override
            .map(str::to_string)
            .unwrap_or_else(|| cdn_url("proj-mylibrary", id, fname));
        let file = mr_file(fname, &url, size, h, true, "required_mod");
        mr_version(MrVersionSpec {
            project_id: "proj-mylibrary",
            id,
            version_number: vn,
            version_type: vt,
            status,
            game_versions: gv,
            loaders,
            files: file.as_str(),
        })
    };

    let versions = [
        // Highest version number overall but wrong game version.
        ver(
            "v-old-mc",
            "9.9.9",
            "release",
            "listed",
            r#"["1.20.1"]"#,
            r#"["fabric"]"#,
            "mylibrary-9.9.9.jar",
            900,
            &h512a,
            None,
        ),
        // Eligible beta — loses to any release regardless of version.
        ver(
            "v-beta",
            "2.0.0-beta",
            "beta",
            "listed",
            r#"["1.21.1","1.21"]"#,
            r#"["fabric","neoforge"]"#,
            "mylibrary-2.0.0-beta.jar",
            800,
            &h512b,
            None,
        ),
        // Eligible low release.
        ver(
            "v-rel-low",
            "1.0.0",
            "release",
            "listed",
            r#"["1.21.1"]"#,
            r#"["fabric"]"#,
            "mylibrary-1.0.0.jar",
            500,
            &h512a,
            None,
        ),
        // Highest *eligible* version number but an untrusted URL —
        // the whole file is ineligible, never partially trusted.
        ver(
            "v-evil",
            "8.8.8",
            "release",
            "listed",
            r#"["1.21.1"]"#,
            r#"["fabric","neoforge"]"#,
            "mylibrary-8.8.8.jar",
            700,
            &h512a,
            Some("https://evil.example.com/mylibrary-8.8.8.jar"),
        ),
        // Eligible alpha on a loader we prefer.
        ver(
            "v-alpha",
            "3.0.0",
            "alpha",
            "listed",
            r#"["1.21.1"]"#,
            r#"["neoforge"]"#,
            "mylibrary-3.0.0.jar",
            600,
            &h512b,
            None,
        ),
        // Not `listed` — drafts/disabled versions are never fetched.
        ver(
            "v-draft",
            "7.7.7",
            "release",
            "draft",
            r#"["1.21.1"]"#,
            r#"["fabric"]"#,
            "mylibrary-7.7.7.jar",
            650,
            &h512a,
            None,
        ),
        // sha1 alone is never sufficient integrity metadata.
        ver(
            "v-nodigest",
            "6.6.6",
            "release",
            "listed",
            r#"["1.21.1"]"#,
            r#"["fabric"]"#,
            "mylibrary-6.6.6.jar",
            640,
            &h1only,
            None,
        ),
        // WINNER: release 1.2.0, dual-loader, sha256+sha512.
        ver(
            "v-rel-high",
            "1.2.0",
            "release",
            "listed",
            r#"["1.21.1","1.21"]"#,
            r#"["fabric","neoforge"]"#,
            "mylibrary-1.2.0.jar",
            1024,
            &h256512,
            None,
        ),
        // Same version number/type as the winner — loses the
        // deterministic lowest-version_id tie-break.
        ver(
            "v-rel-tie",
            "1.2.0",
            "release",
            "listed",
            r#"["1.21.1"]"#,
            r#"["fabric"]"#,
            "mylibrary-1.2.0-fabric.jar",
            1000,
            &h512a,
            None,
        ),
    ];
    format!("[{}]", versions.join(","))
}

// ---------------------------------------------------------------------------
// MissingDepQuery — exact Modrinth v2 URL
// ---------------------------------------------------------------------------

#[test]
fn missing_dependency_from_dual_plan_builds_exact_query_url() {
    let dep_jar = dep_mod_jar();
    let plan = plan_dual_loader_mods_dir([("fabricharm-1.0.0.jar", dep_jar.as_slice())]);
    assert!(plan.rejected_jars.is_empty(), "{:?}", plan.rejected_jars);
    // `minecraft` is runtime-provided; `jei` + `mylibrary` are missing.
    assert_eq!(plan.missing_dependencies.len(), 2, "{:?}", plan.missing_dependencies);

    let dep = plan
        .missing_dependencies
        .iter()
        .find(|d| d.required_mod_id == "mylibrary")
        .expect("mylibrary must be reported missing");
    assert_eq!(dep.mod_id, "fabricharm");
    assert_eq!(dep.version_range.as_deref(), Some(">=1.0"));

    let query = MissingDepQuery::from_missing_dependency(
        dep,
        "1.21.1",
        &[ModLoaderKind::Fabric, ModLoaderKind::NeoForge],
    )
    .expect("query builds");
    assert_eq!(query.requiring_mod_id, "fabricharm");
    assert_eq!(query.required_mod_id, "mylibrary");
    assert_eq!(query.declared_range.as_deref(), Some(">=1.0"));
    assert_eq!(query.target_mc_tag, "1.21.1");
    assert_eq!(
        query.query_url(),
        "https://api.modrinth.com/v2/project/mylibrary/version?game_versions=%5B%221.21.1%22%5D&loaders=%5B%22fabric%22%2C%22neoforge%22%5D"
    );
}

#[test]
fn invalid_dependency_ids_ranges_and_targets_fail_closed() {
    let loaders = &[ModLoaderKind::Fabric];
    let dep = |id: &str, range: Option<&str>| MissingDependency {
        mod_id: "host".to_string(),
        required_mod_id: id.to_string(),
        version_range: range.map(str::to_string),
    };

    let long_id = "a".repeat(65);
    for bad in ["", "A", "has space", "dot.id", "a/b", "under_score?", long_id.as_str()] {
        assert!(
            matches!(
                MissingDepQuery::from_missing_dependency(&dep(bad, None), "1.21.1", loaders),
                Err(DepResolveError::InvalidModId(_))
            ),
            "mod id {bad:?} must be rejected"
        );
    }
    // `-`, `_`, digits are legal.
    for good in ["a", "ok-mod_1", "fabric-api", "0mod"] {
        assert!(
            MissingDepQuery::from_missing_dependency(&dep(good, None), "1.21.1", loaders).is_ok(),
            "mod id {good:?} must be accepted"
        );
    }

    let good = dep("ok-mod_1", None);
    assert!(matches!(
        MissingDepQuery::from_missing_dependency(&good, "not a version", loaders),
        Err(DepResolveError::InvalidTargetVersion(_))
    ));
    assert!(matches!(
        MissingDepQuery::from_missing_dependency(&good, "", loaders),
        Err(DepResolveError::InvalidTargetVersion(_))
    ));

    // Unterminated Maven interval — parse fails closed.
    let bad_range = dep("ok-mod_1", Some("[1.0,2.0"));
    assert!(matches!(
        MissingDepQuery::from_missing_dependency(&bad_range, "1.21.1", loaders),
        Err(DepResolveError::InvalidRange { .. })
    ));
}

// ---------------------------------------------------------------------------
// Modrinth response parsing & selection
// ---------------------------------------------------------------------------

#[test]
fn modrinth_versions_json_selects_exact_compatible_release() {
    let range = VersionRange::parse(">=1.0").unwrap();
    let json = versions_response();
    let artifact = parse_modrinth_versions_json(
        json.as_bytes(),
        "1.21.1",
        &[ModLoaderKind::Fabric, ModLoaderKind::NeoForge],
        &range,
    )
    .expect("response parses")
    .expect("a compatible release exists");

    // Release beats beta/alpha; 1.2.0 beats 1.0.0; lowest version_id wins
    // the same-version tie against v-rel-tie; evil/draft/sha1-only/wrong-MC
    // entries are all ineligible.
    assert_eq!(artifact.version_id, "v-rel-high");
    assert_eq!(artifact.version_number, "1.2.0");
    assert_eq!(artifact.version_type, "release");
    assert_eq!(artifact.project_id.as_deref(), Some("proj-mylibrary"));
    assert_eq!(artifact.file_name, "mylibrary-1.2.0.jar");
    assert!(artifact.primary);
    assert_eq!(artifact.file_type, "required_mod");
    assert_eq!(
        artifact.download_url,
        cdn_url("proj-mylibrary", "v-rel-high", "mylibrary-1.2.0.jar")
    );
    assert_eq!(artifact.size_bytes, 1024);
    assert_eq!(artifact.sha256_hex.as_deref(), Some(rep_hex('c', 64).as_str()));
    assert_eq!(artifact.sha512_hex.as_deref(), Some(rep_hex('d', 128).as_str()));
    assert!(artifact.sha1_hex.is_none());
}

#[test]
fn modrinth_target_tag_is_matched_verbatim() {
    let range = VersionRange::parse(">=1.0").unwrap();
    let json = versions_response();
    // "1.20.1" selects only the wrong-MC entry — proving the game_versions
    // tag is matched verbatim, never normalized.
    let artifact = parse_modrinth_versions_json(
        json.as_bytes(),
        "1.20.1",
        &[ModLoaderKind::Fabric],
        &range,
    )
    .unwrap()
    .expect("1.20.1 has exactly one eligible version");
    assert_eq!(artifact.version_id, "v-old-mc");
}

#[test]
fn modrinth_required_range_excludes_everything() {
    let json = versions_response();
    let range = VersionRange::parse("<1.0").unwrap();
    let got = parse_modrinth_versions_json(
        json.as_bytes(),
        "1.21.1",
        &[ModLoaderKind::Fabric, ModLoaderKind::NeoForge],
        &range,
    )
    .unwrap();
    assert!(got.is_none(), "no version below 1.0 exists");
}

#[test]
fn modrinth_untrusted_or_ineligible_files_fail_closed() {
    let range = VersionRange::parse("*").unwrap();
    let loaders = &[ModLoaderKind::Fabric];
    let good_hashes = hashes(&[("sha512", &rep_hex('a', 128))]);

    let cases = [
        // Untrusted host — not on the CDN allowlist.
        mr_file(
            "x-1.0.0.jar",
            "https://evil.example.com/x-1.0.0.jar",
            100,
            &good_hashes,
            true,
            "required_mod",
        ),
        // Plain HTTP — HTTPS only.
        mr_file(
            "x-1.0.0.jar",
            "http://cdn.modrinth.com/data/x/v/x-1.0.0.jar",
            100,
            &good_hashes,
            true,
            "required_mod",
        ),
        // Non-443 port.
        mr_file(
            "x-1.0.0.jar",
            "https://cdn.modrinth.com:8443/data/x/v/x-1.0.0.jar",
            100,
            &good_hashes,
            true,
            "required_mod",
        ),
        // IP literal host — SSRF guard.
        mr_file(
            "x-1.0.0.jar",
            "https://127.0.0.1/data/x/v/x-1.0.0.jar",
            100,
            &good_hashes,
            true,
            "required_mod",
        ),
        // Credentials-in-URL host spoofing.
        mr_file(
            "x-1.0.0.jar",
            "https://cdn.modrinth.com@evil.example.com/x-1.0.0.jar",
            100,
            &good_hashes,
            true,
            "required_mod",
        ),
        // Not a `.jar` basename.
        mr_file(
            "x-1.0.0.jar.exe",
            "https://cdn.modrinth.com/data/x/v/x-1.0.0.jar.exe",
            100,
            &good_hashes,
            true,
            "required_mod",
        ),
        // Path traversal in the file name.
        mr_file(
            "../x-1.0.0.jar",
            "https://cdn.modrinth.com/data/x/v/../x-1.0.0.jar",
            100,
            &good_hashes,
            true,
            "required_mod",
        ),
        // Oversized declared JAR.
        mr_file(
            "x-1.0.0.jar",
            "https://cdn.modrinth.com/data/x/v/x-1.0.0.jar",
            MAX_MOD_JAR_BYTES + 1,
            &good_hashes,
            true,
            "required_mod",
        ),
        // sha1-only integrity — never sufficient.
        mr_file(
            "x-1.0.0.jar",
            "https://cdn.modrinth.com/data/x/v/x-1.0.0.jar",
            100,
            &hashes(&[("sha1", &rep_hex('e', 40))]),
            true,
            "required_mod",
        ),
        // Malformed sha512 (wrong length) — no usable digest.
        mr_file(
            "x-1.0.0.jar",
            "https://cdn.modrinth.com/data/x/v/x-1.0.0.jar",
            100,
            &hashes(&[("sha512", "nothex")]),
            true,
            "required_mod",
        ),
    ];

    for file in cases {
        let json = single_candidate_json(&file);
        let got = parse_modrinth_versions_json(json.as_bytes(), "1.21.1", loaders, &range)
            .expect("response parses");
        assert!(got.is_none(), "candidate must be ineligible: {file}");
    }
}

#[test]
fn modrinth_malformed_responses_reject() {
    let range = VersionRange::parse("*").unwrap();
    let loaders = &[ModLoaderKind::Fabric];

    assert!(matches!(
        parse_modrinth_versions_json(b"not json", "1.21.1", loaders, &range),
        Err(DepResolveError::Json(_))
    ));
    assert!(matches!(
        parse_modrinth_versions_json(b"{}", "1.21.1", loaders, &range),
        Err(DepResolveError::Json(_))
    ));
    assert!(matches!(
        parse_modrinth_versions_json(b"[]", "bogus!", loaders, &range),
        Err(DepResolveError::InvalidTargetVersion(_))
    ));
    // Empty version list → no candidate, not an error.
    assert!(
        parse_modrinth_versions_json(b"[]", "1.21.1", loaders, &range)
            .unwrap()
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// verify_downloaded_bytes — sha256 / sha512 / size gates
// ---------------------------------------------------------------------------

fn bare_artifact(
    sha256_hex: Option<String>,
    sha512_hex: Option<String>,
    size_bytes: u64,
) -> ResolvedModrinthArtifact {
    ResolvedModrinthArtifact {
        project_slug: "proj-x".to_string(),
        project_id: None,
        version_id: "v-only".to_string(),
        version_number: "1.0.0".to_string(),
        version_type: "release".to_string(),
        file_name: "x-1.0.0.jar".to_string(),
        primary: true,
        file_type: "required_mod".to_string(),
        download_url: "https://cdn.modrinth.com/data/x/v-only/x-1.0.0.jar".to_string(),
        size_bytes,
        sha256_hex,
        sha512_hex,
        sha1_hex: None,
    }
}

#[test]
fn verify_downloaded_bytes_sha256_path() {
    let payload = b"dep-jar-payload";
    let artifact = bare_artifact(
        Some(mod_sync::sha256_hex(payload)),
        Some(rep_hex('d', 128)),
        payload.len() as u64,
    );
    artifact.verify_downloaded_bytes(payload).unwrap();

    // Same length, one flipped byte → digest mismatch.
    let mut tampered = payload.to_vec();
    tampered[0] ^= 0xFF;
    assert!(matches!(
        artifact.verify_downloaded_bytes(&tampered),
        Err(DepResolveError::DigestMismatch("sha256"))
    ));
    // Wrong length → size mismatch before any hashing.
    assert!(matches!(
        artifact.verify_downloaded_bytes(&payload[..payload.len() - 1]),
        Err(DepResolveError::SizeMismatch { .. })
    ));
}

#[test]
fn verify_downloaded_bytes_sha512_only_path() {
    // sha512("dep-jar-payload") — Modrinth's default published digest.
    const SHA512_PAYLOAD: &str = "5d54bbd79de1bf0537d33fc762c2bf3adee19e65f69432fb61792e520f6302746665549c42d053bde81bf90be405adbf962684fe00a1119a6d2da25bd405080c";
    let payload = b"dep-jar-payload";
    let artifact = bare_artifact(None, Some(SHA512_PAYLOAD.to_string()), payload.len() as u64);
    artifact.verify_downloaded_bytes(payload).unwrap();

    let mut tampered = payload.to_vec();
    tampered[0] ^= 0xFF;
    assert!(matches!(
        artifact.verify_downloaded_bytes(&tampered),
        Err(DepResolveError::DigestMismatch("sha512"))
    ));
}

#[test]
fn verify_downloaded_bytes_requires_a_usable_digest() {
    let payload = b"dep-jar-payload";
    // Neither sha256 nor sha512 → fail closed.
    let artifact = bare_artifact(None, None, payload.len() as u64);
    assert!(matches!(
        artifact.verify_downloaded_bytes(payload),
        Err(DepResolveError::DigestUnavailable)
    ));
    // Malformed stored digest → fail closed.
    let artifact = bare_artifact(Some("nothex".to_string()), None, payload.len() as u64);
    assert!(matches!(
        artifact.verify_downloaded_bytes(payload),
        Err(DepResolveError::DigestUnavailable)
    ));
}

// ---------------------------------------------------------------------------
// assess_hot_reload_after_add — restart-free classifier
// ---------------------------------------------------------------------------

#[test]
fn datapack_and_culinary_jar_is_hot_reloadable() {
    let verdict = assess_hot_reload_after_add(&datapack_jar());
    let ReloadRequirement::HotReloadable { reasons } = &verdict else {
        panic!("datapack JAR must be hot-reloadable, got {verdict:?}");
    };
    assert!(verdict.is_hot_reloadable());
    assert!(
        reasons.iter().any(|r| r.contains("datapack")),
        "expected datapack evidence in {reasons:?}"
    );
}

#[test]
fn unified_bundle_jar_is_hot_reloadable() {
    let verdict = assess_hot_reload_after_add(&bundle_jar());
    let ReloadRequirement::HotReloadable { reasons } = &verdict else {
        panic!("bundle JAR must be hot-reloadable, got {verdict:?}");
    };
    assert!(
        reasons.iter().any(|r| r.contains("unified-bundle")),
        "expected bundle evidence in {reasons:?}"
    );
}

#[test]
fn subset_safe_mod_jar_is_hot_reloadable() {
    let verdict = assess_hot_reload_after_add(&subset_safe_mod_jar());
    let ReloadRequirement::HotReloadable { reasons } = &verdict else {
        panic!("subset-safe mod JAR must be hot-reloadable, got {verdict:?}");
    };
    assert!(
        reasons.iter().any(|r| r.contains(".class member(s) verified")),
        "expected subset-VM evidence in {reasons:?}"
    );
}

#[test]
fn deep_tier3_bytecode_requires_restart() {
    let verdict = assess_hot_reload_after_add(&deep_tier3_mod_jar());
    let ReloadRequirement::RestartRequired { reasons } = &verdict else {
        panic!("deep bytecode must require restart, got {verdict:?}");
    };
    assert!(
        reasons.iter().any(|r| r.contains("subset")),
        "expected subset-VM blocker in {reasons:?}"
    );
    assert!(!verdict.is_hot_reloadable());
}

#[test]
fn unliftable_mixin_jar_requires_restart() {
    let verdict = assess_hot_reload_after_add(&unliftable_mixin_jar());
    let ReloadRequirement::RestartRequired { reasons } = &verdict else {
        panic!("mixin JAR must require restart, got {verdict:?}");
    };
    assert!(
        reasons.iter().any(|r| r.contains("Mixin") && r.contains("unliftable")),
        "expected unliftable-mixin blocker in {reasons:?}"
    );
}

#[test]
fn native_library_jar_requires_restart() {
    let verdict = assess_hot_reload_after_add(&native_lib_jar());
    let ReloadRequirement::RestartRequired { reasons } = &verdict else {
        panic!("native lib JAR must require restart, got {verdict:?}");
    };
    assert!(
        reasons.iter().any(|r| r.contains("native")),
        "expected native-library blocker in {reasons:?}"
    );
}

#[test]
fn corrupt_manifest_jar_requires_restart() {
    let verdict = assess_hot_reload_after_add(&corrupt_manifest_jar());
    let ReloadRequirement::RestartRequired { reasons } = &verdict else {
        panic!("corrupt manifest must require restart, got {verdict:?}");
    };
    assert!(
        reasons.iter().any(|r| r.contains("manifest")),
        "expected manifest blocker in {reasons:?}"
    );
}

#[test]
fn non_zip_and_empty_archives_classify() {
    // Not a ZIP at all → cannot prove safety → restart.
    let verdict = assess_hot_reload_after_add(b"definitely not a zip archive");
    assert!(matches!(verdict, ReloadRequirement::RestartRequired { .. }));

    // A valid but empty ZIP has no executable/data surface → trivially
    // hot-reloadable (it is inert in mods/).
    let empty = jar(&[]);
    let verdict = assess_hot_reload_after_add(&empty);
    let ReloadRequirement::HotReloadable { reasons } = &verdict else {
        panic!("empty archive must be hot-reloadable, got {verdict:?}");
    };
    assert!(reasons.iter().any(|r| r.contains("no executable")));
}

// ---------------------------------------------------------------------------
// plan_dep_recovery — EN/JA operator summary
// ---------------------------------------------------------------------------

#[test]
fn plan_dep_recovery_marks_auto_fetch_manual_and_on_disk() {
    let mods_dir = fixture_dir("plan-mods");
    fs::create_dir_all(&mods_dir).unwrap();
    let dep_jar = dep_mod_jar();
    let plan = plan_dual_loader_mods_dir([("fabricharm-1.0.0.jar", dep_jar.as_slice())]);

    let dep = plan
        .missing_dependencies
        .iter()
        .find(|d| d.required_mod_id == "mylibrary")
        .unwrap();
    let query = MissingDepQuery::from_missing_dependency(
        dep,
        "1.21.1",
        &[ModLoaderKind::Fabric, ModLoaderKind::NeoForge],
    )
    .unwrap();
    let artifact = resolve_for_query(&query, versions_response().as_bytes())
        .unwrap()
        .expect("mylibrary resolves a candidate");
    assert_eq!(artifact.project_slug, "mylibrary");

    let recovery = plan_dep_recovery(&mods_dir, &plan, std::slice::from_ref(&artifact));
    assert_eq!(recovery.entries.len(), 2);

    let mylib = recovery
        .entries
        .iter()
        .find(|e| e.required_mod_id == "mylibrary")
        .unwrap();
    assert_eq!(mylib.status, DepRecoveryStatus::AutoFetchReady);
    assert_eq!(mylib.candidate.as_ref().unwrap().version_id, "v-rel-high");
    assert!(
        mylib
            .query_url
            .as_deref()
            .unwrap()
            .contains("/project/mylibrary/version")
    );
    assert!(mylib.instruction_en.contains("/mods reload"));
    assert!(mylib.instruction_ja.contains("/mods reload"));

    let jei = recovery
        .entries
        .iter()
        .find(|e| e.required_mod_id == "jei")
        .unwrap();
    assert_eq!(jei.status, DepRecoveryStatus::ManualOnly);
    assert!(jei.candidate.is_none());
    assert!(jei.instruction_en.contains("mods/"));
    assert!(jei.instruction_en.contains("/mods reload"));

    assert!(recovery.summary_en.contains("AUTO-FETCH"));
    assert!(recovery.summary_en.contains("MANUAL"));
    assert!(recovery.summary_en.contains("/mods reload"));
    assert!(recovery.summary_en.contains("mylibrary"));
    assert!(recovery.summary_ja.contains("自動取得"));
    assert!(recovery.summary_ja.contains("手動"));
    assert!(recovery.summary_ja.contains("/mods reload"));

    // Once the candidate file name sits in mods/, the plan reports it
    // already on disk — nothing left to fetch.
    fs::write(mods_dir.join(&artifact.file_name), &dep_jar).unwrap();
    let recovery2 = plan_dep_recovery(&mods_dir, &plan, &[artifact]);
    let mylib2 = recovery2
        .entries
        .iter()
        .find(|e| e.required_mod_id == "mylibrary")
        .unwrap();
    assert_eq!(mylib2.status, DepRecoveryStatus::AlreadyOnDisk);
    assert!(mylib2.already_on_disk);
}

#[test]
fn plan_dep_recovery_empty_plan_has_clean_summaries() {
    let mods_dir = fixture_dir("plan-empty");
    let plan = plan_dual_loader_mods_dir(std::iter::empty::<(&str, &[u8])>());
    let recovery = plan_dep_recovery(&mods_dir, &plan, &[]);
    assert!(recovery.entries.is_empty());
    assert!(recovery.summary_en.contains("No missing"));
    assert!(recovery.summary_ja.contains("不足"));
}

// ---------------------------------------------------------------------------
// install_resolved_artifact — consent-gated verified materialization
// ---------------------------------------------------------------------------

#[test]
fn install_resolved_artifact_is_consent_gated_and_content_addressed() {
    let dir = fixture_dir("install");
    let mods = dir.join("mods");
    let cache_dir = dir.join("cache");
    fs::create_dir_all(&mods).unwrap();
    let cache = ContentAddressedModCache::new(&cache_dir);

    // The "downloaded" payload is a real datapack JAR; the Modrinth
    // metadata carries its genuine sha256 + declared size.
    let jar_bytes = datapack_jar();
    let sha256 = mod_sync::sha256_hex(&jar_bytes);
    let file_json = mr_file(
        "mylibrary-1.2.0.jar",
        &cdn_url("proj-mylibrary", "v-rel-high", "mylibrary-1.2.0.jar"),
        jar_bytes.len() as u64,
        &hashes(&[("sha256", &sha256), ("sha512", &rep_hex('d', 128))]),
        true,
        "required_mod",
    );
    let json = format!(
        "[{}]",
        mr_version(MrVersionSpec {
            project_id: "proj-mylibrary",
            id: "v-rel-high",
            version_number: "1.2.0",
            version_type: "release",
            status: "listed",
            game_versions: r#"["1.21.1"]"#,
            loaders: r#"["fabric","neoforge"]"#,
            files: file_json.as_str(),
        })
    );
    let artifact = parse_modrinth_versions_json(
        json.as_bytes(),
        "1.21.1",
        &[ModLoaderKind::Fabric, ModLoaderKind::NeoForge],
        &VersionRange::parse("*").unwrap(),
    )
    .unwrap()
    .unwrap();
    artifact.verify_downloaded_bytes(&jar_bytes).unwrap();

    // Denied consent → hard error, zero filesystem writes.
    let err = install_resolved_artifact(&cache, &artifact, &jar_bytes, &mods, &ConsentToken::denied())
        .unwrap_err();
    assert!(matches!(err, DepResolveError::ConsentRequired), "{err:?}");
    assert!(!mods.join(&artifact.file_name).exists());
    assert!(
        fs::read_dir(&cache_dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true),
        "cache root must stay empty/absent on denied consent"
    );

    // Corrupt payload → digest gate fails before any write.
    let mut bad = jar_bytes.clone();
    bad[0] ^= 0xFF;
    let err = install_resolved_artifact(&cache, &artifact, &bad, &mods, &ConsentToken::approved())
        .unwrap_err();
    assert!(
        matches!(err, DepResolveError::DigestMismatch("sha256")),
        "{err:?}"
    );
    assert!(!mods.join(&artifact.file_name).exists());

    // Approved → verified bytes cached content-addressed, then
    // materialized into mods/ under the jail-checked file name.
    let installed =
        install_resolved_artifact(&cache, &artifact, &jar_bytes, &mods, &ConsentToken::approved())
            .unwrap();
    assert_eq!(installed, mods.join(&artifact.file_name));
    assert_eq!(fs::read(&installed).unwrap(), jar_bytes);

    // The cache holds exactly one blob named <sha256>.jar with the same
    // verified bytes — content addressing makes re-installs idempotent.
    let blob = cache_dir.join(format!("{}.jar", sha256));
    assert!(blob.exists(), "content-addressed blob missing at {}", blob.display());
    assert_eq!(fs::read(&blob).unwrap(), jar_bytes);

    let again =
        install_resolved_artifact(&cache, &artifact, &jar_bytes, &mods, &ConsentToken::approved())
            .unwrap();
    assert_eq!(again, installed);
}
