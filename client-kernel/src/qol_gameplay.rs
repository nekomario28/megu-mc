// SPDX-License-Identifier: LGPL-3.0-or-later
//! Pure client-side QoL projections. The caller owns game state, localization,
//! installed-mod detection, and the execution of inventory transfer plans.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use crate::raycast::{EntityRayHit, RayHit};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarvestTool {
    Pickaxe,
    Axe,
    Shovel,
    Hoe,
}

impl HarvestTool {
    const fn label(self) -> &'static str {
        match self {
            Self::Pickaxe => "Pickaxe",
            Self::Axe => "Axe",
            Self::Shovel => "Shovel",
            Self::Hoe => "Hoe",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CropGrowth {
    pub age: u32,
    pub max_age: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContainerFill {
    pub occupied_slots: usize,
    pub total_slots: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FurnaceProgress {
    pub completed: u32,
    pub total: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LookingAtBlock<'a> {
    pub hit: RayHit,
    pub block_id: &'a str,
    /// Localized display name from the caller's resource/translation registry.
    pub display_name: Option<&'a str>,
    pub properties: &'a [(&'a str, &'a str)],
    pub harvestable: Option<bool>,
    pub preferred_tool: Option<HarvestTool>,
    /// Breaking fraction in [0, 1].
    pub break_progress: Option<f32>,
    pub crop_growth: Option<CropGrowth>,
    pub container_fill: Option<ContainerFill>,
    pub furnace_progress: Option<FurnaceProgress>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LookingAtEntity<'a> {
    pub hit: EntityRayHit,
    pub entity_type_id: &'a str,
    pub display_name: Option<&'a str>,
    pub health: Option<f32>,
    pub max_health: Option<f32>,
    pub armor: Option<f32>,
    pub owner: Option<&'a str>,
    pub tamed: Option<bool>,
    /// Explicit namespace for entities whose registry ID is not available.
    pub mod_namespace: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LookingAtTarget<'a> {
    Block(LookingAtBlock<'a>),
    Entity(LookingAtEntity<'a>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HudTarget {
    Block([i32; 3]),
    Entity(i32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookingAtHudInfo {
    pub target: HudTarget,
    pub name: String,
    pub mod_badge: String,
    pub lines: Vec<String>,
}

fn title_case_words(value: &str) -> String {
    value
        .split(['_', '-', ' '])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            format!("{}{}", first.to_uppercase(), chars.as_str())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn resource_parts(id: &str) -> (&str, &str) {
    id.split_once(':').unwrap_or(("minecraft", id))
}

fn display_name(id: &str, localized: Option<&str>) -> String {
    if let Some(name) = localized.filter(|name| !name.trim().is_empty()) {
        return name.to_string();
    }
    let (_, path) = resource_parts(id);
    title_case_words(path.rsplit('/').next().unwrap_or(path))
}

fn namespace_badge(namespace: &str) -> String {
    match namespace {
        "minecraft" => "Minecraft".into(),
        "create" => "Create".into(),
        "farmersdelight" | "farmers_delight" => "Farmer's Delight".into(),
        other => title_case_words(other),
    }
}

fn percentage(numerator: u64, denominator: u64) -> Option<u64> {
    (denominator != 0)
        .then(|| ((u128::from(numerator) * 100) / u128::from(denominator)).min(100) as u64)
}

fn formatted_amount(amount: f32) -> String {
    let formatted = format!("{amount:.2}");
    formatted.trim_end_matches('0').trim_end_matches('.').into()
}

/// Builds a display-only HUD snapshot; any installed-mod override yields `None`.
/// The caller supplies authoritative registry metadata and translated names.
pub fn evaluate_looking_at_hud(
    target: LookingAtTarget<'_>,
    deferred_to_mod: Option<&str>,
) -> Option<LookingAtHudInfo> {
    if deferred_to_mod.is_some() {
        return None;
    }

    match target {
        LookingAtTarget::Block(block) => {
            let (namespace, _) = resource_parts(block.block_id);
            let mut lines = Vec::new();
            match (block.harvestable, block.preferred_tool) {
                (Some(harvestable), tool) => {
                    let verdict = if harvestable { "Yes" } else { "No" };
                    let suffix = tool
                        .map(|tool| format!(" ({})", tool.label()))
                        .unwrap_or_default();
                    lines.push(format!("Harvestable: {verdict}{suffix}"));
                }
                (None, Some(tool)) => lines.push(format!("Preferred tool: {}", tool.label())),
                (None, None) => {}
            }
            if let Some(growth) = block.crop_growth {
                if growth.max_age > 0 && growth.age >= growth.max_age {
                    lines.push("Mature".into());
                } else if let Some(percent) = percentage(growth.age.into(), growth.max_age.into()) {
                    lines.push(format!("Growth: {percent}%"));
                }
            }
            if let Some(progress) = block.furnace_progress {
                if let Some(percent) = percentage(progress.completed.into(), progress.total.into())
                {
                    lines.push(format!("Smelting: {percent}%"));
                }
            }
            if let Some(fill) = block.container_fill {
                if fill.total_slots > 0 {
                    lines.push(format!(
                        "Container: {} / {} slots",
                        fill.occupied_slots.min(fill.total_slots),
                        fill.total_slots
                    ));
                }
            }
            if let Some(progress) = block.break_progress.filter(|progress| progress.is_finite()) {
                lines.push(format!(
                    "Breaking: {:.0}%",
                    progress.clamp(0.0, 1.0) * 100.0
                ));
            }
            for (property, value) in block.properties {
                lines.push(format!("{}: {}", title_case_words(property), value));
            }
            Some(LookingAtHudInfo {
                target: HudTarget::Block(block.hit.block_pos),
                name: display_name(block.block_id, block.display_name),
                mod_badge: namespace_badge(namespace),
                lines,
            })
        }
        LookingAtTarget::Entity(entity) => {
            let (namespace, _) = resource_parts(entity.entity_type_id);
            let mut lines = Vec::new();
            if let (Some(health), Some(max)) = (entity.health, entity.max_health) {
                if health.is_finite() && health >= 0.0 && max.is_finite() && max > 0.0 {
                    lines.push(format!(
                        "HP: {} / {}",
                        formatted_amount(health),
                        formatted_amount(max)
                    ));
                }
            }
            if let Some(armor) = entity
                .armor
                .filter(|armor| armor.is_finite() && *armor >= 0.0)
            {
                lines.push(format!("Armor: {}", formatted_amount(armor)));
            }
            if let Some(tamed) = entity.tamed {
                lines.push(format!("Tamed: {}", if tamed { "Yes" } else { "No" }));
            }
            if let Some(owner) = entity.owner.filter(|owner| !owner.is_empty()) {
                lines.push(format!("Owner: {owner}"));
            }
            Some(LookingAtHudInfo {
                target: HudTarget::Entity(entity.hit.entity_id),
                name: display_name(entity.entity_type_id, entity.display_name),
                mod_badge: namespace_badge(entity.mod_namespace.unwrap_or(namespace)),
                lines,
            })
        }
    }
}

/// A complete item identity includes its components, which may contain NBT data.
/// The fingerprint must be a canonical, collision-free representation supplied
/// by the caller; an ordinary lossy hash cannot guarantee identity preservation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemStack {
    pub item_id: String,
    pub component_fingerprint: String,
    pub display_name: String,
    pub count: u32,
    pub max_stack: u32,
}

impl ItemStack {
    fn same_item(&self, other: &Self) -> bool {
        self.item_id == other.item_id && self.component_fingerprint == other.component_fingerprint
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum InventorySide {
    Container,
    Player,
}

impl InventorySide {
    const fn opposite(self) -> Self {
        match self {
            Self::Container => Self::Player,
            Self::Player => Self::Container,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlotRef {
    pub side: InventorySide,
    pub index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InventoryLocation {
    Slot(SlotRef),
    Cursor,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InventorySnapshot {
    pub container: Vec<Option<ItemStack>>,
    pub player: Vec<Option<ItemStack>>,
    pub cursor: Option<ItemStack>,
}

impl InventorySnapshot {
    fn slots(&self, side: InventorySide) -> &[Option<ItemStack>] {
        match side {
            InventorySide::Container => &self.container,
            InventorySide::Player => &self.player,
        }
    }

    fn get(&self, location: InventoryLocation) -> Option<&ItemStack> {
        match location {
            InventoryLocation::Cursor => self.cursor.as_ref(),
            InventoryLocation::Slot(slot) => self.slots(slot.side).get(slot.index)?.as_ref(),
        }
    }

    fn get_mut(&mut self, location: InventoryLocation) -> &mut Option<ItemStack> {
        match location {
            InventoryLocation::Cursor => &mut self.cursor,
            InventoryLocation::Slot(slot) => match slot.side {
                InventorySide::Container => &mut self.container[slot.index],
                InventorySide::Player => &mut self.player[slot.index],
            },
        }
    }

    fn validate_slot(&self, slot: SlotRef) -> Result<(), QolError> {
        if slot.index >= self.slots(slot.side).len() {
            return Err(QolError::InvalidSlot(slot));
        }
        Ok(())
    }

    fn all_stacks(&self) -> impl Iterator<Item = &ItemStack> {
        self.container
            .iter()
            .chain(&self.player)
            .chain(std::iter::once(&self.cursor))
            .filter_map(Option::as_ref)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QolError {
    InvalidSlot(SlotRef),
    InvalidRange,
    InvalidStack,
    InconsistentMaxStack,
    ConservationViolation,
}

fn validate_stacks<'a>(stacks: impl IntoIterator<Item = &'a ItemStack>) -> Result<(), QolError> {
    let mut capacities = BTreeMap::new();
    for stack in stacks {
        if stack.count == 0 || stack.max_stack == 0 || stack.count > stack.max_stack {
            return Err(QolError::InvalidStack);
        }
        let key = (&stack.item_id, &stack.component_fingerprint);
        if let Some(existing) = capacities.insert(key, stack.max_stack) {
            if existing != stack.max_stack {
                return Err(QolError::InconsistentMaxStack);
            }
        }
    }
    Ok(())
}

fn item_totals<'a>(
    stacks: impl IntoIterator<Item = &'a ItemStack>,
) -> BTreeMap<(String, String), u128> {
    let mut totals = BTreeMap::new();
    for stack in stacks {
        *totals
            .entry((stack.item_id.clone(), stack.component_fingerprint.clone()))
            .or_insert(0) += u128::from(stack.count);
    }
    totals
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MouseTweakAction {
    /// Move one item from the hovered slot into the opposite inventory.
    WheelPush { hovered: SlotRef },
    /// Move one matching item from the opposite inventory into the hovered slot.
    WheelPull { hovered: SlotRef },
    /// Move all matching visited stacks, anchored to the first nonempty slot.
    ShiftLeftDragSweep { visited: Vec<SlotRef> },
    /// Paint exactly one cursor item per distinct visited slot in the stroke.
    RightDragNoDup { visited: Vec<SlotRef> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedTransfer {
    pub from: InventoryLocation,
    pub to: InventoryLocation,
    pub count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotTransferPlan {
    pub transfers: Vec<PlannedTransfer>,
    pub resulting: InventorySnapshot,
}

fn first_destination(
    snapshot: &InventorySnapshot,
    side: InventorySide,
    stack: &ItemStack,
) -> Option<SlotRef> {
    let slots = snapshot.slots(side);
    slots
        .iter()
        .position(|slot| {
            slot.as_ref().is_some_and(|existing| {
                existing.same_item(stack) && existing.count < existing.max_stack
            })
        })
        .or_else(|| slots.iter().position(Option::is_none))
        .map(|index| SlotRef { side, index })
}

/// Updates only the two referenced locations, returning the exact transferred count.
fn transfer(
    snapshot: &mut InventorySnapshot,
    from: InventoryLocation,
    to: InventoryLocation,
    requested: u32,
    transfers: &mut Vec<PlannedTransfer>,
) -> u32 {
    if from == to || requested == 0 {
        return 0;
    }
    let Some(source) = snapshot.get(from).cloned() else {
        return 0;
    };
    let capacity = match snapshot.get(to) {
        Some(destination) if destination.same_item(&source) => {
            destination.max_stack - destination.count
        }
        Some(_) => 0,
        None => source.max_stack,
    };
    let count = requested.min(source.count).min(capacity);
    if count == 0 {
        return 0;
    }

    let from_slot = snapshot.get_mut(from);
    if source.count == count {
        *from_slot = None;
    } else if let Some(stack) = from_slot {
        stack.count -= count;
    }

    let to_slot = snapshot.get_mut(to);
    if let Some(destination) = to_slot {
        destination.count += count;
    } else {
        *to_slot = Some(ItemStack { count, ..source });
    }
    transfers.push(PlannedTransfer { from, to, count });
    count
}

fn shift_transfer_all(
    snapshot: &mut InventorySnapshot,
    source: SlotRef,
    transfers: &mut Vec<PlannedTransfer>,
) {
    let from = InventoryLocation::Slot(source);
    while let Some(stack) = snapshot.get(from).cloned() {
        let Some(destination) = first_destination(snapshot, source.side.opposite(), &stack) else {
            break;
        };
        let moved = transfer(
            snapshot,
            from,
            InventoryLocation::Slot(destination),
            u32::MAX,
            transfers,
        );
        if moved == 0 {
            break;
        }
    }
}

/// Computes an atomic, deterministic plan. An installed mod returns `Ok(None)`.
/// Clients must validate/apply against the same authoritative slot snapshot.
pub fn plan_mouse_tweak(
    action: MouseTweakAction,
    snapshot: &InventorySnapshot,
    deferred_to_mod: Option<&str>,
) -> Result<Option<SlotTransferPlan>, QolError> {
    if deferred_to_mod.is_some() {
        return Ok(None);
    }
    validate_stacks(snapshot.all_stacks())?;
    let visited = match &action {
        MouseTweakAction::WheelPush { hovered } | MouseTweakAction::WheelPull { hovered } => {
            std::slice::from_ref(hovered)
        }
        MouseTweakAction::ShiftLeftDragSweep { visited }
        | MouseTweakAction::RightDragNoDup { visited } => visited.as_slice(),
    };
    for &slot in visited {
        snapshot.validate_slot(slot)?;
    }

    let mut resulting = snapshot.clone();
    let mut transfers = Vec::new();
    match action {
        MouseTweakAction::WheelPush { hovered } => {
            let from = InventoryLocation::Slot(hovered);
            if let Some(stack) = resulting.get(from).cloned() {
                if let Some(destination) =
                    first_destination(&resulting, hovered.side.opposite(), &stack)
                {
                    transfer(
                        &mut resulting,
                        from,
                        InventoryLocation::Slot(destination),
                        1,
                        &mut transfers,
                    );
                }
            }
        }
        MouseTweakAction::WheelPull { hovered } => {
            let destination = InventoryLocation::Slot(hovered);
            let target = resulting.get(destination);
            let source = resulting
                .slots(hovered.side.opposite())
                .iter()
                .position(|stack| {
                    stack.as_ref().is_some_and(|candidate| {
                        target.is_none_or(|target| {
                            target.same_item(candidate) && target.count < target.max_stack
                        })
                    })
                });
            if let Some(index) = source {
                let from = InventoryLocation::Slot(SlotRef {
                    side: hovered.side.opposite(),
                    index,
                });
                transfer(&mut resulting, from, destination, 1, &mut transfers);
            }
        }
        MouseTweakAction::ShiftLeftDragSweep { visited } => {
            let first = visited.iter().find_map(|slot| {
                resulting
                    .get(InventoryLocation::Slot(*slot))
                    .map(|stack| (*slot, stack.clone()))
            });
            if let Some((anchor, matching)) = first {
                let ordered = visited.into_iter().collect::<BTreeSet<_>>();
                for source in ordered {
                    if source.side != anchor.side {
                        continue;
                    }
                    let from = InventoryLocation::Slot(source);
                    if resulting
                        .get(from)
                        .is_some_and(|stack| stack.same_item(&matching))
                    {
                        shift_transfer_all(&mut resulting, source, &mut transfers);
                    }
                }
            }
        }
        MouseTweakAction::RightDragNoDup { visited } => {
            let mut painted = BTreeSet::new();
            for destination in visited {
                if painted.insert(destination) {
                    transfer(
                        &mut resulting,
                        InventoryLocation::Cursor,
                        InventoryLocation::Slot(destination),
                        1,
                        &mut transfers,
                    );
                }
            }
        }
    }

    if item_totals(snapshot.all_stacks()) != item_totals(resulting.all_stacks()) {
        return Err(QolError::ConservationViolation);
    }
    Ok(Some(SlotTransferPlan {
        transfers,
        resulting,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortOrder {
    ByIdAsc,
    ByNameAsc,
    ByCountDesc,
}

/// Sorts only `range`, merging matching components up to each item's capacity.
/// Returns `Ok(false)` without mutation when an installed mod takes precedence.
/// Invalid inputs also leave the input slice unmodified.
pub fn sort_and_merge_inventory(
    slots: &mut [Option<ItemStack>],
    range: Range<usize>,
    order: SortOrder,
    deferred_to_mod: Option<&str>,
) -> Result<bool, QolError> {
    if deferred_to_mod.is_some() {
        return Ok(false);
    }
    if range.start > range.end || range.end > slots.len() {
        return Err(QolError::InvalidRange);
    }
    let selected = &slots[range.clone()];
    validate_stacks(selected.iter().filter_map(Option::as_ref))?;
    let before = item_totals(selected.iter().filter_map(Option::as_ref));

    let mut groups: BTreeMap<(String, String), (ItemStack, u128)> = BTreeMap::new();
    for stack in selected.iter().filter_map(Option::as_ref) {
        let key = (stack.item_id.clone(), stack.component_fingerprint.clone());
        let group = groups.entry(key).or_insert_with(|| (stack.clone(), 0));
        group.1 += u128::from(stack.count);
    }
    let mut packed = Vec::with_capacity(selected.len());
    for (_, (template, mut count)) in groups {
        while count > 0 {
            let amount = count.min(u128::from(template.max_stack)) as u32;
            packed.push(Some(ItemStack {
                count: amount,
                ..template.clone()
            }));
            count -= u128::from(amount);
        }
    }
    packed.sort_by(|left, right| {
        let (Some(left), Some(right)) = (left, right) else {
            return std::cmp::Ordering::Equal;
        };
        let primary = match order {
            SortOrder::ByIdAsc => left.item_id.cmp(&right.item_id),
            SortOrder::ByNameAsc => left
                .display_name
                .to_lowercase()
                .cmp(&right.display_name.to_lowercase()),
            SortOrder::ByCountDesc => right.count.cmp(&left.count),
        };
        primary
            .then_with(|| left.item_id.cmp(&right.item_id))
            .then_with(|| left.component_fingerprint.cmp(&right.component_fingerprint))
            .then_with(|| right.count.cmp(&left.count))
    });
    packed.resize(selected.len(), None);
    if item_totals(packed.iter().filter_map(Option::as_ref)) != before {
        return Err(QolError::ConservationViolation);
    }
    slots[range].clone_from_slice(&packed);
    Ok(true)
}
