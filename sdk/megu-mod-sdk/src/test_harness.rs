// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! In-memory GameTest runner.
//!
//! [`ModTestHarness::from_project`] validates a [`ModProject`] with the same
//! fail-closed rules the loaders apply — mod-id legality, bundle-schema
//! conformance, recipe ingredient references, tag-reference cycles,
//! capability bounds, and duplicate ids — then runs GameTest-style
//! assertions ([`assert_recipe_matches`](ModTestHarness::assert_recipe_matches),
//! [`assert_tag_contains`](ModTestHarness::assert_tag_contains),
//! [`assert_bundle_valid`](ModTestHarness::assert_bundle_valid),
//! [`assert_jar_roundtrip`](ModTestHarness::assert_jar_roundtrip)) entirely
//! in memory. No server, no JVM, sub-millisecond.
//!
//! Every problem is reported as a structured [`SdkDiagnostic`] — stable
//! `code`, dotted `path`, human `message`, and an actionable `fix_hint` —
//! serializable via [`SdkDiagnostic::to_json`] so AI coding agents can
//! auto-fix failures without parsing prose.

use std::collections::{BTreeMap, BTreeSet};

use megu_core::id::ResourceLocation;

use crate::builders::{ModProject, Movement, TagKind};
use crate::diag::mk;
use crate::jar_packer::{verify_stored_zip, BUNDLE_ENTRY, MMOD_MANIFEST};
use crate::{
    check_mod_id, check_version_range, json, normalize_ref,
    MAX_INNER_ENTRIES, MAX_SECTION_ENTRIES,
};

// ---------------------------------------------------------------------------
// Structured diagnostics
// ---------------------------------------------------------------------------

/// One structured validation/assertion finding.
///
/// `code` is stable for programmatic matching (`E_MOD_ID`,
/// `E_DUPLICATE_ID`, `E_TAG_CYCLE`, …); `path` locates the offending value
/// (`"blocks[2].id"`, `"creatures.sky_drake.tame_chance"`); `fix_hint` is a
/// one-line actionable suggestion.
#[derive(Clone, Debug, PartialEq)]
pub struct SdkDiagnostic {
    /// Stable machine code (e.g. `"E_DUPLICATE_ID"`).
    pub code: &'static str,
    /// Dotted path to the offending declaration or field.
    pub path: String,
    /// Human-readable description of the problem.
    pub message: String,
    /// One-line fix suggestion.
    pub fix_hint: String,
}

impl SdkDiagnostic {
    /// Render this diagnostic as one JSON object
    /// (`{"code","path","message","fix_hint"}`).
    pub fn to_json(&self) -> String {
        json::render(&json::obj(vec![
            ("code", json::s(self.code)),
            ("path", json::s(&self.path)),
            ("message", json::s(&self.message)),
            ("fix_hint", json::s(&self.fix_hint)),
        ]))
    }
}

/// Render a diagnostic list as a JSON array — the machine-readable report
/// an agent consumes after a failed build/assertion.
pub fn diagnostics_to_json(diags: &[SdkDiagnostic]) -> String {
    json::render(&json::arr(
        diags
            .iter()
            .map(|d| {
                json::obj(vec![
                    ("code", json::s(d.code)),
                    ("path", json::s(&d.path)),
                    ("message", json::s(&d.message)),
                    ("fix_hint", json::s(&d.fix_hint)),
                ])
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// Project validation battery
// ---------------------------------------------------------------------------

fn dup_check<'a>(
    ids: impl Iterator<Item = &'a str>,
    section: &str,
    diags: &mut Vec<SdkDiagnostic>,
) {
    let mut seen = BTreeSet::new();
    for id in ids {
        if !seen.insert(id.to_string()) {
            diags.push(mk(
                "E_DUPLICATE_ID",
                section,
                format!("duplicate id '{id}' in {section}"),
                "rename or remove the duplicate declaration",
            ));
        }
    }
}

fn cap_check(len: usize, section: &str, diags: &mut Vec<SdkDiagnostic>) {
    if len > MAX_SECTION_ENTRIES {
        diags.push(mk(
            "E_SCHEMA",
            section,
            format!("{len} entries exceeds the {MAX_SECTION_ENTRIES} bundle section cap"),
            "split the declarations or remove extras",
        ));
    }
}

fn rl_ok(s: &str) -> bool {
    ResourceLocation::parse(s).is_ok()
}

fn ref_ok(s: &str) -> bool {
    match s.strip_prefix('#') {
        Some(tag) => rl_ok(tag),
        None => rl_ok(s),
    }
}

/// The full fail-closed validation battery [`ModProjectBuilder::build`],
/// [`ModProject::export_megu_bundle_json`], and
/// [`ModTestHarness::from_project`] all share: mod-id legality, version
/// ranges, duplicate ids, per-spec bounds, capability bounds, recipe
/// reference resolution, tag-reference integrity, and tag cycles.
///
/// Returns every problem found, in declaration order — an empty `Vec`
/// means the project is clean.
pub fn validate_project(project: &ModProject) -> Vec<SdkDiagnostic> {
    let mut diags = Vec::new();

    if let Err(why) = check_mod_id(&project.mod_id) {
        diags.push(mk(
            "E_MOD_ID",
            "mod_id",
            why,
            "use a lowercase [a-z0-9_.-] mod id",
        ));
    }
    if project.version.is_empty() || project.version.chars().any(|c| c.is_control()) {
        diags.push(mk(
            "E_VERSION",
            "version",
            "version must be non-empty free text".to_string(),
            "pass a semver-ish version like '1.0.0'",
        ));
    }
    if let Err(why) = check_version_range(&project.mc_version_range) {
        diags.push(mk(
            "E_VERSION_RANGE",
            "mc_version_range",
            why,
            "use Maven '[1.21.1,1.22)' or SemVer '>=1.21.1 <1.22' syntax",
        ));
    }
    for (i, dep) in project.dependencies.iter().enumerate() {
        if let Err(why) = check_mod_id(&dep.mod_id) {
            diags.push(mk(
                "E_MOD_ID",
                format!("dependencies[{i}]"),
                why,
                "dependency mod ids follow the same [a-z0-9_.-] rules",
            ));
        }
        if let Err(why) = check_version_range(&dep.version_range) {
            diags.push(mk(
                "E_VERSION_RANGE",
                format!("dependencies[{i}].version_range"),
                why,
                "use a Maven or SemVer range",
            ));
        }
    }

    // Section caps (bundle fail-closed bounds).
    cap_check(project.blocks.len(), "blocks", &mut diags);
    cap_check(project.items.len(), "items", &mut diags);
    cap_check(project.recipes.len(), "recipes", &mut diags);
    cap_check(project.tags.len(), "tags", &mut diags);
    cap_check(project.creatures.len(), "creatures", &mut diags);
    cap_check(project.dimensions.len(), "dimensions", &mut diags);
    cap_check(project.locales.len(), "locales", &mut diags);

    // Duplicate ids per section.
    dup_check(project.blocks.iter().map(|b| b.id.as_str()), "blocks", &mut diags);
    dup_check(project.items.iter().map(|i| i.id.as_str()), "items", &mut diags);
    dup_check(
        project.recipes.iter().map(|r| r.id.as_str()),
        "recipes",
        &mut diags,
    );
    let tag_keys: Vec<String> = project
        .tags
        .iter()
        .map(|t| format!("{}:{}", t.kind.dir(), t.id))
        .collect();
    dup_check(tag_keys.iter().map(String::as_str), "tags", &mut diags);
    dup_check(
        project.creatures.iter().map(|c| c.entity_type.as_str()),
        "creatures",
        &mut diags,
    );
    dup_check(
        project.dimensions.iter().map(|d| d.id.as_str()),
        "dimensions",
        &mut diags,
    );
    dup_check(
        project.locales.iter().map(|l| l.locale.as_str()),
        "locales",
        &mut diags,
    );

    // Per-spec field re-validation (hand-constructed projects bypass the
    // builders, so the harness re-runs every bound).
    for (i, b) in project.blocks.iter().enumerate() {
        let ctx = format!("blocks[{i}]({})", b.id);
        if !rl_ok(&b.id) {
            diags.push(mk("E_ID", &ctx, format!("bad block id {:?}", b.id), "use 'ns:path' ids"));
        }
        if !b.hardness.is_finite() || (b.hardness < 0.0 && b.hardness != -1.0) {
            diags.push(mk("E_RANGE", format!("{ctx}.hardness"), "hardness must be >= 0 or -1.0 (unbreakable)".to_string(), "set hardness within bounds"));
        }
        if !b.blast_resistance.is_finite() || b.blast_resistance < 0.0 {
            diags.push(mk("E_RANGE", format!("{ctx}.blast_resistance"), "blast_resistance must be >= 0".to_string(), "set blast_resistance >= 0"));
        }
        if b.light_level > 15 {
            diags.push(mk("E_RANGE", format!("{ctx}.light_level"), "light_level must be 0..=15".to_string(), "use a 0..=15 light level"));
        }
        if let Some(drop) = &b.loot_drop {
            if !rl_ok(drop) {
                diags.push(mk("E_REF_ID", format!("{ctx}.loot_drop"), format!("bad loot drop {drop:?}"), "use an 'ns:item' id"));
            }
        }
        for t in &b.tags {
            if !rl_ok(t) {
                diags.push(mk("E_REF_ID", format!("{ctx}.tags"), format!("bad tag id {t:?}"), "use plain 'ns:tag' ids"));
            }
        }
        if let Some(cap) = &b.capability {
            if let Err(why) = cap.check_bounds() {
                diags.push(mk("E_CAPABILITY", format!("{ctx}.capability"), why, "see CapabilitySpec docs for the bound table"));
            }
        }
    }
    for (i, it) in project.items.iter().enumerate() {
        let ctx = format!("items[{i}]({})", it.id);
        if !rl_ok(&it.id) {
            diags.push(mk("E_ID", &ctx, format!("bad item id {:?}", it.id), "use 'ns:path' ids"));
        }
        if !(1..=64).contains(&it.max_stack) {
            diags.push(mk("E_RANGE", format!("{ctx}.max_stack"), "max_stack must be 1..=64".to_string(), "set max_stack within 1..=64"));
        }
        if let Some(d) = it.durability {
            if d == 0 {
                diags.push(mk("E_RANGE", format!("{ctx}.durability"), "durability must be >= 1".to_string(), "omit .durability() or set >= 1"));
            }
        }
        if let Some(d) = it.attack_damage {
            if !d.is_finite() || d < 0.0 {
                diags.push(mk("E_RANGE", format!("{ctx}.attack_damage"), "attack_damage must be >= 0".to_string(), "set a non-negative damage"));
            }
        }
        if let Some(food) = it.food {
            if food.nutrition > 20 {
                diags.push(mk("E_RANGE", format!("{ctx}.food.nutrition"), "nutrition must be 0..=20".to_string(), "use vanilla hunger bounds"));
            }
            if !food.saturation.is_finite() || food.saturation < 0.0 {
                diags.push(mk("E_RANGE", format!("{ctx}.food.saturation"), "saturation must be >= 0".to_string(), "set a non-negative saturation"));
            }
        }
        for t in &it.tags {
            if !rl_ok(t) {
                diags.push(mk("E_REF_ID", format!("{ctx}.tags"), format!("bad tag id {t:?}"), "use plain 'ns:tag' ids"));
            }
        }
    }

    // Recipe references resolve against declared content. Foreign
    // namespaces are assumed external (vanilla or another mod); same-
    // namespace references must name a declared block/item/tag.
    let item_domain: BTreeSet<&str> = project
        .blocks
        .iter()
        .map(|b| b.id.as_str())
        .chain(project.items.iter().map(|i| i.id.as_str()))
        .collect();
    let item_tag_ids: BTreeSet<&str> = project
        .tags
        .iter()
        .filter(|t| t.kind == TagKind::Item)
        .map(|t| t.id.as_str())
        .collect();
    let block_tag_ids: BTreeSet<&str> = project
        .tags
        .iter()
        .filter(|t| t.kind == TagKind::Block)
        .map(|t| t.id.as_str())
        .collect();
    let check_ref_resolves = |reference: &str,
                              path: String,
                              allow_tag: bool,
                              diags: &mut Vec<SdkDiagnostic>| {
        if let Some(tag) = reference.strip_prefix('#') {
            if !allow_tag {
                diags.push(mk(
                    "E_RECIPE_REF",
                    path,
                    format!("tag reference {reference:?} is not allowed here"),
                    "name a concrete 'ns:item' id",
                ));
                return;
            }
            if !rl_ok(tag) {
                diags.push(mk("E_REF_ID", path, format!("bad tag id {tag:?}"), "use 'ns:tag' ids"));
                return;
            }
            if tag.split(':').next() == Some(project.mod_id.as_str())
                && !item_tag_ids.contains(tag)
            {
                diags.push(mk(
                    "E_RECIPE_REF",
                    path,
                    format!("recipe references undeclared item tag {reference:?}"),
                    "declare it with TagBuilder::item(...) or fix the namespace",
                ));
            }
        } else {
            if !rl_ok(reference) {
                diags.push(mk("E_REF_ID", path, format!("bad item id {reference:?}"), "use 'ns:item' ids"));
                return;
            }
            if reference.split(':').next() == Some(project.mod_id.as_str())
                && !item_domain.contains(reference)
            {
                diags.push(mk(
                    "E_RECIPE_REF",
                    path,
                    format!("recipe references undeclared item {reference:?}"),
                    "declare the item/block or fix the id",
                ));
            }
        }
    };
    for (i, r) in project.recipes.iter().enumerate() {
        let ctx = format!("recipes[{i}]({})", r.id);
        if !rl_ok(&r.id) {
            diags.push(mk("E_ID", &ctx, format!("bad recipe id {:?}", r.id), "use 'ns:path' ids"));
        }
        for input in r.inputs() {
            check_ref_resolves(
                input,
                format!("{ctx}.ingredients[{input}]"),
                true,
                &mut diags,
            );
        }
        check_ref_resolves(
            r.result_item(),
            format!("{ctx}.result"),
            false,
            &mut diags,
        );
        if !(1..=64).contains(&r.result_count()) {
            diags.push(mk(
                "E_RANGE",
                format!("{ctx}.result.count"),
                "result count must be 1..=64".to_string(),
                "set result_count within 1..=64",
            ));
        }
    }

    // Tag values + nested-reference resolution + cycle detection.
    for (i, t) in project.tags.iter().enumerate() {
        let ctx = format!("tags[{i}]({})", t.id);
        if !rl_ok(&t.id) {
            diags.push(mk("E_ID", &ctx, format!("bad tag id {:?}", t.id), "use 'ns:tag' ids"));
        }
        if t.values.len() > MAX_INNER_ENTRIES {
            diags.push(mk(
                "E_SCHEMA",
                &ctx,
                format!("{} members exceeds the {MAX_INNER_ENTRIES} inner cap", t.values.len()),
                "split the tag",
            ));
        }
        for v in &t.values {
            if !ref_ok(v) {
                diags.push(mk("E_REF_ID", format!("{ctx}.values"), format!("bad member {v:?}"), "use 'ns:id' or '#ns:tag'"));
                continue;
            }
            if let Some(nested) = v.strip_prefix('#') {
                let declared = match t.kind {
                    TagKind::Item => &item_tag_ids,
                    TagKind::Block => &block_tag_ids,
                };
                if nested.split(':').next() == Some(project.mod_id.as_str())
                    && !declared.contains(nested)
                {
                    diags.push(mk(
                        "E_TAG_REF",
                        format!("{ctx}.values"),
                        format!("nested tag {v:?} is not declared in this project"),
                        "declare the nested tag or fix the namespace",
                    ));
                }
            }
        }
    }
    // DFS over same-kind nested-tag edges; visiting state detects cycles.
    {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Visiting,
            Done,
        }
        let tag_map: BTreeMap<(TagKind, &str), &crate::builders::TagSpec> = project
            .tags
            .iter()
            .map(|t| ((t.kind, t.id.as_str()), t))
            .collect();
        fn visit<'a>(
            key: (TagKind, &'a str),
            map: &BTreeMap<(TagKind, &'a str), &'a crate::builders::TagSpec>,
            marks: &mut BTreeMap<(TagKind, &'a str), Mark>,
            stack: &mut Vec<&'a str>,
            diags: &mut Vec<SdkDiagnostic>,
        ) {
            match marks.get(&key) {
                Some(Mark::Done) => return,
                Some(Mark::Visiting) => {
                    let mut cycle: Vec<&str> = stack
                        .iter()
                        .skip_while(|&&s| s != key.1)
                        .copied()
                        .collect();
                    cycle.push(key.1);
                    diags.push(mk(
                        "E_TAG_CYCLE",
                        format!("tags.{}", key.1),
                        format!("nested tag cycle: {}", cycle.join(" -> ")),
                        "remove one '#'-reference edge from the cycle",
                    ));
                    return;
                }
                None => {}
            }
            marks.insert(key, Mark::Visiting);
            stack.push(key.1);
            if let Some(spec) = map.get(&key) {
                for v in &spec.values {
                    if let Some(nested) = v.strip_prefix('#') {
                        let nk = (key.0, nested);
                        if map.contains_key(&nk) {
                            visit(nk, map, marks, stack, diags);
                        }
                    }
                }
            }
            stack.pop();
            marks.insert(key, Mark::Done);
        }
        let mut marks = BTreeMap::new();
        let mut stack = Vec::new();
        for spec in &project.tags {
            visit(
                (spec.kind, spec.id.as_str()),
                &tag_map,
                &mut marks,
                &mut stack,
                &mut diags,
            );
        }
    }

    // Creatures.
    for (i, c) in project.creatures.iter().enumerate() {
        let ctx = format!("creatures[{i}]({})", c.entity_type);
        if !rl_ok(&c.entity_type) {
            diags.push(mk("E_ID", &ctx, format!("bad entity_type {:?}", c.entity_type), "use 'ns:path' ids"));
        }
        if !c.max_health.is_finite() || c.max_health <= 0.0 {
            diags.push(mk("E_RANGE", format!("{ctx}.max_health"), "max_health must be > 0".to_string(), "set a positive health"));
        }
        let speed_bad = |v: f64| !v.is_finite() || v < 0.0;
        let speeds: &[f64] = match &c.movement {
            Movement::Land { ground_speed } => std::slice::from_ref(ground_speed),
            Movement::Flying {
                ground_speed,
                fly_speed,
                glide_fall_scale,
            } => &[*ground_speed, *fly_speed, *glide_fall_scale],
            Movement::Aquatic { swim_speed, .. } => std::slice::from_ref(swim_speed),
        };
        if speeds.iter().any(|s| speed_bad(*s)) {
            diags.push(mk("E_RANGE", format!("{ctx}.movement"), "movement speeds must be >= 0".to_string(), "set non-negative speeds"));
        }
        if let Movement::Aquatic {
            suffocates_in_air_ticks: 0,
            ..
        } = c.movement
        {
            diags.push(mk("E_RANGE", format!("{ctx}.movement"), "suffocates_in_air_ticks must be >= 1".to_string(), "give an air budget"));
        }
        if !(0.0..=1.0).contains(&c.tame_chance) {
            diags.push(mk("E_RANGE", format!("{ctx}.tame_chance"), "tame_chance must lie in [0, 1]".to_string(), "use a probability"));
        }
        if let Some(s) = c.growth_stages {
            if !(1..=5).contains(&s) {
                diags.push(mk("E_RANGE", format!("{ctx}.growth_stages"), "growth_stages must be 1..=5".to_string(), "use 1..=5 stages"));
            }
        }
        for (name, ticks) in [
            ("baby_growth_ticks", c.baby_growth_ticks),
            ("breed_cooldown_ticks", c.breed_cooldown_ticks),
        ] {
            if ticks == Some(0) {
                diags.push(mk("E_RANGE", format!("{ctx}.{name}"), format!("{name} must be >= 1"), "omit or set >= 1"));
            }
        }
        for v in c.tame_items.iter().chain(c.breed_items.iter()) {
            if !ref_ok(v) {
                diags.push(mk("E_REF_ID", format!("{ctx}.items"), format!("bad item/tag ref {v:?}"), "use 'ns:item' or '#ns:tag'"));
            }
        }
        for (item, heal) in &c.heal_items {
            if !ref_ok(item) {
                diags.push(mk("E_REF_ID", format!("{ctx}.heal_items"), format!("bad item/tag ref {item:?}"), "use 'ns:item' or '#ns:tag'"));
            }
            if !heal.is_finite() || *heal < 0.0 {
                diags.push(mk("E_RANGE", format!("{ctx}.heal_items"), "heal amount must be >= 0".to_string(), "use a non-negative heal"));
            }
        }
    }

    // Dimensions (vanilla dimension_type bounds).
    for (i, d) in project.dimensions.iter().enumerate() {
        let ctx = format!("dimensions[{i}]({})", d.id);
        if !rl_ok(&d.id) {
            diags.push(mk("E_ID", &ctx, format!("bad dimension id {:?}", d.id), "use 'ns:path' ids"));
        }
        if d.min_y % 16 != 0 || !(-2032..=2031).contains(&d.min_y) {
            diags.push(mk("E_DIMENSION", format!("{ctx}.min_y"), "min_y must be a multiple of 16 in [-2032, 2031]".to_string(), "chunk-align the bottom"));
        }
        if !d.height.is_multiple_of(16) || !(16..=4064).contains(&d.height) {
            diags.push(mk("E_DIMENSION", format!("{ctx}.height"), "height must be a multiple of 16 in [16, 4064]".to_string(), "chunk-align the height"));
        }
        if i64::from(d.min_y) + i64::from(d.height) > 4064 {
            diags.push(mk("E_DIMENSION", &ctx, "min_y + height exceeds 4064".to_string(), "lower min_y or shrink height"));
        }
        if !(1.0e-5..=3.0e7).contains(&d.coordinate_scale) {
            diags.push(mk("E_DIMENSION", format!("{ctx}.coordinate_scale"), "coordinate_scale must lie in [1e-5, 3e7]".to_string(), "1.0 overworld / 8.0 nether"));
        }
        if !(0.0..=1.0).contains(&d.ambient_light) {
            diags.push(mk("E_DIMENSION", format!("{ctx}.ambient_light"), "ambient_light must lie in [0.0, 1.0]".to_string(), "use 0.0..=1.0"));
        }
    }

    // Locales.
    for (i, l) in project.locales.iter().enumerate() {
        let ctx = format!("locales[{i}]({})", l.locale);
        let parts: Vec<&str> = l.locale.split('_').collect();
        let ok = parts.len() == 2
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase()));
        if !ok {
            diags.push(mk("E_LOCALE", &ctx, format!("locale code {:?} must look like 'en_us'", l.locale), "use '<lang>_<region>' lowercase"));
        }
        for key in l.entries.keys() {
            if key.is_empty() || key.chars().any(|c| c.is_control()) {
                diags.push(mk("E_LOCALE", &ctx, format!("bad translation key {key:?}"), "use dotted keys like 'block.<mod>.<id>'"));
            }
        }
    }

    diags
}

// ---------------------------------------------------------------------------
// Minimal bounded JSON parser (for assert_bundle_valid)
// ---------------------------------------------------------------------------

/// A parsed JSON value (object order preserved).
#[derive(Debug, PartialEq)]
enum Jv {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Jv>),
    Obj(Vec<(String, Jv)>),
}

impl Jv {
    fn obj_get<'a>(&'a self, key: &str) -> Option<&'a Jv> {
        match self {
            Jv::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn obj_pairs(&self) -> Option<&Vec<(String, Jv)>> {
        match self {
            Jv::Obj(pairs) => Some(pairs),
            _ => None,
        }
    }
}

struct JParser<'a> {
    b: &'a [u8],
    i: usize,
}

const MAX_JSON_DEPTH: usize = 64;

impl<'a> JParser<'a> {
    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn err<T>(&self, m: &str) -> Result<T, String> {
        Err(format!("JSON byte {}: {m}", self.i))
    }

    fn value(&mut self, depth: usize) -> Result<Jv, String> {
        if depth > MAX_JSON_DEPTH {
            return self.err("nesting too deep");
        }
        self.ws();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Jv::Str(self.string()?)),
            Some(b't') => self.lit("true", Jv::Bool(true)),
            Some(b'f') => self.lit("false", Jv::Bool(false)),
            Some(b'n') => self.lit("null", Jv::Null),
            Some(_) => self.number(),
            None => self.err("unexpected end of input"),
        }
    }

    fn lit(&mut self, word: &str, v: Jv) -> Result<Jv, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            self.err("bad literal")
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return self.err("unterminated string"),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    match self.peek() {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'b') => out.push('\u{8}'),
                        Some(b'f') => out.push('\u{c}'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'u') => {
                            self.i += 1;
                            let hi = self.hex4()?;
                            if (0xD800..0xDC00).contains(&hi) {
                                if self.peek() == Some(b'\\')
                                    && self.b.get(self.i + 1) == Some(&b'u')
                                {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return self.err("bad low surrogate");
                                    }
                                    let cp = 0x1_0000
                                        + (((hi - 0xD800) as u32) << 10)
                                        + ((lo - 0xDC00) as u32);
                                    out.push(
                                        char::from_u32(cp)
                                            .ok_or_else(|| format!("bad char {cp}"))?,
                                    );
                                    continue;
                                }
                                return self.err("lone high surrogate");
                            }
                            out.push(
                                char::from_u32(hi as u32)
                                    .ok_or_else(|| "bad char".to_string())?,
                            );
                            continue;
                        }
                        _ => return self.err("bad escape"),
                    }
                    self.i += 1;
                }
                Some(c) if c < 0x20 => return self.err("control char in string"),
                Some(_) => {
                    // Consume one UTF-8 sequence verbatim.
                    let start = self.i;
                    let len = match self.b[start] {
                        0x00..=0x7f => 1,
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf7 => 4,
                        _ => return self.err("invalid UTF-8"),
                    };
                    let end = start + len;
                    let s = std::str::from_utf8(
                        self.b.get(start..end).ok_or("truncated UTF-8")?,
                    )
                    .map_err(|e| e.to_string())?;
                    out.push_str(s);
                    self.i = end;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u16, String> {
        let end = self.i + 4;
        let hex = self
            .b
            .get(self.i..end)
            .ok_or_else(|| format!("JSON byte {}: truncated \\u escape", self.i))?;
        let s = std::str::from_utf8(hex).map_err(|e| e.to_string())?;
        let v = u16::from_str_radix(s, 16).map_err(|e| e.to_string())?;
        self.i = end;
        Ok(v)
    }

    fn number(&mut self) -> Result<Jv, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
        {
            self.i += 1;
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?;
        text.parse::<f64>()
            .map(Jv::Num)
            .map_err(|_| format!("JSON byte {start}: bad number {text:?}"))
    }

    fn array(&mut self, depth: usize) -> Result<Jv, String> {
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Jv::Arr(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Jv::Arr(items));
                }
                _ => return self.err("expected ',' or ']'"),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Jv, String> {
        self.i += 1;
        let mut pairs = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Jv::Obj(pairs));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return self.err("expected object key");
            }
            let key = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return self.err("expected ':'");
            }
            self.i += 1;
            let v = self.value(depth + 1)?;
            pairs.push((key, v));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Jv::Obj(pairs));
                }
                _ => return self.err("expected ',' or '}'"),
            }
        }
    }
}

fn parse_json_doc(text: &str) -> Result<Jv, String> {
    let mut p = JParser {
        b: text.as_bytes(),
        i: 0,
    };
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("JSON byte {}: trailing data", p.i));
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Bundle-document validation (mirrors from_ast's fail-closed checks)
// ---------------------------------------------------------------------------

const BUNDLE_TOP_KEYS: &[&str] = &[
    "schema",
    "mod_id",
    "mc_version_range",
    "portals",
    "progression_barriers",
    "biome_regions",
    "creatures",
    "custom_channels",
    "spells",
    "custom_statuses",
    "rituals",
];
const BUNDLE_REQUIRED_ARRAYS: &[&str] = &[
    "portals",
    "progression_barriers",
    "biome_regions",
    "creatures",
    "custom_channels",
];
const BUNDLE_OPTIONAL_ARRAYS: &[&str] = &["spells", "custom_statuses", "rituals"];
const CREATURE_KEYS: &[&str] = &[
    "entity_type",
    "max_health",
    "tame_items",
    "tame_chance",
    "breed_items",
    "heal_items",
    "baby_growth_ticks",
    "breed_cooldown_ticks",
    "supports_wander_mode",
    "can_shoulder_perch",
    "rideable_when_tamed",
    "movement",
];

fn schema_diag(path: impl AsRef<str>, msg: impl Into<String>) -> SdkDiagnostic {
    mk(
        "E_SCHEMA",
        path,
        msg,
        "see mod-adapter-core/src/unified_bundle.rs for the accepted key set",
    )
}

/// Validate emitted `megu-mod-bundle/1` text against the same rules
/// `DeclarativeModBundle::from_json` enforces: exact `schema` tag, the
/// closed top-level key set, required/optional array sections, entry caps,
/// `ResourceLocation` ids, a parsable `mc_version_range`, and per-creature
/// field types/bounds.
pub fn validate_bundle_text(text: &str) -> Vec<SdkDiagnostic> {
    let mut diags = Vec::new();
    let root = match parse_json_doc(text) {
        Ok(v) => v,
        Err(e) => {
            diags.push(schema_diag("bundle", e));
            return diags;
        }
    };
    let Some(obj) = root.obj_pairs() else {
        diags.push(schema_diag("bundle", "root must be a JSON object".to_string()));
        return diags;
    };
    for (key, _) in obj {
        if !BUNDLE_TOP_KEYS.contains(&key.as_str()) {
            diags.push(schema_diag(
                "bundle",
                format!("unknown key '{key}'"),
            ));
        }
    }
    match root.obj_get("schema") {
        Some(Jv::Str(s)) if s == crate::builders::BUNDLE_SCHEMA => {}
        Some(Jv::Str(s)) => diags.push(schema_diag(
            "bundle.schema",
            format!("unknown bundle schema {s:?}"),
        )),
        Some(_) => diags.push(schema_diag("bundle.schema", "schema must be a string".to_string())),
        None => diags.push(schema_diag("bundle.schema", "missing 'schema'".to_string())),
    }
    match root.obj_get("mod_id") {
        Some(Jv::Str(s)) if rl_ok(s) => {}
        Some(Jv::Str(s)) => diags.push(schema_diag(
            "bundle.mod_id",
            format!("mod_id {s:?} is not a resource location"),
        )),
        _ => diags.push(schema_diag("bundle.mod_id", "missing or non-string mod_id".to_string())),
    }
    match root.obj_get("mc_version_range") {
        Some(Jv::Str(s)) => {
            if let Err(why) = check_version_range(s) {
                diags.push(schema_diag("bundle.mc_version_range", why));
            }
        }
        _ => diags.push(schema_diag(
            "bundle.mc_version_range",
            "missing or non-string mc_version_range".to_string(),
        )),
    }
    for key in BUNDLE_REQUIRED_ARRAYS
        .iter()
        .chain(BUNDLE_OPTIONAL_ARRAYS.iter())
    {
        let required = BUNDLE_REQUIRED_ARRAYS.contains(key);
        match root.obj_get(key) {
            None if required => diags.push(schema_diag(
                format!("bundle.{key}"),
                format!("missing '{key}'"),
            )),
            None => {}
            Some(Jv::Arr(items)) => {
                if items.len() > MAX_SECTION_ENTRIES {
                    diags.push(schema_diag(
                        format!("bundle.{key}"),
                        format!("{} entries exceeds the {MAX_SECTION_ENTRIES} cap", items.len()),
                    ));
                }
            }
            Some(_) => diags.push(schema_diag(
                format!("bundle.{key}"),
                format!("'{key}' must be an array"),
            )),
        }
    }
    // custom_channels: strings, each a resource location.
    if let Some(Jv::Arr(items)) = root.obj_get("custom_channels") {
        for v in items {
            match v {
                Jv::Str(s) if rl_ok(s) => {}
                _ => diags.push(schema_diag(
                    "bundle.custom_channels[]",
                    "channel ids must be resource-location strings".to_string(),
                )),
            }
        }
    }
    // creatures[]: closed key set + typed fields + bounds.
    if let Some(Jv::Arr(items)) = root.obj_get("creatures") {
        for (i, item) in items.iter().enumerate() {
            let ctx = format!("creatures[{i}]");
            let Some(pairs) = item.obj_pairs() else {
                diags.push(schema_diag(&ctx, "creature must be an object".to_string()));
                continue;
            };
            for (key, _) in pairs {
                if !CREATURE_KEYS.contains(&key.as_str()) {
                    diags.push(schema_diag(&ctx, format!("unknown key '{key}'")));
                }
            }
            match item.obj_get("entity_type") {
                Some(Jv::Str(s)) if rl_ok(s) => {}
                _ => diags.push(schema_diag(
                    format!("{ctx}.entity_type"),
                    "missing or invalid entity_type".to_string(),
                )),
            }
            match item.obj_get("max_health") {
                Some(Jv::Num(n)) if n.is_finite() && *n > 0.0 => {}
                _ => diags.push(schema_diag(
                    format!("{ctx}.max_health"),
                    "max_health must be a number > 0".to_string(),
                )),
            }
            match item.obj_get("tame_chance") {
                None => {}
                Some(Jv::Num(n)) if n.is_finite() && (0.0..=1.0).contains(n) => {}
                _ => diags.push(schema_diag(
                    format!("{ctx}.tame_chance"),
                    "tame_chance must lie in [0, 1]".to_string(),
                )),
            }
            for list_key in ["tame_items", "breed_items"] {
                if let Some(v) = item.obj_get(list_key) {
                    match v {
                        Jv::Arr(members)
                            if members.iter().all(|m| matches!(m, Jv::Str(s) if ref_ok(s))) => {}
                        _ => diags.push(schema_diag(
                            format!("{ctx}.{list_key}"),
                            format!("{list_key} must be an array of item/tag ids"),
                        )),
                    }
                }
            }
            if let Some(v) = item.obj_get("heal_items") {
                let ok = matches!(v, Jv::Arr(entries) if entries.iter().all(|e| {
                    matches!(e.obj_get("item"), Some(Jv::Str(s)) if ref_ok(s))
                        && matches!(e.obj_get("heal"), Some(Jv::Num(n)) if n.is_finite() && *n >= 0.0)
                }));
                if !ok {
                    diags.push(schema_diag(
                        format!("{ctx}.heal_items"),
                        "heal_items entries must be {item, heal >= 0}".to_string(),
                    ));
                }
            }
            for int_key in ["baby_growth_ticks", "breed_cooldown_ticks"] {
                if let Some(v) = item.obj_get(int_key) {
                    let ok = matches!(v, Jv::Num(n) if n.is_finite() && *n >= 1.0 && n.fract() == 0.0);
                    if !ok {
                        diags.push(schema_diag(
                            format!("{ctx}.{int_key}"),
                            format!("{int_key} must be an integer >= 1"),
                        ));
                    }
                }
            }
            for bool_key in [
                "supports_wander_mode",
                "can_shoulder_perch",
                "rideable_when_tamed",
            ] {
                if let Some(v) = item.obj_get(bool_key) {
                    if !matches!(v, Jv::Bool(_)) {
                        diags.push(schema_diag(
                            format!("{ctx}.{bool_key}"),
                            format!("{bool_key} must be a boolean"),
                        ));
                    }
                }
            }
            match item.obj_get("movement") {
                Some(mv) => {
                    let kind = mv.obj_get("kind");
                    let required_fields: &[&str] = match kind {
                        Some(Jv::Str(k)) => match k.as_str() {
                            "land" => &["ground_speed"],
                            "amphibious" => &["land_speed", "swim_speed"],
                            "aquatic" => &["swim_speed", "suffocates_in_air_ticks"],
                            "gliding_flight" => {
                                &["ground_speed", "fly_speed", "glide_fall_scale"]
                            }
                            "wall_climb" => &["ground_speed", "climb_speed"],
                            other => {
                                diags.push(schema_diag(
                                    format!("{ctx}.movement.kind"),
                                    format!("unknown movement profile kind '{other}'"),
                                ));
                                &[]
                            }
                        },
                        _ => {
                            diags.push(schema_diag(
                                format!("{ctx}.movement"),
                                "missing movement.kind".to_string(),
                            ));
                            &[]
                        }
                    };
                    for field in required_fields {
                        match mv.obj_get(field) {
                            Some(Jv::Num(n)) if n.is_finite() && *n >= 0.0 => {}
                            _ => diags.push(schema_diag(
                                format!("{ctx}.movement.{field}"),
                                format!("{field} must be a number >= 0"),
                            )),
                        }
                    }
                }
                None => diags.push(schema_diag(
                    format!("{ctx}.movement"),
                    "missing 'movement'".to_string(),
                )),
            }
        }
    }
    diags
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// In-memory GameTest runner over a validated [`ModProject`].
///
/// `from_project` runs the full [`validate_project`] battery — all
/// diagnostics are returned in one pass, so a bad project never reaches the
/// assertion stage silently.
#[derive(Clone, Debug)]
pub struct ModTestHarness {
    project: ModProject,
    recipes: BTreeMap<String, usize>,
    /// tag id → every member its emitted tag file will contain (declared
    /// `TagSpec` values + block/item `.tag(...)` memberships merged, both
    /// kinds — matching `archive_files`' merge view).
    tag_values: BTreeMap<String, Vec<String>>,
}

impl ModTestHarness {
    /// Validate `project` and index its recipes/tags for assertion lookup.
    ///
    /// # Errors
    ///
    /// Every [`SdkDiagnostic`] the validation battery produced.
    pub fn from_project(project: &ModProject) -> Result<Self, Vec<SdkDiagnostic>> {
        let diags = validate_project(project);
        if !diags.is_empty() {
            return Err(diags);
        }
        let recipes = project
            .recipes
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id.clone(), i))
            .collect();
        let mut tag_values: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for t in &project.tags {
            tag_values
                .entry(t.id.clone())
                .or_default()
                .extend(t.values.iter().cloned());
        }
        for b in &project.blocks {
            for t in &b.tags {
                tag_values
                    .entry(t.clone())
                    .or_default()
                    .push(b.id.clone());
            }
        }
        for i in &project.items {
            for t in &i.tags {
                tag_values
                    .entry(t.clone())
                    .or_default()
                    .push(i.id.clone());
            }
        }
        Ok(Self {
            project: project.clone(),
            recipes,
            tag_values,
        })
    }

    /// The validated project under test.
    pub fn project(&self) -> &ModProject {
        &self.project
    }

    /// Assert recipe `recipe_id` exists with `expected_type`
    /// (`"minecraft:crafting_shaped"` — a bare kind implies `minecraft:`)
    /// producing `expected_result` (`ns:item`).
    ///
    /// # Errors
    ///
    /// `E_ASSERT_RECIPE` diagnostic describing the mismatch.
    pub fn assert_recipe_matches(
        &self,
        recipe_id: &str,
        expected_type: &str,
        expected_result: &str,
    ) -> Result<(), SdkDiagnostic> {
        let qid = if recipe_id.contains(':') {
            recipe_id.to_string()
        } else {
            format!("{}:{recipe_id}", self.project.mod_id)
        };
        let idx = self.recipes.get(&qid).ok_or_else(|| {
            mk(
                "E_ASSERT_RECIPE",
                format!("recipe {qid}"),
                format!("no recipe declared with id '{qid}'"),
                "add the recipe via RecipeBuilder or fix the id",
            )
        })?;
        let recipe = &self.project.recipes[*idx];
        let want_type = if expected_type.contains(':') {
            expected_type.to_string()
        } else {
            format!("minecraft:{expected_type}")
        };
        let got_type = recipe.recipe_type(&self.project.mod_id);
        if got_type != want_type {
            return Err(mk(
                "E_ASSERT_RECIPE",
                format!("recipe {qid}.type"),
                format!("expected type '{want_type}', found '{got_type}'"),
                "use the matching RecipeBuilder constructor",
            ));
        }
        let want_result = normalize_ref(expected_result, "assert_recipe_matches.result")?;
        if recipe.result_item() != want_result {
            return Err(mk(
                "E_ASSERT_RECIPE",
                format!("recipe {qid}.result"),
                format!(
                    "expected result '{want_result}', found '{}'",
                    recipe.result_item()
                ),
                "fix the recipe's result id",
            ));
        }
        Ok(())
    }

    /// Assert tag `tag_id` (item or block kind; bare ids resolve in the
    /// mod namespace) transitively contains `member` — following `#`
    /// nested-tag references.
    ///
    /// # Errors
    ///
    /// `E_ASSERT_TAG` diagnostic when the tag is missing or `member` is
    /// unreachable.
    pub fn assert_tag_contains(
        &self,
        tag_id: &str,
        member: &str,
    ) -> Result<(), SdkDiagnostic> {
        let qid = if tag_id.contains(':') {
            tag_id.to_string()
        } else {
            format!("{}:{tag_id}", self.project.mod_id)
        };
        let member = normalize_ref(member, "assert_tag_contains.member")?;
        if !self.tag_values.contains_key(&qid) {
            return Err(mk(
                "E_ASSERT_TAG",
                format!("tag {qid}"),
                format!("no tag declared with id '{qid}'"),
                "declare it via TagBuilder::item/block or .tag(...)",
            ));
        }
        let mut stack = vec![qid.clone()];
        let mut seen = BTreeSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Some(values) = self.tag_values.get(&id) else {
                continue;
            };
            for v in values {
                if *v == member {
                    return Ok(());
                }
                if let Some(nested) = v.strip_prefix('#') {
                    stack.push(nested.to_string());
                }
            }
        }
        Err(mk(
            "E_ASSERT_TAG",
            format!("tag {qid}"),
            format!("member '{member}' not reachable through '{qid}'"),
            "add the member to the tag or check nested-tag refs",
        ))
    }

    /// Assert the project's emitted `megu-mod-bundle.json` parses and
    /// satisfies the `megu-mod-bundle/1` schema checks end-to-end.
    ///
    /// # Errors
    ///
    /// Every [`SdkDiagnostic`] found in emission or schema validation.
    pub fn assert_bundle_valid(&self) -> Result<(), Vec<SdkDiagnostic>> {
        let text = match self.project.export_megu_bundle_json() {
            Ok(t) => t,
            Err(e) => {
                return Err(vec![mk(
                    "E_ASSERT_BUNDLE",
                    "bundle",
                    format!("bundle emission failed: {e}"),
                    "fix the reported diagnostics first",
                )]);
            }
        };
        let diags = validate_bundle_text(&text);
        if diags.is_empty() {
            Ok(())
        } else {
            Err(diags)
        }
    }

    /// Assert the packed `.mmod` verifies end-to-end: EOCD/CD/LFH walk,
    /// every member name validated, every CRC-32 recomputed, and the
    /// required entries (`megu-mod.toml`, `megu-mod-bundle.json`) present
    /// with byte-identical payloads.
    ///
    /// # Errors
    ///
    /// `E_ASSERT_JAR` diagnostics for any structural or content failure.
    pub fn assert_jar_roundtrip(&self) -> Result<(), Vec<SdkDiagnostic>> {
        let mut diags = Vec::new();
        let bytes = match self.project.export_mmod_bytes() {
            Ok(b) => b,
            Err(e) => {
                diags.push(mk(
                    "E_ASSERT_JAR",
                    "mmod",
                    format!("pack failed: {e}"),
                    "fix the reported diagnostics first",
                ));
                return Err(diags);
            }
        };
        let entries = match verify_stored_zip(&bytes) {
            Ok(e) => e,
            Err(e) => {
                diags.push(mk(
                    "E_ASSERT_JAR",
                    "mmod",
                    format!("archive verification failed: {e}"),
                    "inspect pack_zip_stored output",
                ));
                return Err(diags);
            }
        };
        for required in [MMOD_MANIFEST, BUNDLE_ENTRY] {
            if !entries.iter().any(|e| e.name == required) {
                diags.push(mk(
                    "E_ASSERT_JAR",
                    format!("mmod:{required}"),
                    format!("required member '{required}' missing"),
                    "check archive_files emission",
                ));
            }
        }
        if diags.is_empty() {
            Ok(())
        } else {
            Err(diags)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::test_support::sample_project;
    use crate::builders::*;

    fn mutate(project: &ModProject, f: impl FnOnce(&mut ModProject)) -> ModProject {
        let mut p = project.clone();
        f(&mut p);
        p
    }

    #[test]
    fn clean_project_passes_all_assertions() {
        let project = sample_project();
        let harness = ModTestHarness::from_project(&project).expect("valid project");
        harness
            .assert_bundle_valid()
            .expect("bundle schema valid");
        harness
            .assert_jar_roundtrip()
            .expect("mmod round-trips");
        harness
            .assert_recipe_matches(
                "brass_block",
                "minecraft:crafting_shaped",
                "horizon_demo:brass_block",
            )
            .expect("shaped recipe matches");
        harness
            .assert_recipe_matches(
                "horizon_demo:brass_sheet",
                "horizon_demo:pressing",
                "horizon_demo:brass_sheet",
            )
            .expect("pressing recipe matches");
        harness
            .assert_tag_contains("c:ingots/brass", "horizon_demo:brass_ingot")
            .expect("direct member found");
        harness
            .assert_tag_contains("c:ores/brass", "#c:ores")
            .expect("nested tag member found");
        harness
            .assert_tag_contains("minecraft:mineable/pickaxe", "horizon_demo:sky_forge")
            .expect("membership block tag found");
    }

    #[test]
    fn duplicate_ids_are_caught() {
        let project = mutate(&sample_project(), |p| {
            p.items.push(p.items[0].clone());
        });
        let diags = ModTestHarness::from_project(&project).unwrap_err();
        assert!(diags.iter().any(|d| d.code == "E_DUPLICATE_ID"));
    }

    #[test]
    fn bad_mod_ids_are_caught() {
        for bad in ["BAD_MOD", "my mod", "../evil", "", "mod/../x"] {
            let project = mutate(&sample_project(), |p| p.mod_id = bad.to_string());
            let diags = ModTestHarness::from_project(&project).unwrap_err();
            assert!(
                diags.iter().any(|d| d.code == "E_MOD_ID"),
                "{bad:?} should fail: {diags:?}"
            );
        }
    }

    #[test]
    fn missing_recipe_outputs_are_caught() {
        let project = mutate(&sample_project(), |p| {
            p.recipes.push(RecipeSpec {
                id: "horizon_demo:ghost_recipe".to_string(),
                kind: RecipeKind::Shapeless {
                    ingredients: vec!["horizon_demo:brass_ingot".to_string()],
                    result: crate::builders::ItemResult {
                        item: "horizon_demo:nonexistent_item".to_string(),
                        count: 1,
                    },
                },
            });
        });
        let diags = ModTestHarness::from_project(&project).unwrap_err();
        assert!(
            diags
                .iter()
                .any(|d| d.code == "E_RECIPE_REF" && d.message.contains("nonexistent_item")),
            "{diags:?}"
        );
    }

    #[test]
    fn missing_recipe_ingredient_tags_are_caught() {
        let project = mutate(&sample_project(), |p| {
            p.recipes.push(RecipeSpec {
                id: "horizon_demo:bad_tag_recipe".to_string(),
                kind: RecipeKind::Smelting {
                    input: "#horizon_demo:undeclared_tag".to_string(),
                    result: crate::builders::ItemResult {
                        item: "horizon_demo:brass_ingot".to_string(),
                        count: 1,
                    },
                    experience: 0.1,
                    cook_ticks: 100,
                },
            });
        });
        let diags = ModTestHarness::from_project(&project).unwrap_err();
        assert!(diags.iter().any(|d| d.code == "E_RECIPE_REF"));
    }

    #[test]
    fn tag_cycles_are_caught() {
        let project = mutate(&sample_project(), |p| {
            p.tags.push(TagSpec {
                kind: TagKind::Item,
                id: "horizon_demo:loop_a".to_string(),
                values: vec!["#horizon_demo:loop_b".to_string()],
                replace: false,
            });
            p.tags.push(TagSpec {
                kind: TagKind::Item,
                id: "horizon_demo:loop_b".to_string(),
                values: vec!["#horizon_demo:loop_a".to_string()],
                replace: false,
            });
        });
        let diags = ModTestHarness::from_project(&project).unwrap_err();
        assert!(
            diags.iter().any(|d| d.code == "E_TAG_CYCLE"),
            "{diags:?}"
        );
    }

    #[test]
    fn capability_bounds_are_caught() {
        let project = mutate(&sample_project(), |p| {
            p.blocks[0].capability = Some(CapabilitySpec::ItemHandler { slots: 0 });
        });
        let diags = ModTestHarness::from_project(&project).unwrap_err();
        assert!(diags.iter().any(|d| d.code == "E_CAPABILITY"));
        let project = mutate(&sample_project(), |p| {
            p.blocks[1].capability = Some(CapabilitySpec::EnergyStorage {
                capacity_fe: 100,
                max_io_fe: 200,
            });
        });
        let diags = ModTestHarness::from_project(&project).unwrap_err();
        assert!(diags.iter().any(|d| d.code == "E_CAPABILITY"));
    }

    #[test]
    fn builder_rejects_bad_input_early() {
        assert!(ModProjectBuilder::new("Bad Mod", "1.0").build().is_err());
        assert!(ModProjectBuilder::new("ok_mod", "")
            .build()
            .is_err());
        let diags = ModProjectBuilder::new("ok_mod", "1.0")
            .item(ItemBuilder::new("thing").max_stack(0))
            .build()
            .unwrap_err();
        assert!(diags.iter().any(|d| d.code == "E_RANGE"));
        let diags = ModProjectBuilder::new("ok_mod", "1.0")
            .block(BlockBuilder::new("glow").light_level(16))
            .build()
            .unwrap_err();
        assert!(diags.iter().any(|d| d.code == "E_RANGE"));
        let diags = ModProjectBuilder::new("ok_mod", "1.0")
            .creature(CreatureBuilder::new("pet").growth_stages(9))
            .build()
            .unwrap_err();
        assert!(diags.iter().any(|d| d.code == "E_RANGE"));
        let diags = ModProjectBuilder::new("ok_mod", "1.0")
            .dimension(DimensionBuilder::new("void").min_y(7))
            .build()
            .unwrap_err();
        assert!(diags.iter().any(|d| d.code == "E_DIMENSION"));
    }

    #[test]
    fn bundle_json_has_exactly_the_accepted_keys() {
        let project = sample_project();
        let text = project.export_megu_bundle_json().unwrap();
        let root = parse_json_doc(&text).unwrap();
        let keys: Vec<&str> = root
            .obj_pairs()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        let mut want = vec![
            "schema",
            "mod_id",
            "mc_version_range",
            "portals",
            "progression_barriers",
            "biome_regions",
            "creatures",
            "custom_channels",
        ];
        want.sort_unstable();
        assert_eq!(keys, want);
        assert!(validate_bundle_text(&text).is_empty());
        // The creature section carries the declared drake.
        assert!(text.contains("\"entity_type\": \"horizon_demo:sky_drake\""));
        assert!(text.contains("\"kind\": \"gliding_flight\""));
        // Extended fields stay OUT of the strict bundle (registry covers them).
        assert!(!text.contains("elemental_breath"));
        assert!(!text.contains("growth_stages"));
        // Determinism.
        assert_eq!(text, project.export_megu_bundle_json().unwrap());
    }

    #[test]
    fn diagnostics_serialize_to_json() {
        let project = mutate(&sample_project(), |p| p.mod_id = "BAD".to_string());
        let diags = validate_project(&project);
        assert!(!diags.is_empty());
        let json = diagnostics_to_json(&diags);
        assert!(json.contains("\"code\": \"E_MOD_ID\""));
        let single = diags[0].to_json();
        assert!(single.contains("\"fix_hint\""));
        assert!(single.contains("\"path\""));
    }

    #[test]
    fn assert_failures_report_structured_diagnostics() {
        let project = sample_project();
        let harness = ModTestHarness::from_project(&project).unwrap();
        let d = harness
            .assert_recipe_matches("brass_block", "minecraft:smelting", "x:y")
            .unwrap_err();
        assert_eq!(d.code, "E_ASSERT_RECIPE");
        let d = harness
            .assert_tag_contains("c:ingots/brass", "horizon_demo:missing")
            .unwrap_err();
        assert_eq!(d.code, "E_ASSERT_TAG");
    }
}
