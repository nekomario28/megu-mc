// SPDX-License-Identifier: LGPL-3.0-or-later

//! Integration tests for `loader_extensions`: quilt.mod.json, bounded
//! Jar-in-Jar, accesstransformer.cfg, the `fabric:registry/sync/direct`
//! codec, fluid tags, NeoForge data maps, biome modifiers, and the ten
//! `c:` conventional tag families. JAR fixtures are real stored-entry ZIP
//! archives assembled byte-by-byte — no external jars, no JVM, no crates.

use mod_adapter_core::data_loader::JsonValue;
use mod_adapter_core::ecosystem_compat::{scan_mod_jar, ModLoaderKind};
use mod_adapter_core::loader_extensions::{
    biome_modifier_id_for_path, canonical_conventional_tag_id,
    data_map_key_for_path, decode_fabric_registry_sync_direct,
    encode_fabric_registry_sync_direct, fluid_tag_id_for_path,
    inspect_jar_extensions, inspect_jar_in_jar, is_builtin_data_map,
    is_conventional_family, parse_access_transformer_cfg, parse_biome_modifier,
    parse_data_map, parse_jarjar_metadata, parse_quilt_mod_json,
    parse_tag_file_json, provided_loader_artifact, validate_jar_in_jar_path,
    AtAccess, AtFinalModifier, AtTargetKind, BiomeModifierKind,
    ConventionalTagRegistry, DataMapRegistry, FabricRegistrySyncMap,
    FluidTagRegistry, TagFileEntry, TagRef, ACCESS_TRANSFORMER_PATHS,
    CHANNEL_FABRIC_REGISTRY_SYNC_DIRECT, CONVENTIONAL_TAG_FAMILIES,
    JAR_IN_JAR_METADATA_PATH, MAX_FABRIC_REGISTRY_SYNC_REGISTRIES,
    MAX_JAR_IN_JAR_DEPTH, MAX_JAR_IN_JAR_NESTED_JARS,
    MAX_JAR_IN_JAR_TOTAL_BYTES, NEOFORGE_BUILTIN_DATA_MAPS, QUILT_MOD_JSON,
};
use mod_adapter_core::neoforge_wire::{
    CHANNEL_FABRIC_REGISTRY_SYNC_DIRECT as WIRE_CHANNEL, write_mc_string, write_varint,
};

// ---------------------------------------------------------------------------
// Stored-entry ZIP fixture builder (same byte-exact shape as the
// ecosystem_compat tests).
// ---------------------------------------------------------------------------

use megu_core::hash::crc32_ieee as crc32;

fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let name = name.as_bytes();
        let crc = crc32(data);
        let size = data.len() as u32;
        let offset = out.len() as u32;

        out.extend(0x0403_4B50u32.to_le_bytes());
        out.extend(10u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(crc.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name);
        out.extend(*data);

        central.extend(0x0201_4B50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        central.extend(10u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(crc.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend((name.len() as u16).to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_be_bytes());
        central.extend(0u32.to_le_bytes());
        central.extend(offset.to_le_bytes());
        central.extend(name);
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend(central);
    out.extend(0x0605_4B50u32.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(cd_size.to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

fn fabric_manifest(mod_id: &str) -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":1,"id":"{mod_id}","version":"1.0.0","environment":"*"}}"#
    )
    .into_bytes()
}

fn jarjar_metadata(path: &str) -> Vec<u8> {
    format!(
        r#"{{"jars":[{{"identifier":{{"group":"com.example","artifact":"nested-lib"}},
        "version":{{"artifactVersion":"2.4.1","range":"[2,3)"}},
        "path":"{path}"}}]}}"#
    )
    .into_bytes()
}

// ---------------------------------------------------------------------------
// 1. quilt.mod.json
// ---------------------------------------------------------------------------

#[test]
fn quilt_mod_json_parses_loader_id_version_depends_environment() {
    let manifest = br#"{
        "schema_version": 1,
        "quilt_loader": {
            "group": "com.example",
            "id": "quiltymod",
            "version": "3.2.1",
            "entrypoints": {"init": ["com.example.Main"], "client": [{"value": "com.example.Client"}]},
            "mixins": {"client": ["quilty.mixins.json"]},
            "depends": [
                {"id": "minecraft", "versions": ">=1.21.1"},
                {"id": "quilt_loader", "versions": "*"},
                {"id": "optionalpal", "versions": "^2", "optional": true},
                {"all": [{"id": "req_a"}, {"id": "req_b"}]},
                {"any": [{"id": "alt_a"}, {"id": "alt_b"}]},
                {"only_if": {"id": "conditional_c"}}
            ]
        },
        "minecraft": {"environment": "dedicated_server"}
    }"#;
    let meta = parse_quilt_mod_json(manifest).expect("quilt manifest parses");
    assert_eq!(meta.mod_id, "quiltymod");
    assert_eq!(meta.group, "com.example");
    assert_eq!(meta.version, "3.2.1");
    assert_eq!(
        meta.environment,
        mod_adapter_core::ecosystem_compat::ModEnvironment::Server
    );
    assert_eq!(meta.mc_range.as_deref(), Some(">=1.21.1"));
    assert_eq!(meta.mixin_configs, vec!["quilty.mixins.json"]);
    assert_eq!(
        meta.entrypoints,
        vec!["com.example.Client", "com.example.Main"]
    );
    let dep_ids: Vec<&str> = meta.dependencies.iter().map(|d| d.mod_id.as_str()).collect();
    assert_eq!(
        dep_ids,
        vec![
            "minecraft", "quilt_loader", "optionalpal", "req_a", "req_b",
            "alt_a", "alt_b", "conditional_c",
        ]
    );
    let optional = meta
        .dependencies
        .iter()
        .find(|d| d.mod_id == "optionalpal")
        .unwrap();
    assert!(!optional.required);
    let quilt_loader = meta
        .dependencies
        .iter()
        .find(|d| d.mod_id == "quilt_loader")
        .unwrap();
    assert!(quilt_loader.required);
    assert_eq!(
        provided_loader_artifact("quilt_loader"),
        Some("org.quiltmc:quilt-loader")
    );
    let alt_a = meta.dependencies.iter().find(|d| d.mod_id == "alt_a").unwrap();
    let alt_b = meta.dependencies.iter().find(|d| d.mod_id == "alt_b").unwrap();
    assert!(alt_a.required && alt_b.required);
    assert!(alt_a.alternative_group.is_some());
    assert_eq!(alt_a.alternative_group, alt_b.alternative_group);
    let conditional = meta
        .dependencies
        .iter()
        .find(|d| d.mod_id == "conditional_c")
        .unwrap();
    assert!(conditional.condition.as_deref().is_some_and(|c| c.starts_with("only_if:")));
    assert!(meta.dependencies.iter().all(|d| d.owner_mod_id == "quiltymod"));
}

#[test]
fn quilt_mod_json_accepts_object_depends_and_rejects_bad_schema() {
    let obj_depends = br#"{
        "schema_version": 1,
        "quilt_loader": {
            "id": "objdep",
            "version": {"value": "0.1"},
            "depends": {"minecraft": ">=1.21", "sibling": {"versions": "^1", "optional": true}}
        }
    }"#;
    let meta = parse_quilt_mod_json(obj_depends).expect("object depends parses");
    assert_eq!(meta.version, "0.1");
    assert_eq!(meta.dependencies.len(), 2);
    let sib = meta
        .dependencies
        .iter()
        .find(|d| d.mod_id == "sibling")
        .unwrap();
    assert!(!sib.required); // {"versions": ..., "optional": true}
    assert_eq!(sib.version_range.as_deref(), Some("^1"));

    assert!(parse_quilt_mod_json(br#"{"schema_version": 2, "quilt_loader": {"id": "x", "version": "1"}}"#).is_err());
    assert!(parse_quilt_mod_json(br#"{"schema_version": 1}"#).is_err());
    assert!(parse_quilt_mod_json(
        br#"{"schema_version": 1, "quilt_loader": {"id": "Bad ID", "version": "1"}}"#
    )
    .is_err());
    assert!(parse_quilt_mod_json(
        br#"{"schema_version": 1, "quilt_loader": {"id": "ok", "version": "1"}, "minecraft": {"environment": "weird"}}"#
    )
    .is_err());
    assert!(parse_quilt_mod_json(b"not json").is_err());
}

#[test]
fn scan_mod_jar_classifies_quilt_jar_and_collects_extensions() {
    let quilt_manifest = br#"{
        "schema_version": 1,
        "quilt_loader": {"group": "com.example", "id": "quiltest", "version": "9.9",
            "entrypoints": {"init": ["com.example.Init"]},
            "depends": [{"id": "minecraft", "versions": ">=1.21"}]},
        "minecraft": {"environment": "*"}
    }"#;
    let at_cfg = b"public com.example.Thing field\npublic-f com.example.Thing method()V\n";
    let nested = jar(&[("data/x/x.json", b"{}")]);
    let bytes = jar(&[
        (QUILT_MOD_JSON, quilt_manifest.as_slice()),
        ("META-INF/accesstransformer.cfg", at_cfg.as_slice()),
        (
            JAR_IN_JAR_METADATA_PATH,
            jarjar_metadata("META-INF/jarjar/nested-lib.jar").as_slice(),
        ),
        ("META-INF/jarjar/nested-lib.jar", nested.as_slice()),
        ("pack.mcmeta", br#"{"pack":{"pack_format":48}}"#),
    ]);

    let desc = scan_mod_jar(&bytes).expect("quilt JAR scans");
    assert_eq!(desc.loader, ModLoaderKind::Fabric);
    assert!(desc.loader_manifests.iter().any(|l| matches!(
        l,
        mod_adapter_core::ecosystem_compat::ManifestLoaderKind::Quilt
    )));
    assert_eq!(desc.mod_id, "quiltest");
    assert_eq!(desc.version, "9.9");
    assert_eq!(desc.entrypoints, vec!["com.example.Init"]);
    assert!(desc.extensions.quilt.is_some());
    assert_eq!(
        desc.extensions
            .quilt
            .as_ref()
            .unwrap()
            .group,
        "com.example"
    );
    assert_eq!(desc.extensions.access_transformers.len(), 2);
    assert_eq!(desc.extensions.jar_in_jar.declared.len(), 1);
    assert_eq!(
        desc.extensions.jar_in_jar.declared[0].identifier,
        "com.example:nested-lib"
    );
    assert!(!desc.extensions.requires_jvm_bridge()); // JiJ + visibility/final-removal ATs are declarative
}

// ---------------------------------------------------------------------------
// 2. Jar-in-Jar bounds
// ---------------------------------------------------------------------------

#[test]
fn jarjar_metadata_parses_and_validates_paths() {
    let jars = parse_jarjar_metadata(&jarjar_metadata("META-INF/jarjar/a-1.jar"))
        .expect("metadata parses");
    assert_eq!(jars.len(), 1);
    assert_eq!(jars[0].identifier, "com.example:nested-lib");
    assert_eq!(jars[0].version, "2.4.1");
    assert_eq!(jars[0].version_range.as_deref(), Some("[2,3)"));
    assert_eq!(jars[0].path, "META-INF/jarjar/a-1.jar");

    // String identifier form.
    let meta = br#"{"jars":[{"identifier":"g:a","version":"1.0","path":"META-INF/jarjar/a.jar"}]}"#;
    let parsed = parse_jarjar_metadata(meta).expect("string id form");
    assert_eq!(parsed[0].identifier, "g:a");

    // Traversal, absolute paths, backslashes, NUL, non-.jar — all fail closed.
    for bad in [
        "META-INF/jarjar/../evil.jar",
        "/abs.jar",
        "META-INF\\evil.jar",
        "META-INF/jarjar/evil.txt",
        "other/dir/x.jar",
        "META-INF/jarjar/./x.jar",
        "META-INF/jarjar//x.jar",
        "",
    ] {
        assert!(
            validate_jar_in_jar_path(bad).is_err(),
            "path {bad:?} must be rejected"
        );
    }
    assert!(validate_jar_in_jar_path("META-INF/jarjar/ok-1.0.jar").is_ok());
}

#[test]
fn jar_in_jar_recurses_nested_metadata_and_enforces_bounds() {
    // depth 0 -> 1 -> 2 -> 3 chain of embedded jars.
    let leaf = jar(&[("data/l/x.json", b"{}")]);
    let l3 = jar(&[
        (
            JAR_IN_JAR_METADATA_PATH,
            jarjar_metadata("META-INF/jarjar/leaf.jar").as_slice(),
        ),
        ("META-INF/jarjar/leaf.jar", leaf.as_slice()),
    ]);
    let l2 = jar(&[
        (
            JAR_IN_JAR_METADATA_PATH,
            jarjar_metadata("META-INF/jarjar/l3.jar").as_slice(),
        ),
        ("META-INF/jarjar/l3.jar", l3.as_slice()),
    ]);
    let l1 = jar(&[
        (
            JAR_IN_JAR_METADATA_PATH,
            jarjar_metadata("META-INF/jarjar/l2.jar").as_slice(),
        ),
        ("META-INF/jarjar/l2.jar", l2.as_slice()),
    ]);
    let outer = jar(&[
        ("fabric.mod.json", fabric_manifest("outer").as_slice()),
        (
            JAR_IN_JAR_METADATA_PATH,
            jarjar_metadata("META-INF/jarjar/l1.jar").as_slice(),
        ),
        ("META-INF/jarjar/l1.jar", l1.as_slice()),
    ]);

    let scan = inspect_jar_in_jar(&outer, "outer.jar").expect("bounded scan");
    assert_eq!(scan.declared.len(), 4);
    assert_eq!(scan.extracted_members, 4);
    assert!(scan.max_depth_seen <= MAX_JAR_IN_JAR_DEPTH);
    assert!(scan.total_member_bytes > 0);

    // Missing declared member fails closed.
    let missing = jar(&[(
        JAR_IN_JAR_METADATA_PATH,
        jarjar_metadata("META-INF/jarjar/ghost.jar").as_slice(),
    )]);
    let e = inspect_jar_in_jar(&missing, "missing.jar").unwrap_err();
    assert!(e.reason().contains("no such member"), "{}", e.reason());
}

#[test]
fn jar_in_jar_rejects_overdepth_and_count_bomb() {
    // Depth chain exceeding MAX_JAR_IN_JAR_DEPTH: a five-level chain puts
    // jarjar metadata at depth 4, past the limit.
    let mut chain = jar(&[("data/x/x.json", b"{}")]);
    for i in 0..(MAX_JAR_IN_JAR_DEPTH + 2) {
        let inner_name = format!("META-INF/jarjar/inner{i}.jar");
        chain = jar(&[
            (
                JAR_IN_JAR_METADATA_PATH,
                jarjar_metadata(&inner_name).as_slice(),
            ),
            (inner_name.as_str(), chain.as_slice()),
        ]);
    }
    assert!(inspect_jar_in_jar(&chain, "deep.jar").is_err());

    // Count bomb: MAX_JAR_IN_JAR_NESTED_JARS + 1 declared siblings.
    let payload = jar(&[("data/x/x.json", b"{}")]);
    let mut entries: Vec<(&str, &[u8])> = Vec::new();
    let mut names = Vec::new();
    let mut meta_jars = String::from(r#""jars":["#);
    for i in 0..=MAX_JAR_IN_JAR_NESTED_JARS {
        let path = format!("META-INF/jarjar/b{i}.jar");
        names.push(path.clone());
        meta_jars.push_str(&format!(
            r#"{{"identifier":"g:a{i}","version":"1","path":"{path}"}},"#
        ));
    }
    meta_jars.pop();
    meta_jars.push(']');
    let meta = format!("{{{meta_jars}}}").into_bytes();
    entries.push((JAR_IN_JAR_METADATA_PATH, meta.as_slice()));
    for n in &names {
        entries.push((n.as_str(), payload.as_slice()));
    }
    let bomb = jar(&entries);
    let e = inspect_jar_in_jar(&bomb, "bomb.jar").unwrap_err();
    assert!(
        e.reason().contains("nested JARs"),
        "count bound must fail closed: {}",
        e.reason()
    );
    const { assert!(MAX_JAR_IN_JAR_TOTAL_BYTES == 64 * 1024 * 1024) };
}

// ---------------------------------------------------------------------------
// 3. accesstransformer.cfg
// ---------------------------------------------------------------------------

#[test]
fn access_transformer_cfg_parses_all_directive_forms() {
    let cfg = b"# comment line
public com.example.Foo
protected-f com.example.Foo fieldName
public com.example.Foo doThing()V
private+f com.example.Foo   # trailing comment
default com.example.Bar baz(Ljava/lang/String;)I

public-f net.minecraft.world.item.Item rarity
";
    let entries = parse_access_transformer_cfg(cfg).expect("cfg parses");
    assert_eq!(entries.len(), 6);

    assert_eq!(entries[0].target_kind, AtTargetKind::Class);
    assert_eq!(entries[0].access, AtAccess::Public);
    assert_eq!(entries[0].final_modifier, AtFinalModifier::Keep);
    assert!(!entries[0].requires_jvm_bridge()); // class visibility widening is declarative

    assert_eq!(entries[1].target_kind, AtTargetKind::Field);
    assert_eq!(entries[1].access, AtAccess::Protected);
    assert_eq!(entries[1].final_modifier, AtFinalModifier::Remove);
    assert_eq!(entries[1].member_name.as_deref(), Some("fieldName"));
    assert!(!entries[1].requires_jvm_bridge()); // member-level via shim table

    assert_eq!(entries[2].target_kind, AtTargetKind::Method);
    assert_eq!(entries[2].descriptor.as_deref(), Some("()V"));
    assert!(!entries[2].requires_jvm_bridge());

    assert_eq!(entries[3].access, AtAccess::Private);
    assert_eq!(entries[3].final_modifier, AtFinalModifier::Add);
    assert!(entries[3].requires_jvm_bridge()); // +f is structural

    assert_eq!(entries[4].access, AtAccess::Default);
    assert_eq!(entries[4].target_kind, AtTargetKind::Method);
    assert_eq!(
        entries[4].descriptor.as_deref(),
        Some("(Ljava/lang/String;)I")
    );

    assert_eq!(entries[5].owner_class, "net.minecraft.world.item.Item");
    assert_eq!(entries[5].line_no, 8);
}

#[test]
fn access_transformer_cfg_fails_closed_on_malformed_lines() {
    for bad in [
        b"public" as &[u8],                                   // missing target
        b"public com.x.Foo field extra junk",                 // too many tokens
        b"bogusaccess com.x.Foo field",                       // unknown directive
        b"public com/x/Foo field",                            // slash class name
        b"public com.x.Foo method",                           // fine actually
        b"public com.x.Foo (",                                // bad method target
        b"public com.x.Foo m(",                               // unbalanced desc
    ] {
        let r = parse_access_transformer_cfg(bad);
        if bad == b"public com.x.Foo method" {
            assert!(r.is_ok()); // bare member name = field target
        } else {
            assert!(r.is_err(), "{bad:?} must fail closed");
        }
    }
    assert_eq!(ACCESS_TRANSFORMER_PATHS[0], "META-INF/accesstransformer.cfg");
}

// ---------------------------------------------------------------------------
// 4. fabric:registry/sync/direct
// ---------------------------------------------------------------------------

#[test]
fn fabric_registry_sync_direct_roundtrips_deterministically() {
    assert_eq!(WIRE_CHANNEL, CHANNEL_FABRIC_REGISTRY_SYNC_DIRECT);
    let mut map = FabricRegistrySyncMap::new();
    map.insert(
        "minecraft:block".to_string(),
        vec![
            ("minecraft:stone".to_string(), 1),
            ("minecraft:dirt".to_string(), 3),
            ("mymod:custom".to_string(), 4096),
        ],
    );
    map.insert(
        "minecraft:item".to_string(),
        vec![("minecraft:stick".to_string(), 280)],
    );
    let body = encode_fabric_registry_sync_direct(&map).expect("encode");
    let body2 = encode_fabric_registry_sync_direct(&map).expect("encode");
    assert_eq!(body, body2, "encoding is deterministic");
    let decoded = decode_fabric_registry_sync_direct(&body).expect("decode");
    assert_eq!(decoded, map);

    // Empty map roundtrips too.
    let empty = FabricRegistrySyncMap::new();
    let body = encode_fabric_registry_sync_direct(&empty).unwrap();
    assert_eq!(decode_fabric_registry_sync_direct(&body), Some(empty));
}

#[test]
fn fabric_registry_sync_direct_rejects_malformed_bodies() {
    let mut map = FabricRegistrySyncMap::new();
    map.insert(
        "minecraft:block".to_string(),
        vec![("minecraft:stone".to_string(), 1)],
    );
    let body = encode_fabric_registry_sync_direct(&map).unwrap();

    // Truncation and trailing garbage fail closed.
    assert!(decode_fabric_registry_sync_direct(&body[..body.len() - 1]).is_none());
    let mut extra = body.clone();
    extra.push(0);
    assert!(decode_fabric_registry_sync_direct(&extra).is_none());
    assert!(decode_fabric_registry_sync_direct(&[]).is_none());

    // Inflated registry count beyond what the body can hold.
    let mut inflated = Vec::new();
    write_varint(&mut inflated, MAX_FABRIC_REGISTRY_SYNC_REGISTRIES as u32 + 1);
    assert!(decode_fabric_registry_sync_direct(&inflated).is_none());
    let mut inflated2 = Vec::new();
    write_varint(&mut inflated2, 1);
    write_mc_string(&mut inflated2, "minecraft:block");
    write_varint(&mut inflated2, 10_000_000);
    assert!(decode_fabric_registry_sync_direct(&inflated2).is_none());
}

// ---------------------------------------------------------------------------
// 5. fluid tags
// ---------------------------------------------------------------------------

#[test]
fn fluid_tag_paths_classify_both_layouts_and_parse() {
    assert_eq!(
        fluid_tag_id_for_path("data/c/tags/fluid/water.json"),
        Some("c:water".to_string())
    );
    assert_eq!(
        fluid_tag_id_for_path("data/mymod/tags/fluids/acid.json"),
        Some("mymod:acid".to_string())
    );
    assert_eq!(
        fluid_tag_id_for_path("data/c/tags/fluid/sub/dir.json"),
        Some("c:sub/dir".to_string())
    );
    assert_eq!(fluid_tag_id_for_path("data/c/tags/item/ores.json"), None);
    assert_eq!(fluid_tag_id_for_path("pack.mcmeta"), None);

    let mut reg = FluidTagRegistry::new();
    assert!(reg
        .ingest_member(
            "data/mymod/tags/fluid/molten.json",
            br##"{"values": ["mymod:molten_iron", "#mymod:base_fluids",
                            {"id": "mymod:absent_fluid", "required": false},
                            {"id": "#mymod:missing_tag", "required": false}]}"##,
        )
        .expect("ingest"));
    assert!(reg
        .ingest_member(
            "data/mymod/tags/fluids/base_fluids.json",
            br#"{"values": ["minecraft:water", "minecraft:lava"]}"#,
        )
        .expect("ingest"));
    // Non-fluid members are ignored.
    assert!(!reg
        .ingest_member("data/mymod/tags/item/x.json", br#"{"values": []}"#)
        .unwrap());

    let resolved = reg.resolve("mymod:molten").expect("resolve");
    assert!(resolved.contains("mymod:molten_iron"));
    assert!(resolved.contains("minecraft:water"));
    assert!(resolved.contains("minecraft:lava"));
    // Optional absent entries skip silently — but resolve_with surfaces
    // them when the live registry reports the fluid present.
    assert!(!resolved.contains("mymod:absent_fluid"));
    let with_known = reg
        .resolve_with("mymod:molten", &|id| id == "mymod:absent_fluid")
        .unwrap();
    assert!(with_known.contains("mymod:absent_fluid"));

    // A *required* reference to an absent tag fails closed.
    assert!(reg
        .ingest_member(
            "data/mymod/tags/fluid/broken.json",
            br##"{"values": ["#mymod:does_not_exist"]}"##,
        )
        .unwrap());
    assert!(reg.resolve("mymod:broken").is_err());

    // replace: true clobbers earlier entries.
    let mut reg2 = FluidTagRegistry::new();
    reg2.ingest_member("data/m/tags/fluid/t.json", br#"{"values": ["a:b"]}"#).unwrap();
    reg2.ingest_member(
        "data/m/tags/fluid/t.json",
        br#"{"replace": true, "values": ["a:c"]}"#,
    )
    .unwrap();
    let r = reg2.resolve("m:t").unwrap();
    assert_eq!(r.len(), 1);
    assert!(r.contains("a:c"));

    // Tag file validation.
    let f = parse_tag_file_json(br##"{"values":[{"id":"#ns:t","required":true}]}"##, "t")
        .unwrap();
    assert_eq!(
        f.entries,
        vec![TagFileEntry::Required(TagRef::NestedTag(
            "ns:t".to_string()
        ))]
    );
    assert!(parse_tag_file_json(br#"{"values": "nope"}"#, "t").is_err());
    assert!(parse_tag_file_json(br#"{"values": [{"noid": 1}]}"#, "t").is_err());
}

// ---------------------------------------------------------------------------
// 6. NeoForge data maps
// ---------------------------------------------------------------------------

#[test]
fn data_map_paths_classify_both_layouts() {
    assert_eq!(
        data_map_key_for_path("data/neoforge/data_maps/item/compostables.json"),
        Some(("minecraft:item".to_string(), "compostables".to_string()))
    );
    assert_eq!(
        data_map_key_for_path("data/mymod/neoforge/data_maps/item/furnace_fuels.json"),
        Some(("minecraft:item".to_string(), "furnace_fuels".to_string()))
    );
    assert_eq!(
        data_map_key_for_path("data/mymod/data_maps/mymodreg/sub/custom_map.json"),
        Some(("mymodreg:sub".to_string(), "custom_map".to_string()))
    );
    assert_eq!(data_map_key_for_path("data/mymod/tags/item/x.json"), None);

    for name in NEOFORGE_BUILTIN_DATA_MAPS {
        assert!(is_builtin_data_map(name), "{name} must be built-in");
    }
    assert!(!is_builtin_data_map("my_custom_map"));
}

#[test]
fn data_map_parse_merge_replace_remove() {
    let mut reg = DataMapRegistry::new();
    // Built-in compostables on item registry.
    assert!(reg
        .ingest_member(
            "data/neoforge/data_maps/item/compostables.json",
            br#"{"values": {"minecraft:wheat_seeds": {"chance": 0.3},
                            "minecraft:oak_sapling?": {"chance": 0.3}}}"#,
        )
        .unwrap());
    // A pack override removes an entry and adds one.
    assert!(reg
        .ingest_member(
            "data/mymod/data_maps/item/compostables.json",
            br#"{"remove": ["minecraft:wheat_seeds"],
                 "values": {"mymod:super_seed": {"chance": 1.0}}}"#,
        )
        .unwrap());
    let map = reg.get("minecraft:item", "compostables").expect("map exists");
    assert!(!map.contains_key("minecraft:wheat_seeds"));
    assert!(map.contains_key("mymod:super_seed"));
    assert!(map.contains_key("minecraft:oak_sapling"));

    // replace wipes everything earlier.
    assert!(reg
        .ingest_member(
            "data/mymod/data_maps/item/compostables.json",
            br#"{"replace": true, "values": {"mymod:only": {"chance": 0.5}}}"#,
        )
        .unwrap());
    let map = reg.get("minecraft:item", "compostables").unwrap();
    assert_eq!(map.len(), 1);
    assert!(map.contains_key("mymod:only"));

    // Custom arbitrary data map (non-builtin name) loads the same way.
    assert!(reg
        .ingest_member(
            "data/mymod/data_maps/game_event/vibration_frequencies.json",
            br#"{"values": {"mymod:ping": {"frequency": 42}}}"#,
        )
        .unwrap());
    assert!(reg.get("minecraft:game_event", "vibration_frequencies").is_some());

    // Array-of-records values form.
    let file = parse_data_map(
        br#"{"values": [{"key": "a:b", "value": {"x": 1}}, {"key": "a:c?", "data": {"y": 2}}],
             "remove": ["a:z"]}"#,
        "minecraft:item",
        "custom",
        "t",
    )
    .expect("array values form");
    assert!(file.values.contains_key("a:b"));
    assert!(file.optional_keys.contains("a:c"));
    assert_eq!(file.remove, vec!["a:z"]);

    // Malformed shapes fail closed.
    assert!(parse_data_map(br#"{"remove": "x"}"#, "r", "m", "t").is_err());
    assert!(parse_data_map(br#"{"values": [1]}"#, "r", "m", "t").is_err());
    assert!(parse_data_map(b"[1,2]", "r", "m", "t").is_err());
    if let JsonValue::Object(_) = file.values["a:b"] {
    } else {
        panic!("value payload must be preserved");
    }
}

// ---------------------------------------------------------------------------
// 7. NeoForge biome modifiers
// ---------------------------------------------------------------------------

#[test]
fn biome_modifier_paths_and_core_kinds() {
    assert_eq!(
        biome_modifier_id_for_path("data/mymod/neoforge/biome_modifier/add_ores.json"),
        Some("mymod:add_ores".to_string())
    );
    assert_eq!(
        biome_modifier_id_for_path("data/mymod/neoforge/biome_modifier/sub/dir.json"),
        Some("mymod:sub/dir".to_string())
    );
    assert_eq!(biome_modifier_id_for_path("data/mymod/data_maps/item/x.json"), None);

    let add = parse_biome_modifier(
        "mymod:add_ores",
        br##"{"type": "neoforge:add_features",
             "biomes": "#c:is_overworld",
             "features": [["mymod:ore_1", "mymod:ore_2"], "mymod:ore_3"],
             "step": "underground_ores"}"##,
        "t",
    )
    .expect("add_features");
    assert_eq!(add.kind, BiomeModifierKind::AddFeatures);
    assert_eq!(add.biomes.terms, vec!["#c:is_overworld"]);
    assert_eq!(add.features, vec!["mymod:ore_1", "mymod:ore_2", "mymod:ore_3"]);
    assert_eq!(add.step.as_deref(), Some("underground_ores"));

    let rm = parse_biome_modifier(
        "mymod:rm",
        br#"{"type": "neoforge:remove_features", "biomes": ["minecraft:plains"],
             "features": "minecraft:ore_coal"}"#,
        "t",
    )
    .expect("remove_features");
    assert_eq!(rm.kind, BiomeModifierKind::RemoveFeatures);
    assert_eq!(rm.features, vec!["minecraft:ore_coal"]);

    let spawns = parse_biome_modifier(
        "mymod:spawn",
        br##"{"type": "neoforge:add_spawns", "biomes": "#c:is_nether",
             "spawners": {"type": "mymod:imp", "weight": 5, "minCount": 1, "maxCount": 3}}"##,
        "t",
    )
    .expect("add_spawns");
    assert_eq!(spawns.kind, BiomeModifierKind::AddSpawns);
    assert_eq!(spawns.spawners.len(), 1);

    let rm_spawns = parse_biome_modifier(
        "mymod:rmspawn",
        br#"{"type": "neoforge:remove_spawns", "biomes": "minecraft:plains",
             "spawners": ["minecraft:zombie"]}"#,
        "t",
    )
    .expect("remove_spawns");
    assert_eq!(rm_spawns.kind, BiomeModifierKind::RemoveSpawns);
    assert_eq!(rm_spawns.spawners.len(), 1);

    let none = parse_biome_modifier("mymod:none", br#"{"type": "neoforge:none"}"#, "t")
        .expect("none");
    assert_eq!(none.kind, BiomeModifierKind::None);

    // Custom serializer types stay generic (Other), not rejected.
    let custom = parse_biome_modifier(
        "mymod:custom",
        br##"{"type": "mymod:add_loot", "biomes": "#mymod:wet"}"##,
        "t",
    )
    .expect("custom kind");
    assert_eq!(
        custom.kind,
        BiomeModifierKind::Other("mymod:add_loot".to_string())
    );

    // Required-field enforcement fails closed.
    assert!(parse_biome_modifier(
        "x",
        br#"{"type": "neoforge:add_features", "biomes": "a:b"}"#,
        "t"
    )
    .is_err());
    assert!(parse_biome_modifier(
        "x",
        br#"{"type": "neoforge:add_spawns", "biomes": "a:b"}"#,
        "t"
    )
    .is_err());
    assert!(parse_biome_modifier(
        "x",
        br#"{"type": "neoforge:add_features", "features": "a:f", "step": "underground_ores"}"#,
        "t"
    )
    .is_err()); // missing biomes
}

// ---------------------------------------------------------------------------
// 8. Conventional `c:` tag registry
// ---------------------------------------------------------------------------

#[test]
fn conventional_registry_has_all_ten_families_with_vanilla_baseline() {
    let reg = ConventionalTagRegistry::vanilla_1_21_1_baseline();
    for fam in CONVENTIONAL_TAG_FAMILIES {
        let tag = format!("c:{fam}");
        let members = reg.members(&tag).unwrap_or_else(|| panic!("{tag}"));
        assert!(!members.is_empty(), "{tag} baseline empty");
        assert!(is_conventional_family(&tag));
    }
    assert!(reg.members("c:ores").unwrap().contains("minecraft:iron_ore"));
    assert!(reg.members("c:ingots").unwrap().contains("minecraft:iron_ingot"));
    assert!(reg.members("c:dyes").unwrap().contains("minecraft:blue_dye"));
    assert!(reg
        .members("c:storage_blocks")
        .unwrap()
        .contains("minecraft:diamond_block"));
    assert!(reg.members("c:tools").unwrap().contains("minecraft:shears"));
    assert_eq!(reg.families().count(), 10);
}

#[test]
fn conventional_registry_aliases_forge_and_neoforge_ids() {
    assert_eq!(canonical_conventional_tag_id("forge:ores"), "c:ores");
    assert_eq!(canonical_conventional_tag_id("forge:ores/iron"), "c:ores/iron");
    assert_eq!(canonical_conventional_tag_id("neoforge:dyes"), "c:dyes");
    assert_eq!(
        canonical_conventional_tag_id("forge:not_a_family"),
        "c:not_a_family"
    );
    assert_eq!(
        canonical_conventional_tag_id("minecraft:stone"),
        "minecraft:stone"
    );
    assert!(is_conventional_family("forge:widgets"));

    // Members resolve through the alias too.
    let reg = ConventionalTagRegistry::vanilla_1_21_1_baseline();
    let via_alias = reg.resolve("forge:ores").unwrap();
    assert!(via_alias.contains("minecraft:gold_ore"));
}

#[test]
fn conventional_registry_merges_datapack_tag_files_and_nested_refs() {
    let mut reg = ConventionalTagRegistry::vanilla_1_21_1_baseline();
    // Modpack merges a member into c:ingots via a forge: alias file.
    assert!(reg
        .ingest_item_tag_member(
            "data/forge/tags/item/ingots.json",
            br#"{"values": ["mymod:mythril_ingot"]}"#,
        )
        .unwrap());
    // Sub-tag nested under a family resolves transitively.
    assert!(reg
        .ingest_item_tag_member(
            "data/c/tags/item/ingots/mythril.json",
            br#"{"values": ["mymod:mythril_ingot_alt"]}"#,
        )
        .unwrap());
    // A c: tag referencing the sub-tag plus an optional absent ref.
    assert!(reg
        .ingest_item_tag_member(
            "data/c/tags/item/ores.json",
            br##"{"values": ["#c:ingots/mythril", {"id": "#c:never_defined", "required": false}]}"##,
        )
        .unwrap());
    // Non-conventional members are ignored by the consumer.
    assert!(!reg
        .ingest_item_tag_member("data/m/tags/item/flowers.json", br#"{"values":["a:b"]}"#)
        .unwrap());

    let resolved = reg.resolve("c:ingots").unwrap();
    assert!(resolved.contains("minecraft:iron_ingot")); // baseline kept
    assert!(resolved.contains("mymod:mythril_ingot"));
    let resolved_ores = reg.resolve("c:ores").unwrap();
    assert!(resolved_ores.contains("minecraft:iron_ore"));
    assert!(resolved_ores.contains("mymod:mythril_ingot_alt")); // via #c:ingots/mythril

    // Required reference to a tag with no content fails closed.
    let mut reg2 = ConventionalTagRegistry::new();
    reg2.ingest_item_tag_member(
        "data/c/tags/item/ores.json",
        br##"{"values": ["#c:missing_tag"]}"##,
    )
    .unwrap();
    assert!(reg2.resolve("c:ores").is_err());
}

// ---------------------------------------------------------------------------
// 9. inspect_jar_extensions on a bare JAR
// ---------------------------------------------------------------------------

#[test]
fn inspect_jar_extensions_reports_empty_for_plain_jars() {
    let bytes = jar(&[
        ("fabric.mod.json", fabric_manifest("plain").as_slice()),
        ("pack.mcmeta", br#"{"pack":{"pack_format":48}}"#),
    ]);
    let report = inspect_jar_extensions(&bytes, "plain.jar").expect("scan");
    assert!(report.quilt.is_none());
    assert!(report.jar_in_jar.is_empty());
    assert!(report.access_transformers.is_empty());
    assert!(!report.requires_jvm_bridge());
}
