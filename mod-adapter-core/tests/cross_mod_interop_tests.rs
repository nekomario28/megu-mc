// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Cross-mod interoperability end-to-end tests for `XMOD1-CORE`:
//! multi-hop item pipelines (Mod A → vanilla-style slots → Create-style
//! belt → culinary pot → output chest) with lossless 1.21.1 component
//! preservation, cross-dimension fluid/FE transfers with face filtering
//! and atomic rollback, and the [`UnifiedTagIndex`] nested/`#c:`↔`#forge:`
//! contract.

use std::collections::{BTreeMap, BTreeSet};

use mod_adapter_core::capability_bus::{
    BlockCapabilityBus, BlockCoord, CapabilityFluidStack, CapabilityItemStack,
    CookingPotItemAdapter, EnergyTransferHandler, FaceDirection, ItemTransferHandler,
    SimpleEnergyBuffer, SimpleFluidTank, SimpleSlotInventory,
};
use mod_adapter_core::culinary_adapter::{
    tag_contains, CookingRecipe, CulinaryRegistry, IngredientMatcher,
};
use mod_adapter_core::unified_tags::{TagDomain, UnifiedTagError, UnifiedTagIndex};

// ---------------------------------------------------------------------------
// Stored-ZIP builder (same shape as the other test harnesses use)
// ---------------------------------------------------------------------------

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
        out.extend(10u16.to_le_bytes());
        out.extend([0u8; 8]);
        out.extend(crc.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(name_len.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name_bytes);
        out.extend(bytes);

        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        central.extend(10u16.to_le_bytes());
        central.extend([0u8; 8]);
        central.extend(crc.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(name_len.to_le_bytes());
        central.extend([0u8; 8]);
        central.extend(0u32.to_le_bytes());
        central.extend(offset.to_le_bytes());
        central.extend(name_bytes);
    }
    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend(central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(central_size.to_le_bytes());
    out.extend(central_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

// ---------------------------------------------------------------------------
// A simulated Create-style belt endpoint: FIFO, bounded, preserves
// component bytes verbatim.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ConveyorBeltEndpoint {
    queue: std::collections::VecDeque<CapabilityItemStack>,
    capacity: usize,
}

impl ConveyorBeltEndpoint {
    fn new(capacity: usize) -> Self {
        Self {
            queue: std::collections::VecDeque::new(),
            capacity,
        }
    }
}

impl ItemTransferHandler for ConveyorBeltEndpoint {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn insert_item(
        &mut self,
        _face: Option<FaceDirection>,
        stack: &CapabilityItemStack,
        simulate: bool,
    ) -> CapabilityItemStack {
        if self.queue.len() >= self.capacity {
            return stack.clone();
        }
        if !simulate {
            self.queue.push_back(stack.clone());
        }
        // Belts accept the whole stack entity-by-entity.
        CapabilityItemStack {
            count: 0,
            ..stack.clone()
        }
    }

    fn extract_item(
        &mut self,
        _face: Option<FaceDirection>,
        max_count: u32,
        filter: Option<&dyn Fn(&CapabilityItemStack) -> bool>,
        simulate: bool,
    ) -> Option<CapabilityItemStack> {
        let front = self.queue.front()?;
        if !filter.is_none_or(|f| f(front)) {
            return None;
        }
        let taken = front.count.min(max_count);
        if simulate {
            let mut probe = front.clone();
            return Some(probe.take(taken));
        }
        let front = self.queue.front_mut()?;
        let out = front.take(taken);
        if front.count == 0 {
            self.queue.pop_front();
        }
        Some(out)
    }

    fn slots(&self) -> usize {
        self.capacity
    }
}

/// Face-gated wrapper: delegates to an inner energy buffer but refuses
/// any face not in `allowed` — a stand-in for machines that only expose
/// their FE on specific sides (Mekanism-style).
struct FaceGatedEnergy {
    inner: SimpleEnergyBuffer,
    allowed: BTreeSet<Option<FaceDirection>>,
}

impl FaceGatedEnergy {
    fn new(inner: SimpleEnergyBuffer, allowed: BTreeSet<Option<FaceDirection>>) -> Self {
        Self { inner, allowed }
    }
}

impl EnergyTransferHandler for FaceGatedEnergy {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn receive_energy(
        &mut self,
        face: Option<FaceDirection>,
        max_receive_fe: u64,
        simulate: bool,
    ) -> u64 {
        if !self.allowed.contains(&face) {
            return 0;
        }
        self.inner.receive_energy(face, max_receive_fe, simulate)
    }

    fn extract_energy(
        &mut self,
        face: Option<FaceDirection>,
        max_extract_fe: u64,
        simulate: bool,
    ) -> u64 {
        if !self.allowed.contains(&face) {
            return 0;
        }
        self.inner.extract_energy(face, max_extract_fe, simulate)
    }

    fn energy_stored(&self) -> u64 {
        self.inner.energy_stored()
    }

    fn max_energy_stored(&self) -> u64 {
        self.inner.max_energy_stored()
    }
}

// ---------------------------------------------------------------------------
// 1. Multi-hop cross-mod item pipeline
// ---------------------------------------------------------------------------

#[test]
fn multi_hop_cross_mod_pipeline_preserves_components_and_conserves_items() {
    let mut bus = BlockCapabilityBus::new();
    let dim = "minecraft:overworld";

    let mod_a_machine = BlockCoord::new(dim, 0, 64, 0);
    let hopper_chest = BlockCoord::new(dim, 0, 64, 1);
    let belt = BlockCoord::new(dim, 0, 64, 2);
    let pot = BlockCoord::new(dim, 0, 64, 3);
    let output_chest = BlockCoord::new(dim, 0, 64, 4);

    // Mod A machine exporting a component-carrying custom item.
    let mut source = SimpleSlotInventory::new(2, 64);
    let components = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
    source.set_slot(
        0,
        Some(CapabilityItemStack::with_components(
            "moda:enchanted_iron_ingot",
            10,
            components.clone(),
        )),
    );
    bus.register_item_handler(mod_a_machine.clone(), Box::new(source));

    // Vanilla-style 5x1 hopper/chest slot inventory.
    bus.register_item_handler(
        hopper_chest.clone(),
        Box::new(SimpleSlotInventory::new(5, 64)),
    );

    // Create-style belt endpoint.
    bus.register_item_handler(belt.clone(), Box::new(ConveyorBeltEndpoint::new(8)));

    // Mod B culinary cooking pot.
    bus.register_item_handler(pot.clone(), Box::new(CookingPotItemAdapter::new()));

    // Output chest.
    bus.register_item_handler(
        output_chest.clone(),
        Box::new(SimpleSlotInventory::new(3, 64)),
    );

    // Mod A → hopper.
    let moved = bus.transfer_item_between(
        &mod_a_machine,
        Some(FaceDirection::Down),
        &hopper_chest,
        Some(FaceDirection::Up),
        64,
        None,
    );
    assert_eq!(moved, 10, "all 10 items must leave the source");
    let hopper_stack = bus
        .item_endpoint(&hopper_chest)
        .and_then(|h| h.as_any().downcast_ref::<SimpleSlotInventory>())
        .and_then(|inv| inv.slot(0).cloned());
    assert_eq!(
        hopper_stack,
        Some(CapabilityItemStack::with_components(
            "moda:enchanted_iron_ingot",
            10,
            components.clone()
        )),
        "components must survive the hopper hop byte-for-byte"
    );

    // Hopper → belt (through an item filter accepting only the Mod A id).
    let moved = bus.transfer_item_between(
        &hopper_chest,
        Some(FaceDirection::Down),
        &belt,
        Some(FaceDirection::Up),
        64,
        Some(&|s: &CapabilityItemStack| s.item_id == "moda:enchanted_iron_ingot"),
    );
    assert_eq!(moved, 10);

    // Belt → pot (side face into ingredient inputs).
    let moved = bus.transfer_item_between(
        &belt,
        Some(FaceDirection::Down),
        &pot,
        Some(FaceDirection::North),
        64,
        None,
    );
    assert_eq!(moved, 10);
    let pot_adapter = bus
        .item_endpoint(&pot)
        .and_then(|h| h.as_any().downcast_ref::<CookingPotItemAdapter>())
        .expect("pot adapter registered");
    let (id, count) = pot_adapter.pot().inputs[0]
        .clone()
        .expect("ingredient landed");
    assert_eq!((id.as_str(), count), ("moda:enchanted_iron_ingot", 10));

    // Conservation: nothing left upstream, exactly 10 items in the world.
    assert_eq!(
        bus.transfer_item_between(&mod_a_machine, None, &hopper_chest, None, 64, None),
        0
    );
    assert_eq!(
        bus.transfer_item_between(&hopper_chest, None, &belt, None, 64, None),
        0
    );
    assert_eq!(
        bus.transfer_item_between(&belt, None, &pot, None, 64, None),
        0
    );

    // Fill the pot's remaining inputs, then prove a full target neither
    // duplicates nor loses items.
    for i in 1..6 {
        let mut filler = SimpleSlotInventory::new(1, 64);
        filler.set_slot(
            0,
            Some(CapabilityItemStack::new(format!("moda:filler_{i}"), 1)),
        );
        let c = BlockCoord::new(dim, 9, 64, i);
        bus.register_item_handler(c.clone(), Box::new(filler));
        assert_eq!(
            1,
            bus.transfer_item_between(&c, None, &pot, Some(FaceDirection::West), 1, None)
        );
    }
    let mut extra = SimpleSlotInventory::new(1, 64);
    extra.set_slot(
        0,
        Some(CapabilityItemStack::with_components(
            "moda:rare_gem",
            3,
            vec![9, 9, 9],
        )),
    );
    let extra_coord = BlockCoord::new(dim, 9, 64, 9);
    bus.register_item_handler(extra_coord.clone(), Box::new(extra));
    let moved =
        bus.transfer_item_between(&extra_coord, None, &pot, Some(FaceDirection::West), 3, None);
    assert_eq!(moved, 0, "a full pot must refuse without deleting items");
    let still_there = bus
        .item_endpoint(&extra_coord)
        .and_then(|h| h.as_any().downcast_ref::<SimpleSlotInventory>())
        .and_then(|inv| inv.slot(0).cloned());
    assert_eq!(
        still_there.map(|s| s.count),
        Some(3),
        "source keeps its items"
    );

    // Cook a meal: Mod A tomato + bowl container → Mod B stew, then
    // extract the meal through the bottom face into the output chest.
    let mut registry = CulinaryRegistry::new();
    registry.cooking_recipes.push(CookingRecipe {
        id: "modb:stew".to_string(),
        recipe_type: "modb:cooking".to_string(),
        ingredients: vec![
            IngredientMatcher::Item("moda:enchanted_iron_ingot".to_string()),
            IngredientMatcher::Item("moda:filler_1".to_string()),
            IngredientMatcher::Item("moda:filler_2".to_string()),
            IngredientMatcher::Item("moda:filler_3".to_string()),
            IngredientMatcher::Item("moda:filler_4".to_string()),
            IngredientMatcher::Item("moda:filler_5".to_string()),
        ],
        container: Some("minecraft:bowl".to_string()),
        result_item: "modb:odd_stew".to_string(),
        result_count: 1,
        cooking_time: 40,
        experience: 0.5,
    });
    let bowl_src = BlockCoord::new(dim, 9, 64, 10);
    let mut bowls = SimpleSlotInventory::new(1, 16);
    bowls.set_slot(0, Some(CapabilityItemStack::new("minecraft:bowl", 4)));
    bus.register_item_handler(bowl_src.clone(), Box::new(bowls));
    assert_eq!(
        1,
        bus.transfer_item_between(&bowl_src, None, &pot, Some(FaceDirection::Down), 1, None),
        "bottom face feeds the container slot"
    );

    {
        let adapter = bus
            .item_endpoint_mut(&pot)
            .and_then(|h| h.as_any_mut().downcast_mut::<CookingPotItemAdapter>())
            .expect("cooking pot adapter");
        adapter.pot_mut().heated = true;
        adapter.pot_mut().refresh(&registry, 0);
        assert!(!adapter.pot().is_sleeping(), "recipe armed the pot");
        let result = adapter.advance_to(40, &registry).expect("meal cooked");
        assert_eq!(result.result_item, "modb:odd_stew");
    }

    let moved = bus.transfer_item_between(
        &pot,
        Some(FaceDirection::Down),
        &output_chest,
        Some(FaceDirection::Up),
        4,
        None,
    );
    assert_eq!(moved, 1, "finished meal extracted through the bottom face");
    let meal = bus
        .item_endpoint(&output_chest)
        .and_then(|h| h.as_any().downcast_ref::<SimpleSlotInventory>())
        .and_then(|inv| inv.slot(0).cloned());
    assert_eq!(
        meal.map(|s| (s.item_id, s.count)),
        Some(("modb:odd_stew".to_string(), 1))
    );
}

#[test]
fn full_destination_never_duplicates_or_vanishes_items() {
    let mut bus = BlockCapabilityBus::new();
    let a = BlockCoord::new("minecraft:overworld", 0, 0, 0);
    let b = BlockCoord::new("minecraft:overworld", 1, 0, 0);

    let mut src = SimpleSlotInventory::new(1, 64);
    src.set_slot(
        0,
        Some(CapabilityItemStack::with_components(
            "moda:widget",
            40,
            vec![1, 2, 3, 4],
        )),
    );
    let mut dst = SimpleSlotInventory::new(1, 16);
    dst.set_slot(
        0,
        Some(CapabilityItemStack::with_components(
            "moda:widget",
            10,
            vec![1, 2, 3, 4],
        )),
    );
    bus.register_item_handler(a.clone(), Box::new(src));
    bus.register_item_handler(b.clone(), Box::new(dst));

    // Destination can only take 6 more of the identical stack.
    let moved = bus.transfer_item_between(&a, None, &b, None, 64, None);
    assert_eq!(moved, 6);
    let inv = |c: &BlockCoord, bus: &BlockCapabilityBus| {
        bus.item_endpoint(c)
            .and_then(|h| h.as_any().downcast_ref::<SimpleSlotInventory>())
            .map(|i| i.total_items())
    };
    assert_eq!(inv(&a, &bus), Some(34));
    assert_eq!(inv(&b, &bus), Some(16));
    assert_eq!(34 + 16, 50, "total conserved");

    // Mismatched components must NOT merge — they land in a new slot or
    // bounce entirely when the destination has no room for them.
    let mut src2 = SimpleSlotInventory::new(1, 64);
    src2.set_slot(
        0,
        Some(CapabilityItemStack::with_components(
            "moda:widget",
            5,
            vec![9, 9],
        )),
    );
    let a2 = BlockCoord::new("minecraft:overworld", 2, 0, 0);
    bus.register_item_handler(a2.clone(), Box::new(src2));
    let moved = bus.transfer_item_between(&a2, None, &b, None, 64, None);
    assert_eq!(moved, 0, "no empty slot for the different-components stack");
    assert_eq!(inv(&a2, &bus), Some(5), "rejected stack stays in source");
}

// ---------------------------------------------------------------------------
// 2. Cross-dimension fluid + FE transfers with face filtering & rollback
// ---------------------------------------------------------------------------

#[test]
fn cross_dimension_fluid_transfer_is_atomic() {
    let mut bus = BlockCapabilityBus::new();
    let aether_tank = BlockCoord::new("mod_a:aether", 4, 70, -2);
    let nether_tank = BlockCoord::new("minecraft:the_nether", -3, 40, 7);

    let mut src = SimpleFluidTank::new(4000);
    src.set_fluid(Some(CapabilityFluidStack::with_components(
        "mymod:molten_zinc",
        2000,
        vec![0xAA, 0xBB],
    )));
    bus.register_fluid_handler(aether_tank.clone(), Box::new(src));
    bus.register_fluid_handler(nether_tank.clone(), Box::new(SimpleFluidTank::new(1500)));

    // Destination only holds 1500 — atomic transfer moves exactly that.
    let moved = bus.transfer_fluid_between(
        &aether_tank,
        Some(FaceDirection::Down),
        &nether_tank,
        Some(FaceDirection::Up),
        2000,
    );
    assert_eq!(moved, 1500);
    let src_fluid = bus
        .fluid_endpoint(&aether_tank)
        .and_then(|h| h.as_any().downcast_ref::<SimpleFluidTank>())
        .and_then(|t| t.fluid().cloned());
    let dst_fluid = bus
        .fluid_endpoint(&nether_tank)
        .and_then(|h| h.as_any().downcast_ref::<SimpleFluidTank>())
        .and_then(|t| t.fluid().cloned());
    assert_eq!(src_fluid.map(|f| f.amount_mb), Some(500));
    assert_eq!(
        dst_fluid,
        Some(CapabilityFluidStack::with_components(
            "mymod:molten_zinc",
            1500,
            vec![0xAA, 0xBB]
        )),
        "fluid component bytes survive cross-dimension"
    );

    // A different fluid is rejected by the filled tank — rollback keeps
    // the source whole.
    let mut other = SimpleFluidTank::new(1000);
    other.set_fluid(Some(CapabilityFluidStack::new("othermod:liquid_mana", 800)));
    let other_coord = BlockCoord::new("mod_a:aether", 4, 71, -2);
    bus.register_fluid_handler(other_coord.clone(), Box::new(other));
    let moved = bus.transfer_fluid_between(&other_coord, None, &nether_tank, None, 800);
    assert_eq!(moved, 0);
    let untouched = bus
        .fluid_endpoint(&other_coord)
        .and_then(|h| h.as_any().downcast_ref::<SimpleFluidTank>())
        .and_then(|t| t.fluid().cloned());
    assert_eq!(
        untouched.map(|f| f.amount_mb),
        Some(800),
        "rejected fill rolls back"
    );

    // Full destination: no drain happens at all.
    let before = bus
        .fluid_endpoint(&aether_tank)
        .and_then(|h| h.as_any().downcast_ref::<SimpleFluidTank>())
        .and_then(|t| t.fluid().map(|f| f.amount_mb));
    assert_eq!(
        bus.transfer_fluid_between(&aether_tank, None, &nether_tank, None, 500),
        0
    );
    let after = bus
        .fluid_endpoint(&aether_tank)
        .and_then(|h| h.as_any().downcast_ref::<SimpleFluidTank>())
        .and_then(|t| t.fluid().map(|f| f.amount_mb));
    assert_eq!(before, after, "full destination leaves source untouched");
}

#[test]
fn cross_dimension_energy_transfer_respects_faces_and_rolls_back() {
    let mut bus = BlockCapabilityBus::new();
    let src_coord = BlockCoord::new("mod_a:aether", 0, 80, 0);
    let dst_coord = BlockCoord::new("minecraft:the_nether", 1, 30, 1);

    let mut src = SimpleEnergyBuffer::new(20_000);
    src.set_stored(10_000);
    bus.register_energy_handler(src_coord.clone(), Box::new(src));

    // Destination only accepts FE through its Up face.
    let dst = FaceGatedEnergy::new(
        SimpleEnergyBuffer::new(8_000).with_limits(5_000, 5_000),
        [Some(FaceDirection::Up)].into_iter().collect(),
    );
    bus.register_energy_handler(dst_coord.clone(), Box::new(dst));

    // Wrong face → simulation refuses, nothing is drawn.
    let moved = bus.transfer_energy_between(
        &src_coord,
        Some(FaceDirection::Down),
        &dst_coord,
        Some(FaceDirection::East),
        10_000,
    );
    assert_eq!(moved, 0);
    assert_eq!(
        bus.energy_endpoint(&src_coord).map(|h| h.energy_stored()),
        Some(10_000),
        "rejected face leaves the source fully charged"
    );

    // Right face → moves, clamped by the destination's per-call limit.
    let moved = bus.transfer_energy_between(
        &src_coord,
        Some(FaceDirection::Down),
        &dst_coord,
        Some(FaceDirection::Up),
        10_000,
    );
    assert_eq!(moved, 5_000);
    assert_eq!(
        bus.energy_endpoint(&src_coord).map(|h| h.energy_stored()),
        Some(5_000)
    );
    assert_eq!(
        bus.energy_endpoint(&dst_coord).map(|h| h.energy_stored()),
        Some(5_000)
    );

    // Fill the remainder of the destination, then prove a full target
    // draws nothing.
    let moved = bus.transfer_energy_between(
        &src_coord,
        None,
        &dst_coord,
        Some(FaceDirection::Up),
        10_000,
    );
    assert_eq!(moved, 3_000, "only remaining capacity is delivered");
    assert_eq!(
        bus.transfer_energy_between(&src_coord, None, &dst_coord, Some(FaceDirection::Up), 2_000),
        0
    );
    assert_eq!(
        bus.energy_endpoint(&src_coord).map(|h| h.energy_stored()),
        Some(2_000),
        "full destination means zero draw"
    );
}

// ---------------------------------------------------------------------------
// 3. UnifiedTagIndex
// ---------------------------------------------------------------------------

#[test]
fn unified_tags_expand_nested_and_alias_c_forge_bidirectionally() {
    let mut index = UnifiedTagIndex::new();

    // Mod ships only the legacy forge: spelling.
    index
        .ingest_member(
            "data/forge/tags/items/ingots/iron.json",
            br##"{"values": ["othermod:iron_ingot", "mymod:iron_ingot"]}"##,
        )
        .expect("tag file ingests");
    index
        .ingest_member(
            "data/forge/tags/items/ingots.json",
            br##"{"values": ["#forge:ingots/iron", "minecraft:gold_ingot"]}"##,
        )
        .expect("tag file ingests");
    // Another mod ships the modern c: spelling for a different family.
    index
        .ingest_member(
            "data/c/tags/items/plates/brass.json",
            br##"{"values": ["create:brass_sheet"]}"##,
        )
        .expect("tag file ingests");

    // Query through EITHER spelling — bidirectional aliasing.
    assert!(index.matches(TagDomain::Item, "#c:ingots/iron", "othermod:iron_ingot"));
    assert!(index.matches(TagDomain::Item, "#forge:ingots/iron", "othermod:iron_ingot"));
    assert!(index.matches(TagDomain::Item, "c:ingots", "mymod:iron_ingot"));
    assert!(index.matches(TagDomain::Item, "forge:ingots", "minecraft:gold_ingot"));
    assert!(index.matches(TagDomain::Item, "#forge:plates/brass", "create:brass_sheet"));
    assert!(index.matches(TagDomain::Item, "#c:plates/brass", "create:brass_sheet"));
    assert!(!index.matches(TagDomain::Item, "#c:ingots/iron", "minecraft:copper_ingot"));

    // Domains stay separate: an items tag never leaks into blocks.
    index
        .ingest_member(
            "data/c/tags/blocks/ores/zinc.json",
            br##"{"values": ["create:zinc_ore"]}"##,
        )
        .expect("tag file ingests");
    index
        .ingest_member(
            "data/forge/tags/blocks/ores.json",
            br##"{"values": ["#forge:ores/zinc"]}"##,
        )
        .expect("tag file ingests");
    assert!(index.matches(TagDomain::Block, "#c:ores", "create:zinc_ore"));
    assert!(index.matches(TagDomain::Block, "#forge:ores", "create:zinc_ore"));
    assert!(!index.matches(TagDomain::Item, "#c:ores", "create:zinc_ore"));

    // Chests is a common alias outside the ten conventional families.
    index
        .ingest_member(
            "data/forge/tags/blocks/chests.json",
            br##"{"values": ["storagedrawers:oak_drawer"]}"##,
        )
        .expect("tag file ingests");
    assert!(index.matches(TagDomain::Block, "c:chests", "storagedrawers:oak_drawer"));
    assert!(index.matches(
        TagDomain::Block,
        "forge:chests",
        "storagedrawers:oak_drawer"
    ));
}

#[test]
fn unified_tags_replace_true_and_optional_entries() {
    let mut index = UnifiedTagIndex::new();
    index
        .ingest_member(
            "data/a/tags/items/seeds.json",
            br##"{"values": ["a:old_seed"]}"##,
        )
        .expect("tag file ingests");
    // A later datapack's file for the same tag replaces it entirely.
    index
        .ingest_member(
            "data/a/tags/items/seeds.json",
            br##"{"replace": true, "values": ["b:new_seed"]}"##,
        )
        .expect("tag file ingests");
    let members = index.resolve(TagDomain::Item, "a:seeds").expect("resolves");
    assert_eq!(members, ["b:new_seed".to_string()].into_iter().collect());

    // Optional nested reference to a missing tag is skipped silently;
    // a required one fails closed.
    index
        .ingest_member(
            "data/a/tags/items/mix.json",
            br##"{"values": [
            "a:base",
            {"id": "#absent:missing", "required": false}
        ]}"##,
        )
        .expect("tag file ingests");
    let members = index
        .resolve(TagDomain::Item, "a:mix")
        .expect("optional skipped");
    assert_eq!(members, ["a:base".to_string()].into_iter().collect());

    index
        .ingest_member(
            "data/a/tags/items/strict.json",
            br##"{"values": [{"id": "#absent:missing", "required": true}]}"##,
        )
        .expect("tag file ingests");
    assert!(matches!(
        index.resolve(TagDomain::Item, "a:strict"),
        Err(UnifiedTagError::MissingTag(_))
    ));
    assert!(!index.matches(TagDomain::Item, "a:strict", "a:base"));

    // Optional literal ids resolve only against a live registry.
    index
        .ingest_member(
            "data/a/tags/items/maybe.json",
            br#"{"values": [{"id": "maybe:present", "required": false},
                        {"id": "maybe:absent", "required": false}]}"#,
        )
        .expect("tag file ingests");
    let members = index
        .resolve_with(TagDomain::Item, "a:maybe", &|id| id == "maybe:present")
        .expect("resolves");
    assert_eq!(members, ["maybe:present".to_string()].into_iter().collect());
}

#[test]
fn unified_tags_fail_closed_on_cycles_and_deep_chains() {
    let mut index = UnifiedTagIndex::new();
    index
        .ingest_member(
            "data/a/tags/items/loop_a.json",
            br##"{"values": ["#a:loop_b", "a:kept"]}"##,
        )
        .expect("tag file ingests");
    index
        .ingest_member(
            "data/a/tags/items/loop_b.json",
            br##"{"values": ["#a:loop_a"]}"##,
        )
        .expect("tag file ingests");
    assert!(matches!(
        index.resolve(TagDomain::Item, "a:loop_a"),
        Err(UnifiedTagError::Cycle(_))
    ));
    assert!(!index.matches(TagDomain::Item, "a:loop_a", "a:kept"));

    // A self-referencing tag is a cycle too.
    index
        .ingest_member(
            "data/a/tags/items/selfish.json",
            br##"{"values": ["#a:selfish"]}"##,
        )
        .expect("tag file ingests");
    assert!(matches!(
        index.resolve(TagDomain::Item, "a:selfish"),
        Err(UnifiedTagError::Cycle(_))
    ));

    // Legitimate deep chains resolve; >16 levels fail closed.
    let mut deep = UnifiedTagIndex::new();
    for i in 0..18 {
        let next = if i == 17 {
            r#""deep:leaf""#.to_string()
        } else {
            format!(r##""#deep:level_{}""##, i + 1)
        };
        let doc = format!(r#"{{"values": [{next}]}}"#);
        deep.ingest_member(
            &format!("data/deep/tags/items/level_{i}.json"),
            doc.as_bytes(),
        )
        .expect("tag file ingests");
    }
    assert!(matches!(
        deep.resolve(TagDomain::Item, "deep:level_0"),
        Err(UnifiedTagError::DepthExceeded(_))
    ));
    let mut shallow = UnifiedTagIndex::new();
    for i in 0..17 {
        let next = if i == 16 {
            r#""deep:leaf""#.to_string()
        } else {
            format!(r##""#deep:level_{}""##, i + 1)
        };
        let doc = format!(r#"{{"values": [{next}]}}"#);
        shallow
            .ingest_member(
                &format!("data/deep/tags/items/level_{i}.json"),
                doc.as_bytes(),
            )
            .expect("tag file ingests");
    }
    assert_eq!(
        shallow.resolve(TagDomain::Item, "deep:level_0"),
        Ok(["deep:leaf".to_string()].into_iter().collect())
    );
}

#[test]
fn unified_tags_ingest_jar_including_jar_in_jar_inner_jars() {
    let inner = jar(&[(
        "data/inner/tags/items/gems.json",
        br##"{"values": ["innermod:ruby"]}"##,
    )]);
    let outer = jar(&[
        (
            "data/outer/tags/items/gems.json",
            br##"{"values": ["#inner:gems", "outer:topaz"]}"##,
        ),
        ("META-INF/jarjar/inner.jar", inner.as_slice()),
        (
            "data/outer/tags/fluids/oils.json",
            br##"{"values": ["outer:seed_oil"]}"##,
        ),
    ]);

    let mut index = UnifiedTagIndex::new();
    let stats = index
        .ingest_jar_bytes(&outer, "outer.jar")
        .expect("jar ingested");
    assert_eq!(stats.tag_files, 3);
    assert_eq!(stats.nested_jars, 1);

    // Inner-JAR tags merge into the same index as the outer JAR's.
    assert!(index.matches(TagDomain::Item, "outer:gems", "innermod:ruby"));
    assert!(index.matches(TagDomain::Item, "inner:gems", "innermod:ruby"));
    assert!(index.matches(TagDomain::Item, "outer:gems", "outer:topaz"));
    assert!(index.matches(TagDomain::Fluid, "outer:oils", "outer:seed_oil"));
    assert!(!index.matches(TagDomain::Item, "outer:oils", "outer:seed_oil"));
}

#[test]
fn unified_tags_interoperate_with_culinary_and_auto_adapter_indexes() {
    let mut index = UnifiedTagIndex::new();
    index
        .ingest_member(
            "data/forge/tags/items/crops/tomato.json",
            br##"{"values": ["farmersdelight:tomato", "othermod:tomato"]}"##,
        )
        .expect("tag file ingests");
    index
        .ingest_member(
            "data/c/tags/items/crops.json",
            br##"{"values": ["#c:crops/tomato"]}"##,
        )
        .expect("tag file ingests");

    // Flatten to a culinary TagIndex: alias keys exist for both
    // spellings, and tag_contains resolves through them.
    let flat = index.export_flat(TagDomain::Item);
    assert!(tag_contains(
        &flat,
        "#c:crops/tomato",
        "farmersdelight:tomato"
    ));
    assert!(tag_contains(&flat, "forge:crops/tomato", "othermod:tomato"));
    assert!(tag_contains(&flat, "#c:crops", "farmersdelight:tomato"));
    assert!(!tag_contains(&flat, "#c:crops", "minecraft:wheat_seeds"));

    // Round-trip an existing culinary TagIndex back into the unified
    // index; nested '#' members keep their reference semantics.
    let mut culinary: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    culinary.insert(
        "cookery:ingredients".to_string(),
        ["#forge:crops/tomato", "cookery:rice"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    );
    let mut merged = UnifiedTagIndex::new();
    merged.merge_tag_index(TagDomain::Item, &culinary);
    merged
        .ingest_member(
            "data/c/tags/items/crops/tomato.json",
            br##"{"values": ["farmersdelight:tomato"]}"##,
        )
        .expect("tag file ingests");
    assert!(merged.matches(
        TagDomain::Item,
        "cookery:ingredients",
        "farmersdelight:tomato"
    ));
    assert!(merged.matches(TagDomain::Item, "cookery:ingredients", "cookery:rice"));

    // The auto_adapter flat table (ns:<kind>/<path>) feeds the index too.
    let mut auto_tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    auto_tags.insert(
        "tastypack:items/veggies".to_string(),
        vec!["tastypack:turnip".to_string()],
    );
    auto_tags.insert(
        "tastypack:blocks/heat_sources".to_string(),
        vec!["tastypack:hotplate".to_string()],
    );
    auto_tags.insert(
        "tastypack:entity_types/fish".to_string(),
        vec!["tastypack:codlet".to_string()],
    );
    let merged_count = merged.merge_auto_adapter_tags(&auto_tags);
    assert_eq!(merged_count, 2, "entity_type tags stay out of the index");
    assert!(merged.matches(TagDomain::Item, "tastypack:veggies", "tastypack:turnip"));
    assert!(merged.matches(
        TagDomain::Block,
        "tastypack:heat_sources",
        "tastypack:hotplate"
    ));

    // Vanilla baseline seeds the ten conventional c: families.
    let baseline = UnifiedTagIndex::vanilla_1_21_1_baseline();
    assert!(baseline.matches(TagDomain::Item, "c:ingots", "minecraft:iron_ingot"));
    assert!(baseline.matches(TagDomain::Item, "forge:ores", "minecraft:coal_ore"));
}

#[test]
fn auto_adapter_dimension_mod_and_profile_endpoints_wire_into_the_foundation() {
    use mod_adapter_core::auto_adapter::ModDataBundle;
    use mod_adapter_core::dimension_mod::DimensionModData;
    use mod_adapter_core::profile::ModOffloadProfile;

    // auto_adapter's flat tag table becomes a fully aliased unified index:
    // a file shipped under data/c/ aliases to the forge: spelling, and a
    // mod-namespaced file keeps its own id.
    let mut bundle = ModDataBundle::default();
    bundle.tags.insert(
        "c:items/dusts/iron".to_string(),
        vec!["wiremod:iron_dust".to_string()],
    );
    bundle.tags.insert(
        "wiremod:items/veggies".to_string(),
        vec!["wiremod:turnip".to_string()],
    );
    let index = bundle.unified_tag_index();
    assert!(index.matches(TagDomain::Item, "c:dusts/iron", "wiremod:iron_dust"));
    assert!(index.matches(TagDomain::Item, "forge:dusts/iron", "wiremod:iron_dust"));
    assert!(index.matches(TagDomain::Item, "wiremod:veggies", "wiremod:turnip"));

    // A dimension mod's hidden ore/chest members land in c:ores / c:chests.
    let mut dim_data = DimensionModData::default();
    dim_data.ore_blocks.push("aether:ambrosium_ore".to_string());
    dim_data
        .chest_blocks
        .push("aether:skyroot_chest".to_string());
    let mut index = UnifiedTagIndex::new();
    dim_data.populate_unified_tags(&mut index);
    assert!(index.matches(TagDomain::Block, "c:ores", "aether:ambrosium_ore"));
    assert!(index.matches(TagDomain::Block, "forge:chests", "aether:skyroot_chest"));

    // A declarative offload profile's roles map straight onto bus
    // endpoints — a data-driven mod block registers with zero glue code.
    let profile = ModOffloadProfile::from_json(
        br#"{"mod_id":"wiremod","blocks":[
            {"id":"wiremod:tank","role":"fluid_tank","tank_capacity_mb":5000},
            {"id":"wiremod:crusher","role":"processor"},
            {"id":"wiremod:cable","role":"network_cable"},
            {"id":"wiremod:rock","role":"decorative"}]}"#,
    )
    .expect("profile parses");
    let mut bus = BlockCapabilityBus::new();
    let tank = BlockCoord::new("wiremod:depths", 0, 32, 0);
    let crusher = BlockCoord::new("wiremod:depths", 1, 32, 0);
    let cable = BlockCoord::new("wiremod:depths", 2, 32, 0);
    profile
        .capability_endpoints_for("wiremod:tank")
        .register_into(&mut bus, &tank);
    profile
        .capability_endpoints_for("wiremod:crusher")
        .register_into(&mut bus, &crusher);
    profile
        .capability_endpoints_for("wiremod:cable")
        .register_into(&mut bus, &cable);
    assert!(profile.capability_endpoints_for("wiremod:rock").is_empty());
    assert!(profile
        .capability_endpoints_for("wiremod:unknown")
        .is_empty());

    // The registered endpoints behave like hand-built ones.
    let source_tank = BlockCoord::new("wiremod:depths", 0, 33, 0);
    let mut src = SimpleFluidTank::new(5000);
    src.set_fluid(Some(CapabilityFluidStack::new("wiremod:steam", 4000)));
    bus.register_fluid_handler(source_tank.clone(), Box::new(src));
    assert_eq!(
        bus.transfer_fluid_between(&source_tank, None, &tank, Some(FaceDirection::Up), 4000),
        4000
    );
    assert_eq!(
        bus.fluid_endpoint(&tank).and_then(|h| {
            h.as_any()
                .downcast_ref::<SimpleFluidTank>()
                .and_then(|t| t.fluid().map(|f| f.amount_mb))
        }),
        Some(4000)
    );

    let buffer = BlockCoord::new("wiremod:depths", 3, 32, 0);
    let mut charged = SimpleEnergyBuffer::new(10_000);
    charged.set_stored(8_000);
    bus.register_energy_handler(buffer.clone(), Box::new(charged));
    assert_eq!(
        bus.transfer_energy_between(&buffer, None, &cable, Some(FaceDirection::North), 8_000),
        8_000
    );
}
