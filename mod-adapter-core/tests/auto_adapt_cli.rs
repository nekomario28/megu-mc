// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! End-to-end checks of the compiled `megu-mod-check` auto-adapter CLI.
//! All JAR fixtures are self-contained stored ZIPs with valid central directories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use mod_adapter_core::data_loader::{parse_json, JsonValue};
use mod_adapter_core::unified_bundle::DeclarativeModBundle;

const BIN: &str = env!("CARGO_BIN_EXE_megu-mod-check");
static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "megu-auto-adapt-{}-{}-{tag}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create scratch directory");
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, bytes).expect("write fixture");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str], expected_exit: i32) -> Output {
    let output = Command::new(BIN)
        .args(args)
        .output()
        .expect("spawn megu-mod-check");
    assert_eq!(
        output.status.code(),
        Some(expected_exit),
        "args: {args:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn arg(path: &Path) -> &str {
    path.to_str().expect("UTF-8 test path")
}

fn json(bytes: &[u8]) -> JsonValue {
    parse_json(bytes).expect("parse CLI JSON")
}

fn field<'a>(value: &'a JsonValue, key: &str) -> &'a JsonValue {
    value
        .as_object()
        .expect("JSON object")
        .get(key)
        .unwrap_or_else(|| panic!("missing field {key}"))
}

fn text_field<'a>(value: &'a JsonValue, key: &str) -> &'a str {
    field(value, key).as_str().expect("JSON string")
}

fn count_field(value: &JsonValue, key: &str) -> u64 {
    field(value, key).as_u64().expect("JSON integer")
}

fn bundle_file(path: &Path, expected_mod_id: &str) {
    let body = fs::read_to_string(path).expect("bundle was written");
    let bundle =
        DeclarativeModBundle::from_json(&body).expect("strict bundle parser accepts output");
    assert_eq!(bundle.mod_id, expected_mod_id);
    assert_eq!(
        text_field(&json(body.as_bytes()), "schema"),
        "megu-mod-bundle/1"
    );
}

use megu_core::hash::crc32_ieee as crc32;

fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for &(name, bytes) in entries {
        let size = bytes.len() as u32;
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len() as u16;
        let crc = crc32(bytes);
        let offset = out.len() as u32;

        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(10u16.to_le_bytes()); // minimum ZIP version
        out.extend([0u8; 8]); // flags, method (stored), modification time/date
        out.extend(crc.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(name_len.to_le_bytes());
        out.extend(0u16.to_le_bytes()); // extra field length
        out.extend(name_bytes);
        out.extend(bytes);

        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        central.extend(10u16.to_le_bytes());
        central.extend([0u8; 8]); // flags, method, modification time/date
        central.extend(crc.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(name_len.to_le_bytes());
        central.extend([0u8; 8]); // extra/comment/disk/internal attributes
        central.extend(0u32.to_le_bytes()); // external attributes
        central.extend(offset.to_le_bytes());
        central.extend(name_bytes);
    }
    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend(central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]); // disk numbers
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(central_size.to_le_bytes());
    out.extend(central_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes()); // comment length
    out
}

fn culinary_jar(id: &str) -> Vec<u8> {
    let manifest =
        format!(r#"{{"schemaVersion":1,"id":"{id}","version":"1.2.3","environment":"*"}}"#);
    let recipe_path = format!("data/{id}/recipes/stew.json");
    let tag_path = format!("data/{id}/tags/items/vegetables.json");
    jar(&[
        ("fabric.mod.json", manifest.as_bytes()),
        (
            &recipe_path,
            br#"{"type":"example:cooking","ingredients":["minecraft:potato"],"result":{"id":"minecraft:baked_potato","count":1},"cookingtime":200}"#,
        ),
        (&tag_path, br#"{"values":["minecraft:potato"]}"#),
    ])
}

fn superseded_jar() -> Vec<u8> {
    jar(&[(
        "fabric.mod.json",
        br#"{"schemaVersion":1,"id":"sodium","version":"0.6.13","environment":"*"}"#,
    )])
}

// A minimal valid JVM class carrying @Mixin and an unliftable @Redirect site.
enum CpItem {
    Utf8(String),
    Class(u16),
}

#[derive(Default)]
struct ConstantPool(Vec<CpItem>);

impl ConstantPool {
    fn utf8(&mut self, value: &str) -> u16 {
        self.0.push(CpItem::Utf8(value.to_owned()));
        self.0.len() as u16
    }

    fn class(&mut self, name: &str) -> u16 {
        let name_index = self.utf8(name);
        self.0.push(CpItem::Class(name_index));
        self.0.len() as u16
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in &self.0 {
            match entry {
                CpItem::Utf8(value) => {
                    bytes.push(1);
                    bytes.extend((value.len() as u16).to_be_bytes());
                    bytes.extend(value.as_bytes());
                }
                CpItem::Class(index) => {
                    bytes.push(7);
                    bytes.extend(index.to_be_bytes());
                }
            }
        }
        bytes
    }
}

enum Element {
    String(&'static str),
    Nested(&'static str, Vec<(&'static str, Element)>),
    Array(Vec<Element>),
}

fn annotation(
    bytes: &mut Vec<u8>,
    cp: &mut ConstantPool,
    descriptor: &str,
    fields: &[(&str, Element)],
) {
    bytes.extend(cp.utf8(descriptor).to_be_bytes());
    bytes.extend((fields.len() as u16).to_be_bytes());
    for (name, value) in fields {
        bytes.extend(cp.utf8(name).to_be_bytes());
        element(bytes, cp, value);
    }
}

fn element(bytes: &mut Vec<u8>, cp: &mut ConstantPool, value: &Element) {
    match value {
        Element::String(value) => {
            bytes.push(b's');
            bytes.extend(cp.utf8(value).to_be_bytes());
        }
        Element::Nested(descriptor, fields) => {
            bytes.push(b'@');
            annotation(bytes, cp, descriptor, fields);
        }
        Element::Array(values) => {
            bytes.push(b'[');
            bytes.extend((values.len() as u16).to_be_bytes());
            for value in values {
                element(bytes, cp, value);
            }
        }
    }
}

fn unliftable_class() -> Vec<u8> {
    let mut cp = ConstantPool::default();
    let this_class = cp.class("com/badmod/mixin/BrokenMixin");
    let super_class = cp.class("java/lang/Object");
    let annotation_name = cp.utf8("RuntimeInvisibleAnnotations");
    let descriptor = cp.utf8("()V");
    let method_name = cp.utf8("badmod$redirectHurt");

    let mut class_annotations = 1u16.to_be_bytes().to_vec();
    annotation(
        &mut class_annotations,
        &mut cp,
        "Lorg/spongepowered/asm/mixin/Mixin;",
        &[(
            "targets",
            Element::Array(vec![Element::String(
                "net.minecraft.world.entity.LivingEntity",
            )]),
        )],
    );
    let mut method_annotations = 1u16.to_be_bytes().to_vec();
    annotation(
        &mut method_annotations,
        &mut cp,
        "Lorg/spongepowered/asm/mixin/injection/Redirect;",
        &[
            ("method", Element::String("hurt")),
            (
                "at",
                Element::Nested(
                    "Lorg/spongepowered/asm/mixin/injection/At;",
                    vec![
                        ("value", Element::String("INVOKE")),
                        ("target", Element::String("net/minecraft/x/damage")),
                    ],
                ),
            ),
        ],
    );

    let mut bytes = Vec::new();
    bytes.extend(0xcafe_babeu32.to_be_bytes());
    bytes.extend(0u16.to_be_bytes());
    bytes.extend(61u16.to_be_bytes());
    bytes.extend(((cp.0.len() + 1) as u16).to_be_bytes());
    bytes.extend(cp.encode());
    bytes.extend(0x0021u16.to_be_bytes());
    bytes.extend(this_class.to_be_bytes());
    bytes.extend(super_class.to_be_bytes());
    bytes.extend(0u16.to_be_bytes()); // interfaces
    bytes.extend(0u16.to_be_bytes()); // fields
    bytes.extend(1u16.to_be_bytes()); // methods
    bytes.extend(0x0001u16.to_be_bytes());
    bytes.extend(method_name.to_be_bytes());
    bytes.extend(descriptor.to_be_bytes());
    bytes.extend(1u16.to_be_bytes()); // method attributes
    bytes.extend(annotation_name.to_be_bytes());
    bytes.extend((method_annotations.len() as u32).to_be_bytes());
    bytes.extend(method_annotations);
    bytes.extend(1u16.to_be_bytes()); // class attributes
    bytes.extend(annotation_name.to_be_bytes());
    bytes.extend((class_annotations.len() as u32).to_be_bytes());
    bytes.extend(class_annotations);
    bytes
}

fn unliftable_jar() -> Vec<u8> {
    let class = unliftable_class();
    jar(&[
        (
            "fabric.mod.json",
            br#"{"schemaVersion":1,"id":"badmod","version":"1.0.0","environment":"*","mixins":["bad.mixins.json"]}"#,
        ),
        (
            "bad.mixins.json",
            br#"{"package":"com.badmod.mixin","mixins":["BrokenMixin"]}"#,
        ),
        ("com/badmod/mixin/BrokenMixin.class", &class),
    ])
}

#[test]
fn scaffold_native_writes_a_valid_bundle_and_rejects_unsafe_ids() {
    let scratch = Scratch::new("scaffold");
    let output = scratch.path("my_sample_mod.bundle.json");
    run(
        &[
            "--scaffold-native",
            "my_sample_mod",
            "--version",
            "1.0.0",
            "--out",
            arg(&output),
        ],
        0,
    );
    bundle_file(&output, "my_sample_mod");
    assert_eq!(
        text_field(&json(&fs::read(&output).unwrap()), "mc_version_range"),
        "1.0.0"
    );
    let unsafe_result = run(&["--scaffold-native", "../escape"], 2);
    assert!(!unsafe_result.stderr.is_empty());
}

#[test]
fn adapt_jar_writes_valid_native_bundle() {
    let scratch = Scratch::new("jar");
    let input = scratch.write("culinary.jar", &culinary_jar("tastymod"));
    let output_dir = scratch.path("adapted");
    let result = run(
        &[
            "--adapt-jar",
            arg(&input),
            "--out",
            arg(&output_dir),
            "--json",
        ],
        0,
    );
    let report = json(&result.stdout);
    assert_eq!(text_field(&report, "schema"), "megu-jar-adaptation/1");
    assert_eq!(text_field(&report, "mod_id"), "tastymod");
    assert_eq!(text_field(&report, "tier"), "native_bundle");
    assert_eq!(
        text_field(field(&report, "bundle"), "schema"),
        "megu-mod-bundle/1"
    );
    assert_eq!(text_field(&report, "bundle_file"), "tastymod.bundle.json");
    bundle_file(&output_dir.join("tastymod.bundle.json"), "tastymod");
    assert_eq!(
        fs::read(output_dir.join("adaptation-report.json")).unwrap(),
        result.stdout
    );
}

#[test]
fn adapt_jar_skips_manifest_declared_superseded_mod_even_with_an_unrelated_filename() {
    let scratch = Scratch::new("superseded");
    let input = scratch.write("totally-not-sodium.jar", &superseded_jar());
    let result = run(&["--adapt-jar", arg(&input), "--json"], 0);
    let report = json(&result.stdout);
    assert_eq!(text_field(&report, "schema"), "megu-jar-adaptation/1");
    assert_eq!(text_field(&report, "mod_id"), "sodium");
    assert_eq!(text_field(&report, "tier"), "auto_skipped");
    assert!(matches!(field(&report, "bundle"), JsonValue::Null));
}

#[test]
fn adapt_mods_dir_writes_deterministic_report_and_native_bundle_despite_tier3() {
    let scratch = Scratch::new("mixed");
    let mods = scratch.path("mods");
    fs::create_dir(&mods).unwrap();
    fs::write(mods.join("01-renamed-perf.jar"), superseded_jar()).unwrap();
    fs::write(mods.join("02-tastymod.jar"), culinary_jar("tastymod")).unwrap();
    fs::write(mods.join("03-badmod.jar"), unliftable_jar()).unwrap();

    let out_dir = scratch.path("adapted");
    let args = [
        "--adapt-mods-dir",
        arg(&mods),
        "--out",
        arg(&out_dir),
        "--json",
    ];
    let result = run(&args, 2);
    let report = json(&result.stdout);
    assert_eq!(
        text_field(&report, "schema"),
        "megu-mods-dir-adaptation/1"
    );
    assert_eq!(text_field(&report, "verdict"), "requires_fallback");
    assert_eq!(count_field(&report, "jars_found"), 3);
    let totals = field(&report, "totals");
    assert_eq!(count_field(totals, "auto_skipped"), 1);
    assert_eq!(count_field(totals, "native_bundle"), 1);
    assert_eq!(count_field(totals, "tier2_lifted_shim"), 0);
    assert_eq!(count_field(totals, "tier3_fallback_required"), 1);
    let jars = field(&report, "jars").as_array().expect("JAR rows");
    assert_eq!(jars.len(), 3);
    assert_eq!(text_field(&jars[0], "tier"), "auto_skipped");
    assert_eq!(text_field(&jars[0], "mod_id"), "sodium");
    assert_eq!(text_field(&jars[1], "tier"), "native_bundle");
    assert_eq!(text_field(&jars[1], "mod_id"), "tastymod");
    assert_eq!(text_field(&jars[2], "tier"), "tier3_fallback_required");
    assert_eq!(text_field(&jars[2], "mod_id"), "badmod");
    assert!(!field(&jars[2], "blockers").as_array().unwrap().is_empty());
    assert_eq!(
        count_field(field(&jars[2], "data"), "mixin_config_files"),
        1
    );

    bundle_file(&out_dir.join("tastymod.bundle.json"), "tastymod");
    assert!(!out_dir.join("badmod.bundle.json").exists());
    assert!(!out_dir.join("sodium.bundle.json").exists());
    assert_eq!(
        fs::read(out_dir.join("adaptation-report.json")).unwrap(),
        result.stdout
    );
    assert_eq!(
        run(&args, 2).stdout,
        result.stdout,
        "JSON report must be deterministic"
    );
}

#[test]
fn adapt_mods_dir_all_native_succeeds() {
    let scratch = Scratch::new("all-native");
    let mods = scratch.path("mods");
    fs::create_dir(&mods).unwrap();
    fs::write(mods.join("a.jar"), culinary_jar("food_a")).unwrap();
    fs::write(mods.join("b.jar"), culinary_jar("food_b")).unwrap();
    let result = run(&["--adapt-mods-dir", arg(&mods), "--json"], 0);
    let report = json(&result.stdout);
    assert_eq!(
        text_field(&report, "schema"),
        "megu-mods-dir-adaptation/1"
    );
    assert_eq!(text_field(&report, "verdict"), "adapted");
    assert_eq!(count_field(field(&report, "totals"), "native_bundle"), 2);
    assert_eq!(
        count_field(field(&report, "totals"), "tier3_fallback_required"),
        0
    );
}

#[test]
fn adapt_jar_missing_file_exits_one() {
    let scratch = Scratch::new("missing");
    let result = run(
        &[
            "--adapt-jar",
            arg(&scratch.path("no-such-mod.jar")),
            "--json",
        ],
        1,
    );
    assert!(result.stdout.is_empty());
    assert!(!result.stderr.is_empty());
}
