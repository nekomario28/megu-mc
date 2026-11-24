// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Engine-standard recipe conflict auto-resolver and opt-in tag material
//! unifier (`XMOD1-CORE`).
//!
//! - [`RecipeConflictIndex`] fixes Vanilla's silent recipe-collision bug:
//!   when two loaded mods (or a datapack) register different outputs for
//!   the same crafting/machine input signature, Vanilla picks one
//!   arbitrarily and the other is unreachable. The index keeps every
//!   [`RecipeCandidate`] per normalized [`InputSignature`] key in a
//!   deterministic order (`minecraft:` first, then `recipe_id`
//!   ascending) so a `< i/N >` selector can cycle candidates without the
//!   order ever flickering between sessions.
//! - [`MaterialUnifier`] is a strictly **opt-in** convenience layer
//!   (`enabled: false` by default — strict Vanilla parity) that maps any
//!   item in a shared conventional material tag (`#c:ingots/*`,
//!   `#c:nuggets/*`, `#c:dusts/*`, `#c:plates/*`, `#c:gears/*`,
//!   `#c:ores/*`, `#c:raw_materials/*`, `#c:gems/*`, `#c:rods/*`,
//!   `#c:storage_blocks/*` — with `#forge:*`/`#neoforge:*` spellings
//!   canonicalized by [`crate::unified_tags`]) onto one deterministic
//!   representative item chosen by operator-configured namespace
//!   priority, then lexicographically. An explicit `excluded_items` set
//!   protects quest-unique or otherwise special items from ever being
//!   rewritten.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::unified_tags::{canonical_common_tag_id, TagDomain, UnifiedTagIndex};

/// Conventional material tag families (`c:<family>/<material>`) eligible
/// for unification, in canonical search order. The `c:` prefix is the
/// NeoForge 1.21.1 spelling the [`UnifiedTagIndex`] canonicalizes
/// `forge:`/`neoforge:` onto, so legacy files are covered transparently.
pub const MATERIAL_TAG_FAMILIES: &[&str] = &[
    "ingots",
    "nuggets",
    "dusts",
    "plates",
    "gears",
    "ores",
    "raw_materials",
    "gems",
    "rods",
    "storage_blocks",
];

/// Normalize an item id onto canonical `ns:path` form (bare paths gain
/// the `minecraft:` namespace, mirroring tag-member normalization).
fn normalize_item_id(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains(':') {
        raw.to_string()
    } else {
        format!("minecraft:{raw}")
    }
}

/// Canonical namespace of a normalized `ns:item` id (the part before the
/// first `:`; empty for malformed ids).
fn namespace_of(item_id: &str) -> &str {
    item_id.split_once(':').map_or(item_id, |(ns, _)| ns)
}

// ---------------------------------------------------------------------------
// Input signatures
// ---------------------------------------------------------------------------

/// Builders for normalized crafting/machine input signature keys. A
/// signature is the deterministic string two recipes compare equal when
/// they compete for the same inputs; callers feed them to
/// [`RecipeConflictIndex::register`] / [`RecipeConflictIndex::resolve`].
pub struct InputSignature;

impl InputSignature {
    /// Position-preserving shaped-grid key. `slots` iterates the grid in
    /// row-major order; empty cells become empty segments so
    /// `[Some(a), None]` and `[None, Some(a)]` stay distinct (shaped
    /// recipes are placement-sensitive).
    pub fn shaped<'a>(slots: impl IntoIterator<Item = Option<&'a str>>) -> String {
        let mut key = String::from("shaped:");
        for (i, slot) in slots.into_iter().enumerate() {
            if i > 0 {
                key.push('|');
            }
            if let Some(item) = slot {
                key.push_str(&normalize_item_id(item));
            }
        }
        key
    }

    /// Order-independent shapeless key: item ids normalized, sorted, and
    /// counted as a multiset, so ingredient order never splits a
    /// signature.
    pub fn shapeless<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
        let mut sorted: Vec<String> = items.into_iter().map(normalize_item_id).collect();
        sorted.sort();
        format!("shapeless:{}", sorted.join("+"))
    }

    /// Normalized tag-input key. The tag id is canonicalized through
    /// [`canonical_common_tag_id`], so `#forge:ingots/iron` and
    /// `#c:ingots/iron` collide onto the same signature exactly the way
    /// the merged tag index resolves them.
    pub fn tagged(tag_id: &str) -> String {
        format!("tag:#{}", canonical_common_tag_id(tag_id))
    }

    /// Machine-input key binding a normalized `ns:path` process id
    /// (`"minecraft:smelting"`, `"some_mod:crushing"`, …) to one
    /// normalized input item id.
    pub fn machine(process: &str, input: &str) -> String {
        format!(
            "machine:{}|{}",
            normalize_item_id(process),
            normalize_item_id(input)
        )
    }
}

// ---------------------------------------------------------------------------
// Recipe conflict index
// ---------------------------------------------------------------------------

/// One registered recipe competing for an input signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipeCandidate {
    /// Canonical recipe id (`"minecraft:iron_pickaxe"`,
    /// `"some_mod:crushed_iron_recipe"`, …).
    pub recipe_id: String,
    /// Canonical produced item id.
    pub output_item: String,
    /// Produced item count.
    pub output_count: u16,
    /// Mod namespace that supplied the recipe.
    pub mod_namespace: String,
}

/// Deterministic sort key: vanilla (`minecraft:`-supplied or
/// `minecraft:`-named) candidates first, then `recipe_id` ascending,
/// then `output_item` so equal-named recipes still order stably.
fn candidate_key(c: &RecipeCandidate) -> (u8, &str, &str) {
    let vanilla = c.mod_namespace == "minecraft" || c.recipe_id.starts_with("minecraft:");
    (
        if vanilla { 0 } else { 1 },
        c.recipe_id.as_str(),
        c.output_item.as_str(),
    )
}

/// Do the candidates produce more than one distinct output? A signature
/// where every candidate makes the same `(output_item, output_count)`
/// is redundant, not conflicting.
fn has_distinct_outputs(candidates: &[RecipeCandidate]) -> bool {
    candidates
        .iter()
        .map(|c| (c.output_item.as_str(), c.output_count))
        .collect::<BTreeSet<_>>()
        .len()
        > 1
}

/// Borrowed view of one resolved candidate for a signature — what a
/// `< index/total >` selector row needs.
#[derive(Clone, Copy, Debug)]
pub struct ResolvedRecipeChoice<'a> {
    /// The selected candidate.
    pub selected: &'a RecipeCandidate,
    /// Effective index after wrap-around (`selected_index % total`).
    pub index: usize,
    /// Total candidates registered for the signature.
    pub total_candidates: usize,
    /// `true` when the candidates disagree on output.
    pub has_conflict: bool,
}

/// Input-signature → ordered candidates map powering the `< i/N >`
/// recipe-conflict selector. Always-on: registration is unconditional
/// and `resolve` is a pure lookup, so Vanilla's silent collision becomes
/// a visible, cycleable choice.
#[derive(Debug, Default)]
pub struct RecipeConflictIndex {
    by_signature: BTreeMap<String, Vec<RecipeCandidate>>,
}

impl RecipeConflictIndex {
    /// Empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `candidate` under `signature`, keeping the bucket in the
    /// deterministic [`candidate_key`] order regardless of registration
    /// order.
    pub fn register(&mut self, signature: impl Into<String>, candidate: RecipeCandidate) {
        let bucket = self.by_signature.entry(signature.into()).or_default();
        bucket.push(candidate);
        bucket.sort_by(|a, b| candidate_key(a).cmp(&candidate_key(b)));
    }

    /// Borrow the ordered candidate bucket for `signature`, if any.
    pub fn candidates(&self, signature: &str) -> Option<&[RecipeCandidate]> {
        self.by_signature.get(signature).map(Vec::as_slice)
    }

    /// Number of distinct signatures registered.
    pub fn signature_count(&self) -> usize {
        self.by_signature.len()
    }

    /// Resolve the `< selected_index/total >` choice for `signature`.
    /// `selected_index` wraps modulo the bucket size so `<`/`>` cycling
    /// never leaves the candidate range. `None` when no recipe is
    /// registered for the signature.
    pub fn resolve(
        &self,
        signature: &str,
        selected_index: usize,
    ) -> Option<ResolvedRecipeChoice<'_>> {
        let bucket = self.by_signature.get(signature)?;
        if bucket.is_empty() {
            return None;
        }
        let index = selected_index % bucket.len();
        Some(ResolvedRecipeChoice {
            selected: &bucket[index],
            index,
            total_candidates: bucket.len(),
            has_conflict: has_distinct_outputs(bucket),
        })
    }

    /// How many registered signatures have more than one distinct
    /// `(output_item, output_count)` — the real collision count.
    pub fn conflict_count(&self) -> usize {
        self.by_signature
            .values()
            .filter(|bucket| has_distinct_outputs(bucket))
            .count()
    }

    /// `(signature, candidates)` for every signature whose candidates
    /// disagree on output, in sorted signature order.
    pub fn list_conflicts(&self) -> Vec<(&str, &[RecipeCandidate])> {
        self.by_signature
            .iter()
            .filter(|(_, bucket)| has_distinct_outputs(bucket))
            .map(|(sig, bucket)| (sig.as_str(), bucket.as_slice()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Opt-in tag material unifier
// ---------------------------------------------------------------------------

/// Operator configuration for [`MaterialUnifier`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialUnifyConfig {
    /// Master switch — `false` by default for strict Vanilla parity:
    /// every `unify_item_id` call returns its input untouched.
    pub enabled: bool,
    /// Namespace preference order for the canonical representative: a
    /// member whose namespace appears earlier wins; unlisted namespaces
    /// rank last and ties break lexicographically. Default
    /// `["minecraft"]` keeps vanilla materials canonical.
    pub priority_namespaces: Vec<String>,
    /// Canonical `ns:item` ids never rewritten — quest rewards, unique
    /// or otherwise special items — and never chosen as representatives.
    pub excluded_items: BTreeSet<String>,
}

impl Default for MaterialUnifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            priority_namespaces: vec!["minecraft".to_string()],
            excluded_items: BTreeSet::new(),
        }
    }
}

/// Opt-in tag-driven material unifier. Maps every member of a shared
/// conventional material tag onto one deterministic representative so
/// four mods' steel ingots collapse onto a single canonical stack —
/// but only while `enabled`; disabled it is a zero-overhead passthrough.
pub struct MaterialUnifier {
    config: MaterialUnifyConfig,
    /// `item_id -> representative` memo (interior mutability keeps the
    /// public API `&self`; single-threaded, no locking).
    cache: RefCell<HashMap<String, String>>,
}

impl MaterialUnifier {
    /// Unifier over `config`.
    pub fn new(config: MaterialUnifyConfig) -> Self {
        Self {
            config,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Borrow the active configuration.
    pub fn config(&self) -> &MaterialUnifyConfig {
        &self.config
    }

    /// Drop the memoized mappings (after config edits or tag reloads).
    pub fn clear_cache(&self) {
        self.cache.borrow_mut().clear();
    }

    /// Map `item_id` onto its canonical material representative.
    ///
    /// - `enabled == false` or an excluded item → returns `item_id`
    ///   normalized but otherwise untouched (zero tag lookups).
    /// - Otherwise the item's material tag (`c:<family>/<material>` for
    ///   [`MATERIAL_TAG_FAMILIES`], `forge:` spellings already folded in
    ///   by the index) is resolved and its members ranked by
    ///   [`MaterialUnifyConfig::priority_namespaces`] then
    ///   lexicographically; the winner is returned and memoized.
    ///   Non-material items pass through unchanged.
    pub fn unify_item_id(&self, item_id: &str, tags: &UnifiedTagIndex) -> String {
        let item_id = normalize_item_id(item_id);
        if !self.config.enabled || self.config.excluded_items.contains(&item_id) {
            return item_id;
        }
        if let Some(hit) = self.cache.borrow().get(&item_id) {
            return hit.clone();
        }
        let resolved = self
            .canonical_representative(&item_id, tags)
            .unwrap_or_else(|| item_id.clone());
        self.cache.borrow_mut().insert(item_id.clone(), resolved.clone());
        resolved
    }

    /// First material tag (family order, then sorted tag id) containing
    /// `item_id`, if any.
    fn material_tag_of(&self, item_id: &str, tags: &UnifiedTagIndex) -> Option<String> {
        let ids = tags.tag_ids(TagDomain::Item);
        for family in MATERIAL_TAG_FAMILIES {
            let prefix = format!("c:{family}/");
            for tag_id in &ids {
                if tag_id.starts_with(&prefix) && tags.matches(TagDomain::Item, tag_id, item_id) {
                    return Some(tag_id.clone());
                }
            }
        }
        None
    }

    /// Highest-priority member of `item_id`'s material tag, skipping
    /// excluded ids; `None` when the item is in no material tag or the
    /// tag resolves empty/fails.
    fn canonical_representative(&self, item_id: &str, tags: &UnifiedTagIndex) -> Option<String> {
        let tag = self.material_tag_of(item_id, tags)?;
        let members = tags.resolve(TagDomain::Item, &tag).ok()?;
        members
            .into_iter()
            .filter(|member| !self.config.excluded_items.contains(member))
            .min_by_key(|member| (self.namespace_rank(member), member.clone()))
    }

    /// Lower is better: position inside `priority_namespaces`, or last.
    fn namespace_rank(&self, item_id: &str) -> usize {
        let ns = namespace_of(item_id);
        self.config
            .priority_namespaces
            .iter()
            .position(|priority| priority == ns)
            .unwrap_or(usize::MAX)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(recipe_id: &str, output_item: &str, output_count: u16, ns: &str) -> RecipeCandidate {
        RecipeCandidate {
            recipe_id: recipe_id.to_string(),
            output_item: output_item.to_string(),
            output_count,
            mod_namespace: ns.to_string(),
        }
    }

    #[test]
    fn three_way_conflict_orders_vanilla_first_and_cycles() {
        let mut index = RecipeConflictIndex::new();
        let sig = InputSignature::shapeless(["minecraft:iron_ingot", "minecraft:stick"]);
        // Register in hostile order: alphabetical would not put vanilla
        // first, and registration order must not matter.
        index.register(
            sig.clone(),
            candidate("mod_zeta:iron_wrench", "mod_zeta:iron_wrench", 1, "mod_zeta"),
        );
        index.register(
            sig.clone(),
            candidate("mod_alpha:iron_gear", "mod_alpha:iron_gear", 1, "mod_alpha"),
        );
        index.register(
            sig.clone(),
            candidate("minecraft:iron_shovel", "minecraft:iron_shovel", 1, "minecraft"),
        );

        let first = index.resolve(&sig, 0).expect("registered signature");
        assert!(first.has_conflict, "three distinct outputs conflict");
        assert_eq!(first.total_candidates, 3);
        assert_eq!(first.index, 0);
        assert_eq!(
            first.selected.recipe_id, "minecraft:iron_shovel",
            "vanilla recipe sorts first"
        );

        // Deterministic alphabetical tail after the vanilla candidate.
        let second = index.resolve(&sig, 1).expect("second candidate");
        assert_eq!(second.selected.recipe_id, "mod_alpha:iron_gear");
        let third = index.resolve(&sig, 2).expect("third candidate");
        assert_eq!(third.selected.recipe_id, "mod_zeta:iron_wrench");

        // Wrap-around cycling: < 1/3 > -> < 2/3 > -> < 3/3 > -> < 1/3 >.
        let wrapped = index.resolve(&sig, 3).expect("wraps");
        assert_eq!(wrapped.index, 0);
        assert_eq!(wrapped.selected.recipe_id, "minecraft:iron_shovel");
        let wrapped = index.resolve(&sig, 7).expect("wraps again");
        assert_eq!(wrapped.index, 1);
        assert_eq!(wrapped.selected.recipe_id, "mod_alpha:iron_gear");
    }

    #[test]
    fn non_conflicting_signature_and_unknown_signature() {
        let mut index = RecipeConflictIndex::new();
        let sig = InputSignature::shaped([Some("minecraft:oak_planks"), None]);
        // Two recipes, identical output -> redundant, not a conflict.
        index.register(
            sig.clone(),
            candidate("mod_a:crafting_table", "minecraft:crafting_table", 1, "mod_a"),
        );
        index.register(
            sig.clone(),
            candidate("minecraft:crafting_table", "minecraft:crafting_table", 1, "minecraft"),
        );

        let choice = index.resolve(&sig, 0).expect("registered");
        assert!(!choice.has_conflict, "same output is not a conflict");
        assert_eq!(choice.total_candidates, 2);
        assert_eq!(index.conflict_count(), 0);
        assert!(index.list_conflicts().is_empty());
        assert!(index.resolve("shaped:nothing|here", 0).is_none());
    }

    #[test]
    fn shaped_vs_shapeless_signatures_stay_distinct() {
        let shaped = InputSignature::shaped([Some("iron_ingot"), None, Some("iron_ingot")]);
        let moved = InputSignature::shaped([None, Some("iron_ingot"), Some("iron_ingot")]);
        assert_ne!(shaped, moved, "grid position matters for shaped keys");
        assert!(shaped.starts_with("shaped:"));
        assert!(shaped.contains("minecraft:iron_ingot"));

        let a = InputSignature::shapeless(["minecraft:iron_ingot", "minecraft:stick"]);
        let b = InputSignature::shapeless(["minecraft:stick", "minecraft:iron_ingot"]);
        assert_eq!(a, b, "shapeless signatures sort ingredients");

        assert_eq!(
            InputSignature::tagged("#forge:ingots/iron"),
            InputSignature::tagged("c:ingots/iron"),
            "forge: spelling canonicalizes onto the c: tag key"
        );
        assert_eq!(
            InputSignature::machine("smelting", "iron_ore"),
            "machine:minecraft:smelting|minecraft:iron_ore"
        );
    }

    fn steel_tag_index() -> UnifiedTagIndex {
        // Four mods contribute steel ingots; one file uses the legacy
        // forge: spelling, which the index folds onto c:ingots/steel.
        let mut tags = UnifiedTagIndex::new();
        tags.merge_values(
            TagDomain::Item,
            "c:ingots/steel",
            [
                "mod_gamma:steel_ingot",
                "mod_beta:steel_ingot",
                "minecraft:steel_ingot",
            ],
        );
        tags.merge_values(
            TagDomain::Item,
            "forge:ingots/steel",
            ["mod_delta:steel_ingot"],
        );
        tags
    }

    #[test]
    fn unifier_disabled_is_passthrough() {
        let unifier = MaterialUnifier::new(MaterialUnifyConfig::default());
        let tags = steel_tag_index();
        assert!(!unifier.config().enabled);
        assert_eq!(
            unifier.unify_item_id("mod_gamma:steel_ingot", &tags),
            "mod_gamma:steel_ingot",
            "disabled unifier must not rewrite"
        );
    }

    #[test]
    fn unifier_collapses_four_mods_to_one_canonical_steel() {
        let tags = steel_tag_index();
        // All four mod steels live under the merged c:ingots/steel tag.
        for item in [
            "mod_beta:steel_ingot",
            "mod_gamma:steel_ingot",
            "mod_delta:steel_ingot",
            "minecraft:steel_ingot",
        ] {
            assert!(
                tags.matches(TagDomain::Item, "c:ingots/steel", item),
                "{item} should resolve through the merged tag"
            );
        }

        // Default priority prefers minecraft: — the vanilla ingot wins.
        let unifier = MaterialUnifier::new(MaterialUnifyConfig {
            enabled: true,
            ..Default::default()
        });
        for item in [
            "mod_beta:steel_ingot",
            "mod_gamma:steel_ingot",
            "mod_delta:steel_ingot",
        ] {
            assert_eq!(
                unifier.unify_item_id(item, &tags),
                "minecraft:steel_ingot",
                "{item} unifies onto the canonical representative"
            );
        }

        // Operator override: mod_beta outranks everyone.
        let unifier = MaterialUnifier::new(MaterialUnifyConfig {
            enabled: true,
            priority_namespaces: vec!["mod_beta".to_string()],
            excluded_items: BTreeSet::new(),
        });
        for item in [
            "mod_gamma:steel_ingot",
            "mod_delta:steel_ingot",
            "minecraft:steel_ingot",
        ] {
            assert_eq!(
                unifier.unify_item_id(item, &tags),
                "mod_beta:steel_ingot",
                "priority_namespaces drives the representative"
            );
        }
    }

    #[test]
    fn unifier_respects_exclusions_and_non_materials() {
        let tags = steel_tag_index();
        let mut excluded = BTreeSet::new();
        excluded.insert("mod_gamma:steel_ingot".to_string());
        excluded.insert("minecraft:steel_ingot".to_string());
        let unifier = MaterialUnifier::new(MaterialUnifyConfig {
            enabled: true,
            // No priorities: pure lexicographic fallback. The
            // lexicographically smallest member (minecraft) is excluded,
            // so mod_beta becomes the representative — and mod_gamma is
            // excluded from ever *becoming* one as well.
            priority_namespaces: vec![],
            excluded_items: excluded,
        });

        // Excluded input passes through untouched.
        assert_eq!(
            unifier.unify_item_id("mod_gamma:steel_ingot", &tags),
            "mod_gamma:steel_ingot"
        );
        // Excluded items are also skipped as representatives:
        // minecraft and mod_gamma are both excluded, mod_beta wins
        // lexicographically over mod_delta.
        assert_eq!(
            unifier.unify_item_id("mod_delta:steel_ingot", &tags),
            "mod_beta:steel_ingot"
        );
        // Items outside every material family pass through.
        assert_eq!(
            unifier.unify_item_id("mod_delta:wrench", &tags),
            "mod_delta:wrench"
        );
        // Bare ids normalize before lookup.
        assert_eq!(
            unifier.unify_item_id("unrelated_block", &tags),
            "minecraft:unrelated_block"
        );
    }
}
