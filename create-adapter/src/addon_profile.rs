// SPDX-License-Identifier: LGPL-3.0-or-later

//! Universal Create addon extensibility: a declarative [`CreateAddonProfile`]
//! describing any Create companion mod's kinetic sources, consumers,
//! connectors, transport/fluid blocks, and 6-DOF propulsion blocks, plus
//! zero-JVM JAR/datapack auto-discovery so unknown addons offload without
//! hand-authored Rust code.
//!
//! Every profile carries two synchronized views of the same addon:
//! - `block_defs()`: flagship [`CreateBlockRole`] definitions that merge into
//!   the `CreateFactory` block registry next to the core `create:*` catalog.
//! - `offload`: a [`ModOffloadProfile`] mirror feeding the universal runtime
//!   (`UniversalModRuntime`) so transport/fluid/network pieces also work on
//!   the generic tier and in 4-tier fallback classification.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use mod_adapter_core::blockstates::{
    BlockDef, BlockRegistry, PropertyDef, RegistryError, UniversalBlockRole,
};
use mod_adapter_core::data_loader::{
    parse_json, DataLoadError, ItemFluidIdTable, JsonValue, LoadedModData,
};
use mod_adapter_core::profile::ModOffloadProfile;
use mod_adapter_core::version_compat::{
    check_mod_mc_version, McVersion, VersionCompatVerdict, VersionRange, VersionRangeError,
};
use mod_offload::recipe_processor::{Recipe, Resource, ResourceKind, Stack};

use crate::blockstates::{
    core_create_block_aliases, core_create_block_defs, CreateBlockRegistry, CreateBlockRole,
};
use crate::factory::Axis;

/// A single operation in a data-defined assembly recipe. The resource
/// identities are preserved so a caller can check consumed items and fluids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssemblyStepKind {
    Deploying { held_item: String },
    Pressing,
    Filling { fluid_id: String, amount_mb: u32 },
    Cutting,
    CustomStep(String),
}

/// An assembly whose sequence repeats `loops` times before selecting an output.
/// Result tuple fields are (item id, count, relative selection weight).
#[derive(Clone, Debug, PartialEq)]
pub struct SequencedAssemblyRecipe {
    pub id: String,
    pub ingredient_item: String,
    pub transitional_item: String,
    pub loops: u32,
    pub sequence: Vec<AssemblyStepKind>,
    pub results: Vec<(String, u32, f32)>,
}

/// One resolved machine ingredient: a concrete item or a `#`-tag reference
/// plus the count consumed per operation. Real 1.21.1 mod JARs declare both
/// (`{"item": "..."}`, `{"id": "..."}`, `{"tag": "c:ingots/iron"}`,
/// `neoforge:block_tag`, compound `neoforge:compound` alternatives).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessingIngredient {
    /// Item id (`ns:path`) or tag id (`ns:path`); `is_tag` distinguishes.
    pub id: String,
    /// Whether `id` names a `#`-tag rather than a concrete item.
    pub is_tag: bool,
    /// Item count consumed per operation (>= 1).
    pub count: u32,
}

impl ProcessingIngredient {
    fn item(id: &str) -> Self {
        Self {
            id: id.into(),
            is_tag: false,
            count: 1,
        }
    }

    /// Interning/display key: `"#<id>"` for tags, the bare id for items.
    /// The `#`-prefix matches Minecraft's tag-reference convention and can
    /// never collide with a namespaced item id.
    pub fn resource_key(&self) -> String {
        if self.is_tag {
            format!("#{}", self.id)
        } else {
            self.id.clone()
        }
    }
}

/// A single machine recipe decoded straight from `data/<ns>/recipe(s)/**`
/// JSON: every item/tag ingredient and fluid input in declaration order
/// (compound `neoforge:*` wrappers flattened to their first operand), the
/// chance-weighted item results, and any fluid outputs (`create:emptying`,
/// fluid-producing `create:mixing`). `recipe_type` is preserved verbatim so
/// unknown `<ns>:<machine>` serializers stay data-driven.
#[derive(Clone, Debug, PartialEq)]
pub struct MachineProcessingRecipe {
    pub id: String,
    /// Fully-namespaced recipe type (e.g. `"create:compacting"`,
    /// `"my_mod:pulverizing"`).
    pub recipe_type: String,
    /// Item & tag ingredients in declaration order.
    pub ingredients: Vec<ProcessingIngredient>,
    /// Fluid inputs `(fluid id or #tag, amount_mb)` in declaration order.
    pub fluid_inputs: Vec<(String, u32)>,
    /// Chance-weighted item results `(item id or #tag, count, weight)`.
    pub results: Vec<(String, u32, f32)>,
    /// Fluid outputs `(fluid id or #tag, amount_mb)` in declaration order.
    pub fluid_results: Vec<(String, u32)>,
}

/// Standalone pressing and fluid-spouting recipes share the assembly result
/// representation, including deterministic weighted output selection. The
/// machine families (real Create 1.21.1 `compacting`, `splashing`,
/// `haunting`, `deploying`, `emptying`, `item_application`,
/// `sandpaper_polishing`, `mechanical_crafting`, plus the generic
/// `<ns>:<machine>` fallback) carry the richer [`MachineProcessingRecipe`]
/// payload.
#[derive(Clone, Debug, PartialEq)]
pub enum AddonProcessingRecipe {
    SequencedAssembly(SequencedAssemblyRecipe),
    Pressing {
        id: String,
        ingredient_item: String,
        results: Vec<(String, u32, f32)>,
    },
    Mixing {
        id: String,
        ingredient_item: String,
        /// Fluid inputs `(fluid id or #tag, amount_mb)` declared via
        /// `fluid_stack`/`fluid`/`neoforge:*` operand shapes.
        fluid_inputs: Vec<(String, u32)>,
        results: Vec<(String, u32, f32)>,
        /// Fluid outputs `(fluid id or #tag, amount_mb)` — real 1.21.1
        /// mixing recipes produce `create:honey`/`create:tea`/`minecraft:lava`
        /// with no item result at all.
        fluid_results: Vec<(String, u32)>,
    },
    Milling {
        id: String,
        ingredient_item: String,
        results: Vec<(String, u32, f32)>,
    },
    Crushing {
        id: String,
        ingredient_item: String,
        results: Vec<(String, u32, f32)>,
    },
    Cutting {
        id: String,
        ingredient_item: String,
        results: Vec<(String, u32, f32)>,
    },
    Filling {
        id: String,
        ingredient_item: String,
        fluid_id: String,
        amount_mb: u32,
        results: Vec<(String, u32, f32)>,
    },
    /// Basin compaction (`create:compacting`), often with a fluid input.
    Compacting(MachineProcessingRecipe),
    /// Encased-fan bulk washing (`create:splashing`).
    Splashing(MachineProcessingRecipe),
    /// Encased-fan bulk haunting (`create:haunting`).
    Haunting(MachineProcessingRecipe),
    /// Deployer item application (`create:deploying`); the second
    /// `ingredients` entry is the held item.
    Deploying(MachineProcessingRecipe),
    /// Item drain (`create:emptying`); recovered fluid lands in
    /// [`MachineProcessingRecipe::fluid_results`].
    Emptying(MachineProcessingRecipe),
    /// Right-click item application (`create:item_application`).
    ItemApplication(MachineProcessingRecipe),
    /// Sandpaper polishing (`create:sandpaper_polishing`).
    SandpaperPolishing(MachineProcessingRecipe),
    /// Mechanical-crafter grid (`create:mechanical_crafting`): `key`/`pattern`
    /// usage counts are folded into [`MachineProcessingRecipe::ingredients`].
    MechanicalCrafting(MachineProcessingRecipe),
    /// Data-driven fallback for ANY other `<ns>:<machine>` recipe declaring
    /// standard `ingredients` + `results`/`result` fields.
    GenericMachine(MachineProcessingRecipe),
}

fn item_stack(ids: &mut ItemFluidIdTable, item_id: &str, amount: u32) -> Stack {
    Stack {
        resource: Resource {
            kind: ResourceKind::Item,
            id: ids.intern_item(item_id),
        },
        amount,
    }
}

fn fluid_stack(ids: &mut ItemFluidIdTable, fluid_id: &str, amount: u32) -> Stack {
    Stack {
        resource: Resource {
            kind: ResourceKind::Fluid,
            id: ids.intern_fluid(fluid_id),
        },
        amount,
    }
}

/// Merge a stack into a flat input/output list, summing amounts for repeats
/// of the same resource id (mirrors the generic loader's input folding).
fn merge_processor_stack(list: &mut Vec<Stack>, stack: Stack) {
    if let Some(existing) = list.iter_mut().find(|s| s.resource == stack.resource) {
        existing.amount = existing.amount.saturating_add(stack.amount);
    } else {
        list.push(stack);
    }
}

impl MachineProcessingRecipe {
    /// Flatten this machine recipe into the numeric [`Recipe`] consumed by
    /// `CreateFactory::add_processor`: every item/tag ingredient and fluid
    /// input becomes an input stack (repeat resources merged), chance-
    /// weighted item results collapse deterministically to the dominant one
    /// via [`dominant_result`], and fluid outputs are emitted as fluid
    /// stacks. Returns `None` when the recipe declares no usable outputs.
    pub fn to_processor_recipe(
        &self,
        ids: &mut ItemFluidIdTable,
        recipe_id: u32,
        base_ticks: u32,
    ) -> Option<Recipe> {
        if self.ingredients.is_empty() && self.fluid_inputs.is_empty() {
            return None;
        }
        let mut inputs = Vec::with_capacity(self.ingredients.len() + self.fluid_inputs.len());
        for ingredient in &self.ingredients {
            merge_processor_stack(
                &mut inputs,
                item_stack(ids, &ingredient.resource_key(), ingredient.count),
            );
        }
        for (fluid_id, amount_mb) in &self.fluid_inputs {
            merge_processor_stack(&mut inputs, fluid_stack(ids, fluid_id, *amount_mb));
        }
        let mut outputs = Vec::new();
        if let Some((result_item, result_count, _)) = dominant_result(&self.results) {
            outputs.push(item_stack(ids, result_item, result_count));
        }
        for (fluid_id, amount_mb) in &self.fluid_results {
            merge_processor_stack(&mut outputs, fluid_stack(ids, fluid_id, *amount_mb));
        }
        if outputs.is_empty() {
            return None;
        }
        Some(Recipe {
            id: recipe_id,
            base_ticks,
            inputs,
            outputs,
        })
    }
}

impl AddonProcessingRecipe {
    /// Convert a single-step processing recipe into the numeric [`Recipe`]
    /// consumed by `CreateFactory::add_processor`, interning item/fluid ids
    /// through `ids`. Weighted alternative results collapse deterministically
    /// to the highest-weight entry (last declared wins ties; the parsers
    /// already guarantee at least one positive weight). Returns `None` for
    /// [`Self::SequencedAssembly`]: its transitional multi-step semantics are
    /// driven by [`SequencedAssemblyState`] / [`advance_assembly_step`], not
    /// by the flat recipe processor.
    pub fn to_processor_recipe(
        &self,
        ids: &mut ItemFluidIdTable,
        recipe_id: u32,
        base_ticks: u32,
    ) -> Option<Recipe> {
        match self {
            Self::Compacting(recipe)
            | Self::Splashing(recipe)
            | Self::Haunting(recipe)
            | Self::Deploying(recipe)
            | Self::Emptying(recipe)
            | Self::ItemApplication(recipe)
            | Self::SandpaperPolishing(recipe)
            | Self::MechanicalCrafting(recipe)
            | Self::GenericMachine(recipe) => {
                return recipe.to_processor_recipe(ids, recipe_id, base_ticks);
            }
            Self::Mixing {
                ingredient_item,
                fluid_inputs,
                results,
                fluid_results,
                ..
            } => {
                let mut inputs = Vec::with_capacity(1 + fluid_inputs.len());
                merge_processor_stack(&mut inputs, item_stack(ids, ingredient_item, 1));
                for (fluid_id, amount_mb) in fluid_inputs {
                    merge_processor_stack(&mut inputs, fluid_stack(ids, fluid_id, *amount_mb));
                }
                let mut outputs = Vec::new();
                if let Some((result_item, result_count, _)) = dominant_result(results) {
                    outputs.push(item_stack(ids, result_item, result_count));
                }
                for (fluid_id, amount_mb) in fluid_results {
                    merge_processor_stack(&mut outputs, fluid_stack(ids, fluid_id, *amount_mb));
                }
                if outputs.is_empty() {
                    return None;
                }
                return Some(Recipe {
                    id: recipe_id,
                    base_ticks,
                    inputs,
                    outputs,
                });
            }
            _ => {}
        }
        let (ingredient_item, fluid_id, amount_mb, results) = match self {
            Self::SequencedAssembly(_) => return None,
            Self::Pressing {
                ingredient_item,
                results,
                ..
            }
            | Self::Milling {
                ingredient_item,
                results,
                ..
            }
            | Self::Crushing {
                ingredient_item,
                results,
                ..
            }
            | Self::Cutting {
                ingredient_item,
                results,
                ..
            } => (
                ingredient_item.as_str(),
                None,
                0,
                results.as_slice(),
            ),
            Self::Filling {
                ingredient_item,
                fluid_id,
                amount_mb,
                results,
                ..
            } => (
                ingredient_item.as_str(),
                Some(fluid_id.as_str()),
                *amount_mb,
                results.as_slice(),
            ),
            _ => return None,
        };
        let (result_item, result_count, _) = dominant_result(results)?;
        let mut inputs = vec![Stack {
            resource: Resource {
                kind: ResourceKind::Item,
                id: ids.intern_item(ingredient_item),
            },
            amount: 1,
        }];
        if let Some(fluid_id) = fluid_id {
            inputs.push(Stack {
                resource: Resource {
                    kind: ResourceKind::Fluid,
                    id: ids.intern_fluid(fluid_id),
                },
                amount: amount_mb,
            });
        }
        Some(Recipe {
            id: recipe_id,
            base_ticks,
            inputs,
            outputs: vec![Stack {
                resource: Resource {
                    kind: ResourceKind::Item,
                    id: ids.intern_item(result_item),
                },
                amount: result_count,
            }],
        })
    }
}

/// Highest-weight result entry; `None` when the list is empty or carries no
/// positive weight (parsers reject that shape already — this is a guard).
fn dominant_result(results: &[(String, u32, f32)]) -> Option<(&str, u32, f32)> {
    results
        .iter()
        .filter(|(_, _, weight)| weight.is_finite() && *weight > 0.0)
        .max_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(item, count, weight)| (item.as_str(), *count, *weight))
}

/// Resumable position in one assembly. The item changes to the transitional
/// item on the first accepted step and stays there until the final output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequencedAssemblyState {
    pub recipe_id: String,
    pub current_item: String,
    pub completed_loops: u32,
    pub next_step: usize,
    pub finished: bool,
}

impl SequencedAssemblyState {
    pub fn new(recipe: &SequencedAssemblyRecipe) -> Self {
        Self {
            recipe_id: recipe.id.clone(),
            current_item: recipe.ingredient_item.clone(),
            completed_loops: 0,
            next_step: 0,
            finished: false,
        }
    }
}

/// A rejected step leaves the state unchanged, allowing a correct retry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssemblyOutcome {
    InProgress {
        current_item: String,
        completed_loops: u32,
        next_step: usize,
    },
    Completed {
        item: String,
        count: u32,
    },
    Rejected {
        reason: String,
    },
}

/// Advance exactly one step. `rng_roll` is consumed only for the last step,
/// and must lie in `[0, 1)` at that point. Results use their declared weights
/// normalized to their sum, with zero-weight results never selected.
pub fn advance_assembly_step(
    state: &mut SequencedAssemblyState,
    recipe: &SequencedAssemblyRecipe,
    applied_step: &AssemblyStepKind,
    rng_roll: f32,
) -> AssemblyOutcome {
    let valid_position = state.recipe_id == recipe.id
        && !state.finished
        && (1..=64).contains(&recipe.loops)
        && (1..=32).contains(&recipe.sequence.len())
        && state.completed_loops < recipe.loops
        && state.next_step < recipe.sequence.len()
        && state.current_item.as_str()
            == if state.completed_loops == 0 && state.next_step == 0 {
                recipe.ingredient_item.as_str()
            } else {
                recipe.transitional_item.as_str()
            };
    if !valid_position {
        return AssemblyOutcome::Rejected {
            reason: "invalid or completed assembly state".into(),
        };
    }
    if recipe.sequence[state.next_step] != *applied_step {
        return AssemblyOutcome::Rejected {
            reason: "assembly step out of order or wrong resource".into(),
        };
    }

    let final_step =
        state.completed_loops == recipe.loops - 1 && state.next_step == recipe.sequence.len() - 1;
    let selected = if final_step {
        if !(0.0..1.0).contains(&rng_roll) {
            return AssemblyOutcome::Rejected {
                reason: "final roll must be finite and in [0, 1)".into(),
            };
        }
        match select_assembly_result(&recipe.results, rng_roll) {
            Some(result) => Some(result),
            None => {
                return AssemblyOutcome::Rejected {
                    reason: "assembly results have invalid weights or counts".into(),
                };
            }
        }
    } else {
        None
    };

    if let Some((item, count)) = selected {
        state.current_item = item.clone();
        state.completed_loops = recipe.loops;
        state.next_step = recipe.sequence.len();
        state.finished = true;
        return AssemblyOutcome::Completed { item, count };
    }

    state.current_item = recipe.transitional_item.clone();
    state.next_step += 1;
    if state.next_step == recipe.sequence.len() {
        state.completed_loops += 1;
        state.next_step = 0;
    }
    AssemblyOutcome::InProgress {
        current_item: state.current_item.clone(),
        completed_loops: state.completed_loops,
        next_step: state.next_step,
    }
}

fn select_assembly_result(results: &[(String, u32, f32)], roll: f32) -> Option<(String, u32)> {
    if results.is_empty() || results.len() > 32 {
        return None;
    }
    let mut total = 0.0f64;
    for (_, count, weight) in results {
        if !(1..=64).contains(count) || !weight.is_finite() || *weight < 0.0 {
            return None;
        }
        total += f64::from(*weight);
    }
    if !(total > 0.0 && total.is_finite()) {
        return None;
    }
    let threshold = f64::from(roll) * total;
    let mut cumulative = 0.0;
    let mut last_positive = None;
    for (item, count, weight) in results {
        if *weight > 0.0 {
            last_positive = Some((item.clone(), *count));
            cumulative += f64::from(*weight);
            if threshold < cumulative {
                return last_positive;
            }
        }
    }
    // Guard against floating-point rounding at the upper endpoint.
    last_positive
}

/// Parse a namespaced `*:sequenced_assembly` JSON recipe with bounded counts,
/// a nonempty sequence, and explicit item/fluid operands for every step.
pub fn parse_sequenced_assembly_json(
    id: &str,
    json: &str,
) -> Result<SequencedAssemblyRecipe, String> {
    let root = parse_json(json.as_bytes()).map_err(|err| format!("{id}: {err:?}"))?;
    let obj = root
        .as_object()
        .ok_or_else(|| format!("{id}: recipe root must be an object"))?;
    parse_assembly_object(id, obj)
}

fn parse_assembly_object(
    id: &str,
    obj: &BTreeMap<String, JsonValue>,
) -> Result<SequencedAssemblyRecipe, String> {
    if processing_type(obj)? != "sequenced_assembly" {
        return Err(format!("{id}: expected a sequenced_assembly recipe"));
    }
    let ingredient = obj
        .get("ingredient")
        .ok_or("assembly ingredient is missing")?;
    // Real 1.21.1 assemblies may start from a tag (`{"tag": "c:plates/gold"}`),
    // stored in `#tag` resource-key form.
    let ingredient_item = match parse_ingredient_operand(ingredient)? {
        Some(RecipeOperand::Item(ingredient)) => ingredient.resource_key(),
        _ => return Err("assembly ingredient must be an item or tag".into()),
    };
    let transitional_item = parse_item_id(
        obj.get("transitionalItem")
            .or_else(|| obj.get("transitional_item"))
            .ok_or("assembly transitional item is missing")?,
    )?;
    // `loops` is optional in 1.21.1 (e.g. create:sturdy_sheet omits it).
    let loops = match obj.get("loops") {
        Some(value) => bounded_integer(Some(value), "loops", 1, 64)?,
        None => 1,
    };
    let sequence = obj
        .get("sequence")
        .and_then(JsonValue::as_array)
        .ok_or("assembly sequence must be an array")?;
    if !(1..=32).contains(&sequence.len()) {
        return Err("assembly sequence length must be in 1..=32".into());
    }
    let sequence = sequence
        .iter()
        .map(parse_assembly_step)
        .collect::<Result<Vec<_>, _>>()?;
    let results = parse_assembly_results(obj)?;
    Ok(SequencedAssemblyRecipe {
        id: id.into(),
        ingredient_item,
        transitional_item,
        loops,
        sequence,
        results,
    })
}

fn processing_type(obj: &BTreeMap<String, JsonValue>) -> Result<&str, String> {
    let recipe_type = obj
        .get("type")
        .and_then(JsonValue::as_str)
        .ok_or("recipe type must be a string")?;
    let (namespace, name) = recipe_type
        .split_once(':')
        .ok_or("recipe type must be namespaced")?;
    if namespace.is_empty() || name.is_empty() {
        return Err("recipe type namespace and name must be nonempty".into());
    }
    Ok(name)
}

fn parse_item_id(value: &JsonValue) -> Result<String, String> {
    let id = match value {
        JsonValue::String(item) => Some(item.as_str()),
        JsonValue::Object(obj) => {
            let item = obj.get("item").or_else(|| obj.get("id"));
            match item {
                Some(JsonValue::String(id)) => Some(id.as_str()),
                Some(JsonValue::Object(inner)) => inner.get("id").and_then(JsonValue::as_str),
                _ => None,
            }
        }
        _ => None,
    }
    .filter(|id| !id.is_empty() && !id.trim().is_empty())
    .ok_or("expected a concrete item id")?;
    Ok(id.to_string())
}

fn bounded_integer(
    value: Option<&JsonValue>,
    field: &str,
    min: u32,
    max: u32,
) -> Result<u32, String> {
    let number = value
        .and_then(JsonValue::as_f64)
        .ok_or_else(|| format!("{field} must be an integer"))?;
    if number < f64::from(min) || number > f64::from(max) || number.fract() != 0.0 {
        return Err(format!("{field} must be in {min}..={max}"));
    }
    Ok(number as u32)
}

fn parse_assembly_step(value: &JsonValue) -> Result<AssemblyStepKind, String> {
    let obj = value.as_object().ok_or("assembly step must be an object")?;
    let step_type = processing_type(obj)?;
    match step_type {
        "deploying" => {
            let held = obj
                .get("held_item")
                .or_else(|| obj.get("heldItem"))
                .or_else(|| obj.get("ingredients")?.as_array()?.get(1))
                .ok_or("deploying step is missing held item")?;
            match parse_ingredient_operand(held)? {
                Some(RecipeOperand::Item(held)) => Ok(AssemblyStepKind::Deploying {
                    held_item: held.resource_key(),
                }),
                _ => Err("deploying held item must be a concrete item or tag".into()),
            }
        }
        "pressing" => Ok(AssemblyStepKind::Pressing),
        "filling" => {
            let (fluid_id, amount_mb) = parse_fluid_operand(obj)?;
            Ok(AssemblyStepKind::Filling {
                fluid_id,
                amount_mb,
            })
        }
        "cutting" => Ok(AssemblyStepKind::Cutting),
        _ => Ok(AssemblyStepKind::CustomStep(
            obj.get("type").and_then(JsonValue::as_str).unwrap().into(),
        )),
    }
}

fn parse_fluid_operand(obj: &BTreeMap<String, JsonValue>) -> Result<(String, u32), String> {
    // Object-level fluid keys (declarative shorthand schemas).
    if let Some(candidate) = obj.get("fluid_id").or_else(|| obj.get("fluid")) {
        let (fluid_id, count_obj) = match candidate {
            JsonValue::String(id) => (id.as_str(), obj),
            JsonValue::Object(o) => {
                let id = o
                    .get("fluid")
                    .or_else(|| o.get("id"))
                    .and_then(JsonValue::as_str)
                    .ok_or("filling fluid id must be a string")?;
                (id, o)
            }
            _ => return Err("filling fluid must be a string or object".into()),
        };
        if fluid_id.trim().is_empty() {
            return Err("filling fluid id must be nonempty".into());
        }
        let amount = bounded_integer(
            count_obj
                .get("amount_mb")
                .or_else(|| count_obj.get("amount"))
                .or_else(|| obj.get("amount_mb"))
                .or_else(|| obj.get("amount")),
            "fluid amount",
            1,
            u32::MAX,
        )?;
        return Ok((fluid_id.into(), amount));
    }
    // Ingredient-list fluid operands: legacy `{"fluid": ...}` plus the 1.21.1
    // `{"type": "fluid_stack"}` / `neoforge:single|tag|components|any` forms.
    if let Some(arr) = obj.get("ingredients").and_then(JsonValue::as_array) {
        for ingredient in arr {
            if let Some(RecipeOperand::Fluid(fluid_id, amount_mb)) =
                parse_ingredient_operand(ingredient)?
            {
                return Ok((fluid_id, amount_mb));
            }
        }
    }
    Err("filling recipe is missing a fluid".into())
}

/// Fluid input/output list `(fluid id or #tag, amount_mb)` in
/// declaration order.
type FluidStackList = Vec<(String, u32)>;
/// Chance-weighted item result list `(item id, count, weight)` in
/// declaration order.
type WeightedItemResults = Vec<(String, u32, f32)>;

/// One resolved recipe operand: an item/tag ingredient or a fluid input.
enum RecipeOperand {
    Item(ProcessingIngredient),
    /// `(fluid id or #tag, amount_mb)`.
    Fluid(String, u32),
}

fn non_empty_str(value: &str) -> Option<&str> {
    (!value.trim().is_empty()).then_some(value)
}

/// Extract a fluid id (`ns:path`) or `#`-tag reference from an operand
/// object, covering the real 1.21.1 shapes: `{"fluid": ...}`,
/// `{"fluids": ...}` (string or list), `{"fluid_tag": ...}`,
/// `{"type": "fluid_stack", "id"/"fluid": ...}`,
/// `{"type": "fluid_tag", "tag": ...}`, `neoforge:single` (`fluid`),
/// `neoforge:tag` (`tag` + `amount`), `neoforge:components` (`fluids`) and
/// `neoforge:any`.
fn fluid_operand_id(obj: &BTreeMap<String, JsonValue>) -> Option<String> {
    if let Some(id) = obj.get("fluid").and_then(JsonValue::as_str) {
        return non_empty_str(id).map(str::to_string);
    }
    if let Some(id) = obj.get("fluid_tag").and_then(JsonValue::as_str) {
        return non_empty_str(id).map(|id| format!("#{id}"));
    }
    if let Some(fluids) = obj.get("fluids") {
        match fluids {
            JsonValue::String(id) => return non_empty_str(id).map(str::to_string),
            JsonValue::Array(list) => {
                if let Some(id) = list.iter().find_map(JsonValue::as_str) {
                    return non_empty_str(id).map(str::to_string);
                }
            }
            _ => {}
        }
    }
    let type_name = obj
        .get("type")
        .and_then(JsonValue::as_str)
        .map(|t| t.rsplit(':').next().unwrap_or(t));
    match type_name {
        Some("fluid_tag") => obj
            .get("tag")
            .or_else(|| obj.get("fluid"))
            .and_then(JsonValue::as_str)
            .and_then(non_empty_str)
            .map(|id| format!("#{id}")),
        Some("fluid_stack") | Some("single") | Some("components") | Some("any") => obj
            .get("id")
            .and_then(JsonValue::as_str)
            .and_then(non_empty_str)
            .map(str::to_string)
            .or_else(|| {
                obj.get("tag")
                    .and_then(JsonValue::as_str)
                    .and_then(non_empty_str)
                    .map(|id| format!("#{id}"))
            }),
        // `{"type": "neoforge:tag", "tag": "c:milk", "amount": N}` names a
        // fluid tag; without `amount` it is just an item tag ingredient.
        Some("tag") if obj.contains_key("amount") => obj
            .get("tag")
            .and_then(JsonValue::as_str)
            .and_then(non_empty_str)
            .map(|id| format!("#{id}")),
        _ => None,
    }
}

/// Item count on an operand object (`{"item": ..., "count": N}`); absent or
/// invalid counts default to 1, keeping unparseable extras non-fatal.
fn operand_item_count(obj: &BTreeMap<String, JsonValue>) -> u32 {
    obj.get("count")
        .and_then(JsonValue::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v >= 1)
        .unwrap_or(1)
}

/// Resolve one operand value to an item/tag [`RecipeOperand::Item`] or a
/// [`RecipeOperand::Fluid`]: plain strings, `{"item"|"id"}`, `{"tag"}`,
/// `{"fluid"|"fluids"|"fluid_tag"}`, `fluid_stack`/`neoforge:*` typed
/// operands, `neoforge:block_tag`, nested alternative lists
/// (`ingredients: [[...]]`) and compound wrappers (`neoforge:compound`,
/// `difference`, `intersection`, component matchers) — the latter resolving
/// to their first resolvable sub-operand.
/// Unresolvable operand shapes return `Ok(None)`; explicitly malformed
/// values (e.g. a fluid `amount` outside `1..=u32::MAX`) fail closed so a
/// corrupt recognized recipe still aborts discovery instead of being
/// silently reinterpreted.
fn parse_ingredient_operand(value: &JsonValue) -> Result<Option<RecipeOperand>, String> {
    match value {
        JsonValue::String(id) => Ok(non_empty_str(id)
            .map(|id| RecipeOperand::Item(ProcessingIngredient::item(id)))),
        JsonValue::Array(list) => {
            for element in list {
                if let Some(operand) = parse_ingredient_operand(element)? {
                    return Ok(Some(operand));
                }
            }
            Ok(None)
        }
        JsonValue::Object(obj) => {
            if let Some(fluid_id) = fluid_operand_id(obj) {
                let amount_value = obj.get("amount").or_else(|| obj.get("amount_mb"));
                let amount_mb = if amount_value.is_some() {
                    bounded_integer(amount_value, "fluid amount", 1, u32::MAX)?
                } else {
                    1000
                };
                return Ok(Some(RecipeOperand::Fluid(fluid_id, amount_mb)));
            }
            if let Some(id) = obj
                .get("item")
                .or_else(|| obj.get("id"))
                .and_then(|v| match v {
                    JsonValue::String(id) => Some(id.as_str()),
                    JsonValue::Object(inner) => inner.get("id").and_then(JsonValue::as_str),
                    _ => None,
                })
                .and_then(non_empty_str)
            {
                return Ok(Some(RecipeOperand::Item(ProcessingIngredient {
                    id: id.into(),
                    is_tag: false,
                    count: operand_item_count(obj),
                })));
            }
            if let Some(id) = obj
                .get("tag")
                .and_then(JsonValue::as_str)
                .and_then(non_empty_str)
            {
                return Ok(Some(RecipeOperand::Item(ProcessingIngredient {
                    id: id.into(),
                    is_tag: true,
                    count: operand_item_count(obj),
                })));
            }
            // Compound/wrapper ingredient (`neoforge:compound`, `difference`,
            // `intersection`, `any`, component/data matchers): first
            // resolvable sub-operand wins, deterministically.
            for key in ["ingredients", "children", "entries", "base", "items"] {
                if let Some(operand) = obj
                    .get(key)
                    .map(parse_ingredient_operand)
                    .transpose()?
                    .flatten()
                {
                    return Ok(Some(operand));
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// Collect the full operand list of a processing recipe: `ingredients`
/// (array or nested alternative lists) plus the singular `ingredient`,
/// `input`, `item_input`, `fluid_input` keys, split into item/tag
/// ingredients and fluid inputs, declaration order preserved.
fn parse_ingredient_list(
    obj: &BTreeMap<String, JsonValue>,
) -> Result<(Vec<ProcessingIngredient>, FluidStackList), String> {
    let mut items = Vec::new();
    let mut fluids = Vec::new();
    let push = |operand: RecipeOperand,
                    items: &mut Vec<ProcessingIngredient>,
                    fluids: &mut Vec<(String, u32)>| match operand {
        RecipeOperand::Item(ingredient) => items.push(ingredient),
        RecipeOperand::Fluid(fluid_id, amount_mb) => fluids.push((fluid_id, amount_mb)),
    };
    if let Some(value) = obj.get("ingredients") {
        match value {
            JsonValue::Array(list) => {
                for element in list {
                    if let Some(operand) = parse_ingredient_operand(element)? {
                        push(operand, &mut items, &mut fluids);
                    }
                }
            }
            _ => return Err("processing ingredients must be an array".into()),
        }
    }
    for key in ["ingredient", "input", "item_input", "fluid_input"] {
        if let Some(operand) = obj
            .get(key)
            .map(parse_ingredient_operand)
            .transpose()?
            .flatten()
        {
            push(operand, &mut items, &mut fluids);
        }
    }
    Ok((items, fluids))
}

/// First declared item/tag ingredient as a resource key (`id` or `#tag`),
/// matching the legacy single-`ingredient_item` view.
fn first_item_key(
    id: &str,
    items: &[ProcessingIngredient],
) -> Result<String, String> {
    items
        .first()
        .map(ProcessingIngredient::resource_key)
        .ok_or_else(|| format!("{id}: processing input item is missing"))
}

/// Whether a result object is a fluid output (`{"amount": N, "id": fluid}`,
/// `{"fluid": ...}`, `fluid_stack`-typed). Fluid results carry `(fluid id or
/// #tag, amount_mb)`; everything else stays an item result.
fn parse_fluid_result(value: &JsonValue) -> Result<Option<(String, u32)>, String> {
    let Some(obj) = value.as_object() else {
        return Ok(None);
    };
    let type_name = obj
        .get("type")
        .and_then(JsonValue::as_str)
        .map(|t| t.rsplit(':').next().unwrap_or(t));
    let is_fluid = obj.contains_key("fluid")
        || obj.contains_key("fluids")
        || obj.contains_key("fluid_tag")
        || matches!(
            type_name,
            Some("fluid" | "fluid_stack" | "fluid_tag" | "single" | "components" | "any")
        )
        // Real 1.21.1 fluid results are `{"amount": N, "id": "ns:fluid"}` —
        // `amount` (not `count`) marks a fluid stack.
        || (obj.contains_key("amount")
            && !obj.contains_key("count")
            && obj.contains_key("id"));
    if !is_fluid {
        return Ok(None);
    }
    let fluid_id = fluid_operand_id(obj)
        .or_else(|| {
            obj.get("id")
                .and_then(JsonValue::as_str)
                .and_then(non_empty_str)
                .map(str::to_string)
        })
        .ok_or("fluid result is missing a fluid id")?;
    let amount_mb = match obj.get("amount").or_else(|| obj.get("amount_mb")) {
        Some(value) => bounded_integer(Some(value), "fluid amount", 1, u32::MAX)?,
        None => 1000,
    };
    Ok(Some((fluid_id, amount_mb)))
}

/// Item-id resolver for result entries (`{"item"}`, `{"id"}`, nested
/// `{"item": {"id"}}`); `{"tag"}` results resolve to `#tag`.
fn parse_result_item(value: &JsonValue) -> Result<String, String> {
    if let Ok(id) = parse_item_id(value) {
        return Ok(id);
    }
    if let Some(tag) = value
        .as_object()
        .and_then(|o| o.get("tag"))
        .and_then(JsonValue::as_str)
        .and_then(non_empty_str)
    {
        return Ok(format!("#{tag}"));
    }
    Err("expected a concrete item id".into())
}

/// Split a recipe's `results`/`result`/`output`/`main_output` into
/// chance-weighted item results and fluid outputs. Same bounds as
/// [`parse_assembly_results`]: 1..=32 total entries, item counts
/// 1..=u32::MAX, finite nonnegative weights — but at least one positive-
/// weight item result *or* one fluid result (fluid-producing machines like
/// `create:emptying`/`create:mixing` have no item results at all).
fn parse_machine_results(
    obj: &BTreeMap<String, JsonValue>,
) -> Result<(WeightedItemResults, FluidStackList), String> {
    let result_value = obj
        .get("results")
        .or_else(|| obj.get("result"))
        .or_else(|| obj.get("output"))
        .or_else(|| obj.get("main_output"))
        .ok_or("recipe results are missing")?;
    let results = match result_value {
        JsonValue::Array(items) => items.as_slice(),
        JsonValue::Object(_) => std::slice::from_ref(result_value),
        _ => return Err("recipe results must be an array or object".into()),
    };
    if !(1..=32).contains(&results.len()) {
        return Err("recipe result count must be in 1..=32".into());
    }
    let mut items = Vec::new();
    let mut fluids = Vec::new();
    for result in results {
        if let Some(fluid) = parse_fluid_result(result)? {
            fluids.push(fluid);
            continue;
        }
        let item = parse_result_item(result)?;
        let entry = result
            .as_object()
            .ok_or("recipe result must be an object")?;
        let nested_count = entry
            .get("item")
            .and_then(JsonValue::as_object)
            .and_then(|inner| inner.get("count"));
        let count = match entry.get("count").or(nested_count) {
            Some(value) => bounded_integer(Some(value), "item count", 1, u32::MAX)?,
            None => 1,
        };
        let weight = match entry.get("chance").or_else(|| entry.get("weight")) {
            Some(value) => value.as_f64().ok_or("result weight must be finite")?,
            None => 1.0,
        };
        if !(0.0..=f64::from(f32::MAX)).contains(&weight) {
            return Err("result weight must be nonnegative and finite".into());
        }
        items.push((item, count, weight as f32));
    }
    if items.iter().all(|(_, _, weight)| *weight <= 0.0) && fluids.is_empty() {
        return Err("at least one result must have a positive weight".into());
    }
    Ok((items, fluids))
}

/// Decode the standard machine-recipe shape shared by the Create 1.21.1
/// processing families and the generic `<ns>:<machine>` fallback:
/// `ingredients` + `results`/`result`, fluids and tags included.
fn parse_machine_object(
    id: &str,
    recipe_type: String,
    obj: &BTreeMap<String, JsonValue>,
) -> Result<MachineProcessingRecipe, String> {
    let (ingredients, fluid_inputs) =
        parse_ingredient_list(obj).map_err(|e| format!("{id}: {e}"))?;
    if ingredients.is_empty() && fluid_inputs.is_empty() {
        return Err(format!("{id}: processing input item is missing"));
    }
    let (results, fluid_results) =
        parse_machine_results(obj).map_err(|e| format!("{id}: {e}"))?;
    Ok(MachineProcessingRecipe {
        id: id.into(),
        recipe_type,
        ingredients,
        fluid_inputs,
        results,
        fluid_results,
    })
}

/// Decode `create:mechanical_crafting`'s shaped-grid schema: each `key`
/// symbol's operand is consumed once per occurrence in `pattern`.
fn parse_mechanical_crafting_object(
    id: &str,
    recipe_type: String,
    obj: &BTreeMap<String, JsonValue>,
) -> Result<MachineProcessingRecipe, String> {
    let key = obj
        .get("key")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| format!("{id}: mechanical_crafting key table is missing"))?;
    let pattern = obj
        .get("pattern")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| format!("{id}: mechanical_crafting pattern is missing"))?;
    if !(1..=9).contains(&pattern.len()) {
        return Err(format!("{id}: mechanical_crafting pattern must be 1..=9 rows"));
    }
    let mut usage: BTreeMap<String, u32> = BTreeMap::new();
    for row in pattern {
        let Some(row) = row.as_str() else {
            return Err(format!("{id}: mechanical_crafting pattern rows must be strings"));
        };
        for symbol in row.chars() {
            if symbol != ' ' {
                *usage.entry(symbol.to_string()).or_default() += 1;
            }
        }
    }
    let mut ingredients = Vec::new();
    let mut fluid_inputs = Vec::new();
    for (symbol, count) in &usage {
        let operand = key
            .get(symbol)
            .map(parse_ingredient_operand)
            .transpose()?
            .flatten()
            .ok_or_else(|| {
                format!("{id}: mechanical_crafting key '{symbol}' is missing or unresolvable")
            })?;
        match operand {
            RecipeOperand::Item(mut ingredient) => {
                ingredient.count = ingredient.count.saturating_mul(*count);
                ingredients.push(ingredient);
            }
            RecipeOperand::Fluid(fluid_id, amount_mb) => {
                fluid_inputs.push((fluid_id, amount_mb));
            }
        }
    }
    let (results, fluid_results) =
        parse_machine_results(obj).map_err(|e| format!("{id}: {e}"))?;
    Ok(MachineProcessingRecipe {
        id: id.into(),
        recipe_type,
        ingredients,
        fluid_inputs,
        results,
        fluid_results,
    })
}

fn parse_assembly_results(
    obj: &BTreeMap<String, JsonValue>,
) -> Result<Vec<(String, u32, f32)>, String> {
    let result_value = obj
        .get("results")
        .or_else(|| obj.get("result"))
        .ok_or("recipe results are missing")?;
    let results = match result_value {
        JsonValue::Array(items) => items.as_slice(),
        JsonValue::Object(_) => std::slice::from_ref(result_value),
        _ => return Err("recipe results must be an array or object".into()),
    };
    if !(1..=32).contains(&results.len()) {
        return Err("recipe result count must be in 1..=32".into());
    }
    let mut output = Vec::with_capacity(results.len());
    for result in results {
        let item = parse_item_id(result)?;
        let entry = result
            .as_object()
            .ok_or("recipe result must be an object")?;
        let nested_count = entry
            .get("item")
            .and_then(JsonValue::as_object)
            .and_then(|inner| inner.get("count"));
        let count = match entry.get("count").or(nested_count) {
            Some(value) => bounded_integer(Some(value), "item count", 1, u32::MAX)?,
            None => 1,
        };
        let weight = match entry.get("chance").or_else(|| entry.get("weight")) {
            Some(value) => value.as_f64().ok_or("result weight must be finite")?,
            None => 1.0,
        };
        if !(0.0..=f64::from(f32::MAX)).contains(&weight) {
            return Err("result weight must be nonnegative and finite".into());
        }
        output.push((item, count, weight as f32));
    }
    if !output.iter().any(|(_, _, weight)| *weight > 0.0) {
        return Err("at least one result must have a positive weight".into());
    }
    Ok(output)
}

/// Discover assembly, pressing, filling, and the full Create 1.21.1 machine
/// family (compacting, splashing, haunting, deploying, emptying,
/// item_application, sandpaper_polishing, mechanical_crafting) plus any
/// generic `<ns>:<machine>` recipe from both singular and plural recipe
/// directories under every namespace, including nested paths. Invalid
/// recipe JSON or invalid recognized recipes abort the scan; non-machine
/// serializers (vanilla crafting, smelting, ...) are skipped gracefully.
pub fn discover_addon_processing_recipes(
    root: &Path,
) -> Result<Vec<AddonProcessingRecipe>, String> {
    let data = root.join("data");
    if !data.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    let namespaces = fs::read_dir(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    for namespace in namespaces {
        let namespace = namespace.map_err(|e| format!("{}: {e}", data.display()))?;
        if !namespace.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        for directory in ["recipe", "recipes"] {
            let folder = namespace.path().join(directory);
            if fs::symlink_metadata(&folder).is_ok_and(|metadata| metadata.is_dir()) {
                collect_addon_recipe_paths(&folder, &mut paths)?;
            }
        }
    }
    paths.sort();

    let mut found = Vec::new();
    for path in paths {
        let relative = path.strip_prefix(&data).map_err(|e| e.to_string())?;
        let mut pieces = relative.components();
        let namespace = pieces
            .next()
            .and_then(|part| part.as_os_str().to_str())
            .ok_or("invalid namespace path")?;
        pieces.next(); // recipe or recipes
        let recipe_path = pieces.as_path().with_extension("");
        let id = format!(
            "{namespace}:{}",
            recipe_path.to_string_lossy().replace('\\', "/")
        );
        let metadata = fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if metadata.len() > 4 * 1024 * 1024 {
            return Err(format!(
                "{}: recipe JSON exceeds maximum size",
                path.display()
            ));
        }
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let json = parse_json(&bytes).map_err(|e| format!("{}: {e:?}", path.display()))?;
        let obj = json
            .as_object()
            .ok_or_else(|| format!("{id}: recipe root must be an object"))?;
        let Some(kind) = obj.get("type").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some((namespace, _)) = kind.split_once(':') else {
            continue;
        };
        if namespace.is_empty() {
            continue;
        }
        if let Some(recipe) = parse_addon_processing_object(&id, obj)? {
            found.push(recipe);
        }
    }
    Ok(found)
}

/// Classify one recipe JSON object into an [`AddonProcessingRecipe`].
/// Returns `Ok(None)` for recipe types outside the processing family —
/// vanilla/shared-namespace serializers (`minecraft:*`, `forge:*`, `c:*`,
/// `neoforge:*`, `fabric:*`) and any `<ns>:<type>` that does not declare a
/// standard `ingredients` + `results`/`result` machine shape; recognized-
/// but-malformed recipes fail closed with `Err`.
fn parse_addon_processing_object(
    id: &str,
    obj: &BTreeMap<String, JsonValue>,
) -> Result<Option<AddonProcessingRecipe>, String> {
    let kind = match obj.get("type").and_then(JsonValue::as_str) {
        Some(kind) => kind,
        None => return Ok(None),
    };
    let Some((namespace, kind)) = kind.split_once(':') else {
        return Ok(None);
    };
    if namespace.is_empty() {
        return Ok(None);
    }
    let recipe_type = || format!("{namespace}:{kind}");
    let machine = |id: &str, variant: fn(MachineProcessingRecipe) -> AddonProcessingRecipe| {
        parse_machine_object(id, recipe_type(), obj).map(|recipe| Some(variant(recipe)))
    };
    match kind {
        "sequenced_assembly" => Ok(Some(AddonProcessingRecipe::SequencedAssembly(
            parse_assembly_object(id, obj).map_err(|e| format!("{id}: {e}"))?,
        ))),
        "pressing" | "milling" | "crushing" | "cutting" => {
            let (items, _) = parse_ingredient_list(obj).map_err(|e| format!("{id}: {e}"))?;
            let ingredient_item = first_item_key(id, &items)?;
            let results = parse_assembly_results(obj).map_err(|e| format!("{id}: {e}"))?;
            let fields = (id.to_string(), ingredient_item, results);
            Ok(Some(match kind {
                "pressing" => AddonProcessingRecipe::Pressing {
                    id: fields.0,
                    ingredient_item: fields.1,
                    results: fields.2,
                },
                "milling" => AddonProcessingRecipe::Milling {
                    id: fields.0,
                    ingredient_item: fields.1,
                    results: fields.2,
                },
                "crushing" => AddonProcessingRecipe::Crushing {
                    id: fields.0,
                    ingredient_item: fields.1,
                    results: fields.2,
                },
                _ => AddonProcessingRecipe::Cutting {
                    id: fields.0,
                    ingredient_item: fields.1,
                    results: fields.2,
                },
            }))
        }
        "mixing" => {
            let (items, fluid_inputs) =
                parse_ingredient_list(obj).map_err(|e| format!("{id}: {e}"))?;
            let ingredient_item = first_item_key(id, &items)?;
            let (results, fluid_results) =
                parse_machine_results(obj).map_err(|e| format!("{id}: {e}"))?;
            Ok(Some(AddonProcessingRecipe::Mixing {
                id: id.to_string(),
                ingredient_item,
                fluid_inputs,
                results,
                fluid_results,
            }))
        }
        "filling" => {
            let (items, fluids) = parse_ingredient_list(obj).map_err(|e| format!("{id}: {e}"))?;
            let ingredient_item = first_item_key(id, &items)?;
            let results = parse_assembly_results(obj).map_err(|e| format!("{id}: {e}"))?;
            let (fluid_id, amount_mb) = fluids
                .first()
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| parse_fluid_operand(obj))
                .map_err(|e| format!("{id}: {e}"))?;
            Ok(Some(AddonProcessingRecipe::Filling {
                id: id.to_string(),
                ingredient_item,
                fluid_id,
                amount_mb,
                results,
            }))
        }
        "compacting" => machine(id, AddonProcessingRecipe::Compacting),
        "splashing" => machine(id, AddonProcessingRecipe::Splashing),
        "haunting" => machine(id, AddonProcessingRecipe::Haunting),
        "deploying" => machine(id, AddonProcessingRecipe::Deploying),
        "emptying" => machine(id, AddonProcessingRecipe::Emptying),
        "item_application" => machine(id, AddonProcessingRecipe::ItemApplication),
        "sandpaper_polishing" => machine(id, AddonProcessingRecipe::SandpaperPolishing),
        "mechanical_crafting" => parse_mechanical_crafting_object(id, recipe_type(), obj)
            .map(|recipe| Some(AddonProcessingRecipe::MechanicalCrafting(recipe))),
        _ => {
            // Generic `<ns>:<machine>` fallback: any non-shared namespace
            // declaring standard `ingredients` + `results`/`result` fields.
            // Anything else (`minecraft:crafting_shaped`, `neoforge:*`
            // wrappers, serializers without the standard shape) is skipped
            // gracefully instead of aborting the JAR scan.
            const SHARED_NAMESPACES: [&str; 5] =
                ["minecraft", "c", "forge", "neoforge", "fabric"];
            if SHARED_NAMESPACES.contains(&namespace) {
                return Ok(None);
            }
            match parse_machine_object(id, recipe_type(), obj) {
                Ok(recipe) => Ok(Some(AddonProcessingRecipe::GenericMachine(recipe))),
                Err(_) => Ok(None),
            }
        }
    }
}

fn collect_addon_recipe_paths(folder: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", folder.display()))?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            collect_addon_recipe_paths(&entry.path(), files)?;
        } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "json") {
            files.push(entry.path());
        }
    }
    Ok(())
}

/// Kinetic connector variants understood by `CreateFactory` topology rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectorKind {
    Shaft,
    Cogwheel,
    LargeCogwheel,
    Gearbox,
    Clutch,
    Gearshift,
}

impl ConnectorKind {
    fn role(self) -> CreateBlockRole {
        match self {
            Self::Shaft => CreateBlockRole::Shaft,
            Self::Cogwheel => CreateBlockRole::Cogwheel,
            Self::LargeCogwheel => CreateBlockRole::LargeCogwheel,
            Self::Gearbox => CreateBlockRole::Gearbox,
            Self::Clutch => CreateBlockRole::Clutch,
            Self::Gearshift => CreateBlockRole::Gearshift,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "shaft" => Some(Self::Shaft),
            "cogwheel" => Some(Self::Cogwheel),
            "large_cogwheel" => Some(Self::LargeCogwheel),
            "gearbox" => Some(Self::Gearbox),
            "clutch" => Some(Self::Clutch),
            "gearshift" => Some(Self::Gearshift),
            _ => None,
        }
    }
}

/// Addon block injecting rotation into a kinetic network (motors, engines,
/// energisers, diesel generators). `stress_capacity` is the SU produced at
/// `base_rpm`; `axis` is the block's declared default rotation axis used when
/// the blockstate carries an `axis`/`facing` property.
#[derive(Clone, Debug, PartialEq)]
pub struct KineticSourceSpec {
    pub id: String,
    pub base_rpm: i32,
    pub stress_capacity: f64,
    pub axis: Axis,
}

/// Addon block drawing stress from a kinetic network. `min_rpm` gates
/// speed-dependent behaviour (e.g. propellers only develop thrust at or above
/// this RPM, mills refuse work below it).
#[derive(Clone, Debug, PartialEq)]
pub struct KineticConsumerSpec {
    pub id: String,
    pub stress_impact: f64,
    pub min_rpm: f64,
}

/// Addon kinetic connector block (shafts, cogwheels, gearboxes, clutches,
/// gearshifts, chain drives) participating in `CreateFactory` link rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorSpec {
    pub id: String,
    pub kind: ConnectorKind,
}

/// 6-DOF propulsion model for aeronautics-class addons. Force/torque gains
/// are tuned so [`crate::aeronautics::RigidBodySubWorld`] stays bounded and
/// deterministic at the fixed 20 TPS step.
#[derive(Clone, Debug, PartialEq)]
pub struct PropulsionSpec {
    /// Newtons of thrust per solved RPM per propeller mount.
    pub thrust_per_rpm: f64,
    /// Newtons of lift per buoyancy/envelope block at full-lift altitude.
    pub lift_per_buoyancy_block: f64,
    /// Quadratic aerodynamic drag coefficient (`F = -k * ρ * |v| * v`).
    pub drag_coeff: f64,
    /// Aerodynamic angular damping coefficient (`τ = -k * ρ * ω`).
    pub angular_drag_coeff: f64,
    /// Rudder torque gain (Nm per m/s of sensed local lateral airflow).
    pub rudder_torque: f64,
    /// Altitude (world Y) below which buoyancy lift is undiminished.
    pub lift_full_altitude: f64,
    /// Altitude (world Y) at and above which buoyancy lift reaches zero.
    pub max_lift_altitude: f64,
    /// Kinetic blocks whose mounts produce thrust (propeller bearings,
    /// thrusters); each must also appear in `kinetic_sources` or
    /// `kinetic_consumers` to spin, or it is registered with a default
    /// 8 SU/RPM consumer load.
    pub propeller_blocks: Vec<String>,
    /// Blocks contributing lift (`balloon_envelope`, `levitation_core`).
    pub buoyancy_blocks: Vec<String>,
    /// Blocks contributing control torque (`rudder`, helm blocks).
    pub rudder_blocks: Vec<String>,
}

impl PropulsionSpec {
    /// Reference tuning used by the built-in Create: Aeronautics profile and
    /// as the fallback for discovered aeronautics-class addons.
    pub fn aeronautics_default() -> Self {
        Self {
            thrust_per_rpm: 150.0,
            lift_per_buoyancy_block: 1200.0,
            drag_coeff: 2.0,
            angular_drag_coeff: 80.0,
            rudder_torque: 60.0,
            lift_full_altitude: 200.0,
            max_lift_altitude: 320.0,
            propeller_blocks: Vec::new(),
            buoyancy_blocks: Vec::new(),
            rudder_blocks: Vec::new(),
        }
    }
}

/// Compatibility of a [`CreateAddonProfile`] against the two canonical
/// megu targets: `1.21.1` (`DataVersion` 3955) and latest vanilla `26.3`
/// (`DataVersion` 5023). `None` verdicts mean the profile declares no
/// `mc_version_range` and applies unconditionally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetCompatibilitySummary {
    /// Verdict on the `1.21.1` target.
    pub v1_21_1: Option<VersionCompatVerdict>,
    /// Verdict on the `26.3` target.
    pub v26_3: Option<VersionCompatVerdict>,
}

impl TargetCompatibilitySummary {
    /// Whether both canonical targets accept the profile. Mirrors
    /// [`CreateAddonProfile::supports_both_targets`]: an undeclared range
    /// (`None`) counts as supported, every mismatch or malformed
    /// declaration fails closed.
    pub fn compatible_on_both(&self) -> bool {
        fn supported(verdict: &Option<VersionCompatVerdict>) -> bool {
            matches!(verdict, None | Some(VersionCompatVerdict::Compatible))
        }
        supported(&self.v1_21_1) && supported(&self.v26_3)
    }
}

/// Declarative Create addon profile: Create-specific component rules layered
/// on a [`ModOffloadProfile`] base so one manifest drives both the flagship
/// `create-adapter` registry and the universal `mod-adapter-core` runtime.
#[derive(Clone, Debug)]
pub struct CreateAddonProfile {
    pub mod_id: String,
    /// Universal-tier mirror of this addon (transport pipes, fluid
    /// pipes/tanks/pumps, processors, kinetic network nodes, decorative
    /// blocks). `finalize` keeps it synchronized with the Create-side lists.
    pub offload: ModOffloadProfile,
    pub kinetic_sources: Vec<KineticSourceSpec>,
    pub kinetic_consumers: Vec<KineticConsumerSpec>,
    pub connectors: Vec<ConnectorSpec>,
    /// Blocks that anchor a contraption / airship assembly
    /// (`airship_assembler`, bearings).
    pub assembly_anchors: Vec<String>,
    /// Aeronautics propulsion model; `None` for non-flying addons.
    pub propulsion: Option<PropulsionSpec>,
    /// Additional purely-decorative blocks (controls, cosmetic blocks).
    pub decorative_blocks: Vec<String>,
    /// Ore/chest/barrel blocks folded into the anti-xray hidden set.
    pub hidden_blocks: Vec<String>,
    /// Explicit or discovered blockstate property tables (`id -> props`).
    /// Overrides the axis/facing defaults in `block_defs()`.
    pub block_properties: BTreeMap<String, Vec<PropertyDef>>,
    /// Declared Minecraft version compatibility for this profile, in either
    /// loader syntax — NeoForge/Forge Maven interval (`"[1.21.1,1.22)"`) or
    /// Fabric SemVer (`">=1.21.1 <1.22"`, `"1.21.x"`) — parsed by
    /// [`VersionRange`]. `None` means the profile makes no version claim and
    /// [`Self::supports_mc_version`] returns `true` for every target.
    pub mc_version_range: Option<String>,
    /// Processing recipes discovered from the addon's own
    /// `data/<ns>/recipe(s)/*.json` (sequenced assemblies, pressing, mixing,
    /// milling, crushing, cutting, filling, the Create 1.21.1 machine
    /// families, and generic `<ns>:<machine>` fallbacks). Populated by
    /// [`discover_addon_from_jar`]; empty for hand-authored profiles.
    pub processing_recipes: Vec<AddonProcessingRecipe>,
}

impl CreateAddonProfile {
    /// Empty programmatic profile for `mod_id`; populate fields then call
    /// [`Self::finalize`] to mirror Create-side blocks into `offload`.
    pub fn new(mod_id: impl Into<String>) -> Self {
        let mod_id = mod_id.into();
        Self {
            mod_id: mod_id.clone(),
            offload: ModOffloadProfile {
                mod_id,
                blocks: Vec::new(),
                transport_delay_ticks: BTreeMap::new(),
                tank_capacity_mb: BTreeMap::new(),
                processor_recipe_types: BTreeMap::new(),
            },
            kinetic_sources: Vec::new(),
            kinetic_consumers: Vec::new(),
            connectors: Vec::new(),
            assembly_anchors: Vec::new(),
            propulsion: None,
            decorative_blocks: Vec::new(),
            hidden_blocks: Vec::new(),
            block_properties: BTreeMap::new(),
            mc_version_range: None,
            processing_recipes: Vec::new(),
        }
    }

    /// Parse a declarative Create addon profile JSON document. The universal
    /// `blocks` list shares [`ModOffloadProfile::from_json`]'s schema;
    /// Create-side sections (`kinetic_sources`, `kinetic_consumers`,
    /// `connectors`, `assembly_anchors`, `decorative_blocks`, `hidden_blocks`,
    /// `block_properties`, `propulsion`) extend it.
    pub fn from_json(bytes: &[u8]) -> Result<Self, DataLoadError> {
        let offload = ModOffloadProfile::from_json(bytes)?;
        let root = parse_json(bytes)?;
        let obj = root
            .as_object()
            .ok_or_else(|| DataLoadError::Json("addon profile root must be object".into()))?;
        let mut profile = Self::new(offload.mod_id.clone());
        profile.offload = offload;

        if let Some(arr) = obj.get("kinetic_sources").and_then(JsonValue::as_array) {
            for item in arr {
                let o = json_obj(item, "kinetic_sources entry")?;
                let id = json_str(o, "id")?.to_string();
                profile.kinetic_sources.push(KineticSourceSpec {
                    id,
                    base_rpm: json_f64(o, "rpm").unwrap_or(32.0) as i32,
                    stress_capacity: json_f64(o, "capacity")
                        .or_else(|| json_f64(o, "stress_capacity"))
                        .unwrap_or(512.0),
                    axis: axis_from_name(
                        o.get("axis")
                            .or_else(|| o.get("facing"))
                            .and_then(JsonValue::as_str)
                            .unwrap_or("y"),
                    ),
                });
            }
        }
        if let Some(arr) = obj
            .get("kinetic_consumers")
            .and_then(JsonValue::as_array)
        {
            for item in arr {
                let o = json_obj(item, "kinetic_consumers entry")?;
                profile.kinetic_consumers.push(KineticConsumerSpec {
                    id: json_str(o, "id")?.to_string(),
                    stress_impact: json_f64(o, "impact")
                        .or_else(|| json_f64(o, "stress_impact"))
                        .unwrap_or(4.0),
                    min_rpm: json_f64(o, "min_rpm").unwrap_or(0.0),
                });
            }
        }
        if let Some(arr) = obj.get("connectors").and_then(JsonValue::as_array) {
            for item in arr {
                let o = json_obj(item, "connectors entry")?;
                let kind = ConnectorKind::from_name(json_str(o, "kind")?)
                    .ok_or_else(|| DataLoadError::Json("unknown connector kind".into()))?;
                profile.connectors.push(ConnectorSpec {
                    id: json_str(o, "id")?.to_string(),
                    kind,
                });
            }
        }
        for (key, out) in [
            ("assembly_anchors", &mut profile.assembly_anchors),
            ("decorative_blocks", &mut profile.decorative_blocks),
            ("hidden_blocks", &mut profile.hidden_blocks),
        ] {
            if let Some(arr) = obj.get(key).and_then(JsonValue::as_array) {
                out.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_string)));
            }
        }
        if let Some(props) = obj.get("block_properties").and_then(JsonValue::as_object) {
            for (id, ptable) in props {
                if let Some(pobj) = ptable.as_object() {
                    let defs: Vec<PropertyDef> = pobj
                        .iter()
                        .filter_map(|(name, vals)| {
                            let values: Vec<String> = vals
                                .as_array()?
                                .iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect();
                            (!values.is_empty()).then(|| PropertyDef {
                                name: name.clone(),
                                values,
                            })
                        })
                        .collect();
                    profile.block_properties.insert(id.clone(), defs);
                }
            }
        }
        if let Some(pobj) = obj.get("propulsion").and_then(JsonValue::as_object) {
            let mut spec = PropulsionSpec::aeronautics_default();
            for (key, slot) in [
                ("thrust_per_rpm", &mut spec.thrust_per_rpm),
                ("lift_per_block", &mut spec.lift_per_buoyancy_block),
                ("drag", &mut spec.drag_coeff),
                ("angular_drag", &mut spec.angular_drag_coeff),
                ("rudder_torque", &mut spec.rudder_torque),
                ("lift_full_altitude", &mut spec.lift_full_altitude),
                ("max_lift_altitude", &mut spec.max_lift_altitude),
            ] {
                if let Some(v) = pobj.get(key).and_then(JsonValue::as_f64) {
                    *slot = v;
                }
            }
            for (key, out) in [
                ("propeller_blocks", &mut spec.propeller_blocks),
                ("buoyancy_blocks", &mut spec.buoyancy_blocks),
                ("rudder_blocks", &mut spec.rudder_blocks),
            ] {
                if let Some(arr) = pobj.get(key).and_then(JsonValue::as_array) {
                    out.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_string)));
                }
            }
            profile.propulsion = Some(spec);
        }
        if let Some(range) = obj.get("mc_version_range").and_then(JsonValue::as_str) {
            profile.mc_version_range = Some(range.to_string());
        }
        profile.finalize();
        Ok(profile)
    }

    /// Flagship [`CreateBlockRole`] definitions for every Create-side block in
    /// this profile, ready to merge into a [`CreateBlockRegistry`].
    /// First declaration wins on duplicate ids (sources > connectors >
    /// consumers > offload mirrors > mounts > anchors > decorative).
    pub fn block_defs(&self) -> Vec<BlockDef<CreateBlockRole>> {
        let axis_props: &dyn Fn() -> Vec<PropertyDef> =
            &|| vec![PropertyDef::new("axis", &["x", "y", "z"])];
        let axis_powered_props: &dyn Fn() -> Vec<PropertyDef> = &|| {
            vec![
                PropertyDef::new("axis", &["x", "y", "z"]),
                PropertyDef::new("powered", &["false", "true"]),
            ]
        };
        let facing_props: &dyn Fn() -> Vec<PropertyDef> = &|| {
            vec![PropertyDef::new(
                "facing",
                &["north", "south", "east", "west", "up", "down"],
            )]
        };
        let empty_props: &dyn Fn() -> Vec<PropertyDef> = &Vec::new;

        let mut out: Vec<BlockDef<CreateBlockRole>> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut push = |def: BlockDef<CreateBlockRole>, seen: &mut BTreeSet<String>| {
            if seen.insert(def.id.clone()) {
                out.push(def);
            }
        };

        let propeller_set: BTreeSet<&str> = self
            .propulsion
            .iter()
            .flat_map(|p| p.propeller_blocks.iter().map(String::as_str))
            .collect();

        for s in &self.kinetic_sources {
            push(
                BlockDef {
                    id: s.id.clone(),
                    properties: self.props_for(&s.id, &axis_props),
                    load_factor: 0.0,
                    capacity_factor: s.stress_capacity,
                    role: CreateBlockRole::Generator {
                        driven_rpm: s.base_rpm,
                    },
                },
                &mut seen,
            );
        }
        for c in &self.connectors {
            let powered = matches!(c.kind, ConnectorKind::Clutch | ConnectorKind::Gearshift);
            push(
                BlockDef {
                    id: c.id.clone(),
                    properties: self.props_for(
                        &c.id,
                        if powered {
                            axis_powered_props
                        } else {
                            axis_props
                        },
                    ),
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: c.kind.role(),
                },
                &mut seen,
            );
        }
        for c in &self.kinetic_consumers {
            let default_props = if propeller_set.contains(c.id.as_str()) {
                facing_props
            } else {
                axis_props
            };
            push(
                BlockDef {
                    id: c.id.clone(),
                    properties: self.props_for(&c.id, default_props),
                    load_factor: c.stress_impact,
                    capacity_factor: 0.0,
                    role: if self.offload.processor_recipe_types.contains_key(&c.id) {
                        CreateBlockRole::Processor
                    } else {
                        CreateBlockRole::StressConsumer
                    },
                },
                &mut seen,
            );
        }
        for b in &self.offload.blocks {
            if seen.contains(&b.id) {
                continue;
            }
            let role = match b.role {
                UniversalBlockRole::TransportPipe => CreateBlockRole::Transport,
                UniversalBlockRole::FluidPipe => CreateBlockRole::FluidPipe,
                UniversalBlockRole::FluidTank => CreateBlockRole::FluidTank,
                UniversalBlockRole::FluidPump => CreateBlockRole::FluidPump,
                UniversalBlockRole::Processor => CreateBlockRole::Processor,
                UniversalBlockRole::ContraptionAnchor => CreateBlockRole::ContraptionAnchor,
                _ => continue, // network roles stay universal-tier only
            };
            let def = BlockDef {
                id: b.id.clone(),
                properties: self.props_for(&b.id, &|| b.properties.clone()),
                load_factor: b.load_factor,
                capacity_factor: b.capacity_factor,
                role,
            };
            push(def, &mut seen);
        }
        if let Some(p) = &self.propulsion {
            for id in &p.propeller_blocks {
                push(
                    BlockDef {
                        id: id.clone(),
                        properties: self.props_for(id, facing_props),
                        load_factor: 8.0,
                        capacity_factor: 0.0,
                        role: CreateBlockRole::StressConsumer,
                    },
                    &mut seen,
                );
            }
            for id in p.buoyancy_blocks.iter().chain(&p.rudder_blocks) {
                push(
                    BlockDef {
                        id: id.clone(),
                        properties: self.props_for(id, empty_props),
                        load_factor: 0.0,
                        capacity_factor: 0.0,
                        role: CreateBlockRole::Decorative,
                    },
                    &mut seen,
                );
            }
        }
        for id in self
            .assembly_anchors
            .iter()
            .chain(&self.decorative_blocks)
            .chain(&self.hidden_blocks)
        {
            push(
                BlockDef {
                    id: id.clone(),
                    properties: self.props_for(id, empty_props),
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: if self.assembly_anchors.contains(id) {
                        CreateBlockRole::ContraptionAnchor
                    } else {
                        CreateBlockRole::Decorative
                    },
                },
                &mut seen,
            );
        }
        out
    }

    /// Parse [`Self::mc_version_range`] via [`VersionRange`]. `Ok(None)` when
    /// the profile declares no range; `Err` on a malformed declaration so
    /// callers can fail closed instead of trusting it.
    pub fn parsed_mc_version_range(&self) -> Result<Option<VersionRange>, VersionRangeError> {
        self.mc_version_range
            .as_deref()
            .map(VersionRange::parse)
            .transpose()
    }

    /// Compatibility verdict for this profile on a server running `target` —
    /// [`VersionCompatVerdict`] distinguishing minor/major mismatches and
    /// malformed declarations. `None` when the profile declares no range.
    pub fn check_mc_version(&self, target: McVersion) -> Option<VersionCompatVerdict> {
        self.mc_version_range
            .as_deref()
            .map(|declared| check_mod_mc_version(target, declared))
    }

    /// Whether this profile applies to a server running `target`: `true` when
    /// no range is declared or the declared range contains `target`. A
    /// malformed declaration returns `false` (fail closed — an unreadable
    /// claim must not silently enable a flagship adapter).
    pub fn supports_mc_version(&self, target: McVersion) -> bool {
        match self.parsed_mc_version_range() {
            Ok(Some(range)) => range.contains(target),
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// Whether this profile applies to BOTH canonical megu targets:
    /// `1.21.1` (`DataVersion` 3955) and latest vanilla `26.3` (`DataVersion`
    /// 5023). Profiles declaring no range apply unconditionally; malformed
    /// declarations fail closed on both.
    pub fn supports_both_targets(&self) -> bool {
        self.supports_mc_version(McVersion::V1_21_1) && self.supports_mc_version(McVersion::V26_3)
    }

    /// Per-target [`VersionCompatVerdict`] for the two canonical megu
    /// targets — the diagnostic view behind
    /// [`Self::supports_both_targets`], reporting *why* a profile does not
    /// offload on a given target (minor vs major mismatch vs malformed).
    pub fn target_compatibility_summary(&self) -> TargetCompatibilitySummary {
        TargetCompatibilitySummary {
            v1_21_1: self.check_mc_version(McVersion::V1_21_1),
            v26_3: self.check_mc_version(McVersion::V26_3),
        }
    }

    /// Keep `offload` synchronized: every Create-side block gains a
    /// universal-tier mirror (kinetic sources -> network generators, consumers
    /// -> network consumers/processors, connectors -> network cables,
    /// transport/fluid blocks -> their kernel roles). Explicit `offload`
    /// entries always win.
    pub fn finalize(&mut self) {
        let sources = self.kinetic_sources.clone();
        for s in sources {
            mirror_block(
                &mut self.offload.blocks,
                &s.id,
                0.0,
                s.stress_capacity,
                UniversalBlockRole::NetworkGenerator,
            );
        }
        for c in self.kinetic_consumers.clone() {
            let role = if self.offload.processor_recipe_types.contains_key(&c.id) {
                UniversalBlockRole::Processor
            } else {
                UniversalBlockRole::NetworkConsumer
            };
            mirror_block(&mut self.offload.blocks, &c.id, c.stress_impact, 0.0, role);
        }
        for c in self.connectors.clone() {
            mirror_block(
                &mut self.offload.blocks,
                &c.id,
                0.0,
                0.0,
                UniversalBlockRole::NetworkCable,
            );
        }
        for id in self.assembly_anchors.clone() {
            mirror_block(
                &mut self.offload.blocks,
                &id,
                0.0,
                0.0,
                UniversalBlockRole::ContraptionAnchor,
            );
        }
        for id in self.decorative_blocks.clone().into_iter().chain(self.hidden_blocks.clone()) {
            mirror_block(
                &mut self.offload.blocks,
                &id,
                0.0,
                0.0,
                UniversalBlockRole::Decorative,
            );
        }
        if let Some(p) = &self.propulsion {
            for (ids, role) in [
                (&p.propeller_blocks, UniversalBlockRole::NetworkConsumer),
                (&p.buoyancy_blocks, UniversalBlockRole::Decorative),
                (&p.rudder_blocks, UniversalBlockRole::Decorative),
            ] {
                for id in ids.clone() {
                    mirror_block(&mut self.offload.blocks, &id, 0.0, 0.0, role);
                }
            }
        }
    }

    fn props_for(
        &self,
        id: &str,
        default: &dyn Fn() -> Vec<PropertyDef>,
    ) -> Vec<PropertyDef> {
        self.block_properties
            .get(id)
            .cloned()
            .unwrap_or_else(default)
    }
}

/// Append a universal-tier mirror block unless `id` is already present.
fn mirror_block(
    blocks: &mut Vec<BlockDef<UniversalBlockRole>>,
    id: &str,
    load: f64,
    capacity: f64,
    role: UniversalBlockRole,
) {
    if !blocks.iter().any(|b| b.id == id) {
        blocks.push(BlockDef {
            id: id.to_string(),
            properties: Vec::new(),
            load_factor: load,
            capacity_factor: capacity,
            role,
        });
    }
}

// ---------------------------------------------------------------------------
// Zero-JVM JAR / datapack auto-discovery
// ---------------------------------------------------------------------------

/// How a discovered addon block maps onto the simulation tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiscoveredClass {
    Source,
    Consumer,
    Processor,
    Connector(ConnectorKind),
    Belt,
    Transport,
    FluidPipe,
    FluidPump,
    FluidTank,
    Propeller,
    Buoyancy,
    Rudder,
    Anchor,
    Decorative,
}

/// Auto-discover a [`CreateAddonProfile`] for `mod_id` from data already
/// extracted by [`mod_adapter_core::data_loader`]: `data/<ns>/tags/block/*.json`
/// (and `tags/blocks/*.json`) member lists classify blocks by tag and name
/// heuristics, `assets/<ns>/blockstates/*.json` supplies real property tables,
/// and `data/<ns>/recipes/*.json` recipe types attach to discovered processors.
/// Unknown Create addons (Steam 'n' Rails, Crafts & Additions, Diesel
/// Generators, Big Cannons, Clockwork, ...) thereby register their blocks,
/// ores/chests, and mechanical/transport components without hand-authored Rust.
pub fn discover_create_addon_profile(mod_id: &str, data: &LoadedModData) -> CreateAddonProfile {
    let mut profile = CreateAddonProfile::new(mod_id);
    profile.block_properties = data
        .blockstates
        .iter()
        .filter(|(id, _)| id.starts_with(&format!("{mod_id}:")))
        .map(|(id, props)| (id.clone(), props.clone()))
        .collect();

    let mut classes: BTreeMap<String, DiscoveredClass> = BTreeMap::new();
    // Seed from every blockstate file under this namespace.
    for (id, properties) in &profile.block_properties {
        classes.insert(id.clone(), classify_properties(properties, id_path(id)));
    }
    // Fold in tag members: `data/<ns>/tags/block/*.json` and `tags/blocks/*`.
    for (tag, members) in &data.tags {
        let path = id_path(tag);
        let is_block_tag = path.starts_with("block/") || path.starts_with("blocks/");
        if !is_block_tag {
            continue;
        }
        let tag_class = classify_path(&path[6 + usize::from(path.starts_with("blocks/"))..]);
        for member in members {
            let member = member.strip_prefix('#').unwrap_or(member);
            if !member.starts_with(&format!("{mod_id}:")) {
                continue;
            }
            let member_class = profile
                .block_properties
                .get(member)
                .map(|props| classify_properties(props, id_path(member)))
                .unwrap_or_else(|| classify_path(id_path(member)));
            // A classifying tag (e.g. `<ns>:propellers`) is curated addon
            // intent and beats the bare name heuristic; a generic tag name
            // falls back to the member's own name class.
            let chosen = if tag_class != DiscoveredClass::Decorative {
                tag_class
            } else {
                member_class
            };
            classes.insert(member.to_string(), chosen);
        }
    }

    let mut spec = PropulsionSpec::aeronautics_default();
    for (id, class) in classes {
        let props = profile.block_properties.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        match class {
            DiscoveredClass::Source => {
                profile.kinetic_sources.push(KineticSourceSpec {
                    id,
                    base_rpm: single_property_value(props, "rpm")
                        .and_then(|value| value.parse::<i32>().ok())
                        .filter(|&rpm| rpm > 0)
                        .unwrap_or(64),
                    stress_capacity: positive_property_f64(props, "stress_capacity")
                        .unwrap_or(512.0),
                    axis: property_axis(props).unwrap_or(Axis::Y),
                });
            }
            DiscoveredClass::Consumer => profile.kinetic_consumers.push(KineticConsumerSpec {
                id,
                stress_impact: positive_property_f64(props, "stress_impact").unwrap_or(4.0),
                min_rpm: 1.0,
            }),
            DiscoveredClass::Processor => {
                let recipe_types: Vec<String> = data
                    .recipes
                    .iter()
                    .filter(|r| {
                        r.recipe_type.starts_with(&format!("{mod_id}:"))
                            && recipe_type_matches_block(id_path(&r.recipe_type), id_path(&id))
                    })
                    .map(|r| r.recipe_type.clone())
                    .collect();
                profile.kinetic_consumers.push(KineticConsumerSpec {
                    id: id.clone(),
                    stress_impact: positive_property_f64(props, "stress_impact").unwrap_or(4.0),
                    min_rpm: 1.0,
                });
                if !recipe_types.is_empty() {
                    profile
                        .offload
                        .processor_recipe_types
                        .insert(id.clone(), recipe_types);
                }
                profile.offload.blocks.push(BlockDef {
                    id: id.clone(),
                    properties: Vec::new(),
                    load_factor: positive_property_f64(props, "stress_impact").unwrap_or(4.0),
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::Processor,
                });
            }
            DiscoveredClass::Connector(kind) => profile.connectors.push(ConnectorSpec { id, kind }),
            DiscoveredClass::Belt => {
                profile.offload.blocks.push(BlockDef {
                    id: id.clone(),
                    properties: Vec::new(),
                    load_factor: 1.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::TransportPipe,
                });
                profile.offload.transport_delay_ticks.insert(id, 1);
            }
            DiscoveredClass::Transport => {
                profile.offload.blocks.push(BlockDef {
                    id: id.clone(),
                    properties: Vec::new(),
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::TransportPipe,
                });
                profile.offload.transport_delay_ticks.insert(id, 4);
            }
            DiscoveredClass::FluidPipe => {
                profile.offload.blocks.push(BlockDef {
                    id: id.clone(),
                    properties: Vec::new(),
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::FluidPipe,
                });
                profile.offload.tank_capacity_mb.insert(id, 1_000);
            }
            DiscoveredClass::FluidPump => {
                profile.offload.blocks.push(BlockDef {
                    id: id.clone(),
                    properties: Vec::new(),
                    load_factor: 4.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::FluidPump,
                });
                profile.offload.tank_capacity_mb.insert(id, 1_000);
            }
            DiscoveredClass::FluidTank => {
                profile.offload.blocks.push(BlockDef {
                    id: id.clone(),
                    properties: Vec::new(),
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::FluidTank,
                });
                profile.offload.tank_capacity_mb.insert(id, 8_000);
            }
            DiscoveredClass::Propeller => {
                profile.kinetic_consumers.push(KineticConsumerSpec {
                    id: id.clone(),
                    stress_impact: 8.0,
                    min_rpm: 4.0,
                });
                spec.propeller_blocks.push(id);
            }
            DiscoveredClass::Buoyancy => spec.buoyancy_blocks.push(id),
            DiscoveredClass::Rudder => spec.rudder_blocks.push(id),
            DiscoveredClass::Anchor => profile.assembly_anchors.push(id),
            DiscoveredClass::Decorative => profile.decorative_blocks.push(id),
        }
    }

    if !spec.propeller_blocks.is_empty()
        || !spec.buoyancy_blocks.is_empty()
        || !spec.rudder_blocks.is_empty()
    {
        profile.propulsion = Some(spec);
    }

    // Anti-xray hidden blocks: tag roots and name patterns mirror the bridge's
    // `mod_hidden_root_tag` / name-match rules.
    let mut hidden: BTreeSet<String> = BTreeSet::new();
    for (tag, members) in &data.tags {
        if !hidden_root_tag(tag) {
            continue;
        }
        for member in members {
            let member = member.strip_prefix('#').unwrap_or(member);
            if member.starts_with(&format!("{mod_id}:")) {
                hidden.insert(member.to_string());
            }
        }
    }
    for id in profile.block_properties.keys() {
        let path = id_path(id);
        if path.ends_with("_ore") || path.contains("chest") || path.contains("barrel") {
            hidden.insert(id.clone());
        }
    }
    profile.hidden_blocks = hidden.into_iter().collect();

    profile.finalize();
    profile
}

fn id_path(id: &str) -> &str {
    id.split_once(':').map(|(_, p)| p).unwrap_or(id)
}

/// Kinetic property values are usable as fixed simulation coefficients only
/// when the blockstate declares exactly one value. Variable speed/stress
/// properties remain in the registry for state conversion without guessing a
/// single coefficient from one arbitrary state.
fn single_property_value<'a>(props: &'a [PropertyDef], name: &str) -> Option<&'a str> {
    props
        .iter()
        .find(|prop| prop.name == name && prop.values.len() == 1)
        .map(|prop| prop.values[0].as_str())
}

fn positive_property_f64(props: &[PropertyDef], name: &str) -> Option<f64> {
    single_property_value(props, name)
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
}

fn property_axis(props: &[PropertyDef]) -> Option<Axis> {
    single_property_value(props, "axis")
        .and_then(Axis::from_name)
        .or_else(|| match single_property_value(props, "facing")? {
            "east" | "west" => Some(Axis::X),
            "up" | "down" => Some(Axis::Y),
            "north" | "south" => Some(Axis::Z),
            _ => None,
        })
}

fn classify_properties(props: &[PropertyDef], path: &str) -> DiscoveredClass {
    let name_class = classify_path(path);
    if positive_property_f64(props, "stress_capacity").is_some()
        || single_property_value(props, "rpm")
            .and_then(|value| value.parse::<i32>().ok())
            .is_some_and(|rpm| rpm > 0)
    {
        DiscoveredClass::Source
    } else if positive_property_f64(props, "stress_impact").is_some()
        && name_class == DiscoveredClass::Decorative
    {
        DiscoveredClass::Consumer
    } else {
        name_class
    }
}

/// Match an addon recipe type path to its processing block path by Create's
/// `block -> "<stem>ing"` convention (`rolling_mill`/`rolling`, `grinder`/
/// `grinding`, `mixer`/`mixing`, `press`/`pressing`, `crusher`/`crushing`).
fn recipe_type_matches_block(recipe_path: &str, block_path: &str) -> bool {
    if block_path.starts_with(recipe_path) || recipe_path.starts_with(block_path) {
        return true;
    }
    let stem = recipe_path.strip_suffix("ing").unwrap_or(recipe_path);
    !stem.is_empty() && (block_path.starts_with(stem) || stem.starts_with(block_path))
}

/// Name-heuristic classifier for unknown addon block paths. Order matters:
/// compound names (`propeller_bearing`, `large_cogwheel`, `gearbox`) are
/// checked before their generic substrings.
fn classify_path(path: &str) -> DiscoveredClass {
    let has = |needle: &str| path.contains(needle);
    if has("propeller") || has("thruster") {
        DiscoveredClass::Propeller
    } else if has("balloon") || has("envelope") || has("blimp") || has("levitat") || has("float") {
        DiscoveredClass::Buoyancy
    } else if has("rudder") || has("helm") {
        DiscoveredClass::Rudder
    } else if has("assembler") || has("bearing") {
        DiscoveredClass::Anchor
    } else if has("gearbox") {
        DiscoveredClass::Connector(ConnectorKind::Gearbox)
    } else if has("gearshift") {
        DiscoveredClass::Connector(ConnectorKind::Gearshift)
    } else if has("clutch") {
        DiscoveredClass::Connector(ConnectorKind::Clutch)
    } else if has("large_cogwheel") || has("large_cog") {
        DiscoveredClass::Connector(ConnectorKind::LargeCogwheel)
    } else if has("cogwheel") || has("cog") || has("gear") {
        DiscoveredClass::Connector(ConnectorKind::Cogwheel)
    } else if has("shaft") || has("axle") || has("driveshaft") {
        DiscoveredClass::Connector(ConnectorKind::Shaft)
    } else if has("motor") || has("engine") || has("energiser") || has("flywheel") {
        DiscoveredClass::Source
    } else if has("pump") {
        DiscoveredClass::FluidPump
    } else if has("pipe") {
        DiscoveredClass::FluidPipe
    } else if has("tank") || has("reservoir") {
        DiscoveredClass::FluidTank
    } else if has("belt") {
        DiscoveredClass::Belt
    } else if has("chute") || has("funnel") || has("depot") || has("hopper") {
        DiscoveredClass::Transport
    } else if has("crusher") || has("mill") || has("mixer") || has("press") || has("grinder")
        || has("saw") || has("roller")
    {
        DiscoveredClass::Processor
    } else if has("alternator") || has("coil") || has("fan") || has("brushes") {
        DiscoveredClass::Consumer
    } else {
        DiscoveredClass::Decorative
    }
}

/// Ore/chest tag roots the bridge folds into the anti-xray hidden set
/// (`#c:ores`, `#forge:ores`, `#c:chests`, `#forge:chests`, `*:barrels`).
fn hidden_root_tag(tag: &str) -> bool {
    let path = id_path(tag);
    let leaf = path.rsplit('/').next().unwrap_or(path);
    (path.starts_with("block/") || path.starts_with("blocks/"))
        && matches!(leaf, "ores" | "chests" | "barrels")
}

// ---------------------------------------------------------------------------
// Stored-JAR auto-discovery
// ---------------------------------------------------------------------------

/// Result of [`discover_addon_from_jar`]: the discovered profile plus the
/// count of matching ZIP entries the scanner could not decode.
#[derive(Clone, Debug)]
pub struct DiscoveredAddonJar {
    pub profile: CreateAddonProfile,
    /// `data/`/`assets/` JAR entries using a compression method other than
    /// Stored (0) or Deflate (8). A nonzero count means the profile may be
    /// incomplete — callers should treat it as a soft warning, never a hard
    /// failure. Production mod JARs (all-Deflate) report `0`.
    pub skipped_compressed_entries: usize,
}

/// Namespaces a [`LoadedModData`] scan should treat as addon candidates:
/// every namespace owning blockstates, tag files or tag members, or recipe
/// types — minus the conventional shared namespaces (`c`, `forge`,
/// `neoforge`, `fabric`, `minecraft`) that hold tag registries rather than
/// block inventories, and `create` itself (already in the core registry).
pub fn discover_addon_namespaces(data: &LoadedModData) -> BTreeSet<String> {
    const SHARED: [&str; 6] = ["minecraft", "c", "forge", "neoforge", "fabric", "create"];
    let mut out = BTreeSet::new();
    let mut push_ns = |id: &str| {
        if let Some(ns) = id.split(':').next() {
            if !SHARED.contains(&ns) {
                out.insert(ns.to_string());
            }
        }
    };
    for id in data.blockstates.keys() {
        push_ns(id);
    }
    for (tag, members) in &data.tags {
        push_ns(tag);
        for member in members {
            push_ns(member.strip_prefix('#').unwrap_or(member));
        }
    }
    for recipe in &data.recipes {
        push_ns(&recipe.recipe_type);
    }
    out
}

/// Discover a [`CreateAddonProfile`] for `mod_id` directly from a JAR/ZIP
/// byte slice — Stored *and* Deflate (`method == 8`) entries, no
/// intermediate directory, no hardcoded addon names.
///
/// Sources, all under `data/<mod_id>/`:
/// - `recipe/` + `recipes/` (recursive): sequenced-assembly, pressing,
///   mixing, milling, crushing, cutting, filling, and every Create 1.21.1
///   machine family (compacting, splashing, haunting, deploying, emptying,
///   item_application, sandpaper_polishing, mechanical_crafting) plus the
///   generic `<ns>:<machine>` fallback land in
///   [`CreateAddonProfile::processing_recipes`] via the same fail-closed
///   parser as [`discover_addon_processing_recipes`].
/// - `create_kinetics/` (or `create_kinetics.json`): partial
///   [`CreateAddonProfile::from_json`] documents supplying real per-block
///   kinetic stress/capacity specs (`kinetic_sources`, `kinetic_consumers`,
///   `connectors`, `block_properties`, `propulsion` lists). Applied in
///   sorted-path order, last write wins per block id.
///
/// Blockstates (`assets/<ns>/blockstates/*.json`), tags, and generic recipes
/// flow through [`crate::data_loader::load_mod_data_from_zip`] — the bounded
/// Stored+Deflate scanner shared with the datapack loader — into
/// [`discover_create_addon_profile`], so directional `axis`/`facing` (and
/// every other declared blockstate property) drives real registry expansion.
/// Malformed JSON, oversized entries, or an invalid namespace fail closed.
pub fn discover_addon_from_jar(
    jar: &[u8],
    mod_id: &str,
) -> Result<DiscoveredAddonJar, DataLoadError> {
    if mod_id.is_empty()
        || mod_id.contains(':')
        || mod_id.contains('/')
        || mod_id.chars().any(char::is_whitespace)
    {
        return Err(DataLoadError::Json(format!(
            "invalid addon namespace '{mod_id}'"
        )));
    }
    let mut ids = ItemFluidIdTable::new();
    let (data, scanned) = crate::data_loader::load_mod_data_from_zip(jar, &mut ids)?;
    let skipped_compressed = scanned.skipped_compressed;
    let mut profile = discover_create_addon_profile(mod_id, &data);

    let mut processing = Vec::new();
    for (path, bytes) in &scanned.entries {
        if is_addon_recipe_entry(path, mod_id) {
            let recipe_path = path
                .split_once("/recipe/")
                .or_else(|| path.split_once("/recipes/"))
                .map(|(_, rest)| rest)
                .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path));
            let stem = recipe_path.strip_suffix(".json").unwrap_or(recipe_path);
            let id = format!("{mod_id}:{stem}");
            let json = parse_json(bytes)
                .map_err(|e| DataLoadError::Json(format!("{path}: {e:?}")))?;
            let obj = json
                .as_object()
                .ok_or_else(|| DataLoadError::Json(format!("{path}: recipe root must be object")))?;
            if let Some(recipe) = parse_addon_processing_object(&id, obj)
                .map_err(|e| DataLoadError::Json(format!("{path}: {e}")))?
            {
                processing.push(recipe);
            }
        } else if is_addon_kinetics_entry(path, mod_id) {
            let overrides = CreateAddonProfile::from_json(bytes)
                .map_err(|e| DataLoadError::Json(format!("{path}: {e:?}")))?;
            if overrides.mod_id != mod_id {
                return Err(DataLoadError::Json(format!(
                    "{path}: kinetics document declares mod_id '{}' for namespace '{mod_id}'",
                    overrides.mod_id
                )));
            }
            merge_kinetic_overrides(&mut profile, overrides);
        }
    }
    profile.processing_recipes = processing;
    profile.finalize();
    Ok(DiscoveredAddonJar {
        profile,
        skipped_compressed_entries: skipped_compressed,
    })
}

/// Whether a JAR entry path is a recipe JSON for `mod_id`
/// (`data/<ns>/recipe/**` or `data/<ns>/recipes/**`, any depth).
fn is_addon_recipe_entry(path: &str, mod_id: &str) -> bool {
    path.starts_with(&format!("data/{mod_id}/recipe/"))
        || path.starts_with(&format!("data/{mod_id}/recipes/"))
}

/// Whether a JAR entry path carries kinetic stress/capacity config for
/// `mod_id` (`data/<ns>/create_kinetics.json` or anything under
/// `data/<ns>/create_kinetics/`).
fn is_addon_kinetics_entry(path: &str, mod_id: &str) -> bool {
    path == format!("data/{mod_id}/create_kinetics.json")
        || path.starts_with(&format!("data/{mod_id}/create_kinetics/"))
}

/// Merge one `create_kinetics` override document into a discovered profile:
/// per-id last-wins for kinetic sources/consumers/connectors, union for
/// propulsion block lists and decorative/anchor/hidden sets, and
/// `block_properties`/`mc_version_range` override wholesale.
fn merge_kinetic_overrides(profile: &mut CreateAddonProfile, over: CreateAddonProfile) {
    fn upsert_by_id<T>(list: &mut Vec<T>, item: T, id: &str, get: impl Fn(&T) -> &str) {
        match list.iter_mut().find(|e| get(e) == id) {
            Some(slot) => *slot = item,
            None => list.push(item),
        }
    }
    for spec in over.kinetic_sources {
        let id = spec.id.clone();
        upsert_by_id(&mut profile.kinetic_sources, spec, &id, |s| &s.id);
    }
    for spec in over.kinetic_consumers {
        let id = spec.id.clone();
        upsert_by_id(&mut profile.kinetic_consumers, spec, &id, |s| &s.id);
    }
    for spec in over.connectors {
        let id = spec.id.clone();
        upsert_by_id(&mut profile.connectors, spec, &id, |s| &s.id);
    }
    profile.block_properties.extend(over.block_properties);
    if let Some(p) = over.propulsion {
        match &mut profile.propulsion {
            Some(existing) => {
                for (src, dst) in [
                    (&p.propeller_blocks, &mut existing.propeller_blocks),
                    (&p.buoyancy_blocks, &mut existing.buoyancy_blocks),
                    (&p.rudder_blocks, &mut existing.rudder_blocks),
                ] {
                    for id in src {
                        if !dst.contains(id) {
                            dst.push(id.clone());
                        }
                    }
                }
            }
            None => profile.propulsion = Some(p),
        }
    }
    for (src, dst) in [
        (&over.assembly_anchors, &mut profile.assembly_anchors),
        (&over.decorative_blocks, &mut profile.decorative_blocks),
        (&over.hidden_blocks, &mut profile.hidden_blocks),
    ] {
        for id in src {
            if !dst.contains(id) {
                dst.push(id.clone());
            }
        }
    }
    if over.mc_version_range.is_some() {
        profile.mc_version_range = over.mc_version_range;
    }
}

// ---------------------------------------------------------------------------
// Registry / factory integration
// ---------------------------------------------------------------------------

/// Build the flagship Create block registry with addon definitions merged in
/// after the core `create:*` catalog. Duplicate block ids fail closed via
/// [`RegistryError::DuplicateBlock`].
pub fn create_registry_with_addons(
    base_state_id: u32,
    addons: &[CreateAddonProfile],
) -> Result<CreateBlockRegistry, RegistryError> {
    let mut defs = core_create_block_defs();
    for addon in addons {
        defs.extend(addon.block_defs());
    }
    BlockRegistry::build_with_aliases(base_state_id, defs, &core_create_block_aliases())
}

/// Merge every addon's universal `offload` half into one
/// [`ModOffloadProfile`] (`mod_id` = `"create_addons"`) for
/// [`mod_adapter_core::profile::UniversalModRuntime`].
pub fn merged_offload_profile(addons: &[CreateAddonProfile]) -> ModOffloadProfile {
    let mut merged = ModOffloadProfile {
        mod_id: "create_addons".to_string(),
        blocks: Vec::new(),
        transport_delay_ticks: BTreeMap::new(),
        tank_capacity_mb: BTreeMap::new(),
        processor_recipe_types: BTreeMap::new(),
    };
    for addon in addons {
        merged.extend(&addon.offload);
    }
    merged
}

fn json_obj<'a>(
    value: &'a JsonValue,
    what: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, DataLoadError> {
    value
        .as_object()
        .ok_or_else(|| DataLoadError::Json(format!("{what} must be object")))
}

fn json_str<'a>(
    obj: &'a BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<&'a str, DataLoadError> {
    obj.get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| DataLoadError::Json(format!("missing string '{key}'")))
}

fn json_f64(obj: &BTreeMap<String, JsonValue>, key: &str) -> Option<f64> {
    obj.get(key).and_then(JsonValue::as_f64)
}

fn axis_from_name(name: &str) -> Axis {
    Axis::from_name(name).unwrap_or(match name {
        "east" | "west" => Axis::X,
        "north" | "south" => Axis::Z,
        _ => Axis::Y,
    })
}

// ---------------------------------------------------------------------------
// Built-in reference profiles
// ---------------------------------------------------------------------------

/// Minecraft compatibility declared by every built-in reference profile:
/// the 1.21.x release line starting at 1.21.1 (Maven interval syntax, parsed
/// by [`VersionRange`]). Lets callers gate profile applicability across
/// 1.21.1/1.21.3/1.21.4/1.21.5+ via [`CreateAddonProfile::supports_mc_version`].
pub const BUILTIN_ADDON_MC_VERSION_RANGE: &str = "[1.21.1,1.22)";

/// Declared Minecraft compatibility spanning BOTH canonical megu
/// targets: the `1.21.x` line from 1.21.1 plus the `26.3.x` band, written as
/// a NeoForge Maven union interval (parsed by [`VersionRange`]). Profiles
/// carrying this declaration satisfy
/// [`CreateAddonProfile::supports_both_targets`].
pub const DUAL_TARGET_MC_VERSION_RANGE: &str = "[1.21.1,1.22),[26.3,26.4)";

/// Create: Aeronautics (`createaeronautics`): airship assembly anchor,
/// propeller bearings/thrusters, balloon envelopes, and rudder/helm control.
pub fn createaeronautics_profile() -> CreateAddonProfile {
    let mut p = CreateAddonProfile::new("createaeronautics");
    p.mc_version_range = Some(BUILTIN_ADDON_MC_VERSION_RANGE.to_string());
    p.assembly_anchors = vec!["createaeronautics:airship_assembler".into()];
    p.kinetic_sources = vec![KineticSourceSpec {
        id: "createaeronautics:propeller_engine".into(),
        base_rpm: 64,
        stress_capacity: 2048.0,
        axis: Axis::X,
    }];
    p.kinetic_consumers = vec![
        KineticConsumerSpec {
            id: "createaeronautics:propeller_bearing".into(),
            stress_impact: 8.0,
            min_rpm: 4.0,
        },
        KineticConsumerSpec {
            id: "createaeronautics:gyroscopic_propeller_bearing".into(),
            stress_impact: 8.0,
            min_rpm: 1.0,
        },
        KineticConsumerSpec {
            id: "createaeronautics:torsion_spring".into(),
            stress_impact: 4.0,
            min_rpm: 0.0,
        },
        KineticConsumerSpec {
            id: "createaeronautics:stationary_potato_cannon".into(),
            stress_impact: 2.0,
            min_rpm: 0.0,
        },
    ];
    p.decorative_blocks = vec!["createaeronautics:compass_table".into()];
    let mut spec = PropulsionSpec::aeronautics_default();
    spec.propeller_blocks = vec![
        "createaeronautics:propeller_bearing".into(),
        "createaeronautics:gyroscopic_propeller_bearing".into(),
        "createaeronautics:propeller_engine".into(),
    ];
    spec.buoyancy_blocks = vec![
        "createaeronautics:balloon_envelope".into(),
        "createaeronautics:levitation_core".into(),
    ];
    spec.rudder_blocks = vec!["createaeronautics:rudder".into()];
    p.propulsion = Some(spec);
    p.block_properties = BTreeMap::from([
        (
            "createaeronautics:propeller_bearing".to_string(),
            vec![PropertyDef::new(
                "facing",
                &["north", "south", "east", "west", "up", "down"],
            )],
        ),
        (
            "createaeronautics:propeller_engine".to_string(),
            vec![PropertyDef::new("axis", &["x", "y", "z"])],
        ),
    ]);
    p.finalize();
    p
}

/// Create: Steam 'n' Rails (`railways`): trackside signals, couplers and
/// conductor blocks (all non-kinetic, decorative on the flagship tier).
pub fn railways_profile() -> CreateAddonProfile {
    let mut p = CreateAddonProfile::new("railways");
    p.mc_version_range = Some(BUILTIN_ADDON_MC_VERSION_RANGE.to_string());
    p.decorative_blocks = vec![
        "railways:semaphore".into(),
        "railways:track_coupler".into(),
        "railways:conductor_whistle".into(),
        "railways:yellow_conductor_cap".into(),
    ];
    p.offload.blocks.push(BlockDef {
        id: "railways:portable_fuel_interface".into(),
        properties: Vec::new(),
        load_factor: 0.0,
        capacity_factor: 0.0,
        role: UniversalBlockRole::FluidTank,
    });
    p.offload
        .tank_capacity_mb
        .insert("railways:portable_fuel_interface".into(), 8_000);
    p.finalize();
    p
}

/// Create: Crafts & Additions (`createaddition`): electric motor source,
/// alternator/tesla coil consumers, rolling mill processor, connectors.
pub fn createaddition_profile() -> CreateAddonProfile {
    let mut p = CreateAddonProfile::new("createaddition");
    p.mc_version_range = Some(BUILTIN_ADDON_MC_VERSION_RANGE.to_string());
    p.kinetic_sources = vec![KineticSourceSpec {
        id: "createaddition:electric_motor".into(),
        base_rpm: 96,
        stress_capacity: 4096.0,
        axis: Axis::Y,
    }];
    p.kinetic_consumers = vec![
        KineticConsumerSpec {
            id: "createaddition:alternator".into(),
            stress_impact: 8.0,
            min_rpm: 16.0,
        },
        KineticConsumerSpec {
            id: "createaddition:tesla_coil".into(),
            stress_impact: 16.0,
            min_rpm: 1.0,
        },
        KineticConsumerSpec {
            id: "createaddition:rolling_mill".into(),
            stress_impact: 4.0,
            min_rpm: 1.0,
        },
    ];
    p.decorative_blocks = vec![
        "createaddition:connector".into(),
        "createaddition:accumulator".into(),
        "createaddition:modular_accumulator".into(),
        "createaddition:digital_adapter".into(),
    ];
    p.offload
        .processor_recipe_types
        .insert("createaddition:rolling_mill".into(), vec!["createaddition:rolling".into()]);
    p.finalize();
    p
}

/// Create: New Age (`create_new_age`): tiered energiser sources and
/// generator-coil / carbon-brush consumers.
pub fn create_new_age_profile() -> CreateAddonProfile {
    let mut p = CreateAddonProfile::new("create_new_age");
    p.mc_version_range = Some(BUILTIN_ADDON_MC_VERSION_RANGE.to_string());
    p.kinetic_sources = vec![
        KineticSourceSpec {
            id: "create_new_age:energiser_t1".into(),
            base_rpm: 64,
            stress_capacity: 2048.0,
            axis: Axis::Y,
        },
        KineticSourceSpec {
            id: "create_new_age:energiser_t2".into(),
            base_rpm: 128,
            stress_capacity: 8192.0,
            axis: Axis::Y,
        },
        KineticSourceSpec {
            id: "create_new_age:energiser_t3".into(),
            base_rpm: 256,
            stress_capacity: 32_768.0,
            axis: Axis::Y,
        },
    ];
    p.kinetic_consumers = vec![
        KineticConsumerSpec {
            id: "create_new_age:generator_coil".into(),
            stress_impact: 8.0,
            min_rpm: 1.0,
        },
        KineticConsumerSpec {
            id: "create_new_age:carbon_brushes".into(),
            stress_impact: 2.0,
            min_rpm: 1.0,
        },
    ];
    p.decorative_blocks = vec!["create_new_age:magnetite_block".into()];
    p.finalize();
    p
}

/// All built-in Create addon reference profiles in deterministic order.
pub fn builtin_create_addon_profiles() -> Vec<CreateAddonProfile> {
    vec![
        createaeronautics_profile(),
        createaddition_profile(),
        create_new_age_profile(),
        railways_profile(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_loader::ItemFluidIdTable;
    use mod_adapter_core::data_loader::load_from_stored_zip;

    fn build_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in entries {
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn builtin_profiles_merge_into_create_registry_and_offload() {
        let addons = builtin_create_addon_profiles();
        assert_eq!(
            addons.iter().map(|a| a.mod_id.as_str()).collect::<Vec<_>>(),
            ["createaeronautics", "createaddition", "create_new_age", "railways"]
        );
        let reg = create_registry_with_addons(
            crate::blockstates::DEFAULT_BASE_STATE_ID,
            &addons,
        )
        .unwrap();
        for id in [
            "createaeronautics:airship_assembler",
            "createaeronautics:propeller_bearing",
            "createaeronautics:balloon_envelope",
            "createaeronautics:rudder",
            "railways:semaphore",
            "createaddition:electric_motor",
            "createaddition:alternator",
            "create_new_age:energiser_t1",
            "create_new_age:magnetite_block",
        ] {
            assert!(reg.default_state(id).is_some(), "missing {id}");
        }
        // Universal mirror exposes the same addon to the generic tier.
        let merged = merged_offload_profile(&addons);
        let ids: BTreeSet<&str> = merged.blocks.iter().map(|b| b.id.as_str()).collect();
        assert!(ids.contains("createaddition:electric_motor"));
        assert!(ids.contains("createaeronautics:propeller_bearing"));
        assert_eq!(
            merged.processor_recipe_types["createaddition:rolling_mill"],
            vec!["createaddition:rolling".to_string()]
        );
    }

    #[test]
    fn json_profile_round_trips_create_specific_sections() {
        let profile = CreateAddonProfile::from_json(br#"{
          "mod_id": "democaddon",
          "blocks": [
            {"id": "democaddon:copper_pipe", "role": "fluid_pipe", "tank_capacity_mb": 500}
          ],
          "kinetic_sources": [{"id": "democaddon:motor", "rpm": 128, "capacity": 1024.0, "axis": "z"}],
          "kinetic_consumers": [{"id": "democaddon:mixer", "impact": 6.0, "min_rpm": 8.0}],
          "connectors": [{"id": "democaddon:chain_drive", "kind": "shaft"}],
          "assembly_anchors": ["democaddon:assembler"],
          "hidden_blocks": ["democaddon:tin_ore"],
          "propulsion": {
            "thrust_per_rpm": 42.0,
            "propeller_blocks": ["democaddon:fan_prop"],
            "buoyancy_blocks": ["democaddon:gas_cell"],
            "rudder_blocks": ["democaddon:tail_fin"]
          }
        }"#)
        .unwrap();
        assert_eq!(profile.mod_id, "democaddon");
        assert_eq!(profile.kinetic_sources[0].base_rpm, 128);
        assert_eq!(profile.kinetic_consumers[0].min_rpm, 8.0);
        assert_eq!(profile.connectors[0].kind, ConnectorKind::Shaft);
        let prop = profile.propulsion.as_ref().unwrap();
        assert_eq!(prop.thrust_per_rpm, 42.0);
        assert_eq!(prop.buoyancy_blocks, ["democaddon:gas_cell"]);
        // Offload mirror gained every Create-side block.
        let mirror: BTreeSet<&str> = profile.offload.blocks.iter().map(|b| b.id.as_str()).collect();
        for id in [
            "democaddon:copper_pipe",
            "democaddon:motor",
            "democaddon:mixer",
            "democaddon:chain_drive",
            "democaddon:assembler",
            "democaddon:tin_ore",
            "democaddon:fan_prop",
        ] {
            assert!(mirror.contains(id), "mirror missing {id}");
        }
    }

    #[test]
    fn builtin_profiles_declare_mc_version_range_across_1_21_x() {
        for profile in builtin_create_addon_profiles() {
            assert_eq!(
                profile.mc_version_range.as_deref(),
                Some(BUILTIN_ADDON_MC_VERSION_RANGE),
                "profile {}",
                profile.mod_id
            );
            for target in [
                McVersion::V1_21_1,
                McVersion::V1_21_3,
                McVersion::V1_21_4,
                McVersion::V1_21_5,
            ] {
                assert!(
                    profile.supports_mc_version(target),
                    "profile {} must apply to {target}",
                    profile.mod_id
                );
                assert_eq!(
                    profile.check_mc_version(target),
                    Some(VersionCompatVerdict::Compatible)
                );
            }
            // Other release lines and a hypothetical 1.22 do not apply.
            assert!(!profile.supports_mc_version(McVersion::V1_20_4));
            assert!(!profile.supports_mc_version(McVersion::V1_20_6));
            assert!(!profile.supports_mc_version(McVersion::new(1, 22, 0)));
            assert!(!profile.supports_mc_version(McVersion::V1_21_0));
        }
    }

    #[test]
    fn aeronautics_profile_version_verdicts_distinguish_minor_and_major() {
        let aero = createaeronautics_profile();
        // `[1.21.1,1.22)` excludes 1.21.0 but shares its 1.21.x line -> minor.
        assert_eq!(
            aero.check_mc_version(McVersion::V1_21_0),
            Some(VersionCompatVerdict::MinorMismatch {
                target: McVersion::V1_21_0,
                declared: BUILTIN_ADDON_MC_VERSION_RANGE.to_string(),
            })
        );
        // A 1.20.x server is a different release line entirely -> major.
        assert_eq!(
            aero.check_mc_version(McVersion::V1_20_4),
            Some(VersionCompatVerdict::MajorMismatch {
                target: McVersion::V1_20_4,
                declared: BUILTIN_ADDON_MC_VERSION_RANGE.to_string(),
            })
        );
        // The declared range really parses back into a usable VersionRange.
        let range = aero.parsed_mc_version_range().unwrap().unwrap();
        assert!(range.contains(McVersion::V1_21_4));
        assert!(!range.contains(McVersion::V1_20_6));
    }

    #[test]
    fn profile_version_range_from_json_and_fail_closed_edges() {
        // `mc_version_range` round-trips through declarative profile JSON.
        let profile = CreateAddonProfile::from_json(
            br#"{
          "mod_id": "rangecheck",
          "mc_version_range": ">=1.21.1 <1.21.5",
          "blocks": [{"id": "rangecheck:pipe", "role": "transport_pipe"}]
        }"#,
        )
        .unwrap();
        assert_eq!(
            profile.mc_version_range.as_deref(),
            Some(">=1.21.1 <1.21.5")
        );
        assert!(profile.supports_mc_version(McVersion::V1_21_4));
        assert!(!profile.supports_mc_version(McVersion::V1_21_5));

        // Undeclared profile: no gate, every target applies.
        let mut open = CreateAddonProfile::new("noversion");
        assert!(open.supports_mc_version(McVersion::V1_20_4));
        assert!(open.supports_mc_version(McVersion::new(9, 9, 9)));
        assert!(open.check_mc_version(McVersion::V1_21_1).is_none());
        assert!(open.parsed_mc_version_range().unwrap().is_none());

        // Malformed declaration fails closed: unparseable, unsupported,
        // and the verdict carries the parse reason.
        open.mc_version_range = Some("[unterminated".to_string());
        assert!(!open.supports_mc_version(McVersion::V1_21_1));
        assert!(open.parsed_mc_version_range().is_err());
        assert!(matches!(
            open.check_mc_version(McVersion::V1_21_1),
            Some(VersionCompatVerdict::InvalidRange { .. })
        ));
    }

    #[test]
    fn dual_target_profile_fixture_supports_both_1_21_1_and_26_3() {
        // Declarative dual-target addon manifest (tests/fixtures) declaring
        // the Maven union `[1.21.1,1.22),[26.3,26.4)`.
        let profile = CreateAddonProfile::from_json(include_bytes!(
            "../tests/fixtures/create_dual_target_addon.json"
        ))
        .unwrap();
        assert_eq!(
            profile.mc_version_range.as_deref(),
            Some(DUAL_TARGET_MC_VERSION_RANGE)
        );

        // Both canonical targets accept the profile; neighbors inside the
        // declared bands do too, versions outside either band do not.
        assert!(profile.supports_both_targets());
        assert_eq!(
            profile.check_mc_version(McVersion::V1_21_1),
            Some(VersionCompatVerdict::Compatible)
        );
        assert_eq!(
            profile.check_mc_version(McVersion::V26_3),
            Some(VersionCompatVerdict::Compatible)
        );
        assert!(profile.supports_mc_version(McVersion::V1_21_5));
        assert!(profile.supports_mc_version(McVersion::new(26, 3, 7)));
        assert!(!profile.supports_mc_version(McVersion::new(26, 4, 0)));
        assert!(!profile.supports_mc_version(McVersion::new(1, 22, 0)));
        assert!(!profile.supports_mc_version(McVersion::V1_20_6));
        assert!(!profile.supports_mc_version(McVersion::new(27, 0, 0)));

        // The summary reports Compatible on both targets verbatim.
        let summary = profile.target_compatibility_summary();
        assert_eq!(summary.v1_21_1, Some(VersionCompatVerdict::Compatible));
        assert_eq!(summary.v26_3, Some(VersionCompatVerdict::Compatible));
        assert!(summary.compatible_on_both());

        // Dual-target Create-side blocks merge into the flagship registry
        // and the universal offload mirror alongside the core catalog.
        let reg = create_registry_with_addons(
            crate::blockstates::DEFAULT_BASE_STATE_ID,
            std::slice::from_ref(&profile),
        )
        .unwrap();
        assert!(reg.default_state("createdual:aero_motor").is_some());
        assert!(reg.default_state("createdual:propeller_bearing").is_some());
        assert!(reg.default_state("create:shaft").is_some());

        // The built-in 1.21.x profiles are deliberately single-target: they
        // reject 26.3 as a major mismatch, not a silent pass.
        for builtin in builtin_create_addon_profiles() {
            assert!(
                !builtin.supports_both_targets(),
                "profile {}",
                builtin.mod_id
            );
            let s = builtin.target_compatibility_summary();
            assert_eq!(s.v1_21_1, Some(VersionCompatVerdict::Compatible));
            assert!(matches!(
                s.v26_3,
                Some(VersionCompatVerdict::MajorMismatch { .. })
            ));
            assert!(!s.compatible_on_both());
        }

        // An undeclared range applies unconditionally on both targets; a
        // malformed one fails closed on both.
        let mut open = CreateAddonProfile::new("noversion");
        assert!(open.supports_both_targets());
        assert!(open.target_compatibility_summary().compatible_on_both());
        open.mc_version_range = Some("[unterminated".to_string());
        assert!(!open.supports_both_targets());
        let s = open.target_compatibility_summary();
        assert!(!s.compatible_on_both());
        assert!(matches!(
            s.v26_3,
            Some(VersionCompatVerdict::InvalidRange { .. })
        ));
    }

    #[test]
    fn unknown_addon_jar_autodiscovers_kinetics_transport_and_hidden_blocks() {
        // Synthetic unknown addon "skyaeronaut": tags + blockstates + recipes.
        let jar = build_stored_zip(&[
            (
                "data/skyaeronaut/tags/block/propellers.json",
                br#"{"values": ["skyaeronaut:rotor_bearing"]}"#,
            ),
            (
                "data/skyaeronaut/tags/blocks/buoyant.json",
                br#"{"values": ["skyaeronaut:gas_envelope"]}"#,
            ),
            (
                "data/c/tags/block/ores.json",
                br#"{"values": ["skyaeronaut:sky_iron_ore", "othermod:ore"]}"#,
            ),
            (
                "assets/skyaeronaut/blockstates/rotor_bearing.json",
                br#"{"variants": {"facing=north": {}, "facing=up": {}, "facing=down": {}, "facing=east": {}, "facing=west": {}, "facing=south": {}}}"#,
            ),
            (
                "assets/skyaeronaut/blockstates/driveshaft.json",
                br#"{"variants": {"axis=x": {}, "axis=y": {}, "axis=z": {}}}"#,
            ),
            (
                "assets/skyaeronaut/blockstates/aero_motor.json",
                br#"{"variants": {"axis=x": {}, "axis=y": {}, "axis=z": {}}}"#,
            ),
            (
                "assets/skyaeronaut/blockstates/fluid_tank.json",
                br#"{"variants": {"": {}}}"#,
            ),
            (
                "assets/skyaeronaut/blockstates/grinder.json",
                br#"{"variants": {"": {}}}"#,
            ),
            (
                "data/skyaeronaut/recipes/grinding/dust.json",
                br#"{"type": "skyaeronaut:grinding", "ingredients": [{"item": "minecraft:iron_ore"}], "results": [{"id": "skyaeronaut:iron_dust", "count": 2}]}"#,
            ),
        ]);
        let mut ids = ItemFluidIdTable::new();
        let data = load_from_stored_zip(&jar, &mut ids).unwrap();
        let profile = discover_create_addon_profile("skyaeronaut", &data);

        // Tag-driven propeller + blockstate-driven shaft/motor/tank/grinder.
        let propulsion = profile
            .propulsion
            .as_ref()
            .expect("propeller tag -> propulsion spec");
        assert_eq!(propulsion.propeller_blocks, ["skyaeronaut:rotor_bearing"]);
        assert_eq!(propulsion.buoyancy_blocks, ["skyaeronaut:gas_envelope"]);
        assert!(profile
            .connectors
            .iter()
            .any(|c| c.id == "skyaeronaut:driveshaft" && c.kind == ConnectorKind::Shaft));
        assert!(profile
            .kinetic_sources
            .iter()
            .any(|s| s.id == "skyaeronaut:aero_motor"));
        assert!(profile
            .offload
            .blocks
            .iter()
            .any(|b| b.id == "skyaeronaut:fluid_tank" && b.role == UniversalBlockRole::FluidTank));
        // Grinder matched its `<ns>:grinding` recipe type.
        assert_eq!(
            profile.offload.processor_recipe_types["skyaeronaut:grinder"],
            vec!["skyaeronaut:grinding".to_string()]
        );
        // Ore folded into hidden set; othermod ore ignored.
        assert_eq!(profile.hidden_blocks, ["skyaeronaut:sky_iron_ore"]);
        // Discovered blockstate properties drive registry expansion.
        assert_eq!(
            profile.block_properties["skyaeronaut:rotor_bearing"],
            vec![PropertyDef::new(
                "facing",
                &["down", "east", "north", "south", "up", "west"]
            )]
        );
        // Registry accepts the discovered blocks and expands real properties.
        let reg = create_registry_with_addons(
            crate::blockstates::DEFAULT_BASE_STATE_ID,
            &[profile],
        )
        .unwrap();
        let rotor = reg.default_state("skyaeronaut:rotor_bearing").unwrap();
        assert_eq!(
            reg.encode_canonical(rotor),
            Some("skyaeronaut:rotor_bearing[facing=down]")
        );
        assert!(reg.default_state("skyaeronaut:driveshaft").is_some());
    }
}
