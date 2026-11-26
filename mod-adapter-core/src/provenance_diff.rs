// SPDX-License-Identifier: LGPL-3.0-or-later

//! Deterministic overwrite-provenance diff ledger.
//!
//! The ledger is deliberately domain-generic: loot tables, global loot
//! modifiers, recipes, tags, block models, and translations all append the
//! same small [`ProvenanceRecord`] shape.  Records are stored in load order
//! (which is deterministic for a deterministic layer scan), while conflict
//! and diff output is re-sorted through `BTreeMap` keys so exports never
//! depend on hash iteration order.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Resource family a provenance record belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiffDomain {
    LootTable,
    LootModifier,
    Recipe,
    Tag,
    BlockModel,
    Translation,
}

impl DiffDomain {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LootTable => "loot_table",
            Self::LootModifier => "loot_modifier",
            Self::Recipe => "recipe",
            Self::Tag => "tag",
            Self::BlockModel => "block_model",
            Self::Translation => "translation",
        }
    }
}

/// What one layer did to one resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProvenanceAction {
    Added,
    Overridden,
    ModifiedByGlm,
    MergedAppend,
    MergedReplace,
    Removed,
}

impl ProvenanceAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Overridden => "overridden",
            Self::ModifiedByGlm => "modified_by_glm",
            Self::MergedAppend => "merged_append",
            Self::MergedReplace => "merged_replace",
            Self::Removed => "removed",
        }
    }

    fn is_replace_conflict(self) -> bool {
        matches!(self, Self::Overridden | Self::MergedReplace)
    }
}

/// One layer's contribution to the history of one resource.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProvenanceRecord {
    pub domain: DiffDomain,
    pub resource_id: String,
    pub source_layer: String,
    pub action: ProvenanceAction,
    pub summary: String,
    pub previous_owner: Option<String>,
}

impl ProvenanceRecord {
    pub fn new(
        domain: DiffDomain,
        resource_id: impl Into<String>,
        source_layer: impl Into<String>,
        action: ProvenanceAction,
        summary: impl Into<String>,
        previous_owner: Option<String>,
    ) -> Self {
        Self {
            domain,
            resource_id: resource_id.into(),
            source_layer: source_layer.into(),
            action,
            summary: summary.into(),
            previous_owner,
        }
    }
}

/// A resource override/replace conflict between at least two non-vanilla
/// layers.  `layers` is sorted and deduplicated; `chain` remains in exact
/// load order for inspection UI expansion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceConflict {
    pub domain: DiffDomain,
    pub resource_id: String,
    pub layers: Vec<String>,
    pub actions: Vec<ProvenanceAction>,
    pub chain: Vec<ProvenanceRecord>,
}

/// Compact display badge for inspector tooltips.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvenanceBadge {
    pub text: String,
    pub action: ProvenanceAction,
    pub source_layer: String,
    pub previous_owner: Option<String>,
    pub chain: Vec<String>,
}

/// Counts of changes introduced in `self` relative to a previous snapshot.
/// All fields count distinct resources/conflicts, not duplicate records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReloadDiffSummary {
    pub added_recipes: usize,
    pub overridden_loot_tables: usize,
    pub active_glm_rules: usize,
    pub added_tags: usize,
    pub added_translations: usize,
    pub conflict_count: usize,
}

impl ReloadDiffSummary {
    pub fn is_empty(self) -> bool {
        self.added_recipes == 0
            && self.overridden_loot_tables == 0
            && self.active_glm_rules == 0
            && self.added_tags == 0
            && self.added_translations == 0
            && self.conflict_count == 0
    }
}

/// Append-only deterministic provenance chain store.
#[derive(Clone, Debug, Default)]
pub struct ProvenanceDiffLedger {
    records: Vec<ProvenanceRecord>,
    seen_records: BTreeSet<ProvenanceRecord>,
}

impl ProvenanceDiffLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one record. Exact duplicates are idempotently ignored so a
    /// reload scanner can re-ingest the same byte-identical layer without
    /// manufacturing a fake history loop.
    pub fn record(&mut self, record: ProvenanceRecord) {
        if self.seen_records.insert(record.clone()) {
            self.records.push(record);
        }
    }

    pub fn record_action(
        &mut self,
        domain: DiffDomain,
        resource_id: impl Into<String>,
        source_layer: impl Into<String>,
        action: ProvenanceAction,
        summary: impl Into<String>,
        previous_owner: Option<String>,
    ) {
        self.record(ProvenanceRecord::new(
            domain,
            resource_id,
            source_layer,
            action,
            summary,
            previous_owner,
        ));
    }

    pub fn records(&self) -> &[ProvenanceRecord] {
        &self.records
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Full ordered change chain for one resource (`Vanilla -> Mod A ->
    /// Mod B`, with GLM modifications interleaved in application order).
    pub fn chain(&self, domain: DiffDomain, resource_id: &str) -> Vec<&ProvenanceRecord> {
        self.records
            .iter()
            .filter(|r| r.domain == domain && r.resource_id == resource_id)
            .collect()
    }

    pub fn latest(&self, domain: DiffDomain, resource_id: &str) -> Option<&ProvenanceRecord> {
        self.records
            .iter()
            .rev()
            .find(|r| r.domain == domain && r.resource_id == resource_id)
    }

    /// Resources overridden or replace-merged by two or more distinct
    /// non-vanilla layers. GLM modifications are compositional by design and therefore
    /// do not count as replace conflicts by themselves.
    pub fn conflicts(&self) -> Vec<ResourceConflict> {
        let mut grouped: BTreeMap<(DiffDomain, String), Vec<&ProvenanceRecord>> = BTreeMap::new();
        for record in &self.records {
            grouped
                .entry((record.domain, record.resource_id.clone()))
                .or_default()
                .push(record);
        }

        let mut out = Vec::new();
        for ((domain, resource_id), chain_refs) in grouped {
            let mut layers = BTreeSet::new();
            let mut actions = BTreeSet::new();
            for record in &chain_refs {
                if record.action.is_replace_conflict() && !is_vanilla_layer(&record.source_layer) {
                    layers.insert(record.source_layer.clone());
                    actions.insert(record.action);
                }
            }
            if layers.len() >= 2 {
                out.push(ResourceConflict {
                    domain,
                    resource_id,
                    layers: layers.into_iter().collect(),
                    actions: actions.into_iter().collect(),
                    chain: chain_refs.into_iter().cloned().collect(),
                });
            }
        }
        out
    }

    /// Compact tooltip/inspector badge for the latest state of a resource.
    pub fn badge_for(&self, domain: DiffDomain, resource_id: &str) -> Option<ProvenanceBadge> {
        let chain = self.chain(domain, resource_id);
        let latest = chain.last().copied()?;
        let chain_text: Vec<String> = chain
            .iter()
            .map(|r| format!("{}:{}", r.source_layer, r.action.as_str()))
            .collect();
        let first_owner = chain
            .first()
            .map(|r| r.source_layer.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let text = match latest.action {
            ProvenanceAction::Added => format!("Added by {}", latest.source_layer),
            ProvenanceAction::Overridden => {
                format!("Overridden: {first_owner} -> {}", latest.source_layer)
            }
            ProvenanceAction::ModifiedByGlm => {
                let glm_count = chain
                    .iter()
                    .filter(|r| r.action == ProvenanceAction::ModifiedByGlm)
                    .count();
                format!("Modified by {} (+{glm_count} GLM)", latest.source_layer)
            }
            ProvenanceAction::MergedAppend => {
                format!("Merged (append) by {}", latest.source_layer)
            }
            ProvenanceAction::MergedReplace => {
                format!("Replaced: {first_owner} -> {}", latest.source_layer)
            }
            ProvenanceAction::Removed => format!("Removed by {}", latest.source_layer),
        };
        Some(ProvenanceBadge {
            text,
            action: latest.action,
            source_layer: latest.source_layer.clone(),
            previous_owner: latest.previous_owner.clone(),
            chain: chain_text,
        })
    }

    /// Count changes introduced in this ledger relative to `previous`.
    ///
    /// The counts are distinct-resource deltas: a snapshot reload that adds
    /// one recipe to an unchanged pack reports `added_recipes == 1`, while
    /// an unchanged resource already present in `previous` contributes zero.
    /// `active_glm_rules` counts rules active now that were not active in
    /// the prior snapshot; `conflict_count` counts newly appearing conflict
    /// resources (the conflict severity list itself remains available via
    /// [`Self::conflicts`]).
    pub fn diff_summary_against(&self, previous: &ProvenanceDiffLedger) -> ReloadDiffSummary {
        ReloadDiffSummary {
            added_recipes: self.newly_present_resources(previous, DiffDomain::Recipe),
            overridden_loot_tables: self.new_action_resources(
                previous,
                DiffDomain::LootTable,
                &[
                    ProvenanceAction::Overridden,
                    ProvenanceAction::MergedReplace,
                ],
            ),
            active_glm_rules: self.newly_active_glm_rules(previous),
            added_tags: self.newly_present_resources(previous, DiffDomain::Tag),
            added_translations: self.newly_present_resources(previous, DiffDomain::Translation),
            conflict_count: self.new_conflicts(previous),
        }
    }

    fn newly_present_resources(&self, previous: &Self, domain: DiffDomain) -> usize {
        let before = active_resources(previous, domain);
        active_resources(self, domain).difference(&before).count()
    }

    fn new_action_resources(
        &self,
        previous: &Self,
        domain: DiffDomain,
        actions: &[ProvenanceAction],
    ) -> usize {
        let before = resources_with_actions(previous, domain, actions);
        resources_with_actions(self, domain, actions)
            .difference(&before)
            .count()
    }

    fn newly_active_glm_rules(&self, previous: &Self) -> usize {
        let before = active_resources(previous, DiffDomain::LootModifier);
        active_resources(self, DiffDomain::LootModifier)
            .difference(&before)
            .count()
    }

    fn new_conflicts(&self, previous: &Self) -> usize {
        let before: BTreeSet<(DiffDomain, String)> = previous
            .conflicts()
            .into_iter()
            .map(|c| (c.domain, c.resource_id))
            .collect();
        self.conflicts()
            .into_iter()
            .map(|c| (c.domain, c.resource_id))
            .filter(|key| !before.contains(key))
            .count()
    }

    /// Deterministic JSON export for CLI and inspector surfaces. Object
    /// keys are emitted in fixed order; resource conflict arrays are sorted
    /// by their `BTreeMap` grouping keys rather than hash order.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"schema\":\"megu-provenance-diff/1\",\"records\":[");
        for (index, record) in self.records.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            write!(
                out,
                "{{\"domain\":{},\"resource_id\":{},\"source_layer\":{},\"action\":{},\"summary\":{},\"previous_owner\":{}}}",
                json_string(record.domain.as_str()),
                json_string(&record.resource_id),
                json_string(&record.source_layer),
                json_string(record.action.as_str()),
                json_string(&record.summary),
                match &record.previous_owner {
                    Some(owner) => json_string(owner),
                    None => "null".to_string(),
                }
            )
            .expect("writing to String cannot fail");
        }
        out.push_str("],\"conflicts\":[");
        for (index, conflict) in self.conflicts().iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            write!(
                out,
                "{{\"domain\":{},\"resource_id\":{},\"layers\":{},\"actions\":{}}}",
                json_string(conflict.domain.as_str()),
                json_string(&conflict.resource_id),
                json_string_array(&conflict.layers),
                json_string_array(
                    &conflict
                        .actions
                        .iter()
                        .map(|a| a.as_str().to_string())
                        .collect::<Vec<_>>()
                )
            )
            .expect("writing to String cannot fail");
        }
        out.push_str("]}");
        out
    }
}

fn active_resources(ledger: &ProvenanceDiffLedger, domain: DiffDomain) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut grouped: BTreeMap<&str, Vec<&ProvenanceRecord>> = BTreeMap::new();
    for record in ledger.records().iter().filter(|r| r.domain == domain) {
        grouped
            .entry(record.resource_id.as_str())
            .or_default()
            .push(record);
    }
    for (resource_id, chain) in grouped {
        if chain
            .last()
            .is_some_and(|r| r.action != ProvenanceAction::Removed)
        {
            out.insert(resource_id.to_string());
        }
    }
    out
}

fn resources_with_actions(
    ledger: &ProvenanceDiffLedger,
    domain: DiffDomain,
    actions: &[ProvenanceAction],
) -> BTreeSet<String> {
    ledger
        .records()
        .iter()
        .filter(|r| r.domain == domain && actions.contains(&r.action))
        .map(|r| r.resource_id.clone())
        .collect()
}

fn is_vanilla_layer(layer: &str) -> bool {
    let normalized = layer.trim().to_ascii_lowercase();
    normalized == "vanilla"
        || normalized == "minecraft"
        || normalized == "vanilla base"
        || normalized.starts_with("vanilla:")
}

fn json_string_array(values: &[String]) -> String {
    let mut out = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        out.push_str(&json_string(value));
    }
    out.push(']');
    out
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if c < '\u{20}' => {
                write!(out, "\\u{:04x}", u32::from(c)).expect("writing to String cannot fail");
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
