/// Unified key for chests/barrels/hoppers/dispensers and chest-minecarts.
///
/// A block container names its dimension: a Nether chest and an Overworld chest at the same
/// coordinates are different block entities. A minecart keeps its UUID (the same cart is the
/// same cart in every dimension). Ender-chest storage is the one exception to the block rule:
/// it is per-player and cross-dimension in vanilla, so `ender_container_key` always files it
/// under the Overworld even when the chest was opened in the Nether or the End.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ContainerKey {
    Block(DimBlockPos),
    Cart([u8; 16]),
}

#[cfg(test)]
mod alpha4_fishing_loot_tests {
    use super::*;

    #[test]
    fn fishing_formulas_pin_vanilla_category_weights_and_wait() {
        assert_eq!(fishing_category_weights(0, true), (85, 10, 5));
        assert_eq!(fishing_category_weights(3, true), (82, 4, 11));
        assert_eq!(fishing_category_weights(3, false), (82, 4, 0));
        assert_eq!(fishing_category_weights(100, true), (0, 0, 205));
        assert_eq!(fishing_wait_ticks(0, 0), 100);
        assert_eq!(fishing_wait_ticks(0, 500), 600);
        assert_eq!(fishing_wait_ticks(3, 500), 300);
        assert_eq!(fishing_wait_ticks(3, 0), 20);
    }

    #[test]
    fn open_water_check_checks_all_100_cells_and_rejects_obstructions() {
        let water = |_x, y, _z| (y <= 0, y > 0, false);
        assert!(is_open_water_check(water, 0, 0, 0));
        let obstructed = |x, y, z| (y <= 0, y > 0, x == 2 && y == 1 && z == 0);
        assert!(!is_open_water_check(obstructed, 0, 0, 0));
        let mut seen = std::collections::HashSet::new();
        is_open_water_check(|x, y, z| { seen.insert((x, y, z)); (y <= 10, y > 10, false) }, 4, 10, -3);
        assert_eq!(seen.len(), 100);
    }

    #[test]
    fn empty_datapack_catalogue_fills_vanilla_structure_chests_and_dispenser() {
        let tables = loot_tables();
        let mut loaded = tables.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::mem::take(&mut *loaded);
        drop(loaded);
        for (name, id) in [
            ("desert_pyramid", "minecraft:chests/desert_pyramid"),
            ("jungle_temple", "minecraft:chests/jungle_temple"),
            ("jungle_temple_dispenser", "minecraft:chests/jungle_temple_dispenser"),
            ("simple_dungeon", "minecraft:chests/simple_dungeon"),
            ("trial_chambers_reward", "minecraft:chests/trial_chambers/reward"),
        ] {
            let result = roll_loot_container_slots(
                crate::generation_payload::ContainerKind::Chest,
                id,
                Some(4242),
            );
            assert!(!result.is_empty(), "{name} should have built-in loot");
        }
        *tables.lock().unwrap_or_else(|e| e.into_inner()) = previous;
    }
}
impl ContainerKey {
    /// The Overworld block key every dimension-less caller means until S3 threads a dimension
    /// through it.
    fn ow(pos: BlockPos) -> Self {
        ContainerKey::Block(ow_block(pos))
    }
    /// The block key for `pos` in `dim` — the shape `ContainerKey::Block` carries natively, so a
    /// Nether chest and an Overworld chest at the same coordinates stay separate containers.
    fn at(dim: DimId, pos: BlockPos) -> Self {
        ContainerKey::Block((dim, pos.0, pos.1, pos.2))
    }
}

/// A smelting recipe from a datapack: ingredient, result, count, experience.
type SmeltRecipe = (Ingr, i64, u8, f32);

#[derive(Clone, Debug)]
struct ChestSyncState {
    window: (ContainerKey, Option<ContainerKey>),
    anchor: Option<BlockPos>,
    state_id: i32,
    last_slots: Vec<Stack>,
}

/// 1.21.1 `Player.aiStep()` (`cmx.m_()`): `player.getBoundingBox().inflate(1.0, 0.5, 1.0)`.
/// `Player` (`cmx.<clinit>`) has `0.6f x 1.8f` standing, `0.6f x 1.5f` crouching,
/// `0.6f x 0.6f` fall-flying.
fn player_touch_aabb(x: f64, y: f64, z: f64, sneaking: bool, fall_fly: bool) -> EntityAabb {
    player_bounding_box(x, y, z, sneaking, fall_fly).inflate(1.0, 0.5, 1.0)
}

// ---------- item drops ----------
// item stack components: (type_id, serialized_data) — vanilla 1.21.1
// structured components (SlotComponent). data is the pre-encoded payload.
#[cfg(test)]
type TestChunkRegion = (HashSet<(i32, i32)>, HashSet<(i32, i32)>);
type Comps = Vec<(i64, Vec<u8>)>;
type Stack = (i64, u8, Comps);
// component type ids (SlotComponentType mapper)
#[allow(dead_code)]
const COMP_CUSTOM_DATA: i64 = 0;
const COMP_MAX_DAMAGE: i64 = 2;
const COMP_DAMAGE: i64 = 3;
const COMP_UNBREAKABLE: i64 = 4;
const COMP_CUSTOM_NAME: i64 = 5;
const COMP_LORE: i64 = 7;
const COMP_ENCHANTMENTS: i64 = 9;
// `minecraft:attribute_modifiers` is data_component_type id 12 in 1.21.1,
// verified against proto/src/data_v767.rs `V767_COMPONENTS` (index = protocol
// id) — the same table the COMP_REPAIR_COST=16 / COMP_MAP_ID=26 /
// COMP_TRIM=35 / COMP_BANNER_PATTERNS=49 anchors come from.
const COMP_ATTRIBUTE_MODIFIERS: i64 = 12;
const COMP_STORED_ENCHANTMENTS: i64 = 23;

/// `AttributeModifier.Operation` varint enum (Protocol 767).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // Shared with combat_and_health.rs; preserve existing variant names.
enum ItemAttrOp {
    AddValue = 0,
    AddMultipliedBase = 1,
    AddMultipliedTotal = 2,
}
impl ItemAttrOp {
    fn from_wire(v: i64) -> Option<Self> {
        Some(match v {
            0 => Self::AddValue,
            1 => Self::AddMultipliedBase,
            2 => Self::AddMultipliedTotal,
            _ => return None,
        })
    }
    fn snbt_name(self) -> &'static str {
        match self {
            Self::AddValue => "add_value",
            Self::AddMultipliedBase => "add_multiplied_base",
            Self::AddMultipliedTotal => "add_multiplied_total",
        }
    }
    fn from_snbt(s: &str) -> Option<Self> {
        Some(match s.trim().trim_matches(['\'', '"']) {
            "add_value" => Self::AddValue,
            "add_multiplied_base" => Self::AddMultipliedBase,
            "add_multiplied_total" => Self::AddMultipliedTotal,
            _ => return None,
        })
    }
}

/// `EquipmentSlotGroup` varint enum (Protocol 767, vanilla 1.21.1 order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemAttrSlotGroup {
    Any = 0,
    MainHand = 1,
    OffHand = 2,
    Hand = 3,
    Feet = 4,
    Legs = 5,
    Chest = 6,
    Head = 7,
    Armor = 8,
    Body = 9,
}
impl ItemAttrSlotGroup {
    fn from_wire(v: i64) -> Option<Self> {
        use ItemAttrSlotGroup::*;
        Some(match v {
            0 => Any,
            1 => MainHand,
            2 => OffHand,
            3 => Hand,
            4 => Feet,
            5 => Legs,
            6 => Chest,
            7 => Head,
            8 => Armor,
            9 => Body,
            _ => return None,
        })
    }
    /// Vanilla `EquipmentSlotGroup.test(EquipmentSlot)`: `equipped_in` is the
    /// slot the item actually sits in (MainHand/OffHand/Feet/Legs/Chest/Head or
    /// Body), `self` is the group the modifier was declared for.
    fn applies_to(self, equipped_in: ItemAttrSlotGroup) -> bool {
        use ItemAttrSlotGroup::*;
        match self {
            Any => true,
            Hand => matches!(equipped_in, MainHand | OffHand),
            Armor => matches!(equipped_in, Feet | Legs | Chest | Head),
            _ => self == equipped_in,
        }
    }
    fn snbt_name(self) -> &'static str {
        use ItemAttrSlotGroup::*;
        match self {
            Any => "any",
            MainHand => "mainhand",
            OffHand => "offhand",
            Hand => "hand",
            Feet => "feet",
            Legs => "legs",
            Chest => "chest",
            Head => "head",
            Armor => "armor",
            Body => "body",
        }
    }
    fn from_snbt(s: &str) -> Option<Self> {
        use ItemAttrSlotGroup::*;
        Some(match s.trim().trim_matches(['\'', '"']) {
            "any" => Any,
            "mainhand" => MainHand,
            "offhand" => OffHand,
            "hand" => Hand,
            "feet" => Feet,
            "legs" => Legs,
            "chest" => Chest,
            "head" => Head,
            "armor" => Armor,
            "body" => Body,
            _ => return None,
        })
    }
}

/// One `ItemAttributeModifiers.Entry`: `attribute`, `modifier` (id + amount +
/// operation), `slot`.
#[derive(Clone, Debug, PartialEq)]
struct ItemAttributeModifier {
    attribute_id: i32,
    modifier_id: String,
    amount: f64,
    operation: ItemAttrOp,
    slot: ItemAttrSlotGroup,
}
/// `minecraft:attribute_modifiers` component payload.
#[derive(Clone, Debug, PartialEq)]
struct AttributeModifiersComponent {
    modifiers: Vec<ItemAttributeModifier>,
    show_in_tooltip: bool,
}

/// `ItemAttributeModifiers.STREAM_CODEC` encode (Protocol 767).
fn attr_modifiers_data(c: &AttributeModifiersComponent) -> Vec<u8> {
    let mut d = Vec::new();
    write_varint(&mut d, c.modifiers.len() as i64);
    for m in &c.modifiers {
        write_varint(&mut d, i64::from(m.attribute_id));
        write_varint(&mut d, m.modifier_id.len() as i64);
        d.extend_from_slice(m.modifier_id.as_bytes());
        d.extend_from_slice(&m.amount.to_be_bytes());
        write_varint(&mut d, m.operation as i64);
        write_varint(&mut d, m.slot as i64);
    }
    d.push(u8::from(c.show_in_tooltip));
    d
}
/// `ItemAttributeModifiers.STREAM_CODEC` decode from a prefix of `input`;
/// consumes exactly the component bytes.
fn attr_modifiers_read(input: &mut &[u8]) -> Option<AttributeModifiersComponent> {
    let n = read_protocol_i32(input)?;
    if !(0..=1024).contains(&n) {
        return None;
    }
    let mut modifiers = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let attribute_id = read_protocol_i32(input)?;
        let id_len = usize::try_from(read_protocol_i32(input)?).ok()?;
        if id_len > 32767 {
            return None;
        }
        let (name, rest) = input.split_at_checked(id_len)?;
        *input = rest;
        let modifier_id = std::str::from_utf8(name).ok()?.to_string();
        let amount = f64::from_be_bytes(input.get(..8)?.try_into().unwrap());
        *input = input.get(8..)?;
        let operation = ItemAttrOp::from_wire(i64::from(read_protocol_i32(input)?))?;
        let slot = ItemAttrSlotGroup::from_wire(i64::from(read_protocol_i32(input)?))?;
        modifiers.push(ItemAttributeModifier {
            attribute_id,
            modifier_id,
            amount,
            operation,
            slot,
        });
    }
    let show_in_tooltip = match *input.first()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    *input = input.get(1..)?;
    Some(AttributeModifiersComponent {
        modifiers,
        show_in_tooltip,
    })
}
/// Whole-slice variant for stored component payloads: trailing bytes reject.
fn attr_modifiers_parse(data: &[u8]) -> Option<AttributeModifiersComponent> {
    let mut rest = data;
    attr_modifiers_read(&mut rest).filter(|_| rest.is_empty())
}
fn stack_attr_modifiers(comps: &Comps) -> Option<AttributeModifiersComponent> {
    comp_get(comps, COMP_ATTRIBUTE_MODIFIERS).and_then(attr_modifiers_parse)
}
// max stack size: explicit component wins, else vanilla defaults
// (1.21.1 minecraft-data: unstackable ids and the 16-stack ids)
fn item_stack_max(item: i64, comps: &Comps) -> i64 {
    if let Some(n) = comp_varint(comps, COMP_MAX_STACK_SIZE) {
        return n.clamp(1, 99);
    }
    // A block's item stacks to what the asset says the block stacks to. The ranges
    // below cover only items that place no block, so they are reached for tools,
    // buckets, potions and the like.
    if let Some(st) = item_to_state(item) {
        if let Some(reg) = crate::block_registry::breg() {
            return reg.stack_size_of(st);
        }
    }
    if matches!(item,
        522..=538 | 765..=791 | 794 | 797..=798 | 801 | 818..=847 | 849
        | 856..=879 | 909..=911 | 914..=920 | 930..=931 | 933 | 963..=979
        | 983 | 998 | 1091 | 1093 | 1114 | 1120 | 1124..=1127 | 1130
        | 1156 | 1158 | 1161..=1163 | 1166..=1186 | 1188 | 1192..=1193
        | 1195..=1203 | 1268)
    {
        1
    } else if matches!(item,
        886..=908 | 912 | 927 | 993 | 1092 | 1123 | 1133..=1148 | 1224)
    {
        16
    } else {
        64
    }
}
fn stack_max_of(st: &Stack) -> i64 {
    item_stack_max(st.0, &st.2)
}
fn comp_get(c: &Comps, ty: i64) -> Option<&[u8]> {
    c.iter().find(|(t, _)| *t == ty).map(|(_, d)| d.as_slice())
}
fn comp_set(c: &mut Comps, ty: i64, data: Vec<u8>) {
    if let Some(e) = c.iter_mut().find(|(t, _)| *t == ty) {
        e.1 = data;
    } else {
        c.push((ty, data));
    }
}
fn comp_varint(c: &Comps, ty: i64) -> Option<i64> {
    comp_get(c, ty).map(|d| read_varint_at(d, 0).0)
}
fn comp_set_varint(c: &mut Comps, ty: i64, v: i64) {
    let mut d = Vec::new();
    write_varint(&mut d, v);
    comp_set(c, ty, d);
}
// Use the shared 1.21.1 durability registry for equipment and tools.
fn armor_max_damage(item: i64) -> Option<i64> {
    item_to_name(item).and_then(item_max_durability).map(i64::from)
}
fn tool_max_damage(item: i64) -> Option<i64> {
    item_to_name(item).and_then(item_max_durability).map(i64::from)
}
fn enchant_id(name: &str) -> Option<i64> {
    Some(match name.trim_start_matches("minecraft:") {
        "aqua_affinity" => 0,
        "bane_of_arthropods" => 1,
        "binding_curse" => 2,
        "blast_protection" => 3,
        "breach" => 4,
        "channeling" => 5,
        "density" => 6,
        "depth_strider" => 7,
        "efficiency" => 8,
        "feather_falling" => 9,
        "fire_aspect" => 10,
        "fire_protection" => 11,
        "flame" => 12,
        "fortune" => 13,
        "frost_walker" => 14,
        "impaling" => 15,
        "infinity" => 16,
        "knockback" => 17,
        "looting" => 18,
        "loyalty" => 19,
        "luck_of_the_sea" => 20,
        "lure" => 21,
        "mending" => 22,
        "multishot" => 23,
        "piercing" => 24,
        "power" => 25,
        "projectile_protection" => 26,
        "protection" => 27,
        "punch" => 28,
        "quick_charge" => 29,
        "respiration" => 30,
        "riptide" => 31,
        "sharpness" => 32,
        "silk_touch" => 33,
        "smite" => 34,
        "soul_speed" => 35,
        "sweeping_edge" => 36,
        "swift_sneak" => 37,
        "thorns" => 38,
        "unbreaking" => 39,
        "vanishing_curse" => 40,
        "wind_burst" => 41,
        _ => return None,
    })
}
fn enchant_name(id: i64) -> Option<&'static str> {
    Some(match id {
        0 => "minecraft:aqua_affinity",
        1 => "minecraft:bane_of_arthropods",
        2 => "minecraft:binding_curse",
        3 => "minecraft:blast_protection",
        4 => "minecraft:breach",
        5 => "minecraft:channeling",
        6 => "minecraft:density",
        7 => "minecraft:depth_strider",
        8 => "minecraft:efficiency",
        9 => "minecraft:feather_falling",
        10 => "minecraft:fire_aspect",
        11 => "minecraft:fire_protection",
        12 => "minecraft:flame",
        13 => "minecraft:fortune",
        14 => "minecraft:frost_walker",
        15 => "minecraft:impaling",
        16 => "minecraft:infinity",
        17 => "minecraft:knockback",
        18 => "minecraft:looting",
        19 => "minecraft:loyalty",
        20 => "minecraft:luck_of_the_sea",
        21 => "minecraft:lure",
        22 => "minecraft:mending",
        23 => "minecraft:multishot",
        24 => "minecraft:piercing",
        25 => "minecraft:power",
        26 => "minecraft:projectile_protection",
        27 => "minecraft:protection",
        28 => "minecraft:punch",
        29 => "minecraft:quick_charge",
        30 => "minecraft:respiration",
        31 => "minecraft:riptide",
        32 => "minecraft:sharpness",
        33 => "minecraft:silk_touch",
        34 => "minecraft:smite",
        35 => "minecraft:soul_speed",
        36 => "minecraft:sweeping_edge",
        37 => "minecraft:swift_sneak",
        38 => "minecraft:thorns",
        39 => "minecraft:unbreaking",
        40 => "minecraft:vanishing_curse",
        41 => "minecraft:wind_burst",
        _ => return None,
    })
}
// enchantments component blob: {ench:[{id,lvl}], showTooltip bool}
fn enchantments_data(list: &[(i64, i64)], show_tip: bool) -> Vec<u8> {
    let mut d = Vec::new();
    write_varint(&mut d, list.len() as i64);
    for (id, lv) in list {
        write_varint(&mut d, *id);
        write_varint(&mut d, *lv);
    }
    d.push(show_tip as u8);
    d
}
fn enchantments_get(c: &Comps) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    if let Some(d) = comp_get(c, COMP_ENCHANTMENTS) {
        let (n, mut p) = read_varint_at(d, 0);
        for _ in 0..n.min(64) {
            if p + 2 > d.len() {
                break;
            }
            let (id, p2) = read_varint_at(d, p);
            let (lv, p3) = read_varint_at(d, p2);
            p = p3;
            out.push((id, lv));
        }
    }
    out
}

// Vanilla destroy time: damage/tick = speed/hardness/(canHarvest?30:100), so
// seconds = hardness*1.5/speed when harvestable and hardness*5.0/speed when not.
// hardness < 0 means unbreakable, 0 means instantaneous. `canHarvest` comes from
// the registry's harvestTools data; the tool *speed* side is still a hand table.
/// Vanilla destroy time, owned by the block registry, which reads it from the same
/// asset it reads everything else from. The fallback only applies if the registry
/// failed to load at all.
fn block_hardness(b: i64) -> f32 {
    match crate::block_registry::breg() {
        Some(reg) => reg.hardness_of(b as u32),
        None => 0.6,
    }
}
/// Whether this tool harvests this block, owned by the registry's `harvestTools`
/// data. It decides both the drop and the fast/slow mining multiplier, so the two
/// call sites must ask the same question.
/// Test-only: production goes through item_stack_max, which resolves the item to its
/// block first.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn stack_size_of(st: u32) -> i64 {
    crate::block_registry::breg()
        .map(|reg| reg.stack_size_of(st))
        .unwrap_or(64)
}
fn tool_kind(block: i64) -> i8 {
    match crate::block_registry::breg() {
        Some(reg) => reg.tool_kind_of(block as u32),
        None => -1,
    }
}
fn can_harvest(block: i64, held_item: i64) -> bool {
    match crate::block_registry::breg() {
        Some(reg) => reg.can_harvest(block as u32, held_item),
        None => true,
    }
}
// tool dig speed; kind order in band: sword,shovel,pick,axe,hoe; tier: wood,stone,gold,iron,diamond,netherite
fn tool_dig_speed(held: &Stack, block: i64) -> f32 {
    // A `minecraft:tool` component replaces the vanilla tier/tag rules with the
    // component's own mining-speed rules (1.20.5+ component semantics).
    if let (Some(tool), Some(reg)) = (tool_component_for(held), breg()) {
        let speed = tool_component_speed(&tool, block as u32, reg);
        return mining_speed_with_modifiers(
            speed,
            speed > 1.0,
            enchant_level(&held.2, 8).clamp(0, i32::MAX as i64) as i32,
            None,
            None,
            false,
            true,
        );
    }
    let kind = if (818..=847).contains(&held.0) {
        (held.0 - 818) % 5
    } else {
        -1
    };
    let right_tool = kind >= 0 && kind == i64::from(tool_kind(block));
    let base = if right_tool {
        item_to_name(held.0).map(tool_base_mining_speed).unwrap_or(1.0)
    } else {
        1.0
    };
    mining_speed_with_modifiers(
        base,
        right_tool && base > 1.0,
        enchant_level(&held.2, 8).clamp(0, i32::MAX as i64) as i32,
        None,
        None,
        false,
        true,
    )
}

fn dig_seconds(block: i64, held: &Stack) -> Option<f32> {
    let h = block_hardness(block);
    if h < 0.0 {
        return None;
    }
    if h == 0.0 {
        return Some(0.0);
    }
    let speed = tool_dig_speed(held, block);
    let can = if let (Some(tool), Some(reg)) = (tool_component_for(held), breg()) {
        tool_component_correct(&tool, block as u32, reg)
    } else {
        can_harvest(block, held.0)
    };
    Some(h * if can { 1.5 } else { 5.0 } / speed)
}

fn enchant_level(c: &Comps, id: i64) -> i64 {
    enchantments_get(c)
        .iter()
        .find(|(e, _)| *e == id)
        .map(|(_, l)| *l)
        .unwrap_or(0)
}
fn enchant_max_level(id: i64) -> i64 {
    match id {
        0 | 2 | 12 | 16 | 22 | 23 | 33 | 40 => 1, // aqua..infinity,mending,multishot,silk,vanishing
        14 | 17 | 10 | 28 => 2,                   // frost, knockback, fire_aspect, punch
        7 | 13 | 18 | 19 | 20 | 21 | 29 | 30 | 31 | 35 | 36 | 37 | 38 | 39 | 41 => 3,
        3 | 4 | 9 | 11 | 24 | 26 | 27 => 4, // protection family, feather, breach, piercing
        _ => 5,                             // damage/efficiency/power/impaling/density
    }
}
// vanilla mutually-exclusive groups; some/any overlap forbids
fn enchant_conflicts(a: i64, b: i64) -> bool {
    const GROUPS: &[&[i64]] = &[
        &[27, 3, 11, 26],   // protection family
        &[32, 34, 1, 6, 4], // sharpness/smite/bane/density/breach
        &[33, 13],          // silk_touch vs fortune
        &[16, 22],          // infinity vs mending
        &[31, 19, 5],       // riptide vs loyalty/channeling
        &[7, 14],           // depth_strider vs frost_walker
        &[23, 24],          // multishot vs piercing
    ];
    a != b && GROUPS.iter().any(|g| g.contains(&a) && g.contains(&b))
}
// TAG_String anonymous-NBT payload: type byte + modified-UTF-8 string.
fn nbt_string(text: &str) -> Vec<u8> {
    let mut d = vec![8u8];
    nbt_str(&mut d, text);
    d
}

// vanilla enchantability, approximated: damageable items accept
// unbreaking/mending always; weapons/tools/armor accept their usual sets.
fn enchant_applicable(item: i64, eid: i64) -> bool {
    let band = if (818..=847).contains(&item) {
        Some(((item - 818) / 5, (item - 818) % 5))
    } else {
        None
    };
    let bow = item == name_to_item("bow").unwrap_or(-1);
    let xbow = item == name_to_item("crossbow").unwrap_or(-2);
    let trident = item == name_to_item("trident").unwrap_or(-3);
    let rod = item == name_to_item("fishing_rod").unwrap_or(-4);
    let mace = item == name_to_item("mace").unwrap_or(-5);
    let armor = (1064..=1115).contains(&item); // leather..netherite armor band
    match eid {
        39 | 22 => tool_max_damage(item).is_some() || band.is_some() || armor, // unbreaking/mending
        32 | 34 | 1 | 17 | 10 | 18 | 36 => {
            band.map(|(_, k)| k == 0 || k == 3).unwrap_or(false) || mace
        } // melee
        8 | 13 | 33 => band.map(|(_, k)| (1..=4).contains(&k)).unwrap_or(false), // efficiency/fortune/silk
        25 | 28 | 12 | 16 => bow,    // power/punch/flame/infinity
        29 | 23 | 24 => xbow,        // quick_charge/multishot/piercing
        19 | 31 | 5 | 15 => trident, // loyalty/riptide/channeling/impaling
        20 | 21 => rod,              // luck_of_the_sea/lure
        27 | 3 | 11 | 26 | 38 | 0 | 7 | 9 | 14 | 30 | 35 | 37 | 2 | 40 => armor, // armor enchants+curses
        6 | 4 | 41 => mace, // density/breach/wind_burst
        _ => true,
    }
}
// Apply one point of durability damage to an inventory stack.
fn damage_stack(st: &mut Stack, random: f32) -> bool {
    let Some(maxd) = comp_varint(&st.2, COMP_MAX_DAMAGE)
        .or_else(|| item_to_name(st.0).and_then(item_max_durability).map(i64::from))
    else {
        return false;
    };
    if comp_get(&st.2, COMP_UNBREAKABLE).is_some() {
        return false;
    }
    let level = enchant_level(&st.2, 39).clamp(0, i32::MAX as i64) as i32;
    let armor = item_to_name(st.0).and_then(armor_parts).is_some();
    if !unbreaking_consumes_durability(level, random, armor) {
        return false;
    }
    // `minecraft:tool.damage_per_block`: vanilla `mineBlock` charges this many
    // durability points per break; melee hits keep the flat 1 via the same sink.
    let dmg = comp_varint(&st.2, COMP_DAMAGE).unwrap_or(0) + tool_damage_per_block(st).max(1);
    if dmg >= maxd {
        return true;
    }
    comp_set_varint(&mut st.2, COMP_DAMAGE, dmg);
    false
}
fn damage_held(me: &Arc<Client>) -> bool {
    let slot = 36 + *me.held.lock().unwrap_or_else(|e| e.into_inner());
    damage_inv_slot(me, slot)
}
fn damage_inv_slot(me: &Client, slot: i16) -> bool {
    let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = inv.get_mut(&slot) else {
        return false;
    };
    let random = (world_rng::next_u64() >> 40) as f32 / (1u32 << 24) as f32;
    if damage_stack(st, random) {
        inv.remove(&slot);
        true
    } else {
        false
    }
}

// resend the held slot to the owner after damage/break so the client
// model stays honest
fn held_slot_resync(me: &Client) {
    let hs = *me.held.lock().unwrap_or_else(|e| e.into_inner());
    let slot = 36 + hs;
    let st = me
        .inv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&slot)
        .cloned()
        .unwrap_or((0, 0, Vec::new()));
    let mut p = vec![0u8];
    write_varint(&mut p, i64::from(inv_state_next(me)));
    p.extend_from_slice(&slot.to_be_bytes());
    p.extend_from_slice(&slot_wire(&st));
    if let Ok(mut w) = me.sock.lock() {
        let _ = send_packet(&mut w, 0x15, p);
    }
}

#[derive(Clone)]
struct Drop {
    eid: i32,
    x: f64,
    y: f64,
    z: f64,
    item: i64,
    cnt: u8,
    comps: Comps,
    age: i32,
    target: Option<[u8; 16]>,
    thrower: Option<[u8; 16]>,
}
/// The `SimItem` view lets the `megu_core::zero_change_sim` merge kernels run
/// the live drop pool — `same_stack` is the exact vanilla mergeability test
/// (item id, pickup target, component blob; `thrower` deliberately absent).
impl megu_core::zero_change_sim::SimItem for Drop {
    fn eid(&self) -> i32 {
        self.eid
    }
    fn pos(&self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }
    fn age(&self) -> i32 {
        self.age
    }
    fn count(&self) -> u32 {
        u32::from(self.cnt)
    }
    fn set_count(&mut self, cnt: u32) {
        self.cnt = cnt.min(u32::from(u8::MAX)) as u8;
    }
    fn stack_cap(&self) -> u32 {
        item_stack_max(self.item, &self.comps).max(1) as u32
    }
    fn same_stack(&self, other: &Self) -> bool {
        self.item == other.item && self.target == other.target && self.comps == other.comps
    }
}
impl Drop {
    /// Vanilla keeps an ItemEntity out of reach for its pickup delay. Both collectors of a
    /// drop ask this one question: the fake-actor absorb and the real client's take. The
    /// client path did not ask it, so a player standing on a drop collected it on the tick
    /// it spawned while the actor path refused the same entity. The age read here is the
    /// live entity's, counted in server ticks by the pass that also expires it, so the
    /// delay keeps its existing cadence instead of gaining a second clock.
    fn pickup_delay_elapsed(&self) -> bool {
        self.age >= server_ticks(PICKUP_DELAY_GT) as i32
    }

    /// 1.21.1 `EntityType.ITEM` (`bsx.ag`): `sized(0.25f, 0.25f)` via `EntityDimensions.makeBoundingBox`
    /// (`bsu.a(DDD)`): `[x - 0.125, x + 0.125] x [y, y + 0.25] x [z - 0.125, z + 0.125]`.
    fn bounding_box(&self) -> EntityAabb {
        EntityAabb::from_dimensions(self.x, self.y, self.z, 0.25, 0.25)
    }

    /// 1.21.1 `Player.aiStep()` (`cmx.m_()`) + `ItemEntity.playerTouch(Player)` (`cjh.b_(Lcmx;)V`):
    /// alive non-spectator whose inflated bounding box intersects the drop's bounding box, and
    /// `target == null || target.equals(player.getUUID())`. `thrower` (`cjh.l`) is not gated here.
    fn can_be_picked_up_by_parts(
        target: Option<[u8; 16]>,
        drop_aabb: EntityAabb,
        collector_uuid: Option<[u8; 16]>,
        spectator: bool,
        alive: bool,
        player_aabb: EntityAabb,
    ) -> bool {
        if spectator || !alive {
            return false;
        }
        let target_ok = match (target, collector_uuid) {
            (None, _) => true,
            (Some(owner), Some(uuid)) => owner == uuid,
            (Some(_), None) => false,
        };
        target_ok && player_aabb.intersects(drop_aabb)
    }

    fn can_be_picked_up_by(
        &self,
        uuid: Option<[u8; 16]>,
        spectator: bool,
        alive: bool,
        player_aabb: EntityAabb,
    ) -> bool {
        Self::can_be_picked_up_by_parts(
            self.target,
            self.bounding_box(),
            uuid,
            spectator,
            alive,
            player_aabb,
        )
    }

    /// Vanilla `Level.popResource`/`spawnAtLocation` plus `ItemEntity`
    /// construction: the entity is created at `±0.125` jitter per axis (y biased
    /// by half the 0.25 height) with delta `(±0.1, 0.2, ±0.1)` and then flies
    /// ~5-10 ticks under gravity, so the landing offset is triangular over
    /// roughly ±0.8 cells — most pops land within one block, a tail reaches
    /// past a player's touch AABB. Engine item entities keep no velocity, so
    /// the flight is folded into the resting spot: jitter plus a triangular
    /// drift, then a settle onto the first non-air cell top below the drifted
    /// column. That is the position every later system (pickup AABB, merge,
    /// despawn) observes: a pop whose vanilla flight would leave the player's
    /// reach is out of reach here too, and one dropped into a dug-out cell
    /// sinks the same way. Player-thrown and restored drops never take this
    /// path.
    fn world_scatter(mut self, dim: DimId) -> Drop {
        let jit = || (world_rng::next_below(250) as f64 - 125.0) / 1000.0;
        let drift = || {
            (world_rng::next_below(200) as i64 + world_rng::next_below(200) as i64
                - 200) as f64
                / 1000.0
                * 8.0
        };
        let mut x = self.x + jit() + drift();
        let mut z = self.z + jit() + drift();
        for _ in 0..4 {
            if block_at_dim(dim, x.floor() as i32, self.y.floor() as i32, z.floor() as i32)
                == 0
            {
                break;
            }
            x = (x + self.x) * 0.5;
            z = (z + self.z) * 0.5;
        }
        let mut y = self.y;
        for step in 0..12 {
            let cy = y.floor() as i32 - step;
            if block_at_dim(dim, x.floor() as i32, cy, z.floor() as i32) != 0 {
                y = cy as f64 + 1.0;
                break;
            }
        }
        self.x = x;
        self.y = y;
        self.z = z;
        self
    }
}
static DROPS: OnceLock<Mutex<Vec<Drop>>> = OnceLock::new();
fn drops() -> &'static Mutex<Vec<Drop>> {
    DROPS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Item entities are dimensionless in storage — `Drop` predates dimensions
/// and every literal construction in `main.rs`/`play_protocol.rs` shares that
/// shape — so the dimension an entity was spawned in rides in this side index
/// keyed by eid. Unregistered eids answer `DimId::Overworld`, matching the
/// world every pre-dim spawn meant. `drop_tick` prunes ids as they despawn.
static DROP_DIMS: OnceLock<Mutex<HashMap<i32, DimId>>> = OnceLock::new();
fn drop_dims() -> &'static Mutex<HashMap<i32, DimId>> {
    DROP_DIMS.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Record a spawn's dimension. Overworld stays implicit so the index holds
/// only the exceptions and stays empty on an Overworld-only world.
fn note_drop_dim(eid: i32, dim: DimId) {
    if dim != DimId::Overworld {
        drop_dims()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(eid, dim);
    }
}
/// The dimension an eid's item entity lives in, `DimId::Overworld` when no
/// non-Overworld spawn ever registered it. Readers that scan a whole pool
/// hoist the `drop_dims()` lock instead; single-lookup callers use this.
#[allow(dead_code)]
fn drop_dim_of(eid: i32) -> DimId {
    drop_dims()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&eid)
        .copied()
        .unwrap_or(DimId::Overworld)
}

// blockstate -> dropped item id (vanilla: stone->cobblestone, grass->dirt)
/// The item a block drops, owned by the block registry, which reads it from the same
/// asset everything else comes from. The hand table this replaced listed 13 states out
/// of the 23364 the asset gives a drop, so granite, the stairs and the slabs dropped
/// nothing at all, and glass dropped itself where vanilla drops nothing without silk
/// touch. The fallback only applies if the registry failed to load.
fn block_drop(st: u32) -> Option<i64> {
    crate::block_registry::breg().and_then(|reg| reg.block_drop(st))
}
/// The second drop, for the blocks whose asset entry lists two. Nothing is lost when a block has
/// only one: the list is filtered to the entries that exist.
fn block_drops(st: u32) -> Vec<i64> {
    let reg = crate::block_registry::breg();
    let first = reg.and_then(|r| r.block_drop(st));
    let second = reg.and_then(|r| r.block_drop_second(st));
    [first, second].into_iter().flatten().collect()
}
fn spawn_drop_pkt(d: &Drop) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, d.eid as i64);
    p.extend_from_slice(&[0u8; 16]);
    write_varint(&mut p, 58); // item entity
    p.extend_from_slice(&d.x.to_be_bytes());
    p.extend_from_slice(&d.y.to_be_bytes());
    p.extend_from_slice(&d.z.to_be_bytes());
    p.extend_from_slice(&[0u8; 3]);
    write_varint(&mut p, 0);
    p.extend_from_slice(&[0u8; 6]);
    p
}
fn drop_meta_pkt(d: &Drop) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, d.eid as i64);
    p.push(8); // metadata key 8 = item
    write_varint(&mut p, 7); // type 7 = item_stack
    write_varint(&mut p, d.cnt as i64); // slot: count
    write_varint(&mut p, d.item); // item id
    write_varint(&mut p, d.comps.len() as i64);
    write_varint(&mut p, 0);
    for (ty, data) in &d.comps {
        write_varint(&mut p, *ty);
        p.extend_from_slice(data);
    }
    p.push(0xff);
    p
}
fn collect_pkt(drop_eid: i32, collector: i32, count: u8) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, drop_eid as i64);
    write_varint(&mut p, collector as i64);
    write_varint(&mut p, count as i64);
    p
}

/// Vanilla merges same-item drops within ~0.5 blocks into one stack (fits <=64); the
/// merged entity keeps the fresher age.
///
/// Returns the absorbed eids in merge order, which is the order the destroy packet goes
/// out in.
///
/// Bucket drops by integer cell. A pair within 0.5 blocks differs by at most one on each
/// axis, so only the own cell and its 26 neighbours can satisfy the distance test.
/// Iterating i in order and candidates in ascending index order keeps the first match
/// identical to scanning every pair, so which drop is absorbed does not change.
fn merge_drops(dp: &mut [Drop]) -> Vec<i32> {
    // Nothing to pair, and the index below allocates: an idle world must not pay for a
    // merge it cannot perform.
    if dp.len() < 2 {
        return Vec::new();
    }
    let __prof = tickprof::enabled();
    let mut __t = __prof.then(std::time::Instant::now);
    // `ReusableCellMerger::merge_items` reproduces this scan bit-identically —
    // integer cell buckets, ascending-index candidates, the exact same-stack
    // test, `dist^2 < 0.25`, older-keeps order — while bucketing and scanning
    // inside its own retained scratch instead of the MERGE_*_SCRATCH pair.
    let merged = XP_DROP_MERGER.with(|m| m.borrow_mut().merge_items(dp).to_vec());
    if let Some(t) = __t {
        tickprof::add("drops/merge_scan", t.elapsed());
    }
    merged
}

type DropPickupSnapshot = (i32, f64, f64, Option<[u8; 16]>, EntityAabb);

// ---------- fresh-drop merge on spawn/tick (MEGA-SCALE §3.2) ----------
// Mirrors the `xp_pending`/`xp_merge_pending` design: drops spawned through
// `drop_spawn` are marked so a spawn burst (a mega-blast's coalesced stacks,
// a farm's loot stream) collapses inside the tick it lands in, ahead of the
// per-tick `merge_drops` pass.
static DROP_PENDING: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
fn drop_pending() -> &'static Mutex<HashSet<i32>> {
    DROP_PENDING.get_or_init(|| Mutex::new(HashSet::new()))
}

/// The pool size at which merging fresh drops pays; below it the per-tick
/// `merge_drops` pass is already cheap and this path stays unobservable.
const DROP_MERGE_FLOOR: usize = 256;

/// `drop_spawn` in `dim`: same spawn, plus the side-index entry that tells
/// dimension-aware readers (hopper suction, chunk drains) which world the
/// entity rests in.
fn drop_spawn_in_dim(
    dim: DimId,
    x: f64,
    y: f64,
    z: f64,
    item: i64,
    cnt: u8,
    comps: Comps,
    age: i32,
    target: Option<[u8; 16]>,
    thrower: Option<[u8; 16]>,
) -> i32 {
    let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
    drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
        eid,
        x,
        y,
        z,
        item,
        cnt,
        comps,
        age,
        target,
        thrower,
    });
    note_drop_dim(eid, dim);
    drop_pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(eid);
    eid
}

/// Spawn an item entity, marking it merge-pending so the next `drop_tick`
/// pass can collapse a spawn burst inside the same tick.
fn drop_spawn(
    x: f64,
    y: f64,
    z: f64,
    item: i64,
    cnt: u8,
    comps: Comps,
    age: i32,
    target: Option<[u8; 16]>,
    thrower: Option<[u8; 16]>,
) -> i32 {
    drop_spawn_in_dim(DimId::Overworld, x, y, z, item, cnt, comps, age, target, thrower)
}

/// `drop_spawn` for world drops: the vanilla `popResource` scatter runs first,
/// then the same merge-pending mark so a loot burst still collapses inside the
/// tick it lands in. Player-thrown and restored drops never take this path.
fn drop_spawn_scattered(
    dim: DimId,
    x: f64,
    y: f64,
    z: f64,
    item: i64,
    cnt: u8,
    comps: Comps,
    age: i32,
    target: Option<[u8; 16]>,
    thrower: Option<[u8; 16]>,
) -> i32 {
    let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
    drops().lock().unwrap_or_else(|e| e.into_inner()).push(
        Drop {
            eid,
            x,
            y,
            z,
            item,
            cnt,
            comps,
            age,
            target,
            thrower,
        }
        .world_scatter(dim),
    );
    note_drop_dim(eid, dim);
    drop_pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(eid);
    eid
}

/// Merge `pending` donor eids into the locked drop pool. Keepers (pre-existing
/// drops) precede donors in the pool, so a fresh drop feeds the earliest
/// compatible neighbour within 0.5 up to `item_stack_max`; tombstoned drops
/// are removed in one `retain` pass. Returns the absorbed eids in merge order.
fn drop_merge_pending_locked(dp: &mut Vec<Drop>, pending: &HashSet<i32>) -> Vec<i32> {
    // `ReusableCellMerger::merge_pending_items` is the same keeper/donor
    // kernel both builds used to pick between — 2.0 cells, the exact
    // same-stack compare, `cnt`/`max` transfer bound, `u8::MAX` clamp on
    // survivors — so `mod-suite` and local runs share one engine instead of
    // mirroring `merge_item_stacks`.
    let eaten = XP_DROP_MERGER.with(|m| {
        m.borrow_mut()
            .merge_pending_items(dp, |eid| pending.contains(&eid))
            .to_vec()
    });
    if !eaten.is_empty() {
        let gone: HashSet<i32> = eaten.iter().copied().collect();
        dp.retain(|d| !gone.contains(&d.eid));
    }
    eaten
}

/// Collapse freshly spawned drops into their older neighbours — the merge the
/// `merge_drops` pass would do, done the same tick the burst lands. Runs only
/// once `drops()` outgrows `DROP_MERGE_FLOOR`, so the path cannot fire on the
/// pinned save-payload fixture (which never spawns through `drop_spawn`).
fn drop_merge_pending() {
    let pending: HashSet<i32> = {
        let mut p = drop_pending().lock().unwrap_or_else(|e| e.into_inner());
        if p.is_empty() {
            return;
        }
        std::mem::take(&mut *p)
    };
    let mut dp = drops().lock().unwrap_or_else(|e| e.into_inner());
    if dp.len() <= DROP_MERGE_FLOOR {
        return;
    }
    let eaten = drop_merge_pending_locked(&mut dp, &pending);
    drop(dp);
    for eid in eaten {
        death_slot_hint_forget(eid);
        broadcast_seen_destroy(eid);
    }
}

fn furnace_tick() {
    // The due queue's clock is the sequence of `furnace_tick` calls rather
    // than `TICK_IDX`: a re-armed deadline must fire on the very next call even
    // when the world tick index is frozen (unit tests step this function
    // directly). One game tick runs exactly one `furnace_tick`, so the two
    // clocks never disagree in production.
    let now = FURNACE_TICK_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut due = Vec::new();
    furnace_due_queue()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain_due_into(now, &mut due);
    if due.is_empty() {
        return;
    }
    let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
    // One region borrow for the whole batch. Acquiring the mutex per furnace was
    // what dominated this stage, not the set lookup itself.
    let region = region_guard();
    let mut dirty_pos = FURNACE_DIRTY_SCRATCH.with(scratch_take);
    let mut rearm = Vec::new();
    for pos in due {
        let Some(f) = fs.get_mut(&pos) else {
            // The furnace was removed while queued; the drained heap entry is
            // the last trace of its cancelled deadline.
            continue;
        };
        if !region.is_warm(pos.1, pos.3) {
            // A cold furnace keeps a next-tick deadline: no mutation event
            // announces its chunk warming back up, so the deadline itself is
            // the only thing that can make it run again.
            rearm.push(pos);
            continue;
        }
        FURNACE_TICK_VISITS.fetch_add(1, Ordering::Relaxed);
        // The recipe result is only ever read behind `input.1 > 0`, so an empty
        // input skips the lookup instead of hashing an item it cannot smelt.
        let out = if f.input.1 > 0 {
            // Keyed on the content revision as well as the item, so the answer is
            // resolved once per revision instead of scanning every datapack recipe
            // for every furnace on every tick.
            let rev = content_revision();
            f.recipe_cache
                .get_or_compute((f.input.0, rev), || smelt_out(f.input.0))
        } else {
            None
        };
        // A smelt output never carries components, so an output stack holding any
        // cannot be merged with one.
        let can_smelt = f.input.1 > 0
            && out
                .as_ref()
                .map(|o| {
                    f.output.1 == 0
                        || (f.output.0 == o.0
                            && f.output.2.is_empty()
                            && f.output.1 as i64 + o.1 as i64 <= stack_max_of(&f.output))
                })
                .unwrap_or(false);
        if f.burn_left <= 0 && can_smelt && f.fuel.1 > 0 {
            let t = f
                .fuel_cache
                .get_or_compute(f.fuel.0, || fuel_ticks(f.fuel.0));
            if t > 0 {
                f.burn_left = t;
                f.fuel.1 -= 1;
                if f.fuel.1 == 0 {
                    f.fuel = (0, 0, Vec::new());
                }
                f.dirty = true;
            }
        }
        if f.burn_left > 0 && can_smelt {
            f.burn_left -= 1;
            f.progress += 1;
            if f.progress >= server_ticks(200) as i32 {
                f.progress = 0;
                let r = out.unwrap();
                f.xp += smelt_xp(f.input.0);
                f.output = (r.0, f.output.1 + r.1, f.output.2.clone());
                f.input.1 -= 1;
                if f.input.1 == 0 {
                    f.input = (0, 0, Vec::new());
                }
                f.dirty = true;
            }
        } else if f.progress > 0 {
            f.progress -= 1;
        }
        // A furnace that can still make progress next tick re-arms. One that
        // ended this pass inert — no burn, no cook progress — stays out of the
        // queue and sleeps at zero per-tick cost until a mutation wakes it.
        if f.burn_left > 0 || f.progress > 0 {
            rearm.push(pos);
        }
        if f.dirty {
            f.dirty = false;
            dirty_pos.push(pos);
        }
    }
    drop(region);
    drop(fs);
    if !rearm.is_empty() {
        let mut q = furnace_due_queue().lock().unwrap_or_else(|e| e.into_inner());
        for pos in rearm {
            q.schedule_due(pos, now + 1);
        }
    }
    if dirty_pos.is_empty() {
        FURNACE_DIRTY_SCRATCH.with(|s| scratch_return(s, dirty_pos));
        return;
    }
    for pos in &dirty_pos {
        wake_hoppers_around(*pos);
    }
    let cs: Vec<Arc<Client>> = client_snapshot();
    for c in cs {
        let fp = *c.open_furnace.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((dim, x, y, z)) = fp {
            if dirty_pos.contains(&(dim, x, y, z)) {
                let inv = c.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let slots = furnace_slot_snapshot_dim(dim, (x, y, z));
                furnace_push_items_snap(&c, &inv, &slots);
            }
        }
    }
    FURNACE_DIRTY_SCRATCH.with(|s| scratch_return(s, dirty_pos));
}

static HOPPERS: OnceLock<Mutex<std::collections::HashSet<(i32, i32, i32)>>> = OnceLock::new();
fn hoppers() -> &'static Mutex<std::collections::HashSet<(i32, i32, i32)>> {
    HOPPERS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
/// Where the synthetic per-player ender-chest key starts: far outside the world, so
/// `ContainerKey::Block` can carry it like any container without colliding with a
/// real block position.
const ENDER_KEY_X: i32 = 20_000_000;
const ENDER_KEY_Y: i32 = 310;
fn ender_key(uuid: &[u8; 16]) -> (i32, i32, i32) {
    (
        ENDER_KEY_X + (u32::from_be_bytes([uuid[0], uuid[1], uuid[2], uuid[3]]) & 0xfffff) as i32,
        ENDER_KEY_Y,
        (u32::from_be_bytes([uuid[4], uuid[5], uuid[6], uuid[7]]) & 0xfffff) as i32,
    )
}
/// Is this the synthetic ender-chest key rather than a world block? The key has to be
/// distinguishable, because "the world still holds this container" is false for it by
/// construction: there is no block entity behind an ender chest's per-player storage.
fn is_ender_key(pos: (i32, i32, i32)) -> bool {
    pos.0 >= ENDER_KEY_X && pos.1 == ENDER_KEY_Y
}
/// The storage key for one player's Ender chest. Vanilla Ender chests are per-player and shared
/// across every dimension, so the synthetic position is always filed under the Overworld: opening
/// one from the Nether or the End reaches this exact entry, and its bytes stay in the legacy
/// section where an old build expects them.
fn ender_container_key(uuid: &[u8; 16]) -> ContainerKey {
    let ek = ender_key(uuid);
    ContainerKey::Block((DimId::Overworld, ek.0, ek.1, ek.2))
}

// ---------- idle-hopper sleep probe ----------
//
// `hopper_tick` runs `hopper_push | hopper_pull | hopper_suck` for every due
// hopper on every visit. For a settled array — hoppers whose pull source,
// push target, own slots and suction cell have all come back empty-handed —
// that work is provably wasted: the scan can only ever report "nothing".
//
// The probe fingerprints exactly the state those three scans can observe, in
// bounded O(1) work per visit (containers cap at 54 slots, the suction window
// at the drops resting in 18 spatial cells). When the fingerprint matches the
// one recorded after the last empty-handed scan, the visit is skipped and the
// queue/backoff contract receives the same `did_work = false` a real scan
// would have produced, so scheduling stays bit-identical.
//
// The fingerprint reads live state on every visit instead of relying on
// mutation hooks: container writes also arrive through packet handlers,
// commands and the save loader, and one missed hook would strand items a
// vanilla hopper would have moved. Reading the watched cells directly makes a
// stale "still idle" verdict impossible — any change, from any file or
// thread, lands in the fingerprint on the very next visit.

/// One drop's spawn-time identity for suction-window queries. Position, item
/// and components are immutable after spawn; `cnt` and `age` are deliberately
/// absent — `cnt` is the one mutable field besides `age`, and the "does
/// anything fit?" verdict never reads it (room is checked before any count is
/// spent), so a stale snapshot can never flip a skip decision.
#[derive(Clone)]
struct DropCellSnap {
    eid: i32,
    /// Spawn dimension from the side index — the probe must not answer an
    /// Overworld suction cell with a drop resting at the same Nether coords.
    dim: DimId,
    x: f64,
    y: f64,
    z: f64,
    item: i64,
    comps_hash: u64,
}
/// Drops bucketed by unit cell, rebuilt only when `drops()` membership may
/// have moved. `hopper_suck` and the sleep probe turn the vanilla O(N) entity
/// scan into a lookup over the cells the suction window can touch.
struct DropCells {
    /// Membership version, all O(1) reads taken under the `drops()` lock:
    /// length, the `NEXT_EID` high-water mark, and the two end eids. Every
    /// production spawn draws a fresh eid, so a remove+add that keeps `len`
    /// still moves the mark; count/age edits move nothing and cannot change
    /// window membership.
    len: usize,
    mark: i32,
    first_eid: i32,
    last_eid: i32,
    /// Server tick the map was built on. Membership edits are caught by the
    /// version tuple, but a `/data` teleport rewrites `x/y/z` in place without
    /// touching any of it — rebuilding once per tick bounds that staleness to
    /// under one activation, which is the same freshness a live scan has.
    built_tick: u64,
    map: HashMap<(i32, i32, i32), Vec<DropCellSnap>>,
}
static DROP_CELLS: OnceLock<Mutex<DropCells>> = OnceLock::new();
fn drop_cells() -> &'static Mutex<DropCells> {
    DROP_CELLS.get_or_init(|| {
        Mutex::new(DropCells {
            len: usize::MAX,
            mark: 0,
            first_eid: 0,
            last_eid: 0,
            built_tick: u64::MAX,
            map: HashMap::new(),
        })
    })
}
/// FNV-1a over the component list: order matters (`Comps` equality is a Vec
/// compare), so this folds sequentially rather than summing.
fn comps_fingerprint(comps: &Comps) -> u64 {
    let mut h = megu_core::hash::FNV1A64_OFFSET_BASIS;
    for (ty, data) in comps {
        h = (h ^ (*ty as u64)).wrapping_mul(megu_core::hash::FNV1A64_PRIME);
        h = (h ^ data.len() as u64).wrapping_mul(megu_core::hash::FNV1A64_PRIME);
        for &b in data {
            h = (h ^ u64::from(b)).wrapping_mul(megu_core::hash::FNV1A64_PRIME);
        }
    }
    h
}
/// Refresh the spatial index when `drops()` membership may have moved. The
/// version tuple is read and the map rebuilt under the same `drops()` lock,
/// so the snapshot and its stamp can never disagree.
fn ensure_drop_cells() {
    let dl = drops().lock().unwrap_or_else(|e| e.into_inner());
    let len = dl.len();
    let mark = NEXT_EID.load(Ordering::Relaxed);
    let (first_eid, last_eid) = match (dl.first(), dl.last()) {
        (Some(f), Some(l)) => (f.eid, l.eid),
        _ => (0, 0),
    };
    let tick = TICK_IDX.load(Ordering::Relaxed);
    {
        let dc = drop_cells().lock().unwrap_or_else(|e| e.into_inner());
        if dc.built_tick == tick
            && dc.len == len
            && dc.mark == mark
            && dc.first_eid == first_eid
            && dc.last_eid == last_eid
        {
            return;
        }
    }
    let mut map: HashMap<(i32, i32, i32), Vec<DropCellSnap>> =
        HashMap::with_capacity(len.saturating_mul(2).saturating_add(8));
    let dims = drop_dims().lock().unwrap_or_else(|e| e.into_inner());
    for d in dl.iter() {
        let snap = DropCellSnap {
            eid: d.eid,
            dim: dims.get(&d.eid).copied().unwrap_or(DimId::Overworld),
            x: d.x,
            y: d.y,
            z: d.z,
            item: d.item,
            comps_hash: comps_fingerprint(&d.comps),
        };
        map.entry((d.x.floor() as i32, d.y.floor() as i32, d.z.floor() as i32))
            .or_default()
            .push(snap);
    }
    let mut dc = drop_cells().lock().unwrap_or_else(|e| e.into_inner());
    dc.len = len;
    dc.mark = mark;
    dc.first_eid = first_eid;
    dc.last_eid = last_eid;
    dc.built_tick = tick;
    dc.map = map;
}
/// The exact `hopper_suck` window, evaluated on any position triple.
#[inline]
fn in_suction_cell(x: f64, y: f64, z: f64, hx: i32, hy: i32, hz: i32) -> bool {
    (x - (hx as f64 + 0.5)).abs() < 0.7
        && y > hy as f64 + 0.4
        && y < hy as f64 + 1.6
        && (z - (hz as f64 + 0.5)).abs() < 0.7
}
/// The cell range the suction window can touch for integer hopper coords:
/// x/z span `(h ± 0.2, h + 1.2)` → cells `h-1..=h+1`; y spans `(hy+0.4,
/// hy+1.6)` → cells `hy..=hy+1`. The predicate re-checks every snap exactly,
/// so the coarse range only bounds the lookup count.
fn each_suction_cell(hx: i32, hy: i32, hz: i32, mut f: impl FnMut(i32, i32, i32) -> bool) -> bool {
    for cx in hx - 1..=hx + 1 {
        for cy in hy..=hy + 1 {
            for cz in hz - 1..=hz + 1 {
                if f(cx, cy, cz) {
                    return true;
                }
            }
        }
    }
    false
}
/// Whether any snap sits in the suction cell — the O(1) idle exit for
/// `hopper_suck`. A `false` here is only reached after `ensure_drop_cells`
/// proved the index matches `drops()` membership.
#[allow(dead_code)] // the Overworld view of `suction_nonempty_in`
fn suction_nonempty(hx: i32, hy: i32, hz: i32) -> bool {
    suction_nonempty_in(DimId::Overworld, hx, hy, hz)
}
/// `suction_nonempty` restricted to drops spawned in `dim`.
fn suction_nonempty_in(dim: DimId, hx: i32, hy: i32, hz: i32) -> bool {
    ensure_drop_cells();
    let dc = drop_cells().lock().unwrap_or_else(|e| e.into_inner());
    each_suction_cell(hx, hy, hz, |cx, cy, cz| {
        dc.map.get(&(cx, cy, cz)).is_some_and(|v| {
            v.iter()
                .any(|s| s.dim == dim && in_suction_cell(s.x, s.y, s.z, hx, hy, hz))
        })
    })
}
/// Drops resting in `pos`'s suction cell as spawn-time snapshots, sorted by
/// eid — the same order `hopper_suck` scans them in. The caller owns `out`
/// (a reusable scratch buffer in the probe) so an idle visit allocates
/// nothing.
#[allow(dead_code)] // the Overworld view of `suction_snaps_into_in`
fn suction_snaps_into(hx: i32, hy: i32, hz: i32, out: &mut Vec<DropCellSnap>) {
    suction_snaps_into_in(DimId::Overworld, hx, hy, hz, out)
}
/// `suction_snaps_into` restricted to drops spawned in `dim`.
fn suction_snaps_into_in(dim: DimId, hx: i32, hy: i32, hz: i32, out: &mut Vec<DropCellSnap>) {
    ensure_drop_cells();
    let dc = drop_cells().lock().unwrap_or_else(|e| e.into_inner());
    out.clear();
    each_suction_cell(hx, hy, hz, |cx, cy, cz| {
        if let Some(v) = dc.map.get(&(cx, cy, cz)) {
            out.extend(
                v.iter()
                    .filter(|s| s.dim == dim && in_suction_cell(s.x, s.y, s.z, hx, hy, hz))
                    .cloned(),
            );
        }
        false
    });
    out.sort_by_key(|s| s.eid);
}
/// Sequential 128-bit mix for fingerprint fields whose order is fixed.
#[inline]
fn mixv(acc: u128, x: u64) -> u128 {
    acc.wrapping_mul(0x9E37_79B1_97F4_A7C1)
        .wrapping_add(x as u128)
}
fn mix_pos(acc: u128, pos: Option<(i32, i32, i32)>) -> u128 {
    match pos {
        None => mixv(acc, u64::MAX),
        Some(p) => mixv(mixv(mixv(acc, p.0 as u64), p.1 as u64), p.2 as u64),
    }
}
/// One `(slot, stack)` entry's contribution. Entries are summed into the map
/// fingerprint, so `HashMap` iteration order cannot leak into the result.
fn mix_stack_entry(acc: u128, slot: i16, st: &Stack) -> u128 {
    let h = (slot as i64 as u64 as u128)
        .wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        .wrapping_add(st.0 as u64 as u128);
    let h = h
        .wrapping_mul(0x1656_67B1_9E37_79F9)
        .wrapping_add(st.1 as u128);
    let h = h
        .wrapping_mul(0x85EB_CA77_C2B2_AE63)
        .wrapping_add(comps_fingerprint(&st.2) as u128);
    acc.wrapping_add(h)
}
/// Fingerprint one container map. Absent and present-but-empty hash
/// differently — the scan reads them through the same slot probes, and the
/// distinction costs nothing.
fn mix_container_map(acc: u128, map: Option<&HashMap<i16, Stack>>) -> u128 {
    let Some(m) = map else {
        return mixv(acc, u64::MAX);
    };
    let mut h = m.len() as u128;
    for (slot, st) in m {
        h = mix_stack_entry(h, *slot, st);
    }
    mixv(acc.wrapping_add(h), 0xD6E8_FEB8_6659_FD93)
}
/// `chest_partner` for a state the caller already read: identical pairing
/// rules, minus the redundant `block_at` — the probe fingerprints the resolved
/// position, which is everything the partner lookup can observe.
fn probe_partner_dim(dim: DimId, st: u32, pos: (i32, i32, i32)) -> Option<(i32, i32, i32)> {
    chest_pair_of_dim(dim, st, pos).map(|(right, left)| {
        if pos == right {
            left
        } else {
            right
        }
    })
}
/// The watched-state fingerprint for one hopper visit: everything
/// `hopper_transfer_dim` and `hopper_suck` can observe, and nothing
/// else. Block states cover container-ness and double-chest pairing; the
/// resolved partner positions stand in for the neighbour reads `chest_pair`
/// makes; container maps cover slot contents; pending-loot flags cover the
/// `unpack_block_loot_if_needed` materialization; the suction snaps cover the
/// drop window. `content_revision` rides along so a datapack reload can never
/// hide behind an unchanged world image.
fn hopper_probe_epoch(hx: i32, hy: i32, hz: i32, dst: (i32, i32, i32)) -> u128 {
    hopper_probe_epoch_in_dim(DimId::Overworld, hx, hy, hz, dst)
}
/// `hopper_probe_epoch` in `dim`: identical fingerprint, read against
/// `dim`'s blocks, dim-keyed containers and drops spawned in `dim`.
fn hopper_probe_epoch_in_dim(dim: DimId, hx: i32, hy: i32, hz: i32, dst: (i32, i32, i32)) -> u128 {
    let own = (hx, hy, hz);
    let above = (hx, hy + 1, hz);
    let ast = block_at_dim(dim, above.0, above.1, above.2);
    let dst_st = block_at_dim(dim, dst.0, dst.1, dst.2);
    let ap = probe_partner_dim(dim, ast, above);
    let dp = probe_partner_dim(dim, dst_st, dst);
    let mut acc = mixv(content_revision() as u128, 0x5BD1_E995_27D4_EB2F);
    acc = mixv(acc, ast as u64);
    acc = mixv(acc, dst_st as u64);
    acc = mix_pos(acc, ap);
    acc = mix_pos(acc, dp);
    {
        let all = chests().lock().unwrap_or_else(|e| e.into_inner());
        acc = mix_container_map(acc, all.get(&ContainerKey::at(dim, own)));
        acc = mix_container_map(acc, all.get(&ContainerKey::at(dim, above)));
        if let Some(p) = ap {
            acc = mix_container_map(acc, all.get(&ContainerKey::at(dim, p)));
        }
        acc = mix_container_map(acc, all.get(&ContainerKey::at(dim, dst)));
        if let Some(p) = dp {
            acc = mix_container_map(acc, all.get(&ContainerKey::at(dim, p)));
        }
    }
    {
        let pend = pending_block_loot().lock().unwrap_or_else(|e| e.into_inner());
        for pos in [Some(own), Some(above), ap, Some(dst), dp]
            .into_iter()
            .flatten()
        {
            acc = mixv(acc, u64::from(pend.contains_key(&(dim, pos.0, pos.1, pos.2))));
        }
    }
    for pos in [above, dst] {
        if let Some(furnace) = hopper_furnace_pos(dim, pos) {
            let slots = furnace_slot_snapshot_dim(furnace.0, (furnace.1, furnace.2, furnace.3));
            for (slot, stack) in slots.iter().enumerate() {
                acc = mix_stack_entry(acc, slot as i16, stack);
            }
        }
    }
    let mut snaps = SUCTION_SNAP_SCRATCH.with(scratch_take);
    suction_snaps_into_in(dim, hx, hy, hz, &mut snaps);
    for s in &snaps {
        acc = mixv(acc, s.eid as u64);
        acc = mixv(acc, s.item as u64);
        acc = mixv(acc, s.comps_hash);
        acc = mixv(acc, s.x.to_bits());
        acc = mixv(acc, s.y.to_bits());
        acc = mixv(acc, s.z.to_bits());
    }
    SUCTION_SNAP_SCRATCH.with(|s| scratch_return(s, snaps));
    #[cfg(feature = "mod-suite")]
    if hopper_cap_endpoint_present(dim, above) || hopper_cap_endpoint_present(dim, dst) {
        // A capability-bus endpoint's contents live outside every watcher
        // above: the bus has no revision to fold in, so the only honest
        // fingerprint is one that can never repeat — a hopper facing a mod
        // block re-scans every visit and the backoff ladder still bounds
        // the idle cost.
        acc = mixv(acc, HOPPER_CAP_PROBE_NONCE.fetch_add(1, Ordering::Relaxed));
    }
    acc
}
/// Per-hopper sleep state: the fingerprint recorded after the last scan and
/// whether that scan moved nothing. Only `idle` hoppers may skip, and only on
/// an unchanged fingerprint; an active hopper always re-scans and never pays
/// for the probe at all.
struct HopperSleep {
    epoch: u128,
    idle: bool,
}
type HopperSleepMap = HashMap<(i32, i32, i32), HopperSleep>;
static HOPPER_SLEEP: OnceLock<Mutex<HopperSleepMap>> = OnceLock::new();
fn hopper_sleep() -> &'static Mutex<HopperSleepMap> {
    HOPPER_SLEEP.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Visit accounting the `bot_hopper_*` tests read: real scans versus
/// fingerprint-proven skips.
static HOPPER_SCAN_RUNS: AtomicU64 = AtomicU64::new(0);
static HOPPER_SCAN_SKIPS: AtomicU64 = AtomicU64::new(0);
/// Bound on the sleep map so positions whose hoppers were removed outside the
/// tick path cannot grow it without limit; a sweep only runs past this size.
const HOPPER_SLEEP_MAX: usize = 8192;

// ---------- reusable tick scratch (PERF-MEM-ZERO-ALLOC) ----------
//
// Hot tick loops used to allocate a fresh Vec/HashMap for every dirty batch,
// probe window, merge index and sync diff. Each kind keeps one bounded buffer
// per thread through `scratch_take`/`scratch_return`; a buffer a burst grew
// past the cap is dropped on return so idle RAM stays tiny. Reuse is
// allocation-only: the buffers never carry state across calls.

/// Idle-RAM bound per reusable scratch buffer, measured as
/// `capacity * size_of::<T>()` so element size cannot quietly scale it.
const SCRATCH_CAP_BYTES: usize = 256 * 1024;

/// Check out the thread's scratch buffer, leaving the slot empty until
/// `scratch_return` hands the finished buffer back. Nested use of the same
/// slot just sees an empty Vec.
fn scratch_take<T>(slot: &std::cell::RefCell<Vec<T>>) -> Vec<T> {
    std::mem::take(&mut *slot.borrow_mut())
}

/// Hand a finished scratch buffer back, emptied; a buffer a burst grew past
/// `SCRATCH_CAP_BYTES` is dropped instead of kept.
fn scratch_return<T>(slot: &std::cell::RefCell<Vec<T>>, mut buf: Vec<T>) {
    buf.clear();
    *slot.borrow_mut() = if buf.capacity() * std::mem::size_of::<T>() <= SCRATCH_CAP_BYTES {
        buf
    } else {
        Vec::new()
    };
}

/// Same take/return for a scratch map. `HashMap::capacity` is in entries;
/// control-byte overhead is covered by the `+ 16` per entry.
fn scratch_map_take<K, V, S>(slot: &std::cell::RefCell<HashMap<K, V, S>>) -> HashMap<K, V, S>
where
    S: Default + std::hash::BuildHasher,
{
    std::mem::take(&mut *slot.borrow_mut())
}
fn scratch_map_return<K, V, S>(slot: &std::cell::RefCell<HashMap<K, V, S>>, mut map: HashMap<K, V, S>)
where
    S: Default + std::hash::BuildHasher,
{
    map.clear();
    *slot.borrow_mut() = if map.capacity() * (std::mem::size_of::<(K, V)>() + 16)
        <= SCRATCH_CAP_BYTES
    {
        map
    } else {
        HashMap::default()
    };
}

type BrewingUpdateBatch = HashMap<DimBlockPos, (bool, [WindowPropertyUpdate; 2], bool)>;

thread_local! {
    /// `furnace_tick`'s dirty-position batch.
    static FURNACE_DIRTY_SCRATCH: std::cell::RefCell<Vec<DimBlockPos>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// `live_brewing_tick`'s per-tick update batch. `HashMap::new` is not
    /// const, so this one slot skips the const-init form.
    static BREWING_UPDATES_SCRATCH: std::cell::RefCell<BrewingUpdateBatch> =
        std::cell::RefCell::new(HashMap::new());
    /// `hopper_probe_epoch`'s suction-window snapshot list.
    static SUCTION_SNAP_SCRATCH: std::cell::RefCell<Vec<DropCellSnap>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// `hopper_suck`'s filtered cell candidates.
    static HOPPER_CELL_SCRATCH: std::cell::RefCell<Vec<(i32, i64, u8, Comps)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// `chest_viewer_tick_on`'s per-viewer slot-diff batch.
    static CHEST_SYNC_SCRATCH: std::cell::RefCell<Vec<(i16, i32, Stack)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// `merge_drops`/`merge_xp_orbs`/`drop_merge_pending_locked`'s shared
    /// merge engine: cell index, candidates and output all retained across
    /// calls, replacing the per-pass `CellBuckets`/`Vec` scratch pair.
    static XP_DROP_MERGER: std::cell::RefCell<megu_core::zero_change_sim::ReusableCellMerger> =
        std::cell::RefCell::new(megu_core::zero_change_sim::ReusableCellMerger::new());
    /// `drop_tick`'s despawn eid list.
    static DROP_DESPAWN_SCRATCH: std::cell::RefCell<Vec<i32>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

// ---------- event-driven furnace sleep / idle brewing-stand probe ----------
//
// `furnace_tick` used to resolve the recipe and fuel for every warm furnace on
// every tick, and `live_brewing_tick` still re-derives potion/ingredient state
// for every stand. Furnaces now sleep in a deadline queue: a furnace is
// visited only when a mutation wakes it or a burn/cook deadline it re-armed
// itself comes due, so an idle farm costs zero per tick. Brewing stands keep
// the fingerprint probe — their watched state is cheap to read live, and a
// stand's five slots see no external writers the way furnace slots do.

/// Furnace deadlines: a position is visited only when it wakes itself for
/// burn/cook progress or a mutation event makes it due early.
static FURNACE_DUE_QUEUE: OnceLock<Mutex<tick_queue::DueMachineQueue<DimBlockPos>>> =
    OnceLock::new();
fn furnace_due_queue() -> &'static Mutex<tick_queue::DueMachineQueue<DimBlockPos>> {
    FURNACE_DUE_QUEUE.get_or_init(|| Mutex::new(tick_queue::DueMachineQueue::new()))
}
/// One step of the queue's clock per `furnace_tick` call, so re-armed deadlines
/// fire on the very next drain even where the world tick index is not moving.
static FURNACE_TICK_SEQ: AtomicU64 = AtomicU64::new(0);
/// Warm-furnace visits. The `xpoll_be_sleep` lane reads deltas on this to prove
/// an idle furnace is never stepped.
static FURNACE_TICK_VISITS: AtomicU64 = AtomicU64::new(0);

/// A slot, timer or content write at `(dim, x, y, z)` makes the furnace due on
/// the next `furnace_tick`. The wake is deduped by the queue, so callers may
/// fire it unconditionally after any mutation.
fn wake_furnace_at(dim: DimId, x: i32, y: i32, z: i32) {
    furnace_due_queue()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .wake((dim, x, y, z), FURNACE_TICK_SEQ.load(Ordering::Relaxed));
    wake_hoppers_around((dim, x, y, z));
}

/// The block entity at `(dim, x, y, z)` is gone: cancel its pending deadline so
/// a removed furnace can never be visited again.
fn remove_furnace_at(dim: DimId, x: i32, y: i32, z: i32) {
    furnace_due_queue()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove((dim, x, y, z));
}

/// Whole-store refresh after a load, restore or content-revision bump: clear
/// every deadline and wake each registered furnace once. Each gets one settle
/// visit, re-arms only if it can still burn, and sleeps afterwards.
fn rebuild_furnace_due_queue() {
    let now = FURNACE_TICK_SEQ.load(Ordering::Relaxed);
    let keys: Vec<DimBlockPos> = furnaces()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .copied()
        .collect();
    let mut q = furnace_due_queue().lock().unwrap_or_else(|e| e.into_inner());
    q.clear();
    for key in keys {
        q.wake(key, now);
    }
}

/// Same for brewing stands.
static BREWING_SLEEP: OnceLock<Mutex<HashMap<DimBlockPos, u128>>> = OnceLock::new();
fn brewing_sleep() -> &'static Mutex<HashMap<DimBlockPos, u128>> {
    BREWING_SLEEP.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Visit accounting the `xpoll_be_sleep_*` tests read: full passes versus
/// fingerprint-proven skips.
static BREWING_SLEEP_SKIPS: AtomicU64 = AtomicU64::new(0);
static BREWING_ACTIVE_STEPS: AtomicU64 = AtomicU64::new(0);

/// The watched-state fingerprint for one brewing-stand visit: the five menu
/// slot stacks (bottle slots carry their potion in components), the persisted
/// `Fuel`/`BrewTime` timers and `active_ingredient`, plus the content
/// revision. The derived `bottles`/`ingredient`/`blaze_powder` fields are
/// re-derived from the slots inside `LiveBrewingStand::tick`, so the slots
/// cover them.
fn brewing_epoch(entry: &LiveBrewingStand, rev: u64) -> u128 {
    let mut acc = mixv(rev as u128, 0xD6E8_FEB8_6659_FD93);
    for (slot, stack) in entry.menu.slots.iter().enumerate() {
        acc = mix_stack_entry(acc, slot as i16, stack);
    }
    acc = mixv(acc, u64::from(entry.stand.fuel));
    acc = mixv(acc, u64::from(entry.stand.brew_time));
    mixv(
        acc,
        entry
            .stand
            .active_ingredient
            .map_or(u64::MAX, |i| i as u64),
    )
}

fn hopper_tick() {
    // Only the hoppers that come due are visited, so a tick costs the hoppers
    // that need work rather than every hopper that exists. Every exit re-queues
    // except removal from the registry.
    let due = {
        let mut q = hopper_queue().lock().unwrap_or_else(|e| e.into_inner());
        q.due()
    };
    if due.is_empty() {
        return;
    }
    let region = region_guard();
    for (hx, hy, hz) in due {
        if !region.is_warm(hx, hz) || !sched_ready(hx, hy, hz) {
            requeue_hopper(hx, hy, hz);
            continue;
        }
        let st = block_at(hx, hy, hz);
        let Some((facing, enabled)) = hopper_state_facing(st) else {
            hoppers().lock().unwrap_or_else(|e| e.into_inner()).remove(&(hx, hy, hz));
            hopper_sleep().lock().unwrap_or_else(|e| e.into_inner()).remove(&(hx, hy, hz));
            continue;
        };
        if !enabled {
            sched_result(hx, hy, hz, false);
            requeue_hopper(hx, hy, hz);
            continue;
        } // powered(disabled) hopper holds items
        let (dx, dy, dz) = facing.offset();
        let dst = (hx + dx, hy + dy, hz + dz);
        // Idle-sleep probe: only a hopper whose last scan moved nothing can
        // skip, and only when the watched-state fingerprint is unchanged. The
        // probe runs only for cached-idle hoppers, so a working hopper pays
        // nothing extra.
        let idle_epoch = {
            let sl = hopper_sleep().lock().unwrap_or_else(|e| e.into_inner());
            sl.get(&(hx, hy, hz))
                .and_then(|s| s.idle.then_some(s.epoch))
        };
        let epoch = idle_epoch.map(|_| hopper_probe_epoch(hx, hy, hz, dst));
        if epoch.is_some() && epoch == idle_epoch {
            HOPPER_SCAN_SKIPS.fetch_add(1, Ordering::Relaxed);
            sched_result(hx, hy, hz, false);
            requeue_hopper(hx, hy, hz);
            continue;
        }
        HOPPER_SCAN_RUNS.fetch_add(1, Ordering::Relaxed);
        unpack_block_loot_if_needed(ow_block((hx, hy, hz)));
        // Vanilla 1.21.1 `HopperBlockEntity.tryMoveItems` is
        // `flag = ejectItems(...) | suckInItems(...)` — an *eager* bitwise OR, so
        // the pull runs even when the push already moved an item and even when
        // no container sits in front at all. `SleepingHopperEngine`'s
        // `attempt_transfers` encodes the same shape: push first, then the
        // container half of `suckInItems`, then the item-entity half.
        let (_, mut did) = hopper_transfer_dim(DimId::Overworld, (hx, hy, hz), st)
            .unwrap_or((dst, false));
        did |= hopper_suck(hx, hy, hz);
        {
            let mut sl = hopper_sleep().lock().unwrap_or_else(|e| e.into_inner());
            if sl.len() > HOPPER_SLEEP_MAX {
                let reg = hoppers().lock().unwrap_or_else(|e| e.into_inner());
                sl.retain(|k, _| reg.contains(k));
            }
            sl.insert(
                (hx, hy, hz),
                HopperSleep {
                    // An idle hopper stores the fingerprint it just proved
                    // empty-handed against; an active one stores none, so a
                    // working hopper never pays for a probe it cannot use.
                    epoch: if did {
                        0
                    } else {
                        epoch.unwrap_or_else(|| hopper_probe_epoch(hx, hy, hz, dst))
                    },
                    idle: !did,
                },
            );
        }
        hopper_sched_result(DimId::Overworld, (hx, hy, hz), did);
        requeue_hopper(hx, hy, hz);
    }
}
/// The container slot count a hopper transfer may see at `st`: vanilla reads
/// any `Container` — chests, barrels and shulker boxes are 27 wide, a hopper is
/// `HOPPER_INVENTORY_SIZE` wide — so hopper-to-hopper chains transfer through
/// the same path as a hopper pushing into a chest.
const HOPPER_SLOTS: i16 = HOPPER_INVENTORY_SIZE as i16;
fn hopper_container_slots(dim: DimId, pos: BlockPos) -> Option<i16> {
    let state = block_at_dim(dim, pos.0, pos.1, pos.2);
    let Some(reg) = breg() else {
        // Assetless unit tests keep their legacy fixture states; production
        // classification below always resolves the registered block name.
        #[cfg(test)]
        {
            if block_family_range("hopper")
                .is_some_and(|(first, last)| (first..=last).contains(&state))
            {
                return Some(HOPPER_SLOTS);
            }
            return container_kind(state).map(|_| 27);
        }
        #[cfg(not(test))]
        return None;
    };
    let name = reg.name_of(state);
    if name == "minecraft:hopper" {
        Some(HOPPER_SLOTS)
    } else if matches!(name, "minecraft:chest" | "minecraft:trapped_chest" | "minecraft:barrel")
        || name.ends_with("_shulker_box")
    {
        Some(27)
    } else {
        None
    }
}

fn hopper_state_facing(state: u32) -> Option<(HopperFacing, bool)> {
    let Some(reg) = breg() else {
        // Keep the pre-registry unit fixtures compatible without baking state
        // IDs into the production hopper path.
        #[cfg(test)]
        {
            let (first, last) = block_family_range("hopper")?;
            if !(first..=last).contains(&state) {
                return None;
            }
            let encoded = state - first;
            let facing = match encoded % 5 {
                0 => HopperFacing::Down,
                1 => HopperFacing::North,
                2 => HopperFacing::South,
                3 => HopperFacing::West,
                _ => HopperFacing::East,
            };
            return Some((facing, encoded < 5));
        }
        #[cfg(not(test))]
        return None;
    };
    if reg.name_of(state) != "minecraft:hopper" {
        return None;
    }
    let facing = match reg.get_prop(state, "facing").as_deref()? {
        "down" => HopperFacing::Down,
        "north" => HopperFacing::North,
        "south" => HopperFacing::South,
        "west" => HopperFacing::West,
        "east" => HopperFacing::East,
        _ => return None,
    };
    let enabled = reg.get_prop(state, "enabled").as_deref() != Some("false");
    Some((facing, enabled))
}

fn hopper_furnace_pos(dim: DimId, pos: BlockPos) -> Option<DimBlockPos> {
    let name = breg()?.name_of(block_at_dim(dim, pos.0, pos.1, pos.2));
    matches!(
        name,
        "minecraft:furnace" | "minecraft:smoker" | "minecraft:blast_furnace"
    )
    .then_some((dim, pos.0, pos.1, pos.2))
}

fn hopper_transfer_dim(dim: DimId, pos: BlockPos, state: u32) -> Option<(BlockPos, bool)> {
    let (facing, enabled) = hopper_state_facing(state)?;
    let (dx, dy, dz) = facing.offset();
    let dst = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
    if !enabled {
        return Some((dst, false));
    }
    let mut did = hopper_push_dim(dim, pos.0, pos.1, pos.2, dst);
    did |= hopper_pull_dim(dim, pos.0, pos.1, pos.2);
    // The Overworld `hopper_tick` runs the item-entity half of
    // `suckInItems` itself after this returns (its sleep probe fingerprints
    // that state), so `hopper_suck` stays a second call there.
    // `dim_hopper_tick` has no second call site — non-Overworld hoppers
    // take the same push-pull-suck order here instead.
    if dim != DimId::Overworld {
        did |= hopper_suck_in_dim(dim, pos.0, pos.1, pos.2);
    }
    Some((dst, did))
}

/// Record the vanilla transfer delay in the shared dimension-keyed scheduler
/// backoff map. Event wakeups must not bypass a successful transfer's cooldown.
fn hopper_sched_result(dim: DimId, pos: BlockPos, did_work: bool) {
    sched_result_dim(dim, pos.0, pos.1, pos.2, did_work);
    if did_work {
        let due = TICK_IDX.load(Ordering::Relaxed) + server_ticks(HOPPER_TRANSFER_COOLDOWN as u64);
        backoff()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((dim, pos.0, pos.1, pos.2), (due, 0));
    }
}

fn hopper_furnace_sync(pos: DimBlockPos) {
    let slots = furnace_slot_snapshot_dim(pos.0, (pos.1, pos.2, pos.3));
    for client in client_snapshot() {
        if *client.open_furnace.lock().unwrap_or_else(|e| e.into_inner()) == Some(pos) {
            let inv = client.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
            furnace_push_items_snap(&client, &inv, &slots);
        }
    }
}

/// Sided `AbstractFurnaceBlockEntity` insertion used by hopper ejection.
/// `face` is the face of the furnace the hopper targets: down is its top,
/// horizontal faces are its sides. Exactly one item is moved per activation.
fn hopper_push_furnace(pos: DimBlockPos, face: (i32, i32, i32), stack: &Stack) -> bool {
    let slot = if face == (0, 1, 0) {
        0
    } else if face.1 == 0 {
        1
    } else {
        return false;
    };
    let (item, _, comps) = stack;
    let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
    let Some(furnace) = fs.get_mut(&pos) else { return false };
    let fuel_slot_holds_bucket = furnace.fuel.0 == name_to_item("bucket").unwrap_or(-2);
    let target = if slot == 0 { &mut furnace.input } else { &mut furnace.fuel };
    let accepted = slot == 0
        || fuel_ticks(*item) > 0
        || (*item == name_to_item("bucket").unwrap_or(-1) && fuel_slot_holds_bucket);
    if !accepted || (target.1 > 0 && (target.0 != *item || target.2 != *comps))
        || target.1 >= item_stack_max(*item, comps) as u8
    {
        return false;
    }
    if target.1 == 0 {
        *target = (*item, 1, comps.clone());
    } else {
        target.1 += 1;
    }
    drop(fs);
    wake_furnace_at(pos.0, pos.1, pos.2, pos.3);
    hopper_furnace_sync(pos);
    true
}

/// Inspect the only furnace slots exposed to a hopper below it without
/// consuming an item. `hopper_pull_dim` uses this to check hopper capacity
/// before mutating the furnace inventory.
fn hopper_furnace_peek(pos: DimBlockPos) -> Option<(i64, Comps)> {
    let fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
    let furnace = fs.get(&pos)?;
    if furnace.output.1 > 0 {
        Some((furnace.output.0, furnace.output.2.clone()))
    } else if furnace.fuel.1 > 0 && furnace.fuel.0 == name_to_item("bucket").unwrap_or(-1) {
        Some((furnace.fuel.0, furnace.fuel.2.clone()))
    } else {
        None
    }
}

/// Sided `AbstractFurnaceBlockEntity` extraction for a hopper below the
/// furnace. The output is always extractable; the fuel slot exposes only an
/// empty bucket, which is the lava bucket return path.
fn hopper_pull_furnace(pos: DimBlockPos) -> Option<(i64, Comps)> {
    let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
    let furnace = fs.get_mut(&pos)?;
    let slot = if furnace.output.1 > 0 {
        &mut furnace.output
    } else if furnace.fuel.1 > 0 && furnace.fuel.0 == name_to_item("bucket").unwrap_or(-1) {
        &mut furnace.fuel
    } else {
        return None;
    };
    let result = (slot.0, slot.2.clone());
    slot.1 -= 1;
    if slot.1 == 0 {
        *slot = (0, 0, Vec::new());
    }
    drop(fs);
    wake_furnace_at(pos.0, pos.1, pos.2, pos.3);
    hopper_furnace_sync(pos);
    Some(result)
}

// ---------------------------------------------------------------------------
// XMOD1-HOPPER: vanilla hopper <-> BlockCapabilityBus bridge.
// A hopper facing a block no vanilla container or furnace claims falls
// through to these two entry points; under `mod-suite` they speak the same
// `live_cap_insert_item`/`live_cap_extract_item` contract every mod endpoint
// on the shared bus answers, so hoppers feed Create belts/depots/presses,
// Farmer's Delight cooking pots and any profile-declared mod machine — and
// pull their outputs back out. Without the feature the entry points are the
// old early-exit `false` and nothing on the vanilla path changes.
// ---------------------------------------------------------------------------

/// Marker framing `write_comps` output inside a bus `raw_components` blob.
/// A blob the hopper pushed itself decodes back to the exact `Comps`; one a
/// foreign mod wrote keeps its opaque wrapper so a re-push emits it
/// byte-identical.
#[cfg(feature = "mod-suite")]
const HOPPER_CAP_COMPS_TAG: &[u8; 4] = b"MCP1";
/// Synthetic component type holding a foreign endpoint's opaque
/// `raw_components` verbatim. No real 1.21.1 component id can collide with
/// it, and the enforced single-entry shape makes the wrapper unforgeable.
#[cfg(feature = "mod-suite")]
const HOPPER_CAP_FOREIGN_COMP: i64 = i64::MAX;

/// `Comps` -> the `raw_components` blob a capability endpoint carries: the
/// foreign-blob wrapper unwraps verbatim, empty stays empty (so the Create
/// duct fast path still applies), anything else encodes behind the tag.
#[cfg(feature = "mod-suite")]
fn hopper_cap_comps_encode(comps: &Comps) -> Vec<u8> {
    if comps.len() == 1 && comps[0].0 == HOPPER_CAP_FOREIGN_COMP {
        return comps[0].1.clone();
    }
    if comps.is_empty() {
        return Vec::new();
    }
    let mut raw = Vec::with_capacity(comps.len().saturating_mul(8).saturating_add(4));
    raw.extend_from_slice(HOPPER_CAP_COMPS_TAG);
    write_comps(&mut raw, comps);
    raw
}

/// `raw_components` -> `Comps`: only a blob that re-encodes byte-identical
/// is accepted as our own tagged encoding — everything else becomes the
/// opaque foreign wrapper so round-trips through the hopper never rewrite a
/// mod's bytes.
#[cfg(feature = "mod-suite")]
fn hopper_cap_comps_decode(raw: &[u8]) -> Comps {
    if let Some(body) = raw.strip_prefix(HOPPER_CAP_COMPS_TAG) {
        let mut p = 0;
        let comps = read_comps(body, &mut p);
        if hopper_cap_comps_encode(&comps) == raw {
            return comps;
        }
    }
    if raw.is_empty() {
        Vec::new()
    } else {
        vec![(HOPPER_CAP_FOREIGN_COMP, raw.to_vec())]
    }
}

/// `ejectItems` fallback for a destination no vanilla container or furnace
/// claims: under `mod-suite` the cell may host a capability-bus item
/// endpoint. Vanilla scans the hopper's slots in order and ejects one item
/// from the first the destination accepts — the same candidate order the
/// container and furnace paths use — so a cell with no endpoint answers `0`
/// for every slot and the whole probe costs the bus lookup alone.
#[cfg(feature = "mod-suite")]
fn hopper_push_mod_target(dim: DimId, hx: i32, hy: i32, hz: i32, dst: BlockPos) -> bool {
    let source_pos = ContainerKey::Block((dim, hx, hy, hz));
    let mut candidates: Vec<(i16, Stack)> = chests()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&source_pos)
        .into_iter()
        .flat_map(|slots| slots.iter())
        .filter(|(_, stack)| stack.1 > 0)
        .map(|(slot, stack)| (*slot, stack.clone()))
        .collect();
    candidates.sort_by_key(|(slot, _)| *slot);
    // The face the items enter through, read off the destination block: a
    // hopper above a depot pushes through its Up face, one beside a press
    // through its side.
    let face = live_cap_face_of((hx - dst.0, hy - dst.1, hz - dst.2));
    for (source_slot, (item, _, comps)) in candidates {
        if live_cap_insert_item(
            dim,
            dst.0,
            dst.1,
            dst.2,
            Some(face),
            &live_cap_item_name(item),
            1,
            &hopper_cap_comps_encode(&comps),
            false,
        ) == 0
        {
            continue;
        }
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        let mut remove_source = false;
        if let Some(source) = all
            .get_mut(&source_pos)
            .and_then(|slots| slots.get_mut(&source_slot))
        {
            // Re-check the slot identity before decrementing: a concurrent
            // GUI move must never turn the accepted push into the loss of
            // an unrelated stack.
            if source.0 == item && source.2 == comps && source.1 > 0 {
                source.1 -= 1;
                remove_source = source.1 == 0;
            }
        }
        if remove_source {
            if let Some(slots) = all.get_mut(&source_pos) {
                slots.remove(&source_slot);
            }
        }
        return true;
    }
    false
}

/// Without `mod-suite` no capability bus exists, so a non-container
/// destination still swallows nothing — the vanilla early exit verbatim.
#[cfg(not(feature = "mod-suite"))]
fn hopper_push_mod_target(_dim: DimId, _hx: i32, _hy: i32, _hz: i32, _dst: BlockPos) -> bool {
    false
}

/// Whether `pos` hosts anything a hopper could move items through: a bus
/// item endpoint, or a Create cell whose duct fast path bypasses the bus.
/// Used by the sleep probe — a hopper watching a mod block must never
/// trust an unchanged fingerprint, because the shared bus keeps no content
/// revision the probe could fold in and its mutations are invisible to
/// every watcher above.
#[cfg(feature = "mod-suite")]
fn hopper_cap_endpoint_present(dim: DimId, pos: BlockPos) -> bool {
    let coord = live_cap_coord(dim, pos.0, pos.1, pos.2);
    if live_cap_bus()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .item_endpoint(&coord)
        .is_some()
    {
        return true;
    }
    let fpos = FactoryPos::new(dim.raw() as i32, pos.0, pos.1, pos.2);
    live_cap_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .create_cells
        .contains_key(&fpos)
}

/// Every call yields a fresh value: mixing one into a probe epoch makes it
/// unmatchable, which is how a hopper facing a capability-bus block is
/// kept re-scanning instead of sleeping through a mod machine's output.
#[cfg(feature = "mod-suite")]
static HOPPER_CAP_PROBE_NONCE: AtomicU64 = AtomicU64::new(0);

/// `suckInItems` container fallback for a source block no vanilla container
/// or furnace claims: under `mod-suite` it may expose a capability-bus item
/// endpoint the hopper pulls finished items out of. The probe is
/// simulate-then-commit — a full hopper, or a stack the internal item table
/// cannot name, leaves the endpoint untouched so nothing duplicates or
/// vanishes.
#[cfg(feature = "mod-suite")]
fn hopper_pull_mod_source(dim: DimId, hx: i32, hy: i32, hz: i32) -> bool {
    let src = (hx, hy + 1, hz);
    // The face of the SOURCE block the hopper pulls through: the hopper
    // sits below it, so extraction goes through its Down face.
    let face = FaceDirection::Down;
    let Some(probe) = live_cap_extract_item(dim, src.0, src.1, src.2, Some(face), 1, true)
    else {
        return false;
    };
    let Some(probe_item) = name_to_item(&probe.item_id) else {
        return false;
    };
    let probe_comps = hopper_cap_comps_decode(&probe.raw_components);
    let own_pos = ContainerKey::Block((dim, hx, hy, hz));
    // Capacity check mirrors the container path exactly: merge onto a
    // partial matching stack first, else the first empty of the five slots.
    let fits = {
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        let own = all.entry(own_pos).or_default();
        let max = item_stack_max(probe_item, &probe_comps) as u8;
        own.iter()
            .filter(|(slot, stack)| {
                **slot < HOPPER_SLOTS
                    && stack.0 == probe_item
                    && stack.2 == probe_comps
                    && stack.1 < max
            })
            .map(|(slot, _)| *slot)
            .min()
            .or_else(|| (0..HOPPER_SLOTS).find(|slot| !own.contains_key(slot)))
            .is_some()
    };
    if !fits {
        return false;
    }
    let Some(stack) = live_cap_extract_item(dim, src.0, src.1, src.2, Some(face), 1, false)
    else {
        return false;
    };
    // The committed stack is what actually lands — re-resolve it in case
    // the endpoint's state raced the probe. Whatever cannot land is pushed
    // straight back onto the bus so nothing is ever dropped or duplicated.
    let mut moved = 0u32;
    if let Some(item) = name_to_item(&stack.item_id) {
        let comps = hopper_cap_comps_decode(&stack.raw_components);
        let max = item_stack_max(item, &comps) as u8;
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        let own = all.entry(own_pos).or_default();
        let mut remaining = stack.count.min(u8::MAX as u32) as u8;
        while remaining > 0 {
            let slot = own
                .iter()
                .filter(|(s, v)| {
                    **s < HOPPER_SLOTS && v.0 == item && v.2 == comps && v.1 < max
                })
                .map(|(s, _)| *s)
                .min()
                .or_else(|| (0..HOPPER_SLOTS).find(|s| !own.contains_key(s)));
            let Some(slot) = slot else { break };
            let cur = own.get(&slot).map(|v| v.1).unwrap_or(0);
            let take = remaining.min(max - cur);
            own.insert(slot, (item, cur + take, comps.clone()));
            remaining -= take;
        }
        moved = u32::from(stack.count.min(u8::MAX as u32) as u8 - remaining);
    }
    if moved < stack.count {
        live_cap_insert_item(
            dim,
            src.0,
            src.1,
            src.2,
            Some(face),
            &stack.item_id,
            stack.count - moved,
            &stack.raw_components,
            false,
        );
    }
    moved > 0
}

/// Without `mod-suite` no capability bus exists, so a non-container source
/// still yields nothing — the vanilla early exit verbatim.
#[cfg(not(feature = "mod-suite"))]
fn hopper_pull_mod_source(_dim: DimId, _hx: i32, _hy: i32, _hz: i32) -> bool {
    false
}

/// Vanilla `ejectItems`: one item from the hopper's first compatible slot into
/// the facing container. The dimension is carried through every world and
/// block-entity lookup, including custom registered dimensions.
fn hopper_push_dim(dim: DimId, hx: i32, hy: i32, hz: i32, dst: BlockPos) -> bool {
    let source_pos = ContainerKey::Block((dim, hx, hy, hz));
    if let Some(furnace_pos) = hopper_furnace_pos(dim, dst) {
        let mut candidates: Vec<(i16, Stack)> = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&source_pos)
            .into_iter()
            .flat_map(|slots| slots.iter())
            .filter(|(_, stack)| stack.1 > 0)
            .map(|(slot, stack)| (*slot, stack.clone()))
            .collect();
        candidates.sort_by_key(|(slot, _)| *slot);
        for (source_slot, stack) in candidates {
            if !hopper_push_furnace(
                furnace_pos,
                (hx - dst.0, hy - dst.1, hz - dst.2),
                &stack,
            ) {
                continue;
            }
            let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slots) = all.get_mut(&source_pos) {
                if let Some(source) = slots.get_mut(&source_slot) {
                    source.1 -= 1;
                    if source.1 == 0 {
                        slots.remove(&source_slot);
                    }
                }
            }
            return true;
        }
        return false;
    }

    let Some(dst_slots) = hopper_container_slots(dim, dst) else {
        return hopper_push_mod_target(dim, hx, hy, hz, dst);
    };
    unpack_block_loot_if_needed((dim, dst.0, dst.1, dst.2));
    let partner = chest_partner_dim(dim, dst.0, dst.1, dst.2);
    if let Some(partner) = partner {
        unpack_block_loot_if_needed((dim, partner.0, partner.1, partner.2));
    }
    let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
    let own = all.get(&source_pos).and_then(|slots| {
        slots.iter()
            .filter(|(_, stack)| stack.1 > 0)
            .map(|(slot, stack)| (*slot, stack.clone()))
            .min_by_key(|(slot, _)| *slot)
    });
    let Some((source_slot, (item, _, comps))) = own else {
        return false;
    };
    let stack_max = item_stack_max(item, &comps) as u8;
    let mut destinations = vec![(dst, dst_slots)];
    if let Some(partner) = partner {
        destinations.push((partner, 27));
    }
    for (pos, limit) in destinations {
        let target_pos = ContainerKey::Block((dim, pos.0, pos.1, pos.2));
        let target = all.entry(target_pos).or_default();
        let slot = target
            .iter()
            .filter(|(slot, stack)| {
                **slot < limit && stack.0 == item && stack.2 == comps && stack.1 < stack_max
            })
            .map(|(slot, _)| *slot)
            .min()
            .or_else(|| (0..limit).find(|slot| !target.contains_key(slot)));
        let Some(slot) = slot else { continue };
        let count = target.get(&slot).map(|stack| stack.1).unwrap_or(0);
        target.insert(slot, (item, count + 1, comps.clone()));
        let mut remove_source = false;
        if let Some(source) = all
            .get_mut(&source_pos)
            .and_then(|slots| slots.get_mut(&source_slot))
        {
            source.1 -= 1;
            remove_source = source.1 == 0;
        }
        if remove_source {
            all.get_mut(&source_pos).map(|slots| slots.remove(&source_slot));
        }
        return true;
    }
    false
}

/// Vanilla `suckInItems` container half: one item from the first extractable
/// slot above the hopper, dimension-keyed through the same generic path.
fn hopper_pull_dim(dim: DimId, hx: i32, hy: i32, hz: i32) -> bool {
    let src = (hx, hy + 1, hz);
    if let Some(furnace_pos) = hopper_furnace_pos(dim, src) {
        let Some((item, comps)) = hopper_furnace_peek(furnace_pos) else {
            return false;
        };
        let own_pos = ContainerKey::Block((dim, hx, hy, hz));
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        let own = all.entry(own_pos).or_default();
        let max = item_stack_max(item, &comps) as u8;
        let slot = own
            .iter()
            .filter(|(slot, stack)| {
                **slot < HOPPER_SLOTS && stack.0 == item && stack.2 == comps && stack.1 < max
            })
            .map(|(slot, _)| *slot)
            .min()
            .or_else(|| (0..HOPPER_SLOTS).find(|slot| !own.contains_key(slot)));
        let Some(slot) = slot else { return false };
        let count = own.get(&slot).map(|stack| stack.1).unwrap_or(0);
        own.insert(slot, (item, count + 1, comps));
        drop(all);
        let removed = hopper_pull_furnace(furnace_pos);
        debug_assert!(removed.is_some());
        return removed.is_some();
    }

    let Some(src_slots) = hopper_container_slots(dim, src) else {
        return hopper_pull_mod_source(dim, hx, hy, hz);
    };
    unpack_block_loot_if_needed((dim, src.0, src.1, src.2));
    let partner = chest_partner_dim(dim, src.0, src.1, src.2);
    if let Some(partner) = partner {
        unpack_block_loot_if_needed((dim, partner.0, partner.1, partner.2));
    }
    let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
    let mut sources = vec![(src, src_slots)];
    if let Some(partner) = partner {
        sources.push((partner, 27));
    }
    for (src_pos, limit) in sources {
        let key = ContainerKey::Block((dim, src_pos.0, src_pos.1, src_pos.2));
        let stacks: std::collections::BTreeMap<i16, Stack> = all
            .get(&key)
            .map(|slots| {
                slots.iter()
                    .filter(|(slot, stack)| **slot < limit && stack.1 > 0)
                    .map(|(slot, stack)| (*slot, stack.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (source_slot, (item, count, comps)) in stacks {
            let max = item_stack_max(item, &comps) as u8;
            let own_key = ContainerKey::Block((dim, hx, hy, hz));
            let own = all.entry(own_key).or_default();
            let slot = own
                .iter()
                .filter(|(slot, stack)| {
                    **slot < HOPPER_SLOTS && stack.0 == item && stack.2 == comps && stack.1 < max
                })
                .map(|(slot, _)| *slot)
                .min()
                .or_else(|| (0..HOPPER_SLOTS).find(|slot| !own.contains_key(slot)));
            let Some(slot) = slot else { continue };
            let current = own.get(&slot).map(|stack| stack.1).unwrap_or(0);
            own.insert(slot, (item, current + 1, comps.clone()));
            let source_key = ContainerKey::Block((dim, src_pos.0, src_pos.1, src_pos.2));
            let mut remove_source = false;
            if let Some(source) = all
                .get_mut(&source_key)
                .and_then(|slots| slots.get_mut(&source_slot))
            {
                if count <= 1 {
                    remove_source = true;
                } else {
                    source.1 -= 1;
                }
            }
            if remove_source {
                all.get_mut(&source_key).map(|slots| slots.remove(&source_slot));
            }
            return true;
        }
    }
    false
}

/// Vanilla `suckInItems`' entity half: the first dropped item entity resting in
/// the cell above the hopper loses as much of its stack as the hopper's own
/// five slots can hold (`Hopper.addItem` semantics — a partial take leaves the
/// remainder on the entity). One entity per activation, like the engine's
/// `attempt_suck`.
fn hopper_suck(hx: i32, hy: i32, hz: i32) -> bool {
    hopper_suck_in_dim(DimId::Overworld, hx, hy, hz)
}
/// `hopper_suck` in `dim`: only drops spawned in `dim` are candidates — a
/// Nether drop resting at the same coordinates as the Overworld suction cell
/// is a different entity entirely.
fn hopper_suck_in_dim(dim: DimId, hx: i32, hy: i32, hz: i32) -> bool {
    // O(1) idle exit: the spatial index proves no drop rests in the suction
    // cell, so the vanilla scan below — a full `drops()` walk — cannot find a
    // candidate. When the index says otherwise the original scan runs
    // verbatim, so a stale-extra entry costs one scan and never a wrong take.
    if !suction_nonempty_in(dim, hx, hy, hz) {
        return false;
    }
    let mut cell = HOPPER_CELL_SCRATCH.with(scratch_take);
    {
        let dl = drops().lock().unwrap_or_else(|e| e.into_inner());
        let dims = drop_dims().lock().unwrap_or_else(|e| e.into_inner());
        cell.extend(
            dl.iter()
                .filter(|d| {
                    dims.get(&d.eid).copied().unwrap_or(DimId::Overworld) == dim
                        && (d.x - (hx as f64 + 0.5)).abs() < 0.7
                        && d.y > hy as f64 + 0.4
                        && d.y < hy as f64 + 1.6
                        && (d.z - (hz as f64 + 0.5)).abs() < 0.7
                })
                .map(|d| (d.eid, d.item, d.cnt, d.comps.clone())),
        );
    }
    cell.sort_by_key(|(eid, ..)| *eid);
    let mut sucked = false;
    for (deid, item, cnt, dcp) in cell.drain(..) {
        let smax = item_stack_max(item, &dcp) as u8;
        let mut remaining = cnt;
        let moved = {
            let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
            let own = all.entry(ContainerKey::at(dim, (hx, hy, hz))).or_default();
            let mut moved = 0u8;
            while remaining > 0 {
                let slot = own
                    .iter()
                    .filter(|(s, v)| {
                        **s < HOPPER_SLOTS && v.0 == item && v.2 == dcp && v.1 < smax
                    })
                    .map(|(s, _)| *s)
                    .min()
                    .or_else(|| (0..HOPPER_SLOTS).find(|sl| !own.contains_key(sl)));
                let Some(slot) = slot else { break };
                let cur = own.get(&slot).map(|v| v.1).unwrap_or(0);
                let take = remaining.min(smax - cur);
                own.insert(slot, (item, cur + take, dcp.clone()));
                remaining -= take;
                moved += take;
            }
            moved
        };
        if moved == 0 {
            continue;
        }
        if remaining == 0 {
            drops().lock().unwrap_or_else(|e| e.into_inner()).retain(|d| d.eid != deid);
            let mut p = Vec::new();
            write_varint(&mut p, 1);
            write_varint(&mut p, deid as i64);
            broadcast_seen(deid, |_c| (0x42, p.clone()));
        } else if let Some(d) = drops().lock().unwrap_or_else(|e| e.into_inner()).iter_mut().find(|d| d.eid == deid)
        {
            d.cnt = remaining;
        }
        sucked = true;
        break;
    }
    HOPPER_CELL_SCRATCH.with(|s| scratch_return(s, cell));
    sucked
}
fn drop_tick() {
    // Split under `MEGU_TICK_PROFILE=1`. The bucketed merge landed without a
    // split, so `drops` could not say whether the residual is the pickup scan, the
    // cell build, the neighbourhood walk or the despawn pass.
    let __prof = tickprof::enabled();
    let mut __t = __prof.then(std::time::Instant::now);
    let region = region_guard();
    // A spawn burst (explosion coalesced stacks, a farm's loot stream)
    // collapses into its older neighbours before the pickup scan sees it.
    drop_merge_pending();
    // 1.21.1 Player.aiStep() touch box + !isSpectator() + ItemEntity.target match
    let mut taken: Vec<(i32, Arc<Client>)> = vec![];
    {
        let ds: Vec<DropPickupSnapshot> = drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|d| (d.eid, d.x, d.z, d.target, d.bounding_box()))
            .collect();
        let cls: Vec<Arc<Client>> = client_snapshot();
        let touches: Vec<(bool, bool, EntityAabb)> = cls
            .iter()
            .map(|c| (c.is_spectator(), *c.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0, c.touch_aabb()))
            .collect();
        for (d_eid, dx, dz, d_target, d_aabb) in ds.iter() {
            if !region.is_hot(*dx as i32, *dz as i32) {
                continue;
            }
            for (c, &(spectator, alive, player_aabb)) in cls.iter().zip(touches.iter()) {
                if Drop::can_be_picked_up_by_parts(
                    *d_target,
                    *d_aabb,
                    Some(c.uuid),
                    spectator,
                    alive,
                    player_aabb,
                ) {
                    taken.push((*d_eid, c.clone()));
                }
            }
        }
    }
    if let Some(t) = __t {
        tickprof::add("drops/pickup", t.elapsed());
    }
    __t = __prof.then(std::time::Instant::now);
    // xp orbs float to nearby players and grant xp
    {
        // A farm's orb burst collapses before the pickup scan — same-tick
        // merging instead of one entity per kill all the way to XP_CAP.
        xp_merge_pending();
        let mut collected: Vec<(i32, i32, i16)> = vec![];
        {
            let xs = xps().lock().unwrap_or_else(|e| e.into_inner());
            let cls: Vec<Arc<Client>> = client_snapshot();
            // Snapshot positions once instead of locking each player's pos for
            // every orb-player pair.
            let player_pos: Vec<(i32, f64, f64, f64)> = cls
                .iter()
                .map(|c| {
                    let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
                    (c.eid, p.0, p.1, p.2)
                })
                .collect();
            for x in xs.iter() {
                for &(eid, px, py, pz) in &player_pos {
                    if (px - x.x).powi(2) + (py - x.y).powi(2) + (pz - x.z).powi(2) < 2.56 {
                        collected.push((x.eid, eid, x.val));
                        break;
                    }
                }
            }
        }
        if !collected.is_empty() {
            // One audience snapshot and one retain pass for the whole batch:
            // the per-orb `retain` + `client_snapshot` pair this replaces was
            // O(orbs * pool + orbs * clients) on a 2,000-orb farm.
            let cls: Vec<Arc<Client>> = client_snapshot();
            let mut picked: HashSet<i32> = HashSet::with_capacity(collected.len());
            for (xeid, ceid, val) in collected {
                picked.insert(xeid);
                let Some(c) = cls.iter().find(|c| c.eid == ceid) else {
                    continue;
                };
                // mending: xp first repairs damaged mending gear (2 dur per xp)
                let mut val = val as i64;
                {
                    let mut inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
                    let cands: Vec<i16> = inv
                        .iter()
                        .filter(|(_, (it, _, cc))| {
                            *it > 0
                                && enchant_level(cc, 22) > 0
                                && comp_varint(cc, COMP_DAMAGE).unwrap_or(0) > 0
                        })
                        .map(|(&k, _)| k)
                        .collect();
                    for k in cands {
                        if val <= 0 {
                            break;
                        }
                        let (it, cnt, mut cc) = inv.remove(&k).unwrap();
                        let cur = comp_varint(&cc, COMP_DAMAGE).unwrap_or(0);
                        let heal = (val * 2).min(cur);
                        comp_set_varint(&mut cc, COMP_DAMAGE, cur - heal);
                        inv.insert(k, (it, cnt, cc));
                        val -= (heal + 1) / 2;
                    }
                }
                let _ = send_player_xp(c, |xp| add_player_xp_points(xp, val as i32));
                broadcast_seen_destroy(xeid);
            }
            xps()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|x| !picked.contains(&x.eid));
        }
    }
    if let Some(t) = __t {
        tickprof::add("drops/xp_pickup", t.elapsed());
    }
    __t = __prof.then(std::time::Instant::now);
    for (deid, c) in taken {
        let selected = *c.held.lock().unwrap_or_else(|e| e.into_inner());
        let creative = *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1;
        let spectator = c.is_spectator();
        let alive = *c.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0;
        let player_aabb = c.touch_aabb();
        let (accepted, original, changed, remaining) = {
            // Same inventory -> drops lock order as actor_tick. Read the live
            // entity, not a stack snapshot another collector may have consumed.
            let mut inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
            let mut ds = drops().lock().unwrap_or_else(|e| e.into_inner());
            let Some(d) = ds.iter_mut().find(|d| d.eid == deid) else {
                continue;
            };
            if !d.can_be_picked_up_by(Some(c.uuid), spectator, alive, player_aabb) {
                continue;
            }
            // The delay belongs to the take, not to the candidate scan: it is judged on
            // the live entity under the same locks the insertion uses, so a drop spawned
            // after the snapshot above, or merged into a fresher age, is still too young
            // to collect on this tick.
            if !d.pickup_delay_elapsed() {
                continue;
            }
            let mut source = (d.item, d.cnt, d.comps.clone());
            let original = source.1;
            let before = inv.clone();
            // QOL-DEATH (opt-in): a drop recorded as this player's own death
            // item first tries to return to the slot it was scattered from.
            // The flag is checked inside `death_slot_hint_for`; a disabled
            // feature or a foreign owner falls through to the vanilla add.
            let mut restored = false;
            if let Some(original_slot) = death_slot_hint_for(deid, &c.name) {
                let picked_cnt = source.1;
                source = match insert_with_death_slot_restore(&mut inv, source, original_slot) {
                    None => {
                        restored = true;
                        (0, 0, Vec::new())
                    }
                    Some(rem) => {
                        restored = rem.1 != picked_cnt;
                        rem
                    }
                };
            }
            let accepted = pickup_into_inventory(&mut inv, selected, creative, &mut source);
            if source.1 == original {
                continue;
            }
            d.cnt = source.1;
            let remaining = (d.cnt > 0).then(|| drop_meta_pkt(d));
            if d.cnt == 0 {
                ds.retain(|d| d.eid != deid);
                death_slot_hint_forget(deid);
            }
            let changed: Vec<_> = (5..=45)
                .filter_map(|slot| {
                    inv.get(&slot)
                        .filter(|st| before.get(&slot) != Some(st))
                        .map(|st| (slot, st.clone()))
                })
                .collect();
            if changed.iter().any(|(slot, _)| (5..=8).contains(slot)) {
                *c.armor_pts.lock().unwrap_or_else(|e| e.into_inner()) = armor_pts_of(&inv);
            }
            (accepted || restored, original, changed, remaining)
        };
        for (slot, st) in changed {
            let mut p = vec![0u8];
            write_varint(&mut p, i64::from(inv_state_next(&c)));
            p.extend_from_slice(&slot.to_be_bytes());
            p.extend_from_slice(&slot_wire(&st));
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x15, p);
            }
        }
        // ItemEntity.playerTouch uses add's boolean, NOT the number inserted;
        // its take callback receives the original entity count (cjh.b_(cmx)).
        if accepted {
            broadcast_seen(deid, |_c| (0x6f, collect_pkt(deid, c.eid, original)));
        }
        if let Some(pkt) = remaining {
            broadcast_seen(deid, move |_c| (0x58, pkt.clone()));
        } else {
            broadcast_seen_destroy(deid);
        }
    }
    {
        // despawn drops after 300s (vanilla); a hard cap bounds the flood
        // inside any one despawn window — oldest lose first.
        let mut dp = drops().lock().unwrap_or_else(|e| e.into_inner());
        let life = server_ticks(DROP_LIFE_GT) as i32;
        for d in dp.iter_mut() {
            if region.is_hot(d.x as i32, d.z as i32) {
                d.age += 1;
            }
        }
        let mut exp = DROP_DESPAWN_SCRATCH.with(scratch_take);
        exp.extend(dp.iter().filter(|d| d.age >= life).map(|d| d.eid));
        // One retain pass for the whole expired set, then the per-eid
        // notifications in the same order they always went out.
        {
            let gone: HashSet<i32> = exp.iter().copied().collect();
            dp.retain(|d| !gone.contains(&d.eid));
        }
        for eid in &exp {
            death_slot_hint_forget(*eid);
            broadcast_seen_destroy(*eid);
        }
        DROP_DESPAWN_SCRATCH.with(|s| scratch_return(s, exp));
        // vanilla `ItemEntity.tick`: drops inside fire, soul fire or lava burn
        // away unless the stack is `fire_resistant` (netherite carries it).
        {
            let mut burned = DROP_DESPAWN_SCRATCH.with(scratch_take);
            burned.extend(
                dp.iter()
                    .filter(|d| {
                        !drop_fireproof(d.item, &d.comps)
                            && region.is_hot(d.x as i32, d.z as i32)
                            && breg().is_some_and(|reg| {
                                drop_burns_at(reg.name_of(block_at(
                                    d.x.floor() as i32,
                                    d.y.floor() as i32,
                                    d.z.floor() as i32,
                                )))
                            })
                    })
                    .map(|d| d.eid),
            );
            {
                let gone: HashSet<i32> = burned.iter().copied().collect();
                dp.retain(|d| !gone.contains(&d.eid));
            }
            for eid in &burned {
                death_slot_hint_forget(*eid);
                broadcast_seen_destroy(*eid);
            }
            DROP_DESPAWN_SCRATCH.with(|s| scratch_return(s, burned));
        }
        evict_oldest_front(&mut dp, DROP_CAP, |d| {
            death_slot_hint_forget(d.eid);
            broadcast_seen_destroy(d.eid);
        });
        if let Some(t) = __t {
            tickprof::add("drops/despawn", t.elapsed());
        }
        __t = __prof.then(std::time::Instant::now);
        let merged = merge_drops(&mut dp);
        __t = __prof.then(std::time::Instant::now);
        {
            let gone: HashSet<i32> = merged.iter().copied().collect();
            dp.retain(|d| !gone.contains(&d.eid));
        }
        for eid in &merged {
            death_slot_hint_forget(*eid);
            broadcast_seen_destroy(*eid);
        }
        if let Some(t) = __t {
            tickprof::add("drops/merge_destroy", t.elapsed());
        }
        shrink_if_sparse(&mut dp);
        // Retire eids that left the pool. The index only ever holds
        // non-Overworld spawns, so this stays free on an Overworld world.
        let mut dims = drop_dims().lock().unwrap_or_else(|e| e.into_inner());
        if !dims.is_empty() {
            let live: HashSet<i32> = dp.iter().map(|d| d.eid).collect();
            dims.retain(|eid, _| live.contains(eid));
        }
    }
}

struct Furnace {
    input: Stack,
    fuel: Stack,
    output: Stack,
    burn_left: i32,
    progress: i32,
    dirty: bool,
    xp: f32,
    recipe_cache: CachedLookup<(i64, u64), Option<(i64, u8)>>,
    fuel_cache: CachedLookup<i64, i32>,
}
static FURNACES: Registry<DimBlockPos, Furnace> = OnceLock::new();

fn furnaces() -> &'static LockedMap<DimBlockPos, Furnace> {
    FURNACES.get_or_init(|| Mutex::new(HashMap::new()))
}
// furnace block states classify by registry name (`minecraft:furnace`); the
// numeric range remains only as the no-registry fallback. One store for
// containers regardless of which interface touched them.
fn is_furnace_at(x: i32, y: i32, z: i32) -> bool {
    is_furnace_at_dim(DimId::Overworld, x, y, z)
}
fn is_furnace_at_dim(dim: DimId, x: i32, y: i32, z: i32) -> bool {
    furnace_named(block_at_dim(dim, x, y, z))
}
fn furnace_slots(pos: (i32, i32, i32)) -> HashMap<i16, Stack> {
    furnace_slots_dim(DimId::Overworld, pos)
}
/// `furnace_slots` in `dim`: the store is keyed `DimBlockPos`, so the lookup
/// reads the furnace that lives in `dim`, not whatever shares its coords
/// elsewhere.
fn furnace_slots_dim(dim: DimId, pos: (i32, i32, i32)) -> HashMap<i16, Stack> {
    let fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
    fs.get(&(dim, pos.0, pos.1, pos.2))
        .map(|f| {
            let mut m = HashMap::new();
            if f.input.1 > 0 {
                m.insert(0i16, f.input.clone());
            }
            if f.fuel.1 > 0 {
                m.insert(1i16, f.fuel.clone());
            }
            if f.output.1 > 0 {
                m.insert(2i16, f.output.clone());
            }
            m
        })
        .unwrap_or_default()
}
/// Bumped by the datapack loader. Recipes and ingredient tags are mutable, so
/// anything caching an answer derived from them must key on this.
static CONTENT_REVISION: AtomicU64 = AtomicU64::new(0);
fn content_revision() -> u64 {
    CONTENT_REVISION.load(Ordering::Acquire)
}

/// The smelting result, independent of which furnace is asking. Datapack recipes
/// and tags are mutable, so a caller that caches this must include
/// `content_revision()` in the key. The result never carries components, which is
/// why the cache can hold a plain pair rather than a whole stack.
fn smelt_out(item: i64) -> Option<(i64, u8)> {
    for (ig, o, c, _) in json_smelts().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if ingr_match(ig, item, 0) {
            return Some((*o, *c));
        }
    }
    smelt_result(item).map(|o| (o, 1))
}
fn smelt_result(item: i64) -> Option<i64> {
    for (a, b) in [
        ("beef", "cooked_beef"),
        ("porkchop", "cooked_porkchop"),
        ("chicken", "cooked_chicken"),
        ("mutton", "cooked_mutton"),
        ("sand", "glass"),
        ("cobblestone", "stone"),
        ("potato", "baked_potato"),
        ("oak_log", "charcoal"),
        ("raw_iron", "iron_ingot"),
        ("raw_copper", "copper_ingot"),
        ("raw_gold", "gold_ingot"),
        ("rabbit", "cooked_rabbit"),
        ("cod", "cooked_cod"),
    ] {
        if name_to_item(a) == Some(item) {
            return name_to_item(b);
        }
    }
    None
}
fn smelt_xp(item: i64) -> f32 {
    for (ig, _, _, xp) in json_smelts().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if ingr_match(ig, item, 0) {
            return *xp;
        }
    }
    for (n, x) in [
        ("raw_gold", 1.0f32),
        ("raw_iron", 0.7),
        ("raw_copper", 0.7),
        ("beef", 0.35),
        ("porkchop", 0.35),
        ("chicken", 0.35),
        ("mutton", 0.35),
        ("rabbit", 0.35),
        ("cod", 0.35),
        ("salmon", 0.35),
        ("potato", 0.35),
        ("kelp", 0.35),
        ("oak_log", 0.15),
    ] {
        if name_to_item(n) == Some(item) {
            return x;
        }
    }
    if smelt_result(item).is_some() {
        0.1
    } else {
        0.0
    }
}
// grant accumulated smelting xp: floor + fractional chance (vanilla), as orbs at (px,py,pz)
fn furnace_xp_pop(f: &mut Furnace, px: f64, py: f64, pz: f64) {
    if f.xp <= 0.0 {
        return;
    }
    let whole = f.xp.floor() as i16
        + if (world_rng::next_u64() % 1024) as f32 / 1024.0 < f.xp.fract() {
            1
        } else {
            0
        };
    f.xp = 0.0;
    if whole > 0 {
        xp_spawn(px, py + 1.0, pz, whole);
    }
}
/// Whether `stem` names a wood family for fuel purposes: `{ns}:{stem}_planks`
/// or `{ns}:{stem}_log` resolves to a real item — which covers every vanilla
/// wood and any modded wood that ships the usual companions — plus the one
/// mosaic wood that has no `_planks` form.
fn fuel_wooden_stem(ns: &str, stem: &str) -> bool {
    if stem == "bamboo_mosaic" {
        return true;
    }
    name_to_item(&format!("{ns}:{stem}_planks")).is_some()
        || name_to_item(&format!("{ns}:{stem}_log")).is_some()
}

/// Vanilla 1.21.1 furnace fuel values in game ticks, namespace-agnostic:
/// exact-path fuels match in any namespace, wood-family suffixes
/// (`*_log`/`*_planks`/`*_stairs`/…) require a wooden stem so stone and
/// iron variants stay cold, and modded woods/coals burn through the same
/// rules.
fn fuel_ticks(item: i64) -> i32 {
    let Some(name) = item_to_name(item) else {
        return 0;
    };
    let (ns, path) = name
        .rsplit_once(':')
        .map_or(("minecraft", name), |(ns, path)| (ns, path));
    let wooden = |stem: &str| fuel_wooden_stem(ns, stem);
    let suffix_stem = |suffix: &str| path.strip_suffix(suffix);
    let ticks: u64 = match path {
        "lava_bucket" => 20_000,
        "coal_block" => 16_000,
        "dried_kelp_block" => 4_000,
        "blaze_rod" => 2_400,
        "coal" | "charcoal" => 1_600,
        _ => {
            if path == "boat" || path.ends_with("_boat") || path.ends_with("_raft") {
                1_200
            } else if let Some(stem) = suffix_stem("_slab") {
                if wooden(stem) {
                    150
                } else {
                    0
                }
            } else if ["_log", "_wood", "_stem", "_hyphae", "_planks"]
                .iter()
                .any(|suffix| path.ends_with(suffix))
                || [
                    "_fence",
                    "_fence_gate",
                    "_stairs",
                    "_trapdoor",
                    "_door",
                    "_button",
                    "_pressure_plate",
                ]
                .iter()
                .any(|suffix| {
                    path.strip_suffix(suffix)
                        .is_some_and(|stem| wooden(stem))
                })
            {
                300
            } else if path.ends_with("_hanging_sign") || path.ends_with("_sign") {
                300
            } else if matches!(
                path,
                "crafting_table"
                    | "bookshelf"
                    | "chiseled_bookshelf"
                    | "lectern"
                    | "composter"
                    | "chest"
                    | "trapped_chest"
                    | "barrel"
                    | "jukebox"
                    | "note_block"
                    | "loom"
                    | "cartography_table"
                    | "fletching_table"
                    | "smithing_table"
                    | "daylight_detector"
                    | "bamboo_mosaic"
            ) || path == "banner"
                || path.ends_with("_banner")
            {
                300
            } else if matches!(path, "stick" | "bamboo" | "bowl")
                || path == "sapling"
                || path.ends_with("_sapling")
                || path.ends_with("_wool")
                || path.ends_with("_carpet")
                || path.starts_with("wooden_")
            {
                100
            } else if path == "scaffolding" {
                50
            } else {
                0
            }
        }
    };
    if ticks == 0 {
        0
    } else {
        server_ticks(ticks) as i32
    }
}
static CHESTS: Registry<ContainerKey, HashMap<i16, Stack>> = OnceLock::new();

pub(crate) fn fishing_category_weights(luck: i32, open_water: bool) -> (i32, i32, i32) {
    (
        (85 - luck).max(0),
        (10 - 2 * luck).max(0),
        if open_water { (5 + 2 * luck).max(0) } else { 0 },
    )
}

pub(crate) fn fishing_wait_ticks(lure: i32, rng_roll: u32) -> u32 {
    (100 + rng_roll.min(500))
        .saturating_sub((lure.max(0) as u32).saturating_mul(100))
        .max(20)
}

pub(crate) fn is_open_water_check(
    mut state_at: impl FnMut(i32, i32, i32) -> (bool, bool, bool),
    x: i32,
    y: i32,
    z: i32,
) -> bool {
    for dx in -2..=2 {
        for dz in -2..=2 {
            for dy in -2..=1 {
                let (water_source, air_or_lily, solid) = state_at(x + dx, y + dy, z + dz);
                if solid || (dy <= 0 && !water_source) || (dy > 0 && !air_or_lily) {
                    return false;
                }
            }
        }
    }
    true
}

pub(crate) fn fishing_enchant_levels(client: &Client) -> (i32, i32) {
    let selected = *client.held.lock().unwrap_or_else(|e| e.into_inner());
    let inventory = client.inv.lock().unwrap_or_else(|e| e.into_inner());
    let components = inventory
        .get(&(36 + selected))
        .map(|stack| &stack.2);
    components.map_or((0, 0), |components| {
        (
            enchant_level(components, 20).clamp(0, i32::MAX as i64) as i32,
            enchant_level(components, 21).clamp(0, i32::MAX as i64) as i32,
        )
    })
}

pub(crate) fn fishing_open_water(dim: DimId, x: i32, y: i32, z: i32) -> bool {
    let Some(registry) = breg() else { return false };
    let water = name_to_state("water").unwrap_or(u32::MAX);
    let lily = name_to_state("lily_pad").unwrap_or(u32::MAX);
    is_open_water_check(
        |bx, by, bz| {
            let state = block_at_dim(dim, bx, by, bz);
            (
                state == water,
                registry.is_air(state) || state == lily,
                registry.is_solid(state),
            )
        },
        x,
        y,
        z,
    )
}

fn roll_loot_container_slots(
    kind: crate::generation_payload::ContainerKind,
    loot_table: &str,
    loot_seed: Option<i64>,
) -> HashMap<i16, Stack> {
    use crate::generation_payload::ContainerKind;
    // 1.21.1 `RandomizableContainer.unpackLootTable` treats seed 0 as unseeded (`RandomSource.create()`).
    let seed = loot_seed
        .filter(|&s| s != 0)
        .unwrap_or_else(|| world_rng::next_u64() as i64);
    if loot_table == "minecraft:chests/abandoned_mineshaft" {
        roll_cart_loot(seed)
    } else {
        let pools = loot_tables().lock().unwrap_or_else(|e| e.into_inner()).get(loot_table).map(|p| {
            p.iter()
                .map(|pl| LootPool {
                    rolls: pl.rolls,
                    chance: pl.chance,
                    funcs: pl.funcs.clone(),
                    entries: pl
                        .entries
                        .iter()
                        .map(|e| LootEntry {
                            item: e.item,
                            min: e.min,
                            max: e.max,
                            funcs: e.funcs.clone(),
                        })
                        .collect(),
                })
                .collect::<Vec<_>>()
        });
        let mut out = HashMap::new();
        let items = if let Some(pools) = pools {
            roll_loot_seeded(&pools, seed)
        } else {
            crate::chest_loot::vanilla_loot_roll(loot_table, seed)
                .into_iter()
                .filter_map(|entry| {
                    let item = name_to_item(entry.item)?;
                    let mut components = Vec::new();
                    if let Some((name, level)) = entry.enchantment {
                        if let Some(id) = enchant_id(name) {
                            comp_set(
                                &mut components,
                                COMP_ENCHANTMENTS,
                                enchantments_data(&[(id, i64::from(level))], true),
                            );
                        }
                    }
                    Some((item, entry.count, components))
                })
                .collect()
        };
        if !items.is_empty() {
            // `LootTable.fill` fills `container.getContainerSize()`
            // slots, so the native menu size bounds the roll.
            let slot_limit = match kind {
                ContainerKind::Chest => 27i16,
                ContainerKind::Furnace => 3i16,
                                        ContainerKind::BlockContainer(size) => {
                                            i16::from(size)
                                        }
            };
            let mut slot_order: Vec<i16> = (0..slot_limit).collect();
            if kind != ContainerKind::Furnace {
                let mut shuffle_rng = crate::generation_math::Lcg::new(seed as u64);
                for i in (2..=slot_order.len()).rev() {
                    let j = shuffle_rng.next_bound(i as i32) as usize;
                    slot_order.swap(i - 1, j);
                }
            }
            for (idx, stack) in items.into_iter().take(slot_limit as usize).enumerate() {
                out.insert(slot_order[idx], stack);
            }
        }
        out
    }
}
fn unpack_block_loot_if_needed(pos: DimBlockPos) -> bool {
    with_save_guard(|| {
        // The world edit path calls this for every Overworld block-identity
        // write, which is exactly when vanilla destroys the block entity —
        // including a break followed by re-placing the same block type.
        // Dropping the entry here keeps a same-type rewrite from resurrecting
        // the old payload before the next discovery scan. Speculative callers
        // (hopper moves, window opens, dispenser fire) only ever name plain
        // container positions, and the block-id guard leaves matching entries
        // alone.
        {
            let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(&(id, _)) = store.entries.get(&pos) {
                let current = breg()
                    .and_then(|reg| reg.block_id_of(block_at_dim(pos.0, pos.1, pos.2, pos.3)));
                if current != Some(id) {
                    store.remove_entry(pos);
                }
            }
        }
        let pending = pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()).remove(&pos);
        let Some(entry) = pending else {
            return false;
        };
        let rolled = roll_loot_container_slots(entry.kind, &entry.loot_table, entry.loot_seed);
        let mut store = chests().lock().unwrap_or_else(|e| e.into_inner());
        let slots = store.entry(ContainerKey::Block(pos)).or_default();
        for (slot, stack) in rolled {
            slots.insert(slot, stack);
        }
        true
    })
}
fn window_has_pending_loot(window: (ContainerKey, Option<ContainerKey>)) -> bool {
    let map = pending_block_loot().lock().unwrap_or_else(|e| e.into_inner());
    [Some(window.0), window.1]
        .into_iter()
        .flatten()
        .any(|key| matches!(key, ContainerKey::Block(pos) if !is_ender_key((pos.1, pos.2, pos.3)) && map.contains_key(&pos)))
}
fn unpack_window_loot_if_needed(window: (ContainerKey, Option<ContainerKey>)) {
    for key in [Some(window.0), window.1].into_iter().flatten() {
        if let ContainerKey::Block(pos) = key {
            if !is_ender_key((pos.1, pos.2, pos.3)) {
                unpack_block_loot_if_needed(pos);
            }
        }
    }
}

// ---------- generic JSON loot tables ----------
//
// `crate::chest_loot` evaluates vanilla 1.21.1 loot-table JSON (nested
// alternatives/group/sequence, tag entries, the implemented conditions and
// functions). This is the container-fill half of `LootTable.fill`: the
// evaluated stacks go through the same slot shuffle and count splitting the
// seeded `abandoned_mineshaft` table uses, then land in `slot_limit` slots.

#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn loot_json_tag_items(tag: &str, depth: usize) -> Vec<&'static str> {
    let mut out = Vec::new();
    if depth > 8 {
        return out;
    }
    // Builtin tags are keyed bare ("planks") while datapack tags keep their
    // namespace ("minecraft:planks"); accept either spelling.
    let members = {
        let tags = item_tags();
        let tags = tags.lock().unwrap_or_else(|e| e.into_inner());
        tags.get(tag)
            .or_else(|| {
                tag.strip_prefix("minecraft:")
                    .and_then(|bare| tags.get(bare))
            })
            .cloned()
            .unwrap_or_default()
    };
    for member in members {
        if let Some(inner) = member.strip_prefix('#') {
            out.extend(loot_json_tag_items(inner, depth + 1));
        } else if let Some(name) = name_to_item(&member).and_then(item_to_name) {
            out.push(name);
        }
    }
    out
}

#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn loot_json_stack(item: &crate::chest_loot::LootItem) -> Option<Stack> {
    let id = name_to_item(item.item.as_str())?;
    if id == 0 || item.count <= 0 {
        return None;
    }
    let mut comps = Vec::new();
    if item.damage != 0 {
        comp_set_varint(&mut comps, COMP_DAMAGE, i64::from(item.damage));
    }
    if let Some((ench, lvl)) = &item.enchantment {
        if let Some(eid) = enchant_id(ench) {
            let bare = item
                .item
                .strip_prefix("minecraft:")
                .unwrap_or(item.item.as_str());
            let ty = if bare == "enchanted_book" {
                COMP_STORED_ENCHANTMENTS
            } else {
                COMP_ENCHANTMENTS
            };
            comps.push((ty, enchantments_data(&[(eid, i64::from(*lvl))], true)));
        }
    }
    let max = item_stack_max(id, &comps).clamp(1, 99) as i32;
    let count = item.count.clamp(0, max);
    (count > 0).then_some((id, count as u8, comps))
}

#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn loot_json_next_int(rng: &mut crate::generation_math::Lcg, min: i32, max: i32) -> i32 {
    if min >= max {
        min
    } else {
        rng.next_bound(max - min + 1) + min
    }
}

#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn loot_json_shuffle<T>(list: &mut [T], rng: &mut crate::generation_math::Lcg) {
    for i in (2..=list.len()).rev() {
        let j = rng.next_bound(i as i32) as usize;
        list.swap(i - 1, j);
    }
}

// The same split vanilla applies when a rolled stack does not fit the free
// slots: oversized stacks are divided at random, then the survivors shuffle.
#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn loot_json_split(
    items: Vec<Stack>,
    empty_slots: usize,
    rng: &mut crate::generation_math::Lcg,
) -> Vec<Stack> {
    let mut keep = Vec::new();
    let mut splitable = Vec::new();
    for item in items {
        if item.1 <= 1 {
            keep.push(item);
        } else {
            splitable.push(item);
        }
    }
    let mut available = empty_slots as isize - keep.len() as isize - splitable.len() as isize;
    while available > 0 && !splitable.is_empty() {
        available -= 1;
        let idx = loot_json_next_int(rng, 0, splitable.len() as i32 - 1) as usize;
        let mut stack = splitable.remove(idx);
        let take = loot_json_next_int(rng, 1, i32::from(stack.1) / 2);
        stack.1 -= take as u8;
        let split = (stack.0, take as u8, stack.2.clone());
        for part in [stack, split] {
            if part.1 > 1 && rng.next_bool() {
                splitable.push(part);
            } else {
                keep.push(part);
            }
        }
    }
    keep.extend(splitable);
    loot_json_shuffle(&mut keep, rng);
    keep
}

/// Fill `slot_limit` container slots from a raw loot-table JSON body
/// (`data/<ns>/loot_table/**.json` or a vanilla table). Seed 0 is unseeded,
/// matching `RandomizableContainer.unpackLootTable`.
#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
pub(crate) fn unpack_loot_table_json(
    table_json: &str,
    seed: i64,
    slot_limit: usize,
) -> HashMap<i16, Stack> {
    let mut out = HashMap::new();
    let Some(table) = crate::chest_loot::jparse(table_json.as_bytes()) else {
        return out;
    };
    let tag_items = |tag: &str| loot_json_tag_items(tag, 0);
    let smelt = |item: &str| {
        name_to_item(item)
            .and_then(smelt_result)
            .and_then(item_to_name)
    };
    let max_damage = |item: &str| {
        i32::try_from(item_max_durability(item.trim_start_matches("minecraft:")).unwrap_or(0))
            .unwrap_or(0)
    };
    let resolvers = crate::chest_loot::LootResolvers {
        tags: &tag_items,
        smelt: &smelt,
        max_damage: &max_damage,
    };
    let seed = if seed == 0 {
        world_rng::next_u64() as i64
    } else {
        seed
    };
    let mut rng = crate::generation_math::Lcg::new(seed as u64);
    let items = crate::chest_loot::evaluate_loot_table(
        &table,
        &crate::chest_loot::LootContext::default(),
        &mut rng,
        &resolvers,
    );
    let mut stacks: Vec<Stack> = Vec::new();
    for item in items.iter() {
        if let Some(stack) = loot_json_stack(item) {
            stacks.push(stack);
        }
    }
    let slot_limit = slot_limit.min(i16::MAX as usize);
    let mut slots: Vec<i16> = (0..slot_limit as i16).collect();
    loot_json_shuffle(&mut slots, &mut rng);
    for stack in loot_json_split(stacks, slots.len(), &mut rng) {
        let Some(slot) = slots.pop() else {
            break;
        };
        out.insert(slot, stack);
    }
    out
}

fn chests() -> &'static LockedMap<ContainerKey, HashMap<i16, Stack>> {
    CHESTS.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Is the container a menu was opened on still there? A block container is gone the
/// moment the world block it was built on is gone: vanilla's chunk stops holding the
/// block entity, so there is nothing left for a click to reach.
fn container_exists(key: ContainerKey) -> bool {
    match key {
        ContainerKey::Block(pos) => {
            is_ender_key((pos.1, pos.2, pos.3))
                || container_kind(block_at_dim(pos.0, pos.1, pos.2, pos.3)).is_some()
        }
        ContainerKey::Cart(uuid) => generated_content()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .carts
            .iter()
            .any(|c| c.uuid().ok() == Some(uuid)),
    }
}

// ---------- container openers and lid sync (ContainerOpenersCounter, 1.21.1) ----------
//
// `dqu` (ContainerOpenersCounter) keeps one `openCount` per block entity and calls
// `openerCountChanged(level, pos, state, previous, count)` on *every* change; `dqo`
// (ChestBlockEntity) implements that as `level.blockEvent(pos, block, 1, count)`, i.e.
// one `ClientboundBlockEventPacket(id=1, param=count)` per half. Each half of a double
// chest has its own counter (the combined `Container.startOpen` reaches both), so both
// halves emit. The client lerps the lid from that packet (`getOpenNess`), and a double
// chest renders `max(left, right)` (`dhb$3`).
//
// The port derives the count from the live windows instead of storing it. A stored count
// would be a second copy of "who has this open" that has to be kept true on open, close,
// disconnect, break and reload; vanilla's own counter is per block entity and deliberately
// not serialised, so nothing about it belongs in the save.

/// `ContainerOpenersCounter.getOpenerCount` for one half: how many clients in
/// `audience` hold a menu built on it. A paired window contains both halves, so both
/// halves see the same viewers - which is what `CompoundContainer.startOpen` does.
fn half_openers(audience: &[Arc<Client>], pos: BlockPos) -> u32 {
    half_openers_dim(DimId::Overworld, audience, pos)
}

/// The opener count of every half before a window change, so the change can be
/// diffed rather than assumed.
fn chest_opener_snapshot(audience: &[Arc<Client>], halves: &[BlockPos]) -> Vec<(BlockPos, u32)> {
    halves
        .iter()
        .map(|&pos| (pos, half_openers(audience, pos)))
        .collect()
}

/// 1.21.1 play clientbound 0x09 `ClientboundBlockUpdatePacket` (`acd`): BlockPos, VarInt block state.
fn broadcast_block_update(audience: &[Arc<Client>], pos: BlockPos, state: u32) {
    let mut body = Vec::with_capacity(12);
    write_pos(&mut body, pos.0, pos.1, pos.2);
    write_varint(&mut body, i64::from(state));
    let chunk = (pos.0 >> 4, pos.2 >> 4);
    fanout_in(
        audience,
        |c| {
            if c.chunks.lock().unwrap_or_else(|e| e.into_inner()).contains(&chunk) {
                return true;
            }
            let at = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
            let (dx, dy, dz) = (
                at.0 - f64::from(pos.0),
                at.1 - f64::from(pos.1),
                at.2 - f64::from(pos.2),
            );
            dx * dx + dy * dy + dz * dz <= 64.0 * 64.0
        },
        |_| (0x09, body.clone()),
    );
}

/// `ContainerOpenersCounter.openerCountChanged`: one block event, carrying the new
/// count, for each chest/ender-chest half whose count actually moved.
/// `BarrelBlockEntity` (`dqa$1`) overrides `onOpen`/`onClose` to toggle `open` and
/// broadcast a block update (`0x09`), and leaves `openerCountChanged` empty (no `0x08`).
fn sync_chest_openers(audience: &[Arc<Client>], before: &[(BlockPos, u32)]) {
    for &(pos, previous) in before {
        let now = half_openers(audience, pos);
        if now != previous {
            let st = block_at(pos.0, pos.1, pos.2);
            if let Some(reg) = breg().filter(|reg| reg.name_of(st) == "minecraft:barrel") {
                if (previous == 0) != (now == 0) {
                    let next = reg.with_prop(st, "open", if now > 0 { "true" } else { "false" });
                    if next != st {
                        set_block(pos.0, pos.1, pos.2, next);
                        broadcast_block_update(audience, pos, next);
                    }
                }
            } else {
                // vanilla writes the count with `writeByte`: the wire field is a byte.
                broadcast_block_event(audience, pos, 1, now as u8);
            }
        }
    }
}

/// 1.21.1 play clientbound 0x08 `ClientboundBlockEventPacket`: BlockPos, unsigned
/// byte action id, unsigned byte action param, VarInt **block** id. Verified against
/// the jar: `agh.a` registers `agg.a` (bundle, 0x00) then `agg.c..agg.j` in order, and
/// `agg.j` is `acc` = `ClientboundBlockEventPacket`, the ninth entry = 0x08; the
/// neighbouring entries land where the port's other ids do (0x07 block entity data,
/// 0x09 block update, 0x12 close container as `agg.t`).
fn broadcast_block_event(audience: &[Arc<Client>], pos: BlockPos, action: u8, param: u8) {
    let Some(block) = breg().and_then(|reg| reg.block_id_of(block_at(pos.0, pos.1, pos.2))) else {
        return;
    };
    let mut body = Vec::with_capacity(12);
    write_pos(&mut body, pos.0, pos.1, pos.2);
    body.push(action);
    body.push(param);
    write_varint(&mut body, i64::from(block));
    // vanilla `Level.blockEvent` reaches every player within 64 blocks of the cell.
    fanout_in(
        audience,
        |c| {
            let at = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
            let (dx, dy, dz) = (
                at.0 - f64::from(pos.0),
                at.1 - f64::from(pos.1),
                at.2 - f64::from(pos.2),
            );
            dx * dx + dy * dy + dz * dz <= 64.0 * 64.0
        },
        |_| (0x08, body.clone()),
    );
}

/// Is the menu a client opened still backed by the world? A block window needs its
/// block container (`container_exists`), and a paired one additionally needs both
/// halves to still name each other: the pair *is* block state, so a broken half, a
/// `/setblock` or a re-pairing invalidates the window.
fn window_live(open: (ContainerKey, Option<ContainerKey>)) -> bool {
    let (first, second) = open;
    if !container_exists(first) || second.is_some_and(|key| !container_exists(key)) {
        return false;
    }
    let (
        ContainerKey::Block((rdim, rx, ry, rz)),
        Some(ContainerKey::Block((ldim, lx, ly, lz))),
    ) = (first, second)
    else {
        return true;
    };
    // A pair is block state, so both halves must be in the same dimension.
    rdim == ldim
        && chest_pair_of_dim(
            rdim,
            block_at_dim(rdim, rx, ry, rz),
            (rx, ry, rz),
        ) == Some(((rx, ry, rz), (lx, ly, lz)))
}
fn window_live_for(
    c: &Client,
    open: (ContainerKey, Option<ContainerKey>),
) -> bool {
    if !window_live(open) {
        return false;
    }
    if matches!(open, (ContainerKey::Block(ek), None) if is_ender_key((ek.1, ek.2, ek.3))) {
        let anchor = c
            .chest_sync
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|s| s.window == open)
            .and_then(|s| s.anchor);
        // The anchor lives in the clicking client's dimension: the ender key
        // is always filed under Overworld, so its block must be read where
        // the window was actually opened.
        let client_dim = *c.dim.lock().unwrap_or_else(|e| e.into_inner());
        return anchor
            .is_some_and(|pos| ender_chest_named(block_at_dim(client_dim, pos.0, pos.1, pos.2)));
    }
    true
}

/// The world no longer holds the container behind `key`: every menu built on it dies
/// (`ClientboundContainerClosePacket` on windowId 1, the only id `open_chest_window`
/// allocates) and the opener counts of `halves` drop. Returns how many windows closed.
fn invalidate_container_windows(
    audience: &[Arc<Client>],
    key: ContainerKey,
    halves: &[BlockPos],
) -> usize {
    let before = chest_opener_snapshot(audience, halves);
    let mut closed = 0;
    for c in audience {
        let had = {
            let mut open = c.open_chest.lock().unwrap_or_else(|e| e.into_inner());
            if open.is_some_and(|(a, b)| a == key || b == Some(key)) {
                *open = None;
                true
            } else {
                false
            }
        };
        if had {
            *c.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        if had {
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x12, vec![1u8]);
            }
            closed += 1;
        }
    }
    sync_chest_openers(audience, &before);
    closed
}
/// `invalidate_container_windows` over dimensioned halves: same close loop,
/// opener counts diffed through the `_dim` snapshot/sync pair.
fn invalidate_container_windows_dim(
    audience: &[Arc<Client>],
    key: ContainerKey,
    halves: &[DimBlockPos],
) -> usize {
    let before = chest_opener_snapshot_dim(audience, halves);
    let mut closed = 0;
    for c in audience {
        let had = {
            let mut open = c.open_chest.lock().unwrap_or_else(|e| e.into_inner());
            if open.is_some_and(|(a, b)| a == key || b == Some(key)) {
                *open = None;
                true
            } else {
                false
            }
        };
        if had {
            *c.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        if had {
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x12, vec![1u8]);
            }
            closed += 1;
        }
    }
    sync_chest_openers_dim(audience, &before);
    closed
}
fn invalidate_ender_chest_windows(audience: &[Arc<Client>], pos: BlockPos) -> usize {
    invalidate_ender_chest_windows_dim(audience, DimId::Overworld, pos)
}
/// An ender window's anchor lives in the clicking client's dimension, so only
/// clients in `dim` can hold a menu on the ender chest at `pos` there.
fn invalidate_ender_chest_windows_dim(
    audience: &[Arc<Client>],
    dim: DimId,
    pos: BlockPos,
) -> usize {
    let mut closed = 0;
    for c in audience {
        if *c.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim {
            continue;
        }
        let had = {
            let mut open = c.open_chest.lock().unwrap_or_else(|e| e.into_inner());
            let mut sync = c.chest_sync.lock().unwrap_or_else(|e| e.into_inner());
            let matches_anchor = open.is_some_and(|(a, b)| {
                matches!(a, ContainerKey::Block(ek) if is_ender_key((ek.1, ek.2, ek.3))) && b.is_none()
            }) && sync.as_ref().and_then(|s| s.anchor) == Some(pos);
            if matches_anchor {
                *open = None;
                *sync = None;
                true
            } else {
                false
            }
        };
        if had {
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x12, vec![1u8]);
            }
            closed += 1;
        }
    }
    closed
}
fn invalidate_client_chest_window(
    audience: &[Arc<Client>],
    me: &Client,
    open: (ContainerKey, Option<ContainerKey>),
) {
    if matches!(open, (ContainerKey::Block(ek), None) if is_ender_key((ek.1, ek.2, ek.3))) {
        if let Some((_, Some(pos))) = client_open_window_and_anchor(me) {
            let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
            invalidate_ender_chest_windows_dim(audience, dim, pos);
            return;
        }
    }
    invalidate_container_windows_dim(audience, open.0, &chest_window_halves_dim(open));
}

// ---------- block-state classification ----------
//
// Container/hopper/furnace classification asks the registry for the block
// name and properties, never the raw state id: state ids shift between
// versions and a mod suite renumbers them entirely, while `minecraft:chest`
// keeps its name. The 1.21.1 numeric ranges remain as the fallback for when
// the registry did not load — the same answer the arithmetic gave before
// names were consulted.

fn container_kind(st: u32) -> Option<&'static str> {
    if let Some(reg) = breg() {
        return match reg.name_of(st) {
            "minecraft:chest" | "minecraft:trapped_chest" => Some("Chest"),
            "minecraft:barrel" => Some("Barrel"),
            n if n == "minecraft:shulker_box"
                || (n.starts_with("minecraft:") && n.ends_with("_shulker_box")) =>
            {
                Some("Shulker Box")
            }
            _ => None,
        };
    }
    // 27-slot generic container blocks, 1.21.1 state ids
    if (2954..=2977).contains(&st) {
        return Some("Chest");
    }
    if (9119..=9142).contains(&st) {
        return Some("Chest");
    } // trapped_chest
    if (18408..=18419).contains(&st) {
        return Some("Barrel");
    }
    if (12562..=12663).contains(&st) {
        return Some("Shulker Box");
    }
    None
}
/// `minecraft:furnace`: the plain furnace only — blast furnace and smoker are
/// separate block names and keep their own stores.
fn furnace_named(st: u32) -> bool {
    match breg() {
        Some(reg) => reg.name_of(st) == "minecraft:furnace",
        None => (4295..=4302).contains(&st),
    }
}
/// `minecraft:ender_chest`: every facing state of the ender chest block.
fn ender_chest_named(st: u32) -> bool {
    match breg() {
        Some(reg) => reg.name_of(st) == "minecraft:ender_chest",
        None => (7513..=7520).contains(&st),
    }
}
// ---------- chest world state (ChestBlock / ChestBlockEntity, 1.21.1) ----------
//
// A chest is not "a container at a position": its block state says whether this cell
// is the `left` or `right` half of a pair and which way it faces, and that state is
// the only definition of its partner. None of the functions below infer a pair from
// raw adjacency, because adjacency cannot tell a real pair from two chests that
// happen to touch.
//
// Read from `javap -p -c` on server-1.21.1.jar (mappings from /var/tmp/mcm/server.txt):
//   dhb.g(dtc)      state TYPE -> DoubleBlockCombiner.BlockType: single->SINGLE,
//                   right->FIRST, left->SECOND (`dif$a.a/b/c`)
//   dif.a(..)       own FIRST keeps own BE first, own SECOND puts the neighbour
//                   first: `CompoundContainer(first, second)`
//   bqj.a(int)      slot < first.size ? first.a(slot) : second.a(slot - first.size)
// so the combined container is slots 0..=26 = the RIGHT half and 27..=53 = the LEFT,
// whichever half the player clicked.

/// The horizontal half of vanilla's `Direction`, in 2D data-value order
/// (`Direction.from2DDataValue`: south, west, north, east). Chest pairing only asks
/// horizontal questions, and this order is what `getClockWise()` means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HFace {
    South,
    West,
    North,
    East,
}

impl HFace {
    /// `Direction.fromYaw` = `Mth.floor(yaw / 90 + 0.5) & 3`; the port's yaw is in
    /// degrees with 0 = south, which is the convention `getRotation()` uses.
    fn from_yaw(yaw: f32) -> HFace {
        match ((yaw / 90.0 + 0.5).floor() as i64) & 3 {
            0 => HFace::South,
            1 => HFace::West,
            2 => HFace::North,
            _ => HFace::East,
        }
    }
    /// `Direction.from3DDataValue` for the four horizontal values (DOWN 0, UP 1,
    /// NORTH 2, SOUTH 3, WEST 4, EAST 5).
    fn from_face(face: u8) -> Option<HFace> {
        match face {
            2 => Some(HFace::North),
            3 => Some(HFace::South),
            4 => Some(HFace::West),
            5 => Some(HFace::East),
            _ => None,
        }
    }
    fn opposite(self) -> HFace {
        match self {
            HFace::South => HFace::North,
            HFace::West => HFace::East,
            HFace::North => HFace::South,
            HFace::East => HFace::West,
        }
    }
    /// `Direction.getClockWise()`: north->east, east->south, south->west, west->north.
    fn clock_wise(self) -> HFace {
        match self {
            HFace::North => HFace::East,
            HFace::East => HFace::South,
            HFace::South => HFace::West,
            HFace::West => HFace::North,
        }
    }
    /// `Direction.getCounterClockWise()`: north->west, west->south, south->east,
    /// east->north.
    fn counter_clock_wise(self) -> HFace {
        match self {
            HFace::North => HFace::West,
            HFace::West => HFace::South,
            HFace::South => HFace::East,
            HFace::East => HFace::North,
        }
    }
    /// `Direction.getAxis()`: west/east are the x axis, north/south the z axis.
    fn axis(self) -> u8 {
        match self {
            HFace::West | HFace::East => 0,
            HFace::North | HFace::South => 1,
        }
    }
    fn name(self) -> &'static str {
        match self {
            HFace::South => "south",
            HFace::West => "west",
            HFace::North => "north",
            HFace::East => "east",
        }
    }
    fn parse(name: &str) -> Option<HFace> {
        match name {
            "south" => Some(HFace::South),
            "west" => Some(HFace::West),
            "north" => Some(HFace::North),
            "east" => Some(HFace::East),
            _ => None,
        }
    }
    /// `BlockPos.relative(self)` in x/z.
    fn step(self) -> (i32, i32) {
        match self {
            HFace::South => (0, 1),
            HFace::West => (-1, 0),
            HFace::North => (0, -1),
            HFace::East => (1, 0),
        }
    }
}

/// The chest block this state belongs to. `chest` and `trapped_chest` are different
/// `Block` instances in vanilla, and `BlockState.is(this)` in `candidatePartnerFacing`
/// compares instances, so the two kinds never pair.
fn chest_kind(st: u32) -> Option<&'static str> {
    match breg()?.name_of(st) {
        "minecraft:chest" => Some("minecraft:chest"),
        "minecraft:trapped_chest" => Some("minecraft:trapped_chest"),
        _ => None,
    }
}
fn chest_named(st: u32) -> bool {
    chest_kind(st).is_some()
}
fn chest_facing(st: u32) -> Option<HFace> {
    HFace::parse(&breg()?.get_prop(st, "facing")?)
}
/// `ChestType`: `single`, `left`, `right` (dtu field order a/b/c).
fn chest_type(st: u32) -> Option<&'static str> {
    match breg()?.get_prop(st, "type")?.as_str() {
        "single" => Some("single"),
        "left" => Some("left"),
        "right" => Some("right"),
        _ => None,
    }
}
/// `ChestBlock.getConnectedDirection`: the cell this half is connected to. A `single`
/// chest points at nothing, which is why it cannot be half of a pair.
fn chest_connected_direction(st: u32) -> Option<HFace> {
    let facing = chest_facing(st)?;
    match chest_type(st)? {
        "left" => Some(facing.clock_wise()),
        "right" => Some(facing.counter_clock_wise()),
        _ => None,
    }
}
/// The pair the half at `pos` belongs to, ordered the way `DoubleBlockCombiner`
/// orders it: `(right, left)`. `None` for a `single` state, for two chests that merely
/// touch, for the two kinds mixed, and for a half whose partner does not point back.
fn chest_pair(x: i32, y: i32, z: i32) -> Option<(BlockPos, BlockPos)> {
    chest_pair_of(block_at(x, y, z), (x, y, z))
}
fn chest_pair_of(st: u32, pos: BlockPos) -> Option<(BlockPos, BlockPos)> {
    chest_pair_of_dim(DimId::Overworld, st, pos)
}
fn chest_pair_of_dim(dim: DimId, st: u32, pos: BlockPos) -> Option<(BlockPos, BlockPos)> {
    let facing = chest_facing(st)?;
    let own = chest_type(st)?;
    let dir = chest_connected_direction(st)?;
    let (dx, dz) = dir.step();
    let other = (pos.0 + dx, pos.1, pos.2 + dz);
    let ost = block_at_dim(dim, other.0, other.1, other.2);
    // `BlockState.is(this)`: a trapped chest beside a chest is a different block.
    if breg()?.name_of(ost) != breg()?.name_of(st) {
        return None;
    }
    if chest_facing(ost)? != facing {
        return None;
    }
    // The partner must point back: with equal facings that is exactly "opposite TYPE".
    if chest_connected_direction(ost)? != dir.opposite() {
        return None;
    }
    match own {
        "right" => Some((pos, other)),
        _ => Some((other, pos)),
    }
}
/// The halves the crate at `pos` occupied while it still had state `st`: both halves
/// of its pair, or just the cell when it was `single`. Read from the *old* state, so a
/// removal can still tell which counters to drop. Empty for a non-chest.
fn chest_halves_of(st: u32, pos: BlockPos) -> Vec<BlockPos> {
    if !chest_named(st) {
        return Vec::new();
    }
    match chest_pair_of(st, pos) {
        Some((right, left)) => vec![right, left],
        None => vec![pos],
    }
}
/// The window a container at `pos` opens, in `DoubleBlockCombiner` order: the first key
/// owns slots 0..=26. A barrel, a shulker box or a `single` chest is its own window.
fn chest_window(x: i32, y: i32, z: i32) -> (ContainerKey, Option<ContainerKey>) {
    match chest_pair(x, y, z) {
        Some((right, left)) => (ContainerKey::ow(right), Some(ContainerKey::ow(left))),
        None => (ContainerKey::ow((x, y, z)), None),
    }
}
fn chest_window_dim(dim: DimId, x: i32, y: i32, z: i32) -> (ContainerKey, Option<ContainerKey>) {
    match chest_pair_of_dim(dim, block_at_dim(dim, x, y, z), (x, y, z)) {
        Some((right, left)) => (
            ContainerKey::Block((dim, right.0, right.1, right.2)),
            Some(ContainerKey::Block((dim, left.0, left.1, left.2))),
        ),
        None => (ContainerKey::Block((dim, x, y, z)), None),
    }
}
/// Which cells a window's opener counters live on. The synthetic ender-chest key has
/// no fixed block pos in the key itself; callers with a client anchor use
/// `client_window_halves` to include the clicked ender chest cell.
fn chest_window_halves(open: (ContainerKey, Option<ContainerKey>)) -> Vec<BlockPos> {
    let mut out: Vec<BlockPos> = Vec::new();
    for key in [Some(open.0), open.1].into_iter().flatten() {
        if let ContainerKey::Block((_dim, x, y, z)) = key {
            let pos = (x, y, z);
            if !is_ender_key(pos) && !out.contains(&pos) {
                out.push(pos);
            }
        }
    }
    out
}
type OpenWindowAnchor = ((ContainerKey, Option<ContainerKey>), Option<BlockPos>);

fn client_open_window_and_anchor(c: &Client) -> Option<OpenWindowAnchor> {
    let open = (*c.open_chest.lock().unwrap_or_else(|e| e.into_inner()))?;
    let anchor = c
        .chest_sync
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|s| s.window == open)
        .and_then(|s| s.anchor);
    Some((open, anchor))
}
/// `ChestBlock.isBlockedChestByBlock` (`dhb.a(Ldcc;Ljd;)Z`): true when `pos.above()`
/// `isRedstoneConductor`. Shared by `ChestBlock` and `EnderChestBlock.useWithoutItem`.
fn is_blocked_chest_by_block(pos: BlockPos) -> bool {
    let above = block_at(pos.0, pos.1 + 1, pos.2);
    breg().is_some_and(|reg| reg.is_redstone_conductor(above))
}
/// `ChestBlock.isCatSittingOnChest` (`dhb.b(Ldcx;Ljd;)Z`): true when a sitting `Cat`
/// (`etype == 15`, dimensions `0.6 x 0.7`) intersects `AABB(x, y+1, z, x+1, y+2, z+1)`.
fn is_cat_sitting_on_chest(pos: BlockPos) -> bool {
    let sitting = sitting_cats().lock().unwrap_or_else(|e| e.into_inner());
    if sitting.is_empty() {
        return false;
    }
    let min = [f64::from(pos.0), f64::from(pos.1 + 1), f64::from(pos.2)];
    let max = [
        f64::from(pos.0 + 1),
        f64::from(pos.1 + 2),
        f64::from(pos.2 + 1),
    ];
    let ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
    ms.iter().any(|m| {
        if m.etype != 15 || !sitting.contains(&m.eid) || *m.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0 {
            return false;
        }
        let (mx, my, mz, ..) = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
        aabb_overlaps(
            (min, max),
            ([mx - 0.3, my, mz - 0.3], [mx + 0.3, my + 0.7, mz + 0.3]),
        )
    })
}
/// `ChestBlock.isChestBlockedAt` (`dhb.a(Ldcx;Ljd;)Z`): blocked if the block above is
/// a redstone conductor or a cat is sitting on top.
fn is_chest_blocked_at(pos: BlockPos) -> bool {
    is_blocked_chest_by_block(pos) || is_cat_sitting_on_chest(pos)
}
/// `ChestBlock.getMenuProvider` (`dhb.b` -> `dif.a` with `dhb::isChestBlockedAt`):
/// a single chest is blocked if its cell is blocked; a double chest is blocked if EITHER
/// half is blocked. Barrels and minecart chests never use this predicate.
fn chest_window_blocked(window: (ContainerKey, Option<ContainerKey>)) -> bool {
    for pos in chest_window_halves(window) {
        if is_chest_blocked_at(pos) {
            return true;
        }
    }
    false
}
fn client_eye_pos(c: &Client) -> (f64, f64, f64) {
    let (px, py, pz, ..) = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
    // 1.21.1 `Player` pose eye heights (`cmx.<clinit>`): standing 1.62f, crouching 1.27f.
    let eye_h = if *c.sneaking.lock().unwrap_or_else(|e| e.into_inner()) {
        1.27
    } else {
        1.62
    };
    (px, py + eye_h, pz)
}
fn aabb_dist_sq_to_point(min: [f64; 3], max: [f64; 3], pt: (f64, f64, f64)) -> f64 {
    // 1.21.1 `AABB.distanceToSqr(Vec3)` (`ewx.e(Lexc;)D`).
    let dx = (min[0] - pt.0).max(pt.0 - max[0]).max(0.0);
    let dy = (min[1] - pt.1).max(pt.1 - max[1]).max(0.0);
    let dz = (min[2] - pt.2).max(pt.2 - max[2]).max(0.0);
    dx * dx + dy * dy + dz * dz
}
fn can_interact_with_block(c: &Client, pos: BlockPos, extra: f64) -> bool {
    // 1.21.1 `Player.canInteractWithBlock(BlockPos, double)` (`cmx.a(Ljd;D)Z`):
    // `double d = this.blockInteractionRange() + distance;`
    // `return new AABB(pos).distanceToSqr(this.getEyePosition()) < d * d;`
    // `Attributes.BLOCK_INTERACTION_RANGE` = 4.5, creative modifier `+0.5` -> 5.0.
    let base = if *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1 {
        5.0
    } else {
        4.5
    };
    let d = base + extra;
    let min = [f64::from(pos.0), f64::from(pos.1), f64::from(pos.2)];
    let max = [
        f64::from(pos.0 + 1),
        f64::from(pos.1 + 1),
        f64::from(pos.2 + 1),
    ];
    aabb_dist_sq_to_point(min, max, client_eye_pos(c)) < d * d
}
/// 1.21.1 `ChestMenu.stillValid` (`cqc.b`) -> `Container.stillValidBlockEntity`
/// (`bqk.a(Ldqh;Lcmx;F)` with `4.0f`) / `CompoundContainer.stillValid` (`bqj.a` checks
/// both halves) / `ContainerEntity.isChestVehicleStillValid` (`cox.g`).
fn chest_still_valid(
    c: &Client,
    open: (ContainerKey, Option<ContainerKey>),
    anchor: Option<BlockPos>,
) -> bool {
    if !window_live(open) {
        return false;
    }
    match open {
        (ContainerKey::Block((_dim, x, y, z)), second) => {
            let first = (x, y, z);
            let first_ok = if is_ender_key(first) {
                // The anchor lives in the clicking client's dimension: the
                // ender key is filed under Overworld, so the block check must
                // read the dimension the window was opened in.
                let client_dim = *c.dim.lock().unwrap_or_else(|e| e.into_inner());
                anchor.is_some_and(|pos| {
                    ender_chest_named(block_at_dim(client_dim, pos.0, pos.1, pos.2))
                        && can_interact_with_block(c, pos, 4.0)
                })
            } else {
                can_interact_with_block(c, first, 4.0)
            };
            if !first_ok {
                return false;
            }
            match second {
                Some(ContainerKey::Block((_, x2, y2, z2))) => {
                    can_interact_with_block(c, (x2, y2, z2), 4.0)
                }
                _ => true,
            }
        }
        (ContainerKey::Cart(uuid), _) => {
            let cart_pos = generated_content()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .carts
                .iter()
                .find(|cart| cart.uuid().ok() == Some(uuid))
                .and_then(|cart| cart.pos().ok());
            let Some(pos) = cart_pos else {
                return false;
            };
            // `Attributes.ENTITY_INTERACTION_RANGE` = 3.0, creative modifier `+2.0` -> 5.0.
            let base = if *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1 {
                5.0
            } else {
                3.0
            };
            let d = base + 4.0;
            let min = [pos[0] - 0.49, pos[1], pos[2] - 0.49];
            let max = [pos[0] + 0.49, pos[1] + 0.7, pos[2] + 0.49];
            aabb_dist_sq_to_point(min, max, client_eye_pos(c)) < d * d
        }
    }
}
fn chest_window_slots(window: (ContainerKey, Option<ContainerKey>)) -> Vec<Stack> {
    let first = window.0;
    let rows = if window.1.is_some() { 54usize } else { 27usize };
    let mut out = vec![(0, 0, Vec::new()); rows];
    let all = chests().lock().unwrap_or_else(|e| e.into_inner());
    let ch = all.get(&first).cloned().unwrap_or_default();
    for i in 0..27i16 {
        if let Some(st) = ch.get(&i) {
            if st.1 > 0 {
                out[i as usize] = st.clone();
            }
        }
    }
    if let Some(second) = window.1 {
        if let Some(ch2) = all.get(&second) {
            for i in 0..27i16 {
                if let Some(st) = ch2.get(&i) {
                    if st.1 > 0 {
                        out[27 + i as usize] = st.clone();
                    }
                }
            }
        }
    }
    out
}
/// Per-tick `ServerPlayer.tick` (`aqv.l`) container maintenance: closes viewers that
/// fail `stillValid` (emitting `0x12` and updating opener counts via `0x08`) and runs
/// `AbstractContainerMenu.broadcastChanges` (`cpu.d`) for remaining viewers by diffing
/// container slots `0..rows` and sending `0x15` (`ClientboundContainerSetSlotPacket`)
/// with incremented `stateId` (`(stateId + 1) & 32767`).
fn chest_viewer_tick_on(audience: &[Arc<Client>]) {
    if audience.is_empty() {
        return;
    }
    let mut updates = CHEST_SYNC_SCRATCH.with(scratch_take);
    for c in audience {
        let open = *c.open_chest.lock().unwrap_or_else(|e| e.into_inner());
        let Some(win) = open else {
            *c.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
            continue;
        };
        let anchor = c
            .chest_sync
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|s| s.window == win)
            .and_then(|s| s.anchor);
        if !chest_still_valid(c, win, anchor) {
            // Anchor halves live in the clicking client's dimension; block
            // halves carry their own dim in the key.
            let client_dim = *c.dim.lock().unwrap_or_else(|e| e.into_inner());
            let halves = client_window_halves_dim(win, anchor, client_dim);
            let before = chest_opener_snapshot_dim(audience, &halves);
            *c.open_chest.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *c.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x12, vec![1u8]);
            }
            sync_chest_openers_dim(audience, &before);
            continue;
        }
        // Borrow-diff: every slot read and the sync-state compare run under
        // one `chests()` hold, so an unchanged window costs the lock plus the
        // lookups — no per-tick clone of the slot map, no `Vec` rebuild. A
        // `Stack` only clones when the slot actually changed.
        let rows = if win.1.is_some() { 54usize } else { 27usize };
        updates.clear();
        {
            let all = chests().lock().unwrap_or_else(|e| e.into_inner());
            let mut sync_guard = c.chest_sync.lock().unwrap_or_else(|e| e.into_inner());
            let sync = sync_guard.get_or_insert_with(|| ChestSyncState {
                window: win,
                anchor,
                state_id: 0,
                last_slots: vec![Stack::default(); rows],
            });
            if sync.window != win || sync.last_slots.len() != rows {
                sync.window = win;
                sync.anchor = anchor;
                sync.state_id = 0;
                sync.last_slots = vec![Stack::default(); rows];
            }
            for idx in 0..rows {
                // `DoubleBlockCombiner` order: the first key owns slots
                // 0..=26, the second 27..=53 — the layout `chest_window_slots`
                // materialized as a `Vec`, read slot-wise instead.
                let (key, slot) = match idx < 27 {
                    true => (win.0, idx as i16),
                    false => match win.1 {
                        Some(k) => (k, (idx - 27) as i16),
                        None => break,
                    },
                };
                let stack = all
                    .get(&key)
                    .and_then(|m| m.get(&slot))
                    .filter(|st| st.1 > 0);
                let unchanged = match stack {
                    Some(st) => *st == sync.last_slots[idx],
                    None => sync.last_slots[idx].1 == 0,
                };
                if unchanged {
                    continue;
                }
                let cur = stack.cloned().unwrap_or_default();
                sync.state_id = (sync.state_id + 1) & 0x7fff;
                sync.last_slots[idx] = cur.clone();
                updates.push((idx as i16, sync.state_id, cur));
            }
        }
        if !updates.is_empty() {
            if let Ok(mut w) = c.sock.lock() {
                for (slot, state_id, stack) in updates.drain(..) {
                    let mut p = vec![1u8];
                    write_varint(&mut p, i64::from(state_id));
                    p.extend_from_slice(&slot.to_be_bytes());
                    p.extend_from_slice(&slot_wire(&stack));
                    let _ = send_packet(&mut w, 0x15, p);
                }
            }
        }
    }
    CHEST_SYNC_SCRATCH.with(|s| scratch_return(s, updates));
}
fn chest_viewer_tick() {
    let audience = client_snapshot();
    chest_viewer_tick_on(&audience);
}
/// `ChestBlock.candidatePartnerFacing`: the facing of the chest at `pos` if a new one
/// could pair with it. `is(this)` plus `TYPE == SINGLE` is the whole gate, so an
/// already-paired half is never a candidate.
fn chest_candidate_partner_facing(pos: BlockPos, name: &str) -> Option<HFace> {
    let st = block_at(pos.0, pos.1, pos.2);
    if breg()?.name_of(st) != name {
        return None;
    }
    if chest_type(st)? != "single" {
        return None;
    }
    chest_facing(st)
}
/// `ChestBlock.getStateForPlacement`: facing from the player (opposite of the way they
/// look), `type` from the neighbour this placement may pair with, `waterlogged` from
/// the fluid state of the cell being replaced.
fn chest_placement_state(
    name: &str,
    pos: BlockPos,
    yaw: f32,
    sneaking: bool,
    face: u8,
) -> u32 {
    let Some(reg) = breg() else { return 0 };
    let mut facing = HFace::from_yaw(yaw).opposite();
    let mut kind = "single";
    // Sneaking against a horizontal face claims that side: only the clicked block may
    // become the partner, and only when its facing is on the other axis.
    if sneaking {
        if let Some(clicked) = HFace::from_face(face) {
            let back = clicked.opposite();
            let (bx, bz) = back.step();
            let behind = (pos.0 + bx, pos.1, pos.2 + bz);
            if let Some(candidate) = chest_candidate_partner_facing(behind, name) {
                if candidate.axis() != clicked.axis() {
                    facing = candidate;
                    kind = if candidate.counter_clock_wise() == back {
                        "right"
                    } else {
                        "left"
                    };
                }
            }
        }
    }
    // Otherwise the pair axis is perpendicular to facing: clockwise is LEFT, then
    // counter-clockwise is RIGHT (vanilla checks them in that order, first match wins).
    if kind == "single" && !sneaking {
        let (cx, cz) = facing.clock_wise().step();
        let clockwise = (pos.0 + cx, pos.1, pos.2 + cz);
        if chest_candidate_partner_facing(clockwise, name) == Some(facing) {
            kind = "left";
        } else {
            let (cx, cz) = facing.counter_clock_wise().step();
            let counter = (pos.0 + cx, pos.1, pos.2 + cz);
            if chest_candidate_partner_facing(counter, name) == Some(facing) {
                kind = "right";
            }
        }
    }
    let waterlogged = reg.fluid_name(block_at(pos.0, pos.1, pos.2)) == "minecraft:water";
    let st = reg.state_of(name, None);
    let st = reg.with_prop(st, "facing", facing.name());
    let st = reg.with_prop(st, "type", kind);
    reg.with_prop(st, "waterlogged", if waterlogged { "true" } else { "false" })
}
/// `ChestBlock.updateShape`: the state this chest should take now that its neighbour in
/// direction `dir` changed. The `else` is vanilla's and is load-bearing: when the
/// neighbour *is* the same chest block only the pairing branch may change us, so a half
/// left facing a chest that no longer points back keeps its stale `left`/`right` rather
/// than unpairing.
fn chest_shape_after(own: u32, own_pos: BlockPos, dir: HFace) -> Option<u32> {
    let reg = breg()?;
    let (dx, dz) = dir.step();
    let nst = block_at(own_pos.0 + dx, own_pos.1, own_pos.2 + dz);
    if reg.name_of(nst) == reg.name_of(own) {
        let own_type = chest_type(own)?;
        let ntype = chest_type(nst)?;
        if own_type == "single"
            && ntype != "single"
            && chest_facing(own)? == chest_facing(nst)?
            && chest_connected_direction(nst)? == dir.opposite()
        {
            let opposite = if ntype == "left" { "right" } else { "left" };
            return Some(reg.with_prop(own, "type", opposite));
        }
        return None;
    }
    if chest_connected_direction(own)? == dir {
        return Some(reg.with_prop(own, "type", "single"));
    }
    None
}
/// `Block.updateNeighbourShapes` for chests: after a write, each chest beside the cell
/// re-derives its own state. The written cell is deliberately not repaired - vanilla
/// only ever calls a block's own `updateShape` from its neighbours, so a `/setblock`ed
/// ghost half stays a ghost half in both implementations.
fn repair_chest_neighbours(pos: BlockPos) {
    for dir in [HFace::South, HFace::West, HFace::North, HFace::East] {
        let (dx, dz) = dir.step();
        let np = (pos.0 + dx, pos.1, pos.2 + dz);
        let own = block_at(np.0, np.1, np.2);
        if !chest_named(own) {
            continue;
        }
        // `dir` from the neighbour's own point of view is the direction it looks back.
        if let Some(next) = chest_shape_after(own, np, dir.opposite()) {
            if next != own {
                set_block(np.0, np.1, np.2, next);
                note_block_update(np.0, np.1, np.2, next);
            }
        }
    }
}
/// Does a write at `pos` have to repair chest pairing? True when the cell itself is (or
/// was) a chest, or when a horizontal neighbour is one.
fn chest_touches(pos: BlockPos, before: u32, after: u32) -> bool {
    if chest_named(before) || chest_named(after) {
        return true;
    }
    [HFace::South, HFace::West, HFace::North, HFace::East]
        .iter()
        .any(|dir| {
            let (dx, dz) = dir.step();
            chest_named(block_at(pos.0 + dx, pos.1, pos.2 + dz))
        })
}

fn name_to_item(n: &str) -> Option<i64> {
    #[cfg(feature = "neoforge-events")]
    if let Some(state) = java_compat::NEOFORGE.get().and_then(|runtime| {
        runtime.registry.runtime_id(landing_types::ModRegistry::Item, n)
    }) {
        return Some(i64::from(state));
    }
    let n = n.strip_prefix("minecraft:").unwrap_or(n);
    #[cfg(feature = "mod-suite")]
    if let Some(id) = live_create_resolve_item(n) {
        return Some(id);
    }
    Some(match n {
        "air" => 0,
        "stone" => 1,
        "granite" => 2,
        "polished_granite" => 3,
        "diorite" => 4,
        "polished_diorite" => 5,
        "andesite" => 6,
        "polished_andesite" => 7,
        "deepslate" => 8,
        "cobbled_deepslate" => 9,
        "polished_deepslate" => 10,
        "calcite" => 11,
        "tuff" => 12,
        "tuff_slab" => 13,
        "tuff_stairs" => 14,
        "tuff_wall" => 15,
        "chiseled_tuff" => 16,
        "polished_tuff" => 17,
        "polished_tuff_slab" => 18,
        "polished_tuff_stairs" => 19,
        "polished_tuff_wall" => 20,
        "tuff_bricks" => 21,
        "tuff_brick_slab" => 22,
        "tuff_brick_stairs" => 23,
        "tuff_brick_wall" => 24,
        "chiseled_tuff_bricks" => 25,
        "dripstone_block" => 26,
        "grass_block" => 27,
        "dirt" => 28,
        "coarse_dirt" => 29,
        "podzol" => 30,
        "rooted_dirt" => 31,
        "mud" => 32,
        "crimson_nylium" => 33,
        "warped_nylium" => 34,
        "cobblestone" => 35,
        "oak_planks" => 36,
        "spruce_planks" => 37,
        "birch_planks" => 38,
        "jungle_planks" => 39,
        "acacia_planks" => 40,
        "cherry_planks" => 41,
        "dark_oak_planks" => 42,
        "mangrove_planks" => 43,
        "bamboo_planks" => 44,
        "crimson_planks" => 45,
        "warped_planks" => 46,
        "bamboo_mosaic" => 47,
        "oak_sapling" => 48,
        "spruce_sapling" => 49,
        "birch_sapling" => 50,
        "jungle_sapling" => 51,
        "acacia_sapling" => 52,
        "cherry_sapling" => 53,
        "dark_oak_sapling" => 54,
        "mangrove_propagule" => 55,
        "bedrock" => 56,
        "sand" => 57,
        "suspicious_sand" => 58,
        "suspicious_gravel" => 59,
        "red_sand" => 60,
        "gravel" => 61,
        "coal_ore" => 62,
        "deepslate_coal_ore" => 63,
        "iron_ore" => 64,
        "deepslate_iron_ore" => 65,
        "copper_ore" => 66,
        "deepslate_copper_ore" => 67,
        "gold_ore" => 68,
        "deepslate_gold_ore" => 69,
        "redstone_ore" => 70,
        "deepslate_redstone_ore" => 71,
        "emerald_ore" => 72,
        "deepslate_emerald_ore" => 73,
        "lapis_ore" => 74,
        "deepslate_lapis_ore" => 75,
        "diamond_ore" => 76,
        "deepslate_diamond_ore" => 77,
        "nether_gold_ore" => 78,
        "nether_quartz_ore" => 79,
        "ancient_debris" => 80,
        "coal_block" => 81,
        "raw_iron_block" => 82,
        "raw_copper_block" => 83,
        "raw_gold_block" => 84,
        "heavy_core" => 85,
        "amethyst_block" => 86,
        "budding_amethyst" => 87,
        "iron_block" => 88,
        "copper_block" => 89,
        "gold_block" => 90,
        "diamond_block" => 91,
        "netherite_block" => 92,
        "exposed_copper" => 93,
        "weathered_copper" => 94,
        "oxidized_copper" => 95,
        "chiseled_copper" => 96,
        "exposed_chiseled_copper" => 97,
        "weathered_chiseled_copper" => 98,
        "oxidized_chiseled_copper" => 99,
        "cut_copper" => 100,
        "exposed_cut_copper" => 101,
        "weathered_cut_copper" => 102,
        "oxidized_cut_copper" => 103,
        "cut_copper_stairs" => 104,
        "exposed_cut_copper_stairs" => 105,
        "weathered_cut_copper_stairs" => 106,
        "oxidized_cut_copper_stairs" => 107,
        "cut_copper_slab" => 108,
        "exposed_cut_copper_slab" => 109,
        "weathered_cut_copper_slab" => 110,
        "oxidized_cut_copper_slab" => 111,
        "waxed_copper_block" => 112,
        "waxed_exposed_copper" => 113,
        "waxed_weathered_copper" => 114,
        "waxed_oxidized_copper" => 115,
        "waxed_chiseled_copper" => 116,
        "waxed_exposed_chiseled_copper" => 117,
        "waxed_weathered_chiseled_copper" => 118,
        "waxed_oxidized_chiseled_copper" => 119,
        "waxed_cut_copper" => 120,
        "waxed_exposed_cut_copper" => 121,
        "waxed_weathered_cut_copper" => 122,
        "waxed_oxidized_cut_copper" => 123,
        "waxed_cut_copper_stairs" => 124,
        "waxed_exposed_cut_copper_stairs" => 125,
        "waxed_weathered_cut_copper_stairs" => 126,
        "waxed_oxidized_cut_copper_stairs" => 127,
        "waxed_cut_copper_slab" => 128,
        "waxed_exposed_cut_copper_slab" => 129,
        "waxed_weathered_cut_copper_slab" => 130,
        "waxed_oxidized_cut_copper_slab" => 131,
        "oak_log" => 132,
        "spruce_log" => 133,
        "birch_log" => 134,
        "jungle_log" => 135,
        "acacia_log" => 136,
        "cherry_log" => 137,
        "dark_oak_log" => 138,
        "mangrove_log" => 139,
        "mangrove_roots" => 140,
        "muddy_mangrove_roots" => 141,
        "crimson_stem" => 142,
        "warped_stem" => 143,
        "bamboo_block" => 144,
        "stripped_oak_log" => 145,
        "stripped_spruce_log" => 146,
        "stripped_birch_log" => 147,
        "stripped_jungle_log" => 148,
        "stripped_acacia_log" => 149,
        "stripped_cherry_log" => 150,
        "stripped_dark_oak_log" => 151,
        "stripped_mangrove_log" => 152,
        "stripped_crimson_stem" => 153,
        "stripped_warped_stem" => 154,
        "stripped_oak_wood" => 155,
        "stripped_spruce_wood" => 156,
        "stripped_birch_wood" => 157,
        "stripped_jungle_wood" => 158,
        "stripped_acacia_wood" => 159,
        "stripped_cherry_wood" => 160,
        "stripped_dark_oak_wood" => 161,
        "stripped_mangrove_wood" => 162,
        "stripped_crimson_hyphae" => 163,
        "stripped_warped_hyphae" => 164,
        "stripped_bamboo_block" => 165,
        "oak_wood" => 166,
        "spruce_wood" => 167,
        "birch_wood" => 168,
        "jungle_wood" => 169,
        "acacia_wood" => 170,
        "cherry_wood" => 171,
        "dark_oak_wood" => 172,
        "mangrove_wood" => 173,
        "crimson_hyphae" => 174,
        "warped_hyphae" => 175,
        "oak_leaves" => 176,
        "spruce_leaves" => 177,
        "birch_leaves" => 178,
        "jungle_leaves" => 179,
        "acacia_leaves" => 180,
        "cherry_leaves" => 181,
        "dark_oak_leaves" => 182,
        "mangrove_leaves" => 183,
        "azalea_leaves" => 184,
        "flowering_azalea_leaves" => 185,
        "sponge" => 186,
        "wet_sponge" => 187,
        "glass" => 188,
        "tinted_glass" => 189,
        "lapis_block" => 190,
        "sandstone" => 191,
        "chiseled_sandstone" => 192,
        "cut_sandstone" => 193,
        "cobweb" => 194,
        "short_grass" => 195,
        "fern" => 196,
        "azalea" => 197,
        "flowering_azalea" => 198,
        "dead_bush" => 199,
        "seagrass" => 200,
        "sea_pickle" => 201,
        "white_wool" => 202,
        "orange_wool" => 203,
        "magenta_wool" => 204,
        "light_blue_wool" => 205,
        "yellow_wool" => 206,
        "lime_wool" => 207,
        "pink_wool" => 208,
        "gray_wool" => 209,
        "light_gray_wool" => 210,
        "cyan_wool" => 211,
        "purple_wool" => 212,
        "blue_wool" => 213,
        "brown_wool" => 214,
        "green_wool" => 215,
        "red_wool" => 216,
        "black_wool" => 217,
        "dandelion" => 218,
        "poppy" => 219,
        "blue_orchid" => 220,
        "allium" => 221,
        "azure_bluet" => 222,
        "red_tulip" => 223,
        "orange_tulip" => 224,
        "white_tulip" => 225,
        "pink_tulip" => 226,
        "oxeye_daisy" => 227,
        "cornflower" => 228,
        "lily_of_the_valley" => 229,
        "wither_rose" => 230,
        "torchflower" => 231,
        "pitcher_plant" => 232,
        "spore_blossom" => 233,
        "brown_mushroom" => 234,
        "red_mushroom" => 235,
        "crimson_fungus" => 236,
        "warped_fungus" => 237,
        "crimson_roots" => 238,
        "warped_roots" => 239,
        "nether_sprouts" => 240,
        "weeping_vines" => 241,
        "twisting_vines" => 242,
        "sugar_cane" => 243,
        "kelp" => 244,
        "moss_carpet" => 245,
        "pink_petals" => 246,
        "moss_block" => 247,
        "hanging_roots" => 248,
        "big_dripleaf" => 249,
        "small_dripleaf" => 250,
        "bamboo" => 251,
        "oak_slab" => 252,
        "spruce_slab" => 253,
        "birch_slab" => 254,
        "jungle_slab" => 255,
        "acacia_slab" => 256,
        "cherry_slab" => 257,
        "dark_oak_slab" => 258,
        "mangrove_slab" => 259,
        "bamboo_slab" => 260,
        "bamboo_mosaic_slab" => 261,
        "crimson_slab" => 262,
        "warped_slab" => 263,
        "stone_slab" => 264,
        "smooth_stone_slab" => 265,
        "sandstone_slab" => 266,
        "cut_sandstone_slab" => 267,
        "petrified_oak_slab" => 268,
        "cobblestone_slab" => 269,
        "brick_slab" => 270,
        "stone_brick_slab" => 271,
        "mud_brick_slab" => 272,
        "nether_brick_slab" => 273,
        "quartz_slab" => 274,
        "red_sandstone_slab" => 275,
        "cut_red_sandstone_slab" => 276,
        "purpur_slab" => 277,
        "prismarine_slab" => 278,
        "prismarine_brick_slab" => 279,
        "dark_prismarine_slab" => 280,
        "smooth_quartz" => 281,
        "smooth_red_sandstone" => 282,
        "smooth_sandstone" => 283,
        "smooth_stone" => 284,
        "bricks" => 285,
        "bookshelf" => 286,
        "chiseled_bookshelf" => 287,
        "decorated_pot" => 288,
        "mossy_cobblestone" => 289,
        "obsidian" => 290,
        "torch" => 291,
        "end_rod" => 292,
        "chorus_plant" => 293,
        "chorus_flower" => 294,
        "purpur_block" => 295,
        "purpur_pillar" => 296,
        "purpur_stairs" => 297,
        "spawner" => 298,
        "chest" => 299,
        "crafting_table" => 300,
        "farmland" => 301,
        "furnace" => 302,
        "ladder" => 303,
        "cobblestone_stairs" => 304,
        "snow" => 305,
        "ice" => 306,
        "snow_block" => 307,
        "cactus" => 308,
        "clay" => 309,
        "jukebox" => 310,
        "oak_fence" => 311,
        "spruce_fence" => 312,
        "birch_fence" => 313,
        "jungle_fence" => 314,
        "acacia_fence" => 315,
        "cherry_fence" => 316,
        "dark_oak_fence" => 317,
        "mangrove_fence" => 318,
        "bamboo_fence" => 319,
        "crimson_fence" => 320,
        "warped_fence" => 321,
        "pumpkin" => 322,
        "carved_pumpkin" => 323,
        "jack_o_lantern" => 324,
        "netherrack" => 325,
        "soul_sand" => 326,
        "soul_soil" => 327,
        "basalt" => 328,
        "polished_basalt" => 329,
        "smooth_basalt" => 330,
        "soul_torch" => 331,
        "glowstone" => 332,
        "infested_stone" => 333,
        "infested_cobblestone" => 334,
        "infested_stone_bricks" => 335,
        "infested_mossy_stone_bricks" => 336,
        "infested_cracked_stone_bricks" => 337,
        "infested_chiseled_stone_bricks" => 338,
        "infested_deepslate" => 339,
        "stone_bricks" => 340,
        "mossy_stone_bricks" => 341,
        "cracked_stone_bricks" => 342,
        "chiseled_stone_bricks" => 343,
        "packed_mud" => 344,
        "mud_bricks" => 345,
        "deepslate_bricks" => 346,
        "cracked_deepslate_bricks" => 347,
        "deepslate_tiles" => 348,
        "cracked_deepslate_tiles" => 349,
        "chiseled_deepslate" => 350,
        "reinforced_deepslate" => 351,
        "brown_mushroom_block" => 352,
        "red_mushroom_block" => 353,
        "mushroom_stem" => 354,
        "iron_bars" => 355,
        "chain" => 356,
        "glass_pane" => 357,
        "melon" => 358,
        "vine" => 359,
        "glow_lichen" => 360,
        "brick_stairs" => 361,
        "stone_brick_stairs" => 362,
        "mud_brick_stairs" => 363,
        "mycelium" => 364,
        "lily_pad" => 365,
        "nether_bricks" => 366,
        "cracked_nether_bricks" => 367,
        "chiseled_nether_bricks" => 368,
        "nether_brick_fence" => 369,
        "nether_brick_stairs" => 370,
        "sculk" => 371,
        "sculk_vein" => 372,
        "sculk_catalyst" => 373,
        "sculk_shrieker" => 374,
        "enchanting_table" => 375,
        "end_portal_frame" => 376,
        "end_stone" => 377,
        "end_stone_bricks" => 378,
        "dragon_egg" => 379,
        "sandstone_stairs" => 380,
        "ender_chest" => 381,
        "emerald_block" => 382,
        "oak_stairs" => 383,
        "spruce_stairs" => 384,
        "birch_stairs" => 385,
        "jungle_stairs" => 386,
        "acacia_stairs" => 387,
        "cherry_stairs" => 388,
        "dark_oak_stairs" => 389,
        "mangrove_stairs" => 390,
        "bamboo_stairs" => 391,
        "bamboo_mosaic_stairs" => 392,
        "crimson_stairs" => 393,
        "warped_stairs" => 394,
        "command_block" => 395,
        "beacon" => 396,
        "cobblestone_wall" => 397,
        "mossy_cobblestone_wall" => 398,
        "brick_wall" => 399,
        "prismarine_wall" => 400,
        "red_sandstone_wall" => 401,
        "mossy_stone_brick_wall" => 402,
        "granite_wall" => 403,
        "stone_brick_wall" => 404,
        "mud_brick_wall" => 405,
        "nether_brick_wall" => 406,
        "andesite_wall" => 407,
        "red_nether_brick_wall" => 408,
        "sandstone_wall" => 409,
        "end_stone_brick_wall" => 410,
        "diorite_wall" => 411,
        "blackstone_wall" => 412,
        "polished_blackstone_wall" => 413,
        "polished_blackstone_brick_wall" => 414,
        "cobbled_deepslate_wall" => 415,
        "polished_deepslate_wall" => 416,
        "deepslate_brick_wall" => 417,
        "deepslate_tile_wall" => 418,
        "anvil" => 419,
        "chipped_anvil" => 420,
        "damaged_anvil" => 421,
        "chiseled_quartz_block" => 422,
        "quartz_block" => 423,
        "quartz_bricks" => 424,
        "quartz_pillar" => 425,
        "quartz_stairs" => 426,
        "white_terracotta" => 427,
        "orange_terracotta" => 428,
        "magenta_terracotta" => 429,
        "light_blue_terracotta" => 430,
        "yellow_terracotta" => 431,
        "lime_terracotta" => 432,
        "pink_terracotta" => 433,
        "gray_terracotta" => 434,
        "light_gray_terracotta" => 435,
        "cyan_terracotta" => 436,
        "purple_terracotta" => 437,
        "blue_terracotta" => 438,
        "brown_terracotta" => 439,
        "green_terracotta" => 440,
        "red_terracotta" => 441,
        "black_terracotta" => 442,
        "barrier" => 443,
        "light" => 444,
        "hay_block" => 445,
        "white_carpet" => 446,
        "orange_carpet" => 447,
        "magenta_carpet" => 448,
        "light_blue_carpet" => 449,
        "yellow_carpet" => 450,
        "lime_carpet" => 451,
        "pink_carpet" => 452,
        "gray_carpet" => 453,
        "light_gray_carpet" => 454,
        "cyan_carpet" => 455,
        "purple_carpet" => 456,
        "blue_carpet" => 457,
        "brown_carpet" => 458,
        "green_carpet" => 459,
        "red_carpet" => 460,
        "black_carpet" => 461,
        "terracotta" => 462,
        "packed_ice" => 463,
        "dirt_path" => 464,
        "sunflower" => 465,
        "lilac" => 466,
        "rose_bush" => 467,
        "peony" => 468,
        "tall_grass" => 469,
        "large_fern" => 470,
        "white_stained_glass" => 471,
        "orange_stained_glass" => 472,
        "magenta_stained_glass" => 473,
        "light_blue_stained_glass" => 474,
        "yellow_stained_glass" => 475,
        "lime_stained_glass" => 476,
        "pink_stained_glass" => 477,
        "gray_stained_glass" => 478,
        "light_gray_stained_glass" => 479,
        "cyan_stained_glass" => 480,
        "purple_stained_glass" => 481,
        "blue_stained_glass" => 482,
        "brown_stained_glass" => 483,
        "green_stained_glass" => 484,
        "red_stained_glass" => 485,
        "black_stained_glass" => 486,
        "white_stained_glass_pane" => 487,
        "orange_stained_glass_pane" => 488,
        "magenta_stained_glass_pane" => 489,
        "light_blue_stained_glass_pane" => 490,
        "yellow_stained_glass_pane" => 491,
        "lime_stained_glass_pane" => 492,
        "pink_stained_glass_pane" => 493,
        "gray_stained_glass_pane" => 494,
        "light_gray_stained_glass_pane" => 495,
        "cyan_stained_glass_pane" => 496,
        "purple_stained_glass_pane" => 497,
        "blue_stained_glass_pane" => 498,
        "brown_stained_glass_pane" => 499,
        "green_stained_glass_pane" => 500,
        "red_stained_glass_pane" => 501,
        "black_stained_glass_pane" => 502,
        "prismarine" => 503,
        "prismarine_bricks" => 504,
        "dark_prismarine" => 505,
        "prismarine_stairs" => 506,
        "prismarine_brick_stairs" => 507,
        "dark_prismarine_stairs" => 508,
        "sea_lantern" => 509,
        "red_sandstone" => 510,
        "chiseled_red_sandstone" => 511,
        "cut_red_sandstone" => 512,
        "red_sandstone_stairs" => 513,
        "repeating_command_block" => 514,
        "chain_command_block" => 515,
        "magma_block" => 516,
        "nether_wart_block" => 517,
        "warped_wart_block" => 518,
        "red_nether_bricks" => 519,
        "bone_block" => 520,
        "structure_void" => 521,
        "shulker_box" => 522,
        "white_shulker_box" => 523,
        "orange_shulker_box" => 524,
        "magenta_shulker_box" => 525,
        "light_blue_shulker_box" => 526,
        "yellow_shulker_box" => 527,
        "lime_shulker_box" => 528,
        "pink_shulker_box" => 529,
        "gray_shulker_box" => 530,
        "light_gray_shulker_box" => 531,
        "cyan_shulker_box" => 532,
        "purple_shulker_box" => 533,
        "blue_shulker_box" => 534,
        "brown_shulker_box" => 535,
        "green_shulker_box" => 536,
        "red_shulker_box" => 537,
        "black_shulker_box" => 538,
        "white_glazed_terracotta" => 539,
        "orange_glazed_terracotta" => 540,
        "magenta_glazed_terracotta" => 541,
        "light_blue_glazed_terracotta" => 542,
        "yellow_glazed_terracotta" => 543,
        "lime_glazed_terracotta" => 544,
        "pink_glazed_terracotta" => 545,
        "gray_glazed_terracotta" => 546,
        "light_gray_glazed_terracotta" => 547,
        "cyan_glazed_terracotta" => 548,
        "purple_glazed_terracotta" => 549,
        "blue_glazed_terracotta" => 550,
        "brown_glazed_terracotta" => 551,
        "green_glazed_terracotta" => 552,
        "red_glazed_terracotta" => 553,
        "black_glazed_terracotta" => 554,
        "white_concrete" => 555,
        "orange_concrete" => 556,
        "magenta_concrete" => 557,
        "light_blue_concrete" => 558,
        "yellow_concrete" => 559,
        "lime_concrete" => 560,
        "pink_concrete" => 561,
        "gray_concrete" => 562,
        "light_gray_concrete" => 563,
        "cyan_concrete" => 564,
        "purple_concrete" => 565,
        "blue_concrete" => 566,
        "brown_concrete" => 567,
        "green_concrete" => 568,
        "red_concrete" => 569,
        "black_concrete" => 570,
        "white_concrete_powder" => 571,
        "orange_concrete_powder" => 572,
        "magenta_concrete_powder" => 573,
        "light_blue_concrete_powder" => 574,
        "yellow_concrete_powder" => 575,
        "lime_concrete_powder" => 576,
        "pink_concrete_powder" => 577,
        "gray_concrete_powder" => 578,
        "light_gray_concrete_powder" => 579,
        "cyan_concrete_powder" => 580,
        "purple_concrete_powder" => 581,
        "blue_concrete_powder" => 582,
        "brown_concrete_powder" => 583,
        "green_concrete_powder" => 584,
        "red_concrete_powder" => 585,
        "black_concrete_powder" => 586,
        "turtle_egg" => 587,
        "sniffer_egg" => 588,
        "dead_tube_coral_block" => 589,
        "dead_brain_coral_block" => 590,
        "dead_bubble_coral_block" => 591,
        "dead_fire_coral_block" => 592,
        "dead_horn_coral_block" => 593,
        "tube_coral_block" => 594,
        "brain_coral_block" => 595,
        "bubble_coral_block" => 596,
        "fire_coral_block" => 597,
        "horn_coral_block" => 598,
        "tube_coral" => 599,
        "brain_coral" => 600,
        "bubble_coral" => 601,
        "fire_coral" => 602,
        "horn_coral" => 603,
        "dead_brain_coral" => 604,
        "dead_bubble_coral" => 605,
        "dead_fire_coral" => 606,
        "dead_horn_coral" => 607,
        "dead_tube_coral" => 608,
        "tube_coral_fan" => 609,
        "brain_coral_fan" => 610,
        "bubble_coral_fan" => 611,
        "fire_coral_fan" => 612,
        "horn_coral_fan" => 613,
        "dead_tube_coral_fan" => 614,
        "dead_brain_coral_fan" => 615,
        "dead_bubble_coral_fan" => 616,
        "dead_fire_coral_fan" => 617,
        "dead_horn_coral_fan" => 618,
        "blue_ice" => 619,
        "conduit" => 620,
        "polished_granite_stairs" => 621,
        "smooth_red_sandstone_stairs" => 622,
        "mossy_stone_brick_stairs" => 623,
        "polished_diorite_stairs" => 624,
        "mossy_cobblestone_stairs" => 625,
        "end_stone_brick_stairs" => 626,
        "stone_stairs" => 627,
        "smooth_sandstone_stairs" => 628,
        "smooth_quartz_stairs" => 629,
        "granite_stairs" => 630,
        "andesite_stairs" => 631,
        "red_nether_brick_stairs" => 632,
        "polished_andesite_stairs" => 633,
        "diorite_stairs" => 634,
        "cobbled_deepslate_stairs" => 635,
        "polished_deepslate_stairs" => 636,
        "deepslate_brick_stairs" => 637,
        "deepslate_tile_stairs" => 638,
        "polished_granite_slab" => 639,
        "smooth_red_sandstone_slab" => 640,
        "mossy_stone_brick_slab" => 641,
        "polished_diorite_slab" => 642,
        "mossy_cobblestone_slab" => 643,
        "end_stone_brick_slab" => 644,
        "smooth_sandstone_slab" => 645,
        "smooth_quartz_slab" => 646,
        "granite_slab" => 647,
        "andesite_slab" => 648,
        "red_nether_brick_slab" => 649,
        "polished_andesite_slab" => 650,
        "diorite_slab" => 651,
        "cobbled_deepslate_slab" => 652,
        "polished_deepslate_slab" => 653,
        "deepslate_brick_slab" => 654,
        "deepslate_tile_slab" => 655,
        "scaffolding" => 656,
        "redstone" => 657,
        "redstone_torch" => 658,
        "redstone_block" => 659,
        "repeater" => 660,
        "comparator" => 661,
        "piston" => 662,
        "sticky_piston" => 663,
        "slime_block" => 664,
        "honey_block" => 665,
        "observer" => 666,
        "hopper" => 667,
        "dispenser" => 668,
        "dropper" => 669,
        "lectern" => 670,
        "target" => 671,
        "lever" => 672,
        "lightning_rod" => 673,
        "daylight_detector" => 674,
        "sculk_sensor" => 675,
        "calibrated_sculk_sensor" => 676,
        "tripwire_hook" => 677,
        "trapped_chest" => 678,
        "tnt" => 679,
        "redstone_lamp" => 680,
        "note_block" => 681,
        "stone_button" => 682,
        "polished_blackstone_button" => 683,
        "oak_button" => 684,
        "spruce_button" => 685,
        "birch_button" => 686,
        "jungle_button" => 687,
        "acacia_button" => 688,
        "cherry_button" => 689,
        "dark_oak_button" => 690,
        "mangrove_button" => 691,
        "bamboo_button" => 692,
        "crimson_button" => 693,
        "warped_button" => 694,
        "stone_pressure_plate" => 695,
        "polished_blackstone_pressure_plate" => 696,
        "light_weighted_pressure_plate" => 697,
        "heavy_weighted_pressure_plate" => 698,
        "oak_pressure_plate" => 699,
        "spruce_pressure_plate" => 700,
        "birch_pressure_plate" => 701,
        "jungle_pressure_plate" => 702,
        "acacia_pressure_plate" => 703,
        "cherry_pressure_plate" => 704,
        "dark_oak_pressure_plate" => 705,
        "mangrove_pressure_plate" => 706,
        "bamboo_pressure_plate" => 707,
        "crimson_pressure_plate" => 708,
        "warped_pressure_plate" => 709,
        "iron_door" => 710,
        "oak_door" => 711,
        "spruce_door" => 712,
        "birch_door" => 713,
        "jungle_door" => 714,
        "acacia_door" => 715,
        "cherry_door" => 716,
        "dark_oak_door" => 717,
        "mangrove_door" => 718,
        "bamboo_door" => 719,
        "crimson_door" => 720,
        "warped_door" => 721,
        "copper_door" => 722,
        "exposed_copper_door" => 723,
        "weathered_copper_door" => 724,
        "oxidized_copper_door" => 725,
        "waxed_copper_door" => 726,
        "waxed_exposed_copper_door" => 727,
        "waxed_weathered_copper_door" => 728,
        "waxed_oxidized_copper_door" => 729,
        "iron_trapdoor" => 730,
        "oak_trapdoor" => 731,
        "spruce_trapdoor" => 732,
        "birch_trapdoor" => 733,
        "jungle_trapdoor" => 734,
        "acacia_trapdoor" => 735,
        "cherry_trapdoor" => 736,
        "dark_oak_trapdoor" => 737,
        "mangrove_trapdoor" => 738,
        "bamboo_trapdoor" => 739,
        "crimson_trapdoor" => 740,
        "warped_trapdoor" => 741,
        "copper_trapdoor" => 742,
        "exposed_copper_trapdoor" => 743,
        "weathered_copper_trapdoor" => 744,
        "oxidized_copper_trapdoor" => 745,
        "waxed_copper_trapdoor" => 746,
        "waxed_exposed_copper_trapdoor" => 747,
        "waxed_weathered_copper_trapdoor" => 748,
        "waxed_oxidized_copper_trapdoor" => 749,
        "oak_fence_gate" => 750,
        "spruce_fence_gate" => 751,
        "birch_fence_gate" => 752,
        "jungle_fence_gate" => 753,
        "acacia_fence_gate" => 754,
        "cherry_fence_gate" => 755,
        "dark_oak_fence_gate" => 756,
        "mangrove_fence_gate" => 757,
        "bamboo_fence_gate" => 758,
        "crimson_fence_gate" => 759,
        "warped_fence_gate" => 760,
        "powered_rail" => 761,
        "detector_rail" => 762,
        "rail" => 763,
        "activator_rail" => 764,
        "saddle" => 765,
        "minecart" => 766,
        "chest_minecart" => 767,
        "furnace_minecart" => 768,
        "tnt_minecart" => 769,
        "hopper_minecart" => 770,
        "carrot_on_a_stick" => 771,
        "warped_fungus_on_a_stick" => 772,
        "elytra" => 773,
        "oak_boat" => 774,
        "oak_chest_boat" => 775,
        "spruce_boat" => 776,
        "spruce_chest_boat" => 777,
        "birch_boat" => 778,
        "birch_chest_boat" => 779,
        "jungle_boat" => 780,
        "jungle_chest_boat" => 781,
        "acacia_boat" => 782,
        "acacia_chest_boat" => 783,
        "cherry_boat" => 784,
        "cherry_chest_boat" => 785,
        "dark_oak_boat" => 786,
        "dark_oak_chest_boat" => 787,
        "mangrove_boat" => 788,
        "mangrove_chest_boat" => 789,
        "bamboo_raft" => 790,
        "bamboo_chest_raft" => 791,
        "structure_block" => 792,
        "jigsaw" => 793,
        "turtle_helmet" => 794,
        "turtle_scute" => 795,
        "armadillo_scute" => 796,
        "wolf_armor" => 797,
        "flint_and_steel" => 798,
        "bowl" => 799,
        "apple" => 800,
        "bow" => 801,
        "arrow" => 802,
        "coal" => 803,
        "charcoal" => 804,
        "diamond" => 805,
        "emerald" => 806,
        "lapis_lazuli" => 807,
        "quartz" => 808,
        "amethyst_shard" => 809,
        "raw_iron" => 810,
        "iron_ingot" => 811,
        "raw_copper" => 812,
        "copper_ingot" => 813,
        "raw_gold" => 814,
        "gold_ingot" => 815,
        "netherite_ingot" => 816,
        "netherite_scrap" => 817,
        "wooden_sword" => 818,
        "wooden_shovel" => 819,
        "wooden_pickaxe" => 820,
        "wooden_axe" => 821,
        "wooden_hoe" => 822,
        "stone_sword" => 823,
        "stone_shovel" => 824,
        "stone_pickaxe" => 825,
        "stone_axe" => 826,
        "stone_hoe" => 827,
        "golden_sword" => 828,
        "golden_shovel" => 829,
        "golden_pickaxe" => 830,
        "golden_axe" => 831,
        "golden_hoe" => 832,
        "iron_sword" => 833,
        "iron_shovel" => 834,
        "iron_pickaxe" => 835,
        "iron_axe" => 836,
        "iron_hoe" => 837,
        "diamond_sword" => 838,
        "diamond_shovel" => 839,
        "diamond_pickaxe" => 840,
        "diamond_axe" => 841,
        "diamond_hoe" => 842,
        "netherite_sword" => 843,
        "netherite_shovel" => 844,
        "netherite_pickaxe" => 845,
        "netherite_axe" => 846,
        "netherite_hoe" => 847,
        "stick" => 848,
        "mushroom_stew" => 849,
        "string" => 850,
        "feather" => 851,
        "gunpowder" => 852,
        "wheat_seeds" => 853,
        "wheat" => 854,
        "bread" => 855,
        "leather_helmet" => 856,
        "leather_chestplate" => 857,
        "leather_leggings" => 858,
        "leather_boots" => 859,
        "chainmail_helmet" => 860,
        "chainmail_chestplate" => 861,
        "chainmail_leggings" => 862,
        "chainmail_boots" => 863,
        "iron_helmet" => 864,
        "iron_chestplate" => 865,
        "iron_leggings" => 866,
        "iron_boots" => 867,
        "diamond_helmet" => 868,
        "diamond_chestplate" => 869,
        "diamond_leggings" => 870,
        "diamond_boots" => 871,
        "golden_helmet" => 872,
        "golden_chestplate" => 873,
        "golden_leggings" => 874,
        "golden_boots" => 875,
        "netherite_helmet" => 876,
        "netherite_chestplate" => 877,
        "netherite_leggings" => 878,
        "netherite_boots" => 879,
        "flint" => 880,
        "porkchop" => 881,
        "cooked_porkchop" => 882,
        "painting" => 883,
        "golden_apple" => 884,
        "enchanted_golden_apple" => 885,
        "oak_sign" => 886,
        "spruce_sign" => 887,
        "birch_sign" => 888,
        "jungle_sign" => 889,
        "acacia_sign" => 890,
        "cherry_sign" => 891,
        "dark_oak_sign" => 892,
        "mangrove_sign" => 893,
        "bamboo_sign" => 894,
        "crimson_sign" => 895,
        "warped_sign" => 896,
        "oak_hanging_sign" => 897,
        "spruce_hanging_sign" => 898,
        "birch_hanging_sign" => 899,
        "jungle_hanging_sign" => 900,
        "acacia_hanging_sign" => 901,
        "cherry_hanging_sign" => 902,
        "dark_oak_hanging_sign" => 903,
        "mangrove_hanging_sign" => 904,
        "bamboo_hanging_sign" => 905,
        "crimson_hanging_sign" => 906,
        "warped_hanging_sign" => 907,
        "bucket" => 908,
        "water_bucket" => 909,
        "lava_bucket" => 910,
        "powder_snow_bucket" => 911,
        "snowball" => 912,
        "leather" => 913,
        "milk_bucket" => 914,
        "pufferfish_bucket" => 915,
        "salmon_bucket" => 916,
        "cod_bucket" => 917,
        "tropical_fish_bucket" => 918,
        "axolotl_bucket" => 919,
        "tadpole_bucket" => 920,
        "brick" => 921,
        "clay_ball" => 922,
        "dried_kelp_block" => 923,
        "paper" => 924,
        "book" => 925,
        "slime_ball" => 926,
        "egg" => 927,
        "compass" => 928,
        "recovery_compass" => 929,
        "bundle" => 930,
        "fishing_rod" => 931,
        "clock" => 932,
        "spyglass" => 933,
        "glowstone_dust" => 934,
        "cod" => 935,
        "salmon" => 936,
        "tropical_fish" => 937,
        "pufferfish" => 938,
        "cooked_cod" => 939,
        "cooked_salmon" => 940,
        "ink_sac" => 941,
        "glow_ink_sac" => 942,
        "cocoa_beans" => 943,
        "white_dye" => 944,
        "orange_dye" => 945,
        "magenta_dye" => 946,
        "light_blue_dye" => 947,
        "yellow_dye" => 948,
        "lime_dye" => 949,
        "pink_dye" => 950,
        "gray_dye" => 951,
        "light_gray_dye" => 952,
        "cyan_dye" => 953,
        "purple_dye" => 954,
        "blue_dye" => 955,
        "brown_dye" => 956,
        "green_dye" => 957,
        "red_dye" => 958,
        "black_dye" => 959,
        "bone_meal" => 960,
        "bone" => 961,
        "sugar" => 962,
        "cake" => 963,
        "white_bed" => 964,
        "orange_bed" => 965,
        "magenta_bed" => 966,
        "light_blue_bed" => 967,
        "yellow_bed" => 968,
        "lime_bed" => 969,
        "pink_bed" => 970,
        "gray_bed" => 971,
        "light_gray_bed" => 972,
        "cyan_bed" => 973,
        "purple_bed" => 974,
        "blue_bed" => 975,
        "brown_bed" => 976,
        "green_bed" => 977,
        "red_bed" => 978,
        "black_bed" => 979,
        "cookie" => 980,
        "crafter" => 981,
        "filled_map" => 982,
        "shears" => 983,
        "melon_slice" => 984,
        "dried_kelp" => 985,
        "pumpkin_seeds" => 986,
        "melon_seeds" => 987,
        "beef" => 988,
        "cooked_beef" => 989,
        "chicken" => 990,
        "cooked_chicken" => 991,
        "rotten_flesh" => 992,
        "ender_pearl" => 993,
        "blaze_rod" => 994,
        "ghast_tear" => 995,
        "gold_nugget" => 996,
        "nether_wart" => 997,
        "potion" => 998,
        "glass_bottle" => 999,
        "spider_eye" => 1000,
        "fermented_spider_eye" => 1001,
        "blaze_powder" => 1002,
        "magma_cream" => 1003,
        "brewing_stand" => 1004,
        "cauldron" => 1005,
        "ender_eye" => 1006,
        "glistering_melon_slice" => 1007,
        "armadillo_spawn_egg" => 1008,
        "allay_spawn_egg" => 1009,
        "axolotl_spawn_egg" => 1010,
        "bat_spawn_egg" => 1011,
        "bee_spawn_egg" => 1012,
        "blaze_spawn_egg" => 1013,
        "bogged_spawn_egg" => 1014,
        "breeze_spawn_egg" => 1015,
        "cat_spawn_egg" => 1016,
        "camel_spawn_egg" => 1017,
        "cave_spider_spawn_egg" => 1018,
        "chicken_spawn_egg" => 1019,
        "cod_spawn_egg" => 1020,
        "cow_spawn_egg" => 1021,
        "creeper_spawn_egg" => 1022,
        "dolphin_spawn_egg" => 1023,
        "donkey_spawn_egg" => 1024,
        "drowned_spawn_egg" => 1025,
        "elder_guardian_spawn_egg" => 1026,
        "ender_dragon_spawn_egg" => 1027,
        "enderman_spawn_egg" => 1028,
        "endermite_spawn_egg" => 1029,
        "evoker_spawn_egg" => 1030,
        "fox_spawn_egg" => 1031,
        "frog_spawn_egg" => 1032,
        "ghast_spawn_egg" => 1033,
        "glow_squid_spawn_egg" => 1034,
        "goat_spawn_egg" => 1035,
        "guardian_spawn_egg" => 1036,
        "hoglin_spawn_egg" => 1037,
        "horse_spawn_egg" => 1038,
        "husk_spawn_egg" => 1039,
        "iron_golem_spawn_egg" => 1040,
        "llama_spawn_egg" => 1041,
        "magma_cube_spawn_egg" => 1042,
        "mooshroom_spawn_egg" => 1043,
        "mule_spawn_egg" => 1044,
        "ocelot_spawn_egg" => 1045,
        "panda_spawn_egg" => 1046,
        "parrot_spawn_egg" => 1047,
        "phantom_spawn_egg" => 1048,
        "pig_spawn_egg" => 1049,
        "piglin_spawn_egg" => 1050,
        "piglin_brute_spawn_egg" => 1051,
        "pillager_spawn_egg" => 1052,
        "polar_bear_spawn_egg" => 1053,
        "pufferfish_spawn_egg" => 1054,
        "rabbit_spawn_egg" => 1055,
        "ravager_spawn_egg" => 1056,
        "salmon_spawn_egg" => 1057,
        "sheep_spawn_egg" => 1058,
        "shulker_spawn_egg" => 1059,
        "silverfish_spawn_egg" => 1060,
        "skeleton_spawn_egg" => 1061,
        "skeleton_horse_spawn_egg" => 1062,
        "slime_spawn_egg" => 1063,
        "sniffer_spawn_egg" => 1064,
        "snow_golem_spawn_egg" => 1065,
        "spider_spawn_egg" => 1066,
        "squid_spawn_egg" => 1067,
        "stray_spawn_egg" => 1068,
        "strider_spawn_egg" => 1069,
        "tadpole_spawn_egg" => 1070,
        "trader_llama_spawn_egg" => 1071,
        "tropical_fish_spawn_egg" => 1072,
        "turtle_spawn_egg" => 1073,
        "vex_spawn_egg" => 1074,
        "villager_spawn_egg" => 1075,
        "vindicator_spawn_egg" => 1076,
        "wandering_trader_spawn_egg" => 1077,
        "warden_spawn_egg" => 1078,
        "witch_spawn_egg" => 1079,
        "wither_spawn_egg" => 1080,
        "wither_skeleton_spawn_egg" => 1081,
        "wolf_spawn_egg" => 1082,
        "zoglin_spawn_egg" => 1083,
        "zombie_spawn_egg" => 1084,
        "zombie_horse_spawn_egg" => 1085,
        "zombie_villager_spawn_egg" => 1086,
        "zombified_piglin_spawn_egg" => 1087,
        "experience_bottle" => 1088,
        "fire_charge" => 1089,
        "wind_charge" => 1090,
        "writable_book" => 1091,
        "written_book" => 1092,
        "mace" => 1093,
        "item_frame" => 1094,
        "glow_item_frame" => 1095,
        "flower_pot" => 1096,
        "carrot" => 1097,
        "potato" => 1098,
        "baked_potato" => 1099,
        "poisonous_potato" => 1100,
        "map" => 1101,
        "golden_carrot" => 1102,
        "skeleton_skull" => 1103,
        "wither_skeleton_skull" => 1104,
        "player_head" => 1105,
        "zombie_head" => 1106,
        "creeper_head" => 1107,
        "dragon_head" => 1108,
        "piglin_head" => 1109,
        "nether_star" => 1110,
        "pumpkin_pie" => 1111,
        "firework_rocket" => 1112,
        "firework_star" => 1113,
        "enchanted_book" => 1114,
        "nether_brick" => 1115,
        "prismarine_shard" => 1116,
        "prismarine_crystals" => 1117,
        "rabbit" => 1118,
        "cooked_rabbit" => 1119,
        "rabbit_stew" => 1120,
        "rabbit_foot" => 1121,
        "rabbit_hide" => 1122,
        "armor_stand" => 1123,
        "iron_horse_armor" => 1124,
        "golden_horse_armor" => 1125,
        "diamond_horse_armor" => 1126,
        "leather_horse_armor" => 1127,
        "lead" => 1128,
        "name_tag" => 1129,
        "command_block_minecart" => 1130,
        "mutton" => 1131,
        "cooked_mutton" => 1132,
        "white_banner" => 1133,
        "orange_banner" => 1134,
        "magenta_banner" => 1135,
        "light_blue_banner" => 1136,
        "yellow_banner" => 1137,
        "lime_banner" => 1138,
        "pink_banner" => 1139,
        "gray_banner" => 1140,
        "light_gray_banner" => 1141,
        "cyan_banner" => 1142,
        "purple_banner" => 1143,
        "blue_banner" => 1144,
        "brown_banner" => 1145,
        "green_banner" => 1146,
        "red_banner" => 1147,
        "black_banner" => 1148,
        "end_crystal" => 1149,
        "chorus_fruit" => 1150,
        "popped_chorus_fruit" => 1151,
        "torchflower_seeds" => 1152,
        "pitcher_pod" => 1153,
        "beetroot" => 1154,
        "beetroot_seeds" => 1155,
        "beetroot_soup" => 1156,
        "dragon_breath" => 1157,
        "splash_potion" => 1158,
        "spectral_arrow" => 1159,
        "tipped_arrow" => 1160,
        "lingering_potion" => 1161,
        "shield" => 1162,
        "totem_of_undying" => 1163,
        "shulker_shell" => 1164,
        "iron_nugget" => 1165,
        "knowledge_book" => 1166,
        "debug_stick" => 1167,
        "music_disc_13" => 1168,
        "music_disc_cat" => 1169,
        "music_disc_blocks" => 1170,
        "music_disc_chirp" => 1171,
        "music_disc_creator" => 1172,
        "music_disc_creator_music_box" => 1173,
        "music_disc_far" => 1174,
        "music_disc_mall" => 1175,
        "music_disc_mellohi" => 1176,
        "music_disc_stal" => 1177,
        "music_disc_strad" => 1178,
        "music_disc_ward" => 1179,
        "music_disc_11" => 1180,
        "music_disc_wait" => 1181,
        "music_disc_otherside" => 1182,
        "music_disc_relic" => 1183,
        "music_disc_5" => 1184,
        "music_disc_pigstep" => 1185,
        "music_disc_precipice" => 1186,
        "disc_fragment_5" => 1187,
        "trident" => 1188,
        "phantom_membrane" => 1189,
        "nautilus_shell" => 1190,
        "heart_of_the_sea" => 1191,
        "crossbow" => 1192,
        "suspicious_stew" => 1193,
        "loom" => 1194,
        "flower_banner_pattern" => 1195,
        "creeper_banner_pattern" => 1196,
        "skull_banner_pattern" => 1197,
        "mojang_banner_pattern" => 1198,
        "globe_banner_pattern" => 1199,
        "piglin_banner_pattern" => 1200,
        "flow_banner_pattern" => 1201,
        "guster_banner_pattern" => 1202,
        "goat_horn" => 1203,
        "composter" => 1204,
        "barrel" => 1205,
        "smoker" => 1206,
        "blast_furnace" => 1207,
        "cartography_table" => 1208,
        "fletching_table" => 1209,
        "grindstone" => 1210,
        "smithing_table" => 1211,
        "stonecutter" => 1212,
        "bell" => 1213,
        "lantern" => 1214,
        "soul_lantern" => 1215,
        "sweet_berries" => 1216,
        "glow_berries" => 1217,
        "campfire" => 1218,
        "soul_campfire" => 1219,
        "shroomlight" => 1220,
        "honeycomb" => 1221,
        "bee_nest" => 1222,
        "beehive" => 1223,
        "honey_bottle" => 1224,
        "honeycomb_block" => 1225,
        "lodestone" => 1226,
        "crying_obsidian" => 1227,
        "blackstone" => 1228,
        "blackstone_slab" => 1229,
        "blackstone_stairs" => 1230,
        "gilded_blackstone" => 1231,
        "polished_blackstone" => 1232,
        "polished_blackstone_slab" => 1233,
        "polished_blackstone_stairs" => 1234,
        "chiseled_polished_blackstone" => 1235,
        "polished_blackstone_bricks" => 1236,
        "polished_blackstone_brick_slab" => 1237,
        "polished_blackstone_brick_stairs" => 1238,
        "cracked_polished_blackstone_bricks" => 1239,
        "respawn_anchor" => 1240,
        "candle" => 1241,
        "white_candle" => 1242,
        "orange_candle" => 1243,
        "magenta_candle" => 1244,
        "light_blue_candle" => 1245,
        "yellow_candle" => 1246,
        "lime_candle" => 1247,
        "pink_candle" => 1248,
        "gray_candle" => 1249,
        "light_gray_candle" => 1250,
        "cyan_candle" => 1251,
        "purple_candle" => 1252,
        "blue_candle" => 1253,
        "brown_candle" => 1254,
        "green_candle" => 1255,
        "red_candle" => 1256,
        "black_candle" => 1257,
        "small_amethyst_bud" => 1258,
        "medium_amethyst_bud" => 1259,
        "large_amethyst_bud" => 1260,
        "amethyst_cluster" => 1261,
        "pointed_dripstone" => 1262,
        "ochre_froglight" => 1263,
        "verdant_froglight" => 1264,
        "pearlescent_froglight" => 1265,
        "frogspawn" => 1266,
        "echo_shard" => 1267,
        "brush" => 1268,
        "netherite_upgrade_smithing_template" => 1269,
        "sentry_armor_trim_smithing_template" => 1270,
        "dune_armor_trim_smithing_template" => 1271,
        "coast_armor_trim_smithing_template" => 1272,
        "wild_armor_trim_smithing_template" => 1273,
        "ward_armor_trim_smithing_template" => 1274,
        "eye_armor_trim_smithing_template" => 1275,
        "vex_armor_trim_smithing_template" => 1276,
        "tide_armor_trim_smithing_template" => 1277,
        "snout_armor_trim_smithing_template" => 1278,
        "rib_armor_trim_smithing_template" => 1279,
        "spire_armor_trim_smithing_template" => 1280,
        "wayfinder_armor_trim_smithing_template" => 1281,
        "shaper_armor_trim_smithing_template" => 1282,
        "silence_armor_trim_smithing_template" => 1283,
        "raiser_armor_trim_smithing_template" => 1284,
        "host_armor_trim_smithing_template" => 1285,
        "flow_armor_trim_smithing_template" => 1286,
        "bolt_armor_trim_smithing_template" => 1287,
        "angler_pottery_sherd" => 1288,
        "archer_pottery_sherd" => 1289,
        "arms_up_pottery_sherd" => 1290,
        "blade_pottery_sherd" => 1291,
        "brewer_pottery_sherd" => 1292,
        "burn_pottery_sherd" => 1293,
        "danger_pottery_sherd" => 1294,
        "explorer_pottery_sherd" => 1295,
        "flow_pottery_sherd" => 1296,
        "friend_pottery_sherd" => 1297,
        "guster_pottery_sherd" => 1298,
        "heart_pottery_sherd" => 1299,
        "heartbreak_pottery_sherd" => 1300,
        "howl_pottery_sherd" => 1301,
        "miner_pottery_sherd" => 1302,
        "mourner_pottery_sherd" => 1303,
        "plenty_pottery_sherd" => 1304,
        "prize_pottery_sherd" => 1305,
        "scrape_pottery_sherd" => 1306,
        "sheaf_pottery_sherd" => 1307,
        "shelter_pottery_sherd" => 1308,
        "skull_pottery_sherd" => 1309,
        "snort_pottery_sherd" => 1310,
        "copper_grate" => 1311,
        "exposed_copper_grate" => 1312,
        "weathered_copper_grate" => 1313,
        "oxidized_copper_grate" => 1314,
        "waxed_copper_grate" => 1315,
        "waxed_exposed_copper_grate" => 1316,
        "waxed_weathered_copper_grate" => 1317,
        "waxed_oxidized_copper_grate" => 1318,
        "copper_bulb" => 1319,
        "exposed_copper_bulb" => 1320,
        "weathered_copper_bulb" => 1321,
        "oxidized_copper_bulb" => 1322,
        "waxed_copper_bulb" => 1323,
        "waxed_exposed_copper_bulb" => 1324,
        "waxed_weathered_copper_bulb" => 1325,
        "waxed_oxidized_copper_bulb" => 1326,
        "trial_spawner" => 1327,
        "trial_key" => 1328,
        "ominous_trial_key" => 1329,
        "vault" => 1330,
        "ominous_bottle" => 1331,
        "breeze_rod" => 1332,
        _ => return None,
    })
}

/// Vanilla `DyeColor` name for a `*_dye` item id; `None` for non-dyes. Item
/// ids mirror the `name_to_item` table above (white_dye=944 .. black_dye=959).
fn dye_color_for_item(item: i64) -> Option<&'static str> {
    Some(match item {
        944 => "white",
        945 => "orange",
        946 => "magenta",
        947 => "light_blue",
        948 => "yellow",
        949 => "lime",
        950 => "pink",
        951 => "gray",
        952 => "light_gray",
        953 => "cyan",
        954 => "purple",
        955 => "blue",
        956 => "brown",
        957 => "green",
        958 => "red",
        959 => "black",
        _ => return None,
    })
}
static ITEM_NAMES: [&str; 1333] = [
    "air", "stone", "granite", "polished_granite", "diorite", "polished_diorite",
    "andesite", "polished_andesite", "deepslate", "cobbled_deepslate", "polished_deepslate", "calcite",
    "tuff", "tuff_slab", "tuff_stairs", "tuff_wall", "chiseled_tuff", "polished_tuff",
    "polished_tuff_slab", "polished_tuff_stairs", "polished_tuff_wall", "tuff_bricks", "tuff_brick_slab", "tuff_brick_stairs",
    "tuff_brick_wall", "chiseled_tuff_bricks", "dripstone_block", "grass_block", "dirt", "coarse_dirt",
    "podzol", "rooted_dirt", "mud", "crimson_nylium", "warped_nylium", "cobblestone",
    "oak_planks", "spruce_planks", "birch_planks", "jungle_planks", "acacia_planks", "cherry_planks",
    "dark_oak_planks", "mangrove_planks", "bamboo_planks", "crimson_planks", "warped_planks", "bamboo_mosaic",
    "oak_sapling", "spruce_sapling", "birch_sapling", "jungle_sapling", "acacia_sapling", "cherry_sapling",
    "dark_oak_sapling", "mangrove_propagule", "bedrock", "sand", "suspicious_sand", "suspicious_gravel",
    "red_sand", "gravel", "coal_ore", "deepslate_coal_ore", "iron_ore", "deepslate_iron_ore",
    "copper_ore", "deepslate_copper_ore", "gold_ore", "deepslate_gold_ore", "redstone_ore", "deepslate_redstone_ore",
    "emerald_ore", "deepslate_emerald_ore", "lapis_ore", "deepslate_lapis_ore", "diamond_ore", "deepslate_diamond_ore",
    "nether_gold_ore", "nether_quartz_ore", "ancient_debris", "coal_block", "raw_iron_block", "raw_copper_block",
    "raw_gold_block", "heavy_core", "amethyst_block", "budding_amethyst", "iron_block", "copper_block",
    "gold_block", "diamond_block", "netherite_block", "exposed_copper", "weathered_copper", "oxidized_copper",
    "chiseled_copper", "exposed_chiseled_copper", "weathered_chiseled_copper", "oxidized_chiseled_copper", "cut_copper", "exposed_cut_copper",
    "weathered_cut_copper", "oxidized_cut_copper", "cut_copper_stairs", "exposed_cut_copper_stairs", "weathered_cut_copper_stairs", "oxidized_cut_copper_stairs",
    "cut_copper_slab", "exposed_cut_copper_slab", "weathered_cut_copper_slab", "oxidized_cut_copper_slab", "waxed_copper_block", "waxed_exposed_copper",
    "waxed_weathered_copper", "waxed_oxidized_copper", "waxed_chiseled_copper", "waxed_exposed_chiseled_copper", "waxed_weathered_chiseled_copper", "waxed_oxidized_chiseled_copper",
    "waxed_cut_copper", "waxed_exposed_cut_copper", "waxed_weathered_cut_copper", "waxed_oxidized_cut_copper", "waxed_cut_copper_stairs", "waxed_exposed_cut_copper_stairs",
    "waxed_weathered_cut_copper_stairs", "waxed_oxidized_cut_copper_stairs", "waxed_cut_copper_slab", "waxed_exposed_cut_copper_slab", "waxed_weathered_cut_copper_slab", "waxed_oxidized_cut_copper_slab",
    "oak_log", "spruce_log", "birch_log", "jungle_log", "acacia_log", "cherry_log",
    "dark_oak_log", "mangrove_log", "mangrove_roots", "muddy_mangrove_roots", "crimson_stem", "warped_stem",
    "bamboo_block", "stripped_oak_log", "stripped_spruce_log", "stripped_birch_log", "stripped_jungle_log", "stripped_acacia_log",
    "stripped_cherry_log", "stripped_dark_oak_log", "stripped_mangrove_log", "stripped_crimson_stem", "stripped_warped_stem", "stripped_oak_wood",
    "stripped_spruce_wood", "stripped_birch_wood", "stripped_jungle_wood", "stripped_acacia_wood", "stripped_cherry_wood", "stripped_dark_oak_wood",
    "stripped_mangrove_wood", "stripped_crimson_hyphae", "stripped_warped_hyphae", "stripped_bamboo_block", "oak_wood", "spruce_wood",
    "birch_wood", "jungle_wood", "acacia_wood", "cherry_wood", "dark_oak_wood", "mangrove_wood",
    "crimson_hyphae", "warped_hyphae", "oak_leaves", "spruce_leaves", "birch_leaves", "jungle_leaves",
    "acacia_leaves", "cherry_leaves", "dark_oak_leaves", "mangrove_leaves", "azalea_leaves", "flowering_azalea_leaves",
    "sponge", "wet_sponge", "glass", "tinted_glass", "lapis_block", "sandstone",
    "chiseled_sandstone", "cut_sandstone", "cobweb", "short_grass", "fern", "azalea",
    "flowering_azalea", "dead_bush", "seagrass", "sea_pickle", "white_wool", "orange_wool",
    "magenta_wool", "light_blue_wool", "yellow_wool", "lime_wool", "pink_wool", "gray_wool",
    "light_gray_wool", "cyan_wool", "purple_wool", "blue_wool", "brown_wool", "green_wool",
    "red_wool", "black_wool", "dandelion", "poppy", "blue_orchid", "allium",
    "azure_bluet", "red_tulip", "orange_tulip", "white_tulip", "pink_tulip", "oxeye_daisy",
    "cornflower", "lily_of_the_valley", "wither_rose", "torchflower", "pitcher_plant", "spore_blossom",
    "brown_mushroom", "red_mushroom", "crimson_fungus", "warped_fungus", "crimson_roots", "warped_roots",
    "nether_sprouts", "weeping_vines", "twisting_vines", "sugar_cane", "kelp", "moss_carpet",
    "pink_petals", "moss_block", "hanging_roots", "big_dripleaf", "small_dripleaf", "bamboo",
    "oak_slab", "spruce_slab", "birch_slab", "jungle_slab", "acacia_slab", "cherry_slab",
    "dark_oak_slab", "mangrove_slab", "bamboo_slab", "bamboo_mosaic_slab", "crimson_slab", "warped_slab",
    "stone_slab", "smooth_stone_slab", "sandstone_slab", "cut_sandstone_slab", "petrified_oak_slab", "cobblestone_slab",
    "brick_slab", "stone_brick_slab", "mud_brick_slab", "nether_brick_slab", "quartz_slab", "red_sandstone_slab",
    "cut_red_sandstone_slab", "purpur_slab", "prismarine_slab", "prismarine_brick_slab", "dark_prismarine_slab", "smooth_quartz",
    "smooth_red_sandstone", "smooth_sandstone", "smooth_stone", "bricks", "bookshelf", "chiseled_bookshelf",
    "decorated_pot", "mossy_cobblestone", "obsidian", "torch", "end_rod", "chorus_plant",
    "chorus_flower", "purpur_block", "purpur_pillar", "purpur_stairs", "spawner", "chest",
    "crafting_table", "farmland", "furnace", "ladder", "cobblestone_stairs", "snow",
    "ice", "snow_block", "cactus", "clay", "jukebox", "oak_fence",
    "spruce_fence", "birch_fence", "jungle_fence", "acacia_fence", "cherry_fence", "dark_oak_fence",
    "mangrove_fence", "bamboo_fence", "crimson_fence", "warped_fence", "pumpkin", "carved_pumpkin",
    "jack_o_lantern", "netherrack", "soul_sand", "soul_soil", "basalt", "polished_basalt",
    "smooth_basalt", "soul_torch", "glowstone", "infested_stone", "infested_cobblestone", "infested_stone_bricks",
    "infested_mossy_stone_bricks", "infested_cracked_stone_bricks", "infested_chiseled_stone_bricks", "infested_deepslate", "stone_bricks", "mossy_stone_bricks",
    "cracked_stone_bricks", "chiseled_stone_bricks", "packed_mud", "mud_bricks", "deepslate_bricks", "cracked_deepslate_bricks",
    "deepslate_tiles", "cracked_deepslate_tiles", "chiseled_deepslate", "reinforced_deepslate", "brown_mushroom_block", "red_mushroom_block",
    "mushroom_stem", "iron_bars", "chain", "glass_pane", "melon", "vine",
    "glow_lichen", "brick_stairs", "stone_brick_stairs", "mud_brick_stairs", "mycelium", "lily_pad",
    "nether_bricks", "cracked_nether_bricks", "chiseled_nether_bricks", "nether_brick_fence", "nether_brick_stairs", "sculk",
    "sculk_vein", "sculk_catalyst", "sculk_shrieker", "enchanting_table", "end_portal_frame", "end_stone",
    "end_stone_bricks", "dragon_egg", "sandstone_stairs", "ender_chest", "emerald_block", "oak_stairs",
    "spruce_stairs", "birch_stairs", "jungle_stairs", "acacia_stairs", "cherry_stairs", "dark_oak_stairs",
    "mangrove_stairs", "bamboo_stairs", "bamboo_mosaic_stairs", "crimson_stairs", "warped_stairs", "command_block",
    "beacon", "cobblestone_wall", "mossy_cobblestone_wall", "brick_wall", "prismarine_wall", "red_sandstone_wall",
    "mossy_stone_brick_wall", "granite_wall", "stone_brick_wall", "mud_brick_wall", "nether_brick_wall", "andesite_wall",
    "red_nether_brick_wall", "sandstone_wall", "end_stone_brick_wall", "diorite_wall", "blackstone_wall", "polished_blackstone_wall",
    "polished_blackstone_brick_wall", "cobbled_deepslate_wall", "polished_deepslate_wall", "deepslate_brick_wall", "deepslate_tile_wall", "anvil",
    "chipped_anvil", "damaged_anvil", "chiseled_quartz_block", "quartz_block", "quartz_bricks", "quartz_pillar",
    "quartz_stairs", "white_terracotta", "orange_terracotta", "magenta_terracotta", "light_blue_terracotta", "yellow_terracotta",
    "lime_terracotta", "pink_terracotta", "gray_terracotta", "light_gray_terracotta", "cyan_terracotta", "purple_terracotta",
    "blue_terracotta", "brown_terracotta", "green_terracotta", "red_terracotta", "black_terracotta", "barrier",
    "light", "hay_block", "white_carpet", "orange_carpet", "magenta_carpet", "light_blue_carpet",
    "yellow_carpet", "lime_carpet", "pink_carpet", "gray_carpet", "light_gray_carpet", "cyan_carpet",
    "purple_carpet", "blue_carpet", "brown_carpet", "green_carpet", "red_carpet", "black_carpet",
    "terracotta", "packed_ice", "dirt_path", "sunflower", "lilac", "rose_bush",
    "peony", "tall_grass", "large_fern", "white_stained_glass", "orange_stained_glass", "magenta_stained_glass",
    "light_blue_stained_glass", "yellow_stained_glass", "lime_stained_glass", "pink_stained_glass", "gray_stained_glass", "light_gray_stained_glass",
    "cyan_stained_glass", "purple_stained_glass", "blue_stained_glass", "brown_stained_glass", "green_stained_glass", "red_stained_glass",
    "black_stained_glass", "white_stained_glass_pane", "orange_stained_glass_pane", "magenta_stained_glass_pane", "light_blue_stained_glass_pane", "yellow_stained_glass_pane",
    "lime_stained_glass_pane", "pink_stained_glass_pane", "gray_stained_glass_pane", "light_gray_stained_glass_pane", "cyan_stained_glass_pane", "purple_stained_glass_pane",
    "blue_stained_glass_pane", "brown_stained_glass_pane", "green_stained_glass_pane", "red_stained_glass_pane", "black_stained_glass_pane", "prismarine",
    "prismarine_bricks", "dark_prismarine", "prismarine_stairs", "prismarine_brick_stairs", "dark_prismarine_stairs", "sea_lantern",
    "red_sandstone", "chiseled_red_sandstone", "cut_red_sandstone", "red_sandstone_stairs", "repeating_command_block", "chain_command_block",
    "magma_block", "nether_wart_block", "warped_wart_block", "red_nether_bricks", "bone_block", "structure_void",
    "shulker_box", "white_shulker_box", "orange_shulker_box", "magenta_shulker_box", "light_blue_shulker_box", "yellow_shulker_box",
    "lime_shulker_box", "pink_shulker_box", "gray_shulker_box", "light_gray_shulker_box", "cyan_shulker_box", "purple_shulker_box",
    "blue_shulker_box", "brown_shulker_box", "green_shulker_box", "red_shulker_box", "black_shulker_box", "white_glazed_terracotta",
    "orange_glazed_terracotta", "magenta_glazed_terracotta", "light_blue_glazed_terracotta", "yellow_glazed_terracotta", "lime_glazed_terracotta", "pink_glazed_terracotta",
    "gray_glazed_terracotta", "light_gray_glazed_terracotta", "cyan_glazed_terracotta", "purple_glazed_terracotta", "blue_glazed_terracotta", "brown_glazed_terracotta",
    "green_glazed_terracotta", "red_glazed_terracotta", "black_glazed_terracotta", "white_concrete", "orange_concrete", "magenta_concrete",
    "light_blue_concrete", "yellow_concrete", "lime_concrete", "pink_concrete", "gray_concrete", "light_gray_concrete",
    "cyan_concrete", "purple_concrete", "blue_concrete", "brown_concrete", "green_concrete", "red_concrete",
    "black_concrete", "white_concrete_powder", "orange_concrete_powder", "magenta_concrete_powder", "light_blue_concrete_powder", "yellow_concrete_powder",
    "lime_concrete_powder", "pink_concrete_powder", "gray_concrete_powder", "light_gray_concrete_powder", "cyan_concrete_powder", "purple_concrete_powder",
    "blue_concrete_powder", "brown_concrete_powder", "green_concrete_powder", "red_concrete_powder", "black_concrete_powder", "turtle_egg",
    "sniffer_egg", "dead_tube_coral_block", "dead_brain_coral_block", "dead_bubble_coral_block", "dead_fire_coral_block", "dead_horn_coral_block",
    "tube_coral_block", "brain_coral_block", "bubble_coral_block", "fire_coral_block", "horn_coral_block", "tube_coral",
    "brain_coral", "bubble_coral", "fire_coral", "horn_coral", "dead_brain_coral", "dead_bubble_coral",
    "dead_fire_coral", "dead_horn_coral", "dead_tube_coral", "tube_coral_fan", "brain_coral_fan", "bubble_coral_fan",
    "fire_coral_fan", "horn_coral_fan", "dead_tube_coral_fan", "dead_brain_coral_fan", "dead_bubble_coral_fan", "dead_fire_coral_fan",
    "dead_horn_coral_fan", "blue_ice", "conduit", "polished_granite_stairs", "smooth_red_sandstone_stairs", "mossy_stone_brick_stairs",
    "polished_diorite_stairs", "mossy_cobblestone_stairs", "end_stone_brick_stairs", "stone_stairs", "smooth_sandstone_stairs", "smooth_quartz_stairs",
    "granite_stairs", "andesite_stairs", "red_nether_brick_stairs", "polished_andesite_stairs", "diorite_stairs", "cobbled_deepslate_stairs",
    "polished_deepslate_stairs", "deepslate_brick_stairs", "deepslate_tile_stairs", "polished_granite_slab", "smooth_red_sandstone_slab", "mossy_stone_brick_slab",
    "polished_diorite_slab", "mossy_cobblestone_slab", "end_stone_brick_slab", "smooth_sandstone_slab", "smooth_quartz_slab", "granite_slab",
    "andesite_slab", "red_nether_brick_slab", "polished_andesite_slab", "diorite_slab", "cobbled_deepslate_slab", "polished_deepslate_slab",
    "deepslate_brick_slab", "deepslate_tile_slab", "scaffolding", "redstone", "redstone_torch", "redstone_block",
    "repeater", "comparator", "piston", "sticky_piston", "slime_block", "honey_block",
    "observer", "hopper", "dispenser", "dropper", "lectern", "target",
    "lever", "lightning_rod", "daylight_detector", "sculk_sensor", "calibrated_sculk_sensor", "tripwire_hook",
    "trapped_chest", "tnt", "redstone_lamp", "note_block", "stone_button", "polished_blackstone_button",
    "oak_button", "spruce_button", "birch_button", "jungle_button", "acacia_button", "cherry_button",
    "dark_oak_button", "mangrove_button", "bamboo_button", "crimson_button", "warped_button", "stone_pressure_plate",
    "polished_blackstone_pressure_plate", "light_weighted_pressure_plate", "heavy_weighted_pressure_plate", "oak_pressure_plate", "spruce_pressure_plate", "birch_pressure_plate",
    "jungle_pressure_plate", "acacia_pressure_plate", "cherry_pressure_plate", "dark_oak_pressure_plate", "mangrove_pressure_plate", "bamboo_pressure_plate",
    "crimson_pressure_plate", "warped_pressure_plate", "iron_door", "oak_door", "spruce_door", "birch_door",
    "jungle_door", "acacia_door", "cherry_door", "dark_oak_door", "mangrove_door", "bamboo_door",
    "crimson_door", "warped_door", "copper_door", "exposed_copper_door", "weathered_copper_door", "oxidized_copper_door",
    "waxed_copper_door", "waxed_exposed_copper_door", "waxed_weathered_copper_door", "waxed_oxidized_copper_door", "iron_trapdoor", "oak_trapdoor",
    "spruce_trapdoor", "birch_trapdoor", "jungle_trapdoor", "acacia_trapdoor", "cherry_trapdoor", "dark_oak_trapdoor",
    "mangrove_trapdoor", "bamboo_trapdoor", "crimson_trapdoor", "warped_trapdoor", "copper_trapdoor", "exposed_copper_trapdoor",
    "weathered_copper_trapdoor", "oxidized_copper_trapdoor", "waxed_copper_trapdoor", "waxed_exposed_copper_trapdoor", "waxed_weathered_copper_trapdoor", "waxed_oxidized_copper_trapdoor",
    "oak_fence_gate", "spruce_fence_gate", "birch_fence_gate", "jungle_fence_gate", "acacia_fence_gate", "cherry_fence_gate",
    "dark_oak_fence_gate", "mangrove_fence_gate", "bamboo_fence_gate", "crimson_fence_gate", "warped_fence_gate", "powered_rail",
    "detector_rail", "rail", "activator_rail", "saddle", "minecart", "chest_minecart",
    "furnace_minecart", "tnt_minecart", "hopper_minecart", "carrot_on_a_stick", "warped_fungus_on_a_stick", "elytra",
    "oak_boat", "oak_chest_boat", "spruce_boat", "spruce_chest_boat", "birch_boat", "birch_chest_boat",
    "jungle_boat", "jungle_chest_boat", "acacia_boat", "acacia_chest_boat", "cherry_boat", "cherry_chest_boat",
    "dark_oak_boat", "dark_oak_chest_boat", "mangrove_boat", "mangrove_chest_boat", "bamboo_raft", "bamboo_chest_raft",
    "structure_block", "jigsaw", "turtle_helmet", "turtle_scute", "armadillo_scute", "wolf_armor",
    "flint_and_steel", "bowl", "apple", "bow", "arrow", "coal",
    "charcoal", "diamond", "emerald", "lapis_lazuli", "quartz", "amethyst_shard",
    "raw_iron", "iron_ingot", "raw_copper", "copper_ingot", "raw_gold", "gold_ingot",
    "netherite_ingot", "netherite_scrap", "wooden_sword", "wooden_shovel", "wooden_pickaxe", "wooden_axe",
    "wooden_hoe", "stone_sword", "stone_shovel", "stone_pickaxe", "stone_axe", "stone_hoe",
    "golden_sword", "golden_shovel", "golden_pickaxe", "golden_axe", "golden_hoe", "iron_sword",
    "iron_shovel", "iron_pickaxe", "iron_axe", "iron_hoe", "diamond_sword", "diamond_shovel",
    "diamond_pickaxe", "diamond_axe", "diamond_hoe", "netherite_sword", "netherite_shovel", "netherite_pickaxe",
    "netherite_axe", "netherite_hoe", "stick", "mushroom_stew", "string", "feather",
    "gunpowder", "wheat_seeds", "wheat", "bread", "leather_helmet", "leather_chestplate",
    "leather_leggings", "leather_boots", "chainmail_helmet", "chainmail_chestplate", "chainmail_leggings", "chainmail_boots",
    "iron_helmet", "iron_chestplate", "iron_leggings", "iron_boots", "diamond_helmet", "diamond_chestplate",
    "diamond_leggings", "diamond_boots", "golden_helmet", "golden_chestplate", "golden_leggings", "golden_boots",
    "netherite_helmet", "netherite_chestplate", "netherite_leggings", "netherite_boots", "flint", "porkchop",
    "cooked_porkchop", "painting", "golden_apple", "enchanted_golden_apple", "oak_sign", "spruce_sign",
    "birch_sign", "jungle_sign", "acacia_sign", "cherry_sign", "dark_oak_sign", "mangrove_sign",
    "bamboo_sign", "crimson_sign", "warped_sign", "oak_hanging_sign", "spruce_hanging_sign", "birch_hanging_sign",
    "jungle_hanging_sign", "acacia_hanging_sign", "cherry_hanging_sign", "dark_oak_hanging_sign", "mangrove_hanging_sign", "bamboo_hanging_sign",
    "crimson_hanging_sign", "warped_hanging_sign", "bucket", "water_bucket", "lava_bucket", "powder_snow_bucket",
    "snowball", "leather", "milk_bucket", "pufferfish_bucket", "salmon_bucket", "cod_bucket",
    "tropical_fish_bucket", "axolotl_bucket", "tadpole_bucket", "brick", "clay_ball", "dried_kelp_block",
    "paper", "book", "slime_ball", "egg", "compass", "recovery_compass",
    "bundle", "fishing_rod", "clock", "spyglass", "glowstone_dust", "cod",
    "salmon", "tropical_fish", "pufferfish", "cooked_cod", "cooked_salmon", "ink_sac",
    "glow_ink_sac", "cocoa_beans", "white_dye", "orange_dye", "magenta_dye", "light_blue_dye",
    "yellow_dye", "lime_dye", "pink_dye", "gray_dye", "light_gray_dye", "cyan_dye",
    "purple_dye", "blue_dye", "brown_dye", "green_dye", "red_dye", "black_dye",
    "bone_meal", "bone", "sugar", "cake", "white_bed", "orange_bed",
    "magenta_bed", "light_blue_bed", "yellow_bed", "lime_bed", "pink_bed", "gray_bed",
    "light_gray_bed", "cyan_bed", "purple_bed", "blue_bed", "brown_bed", "green_bed",
    "red_bed", "black_bed", "cookie", "crafter", "filled_map", "shears",
    "melon_slice", "dried_kelp", "pumpkin_seeds", "melon_seeds", "beef", "cooked_beef",
    "chicken", "cooked_chicken", "rotten_flesh", "ender_pearl", "blaze_rod", "ghast_tear",
    "gold_nugget", "nether_wart", "potion", "glass_bottle", "spider_eye", "fermented_spider_eye",
    "blaze_powder", "magma_cream", "brewing_stand", "cauldron", "ender_eye", "glistering_melon_slice",
    "armadillo_spawn_egg", "allay_spawn_egg", "axolotl_spawn_egg", "bat_spawn_egg", "bee_spawn_egg", "blaze_spawn_egg",
    "bogged_spawn_egg", "breeze_spawn_egg", "cat_spawn_egg", "camel_spawn_egg", "cave_spider_spawn_egg", "chicken_spawn_egg",
    "cod_spawn_egg", "cow_spawn_egg", "creeper_spawn_egg", "dolphin_spawn_egg", "donkey_spawn_egg", "drowned_spawn_egg",
    "elder_guardian_spawn_egg", "ender_dragon_spawn_egg", "enderman_spawn_egg", "endermite_spawn_egg", "evoker_spawn_egg", "fox_spawn_egg",
    "frog_spawn_egg", "ghast_spawn_egg", "glow_squid_spawn_egg", "goat_spawn_egg", "guardian_spawn_egg", "hoglin_spawn_egg",
    "horse_spawn_egg", "husk_spawn_egg", "iron_golem_spawn_egg", "llama_spawn_egg", "magma_cube_spawn_egg", "mooshroom_spawn_egg",
    "mule_spawn_egg", "ocelot_spawn_egg", "panda_spawn_egg", "parrot_spawn_egg", "phantom_spawn_egg", "pig_spawn_egg",
    "piglin_spawn_egg", "piglin_brute_spawn_egg", "pillager_spawn_egg", "polar_bear_spawn_egg", "pufferfish_spawn_egg", "rabbit_spawn_egg",
    "ravager_spawn_egg", "salmon_spawn_egg", "sheep_spawn_egg", "shulker_spawn_egg", "silverfish_spawn_egg", "skeleton_spawn_egg",
    "skeleton_horse_spawn_egg", "slime_spawn_egg", "sniffer_spawn_egg", "snow_golem_spawn_egg", "spider_spawn_egg", "squid_spawn_egg",
    "stray_spawn_egg", "strider_spawn_egg", "tadpole_spawn_egg", "trader_llama_spawn_egg", "tropical_fish_spawn_egg", "turtle_spawn_egg",
    "vex_spawn_egg", "villager_spawn_egg", "vindicator_spawn_egg", "wandering_trader_spawn_egg", "warden_spawn_egg", "witch_spawn_egg",
    "wither_spawn_egg", "wither_skeleton_spawn_egg", "wolf_spawn_egg", "zoglin_spawn_egg", "zombie_spawn_egg", "zombie_horse_spawn_egg",
    "zombie_villager_spawn_egg", "zombified_piglin_spawn_egg", "experience_bottle", "fire_charge", "wind_charge", "writable_book",
    "written_book", "mace", "item_frame", "glow_item_frame", "flower_pot", "carrot",
    "potato", "baked_potato", "poisonous_potato", "map", "golden_carrot", "skeleton_skull",
    "wither_skeleton_skull", "player_head", "zombie_head", "creeper_head", "dragon_head", "piglin_head",
    "nether_star", "pumpkin_pie", "firework_rocket", "firework_star", "enchanted_book", "nether_brick",
    "prismarine_shard", "prismarine_crystals", "rabbit", "cooked_rabbit", "rabbit_stew", "rabbit_foot",
    "rabbit_hide", "armor_stand", "iron_horse_armor", "golden_horse_armor", "diamond_horse_armor", "leather_horse_armor",
    "lead", "name_tag", "command_block_minecart", "mutton", "cooked_mutton", "white_banner",
    "orange_banner", "magenta_banner", "light_blue_banner", "yellow_banner", "lime_banner", "pink_banner",
    "gray_banner", "light_gray_banner", "cyan_banner", "purple_banner", "blue_banner", "brown_banner",
    "green_banner", "red_banner", "black_banner", "end_crystal", "chorus_fruit", "popped_chorus_fruit",
    "torchflower_seeds", "pitcher_pod", "beetroot", "beetroot_seeds", "beetroot_soup", "dragon_breath",
    "splash_potion", "spectral_arrow", "tipped_arrow", "lingering_potion", "shield", "totem_of_undying",
    "shulker_shell", "iron_nugget", "knowledge_book", "debug_stick", "music_disc_13", "music_disc_cat",
    "music_disc_blocks", "music_disc_chirp", "music_disc_creator", "music_disc_creator_music_box", "music_disc_far", "music_disc_mall",
    "music_disc_mellohi", "music_disc_stal", "music_disc_strad", "music_disc_ward", "music_disc_11", "music_disc_wait",
    "music_disc_otherside", "music_disc_relic", "music_disc_5", "music_disc_pigstep", "music_disc_precipice", "disc_fragment_5",
    "trident", "phantom_membrane", "nautilus_shell", "heart_of_the_sea", "crossbow", "suspicious_stew",
    "loom", "flower_banner_pattern", "creeper_banner_pattern", "skull_banner_pattern", "mojang_banner_pattern", "globe_banner_pattern",
    "piglin_banner_pattern", "flow_banner_pattern", "guster_banner_pattern", "goat_horn", "composter", "barrel",
    "smoker", "blast_furnace", "cartography_table", "fletching_table", "grindstone", "smithing_table",
    "stonecutter", "bell", "lantern", "soul_lantern", "sweet_berries", "glow_berries",
    "campfire", "soul_campfire", "shroomlight", "honeycomb", "bee_nest", "beehive",
    "honey_bottle", "honeycomb_block", "lodestone", "crying_obsidian", "blackstone", "blackstone_slab",
    "blackstone_stairs", "gilded_blackstone", "polished_blackstone", "polished_blackstone_slab", "polished_blackstone_stairs", "chiseled_polished_blackstone",
    "polished_blackstone_bricks", "polished_blackstone_brick_slab", "polished_blackstone_brick_stairs", "cracked_polished_blackstone_bricks", "respawn_anchor", "candle",
    "white_candle", "orange_candle", "magenta_candle", "light_blue_candle", "yellow_candle", "lime_candle",
    "pink_candle", "gray_candle", "light_gray_candle", "cyan_candle", "purple_candle", "blue_candle",
    "brown_candle", "green_candle", "red_candle", "black_candle", "small_amethyst_bud", "medium_amethyst_bud",
    "large_amethyst_bud", "amethyst_cluster", "pointed_dripstone", "ochre_froglight", "verdant_froglight", "pearlescent_froglight",
    "frogspawn", "echo_shard", "brush", "netherite_upgrade_smithing_template", "sentry_armor_trim_smithing_template", "dune_armor_trim_smithing_template",
    "coast_armor_trim_smithing_template", "wild_armor_trim_smithing_template", "ward_armor_trim_smithing_template", "eye_armor_trim_smithing_template", "vex_armor_trim_smithing_template", "tide_armor_trim_smithing_template",
    "snout_armor_trim_smithing_template", "rib_armor_trim_smithing_template", "spire_armor_trim_smithing_template", "wayfinder_armor_trim_smithing_template", "shaper_armor_trim_smithing_template", "silence_armor_trim_smithing_template",
    "raiser_armor_trim_smithing_template", "host_armor_trim_smithing_template", "flow_armor_trim_smithing_template", "bolt_armor_trim_smithing_template", "angler_pottery_sherd", "archer_pottery_sherd",
    "arms_up_pottery_sherd", "blade_pottery_sherd", "brewer_pottery_sherd", "burn_pottery_sherd", "danger_pottery_sherd", "explorer_pottery_sherd",
    "flow_pottery_sherd", "friend_pottery_sherd", "guster_pottery_sherd", "heart_pottery_sherd", "heartbreak_pottery_sherd", "howl_pottery_sherd",
    "miner_pottery_sherd", "mourner_pottery_sherd", "plenty_pottery_sherd", "prize_pottery_sherd", "scrape_pottery_sherd", "sheaf_pottery_sherd",
    "shelter_pottery_sherd", "skull_pottery_sherd", "snort_pottery_sherd", "copper_grate", "exposed_copper_grate", "weathered_copper_grate",
    "oxidized_copper_grate", "waxed_copper_grate", "waxed_exposed_copper_grate", "waxed_weathered_copper_grate", "waxed_oxidized_copper_grate", "copper_bulb",
    "exposed_copper_bulb", "weathered_copper_bulb", "oxidized_copper_bulb", "waxed_copper_bulb", "waxed_exposed_copper_bulb", "waxed_weathered_copper_bulb",
    "waxed_oxidized_copper_bulb", "trial_spawner", "trial_key", "ominous_trial_key", "vault", "ominous_bottle",
    "breeze_rod",
];
fn item_to_name(id: i64) -> Option<&'static str> {
    #[cfg(feature = "mod-suite")]
    if let Some(name) = live_create_item_name(id) {
        return Some(name);
    }
    usize::try_from(id).ok().and_then(|i| ITEM_NAMES.get(i).copied())
}
fn read_nbt_string_bytes(data: &[u8], p: &mut usize) -> Option<String> {
    if *p + 3 > data.len() || data[*p] != 8 {
        return None;
    }
    let len = u16::from_be_bytes([data[*p + 1], data[*p + 2]]) as usize;
    if *p + 3 + len > data.len() {
        return None;
    }
    let text = String::from_utf8_lossy(&data[*p + 3..*p + 3 + len]).into_owned();
    *p += 3 + len;
    Some(text)
}
fn nbt_int_val(tag: &crate::nbt::Tag) -> Option<i64> {
    match tag {
        crate::nbt::Tag::Byte(v) => Some(i64::from(*v)),
        crate::nbt::Tag::Short(v) => Some(i64::from(*v)),
        crate::nbt::Tag::Int(v) => Some(i64::from(*v)),
        crate::nbt::Tag::Long(v) => Some(*v),
        _ => None,
    }
}
fn stack_to_nbt(slot: i16, (item, count, comps): &Stack) -> Option<crate::nbt::Tag> {
    use crate::nbt::{NbtString, Tag};
    if *item <= 0 || *count == 0 {
        return None;
    }
    let name = item_to_name(*item)?;
    let mut entry = std::collections::BTreeMap::new();
    entry.insert(NbtString::text("Slot"), Tag::Byte(slot as i8));
    entry.insert(
        NbtString::text("id"),
        Tag::String(NbtString::text(&format!("minecraft:{name}"))),
    );
    entry.insert(NbtString::text("count"), Tag::Int(i32::from(*count)));
    if !comps.is_empty() {
        let mut comp_map = std::collections::BTreeMap::new();
        for (ty, data) in comps {
            match *ty {
                COMP_MAX_STACK_SIZE => {
                    comp_map.insert(
                        NbtString::text("minecraft:max_stack_size"),
                        Tag::Int(read_varint_at(data, 0).0 as i32),
                    );
                }
                COMP_MAX_DAMAGE => {
                    comp_map.insert(
                        NbtString::text("minecraft:max_damage"),
                        Tag::Int(read_varint_at(data, 0).0 as i32),
                    );
                }
                COMP_DAMAGE => {
                    comp_map.insert(
                        NbtString::text("minecraft:damage"),
                        Tag::Int(read_varint_at(data, 0).0 as i32),
                    );
                }
                COMP_UNBREAKABLE => {
                    let show = data.first().copied().unwrap_or(1) != 0;
                    let mut inner = std::collections::BTreeMap::new();
                    // `show_in_tooltip` is an optionalFieldOf(default true) — the
                    // game only serializes it when it is false.
                    if !show {
                        inner.insert(NbtString::text("show_in_tooltip"), Tag::Byte(0));
                    }
                    comp_map.insert(
                        NbtString::text("minecraft:unbreakable"),
                        Tag::Compound(inner),
                    );
                }
                COMP_CUSTOM_NAME => {
                    let mut p = 0;
                    if let Some(text) = read_nbt_string_bytes(data, &mut p) {
                        comp_map.insert(
                            NbtString::text("minecraft:custom_name"),
                            Tag::String(NbtString::text(&text)),
                        );
                    }
                }
                COMP_LORE => {
                    let (n, mut p) = read_varint_at(data, 0);
                    let mut list = Vec::new();
                    for _ in 0..n.min(128) {
                        if let Some(line) = read_nbt_string_bytes(data, &mut p) {
                            list.push(Tag::String(NbtString::text(&line)));
                        }
                    }
                    let kind = if list.is_empty() { 0 } else { 8 };
                    comp_map.insert(NbtString::text("minecraft:lore"), Tag::List(kind, list));
                }
                COMP_ENCHANTMENTS | COMP_STORED_ENCHANTMENTS => {
                    let (n, mut p) = read_varint_at(data, 0);
                    let mut levels = std::collections::BTreeMap::new();
                    for _ in 0..n.min(64) {
                        if p + 2 > data.len() {
                            break;
                        }
                        let (id, p2) = read_varint_at(data, p);
                        let (lvl, p3) = read_varint_at(data, p2);
                        p = p3;
                        if let Some(ename) = enchant_name(id) {
                            levels.insert(NbtString::text(ename), Tag::Int(lvl as i32));
                        }
                    }
                    let show = data.get(p).copied().unwrap_or(1) != 0;
                    let mut inner = std::collections::BTreeMap::new();
                    inner.insert(NbtString::text("levels"), Tag::Compound(levels));
                    // `show_in_tooltip` defaults to true and is only written when
                    // the stack hides its enchantments.
                    if !show {
                        inner.insert(NbtString::text("show_in_tooltip"), Tag::Byte(0));
                    }
                    let key = if *ty == COMP_STORED_ENCHANTMENTS {
                        "minecraft:stored_enchantments"
                    } else {
                        "minecraft:enchantments"
                    };
                    comp_map.insert(NbtString::text(key), Tag::Compound(inner));
                }
                COMP_REPAIR_COST => {
                    comp_map.insert(
                        NbtString::text("minecraft:repair_cost"),
                        Tag::Int(read_varint_at(data, 0).0 as i32),
                    );
                }
                COMP_ATTRIBUTE_MODIFIERS => {
                    if let Some(amc) = attr_modifiers_parse(data) {
                        let mut list = Vec::new();
                        for m in &amc.modifiers {
                            let Some(aname) = attr_key_name(i64::from(m.attribute_id)) else {
                                continue;
                            };
                            let mut e = std::collections::BTreeMap::new();
                            e.insert(
                                NbtString::text("type"),
                                Tag::String(NbtString::text(aname)),
                            );
                            e.insert(
                                NbtString::text("id"),
                                Tag::String(NbtString::text(&m.modifier_id)),
                            );
                            e.insert(NbtString::text("amount"), Tag::Double(m.amount.to_bits()));
                            e.insert(
                                NbtString::text("operation"),
                                Tag::String(NbtString::text(m.operation.snbt_name())),
                            );
                            e.insert(
                                NbtString::text("slot"),
                                Tag::String(NbtString::text(m.slot.snbt_name())),
                            );
                            list.push(Tag::Compound(e));
                        }
                        let mut inner = std::collections::BTreeMap::new();
                        inner.insert(
                            NbtString::text("modifiers"),
                            Tag::List(if list.is_empty() { 0 } else { 10 }, list),
                        );
                        // `show_in_tooltip` defaults to true; only written when
                        // the modifiers are hidden.
                        if !amc.show_in_tooltip {
                            inner.insert(
                                NbtString::text("show_in_tooltip"),
                                Tag::Byte(0),
                            );
                        }
                        comp_map.insert(
                            NbtString::text("minecraft:attribute_modifiers"),
                            Tag::Compound(inner),
                        );
                    }
                }
                COMP_WRITABLE_BOOK_CONTENT | COMP_WRITTEN_BOOK_CONTENT => {
                    if let Some(fields) = book_content_nbt(*ty, data) {
                        comp_map.insert(
                            NbtString::text(comp_wire_name(*ty).unwrap_or_default()),
                            Tag::Compound(fields),
                        );
                    }
                }
                COMP_JUKEBOX_PLAYABLE => {
                    if let Some(fields) = jukebox_playable_nbt(data) {
                        comp_map.insert(
                            NbtString::text("minecraft:jukebox_playable"),
                            fields,
                        );
                    }
                }
                _ => {
                    // Preserve components this codec does not model
                    // structurally as their exact wire payload, so a
                    // cart/block-entity save never silently drops data.
                    if let Some(name) = comp_wire_name(*ty) {
                        comp_map.insert(NbtString::text(name), Tag::ByteArray(data.clone()));
                    }
                }
            }
        }
        if !comp_map.is_empty() {
            entry.insert(NbtString::text("components"), Tag::Compound(comp_map));
        }
    }
    Some(Tag::Compound(entry))
}
fn stacks_to_items_nbt(slots: &HashMap<i16, Stack>) -> crate::nbt::Tag {
    let mut keys: Vec<i16> = slots.keys().copied().collect();
    keys.sort_unstable();
    let items: Vec<crate::nbt::Tag> = keys
        .into_iter()
        .filter_map(|slot| slots.get(&slot).and_then(|st| stack_to_nbt(slot, st)))
        .collect();
    crate::nbt::Tag::List(if items.is_empty() { 0 } else { 10 }, items)
}
fn items_nbt_to_stacks(tag: &crate::nbt::Tag) -> Result<HashMap<i16, Stack>, &'static str> {
    use crate::nbt::Tag;
    let list = tag.list().ok_or("cart Items is not a list")?;
    let mut out = HashMap::new();
    for entry in list {
        let slot = match entry.get("Slot") {
            Some(Tag::Byte(b)) => i16::from(*b),
            Some(Tag::Short(s)) => *s,
            Some(Tag::Int(i)) => *i as i16,
            _ => return Err("cart item Slot missing"),
        };
        let id = entry
            .get("id")
            .and_then(Tag::string)
            .ok_or("cart item id missing")?
            .to_utf8()
            .map_err(|_| "invalid cart item id")?;
        let item = name_to_item(&id).ok_or("unknown cart item id")?;
        let count_i = entry
            .get("count")
            .or_else(|| entry.get("Count"))
            .and_then(nbt_int_val)
            .unwrap_or(1);
        if count_i <= 0 {
            continue;
        }
        let count = u8::try_from(count_i).map_err(|_| "cart item count out of range")?;
        let mut comps: Comps = Vec::new();
        if let Some(Tag::Compound(cfields)) = entry.get("components") {
            for (k, v) in cfields {
                let Ok(key_str) = k.to_utf8() else {
                    continue;
                };
                match key_str.trim_start_matches("minecraft:") {
                    "max_stack_size" => {
                        if let Some(n) = nbt_int_val(v) {
                            comp_set_varint(&mut comps, COMP_MAX_STACK_SIZE, n);
                        }
                    }
                    "max_damage" => {
                        if let Some(n) = nbt_int_val(v) {
                            comp_set_varint(&mut comps, COMP_MAX_DAMAGE, n);
                        }
                    }
                    "damage" => {
                        if let Some(n) = nbt_int_val(v) {
                            comp_set_varint(&mut comps, COMP_DAMAGE, n);
                        }
                    }
                    "repair_cost" => {
                        if let Some(n) = nbt_int_val(v) {
                            comp_set_varint(&mut comps, COMP_REPAIR_COST, n);
                        }
                    }
                    "attribute_modifiers" => {
                        let mut modifiers = Vec::new();
                        if let Some(list) = v.get("modifiers").and_then(|t| t.list()) {
                            for e in list {
                                let Some(aname) = e
                                    .get("type")
                                    .and_then(|t| t.string())
                                    .and_then(|s| s.to_utf8().ok())
                                else {
                                    continue;
                                };
                                let Some(attribute_id) = attr_key_id(&aname) else {
                                    continue;
                                };
                                let modifier_id = e
                                    .get("id")
                                    .and_then(|t| t.string())
                                    .and_then(|s| s.to_utf8().ok())
                                    .unwrap_or_else(|| "megu:custom".to_string());
                                let amount = e
                                    .get("amount")
                                    .and_then(|t| match t {
                                        Tag::Double(bits) => Some(f64::from_bits(*bits)),
                                        Tag::Float(bits) => Some(f64::from(f32::from_bits(*bits))),
                                        _ => nbt_int_val(t).map(|n| n as f64),
                                    })
                                    .unwrap_or(0.0);
                                let operation = e
                                    .get("operation")
                                    .and_then(|t| t.string())
                                    .and_then(|s| s.to_utf8().ok())
                                    .as_deref()
                                    .and_then(ItemAttrOp::from_snbt)
                                    .unwrap_or(ItemAttrOp::AddValue);
                                let slot = e
                                    .get("slot")
                                    .and_then(|t| t.string())
                                    .and_then(|s| s.to_utf8().ok())
                                    .as_deref()
                                    .and_then(ItemAttrSlotGroup::from_snbt)
                                    .unwrap_or(ItemAttrSlotGroup::Any);
                                modifiers.push(ItemAttributeModifier {
                                    attribute_id: attribute_id as i32,
                                    modifier_id,
                                    amount,
                                    operation,
                                    slot,
                                });
                            }
                        }
                        let show_in_tooltip = v
                            .get("show_in_tooltip")
                            .and_then(|t| match t {
                                Tag::Byte(b) => Some(*b != 0),
                                _ => None,
                            })
                            .unwrap_or(true);
                        comp_set(
                            &mut comps,
                            COMP_ATTRIBUTE_MODIFIERS,
                            attr_modifiers_data(&AttributeModifiersComponent {
                                modifiers,
                                show_in_tooltip,
                            }),
                        );
                    }
                    "unbreakable" => {
                        let show = v
                            .get("show_in_tooltip")
                            .and_then(|t| match t {
                                Tag::Byte(b) => Some(*b != 0),
                                _ => None,
                            })
                            .unwrap_or(true);
                        comp_set(&mut comps, COMP_UNBREAKABLE, vec![u8::from(show)]);
                    }
                    "custom_name" => {
                        if let Some(text) = v.string().and_then(|s| s.to_utf8().ok()) {
                            comp_set(&mut comps, COMP_CUSTOM_NAME, nbt_string(&text));
                        }
                    }
                    "lore" => {
                        if let Some(lines) = v.list() {
                            let mut d = Vec::new();
                            write_varint(&mut d, lines.len() as i64);
                            for line in lines {
                                if let Some(s) = line.string().and_then(|s| s.to_utf8().ok()) {
                                    d.extend_from_slice(&nbt_string(&s));
                                }
                            }
                            comp_set(&mut comps, COMP_LORE, d);
                        }
                    }
                    name @ ("enchantments" | "stored_enchantments") => {
                        let comp_ty = if name == "stored_enchantments" {
                            COMP_STORED_ENCHANTMENTS
                        } else {
                            COMP_ENCHANTMENTS
                        };
                        let show = v
                            .get("show_in_tooltip")
                            .and_then(|t| match t {
                                Tag::Byte(b) => Some(*b != 0),
                                _ => None,
                            })
                            .unwrap_or(true);
                        let levels_tag = v.get("levels").unwrap_or(v);
                        if let Tag::Compound(lmap) = levels_tag {
                            let mut list = Vec::new();
                            for (ek, ev) in lmap {
                                if let (Ok(ename), Some(lvl)) = (ek.to_utf8(), nbt_int_val(ev)) {
                                    if let Some(eid) = enchant_id(&ename) {
                                        list.push((eid, lvl));
                                    }
                                }
                            }
                            comp_set(&mut comps, comp_ty, enchantments_data(&list, show));
                        }
                    }
                    "writable_book_content" | "written_book_content" => {
                        let comp_ty = if key_str.ends_with("written_book_content")
                            && !key_str.ends_with("writable_book_content")
                        {
                            COMP_WRITTEN_BOOK_CONTENT
                        } else {
                            COMP_WRITABLE_BOOK_CONTENT
                        };
                        if let Some(data) = book_content_data(comp_ty, v) {
                            comp_set(&mut comps, comp_ty, data);
                        }
                    }
                    "jukebox_playable" => {
                        if let Some(data) = jukebox_playable_data(v) {
                            comp_set(&mut comps, COMP_JUKEBOX_PLAYABLE, data);
                        }
                    }
                    _ => {
                        if let (Some(id), Tag::ByteArray(bytes)) =
                            (comp_id_for_name(&key_str), v)
                        {
                            comp_set(&mut comps, id, bytes.clone());
                        }
                    }
                }
            }
        }
        out.insert(slot, (item, count, comps));
    }
    Ok(out)
}

// Block-entity NBT payloads and transitions for the runtime integration.
// The integration supplies world inputs and owns blockstate, ticks, and packets.
use crate::brigadier_wire::ResourceLocation;
use crate::nbt::Tag;

type BlockEntityNbtFields = BTreeMap<NbtString, Tag>;

fn block_entity_compound(fields: impl IntoIterator<Item = (&'static str, Tag)>) -> Tag {
    Tag::Compound(
        fields
            .into_iter()
            .map(|(key, tag)| (NbtString::text(key), tag))
            .collect(),
    )
}

fn block_entity_put(fields: &mut BlockEntityNbtFields, key: &str, tag: Tag) {
    fields.insert(NbtString::text(key), tag);
}

fn block_entity_string(value: &str) -> Tag {
    Tag::String(NbtString::text(value))
}

fn block_entity_str_field(tag: &Tag, key: &str) -> Option<String> {
    tag.get(key)?.string()?.to_utf8().ok()
}

fn block_entity_resource(tag: &Tag, key: &str) -> Option<ResourceLocation> {
    ResourceLocation::parse(&block_entity_str_field(tag, key)?).ok()
}

fn block_entity_number(tag: &Tag, key: &str) -> Option<i64> {
    tag.get(key).and_then(nbt_int_val)
}

fn block_entity_int(tag: &Tag, key: &str) -> i32 {
    block_entity_number(tag, key)
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or_default()
}

fn block_entity_list(kind: u8, values: Vec<Tag>) -> Tag {
    Tag::List(if values.is_empty() { 0 } else { kind }, values)
}

fn block_entity_entries<'a>(tag: &'a Tag, key: &str) -> &'a [Tag] {
    tag.get(key).and_then(Tag::list).unwrap_or_default()
}

fn block_entity_int_array<'a>(tag: &'a Tag, key: &str) -> &'a [i32] {
    match tag.get(key) {
        Some(Tag::IntArray(values)) => values,
        _ => &[],
    }
}

fn block_entity_compound_field(tag: &Tag, key: &str) -> Option<Tag> {
    tag.get(key)
        .filter(|value| matches!(value, Tag::Compound(_)))
        .cloned()
}

/// Keep the item component patch as NBT so book contents, custom songs, and other
/// components survive persistence independently of the protocol inventory codec.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BlockEntityItem {
    pub(crate) id: ResourceLocation,
    count: u8,
    pub(crate) components: BlockEntityNbtFields,
}

impl BlockEntityItem {
    pub(crate) fn new(id: &str, count: u8) -> Option<Self> {
        let id = ResourceLocation::parse(id).ok()?;
        if id.path() == "air" && id.namespace() == "minecraft" {
            return None;
        }
        (1..=99).contains(&count).then_some(Self {
            id,
            count,
            components: BlockEntityNbtFields::new(),
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn count(&self) -> u8 {
        self.count
    }

    fn vanilla(&self, names: &[&str]) -> bool {
        self.id.namespace() == "minecraft" && names.contains(&self.id.path())
    }

    fn same_item(&self, other: &Self) -> bool {
        self.id == other.id && self.components == other.components
    }

    fn take(source: &mut Option<Self>, count: u8) -> Option<Self> {
        let item = source.as_mut()?;
        let count = count.min(item.count);
        if count == item.count {
            return source.take();
        }
        let mut split = item.clone();
        split.count = count;
        item.count -= count;
        Some(split)
    }

    pub(crate) fn from_nbt(tag: &Tag) -> Option<Self> {
        let count = u8::try_from(block_entity_number(tag, "count").unwrap_or(1)).ok()?;
        let mut item = Self::new(&block_entity_str_field(tag, "id")?, count)?;
        if let Some(Tag::Compound(components)) = tag.get("components") {
            item.components = components.clone();
        }
        Some(item)
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        Tag::Compound(self.nbt_fields())
    }

    fn nbt_fields(&self) -> BlockEntityNbtFields {
        let mut fields = BlockEntityNbtFields::new();
        block_entity_put(&mut fields, "id", block_entity_string(&self.id.to_string()));
        block_entity_put(&mut fields, "count", Tag::Int(i32::from(self.count)));
        if !self.components.is_empty() {
            block_entity_put(
                &mut fields,
                "components",
                Tag::Compound(self.components.clone()),
            );
        }
        fields
    }
}

fn block_entity_item_field(tag: &Tag, key: &str) -> Option<BlockEntityItem> {
    tag.get(key).and_then(BlockEntityItem::from_nbt)
}

fn block_entity_put_item(
    fields: &mut BlockEntityNbtFields,
    key: &str,
    item: Option<&BlockEntityItem>,
) {
    if let Some(item) = item {
        block_entity_put(fields, key, item.to_nbt());
    }
}

fn block_entity_read_slots<const N: usize>(tag: &Tag) -> [Option<BlockEntityItem>; N] {
    let mut slots = std::array::from_fn(|_| None);
    for entry in block_entity_entries(tag, "Items") {
        if let Some(slot) = block_entity_number(entry, "Slot")
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|n| slots.get_mut(n))
        {
            *slot = BlockEntityItem::from_nbt(entry);
        }
    }
    slots
}

fn block_entity_write_slots(slots: &[Option<BlockEntityItem>]) -> Tag {
    let values = slots
        .iter()
        .enumerate()
        .filter_map(|(slot, item)| {
            let mut fields = item.as_ref()?.nbt_fields();
            block_entity_put(&mut fields, "Slot", Tag::Byte(slot as i8));
            Some(Tag::Compound(fields))
        })
        .collect();
    block_entity_list(10, values)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BeaconEffect {
    Speed,
    Haste,
    Resistance,
    JumpBoost,
    Strength,
    Regeneration,
}

impl BeaconEffect {
    const IDS: [(Self, &'static str); 6] = [
        (Self::Speed, "speed"),
        (Self::Haste, "haste"),
        (Self::Resistance, "resistance"),
        (Self::JumpBoost, "jump_boost"),
        (Self::Strength, "strength"),
        (Self::Regeneration, "regeneration"),
    ];

    fn read(tag: &Tag, key: &str) -> Option<Self> {
        let id = block_entity_resource(tag, key)?;
        if id.namespace() != "minecraft" {
            return None;
        }
        Self::IDS
            .iter()
            .find(|(_, name)| *name == id.path())
            .map(|(effect, _)| *effect)
    }

    fn write(self) -> Tag {
        block_entity_string(&format!("minecraft:{}", Self::IDS[self as usize].1))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BeaconPulse {
    pub(crate) range: i32,
    pub(crate) duration: i32,
    pub(crate) primary: BeaconEffect,
    pub(crate) amplifier: u8,
    pub(crate) secondary: Option<BeaconEffect>,
}

#[derive(Debug, Default, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Beacon {
    levels: i32,
    pub(crate) primary: Option<BeaconEffect>,
    pub(crate) secondary: Option<BeaconEffect>,
    pub(crate) custom_name: Option<String>,
    pub(crate) lock: Option<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Beacon {
    pub(crate) fn levels(&self) -> i32 {
        self.levels
    }

    pub(crate) fn from_nbt(tag: &Tag) -> Self {
        Self {
            // Preserve the saved tier snapshot until the next available world scan.
            levels: block_entity_int(tag, "Levels").clamp(0, 4),
            primary: BeaconEffect::read(tag, "primary_effect"),
            secondary: BeaconEffect::read(tag, "secondary_effect"),
            custom_name: block_entity_str_field(tag, "CustomName"),
            lock: block_entity_str_field(tag, "Lock"),
        }
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        block_entity_put(&mut fields, "Levels", Tag::Int(self.levels));
        for (key, effect) in [
            ("primary_effect", self.primary),
            ("secondary_effect", self.secondary),
        ] {
            if let Some(effect) = effect {
                block_entity_put(&mut fields, key, effect.write());
            }
        }
        for (key, value) in [("CustomName", &self.custom_name), ("Lock", &self.lock)] {
            if let Some(value) = value {
                block_entity_put(&mut fields, key, block_entity_string(value));
            }
        }
        Tag::Compound(fields)
    }

    /// The menu's payment slot accepts these five items. The effect enum matches the
    /// server's allowlist; the client's tier restrictions are applied in its UI.
    pub(crate) fn confirm(
        &mut self,
        primary: Option<BeaconEffect>,
        secondary: Option<BeaconEffect>,
        payment: &mut Option<BlockEntityItem>,
    ) -> bool {
        if !payment.as_ref().is_some_and(|item| {
            item.vanilla(&[
                "netherite_ingot",
                "emerald",
                "diamond",
                "gold_ingot",
                "iron_ingot",
            ])
        }) {
            return false;
        }
        BlockEntityItem::take(payment, 1);
        self.primary = primary;
        self.secondary = secondary;
        true
    }

    /// The caller supplies the beam result and checks blocks relative to the beacon.
    pub(crate) fn tick(
        &mut self,
        now: i64,
        beam_clear: bool,
        mut base_block: impl FnMut(i32, i32, i32) -> bool,
    ) -> Option<BeaconPulse> {
        if now % 80 != 0 || !beam_clear {
            return None;
        }
        self.levels = 0;
        for depth in 1..=4 {
            let complete =
                (-depth..=depth).all(|x| (-depth..=depth).all(|z| base_block(x, -depth, z)));
            if !complete {
                break;
            }
            self.levels = depth;
        }
        if self.levels == 0 {
            return None;
        }
        let primary = self.primary?;
        let secondary = self.secondary.filter(|_| self.levels == 4);
        Some(BeaconPulse {
            range: self.levels * 10 + 10,
            duration: (9 + self.levels * 2) * 20,
            primary,
            amplifier: u8::from(secondary == Some(primary)),
            secondary: secondary.filter(|&effect| effect != primary),
        })
    }
}

/// `BeaconMenu` (1.21.1): a single payment slot (`ItemTags.BEACON_PAYMENT_ITEMS`,
/// max stack size 1) plus the three `ContainerData` rows the screen reads —
/// power level, primary effect, secondary effect. `power_level` mirrors the
/// anchored `Beacon` block entity on every sync; `anchor` lets
/// `apply_live_beacon_update` write a confirmed choice back to it.
#[derive(Default)]
struct BeaconMenu {
    payment: Stack,
    anchor: Option<DimBlockPos>,
    power_level: i16,
}

/// `#minecraft:beacon_payment_items` (1.21.1): the five items the beacon's
/// payment slot and `BeaconBlockEntity.setEffects` both accept.
fn beacon_payment_item(stack: &Stack) -> bool {
    item_to_name(stack.0).is_some_and(|item| {
        matches!(
            item,
            "iron_ingot" | "gold_ingot" | "diamond" | "emerald" | "netherite_ingot"
        )
    })
}

/// `minecraft:mob_effect` registry id a beacon `ContainerData` row carries
/// (the same id space `set_beacon_effect` decodes; -1 when no effect is set).
fn beacon_effect_wire_id(effect: BeaconEffect) -> i16 {
    // name_to_effect is the internal effect id; the registry id is +1
    // (verified against `beacon_effect_from_wire`'s decoder).
    name_to_effect(BeaconEffect::IDS[effect as usize].1).unwrap() as i16 + 1
}

/// The pyramid tier that unlocks an effect on the vanilla beacon screen:
/// tier 1 Speed/Haste, tier 2 Resistance/Jump Boost, tier 3 Strength,
/// tier 4 Regeneration.
#[cfg_attr(not(test), allow(dead_code))]
fn beacon_effect_tier(effect: BeaconEffect) -> i16 {
    match effect {
        BeaconEffect::Speed | BeaconEffect::Haste => 1,
        BeaconEffect::Resistance | BeaconEffect::JumpBoost => 2,
        BeaconEffect::Strength => 3,
        BeaconEffect::Regeneration => 4,
    }
}

/// Vanilla `BeaconScreen`/`BeaconMenu` gating: a primary effect must be
/// unlocked by the pyramid's level, and a secondary exists only at level 4 —
/// either Regeneration or the primary again (which vanilla applies as Tier II).
#[cfg_attr(not(test), allow(dead_code))]
fn beacon_selection_allowed(
    power_level: i16,
    primary: Option<BeaconEffect>,
    secondary: Option<BeaconEffect>,
) -> bool {
    if power_level < 1 || (primary.is_none() && secondary.is_some()) {
        return false;
    }
    if let Some(primary) = primary {
        if beacon_effect_tier(primary) > power_level {
            return false;
        }
    }
    if let Some(secondary) = secondary {
        if power_level < 4 {
            return false;
        }
        if secondary != BeaconEffect::Regeneration && Some(secondary) != primary {
            return false;
        }
    }
    true
}

/// Applies a beacon menu confirmation (vanilla `BeaconMenu`'s effect choice
/// reaching `BeaconBlockEntity`): validates the effects against the pyramid
/// tiers, consumes one `#minecraft:beacon_payment_items` item from slot 0,
/// and updates the beacon state. Returns whether the update was applied.
#[cfg_attr(not(test), allow(dead_code))]
fn apply_live_beacon_update(
    menu: &mut BeaconMenu,
    primary_effect: Option<BeaconEffect>,
    secondary_effect: Option<BeaconEffect>,
) -> bool {
    if !beacon_payment_item(&menu.payment) {
        return false;
    }
    // The pyramid tier gates on the anchored block entity's real level so the
    // check cannot run on a stale menu mirror; the menu's own power_level is
    // the fallback for an unanchored menu.
    if let Some((dim, x, y, z)) = menu.anchor {
        let Some(reg) = breg() else { return false };
        let state = block_at_dim(dim, x, y, z);
        let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
        let Some(LiveBlockEntity::Beacon(beacon)) =
            store.at(reg, (dim, x, y, z), state)
        else {
            return false;
        };
        menu.power_level = beacon.levels() as i16;
        if !beacon_selection_allowed(menu.power_level, primary_effect, secondary_effect) {
            return false;
        }
        beacon.primary = primary_effect;
        beacon.secondary = secondary_effect;
    } else if !beacon_selection_allowed(menu.power_level, primary_effect, secondary_effect) {
        return false;
    }
    // Slot 0 pays exactly one item; `getMaxStackSize` on the vanilla slot is 1.
    menu.payment = if menu.payment.1 <= 1 {
        Stack::default()
    } else {
        (menu.payment.0, menu.payment.1 - 1, menu.payment.2.clone())
    };
    true
}

#[derive(Debug, Default, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Campfire {
    pub(crate) slots: [Option<BlockEntityItem>; 4],
    pub(crate) cooking_times: [i32; 4],
    pub(crate) cooking_total_times: [i32; 4],
}

#[cfg_attr(not(test), allow(dead_code))]
impl Campfire {
    pub(crate) fn from_nbt(tag: &Tag) -> Self {
        fn times(tag: &Tag, key: &str) -> [i32; 4] {
            let mut ticks = [0; 4];
            for (tick, &saved) in ticks.iter_mut().zip(block_entity_int_array(tag, key)) {
                *tick = saved.max(0);
            }
            ticks
        }
        Self {
            slots: block_entity_read_slots(tag),
            cooking_times: times(tag, "CookingTimes"),
            cooking_total_times: times(tag, "CookingTotalTimes"),
        }
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        block_entity_compound([
            ("Items", block_entity_write_slots(&self.slots)),
            ("CookingTimes", Tag::IntArray(self.cooking_times.to_vec())),
            (
                "CookingTotalTimes",
                Tag::IntArray(self.cooking_total_times.to_vec()),
            ),
        ])
    }

    pub(crate) fn place(
        &mut self,
        held: &mut Option<BlockEntityItem>,
        recipe_time: impl FnOnce(&BlockEntityItem) -> Option<i32>,
    ) -> Option<usize> {
        let slot = self.slots.iter().position(Option::is_none)?;
        let total = recipe_time(held.as_ref()?)?;
        self.cooking_times[slot] = 0;
        self.cooking_total_times[slot] = total.max(0);
        self.slots[slot] = BlockEntityItem::take(held, 1);
        Some(slot)
    }

    /// Return cooked stacks in slot order. If a recipe disappeared after placement,
    /// the original item is returned. The caller can reject feature-disabled outputs.
    pub(crate) fn tick(
        &mut self,
        lit: bool,
        mut cook: impl FnMut(&BlockEntityItem) -> Option<BlockEntityItem>,
        mut enabled: impl FnMut(&BlockEntityItem) -> bool,
    ) -> Vec<BlockEntityItem> {
        let mut drops = Vec::new();
        for ((slot, time), &total) in self
            .slots
            .iter_mut()
            .zip(&mut self.cooking_times)
            .zip(&self.cooking_total_times)
        {
            if !lit {
                if *time > 0 {
                    *time = (*time - 2).clamp(0, total.max(0));
                }
            } else if let Some(item) = slot.as_ref() {
                *time = time.saturating_add(1);
                if *time >= total {
                    let cooked = cook(item);
                    if enabled(cooked.as_ref().unwrap_or(item)) {
                        drops.extend(cooked.or_else(|| slot.take()));
                        *slot = None;
                    }
                }
            }
        }
        drops
    }
}

#[derive(Debug, Default, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Lectern {
    book: Option<BlockEntityItem>,
    page: i32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Lectern {
    pub(crate) fn from_nbt(tag: &Tag) -> Self {
        let mut lectern = Self {
            book: block_entity_item_field(tag, "Book"),
            page: 0,
        };
        lectern.set_page(block_entity_int(tag, "Page"));
        lectern
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        match &self.book {
            Some(book) => {
                block_entity_compound([("Book", book.to_nbt()), ("Page", Tag::Int(self.page))])
            }
            None => block_entity_compound([]),
        }
    }

    pub(crate) fn place(&mut self, held: &mut Option<BlockEntityItem>) -> bool {
        if self.book.is_some()
            || !held
                .as_ref()
                .is_some_and(|item| item.vanilla(&["writable_book", "written_book"]))
        {
            return false;
        }
        self.book = BlockEntityItem::take(held, 1);
        self.page = 0;
        true
    }

    pub(crate) fn take_book(&mut self) -> Option<BlockEntityItem> {
        self.page = 0;
        self.book.take()
    }

    pub(crate) fn page(&self) -> i32 {
        self.page
    }

    pub(crate) fn page_count(&self) -> i32 {
        let pages = self
            .book
            .as_ref()
            .and_then(|book| {
                [
                    "minecraft:written_book_content",
                    "minecraft:writable_book_content",
                ]
                .iter()
                .find_map(|key| book.components.get(&NbtString::text(key)))
            })
            .and_then(|content| content.get("pages"))
            .and_then(Tag::list);
        pages.map_or(0, |pages| pages.len() as i32)
    }

    /// True requests the two-tick page-turn pulse from the block. Vanilla's clamp
    /// returns -1 for a book with no pages, including when the saved book is empty.
    pub(crate) fn set_page(&mut self, page: i32) -> bool {
        let page = page.max(0).min(self.page_count() - 1);
        let changed = page != self.page;
        self.page = page;
        changed
    }

    pub(crate) fn comparator_signal(&self) -> i32 {
        if self.book.is_none() {
            return 0;
        }
        let pages = self.page_count();
        let fraction = if pages > 1 {
            self.page as f32 / (pages - 1) as f32
        } else {
            1.0
        };
        let valid_book = self
            .book
            .as_ref()
            .is_some_and(|book| book.vanilla(&["writable_book", "written_book"]));
        (fraction * 14.0).floor() as i32 + i32::from(valid_book)
    }
}

#[derive(Debug, Default, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ChiseledBookshelf {
    pub(crate) slots: [Option<BlockEntityItem>; 6],
    last_interacted_slot: Option<usize>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ChiseledBookshelf {
    pub(crate) fn from_nbt(tag: &Tag) -> Self {
        let slot = block_entity_number(tag, "last_interacted_slot").unwrap_or(-1);
        Self {
            slots: block_entity_read_slots(tag),
            last_interacted_slot: (0..6).contains(&slot).then_some(slot as usize),
        }
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        block_entity_compound([
            ("Items", block_entity_write_slots(&self.slots)),
            (
                "last_interacted_slot",
                Tag::Int(self.last_interacted_slot.map_or(-1, |slot| slot as i32)),
            ),
        ])
    }

    pub(crate) fn insert_book(&mut self, index: usize, held: &mut Option<BlockEntityItem>) -> bool {
        let Some(slot @ None) = self.slots.get_mut(index) else {
            return false;
        };
        if !held.as_ref().is_some_and(|item| {
            item.vanilla(&[
                "book",
                "written_book",
                "enchanted_book",
                "writable_book",
                "knowledge_book",
            ])
        }) {
            return false;
        }
        *slot = BlockEntityItem::take(held, 1);
        self.last_interacted_slot = Some(index);
        true
    }

    pub(crate) fn remove_book(&mut self, index: usize) -> Option<BlockEntityItem> {
        let book = self.slots.get_mut(index)?.take()?;
        self.last_interacted_slot = Some(index);
        Some(book)
    }

    pub(crate) fn comparator_signal(&self) -> u8 {
        self.last_interacted_slot.map_or(0, |slot| slot as u8 + 1)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlockLootTable {
    pub(crate) id: ResourceLocation,
    pub(crate) seed: i64,
}

impl BlockLootTable {
    fn from_nbt(tag: &Tag) -> Option<Self> {
        Some(Self {
            id: block_entity_resource(tag, "LootTable")?,
            seed: block_entity_number(tag, "LootTableSeed").unwrap_or_default(),
        })
    }

    fn write(&self, fields: &mut BlockEntityNbtFields) {
        block_entity_put(
            fields,
            "LootTable",
            block_entity_string(&self.id.to_string()),
        );
        if self.seed != 0 {
            block_entity_put(fields, "LootTableSeed", Tag::Long(self.seed));
        }
    }
}

#[derive(Debug, Default, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct DecoratedPot {
    // Back, left, right, front, in the order used by the PotDecorations codec.
    pub(crate) sherds: [Option<ResourceLocation>; 4],
    pub(crate) item: Option<BlockEntityItem>,
    pub(crate) loot_table: Option<BlockLootTable>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl DecoratedPot {
    pub(crate) fn from_nbt(tag: &Tag) -> Self {
        let mut pot = Self {
            loot_table: BlockLootTable::from_nbt(tag),
            ..Self::default()
        };
        if pot.loot_table.is_none() {
            pot.item = block_entity_item_field(tag, "item");
        }
        let sherds = block_entity_entries(tag, "sherds");
        if sherds.len() <= 4 {
            let decorations: Option<Vec<_>> = sherds
                .iter()
                .map(|tag| ResourceLocation::parse(&tag.string()?.to_utf8().ok()?).ok())
                .collect();
            if let Some(decorations) = decorations {
                for (slot, id) in pot.sherds.iter_mut().zip(decorations) {
                    *slot = (id.namespace() != "minecraft" || id.path() != "brick").then_some(id);
                }
            }
        }
        pot
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        if self.sherds.iter().any(Option::is_some) {
            block_entity_put(
                &mut fields,
                "sherds",
                block_entity_list(
                    8,
                    self.sherds
                        .iter()
                        .map(|side| {
                            block_entity_string(
                                &side
                                    .as_ref()
                                    .map_or_else(|| "minecraft:brick".into(), ToString::to_string),
                            )
                        })
                        .collect(),
                ),
            );
        }
        if let Some(loot) = &self.loot_table {
            loot.write(&mut fields);
        } else {
            block_entity_put_item(&mut fields, "item", self.item.as_ref());
        }
        Tag::Compound(fields)
    }

    /// Insert one item. The caller supplies the registry's effective stack limit.
    pub(crate) fn insert(&mut self, held: &mut Option<BlockEntityItem>, stack_limit: u8) -> bool {
        let Some(source) = held.as_ref() else {
            return false;
        };
        if self.loot_table.is_some()
            || stack_limit == 0
            || self
                .item
                .as_ref()
                .is_some_and(|item| !item.same_item(source) || item.count >= stack_limit.min(99))
        {
            return false;
        }
        let Some(added) = BlockEntityItem::take(held, 1) else {
            return false;
        };
        if let Some(item) = &mut self.item {
            item.count += 1;
        } else {
            self.item = Some(added);
        }
        true
    }

    pub(crate) fn comparator_signal(&self, stack_limit: u8) -> u8 {
        let Some(item) = &self.item else {
            return 0;
        };
        ((f32::from(item.count) / f32::from(stack_limit.clamp(1, 99))).min(1.0) * 14.0).floor()
            as u8
            + 1
    }
}

#[derive(Debug, Default, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Crafter {
    slots: [Option<BlockEntityItem>; 9],
    disabled: [bool; 9],
    pub(crate) crafting_ticks_remaining: i32,
    pub(crate) triggered: bool,
    pub(crate) loot_table: Option<BlockLootTable>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Crafter {
    pub(crate) fn from_nbt(tag: &Tag) -> Self {
        let loot_table = BlockLootTable::from_nbt(tag);
        let mut crafter = Self {
            slots: if loot_table.is_none() {
                block_entity_read_slots(tag)
            } else {
                Default::default()
            },
            crafting_ticks_remaining: block_entity_int(tag, "crafting_ticks_remaining").max(0),
            triggered: block_entity_int(tag, "triggered") == 1,
            loot_table,
            ..Self::default()
        };
        for &slot in block_entity_int_array(tag, "disabled_slots") {
            if let Ok(slot) = usize::try_from(slot) {
                crafter.set_slot_disabled(slot, true);
            }
        }
        crafter
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        block_entity_put(
            &mut fields,
            "crafting_ticks_remaining",
            Tag::Int(self.crafting_ticks_remaining),
        );
        block_entity_put(
            &mut fields,
            "triggered",
            Tag::Int(i32::from(self.triggered)),
        );
        let disabled = self
            .disabled
            .iter()
            .enumerate()
            .filter_map(|(slot, &disabled)| disabled.then_some(slot as i32))
            .collect();
        block_entity_put(&mut fields, "disabled_slots", Tag::IntArray(disabled));
        if let Some(loot) = &self.loot_table {
            loot.write(&mut fields);
        } else {
            block_entity_put(&mut fields, "Items", block_entity_write_slots(&self.slots));
        }
        Tag::Compound(fields)
    }

    pub(crate) fn slots(&self) -> &[Option<BlockEntityItem>; 9] {
        &self.slots
    }

    pub(crate) fn set_slot_disabled(&mut self, index: usize, disabled: bool) -> bool {
        if !matches!(self.slots.get(index), Some(None)) {
            return false;
        }
        self.disabled[index] = disabled;
        true
    }

    pub(crate) fn set_slot(&mut self, index: usize, item: Option<BlockEntityItem>) -> bool {
        let Some(slot) = self.slots.get_mut(index) else {
            return false;
        };
        if self.loot_table.is_some() {
            return false;
        }
        self.disabled[index] = false;
        *slot = item;
        true
    }

    /// Automation prefers empty enabled slots, then spreads matching stacks by count.
    pub(crate) fn insert(
        &mut self,
        held: &mut Option<BlockEntityItem>,
        stack_limit: u8,
    ) -> Option<usize> {
        if self.loot_table.is_some() || stack_limit == 0 {
            return None;
        }
        let source = held.as_ref()?;
        let index = self
            .slots
            .iter()
            .zip(self.disabled)
            .enumerate()
            .filter_map(|(index, (slot, disabled))| {
                if disabled {
                    return None;
                }
                let count = match slot {
                    None => 0,
                    Some(item) if item.same_item(source) && item.count < stack_limit.min(99) => {
                        item.count
                    }
                    _ => return None,
                };
                Some((count, index))
            })
            .min()?
            .1;
        let added = BlockEntityItem::take(held, 1)?;
        if let Some(item) = &mut self.slots[index] {
            item.count += 1;
        } else {
            self.slots[index] = Some(added);
        }
        Some(index)
    }

    pub(crate) fn comparator_signal(&self) -> u8 {
        self.slots
            .iter()
            .zip(self.disabled)
            .filter(|(item, disabled)| *disabled || item.is_some())
            .count() as u8
    }

    /// A rising redstone edge schedules one block tick four ticks later.
    pub(crate) fn set_powered(&mut self, powered: bool) -> Option<i32> {
        let previous = std::mem::replace(&mut self.triggered, powered);
        (powered && !previous).then_some(4)
    }

    /// Execute the scheduled craft. The caller routes the result and crafting
    /// remainders to the facing inventory or ejects them.
    pub(crate) fn craft(
        &mut self,
        recipe: impl FnOnce(
            &[Option<BlockEntityItem>; 9],
        ) -> Option<(BlockEntityItem, Vec<BlockEntityItem>)>,
    ) -> Option<(BlockEntityItem, Vec<BlockEntityItem>)> {
        if self.loot_table.is_some() || self.slots.iter().all(Option::is_none) {
            return None;
        }
        let result = recipe(&self.slots)?;
        for slot in &mut self.slots {
            BlockEntityItem::take(slot, 1);
        }
        self.crafting_ticks_remaining = 6;
        Some(result)
    }

    /// True when the block should clear its crafting animation.
    pub(crate) fn tick(&mut self) -> bool {
        if self.crafting_ticks_remaining <= 0 {
            return false;
        }
        self.crafting_ticks_remaining -= 1;
        self.crafting_ticks_remaining == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JukeboxSong {
    pub(crate) length_in_ticks: i32,
    pub(crate) comparator_output: u8,
}

impl JukeboxSong {
    fn playback_end(self) -> i64 {
        i64::from(self.length_in_ticks.max(0)) + 20
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JukeboxTick {
    MusicNote,
    Stopped,
}

#[derive(Debug, Default, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Jukebox {
    record: Option<BlockEntityItem>,
    playback: Option<(i64, JukeboxSong)>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Jukebox {
    pub(crate) fn from_nbt(
        tag: &Tag,
        song: impl FnOnce(&BlockEntityItem) -> Option<JukeboxSong>,
    ) -> Self {
        let record = block_entity_item_field(tag, "RecordItem");
        let playback = match (record.as_ref(), tag.get("ticks_since_song_started")) {
            (Some(item), Some(Tag::Long(ticks))) => song(item)
                .filter(|song| *ticks >= 0 && *ticks < song.playback_end())
                .map(|song| (*ticks, song)),
            _ => None,
        };
        Self { record, playback }
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        block_entity_put_item(&mut fields, "RecordItem", self.record.as_ref());
        if let Some((ticks, _)) = self.playback {
            block_entity_put(&mut fields, "ticks_since_song_started", Tag::Long(ticks));
        }
        Tag::Compound(fields)
    }

    /// Resolve established record songs through the registry. An unresolved
    /// jukebox_playable component still permits insertion without playback.
    pub(crate) fn insert(
        &mut self,
        held: &mut Option<BlockEntityItem>,
        song: impl FnOnce(&BlockEntityItem) -> Option<JukeboxSong>,
    ) -> bool {
        let Some(item) = held.as_ref().filter(|_| self.record.is_none()) else {
            return false;
        };
        let song = song(item);
        if song.is_none()
            && !item
                .components
                .contains_key(&NbtString::text("minecraft:jukebox_playable"))
        {
            return false;
        }
        self.record = BlockEntityItem::take(held, 1);
        self.playback = song.map(|song| (0, song));
        true
    }

    pub(crate) fn eject(&mut self) -> Option<BlockEntityItem> {
        self.playback = None;
        self.record.take()
    }

    pub(crate) fn comparator_signal(
        &self,
        song: impl FnOnce(&BlockEntityItem) -> Option<JukeboxSong>,
    ) -> u8 {
        self.record
            .as_ref()
            .and_then(song)
            .map_or(0, |song| song.comparator_output.min(15))
    }

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn playing_ticks(&self) -> Option<i64> {
        self.playback.map(|(ticks, _)| ticks)
    }

    pub(crate) fn tick(&mut self) -> Option<JukeboxTick> {
        let (ticks, song) = self.playback.as_mut()?;
        if *ticks >= song.playback_end() {
            self.playback = None;
            return Some(JukeboxTick::Stopped);
        }
        let note = *ticks % 20 == 0;
        *ticks += 1;
        note.then_some(JukeboxTick::MusicNote)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BannerColor {
    White,
    Orange,
    Magenta,
    LightBlue,
    Yellow,
    Lime,
    Pink,
    Gray,
    LightGray,
    Cyan,
    Purple,
    Blue,
    Brown,
    Green,
    Red,
    Black,
}

impl BannerColor {
    const IDS: [(Self, &'static str); 16] = [
        (Self::White, "white"),
        (Self::Orange, "orange"),
        (Self::Magenta, "magenta"),
        (Self::LightBlue, "light_blue"),
        (Self::Yellow, "yellow"),
        (Self::Lime, "lime"),
        (Self::Pink, "pink"),
        (Self::Gray, "gray"),
        (Self::LightGray, "light_gray"),
        (Self::Cyan, "cyan"),
        (Self::Purple, "purple"),
        (Self::Blue, "blue"),
        (Self::Brown, "brown"),
        (Self::Green, "green"),
        (Self::Red, "red"),
        (Self::Black, "black"),
    ];

    fn from_nbt(tag: &Tag) -> Option<Self> {
        let color = block_entity_str_field(tag, "color")?;
        Self::IDS
            .iter()
            .find(|(_, name)| *name == color)
            .map(|(value, _)| *value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BannerPattern {
    pub(crate) pattern: ResourceLocation,
    pub(crate) color: BannerColor,
}

#[derive(Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Banner {
    pub(crate) patterns: Vec<BannerPattern>,
    pub(crate) custom_name: Option<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Banner {
    pub(crate) fn from_nbt(tag: &Tag) -> Self {
        let patterns = block_entity_entries(tag, "patterns")
            .iter()
            .filter_map(|layer| {
                Some(BannerPattern {
                    pattern: block_entity_resource(layer, "pattern")?,
                    color: BannerColor::from_nbt(layer)?,
                })
            })
            .collect();
        Self {
            patterns,
            custom_name: block_entity_str_field(tag, "CustomName"),
        }
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        if !self.patterns.is_empty() {
            let patterns = self
                .patterns
                .iter()
                .map(|layer| {
                    block_entity_compound([
                        ("pattern", block_entity_string(&layer.pattern.to_string())),
                        (
                            "color",
                            block_entity_string(BannerColor::IDS[layer.color as usize].1),
                        ),
                    ])
                })
                .collect();
            block_entity_put(&mut fields, "patterns", block_entity_list(10, patterns));
        }
        if let Some(name) = &self.custom_name {
            block_entity_put(&mut fields, "CustomName", block_entity_string(name));
        }
        Tag::Compound(fields)
    }

    /// Washing removes the last layer.
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn remove_last_pattern(&mut self) -> Option<BannerPattern> {
        self.patterns.pop()
    }
}

type BlockEntityUuid = [u8; 16];

fn block_entity_uuid(tag: &Tag) -> Option<BlockEntityUuid> {
    let Tag::IntArray(parts) = tag else {
        return None;
    };
    let parts: &[i32; 4] = parts.as_slice().try_into().ok()?;
    let mut uuid = [0; 16];
    for (bytes, word) in uuid.as_chunks_mut::<4>().0.iter_mut().zip(parts) {
        bytes.copy_from_slice(&word.to_be_bytes());
    }
    Some(uuid)
}

fn block_entity_uuid_nbt(uuid: &BlockEntityUuid) -> Tag {
    Tag::IntArray(
        uuid.as_chunks::<4>().0.iter()
            .map(|bytes| i32::from_be_bytes(*bytes))
            .collect(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfileProperty {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) signature: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SkullProfile {
    pub(crate) name: Option<String>,
    pub(crate) id: Option<BlockEntityUuid>,
    pub(crate) properties: Vec<ProfileProperty>,
}

impl SkullProfile {
    fn from_nbt(tag: &Tag) -> Option<Self> {
        if let Some(name) = tag.string() {
            return Some(Self {
                name: Some(name.to_utf8().ok()?),
                ..Self::default()
            });
        }
        let Tag::Compound(_) = tag else {
            return None;
        };
        let mut properties = Vec::new();
        match tag.get("properties") {
            Some(Tag::Compound(groups)) => {
                for (name, values) in groups {
                    let name = name.to_utf8().ok()?;
                    for value in values.list()? {
                        properties.push(ProfileProperty {
                            name: name.clone(),
                            value: value.string()?.to_utf8().ok()?,
                            signature: None,
                        });
                    }
                }
            }
            Some(Tag::List(_, values)) => {
                for value in values {
                    properties.push(ProfileProperty {
                        name: block_entity_str_field(value, "name")?,
                        value: block_entity_str_field(value, "value")?,
                        signature: block_entity_str_field(value, "signature"),
                    });
                }
            }
            Some(_) => return None,
            None => {}
        }
        Some(Self {
            name: block_entity_str_field(tag, "name"),
            id: tag.get("id").and_then(block_entity_field_uuid),
            properties,
        })
    }

    fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        if let Some(name) = &self.name {
            block_entity_put(&mut fields, "name", block_entity_string(name));
        }
        if let Some(id) = self.id {
            block_entity_put(&mut fields, "id", block_entity_uuid_nbt(&id));
        }
        if !self.properties.is_empty() {
            let properties = self
                .properties
                .iter()
                .map(|property| {
                    let mut fields = BlockEntityNbtFields::new();
                    block_entity_put(&mut fields, "name", block_entity_string(&property.name));
                    block_entity_put(&mut fields, "value", block_entity_string(&property.value));
                    if let Some(signature) = &property.signature {
                        block_entity_put(&mut fields, "signature", block_entity_string(signature));
                    }
                    Tag::Compound(fields)
                })
                .collect();
            block_entity_put(&mut fields, "properties", block_entity_list(10, properties));
        }
        Tag::Compound(fields)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn needs_resolution(&self) -> bool {
        self.properties.is_empty() && self.name.is_some() != self.id.is_some()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Skull {
    pub(crate) note_block_sound: Option<ResourceLocation>,
    pub(crate) profile: Option<SkullProfile>,
    pub(crate) custom_name: Option<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Skull {
    pub(crate) fn from_nbt(tag: &Tag) -> Self {
        Self {
            note_block_sound: block_entity_resource(tag, "note_block_sound"),
            profile: tag.get("profile").and_then(SkullProfile::from_nbt),
            custom_name: block_entity_str_field(tag, "custom_name"),
        }
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        if let Some(sound) = &self.note_block_sound {
            block_entity_put(
                &mut fields,
                "note_block_sound",
                block_entity_string(&sound.to_string()),
            );
        }
        if let Some(profile) = &self.profile {
            block_entity_put(&mut fields, "profile", profile.to_nbt());
        }
        if let Some(name) = &self.custom_name {
            block_entity_put(&mut fields, "custom_name", block_entity_string(name));
        }
        Tag::Compound(fields)
    }
}

#[derive(Debug, Default, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Brushable {
    pub(crate) item: Option<BlockEntityItem>,
    pub(crate) loot_table: Option<BlockLootTable>,
    pub(crate) hit_direction: Option<Face>,
    brushes: u8,
    next_brush_at: i64,
    decay_at: i64,
    completed: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) enum BrushResult {
    CoolingDown,
    Progress {
        dusted: u8,
    },
    Completed {
        loot: Option<BlockEntityItem>,
        direction: Face,
    },
}

#[cfg_attr(not(test), allow(dead_code))]
impl Brushable {
    // Direction ordinal order in the vanilla client update tag.
    const DIRECTIONS: [Face; 6] = [
        Face::Down,
        Face::Up,
        Face::North,
        Face::South,
        Face::West,
        Face::East,
    ];

    pub(crate) fn from_nbt(tag: &Tag) -> Self {
        let loot_table = BlockLootTable::from_nbt(tag);
        let item = if loot_table.is_none() {
            block_entity_item_field(tag, "item")
        } else {
            None
        };
        let hit_direction = block_entity_number(tag, "hit_direction")
            .and_then(|value| usize::try_from(value).ok())
            .and_then(|index| Self::DIRECTIONS.get(index))
            .copied();
        Self {
            item,
            loot_table,
            hit_direction,
            ..Self::default()
        }
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        if let Some(loot) = &self.loot_table {
            loot.write(&mut fields);
        } else {
            block_entity_put_item(&mut fields, "item", self.item.as_ref());
        }
        Tag::Compound(fields)
    }

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn update_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        if let Some(index) = Self::DIRECTIONS
            .iter()
            .position(|&face| Some(face) == self.hit_direction)
        {
            block_entity_put(&mut fields, "hit_direction", Tag::Int(index as i32));
        }
        block_entity_put_item(&mut fields, "item", self.item.as_ref());
        Tag::Compound(fields)
    }

    pub(crate) fn brush(
        &mut self,
        now: i64,
        face: Face,
        roll: impl FnOnce(&BlockLootTable) -> Option<BlockEntityItem>,
    ) -> BrushResult {
        if self.completed {
            return BrushResult::CoolingDown;
        }
        let direction = *self.hit_direction.get_or_insert(face);
        self.decay_at = now.saturating_add(40);
        if now < self.next_brush_at {
            return BrushResult::CoolingDown;
        }
        self.next_brush_at = now.saturating_add(10);
        if let Some(table) = self.loot_table.take() {
            self.item = roll(&table);
        }
        self.brushes += 1;
        if self.brushes == 10 {
            self.completed = true;
            BrushResult::Completed {
                loot: self.item.take(),
                direction,
            }
        } else {
            BrushResult::Progress {
                dusted: self.dusted(),
            }
        }
    }

    pub(crate) fn dusted(&self) -> u8 {
        match self.brushes {
            0 => 0,
            1..=2 => 1,
            3..=5 => 2,
            _ => 3,
        }
    }

    /// Returns a blockstate update when the visible dust level changes.
    pub(crate) fn tick(&mut self, now: i64) -> Option<u8> {
        if self.completed || (self.brushes != 0 && now < self.decay_at) {
            return None;
        }
        let before = self.dusted();
        self.brushes = self.brushes.saturating_sub(2);
        self.decay_at = now.saturating_add(4);
        if self.brushes == 0 {
            self.hit_direction = None;
            self.decay_at = 0;
            self.next_brush_at = 0;
        }
        let after = self.dusted();
        (after != before).then_some(after)
    }
}

fn block_entity_read_uuids(tag: &Tag, key: &str) -> Vec<BlockEntityUuid> {
    let mut seen = std::collections::HashSet::new();
    block_entity_entries(tag, key)
        .iter()
        .filter_map(block_entity_field_uuid)
        .filter(|id| seen.insert(*id))
        .collect()
}

fn block_entity_put_uuids(fields: &mut BlockEntityNbtFields, key: &str, uuids: &[BlockEntityUuid]) {
    if !uuids.is_empty() {
        block_entity_put(
            fields,
            key,
            block_entity_list(11, uuids.iter().map(block_entity_uuid_nbt).collect()),
        );
    }
}

#[derive(Debug, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct TrialSpawner {
    // The normal/ominous configs and SpawnData have their own codecs. Keep their
    // nested settings intact while exposing the counters needed for game ticks.
    pub(crate) normal_config: Option<Tag>,
    pub(crate) ominous_config: Option<Tag>,
    pub(crate) spawn_data: Option<Tag>,
    pub(crate) ejecting_loot_table: Option<ResourceLocation>,
    pub(crate) registered_players: Vec<BlockEntityUuid>,
    pub(crate) current_mobs: Vec<BlockEntityUuid>,
    pub(crate) cooldown_ends_at: i64,
    pub(crate) next_mob_spawns_at: i64,
    pub(crate) total_mobs_spawned: i32,
    pub(crate) target_cooldown_length: i32,
    pub(crate) required_player_range: i32,
}

impl Default for TrialSpawner {
    fn default() -> Self {
        Self {
            normal_config: None,
            ominous_config: None,
            spawn_data: None,
            ejecting_loot_table: None,
            registered_players: Vec::new(),
            current_mobs: Vec::new(),
            cooldown_ends_at: 0,
            next_mob_spawns_at: 0,
            total_mobs_spawned: 0,
            target_cooldown_length: 36_000,
            required_player_range: 14,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl TrialSpawner {
    pub(crate) fn from_nbt(tag: &Tag) -> Self {
        let defaults = Self::default();
        Self {
            normal_config: block_entity_compound_field(tag, "normal_config"),
            ominous_config: block_entity_compound_field(tag, "ominous_config"),
            spawn_data: block_entity_compound_field(tag, "spawn_data"),
            ejecting_loot_table: block_entity_resource(tag, "ejecting_loot_table"),
            registered_players: block_entity_read_uuids(tag, "registered_players"),
            current_mobs: block_entity_read_uuids(tag, "current_mobs"),
            cooldown_ends_at: block_entity_number(tag, "cooldown_ends_at").unwrap_or_default(),
            next_mob_spawns_at: block_entity_number(tag, "next_mob_spawns_at").unwrap_or_default(),
            total_mobs_spawned: block_entity_int(tag, "total_mobs_spawned").max(0),
            target_cooldown_length: tag
                .get("target_cooldown_length")
                .and_then(Tag::int)
                .filter(|&value| value >= 0)
                .unwrap_or(defaults.target_cooldown_length),
            required_player_range: tag
                .get("required_player_range")
                .and_then(Tag::int)
                .filter(|value| (1..=128).contains(value))
                .unwrap_or(defaults.required_player_range),
        }
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let defaults = Self::default();
        let mut fields = BlockEntityNbtFields::new();
        for (key, value) in [
            ("normal_config", &self.normal_config),
            ("ominous_config", &self.ominous_config),
            ("spawn_data", &self.spawn_data),
        ] {
            if let Some(value) = value {
                block_entity_put(&mut fields, key, value.clone());
            }
        }
        if let Some(table) = &self.ejecting_loot_table {
            block_entity_put(
                &mut fields,
                "ejecting_loot_table",
                block_entity_string(&table.to_string()),
            );
        }
        block_entity_put_uuids(&mut fields, "registered_players", &self.registered_players);
        block_entity_put_uuids(&mut fields, "current_mobs", &self.current_mobs);
        for (key, value) in [
            ("cooldown_ends_at", self.cooldown_ends_at),
            ("next_mob_spawns_at", self.next_mob_spawns_at),
        ] {
            if value != 0 {
                block_entity_put(&mut fields, key, Tag::Long(value));
            }
        }
        for (key, value, default) in [
            ("total_mobs_spawned", self.total_mobs_spawned, 0),
            (
                "target_cooldown_length",
                self.target_cooldown_length,
                defaults.target_cooldown_length,
            ),
            (
                "required_player_range",
                self.required_player_range,
                defaults.required_player_range,
            ),
        ] {
            if value != default {
                block_entity_put(&mut fields, key, Tag::Int(value));
            }
        }
        Tag::Compound(fields)
    }

    /// Record a successful world spawn while the caller's active encounter has capacity.
    pub(crate) fn record_spawn(&mut self, now: i64, interval: i32, mob: BlockEntityUuid) -> bool {
        if now < self.next_mob_spawns_at || self.current_mobs.contains(&mob) {
            return false;
        }
        self.current_mobs.push(mob);
        self.total_mobs_spawned = self.total_mobs_spawned.saturating_add(1);
        self.next_mob_spawns_at = now.saturating_add(i64::from(interval.max(0)));
        true
    }

    pub(crate) fn remove_mob(&mut self, now: i64, interval: i32, mob: BlockEntityUuid) -> bool {
        let Some(index) = self.current_mobs.iter().position(|&id| id == mob) else {
            return false;
        };
        self.current_mobs.swap_remove(index);
        self.next_mob_spawns_at = now.saturating_add(i64::from(interval.max(0)));
        true
    }

    /// The caller determines whether the encounter's spawn quota has been reached.
    pub(crate) fn begin_rewards(&mut self, now: i64) -> bool {
        if !self.current_mobs.is_empty() {
            return false;
        }
        self.cooldown_ends_at = now.saturating_add(i64::from(self.target_cooldown_length));
        self.total_mobs_spawned = 0;
        self.next_mob_spawns_at = 0;
        true
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum VaultPhase {
    #[default]
    Inactive,
    Active,
    Unlocking,
    Ejecting,
}

#[derive(Debug, PartialEq)]
pub(crate) struct VaultConfig {
    pub(crate) loot_table: ResourceLocation,
    pub(crate) activation_range: f64,
    pub(crate) deactivation_range: f64,
    pub(crate) key_item: Option<BlockEntityItem>,
    pub(crate) override_loot_table_to_display: Option<ResourceLocation>,
}

fn block_entity_double_field(tag: &Tag, key: &str) -> Option<f64> {
    match tag.get(key)? {
        Tag::Double(bits) => Some(f64::from_bits(*bits)),
        Tag::Float(bits) => Some(f64::from(f32::from_bits(*bits))),
        value => nbt_int_val(value).map(|n| n as f64),
    }
    .filter(|value| value.is_finite())
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            loot_table: ResourceLocation::new("minecraft", "chests/trial_chambers/reward")
                .expect("valid built-in resource"),
            activation_range: 4.0,
            deactivation_range: 4.5,
            key_item: BlockEntityItem::new("minecraft:trial_key", 1),
            override_loot_table_to_display: None,
        }
    }
}

impl VaultConfig {
    fn from_nbt(tag: &Tag) -> Self {
        let defaults = Self::default();
        let activation_range =
            block_entity_double_field(tag, "activation_range").unwrap_or(defaults.activation_range);
        let deactivation_range = block_entity_double_field(tag, "deactivation_range")
            .unwrap_or(defaults.deactivation_range);
        if activation_range > deactivation_range {
            return defaults;
        }
        Self {
            loot_table: block_entity_resource(tag, "loot_table").unwrap_or(defaults.loot_table),
            activation_range,
            deactivation_range,
            key_item: block_entity_item_field(tag, "key_item"),
            override_loot_table_to_display: block_entity_resource(
                tag,
                "override_loot_table_to_display",
            ),
        }
    }

    fn to_nbt(&self) -> Tag {
        let defaults = Self::default();
        let mut fields = BlockEntityNbtFields::new();
        if self.loot_table != defaults.loot_table {
            block_entity_put(
                &mut fields,
                "loot_table",
                block_entity_string(&self.loot_table.to_string()),
            );
        }
        for (key, value, default) in [
            (
                "activation_range",
                self.activation_range,
                defaults.activation_range,
            ),
            (
                "deactivation_range",
                self.deactivation_range,
                defaults.deactivation_range,
            ),
        ] {
            if value != default {
                block_entity_put(&mut fields, key, Tag::Double(value.to_bits()));
            }
        }
        block_entity_put_item(&mut fields, "key_item", self.key_item.as_ref());
        if let Some(table) = &self.override_loot_table_to_display {
            block_entity_put(
                &mut fields,
                "override_loot_table_to_display",
                block_entity_string(&table.to_string()),
            );
        }
        Tag::Compound(fields)
    }
}

#[derive(Debug, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Vault {
    // The phase belongs to the blockstate and is supplied when loading the payload.
    pub(crate) phase: VaultPhase,
    pub(crate) config: VaultConfig,
    pub(crate) rewarded_players: Vec<BlockEntityUuid>,
    pub(crate) connected_players: Vec<BlockEntityUuid>,
    pub(crate) display_item: Option<BlockEntityItem>,
    pub(crate) connected_particles_range: f64,
    pub(crate) state_updating_resumes_at: i64,
    pub(crate) items_to_eject: Vec<BlockEntityItem>,
    pub(crate) total_ejections_needed: i32,
}

impl Default for Vault {
    fn default() -> Self {
        Self {
            phase: VaultPhase::Inactive,
            config: VaultConfig::default(),
            rewarded_players: Vec::new(),
            connected_players: Vec::new(),
            display_item: None,
            connected_particles_range: 4.5,
            state_updating_resumes_at: 0,
            items_to_eject: Vec::new(),
            total_ejections_needed: 0,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl Vault {
    pub(crate) fn from_nbt(tag: &Tag, phase: VaultPhase) -> Self {
        let empty = Tag::End;
        let server = tag.get("server_data").unwrap_or(&empty);
        let shared = tag.get("shared_data").unwrap_or(&empty);
        Self {
            phase,
            config: tag
                .get("config")
                .filter(|tag| matches!(tag, Tag::Compound(_)))
                .map(VaultConfig::from_nbt)
                .unwrap_or_default(),
            rewarded_players: block_entity_read_uuids(server, "rewarded_players"),
            connected_players: block_entity_read_uuids(shared, "connected_players"),
            display_item: block_entity_item_field(shared, "display_item"),
            connected_particles_range: block_entity_double_field(
                shared,
                "connected_particles_range",
            )
            .unwrap_or(4.5),
            state_updating_resumes_at: block_entity_number(server, "state_updating_resumes_at")
                .unwrap_or_default(),
            items_to_eject: block_entity_entries(server, "items_to_eject")
                .iter()
                .filter_map(BlockEntityItem::from_nbt)
                .collect(),
            total_ejections_needed: block_entity_int(server, "total_ejections_needed"),
        }
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut server = BlockEntityNbtFields::new();
        block_entity_put_uuids(&mut server, "rewarded_players", &self.rewarded_players);
        if self.state_updating_resumes_at != 0 {
            block_entity_put(
                &mut server,
                "state_updating_resumes_at",
                Tag::Long(self.state_updating_resumes_at),
            );
        }
        if !self.items_to_eject.is_empty() {
            block_entity_put(
                &mut server,
                "items_to_eject",
                block_entity_list(
                    10,
                    self.items_to_eject
                        .iter()
                        .map(BlockEntityItem::to_nbt)
                        .collect(),
                ),
            );
        }
        if self.total_ejections_needed != 0 {
            block_entity_put(
                &mut server,
                "total_ejections_needed",
                Tag::Int(self.total_ejections_needed),
            );
        }

        let mut shared = BlockEntityNbtFields::new();
        block_entity_put_uuids(&mut shared, "connected_players", &self.connected_players);
        block_entity_put_item(&mut shared, "display_item", self.display_item.as_ref());
        if self.connected_particles_range != 4.5 {
            block_entity_put(
                &mut shared,
                "connected_particles_range",
                Tag::Double(self.connected_particles_range.to_bits()),
            );
        }
        block_entity_compound([
            ("config", self.config.to_nbt()),
            ("server_data", Tag::Compound(server)),
            ("shared_data", Tag::Compound(shared)),
        ])
    }

    pub(crate) fn try_unlock(
        &mut self,
        now: i64,
        player: BlockEntityUuid,
        held: &mut Option<BlockEntityItem>,
        roll: impl FnOnce(&ResourceLocation) -> Vec<BlockEntityItem>,
    ) -> bool {
        let Some(key) = self.config.key_item.as_ref() else {
            return false;
        };
        if self.phase != VaultPhase::Active
            || (self.config.loot_table.namespace() == "minecraft"
                && self.config.loot_table.path() == "empty")
            || self.rewarded_players.contains(&player)
            || !held
                .as_ref()
                .is_some_and(|held| held.same_item(key) && held.count >= key.count)
        {
            return false;
        }
        let items = roll(&self.config.loot_table);
        if items.is_empty() {
            return false;
        }
        BlockEntityItem::take(held, key.count);
        self.display_item = items.last().cloned();
        self.total_ejections_needed = items.len() as i32;
        self.items_to_eject = items;
        self.rewarded_players.push(player);
        evict_oldest_front(&mut self.rewarded_players, 128, |_| {});
        self.connected_players
            .retain(|&connected| connected != player);
        self.phase = VaultPhase::Unlocking;
        self.state_updating_resumes_at = now.saturating_add(14);
        true
    }

    /// The caller detects nearby players within the requested radius and handles
    /// the display preview and world effects on a phase change.
    pub(crate) fn tick(
        &mut self,
        now: i64,
        nearby: impl FnOnce(f64) -> Vec<BlockEntityUuid>,
    ) -> Option<BlockEntityItem> {
        if now < self.state_updating_resumes_at {
            return None;
        }
        self.state_updating_resumes_at = now.saturating_add(20);
        match self.phase {
            VaultPhase::Unlocking => self.phase = VaultPhase::Ejecting,
            VaultPhase::Ejecting if !self.items_to_eject.is_empty() => {
                let item = self.items_to_eject.pop();
                self.display_item = self.items_to_eject.last().cloned();
                return item;
            }
            _ => {
                let range = if self.phase == VaultPhase::Inactive {
                    self.config.activation_range
                } else {
                    self.config.deactivation_range
                };
                self.total_ejections_needed = 0;
                let mut seen = std::collections::HashSet::new();
                self.connected_players = nearby(range)
                    .into_iter()
                    .filter(|id| !self.rewarded_players.contains(id) && seen.insert(*id))
                    .collect();
                self.phase = if self.connected_players.is_empty() {
                    self.display_item = None;
                    VaultPhase::Inactive
                } else {
                    VaultPhase::Active
                };
            }
        }
        None
    }
}

// The block identity owns the payload. Property changes (including updates made
// by these entities) keep the same entry.
#[derive(Debug)]
enum LiveBlockEntity {
    Beacon(Beacon),
    Campfire(Campfire),
    Lectern(Lectern, Option<i64>),
    ChiseledBookshelf(ChiseledBookshelf),
    DecoratedPot(DecoratedPot),
    Crafter(Box<Crafter>, Option<i64>),
    Jukebox(Jukebox),
    Banner(Banner),
    Skull(Skull),
    Brushable(Brushable),
    TrialSpawner(TrialSpawner),
    Vault(Vault),
}

fn live_block_entity_id(name: &str) -> Option<&'static str> {
    let bare = name.strip_prefix("minecraft:").unwrap_or(name);
    Some(match bare {
        "beacon" => "minecraft:beacon",
        "campfire" | "soul_campfire" => "minecraft:campfire",
        "lectern" => "minecraft:lectern",
        "chiseled_bookshelf" => "minecraft:chiseled_bookshelf",
        "decorated_pot" => "minecraft:decorated_pot",
        "crafter" => "minecraft:crafter",
        "jukebox" => "minecraft:jukebox",
        "suspicious_sand" | "suspicious_gravel" => "minecraft:brushable_block",
        "trial_spawner" => "minecraft:trial_spawner",
        "vault" => "minecraft:vault",
        name if name.ends_with("_banner") => "minecraft:banner",
        "skeleton_skull"
        | "wither_skeleton_skull"
        | "zombie_head"
        | "player_head"
        | "creeper_head"
        | "dragon_head"
        | "piglin_head"
        | "skeleton_wall_skull"
        | "wither_skeleton_wall_skull"
        | "zombie_wall_head"
        | "player_wall_head"
        | "creeper_wall_head"
        | "dragon_wall_head"
        | "piglin_wall_head" => "minecraft:skull",
        _ => return None,
    })
}

impl LiveBlockEntity {
    fn load(reg: &BReg, state: u32, id: &str, tag: &Tag) -> Self {
        match id {
            "minecraft:beacon" => Self::Beacon(Beacon::from_nbt(tag)),
            "minecraft:campfire" => Self::Campfire(Campfire::from_nbt(tag)),
            "minecraft:lectern" => Self::Lectern(
                Lectern::from_nbt(tag),
                (reg.get_prop(state, "powered").as_deref() == Some("true"))
                    .then(|| live_block_entity_time().saturating_add(2)),
            ),
            "minecraft:chiseled_bookshelf" => {
                Self::ChiseledBookshelf(ChiseledBookshelf::from_nbt(tag))
            }
            "minecraft:decorated_pot" => Self::DecoratedPot(DecoratedPot::from_nbt(tag)),
            "minecraft:crafter" => Self::Crafter(Box::new(Crafter::from_nbt(tag)), None),
            "minecraft:jukebox" => Self::Jukebox(Jukebox::from_nbt(tag, jukebox_song_for)),
            "minecraft:banner" => Self::Banner(Banner::from_nbt(tag)),
            "minecraft:skull" => Self::Skull(Skull::from_nbt(tag)),
            "minecraft:brushable_block" => Self::Brushable(Brushable::from_nbt(tag)),
            "minecraft:trial_spawner" => Self::TrialSpawner(TrialSpawner::from_nbt(tag)),
            "minecraft:vault" => {
                let phase = match reg.get_prop(state, "vault_state").as_deref() {
                    Some("active") => VaultPhase::Active,
                    Some("unlocking") => VaultPhase::Unlocking,
                    Some("ejecting") => VaultPhase::Ejecting,
                    _ => VaultPhase::Inactive,
                };
                let mut vault = Vault::from_nbt(tag, phase);
                if tag.get("config").is_none()
                    && reg.get_prop(state, "ominous").as_deref() == Some("true")
                {
                    vault.config.key_item = BlockEntityItem::new("ominous_trial_key", 1);
                    vault.config.loot_table =
                        ResourceLocation::parse("minecraft:chests/trial_chambers/reward_ominous")
                            .unwrap();
                }
                Self::Vault(vault)
            }
            _ => unreachable!("unsupported block entity type"),
        }
    }

    // This payload is separate from the block state and scheduled ticks.
    #[cfg_attr(not(test), allow(dead_code))]
    fn to_nbt(&self) -> Tag {
        match self {
            Self::Beacon(entity) => entity.to_nbt(),
            Self::Campfire(entity) => entity.to_nbt(),
            Self::Lectern(entity, _) => entity.to_nbt(),
            Self::ChiseledBookshelf(entity) => entity.to_nbt(),
            Self::DecoratedPot(entity) => entity.to_nbt(),
            Self::Crafter(entity, _) => entity.to_nbt(),
            Self::Jukebox(entity) => entity.to_nbt(),
            Self::Banner(entity) => entity.to_nbt(),
            Self::Skull(entity) => entity.to_nbt(),
            Self::Brushable(entity) => entity.to_nbt(),
            Self::TrialSpawner(entity) => entity.to_nbt(),
            Self::Vault(entity) => entity.to_nbt(),
        }
    }
}

#[derive(Default)]
struct LiveBlockEntityStore {
    // (block id, entity). Vanilla drops the block entity the moment the block
    // identity changes; same-block state writes (rotation, power) keep it.
    entries: HashMap<DimBlockPos, (i32, LiveBlockEntity)>,
    /// `entries` positions bucketed by chunk: a maintained index so a
    /// chunk-local query never walks the whole world map. `insert_entry`,
    /// `remove_entry` and `at` keep it in step; `discover` re-derives every
    /// changed chunk's bucket, which also picks up entries written through
    /// `entries` directly (a path the save/restore code outside this impl
    /// still takes).
    chunk_index: HashMap<DimChunkKey, Vec<(i32, i32, i32)>>,
    chunk_versions: HashMap<DimChunkKey, u64>,
    scanned_at: u64,
}

fn live_block_entities() -> &'static Mutex<LiveBlockEntityStore> {
    static STORE: OnceLock<Mutex<LiveBlockEntityStore>> = OnceLock::new();
    STORE.get_or_init(Mutex::default)
}

pub(crate) fn materialize_generated_brushable(
    dim: DimId,
    pos: (i32, i32, i32),
    loot_table: &str,
    loot_seed: Option<i64>,
) {
    let Some(reg) = breg() else { return };
    let key = (dim, pos.0 >> 4, pos.2 >> 4);
    let state = world()
        .read(&key)
        .get(&key)
        .and_then(|chunk| chunk.get(&local_idx(pos.0, pos.1, pos.2)))
        .copied()
        .unwrap_or_else(|| {
            let block = if loot_table.ends_with("ocean_ruin_cold")
                || loot_table.starts_with("minecraft:archaeology/trail_ruins")
            {
                "suspicious_gravel"
            } else {
                "suspicious_sand"
            };
            name_to_state(block).unwrap()
        });
    let mut fields = BlockEntityNbtFields::new();
    block_entity_put(&mut fields, "LootTable", block_entity_string(loot_table));
    if let Some(seed) = loot_seed {
        block_entity_put(&mut fields, "LootTableSeed", Tag::Long(seed));
    }
    let entity = LiveBlockEntity::Brushable(Brushable::from_nbt(&Tag::Compound(fields)));
    if let Some(block) = reg.block_id_of(state) {
        live_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert_entry((dim, pos.0, pos.1, pos.2), (block, entity));
    }
}

#[cfg(test)]
pub(crate) fn generated_brushable_loot_table(dim: DimId, pos: (i32, i32, i32)) -> Option<String> {
    let store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
    let (_, entity) = store.entries.get(&(dim, pos.0, pos.1, pos.2))?;
    let LiveBlockEntity::Brushable(brushable) = entity else { return None };
    brushable.loot_table.as_ref().map(|table| table.id.to_string())
}

impl LiveBlockEntityStore {
    /// The chunk an entry position falls in. `div_euclid` matches the region
    /// format for negative coordinates: x = -1 is chunk -1, not chunk 0.
    fn chunk_of(key: DimBlockPos) -> DimChunkKey {
        (key.0, key.1.div_euclid(16), key.3.div_euclid(16))
    }
    fn index_add(&mut self, key: DimBlockPos) {
        let pos = (key.1, key.2, key.3);
        let v = self.chunk_index.entry(Self::chunk_of(key)).or_default();
        if !v.contains(&pos) {
            v.push(pos);
        }
    }
    fn index_drop(&mut self, key: DimBlockPos) {
        let ckey = Self::chunk_of(key);
        if let Some(v) = self.chunk_index.get_mut(&ckey) {
            v.retain(|&p| p != (key.1, key.2, key.3));
            if v.is_empty() {
                self.chunk_index.remove(&ckey);
            }
        }
    }
    /// Insert or replace one entry, keeping the chunk index in step.
    fn insert_entry(&mut self, key: DimBlockPos, entry: (i32, LiveBlockEntity)) {
        if self.entries.insert(key, entry).is_none() {
            self.index_add(key);
        }
    }
    /// Remove one entry, keeping the chunk index in step.
    fn remove_entry(&mut self, key: DimBlockPos) -> Option<(i32, LiveBlockEntity)> {
        let out = self.entries.remove(&key);
        if out.is_some() {
            self.index_drop(key);
        }
        out
    }
    /// Positions of the entries inside chunk `(dim, cx, cz)` — an indexed
    /// lookup, O(chunk population) instead of an O(world) scan. Each indexed
    /// position is re-validated against `entries` on the way out, so a write
    /// that bypassed `insert_entry`/`remove_entry` can never make this return
    /// a cell the store no longer holds; `discover` rebuilds the bucket and
    /// picks a missed entry back up. Chunk-local readers land here as the
    /// callers are threaded per-dimension.
    #[cfg_attr(not(test), allow(dead_code))]
    fn entries_in_chunk(&self, dim: DimId, cx: i32, cz: i32) -> Vec<DimBlockPos> {
        self.chunk_index
            .get(&(dim, cx, cz))
            .map(|v| {
                v.iter()
                    .filter(|&&(x, y, z)| self.entries.contains_key(&(dim, x, y, z)))
                    .map(|&(x, y, z)| (dim, x, y, z))
                    .collect()
            })
            .unwrap_or_default()
    }
    /// Rebuild the indexed buckets for `chunks` in one `entries` pass — the
    /// drift repair for `retain` drops and for writers that reach `entries`
    /// without the indexed mutators.
    fn reindex_chunks(&mut self, chunks: &HashSet<DimChunkKey>) {
        for &ckey in chunks {
            self.chunk_index.remove(&ckey);
        }
        for &(dim, x, y, z) in self.entries.keys() {
            let ckey = (dim, x.div_euclid(16), z.div_euclid(16));
            if chunks.contains(&ckey) {
                self.chunk_index.entry(ckey).or_default().push((x, y, z));
            }
        }
    }
    /// Whole-index rebuild: the first `discover` scan, when nothing about the
    /// index can be trusted yet — `entries` may hold cells written before any
    /// indexed mutator ran.
    fn rebuild_index(&mut self) {
        self.chunk_index.clear();
        let keys: Vec<DimBlockPos> = self.entries.keys().copied().collect();
        for key in keys {
            self.index_add(key);
        }
    }

    fn at(&mut self, reg: &BReg, key: DimBlockPos, state: u32) -> Option<&mut LiveBlockEntity> {
        let block = reg.block_id_of(state)?;
        if self.entries.get(&key).is_some_and(|&(id, _)| id != block) {
            self.remove_entry(key);
        }
        let id = live_block_entity_id(reg.name_of(state))?;
        if let std::collections::hash_map::Entry::Vacant(e) = self.entries.entry(key) {
            e.insert((block, LiveBlockEntity::load(reg, state, id, &Tag::End)));
            self.index_add(key);
        }
        Some(&mut self.entries.get_mut(&key)?.1)
    }

    // The world edit path already publishes per-chunk write versions. Read only
    // changed chunks, without holding the overlay lock during entity updates.
    fn discover(&mut self, reg: &BReg) {
        let sequence = CHUNK_VERSION_SEQ.load(Ordering::Relaxed);
        if self.scanned_at == sequence {
            return;
        }
        let first_scan = self.scanned_at == 0;
        self.scanned_at = sequence;
        let mut changed = chunk_versions()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(&key, &version)| {
                (self.chunk_versions.insert(key, version) != Some(version)).then_some(key)
            })
            .collect::<HashSet<_>>();
        let placements = {
            let edits = world().read_all();
            if first_scan {
                changed.extend(edits.keys().copied());
            }
            let mut placements = Vec::new();
            for &(dim, cx, cz) in &changed {
                let Some(cells) = edits.get(&(dim, cx, cz)) else {
                    continue;
                };
                placements.extend(cells.iter().filter_map(|(&index, &state)| {
                    live_block_entity_id(reg.name_of(state))?;
                    let key = (
                        dim,
                        (cx << 4) + (index & 15) as i32,
                        MIN_Y + (index >> 8) as i32,
                        (cz << 4) + ((index >> 4) & 15) as i32,
                    );
                    Some((key, state))
                }));
            }
            placements
        };
        self.entries.retain(|&(dim, x, y, z), (id, _)| {
            !changed.contains(&(dim, x >> 4, z >> 4))
                || reg.block_id_of(block_at_dim(dim, x, y, z)) == Some(*id)
        });
        // The retain may have dropped indexed positions, and writes that
        // bypassed `insert_entry` may have added entries the index missed;
        // every changed chunk's bucket is re-derived in one pass so neither
        // drift reaches `entries_in_chunk`.
        if first_scan {
            self.rebuild_index();
        } else {
            self.reindex_chunks(&changed);
        }
        for (key, state) in placements {
            self.at(reg, key, state);
        }
    }
}

// Snapshot callers keep the block state alongside this vanilla NBT payload.
#[cfg_attr(not(test), allow(dead_code))]
fn live_block_entity_snapshot(key @ (dim, x, y, z): DimBlockPos) -> Option<(u32, Tag)> {
    with_save_guard(|| {
        let reg = breg()?;
        let state = block_at_dim(dim, x, y, z);
        let id = live_block_entity_id(reg.name_of(state))?;
        let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
        let Tag::Compound(mut fields) = store.at(reg, key, state)?.to_nbt() else {
            unreachable!("block entity payload must be a compound");
        };
        for (field, value) in [("x", x), ("y", y), ("z", z)] {
            block_entity_put(&mut fields, field, Tag::Int(value));
        }
        block_entity_put(&mut fields, "id", block_entity_string(id));
        Some((state, Tag::Compound(fields)))
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn restore_live_block_entity(key @ (dim, x, y, z): DimBlockPos, tag: &Tag) -> bool {
    with_save_guard(|| {
        let Some(reg) = breg() else { return false };
        let state = block_at_dim(dim, x, y, z);
        let Some(block) = reg.block_id_of(state) else {
            return false;
        };
        let Some(id) = live_block_entity_id(reg.name_of(state)) else {
            return false;
        };
        if block_entity_str_field(tag, "id").as_deref() != Some(id) {
            return false;
        }
        let entity = LiveBlockEntity::load(reg, state, id, tag);
        live_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert_entry(key, (block, entity));
        true
    })
}

impl BlockEntityItem {
    fn from_stack(stack: &Stack) -> Option<Self> {
        let item = Self::from_nbt(&stack_to_nbt(0, stack)?)?;
        // Only accept component patches the shared inventory codec can round-trip.
        let restored = item.to_stack()?;
        (stack.2.len() == restored.2.len()
            && stack
                .2
                .iter()
                .all(|component| restored.2.contains(component)))
        .then_some(item)
    }

    fn to_stack(&self) -> Option<Stack> {
        let mut fields = self.nbt_fields();
        block_entity_put(&mut fields, "Slot", Tag::Byte(0));
        let stack = items_nbt_to_stacks(&block_entity_list(10, vec![Tag::Compound(fields)]))
            .ok()?
            .remove(&0)?;
        let round_trip = Self::from_nbt(&stack_to_nbt(0, &stack)?)?;
        (*self == round_trip).then_some(stack)
    }
}

// Built-in campfire recipes from the 1.21.1 server recipe data. The shared
// smelting table combines recipe types and discards cookingtime.
fn live_campfire_result(item: &BlockEntityItem) -> Option<BlockEntityItem> {
    if item.id.namespace() != "minecraft" {
        return None;
    }
    let result = match item.id.path() {
        "beef" => "cooked_beef",
        "chicken" => "cooked_chicken",
        "cod" => "cooked_cod",
        "kelp" => "dried_kelp",
        "mutton" => "cooked_mutton",
        "porkchop" => "cooked_porkchop",
        "potato" => "baked_potato",
        "rabbit" => "cooked_rabbit",
        "salmon" => "cooked_salmon",
        _ => return None,
    };
    BlockEntityItem::new(result, 1)
}

fn live_jukebox_song(item: &BlockEntityItem) -> Option<JukeboxSong> {
    live_jukebox_song_entry(item).map(|(_, song)| song)
}

fn live_jukebox_song_entry(item: &BlockEntityItem) -> Option<(i32, JukeboxSong)> {
    static SONGS: OnceLock<HashMap<String, (i32, JukeboxSong)>> = OnceLock::new();
    let songs = SONGS.get_or_init(|| {
        load_config_packets()
            .iter()
            .filter_map(|(_, packet)| config_registry_header(packet))
            .find(|(id, _)| *id == "minecraft:jukebox_song")
            .and_then(|(_, body)| registry_wire_entries(body))
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(id, (name, mut payload))| {
                if !read_merchant_bool(&mut payload)? {
                    return None;
                }
                let tag = nbt::read_network_nbt(&mut payload).ok()?;
                let duration = block_entity_double_field(&tag, "length_in_seconds")? * 20.0;
                Some((
                    name.to_owned(),
                    (
                        id as i32,
                        JukeboxSong {
                            length_in_ticks: duration.ceil() as i32,
                            comparator_output: u8::try_from(block_entity_number(
                                &tag,
                                "comparator_output",
                            )?)
                            .ok()?,
                        },
                    ),
                ))
            })
            .collect()
    });
    let component = NbtString::text("minecraft:jukebox_playable");
    if let Some(playable) = item.components.get(&component) {
        return songs
            .get(&block_entity_resource(playable, "song")?.to_string())
            .copied();
    }
    let disc = item.id.path().strip_prefix("music_disc_")?;
    songs
        .get(&format!("{}:{disc}", item.id.namespace()))
        .copied()
}

fn live_block_entity_time() -> i64 {
    (TICK_IDX.load(Ordering::Relaxed) * u64::from(GAME_TICKS_PER_TICK)) as i64
}

fn live_block_entity_loot(table: &ResourceLocation, seed: i64) -> Vec<BlockEntityItem> {
    loot_tables()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&table.to_string())
        .map(|pools| {
            roll_loot_seeded(pools, seed)
                .iter()
                .filter_map(BlockEntityItem::from_stack)
                .collect()
        })
        .unwrap_or_default()
}

fn emit_block_entity_item((dim, x, y, z): DimBlockPos, item: BlockEntityItem) {
    // The existing item-entity simulation and broadcasts are Overworld-only.
    if dim == DimId::Overworld {
        if let Some(stack) = item.to_stack() {
            drop_live_stack((x as f64 + 0.5, y as f64 + 0.5, z as f64 + 0.5), stack);
        }
    }
}

fn update_live_block_entity_state((dim, x, y, z): DimBlockPos, before: u32, after: u32) {
    if before != after && set_block_if_inner_dim(dim, x, y, z, after, Some(before)) {
        note_block_update_dim(dim, x, y, z, after);
    }
}

fn live_vault_state(reg: &BReg, state: u32, vault: &Vault) -> u32 {
    let phase = match vault.phase {
        VaultPhase::Inactive => "inactive",
        VaultPhase::Active => "active",
        VaultPhase::Unlocking => "unlocking",
        VaultPhase::Ejecting => "ejecting",
    };
    reg.with_prop(state, "vault_state", phase)
}

fn live_entity_hit(c: &Client, (x, y, z): BlockPos) -> Option<(RsDir, [f64; 3])> {
    let (eye_x, eye_y, eye_z) = client_eye_pos(c);
    let eye = [eye_x - x as f64, eye_y - y as f64, eye_z - z as f64];
    let (_, _, _, yaw, pitch, _) = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
    let (yaw_sin, yaw_cos) = f64::from(yaw).to_radians().sin_cos();
    let (pitch_sin, pitch_cos) = f64::from(pitch).to_radians().sin_cos();
    let ray = [-yaw_sin * pitch_cos, -pitch_sin, yaw_cos * pitch_cos];
    RsDir::ALL
        .into_iter()
        .filter_map(|face| {
            let axis = match face {
                RsDir::Up | RsDir::Down => 1,
                RsDir::West | RsDir::East => 0,
                RsDir::North | RsDir::South => 2,
            };
            let upper = matches!(face, RsDir::Up | RsDir::East | RsDir::South);
            if ray[axis] == 0.0 || (ray[axis] > 0.0) == upper {
                return None;
            }
            let distance = (f64::from(upper) - eye[axis]) / ray[axis];
            let hit = std::array::from_fn(|i| eye[i] + distance * ray[i]);
            (distance >= 0.0 && hit.iter().all(|value| (-1e-7..=1.0 + 1e-7).contains(value)))
                .then_some((distance, face, hit))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, face, hit)| (face, hit))
}

fn live_bookshelf_slot(reg: &BReg, state: u32, hit: (RsDir, [f64; 3])) -> Option<usize> {
    let (face, [x, y, z]) = hit;
    if reg.get_prop(state, "facing").as_deref() != Some(face.property()) {
        return None;
    }
    let horizontal = match face {
        RsDir::North => 1.0 - x,
        RsDir::South => x,
        RsDir::West => z,
        RsDir::East => 1.0 - z,
        _ => return None,
    };
    Some(usize::from(y < 0.5) * 3 + ((horizontal * 3.0) as usize).min(2))
}

fn use_live_block_entity(c: &Client, key @ (dim, x, y, z): DimBlockPos, state: u32) -> bool {
    let Some(reg) = breg() else { return false };
    // Component-driven block uses that are not block-entity interactions:
    // lodestone compasses, cauldron dye washing, and the debug stick.
    if !c.is_spectator() && use_item_component_on_block(c, key, state) {
        return true;
    }
    if live_block_entity_id(reg.name_of(state)).is_none() || c.is_spectator() {
        return false;
    }
    if !container_lock_gate(c, key) {
        return true;
    }
    with_save_guard(|| {
        if block_at_dim(dim, x, y, z) != state {
            return false;
        }
        if reg.name_of(state) == "minecraft:lectern"
            && reg.get_prop(state, "has_book").as_deref() == Some("true")
        {
            open_live_menu(c, Some(key), LiveMenu::Lectern);
            return true;
        }
        let creative = *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1;
        let held_index = *c.held.lock().unwrap_or_else(|e| e.into_inner());
        let selected = 36 + held_index;
        let hit = || live_entity_hit(c, (x, y, z));
        let mut inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
        let stack = inv.get(&selected).cloned().unwrap_or_default();
        let mut held = BlockEntityItem::from_stack(&stack);
        let held_before = held.as_ref().map_or(0, BlockEntityItem::count);
        let mut next_state = state;
        let mut ejected = None;
        let mut brush_breaks = false;
        let mut event = None;
        let mut inventory_changed = false;
        let used = {
            let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
            let Some(entity) = store.at(reg, key, state) else {
                return false;
            };
            match entity {
                LiveBlockEntity::Campfire(fire)
                    if reg.get_prop(state, "lit").as_deref() == Some("true") =>
                {
                    fire.place(&mut held, |item| live_campfire_result(item).map(|_| 600))
                        .is_some()
                }
                LiveBlockEntity::Lectern(lectern, _) => {
                    if lectern.place(&mut held) {
                        next_state = reg.with_prop(state, "has_book", "true");
                        true
                    } else {
                        false
                    }
                }
                LiveBlockEntity::ChiseledBookshelf(shelf) => {
                    let Some(slot) = hit().and_then(|hit| live_bookshelf_slot(reg, state, hit))
                    else {
                        return false;
                    };
                    if let Some(book) = &shelf.slots[slot] {
                        let Some(mut stack) = book.to_stack() else {
                            return false;
                        };
                        if dim == DimId::Overworld {
                            ejected = shelf.remove_book(slot);
                        } else {
                            let mut candidate = inv.clone();
                            pickup_into_inventory(&mut candidate, held_index, false, &mut stack);
                            if stack.1 != 0 {
                                return false;
                            }
                            *inv = candidate;
                            inventory_changed = true;
                            shelf.remove_book(slot);
                        }
                    } else if !shelf.insert_book(slot, &mut held) {
                        return false;
                    }
                    next_state = reg.with_prop(
                        state,
                        &format!("slot_{slot}_occupied"),
                        if shelf.slots[slot].is_some() {
                            "true"
                        } else {
                            "false"
                        },
                    );
                    true
                }
                LiveBlockEntity::DecoratedPot(pot) => {
                    pot.insert(&mut held, stack_max_of(&stack) as u8)
                }
                LiveBlockEntity::Jukebox(box_) => {
                    if let Some(record) = &box_.record {
                        if dim != DimId::Overworld || record.to_stack().is_none() {
                            return false;
                        }
                        if box_.playback.is_some() {
                            event = Some((1011, 0));
                        }
                        ejected = box_.eject();
                        next_state = reg.with_prop(state, "has_record", "false");
                    } else if box_.insert(&mut held, jukebox_song_for) {
                        if let Some((id, _)) =
                            box_.record.as_ref().and_then(live_jukebox_song_entry)
                        {
                            event = Some((1010, id));
                        }
                        next_state = reg.with_prop(state, "has_record", "true");
                    } else {
                        return false;
                    }
                    true
                }
                LiveBlockEntity::Brushable(brushable)
                    if dim == DimId::Overworld && item_to_name(stack.0) == Some("brush") =>
                {
                    let Some((face, _)) = hit() else { return false };
                    match brushable.brush(
                        live_block_entity_time(),
                        Brushable::DIRECTIONS[face as usize],
                        |table| {
                            live_block_entity_loot(
                                &table.id,
                                if table.seed == 0 {
                                    world_rng::next_u64() as i64
                                } else {
                                    table.seed
                                },
                            )
                            .into_iter()
                            .next()
                        },
                    ) {
                        BrushResult::CoolingDown => {}
                        BrushResult::Progress { dusted } => {
                            next_state = reg.with_prop(state, "dusted", &dusted.to_string());
                        }
                        BrushResult::Completed { loot, .. } => {
                            let replacement = match reg.name_of(state) {
                                "minecraft:suspicious_sand" => "minecraft:sand",
                                _ => "minecraft:gravel",
                            };
                            next_state = reg.state_of(replacement, None);
                            ejected = loot;
                            brush_breaks = !creative;
                            store.remove_entry(key);
                        }
                    }
                    true
                }
                LiveBlockEntity::Vault(vault) if dim == DimId::Overworld => {
                    let unlocked =
                        vault.try_unlock(live_block_entity_time(), c.uuid, &mut held, |table| {
                            live_block_entity_loot(table, world_rng::next_u64() as i64)
                        });
                    next_state = live_vault_state(reg, state, vault);
                    unlocked
                }
                _ => false,
            }
        };
        if !used {
            return false;
        }
        let consumed = held_before - held.as_ref().map_or(0, BlockEntityItem::count);
        let to_inventory =
            ejected.take_if(|_| reg.name_of(state) == "minecraft:chiseled_bookshelf");
        if let Some(mut stack) = to_inventory.as_ref().and_then(BlockEntityItem::to_stack) {
            inventory_changed = pickup_into_inventory(&mut inv, held_index, creative, &mut stack);
            if stack.1 != 0 && dim == DimId::Overworld {
                drop_live_stack((x as f64 + 0.5, y as f64 + 0.5, z as f64 + 0.5), stack);
            }
        }
        if consumed != 0 && !creative {
            map_take(&mut inv, selected, consumed);
            inventory_changed = true;
        }
        if inventory_changed {
            let cursor = c.cursor.lock().unwrap_or_else(|e| e.into_inner());
            let mut sock = c.sock.lock().unwrap_or_else(|e| e.into_inner());
            send_live_contents(&mut sock, 0, inv_state_next(c), &[], &inv, 0..46, &cursor);
        }
        drop(inv);
        if brush_breaks {
            damage_inv_slot(c, selected);
            held_slot_resync(c);
        }
        update_live_block_entity_state(key, state, next_state);
        if let Some(item) = ejected {
            emit_block_entity_item(key, item);
        }
        if let Some((event, data)) = event {
            live_block_entity_event(key, event, data);
        }
        true
    })
}

fn live_block_entity_event((dim, x, y, z): DimBlockPos, event: i32, data: i32) {
    let mut packet = event.to_be_bytes().to_vec();
    write_pos(&mut packet, x, y, z);
    packet.extend_from_slice(&data.to_be_bytes());
    packet.push(0); // dimension-scoped event
    broadcast_chunk_dim(dim, x >> 4, z >> 4, |_| (0x28, packet.clone()));
}

fn live_beacon_pulse((dim, x, y, z): DimBlockPos, pulse: BeaconPulse, players: &[Arc<Client>]) {
    let range = f64::from(pulse.range);
    for player in players {
        if *player.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim || player.is_spectator() {
            continue;
        }
        let (px, py, pz, ..) = *player.pos.lock().unwrap_or_else(|e| e.into_inner());
        if !(x as f64 - range..x as f64 + 1.0 + range).contains(&px)
            || !(z as f64 - range..z as f64 + 1.0 + range).contains(&pz)
            || !(y as f64 - range..(dim.min_y() + dim.height()) as f64 + range).contains(&py)
        {
            continue;
        }
        for (effect, amp) in [(Some(pulse.primary), pulse.amplifier), (pulse.secondary, 0)] {
            let Some(effect) = effect else { continue };
            let id = name_to_effect(BeaconEffect::IDS[effect as usize].1).unwrap();
            let amp = i32::from(amp);
            let left = server_ticks(pulse.duration as u64) as i32;
            {
                let mut effects = player.effs.lock().unwrap_or_else(|e| e.into_inner());
                if effects
                    .iter()
                    .any(|e| e.id == id && (e.amp > amp || e.amp == amp && e.left >= left))
                {
                    continue;
                }
                effects.retain(|e| e.id != id);
                effects.push(Eff { id, amp, left });
            }
            let packet = entity_effect_pkt(player.eid, id, amp, pulse.duration);
            let _ = send_packet(&mut player.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x76, packet.clone());
            broadcast_seen_ex(player.eid, player.eid, |_| (0x76, packet.clone()));
        }
    }
}

fn nearby_block_entity_players(
    players: &[Arc<Client>],
    (dim, x, y, z): DimBlockPos,
    range: f64,
) -> Vec<BlockEntityUuid> {
    players
        .iter()
        .filter_map(|player| {
            if *player.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim
                || player.is_spectator()
                || *player.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0
            {
                return None;
            }
            let (px, py, pz, ..) = *player.pos.lock().unwrap_or_else(|e| e.into_inner());
            let distance = (px - x as f64 - 0.5).powi(2)
                + (py - y as f64 - 0.5).powi(2)
                + (pz - z as f64 - 0.5).powi(2);
            (distance <= range * range).then_some(player.uuid)
        })
        .collect()
}

fn live_crafter_recipe(
    slots: &[Option<BlockEntityItem>; 9],
) -> Option<(BlockEntityItem, Vec<BlockEntityItem>)> {
    let mut grid = HashMap::new();
    for (i, item) in slots.iter().enumerate() {
        if let Some(item) = item {
            grid.insert(i as i16 + 1, item.to_stack()?);
        }
    }
    let result = BlockEntityItem::from_stack(&craft3_result(&grid)?)?;
    let remainders = slots
        .iter()
        .flatten()
        .filter(|item| {
            item.vanilla(&[
                "milk_bucket",
                "water_bucket",
                "lava_bucket",
                "powder_snow_bucket",
            ])
        })
        .filter_map(|_| BlockEntityItem::new("bucket", 1))
        .collect();
    Some((result, remainders))
}

fn live_crafter_powered(reg: &'static BReg, (dim, x, y, z): DimBlockPos, now: i64) -> bool {
    let mut dimensions = live_redstone().lock().unwrap_or_else(|e| e.into_inner());
    let mut empty = LiveRedstoneState::default();
    let mut world = LiveRedstoneWorld {
        dim,
        reg,
        state: dimensions.get_mut(&dim).unwrap_or(&mut empty),
        now: now as u64,
        reads: Default::default(),
        writes: Default::default(),
        dispensers: Vec::new(),
    };
    let mut torches = BTreeMap::new();
    let adapter = RedstoneWorldAdapter::new(reg, &mut world, &mut torches);
    rs_neighbor_signal(&adapter, RsPos(x, y, z), true) != 0
}

fn tick_live_trial_spawner(
    spawner: &mut TrialSpawner,
    reg: &BReg,
    key @ (dim, x, y, z): DimBlockPos,
    state: u32,
    now: i64,
    players: &[Arc<Client>],
    items: &mut Vec<BlockEntityItem>,
) -> u32 {
    let property = "trial_spawner_state";
    let phase = reg.get_prop(state, property);
    let Some(spawn) = spawner.spawn_data.as_ref() else {
        return reg.with_prop(state, property, "inactive");
    };
    let Some(spec) = spawn
        .get("entity")
        .and_then(|entity| block_entity_str_field(entity, "id"))
        .and_then(|id| entity_spec_by_name(&id))
        .filter(|spec| spec.max_health > 0.0)
    else {
        return state;
    };
    if DIFFICULTY.load(Ordering::Relaxed) == 0 {
        return state;
    }
    match phase.as_deref() {
        Some("cooldown") if now < spawner.cooldown_ends_at => return state,
        Some("cooldown") => {
            spawner.registered_players.clear();
            spawner.ejecting_loot_table = None;
            return reg.with_prop(state, property, "waiting_for_players");
        }
        Some("waiting_for_reward_ejection" | "ejecting_reward") => {
            if now < spawner.next_mob_spawns_at {
                return state;
            }
            if let Some(table) = &spawner.ejecting_loot_table {
                if dim != DimId::Overworld {
                    return state;
                }
                for _ in &spawner.registered_players {
                    items.extend(live_block_entity_loot(table, world_rng::next_u64() as i64));
                }
            }
            return reg.with_prop(state, property, "cooldown");
        }
        _ => {}
    }
    let nearby =
        nearby_block_entity_players(players, key, f64::from(spawner.required_player_range));
    for uuid in nearby {
        if !spawner.registered_players.contains(&uuid) {
            spawner.registered_players.push(uuid);
        }
    }
    if spawner.registered_players.is_empty() {
        return reg.with_prop(state, property, "waiting_for_players");
    }
    let config = if reg.get_prop(state, "ominous").as_deref() == Some("true") {
        spawner.ominous_config.as_ref()
    } else {
        spawner.normal_config.as_ref()
    }
    .unwrap_or(&Tag::End);
    let additional = spawner.registered_players.len().saturating_sub(1) as f64;
    let quota = |base, per_player, default, default_extra| {
        (block_entity_double_field(config, base).unwrap_or(default)
            + additional * block_entity_double_field(config, per_player).unwrap_or(default_extra))
        .floor()
        .max(0.0) as i32
    };
    let total = quota("total_mobs", "total_mobs_added_per_player", 6.0, 2.0);
    let simultaneous = quota(
        "simultaneous_mobs",
        "simultaneous_mobs_added_per_player",
        2.0,
        1.0,
    );
    let interval = block_entity_number(config, "ticks_between_spawn")
        .and_then(|value| i32::try_from(value).ok())
        .filter(|&value| value >= 0)
        .unwrap_or(40);
    let range = block_entity_number(config, "spawn_range")
        .filter(|value| (1..=128).contains(value))
        .unwrap_or(4);
    let mut mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
    for uuid in spawner.current_mobs.clone() {
        if !mobs
            .iter()
            .any(|mob| mob.dim == dim && mob.uuid == uuid && *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0)
        {
            spawner.remove_mob(now, interval, uuid);
        }
    }
    if spawner.total_mobs_spawned >= total {
        if spawner.begin_rewards(now) {
            spawner.next_mob_spawns_at = now.saturating_add(40);
            return reg.with_prop(state, property, "waiting_for_reward_ejection");
        }
    } else if spawner.current_mobs.len() < simultaneous as usize
        && now >= spawner.next_mob_spawns_at
        && mobs.len() < MOB_CAP
    {
        let mut rng = generation_math::Lcg::new(world_rng::next_u64());
        let px = x as f64 + 0.5 + (rng.next_f32() - rng.next_f32()) as f64 * range as f64;
        let pz = z as f64 + 0.5 + (rng.next_f32() - rng.next_f32()) as f64 * range as f64;
        let py = y as f64 + f64::from(rng.next_bound(3) - 1);
        let half = f64::from(spec.width) / 2.0;
        if live_mob_block_boxes(
            dim,
            reg,
            [px - half, py, pz - half],
            [px + half, py + f64::from(spec.height), pz + half],
        )
        .is_empty()
        {
            let mob = new_live_mob(
                spec.id,
                dim,
                (px, py, pz, rng.next_f32() * 360.0, 0.0),
                spec.max_health,
                None,
            );
            if spawner.record_spawn(now, interval, mob.uuid) {
                mobs.push(mob);
            }
        }
    }
    reg.with_prop(state, property, "active")
}

fn tick_live_block_entities(players: &[Arc<Client>]) {
    let Some(reg) = breg() else { return };
    with_save_guard(|| {
        let game_time = live_block_entity_time();
        let updates = {
            let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
            store.discover(reg);
            let mut updates = Vec::new();
            for (&key @ (dim, x, y, z), (_, entity)) in &mut store.entries {
                if matches!(
                    entity,
                    LiveBlockEntity::Banner(_)
                        | LiveBlockEntity::Skull(_)
                        | LiveBlockEntity::DecoratedPot(_)
                        | LiveBlockEntity::ChiseledBookshelf(_)
                ) {
                    continue;
                }
                let state = block_at_dim(dim, x, y, z);
                let mut after = state;
                let mut items = Vec::new();
                let mut pulse = None;
                let mut event = None;
                let lit = || reg.get_prop(state, "lit").as_deref() == Some("true");
                for now in game_time..game_time + i64::from(GAME_TICKS_PER_TICK) {
                    match entity {
                        LiveBlockEntity::Beacon(beacon) if now % 80 == 0 => {
                            let beam_clear = ((y + 1)..dim.min_y() + dim.height()).all(|y| {
                                let above = block_at_dim(dim, x, y, z);
                                reg.opacity_of(above) < 15
                                    || reg.name_of(above) == "minecraft:bedrock"
                            });
                            pulse = beacon.tick(now, beam_clear, |dx, dy, dz| {
                                let base = block_at_dim(dim, x + dx, y + dy, z + dz);
                                matches!(
                                    reg.name_of(base),
                                    "minecraft:iron_block"
                                        | "minecraft:gold_block"
                                        | "minecraft:emerald_block"
                                        | "minecraft:diamond_block"
                                        | "minecraft:netherite_block"
                                )
                            });
                        }
                        LiveBlockEntity::Campfire(fire) => {
                            items.extend(fire.tick(lit(), live_campfire_result, |item| {
                                dim == DimId::Overworld && item.to_stack().is_some()
                            }));
                        }
                        LiveBlockEntity::Lectern(_, due) => {
                            if due.is_some_and(|time| now >= time) {
                                *due = None;
                                after = reg.with_prop(after, "powered", "false");
                            }
                        }
                        LiveBlockEntity::Crafter(crafter, due) => {
                            crafter.tick();
                            if crafter.crafting_ticks_remaining == 0 {
                                after = reg.with_prop(after, "crafting", "false");
                            }
                            if now == game_time {
                                let powered = live_crafter_powered(reg, key, now);
                                if let Some(delay) = crafter.set_powered(powered) {
                                    due.get_or_insert(now.saturating_add(i64::from(delay)));
                                }
                                after = reg.with_prop(
                                    after,
                                    "triggered",
                                    if powered { "true" } else { "false" },
                                );
                            }
                            if due.is_some_and(|time| now >= time) {
                                *due = None;
                                if dim == DimId::Overworld {
                                    if let Some((result, remainders)) =
                                        crafter.craft(live_crafter_recipe)
                                    {
                                        after = reg.with_prop(after, "crafting", "true");
                                        items.push(result);
                                        items.extend(remainders);
                                    }
                                }
                            }
                        }
                        LiveBlockEntity::Jukebox(jukebox) => {
                            if jukebox.tick() == Some(JukeboxTick::Stopped) {
                                event = Some((1011, 0));
                            }
                        }
                        LiveBlockEntity::Brushable(brushable) => {
                            brushable.tick(now);
                            after = reg.with_prop(after, "dusted", &brushable.dusted().to_string());
                        }
                        LiveBlockEntity::TrialSpawner(spawner) => {
                            after = tick_live_trial_spawner(
                                spawner, reg, key, after, now, players, &mut items,
                            );
                        }
                        LiveBlockEntity::Vault(vault) => {
                            if dim == DimId::Overworld || vault.items_to_eject.is_empty() {
                                items.extend(vault.tick(now, |range| {
                                    nearby_block_entity_players(players, key, range)
                                }));
                            }
                            after = live_vault_state(reg, after, vault);
                        }
                        _ => {}
                    }
                }
                if after != state || !items.is_empty() || pulse.is_some() || event.is_some() {
                    updates.push((key, state, after, items, pulse, event));
                }
            }
            updates
        };
        for (key, state, after, items, pulse, event) in updates {
            update_live_block_entity_state(key, state, after);
            if reg.name_of(state) == "minecraft:crafter" {
                for player in players {
                    let Some(connection) = live_menu_connection(player) else {
                        continue;
                    };
                    let mut active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(session) = active.as_mut().filter(|session| {
                        matches!(session.menu, LiveMenu::Crafter) && session.anchor == Some(key)
                    }) {
                        sync_live_menu(player, session, false);
                    }
                }
            }
            for item in items {
                emit_block_entity_item(key, item);
            }
            if let Some(pulse) = pulse {
                live_beacon_pulse(key, pulse, players);
            }
            if let Some((event, data)) = event {
                live_block_entity_event(key, event, data);
            }
        }
    });
}

// ---------- item -> blockstate (1.21.1 ids) ----------
fn item_to_state(item: i64) -> Option<u32> {
    Some(match item {
        1 => 1,
        27 => 9,
        28 => 10,
        35 => 14,
        36 => 15,
        57 => 112,
        88 => 2092,
        91 => 4276,
        132 => 131,
        188 => 519,
        290 => 2354,
        291 => 2355,
        299 => 2955, // chest's 1.21.1 default state (single, north, dry)
        678 => 9120, // trapped_chest's default state (single, north, dry)
        _ => {
            let name = item_to_name(item)?;
            // Block entities plus the workstation blocks, which are BlockItems
            // with no entity data.
            if live_block_entity_id(name).is_none()
                && !matches!(
                    name,
                    "stonecutter" | "grindstone" | "loom" | "cartography_table"
                )
            {
                return None;
            }
            let reg = breg()?;
            reg.state_of(&format!("minecraft:{name}"), None)
        }
    })
}

fn slot_wire(s: &Stack) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, s.1 as i64);
    if s.1 > 0 {
        write_varint(&mut p, s.0);
        // Removed default components have no payload in the protocol. Preserve
        // them as an internal, negative-id entry without changing Stack's API.
        let removed = comp_get(&s.2, SLOT_REMOVED_COMPONENTS).unwrap_or_default();
        let mut ids = removed;
        let mut removed_count = 0;
        while !ids.is_empty() {
            if read_protocol_i32(&mut ids).is_none() {
                break;
            }
            removed_count += 1;
        }
        write_varint(&mut p, s.2.iter().filter(|(ty, _)| *ty >= 0).count() as i64);
        write_varint(&mut p, removed_count);
        for (ty, data) in &s.2 {
            if *ty >= 0 {
                write_varint(&mut p, *ty);
                p.extend_from_slice(data);
            }
        }
        p.extend_from_slice(removed);
    }
    p
}
// The on-wire removed-component list is stored as raw VarInts under this
// private marker. Component type ids in protocol 767 are all non-negative.
const SLOT_REMOVED_COMPONENTS: i64 = -1;
const SLOT_COMPONENT_TYPE_COUNT: i64 = 57;

// Protocol 767 data-component boundaries. Keep each payload's original bytes
// (including non-canonical VarInts and NBT) instead of decoding and rebuilding
// values. The typed readers only establish where the next component begins.
fn slot_read_varint(b: &[u8], p: &mut usize) -> Option<i64> {
    let mut rest = b.get(*p..)?;
    let value = read_protocol_i32(&mut rest)?;
    *p = b.len() - rest.len();
    Some(i64::from(value))
}

fn slot_read_bytes(b: &[u8], p: &mut usize, len: usize) -> Option<()> {
    let end = (*p).checked_add(len)?;
    b.get(*p..end)?;
    *p = end;
    Some(())
}

fn slot_read_bool(b: &[u8], p: &mut usize) -> Option<bool> {
    let value = *b.get(*p)?;
    if value > 1 {
        return None;
    }
    *p += 1;
    Some(value != 0)
}

fn slot_read_count(b: &[u8], p: &mut usize, max: i64) -> Option<usize> {
    usize::try_from(slot_read_varint(b, p)?).ok().filter(|&n| n <= max as usize)
}

fn slot_read_string(b: &[u8], p: &mut usize) -> Option<()> {
    let len = slot_read_count(b, p, 98301)?;
    let text = b.get(*p..(*p).checked_add(len)?)?;
    std::str::from_utf8(text).ok()?;
    *p += len;
    Some(())
}

fn slot_read_nbt_payload(b: &[u8], p: &mut usize, tag: u8, depth: usize) -> Option<()> {
    if depth > 64 {
        return None;
    }
    match tag {
        1 => slot_read_bytes(b, p, 1)?,
        2 => slot_read_bytes(b, p, 2)?,
        3 | 5 => slot_read_bytes(b, p, 4)?,
        4 | 6 => slot_read_bytes(b, p, 8)?,
        7 | 11 | 12 => {
            let length = i32::from_be_bytes(b.get(*p..(*p).checked_add(4)?)?.try_into().ok()?);
            *p += 4;
            let length = usize::try_from(length).ok()?;
            let element_size = match tag {
                7 => 1,
                11 => 4,
                12 => 8,
                _ => return None,
            };
            slot_read_bytes(b, p, length.checked_mul(element_size)?)?;
        }
        8 => {
            let length = u16::from_be_bytes(b.get(*p..(*p).checked_add(2)?)?.try_into().ok()?)
                as usize;
            slot_read_bytes(b, p, 2 + length)?;
        }
        9 => {
            let element_tag = *b.get(*p)?;
            *p += 1;
            let length = i32::from_be_bytes(b.get(*p..(*p).checked_add(4)?)?.try_into().ok()?);
            *p += 4;
            let length = usize::try_from(length).ok()?;
            if length > 1_000_000 || (element_tag == 0 && length != 0) {
                return None;
            }
            for _ in 0..length {
                slot_read_nbt_payload(b, p, element_tag, depth + 1)?;
            }
        }
        10 => loop {
            let child_tag = *b.get(*p)?;
            *p += 1;
            if child_tag == 0 {
                break;
            }
            if child_tag > 12 {
                return None;
            }
            let name_length =
                u16::from_be_bytes(b.get(*p..(*p).checked_add(2)?)?.try_into().ok()?) as usize;
            slot_read_bytes(b, p, 2 + name_length)?;
            slot_read_nbt_payload(b, p, child_tag, depth + 1)?;
        },
        _ => return None,
    }
    Some(())
}

fn slot_read_nbt(b: &[u8], p: &mut usize) -> Option<()> {
    let tag = *b.get(*p)?;
    if !(1..=12).contains(&tag) {
        return None;
    }
    *p += 1;
    slot_read_nbt_payload(b, p, tag, 0)
}

fn slot_read_optional_nbt(b: &[u8], p: &mut usize) -> Option<()> {
    if *b.get(*p)? == 0 {
        *p += 1;
        Some(())
    } else {
        slot_read_nbt(b, p)
    }
}

fn slot_read_text(b: &[u8], p: &mut usize) -> Option<()> {
    // Text components are unnamed NBT values, commonly strings or compounds.
    slot_read_nbt(b, p)
}

fn slot_read_optional_string(b: &[u8], p: &mut usize) -> Option<()> {
    if slot_read_bool(b, p)? {
        slot_read_string(b, p)?;
    }
    Some(())
}

fn slot_read_holder_set(b: &[u8], p: &mut usize) -> Option<()> {
    let kind = slot_read_varint(b, p)?;
    if kind == 0 {
        slot_read_string(b, p)?;
    } else {
        for _ in 0..usize::try_from(kind - 1).ok().filter(|&n| n <= 4096)? {
            slot_read_varint(b, p)?;
        }
    }
    Some(())
}

fn slot_read_block_predicate(b: &[u8], p: &mut usize) -> Option<()> {
    if slot_read_bool(b, p)? {
        slot_read_holder_set(b, p)?;
    }
    if slot_read_bool(b, p)? {
        for _ in 0..slot_read_count(b, p, 256)? {
            slot_read_string(b, p)?;
            if slot_read_bool(b, p)? {
                slot_read_string(b, p)?;
            } else {
                slot_read_string(b, p)?;
                slot_read_string(b, p)?;
            }
        }
    }
    slot_read_optional_nbt(b, p)
}

fn slot_read_trim_material(b: &[u8], p: &mut usize) -> Option<()> {
    slot_read_string(b, p)?;
    slot_read_varint(b, p)?;
    for _ in 0..slot_read_count(b, p, 256)? {
        slot_read_varint(b, p)?;
        slot_read_string(b, p)?;
    }
    slot_read_nbt(b, p)
}

fn slot_read_trim_pattern(b: &[u8], p: &mut usize) -> Option<()> {
    slot_read_string(b, p)?;
    slot_read_varint(b, p)?;
    slot_read_nbt(b, p)?;
    slot_read_bool(b, p)?;
    Some(())
}

fn slot_read_sound_event(b: &[u8], p: &mut usize) -> Option<()> {
    slot_read_string(b, p)?;
    if slot_read_bool(b, p)? {
        slot_read_bytes(b, p, 4)?;
    }
    Some(())
}

fn slot_read_sound_holder(b: &[u8], p: &mut usize) -> Option<()> {
    if slot_read_varint(b, p)? == 0 {
        slot_read_sound_event(b, p)?;
    }
    Some(())
}

fn slot_read_instrument_data(b: &[u8], p: &mut usize) -> Option<()> {
    slot_read_sound_holder(b, p)?;
    slot_read_bytes(b, p, 8)?;
    slot_read_nbt(b, p)
}

fn slot_read_jukebox_song_data(b: &[u8], p: &mut usize) -> Option<()> {
    slot_read_sound_holder(b, p)?;
    slot_read_nbt(b, p)?;
    slot_read_bytes(b, p, 4)?;
    slot_read_varint(b, p)?;
    Some(())
}

fn slot_read_firework_explosion(b: &[u8], p: &mut usize) -> Option<()> {
    if !(0..=4).contains(&slot_read_varint(b, p)?) {
        return None;
    }
    for _ in 0..2 {
        let count = slot_read_count(b, p, 256)?;
        slot_read_bytes(b, p, count * 4)?;
    }
    slot_read_bool(b, p)?;
    slot_read_bool(b, p)?;
    Some(())
}

fn slot_read_component(b: &[u8], p: &mut usize, ty: i64, depth: usize) -> Option<()> {
    match ty {
        0 => slot_read_nbt(b, p)?,
        1..=3 | 13 | 16 | 26 | 28 | 41 | 50 => {
            slot_read_varint(b, p)?;
        }
        8 => {
            if !(0..=3).contains(&slot_read_varint(b, p)?) {
                return None;
            }
        }
        4 | 18 => { slot_read_bool(b, p)?; }
        5 | 6 => slot_read_text(b, p)?,
        7 => for _ in 0..slot_read_count(b, p, 256)? { slot_read_optional_nbt(b, p)?; },
        9 | 23 => {
            for _ in 0..slot_read_count(b, p, 256)? {
                slot_read_varint(b, p)?;
                slot_read_varint(b, p)?;
            }
            slot_read_bool(b, p)?;
        }
        10 | 11 => {
            for _ in 0..slot_read_count(b, p, 256)? {
                slot_read_block_predicate(b, p)?;
            }
            slot_read_bool(b, p)?;
        }
        COMP_ATTRIBUTE_MODIFIERS => {
            let mut rest = b.get(*p..)?;
            attr_modifiers_read(&mut rest)?;
            *p = b.len() - rest.len();
        }
        14 | 15 | 17 | 21 => {} // Unit components have zero-byte payloads.
        19 | 27 | 36..=39 | 43 | 55 | 56 => slot_read_nbt(b, p)?,
        20 => {
            slot_read_varint(b, p)?; // nutrition
            slot_read_bytes(b, p, 4)?; // saturation
            slot_read_bool(b, p)?; // can always eat
            slot_read_bytes(b, p, 4)?; // eating seconds
            slot_read_stack(b, p, depth + 1)?; // conversion item, possibly empty
            for _ in 0..slot_read_count(b, p, 256)? {
                slot_read_varint(b, p)?;
                slot_read_bytes(b, p, 4)?; // probability
            }
        }
        22 => {
            for _ in 0..slot_read_count(b, p, 256)? {
                slot_read_holder_set(b, p)?;
                if slot_read_bool(b, p)? { slot_read_bytes(b, p, 4)?; }
                if slot_read_bool(b, p)? { slot_read_bool(b, p)?; }
            }
            slot_read_bytes(b, p, 4)?;
            slot_read_varint(b, p)?;
        }
        24 => {
            slot_read_bytes(b, p, 4)?;
            slot_read_bool(b, p)?;
        }
        25 => slot_read_bytes(b, p, 4)?,
        29 | 30 | 52 => {
            for _ in 0..slot_read_count(b, p, 256)? {
                slot_read_stack(b, p, depth + 1)?;
            }
        }
        COMP_POTION_CONTENTS => {
            let mut rest = b.get(*p..)?;
            if read_merchant_bool(&mut rest)? { read_protocol_i32(&mut rest)?; }
            if read_merchant_bool(&mut rest)? { rest = rest.get(4..)?; }
            for _ in 0..read_protocol_i32(&mut rest).filter(|&n| (0..=256).contains(&n))? {
                read_protocol_i32(&mut rest)?;
                read_live_potion_effect_details(&mut rest, 0)?;
            }
            *p = b.len() - rest.len();
            slot_read_optional_string(b, p)?;
        }
        32 => for _ in 0..slot_read_count(b, p, 256)? {
            slot_read_varint(b, p)?;
            slot_read_varint(b, p)?;
        },
        33 => for _ in 0..slot_read_count(b, p, 100)? {
            slot_read_string(b, p)?;
            slot_read_optional_string(b, p)?;
        },
        34 => {
            slot_read_string(b, p)?;
            slot_read_optional_string(b, p)?;
            slot_read_string(b, p)?;
            slot_read_varint(b, p)?;
            for _ in 0..slot_read_count(b, p, 100)? {
                slot_read_text(b, p)?;
                slot_read_optional_nbt(b, p)?;
            }
            slot_read_bool(b, p)?;
        }
        COMP_TRIM => {
            if slot_read_varint(b, p)? == 0 { slot_read_trim_material(b, p)?; }
            if slot_read_varint(b, p)? == 0 { slot_read_trim_pattern(b, p)?; }
            slot_read_bool(b, p)?;
        }
        40 => {
            if slot_read_varint(b, p)? == 0 {
                slot_read_instrument_data(b, p)?;
            }
        }
        42 => {
            if slot_read_bool(b, p)? {
                if slot_read_varint(b, p)? == 0 {
                    slot_read_jukebox_song_data(b, p)?;
                }
            } else {
                slot_read_string(b, p)?;
            }
            slot_read_bool(b, p)?;
        }
        44 => {
            if slot_read_bool(b, p)? {
                slot_read_string(b, p)?;
                slot_read_bytes(b, p, 8)?;
            }
            slot_read_bool(b, p)?;
        }
        45 => slot_read_firework_explosion(b, p)?,
        46 => {
            slot_read_varint(b, p)?;
            for _ in 0..slot_read_count(b, p, 256)? { slot_read_firework_explosion(b, p)?; }
        }
        47 => {
            if slot_read_bool(b, p)? { slot_read_string(b, p)?; }
            if slot_read_bool(b, p)? { slot_read_bytes(b, p, 16)?; }
            for _ in 0..slot_read_count(b, p, 256)? {
                slot_read_string(b, p)?;
                slot_read_string(b, p)?;
                if slot_read_bool(b, p)? { slot_read_string(b, p)?; }
            }
        }
        48 => slot_read_string(b, p)?,
        COMP_BANNER_PATTERNS => {
            for _ in 0..slot_read_count(b, p, 256)? {
                if slot_read_varint(b, p)? == 0 {
                    slot_read_string(b, p)?;
                    slot_read_string(b, p)?;
                }
                if !(0..=15).contains(&slot_read_varint(b, p)?) { return None; }
            }
        }
        51 => for _ in 0..slot_read_count(b, p, 4)? { slot_read_varint(b, p)?; },
        53 => for _ in 0..slot_read_count(b, p, 256)? {
            slot_read_string(b, p)?;
            slot_read_string(b, p)?;
        },
        54 => for _ in 0..slot_read_count(b, p, 256)? {
            slot_read_nbt(b, p)?;
            slot_read_varint(b, p)?;
            slot_read_varint(b, p)?;
        },
        _ => return None,
    }
    Some(())
}

fn slot_read_stack(b: &[u8], p: &mut usize, depth: usize) -> Option<Stack> {
    if depth > 16 { return None; }
    let cnt = slot_read_varint(b, p)?;
    if cnt == 0 { return Some((0, 0, Vec::new())); }
    if !(1..=127).contains(&cnt) { return None; }
    let item = slot_read_varint(b, p)?;
    if !(0..=i32::MAX as i64).contains(&item) { return None; }
    let nadd = slot_read_count(b, p, 128)?;
    let nrem = slot_read_count(b, p, 128)?;
    let mut comps = Vec::with_capacity(nadd + usize::from(nrem != 0));
    for _ in 0..nadd {
        let ty = slot_read_varint(b, p)?;
        if !(0..SLOT_COMPONENT_TYPE_COUNT).contains(&ty) { return None; }
        let start = *p;
        slot_read_component(b, p, ty, depth)?;
        comps.push((ty, b.get(start..*p)?.to_vec()));
    }
    if nrem != 0 {
        let start = *p;
        for _ in 0..nrem {
            let ty = slot_read_varint(b, p)?;
            if !(0..SLOT_COMPONENT_TYPE_COUNT).contains(&ty) { return None; }
        }
        comps.push((SLOT_REMOVED_COMPONENTS, b.get(start..*p)?.to_vec()));
    }
    Some((item, cnt as u8, comps))
}

fn slot_read_wire(b: &[u8]) -> Option<(i16, Stack)> {
    let slot = i16::from_be_bytes(b.get(..2)?.try_into().ok()?);
    let mut position = 2;
    let stack = slot_read_stack(b, &mut position, 0)?;
    (position == b.len()).then_some((slot, stack))
}

/// Bound on the summed payload bytes of a stack's added components. Each
/// component is structurally validated during `slot_read_wire`, but without a
/// total cap a creative-slot packet could store (and re-broadcast through
/// `0x15`, drops, and pickups) a multi-megabyte component blob per stack.
const SLOT_COMPONENT_WIRE_LIMIT: usize = 8192;

/// Total wire bytes of a stack's added-component payloads — the same bytes
/// `slot_wire` would emit for `st.2` entries with nonnegative type ids.
fn slot_component_wire_bytes(st: &Stack) -> usize {
    st.2.iter()
        .filter(|(ty, _)| *ty >= 0)
        .map(|(_, data)| data.len())
        .sum()
}

/// `0x32` admission check: a parsed stack is stored, echoed, and later
/// re-broadcast through drops/pickups only while its component payloads fit
/// the bound — anything larger is dropped fail-closed before mutation.
fn creative_slot_acceptable(st: &Stack) -> bool {
    slot_component_wire_bytes(st) <= SLOT_COMPONENT_WIRE_LIMIT
        && (st.1 == 0 || (st.0 > 0 && item_to_name(st.0).is_some()))
        && st.2
            .iter()
            .all(|(ty, data)| *ty < 0 || apply_item_component_behavior(*ty, data).is_some())
}

/// The creative wire count can exceed the resolved item's stack limit. Apply
/// the same component-aware maximum as every other inventory entry point.
fn creative_slot_clamp(st: &mut Stack) {
    if st.1 > 0 {
        st.1 = st.1.min(item_stack_max(st.0, &st.2).clamp(1, 99) as u8);
    }
}

fn creative_inventory_slot(slot: i16) -> bool {
    (0..=45).contains(&slot)
}

#[cfg(test)]
mod creative_slot_wire_tests {
    use super::*;

    #[test]
    fn unknown_component_rejects_entire_stack() {
        let mut packet = 0i16.to_be_bytes().to_vec();
        write_varint(&mut packet, 1); // count
        write_varint(&mut packet, 1); // item
        write_varint(&mut packet, 1); // added components
        write_varint(&mut packet, 0); // removed components
        write_varint(&mut packet, 200); // unknown component id
        packet.push(0xaa);
        assert_eq!(slot_read_wire(&packet), None);
    }

    #[test]
    fn malformed_creative_stack_is_rejected() {
        let mut packet = 0i16.to_be_bytes().to_vec();
        write_varint(&mut packet, 1);
        write_varint(&mut packet, 1);
        write_varint(&mut packet, 1);
        write_varint(&mut packet, 0);
        write_varint(&mut packet, COMP_DAMAGE);
        assert_eq!(slot_read_wire(&packet), None);
    }

    #[test]
    fn creative_slot_range_matches_player_menu() {
        assert!(!creative_inventory_slot(-1));
        assert!(creative_inventory_slot(0));
        assert!(creative_inventory_slot(45));
        assert!(!creative_inventory_slot(46));
    }

    #[test]
    fn creative_unknown_item_is_rejected_and_empty_stack_is_allowed() {
        assert!(!creative_slot_acceptable(&(i64::MAX, 1, Vec::new())));
        assert!(creative_slot_acceptable(&(0, 0, Vec::new())));
    }

    #[test]
    fn creative_count_uses_item_and_component_maximum() {
        let mut sixty_four = (name_to_item("stone").unwrap(), 127, Vec::new());
        assert!(creative_slot_acceptable(&sixty_four));
        creative_slot_clamp(&mut sixty_four);
        assert_eq!(sixty_four.1, 64);

        let mut pearl = (name_to_item("ender_pearl").unwrap(), 127, Vec::new());
        creative_slot_clamp(&mut pearl);
        assert_eq!(pearl.1, 16);

        let mut override_stack = (name_to_item("stone").unwrap(), 127, Vec::new());
        comp_set_varint(&mut override_stack.2, 1, 120);
        creative_slot_clamp(&mut override_stack);
        assert_eq!(override_stack.1, 99);
    }
}
fn write_open_chest_packets(
    s: &mut TcpStream,
    wide: bool,
    initial: &[Stack],
    inv: &HashMap<i16, Stack>,
    title: &str,
    state_id: i32,
) {
    // `first` owns window slots 0..=26 and `second` 27..=53. For a chest pair that is
    // `DoubleBlockCombiner`'s order (`CompoundContainer(right, left)`), not a click
    // order: the caller passes the right half first.
    let rows = initial.len() as i16;
    let mut p = Vec::new();
    write_varint(&mut p, 1); // windowId
    write_varint(&mut p, if wide { 5 } else { 2 }); // generic_9x6 / generic_9x3
    p.extend_from_slice(&chat_nbt(if wide { "Large Chest" } else { title }));
    let _ = send_packet(s, 0x33, p);
    let mut q = vec![1u8];
    write_varint(&mut q, i64::from(state_id));
    write_varint(&mut q, (rows + 36) as i64);
    for norm in initial {
        q.extend_from_slice(&slot_wire(norm));
    }
    for wsl in rows..(rows + 36) {
        let islot = wsl - rows + 9; // window inv section -> inv slots 9..44
        let (it, c, _cc) = inv.get(&islot).cloned().unwrap_or((0, 0, Vec::new()));
        q.extend_from_slice(&slot_wire(&(it, c, _cc)));
    }
    q.extend_from_slice(&slot_wire(&(0, 0, Vec::new()))); // carried
    let _ = send_packet(s, 0x13, q);
}
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn open_chest_window(
    s: &mut TcpStream,
    first: ContainerKey,
    second: Option<ContainerKey>,
    inv: &HashMap<i16, Stack>,
    title: &str,
    state_id: i32,
) {
    let initial = chest_window_slots((first, second));
    write_open_chest_packets(s, second.is_some(), &initial, inv, title, state_id);
}
fn open_chest_for_client(
    me: &Client,
    window: (ContainerKey, Option<ContainerKey>),
    anchor: Option<BlockPos>,
    inv: &HashMap<i16, Stack>,
    title: &str,
) {
    if !me.is_spectator() {
        unpack_window_loot_if_needed(window);
        let locked = [window.0, window.1.unwrap_or(window.0)].iter().any(|key| {
            matches!(key, ContainerKey::Block(block_key) if !container_lock_gate(me, *block_key))
        });
        if locked {
            return;
        }
    }
    let initial = chest_window_slots(window);
    *me.open_chest.lock().unwrap_or_else(|e| e.into_inner()) = Some(window);
    *me.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = Some(ChestSyncState {
        window,
        anchor,
        state_id: 0,
        last_slots: initial.clone(),
    });
    // Vanilla's `sendInitial` runs `incrementStateId()` on the fresh menu: the
    // open snapshot carries 1 and every later delta continues from there.
    let open_state_id = {
        let mut sync_guard = me.chest_sync.lock().unwrap_or_else(|e| e.into_inner());
        let sync = sync_guard.as_mut().unwrap();
        sync.state_id = (sync.state_id + 1) & 0x7fff;
        sync.state_id
    };
    if let Ok(mut w) = me.sock.lock() {
        write_open_chest_packets(&mut w, window.1.is_some(), &initial, inv, title, open_state_id);
    }
}
fn armor_removal_blocked(slot: i16, stack: Option<&Stack>, creative: bool) -> bool {
    stack.is_some_and(|stack| {
        stack.1 > 0
            && comp_get(&stack.2, COMP_CREATIVE_SLOT_LOCK).is_some()
    }) || (!creative
        && (5..=8).contains(&slot)
        && stack.is_some_and(|stack| stack.1 > 0 && enchant_level(&stack.2, 2) > 0))
}

// which armor slot (5=helm..8=boots) an item equips into, or -1
fn armor_slot_for(it: i64) -> i16 {
    let n = |x: &str| name_to_item(x).unwrap_or(-1);
    if [
        n("leather_helmet"),
        n("golden_helmet"),
        n("chainmail_helmet"),
        n("iron_helmet"),
        n("diamond_helmet"),
        n("netherite_helmet"),
        n("turtle_helmet"),
    ]
    .contains(&it)
    {
        return 5;
    }
    if [
        n("leather_chestplate"),
        n("golden_chestplate"),
        n("chainmail_chestplate"),
        n("iron_chestplate"),
        n("diamond_chestplate"),
        n("netherite_chestplate"),
        n("elytra"),
    ]
    .contains(&it)
    {
        return 6;
    }
    if [
        n("leather_leggings"),
        n("golden_leggings"),
        n("chainmail_leggings"),
        n("iron_leggings"),
        n("diamond_leggings"),
        n("netherite_leggings"),
    ]
    .contains(&it)
    {
        return 7;
    }
    if [
        n("leather_boots"),
        n("golden_boots"),
        n("chainmail_boots"),
        n("iron_boots"),
        n("diamond_boots"),
        n("netherite_boots"),
    ]
    .contains(&it)
    {
        return 8;
    }
    -1
}
// remove up to n items of the stack at k (n>=stack size takes all);
// returns what was removed
fn map_take(map: &mut HashMap<i16, Stack>, k: i16, n: u8) -> Option<Stack> {
    let st = map.get(&k).cloned()?;
    if st.1 == 0 {
        return None;
    }
    let take = n.min(st.1);
    if take >= st.1 {
        map.remove(&k);
    } else {
        map.get_mut(&k).unwrap().1 -= take;
    }
    Some((st.0, take, st.2))
}
// Slot.safeInsert for an unrestricted slot: compatible components, bounded count.
fn insert_into_slot(map: &mut HashMap<i16, Stack>, slot: i16, source: &mut Stack, amount: u8) {
    let target = map.get(&slot).filter(|st| st.1 > 0);
    if target.is_some_and(|st| st.0 != source.0 || st.2 != source.2) {
        return;
    }
    let count = target.map_or(0, |st| st.1);
    let room = (stack_max_of(source) - i64::from(count)).max(0) as u8;
    let moved = amount.min(source.1).min(room);
    if moved > 0 {
        map.insert(slot, (source.0, count + moved, source.2.clone()));
        source.1 -= moved;
        if source.1 == 0 {
            *source = (0, 0, Vec::new());
        }
    }
}
// Inventory pickup uses hotbar-first native indices, not menu traversal order.
fn pickup_inventory_order() -> impl Iterator<Item = i16> + Clone {
    (36..=44).chain(9..=35)
}
fn pickup_is_damaged(st: &Stack) -> bool {
    comp_varint(&st.2, COMP_DAMAGE).unwrap_or(0) > 0
        && comp_get(&st.2, COMP_UNBREAKABLE).is_none()
        && comp_varint(&st.2, COMP_MAX_DAMAGE)
            .or_else(|| tool_max_damage(st.0))
            .or_else(|| armor_max_damage(st.0))
            .is_some_and(|max| max > 0)
}
fn pickup_slot_with_space(map: &HashMap<i16, Stack>, selected: i16, source: &Stack) -> Option<i16> {
    [36 + selected, 45]
        .into_iter()
        .chain(pickup_inventory_order())
        .find(|key| {
            map.get(key).is_some_and(|st| {
                st.1 > 0
                    && st.0 == source.0
                    && st.2 == source.2
                    && stack_max_of(st) > 1
                    && !pickup_is_damaged(st)
                    && i64::from(st.1) < stack_max_of(st)
            })
        })
}
// 1.21.1 Inventory.add (cmw.f/c): selected -> offhand -> hotbar/main
// merges, then hotbar/main empties, repeated until exhausted or no progress.
// Horizon inventory_pickup informed the port; actual-class vectors pin this version.
fn pickup_into_inventory(
    map: &mut HashMap<i16, Stack>,
    selected: i16,
    creative: bool,
    source: &mut Stack,
) -> bool {
    if source.0 == 0 || source.1 == 0 {
        return false;
    }
    let damaged = pickup_is_damaged(source);
    loop {
        let before = source.1;
        let free = || pickup_inventory_order().find(|key| map.get(key).is_none_or(|st| st.1 == 0));
        let slot = if damaged {
            free()
        } else {
            pickup_slot_with_space(map, selected, source).or_else(free)
        };
        if let Some(slot) = slot {
            if damaged {
                map.insert(slot, std::mem::take(source));
            } else {
                insert_into_slot(map, slot, source, source.1);
            }
        }
        if source.1 == 0 || source.1 == before {
            // Java compares with the LAST iteration's count. A partial insert
            // that subsequently stalls returns false, despite changing slots.
            if creative && source.1 == before {
                *source = (0, 0, vec![]);
                return true;
            }
            return source.1 < before;
        }
    }
}
fn merge_into_slots(
    map: &mut HashMap<i16, Stack>,
    order: impl Iterator<Item = i16>,
    source: &mut Stack,
) {
    if stack_max_of(source) <= 1 {
        return;
    }
    for slot in order {
        if source.1 == 0 {
            break;
        }
        if map.get(&slot).is_some_and(|st| st.1 > 0) {
            insert_into_slot(map, slot, source, source.1);
        }
    }
}
// Horizon menu::move_to's two phases (MIT): merge first, then one empty slot.
// The order is supplied by the menu; a chest sends items to inventory in reverse.
fn move_to_slots(
    map: &mut HashMap<i16, Stack>,
    order: impl Iterator<Item = i16> + Clone,
    mut source: Stack,
) -> Stack {
    merge_into_slots(map, order.clone(), &mut source);
    if source.1 > 0 {
        for slot in order {
            if map.get(&slot).is_none_or(|st| st.1 == 0) {
                let amount = source.1;
                insert_into_slot(map, slot, &mut source, amount);
                break;
            }
        }
    }
    source
}
// ---------- QOL-DEATH: opt-in death-drop original-slot restore ----------
// Strictly opt-in: `drop_tick` only consults the hint table while this flag is
// set, so a server that never opts in runs the vanilla `pickup_into_inventory`
// path unchanged. The flag lives here until the lane owning the death path
// exposes a toggle; the setter exists for that wiring and for tests.
static DEATH_SLOT_RESTORE: AtomicBool = AtomicBool::new(false);
fn death_slot_restore_enabled() -> bool {
    DEATH_SLOT_RESTORE.load(Ordering::Relaxed)
}
#[cfg_attr(not(test), allow(dead_code))]
fn set_death_slot_restore(enabled: bool) {
    DEATH_SLOT_RESTORE.store(enabled, Ordering::Relaxed);
}

/// Where a death-scattered stack last lived: a hotbar/main storage slot
/// (9..=44), an armor slot (5..=8) or the offhand (45). Keyed by the drop
/// entity's eid — the `drop_id` the pickup path sees — and honoured only for
/// the named owner.
struct DeathSlotHint {
    owner_name: String,
    original_slot: i16,
}
/// Hard bound on live hints so a death-drop flood cannot grow this table
/// without end; the oldest recorded slot loses its hint first.
const MAX_DEATH_SLOT_HINTS: usize = 4096;
struct DeathSlotHintTable {
    map: HashMap<i32, DeathSlotHint>,
    order: std::collections::VecDeque<i32>,
}
static DEATH_SLOT_HINTS: OnceLock<Mutex<DeathSlotHintTable>> = OnceLock::new();
fn death_slot_hints() -> &'static Mutex<DeathSlotHintTable> {
    DEATH_SLOT_HINTS.get_or_init(|| {
        Mutex::new(DeathSlotHintTable {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
        })
    })
}
/// Record that `drop_eid` was scattered from `original_slot` of `owner_name`'s
/// inventory. Leaf lock: never held while another lock is taken.
#[cfg_attr(not(test), allow(dead_code))]
fn death_slot_hint_record(drop_eid: i32, owner_name: &str, original_slot: i16) {
    let mut hints = death_slot_hints().lock().unwrap_or_else(|e| e.into_inner());
    if !hints.map.contains_key(&drop_eid) {
        hints.order.push_back(drop_eid);
    }
    hints.map.insert(
        drop_eid,
        DeathSlotHint {
            owner_name: owner_name.to_owned(),
            original_slot,
        },
    );
    while hints.order.len() > MAX_DEATH_SLOT_HINTS {
        if let Some(old) = hints.order.pop_front() {
            hints.map.remove(&old);
        }
    }
}
/// Drop a hint whose entity is gone (picked dry, despawned, merged or evicted)
/// so the table cannot hand a dead entity's slot to a later drop.
fn death_slot_hint_forget(drop_eid: i32) {
    let mut hints = death_slot_hints().lock().unwrap_or_else(|e| e.into_inner());
    if hints.map.remove(&drop_eid).is_some() {
        hints.order.retain(|eid| *eid != drop_eid);
    }
}
/// The slot a pickup may restore to: `None` while the feature is off, when no
/// hint exists for this drop, or when the drop is not the picker's own death
/// item (a foreign player always gets the normal pickup).
fn death_slot_hint_for(drop_eid: i32, picker_name: &str) -> Option<i16> {
    if !death_slot_restore_enabled() {
        return None;
    }
    death_slot_hints()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map
        .get(&drop_eid)
        .filter(|hint| hint.owner_name == picker_name)
        .map(|hint| hint.original_slot)
}
/// `original_slot` is restorable for `st` when it names a storage slot, the
/// offhand, or the armor slot `st`'s piece equips into. A crafting-grid index,
/// an out-of-range index or the wrong armor slot is not a legal target, so the
/// insert falls back to the normal pickup.
fn death_restore_slot_accepts(original_slot: i16, st: &Stack) -> bool {
    match original_slot {
        5..=8 => armor_slot_for(st.0) == original_slot,
        9..=45 => true,
        _ => false,
    }
}
/// Collision-safe original-slot restore for a death-scattered stack.
///
/// Conservation contract: the item multiset of `inv` plus the returned
/// remainder equals `inv` plus `picked` on entry — under every collision no
/// item is overwritten, voided or duplicated.
///
/// - Case A (slot empty): `picked` goes straight into `original_slot`.
/// - Case B (slot holds the same stack): merge to the slot's stack limit, then
///   the remainder inserts like any picked-up stack.
/// - Case C (slot holds a different item): the collider relocates to a free
///   hotbar/main slot — never into an armor slot — and `picked` takes the
///   slot back. With no free slot nothing is moved: `picked` merges elsewhere
///   or comes back to the ground whole.
/// - Case D (slot not restorable for this item): normal inventory insert.
fn insert_with_death_slot_restore(
    inv: &mut HashMap<i16, Stack>,
    picked: Stack,
    original_slot: i16,
) -> Option<Stack> {
    if picked.0 == 0 || picked.1 == 0 {
        return None;
    }
    let mut picked = picked;
    if !death_restore_slot_accepts(original_slot, &picked) {
        pickup_into_inventory(inv, 0, false, &mut picked);
        return (picked.1 > 0).then_some(picked);
    }
    match inv.get(&original_slot).filter(|st| st.1 > 0).cloned() {
        None => {
            inv.insert(original_slot, picked);
            None
        }
        Some(existing) if existing.0 == picked.0 && existing.2 == picked.2 => {
            let amount = picked.1;
            insert_into_slot(inv, original_slot, &mut picked, amount);
            if picked.1 > 0 {
                pickup_into_inventory(inv, 0, false, &mut picked);
            }
            (picked.1 > 0).then_some(picked)
        }
        Some(existing) => {
            if let Some(free) = pickup_inventory_order()
                .find(|slot| inv.get(slot).is_none_or(|st| st.1 == 0))
            {
                inv.insert(free, existing);
                inv.insert(original_slot, picked);
                None
            } else {
                pickup_into_inventory(inv, 0, false, &mut picked);
                (picked.1 > 0).then_some(picked)
            }
        }
    }
}
/// Spawn a drop that carries a death-slot hint: a real item entity whose
/// pickup may restore it to `original_slot` for `owner_name` while the opt-in
/// is set. Returns the drop id, or -1 for an empty stack.
#[cfg_attr(not(test), allow(dead_code))]
fn spawn_death_drop(
    (x, y, z): (f64, f64, f64),
    stack: Stack,
    owner_name: &str,
    original_slot: i16,
) -> i32 {
    if stack.1 == 0 {
        return -1;
    }
    let eid = drop_spawn(
        x,
        y,
        z,
        stack.0,
        stack.1,
        stack.2,
        -(server_ticks(30) as i32),
        None,
        None,
    );
    death_slot_hint_record(eid, owner_name, original_slot);
    eid
}
// The single/double chest menu shares one transfer owner; storage remains per container.
fn quick_move_chest(
    all: &mut HashMap<ContainerKey, HashMap<i16, Stack>>,
    inv: &mut HashMap<i16, Stack>,
    cp: ContainerKey,
    cp2: Option<ContainerKey>,
    slot: i16,
) {
    let rows = if cp2.is_some() { 54 } else { 27 };
    if !(0..rows + 36).contains(&slot) {
        return;
    }
    if slot < rows {
        let (pos, key) = if slot < 27 {
            (cp, slot)
        } else {
            (cp2.unwrap(), slot - 27)
        };
        if let Some(st) = map_take(all.entry(pos).or_default(), key, u8::MAX) {
            let rem = move_to_slots(inv, (9..=44).rev(), st);
            if rem.1 > 0 {
                all.get_mut(&pos).unwrap().insert(key, rem);
            }
        }
    } else if let Some(mut rem) = map_take(inv, slot - rows + 9, u8::MAX) {
        // Merge across BOTH halves before considering any empty slot.
        for pos in [cp].into_iter().chain(cp2) {
            merge_into_slots(all.entry(pos).or_default(), 0..27, &mut rem);
        }
        'empty: for pos in [cp].into_iter().chain(cp2) {
            if rem.1 == 0 {
                break;
            }
            let map = all.entry(pos).or_default();
            for key in 0..27 {
                if map.get(&key).is_none_or(|st| st.1 == 0) {
                    let amount = rem.1;
                    insert_into_slot(map, key, &mut rem, amount);
                    break 'empty;
                }
            }
        }
        if rem.1 > 0 {
            inv.insert(slot - rows + 9, rem);
        }
    }
}
// drag-paint one slot: place `per` items of the cursor stack into the
// resolved window slot (merging if compatible), then resync
fn paint_slot(me: &Client, wsl: i16, per: u8, cur: &mut Stack) {
    // Never paint more than the cursor holds: every arm below mutates the
    // destination and then does `cur.1 -= per`, which must not underflow.
    if per == 0 || cur.1 < per {
        return;
    }
    let chest_pos = *me.open_chest.lock().unwrap_or_else(|e| e.into_inner());
    let furnace_pos = *me.open_furnace.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cp, cp2)) = chest_pos {
        if !window_live_for(me, (cp, cp2)) {
            invalidate_client_chest_window(&client_snapshot(), me, (cp, cp2));
            return;
        }
        let rows = if cp2.is_some() { 54i16 } else { 27i16 };
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        if wsl < 0 || wsl >= rows + 36 {
            return;
        }
        let (m, gslot) = if wsl < 27 {
            (all.entry(cp).or_default(), wsl)
        } else if wsl < rows {
            (all.entry(cp2.unwrap()).or_default(), wsl - 27)
        } else {
            (&mut *inv, wsl - rows + 9)
        };
        let st = m.entry(gslot).or_insert((0, 0, Vec::new()));
        if (st.1 == 0 || (st.0 == cur.0 && st.2 == cur.2))
            && (st.1 as i64 + per as i64) <= item_stack_max(cur.0, &cur.2)
        {
            if st.1 == 0 {
                *st = (cur.0, per, cur.2.clone());
            } else {
                st.1 += per;
            }
            cur.1 -= per;
            resync_slot(me, 1, wsl, st);
        }
    } else if furnace_pos.is_some() {
        // vanilla doesn't allow painting into furnace input/fuel/output;
        // only inventory slots 3..38 (key = wsl+6)
        if (3..39).contains(&wsl) {
            let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
            let k = wsl + 6;
            let st = inv.entry(k).or_insert((0, 0, Vec::new()));
            if (st.1 == 0 || (st.0 == cur.0 && st.2 == cur.2))
                && (st.1 as i64 + per as i64) <= item_stack_max(cur.0, &cur.2)
            {
                if st.1 == 0 {
                    *st = (cur.0, per, cur.2.clone());
                } else {
                    st.1 += per;
                }
                cur.1 -= per;
                resync_slot(me, 2, wsl, st);
            }
        }
    } else {
        // inventory window: paintable slots are 9-44 (inv keys) + craft 1-4
        if (9..=44).contains(&wsl) {
            let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
            let st = inv.entry(wsl).or_insert((0, 0, Vec::new()));
            if (st.1 == 0 || (st.0 == cur.0 && st.2 == cur.2))
                && (st.1 as i64 + per as i64) <= item_stack_max(cur.0, &cur.2)
            {
                if st.1 == 0 {
                    *st = (cur.0, per, cur.2.clone());
                } else {
                    st.1 += per;
                }
                cur.1 -= per;
                resync_slot(me, 0, wsl, st);
            }
        } else if (1..=4).contains(&wsl) {
            let mut grid = me.craft.lock().unwrap_or_else(|e| e.into_inner());
            let st = grid.entry(wsl).or_insert((0, 0, Vec::new()));
            if (st.1 == 0 || (st.0 == cur.0 && st.2 == cur.2))
                && (st.1 as i64 + per as i64) <= item_stack_max(cur.0, &cur.2)
            {
                if st.1 == 0 {
                    *st = (cur.0, per, cur.2.clone());
                } else {
                    st.1 += per;
                }
                cur.1 -= per;
                resync_slot(me, 0, wsl, st);
            }
        }
    }
}
fn dr_btn_right(btn: i32) -> bool {
    btn >= 4
}
fn send_chest_slot(me: &Client, wslot: i16, st: &Stack) {
    let norm = if st.1 > 0 {
        st.clone()
    } else {
        (0, 0, Vec::new())
    };
    let state_id = {
        let mut sync_guard = me.chest_sync.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sync) = sync_guard.as_mut() {
            sync.state_id = (sync.state_id + 1) & 0x7fff;
            if wslot >= 0 {
                if let Some(slot_ref) = sync.last_slots.get_mut(wslot as usize) {
                    *slot_ref = norm.clone();
                }
            }
            sync.state_id
        } else {
            0
        }
    };
    let mut p = vec![1u8];
    write_varint(&mut p, i64::from(state_id));
    p.extend_from_slice(&wslot.to_be_bytes());
    p.extend_from_slice(&slot_wire(&norm));
    if let Ok(mut w) = me.sock.lock() {
        let _ = send_packet(&mut w, 0x15, p);
    }
}
// re-send one window slot so the client can't drift
fn resync_slot(me: &Client, wid: u8, wslot: i16, st: &Stack) {
    if wid == 1 {
        send_chest_slot(me, wslot, st);
        if let Some((first, second)) = *me.open_chest.lock().unwrap_or_else(|e| e.into_inner()) {
            let chest_rows = if second.is_some() { 54 } else { 27 };
            if (0..chest_rows).contains(&wslot) {
                let key = if wslot < 27 { first } else { second.unwrap_or(first) };
                if let ContainerKey::Block((dim, x, y, z)) = key {
                    if !is_ender_key((x, y, z)) {
                        wake_hoppers_around((dim, x, y, z));
                    }
                }
            }
        }
        return;
    }
    let mut p = vec![wid];
    // The player inventory menu shares `inv_state_id` (wid 0/2/3 and the
    // -1/-2 echoes); other windows keep their own counters (`send_chest_slot`
    // for wid 1). Every packet consumes one `incrementStateId` like vanilla.
    write_varint(&mut p, i64::from(inv_state_next(me)));
    p.extend_from_slice(&wslot.to_be_bytes());
    p.extend_from_slice(&slot_wire(st));
    if let Ok(mut w) = me.sock.lock() {
        let _ = send_packet(&mut w, 0x15, p);
    }
}
// Plain, unrestricted slots only; result/equipment slots retain their own policy.
// Adapted from Horizon menu::pickup_inner (MIT; third_party/licenses/horizon.txt),
// checked against 1.21.1 AbstractContainerMenu.doClick / Slot.safeInsert.
fn click_slot(m: &mut HashMap<i16, Stack>, cur: &mut Stack, slot: i16, btn: i8) {
    if !matches!(btn, 0 | 1) {
        return;
    }
    if cur.1 == 0 {
        // Right-click an occupied bundle with an empty cursor extracts the last
        // inserted stack (`BundleItem.overrideOtherStackedOnMe`, LIFO).
        if btn == 1 {
            if let Some(st) = m.get(&slot) {
                if is_bundle_item(st.0) {
                    let mut contents = bundle_contents_of(st).unwrap_or_default();
                    if let Some((out, data)) = bundle_try_extract(&mut contents) {
                        let mut bundle = st.clone();
                        comp_set(&mut bundle.2, COMP_BUNDLE_CONTENTS, data);
                        m.insert(slot, bundle);
                        *cur = out;
                        return;
                    }
                }
            }
        }
        if let Some(st) = m.get(&slot) {
            let amount = if btn == 0 { st.1 } else { st.1.div_ceil(2) };
            if let Some(taken) = map_take(m, slot, amount) {
                *cur = taken;
            }
        }
        return;
    }
    // Right-click `bundle_contents` interactions, both directions:
    // holding a bundle inserts the slot's stack into it; clicking a bundle
    // with a stack inserts as much of the cursor as the 64-weight budget fits.
    if btn == 1 {
        let target = m.get(&slot).cloned().unwrap_or((0, 0, Vec::new()));
        if is_bundle_item(cur.0) && target.1 > 0 && !is_bundle_item(target.0) {
            let mut contents = bundle_contents_of(cur).unwrap_or_default();
            let (data, remaining) = bundle_try_insert(&mut contents, &target);
            if remaining < target.1 {
                comp_set(&mut cur.2, COMP_BUNDLE_CONTENTS, data);
                if remaining == 0 {
                    m.remove(&slot);
                } else {
                    m.insert(slot, (target.0, remaining, target.2));
                }
            }
            return;
        }
        if is_bundle_item(target.0) && !is_bundle_item(cur.0) {
            let mut contents = bundle_contents_of(&target).unwrap_or_default();
            let (data, remaining) = bundle_try_insert(&mut contents, cur);
            if remaining < cur.1 {
                let mut bundle = target;
                comp_set(&mut bundle.2, COMP_BUNDLE_CONTENTS, data);
                m.insert(slot, bundle);
                cur.1 = remaining;
                if cur.1 == 0 {
                    *cur = (0, 0, Vec::new());
                }
            }
            return;
        }
    }
    let target = m.get(&slot).filter(|st| st.1 > 0).cloned();
    if target
        .as_ref()
        .is_none_or(|st| st.0 == cur.0 && st.2 == cur.2)
    {
        let amount = if btn == 0 { cur.1 } else { 1 };
        insert_into_slot(m, slot, cur, amount);
    } else if i64::from(cur.1) <= stack_max_of(cur) {
        // Both mouse buttons swap unlike stacks, but never bypass the slot limit.
        *cur = m.insert(slot, cur.clone()).unwrap();
    }
}
// vanilla item tags used by recipes
fn tag_planks(it: i64) -> bool {
    (36..=46).contains(&it)
}
// log id -> its own plank item (vanilla: oak_log->oak_planks, not oak for all)
fn planks_for_log(it: i64) -> Option<i64> {
    Some(match it {
        132 => 36,
        133 => 37,
        134 => 38,
        135 => 39,
        136 => 40,
        137 => 41,
        138 => 42,
        139 => 43,
        144 => 44, // logs + bamboo_block
        142 => 45,
        143 => 46, // crimson/warped stems
        _ => return None,
    })
}
fn craft_result(grid: &HashMap<i16, Stack>) -> Option<Stack> {
    if let Some(r) = json_craft_match(grid, 2) {
        return Some(r);
    }
    let stick = name_to_item("stick")?;
    let table = name_to_item("crafting_table")?;
    let vals: Vec<(i16, i64)> = (1..=4)
        .filter_map(|i| grid.get(&i).map(|(it, _, _)| (i, *it)))
        .collect();
    if vals.len() == 1 {
        if let Some(p) = planks_for_log(vals[0].1) {
            return Some((p, 4, Vec::new()));
        }
    }
    if vals.len() == 4 && vals.iter().all(|v| tag_planks(v.1)) {
        return Some((table, 1, Vec::new()));
    }
    // sticks: two planks vertical (1&3 or 2&4)
    if vals.len() == 2
        && vals.iter().all(|v| tag_planks(v.1))
        && ((vals[0].0 == 1 && vals[1].0 == 3) || (vals[0].0 == 2 && vals[1].0 == 4))
    {
        return Some((stick, 4, Vec::new()));
    }
    // leather armor + dyes: vanilla shapeless `armor_dye` recipe, mixing the
    // `dyed_color` component like the cauldron does.
    dyed_craft_any((1..=4).filter_map(|i| grid.get(&i)))
}
fn open_furnace_window(me: &Client, inv: &HashMap<i16, Stack>, slots: &[Stack; 3]) {
    let mut p = Vec::new();
    write_varint(&mut p, 2); // windowId 2 (chest uses 1)
    write_varint(&mut p, 14); // furnace menu
    p.extend_from_slice(&chat_nbt("Furnace"));
    if let Ok(mut s) = me.sock.lock() {
        let _ = send_packet(&mut s, 0x33, p);
    }
    furnace_push_items_snap(me, inv, slots);
}
fn furnace_slot_snapshot_dim(dim: DimId, pos: (i32, i32, i32)) -> [Stack; 3] {
    let fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
    let f = fs.get(&(dim, pos.0, pos.1, pos.2));
    [
        f.map(|f| f.input.clone()).unwrap_or((0, 0, Vec::new())),
        f.map(|f| f.fuel.clone()).unwrap_or((0, 0, Vec::new())),
        f.map(|f| f.output.clone()).unwrap_or((0, 0, Vec::new())),
    ]
}
fn furnace_push_items_snap(me: &Client, inv: &HashMap<i16, Stack>, slots: &[Stack; 3]) {
    let mut q = vec![2u8];
    write_varint(&mut q, i64::from(inv_state_next(me)));
    write_varint(&mut q, 39);
    for slot in slots {
        q.extend_from_slice(&slot_wire(slot));
    }
    for wsl in 3..39i16 {
        let (it, c, _cc) = inv.get(&(wsl + 6)).cloned().unwrap_or((0, 0, Vec::new()));
        q.extend_from_slice(&slot_wire(&(it, c, _cc)));
    }
    q.extend_from_slice(&slot_wire(&(0, 0, Vec::new())));
    if let Ok(mut s) = me.sock.lock() {
        let _ = send_packet(&mut s, 0x13, q);
    }
}
// The frozen reference in furnace_cache_tests still exercises the original signature.
#[cfg(test)]
fn furnace_push_items(me: &Client, pos: (i32, i32, i32), inv: &HashMap<i16, Stack>) {
    let slots = furnace_slot_snapshot_dim(DimId::Overworld, pos);
    furnace_push_items_snap(me, inv, &slots);
}
fn ingr_match(ig: &Ingr, item: i64, depth: usize) -> bool {
    match ig {
        Ingr::Item(i) => *i == item,
        Ingr::Tag(t) => {
            if depth > 8 {
                return false;
            }
            let tag = if t.contains(':') {
                t.clone()
            } else {
                format!("minecraft:{t}")
            };
            // The datapack registry's `item` tags expand nested `#ref`s with a
            // cycle guard, so tags loaded by `/reload` (or a pack the legacy
            // mirror has not been refreshed with) still answer membership.
            {
                let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
                if let Some(map) = registry.tags.get("item") {
                    if map.contains_key(&tag) && dp_item_tag_match(map, &tag, item) {
                        return true;
                    }
                }
            }
            let members = item_tags()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(t)
                .cloned()
                .unwrap_or_default();
            members.iter().any(|m| {
                if let Some(inner) = m.strip_prefix('#') {
                    ingr_match(&Ingr::Tag(inner.to_string()), item, depth + 1)
                } else {
                    name_to_item(m).map(|i| i == item).unwrap_or(false)
                }
            }) || vanilla_item_tag_fallback(&tag, item)
        }
    }
}

// ---------- Lane W2: datapack recipes + tags in the live menus ----------

/// `true` when `tag` expands (through nested `#ref`s, cycle-guarded) to
/// `item` in the given `item` tag map. Members resolve through `name_to_item`
/// exactly like the loader's own ingredient normalization does.
fn dp_item_tag_match(map: &HashMap<String, Vec<String>>, tag: &str, item: i64) -> bool {
    let mut members = Vec::new();
    tag_expand_into(
        map,
        tag,
        &mut HashSet::new(),
        &mut HashSet::new(),
        &mut members,
    );
    members
        .iter()
        .any(|m| name_to_item(m.trim_start_matches("minecraft:")) == Some(item))
}

/// The captured vanilla datapack ships no item tags, so the memberships the
/// 1.21.1 smithing recipes rely on are filled in here: the same fallbacks the
/// crafting parity harness uses for `diamond_tool_materials`,
/// `trimmable_armor` and `trim_materials`.
fn vanilla_item_tag_fallback(tag: &str, item: i64) -> bool {
    let Some(name) = item_to_name(item) else {
        return false;
    };
    match tag {
        "minecraft:diamond_tool_materials" => {
            tool_parts(name).is_some_and(|(tier, _)| tier == "diamond")
        }
        "minecraft:trimmable_armor" => armor_parts(name).is_some(),
        "minecraft:trim_materials" => TRIM_MATERIAL_ITEMS.contains(&name),
        _ => false,
    }
}

/// `ingr_match` for a caller that already holds the datapack registry: the
/// registry `item` tags first, then the legacy mirror, then the vanilla
/// smithing-tag fallbacks. `legacy` must be locked *before* the registry, the
/// same order `DatapackContent::publish` acquires them.
fn dp_ingr_match_locked(
    registry: &DatapackRegistry,
    legacy: &HashMap<String, Vec<String>>,
    ig: &Ingr,
    item: i64,
    depth: usize,
) -> bool {
    match ig {
        Ingr::Item(i) => *i == item,
        Ingr::Tag(t) => {
            if depth > 8 {
                return false;
            }
            let tag = if t.contains(':') {
                t.clone()
            } else {
                format!("minecraft:{t}")
            };
            if let Some(map) = registry.tags.get("item") {
                if map.contains_key(&tag) && dp_item_tag_match(map, &tag, item) {
                    return true;
                }
            }
            if legacy.get(&tag).cloned().unwrap_or_default().iter().any(|m| {
                match m.strip_prefix('#') {
                    Some(inner) => dp_ingr_match_locked(
                        registry,
                        legacy,
                        &Ingr::Tag(inner.to_owned()),
                        item,
                        depth + 1,
                    ),
                    None => name_to_item(m) == Some(item),
                }
            }) {
                return true;
            }
            vanilla_item_tag_fallback(&tag, item)
        }
    }
}

/// `true` when `stack` matches any alternative of one recipe ingredient slot.
fn dp_slot_matches(
    registry: &DatapackRegistry,
    legacy: &HashMap<String, Vec<String>>,
    ingredients: &[Ingr],
    stack: &Stack,
) -> bool {
    stack.1 > 0
        && ingredients
            .iter()
            .any(|ig| dp_ingr_match_locked(registry, legacy, ig, stack.0, 0))
}

/// Datapack `stonecutting` recipes matching one input item, in stable order.
/// `Ingr::Tag` alternatives resolve through the loaded `item` tags.
fn live_dp_stonecut_recipes(item: i64) -> Vec<(i64, u8)> {
    let legacy = item_tags().lock().unwrap_or_else(|e| e.into_inner());
    let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<(i64, u8)> = registry
        .stonecutting
        .iter()
        .filter(|recipe| {
            recipe
                .ingredients
                .iter()
                .any(|ig| dp_ingr_match_locked(&registry, &legacy, ig, item, 0))
        })
        .map(|recipe| (recipe.result, recipe.count))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The offered list the live stonecutter actually crafts from: datapack
/// recipes first, then the builtin table for inputs the loaded packs do not
/// cover (and the whole table when no datapack is loaded).
fn live_stonecutter_recipes(input: &Stack) -> Vec<(i64, u8)> {
    if input.1 == 0 {
        return Vec::new();
    }
    let mut out = live_dp_stonecut_recipes(input.0);
    if let Some(name) = item_to_name(input.0) {
        for recipe in wm_stonecut_recipes(name) {
            if !out.contains(&recipe) {
                out.push(recipe);
            }
        }
    }
    out
}

fn live_stonecutter_result(menu: &StonecutterMenu) -> Stack {
    menu.selected_recipe
        .and_then(|index| live_stonecutter_recipes(&menu.input).get(index).copied())
        .map(|(item, count)| (item, count, Vec::new()))
        .unwrap_or_default()
}

fn live_stonecutter_select(menu: &mut StonecutterMenu, index: usize) -> bool {
    if index < live_stonecutter_recipes(&menu.input).len() {
        menu.selected_recipe = Some(index);
        true
    } else {
        menu.selected_recipe = None;
        false
    }
}

fn live_stonecutter_take(menu: &mut StonecutterMenu) -> Option<Stack> {
    let out = live_stonecutter_result(menu);
    if out.1 == 0 || menu.input.1 == 0 {
        return None;
    }
    menu.input.1 -= 1;
    if menu.input.1 == 0 {
        menu.input = Stack::default();
        menu.selected_recipe = None;
    }
    Some(out)
}

/// `SmithingMenu.mayPlace` over the loaded datapack recipes: the recipe
/// manager's template/base/addition ingredient sets decide what each slot
/// accepts, covering custom templates and additions the builtin rules reject.
fn live_dp_smithing_slot_accepts(slot: usize, stack: &Stack) -> bool {
    let legacy = item_tags().lock().unwrap_or_else(|e| e.into_inner());
    let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
    registry
        .smithing_transform
        .iter()
        .chain(&registry.smithing_trim)
        .any(|recipe| {
            let ingredients = match slot {
                0 => &recipe.template,
                1 => &recipe.base,
                _ => &recipe.addition,
            };
            dp_slot_matches(&registry, &legacy, ingredients, stack)
        })
}

/// A matched datapack `smithing_trim`: the base stack keeps its item and
/// components and gains `armor_trim` when pattern and material resolve to the
/// trim registries. An already-identical trim refuses, like vanilla.
fn live_dp_smithing_trim_output(template: &Stack, base: &Stack, addition: &Stack) -> Option<Stack> {
    let pattern = item_to_name(template.0)?.strip_suffix("_armor_trim_smithing_template")?;
    let material = item_to_name(addition.0)?;
    let pattern_id = TRIM_PATTERNS.iter().position(|name| *name == pattern)?;
    let material_id = TRIM_MATERIAL_ITEMS
        .iter()
        .position(|name| *name == material)?;
    let trim = [material_id as u8 + 1, pattern_id as u8 + 1, 1];
    if comp_get(&base.2, COMP_TRIM).and_then(|data| data.get(..2)) == Some(&trim[..2]) {
        return None;
    }
    let mut result = (base.0, 1, base.2.clone());
    comp_set(&mut result.2, COMP_TRIM, trim.to_vec());
    Some(result)
}

/// The live smithing table's output: loaded `smithing_transform` and
/// `smithing_trim` recipes first (tag ingredients resolved through the `item`
/// tags), then the builtin upgrade/trim rules when no datapack recipe
/// applies, so vanilla behavior survives with no datapack loaded.
fn live_smithing_result(template: &Stack, base: &Stack, addition: &Stack) -> Option<Stack> {
    if template.1 == 0 || base.1 == 0 || addition.1 == 0 {
        return None;
    }
    {
        let legacy = item_tags().lock().unwrap_or_else(|e| e.into_inner());
        let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
        let matches = |recipe: &SmithingRecipe| {
            dp_slot_matches(&registry, &legacy, &recipe.template, template)
                && dp_slot_matches(&registry, &legacy, &recipe.base, base)
                && dp_slot_matches(&registry, &legacy, &recipe.addition, addition)
        };
        if let Some(result) = registry.smithing_transform.iter().find_map(|recipe| {
            if !matches(recipe) {
                return None;
            }
            // Vanilla `transmuteCopy`: the result item keeps the base's
            // component patch.
            recipe.result.map(|item| (item, 1, base.2.clone()))
        }) {
            return Some(result);
        }
        if registry.smithing_trim.iter().any(matches) {
            if let Some(result) = live_dp_smithing_trim_output(template, base, addition) {
                return Some(result);
            }
        }
    }
    smithing_result(template, base, addition)
}

fn live_smithing_take(
    menu: &mut SmithingMenu,
    can_receive: impl FnOnce(&Stack) -> bool,
) -> Option<Stack> {
    let result =
        live_smithing_result(&menu.template, &menu.base, &menu.addition).filter(can_receive)?;
    for stack in [&mut menu.template, &mut menu.base, &mut menu.addition] {
        consume_menu_stack(stack, 1);
    }
    Some(result)
}

/// The analog signal a live comparator reads at `pos`: the W-BE block-entity
/// store first (lectern, chiseled bookshelf, jukebox, decorated pot, crafter,
/// brewing stand, spawner), then the live block entities menus materialize,
/// then the classic container fullness for chests, barrels, dispensers,
/// droppers, hoppers and furnaces.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn live_comparator_signal_at(dim: DimId, x: i32, y: i32, z: i32) -> Option<u8> {
    if let Some(signal) = comparator_signal_at_be(dim, (x, y, z)) {
        return Some(signal);
    }
    {
        let store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, entity)) = store.entries.get(&(dim, x, y, z)) {
            return Some(match entity {
                LiveBlockEntity::Lectern(lectern, _) => {
                    lectern.comparator_signal().clamp(0, 15) as u8
                }
                LiveBlockEntity::ChiseledBookshelf(shelf) => shelf.comparator_signal(),
                LiveBlockEntity::DecoratedPot(pot) => {
                    let stack_limit = pot
                        .item
                        .as_ref()
                        .and_then(BlockEntityItem::to_stack)
                        .map(|stack| stack_max_of(&stack) as u8)
                        .unwrap_or(64);
                    pot.comparator_signal(stack_limit)
                }
                LiveBlockEntity::Crafter(crafter, _) => crafter.comparator_signal(),
                LiveBlockEntity::Jukebox(box_) => box_.comparator_signal(jukebox_song_for),
                // Stored block entities with no analog output report 0, just
                // like their W-BE counterparts (spawner, beacon, campfire...).
                _ => 0,
            });
        }
    }
    let fullness = |stack: &Stack| (u32::from(stack.1), stack_max_of(stack) as u32);
    let name = breg()?.name_of(block_at_dim(dim, x, y, z));
    if name == "minecraft:brewing_stand" {
        let stands = live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner());
        let slots = stands
            .get(&(dim, x, y, z))
            .map(|entry| entry.menu.slots.iter().map(fullness).collect::<Vec<_>>())
            .unwrap_or_default();
        return Some(container_analog_output_signal(&slots, 5));
    }
    let size = match name {
        "minecraft:chest" | "minecraft:trapped_chest" | "minecraft:barrel" => 27,
        "minecraft:dispenser" | "minecraft:dropper" => 9,
        "minecraft:hopper" => 5,
        "minecraft:furnace" | "minecraft:blast_furnace" | "minecraft:smoker" => {
            let furnaces = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            let slots = furnaces
                .get(&(dim, x, y, z))
                .map(|f| [&f.input, &f.fuel, &f.output].map(fullness));
            return Some(container_analog_output_signal(
                slots.as_ref().map_or(&[], |s| &s[..]),
                3,
            ));
        }
        _ => return None,
    };
    let chests = chests().lock().unwrap_or_else(|e| e.into_inner());
    let window = chest_window_dim(dim, x, y, z);
    let containers = [Some(window.0), window.1];
    let slots = containers
        .into_iter()
        .flatten()
        .flat_map(|key| {
            chests
                .get(&key)
                .into_iter()
                .flat_map(|slots| slots.values())
        })
        .map(fullness)
        .collect::<Vec<_>>();
    Some(container_analog_output_signal(
        &slots,
        size * if window.1.is_some() { 2 } else { 1 },
    ))
}
fn json_craft_match(g: &HashMap<i16, Stack>, dim: i16) -> Option<Stack> {
    // vanilla translation: the pattern's bounding box may sit at any
    // offset inside the grid; cells outside it must be air
    let at = |x: i16, y: i16| g.get(&(y * dim + x + 1)).map(|e| e.0).unwrap_or(0);
    for r in json_recipes().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if !r.shaped {
            // shapeless: multiset of ingredients == multiset of grid items
            let mut items: Vec<i64> = (1..=(dim * dim))
                .filter_map(|i| {
                    let v = g.get(&i).map(|e| e.0).unwrap_or(0);
                    if v > 0 && g.get(&i).map(|e| e.1).unwrap_or(0) > 0 {
                        Some(v)
                    } else {
                        None
                    }
                })
                .collect();
            if items.len() != r.ing.len() {
                continue;
            }
            let mut ok = true;
            for ig in &r.ing {
                if let Some(p) = items.iter().position(|i| ingr_match(ig, *i, 0)) {
                    items.remove(p);
                } else {
                    ok = false;
                    break;
                }
            }
            if ok {
                return Some((r.out, r.out_cnt, Vec::new()));
            }
            continue;
        }
        // bounding box of non-space cells
        let (mut x0, mut y0, mut x1, mut y1) = (99i16, 99i16, -1i16, -1i16);
        for (ry, row) in r.rows.iter().enumerate() {
            for (rx, ch) in row.chars().enumerate() {
                if ch != ' ' {
                    x0 = x0.min(rx as i16);
                    x1 = x1.max(rx as i16);
                    y0 = y0.min(ry as i16);
                    y1 = y1.max(ry as i16);
                }
            }
        }
        if x1 < 0 {
            continue;
        }
        let (pw, ph) = (x1 - x0 + 1, y1 - y0 + 1);
        if pw > dim || ph > dim {
            continue;
        }
        for oy in 0..=(dim - ph) {
            for ox in 0..=(dim - pw) {
                let mut ok = true;
                for gy in 0..dim {
                    for gx in 0..dim {
                        let need: Option<char> =
                            if gx >= ox && gx < ox + pw && gy >= oy && gy < oy + ph {
                                r.rows[(y0 + gy - oy) as usize]
                                    .chars()
                                    .nth((x0 + gx - ox) as usize)
                            } else {
                                Some(' ')
                            };
                        let cell = at(gx, gy);
                        match need {
                            Some(' ') | None => {
                                if cell != 0 {
                                    ok = false;
                                    break;
                                }
                            }
                            Some(ch) => match r.key.get(&ch) {
                                Some(ig) => {
                                    if !ingr_match(ig, cell, 0) {
                                        ok = false;
                                        break;
                                    }
                                }
                                None => {
                                    if cell != 0 {
                                        ok = false;
                                        break;
                                    }
                                }
                            },
                        }
                    }
                    if !ok {
                        break;
                    }
                }
                if ok {
                    return Some((r.out, r.out_cnt, Vec::new()));
                }
                // this offset failed: try the next one. The label used to point
                // at the outer loop, so only ox = 0 was ever tried and a pattern
                // that needed a horizontal shift never matched.
                continue;
            }
        }
    }
    None
}
fn craft3_result(g: &HashMap<i16, Stack>) -> Option<Stack> {
    if let Some(r) = json_craft_match(g, 3) {
        return Some(r);
    }
    let at = |i: i16| g.get(&i).map(|e| e.0).unwrap_or(0);
    let is = |i: i16, n: &str| name_to_item(n) == Some(at(i));
    let planks = |i: i16| tag_planks(at(i)); // #minecraft:planks — any wood
    let air = |i: i16| at(i) == 0;
    // chest / furnace: full ring around empty center
    for (mat, res) in [("oak_planks", "chest"), ("cobblestone", "furnace")] {
        let ok = if mat == "oak_planks" {
            [1, 2, 3, 4, 6, 7, 8, 9].iter().all(|&i| planks(i)) && air(5)
        } else {
            [1, 2, 3, 4, 6, 7, 8, 9].iter().all(|&i| is(i, mat)) && air(5)
        };
        if ok {
            return name_to_item(res).map(|r| (r, 1, Vec::new()));
        }
    }
    // torch: coal directly above stick
    for (c, st) in [(1, 4), (2, 5), (3, 6), (4, 7), (5, 8), (6, 9)] {
        if is(c, "coal") && is(st, "stick") && (1..=9).filter(|&i| !air(i)).count() == 2 {
            return name_to_item("torch").map(|r| (r, 4, Vec::new()));
        }
    }
    // bread: 3 wheat in a row
    for (a, b, c) in [(1i16, 2, 3), (4, 5, 6), (7, 8, 9)] {
        if is(a, "wheat")
            && is(b, "wheat")
            && is(c, "wheat")
            && (1..=9).filter(|&i| !air(i)).count() == 3
        {
            return name_to_item("bread").map(|r| (r, 1, Vec::new()));
        }
    }
    // generic tool/armor recipes per material
    for (mat, prefix) in [
        ("oak_planks", "wooden"),
        ("cobblestone", "stone"),
        ("iron_ingot", "iron"),
        ("gold_ingot", "golden"),
        ("diamond", "diamond"),
    ] {
        let stick = |i: i16| is(i, "stick");
        let m = |i: i16| {
            if mat == "oak_planks" {
                planks(i)
            } else {
                is(i, mat)
            }
        };
        // pickaxe: mat mat mat / _ stick _ / _ stick _
        if m(1) && m(2) && m(3) && stick(5) && stick(8) && air(4) && air(6) && air(7) && air(9) {
            return name_to_item(&format!("{}_pickaxe", prefix)).map(|r| (r, 1, Vec::new()));
        }
        // axe: mat mat / mat stick / _ stick (either column)
        if (m(1) && m(2) && m(4) && stick(5) && stick(8) && air(3) && air(6) && air(7) && air(9))
            || (m(2)
                && m(3)
                && m(6)
                && stick(5)
                && stick(8)
                && air(1)
                && air(4)
                && air(7)
                && air(9))
        {
            return name_to_item(&format!("{}_axe", prefix)).map(|r| (r, 1, Vec::new()));
        }
        // shovel: mat / stick / stick, any column
        for col in [0i16, 1, 2] {
            if m(1 + col)
                && stick(4 + col)
                && stick(7 + col)
                && (1..=9).filter(|&i| !air(i)).count() == 3
            {
                return name_to_item(&format!("{}_shovel", prefix)).map(|r| (r, 1, Vec::new()));
            }
        }
        // sword: mat / mat / stick, any column
        for col in [0i16, 1, 2] {
            if m(1 + col)
                && m(4 + col)
                && stick(7 + col)
                && (1..=9).filter(|&i| !air(i)).count() == 3
            {
                return name_to_item(&format!("{}_sword", prefix)).map(|r| (r, 1, Vec::new()));
            }
        }
        // hoe: mat mat / _ stick / _ stick (or mirrored)
        if (m(1) && m(2) && stick(5) && stick(8) && air(3) && air(4) && air(6) && air(7) && air(9))
            || (m(2)
                && m(3)
                && stick(5)
                && stick(8)
                && air(1)
                && air(4)
                && air(6)
                && air(7)
                && air(9))
        {
            return name_to_item(&format!("{}_hoe", prefix)).map(|r| (r, 1, Vec::new()));
        }
        // armor: helmet=5, chest=8, leggings=7, boots=4
        if mat != "cobblestone" {
            if m(1) && m(2) && m(3) && m(4) && m(6) && air(5) && air(7) && air(8) && air(9) {
                return name_to_item(&format!("{}_helmet", prefix)).map(|r| (r, 1, Vec::new()));
            }
            if m(1) && m(3) && m(4) && m(5) && m(6) && m(7) && m(8) && m(9) && air(2) {
                return name_to_item(&format!("{}_chestplate", prefix)).map(|r| (r, 1, Vec::new()));
            }
            if m(1) && m(2) && m(3) && m(4) && m(6) && m(7) && m(9) && air(5) && air(8) {
                return name_to_item(&format!("{}_leggings", prefix)).map(|r| (r, 1, Vec::new()));
            }
            if m(4) && m(6) && m(7) && m(9) && (1..=9).filter(|&i| !air(i)).count() == 4 {
                return name_to_item(&format!("{}_boots", prefix)).map(|r| (r, 1, Vec::new()));
            }
        }
    }
    // leather armor pieces on their own
    {
        let l = |i: i16| is(i, "leather");
        if l(1) && l(2) && l(3) && l(4) && l(6) && air(5) && air(7) && air(8) && air(9) {
            return name_to_item("leather_helmet").map(|r| (r, 1, Vec::new()));
        }
        if l(1) && l(3) && l(4) && l(5) && l(6) && l(7) && l(8) && l(9) && air(2) {
            return name_to_item("leather_chestplate").map(|r| (r, 1, Vec::new()));
        }
        if l(1) && l(2) && l(3) && l(4) && l(6) && l(7) && l(9) && air(5) && air(8) {
            return name_to_item("leather_leggings").map(|r| (r, 1, Vec::new()));
        }
        if l(4) && l(6) && l(7) && l(9) && (1..=9).filter(|&i| !air(i)).count() == 4 {
            return name_to_item("leather_boots").map(|r| (r, 1, Vec::new()));
        }
    }
    // bucket: iron V-shape
    if is(4, "iron_ingot")
        && is(6, "iron_ingot")
        && is(8, "iron_ingot")
        && (1..=9).filter(|&i| !air(i)).count() == 3
    {
        return name_to_item("bucket").map(|r| (r, 1, Vec::new()));
    }
    // shears: 2 iron diagonal-ish
    if ((is(5, "iron_ingot") && is(8, "iron_ingot"))
        || (is(5, "iron_ingot") && is(9, "iron_ingot")))
        && (1..=9).filter(|&i| !air(i)).count() == 2
    {
        return name_to_item("shears").map(|r| (r, 1, Vec::new()));
    }
    // shield: planks across + iron center-top
    if planks(1)
        && is(2, "iron_ingot")
        && planks(3)
        && planks(4)
        && planks(5)
        && planks(6)
        && planks(8)
        && air(7)
        && air(9)
    {
        return name_to_item("shield").map(|r| (r, 1, Vec::new()));
    }
    // fishing rod: 3 sticks diagonal + 2 string
    if is(3, "stick")
        && is(5, "stick")
        && is(7, "stick")
        && is(6, "string")
        && is(9, "string")
        && (1..=9).filter(|&i| !air(i)).count() == 5
    {
        return name_to_item("fishing_rod").map(|r| (r, 1, Vec::new()));
    }
    // bow: sticks + strings
    if is(2, "stick")
        && is(4, "stick")
        && is(8, "stick")
        && is(3, "string")
        && is(6, "string")
        && is(9, "string")
        && air(1)
        && air(5)
        && air(7)
    {
        return name_to_item("bow").map(|r| (r, 1, Vec::new()));
    }
    // also accept 2x2-style recipes in the big grid
    let g2: HashMap<i16, Stack> = g.clone();
    // single log -> its own planks (vanilla #minecraft:logs, type-preserving)
    if (1..=9).filter(|&i| !air(i)).count() == 1 {
        if let Some(it) = (1..=9).find(|&i| !air(i)).map(at) {
            if let Some(p) = planks_for_log(it) {
                return Some((p, 4, Vec::new()));
            }
        }
    }
    if (1..=9).filter(|&i| !air(i)).count() == 4 {
        // any 2x2 block of planks -> crafting_table
        for (a, b, c, d) in [(1i16, 2, 4, 5), (2, 3, 5, 6), (4, 5, 7, 8), (5, 6, 8, 9)] {
            if planks(a) && planks(b) && planks(c) && planks(d) {
                return name_to_item("crafting_table").map(|r| (r, 1, Vec::new()));
            }
        }
    }
    let _ = g2;
    // leather armor + dyes: vanilla shapeless `armor_dye` recipe.
    dyed_craft_any((1..=9).filter_map(|i| g.get(&i)))
}

enum LiveMenu {
    Anvil(AnvilMenu),
    Enchanting(EnchantmentMenu),
    Brewing,
    Crafter,
    Lectern,
    Smithing(SmithingMenu),
    Merchant(i32, MerchantMenu),
    Stonecutter(StonecutterMenu),
    Grindstone(GrindstoneMenu),
    Loom(LoomMenu),
    Cartography(CartographyMenu),
    Beacon(BeaconMenu),
}

impl LiveMenu {
    fn screen(&self) -> (i64, &'static str) {
        match self {
            Self::Anvil(_) => (8, "Repair & Name"),
            // `minecraft:beacon` is index 9 in the protocol-767 menu registry
            // (`proto::data_v767::V767_MENUS`; crafter_3x3 holds 7, anvil 8).
            Self::Beacon(_) => (9, "Beacon"),
            Self::Brewing => (11, "Brewing Stand"),
            Self::Crafter => (7, "Crafter"),
            Self::Lectern => (17, "Lectern"),
            Self::Enchanting(_) => (13, "Enchant"),
            Self::Merchant(..) => (19, "Villager"),
            Self::Smithing(_) => (21, "Upgrade Gear"),
            Self::Stonecutter(_) => StonecutterMenu::screen(),
            Self::Grindstone(_) => GrindstoneMenu::screen(),
            Self::Loom(_) => LoomMenu::screen(),
            Self::Cartography(_) => CartographyMenu::screen(),
        }
    }

    fn input_slots(&mut self) -> Vec<&mut Stack> {
        match self {
            Self::Anvil(menu) => vec![&mut menu.left, &mut menu.right],
            Self::Enchanting(menu) => vec![&mut menu.item, &mut menu.lapis],
            Self::Smithing(menu) => vec![&mut menu.template, &mut menu.base, &mut menu.addition],
            Self::Merchant(_, menu) => menu.payments.iter_mut().collect(),
            Self::Stonecutter(menu) => vec![&mut menu.input],
            Self::Grindstone(menu) => vec![&mut menu.top, &mut menu.bottom],
            Self::Loom(menu) => vec![&mut menu.banner, &mut menu.dye, &mut menu.pattern_item],
            Self::Cartography(menu) => vec![&mut menu.map, &mut menu.addition],
            Self::Beacon(menu) => vec![&mut menu.payment],
            Self::Brewing | Self::Crafter | Self::Lectern => Vec::new(),
        }
    }
}

struct LiveMenuSession {
    id: u8,
    state_id: i32,
    anchor: Option<DimBlockPos>,
    menu: LiveMenu,
}

struct LiveMenuConnection {
    active: Mutex<Option<LiveMenuSession>>,
    next_id: std::sync::atomic::AtomicU8,
    enchantment_seed: Mutex<i32>,
}

fn live_menu_connection(c: &Client) -> Option<Arc<LiveMenuConnection>> {
    // Release the player-state registry before locking a menu. Menu actions
    // can update XP, which briefly reacquires the registry.
    player_mechanism_sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&c.eid)
        .map(|session| Arc::clone(&session.menus))
}

fn live_menu_id(c: &Client) -> Option<u8> {
    live_menu_connection(c)?
        .active
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|menu| menu.id)
}

fn live_villagers() -> &'static LockedMap<i32, Villager> {
    static VILLAGERS: Registry<i32, Villager> = OnceLock::new();
    VILLAGERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn farmer_merchant(mob: &Mob) -> Villager {
    let mut villager = Villager::new(mob.uuid, VillagerProfession::Farmer);
    villager.offers = [
        ("wheat", 20, "emerald", 1, 2),
        ("emerald", 1, "bread", 6, 1),
    ]
    .into_iter()
    .filter_map(|(buy, count, sell, result_count, xp_reward)| {
        Some(MerchantOffer {
            cost_a: ItemCost {
                item: name_to_item(buy)?,
                count,
                components: Vec::new(),
            },
            cost_b: None,
            result: (name_to_item(sell)?, result_count, Vec::new()),
            uses: 0,
            max_uses: 16,
            xp_reward,
            price_multiplier: 0.05,
            demand: 0,
            special_price_diff: 0,
        })
    })
    .collect();
    villager
}

fn merchant_item_prototype(item: i64) -> &'static Comps {
    static PROTOTYPES: OnceLock<[Comps; 100]> = OnceLock::new();
    let prototypes = PROTOTYPES.get_or_init(|| {
        std::array::from_fn(|count| {
            let mut components = Vec::new();
            comp_set_varint(&mut components, MERCHANT_MAX_STACK_SIZE, count as i64);
            components
        })
    });
    &prototypes[item_stack_max(item, &Vec::new()) as usize]
}

fn live_offers_payload(window: u8, merchant: &Villager) -> Vec<u8> {
    let packet = MerchantOffersPacket {
        window_id: i32::from(window),
        offers: merchant.offers.clone(),
        villager_level: merchant.level() as i32,
        villager_xp: merchant.villager_xp,
        show_progress: true,
        can_restock: false,
    };
    encode_merchant_offers_pkt(&packet)
}

fn open_live_merchant(c: &Client, mob: &Mob) {
    if c.is_spectator() || *c.dim.lock().unwrap_or_else(|e| e.into_inner()) != DimId::Overworld {
        return;
    }
    let Some(connection) = live_menu_connection(c) else {
        return;
    };
    if matches!(connection.active.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|s| &s.menu),
                Some(LiveMenu::Merchant(id, _)) if *id == mob.eid)
    {
        return;
    }
    // Bind a nearby workstation before the table opens: claiming it assigns
    // the site profession and its real trade rows. Only a villager with no
    // workstation nearby keeps the historical farmer fallback.
    let dim = *c.dim.lock().unwrap_or_else(|e| e.into_inner());
    {
        let mut villagers = live_villagers().lock().unwrap_or_else(|e| e.into_inner());
        let villager = villagers
            .entry(mob.eid)
            .or_insert_with(|| Villager::new(mob.uuid, VillagerProfession::None));
        if villager.job_site.is_none() || villager.offers.is_empty() {
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            live_villager_bind_pois(
                dim,
                (x.floor() as i32, y.floor() as i32, z.floor() as i32),
                villager,
            );
        }
        if matches!(villager.profession, VillagerProfession::None) {
            *villager = farmer_merchant(mob);
        }
    }
    let opened = {
        let mut villagers = live_villagers().lock().unwrap_or_else(|e| e.into_inner());
        let registries = live_pois().lock().unwrap_or_else(|e| e.into_inner());
        let empty = PoiRegistry::default();
        let pois = registries.get(&dim).unwrap_or(&empty);
        villagers
            .get_mut(&mob.eid)
            .is_some_and(|villager| villager.start_trading(pois, c.uuid, None))
    };
    if opened {
        open_live_menu(
            c,
            None,
            LiveMenu::Merchant(mob.eid, MerchantMenu::default()),
        );
    }
}

fn open_live_block_menu(c: &Client, key @ (dim, x, y, z): DimBlockPos, state: u32) -> bool {
    let Some(reg) = breg() else { return false };
    let menu = match reg.name_of(state) {
        "minecraft:anvil" | "minecraft:chipped_anvil" | "minecraft:damaged_anvil" => {
            LiveMenu::Anvil(AnvilMenu::default())
        }
        "minecraft:enchanting_table" => LiveMenu::Enchanting(EnchantmentMenu::default()),
        "minecraft:brewing_stand" => LiveMenu::Brewing,
        "minecraft:crafter" => LiveMenu::Crafter,
        "minecraft:smithing_table" => LiveMenu::Smithing(SmithingMenu::default()),
        "minecraft:stonecutter" => LiveMenu::Stonecutter(StonecutterMenu::default()),
        "minecraft:grindstone" => LiveMenu::Grindstone(GrindstoneMenu::default()),
        "minecraft:loom" => LiveMenu::Loom(LoomMenu::default()),
        "minecraft:cartography_table" => LiveMenu::Cartography(CartographyMenu::default()),
        "minecraft:beacon" => LiveMenu::Beacon(BeaconMenu {
            anchor: Some(key),
            ..Default::default()
        }),
        _ => return use_live_block_entity(c, key, state),
    };
    if !c.is_spectator() {
        // Recheck under the block transaction lock before creating the stand entity.
        with_save_guard(|| {
            if block_at_dim(dim, x, y, z) != state {
                return;
            }
            if matches!(menu, LiveMenu::Brewing) {
                live_brewing_stands()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(key)
                    .or_default();
            }
            if matches!(menu, LiveMenu::Crafter) {
                live_block_entities().lock().unwrap_or_else(|e| e.into_inner()).at(reg, key, state);
            }
            open_live_menu(c, Some(key), menu);
        });
    }
    true
}

fn open_live_menu(c: &Client, anchor: Option<DimBlockPos>, menu: LiveMenu) {
    if let Some(key) = anchor {
        if !container_lock_gate(c, key) {
            return;
        }
    }
    let Some(connection) = live_menu_connection(c) else {
        return;
    };
    close_live_menu(c, false, false);
    if let Some((window, old_anchor)) = client_open_window_and_anchor(c) {
        let audience = client_snapshot();
        let client_dim = *c.dim.lock().unwrap_or_else(|e| e.into_inner());
        let halves = client_window_halves_dim(window, old_anchor, client_dim);
        let before = chest_opener_snapshot_dim(&audience, &halves);
        *c.open_chest.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *c.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
        sync_chest_openers_dim(&audience, &before);
    }
    *c.open_furnace.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if std::mem::take(&mut *c.open_table.lock().unwrap_or_else(|e| e.into_inner())) {
        let grid = std::mem::take(&mut *c.craft9.lock().unwrap_or_else(|e| e.into_inner()));
        return_live_inputs(c, grid.into_values());
    }

    // The older windows reserve IDs 1–3. A new ID lets clicks from a replaced
    // menu be rejected even if the next menu has the same layout.
    let id = loop {
        let current = connection.next_id.load(Ordering::Relaxed);
        let next = if current == 100 { 4 } else { current + 1 };
        if connection
            .next_id
            .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break current;
        }
    };
    let mut active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
    let session = active.insert(LiveMenuSession {
        id,
        state_id: 0,
        anchor,
        menu,
    });
    refresh_live_enchanting(&connection, session);
    let (kind, title) = session.menu.screen();
    let mut packet = Vec::new();
    write_varint(&mut packet, i64::from(id));
    write_varint(&mut packet, kind);
    packet.extend_from_slice(&chat_nbt(title));
    let _ = send_packet(&mut c.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x33, packet);
    sync_live_menu(c, session, true);
}

fn send_live_contents(
    sock: &mut TcpStream,
    id: u8,
    state_id: i32,
    slots: &[Stack],
    inventory: &HashMap<i16, Stack>,
    player_slots: std::ops::Range<i16>,
    cursor: &Stack,
) {
    let mut packet = vec![id];
    write_varint(&mut packet, i64::from(state_id));
    write_varint(&mut packet, (slots.len() + player_slots.len()) as i64);
    for stack in slots {
        packet.extend_from_slice(&slot_wire(stack));
    }
    let empty = Stack::default();
    for slot in player_slots {
        packet.extend_from_slice(&slot_wire(inventory.get(&slot).unwrap_or(&empty)));
    }
    packet.extend_from_slice(&slot_wire(cursor));
    let _ = send_packet(sock, 0x13, packet);
}

fn anvil_hover_name(stack: &Stack) -> String {
    // `custom_name` (the anvil rename) wins, then `item_name` (the fixed
    // component name), then the registry name — vanilla's `getHoverName`.
    if let Some(text) = stack_item_text(stack, COMP_CUSTOM_NAME) {
        return text;
    }
    if let Some(text) = stack_item_text(stack, COMP_ITEM_NAME) {
        return text;
    }
    item_to_name(stack.0).unwrap_or_default().to_string()
}

// `minecraft:banner_patterns` on the 1.21.1 component wire. The
// `minecraft:data_component_type` tail is pinned by the codebase's anchors
// (trim=35); the registry then runs debug_stick_state ..= note_block_sound
// before banner_patterns at 49.
const COMP_BANNER_PATTERNS: i64 = 49;

// `minecraft:banner_pattern` registry order from the captured 1.21.1
// Configuration packet (config_packets.bin): `base` at 0, then alphabetical.
const BANNER_PATTERN_REGISTRY: &[&str] = &[
    "base",
    "border",
    "bricks",
    "circle",
    "creeper",
    "cross",
    "curly_border",
    "diagonal_left",
    "diagonal_right",
    "diagonal_up_left",
    "diagonal_up_right",
    "flow",
    "flower",
    "globe",
    "gradient",
    "gradient_up",
    "guster",
    "half_horizontal",
    "half_horizontal_bottom",
    "half_vertical",
    "half_vertical_right",
    "mojang",
    "piglin",
    "rhombus",
    "skull",
    "small_stripes",
    "square_bottom_left",
    "square_bottom_right",
    "square_top_left",
    "square_top_right",
    "straight_cross",
    "stripe_bottom",
    "stripe_center",
    "stripe_downleft",
    "stripe_downright",
    "stripe_left",
    "stripe_middle",
    "stripe_right",
    "stripe_top",
    "triangle_bottom",
    "triangle_top",
    "triangles_bottom",
    "triangles_top",
];

// Read the banner's banner_patterns component into menu layers. Inline
// (non-registry) pattern definitions keep their asset id as the pattern name,
// which re-encodes inline.
fn live_banner_layers(stack: &Stack) -> Vec<BannerLayer> {
    let mut layers = Vec::new();
    let Some(mut data) = comp_get(&stack.2, COMP_BANNER_PATTERNS) else {
        return layers;
    };
    let Some(count) = read_protocol_i32(&mut data) else {
        return layers;
    };
    for _ in 0..count.clamp(0, 256) {
        let Some(holder) = read_protocol_i32(&mut data) else {
            break;
        };
        let pattern = if holder <= 0 {
            let Some(asset) = registry_wire_name(&mut data) else {
                break;
            };
            let Some(_) = registry_wire_name(&mut data) else {
                break;
            };
            asset.to_string()
        } else {
            let Some(&name) = BANNER_PATTERN_REGISTRY.get((holder - 1) as usize) else {
                break;
            };
            name.to_string()
        };
        let Some(color) = read_protocol_i32(&mut data) else {
            break;
        };
        if !(0..=15).contains(&color) {
            break;
        }
        layers.push(BannerLayer {
            pattern,
            color: color as u8,
        });
    }
    layers
}

fn live_banner_patterns_data(layers: &[BannerLayer]) -> Vec<u8> {
    let mut data = Vec::new();
    write_varint(&mut data, layers.len() as i64);
    for layer in layers {
        if let Some(id) = BANNER_PATTERN_REGISTRY
            .iter()
            .position(|name| *name == layer.pattern)
        {
            // Holder streams reserve zero for inline definitions (COMP_TRIM).
            write_varint(&mut data, id as i64 + 1);
        } else {
            write_varint(&mut data, 0);
            write_str(&mut data, &layer.pattern);
            write_str(&mut data, "");
        }
        write_varint(&mut data, i64::from(layer.color));
    }
    data
}

// `LoomMenu.slotsChanged`/`updateSelectablePatterns` (1.21.1): the layer list
// follows the banner's banner_patterns component, and the selected pattern
// index only resets when the offered list changes - removing banner or dye
// empties the result but keeps the selection.
fn live_loom_refresh(menu: &mut LoomMenu) {
    menu.banner_layers = live_banner_layers(&menu.banner);
    let patterns = menu.selectable_patterns();
    let keep = menu
        .selected_pattern
        .as_ref()
        .is_some_and(|selected| patterns.iter().any(|entry| entry == selected));
    if !keep {
        menu.selected_pattern = None;
    }
}

// The client previews the result with the appended layer, so the wire stack
// carries the full (existing + selected) layer list.
fn live_loom_result(menu: &LoomMenu) -> Stack {
    let mut result = menu.result();
    if result.1 == 0 {
        return result;
    }
    let mut layers = menu.banner_layers.clone();
    if let (Some(pattern), Some(color)) = (
        menu.selected_pattern.clone(),
        wm_dye_color_index(&menu.dye),
    ) {
        layers.push(BannerLayer { pattern, color });
    }
    comp_set(
        &mut result.2,
        COMP_BANNER_PATTERNS,
        live_banner_patterns_data(&layers),
    );
    result
}

// Server-side map data (vanilla MapItemSavedData): the filled_map stack carries
// only its map_id component; scale and the locked flag live here keyed by it.
fn live_map_states() -> &'static LockedMap<i64, MapState> {
    static STATES: Registry<i64, MapState> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn live_next_map_id() -> i64 {
    static NEXT_MAP_ID: AtomicU64 = AtomicU64::new(0);
    NEXT_MAP_ID.fetch_add(1, Ordering::Relaxed) as i64
}

fn live_map_state(stack: &Stack) -> Option<MapState> {
    if stack.1 != 1 || item_to_name(stack.0) != Some("filled_map") {
        return None;
    }
    let map_id = comp_varint(&stack.2, COMP_MAP_ID)?;
    // `map_post_processing`: 0 marks the map locked (vanilla `MapItem` clone
    // semantics); 1 marks a scaled (zoomed-out) map.
    let locked = comp_varint(&stack.2, COMP_MAP_POST_PROCESSING) == Some(0);
    Some(
        live_map_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&map_id)
            .copied()
            .map(|state| MapState {
                locked: locked || state.locked,
                ..state
            })
            .unwrap_or(MapState {
                map_id,
                scale: 0,
                locked,
            }),
    )
}

fn sync_live_menu(c: &Client, session: &mut LiveMenuSession, send_offers: bool) {
    let mut properties = Vec::new();
    let mut offers = None;
    let slots = match &mut session.menu {
        LiveMenu::Anvil(menu) => {
            let result = menu.result(&anvil_hover_name(&menu.left), *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1);
            properties.extend(anvil_window_properties(
                result.as_ref().map_or(0, |r| r.level_cost),
            ));
            vec![
                menu.left.clone(),
                menu.right.clone(),
                result.map_or_else(|_| Stack::default(), |r| r.stack),
            ]
        }
        LiveMenu::Enchanting(menu) => {
            properties.extend(enchanting_window_properties(menu));
            vec![menu.item.clone(), menu.lapis.clone()]
        }
        LiveMenu::Brewing => {
            let stands = live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner());
            let Some(entry) = stands.get(&session.anchor.unwrap()) else {
                return;
            };
            properties.extend(brewing_window_properties(&entry.stand));
            entry.menu.slots.to_vec()
        }
        LiveMenu::Crafter => {
            let store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
            let Some((_, LiveBlockEntity::Crafter(crafter, _))) =
                store.entries.get(&session.anchor.unwrap())
            else {
                return;
            };
            properties.extend((0..9).map(|slot| WindowPropertyUpdate {
                property: slot as i16,
                value: i16::from(crafter.disabled[slot]),
            }));
            properties.push(WindowPropertyUpdate {
                property: 9,
                value: i16::from(crafter.triggered),
            });
            let mut slots = crafter
                .slots()
                .iter()
                .map(|item| {
                    item.as_ref()
                        .and_then(BlockEntityItem::to_stack)
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>();
            let result =
                live_crafter_recipe(crafter.slots()).and_then(|(result, _)| result.to_stack());
            slots.push(result.unwrap_or_default());
            slots
        }
        LiveMenu::Lectern => {
            let (dim, x, y, z) = session.anchor.unwrap();
            let (Some(reg), state) = (breg(), block_at_dim(dim, x, y, z)) else {
                return;
            };
            let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
            let Some(LiveBlockEntity::Lectern(lectern, _)) = store.at(reg, (dim, x, y, z), state)
            else {
                return;
            };
            properties.push(WindowPropertyUpdate {
                property: 0,
                value: lectern.page() as i16,
            });
            vec![lectern
                .book
                .as_ref()
                .and_then(BlockEntityItem::to_stack)
                .unwrap_or_default()]
        }
        LiveMenu::Smithing(menu) => {
            vec![
                menu.template.clone(),
                menu.base.clone(),
                menu.addition.clone(),
                live_smithing_result(&menu.template, &menu.base, &menu.addition)
                    .unwrap_or_default(),
            ]
        }
        LiveMenu::Merchant(id, menu) => {
            let merchants = live_villagers().lock().unwrap_or_else(|e| e.into_inner());
            let Some(villager) = merchants.get(id) else {
                return;
            };
            let output = menu
                .active_offer(&villager.offers, &MerchantItems(&merchant_item_comps))
                .map(|i| villager.offers[i].result.clone())
                .unwrap_or_default();
            if send_offers {
                offers = Some(live_offers_payload(session.id, villager));
            }
            vec![menu.payments[0].clone(), menu.payments[1].clone(), output]
        }
        LiveMenu::Stonecutter(menu) => {
            properties.push(WindowPropertyUpdate {
                property: 0,
                value: menu.selected_recipe.map_or(-1, |index| index as i16),
            });
            vec![menu.input.clone(), live_stonecutter_result(menu)]
        }
        LiveMenu::Grindstone(menu) => vec![
            menu.top.clone(),
            menu.bottom.clone(),
            menu.compute()
                .map(|(result, _)| result)
                .unwrap_or_default(),
        ],
        LiveMenu::Loom(menu) => {
            live_loom_refresh(menu);
            properties.push(WindowPropertyUpdate {
                property: 0,
                value: menu
                    .selected_pattern
                    .as_deref()
                    .and_then(|selected| {
                        menu.selectable_patterns()
                            .iter()
                            .position(|entry| entry == selected)
                    })
                    .map_or(-1, |index| index as i16),
            });
            vec![
                menu.banner.clone(),
                menu.dye.clone(),
                menu.pattern_item.clone(),
                live_loom_result(menu),
            ]
        }
        LiveMenu::Cartography(menu) => {
            menu.map_state = live_map_state(&menu.map);
            let result = menu
                .preview()
                .map(|(mut result, state)| {
                    // Clone/lock keep the source map_id; a zoom's fresh id is
                    // allocated at take time by the caller-owned counter.
                    comp_set_varint(&mut result.2, COMP_MAP_ID, state.map_id);
                    result
                })
                .unwrap_or_default();
            vec![menu.map.clone(), menu.addition.clone(), result]
        }
        LiveMenu::Beacon(menu) => {
            // The beacon block entity is the source of truth for all three
            // `ContainerData` rows (vanilla BeaconBlockEntity.dataAccess).
            let (dim, x, y, z) = session.anchor.unwrap();
            let (Some(reg), state) = (breg(), block_at_dim(dim, x, y, z)) else {
                return;
            };
            let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
            let Some(LiveBlockEntity::Beacon(beacon)) =
                store.at(reg, (dim, x, y, z), state)
            else {
                return;
            };
            menu.power_level = beacon.levels() as i16;
            properties.extend([
                WindowPropertyUpdate {
                    property: 0,
                    value: menu.power_level,
                },
                WindowPropertyUpdate {
                    property: 1,
                    value: beacon.primary.map_or(-1, beacon_wire_effect),
                },
                WindowPropertyUpdate {
                    property: 2,
                    value: beacon.secondary.map_or(-1, beacon_wire_effect),
                },
            ]);
            vec![menu.payment.clone()]
        }
    };
    session.state_id = (session.state_id + 1) & 0x7fff;
    let inventory = c.inv.lock().unwrap_or_else(|e| e.into_inner());
    let cursor = c.cursor.lock().unwrap_or_else(|e| e.into_inner());
    let mut sock = c.sock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(payload) = offers {
        let _ = send_packet(&mut sock, 0x2d, payload);
    }
    send_live_contents(
        &mut sock,
        session.id,
        session.state_id,
        &slots,
        &inventory,
        if matches!(session.menu, LiveMenu::Lectern) {
            0..0
        } else {
            9..45
        },
        &cursor,
    );
    for property in properties {
        let _ = send_packet(&mut sock, 0x14, property.encode(session.id).to_vec());
    }
}

fn drop_live_stack((x, y, z): (f64, f64, f64), stack: Stack) {
    if stack.1 == 0 {
        return;
    }
    drop_spawn(
        x,
        y,
        z,
        stack.0,
        stack.1,
        stack.2,
        -(server_ticks(30) as i32),
        None,
        None,
    );
}

fn return_live_inputs(c: &Client, stacks: impl IntoIterator<Item = Stack>) {
    let (x, y, z, ..) = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
    let selected = *c.held.lock().unwrap_or_else(|e| e.into_inner());
    let mut inventory = c.inv.lock().unwrap_or_else(|e| e.into_inner());
    for mut stack in stacks {
        pickup_into_inventory(&mut inventory, selected, false, &mut stack);
        drop_live_stack((x, y + 1.3, z), stack);
    }
}

fn close_live_menu(c: &Client, send_close: bool, sync: bool) {
    close_live_menu_id(c, None, send_close, sync);
}

fn close_live_menu_id(c: &Client, id: Option<u8>, send_close: bool, sync: bool) {
    let Some(connection) = live_menu_connection(c) else {
        return;
    };
    let mut active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
    if id.is_some_and(|id| active.as_ref().is_none_or(|s| s.id != id)) {
        return;
    }
    let Some(mut session) = active.take() else {
        return;
    };
    if let LiveMenu::Merchant(id, _) = session.menu {
        if let Some(villager) = live_villagers().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
            if villager.trading_player == Some(c.uuid) {
                villager.stop_trading();
            }
        }
    }
    let cursor = std::mem::take(&mut *c.cursor.lock().unwrap_or_else(|e| e.into_inner()));
    return_live_inputs(
        c,
        session
            .menu
            .input_slots()
            .into_iter()
            .map(std::mem::take)
            .chain([cursor]),
    );
    if sync {
        let inventory = c.inv.lock().unwrap_or_else(|e| e.into_inner());
        let mut sock = c.sock.lock().unwrap_or_else(|e| e.into_inner());
        if send_close {
            let _ = send_packet(&mut sock, 0x12, vec![session.id]);
        }
        send_live_contents(&mut sock, 0, inv_state_next(c), &[], &inventory, 0..46, &Stack::default());
    }
}

fn refresh_live_enchanting(connection: &LiveMenuConnection, session: &mut LiveMenuSession) {
    if let (LiveMenu::Enchanting(menu), Some((dim, x, y, z)), Some(reg)) =
        (&mut session.menu, session.anchor, breg())
    {
        let shelves = count_enchanting_bookshelves(
            (x, y, z),
            |x, y, z| reg.name_of(block_at_dim(dim, x, y, z)) == "minecraft:bookshelf",
            |x, y, z| {
                reg.in_tag(
                    block_at_dim(dim, x, y, z),
                    "minecraft:enchantment_power_transmitter",
                )
            },
        );
        menu.refresh_offers(*connection.enchantment_seed.lock().unwrap_or_else(|e| e.into_inner()), shelves);
    }
}

fn live_anvil_rename(c: &Client, text: &str) {
    let Some(connection) = live_menu_connection(c) else {
        return;
    };
    let mut active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(session) = active.as_mut() {
        if let LiveMenu::Anvil(menu) = &mut session.menu {
            if menu.rename(text) {
                sync_live_menu(c, session, false);
            }
        }
    }
}

fn live_enchant_button(c: &Client, window: i32, button: i32) {
    let Some(connection) = live_menu_connection(c) else {
        return;
    };
    let mut active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
    let Some(session) = active.as_mut().filter(|s| i32::from(s.id) == window) else {
        return;
    };
    if matches!(session.menu, LiveMenu::Lectern) {
        if live_menu_valid(c, session) {
            live_lectern_button(c, session.anchor.unwrap(), button);
            sync_live_menu(c, session, false);
        }
        return;
    }
    if matches!(session.menu, LiveMenu::Stonecutter(_) | LiveMenu::Loom(_)) {
        if button >= 0 && live_menu_valid(c, session) {
            match &mut session.menu {
                // Vanilla `clickMenuButton`: the button is an index into the
                // available recipes (stonecutter) or selectable patterns (loom).
                LiveMenu::Stonecutter(menu) => {
                    live_stonecutter_select(menu, button as usize);
                }
                LiveMenu::Loom(menu) => {
                    if let Some(pattern) =
                        menu.selectable_patterns().get(button as usize).cloned()
                    {
                        menu.select_pattern(&pattern);
                    }
                }
                _ => {}
            }
            sync_live_menu(c, session, false);
        }
        return;
    }
    if let LiveMenu::Enchanting(menu) = &mut session.menu {
        let creative = *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1;
        let mut seed = connection.enchantment_seed.lock().unwrap_or_else(|e| e.into_inner());
        let mut rng = generation_math::Lcg::new(i64::from(*seed) as u64);
        let _ = send_player_xp(c, |xp| {
            menu.click_button(button, xp, &mut seed, creative, &mut rng);
            Ok(())
        });
        drop(seed);
        sync_live_menu(c, session, false);
    }
}

fn live_lectern_button(c: &Client, key @ (dim, x, y, z): DimBlockPos, button: i32) {
    let Some(reg) = breg() else { return };
    let state = block_at_dim(dim, x, y, z);
    let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
    let Some(LiveBlockEntity::Lectern(lectern, pulse_end)) = store.at(reg, key, state) else {
        return;
    };
    if button == 3 && matches!(*c.gm.lock().unwrap_or_else(|e| e.into_inner()), 0 | 1) {
        if let Some(book) = lectern.book.as_ref().and_then(BlockEntityItem::to_stack) {
            lectern.take_book();
            *pulse_end = None;
            drop(store);
            return_live_inputs(c, [book]);
            let state_without_book = reg.with_prop(state, "has_book", "false");
            update_live_block_entity_state(
                key,
                state,
                reg.with_prop(state_without_book, "powered", "false"),
            );
        }
        return;
    }
    let page = match button {
        1 => lectern.page() - 1,
        2 => lectern.page() + 1,
        100.. => button - 100,
        _ => return,
    };
    if lectern.book.is_some() && lectern.set_page(page) {
        *pulse_end = Some(live_block_entity_time() + 2);
        drop(store);
        update_live_block_entity_state(key, state, reg.with_prop(state, "powered", "true"));
    }
}

fn live_select_trade(c: &Client, index: i32) {
    let Some(connection) = live_menu_connection(c) else {
        return;
    };
    let mut active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(session) = active.as_mut() {
        if let LiveMenu::Merchant(id, menu) = &mut session.menu {
            if let Some(villager) = live_villagers().lock().unwrap_or_else(|e| e.into_inner()).get(id) {
                menu.select_trade(
                    index,
                    &villager.offers,
                    &mut c.inv.lock().unwrap_or_else(|e| e.into_inner()),
                    &MerchantItems(&merchant_item_comps),
                );
            }
            sync_live_menu(c, session, false);
        }
    }
}

fn live_input_limit(menu: &LiveMenu, slot: usize, stack: &Stack) -> u8 {
    let item = item_to_name(stack.0).unwrap_or_default();
    match menu {
        LiveMenu::Enchanting(_) if slot == 0 => 1,
        LiveMenu::Enchanting(_) if item != "lapis_lazuli" => 0,
        // Datapack smithing recipes widen `mayPlace` past the builtin rules:
        // any loaded recipe's ingredient set accepts its slot's item.
        LiveMenu::Smithing(_) if live_dp_smithing_slot_accepts(slot, stack) => {
            stack_max_of(stack) as u8
        }
        LiveMenu::Smithing(_) if slot == 0 && !item.ends_with("_smithing_template") => 0,
        LiveMenu::Smithing(_)
            if slot == 1 && tool_parts(item).is_none() && armor_parts(item).is_none() =>
        {
            0
        }
        LiveMenu::Smithing(_) if slot == 2 && !TRIM_MATERIAL_ITEMS.contains(&item) => 0,
        // Vanilla `GrindstoneMenu` mayPlace: isDamageableItem ||
        // EnchantmentHelper.hasAnyEnchantments (covers stored enchantments).
        LiveMenu::Grindstone(_)
            if wm_stack_max_damage(stack).is_none() && !wm_stack_enchanted(stack) =>
        {
            0
        }
        LiveMenu::Loom(_) if slot == 0 && wm_banner_name(stack).is_none() => 0,
        LiveMenu::Loom(_) if slot == 1 && wm_dye_color_index(stack).is_none() => 0,
        LiveMenu::Loom(_) if slot == 2 && wm_pattern_item_name(stack).is_none() => 0,
        LiveMenu::Cartography(_) if slot == 0 && item != "filled_map" => 0,
        LiveMenu::Cartography(_)
            if slot == 1 && !matches!(item, "paper" | "map" | "glass_pane") =>
        {
            0
        }
        // `BeaconMenu`'s slot: `#minecraft:beacon_payment_items` and at most
        // one at a time (`Slot.getMaxStackSize() == 1`).
        LiveMenu::Beacon(_) if !beacon_payment_item(stack) => 0,
        LiveMenu::Beacon(_) => 1,
        _ => stack_max_of(stack) as u8,
    }
}

fn click_live_input(target: &mut Stack, cursor: &mut Stack, button: i8, limit: u8) {
    if cursor.1 == 0 || limit == stack_max_of(cursor) as u8 {
        let mut slot = HashMap::from([(0, std::mem::take(target))]);
        click_slot(&mut slot, cursor, 0, button);
        *target = slot.remove(&0).unwrap_or_default();
    } else if limit != 0 && target.1 == 0 {
        *target = (cursor.0, 1, cursor.2.clone());
        consume_menu_stack(cursor, 1);
    } else if limit != 0 && cursor.1 <= limit {
        std::mem::swap(target, cursor);
    }
}

fn live_menu_click(c: &Client, window: u8, slot: i16, button: i8, mode: i64) {
    let Some(connection) = live_menu_connection(c) else {
        return;
    };
    let mut active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
    let Some(session) = active.as_mut().filter(|s| s.id == window) else {
        return;
    };
    if !live_menu_valid(c, session) {
        drop(active);
        close_live_menu(c, true, true);
        return;
    }
    if !matches!(button, 0 | 1) || !matches!(mode, 0 | 1) {
        sync_live_menu(c, session, false);
        return;
    }
    let slots = match session.menu {
        LiveMenu::Anvil(_) | LiveMenu::Merchant(..) => 3,
        LiveMenu::Enchanting(_) => 2,
        LiveMenu::Brewing => 5,
        LiveMenu::Crafter => 10,
        LiveMenu::Lectern => {
            sync_live_menu(c, session, false);
            return;
        }
        LiveMenu::Smithing(_) => 4,
        LiveMenu::Stonecutter(_) => 2,
        LiveMenu::Grindstone(_) | LiveMenu::Cartography(_) => 3,
        LiveMenu::Loom(_) => 4,
        LiveMenu::Beacon(_) => 1,
    };
    let output_slot = match session.menu {
        LiveMenu::Crafter => 9,
        LiveMenu::Anvil(_)
        | LiveMenu::Merchant(..)
        | LiveMenu::Smithing(_)
        | LiveMenu::Stonecutter(_)
        | LiveMenu::Grindstone(_)
        | LiveMenu::Loom(_)
        | LiveMenu::Cartography(_) => slots - 1,
        _ => -1,
    };
    let mut inventory = c.inv.lock().unwrap_or_else(|e| e.into_inner());
    let mut cursor = c.cursor.lock().unwrap_or_else(|e| e.into_inner());
    let mut offer_changed = false;
    let mut wear_anvil = false;
    let mut brewing_changed = false;
    let mut grind_xp = 0i64;
    let mut carto_state = None;

    if slot == output_slot && slot >= 0 {
        let mut quick_move_inventory = None;
        let selected = *c.held.lock().unwrap_or_else(|e| e.into_inner());
        let mut receive = |result: &Stack| {
            if mode == 1 {
                let mut trial = inventory.clone();
                let mut remainder = result.clone();
                pickup_into_inventory(&mut trial, selected, false, &mut remainder);
                if remainder.1 == 0 {
                    quick_move_inventory = Some(trial);
                    return true;
                }
                return false;
            }
            cursor.1 == 0
                || (cursor.0 == result.0
                    && cursor.2 == result.2
                    && i64::from(cursor.1) + i64::from(result.1) <= stack_max_of(result))
        };
        let result = match &mut session.menu {
            LiveMenu::Anvil(menu) => {
                let name = anvil_hover_name(&menu.left);
                let creative = *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1;
                let mut output = None;
                let _ = send_player_xp(c, |xp| {
                    if let Some(result) = menu.take_result(&name, xp, creative, receive, || {
                        // Use 24 random bits so the sample fits in f32's [0, 1).
                        Some((world_rng::next_u64() >> 40) as f32 / 16_777_216.0)
                    }) {
                        wear_anvil = result.damage_anvil;
                        output = Some(result.output);
                    }
                    Ok(())
                });
                output
            }
            LiveMenu::Smithing(menu) => live_smithing_take(menu, receive),
            LiveMenu::Stonecutter(menu) => {
                let result = live_stonecutter_result(menu);
                if result.1 != 0 && receive(&result) {
                    live_stonecutter_take(menu)
                } else {
                    None
                }
            }
            LiveMenu::Grindstone(menu) => menu.compute().and_then(|(result, _)| {
                if !receive(&result) {
                    return None;
                }
                // Deterministic [0, 1) roll of vanilla getExperienceAmount's
                // nextInt(j) draw.
                let roll = (world_rng::next_u64() >> 11) as f64 / 9007199254740992.0;
                menu.take_result(roll).map(|(output, xp)| {
                    grind_xp = xp;
                    output
                })
            }),
            LiveMenu::Loom(menu) => {
                let result = menu.result();
                if result.1 != 0 && receive(&result) {
                    menu.take_result().map(|(mut output, layers)| {
                        comp_set(
                            &mut output.2,
                            COMP_BANNER_PATTERNS,
                            live_banner_patterns_data(&layers),
                        );
                        output
                    })
                } else {
                    None
                }
            }
            LiveMenu::Cartography(menu) => {
                menu.map_state = live_map_state(&menu.map);
                menu.preview().and_then(|(result, _)| {
                    if !receive(&result) {
                        return None;
                    }
                    let new_map_id = if item_to_name(menu.addition.0) == Some("paper") {
                        live_next_map_id()
                    } else {
                        -1
                    };
                    menu.take_result(new_map_id).map(|(output, state)| {
                        carto_state = Some(state);
                        output
                    })
                })
            }
            LiveMenu::Merchant(id, menu) => {
                let mut villagers = live_villagers().lock().unwrap_or_else(|e| e.into_inner());
                let items = MerchantItems(&merchant_item_comps);
                let result = villagers.get_mut(id).and_then(|villager| {
                    let index = menu.active_offer(&villager.offers, &items)?;
                    receive(&villager.offers[index].result).then_some(())?;
                    villager
                        .trade(c.uuid, menu, &items)
                        .map(|trade| trade.result)
                });
                offer_changed = result.is_some();
                result
            }
            _ => None,
        };
        if let Some(mut result) = result {
            if let Some(updated) = quick_move_inventory {
                *inventory = updated;
            } else {
                result.1 += cursor.1;
                *cursor = result;
            }
            // Vanilla `GrindstoneMenu` awards the disenchant refund straight to
            // the player; a cartography zoom/clone/lock registers the map state
            // under the id the result stack now carries.
            if grind_xp > 0 {
                let _ = send_player_xp(c, |xp| add_player_xp_points(xp, grind_xp as i32));
            }
            if let Some(state) = carto_state {
                live_map_states().lock().unwrap_or_else(|e| e.into_inner()).insert(state.map_id, state);
            }
        }
    } else if (0..slots).contains(&slot) {
        if let LiveMenu::Brewing = session.menu {
            let mut stands = live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = stands.get_mut(&session.anchor.unwrap()) {
                brewing_changed = if mode == 0 {
                    entry.menu.click_slot(slot, button, &mut cursor)
                } else {
                    let stack = &mut entry.menu.slots[slot as usize];
                    let before = stack.1;
                    pickup_into_inventory(&mut inventory, *c.held.lock().unwrap_or_else(|e| e.into_inner()), false, stack);
                    before != stack.1
                };
            }
        } else if matches!(session.menu, LiveMenu::Crafter) {
            let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, LiveBlockEntity::Crafter(crafter, _))) =
                store.entries.get_mut(&session.anchor.unwrap())
            {
                let index = slot as usize;
                let original = &crafter.slots()[index];
                if !crafter.disabled[index]
                    && crafter.loot_table.is_none()
                    && (original.is_none()
                        || original
                            .as_ref()
                            .and_then(BlockEntityItem::to_stack)
                            .is_some())
                    && (cursor.1 == 0 || BlockEntityItem::from_stack(&cursor).is_some())
                {
                    let mut target = original
                        .as_ref()
                        .and_then(BlockEntityItem::to_stack)
                        .unwrap_or_default();
                    if mode == 0 {
                        let limit = stack_max_of(&cursor) as u8;
                        click_live_input(&mut target, &mut cursor, button, limit);
                    } else {
                        pickup_into_inventory(
                            &mut inventory,
                            *c.held.lock().unwrap_or_else(|e| e.into_inner()),
                            false,
                            &mut target,
                        );
                    }
                    crafter.set_slot(index, BlockEntityItem::from_stack(&target));
                }
            }
        } else {
            // Vanilla `StonecutterMenu.slotsChanged`: a different input item
            // resets the selected recipe index (count changes do not).
            let stonecut_item = match &session.menu {
                LiveMenu::Stonecutter(menu) => Some(menu.input.0),
                _ => None,
            };
            let limit = live_input_limit(&session.menu, slot as usize, &cursor);
            let mut inputs = session.menu.input_slots();
            let target = &mut inputs[slot as usize];
            if mode == 0 {
                click_live_input(target, &mut cursor, button, limit);
            } else {
                pickup_into_inventory(&mut inventory, *c.held.lock().unwrap_or_else(|e| e.into_inner()), false, target);
            }
            drop(inputs);
            if let (LiveMenu::Stonecutter(menu), Some(before)) =
                (&mut session.menu, stonecut_item)
            {
                if menu.input.0 != before {
                    menu.selected_recipe = None;
                }
            }
        }
    } else if (slots..slots + 36).contains(&slot) {
        let inv_slot = slot - slots + 9;
        let stonecut_item = match &session.menu {
            LiveMenu::Stonecutter(menu) => Some(menu.input.0),
            _ => None,
        };
        if mode == 0 {
            click_slot(&mut inventory, &mut cursor, inv_slot, button);
        } else if let Some(mut source) = inventory.remove(&inv_slot) {
            if let LiveMenu::Brewing = session.menu {
                let mut stands = live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = stands.get_mut(&session.anchor.unwrap()) {
                    // Only merge or use an empty slot during shift-click.
                    for target in [4, 3, 0, 1, 2] {
                        let placed = &entry.menu.slots[target as usize];
                        if source.1 != 0
                            && (placed.1 == 0 || (placed.0 == source.0 && placed.2 == source.2))
                        {
                            brewing_changed |= entry.menu.click_slot(target, 0, &mut source);
                        }
                    }
                }
            } else if matches!(session.menu, LiveMenu::Crafter) {
                let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
                if let Some((_, LiveBlockEntity::Crafter(crafter, _))) =
                    store.entries.get_mut(&session.anchor.unwrap())
                {
                    if let Some(item) = BlockEntityItem::from_stack(&source) {
                        let mut remaining = Some(item);
                        let limit = stack_max_of(&source) as u8;
                        while crafter.insert(&mut remaining, limit).is_some() {}
                        source.1 = remaining.as_ref().map_or(0, BlockEntityItem::count);
                    }
                }
            } else if !matches!(session.menu, LiveMenu::Merchant(..)) {
                // Lapis belongs in the second slot, even when the first is empty.
                let first = usize::from(
                    matches!(session.menu, LiveMenu::Enchanting(_))
                        && item_to_name(source.0) == Some("lapis_lazuli"),
                );
                for index in first..session.menu.input_slots().len() {
                    let limit = live_input_limit(&session.menu, index, &source);
                    let mut inputs = session.menu.input_slots();
                    let target = &mut inputs[index];
                    if source.1 != 0
                        && (target.1 == 0 || (target.0 == source.0 && target.2 == source.2))
                    {
                        click_live_input(target, &mut source, 0, limit);
                    }
                }
            }
            if source.1 != 0 {
                inventory.insert(inv_slot, source);
            }
        }
        if let (LiveMenu::Stonecutter(menu), Some(before)) =
            (&mut session.menu, stonecut_item)
        {
            if menu.input.0 != before {
                menu.selected_recipe = None;
            }
        }
    } else if slot == -999 && mode == 0 && cursor.1 != 0 {
        let (x, y, z, ..) = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
        let count = if button == 0 { cursor.1 } else { 1 };
        drop_live_stack((x, y + 1.3, z), (cursor.0, count, cursor.2.clone()));
        consume_menu_stack(&mut cursor, count);
    }

    drop(cursor);
    drop(inventory);
    if (0..2).contains(&slot) || mode == 1 {
        refresh_live_enchanting(&connection, session);
    }
    sync_live_menu(c, session, offer_changed);
    let anchor = session.anchor;
    drop(active);
    if wear_anvil {
        if let Some((dim, x, y, z)) = anchor {
            wear_live_anvil(dim, x, y, z);
        }
    }
    if brewing_changed {
        if let Some(key) = anchor {
            update_brewing_bottles(key);
            for viewer in client_snapshot() {
                if viewer.eid == c.eid {
                    continue;
                }
                let Some(connection) = live_menu_connection(&viewer) else {
                    continue;
                };
                let mut active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(session) = active
                    .as_mut()
                    .filter(|s| matches!(s.menu, LiveMenu::Brewing) && s.anchor == Some(key))
                {
                    sync_live_menu(&viewer, session, false);
                }
            }
        }
    }
    if offer_changed {
        let (x, y, z, ..) = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
        xp_spawn(x, y + 1.0, z, (3 + world_rng::next_u64() % 4) as i16);
    }
}

fn wear_live_anvil(dim: DimId, x: i32, y: i32, z: i32) {
    let Some(reg) = breg() else { return };
    let state = block_at_dim(dim, x, y, z);
    let next = match reg.name_of(state) {
        "minecraft:anvil" => "minecraft:chipped_anvil",
        "minecraft:chipped_anvil" => "minecraft:damaged_anvil",
        "minecraft:damaged_anvil" => "minecraft:air",
        _ => return,
    };
    let mut updated = reg.state_of(next, None);
    if let Some(facing) = reg.get_prop(state, "facing") {
        updated = reg.with_prop(updated, "facing", &facing);
    }
    if set_block_if_dim(dim, x, y, z, updated, Some(state)) {
        note_block_update_dim(dim, x, y, z, updated);
    }
}

fn live_menu_valid(c: &Client, session: &LiveMenuSession) -> bool {
    let dim = *c.dim.lock().unwrap_or_else(|e| e.into_inner());
    let (px, py, pz, ..) = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
    let close_to =
        |x: f64, y: f64, z: f64| (px - x).powi(2) + (py - y).powi(2) + (pz - z).powi(2) <= 64.0;
    if let LiveMenu::Merchant(eid, _) = session.menu {
        return dim == DimId::Overworld
            && mobs().lock().unwrap_or_else(|e| e.into_inner()).iter().any(|mob| {
                if mob.eid != eid {
                    return false;
                }
                let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
                close_to(x, y, z)
            });
    }
    let Some((anchor_dim, x, y, z)) = session.anchor else {
        return false;
    };
    if dim != anchor_dim || !close_to(f64::from(x) + 0.5, f64::from(y) + 0.5, f64::from(z) + 0.5) {
        return false;
    }
    let Some(reg) = breg() else { return false };
    let state = block_at_dim(dim, x, y, z);
    if matches!(session.menu, LiveMenu::Lectern) {
        return reg.name_of(state) == "minecraft:lectern"
            && reg.get_prop(state, "has_book").as_deref() == Some("true");
    }
    matches!(
        (&session.menu, reg.name_of(state)),
        (
            LiveMenu::Anvil(_),
            "minecraft:anvil" | "minecraft:chipped_anvil" | "minecraft:damaged_anvil"
        ) | (LiveMenu::Enchanting(_), "minecraft:enchanting_table")
            | (LiveMenu::Brewing, "minecraft:brewing_stand")
            | (LiveMenu::Crafter, "minecraft:crafter")
            | (LiveMenu::Smithing(_), "minecraft:smithing_table")
            | (LiveMenu::Stonecutter(_), "minecraft:stonecutter")
            | (LiveMenu::Grindstone(_), "minecraft:grindstone")
            | (LiveMenu::Loom(_), "minecraft:loom")
            | (LiveMenu::Cartography(_), "minecraft:cartography_table")
            | (LiveMenu::Beacon(_), "minecraft:beacon")
    )
}

fn live_menu_tick() {
    let players = client_snapshot();
    tick_live_block_entities(&players);
    for c in players {
        let Some(connection) = live_menu_connection(&c) else {
            continue;
        };
        let active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
        if active.as_ref().is_some_and(|s| !live_menu_valid(&c, s)) {
            // Hold the session through validation. The close then checks the
            // same ID, since the connection can open a new menu between them.
            let id = active.as_ref().unwrap().id;
            drop(active);
            close_live_menu_id(&c, Some(id), true, true);
        }
    }
    if TICK_IDX
        .load(Ordering::Relaxed)
        .is_multiple_of(server_ticks(20))
        && !live_villagers().lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    {
        let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
        let alive: HashSet<_> = mobs.iter().map(|mob| mob.eid).collect();
        live_villagers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|id, _| alive.contains(id));
    }
}

const COMP_POTION_CONTENTS: i64 = 31;

// The built-in potion registry follows Potions.<clinit> in the 1.21.1 server.
const LIVE_POTIONS: &[(&str, PotionKind)] = &[
    ("water", PotionKind::Water),
    ("mundane", PotionKind::Mundane),
    ("thick", PotionKind::Thick),
    ("awkward", PotionKind::Awkward),
    ("night_vision", PotionKind::NightVision),
    ("long_night_vision", PotionKind::LongNightVision),
    ("invisibility", PotionKind::Invisibility),
    ("long_invisibility", PotionKind::LongInvisibility),
    ("leaping", PotionKind::Leaping),
    ("long_leaping", PotionKind::LongLeaping),
    ("strong_leaping", PotionKind::StrongLeaping),
    ("fire_resistance", PotionKind::FireResistance),
    ("long_fire_resistance", PotionKind::LongFireResistance),
    ("swiftness", PotionKind::Swiftness),
    ("long_swiftness", PotionKind::LongSwiftness),
    ("strong_swiftness", PotionKind::StrongSwiftness),
    ("slowness", PotionKind::Slowness),
    ("long_slowness", PotionKind::LongSlowness),
    ("strong_slowness", PotionKind::StrongSlowness),
    ("turtle_master", PotionKind::TurtleMaster),
    ("long_turtle_master", PotionKind::LongTurtleMaster),
    ("strong_turtle_master", PotionKind::StrongTurtleMaster),
    ("water_breathing", PotionKind::WaterBreathing),
    ("long_water_breathing", PotionKind::LongWaterBreathing),
    ("healing", PotionKind::Healing),
    ("strong_healing", PotionKind::StrongHealing),
    ("harming", PotionKind::Harming),
    ("strong_harming", PotionKind::StrongHarming),
    ("poison", PotionKind::Poison),
    ("long_poison", PotionKind::LongPoison),
    ("strong_poison", PotionKind::StrongPoison),
    ("regeneration", PotionKind::Regeneration),
    ("long_regeneration", PotionKind::LongRegeneration),
    ("strong_regeneration", PotionKind::StrongRegeneration),
    ("strength", PotionKind::Strength),
    ("long_strength", PotionKind::LongStrength),
    ("strong_strength", PotionKind::StrongStrength),
    ("weakness", PotionKind::Weakness),
    ("long_weakness", PotionKind::LongWeakness),
    ("luck", PotionKind::Luck),
    ("slow_falling", PotionKind::SlowFalling),
    ("long_slow_falling", PotionKind::LongSlowFalling),
    ("wind_charged", PotionKind::WindCharged),
    ("weaving", PotionKind::Weaving),
    ("oozing", PotionKind::Oozing),
    ("infested", PotionKind::Infested),
];

fn live_potion_stack(stack: &Stack) -> Option<BrewingPotion> {
    let form = match item_to_name(stack.0)? {
        "potion" => PotionForm::Drinkable,
        "splash_potion" => PotionForm::Splash,
        "lingering_potion" => PotionForm::Lingering,
        _ => return None,
    };
    let mut contents = comp_get(&stack.2, COMP_POTION_CONTENTS)?.strip_prefix(&[1])?;
    let id = usize::try_from(read_protocol_i32(&mut contents)?).ok()?;
    Some(BrewingPotion {
        form,
        kind: LIVE_POTIONS.get(id)?.1,
    })
}

fn read_live_potion_effect_details(data: &mut &[u8], depth: usize) -> Option<()> {
    if depth == 16 {
        return None;
    }
    read_protocol_i32(data)?; // amplifier
    read_protocol_i32(data)?; // duration
    for _ in 0..3 {
        read_merchant_bool(data)?; // ambient, visible, icon
    }
    if read_merchant_bool(data)? {
        read_live_potion_effect_details(data, depth + 1)?;
    }
    Some(())
}

fn update_live_potion(stack: &mut Stack, potion: BrewingPotion) {
    let name = match potion.form {
        PotionForm::Drinkable => "potion",
        PotionForm::Splash => "splash_potion",
        PotionForm::Lingering => "lingering_potion",
    };
    let id = LIVE_POTIONS
        .iter()
        .position(|&(_, kind)| kind == potion.kind)
        .unwrap();
    let mut previous = comp_get(&stack.2, COMP_POTION_CONTENTS).unwrap_or(&[1, 0, 0, 0]);
    previous = &previous[1..];
    read_varint(&mut previous);
    let mut contents = vec![1];
    write_varint(&mut contents, id as i64);
    contents.extend_from_slice(previous);
    stack.0 = name_to_item(name).unwrap();
    comp_set(&mut stack.2, COMP_POTION_CONTENTS, contents);
}

#[derive(Default)]
struct LiveBrewingStand {
    stand: BrewingStand,
    menu: BrewingMenu,
}

impl LiveBrewingStand {
    fn tick(&mut self) -> (bool, bool, bool) {
        let [first, second, third, ingredient, powder] = &mut self.menu.slots;
        self.stand.bottles = [first, second, third].map(|stack| live_potion_stack(stack));
        self.stand.ingredient = item_to_name(ingredient.0)
            .and_then(BrewingIngredient::from_item)
            .map(|item| (item, ingredient.1));
        self.stand.blaze_powder = powder.1;
        let before = (self.stand.brew_time, self.stand.fuel, powder.1);
        let tick = self.stand.tick();
        consume_menu_stack(powder, powder.1 - self.stand.blaze_powder);
        let mut slots_changed = before.2 != powder.1;
        let mut eject = false;
        if let BrewingTick::Completed {
            ejected_glass_bottle,
        } = tick
        {
            for (stack, potion) in self.menu.slots[..3].iter_mut().zip(self.stand.bottles) {
                if let Some(potion) = potion.filter(|p| Some(*p) != live_potion_stack(stack)) {
                    update_live_potion(stack, potion);
                }
            }
            let ingredient = &mut self.menu.slots[3];
            consume_menu_stack(ingredient, 1);
            if self
                .stand
                .ingredient
                .is_some_and(|(item, _)| item == BrewingIngredient::GlassBottle)
            {
                *ingredient = (name_to_item("glass_bottle").unwrap(), 1, Vec::new());
            }
            slots_changed = true;
            eject = ejected_glass_bottle;
        }
        let properties_changed = (before.0, before.1) != (self.stand.brew_time, self.stand.fuel);
        (slots_changed, properties_changed, eject)
    }
}

fn live_brewing_stands() -> &'static LockedMap<DimBlockPos, LiveBrewingStand> {
    static STANDS: Registry<DimBlockPos, LiveBrewingStand> = OnceLock::new();
    STANDS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn update_brewing_bottles(key @ (dim, x, y, z): DimBlockPos) {
    let Some(reg) = breg() else { return };
    let bottles = live_brewing_stands()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|entry| std::array::from_fn::<_, 3, _>(|i| entry.menu.slots[i].1 != 0));
    let Some(bottles) = bottles else { return };
    let state = block_at_dim(dim, x, y, z);
    if reg.name_of(state) != "minecraft:brewing_stand" {
        return;
    }
    let mut updated = state;
    for (prop, occupied) in ["has_bottle_0", "has_bottle_1", "has_bottle_2"]
        .into_iter()
        .zip(bottles)
    {
        updated = reg.with_prop(updated, prop, if occupied { "true" } else { "false" });
    }
    if updated != state && set_block_if_dim(dim, x, y, z, updated, Some(state)) {
        note_block_update_dim(dim, x, y, z, updated);
    } else if nearby_redstone(reg, dim, RsPos(x, y, z)) {
        with_redstone_origins(|| run_redstone_pass(dim, RsPos(x, y, z), None));
    }
}

fn remove_live_brewing_stand(key @ (dim, x, y, z): DimBlockPos) {
    brewing_sleep()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    if let Some(entry) = live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner()).remove(&key) {
        // Item entities currently live in the Overworld simulation.
        if dim == DimId::Overworld {
            for stack in entry.menu.slots {
                drop_live_stack(
                    (f64::from(x) + 0.5, f64::from(y) + 0.5, f64::from(z) + 0.5),
                    stack,
                );
            }
        }
    }
}

fn live_brewing_tick() {
    with_save_guard(|| {
        let mut updates = BREWING_UPDATES_SCRATCH.with(scratch_map_take);
        {
            let mut stands = live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner());
            let mut sleep = brewing_sleep().lock().unwrap_or_else(|e| e.into_inner());
            for (&key, entry) in stands.iter_mut() {
                // Idle-sleep probe (the BOT-HOPPER cross-apply): an inert stand
                // is a pure function of its slots, persisted timers and the
                // content revision, so an unchanged fingerprint skips the
                // per-pass potion/ingredient re-derivation entirely. A pass is
                // only recorded while `brewing_inert` proves the next one a
                // no-op — `brew_time == 0` alone does not, because a stand
                // that just completed or cancelled can start again.
                if entry.stand.brew_time == 0 {
                    let fp = brewing_epoch(entry, content_revision());
                    if sleep.get(&key) == Some(&fp) {
                        BREWING_SLEEP_SKIPS.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    BREWING_ACTIVE_STEPS.fetch_add(1, Ordering::Relaxed);
                    let (mut slots_changed, mut properties_changed, mut eject) =
                        (false, false, false);
                    for _ in 0..GAME_TICKS_PER_TICK {
                        let (slots, properties, bottle) = entry.tick();
                        slots_changed |= slots;
                        properties_changed |= properties;
                        eject |= bottle;
                    }
                    if brewing_inert(&entry.stand) {
                        sleep.insert(key, brewing_epoch(entry, content_revision()));
                    } else {
                        sleep.remove(&key);
                    }
                    if slots_changed || properties_changed {
                        updates.insert(
                            key,
                            (
                                slots_changed,
                                brewing_window_properties(&entry.stand),
                                eject,
                            ),
                        );
                    }
                    continue;
                }
                BREWING_ACTIVE_STEPS.fetch_add(1, Ordering::Relaxed);
                sleep.remove(&key);
                let (mut slots_changed, mut properties_changed, mut eject) =
                    (false, false, false);
                for _ in 0..GAME_TICKS_PER_TICK {
                    let (slots, properties, bottle) = entry.tick();
                    slots_changed |= slots;
                    properties_changed |= properties;
                    eject |= bottle;
                }
                if slots_changed || properties_changed {
                    updates.insert(
                        key,
                        (
                            slots_changed,
                            brewing_window_properties(&entry.stand),
                            eject,
                        ),
                    );
                }
            }
            // Entries for positions whose stand was removed outside
            // `remove_live_brewing_stand` can only make the map larger than
            // the store; sweep then.
            if sleep.len() > stands.len() {
                sleep.retain(|k, _| stands.contains_key(k));
            }
        }
        if updates.is_empty() {
            BREWING_UPDATES_SCRATCH.with(|s| scratch_map_return(s, updates));
            return;
        }
        for (&key @ (dim, x, y, z), &(slots, _, eject)) in &updates {
            if slots {
                update_brewing_bottles(key);
            }
            if eject && dim == DimId::Overworld {
                let stack = (name_to_item("glass_bottle").unwrap(), 1, Vec::new());
                drop_live_stack(
                    (f64::from(x) + 0.5, f64::from(y) + 0.5, f64::from(z) + 0.5),
                    stack,
                );
            }
        }
        for client in client_snapshot() {
            let Some(connection) = live_menu_connection(&client) else {
                continue;
            };
            let mut active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
            let Some(session) = active.as_mut() else {
                continue;
            };
            if !matches!(session.menu, LiveMenu::Brewing) {
                continue;
            }
            if let Some((slots, properties, _)) = session.anchor.and_then(|key| updates.get(&key)) {
                if *slots {
                    sync_live_menu(&client, session, false);
                } else {
                    let mut sock = client.sock.lock().unwrap_or_else(|e| e.into_inner());
                    for property in properties {
                        let _ = send_packet(&mut sock, 0x14, property.encode(session.id).to_vec());
                    }
                }
            }
        }
        BREWING_UPDATES_SCRATCH.with(|s| scratch_map_return(s, updates));
    });
}

fn always_edible(item: i64) -> bool {
    [
        name_to_item("golden_apple"),
        name_to_item("enchanted_golden_apple"),
        name_to_item("chorus_fruit"),
        name_to_item("honey_bottle"),
    ]
    .contains(&Some(item))
}
fn sat_value(item: i64) -> f32 {
    for (n, v) in [
        ("cooked_beef", 12.8f32),
        ("cooked_porkchop", 12.8),
        ("cooked_mutton", 9.6),
        ("cooked_rabbit", 10.0),
        ("cooked_chicken", 7.2),
        ("golden_apple", 9.6),
        ("enchanted_golden_apple", 9.6),
        ("bread", 6.0),
        ("baked_potato", 6.0),
        ("carrot", 3.6),
        ("apple", 2.4),
        ("chorus_fruit", 2.4),
        ("honey_bottle", 1.2),
        ("melon_slice", 1.2),
        ("beef", 1.8),
        ("porkchop", 1.8),
        ("mutton", 1.2),
        ("rabbit", 1.8),
        ("chicken", 1.2),
        ("potato", 0.6),
        ("beetroot", 1.2),
        ("rotten_flesh", 0.8),
        ("cookie", 0.4),
        ("dried_kelp", 0.2),
    ] {
        if name_to_item(n) == Some(item) {
            return v;
        }
    }
    0.0
}

fn food_value(item: i64) -> i32 {
    for (n, v) in [
        ("cooked_beef", 8),
        ("cooked_porkchop", 8),
        ("cooked_chicken", 6),
        ("cooked_mutton", 6),
        ("bread", 5),
        ("baked_potato", 5),
        ("cooked_rabbit", 5),
        ("apple", 4),
        ("rotten_flesh", 4),
        ("carrot", 3),
        ("beef", 3),
        ("porkchop", 3),
        ("rabbit", 3),
        ("mutton", 2),
        ("melon_slice", 2),
        ("cookie", 2),
        ("chicken", 1),
        ("potato", 1),
        ("beetroot", 1),
        ("dried_kelp", 1),
        ("honey_bottle", 6),
        ("chorus_fruit", 4),
        ("golden_apple", 4),
        ("enchanted_golden_apple", 4),
    ] {
        if name_to_item(n) == Some(item) {
            return v;
        }
    }
    0
}

fn item_tags() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static T: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(builtin_item_tags()))
}
fn builtin_item_tags() -> HashMap<String, Vec<String>> {
    [
        (
            "planks",
            &[
                "oak_planks",
                "spruce_planks",
                "birch_planks",
                "jungle_planks",
                "acacia_planks",
                "dark_oak_planks",
                "mangrove_planks",
                "cherry_planks",
                "crimson_planks",
                "warped_planks",
            ][..],
        ),
        (
            "logs",
            &[
                "oak_log",
                "spruce_log",
                "birch_log",
                "jungle_log",
                "acacia_log",
                "dark_oak_log",
                "mangrove_log",
                "cherry_log",
                "crimson_stem",
                "warped_stem",
            ][..],
        ),
        (
            "wool",
            &[
                "white_wool",
                "orange_wool",
                "magenta_wool",
                "light_blue_wool",
                "yellow_wool",
                "lime_wool",
                "pink_wool",
                "gray_wool",
                "light_gray_wool",
                "cyan_wool",
                "purple_wool",
                "blue_wool",
                "brown_wool",
                "green_wool",
                "red_wool",
                "black_wool",
            ][..],
        ),
    ]
    .iter()
    .map(|(k, v)| {
        (
            format!("minecraft:{}", k),
            v.iter().map(|s| s.to_string()).collect(),
        )
    })
    .collect()
}

// datapack crafting recipes (data/<ns>/recipe/**.json)
enum Ingr {
    Item(i64),
    Tag(String),
}
struct JsonRecipe {
    shaped: bool,
    rows: Vec<String>,        // shaped pattern rows (' '=air)
    key: HashMap<char, Ingr>, // shaped ingredient map
    ing: Vec<Ingr>,           // shapeless ingredients
    out: i64,
    out_cnt: u8,
}
fn json_recipes() -> &'static Mutex<Vec<JsonRecipe>> {
    static R: OnceLock<Mutex<Vec<JsonRecipe>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Vec::new()))
}
// datapack furnace recipes: smelting/blasting/smoking/campfire_cooking
fn json_smelts() -> &'static Mutex<Vec<SmeltRecipe>> {
    static R: OnceLock<Mutex<Vec<SmeltRecipe>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Vec::new()))
}

/// `ItemCooldowns` entry for `chorus_fruit`: vanilla arms a 20-tick (1 s)
/// cooldown on every eat — `ChorusFruitItem.finishUsingItem` ends with
/// `player.getItemCooldowns().addCooldown` — so a use inside it fails.
static CHORUS_CD: Registry<i32, Instant> = OnceLock::new();
fn chorus_cd() -> &'static Mutex<HashMap<i32, Instant>> {
    CHORUS_CD.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Entries die with the 1 s cooldown; the cap only binds pathological
/// reconnect storms that never let the expiry sweep run.
const MAX_CHORUS_CD_ENTRIES: usize = 4096;

/// Whether `eid`'s chorus cooldown is still running server-side.
fn chorus_on_cooldown(eid: i32) -> bool {
    chorus_cd()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&eid)
        .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
}

/// Arm the cooldown and send `SetCooldown` (0x17) so the client greys the icon
/// for 20 game ticks — the same wire shape the ender-pearl cooldown uses.
fn chorus_cooldown_start(me: &Client, item: i64) {
    {
        let mut cds = chorus_cd().lock().unwrap_or_else(|e| e.into_inner());
        // An entry is dead weight once the cooldown elapses; sweeping here
        // keeps the map bounded by concurrent cooldowns, not lifetime eats.
        cds.retain(|_, t| t.elapsed() < Duration::from_secs(1));
        if cds.len() >= MAX_CHORUS_CD_ENTRIES && !cds.contains_key(&me.eid) {
            if let Some(&oldest) = cds.keys().min() {
                cds.remove(&oldest);
            }
        }
        cds.insert(me.eid, Instant::now());
    }
    let mut cp = Vec::new();
    write_varint(&mut cp, item);
    write_varint(&mut cp, 20);
    let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x17, cp);
}

/// One `LivingEntity.randomTeleport` roll in the player's own dimension, as
/// `ChorusFruitItem.finishUsingItem` drives it: `x`/`z` scatter ±8 blocks and
/// `y` rolls ±8 clamped into `dim`'s build window; then the vanilla downward
/// scan lands the player on the first motion-blocking floor, committing only
/// when the feet and head cells are collision- and fluid-free. `None` is a
/// failed roll — a void column or a landing inside a block — which the caller
/// may re-roll up to sixteen times.
fn find_chorus_teleport_spot_dim(
    dim: DimId,
    (px, py, pz): (f64, f64, f64),
    (roll_x, roll_y, roll_z): (f64, i32, f64),
) -> Option<(f64, f64, f64)> {
    let reg = crate::block_registry::breg()?;
    let nx = px + (roll_x - 0.5) * 16.0;
    // `ChorusFruitItem` rolls `getY() + nextInt(16) - 8` as a double and clamps
    // into `min_y ..= min_y + height - 2`; the scan's `containing` floors it.
    let rolled_y = (py + roll_y as f64)
        .clamp(dim.min_y() as f64, (dim.min_y() + dim.height() - 2) as f64);
    let ny = rolled_y.floor() as i32;
    let nz = pz + (roll_z - 0.5) * 16.0;
    let (bx, bz) = (nx.floor() as i32, nz.floor() as i32);
    // `randomTeleport` only descends: step down while the cell below does not
    // stop motion; reaching the floor of the world unfounded fails the roll.
    let mut y = ny;
    while y > dim.min_y() && !reg.blocks_motion(block_at_dim(dim, bx, y - 1, bz)) {
        y -= 1;
    }
    if y <= dim.min_y() {
        return None;
    }
    // The standing box must be free of collisions and fluid before commit.
    let feet = block_at_dim(dim, bx, y, bz);
    let head = block_at_dim(dim, bx, y + 1, bz);
    if reg.blocks_motion(feet)
        || reg.blocks_motion(head)
        || reg.is_liquid(feet)
        || reg.is_liquid(head)
    {
        return None;
    }
    // `randomTeleport` decrements the rolled double y by whole steps, so the
    // landing keeps the fraction the rolled y carried into the block.
    Some((nx, y as f64 + (rolled_y - rolled_y.floor()), nz))
}

// air-use of the held item — shared by use_item (0x39) and the 1.21+
// block_place form ({-1,255,-1}, face -1) that the protocol emits when a
// usable item is activated in the air (bow draw, crossbow load, throws,
// food, shield). seq is acked here; callers must not ack again.
fn air_use_item(me: &Arc<Client>, held_slot: i16, seq: i64, _h: Option<i64>, eat_release: bool) {
    let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x05, ack(seq));
    {
        let inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let main = inv.get(&(36 + held_slot)).map(|v| v.0).unwrap_or(0);
        if main == 931 {
            // fishing rod: cast or reel
            drop(inv);
            let mut fb = bobbers().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ix) = fb.iter().position(|b| b.owner == me.eid) {
                let b = fb.remove(ix);
                broadcast_seen_destroy(b.eid);
                if b.bit {
                    // catch!
                    // datapack gameplay/fishing table wins; fallback raw fish
                    let table: Option<Vec<LootPool>> = loot_tables()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get("minecraft:gameplay/fishing")
                        .map(|p| {
                            p.iter()
                                .map(|pl| LootPool {
                                    rolls: pl.rolls,
                                    chance: pl.chance,
                                    funcs: pl.funcs.clone(),
                                    entries: pl
                                        .entries
                                        .iter()
                                        .map(|e| LootEntry {
                                            item: e.item,
                                            min: e.min,
                                            max: e.max,
                                            funcs: e.funcs.clone(),
                                        })
                                        .collect(),
                                })
                                .collect()
                        });
                    // Luck of the Sea: `apply_fishing_enchants` parked this
                    // bobber's re-weighted catch table under the owner's eid
                    // (`treasure = 5 + 2*luck` encodes the level exactly), so
                    // the reel reads that store; the enchant scan is the
                    // fallback for a bobber the movement tick never visited.
                    let luck = fishing_luck()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&me.eid)
                        .map(|w| (w.treasure - FISHING_BASE_WEIGHTS.treasure) / 2)
                        .unwrap_or_else(|| fishing_enchant_levels(me).0);
                    let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
                    let open_water = fishing_open_water(
                        dim,
                        b.x.floor() as i32,
                        b.y.floor() as i32,
                        b.z.floor() as i32,
                    );
                    let context = crate::chest_loot::LootContext {
                        luck: luck as f32,
                        this_in_open_water: open_water,
                        ..Default::default()
                    };
                    let built_in = crate::chest_loot::vanilla_loot_roll_with_context(
                        "minecraft:gameplay/fishing",
                        b.age as i64,
                        &context,
                    )
                    .into_iter()
                    .find_map(|entry| {
                        let item = name_to_item(entry.item)?;
                        let mut components = Vec::new();
                        if let Some((name, level)) = entry.enchantment {
                            if let Some(id) = enchant_id(name) {
                                comp_set(
                                    &mut components,
                                    COMP_ENCHANTMENTS,
                                    enchantments_data(&[(id, i64::from(level))], true),
                                );
                            }
                        }
                        Some((item, entry.count, components))
                    });
                    let catch = table.and_then(|p| roll_loot(&p).into_iter().next()).or(built_in);
                    let Some((fish, fcnt, fcomps)) = catch else {
                        return;
                    };
                    let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                    let slot = (9..=44).find(|sl| !inv.contains_key(sl)).unwrap_or(9);
                    inv.insert(slot, (fish, fcnt, fcomps.clone()));
                    let mut p = vec![4u8];
                    write_varint(&mut p, 0);
                    p.extend_from_slice(&slot.to_be_bytes());
                    p.extend_from_slice(&slot_wire(&(fish, fcnt, fcomps)));
                    let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x15, p);
                    drop(inv);
                    let xp = (world_rng::next_u64() % 6 + 1) as i32;
                    let _ = send_player_xp(me, |xp_state| {
                        let _ = add_player_xp_points(xp_state, xp);
                        Ok(())
                    });
                    damage_held(me);
                    held_slot_resync(me);
                }
            } else {
                let (x, y, z, yaw, pitch, _g) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
                let yr = (yaw as f64).to_radians();
                let pr = (pitch as f64).to_radians();
                let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                let b = Bobber {
                    eid,
                    owner: me.eid,
                    x,
                    y: y + 1.5,
                    z,
                    dx: -yr.sin() * pr.cos() * 1.2,
                    dy: -pr.sin() * 1.2 + 0.2,
                    dz: yr.cos() * pr.cos() * 1.2,
                    age: 0,
                    bit: false,
                    dip: 0,
                };
                fb.push(b);
                // spawn streams via interest_tick
            }
            return;
        }
        if main == 993 {
            // ender pearl: throw (vanilla 20t cooldown)
            if me.pearl_cd.lock().unwrap_or_else(|e| e.into_inner()).elapsed() < Duration::from_secs(1) {
                return;
            }
            *me.pearl_cd.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
            {
                let mut cp = Vec::new();
                write_varint(&mut cp, 993); // itemID
                write_varint(&mut cp, 20); // cooldownTicks
                if let Ok(mut w) = me.sock.lock() {
                    let _ = send_packet(&mut w, 0x17, cp);
                }
            }
            drop(inv);
            let (x, y, z, yaw, pitch, _g) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
            let yr = (yaw as f64).to_radians();
            let pr = (pitch as f64).to_radians();
            let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
            let pe = Pearl {
                eid,
                owner: me.eid,
                x,
                y: y + 1.5,
                z,
                dx: -yr.sin() * pr.cos() * 2.0,
                dy: -pr.sin() * 2.0 + 0.3,
                dz: yr.cos() * pr.cos() * 2.0,
                ttl: server_ticks(PEARL_TTL_GT) as i32,
            };
            pearls().lock().unwrap_or_else(|e| e.into_inner()).push(pe);
            // spawn streams via interest_tick
            if *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1 {
                let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                let hs = 36 + held_slot;
                if let Some((it, c, _cc)) = inv.get(&hs).cloned() {
                    if it == 993 {
                        if c <= 1 {
                            inv.remove(&hs);
                        } else {
                            inv.insert(hs, (it, c - 1, _cc));
                        }
                    }
                }
                drop(inv);
                held_slot_resync(me);
            }
            return;
        }
        if main == 912 || main == 927 {
            // snowball/egg: throw (egg may hatch a chicken)
            drop(inv);
            let (x, y, z, yaw, pitch, _g) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
            let yr = (yaw as f64).to_radians();
            let pr = (pitch as f64).to_radians();
            let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
            let sb = Snowball {
                eid,
                owner: me.eid,
                x,
                y: y + 1.5,
                z,
                dx: -yr.sin() * pr.cos() * 1.5,
                dy: -pr.sin() * 1.5 + 0.25,
                dz: yr.cos() * pr.cos() * 1.5,
                ttl: server_ticks(SNOWBALL_TTL_GT) as i32,
                egg: main == 927,
            };
            snowballs().lock().unwrap_or_else(|e| e.into_inner()).push(sb);
            // spawn streams via interest_tick
            if *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1 {
                let mut inv2 = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                let hs = 36 + held_slot;
                if let Some((it, c, _cc)) = inv2.get(&hs).cloned() {
                    if it == 912 || it == 927 {
                        if c <= 1 {
                            inv2.remove(&hs);
                        } else {
                            inv2.insert(hs, (it, c - 1, _cc));
                        }
                    }
                }
                drop(inv2);
                held_slot_resync(me);
            }
            return;
        }
        if main == 1162 || (_h == Some(1) && inv.get(&45).map(|v| v.0).unwrap_or(0) == 1162) {
            *me.blocking.lock().unwrap_or_else(|e| e.into_inner()) = true;
            ac_v2_note_use_item(me);
            return;
        }
        let asl = armor_slot_for(main);
        if asl > 0 {
            // vanilla: right-click equips armor into its slot (swaps)
            drop(inv);
            let hs = 36 + held_slot;
            let creative = *me.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1;
            let mut inv2 = me.inv.lock().unwrap_or_else(|e| e.into_inner());
            if armor_removal_blocked(asl, inv2.get(&asl), creative) {
                drop(inv2);
                held_slot_resync(me);
                return;
            }
            if let Some(st) = inv2.get(&hs).cloned() {
                let cur = inv2.get(&asl).cloned().unwrap_or((0, 0, Vec::new()));
                inv2.insert(asl, (st.0, 1, st.2.clone()));
                let leftover = st.1 - 1;
                if cur.0 != 0 {
                    if leftover > 0 {
                        inv2.insert(hs, (st.0, leftover, st.2));
                        let free = (9..=44i16).find(|sl| !inv2.contains_key(sl)).unwrap_or(9);
                        inv2.insert(free, cur);
                    } else {
                        inv2.insert(hs, cur);
                    }
                } else if leftover > 0 {
                    inv2.insert(hs, (st.0, leftover, st.2));
                } else {
                    inv2.remove(&hs);
                }
            }
            drop(inv2);
            held_slot_resync(me);
            {
                let st = me
                    .inv
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&asl)
                    .cloned()
                    .unwrap_or((0, 0, Vec::new()));
                let mut p = vec![0u8];
                write_varint(&mut p, i64::from(inv_state_next(me)));
                p.extend_from_slice(&asl.to_be_bytes());
                p.extend_from_slice(&slot_wire(&st));
                if let Ok(mut w) = me.sock.lock() {
                    let _ = send_packet(&mut w, 0x15, p);
                }
            }
            return;
        }
        if main == 914 {
            // milk_bucket: clears all effects, leaves a bucket
            if !eat_release {
                // vanilla: milk is a 32-tick drink like food
                *me.eat_start.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                ac_v2_note_use_item(me);
                return;
            }
            drop(inv);
            let gone: Vec<i32> = {
                let mut ef = me.effs.lock().unwrap_or_else(|e| e.into_inner());
                let g: Vec<i32> = ef.iter().map(|e| e.id).collect();
                ef.clear();
                g
            };
            for id in &gone {
                let pk = remove_effect_pkt(me.eid, *id);
                let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x43, pk.clone());
                broadcast_seen(me.eid, |_c| (0x43, pk.clone()));
            }
            if *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1 {
                let mut iv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                let hs = 36 + held_slot;
                iv.insert(hs, (name_to_item("bucket").unwrap_or(0), 1, Vec::new()));
            }
            held_slot_resync(me);
            return;
        }
        if main == 1192
            && inv
                .get(&(36 + held_slot))
                .is_some_and(|st| comp_get(&st.2, COMP_CHARGED_PROJECTILES).is_some())
        {
            // A loaded crossbow fires its stored `charged_projectiles` on use
            // instead of starting another draw.
            drop(inv);
            crossbow_fire_loaded(me, held_slot);
            return;
        }
        if main == 1203 {
            // goat horn: the `instrument` component names the sound event.
            drop(inv);
            goat_horn_use(me, held_slot);
            return;
        }
        if main == 1166 {
            // knowledge book: the `recipes` component grants recipe ids.
            drop(inv);
            knowledge_book_use(me, held_slot);
            return;
        }
        if main == 1112 {
            // firework rocket: the `fireworks` component drives the launch.
            drop(inv);
            firework_rocket_use(me, held_slot);
            return;
        }
        if main == 801 || main == 1192 || main == name_to_item("trident").unwrap_or(-9) {
            *me.bow_draw.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            ac_v2_note_use_item(me);
            return;
        }
    }
    if *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1 {
        // prefer held slot; fall back to first food item in inventory
        let (slot, it, fc, eaten) = {
            let inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
            let hs = 36 + held_slot;
            match inv.get(&hs) {
                Some(st) if stack_food(st).is_some() => {
                    (hs, st.0, stack_food(st), st.clone())
                }
                _ => inv
                    .iter()
                    .find(|(_, st)| stack_food(st).is_some())
                    .map(|(&sl, st)| (sl, st.0, stack_food(st), st.clone()))
                    .unwrap_or((hs, 0, None, (0, 0, Vec::new()))),
            }
        };
        let cur_food = *me.food.lock().unwrap_or_else(|e| e.into_inner());
        // `ItemCooldowns.isOnCooldown` fails the use outright: a cooling-down
        // chorus fruit never starts the eat.
        let chorus_cd_hit =
            it == name_to_item("chorus_fruit").unwrap_or(-9) && chorus_on_cooldown(me.eid);
        let edible = !chorus_cd_hit
            && fc
                .as_ref()
                .is_some_and(|fc| cur_food < 20 || fc.can_always_eat);
        if edible && !eat_release {
            // vanilla: eating holds use for 32 ticks; nothing consumed yet
            *me.eat_start.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            ac_v2_note_use_item(me);
        }
        if edible && eat_release {
            let fc = fc.clone().unwrap_or(FoodComponent {
                nutrition: 0,
                saturation: 0.0,
                can_always_eat: false,
                eat_seconds: 1.6,
                converts_to: (0, 0, Vec::new()),
                effects: Vec::new(),
            });
            let mut f = me.food.lock().unwrap_or_else(|e| e.into_inner());
            let mut st = me.sat.lock().unwrap_or_else(|e| e.into_inner());
            // The component's `saturation` is the vanilla saturation modifier:
            // gain = modifier * nutrition * 2.
            (*f, *st) = FoodData::values_after_eating(
                *f,
                *st,
                fc.nutrition as i32,
                fc.saturation * fc.nutrition as f32 * 2.0,
            );
            drop(st);
            drop(f);
            {
                let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                let (nc, cc2) = inv
                    .get(&slot)
                    .map(|e| (e.1.saturating_sub(1), e.2.clone()))
                    .unwrap_or((0, Vec::new()));
                if nc == 0 {
                    // `using_converts_to`: the consumed stack leaves its
                    // conversion item (ominous bottle -> glass_bottle, stews
                    // -> bowl).
                    if fc.converts_to.0 > 0 && fc.converts_to.1 > 0 {
                        inv.insert(slot, fc.converts_to.clone());
                    } else {
                        inv.remove(&slot);
                    }
                } else {
                    inv.insert(slot, (it, nc, cc2));
                }
            }
            let st_now = me
                .inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&slot)
                .cloned()
                .unwrap_or((0, 0, Vec::new()));
            let mut p = vec![0u8];
            write_varint(&mut p, i64::from(inv_state_next(me)));
            p.extend_from_slice(&slot.to_be_bytes());
            p.extend_from_slice(&slot_wire(&st_now));
            let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x15, p);
            let hh = *me.hp.lock().unwrap_or_else(|e| e.into_inner());
            let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x5d, health_pkt_c(hh, me));
            // vanilla food side-effects: component `effects`, suspicious stew
            // rolls, and the item defaults table.
            for &(ch, eid2, amp, ticks) in &stack_food_effects(&eaten) {
                if (world_rng::next_u64() % 100) < ch {
                    {
                        let mut ef = me.effs.lock().unwrap_or_else(|e| e.into_inner());
                        ef.retain(|e| e.id != eid2);
                        ef.push(Eff {
                            id: eid2,
                            amp,
                            left: ticks,
                        });
                    }
                    if eid2 == 21 {
                        *me.absorb.lock().unwrap_or_else(|e| e.into_inner()) = (amp + 1) as f32 * 4.0;
                    }
                    let pk = entity_effect_pkt(me.eid, eid2, amp, ticks);
                    let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x76, pk.clone());
                    broadcast_seen(me.eid, |_c| (0x76, pk.clone()));
                }
            }
            // honey bottle clears poison only
            if it == name_to_item("honey_bottle").unwrap_or(-9) {
                let had = {
                    let mut ef = me.effs.lock().unwrap_or_else(|e| e.into_inner());
                    let h = ef.iter().any(|e| e.id == 18);
                    ef.retain(|e| e.id != 18);
                    h
                };
                if had {
                    let pk = remove_effect_pkt(me.eid, 18);
                    let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x43, pk.clone());
                    broadcast_seen(me.eid, |_c| (0x43, pk.clone()));
                }
            }
            // chorus fruit: `ChorusFruitItem.finishUsingItem` — up to sixteen
            // `LivingEntity.randomTeleport` rolls inside the player's own
            // dimension, then the vanilla 20-tick item cooldown whether a roll
            // landed or not.
            if it == name_to_item("chorus_fruit").unwrap_or(-9) {
                // `finishUsingItem` dismounts a rider before its first roll.
                player_dismount(me, false);
                let (px, py, pz, yaw, pitch, _) =
                    *me.pos.lock().unwrap_or_else(|e| e.into_inner());
                let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
                let mut landing = None;
                for _ in 0..16 {
                    let roll = (
                        (world_rng::next_u64() % 1000) as f64 / 1000.0,
                        (world_rng::next_u64() % 16) as i32 - 8,
                        (world_rng::next_u64() % 1000) as f64 / 1000.0,
                    );
                    if let Some(spot) = find_chorus_teleport_spot_dim(dim, (px, py, pz), roll) {
                        landing = Some(spot);
                        break;
                    }
                }
                if let Some((nx, ny, nz)) = landing {
                    {
                        let mut w = me.sock.lock().unwrap_or_else(|e| e.into_inner());
                        teleport_in_dimension(me, &mut w, (nx, ny, nz, yaw, pitch));
                    }
                    // `randomTeleport` zeroes the fall distance: the meter
                    // re-bases at the landing so the hop arrives clean.
                    *me.fall_from.lock().unwrap_or_else(|e| e.into_inner()) = ny;
                    // `entity.chorus_fruit.teleport` rides the entity for the
                    // trackers watching it.
                    broadcast_entity_sound(me.eid, "minecraft:entity.chorus_fruit.teleport", 7);
                }
                chorus_cooldown_start(me, it);
            }
        }
    }
}

#[cfg(test)]
mod p1_combat_item_tests {
    use super::*;

    fn am_mod(attribute_id: i32, amount: f64, op: ItemAttrOp, slot: ItemAttrSlotGroup) -> ItemAttributeModifier {
        ItemAttributeModifier {
            attribute_id,
            modifier_id: "megu:test".to_string(),
            amount,
            operation: op,
            slot,
        }
    }

    #[test]
    fn p1_combat_attribute_modifiers_codec_roundtrip() {
        let comp = AttributeModifiersComponent {
            modifiers: vec![
                am_mod(2, 3.5, ItemAttrOp::AddValue, ItemAttrSlotGroup::MainHand),
                am_mod(4, -1.25, ItemAttrOp::AddMultipliedBase, ItemAttrSlotGroup::Any),
                am_mod(16, 8.0, ItemAttrOp::AddMultipliedTotal, ItemAttrSlotGroup::Armor),
            ],
            show_in_tooltip: false,
        };
        let data = attr_modifiers_data(&comp);
        assert_eq!(attr_modifiers_parse(&data), Some(comp));
        // Trailing garbage and truncation both reject.
        let mut long = data.clone();
        long.push(0);
        assert_eq!(attr_modifiers_parse(&long), None);
        assert_eq!(attr_modifiers_parse(&data[..data.len() - 1]), None);
        // Bad operation rejects.
        let mut bad_op = attr_modifiers_data(&AttributeModifiersComponent {
            modifiers: vec![am_mod(2, 1.0, ItemAttrOp::AddValue, ItemAttrSlotGroup::Any)],
            show_in_tooltip: true,
        });
        let n = bad_op.len();
        bad_op[n - 3] = 7; // operation byte position (op, slot, bool tail)
        assert_eq!(attr_modifiers_parse(&bad_op), None);
    }

    #[test]
    fn p1_combat_attribute_modifiers_slot_wire_roundtrip() {
        let sword = name_to_item("diamond_sword").unwrap();
        let comp = AttributeModifiersComponent {
            modifiers: vec![
                am_mod(2, 3.0, ItemAttrOp::AddValue, ItemAttrSlotGroup::MainHand),
                am_mod(4, -2.4, ItemAttrOp::AddValue, ItemAttrSlotGroup::MainHand),
            ],
            show_in_tooltip: true,
        };
        let mut comps = Vec::new();
        comp_set(&mut comps, COMP_ATTRIBUTE_MODIFIERS, attr_modifiers_data(&comp));
        let st: Stack = (sword, 1, comps);
        let mut packet = 9i16.to_be_bytes().to_vec();
        packet.extend_from_slice(&slot_wire(&st));
        let (slot, (item, cnt, wire_comps)) = slot_read_wire(&packet).unwrap();
        assert_eq!(slot, 9);
        assert_eq!((item, cnt), (sword, 1));
        assert_eq!(
            comp_get(&wire_comps, COMP_ATTRIBUTE_MODIFIERS).and_then(attr_modifiers_parse),
            Some(comp)
        );
    }

    #[test]
    fn p1_combat_attribute_modifiers_nbt_roundtrip() {
        let sword = name_to_item("diamond_sword").unwrap();
        let comp = AttributeModifiersComponent {
            modifiers: vec![am_mod(
                13,
                0.35,
                ItemAttrOp::AddValue,
                ItemAttrSlotGroup::Chest,
            )],
            show_in_tooltip: true,
        };
        let mut comps = Vec::new();
        comp_set(&mut comps, COMP_ATTRIBUTE_MODIFIERS, attr_modifiers_data(&comp));
        let st: Stack = (sword, 1, comps);
        let tag = stack_to_nbt(6, &st).unwrap();
        let items = crate::nbt::Tag::List(10, vec![tag]);
        let back = items_nbt_to_stacks(&items).unwrap();
        let (_, _, round) = back.get(&6).unwrap();
        assert_eq!(
            comp_get(round, COMP_ATTRIBUTE_MODIFIERS).and_then(attr_modifiers_parse),
            Some(comp)
        );
    }
}

#[cfg(test)]
mod p5a_beacon_tests {
    use super::*;
    #[cfg(megu_vanilla_data)]
    use crate::effect_tests::client_pair;
    use std::io::Read;

    /// Restores the block-entity store, placed cells, menu sessions, clients
    /// and drops around a live beacon-menu test.
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    struct BeaconWorld {
        store: LiveBlockEntityStore,
        cells: Vec<(DimBlockPos, u32)>,
        sessions: HashMap<i32, PlayerMechanismSession>,
        players: Vec<Arc<Client>>,
    }

    impl BeaconWorld {
        #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
        fn new() -> Self {
            Self {
                store: std::mem::take(&mut *live_block_entities().lock().unwrap_or_else(|e| e.into_inner())),
                cells: Vec::new(),
                sessions: std::mem::take(&mut *player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner())),
                players: std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner())),
            }
        }

        #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
        fn place(&mut self, key @ (dim, x, y, z): DimBlockPos, block: &str) -> u32 {
            let state = item_to_state(name_to_item(block).unwrap()).unwrap();
            let old = block_at_dim(dim, x, y, z);
            self.cells.push((key, old));
            place_block_dim(dim, x, y, z, state);
            assert_eq!(block_at_dim(dim, x, y, z), state, "{block}");
            state
        }
    }

    impl std::ops::Drop for BeaconWorld {
        fn drop(&mut self) {
            for &((dim, x, y, z), state) in self.cells.iter().rev() {
                set_block_dim(dim, x, y, z, state);
            }
            *live_block_entities().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.store);
            *player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.sessions);
            *clients().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.players);
        }
    }

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn attach(c: &Client) {
        c.enter_play_wire(None, Default::default());
        player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner()).insert(
            c.eid,
            PlayerMechanismSession {
                xp_bar: (0, 0.0),
                food_timer: 0,
                menus: Arc::new(LiveMenuConnection {
                    active: Mutex::new(None),
                    next_id: std::sync::atomic::AtomicU8::new(4),
                    enchantment_seed: Mutex::new(0),
                }),
            },
        );
    }

    /// Drain every frame the socket received; `read` hits the fixture's 1s
    /// timeout once the batch is exhausted.
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn read_frames(rx: &mut TcpStream) -> Vec<(i64, Vec<u8>)> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = rx.read(&mut chunk) {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let mut frames = Vec::new();
        let mut rest = buf.as_slice();
        while !rest.is_empty() {
            let mut view = rest;
            let Some(len) = read_varint(&mut view) else { break };
            let len = len as usize;
            if view.len() < len {
                break;
            }
            let mut body = &view[..len];
            rest = &view[len..];
            let Some(id) = read_varint(&mut body) else { break };
            frames.push((id, body.to_vec()));
        }
        frames
    }

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn beacon_of(key: DimBlockPos) -> (Option<BeaconEffect>, Option<BeaconEffect>, i32) {
        let store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
        let Some((_, LiveBlockEntity::Beacon(beacon))) = store.entries.get(&key) else {
            panic!("beacon entity missing at {key:?}");
        };
        (beacon.primary, beacon.secondary, beacon.levels)
    }

    // #204 — `minecraft:beacon` is registry id 9 on protocol 767 (index of
    // `proto::data_v767::V767_MENUS`; crafter_3x3 is 7, anvil 8), and the menu
    // owns exactly one input slot: the payment slot.
    #[test]
    fn p5a_parity_beacon_menu_type_and_slots() {
        let mut menu = LiveMenu::Beacon(BeaconMenu::default());
        assert_eq!(menu.screen(), (9, "Beacon"));
        assert_eq!(menu.input_slots().len(), 1);
    }

    // #204 — slot 0 accepts only `#minecraft:beacon_payment_items`, stacked to
    // a maximum of one (vanilla `Slot.getMaxStackSize() == 1`).
    #[test]
    fn p5a_parity_beacon_payment_slot_accepts_only_tag_items() {
        let menu = LiveMenu::Beacon(BeaconMenu::default());
        for item in [
            "iron_ingot",
            "gold_ingot",
            "diamond",
            "emerald",
            "netherite_ingot",
        ] {
            let st = (name_to_item(item).unwrap(), 1, Vec::new());
            assert_eq!(live_input_limit(&menu, 0, &st), 1, "{item}");
            assert!(beacon_payment_item(&st), "{item}");
        }
        for item in ["coal", "iron_sword", "stone", "iron_block", "bread"] {
            let st = (name_to_item(item).unwrap(), 1, Vec::new());
            assert_eq!(live_input_limit(&menu, 0, &st), 0, "{item}");
            assert!(!beacon_payment_item(&st), "{item}");
        }
    }

    // #204 — the pyramid tiers gate the choice exactly like the vanilla
    // beacon screen: tier 1 Speed/Haste, tier 2 Resistance/Jump Boost,
    // tier 3 Strength, tier 4 Regeneration or the primary again (Tier II).
    #[test]
    fn p5a_parity_beacon_pyramid_tier_gating() {
        use BeaconEffect::*;
        // Level 0: nothing selectable.
        assert!(!beacon_selection_allowed(0, Some(Speed), None));
        // Level 1: Speed/Haste only — Strength and Regeneration refused.
        assert!(beacon_selection_allowed(1, Some(Speed), None));
        assert!(beacon_selection_allowed(1, Some(Haste), None));
        assert!(!beacon_selection_allowed(1, Some(Strength), None));
        assert!(!beacon_selection_allowed(1, Some(Regeneration), None));
        assert!(!beacon_selection_allowed(1, Some(Speed), Some(Regeneration)));
        // Level 2 unlocks Resistance/Jump Boost but no secondary.
        assert!(beacon_selection_allowed(2, Some(JumpBoost), None));
        assert!(beacon_selection_allowed(2, Some(Resistance), None));
        assert!(!beacon_selection_allowed(2, Some(Strength), None));
        assert!(!beacon_selection_allowed(2, Some(Speed), Some(Speed)));
        // Level 3 unlocks Strength; secondary still gated to level 4.
        assert!(beacon_selection_allowed(3, Some(Strength), None));
        assert!(!beacon_selection_allowed(3, Some(Regeneration), None));
        assert!(!beacon_selection_allowed(3, Some(Strength), Some(Regeneration)));
        // Level 4: Regeneration as primary, Regeneration as secondary, or the
        // primary repeated for the Tier-II upgrade.
        assert!(beacon_selection_allowed(4, Some(Regeneration), None));
        assert!(beacon_selection_allowed(4, Some(Strength), Some(Regeneration)));
        assert!(beacon_selection_allowed(4, Some(Strength), Some(Strength)));
        // ...but never an unrelated tier effect as the secondary.
        assert!(!beacon_selection_allowed(4, Some(Strength), Some(JumpBoost)));
        assert!(!beacon_selection_allowed(4, Some(Strength), Some(Speed)));
        // A secondary with no primary is not a valid selection.
        assert!(!beacon_selection_allowed(4, None, Some(Regeneration)));
        assert_eq!(beacon_effect_tier(Speed), 1);
        assert_eq!(beacon_effect_tier(Haste), 1);
        assert_eq!(beacon_effect_tier(Resistance), 2);
        assert_eq!(beacon_effect_tier(JumpBoost), 2);
        assert_eq!(beacon_effect_tier(Strength), 3);
        assert_eq!(beacon_effect_tier(Regeneration), 4);
    }

    // #204 — applying an unanchored update still enforces payment + tiers and
    // consumes exactly one payment item.
    #[test]
    fn p5a_parity_beacon_apply_consumes_one_payment() {
        let diamond = name_to_item("diamond").unwrap();
        let mut menu = BeaconMenu {
            payment: (diamond, 3, Vec::new()),
            anchor: None,
            power_level: 4,
        };
        assert!(apply_live_beacon_update(
            &mut menu,
            Some(BeaconEffect::Strength),
            Some(BeaconEffect::Regeneration)
        ));
        assert_eq!(menu.payment, (diamond, 2, Vec::new()));
        // Tier gate rejects before paying: payment untouched.
        menu.power_level = 1;
        assert!(!apply_live_beacon_update(
            &mut menu,
            Some(BeaconEffect::Strength),
            None
        ));
        assert_eq!(menu.payment.1, 2);
        // A non-payment item never reaches the entity.
        menu.payment = (name_to_item("coal").unwrap(), 1, Vec::new());
        assert!(!apply_live_beacon_update(&mut menu, Some(BeaconEffect::Speed), None));
        // Slot drains to empty after the last valid payment.
        menu.payment = (diamond, 1, Vec::new());
        menu.power_level = 4;
        assert!(apply_live_beacon_update(&mut menu, Some(BeaconEffect::Speed), None));
        assert_eq!(menu.payment.1, 0);
        assert!(!apply_live_beacon_update(&mut menu, Some(BeaconEffect::Speed), None));
    }

    // #204 — opening a real beacon emits the open packet (menu type 9), the
    // three `ContainerSetData` rows, and the payment slot; `apply` writes the
    // choice into the anchored block entity.
    #[test]
    #[cfg(megu_vanilla_data)]
    fn p5a_parity_beacon_live_menu_wire_and_apply() {
        let _guard = world_guard();
        let mut world = BeaconWorld::new();
        let key @ (dim, x, y, z) = (DimId::Overworld, 9_300_000, 220, 9_300_000);
        let state = world.place(key, "beacon");
        // Seed a level-4 pyramid with a running Speed effect.
        {
            let reg = breg().unwrap();
            let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
            let Some(LiveBlockEntity::Beacon(beacon)) =
                store.at(reg, key, block_at_dim(dim, x, y, z))
            else {
                panic!("beacon entity must materialize");
            };
            beacon.levels = 4;
            beacon.primary = Some(BeaconEffect::Speed);
        }
        let (mut c, mut rx) = client_pair();
        c.eid = 5252;
        *c.pos.lock().unwrap_or_else(|e| e.into_inner()) = (
            f64::from(x) + 0.5,
            f64::from(y) + 1.0,
            f64::from(z) + 0.5,
            0.0,
            0.0,
            true,
        );
        attach(&c);
        assert!(open_live_block_menu(&c, key, state));
        let frames = read_frames(&mut rx);
        let (window, kind) = frames
            .iter()
            .find(|(id, _)| *id == 0x33)
            .map(|(_, body)| {
                let mut b = body.as_slice();
                let window = read_varint(&mut b).unwrap();
                (window as u8, read_varint(&mut b).unwrap())
            })
            .expect("0x33 open packet");
        assert_eq!(kind, 9, "beacon menu type");
        let props: Vec<(i16, i16)> = frames
            .iter()
            .filter(|(id, _)| *id == 0x14)
            .map(|(_, b)| {
                assert_eq!(b[0], window);
                assert_eq!(b.len(), 5);
                (
                    i16::from_be_bytes([b[1], b[2]]),
                    i16::from_be_bytes([b[3], b[4]]),
                )
            })
            .collect();
        assert_eq!(
            props,
            vec![(0, 4), (1, 1), (2, -1)],
            "level 4, primary speed(1), no secondary"
        );
        let conn = live_menu_connection(&c).unwrap();
        let mut active = conn.active.lock().unwrap_or_else(|e| e.into_inner());
        let session = active.as_mut().unwrap();
        assert!(live_menu_valid(&c, session));
        let LiveMenu::Beacon(menu) = &mut session.menu else {
            panic!("beacon menu expected");
        };
        assert_eq!(menu.power_level, 4);
        // A diamond in slot 0 pays for the Strength + Regeneration choice.
        menu.payment = (name_to_item("diamond").unwrap(), 1, Vec::new());
        assert!(apply_live_beacon_update(
            menu,
            Some(BeaconEffect::Strength),
            Some(BeaconEffect::Regeneration)
        ));
        assert_eq!(menu.payment.1, 0);
        assert_eq!(
            beacon_of(key),
            (
                Some(BeaconEffect::Strength),
                Some(BeaconEffect::Regeneration),
                4
            )
        );
        // Tier enforcement is live: a level-1 beacon refuses Strength.
        let key2 @ (dim2, x2, y2, z2) = (DimId::Overworld, 9_300_001, 220, 9_300_000);
        world.place(key2, "beacon");
        {
            let reg = breg().unwrap();
            let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
            let Some(LiveBlockEntity::Beacon(beacon)) =
                store.at(reg, (dim2, x2, y2, z2), block_at_dim(dim2, x2, y2, z2))
            else {
                panic!("second beacon entity must materialize");
            };
            beacon.levels = 1;
        }
        let mut low = BeaconMenu {
            payment: (name_to_item("iron_ingot").unwrap(), 1, Vec::new()),
            anchor: Some(key2),
            power_level: 0,
        };
        assert!(!apply_live_beacon_update(&mut low, Some(BeaconEffect::Strength), None));
        assert_eq!(low.payment.1, 1, "rejected choice never consumes payment");
        assert_eq!(beacon_of(key2), (None, None, 1));
        assert!(apply_live_beacon_update(&mut low, Some(BeaconEffect::Haste), None));
        assert_eq!(beacon_of(key2), (Some(BeaconEffect::Haste), None, 1));
        drop(active);
        close_live_menu(&c, false, false);
    }

    // #204 — a click on slot 0 follows the payment rules through the real
    // `live_menu_click` path: one item lands, non-payment items are refused.
    #[test]
    #[cfg(megu_vanilla_data)]
    fn p5a_parity_beacon_slot_click_enforces_payment_rules() {
        let _guard = world_guard();
        let mut world = BeaconWorld::new();
        let key @ (_dim, x, y, z) = (DimId::Overworld, 9_300_010, 220, 9_300_010);
        let state = world.place(key, "beacon");
        let (mut c, mut rx) = client_pair();
        c.eid = 5253;
        *c.pos.lock().unwrap_or_else(|e| e.into_inner()) = (
            f64::from(x) + 0.5,
            f64::from(y) + 1.0,
            f64::from(z) + 0.5,
            0.0,
            0.0,
            true,
        );
        attach(&c);
        assert!(open_live_block_menu(&c, key, state));
        let _ = read_frames(&mut rx);
        let window = live_menu_id(&c).unwrap();
        // Cursor with 3 iron ingots: a click pays exactly one into slot 0.
        let iron = name_to_item("iron_ingot").unwrap();
        *c.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (iron, 3, Vec::new());
        live_menu_click(&c, window, 0, 0, 0);
        {
            let conn = live_menu_connection(&c).unwrap();
            let active = conn.active.lock().unwrap_or_else(|e| e.into_inner());
            let LiveMenu::Beacon(menu) = &active.as_ref().unwrap().menu else {
                panic!("beacon menu expected");
            };
            assert_eq!(menu.payment, (iron, 1, Vec::new()));
        }
        assert_eq!(c.cursor.lock().unwrap_or_else(|e| e.into_inner()).1, 2);
        // A sword on the cursor cannot displace the payment or enter the slot.
        let sword = name_to_item("iron_sword").unwrap();
        *c.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (sword, 1, Vec::new());
        live_menu_click(&c, window, 0, 0, 0);
        {
            let conn = live_menu_connection(&c).unwrap();
            let active = conn.active.lock().unwrap_or_else(|e| e.into_inner());
            let LiveMenu::Beacon(menu) = &active.as_ref().unwrap().menu else {
                panic!("beacon menu expected");
            };
            assert_eq!(menu.payment, (iron, 1, Vec::new()));
        }
        assert_eq!(*c.cursor.lock().unwrap_or_else(|e| e.into_inner()), (sword, 1, Vec::new()));
        // With an empty cursor the payment comes back out of slot 0.
        *c.cursor.lock().unwrap_or_else(|e| e.into_inner()) = Stack::default();
        live_menu_click(&c, window, 0, 0, 0);
        let conn = live_menu_connection(&c).unwrap();
        let active = conn.active.lock().unwrap_or_else(|e| e.into_inner());
        let LiveMenu::Beacon(menu) = &active.as_ref().unwrap().menu else {
            panic!("beacon menu expected");
        };
        assert_eq!(menu.payment.1, 0);
        drop(active);
        close_live_menu(&c, false, false);
    }
}

#[cfg(test)]
mod p6c_hopper_parity_tests {
    use super::*;

    const HY: i32 = 120;
    const HPOS: (i32, i32, i32) = (0, HY, 0);

    struct SavedWorld {
        hoppers: HashSet<(i32, i32, i32)>,
        chests: HashMap<ContainerKey, HashMap<i16, Stack>>,
        drops: Vec<Drop>,
        forced: HashSet<(i32, i32)>,
        backoff: HashMap<DimBlockPos, (u64, u32)>,
        tick: u64,
        index: u64,
        hot: HashSet<(i32, i32)>,
        warm: HashSet<(i32, i32)>,
    }

    fn save_world() -> SavedWorld {
        SavedWorld {
            hoppers: std::mem::take(&mut *hoppers().lock().unwrap_or_else(|e| e.into_inner())),
            chests: std::mem::take(&mut *chests().lock().unwrap_or_else(|e| e.into_inner())),
            drops: std::mem::take(&mut *drops().lock().unwrap_or_else(|e| e.into_inner())),
            forced: std::mem::take(&mut *forceloaded().lock().unwrap_or_else(|e| e.into_inner())),
            backoff: std::mem::take(&mut *backoff().lock().unwrap_or_else(|e| e.into_inner())),
            tick: TICK.swap(1, Ordering::Relaxed),
            index: TICK_IDX.swap(0, Ordering::Relaxed),
            hot: test_region().0,
            warm: test_region().1,
        }
    }

    fn restore_world(saved: SavedWorld) {
        for y in HY - 1..=HY + 1 {
            set_block(0, y, 0, 0);
        }
        set_block(1, HY, 0, 0);
        hopper_queue().lock().unwrap_or_else(|e| e.into_inner()).cancel(HPOS);
        *hoppers().lock().unwrap_or_else(|e| e.into_inner()) = saved.hoppers;
        *chests().lock().unwrap_or_else(|e| e.into_inner()) = saved.chests;
        *drops().lock().unwrap_or_else(|e| e.into_inner()) = saved.drops;
        *forceloaded().lock().unwrap_or_else(|e| e.into_inner()) = saved.forced;
        *backoff().lock().unwrap_or_else(|e| e.into_inner()) = saved.backoff;
        TICK.store(saved.tick, Ordering::Relaxed);
        TICK_IDX.store(saved.index, Ordering::Relaxed);
        set_test_region(saved.hot, saved.warm);
    }

    /// One warm chunk so `hopper_tick`'s `is_warm` gate passes, and a clean
    /// queue slot so `set_block`'s schedule call cannot lose to a leftover.
    fn begin_hopper_world() {
        set_test_region(HashSet::from([(0, 0)]), HashSet::from([(0, 0)]));
        hopper_queue().lock().unwrap_or_else(|e| e.into_inner()).cancel(HPOS);
    }

    fn put(pos: (i32, i32, i32), slot: i16, stack: Stack) {
        chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(ContainerKey::ow(pos))
            .or_default()
            .insert(slot, stack);
    }

    fn stacks_at(pos: (i32, i32, i32)) -> Vec<(i64, u8)> {
        let mut v: Vec<(i64, u8)> = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ContainerKey::ow(pos))
            .map(|m| m.values().map(|(i, c, _)| (*i, *c)).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn p6c_parity_hopper_pushes_and_pulls_in_one_activation() {
        let _guard = world_guard();
        let saved = save_world();
        begin_hopper_world();
        let hy = HY;
        let chest_st = name_to_state("chest").unwrap_or(2954);
        let push_item = name_to_item("raw_iron").unwrap_or(1);
        let pull_item = name_to_item("coal").unwrap_or(2);
        set_block(0, hy, 0, 9225); // hopper facing down
        set_block(0, hy - 1, 0, chest_st);
        set_block(0, hy + 1, 0, chest_st);
        put(HPOS, 0, (push_item, 1, Vec::new()));
        put((0, hy + 1, 0), 0, (pull_item, 3, Vec::new()));

        hopper_tick();
        // Vanilla `ejectItems | suckInItems`: the primed hopper pushed its own
        // item down AND pulled one from the chest above into its own slots in
        // the same activation — the pull lands in the hopper, not the output.
        assert_eq!(stacks_at((0, hy - 1, 0)), vec![(push_item, 1)]);
        assert_eq!(stacks_at((0, hy + 1, 0)), vec![(pull_item, 2)]);
        assert_eq!(stacks_at(HPOS), vec![(pull_item, 1)]);

        for tick in 1..HOPPER_TRANSFER_COOLDOWN {
            TICK_IDX.fetch_add(1, Ordering::Relaxed);
            if tick == 1 {
                wake_hoppers_around(ow_block((0, hy + 1, 0)));
            }
            hopper_tick();
            assert_eq!(
                stacks_at((0, hy - 1, 0)),
                vec![(push_item, 1)],
                "live transfer tick {tick} must remain inside the cooldown"
            );
        }
        TICK_IDX.fetch_add(1, Ordering::Relaxed);
        hopper_tick();
        // The eighth server tick activates the hopper again: the pulled item is
        // the lowest occupied slot, so it pushes while another item is pulled.
        assert_eq!(stacks_at((0, hy - 1, 0)), vec![(pull_item, 1), (push_item, 1)]);
        assert_eq!(stacks_at((0, hy + 1, 0)), vec![(pull_item, 1)]);
        assert_eq!(stacks_at(HPOS), vec![(pull_item, 1)]);
        restore_world(saved);
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn p6c_parity_powered_hopper_holds_items_and_pull_needs_no_push_target() {
        let _guard = world_guard();
        let saved = save_world();
        begin_hopper_world();
        let hy = HY;
        let chest_st = name_to_state("chest").unwrap_or(2954);
        let item = name_to_item("raw_iron").unwrap_or(1);

        // Powered (locked) hopper: block updates recompute `enabled`, so the
        // lock must come from real power — a redstone block beside it.
        set_block(0, hy, 0, 9225);
        set_block(0, hy - 1, 0, chest_st);
        set_block(0, hy + 1, 0, chest_st);
        set_block(1, hy, 0, name_to_state("redstone_block").unwrap_or(9223));
        assert_eq!(block_at(0, hy, 0), 9230, "live update powered the hopper");
        put(HPOS, 0, (item, 1, Vec::new()));
        put((0, hy + 1, 0), 0, (item, 3, Vec::new()));
        for _ in 0..4 {
            hopper_tick();
        }
        assert_eq!(stacks_at((0, hy - 1, 0)), vec![]);
        assert_eq!(stacks_at((0, hy + 1, 0)), vec![(item, 3)]);
        assert_eq!(stacks_at(HPOS), vec![(item, 1)]);

        // Unpowered hopper facing east into plain air: vanilla still runs
        // `suckInItems` — a hopper with nothing in front still pulls from the
        // container above into its own five slots.
        set_block(1, hy, 0, 0);
        assert!(block_at(0, hy, 0) < 9230, "hopper unlocks when power leaves");
        set_block(0, hy, 0, 9229); // facing east, no container there
        hopper_tick();
        assert_eq!(stacks_at(HPOS), vec![(item, 2)]);
        assert_eq!(stacks_at((0, hy + 1, 0)), vec![(item, 2)]);
        restore_world(saved);
    }

    #[test]
    fn p6c_parity_hopper_sucks_drop_stack_into_own_slots() {
        let _guard = world_guard();
        let saved = save_world();
        begin_hopper_world();
        let hy = HY;
        let item = name_to_item("raw_iron").unwrap_or(1);
        set_block(0, hy, 0, 9229); // facing east into air, no container above
        drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
            eid: 9001,
            x: 0.5,
            y: hy as f64 + 1.0,
            z: 0.5,
            item,
            cnt: 10,
            comps: Vec::new(),
            age: 0,
            target: None,
            thrower: None,
        });
        hopper_tick();
        // Vanilla `Hopper.addItem`: the entity's whole stack fits the hopper's
        // five slots, so the entity leaves with nothing — not one item.
        assert_eq!(stacks_at(HPOS), vec![(item, 10)]);
        assert!(drops().lock().unwrap_or_else(|e| e.into_inner()).is_empty());

        // A stack that does not fit leaves the remainder on the entity.
        let slot_items = [1u8, 2, 3, 4];
        for (i, c) in slot_items.iter().enumerate() {
            put(HPOS, i as i16, (item, *c, Vec::new()));
        }
        drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
            eid: 9002,
            x: 0.5,
            y: hy as f64 + 1.0,
            z: 0.5,
            item,
            cnt: 64,
            comps: Vec::new(),
            age: 0,
            target: None,
            thrower: None,
        });
        for _ in 0..HOPPER_TRANSFER_COOLDOWN {
            TICK_IDX.fetch_add(1, Ordering::Relaxed);
            hopper_tick();
        }
        // `addItem` fills the lowest partial first: slot 0 tops to 64 (63
        // taken), the 64th item lands on slot 1 — all of it fits.
        assert_eq!(
            stacks_at(HPOS),
            vec![(item, 3), (item, 3), (item, 4), (item, 64)]
        );
        assert!(drops().lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        restore_world(saved);
    }

    #[test]
    fn p6c_parity_sleeping_engine_eager_pair_and_eight_tick_cooldown() {
        // The live path and the canonical engine must agree on the vanilla
        // shape: two items per activation, then eight ticks of cooldown.
        let mut engine = SleepingHopperEngine::default();
        let hopper = (DimId::Overworld, 0, 65, 0);
        let above = (DimId::Overworld, 0, 66, 0);
        let below = (DimId::Overworld, 0, 64, 0);
        engine.place_hopper(hopper, HopperFacing::Down);
        engine.set_container(above, vec![Some((99, 2, Vec::new())), None]);
        engine.set_container(below, vec![None; 27]);
        engine.hoppers.get_mut(&hopper).unwrap().slots[0] = Some((42, 1, Vec::new()));

        let stats = engine.tick();
        assert_eq!(
            stats.items_transferred, 2,
            "one push plus one pull in the same activation"
        );
        // Ticks 1..=7 move nothing — the cooldown is HOPPER_TRANSFER_COOLDOWN.
        for tick in 1..HOPPER_TRANSFER_COOLDOWN {
            let stats = engine.tick();
            assert_eq!(
                stats.items_transferred, 0,
                "cooldown tick {tick} must not transfer"
            );
        }
        let stats = engine.tick();
        // Push the pulled item down and pull the second item in: another
        // eager pair.
        assert_eq!(stats.items_transferred, 2, "activation after the cooldown");
        for _ in 0..HOPPER_TRANSFER_COOLDOWN - 1 {
            engine.tick();
        }
        // Third activation: the source is empty, so only the push moves.
        assert_eq!(engine.tick().items_transferred, 1);
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod p8b2_parity_component_wire_tests {
    use super::*;

    fn vi(number: i64) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_varint(&mut bytes, number);
        bytes
    }

    fn string(text: &str) -> Vec<u8> {
        [vi(text.len() as i64), text.as_bytes().to_vec()].concat()
    }

    fn named_text() -> Vec<u8> {
        vec![8, 0, 1, b'X'] // unnamed NBT string
    }

    fn compound() -> Vec<u8> {
        vec![10, 0] // unnamed empty NBT compound
    }

    fn all_v767_component_samples() -> Comps {
        let nested = slot_wire(&(6, 1, vec![(16, vi(4)), (SLOT_REMOVED_COMPONENTS, vi(3))]));
        let empty = vi(0);
        let holder_set = [vi(0), string("minecraft:stone")].concat();
        let block_predicate = [
            vec![1], holder_set.clone(), vec![1], vi(1), string("facing"),
            vec![1], string("north"), vec![0],
        ].concat();
        let enchantments = [vi(1), vi(1), vi(3), vec![1]].concat();
        let tool = [
            vi(1), holder_set, vec![1], 2.0f32.to_be_bytes().to_vec(),
            vec![1, 1], 1.0f32.to_be_bytes().to_vec(), vi(1),
        ].concat();
        let food = [
            vi(5), 0.6f32.to_be_bytes().to_vec(), vec![1],
            1.6f32.to_be_bytes().to_vec(), empty.clone(), vi(1), vi(2),
            0.4f32.to_be_bytes().to_vec(),
        ].concat();
        let potion = [
            vec![1], vi(2), vec![1], vec![0x00, 0x33, 0x55, 0x77], vi(1),
            vi(3), vi(1), vi(60), vec![0, 1, 1, 0], vec![1], string("custom"),
        ].concat();
        let trim_material = [
            string("minecraft:gold"), vi(1), vi(1), vi(2),
            string("minecraft:gold"), named_text(),
        ].concat();
        let trim_pattern = [
            string("minecraft:coast"), vi(2), named_text(), vec![1],
        ].concat();
        let trim = [
            vi(0), trim_material, vi(0), trim_pattern, vec![0],
        ].concat();
        let firework_explosion = [
            vi(2), vi(1), vec![1, 2, 3, 4], vi(1),
            vec![5, 6, 7, 8], vec![1, 0],
        ].concat();
        let sound_event = [string("minecraft:test"), vec![0]].concat();
        let song = [
            vi(0), sound_event.clone(), named_text(),
            30.0f32.to_be_bytes().to_vec(), vi(3),
        ].concat();
        vec![
            (0, compound()), // custom_data
            (1, vi(64)), // max_stack_size
            (2, vi(1561)), // max_damage
            (3, vi(23)), // damage
            (4, vec![1]), // unbreakable
            (5, named_text()), // custom_name
            (6, named_text()), // item_name
            (7, [vi(1), vec![0]].concat()), // lore, nullable text
            (8, vi(3)), // rarity
            (9, enchantments.clone()), // enchantments
            (10, [vi(1), block_predicate.clone(), vec![1]].concat()), // can_place_on
            (11, [vi(1), block_predicate, vec![0]].concat()), // can_break
            (12, [vi(0), vec![1]].concat()), // attribute_modifiers
            (13, vi(12)), // custom_model_data
            (14, vec![]), // hide_additional_tooltip
            (15, vec![]), // hide_tooltip
            (16, vi(7)), // repair_cost
            (17, vec![]), // creative_slot_lock
            (18, vec![1]), // enchantment_glint_override
            (19, compound()), // intangible_projectile
            (20, food), // food
            (21, vec![]), // fire_resistant
            (22, tool), // tool
            (23, enchantments), // stored_enchantments
            (24, vec![0x11, 0x22, 0x33, 0x44, 1]), // dyed_color
            (25, vec![0x11, 0x22, 0x33, 0x44]), // map_color
            (26, vi(42)), // map_id
            (27, compound()), // map_decorations
            (28, vi(1)), // map_post_processing
            (29, [vi(1), nested.clone()].concat()), // charged_projectiles
            (30, [vi(1), nested.clone()].concat()), // bundle_contents
            (31, potion), // potion_contents
            (32, [vi(1), vi(1), vi(300)].concat()), // suspicious_stew_effects
            (33, [vi(1), string("raw"), vec![1], string("filtered")].concat()), // writable_book
            (34, [
                string("title"), vec![1], string("filtered"), string("author"),
                vi(1), vi(1), named_text(), vec![0], vec![1],
            ].concat()), // written_book
            (35, trim), // trim with inline holders
            (36, compound()), // debug_stick_state
            (37, compound()), // entity_data
            (38, compound()), // bucket_entity_data
            (39, compound()), // block_entity_data
            (40, [vi(0), vi(0), sound_event, 1.0f32.to_be_bytes().to_vec(),
                  16.0f32.to_be_bytes().to_vec(), named_text()].concat()), // instrument
            (41, vi(3)), // ominous_bottle_amplifier
            (42, [vec![1], vi(0), song, vec![1]].concat()), // jukebox_playable
            (43, compound()), // recipes
            (44, [vec![1], string("minecraft:overworld"), vec![0; 8], vec![1]].concat()), // lodestone
            (45, firework_explosion.clone()), // firework_explosion
            (46, [vi(2), vi(1), firework_explosion].concat()), // fireworks
            (47, [vec![1], string("player"), vec![0], vi(0)].concat()), // profile
            (48, string("minecraft:test")), // note_block_sound
            (49, [vi(1), vi(0), string("minecraft:stripe"), string("stripe"), vi(5)].concat()), // banners
            (50, vi(5)), // base_color
            (51, [vi(4), vi(1), vi(2), vi(3), vi(4)].concat()), // pot_decorations
            (52, [vi(1), nested].concat()), // container
            (53, [vi(1), string("facing"), string("north")].concat()), // block_state
            (54, [vi(1), compound(), vi(42), vi(20)].concat()), // bees
            (55, compound()), // lock
            (56, compound()), // container_loot
        ]
    }

    #[test]
    fn p8b2_parity_all_57_components_and_nested_slots_roundtrip_losslessly() {
        let components = all_v767_component_samples();
        assert!(include_str!("../proto/src/data_v767.rs")
            .contains("pub(crate) static V767_COMPONENTS: [&str; 57]"));
        assert_eq!(components.len(), SLOT_COMPONENT_TYPE_COUNT as usize);
        for (expected_id, (id, _)) in components.iter().enumerate() {
            assert_eq!(*id, expected_id as i64);
        }
        let invalid: Vec<_> = components.iter().filter_map(|(id, bytes)| {
            let mut position = 0;
            let result = slot_read_component(bytes, &mut position, *id, 0);
            (result.is_none() || position != bytes.len())
                .then_some((*id, position, bytes.len()))
        }).collect();
        assert!(invalid.is_empty(), "invalid component boundaries: {invalid:?}");
        let stack: Stack = (6, 1, components);
        let mut packet = 12i16.to_be_bytes().to_vec();
        packet.extend_from_slice(&slot_wire(&stack));
        let (slot, decoded) = slot_read_wire(&packet).expect("all component boundaries decode");
        assert_eq!(slot, 12);
        assert_eq!(decoded, stack);
        assert_eq!(slot_wire(&decoded), slot_wire(&stack));
    }

    #[test]
    fn p8b2_parity_creative_removed_and_noncanonical_component_payloads() {
        let original: Stack = (
            4, 3,
            vec![
                (COMP_CUSTOM_NAME, named_text()),
                (16, vec![0x80, 0x00]), // repair_cost
                (SLOT_REMOVED_COMPONENTS, [vi(3), vi(4), vi(12)].concat()),
            ],
        );
        let mut packet = 45i16.to_be_bytes().to_vec();
        packet.extend_from_slice(&slot_wire(&original));
        assert_eq!(slot_read_wire(&packet), Some((45, original)));
        assert!(creative_inventory_slot(45));
        packet.push(1);
        assert!(slot_read_wire(&packet).is_none(), "trailing data must be rejected");
    }

    #[test]
    fn p8b2_parity_truncated_and_out_of_range_components_are_rejected() {
        let stack: Stack = (7, 1, all_v767_component_samples());
        let mut packet = 10i16.to_be_bytes().to_vec();
        packet.extend_from_slice(&slot_wire(&stack));
        for end in 0..packet.len() {
            assert!(slot_read_wire(&packet[..end]).is_none(), "truncated at byte {end}");
        }
        let mut invalid = 10i16.to_be_bytes().to_vec();
        invalid.extend_from_slice(&[1, 1, 1, 0]);
        write_varint(&mut invalid, SLOT_COMPONENT_TYPE_COUNT);
        assert!(slot_read_wire(&invalid).is_none());
    }
}

#[cfg(test)]
mod w2_live_wire_tests {
    use super::*;

    /// Saves and restores the datapack registry, legacy tag/recipe globals and
    /// the live block-entity store around a live-wiring test.
    struct DatapackWorld {
        registry: DatapackRegistry,
        items: HashMap<String, Vec<String>>,
        recipes: Vec<JsonRecipe>,
        smelts: Vec<SmeltRecipe>,
        entities: LiveBlockEntityStore,
    }

    impl DatapackWorld {
        fn new() -> Self {
            Self {
                registry: std::mem::take(
                    &mut *datapack_registry().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                items: std::mem::take(
                    &mut *item_tags().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                recipes: std::mem::take(
                    &mut *json_recipes().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                smelts: std::mem::take(
                    &mut *json_smelts().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                entities: std::mem::take(
                    &mut *live_block_entities().lock().unwrap_or_else(|e| e.into_inner()),
                ),
            }
        }

        fn set_item_tag(&mut self, tag: &str, members: &[&str]) {
            self.registry_mut()
                .tags
                .entry("item".to_owned())
                .or_default()
                .insert(tag.to_owned(), members.iter().map(|m| m.to_string()).collect());
        }

        fn registry_mut(&mut self) -> std::sync::MutexGuard<'_, DatapackRegistry> {
            datapack_registry().lock().unwrap_or_else(|e| e.into_inner())
        }

        fn recipes_mut(&mut self) -> std::sync::MutexGuard<'_, Vec<JsonRecipe>> {
            json_recipes().lock().unwrap_or_else(|e| e.into_inner())
        }

        fn smelts_mut(&mut self) -> std::sync::MutexGuard<'_, Vec<SmeltRecipe>> {
            json_smelts().lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    impl std::ops::Drop for DatapackWorld {
        fn drop(&mut self) {
            *datapack_registry().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.registry);
            *item_tags().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.items);
            *json_recipes().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.recipes);
            *json_smelts().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.smelts);
            *live_block_entities().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.entities);
        }
    }

    #[test]
    fn w2_live_wire_datapack_stonecutting_with_tag_ingredient_crafts() {
        let _guard = world_guard();
        let mut world = DatapackWorld::new();
        let Some(stone) = name_to_item("stone") else {
            return;
        };
        let Some(glass) = name_to_item("glass") else {
            return;
        };
        // A nested `#ref` ingredient proves the `item` tag expansion runs.
        world.set_item_tag("test:w2_cut", &["#test:w2_cut_inner"]);
        world.set_item_tag("test:w2_cut_inner", &["minecraft:stone"]);
        world
            .registry_mut()
            .stonecutting
            .push(StonecuttingRecipe {
                ingredients: vec![Ingr::Tag("test:w2_cut".to_owned())],
                result: glass,
                count: 3,
            });

        let mut menu = StonecutterMenu::default();
        menu.set_input((stone, 8, Vec::new()));
        let recipes = live_stonecutter_recipes(&menu.input);
        assert_eq!(recipes.first(), Some(&(glass, 3)), "recipes: {recipes:?}");
        // The builtin table still answers for inputs the pack also covers.
        assert!(recipes.len() > 1, "builtin stonecut recipes remain offered");
        assert!(live_stonecutter_select(&mut menu, 0));
        assert_eq!(live_stonecutter_result(&menu), (glass, 3, Vec::new()));
        assert_eq!(
            live_stonecutter_take(&mut menu),
            Some((glass, 3, Vec::new()))
        );
        assert_eq!((menu.input.0, menu.input.1), (stone, 7));
        // An input the datapack does not cover keeps the vanilla fallback.
        assert!(live_dp_stonecut_recipes(name_to_item("cobblestone").unwrap_or(-1)).is_empty());
    }

    #[test]
    fn w2_live_wire_datapack_smithing_transform_produces_result() {
        let _guard = world_guard();
        let mut world = DatapackWorld::new();
        let (Some(template), Some(base), Some(addition), Some(result)) = (
            name_to_item("netherite_upgrade_smithing_template"),
            name_to_item("iron_sword"),
            name_to_item("stick"),
            name_to_item("netherite_sword"),
        ) else {
            return;
        };
        // A tag base proves tag ingredients resolve inside the live menu.
        world.set_item_tag("test:w2_upgradable", &["minecraft:iron_sword"]);
        world
            .registry_mut()
            .smithing_transform
            .push(SmithingRecipe {
                template: vec![Ingr::Item(template)],
                base: vec![Ingr::Tag("test:w2_upgradable".to_owned())],
                addition: vec![Ingr::Item(addition)],
                result: Some(result),
            });

        let mut damaged: Stack = (base, 1, Vec::new());
        comp_set_varint(&mut damaged.2, COMP_DAMAGE, 12);
        let mut menu = SmithingMenu {
            template: (template, 1, Vec::new()),
            base: damaged.clone(),
            addition: (addition, 1, Vec::new()),
        };
        // Vanilla `transmuteCopy`: the result item keeps the base's components.
        assert_eq!(
            live_smithing_result(&menu.template, &menu.base, &menu.addition),
            Some((result, 1, damaged.2.clone()))
        );
        // A stick is no trim material, so the builtin slot-2 rule rejects it;
        // the loaded recipe's addition set admits it (vanilla `mayPlace`).
        assert_eq!(
            live_input_limit(
                &LiveMenu::Smithing(SmithingMenu::default()),
                2,
                &(addition, 1, Vec::new())
            ),
            64
        );
        assert_eq!(live_smithing_take(&mut menu, |_| true), Some((result, 1, damaged.2)));
        assert!(menu.template.1 == 0 && menu.base.1 == 0 && menu.addition.1 == 0);
    }

    #[test]
    fn w2_live_wire_datapack_smithing_trim_applies_armor_trim() {
        let _guard = world_guard();
        let mut world = DatapackWorld::new();
        let (Some(template), Some(base), Some(addition)) = (
            name_to_item("bolt_armor_trim_smithing_template"),
            name_to_item("iron_chestplate"),
            name_to_item("redstone"),
        ) else {
            return;
        };
        // The vanilla tag spellings resolve through the fallback memberships.
        world.registry_mut().smithing_trim.push(SmithingRecipe {
            template: vec![Ingr::Item(template)],
            base: vec![Ingr::Tag("minecraft:trimmable_armor".to_owned())],
            addition: vec![Ingr::Tag("minecraft:trim_materials".to_owned())],
            result: None,
        });

        let mut menu = SmithingMenu {
            template: (template, 1, Vec::new()),
            base: (base, 1, Vec::new()),
            addition: (addition, 1, Vec::new()),
        };
        let result =
            live_smithing_result(&menu.template, &menu.base, &menu.addition).expect("trim output");
        assert_eq!(result.0, base);
        // bolt is TRIM_PATTERNS[0], redstone is TRIM_MATERIAL_ITEMS[9].
        assert_eq!(comp_get(&result.2, COMP_TRIM), Some([10, 1, 1].as_slice()));
        assert_eq!(live_smithing_take(&mut menu, |_| true), Some(result.clone()));
        // The same trim applied twice refuses, like vanilla's trim recipe.
        assert_eq!(
            live_smithing_result(
                &(template, 1, Vec::new()),
                &result,
                &(addition, 1, Vec::new())
            ),
            None
        );
    }

    #[test]
    fn w2_live_wire_datapack_item_tags_match_crafting_and_smelting() {
        let _guard = world_guard();
        let mut world = DatapackWorld::new();
        let (Some(coal), Some(result), Some(gold)) = (
            name_to_item("coal"),
            name_to_item("torch"),
            name_to_item("gold_ingot"),
        ) else {
            return;
        };
        world.set_item_tag("test:w2_ing", &["#test:w2_ing_inner"]);
        world.set_item_tag("test:w2_ing_inner", &["minecraft:coal"]);
        assert!(ingr_match(&Ingr::Tag("test:w2_ing".to_owned()), coal, 0));
        assert!(!ingr_match(&Ingr::Tag("test:w2_ing".to_owned()), gold, 0));

        world.recipes_mut().push(JsonRecipe {
            shaped: false,
            rows: Vec::new(),
            key: HashMap::new(),
            ing: vec![Ingr::Tag("test:w2_ing".to_owned())],
            out: result,
            out_cnt: 4,
        });
        let grid: HashMap<i16, Stack> = HashMap::from([(1i16, (coal, 1, Vec::new()))]);
        assert_eq!(json_craft_match(&grid, 2), Some((result, 4, Vec::new())));

        world
            .smelts_mut()
            .push((Ingr::Tag("test:w2_ing".to_owned()), gold, 2, 1.5));
        assert_eq!(smelt_out(coal), Some((gold, 2)));
        assert_eq!(smelt_xp(coal), 1.5);
    }

    #[test]
    fn w2_live_wire_comparator_reads_world_be_and_live_stores() {
        let _guard = world_guard();
        let _world = DatapackWorld::new();
        let pos = (4_500_011, 120, 4_500_011);
        let (x, y, z) = pos;

        // W-BE chiseled bookshelf: last interacted slot + 1.
        let mut shelf = ChiseledBookshelfBlockEntity::default();
        assert!(shelf.insert_book(4, "minecraft:book"));
        register_world_block_entity(
            DimId::Overworld,
            pos,
            VanillaBlockEntity::ChiseledBookshelf(shelf),
        );
        assert_eq!(live_comparator_signal_at(DimId::Overworld, x, y, z), Some(5));

        // W-BE jukebox: the song registry's comparator_output (cat = 2).
        register_world_block_entity(
            DimId::Overworld,
            (x + 1, y, z),
            VanillaBlockEntity::Jukebox(JukeboxBlockEntity {
                record_item: Some("minecraft:music_disc_cat".to_owned()),
                ..Default::default()
            }),
        );
        assert_eq!(live_comparator_signal_at(DimId::Overworld, x + 1, y, z), Some(2));

        // W-BE lectern: floor(page / (pages - 1) * 14) + 1 = 8 at page 7 of 15.
        register_world_block_entity(
            DimId::Overworld,
            (x, y, z + 1),
            VanillaBlockEntity::Lectern(LecternBlockEntity {
                book_item: Some("minecraft:written_book".to_owned()),
                page: 7,
                total_pages: 15,
            }),
        );
        assert_eq!(live_comparator_signal_at(DimId::Overworld, x, y, z + 1), Some(8));

        // W-BE spawner: stored but has no analog output.
        register_world_block_entity(
            DimId::Overworld,
            (x + 1, y, z + 1),
            VanillaBlockEntity::MobSpawner(MobSpawnerBlockEntity::default()),
        );
        assert_eq!(live_comparator_signal_at(DimId::Overworld, x + 1, y, z + 1), Some(0));

        // The live block-entity store is consulted when no W-BE entity exists.
        let mut live_shelf = ChiseledBookshelf::default();
        let mut book = BlockEntityItem::new("minecraft:book", 1);
        assert!(live_shelf.insert_book(2, &mut book));
        live_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert_entry(
                (DimId::Overworld, x + 2, y, z),
                (0, LiveBlockEntity::ChiseledBookshelf(live_shelf)),
            );
        assert_eq!(live_comparator_signal_at(DimId::Overworld, x + 2, y, z), Some(3));

        // Cleanup: absent positions read nothing.
        remove_world_block_entity(DimId::Overworld, pos);
        assert_eq!(live_comparator_signal_at(DimId::Overworld, x, y, z), None);
    }
}

#[cfg(test)]
mod qol_death_restore_tests {
    use super::*;
    use crate::effect_tests::client_pair;

    fn item(name: &str) -> i64 {
        name_to_item(name).unwrap()
    }
    fn stack(item: i64, cnt: u8) -> Stack {
        (item, cnt, Vec::new())
    }
    /// The multiset the conservation proof quantifies over: `inv` slots plus
    /// whatever is left on the ground, keyed by item id AND components so a
    /// component-silent overwrite counts as loss.
    fn multiset(inv: &HashMap<i16, Stack>, ground: &[Stack]) -> HashMap<(i64, Comps), u64> {
        let mut m: HashMap<(i64, Comps), u64> = HashMap::new();
        for st in inv.values().chain(ground.iter()) {
            if st.0 != 0 && st.1 > 0 {
                *m.entry((st.0, st.2.clone())).or_default() += u64::from(st.1);
            }
        }
        m
    }
    /// multiset(inv_before) + multiset(picked) == multiset(inv_after) + multiset(remaining).
    fn assert_conserved(
        before: &HashMap<i16, Stack>,
        picked: &Stack,
        after: &HashMap<i16, Stack>,
        remaining: Option<&Stack>,
    ) {
        let lhs = multiset(before, std::slice::from_ref(picked));
        let rhs = multiset(after, remaining.map_or(&[][..], std::slice::from_ref));
        assert_eq!(lhs, rhs, "item multiset not conserved: {before:?} + {picked:?} -> {after:?} + {remaining:?}");
    }
    /// Run the restore and assert the conservation invariant on this call.
    fn run_restore(inv: &mut HashMap<i16, Stack>, picked: Stack, slot: i16) -> Option<Stack> {
        let before = inv.clone();
        let rem = insert_with_death_slot_restore(inv, picked.clone(), slot);
        assert_conserved(&before, &picked, inv, rem.as_ref());
        rem
    }
    /// Save/restore for the shared fixtures the live `drop_tick` reads.
    struct DeathDropWorld {
        clients: Vec<Arc<Client>>,
        drops: Vec<Drop>,
        hints: DeathSlotHintTable,
        region: RegionSets,
    }
    impl DeathDropWorld {
        fn take() -> Self {
            Self {
                clients: std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner())),
                drops: std::mem::take(&mut *drops().lock().unwrap_or_else(|e| e.into_inner())),
                hints: std::mem::replace(
                    &mut *death_slot_hints().lock().unwrap_or_else(|e| e.into_inner()),
                    DeathSlotHintTable {
                        map: HashMap::new(),
                        order: std::collections::VecDeque::new(),
                    },
                ),
                region: test_region(),
            }
        }
    }
    impl std::ops::Drop for DeathDropWorld {
        fn drop(&mut self) {
            *clients().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.clients);
            *drops().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.drops);
            *death_slot_hints().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::replace(
                    &mut self.hints,
                    DeathSlotHintTable {
                        map: HashMap::new(),
                        order: std::collections::VecDeque::new(),
                    },
                );
            set_test_region(self.region.0.clone(), self.region.1.clone());
            DEATH_SLOT_RESTORE.store(false, Ordering::Relaxed);
        }
    }
    /// A live client standing on a drop at the origin of a hot region.
    fn live_client(name: &str) -> Arc<Client> {
        let (mut client, _receiver) = client_pair();
        client.name = name.to_owned();
        let client = Arc::new(client);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(client.clone());
        client
    }
    fn push_death_drop(eid: i32, item: i64, cnt: u8, owner: &str, slot: i16) {
        drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
            eid,
            x: 0.0,
            y: 70.0,
            z: 0.0,
            item,
            cnt,
            comps: vec![],
            age: 100,
            target: None,
            thrower: None,
        });
        death_slot_hint_record(eid, owner, slot);
    }

    /// Case A over a whole kit: every stack lands back in the exact slot it
    /// was scattered from — hotbar, the four armor slots, the offhand and the
    /// main grid — with nothing left on the ground.
    #[test]
    fn qol_death_restore_empty_inventory_returns_full_kit_to_original_slots() {
        let sword = item("diamond_sword");
        let pickaxe = item("diamond_pickaxe");
        let helmet = item("diamond_helmet");
        let chestplate = item("diamond_chestplate");
        let leggings = item("diamond_leggings");
        let boots = item("diamond_boots");
        let shield = item("shield");
        let dirt = item("dirt");
        let cobble = item("cobblestone");
        let kit: [(i16, Stack); 9] = [
            (36, stack(sword, 1)),
            (37, stack(pickaxe, 1)),
            (5, stack(helmet, 1)),
            (6, stack(chestplate, 1)),
            (7, stack(leggings, 1)),
            (8, stack(boots, 1)),
            (45, stack(shield, 1)),
            (9, stack(dirt, 64)),
            (20, stack(cobble, 48)),
        ];
        let mut inv = HashMap::new();
        for (slot, st) in &kit {
            assert_eq!(run_restore(&mut inv, st.clone(), *slot), None, "slot {slot} rejected its item");
            assert_eq!(inv.get(slot), Some(st), "slot {slot} does not hold its own item");
        }
        assert_eq!(inv.len(), kit.len());
    }

    /// Case C on a hotbar slot: the cobblestone picked up on the way to the
    /// death spot relocates to the first free storage slot, the sword takes
    /// slot 36 back, and the total multiset is conserved.
    #[test]
    fn qol_death_restore_occupied_hotbar_relocates_collider_and_conserves() {
        let sword = item("diamond_sword");
        let cobble = item("cobblestone");
        let mut inv = HashMap::from([(36i16, stack(cobble, 64)), (9, stack(cobble, 10))]);
        assert_eq!(run_restore(&mut inv, stack(sword, 1), 36), None);
        assert_eq!(inv.get(&36), Some(&stack(sword, 1)));
        // Hotbar-first order: 37 is the first free storage slot.
        assert_eq!(inv.get(&37), Some(&stack(cobble, 64)));
        assert_eq!(inv.get(&9), Some(&stack(cobble, 10)));
        assert_eq!(inv.len(), 3);
    }

    /// Case C on an armor slot: the leather boots the player found on the way
    /// relocate into a free storage slot (never another armor slot) and the
    /// netherite boots equip into slot 8.
    #[test]
    fn qol_death_restore_occupied_armor_slot_relocates_and_equips() {
        let netherite = item("netherite_boots");
        let leather = item("leather_boots");
        let dirt = item("dirt");
        let mut inv = HashMap::from([(8i16, stack(leather, 1)), (36, stack(dirt, 64))]);
        assert_eq!(run_restore(&mut inv, stack(netherite, 1), 8), None);
        assert_eq!(inv.get(&8), Some(&stack(netherite, 1)));
        assert_eq!(inv.get(&37), Some(&stack(leather, 1)));
        assert_eq!(inv.get(&36), Some(&stack(dirt, 64)));
        assert_eq!(inv.len(), 3);
    }

    /// The same collision on the offhand: whatever the player currently holds
    /// there relocates to storage so the dropped shield comes home.
    #[test]
    fn qol_death_restore_occupied_offhand_relocates_and_conserves() {
        let shield = item("shield");
        let torch = item("torch");
        let mut inv = HashMap::from([(45i16, stack(torch, 20))]);
        assert_eq!(run_restore(&mut inv, stack(shield, 1), 45), None);
        assert_eq!(inv.get(&45), Some(&stack(shield, 1)));
        assert_eq!(inv.get(&36), Some(&stack(torch, 20)));
        assert_eq!(inv.len(), 2);
    }

    /// Case B: the original slot already holds the same stack — merge up to
    /// the stack limit, then the overflow lands in the first free slot.
    #[test]
    fn qol_death_restore_merge_fills_original_slot_then_overflows() {
        let dirt = item("dirt");
        let mut inv = HashMap::from([(36i16, stack(dirt, 60))]);
        assert_eq!(run_restore(&mut inv, stack(dirt, 10), 36), None);
        assert_eq!(inv.get(&36), Some(&stack(dirt, 64)));
        assert_eq!(inv.get(&37), Some(&stack(dirt, 6)));
        assert_eq!(inv.len(), 2);
    }

    /// Case B against an unstackable armor slot: same item cannot merge past
    /// one, so the pickup inserts like a normal stack elsewhere.
    #[test]
    fn qol_death_restore_unstackable_collision_with_self_inserts_elsewhere() {
        let helmet = item("iron_helmet");
        let mut inv = HashMap::from([(5i16, stack(helmet, 1))]);
        assert_eq!(run_restore(&mut inv, stack(helmet, 1), 5), None);
        assert_eq!(inv.get(&5), Some(&stack(helmet, 1)));
        assert_eq!(inv.get(&36), Some(&stack(helmet, 1)));
        assert_eq!(inv.len(), 2);
    }

    /// A completely full storage grid gives Case C nowhere to relocate and the
    /// merge fallback nowhere to go: the whole stack stays on the ground and
    /// not one of the 36 resident stacks is overwritten.
    #[test]
    fn qol_death_restore_full_inventory_loses_nothing_and_overwrites_nothing() {
        let dirt = item("dirt");
        let sword = item("diamond_sword");
        let netherite = item("netherite_boots");
        let leather = item("leather_boots");
        let mut inv: HashMap<i16, Stack> = (9..=44).map(|s| (s, stack(dirt, 64))).collect();
        inv.insert(8, stack(leather, 1));
        let before = inv.clone();
        assert_eq!(
            run_restore(&mut inv, stack(sword, 1), 36),
            Some(stack(sword, 1)),
            "the whole remainder must go back to the ground"
        );
        assert_eq!(inv, before, "a full inventory must not lose or overwrite a single item");
        // An armor collision on a full grid resolves the same way: the worn
        // boots keep slot 8 and the picked pair goes back to the ground whole.
        assert_eq!(
            run_restore(&mut inv, stack(netherite, 1), 8),
            Some(stack(netherite, 1))
        );
        assert_eq!(inv, before);
    }

    /// Full except for one mergeable partial: the picked stack merges as far
    /// as it can and only the true remainder stays on the ground.
    #[test]
    fn qol_death_restore_full_inventory_partial_merge_keeps_exact_remainder() {
        let dirt = item("dirt");
        let mut inv: HashMap<i16, Stack> = (9..=44).map(|s| (s, stack(dirt, 64))).collect();
        inv.insert(20, stack(dirt, 63));
        assert_eq!(
            run_restore(&mut inv, stack(dirt, 10), 20),
            Some(stack(dirt, 9)),
            "one fits, nine stay on the ground"
        );
        assert_eq!(inv.get(&20), Some(&stack(dirt, 64)));
    }

    /// Case D: a hint that names a crafting-grid index, an out-of-range index
    /// or the wrong armor slot falls back to the normal insert (hotbar first).
    #[test]
    fn qol_death_restore_bad_slot_and_wrong_armor_fall_back_to_normal_insert() {
        let helmet = item("diamond_helmet");
        let dirt = item("dirt");
        for slot in [-1i16, 0, 2, 4, 46, 100] {
            let mut inv = HashMap::new();
            assert_eq!(run_restore(&mut inv, stack(dirt, 5), slot), None);
            assert_eq!(inv, HashMap::from([(36i16, stack(dirt, 5))]), "slot {slot}");
        }
        // A helmet hinted for the boots slot is not a legal target either.
        let mut inv = HashMap::new();
        assert_eq!(run_restore(&mut inv, stack(helmet, 1), 8), None);
        assert_eq!(inv, HashMap::from([(36i16, stack(helmet, 1))]));
        // ...while it is legal for its own slot.
        let mut inv = HashMap::new();
        assert_eq!(run_restore(&mut inv, stack(helmet, 1), 5), None);
        assert_eq!(inv, HashMap::from([(5i16, stack(helmet, 1))]));
    }

    /// The gating: off by default, owner-name match required, and the table
    /// forgets a drop its entity is done with.
    #[test]
    fn qol_death_restore_hint_requires_opt_in_and_owner_name() {
        let _guard = world_guard();
        let _snapshot = DeathDropWorld::take();
        death_slot_hint_record(7001, "alice", 36);
        // Disabled by default: even the owner gets the vanilla answer.
        assert!(!death_slot_restore_enabled());
        assert_eq!(death_slot_hint_for(7001, "alice"), None);
        set_death_slot_restore(true);
        assert_eq!(death_slot_hint_for(7001, "alice"), Some(36));
        // A foreign player is not the owner: no hint, normal pickup.
        assert_eq!(death_slot_hint_for(7001, "bob"), None);
        assert_eq!(death_slot_hint_for(9999, "alice"), None);
        death_slot_hint_forget(7001);
        assert_eq!(death_slot_hint_for(7001, "alice"), None);
    }

    /// The table is bounded at MAX_DEATH_SLOT_HINTS and evicts oldest-first.
    #[test]
    fn qol_death_restore_hint_table_stays_bounded() {
        let _guard = world_guard();
        let _snapshot = DeathDropWorld::take();
        let base = 5_000_000i32;
        for i in 0..(MAX_DEATH_SLOT_HINTS as i32 + 8) {
            death_slot_hint_record(base + i, "cap-owner", 9);
        }
        let hints = death_slot_hints().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(hints.map.len(), MAX_DEATH_SLOT_HINTS);
        assert_eq!(hints.order.len(), MAX_DEATH_SLOT_HINTS);
        assert!(!hints.map.contains_key(&base), "oldest hint must evict first");
        assert!(hints.map.contains_key(&(base + MAX_DEATH_SLOT_HINTS as i32 + 7)));
    }

    /// Live path, enabled: Alice's death-scattered sword comes back to slot 36
    /// through the real `drop_tick`, pushing the cobblestone she picked up on
    /// the way out of the slot. The hint is forgotten with the entity.
    #[test]
    fn qol_death_restore_drop_tick_restores_original_slot_for_owner() {
        let _guard = world_guard();
        let _snapshot = DeathDropWorld::take();
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        set_death_slot_restore(true);
        let sword = item("diamond_sword");
        let cobble = item("cobblestone");
        let client = live_client("alice");
        client
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(36, stack(cobble, 32));
        // The death-path entry point: a real entity plus its slot hint. Age it
        // past the pickup delay the way 30+ live ticks of `drop_tick` would.
        let deid = spawn_death_drop((0.0, 70.0, 0.0), stack(sword, 1), "alice", 36);
        {
            let mut ds = drops().lock().unwrap_or_else(|e| e.into_inner());
            ds.iter_mut().find(|d| d.eid == deid).unwrap().age = 100;
        }
        drop_tick();
        let inv = client.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(inv.get(&36), Some(&stack(sword, 1)), "sword did not come home");
        assert_eq!(inv.get(&37), Some(&stack(cobble, 32)), "collider was lost or overwritten");
        assert!(drops().lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        assert!(death_slot_hints().lock().unwrap_or_else(|e| e.into_inner()).map.is_empty());
    }

    /// Live path, enabled: armor slots resync and armor points follow an
    /// equipped piece, not just the storage grid.
    #[test]
    fn qol_death_restore_drop_tick_equips_armor_and_updates_armor_points() {
        let _guard = world_guard();
        let _snapshot = DeathDropWorld::take();
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        set_death_slot_restore(true);
        let netherite = item("netherite_boots");
        let client = live_client("alice");
        push_death_drop(7778, netherite, 1, "alice", 8);
        drop_tick();
        let inv = client.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(inv.get(&8), Some(&stack(netherite, 1)));
        assert_eq!(*client.armor_pts.lock().unwrap_or_else(|e| e.into_inner()), armor_pts_of(&inv));
        assert_eq!(*client.armor_pts.lock().unwrap_or_else(|e| e.into_inner()), 3);
    }

    /// Live path, disabled: the identical drop with the identical hint takes
    /// the vanilla first-free-slot landing (36), not the hinted slot 20.
    #[test]
    fn qol_death_restore_disabled_flag_leaves_vanilla_pickup_unchanged() {
        let _guard = world_guard();
        let _snapshot = DeathDropWorld::take();
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        let sword = item("diamond_sword");
        let client = live_client("alice");
        push_death_drop(7779, sword, 1, "alice", 20);
        assert!(!death_slot_restore_enabled());
        drop_tick();
        let inv = client.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(inv, HashMap::from([(36i16, stack(sword, 1))]), "hint applied while disabled");
    }

    /// Live path, enabled: Bob picking up Alice's death drop is a normal
    /// pickup — the hint only answers to the owner name.
    #[test]
    fn qol_death_restore_foreign_player_pickup_ignores_the_owner_hint() {
        let _guard = world_guard();
        let _snapshot = DeathDropWorld::take();
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        set_death_slot_restore(true);
        let sword = item("diamond_sword");
        let client = live_client("bob");
        push_death_drop(7780, sword, 1, "alice", 20);
        drop_tick();
        let inv = client.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(inv, HashMap::from([(36i16, stack(sword, 1))]), "foreign pickup used the hint");
        // The hint outlives the pickup attempt: the entity is gone but the
        // table retired it with the drop.
        assert!(death_slot_hints().lock().unwrap_or_else(|e| e.into_inner()).map.is_empty());
    }

    /// 500 randomized steps: every single restore call must satisfy the exact
    /// conservation invariant — inv_before + picked == inv_after + ground —
    /// counting components as part of the item identity. Inventories are
    /// perturbed between steps to manufacture empty/merge/collision/full and
    /// invalid-slot cases in every proportion.
    #[test]
    fn qol_death_restore_stress_500_steps_conserves_every_multiset() {
        let dirt = item("dirt");
        let cobble = item("cobblestone");
        let apple = item("apple");
        let sword = item("iron_sword");
        let helmet = item("iron_helmet");
        let chest = item("iron_chestplate");
        let boots = item("iron_boots");
        let shield = item("shield");
        let pool = [dirt, cobble, apple, sword, helmet, chest, boots, shield];
        let mut rng = 0x9E3779B97F4A7C15u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut picked_id = 0i64;
        let mut gen_picked = |next: &mut dyn FnMut() -> u64| -> Stack {
            let it = pool[(next() % pool.len() as u64) as usize];
            let mut comps: Comps = Vec::new();
            // One in four stacks carries a damage component so merges have to
            // respect component identity, not just item id.
            if next().is_multiple_of(4) {
                comp_set_varint(&mut comps, COMP_DAMAGE, (next() % 8) as i64);
            }
            if next().is_multiple_of(16) {
                comps.push((COMP_CUSTOM_NAME, nbt_string(&format!("stress-{picked_id}"))));
            }
            picked_id += 1;
            let max = item_stack_max(it, &comps).clamp(1, 64) as u64;
            (it, (1 + next() % max).min(u64::from(u8::MAX)) as u8, comps)
        };
        let mut inv: HashMap<i16, Stack> = HashMap::new();
        let mut ground: Vec<Stack> = Vec::new();
        let mut steps = 0;
        while steps < 500 {
            steps += 1;
            // Perturb: fill a random legal slot (collision fodder) or clear one.
            match next() % 5 {
                0 | 1 => {
                    let slot = [5i16, 6, 7, 8, 45, 9, 10, 36, 37, 44]
                        [(next() % 10) as usize];
                    inv.insert(slot, gen_picked(&mut next));
                }
                2 => {
                    let slot = 5 + (next() % 41) as i16;
                    inv.remove(&slot);
                }
                _ => {}
            }
            // Slots biased toward legal targets so collisions are frequent but
            // out-of-range and crafting-grid indices still appear.
            let slot = match next() % 8 {
                0 => -1 + (next() % 8) as i16,            // -1..=6: invalid/craft/armor edge
                1 => 46 + (next() % 4) as i16,            // 46..=49: out of range
                2 => 5 + (next() % 4) as i16,             // armor
                3 => 45,                                  // offhand
                _ => 9 + (next() % 36) as i16,            // storage
            };
            let picked = gen_picked(&mut next);
            let rem = run_restore(&mut inv, picked, slot);
            if let Some(rem) = rem {
                ground.push(rem);
            }
            // Every resident stack respects its own stack limit.
            for (s, st) in &inv {
                assert!(
                    i64::from(st.1) <= stack_max_of(st),
                    "slot {s} over its stack limit: {st:?}"
                );
            }
        }
        // Sanity: the run actually produced the collision cases it exists to
        // cover, not an empty inventory walk-through.
        assert!(!inv.is_empty());
        assert!(ground.iter().any(|st| st.1 > 0) || inv.len() >= 36);
    }
}

#[cfg(test)]
mod bot_hopper_tests {
    use super::*;

    const BHY: i32 = 140;

    /// Save/restore for every shared fixture the live hopper and drop paths
    /// read — including the sleep cache and the drop-cell index this lane adds.
    struct SavedWorld {
        hoppers: HashSet<(i32, i32, i32)>,
        chests: HashMap<ContainerKey, HashMap<i16, Stack>>,
        drops: Vec<Drop>,
        forced: HashSet<(i32, i32)>,
        backoff: HashMap<DimBlockPos, (u64, u32)>,
        sleep: HashMap<(i32, i32, i32), HopperSleep>,
        cells: DropCells,
        tick: u64,
        index: u64,
        hot: HashSet<(i32, i32)>,
        warm: HashSet<(i32, i32)>,
        runs: u64,
        skips: u64,
        placed: Vec<(i32, i32, i32)>,
    }

    fn save_world() -> SavedWorld {
        SavedWorld {
            hoppers: std::mem::take(&mut *hoppers().lock().unwrap_or_else(|e| e.into_inner())),
            chests: std::mem::take(&mut *chests().lock().unwrap_or_else(|e| e.into_inner())),
            drops: std::mem::take(&mut *drops().lock().unwrap_or_else(|e| e.into_inner())),
            forced: std::mem::take(&mut *forceloaded().lock().unwrap_or_else(|e| e.into_inner())),
            backoff: std::mem::take(&mut *backoff().lock().unwrap_or_else(|e| e.into_inner())),
            sleep: std::mem::take(&mut *hopper_sleep().lock().unwrap_or_else(|e| e.into_inner())),
            cells: std::mem::replace(
                &mut *drop_cells().lock().unwrap_or_else(|e| e.into_inner()),
                DropCells {
                    len: usize::MAX,
                    mark: 0,
                    first_eid: 0,
                    last_eid: 0,
                    built_tick: u64::MAX,
                    map: HashMap::new(),
                },
            ),
            tick: TICK.swap(1, Ordering::Relaxed),
            index: TICK_IDX.swap(0, Ordering::Relaxed),
            hot: test_region().0,
            warm: test_region().1,
            runs: HOPPER_SCAN_RUNS.swap(0, Ordering::Relaxed),
            skips: HOPPER_SCAN_SKIPS.swap(0, Ordering::Relaxed),
            placed: Vec::new(),
        }
    }

    fn restore_world(saved: SavedWorld) {
        for pos in &saved.placed {
            set_block(pos.0, pos.1, pos.2, 0);
            hopper_queue()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cancel(*pos);
        }
        *hoppers().lock().unwrap_or_else(|e| e.into_inner()) = saved.hoppers;
        *chests().lock().unwrap_or_else(|e| e.into_inner()) = saved.chests;
        *drops().lock().unwrap_or_else(|e| e.into_inner()) = saved.drops;
        *forceloaded().lock().unwrap_or_else(|e| e.into_inner()) = saved.forced;
        *backoff().lock().unwrap_or_else(|e| e.into_inner()) = saved.backoff;
        *hopper_sleep().lock().unwrap_or_else(|e| e.into_inner()) = saved.sleep;
        *drop_cells().lock().unwrap_or_else(|e| e.into_inner()) = saved.cells;
        TICK.store(saved.tick, Ordering::Relaxed);
        TICK_IDX.store(saved.index, Ordering::Relaxed);
        HOPPER_SCAN_RUNS.store(saved.runs, Ordering::Relaxed);
        HOPPER_SCAN_SKIPS.store(saved.skips, Ordering::Relaxed);
        set_test_region(saved.hot, saved.warm);
    }

    /// Warm every chunk the fixture touches so `hopper_tick`'s `is_warm` gate
    /// and `drop_tick`'s hot despawn pass see the whole array.
    fn warm_chunks(x0: i32, z0: i32, x1: i32, z1: i32) -> TestChunkRegion {
        let mut set = HashSet::new();
        for cx in x0.div_euclid(16)..=x1.div_euclid(16) {
            for cz in z0.div_euclid(16)..=z1.div_euclid(16) {
                set.insert((cx, cz));
            }
        }
        (set.clone(), set)
    }

    fn put(pos: (i32, i32, i32), slot: i16, stack: Stack) {
        chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(ContainerKey::ow(pos))
            .or_default()
            .insert(slot, stack);
    }

    fn stacks_at(pos: (i32, i32, i32)) -> Vec<(i64, u8)> {
        let mut v: Vec<(i64, u8)> = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ContainerKey::ow(pos))
            .map(|m| m.values().map(|(i, c, _)| (*i, *c)).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Put every test hopper on this tick's due bucket with no backoff, the
    /// cadence vanilla gives a hopper: one activation per game tick.
    fn make_all_due(placed: &[(i32, i32, i32)]) {
        let mut b = backoff().lock().unwrap_or_else(|e| e.into_inner());
        let mut q = hopper_queue().lock().unwrap_or_else(|e| e.into_inner());
        for pos in placed {
            b.remove(&(DimId::Overworld, pos.0, pos.1, pos.2));
            q.requeue(*pos, 1);
        }
    }

    fn counters() -> (u64, u64) {
        (
            HOPPER_SCAN_RUNS.load(Ordering::Relaxed),
            HOPPER_SCAN_SKIPS.load(Ordering::Relaxed),
        )
    }

    /// 500 idle hoppers plus one working hopper: after the first scans settle,
    /// every idle visit must be an O(1) fingerprint skip while the working
    /// hopper keeps scanning — and a chest edit or a drop above an idle hopper
    /// must wake it on that same `hopper_tick`.
    #[test]
    fn bot_hopper_idle_array_skips_scans_and_wakes_same_tick() {
        let _guard = world_guard();
        let mut saved = save_world();
        let n_idle = 500;
        // 23x22 grid at y=BHY, two blocks apart, all facing down into air:
        // pull source empty, push target air, suction cell empty -> every one
        // settles idle. Two blocks keeps the windows from overlapping.
        let mut placed: Vec<(i32, i32, i32)> = Vec::with_capacity(n_idle + 1);
        let (hot, warm) = warm_chunks(0, 0, 63, 95);
        set_test_region(hot, warm);
        for i in 0..n_idle {
            let pos = (
                ((i % 23) * 2 + 8) as i32,
                BHY,
                ((i / 23) * 2 + 8) as i32,
            );
            set_block(pos.0, pos.1, pos.2, 9225);
            placed.push(pos);
        }
        saved.placed = placed.clone();
        // The one active hopper: a chest above it keeps it working every tick.
        // z=80 sits outside the idle grid (which ends at z=50) but inside the
        // warm chunks.
        let active = (8, BHY, 80);
        let chest_st = name_to_state("chest").unwrap_or(2954);
        let item = name_to_item("raw_iron").unwrap_or(1);
        set_block(active.0, active.1, active.2, 9225);
        set_block(active.0, active.1 + 1, active.2, chest_st);
        put((active.0, active.1 + 1, active.2), 0, (item, 64, Vec::new()));
        placed.push(active);
        saved.placed = placed.clone();
        saved.placed.push((active.0, active.1 + 1, active.2));

        // Three settle ticks: first visit scans (no cache), the hopper stores
        // its fingerprint, the next visits skip.
        for _ in 0..3 {
            make_all_due(&placed);
            hopper_tick();
        }
        let (runs0, skips0) = counters();
        assert!(skips0 > 0, "idle hoppers must skip scans once settled");

        // Five more ticks: skips accumulate at the idle rate, scans stay flat
        // (only the active hopper plus any still-settling entries).
        for _ in 0..5 {
            make_all_due(&placed);
            hopper_tick();
        }
        let (runs1, skips1) = counters();
        assert_eq!(runs1 - runs0, 5, "only the active hopper may scan per tick");
        assert_eq!(skips1 - skips0, 5 * n_idle as u64, "every idle hopper skips every tick");

        // Wake on a chest edit, same tick: put an item in the chest above one
        // idle hopper and run exactly one activation pass.
        let victim = placed[0];
        set_block(victim.0, victim.1 + 1, victim.2, chest_st);
        saved.placed.push((victim.0, victim.1 + 1, victim.2));
        put((victim.0, victim.1 + 1, victim.2), 0, (item, 1, Vec::new()));
        let (runs_w, skips_w) = counters();
        make_all_due(&placed);
        hopper_tick();
        let (runs_a, skips_a) = counters();
        assert_eq!(runs_a - runs_w, 2, "victim + active hopper scan; the rest skip");
        assert_eq!(skips_a - skips_w, n_idle as u64 - 1);
        assert_eq!(
            stacks_at(victim),
            vec![(item, 1)],
            "the woken hopper must pull the chest item on the same tick"
        );

        // One settle tick so the first victim re-idles before the drop wake:
        // it scanned last activation, so it is owed exactly one more scan.
        make_all_due(&placed);
        hopper_tick();

        // Wake on a drop above, same tick: a second idle hopper sucks the item
        // entity out of its suction cell during the very next activation.
        let victim2 = placed[1];
        drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
            eid: 71001,
            x: victim2.0 as f64 + 0.5,
            y: victim2.1 as f64 + 1.0,
            z: victim2.2 as f64 + 0.5,
            item,
            cnt: 3,
            comps: Vec::new(),
            age: 0,
            target: None,
            thrower: None,
        });
        let (runs_w2, skips_w2) = counters();
        make_all_due(&placed);
        hopper_tick();
        let (runs_a2, skips_a2) = counters();
        assert_eq!(runs_a2 - runs_w2, 2, "drop wakes its hopper plus the active one");
        assert_eq!(skips_a2 - skips_w2, n_idle as u64 - 1);
        assert_eq!(stacks_at(victim2), vec![(item, 3)], "sucked stack lands same tick");
        eprintln!(
            "bot_hopper_idle_array: runs={} skips={} ({} idle hoppers, 1 active)",
            runs_a2, skips_a2, n_idle
        );
        restore_world(saved);
    }

    /// Scalar reference for the drop merge: the original full pairwise scan,
    /// kept verbatim so the spatial production path can be held to it — the
    /// same contract `merge_tests::reference_outcome` pins.
    fn scalar_merge_outcome(mut dp: Vec<Drop>) -> Vec<(i32, i64, u8)> {
        let mut merged: Vec<i32> = Vec::new();
        for i in 0..dp.len() {
            for j in (i + 1)..dp.len() {
                let (ax, ay, az, ait, acnt, aage) = {
                    let a = &dp[i];
                    (a.x, a.y, a.z, a.item, a.cnt, a.age)
                };
                let (bx, by, bz, bit, bcnt, bage) = {
                    let b = &dp[j];
                    (b.x, b.y, b.z, b.item, b.cnt, b.age)
                };
                if ait == bit
                    && dp[i].target == dp[j].target
                    && dp[i].comps == dp[j].comps
                    && (ax - bx).powi(2) + (ay - by).powi(2) + (az - bz).powi(2) < 0.25
                    && acnt as u16 + bcnt as u16 <= item_stack_max(ait, &dp[i].comps) as u16
                {
                    let (keep, eat) = if aage <= bage { (i, j) } else { (j, i) };
                    dp[keep].cnt = dp[keep].cnt.saturating_add(dp[eat].cnt);
                    merged.push(dp[eat].eid);
                    break;
                }
            }
        }
        let mut out: Vec<(i32, i64, u8)> = dp
            .into_iter()
            .filter(|d| !merged.contains(&d.eid))
            .map(|d| (d.eid, d.item, d.cnt))
            .collect();
        out.sort_unstable();
        out
    }

    fn drop_at(eid: i32, x: f64, y: f64, z: f64, item: i64, cnt: u8, age: i32) -> Drop {
        Drop {
            eid,
            x,
            y,
            z,
            item,
            cnt,
            comps: Vec::new(),
            age,
            target: None,
            thrower: None,
        }
    }

    /// 500 drops in a kill chamber: 250 isolated merge pairs three blocks
    /// apart, so every merge is a clean pairwise absorption. Item totals must
    /// be conserved exactly and the spatial path must reproduce the scalar
    /// reference outcome drop for drop.
    #[test]
    fn bot_hopper_merge_500_drops_conserves_totals_and_matches_scalar() {
        let _guard = world_guard();
        let item = name_to_item("rotten_flesh").unwrap_or(1);
        let mut dp: Vec<Drop> = Vec::with_capacity(500);
        let mut eid = 80000i32;
        // 16x16 grid of pair sites, 48 blocks apart pair-to-pair, 0.4 inside;
        // 250 complete pairs fill the 500 drops exactly.
        'outer: for gx in 0..16 {
            for gz in 0..16 {
                let bx = gx * 48 + 4;
                let bz = gz * 48 + 4;
                let age_a = eid % 97;
                let age_b = eid % 89;
                dp.push(drop_at(eid, bx as f64 + 0.1, 70.0, bz as f64 + 0.1, item, 1, age_a));
                eid += 1;
                if dp.len() == 500 {
                    break 'outer;
                }
                dp.push(drop_at(eid, bx as f64 + 0.5, 70.0, bz as f64 + 0.1, item, 1, age_b));
                eid += 1;
                if dp.len() == 500 {
                    break 'outer;
                }
            }
        }
        assert_eq!(dp.len(), 500);
        let total_before: u64 = dp.iter().map(|d| u64::from(d.cnt)).sum();

        let t0 = std::time::Instant::now();
        let mut prod = dp.clone();
        let absorbed = merge_drops(&mut prod);
        let merge_us = t0.elapsed().as_micros();
        let mut survivors: Vec<(i32, i64, u8)> = prod
            .iter()
            .filter(|d| !absorbed.contains(&d.eid))
            .map(|d| (d.eid, d.item, d.cnt))
            .collect();
        survivors.sort_unstable();
        let t1 = std::time::Instant::now();
        let expected = scalar_merge_outcome(dp.clone());
        let scalar_us = t1.elapsed().as_micros();
        assert_eq!(survivors, expected, "spatial merge diverged from scalar scan");
        let total_after: u64 = survivors.iter().map(|(_, _, c)| u64::from(*c)).sum();
        assert_eq!(total_after, total_before, "merge must conserve every item");
        eprintln!(
            "bot_hopper_merge_500_pairs: spatial={}us scalar={}us survivors={}",
            merge_us,
            scalar_us,
            survivors.len()
        );
    }

    /// The dense kill chamber: 500 drops packed into a 9x9x3 cellar where
    /// merge chains and re-merge order matter. The spatial path must reproduce
    /// the scalar reference exactly, and the merge stage must stay far under
    /// the pairwise scan's cost.
    #[test]
    fn bot_hopper_merge_500_dense_drops_parity_and_timing() {
        let _guard = world_guard();
        let item = name_to_item("rotten_flesh").unwrap_or(1);
        let mut dp: Vec<Drop> = Vec::with_capacity(500);
        for i in 0..500 {
            let h = (i as u64)
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            dp.push(drop_at(
                90000 + i,
                0.5 + (h % 9) as f64 + ((h >> 8) % 10) as f64 / 64.0,
                70.0 + ((h >> 16) % 3) as f64 + ((h >> 24) % 10) as f64 / 64.0,
                0.5 + ((h >> 32) % 9) as f64 + ((h >> 40) % 10) as f64 / 64.0,
                item,
                ((h >> 48) % 3) as u8 + 1,
                (h % 251) as i32,
            ));
        }
        let t0 = std::time::Instant::now();
        let mut prod = dp.clone();
        let absorbed = merge_drops(&mut prod);
        let merge_us = t0.elapsed().as_micros();
        let mut survivors: Vec<(i32, i64, u8)> = prod
            .iter()
            .filter(|d| !absorbed.contains(&d.eid))
            .map(|d| (d.eid, d.item, d.cnt))
            .collect();
        survivors.sort_unstable();
        let expected = scalar_merge_outcome(dp.clone());
        assert_eq!(survivors, expected, "dense merge diverged from scalar scan");
        // The lane budget is <1ms for the 500-drop merge stage; the scalar
        // scan is timed alongside purely for the report's ratio.
        let t1 = std::time::Instant::now();
        let _ = scalar_merge_outcome(dp);
        let scalar_us = t1.elapsed().as_micros();
        eprintln!(
            "bot_hopper_merge_500_dense: spatial={}us scalar={}us survivors={}",
            merge_us,
            scalar_us,
            survivors.len()
        );
        assert!(
            merge_us < 1000,
            "500-drop spatial merge must stay under 1ms, took {merge_us}us"
        );
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod f2_hopper_furnace_tests {
    use super::*;

    struct SavedWorld {
        blocks: Vec<(DimBlockPos, u32)>,
        containers: HashMap<ContainerKey, HashMap<i16, Stack>>,
        furnaces: HashMap<DimBlockPos, Furnace>,
        furnace_due: tick_queue::DueMachineQueue<DimBlockPos>,
        recipes: Vec<SmeltRecipe>,
        furnace_seq: u64,
        furnace_visits: u64,
        content_revision: u64,
        region: TestChunkRegion,
        hoppers: HashSet<BlockPos>,
        dim_hoppers: HashSet<DimBlockPos>,
        hopper_queue: tick_queue::TickQueue,
        dim_hopper_queues: HashMap<DimId, tick_queue::TickQueue>,
        backoff: HashMap<DimBlockPos, (u64, u32)>,
        hopper_sleep: HashMap<BlockPos, HopperSleep>,
    }

    impl SavedWorld {
        fn take(blocks: &[DimBlockPos], warm: HashSet<(i32, i32)>) -> Self {
            let old_blocks = blocks
                .iter()
                .map(|&(dim, x, y, z)| ((dim, x, y, z), block_at_dim(dim, x, y, z)))
                .collect();
            let furnace_due = std::mem::replace(
                &mut *furnace_due_queue()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
                tick_queue::DueMachineQueue::new(),
            );
            let saved = Self {
                blocks: old_blocks,
                containers: std::mem::take(&mut *chests().lock().unwrap_or_else(|e| e.into_inner())),
                furnaces: std::mem::take(&mut *furnaces().lock().unwrap_or_else(|e| e.into_inner())),
                furnace_due,
                recipes: std::mem::take(&mut *json_smelts().lock().unwrap_or_else(|e| e.into_inner())),
                furnace_seq: FURNACE_TICK_SEQ.swap(0, Ordering::Relaxed),
                furnace_visits: FURNACE_TICK_VISITS.swap(0, Ordering::Relaxed),
                content_revision: CONTENT_REVISION.swap(0, Ordering::Relaxed),
                region: test_region(),
                hoppers: std::mem::take(&mut *hoppers().lock().unwrap_or_else(|e| e.into_inner())),
                dim_hoppers: std::mem::take(&mut *dim_hoppers().lock().unwrap_or_else(|e| e.into_inner())),
                hopper_queue: std::mem::replace(
                    &mut *hopper_queue().lock().unwrap_or_else(|e| e.into_inner()),
                    tick_queue::TickQueue::new(),
                ),
                dim_hopper_queues: std::mem::take(
                    &mut *dim_hopper_queues().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                backoff: std::mem::take(&mut *backoff().lock().unwrap_or_else(|e| e.into_inner())),
                hopper_sleep: std::mem::take(&mut *hopper_sleep().lock().unwrap_or_else(|e| e.into_inner())),
            };
            let hot = warm.clone();
            set_test_region(hot, warm);
            saved
        }
    }

    impl std::ops::Drop for SavedWorld {
        fn drop(&mut self) {
            for &((dim, x, y, z), state) in &self.blocks {
                set_block_dim(dim, x, y, z, state);
            }
            *chests().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.containers);
            *furnaces().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.furnaces);
            *furnace_due_queue().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::replace(&mut self.furnace_due, tick_queue::DueMachineQueue::new());
            *json_smelts().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.recipes);
            *hoppers().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.hoppers);
            *dim_hoppers().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.dim_hoppers);
            *hopper_queue().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::replace(&mut self.hopper_queue, tick_queue::TickQueue::new());
            *dim_hopper_queues().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.dim_hopper_queues);
            *backoff().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.backoff);
            *hopper_sleep().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.hopper_sleep);
            FURNACE_TICK_SEQ.store(self.furnace_seq, Ordering::Relaxed);
            FURNACE_TICK_VISITS.store(self.furnace_visits, Ordering::Relaxed);
            CONTENT_REVISION.store(self.content_revision, Ordering::Relaxed);
            set_test_region(self.region.0.clone(), self.region.1.clone());
        }
    }

    fn blank_furnace() -> Furnace {
        Furnace {
            input: (0, 0, Vec::new()),
            fuel: (0, 0, Vec::new()),
            output: (0, 0, Vec::new()),
            burn_left: 0,
            progress: 0,
            dirty: false,
            xp: 0.0,
            recipe_cache: CachedLookup::default(),
            fuel_cache: CachedLookup::default(),
        }
    }

    fn states() -> (&'static crate::block_registry::BReg, u32, u32, u32) {
        let reg = breg().expect("block registry is initialized for tests");
        (
            reg,
            reg.state_of("minecraft:hopper", None),
            reg.state_of("minecraft:furnace", None),
            reg.state_of("minecraft:chest", None),
        )
    }

    fn hopper_state(
        reg: &crate::block_registry::BReg,
        base: u32,
        facing: &str,
        enabled: bool,
    ) -> u32 {
        let state = reg.with_prop(base, "facing", facing);
        reg.with_prop(state, "enabled", if enabled { "true" } else { "false" })
    }

    fn place(dim: DimId, pos: BlockPos, state: u32) {
        set_block_dim(dim, pos.0, pos.1, pos.2, state);
    }

    fn put(dim: DimId, pos: BlockPos, slot: i16, stack: Stack) {
        chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(ContainerKey::Block((dim, pos.0, pos.1, pos.2)))
            .or_default()
            .insert(slot, stack);
    }

    fn items(dim: DimId, pos: BlockPos) -> Vec<(i64, u8)> {
        let mut stacks: Vec<_> = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ContainerKey::Block((dim, pos.0, pos.1, pos.2)))
            .into_iter()
            .flat_map(|slots| slots.values())
            .filter(|stack| stack.1 > 0)
            .map(|stack| (stack.0, stack.1))
            .collect();
        stacks.sort_unstable();
        stacks
    }

    fn warm_for(blocks: &[DimBlockPos]) -> HashSet<(i32, i32)> {
        blocks
            .iter()
            .map(|(_, x, _, z)| (x.div_euclid(16), z.div_euclid(16)))
            .collect()
    }

    #[test]
    fn f2_vertical_hoppers_wake_smelt_and_extract_output() {
        let _guard = world_guard();
        let dim = *DimId::all_registered().first().expect("overworld dimension exists");
        let (reg, hopper, furnace_state, chest_state) = states();
        let furnace = (3, 80, 3);
        let top = (3, 81, 3);
        let side = (2, 80, 3);
        let bottom = (3, 79, 3);
        let output_chest = (3, 78, 3);
        let block_positions = [
            (dim, furnace.0, furnace.1, furnace.2),
            (dim, output_chest.0, output_chest.1, output_chest.2),
        ];
        let _saved = SavedWorld::take(&block_positions, warm_for(&block_positions));
        place(dim, furnace, furnace_state);
        place(dim, output_chest, chest_state);
        let ore = name_to_item("iron_ore").expect("iron ore item");
        let ingot = name_to_item("iron_ingot").expect("iron ingot item");
        let coal = name_to_item("coal").expect("coal item");
        json_smelts()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((Ingr::Item(ore), ingot, 1, 0.7));
        CONTENT_REVISION.fetch_add(1, Ordering::Relaxed);
        furnaces()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((dim, furnace.0, furnace.1, furnace.2), blank_furnace());
        wake_furnace_at(dim, furnace.0, furnace.1, furnace.2);
        furnace_tick();
        assert_eq!(
            furnace_due_queue().lock().unwrap_or_else(|e| e.into_inner()).len(),
            0,
            "an idle furnace settles into sleep before hopper insertion"
        );
        put(dim, top, 0, (ore, 1, Vec::new()));
        put(dim, side, 0, (coal, 1, Vec::new()));

        let top_state = hopper_state(reg, hopper, "down", true);
        let side_state = hopper_state(reg, hopper, "east", true);
        let bottom_state = hopper_state(reg, hopper, "down", true);
        assert!(hopper_transfer_dim(dim, top, top_state).unwrap().1);
        assert!(hopper_transfer_dim(dim, side, side_state).unwrap().1);
        assert!(items(dim, top).is_empty());
        assert!(items(dim, side).is_empty());
        assert_eq!(
            furnace_due_queue().lock().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "hopper slot changes must wake the sleeping furnace"
        );

        for _ in 0..server_ticks(200) {
            furnace_tick();
        }
        assert_eq!(
            furnaces().lock().unwrap_or_else(|e| e.into_inner())
                .get(&(dim, furnace.0, furnace.1, furnace.2)).unwrap().output,
            (ingot, 1, Vec::new()),
            "the slot wake must cook iron ore to an ingot over 200 ticks"
        );
        assert!(hopper_transfer_dim(dim, bottom, bottom_state).unwrap().1);
        assert_eq!(items(dim, bottom), vec![(ingot, 1)]);
        assert!(hopper_transfer_dim(dim, bottom, bottom_state).unwrap().1);
        assert_eq!(items(dim, output_chest), vec![(ingot, 1)]);
    }

    #[test]
    fn f2_sided_rules_reject_wrong_items_and_return_bucket() {
        let _guard = world_guard();
        let dim = *DimId::all_registered().first().expect("a registered dimension exists");
        let (reg, hopper, furnace_state, _) = states();
        let furnace = (5, 80, 5);
        let side = (4, 80, 5);
        let bottom = (5, 79, 5);
        let block_positions = [(dim, furnace.0, furnace.1, furnace.2)];
        let _saved = SavedWorld::take(&block_positions, warm_for(&block_positions));
        place(dim, furnace, furnace_state);
        let ore = name_to_item("raw_iron").expect("raw iron item");
        let coal = name_to_item("coal").expect("coal item");
        let stone = name_to_item("stone").expect("stone item");
        let ingot = name_to_item("iron_ingot").expect("iron ingot item");
        let bucket = name_to_item("bucket").expect("empty bucket item");
        let mut f = blank_furnace();
        f.input = (ore, 2, Vec::new());
        f.fuel = (coal, 2, Vec::new());
        furnaces()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((dim, furnace.0, furnace.1, furnace.2), f);
        put(dim, side, 0, (stone, 1, Vec::new()));

        assert!(!hopper_transfer_dim(dim, side, hopper_state(reg, hopper, "east", true)).unwrap().1);
        assert!(!hopper_transfer_dim(dim, bottom, hopper_state(reg, hopper, "down", true)).unwrap().1);
        {
            let fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            let f = fs.get(&(dim, furnace.0, furnace.1, furnace.2)).unwrap();
            assert_eq!(f.input, (ore, 2, Vec::new()));
            assert_eq!(f.fuel, (coal, 2, Vec::new()));
        }

        furnaces()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&(dim, furnace.0, furnace.1, furnace.2)).unwrap().output =
            (ingot, 1, Vec::new());
        assert!(hopper_transfer_dim(dim, bottom, hopper_state(reg, hopper, "down", true)).unwrap().1);
        assert_eq!(items(dim, bottom), vec![(ingot, 1)]);
        furnaces()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&(dim, furnace.0, furnace.1, furnace.2)).unwrap().fuel =
            (bucket, 1, Vec::new());
        assert!(hopper_transfer_dim(dim, bottom, hopper_state(reg, hopper, "down", true)).unwrap().1);
        assert_eq!(items(dim, bottom), vec![(bucket, 1), (ingot, 1)]);
    }

    #[test]
    fn f2_locked_hopper_and_distinct_dimensions_never_leak() {
        let _guard = world_guard();
        let dims = DimId::all_registered();
        assert!(dims.len() >= 3, "the three vanilla dimensions are registered");
        let (reg, hopper, furnace_state, _) = states();
        let a = dims[0];
        let b = dims[1];
        let c = dims[2];
        let furnace = (7, 80, 7);
        let top = (7, 81, 7);
        let side = (6, 80, 7);
        let block_positions = [
            (a, furnace.0, furnace.1, furnace.2),
            (b, furnace.0, furnace.1, furnace.2),
            (c, furnace.0, furnace.1, furnace.2),
        ];
        let _saved = SavedWorld::take(&block_positions, warm_for(&block_positions));
        let station_states = [
            furnace_state,
            reg.state_of("minecraft:smoker", None),
            reg.state_of("minecraft:blast_furnace", None),
        ];
        for (dim, state) in [a, b, c].into_iter().zip(station_states) {
            place(dim, furnace, state);
            furnaces()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((dim, furnace.0, furnace.1, furnace.2), blank_furnace());
        }
        let ore = name_to_item("raw_iron").expect("raw iron item");
        let coal = name_to_item("coal").expect("coal item");
        put(a, top, 0, (ore, 1, Vec::new()));
        put(b, top, 0, (ore, 1, Vec::new()));
        put(c, side, 0, (coal, 1, Vec::new()));

        let locked = hopper_state(reg, hopper, "down", false);
        let top_state = hopper_state(reg, hopper, "down", true);
        let side_state = hopper_state(reg, hopper, "east", true);
        assert!(!hopper_transfer_dim(a, top, locked).unwrap().1);
        assert!(hopper_transfer_dim(b, top, top_state).unwrap().1);
        assert!(hopper_transfer_dim(c, side, side_state).unwrap().1);
        assert_eq!(items(a, top), vec![(ore, 1)], "locked hopper retains its item");
        assert_eq!(items(b, top), Vec::<(i64, u8)>::new());
        assert_eq!(items(c, side), Vec::<(i64, u8)>::new());
        let fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(fs.get(&(a, furnace.0, furnace.1, furnace.2)).unwrap().input.1, 0);
        assert_eq!(fs.get(&(b, furnace.0, furnace.1, furnace.2)).unwrap().input, (ore, 1, Vec::new()));
        assert_eq!(fs.get(&(c, furnace.0, furnace.1, furnace.2)).unwrap().fuel, (coal, 1, Vec::new()));
    }
}

#[cfg(test)]
mod xpoll_be_sleep_tests {
    use super::*;

    /// Save/restore for every shared fixture `furnace_tick` and
    /// `live_brewing_tick` can read, including the sleep caches and counters
    /// this lane adds.
    struct SavedBe {
        furnaces: HashMap<DimBlockPos, Furnace>,
        stands: HashMap<DimBlockPos, LiveBrewingStand>,
        bsleep: HashMap<DimBlockPos, u128>,
        fvisits: u64,
        bskips: u64,
        bsteps: u64,
        region: TestChunkRegion,
    }

    fn save_be() -> SavedBe {
        furnace_due_queue()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        SavedBe {
            furnaces: std::mem::take(&mut *furnaces().lock().unwrap_or_else(|e| e.into_inner())),
            stands: std::mem::take(
                &mut *live_brewing_stands()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            ),
            bsleep: std::mem::take(&mut *brewing_sleep().lock().unwrap_or_else(|e| e.into_inner())),
            fvisits: FURNACE_TICK_VISITS.swap(0, Ordering::Relaxed),
            bskips: BREWING_SLEEP_SKIPS.swap(0, Ordering::Relaxed),
            bsteps: BREWING_ACTIVE_STEPS.swap(0, Ordering::Relaxed),
            region: test_region(),
        }
    }

    fn restore_be(saved: SavedBe) {
        *furnaces().lock().unwrap_or_else(|e| e.into_inner()) = saved.furnaces;
        // The restored store is authoritative: deadlines the test's removed
        // furnaces left behind are dropped and the surviving entries get a
        // fresh settle pass.
        rebuild_furnace_due_queue();
        *live_brewing_stands()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = saved.stands;
        *brewing_sleep().lock().unwrap_or_else(|e| e.into_inner()) = saved.bsleep;
        FURNACE_TICK_VISITS.store(saved.fvisits, Ordering::Relaxed);
        BREWING_SLEEP_SKIPS.store(saved.bskips, Ordering::Relaxed);
        BREWING_ACTIVE_STEPS.store(saved.bsteps, Ordering::Relaxed);
        set_test_region(saved.region.0, saved.region.1);
    }

    fn warm_block_range(x0: i32, z0: i32, x1: i32, z1: i32) -> HashSet<(i32, i32)> {
        let mut set = HashSet::new();
        for cx in x0.div_euclid(16)..=x1.div_euclid(16) {
            for cz in z0.div_euclid(16)..=z1.div_euclid(16) {
                set.insert((cx, cz));
            }
        }
        set
    }

    fn blank_furnace() -> Furnace {
        Furnace {
            input: (0, 0, Vec::new()),
            fuel: (0, 0, Vec::new()),
            output: (0, 0, Vec::new()),
            burn_left: 0,
            progress: 0,
            dirty: false,
            xp: 0.0,
            recipe_cache: CachedLookup::default(),
            fuel_cache: CachedLookup::default(),
        }
    }

    fn furnace_visits() -> u64 {
        FURNACE_TICK_VISITS.load(Ordering::Relaxed)
    }

    fn brewing_counters() -> (u64, u64) {
        (
            BREWING_ACTIVE_STEPS.load(Ordering::Relaxed),
            BREWING_SLEEP_SKIPS.load(Ordering::Relaxed),
        )
    }

    /// 1,000 idle furnaces plus one cooking furnace: the settle pass visits
    /// each once, then 50 ticks visit ONLY the cooker — the idle farm is
    /// drained zero times, so it costs zero per tick.
    #[test]
    fn xpoll_be_sleep_1000_idle_furnaces_zero_visits_while_one_cooks() {
        let _guard = world_guard();
        let saved = save_be();
        let n_idle = 1_000usize;
        let iron = name_to_item("raw_iron").unwrap();
        let coal = name_to_item("coal").unwrap();
        let warm = warm_block_range(0, 0, 63, 63);
        set_test_region(warm.clone(), warm);

        {
            let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            // 32x32 even grid inside the 64x64 warm range; (62,140,62) is the
            // one grid corner no idle entry can occupy.
            for i in 0..n_idle {
                let pos = ow_block((((i % 32) * 2) as i32, 140, ((i / 32) * 2) as i32));
                fs.insert(pos, blank_furnace());
            }
            // The one active furnace: iron + coal, cooking from tick one.
            let mut active = blank_furnace();
            active.input = (iron, 1, Vec::new());
            active.fuel = (coal, 1, Vec::new());
            fs.insert(ow_block((62, 140, 62)), active);
        }
        let active_pos = ow_block((62, 140, 62));
        // Model the load/insert event: every registered furnace gets one
        // settle visit that proves the idle ones inert.
        rebuild_furnace_due_queue();

        let t0 = std::time::Instant::now();
        furnace_tick();
        let settle_us = t0.elapsed().as_micros();
        assert_eq!(
            furnace_visits(),
            1_001,
            "the settle pass steps every furnace exactly once"
        );
        {
            let fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            let f = fs.get(&active_pos).unwrap();
            assert_eq!(f.progress, 1, "the cooker ignites on the settle tick");
            assert_eq!(f.burn_left, server_ticks(1600) as i32 - 1);
        }
        // 50 further ticks: the idle thousand are never drained, only the
        // cooker re-arms itself.
        let t0 = std::time::Instant::now();
        for _ in 0..50 {
            furnace_tick();
        }
        let fifty_ticks_us = t0.elapsed().as_micros();
        assert_eq!(
            furnace_visits(),
            1_001 + 50,
            "only the active furnace may be visited after the settle pass"
        );
        {
            let fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            let f = fs.get(&active_pos).unwrap();
            assert_eq!(f.progress, 51, "the cooker advances one step per tick");
            assert_eq!(f.burn_left, server_ticks(1600) as i32 - 51);
            assert_eq!(f.output, (0, 0, Vec::new()));
        }
        assert_eq!(
            furnace_due_queue()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            1,
            "only the cooker still holds a deadline"
        );
        eprintln!(
            "xpoll_be_sleep: 1000 idle + 1 active: settle={}us 50t={}us visits=1051",
            settle_us, fifty_ticks_us
        );
        restore_be(saved);
    }

    /// Wake-on-event: a sleeping furnace that receives fuel + input through
    /// `wake_furnace_at` (the hook every slot writer calls) ignites on the
    /// next tick and completes the smelt at exactly the vanilla cook time.
    #[test]
    fn xpoll_be_sleep_furnace_wake_on_event_smelts_vanilla() {
        let _guard = world_guard();
        let saved = save_be();
        let iron = name_to_item("raw_iron").unwrap();
        let coal = name_to_item("coal").unwrap();
        let warm = warm_block_range(0, 0, 15, 15);
        set_test_region(warm.clone(), warm);
        let pos = ow_block((4, 140, 4));

        {
            let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            fs.insert(pos, blank_furnace());
        }
        rebuild_furnace_due_queue();
        // The settle pass proves it inert and it leaves the queue.
        furnace_tick();
        assert_eq!(furnace_visits(), 1);
        assert!(
            furnace_due_queue()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "an inert furnace must not stay scheduled"
        );

        // The mutation event: fuel + input land, the writer wakes it.
        {
            let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            let f = fs.get_mut(&pos).unwrap();
            f.input = (iron, 1, Vec::new());
            f.fuel = (coal, 1, Vec::new());
        }
        wake_furnace_at(DimId::Overworld, 4, 140, 4);
        let before = furnace_visits();
        furnace_tick();
        assert_eq!(furnace_visits() - before, 1, "the wake drains exactly once");
        {
            let fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            let f = fs.get(&pos).unwrap();
            assert_eq!(f.progress, 1, "woken furnace cooks on the wake tick");
            assert_eq!(f.burn_left, server_ticks(1600) as i32 - 1);
        }

        // The rest of the cook is vanilla-paced self re-arming: no more wakes,
        // exactly `server_ticks(200)` progress steps produce the ingot.
        for _ in 0..server_ticks(200) as usize - 1 {
            furnace_tick();
        }
        {
            let fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            let f = fs.get(&pos).unwrap();
            assert_eq!(
                f.output,
                (name_to_item("iron_ingot").unwrap(), 1, Vec::new()),
                "the smelt completes after the vanilla cook time"
            );
            assert_eq!(f.input, (0, 0, Vec::new()));
            assert!(f.xp > 0.0, "the smelt banks its experience");
        }
        // `burn_left` only drains while smelting, so the leftover burn keeps
        // the furnace scheduled — the reference tick visits it every tick too.
        let before = furnace_visits();
        furnace_tick();
        assert_eq!(furnace_visits() - before, 1);
        // Clear the residual burn the way a `/data` BurnTime write does (the
        // write path wakes it itself); the next pass proves it inert and it
        // falls back asleep for good.
        {
            let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            fs.get_mut(&pos).unwrap().burn_left = 0;
        }
        wake_furnace_at(DimId::Overworld, 4, 140, 4);
        furnace_tick();
        let before = furnace_visits();
        furnace_tick();
        assert_eq!(
            furnace_visits() - before,
            0,
            "a fully inert furnace must leave the queue"
        );
        restore_be(saved);
    }

    /// Remove-on-break: cancelling the deadline along with the store entry
    /// leaves nothing queued — later ticks never visit the ghost.
    #[test]
    fn xpoll_be_sleep_furnace_remove_on_break_cancels_deadline() {
        let _guard = world_guard();
        let saved = save_be();
        let iron = name_to_item("raw_iron").unwrap();
        let coal = name_to_item("coal").unwrap();
        let warm = warm_block_range(0, 0, 15, 15);
        set_test_region(warm.clone(), warm);
        let pos = ow_block((4, 140, 4));

        {
            let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            let mut active = blank_furnace();
            active.input = (iron, 1, Vec::new());
            active.fuel = (coal, 1, Vec::new());
            fs.insert(pos, active);
        }
        rebuild_furnace_due_queue();
        furnace_tick();
        assert_eq!(furnace_visits(), 1);
        assert_eq!(
            furnace_due_queue()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            1,
            "a burning furnace re-arms itself"
        );

        // The break path removes the entry and cancels its deadline together.
        furnaces()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pos);
        remove_furnace_at(DimId::Overworld, 4, 140, 4);
        assert!(
            furnace_due_queue()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "a broken furnace must not stay scheduled"
        );

        let before = furnace_visits();
        for _ in 0..10 {
            furnace_tick();
        }
        assert_eq!(
            furnace_visits() - before,
            0,
            "no ghost visit may fire for the removed furnace"
        );
        restore_be(saved);
    }

    /// An idle brewing stand skips the per-pass potion/ingredient derivation,
    /// and wakes on the exact same tick when an ingredient or blaze powder
    /// lands in its slots.
    #[test]
    fn xpoll_be_sleep_brewing_stand_skips_idle_and_wakes_on_ingredient() {
        let _guard = world_guard();
        let saved = save_be();
        let water = || {
            (
                name_to_item("potion").unwrap(),
                1,
                vec![(COMP_POTION_CONTENTS, vec![1, 0])],
            )
        };

        // Stand A: charged with fuel, water bottles in, nothing to brew.
        let key_a: DimBlockPos = (DimId::Overworld, 8, 140, 8);
        {
            let mut menu = BrewingMenu::default();
            menu.slots[0] = water();
            let stand = BrewingStand {
                fuel: 20,
                ..Default::default()
            };
            live_brewing_stands()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key_a, LiveBrewingStand { stand, menu });
        }
        // Stand B: no fuel, no powder, nothing brewable.
        let key_b: DimBlockPos = (DimId::Overworld, 10, 140, 8);
        live_brewing_stands()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key_b, LiveBrewingStand::default());

        // Tick 1 proves both stands inert and records their fingerprints;
        // the following ticks are fingerprint skips.
        for _ in 0..3 {
            live_brewing_tick();
        }
        let (steps, skips) = brewing_counters();
        assert_eq!(steps, 2, "each stand pays one proving pass");
        assert_eq!(skips, 2 * 2, "both stands skip the settle ticks");

        // Insert nether wart into A: the slot fingerprint changes, so the
        // very next tick wakes the stand and the brew starts like vanilla.
        {
            let mut stands = live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner());
            stands.get_mut(&key_a).unwrap().menu.slots[3] =
                (name_to_item("nether_wart").unwrap(), 1, Vec::new());
        }
        let (steps_w, skips_w) = brewing_counters();
        live_brewing_tick();
        let (steps_a, skips_a) = brewing_counters();
        {
            let stands = live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner());
            let a = &stands.get(&key_a).unwrap().stand;
            assert_eq!(a.brew_time, 400, "the wake tick starts the brew");
            assert_eq!(a.fuel, 19, "one fuel point spent on the same tick");
            assert_eq!(a.active_ingredient, Some(BrewingIngredient::NetherWart));
        }
        assert_eq!(steps_a - steps_w, 1, "only the woken stand pays a pass");
        assert_eq!(skips_a - skips_w, 1, "the untouched stand still skips");

        // Let B prove inert again, then drop blaze powder into its fuel slot:
        // same-tick wake burns one powder into 20 fuel.
        live_brewing_tick();
        {
            let mut stands = live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner());
            stands.get_mut(&key_b).unwrap().menu.slots[4] =
                (name_to_item("blaze_powder").unwrap(), 1, Vec::new());
        }
        let (_, skips_b) = brewing_counters();
        live_brewing_tick();
        let (steps_b, skips_c) = brewing_counters();
        {
            let stands = live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner());
            let b = stands.get(&key_b).unwrap();
            assert_eq!(b.stand.fuel, 20, "powder burns to fuel on the wake tick");
            assert_eq!(b.menu.slots[4].1, 0, "one powder consumed");
        }
        assert_eq!(skips_c - skips_b, 0, "powder wake is a real pass, not a skip");
        let _ = steps_b;
        eprintln!(
            "xpoll_be_sleep: brewing skips={} steps={} (wart + powder same-tick wakes)",
            BREWING_SLEEP_SKIPS.load(Ordering::Relaxed),
            BREWING_ACTIVE_STEPS.load(Ordering::Relaxed)
        );
        restore_be(saved);
    }
}

// ===========================================================================
// Minecraft 1.21.1 (protocol 767) item data components: server behavior.
// The ids below are the canonical `minecraft:data_component_type` indices, in
// registry order, matching `proto`'s `V767_COMPONENTS` table. Each one feeds at
// least one production path: stacking, durability, mining, food, projectiles,
// bundles, containers, block use, or the inventory-NBT codec.
// ===========================================================================

const COMP_MAX_STACK_SIZE: i64 = 1;
const COMP_ITEM_NAME: i64 = 6;
const COMP_RARITY: i64 = 8;
const COMP_CUSTOM_MODEL_DATA: i64 = 13;
const COMP_HIDE_ADDITIONAL_TOOLTIP: i64 = 14;
const COMP_HIDE_TOOLTIP: i64 = 15;
const COMP_REPAIR_COST: i64 = 16;
const COMP_CREATIVE_SLOT_LOCK: i64 = 17;
const COMP_ENCHANTMENT_GLINT_OVERRIDE: i64 = 18;
const COMP_INTANGIBLE_PROJECTILE: i64 = 19;
const COMP_FOOD: i64 = 20;
const COMP_FIRE_RESISTANT: i64 = 21;
const COMP_TOOL: i64 = 22;
const COMP_DYED_COLOR: i64 = 24;
const COMP_MAP_COLOR: i64 = 25;
const COMP_MAP_ID: i64 = 26;
const COMP_MAP_DECORATIONS: i64 = 27;
const COMP_MAP_POST_PROCESSING: i64 = 28;
const COMP_CHARGED_PROJECTILES: i64 = 29;
const COMP_BUNDLE_CONTENTS: i64 = 30;
const COMP_SUSPICIOUS_STEW_EFFECTS: i64 = 32;
const COMP_WRITABLE_BOOK_CONTENT: i64 = 33;
const COMP_WRITTEN_BOOK_CONTENT: i64 = 34;
const COMP_TRIM: i64 = 35;
const COMP_DEBUG_STICK_STATE: i64 = 36;
const COMP_ENTITY_DATA: i64 = 37;
const COMP_BUCKET_ENTITY_DATA: i64 = 38;
const COMP_BLOCK_ENTITY_DATA: i64 = 39;
const COMP_INSTRUMENT: i64 = 40;
const COMP_OMINOUS_BOTTLE_AMPLIFIER: i64 = 41;
const COMP_JUKEBOX_PLAYABLE: i64 = 42;
const COMP_RECIPES: i64 = 43;
const COMP_LODESTONE_TRACKER: i64 = 44;
const COMP_FIREWORK_EXPLOSION: i64 = 45;
const COMP_FIREWORKS: i64 = 46;
const COMP_PROFILE: i64 = 47;
const COMP_NOTE_BLOCK_SOUND: i64 = 48;
const COMP_BASE_COLOR: i64 = 50;
const COMP_POT_DECORATIONS: i64 = 51;
const COMP_CONTAINER: i64 = 52;
const COMP_BLOCK_STATE: i64 = 53;
const COMP_BEES: i64 = 54;
const COMP_LOCK: i64 = 55;
const COMP_CONTAINER_LOOT: i64 = 56;

/// Canonical `minecraft:data_component_type` names in registry order. The
/// inventory-NBT codec uses the table to preserve payloads verbatim for the
/// components it does not model structurally.
static COMP_WIRE_NAMES: [&str; 57] = [
    "minecraft:custom_data", "minecraft:max_stack_size", "minecraft:max_damage",
    "minecraft:damage", "minecraft:unbreakable", "minecraft:custom_name",
    "minecraft:item_name", "minecraft:lore", "minecraft:rarity",
    "minecraft:enchantments", "minecraft:can_place_on", "minecraft:can_break",
    "minecraft:attribute_modifiers", "minecraft:custom_model_data",
    "minecraft:hide_additional_tooltip", "minecraft:hide_tooltip",
    "minecraft:repair_cost", "minecraft:creative_slot_lock",
    "minecraft:enchantment_glint_override", "minecraft:intangible_projectile",
    "minecraft:food", "minecraft:fire_resistant", "minecraft:tool",
    "minecraft:stored_enchantments", "minecraft:dyed_color",
    "minecraft:map_color", "minecraft:map_id", "minecraft:map_decorations",
    "minecraft:map_post_processing", "minecraft:charged_projectiles",
    "minecraft:bundle_contents", "minecraft:potion_contents",
    "minecraft:suspicious_stew_effects", "minecraft:writable_book_content",
    "minecraft:written_book_content", "minecraft:trim",
    "minecraft:debug_stick_state", "minecraft:entity_data",
    "minecraft:bucket_entity_data", "minecraft:block_entity_data",
    "minecraft:instrument", "minecraft:ominous_bottle_amplifier",
    "minecraft:jukebox_playable", "minecraft:recipes",
    "minecraft:lodestone_tracker", "minecraft:firework_explosion",
    "minecraft:fireworks", "minecraft:profile", "minecraft:note_block_sound",
    "minecraft:banner_patterns", "minecraft:base_color",
    "minecraft:pot_decorations", "minecraft:container", "minecraft:block_state",
    "minecraft:bees", "minecraft:lock", "minecraft:container_loot",
];

fn comp_wire_name(ty: i64) -> Option<&'static str> {
    COMP_WIRE_NAMES.get(usize::try_from(ty).ok()?).copied()
}

fn comp_id_for_name(name: &str) -> Option<i64> {
    COMP_WIRE_NAMES
        .iter()
        .position(|n| {
            *n == name || n.strip_prefix("minecraft:") == Some(name)
        })
        .map(|i| i as i64)
}

fn comp_read_f32(data: &[u8], p: &mut usize) -> Option<f32> {
    let v = f32::from_be_bytes(data.get(*p..*p + 4)?.try_into().ok()?);
    *p += 4;
    Some(v)
}

fn comp_read_string(data: &[u8], p: &mut usize) -> Option<String> {
    let len = usize::try_from(slot_read_varint(data, p)?)
        .ok()
        .filter(|&n| n <= 98301)?;
    let end = (*p).checked_add(len)?;
    let text = std::str::from_utf8(data.get(*p..end)?).ok()?.to_string();
    *p = end;
    Some(text)
}

/// The plain-text payload of a `custom_name`/`item_name`-style text component:
/// a TAG_String of the text, or a compound carrying a `text` member.
fn comp_text_string(data: &[u8]) -> Option<String> {
    let mut p = 0;
    if let Some(text) = read_nbt_string_bytes(data, &mut p) {
        return Some(text);
    }
    let mut rest = data;
    let tag = crate::nbt::read_network_nbt(&mut rest).ok()?;
    tag.get("text")
        .and_then(|t| t.string())
        .and_then(|s| s.to_utf8().ok())
}

fn stack_item_text(st: &Stack, ty: i64) -> Option<String> {
    comp_get(&st.2, ty).and_then(comp_text_string)
}

/// `ItemStack.hasFoil`: enchantment glint override wins; an enchanted stack
/// glints either way. Used for the creative/pick integrity check below.
fn stack_enchantment_glint(st: &Stack) -> bool {
    match comp_get(&st.2, COMP_ENCHANTMENT_GLINT_OVERRIDE) {
        Some([v]) => *v != 0,
        Some(_) => true,
        None => enchantments_get(&st.2).iter().any(|(_, l)| *l > 0),
    }
}

/// Effective stack food for `stack`: an explicit `minecraft:food` component
/// wins; otherwise the vanilla item defaults apply. Returns `None` for items
/// that are not food at all.
#[derive(Clone, Debug, PartialEq)]
struct FoodComponent {
    nutrition: i64,
    saturation: f32,
    can_always_eat: bool,
    eat_seconds: f32,
    converts_to: Stack,
    /// (mob effect registry id, probability 0..=1)
    effects: Vec<(i64, f32)>,
}

fn decode_food_component(data: &[u8]) -> Option<FoodComponent> {
    let mut p = 0;
    let nutrition = slot_read_varint(data, &mut p)?;
    if nutrition < 0 {
        return None;
    }
    let saturation = comp_read_f32(data, &mut p)?;
    if !saturation.is_finite() || saturation < 0.0 {
        return None;
    }
    let can_always_eat = slot_read_bool(data, &mut p)?;
    let eat_seconds = comp_read_f32(data, &mut p)?;
    if !eat_seconds.is_finite() || eat_seconds <= 0.0 {
        return None;
    }
    let converts_to = slot_read_stack(data, &mut p, 1)?;
    let n = slot_read_count(data, &mut p, 256)?;
    let mut effects = Vec::with_capacity(n);
    for _ in 0..n {
        let id = slot_read_varint(data, &mut p)?;
        let probability = comp_read_f32(data, &mut p)?;
        if !(0.0..=1.0).contains(&probability) {
            return None;
        }
        effects.push((id, probability));
    }
    (p == data.len()).then_some(FoodComponent {
        nutrition,
        saturation,
        can_always_eat,
        eat_seconds,
        converts_to,
        effects,
    })
}

/// `minecraft:tool`: `Tool.rules` + `defaultMiningSpeed` + `damagePerBlock`.
/// Vanilla evaluates rules in order; the first matching rule that carries a
/// speed wins for `getDestroySpeed`, the first matching rule carrying
/// `correctForDrops` decides `isCorrectToolForDrops`.
#[derive(Clone, Debug, PartialEq)]
struct ToolComponent {
    rules: Vec<ToolRule>,
    default_speed: f32,
    damage_per_block: i64,
}

#[derive(Clone, Debug, PartialEq)]
struct ToolRule {
    /// `kind == 0` holder set: a block tag name.
    tag: Option<String>,
    /// `kind > 0` holder set: the explicit block ids.
    ids: Vec<i64>,
    speed: Option<f32>,
    correct_for_drops: Option<bool>,
}

fn decode_tool_component(data: &[u8]) -> Option<ToolComponent> {
    let mut p = 0;
    let n = slot_read_count(data, &mut p, 256)?;
    let mut rules = Vec::with_capacity(n);
    for _ in 0..n {
        let kind = slot_read_varint(data, &mut p)?;
        let (tag, ids) = if kind == 0 {
            (Some(comp_read_string(data, &mut p)?), Vec::new())
        } else {
            let count = usize::try_from(kind - 1)
                .ok()
                .filter(|&n| n <= 4096)?;
            let mut ids = Vec::with_capacity(count);
            for _ in 0..count {
                ids.push(slot_read_varint(data, &mut p)?);
            }
            (None, ids)
        };
        let speed = if slot_read_bool(data, &mut p)? {
            let v = comp_read_f32(data, &mut p)?;
            if !v.is_finite() || v < 0.0 {
                return None;
            }
            Some(v)
        } else {
            None
        };
        let correct_for_drops = if slot_read_bool(data, &mut p)? {
            Some(slot_read_bool(data, &mut p)?)
        } else {
            None
        };
        rules.push(ToolRule {
            tag,
            ids,
            speed,
            correct_for_drops,
        });
    }
    let default_speed = comp_read_f32(data, &mut p)?;
    if !default_speed.is_finite() || default_speed < 0.0 {
        return None;
    }
    let damage_per_block = slot_read_varint(data, &mut p)?;
    if damage_per_block < 0 {
        return None;
    }
    (p == data.len()).then_some(ToolComponent {
        rules,
        default_speed,
        damage_per_block,
    })
}

fn tool_component_for(st: &Stack) -> Option<ToolComponent> {
    comp_get(&st.2, COMP_TOOL).and_then(decode_tool_component)
}

fn tool_rule_matches(rule: &ToolRule, state: u32, reg: &crate::block_registry::BReg) -> bool {
    if let Some(tag) = &rule.tag {
        return reg.in_tag(state, tag);
    }
    reg.block_id_of(state)
        .map(|id| rule.ids.contains(&i64::from(id)))
        .unwrap_or(false)
}

/// Vanilla `Tool.getMiningSpeed`: first matching rule with a speed; otherwise
/// the component's default speed.
fn tool_component_speed(tool: &ToolComponent, state: u32, reg: &crate::block_registry::BReg) -> f32 {
    for rule in &tool.rules {
        if let Some(speed) = rule.speed {
            if tool_rule_matches(rule, state, reg) {
                return speed;
            }
        }
    }
    tool.default_speed
}

/// Vanilla `Tool.isCorrectForDrops`: first matching rule carrying the flag;
/// false when no rule both matches and declares it.
fn tool_component_correct(tool: &ToolComponent, state: u32, reg: &crate::block_registry::BReg) -> bool {
    for rule in &tool.rules {
        if let Some(correct) = rule.correct_for_drops {
            if tool_rule_matches(rule, state, reg) {
                return correct;
            }
        }
    }
    false
}

/// Damage a tool component charges per broken block (`mineBlock` semantics).
fn tool_damage_per_block(st: &Stack) -> i64 {
    tool_component_for(st)
        .map(|tool| tool.damage_per_block)
        .unwrap_or(1)
}

/// A nested-stack list payload (`charged_projectiles`, `bundle_contents`,
/// `container`), encoded as `count + slot_wire` entries.
fn decode_stack_list(data: &[u8], max: usize) -> Option<Vec<Stack>> {
    let mut p = 0;
    let n = slot_read_count(data, &mut p, 256)?;
    if n > max {
        return None;
    }
    let mut stacks = Vec::with_capacity(n);
    for _ in 0..n {
        stacks.push(slot_read_stack(data, &mut p, 1)?);
    }
    (p == data.len()).then_some(stacks)
}

fn encode_stack_list(stacks: &[Stack]) -> Vec<u8> {
    let mut data = Vec::new();
    write_varint(&mut data, stacks.len() as i64);
    for st in stacks {
        data.extend_from_slice(&slot_wire(st));
    }
    data
}

/// `minecraft:bundle_contents` weight: vanilla `BundleItem.getWeight` is
/// `count * (64 / maxStackSize)`, where each entry's max is its own
/// component-aware limit. A full bundle weighs 64.
const BUNDLE_WEIGHT_MAX: i64 = 64;

fn bundle_stack_weight(st: &Stack) -> i64 {
    let per = (BUNDLE_WEIGHT_MAX / stack_max_of(st).max(1)).max(1);
    per.saturating_mul(i64::from(st.1))
}

fn bundle_contents_of(st: &Stack) -> Option<Vec<Stack>> {
    comp_get(&st.2, COMP_BUNDLE_CONTENTS).map(|data| {
        decode_stack_list(data, 256).unwrap_or_default()
    })
}

fn bundle_total_weight(st: &Stack) -> i64 {
    bundle_contents_of(st)
        .unwrap_or_default()
        .iter()
        .map(bundle_stack_weight)
        .sum()
}

/// Vanilla `BundleItem.overrideStackedOnOther`: insert as much of `add` as the
/// 64-weight budget allows, splitting the cursor stack when it does not fit.
/// Returns the bundle's new `bundle_contents` payload and the leftover count.
fn bundle_try_insert(contents: &mut Vec<Stack>, add: &Stack) -> (Vec<u8>, u8) {
    let mut remaining = add.1;
    let mut weight: i64 = contents.iter().map(bundle_stack_weight).sum();
    // Fill an existing compatible entry first (vanilla merges before appending).
    for entry in contents.iter_mut() {
        if remaining == 0 || weight >= BUNDLE_WEIGHT_MAX {
            break;
        }
        if entry.0 != add.0 || entry.2 != add.2 {
            continue;
        }
        let per = (BUNDLE_WEIGHT_MAX / stack_max_of(entry).max(1)).max(1);
        let room = stack_max_of(entry) - i64::from(entry.1);
        let fits = ((BUNDLE_WEIGHT_MAX - weight) / per)
            .min(room)
            .min(i64::from(remaining));
        if fits > 0 {
            entry.1 += fits as u8;
            remaining -= fits as u8;
            weight += per * fits;
        }
    }
    if remaining > 0 && weight < BUNDLE_WEIGHT_MAX {
        let per = (BUNDLE_WEIGHT_MAX / stack_max_of(add).max(1)).max(1);
        let fits = ((BUNDLE_WEIGHT_MAX - weight) / per).min(i64::from(remaining));
        if fits > 0 {
            contents.push((add.0, fits as u8, add.2.clone()));
            remaining -= fits as u8;
        }
    }
    (encode_stack_list(contents), remaining)
}

/// Vanilla `BundleItem.overrideOtherStackedOnMe` with an empty cursor:
/// removes the *last* non-empty entry (bundles are LIFO) and yields it.
fn bundle_try_extract(contents: &mut [Stack]) -> Option<(Stack, Vec<u8>)> {
    let st = contents.iter_mut().find(|e| e.1 > 0)?;
    let out = st.clone();
    st.1 = 0;
    Some((out, encode_stack_list(contents)))
}

fn is_bundle_item(item: i64) -> bool {
    item == name_to_item("bundle").unwrap_or(-1)
}

/// `minecraft:suspicious_stew_effects`: `(effect id, duration game ticks)` —
/// vanilla `SuspiciousStewEffects`.
fn decode_suspicious_stew_effects(data: &[u8]) -> Option<Vec<(i64, i64)>> {
    let mut p = 0;
    let n = slot_read_count(data, &mut p, 256)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let id = slot_read_varint(data, &mut p)?;
        let duration = slot_read_varint(data, &mut p)?;
        if !(0..=160).contains(&duration) {
            return None;
        }
        out.push((id, duration));
    }
    (p == data.len()).then_some(out)
}

/// The effective food for a stack: the `food` component wins, then vanilla
/// item defaults (`food_value`/`sat_value`/always-edible/side-effects tables).
fn stack_food(st: &Stack) -> Option<FoodComponent> {
    if let Some(fc) = comp_get(&st.2, COMP_FOOD).and_then(decode_food_component) {
        return Some(fc);
    }
    let nutrition = i64::from(food_value(st.0));
    let ominous = item_to_name(st.0) == Some("ominous_bottle");
    let stew = item_to_name(st.0) == Some("suspicious_stew");
    if nutrition == 0 && !ominous && !stew {
        return None;
    }
    let (nutrition, saturation, can_always_eat) = if ominous {
        (0, 0.0, true)
    } else if stew {
        // 7.2 gain over 6 nutrition is a 0.6 saturation modifier.
        (6, 0.6, false)
    } else {
        (
            nutrition,
            if nutrition > 0 {
                sat_value(st.0) / (2 * nutrition) as f32
            } else {
                0.0
            },
            always_edible(st.0),
        )
    };
    let converts_to = if ominous || stew {
        (
            name_to_item(if ominous { "glass_bottle" } else { "bowl" }).unwrap_or(0),
            1,
            Vec::new(),
        )
    } else {
        (0, 0, Vec::new())
    };
    Some(FoodComponent {
        nutrition,
        saturation,
        can_always_eat,
        eat_seconds: 1.6,
        converts_to,
        effects: Vec::new(),
    })
}

/// Duration (server ticks) of a food effect entry after `stack_food`: the
/// folded `(id*1000+amp, probability)` pairs above are split back out by the
/// consumer, which pairs them with the side-effect tables' vanilla durations.
fn stack_food_effects(st: &Stack) -> Vec<(u64, i32, i32, i32)> {
    let mut out = Vec::new();
    if comp_get(&st.2, COMP_FOOD).is_none() {
        if item_to_name(st.0) == Some("suspicious_stew") {
            if let Some(list) = comp_get(&st.2, COMP_SUSPICIOUS_STEW_EFFECTS)
                .and_then(decode_suspicious_stew_effects)
            {
                out.extend(
                    list.into_iter()
                        .map(|(id, duration)| (100, id as i32, 0, server_ticks(duration as u64) as i32)),
                );
            }
            return out;
        }
        if item_to_name(st.0) == Some("ominous_bottle") {
            let amp = ominous_bottle_amplifier(st).unwrap_or(0);
            out.push((100, 30, amp as i32, server_ticks(120_000) as i32));
            return out;
        }
        return food_effects(st.0).to_vec();
    }
    if let Some(fc) = comp_get(&st.2, COMP_FOOD).and_then(decode_food_component) {
        out.extend(
            fc.effects
                .iter()
                .map(|&(id, prob)| {
                    (
                        (prob * 100.0) as u64,
                        id as i32,
                        0,
                        server_ticks(1200) as i32,
                    )
                }),
        );
    }
    out
}

/// `minecraft:ominous_bottle_amplifier` clamped to vanilla's 0..=4 domain.
fn ominous_bottle_amplifier(st: &Stack) -> Option<i64> {
    comp_varint(&st.2, COMP_OMINOUS_BOTTLE_AMPLIFIER).map(|a| a.clamp(0, 4))
}

/// Whether a `can_break`/`can_place_on` predicate set permits `state`
/// (adventure-mode predicates; the server applies them to block-break and
/// block-place decisions like vanilla's `AdventureModePredicate.test`).
#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn block_predicates_allow(data: &[u8], state: u32, reg: &crate::block_registry::BReg) -> bool {
    let mut p = 0;
    let Some(n) = slot_read_count(data, &mut p, 256) else {
        return true;
    };
    let mut any = false;
    for _ in 0..n {
        let start = p;
        if slot_read_block_predicate(data, &mut p).is_none() {
            return false;
        }
        if p == start {
            return false;
        }
        let mut q = start;
        if predicate_matches_state(data, &mut q, state, reg) {
            any = true;
        }
    }
    n == 0 || any
}

/// Re-reads one `can_break`/`can_place_on` block predicate
/// (`{blocks?, properties?, nbt?}`) and tests whether it covers `state`.
#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn predicate_matches_state(
    data: &[u8],
    p: &mut usize,
    state: u32,
    reg: &crate::block_registry::BReg,
) -> bool {
    if slot_read_bool(data, p) == Some(true) {
        // `blocks`: a block holder set (tag name or explicit ids).
        let kind = match slot_read_varint(data, p) {
            Some(k) => k,
            None => return false,
        };
        let covered = if kind == 0 {
            match comp_read_string(data, p) {
                Some(tag) => reg.in_tag(state, &tag),
                None => return false,
            }
        } else {
            let count = usize::try_from(kind - 1).ok().filter(|&n| n <= 4096);
            let Some(count) = count else { return false };
            let mut covered = false;
            for _ in 0..count {
                match slot_read_varint(data, p) {
                    Some(v) => {
                        if reg.block_id_of(state) == Some(v as i32) {
                            covered = true;
                        }
                    }
                    None => return false,
                }
            }
            covered
        };
        if !covered {
            return false;
        }
    } else if slot_read_bool(data, p).is_none() {
        return false;
    }
    if slot_read_bool(data, p) == Some(true) {
        // `properties`: exact `name=value` or numeric `[min,max]` matchers.
        let Some(n) = slot_read_count(data, p, 256) else {
            return false;
        };
        for _ in 0..n {
            let Some(name) = comp_read_string(data, p) else {
                return false;
            };
            let current = reg.get_prop(state, &name);
            if slot_read_bool(data, p) == Some(true) {
                let Some(want) = comp_read_string(data, p) else {
                    return false;
                };
                if current.as_deref() != Some(want.as_str()) {
                    return false;
                }
            } else {
                let (Some(min), Some(max)) = (
                    comp_read_string(data, p),
                    comp_read_string(data, p),
                ) else {
                    return false;
                };
                let (Ok(min), Ok(max), Some(cur)) = (
                    min.parse::<i64>(),
                    max.parse::<i64>(),
                    current.as_deref().and_then(|v| v.parse::<i64>().ok()),
                ) else {
                    return false;
                };
                if !(min..=max).contains(&cur) {
                    return false;
                }
            }
        }
    }
    // Optional `nbt` matcher: block entities are not covered here.
    slot_read_optional_nbt(data, p).is_some()
}

#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn stack_can_break(st: &Stack, state: u32, reg: &crate::block_registry::BReg) -> bool {
    match comp_get(&st.2, COMP_CAN_BREAK) {
        Some(data) => block_predicates_allow(data, state, reg),
        None => true,
    }
}

#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn stack_can_place_on(st: &Stack, state: u32, reg: &crate::block_registry::BReg) -> bool {
    match comp_get(&st.2, COMP_CAN_PLACE_ON) {
        Some(data) => block_predicates_allow(data, state, reg),
        None => true,
    }
}

/// `minecraft:dyed_color`: 4-byte RGB float + `show_in_tooltip` flag.
fn stack_dyed_rgb(st: &Stack) -> Option<i64> {
    comp_get(&st.2, COMP_DYED_COLOR)
        .and_then(|d| d.get(..4))
        .map(|b| i64::from(i32::from_be_bytes(b.try_into().unwrap_or_default())))
}

fn dyeable_item(item: i64) -> bool {
    matches!(
        item_to_name(item),
        Some(
            "leather_helmet"
                | "leather_chestplate"
                | "leather_leggings"
                | "leather_boots"
                | "leather_horse_armor"
                | "wolf_armor"
        )
    )
}

/// Vanilla `DyedItemColor` mixing: blend the dye colors into the current color.
fn dyed_color_data(rgb: i64) -> Vec<u8> {
    let mut d = (rgb as i32).to_be_bytes().to_vec();
    d.push(1);
    d
}

const DYE_RGB: [i64; 16] = [
    0xF9FFFE, 0xF9801D, 0xC74EBD, 0x3AB3DA, 0xFED83D, 0x80C71F, 0xF38BAA, 0x474F52,
    0x9D9D97, 0x169C9C, 0x8932B8, 0x3C44AA, 0x835432, 0x5E7C16, 0xB02E26, 0x1D1D21,
];

fn dye_item_rgb(item: i64) -> Option<i64> {
    dye_color_for_item(item)
        .and_then(|name| {
            [
                "white", "orange", "magenta", "light_blue", "yellow", "lime", "pink", "gray",
                "light_gray", "cyan", "purple", "blue", "brown", "green", "red", "black",
            ]
            .iter()
            .position(|n| *n == name)
        })
        .map(|i| DYE_RGB[i])
}

/// Vanilla `DyedItemColor.applyDyes`: average the existing color with the dyes.
fn dyed_craft_result(armor: &Stack, dyes: &[i64]) -> Option<Stack> {
    if !dyeable_item(armor.0) || dyes.is_empty() {
        return None;
    }
    let mut rgbs = vec![stack_dyed_rgb(armor).unwrap_or(0xA06540)];
    rgbs.extend(dyes.iter().filter_map(|&it| dye_item_rgb(it)));
    let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
    for c in &rgbs {
        r += ((c >> 16) & 0xFF) as u64;
        g += ((c >> 8) & 0xFF) as u64;
        b += (c & 0xFF) as u64;
    }
    let n = rgbs.len() as u64;
    let rgb = ((r / n) << 16) | ((g / n) << 8) | (b / n);
    let mut out = (armor.0, 1, armor.2.clone());
    comp_set(&mut out.2, COMP_DYED_COLOR, dyed_color_data(rgb as i64));
    Some(out)
}

/// `minecraft:instrument` sound event for a goat horn. Registry-id holders map
/// to vanilla's `item.goat_horn.sound.N`; inline instrument data carries the
/// sound holder directly.
fn instrument_sound(data: &[u8]) -> Option<String> {
    let mut p = 0;
    let id = slot_read_varint(data, &mut p)?;
    if id != 0 {
        let sound = id - 1;
        if !(0..=7).contains(&sound) {
            return None;
        }
        return Some(format!("minecraft:item.goat_horn.sound.{sound}"));
    }
    // Inline `Instrument` data: sound-event holder, use duration, range, nbt.
    let kind = slot_read_varint(data, &mut p)?;
    if kind != 0 {
        return Some(format!("minecraft:item.goat_horn.sound.{}", (kind - 1).clamp(0, 7)));
    }
    // `slot_read_sound_event`: string id + optional fixed-range data.
    let name = comp_read_string(data, &mut p)?;
    Some(name)
}

/// `minecraft:lodestone_tracker` decode: `{has_global_pos, dimension, pos}` +
/// `tracked`.
fn decode_lodestone_tracker(data: &[u8]) -> Option<(Option<(String, i64)>, bool)> {
    let mut p = 0;
    let target = if slot_read_bool(data, &mut p)? {
        let dim = comp_read_string(data, &mut p)?;
        let pos = i64::from_be_bytes(data.get(p..p + 8)?.try_into().ok()?);
        p += 8;
        Some((dim, pos))
    } else {
        None
    };
    let tracked = slot_read_bool(data, &mut p)?;
    (p == data.len()).then_some((target, tracked))
}

/// `minecraft:lock`: a LockCode — an item predicate in NBT form. A bare string
/// is the legacy custom-name lock; compound keys name component predicates.
#[derive(Clone, Debug, PartialEq)]
enum LockPredicate {
    CustomName(String),
    Components {
        custom_name: Option<String>,
        custom_data: Option<crate::nbt::Tag>,
    },
}

fn lock_predicate_from_tag(tag: &crate::nbt::Tag) -> Option<LockPredicate> {
    match tag {
        crate::nbt::Tag::String(s) => {
            s.to_utf8().ok().map(LockPredicate::CustomName)
        }
        crate::nbt::Tag::Compound(map) => {
            let find = |k: &str| {
                map.iter().find(|(nk, _)| {
                    nk.to_utf8()
                        .map(|n| n == k || n == format!("minecraft:{k}"))
                        .unwrap_or(false)
                })
            };
            let custom_name = find("custom_name")
                .or_else(|| find("name"))
                .and_then(|(_, v)| v.string())
                .and_then(|s| s.to_utf8().ok());
            let custom_data = find("custom_data").map(|(_, v)| v.clone());
            Some(LockPredicate::Components {
                custom_name,
                custom_data,
            })
        }
        _ => None,
    }
}

fn decode_lock_predicate(data: &[u8]) -> Option<LockPredicate> {
    let mut p = 0;
    if let Some(name) = read_nbt_string_bytes(data, &mut p) {
        if p == data.len() {
            return Some(LockPredicate::CustomName(name));
        }
    }
    let mut rest = data;
    crate::nbt::read_network_nbt(&mut rest)
        .ok()
        .filter(|_| rest.is_empty())
        .and_then(|tag| lock_predicate_from_tag(&tag))
}

fn stack_lock_predicate(st: &Stack) -> Option<LockPredicate> {
    comp_get(&st.2, COMP_LOCK).and_then(decode_lock_predicate)
}

/// NBT subset match used by lock `custom_data` predicates: every field of
/// `want` must equal the matching field of `have`.
fn nbt_subset(want: &crate::nbt::Tag, have: &crate::nbt::Tag) -> bool {
    use crate::nbt::Tag;
    match (want, have) {
        (Tag::Compound(w), Tag::Compound(h)) => w.iter().all(|(k, v)| {
            h.iter()
                .find(|(hk, _)| *hk == k)
                .is_some_and(|(_, hv)| nbt_subset(v, hv))
        }),
        _ => want == have,
    }
}

/// `LockableBlockEntity.canOpen`: true when no lock is set, or the held stack
/// satisfies the predicate (vanilla: `LockCode` unlocks on `custom_name` and
/// `custom_data` component equality).
fn container_unlocked(lock: Option<&LockPredicate>, held: &Stack) -> bool {
    let Some(pred) = lock else { return true };
    match pred {
        LockPredicate::CustomName(want) => {
            stack_item_text(held, COMP_CUSTOM_NAME).as_deref() == Some(want.as_str())
        }
        LockPredicate::Components {
            custom_name,
            custom_data,
        } => {
            if let Some(want) = custom_name {
                if stack_item_text(held, COMP_CUSTOM_NAME).as_deref() != Some(want.as_str()) {
                    return false;
                }
            }
            if let Some(want) = custom_data {
                let Some(raw) = comp_get(&held.2, COMP_CUSTOM_DATA) else {
                    return false;
                };
                let mut rest = raw;
                let Ok(have) = crate::nbt::read_network_nbt(&mut rest) else {
                    return false;
                };
                if !nbt_subset(want, &have) {
                    return false;
                }
            }
            true
        }
    }
}

/// The `lock` stored on a block entity, if any (vanilla `LockableBlockEntity`
/// serializes the predicate under `lock`).
fn block_entity_lock(key: DimBlockPos) -> Option<LockPredicate> {
    let (_state, tag) = live_block_entity_snapshot(key)?;
    lock_predicate_from_tag(tag.get("lock")?)
}

/// Deny container/BE-menu access when the block is locked and the held stack
/// does not satisfy the predicate. Returns true when access is allowed.
fn container_lock_gate(c: &Client, key: DimBlockPos) -> bool {
    let Some(pred) = block_entity_lock(key) else { return true };
    let held_index = *c.held.lock().unwrap_or_else(|e| e.into_inner());
    let held = c
        .inv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(36 + held_index))
        .cloned()
        .unwrap_or_default();
    container_unlocked(Some(&pred), &held)
}

/// `minecraft:container`: a `ItemContainerContents` — the shulker box /
/// container item's stored slots, one wire entry per occupied slot.
fn container_slots(st: &Stack) -> Option<Vec<Stack>> {
    comp_get(&st.2, COMP_CONTAINER).and_then(|d| decode_stack_list(d, 256))
}

/// `minecraft:container_loot` (`{loot_table, seed}`) as the staged-loot record
/// `pending_block_loot` consumes when a chest is generated.
fn container_loot_entry(data: &[u8]) -> Option<(String, i64)> {
    let mut rest = data;
    let tag = crate::nbt::read_network_nbt(&mut rest).ok()?;
    let table = tag
        .get("loot_table")
        .or_else(|| tag.get("LootTable"))
        .and_then(|t| t.string())
        .and_then(|s| s.to_utf8().ok())?;
    let seed = tag
        .get("seed")
        .or_else(|| tag.get("LootTableSeed"))
        .and_then(nbt_int_val)
        .unwrap_or(0);
    Some((table, seed))
}

/// `minecraft:entity_data` / `minecraft:bucket_entity_data`: opaque entity NBT
/// merged into the spawn/savegame tag when an egg, armor stand, or mob bucket
/// releases its entity.
fn item_entity_data(st: &Stack, ty: i64) -> Option<crate::nbt::Tag> {
    let raw = comp_get(&st.2, ty)?;
    let mut rest = raw;
    crate::nbt::read_network_nbt(&mut rest).ok()
}

/// `minecraft:block_entity_data`: the block-entity NBT patch a block item
/// applies on placement (shulker contents aside, these merge verbatim).
fn item_block_entity_data(st: &Stack) -> Option<crate::nbt::Tag> {
    item_entity_data(st, COMP_BLOCK_ENTITY_DATA)
}

/// `minecraft:block_state`: `{property: value}` overrides applied to the
/// placed block state, like vanilla `BlockItem.updateCustomBlockEntityTag`'s
/// state application.
#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn item_block_state_override(st: &Stack, base: u32) -> u32 {
    let Some(reg) = breg() else { return base };
    let Some(data) = comp_get(&st.2, COMP_BLOCK_STATE) else {
        return base;
    };
    let mut p = 0;
    let Ok(n) = usize::try_from(slot_read_varint(data, &mut p).unwrap_or(0)) else {
        return base;
    };
    let mut state = base;
    for _ in 0..n.min(256) {
        let (Some(name), Some(value)) =
            (comp_read_string(data, &mut p), comp_read_string(data, &mut p))
        else {
            return state;
        };
        state = reg.with_prop(state, &name, &value);
    }
    state
}

/// `minecraft:bees`: `(entity NBT, ticks_in_hive, min_ticks_in_hive)`.
fn beehive_bees(st: &Stack) -> Vec<(crate::nbt::Tag, i64, i64)> {
    let mut out = Vec::new();
    let Some(data) = comp_get(&st.2, COMP_BEES) else {
        return out;
    };
    let mut p = 0;
    let Some(n) = slot_read_count(data, &mut p, 256) else {
        return out;
    };
    for _ in 0..n {
        let start = p;
        if slot_read_nbt(data, &mut p).is_none() {
            break;
        }
        let mut rest = &data[start..p];
        let Ok(tag) = crate::nbt::read_network_nbt(&mut rest) else {
            break;
        };
        let (Some(a), Some(b)) = (
            slot_read_varint(data, &mut p),
            slot_read_varint(data, &mut p),
        ) else {
            break;
        };
        out.push((tag, a, b));
    }
    out
}

/// `minecraft:pot_decorations`: up to 4 sherd item ids, in
/// decorated-pot `sherds` order (back, left, right, front).
fn pot_item_sherds(st: &Stack) -> Vec<i64> {
    let mut out = Vec::new();
    let Some(data) = comp_get(&st.2, COMP_POT_DECORATIONS) else {
        return out;
    };
    let mut p = 0;
    let Some(n) = slot_read_count(data, &mut p, 4) else {
        return out;
    };
    for _ in 0..n {
        match slot_read_varint(data, &mut p) {
            Some(id) => out.push(id),
            None => break,
        }
    }
    out
}

/// `minecraft:profile` on a player head → the `Skull` block entity's profile
/// fields (`name`, `id`, `properties`).
fn skull_item_profile(st: &Stack) -> Option<crate::nbt::Tag> {
    if item_to_name(st.0) != Some("player_head") {
        return None;
    }
    let data = comp_get(&st.2, COMP_PROFILE)?;
    let mut p = 0;
    let mut fields = std::collections::BTreeMap::new();
    if slot_read_bool(data, &mut p)? {
        if let Some(name) = comp_read_string(data, &mut p) {
            fields.insert(
                crate::nbt::NbtString::text("name"),
                crate::nbt::Tag::String(crate::nbt::NbtString::text(&name)),
            );
        }
    }
    if slot_read_bool(data, &mut p)? {
        if let Some(uuid) = data.get(p..p + 16) {
            p += 16;
            fields.insert(
                crate::nbt::NbtString::text("id"),
                crate::nbt::Tag::IntArray(
                    uuid.chunks(4)
                        .map(|c| i32::from_be_bytes(c.try_into().unwrap_or_default()))
                        .collect(),
                ),
            );
        }
    }
    let n = slot_read_count(data, &mut p, 256)?;
    let mut props = Vec::new();
    for _ in 0..n {
        let (Some(pname), Some(pval)) = (
            comp_read_string(data, &mut p),
            comp_read_string(data, &mut p),
        ) else {
            break;
        };
        let mut entry = std::collections::BTreeMap::new();
        entry.insert(
            crate::nbt::NbtString::text("name"),
            crate::nbt::Tag::String(crate::nbt::NbtString::text(&pname)),
        );
        entry.insert(
            crate::nbt::NbtString::text("value"),
            crate::nbt::Tag::String(crate::nbt::NbtString::text(&pval)),
        );
        if slot_read_bool(data, &mut p)? {
            if let Some(sig) = comp_read_string(data, &mut p) {
                entry.insert(
                    crate::nbt::NbtString::text("signature"),
                    crate::nbt::Tag::String(crate::nbt::NbtString::text(&sig)),
                );
            }
        }
        props.push(crate::nbt::Tag::Compound(entry));
    }
    if !props.is_empty() {
        fields.insert(
            crate::nbt::NbtString::text("properties"),
            crate::nbt::Tag::List(10, props),
        );
    }
    Some(crate::nbt::Tag::Compound(fields))
}

/// `minecraft:base_color` on a banner item → the `Banner` block entity's
/// `base` color index.
fn banner_item_base_color(st: &Stack) -> Option<i64> {
    comp_varint(&st.2, COMP_BASE_COLOR).map(|c| c.clamp(0, 15))
}

/// `minecraft:map_color` RGB as stored on the filled_map item.
fn map_item_color(st: &Stack) -> Option<i64> {
    comp_get(&st.2, COMP_MAP_COLOR)
        .and_then(|d| d.get(..4))
        .map(|b| i64::from(i32::from_be_bytes(b.try_into().unwrap_or_default())))
}

/// `minecraft:map_decorations` raw NBT for the map item's decoration list.
fn map_item_decorations(st: &Stack) -> Option<crate::nbt::Tag> {
    let raw = comp_get(&st.2, COMP_MAP_DECORATIONS)?;
    let mut rest = raw;
    crate::nbt::read_network_nbt(&mut rest).ok()
}

/// `minecraft:debug_stick_state`: `{block: property}` — the selected property
/// per block id that right-click cycles.
fn debug_stick_selection(data: &[u8], block_name: &str) -> Option<String> {
    let mut rest = data;
    let tag = crate::nbt::read_network_nbt(&mut rest).ok()?;
    tag.get(block_name)
        .or_else(|| tag.get(block_name.trim_start_matches("minecraft:")))
        .and_then(|t| t.string())
        .and_then(|s| s.to_utf8().ok())
}

/// Property-value candidates used to cycle a selected debug-stick property.
/// `with_prop` validates membership, so probing a table wider than the
/// property's domain is safe.
const DEBUG_STICK_PROP_VALUES: &[&str] = &[
    "true", "false",
    "north", "east", "south", "west", "up", "down",
    "x", "y", "z",
    "top", "bottom", "double", "upper", "lower",
    "none", "low", "tall", "side", "tip", "tip_merge", "middle", "frustum",
    "single", "front", "back", "left", "right",
    "normal", "sticky", "floor", "wall", "ceiling",
    "unstable", "persistent",
    "short", "half", "partial", "full",
    "small", "large", "left_arm", "right_arm", "center",
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9",
    "10", "11", "12", "13", "14", "15",
    "16", "17", "18", "19", "20", "21", "22", "23", "24", "25",
];

/// Cycle the `debug_stick_state`-selected property of `state` to its next
/// value in the property's declared order (vanilla `DebugStickItem.useOn`).
fn debug_stick_cycle(state: u32, comps: &Comps) -> Option<u32> {
    let reg = breg()?;
    let block_name = reg.name_of(state);
    let data = comp_get(comps, COMP_DEBUG_STICK_STATE)?;
    let prop = debug_stick_selection(data, block_name)
        .or_else(|| reg.properties_of(state)?.first().map(|(n, _)| n.clone()))?;
    let current = reg.get_prop(state, &prop)?;
    // Probe the candidate table, preserving the registry's value order by
    // walking the block's state range: for each candidate value the registry
    // accepts, collect the resulting state.
    let mut values: Vec<String> = Vec::new();
    for candidate in DEBUG_STICK_PROP_VALUES {
        let ns = reg.with_prop(state, &prop, candidate);
        if reg.get_prop(ns, &prop).as_deref() == Some(*candidate) {
            values.push((*candidate).to_string());
        }
    }
    if values.is_empty() {
        return None;
    }
    let next = values
        .iter()
        .position(|v| *v == current)
        .map(|i| (i + 1) % values.len())
        .unwrap_or(0);
    Some(reg.with_prop(state, &prop, &values[next]))
}

/// `minecraft:recipes` on a knowledge book: recipe ids to grant.
fn knowledge_book_recipes(st: &Stack) -> Vec<String> {
    let Some(raw) = comp_get(&st.2, COMP_RECIPES) else {
        return Vec::new();
    };
    let mut rest = raw;
    let Ok(tag) = crate::nbt::read_network_nbt(&mut rest) else {
        return Vec::new();
    };
    let entries = tag
        .get("recipes")
        .and_then(|t| t.list())
        .map(<[_]>::to_vec)
        .unwrap_or_else(|| vec![tag]);
    entries
        .iter()
        .filter_map(|t| t.string().and_then(|s| s.to_utf8().ok()))
        .collect()
}

fn learned_recipes() -> &'static Mutex<HashMap<i32, std::collections::BTreeSet<String>>> {
    static LEARNED: Registry<i32, std::collections::BTreeSet<String>> = OnceLock::new();
    LEARNED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `minecraft:fireworks`: flight duration (in gunpowder units) + explosions.
fn decode_fireworks(data: &[u8]) -> Option<(i64, i64)> {
    let mut p = 0;
    let flight = slot_read_varint(data, &mut p)?;
    if !(0..=255).contains(&flight) {
        return None;
    }
    let n = slot_read_count(data, &mut p, 256)?;
    for _ in 0..n {
        slot_read_firework_explosion(data, &mut p)?;
    }
    (p == data.len()).then_some((flight, n as i64))
}

/// A `minecraft:firework_explosion` payload: shape + color lists + flags.
#[derive(Clone, Debug, PartialEq)]
struct FireworkExplosion {
    shape: i64,
    colors: Vec<i32>,
    fade: Vec<i32>,
    has_trail: bool,
    has_twinkle: bool,
}

fn decode_firework_explosion(data: &[u8]) -> Option<FireworkExplosion> {
    let mut p = 0;
    let shape = slot_read_varint(data, &mut p)?;
    if !(0..=4).contains(&shape) {
        return None;
    }
    let read_colors = |p: &mut usize| -> Option<Vec<i32>> {
        let n = slot_read_count(data, p, 256)?;
        let bytes = data.get(*p..*p + n * 4)?;
        *p += n * 4;
        Some(
            bytes
                .chunks(4)
                .map(|c| i32::from_be_bytes(c.try_into().unwrap_or_default()))
                .collect(),
        )
    };
    let colors = read_colors(&mut p)?;
    let fade = read_colors(&mut p)?;
    let has_trail = slot_read_bool(data, &mut p)?;
    let has_twinkle = slot_read_bool(data, &mut p)?;
    (p == data.len()).then_some(FireworkExplosion {
        shape,
        colors,
        fade,
        has_trail,
        has_twinkle,
    })
}

/// `minecraft:charged_projectiles` on a crossbow: the loaded stacks.
fn charged_projectiles_of(st: &Stack) -> Option<Vec<Stack>> {
    comp_get(&st.2, COMP_CHARGED_PROJECTILES)
        .and_then(|d| decode_stack_list(d, 3))
        .map(|v| {
            v.into_iter()
                .filter(|s| s.1 > 0)
                .collect()
        })
}

/// Whether a dropped stack survives fire/lava (vanilla
/// `ItemEntity`/`FIRE_RESISTANT` — netherite items carry it by default).
fn drop_fireproof(item: i64, comps: &Comps) -> bool {
    comp_get(comps, COMP_FIRE_RESISTANT).is_some()
        || item_to_name(item).is_some_and(|n| n.starts_with("netherite_"))
}

/// Block states that burn a dropped item: fire, soul fire, and lava.
fn drop_burns_at(state_name: &str) -> bool {
    matches!(
        state_name,
        "minecraft:fire" | "minecraft:soul_fire" | "minecraft:lava"
    )
}

/// Typed semantic result of `apply_item_component_behavior`. Each arm carries
/// what a gameplay path needs from the component; `Marker` covers components
/// whose server-side contract is validation + wire passthrough only.
#[derive(Clone, Debug, PartialEq)]
enum ComponentEffect {
    Marker,
    StackLimit(i64),
    Rarity(i64),
    DisplayName(String),
    Glint(bool),
    RepairCost(i64),
    ModelData(i64),
    Food(FoodComponent),
    Tool(ToolComponent),
    StackList(Vec<Stack>),
    StewEffects(Vec<(i64, i64)>),
    Instrument(String),
    Amplifier(i64),
    MapId(i64),
    MapPostProcessing(i64),
    BaseColor(i64),
    Sherds(Vec<i64>),
    OpaqueNbt(crate::nbt::Tag),
    Text,
    Lock(LockPredicate),
    LootEntry((String, i64)),
    Explosion(FireworkExplosion),
    Fireworks(i64, i64),
    Lodestone(Option<(String, i64)>, bool),
    Bees(usize),
    Profile(crate::nbt::Tag),
    Sound(String),
    BlockOverrides(usize),
    DamageVarint(i64),
    Predicates(usize),
    Recipes(Vec<String>),
}

/// `apply_item_component_behavior`: the semantic entry point for
/// `minecraft:data_component_type` payloads. Validates the payload against
/// the component's vanilla domain and decodes the fields gameplay paths
/// consume. Returns `None` for out-of-registry ids or malformed payloads —
/// callers use it to reject stacks the wire reader accepted structurally but
/// whose values are out of domain.
fn apply_item_component_behavior(ty: i64, data: &[u8]) -> Option<ComponentEffect> {
    match ty {
        COMP_CUSTOM_DATA
        | COMP_INTANGIBLE_PROJECTILE
        | COMP_DEBUG_STICK_STATE
        | COMP_RECIPES => {
            let mut p = 0;
            slot_read_nbt(data, &mut p)?;
            if p != data.len() {
                return None;
            }
            if ty == COMP_RECIPES {
                let mut st = (0, 1, Vec::new());
                comp_set(&mut st.2, COMP_RECIPES, data.to_vec());
                return Some(ComponentEffect::Recipes(knowledge_book_recipes(&st)));
            }
            let mut rest = data;
            match crate::nbt::read_network_nbt(&mut rest) {
                Ok(tag) => Some(ComponentEffect::OpaqueNbt(tag)),
                Err(_) => Some(ComponentEffect::Marker),
            }
        }
        COMP_ENTITY_DATA | COMP_BUCKET_ENTITY_DATA => {
            let tmp = (0, 1, vec![(ty, data.to_vec())]);
            item_entity_data(&tmp, ty).map(ComponentEffect::OpaqueNbt)
        }
        COMP_BLOCK_ENTITY_DATA => {
            let tmp = (0, 1, vec![(ty, data.to_vec())]);
            item_block_entity_data(&tmp).map(ComponentEffect::OpaqueNbt)
        }
        COMP_MAP_DECORATIONS => {
            let tmp = (0, 1, vec![(ty, data.to_vec())]);
            map_item_decorations(&tmp).map(ComponentEffect::OpaqueNbt)
        }
        COMP_MAX_STACK_SIZE => {
            let mut p = 0;
            let n = slot_read_varint(data, &mut p)?;
            ((1..=99).contains(&n) && p == data.len())
                .then_some(ComponentEffect::StackLimit(n))
        }
        COMP_MAX_DAMAGE => {
            let mut p = 0;
            let n = slot_read_varint(data, &mut p)?;
            (n > 0 && p == data.len()).then_some(ComponentEffect::DamageVarint(n))
        }
        COMP_DAMAGE => {
            let mut p = 0;
            let n = slot_read_varint(data, &mut p)?;
            (n >= 0 && p == data.len()).then_some(ComponentEffect::DamageVarint(n))
        }
        COMP_UNBREAKABLE | COMP_ENCHANTMENT_GLINT_OVERRIDE => {
            let mut p = 0;
            slot_read_bool(data, &mut p)?;
            if p != data.len() {
                return None;
            }
            let tmp = (0, 1, vec![(ty, data.to_vec())]);
            Some(ComponentEffect::Glint(stack_enchantment_glint(&tmp)))
        }
        COMP_CUSTOM_NAME | COMP_ITEM_NAME => {
            comp_text_string(data).map(ComponentEffect::DisplayName)
        }
        COMP_LORE => {
            let mut p = 0;
            let n = slot_read_count(data, &mut p, 256)?;
            for _ in 0..n {
                slot_read_optional_nbt(data, &mut p)?;
            }
            (p == data.len()).then_some(ComponentEffect::Text)
        }
        COMP_RARITY => {
            let mut p = 0;
            let v = slot_read_varint(data, &mut p)?;
            ((0..=3).contains(&v) && p == data.len())
                .then_some(ComponentEffect::Rarity(v))
        }
        COMP_CAN_PLACE_ON | COMP_CAN_BREAK => {
            let mut p = 0;
            let n = slot_read_count(data, &mut p, 256)?;
            for _ in 0..n {
                slot_read_block_predicate(data, &mut p)?;
            }
            slot_read_bool(data, &mut p)?;
            (p == data.len()).then_some(ComponentEffect::Predicates(n))
        }
        COMP_ATTRIBUTE_MODIFIERS => {
            let mut rest = data;
            attr_modifiers_read(&mut rest)?;
            rest.is_empty().then_some(ComponentEffect::Marker)
        }
        COMP_CUSTOM_MODEL_DATA | COMP_REPAIR_COST | COMP_MAP_ID => {
            let mut p = 0;
            let v = slot_read_varint(data, &mut p)?;
            if p != data.len() {
                return None;
            }
            if ty == COMP_MAP_ID {
                (v >= 0).then_some(ComponentEffect::MapId(v))
            } else if ty == COMP_REPAIR_COST {
                (v >= 0).then_some(ComponentEffect::RepairCost(v))
            } else {
                Some(ComponentEffect::ModelData(v))
            }
        }
        COMP_HIDE_ADDITIONAL_TOOLTIP
        | COMP_HIDE_TOOLTIP
        | COMP_CREATIVE_SLOT_LOCK
        | COMP_FIRE_RESISTANT => {
            data.is_empty().then_some(ComponentEffect::Marker)
        }
        COMP_FOOD => decode_food_component(data).map(ComponentEffect::Food),
        COMP_TOOL => decode_tool_component(data).map(ComponentEffect::Tool),
        COMP_ENCHANTMENTS | COMP_STORED_ENCHANTMENTS => {
            let mut p = 0;
            let n = slot_read_count(data, &mut p, 256)?;
            for _ in 0..n {
                slot_read_varint(data, &mut p)?;
                slot_read_varint(data, &mut p)?;
            }
            slot_read_bool(data, &mut p)?;
            (p == data.len()).then_some(ComponentEffect::Marker)
        }
        COMP_DYED_COLOR => {
            let mut p = 0;
            comp_read_f32(data, &mut p)?;
            slot_read_bool(data, &mut p)?;
            (p == data.len()).then_some(ComponentEffect::Marker)
        }
        COMP_MAP_COLOR => {
            let tmp = (0, 1, vec![(ty, data.to_vec())]);
            map_item_color(&tmp).map(|_| ComponentEffect::Marker)
        }
        COMP_MAP_POST_PROCESSING => {
            let mut p = 0;
            let v = slot_read_varint(data, &mut p)?;
            ((0..=1).contains(&v) && p == data.len())
                .then_some(ComponentEffect::MapPostProcessing(v))
        }
        COMP_CHARGED_PROJECTILES => {
            decode_stack_list(data, 3).map(ComponentEffect::StackList)
        }
        COMP_BUNDLE_CONTENTS => {
            let stacks = decode_stack_list(data, 256)?;
            // Vanilla `BundleItem` never holds more than the 64-weight budget;
            // `bundle_total_weight` runs the same accounting `click_slot` uses.
            let tmp = (
                name_to_item("bundle").unwrap_or(0),
                1,
                vec![(ty, data.to_vec())],
            );
            (bundle_total_weight(&tmp) <= BUNDLE_WEIGHT_MAX).then_some(
                ComponentEffect::StackList(
                    stacks.into_iter().filter(|s| s.1 > 0).collect::<Vec<_>>(),
                ),
            )
        }
        COMP_CONTAINER => {
            let tmp = (0, 1, vec![(ty, data.to_vec())]);
            container_slots(&tmp).map(ComponentEffect::StackList)
        }
        COMP_POTION_CONTENTS => {
            let mut rest = data;
            if read_merchant_bool(&mut rest)? {
                read_protocol_i32(&mut rest)?;
            }
            if read_merchant_bool(&mut rest)? {
                rest = rest.get(4..)?;
            }
            for _ in 0..read_protocol_i32(&mut rest).filter(|&n| (0..=256).contains(&n))? {
                read_protocol_i32(&mut rest)?;
                read_live_potion_effect_details(&mut rest, 0)?;
            }
            let consumed = data.len() - rest.len();
            let mut p = consumed;
            slot_read_optional_string(data, &mut p)?;
            (p == data.len()).then_some(ComponentEffect::Marker)
        }
        COMP_SUSPICIOUS_STEW_EFFECTS => {
            decode_suspicious_stew_effects(data).map(ComponentEffect::StewEffects)
        }
        COMP_WRITABLE_BOOK_CONTENT => {
            let mut p = 0;
            let n = slot_read_count(data, &mut p, 100)?;
            for _ in 0..n {
                slot_read_string(data, &mut p)?;
                slot_read_optional_string(data, &mut p)?;
            }
            (p == data.len()).then_some(ComponentEffect::Text)
        }
        COMP_WRITTEN_BOOK_CONTENT => {
            let mut p = 0;
            slot_read_string(data, &mut p)?;
            slot_read_optional_string(data, &mut p)?;
            slot_read_string(data, &mut p)?;
            let generation = slot_read_varint(data, &mut p)?;
            if !(0..=3).contains(&generation) {
                return None;
            }
            let n = slot_read_count(data, &mut p, 100)?;
            for _ in 0..n {
                slot_read_text(data, &mut p)?;
                slot_read_optional_nbt(data, &mut p)?;
            }
            slot_read_bool(data, &mut p)?;
            (p == data.len()).then_some(ComponentEffect::Text)
        }
        COMP_TRIM => {
            let mut p = 0;
            if slot_read_varint(data, &mut p)? == 0 {
                slot_read_trim_material(data, &mut p)?;
            }
            if slot_read_varint(data, &mut p)? == 0 {
                slot_read_trim_pattern(data, &mut p)?;
            }
            slot_read_bool(data, &mut p)?;
            (p == data.len()).then_some(ComponentEffect::Marker)
        }
        COMP_INSTRUMENT => {
            instrument_sound(data).map(ComponentEffect::Instrument)
        }
        COMP_OMINOUS_BOTTLE_AMPLIFIER => {
            let tmp = (0, 1, vec![(ty, data.to_vec())]);
            ominous_bottle_amplifier(&tmp).map(ComponentEffect::Amplifier)
        }
        COMP_JUKEBOX_PLAYABLE => {
            let mut p = 0;
            if slot_read_bool(data, &mut p)? {
                if slot_read_varint(data, &mut p)? == 0 {
                    slot_read_jukebox_song_data(data, &mut p)?;
                }
            } else {
                slot_read_string(data, &mut p)?;
            }
            slot_read_bool(data, &mut p)?;
            (p == data.len()).then_some(ComponentEffect::Marker)
        }
        COMP_LODESTONE_TRACKER => {
            decode_lodestone_tracker(data)
                .map(|(target, tracked)| ComponentEffect::Lodestone(target, tracked))
        }
        COMP_FIREWORK_EXPLOSION => {
            decode_firework_explosion(data).map(ComponentEffect::Explosion)
        }
        COMP_FIREWORKS => {
            decode_fireworks(data).map(|(flight, n)| ComponentEffect::Fireworks(flight, n))
        }
        COMP_PROFILE => {
            let mut st = (
                name_to_item("player_head").unwrap_or(0),
                1,
                vec![(COMP_PROFILE, data.to_vec())],
            );
            let _ = &mut st;
            skull_item_profile(&st).map(ComponentEffect::Profile)
        }
        COMP_NOTE_BLOCK_SOUND => {
            let mut p = 0;
            comp_read_string(data, &mut p)
                .filter(|_| p == data.len())
                .map(ComponentEffect::Sound)
        }
        COMP_BANNER_PATTERNS => {
            let mut p = 0;
            let n = slot_read_count(data, &mut p, 256)?;
            for _ in 0..n {
                if slot_read_varint(data, &mut p)? == 0 {
                    slot_read_string(data, &mut p)?;
                    slot_read_string(data, &mut p)?;
                }
                if !(0..=15).contains(&slot_read_varint(data, &mut p)?) {
                    return None;
                }
            }
            (p == data.len()).then_some(ComponentEffect::Marker)
        }
        COMP_BASE_COLOR => {
            let tmp = (0, 1, vec![(ty, data.to_vec())]);
            banner_item_base_color(&tmp).map(ComponentEffect::BaseColor)
        }
        COMP_POT_DECORATIONS => {
            let mut p = 0;
            let n = slot_read_count(data, &mut p, 4)?;
            for _ in 0..n {
                slot_read_varint(data, &mut p)?;
            }
            if p != data.len() {
                return None;
            }
            let tmp = (0, 1, vec![(ty, data.to_vec())]);
            Some(ComponentEffect::Sherds(pot_item_sherds(&tmp)))
        }
        COMP_BLOCK_STATE => {
            let mut p = 0;
            let n = slot_read_count(data, &mut p, 256)?;
            for _ in 0..n {
                slot_read_string(data, &mut p)?;
                slot_read_string(data, &mut p)?;
            }
            (p == data.len()).then_some(ComponentEffect::BlockOverrides(n))
        }
        COMP_BEES => {
            let mut p = 0;
            let n = slot_read_count(data, &mut p, 256)?;
            for _ in 0..n {
                slot_read_nbt(data, &mut p)?;
                slot_read_varint(data, &mut p)?;
                slot_read_varint(data, &mut p)?;
            }
            if p != data.len() {
                return None;
            }
            let tmp = (0, 1, vec![(ty, data.to_vec())]);
            Some(ComponentEffect::Bees(beehive_bees(&tmp).len()))
        }
        COMP_LOCK => {
            let tmp = (0, 1, vec![(ty, data.to_vec())]);
            stack_lock_predicate(&tmp).map(ComponentEffect::Lock)
        }
        COMP_CONTAINER_LOOT => {
            container_loot_entry(data).map(ComponentEffect::LootEntry)
        }
        _ => None,
    }
}

/// Write a varint-length UTF-8 string in the slot/component wire form.
fn comp_write_string(out: &mut Vec<u8>, text: &str) {
    write_varint(out, text.len() as i64);
    out.extend_from_slice(text.as_bytes());
}

/// `useItemOn` handling for component-driven block interactions that are not
/// block-entity menus: lodestone compasses, cauldron dye washing, and debug
/// sticks. Returns true when the interaction consumed the use.
fn use_item_component_on_block(
    c: &Client,
    key @ (dim, x, y, z): DimBlockPos,
    state: u32,
) -> bool {
    let Some(reg) = breg() else { return false };
    let block_name = reg.name_of(state);
    let held_index = *c.held.lock().unwrap_or_else(|e| e.into_inner());
    let selected = 36 + held_index;
    let held = {
        let inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
        inv.get(&selected).cloned().unwrap_or_default()
    };
    if held.1 == 0 {
        return false;
    }
    // LodestoneCompassItem.useOn: bind the compass to this lodestone.
    if block_name == "minecraft:lodestone"
        && held.0 == name_to_item("compass").unwrap_or(-1)
    {
        let pos = ((i64::from(x) & 0x3FFFFFF) << 38)
            | ((i64::from(z) & 0x3FFFFFF) << 12)
            | (i64::from(y) & 0xFFF);
        let mut data = vec![1u8];
        comp_write_string(&mut data, dim.dimension_id());
        data.extend_from_slice(&pos.to_be_bytes());
        data.push(1);
        let mut st = held;
        comp_set(&mut st.2, COMP_LODESTONE_TRACKER, data);
        c.inv.lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(selected, st);
        held_slot_resync(c);
        return true;
    }
    // LayeredCauldronBlock + DyedItemColor: washing a dyed armor piece removes
    // `dyed_color` and consumes one water level.
    if block_name == "minecraft:water_cauldron"
        && comp_get(&held.2, COMP_DYED_COLOR).is_some()
        && dyeable_item(held.0)
    {
        let level: i64 = reg
            .get_prop(state, "level")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let mut st = held;
        st.2.retain(|(ty, _)| *ty != COMP_DYED_COLOR);
        c.inv.lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(selected, st);
        let next = if level <= 1 {
            name_to_state("cauldron").unwrap_or(state)
        } else {
            reg.with_prop(state, "level", &(level - 1).to_string())
        };
        if next != state {
            update_live_block_entity_state(key, state, next);
        }
        held_slot_resync(c);
        return true;
    }
    // DebugStickItem.useOn: cycle the component-selected property value.
    if held.0 == name_to_item("debug_stick").unwrap_or(-1) {
        if let Some(next) = debug_stick_cycle(state, &held.2) {
            if next != state {
                update_live_block_entity_state(key, state, next);
            }
            return true;
        }
    }
    false
}

/// Fire a loaded crossbow: each `charged_projectiles` entry becomes a real
/// projectile (arrows and firework rockets), then the component clears.
fn crossbow_fire_loaded(me: &Arc<Client>, held_slot: i16) {
    let selected = 36 + held_slot;
    let held = me
        .inv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&selected)
        .cloned()
        .unwrap_or_default();
    let Some(projectiles) = charged_projectiles_of(&held) else {
        return;
    };
    if projectiles.is_empty() {
        return;
    }
    {
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(st) = inv.get_mut(&selected) {
            st.2.retain(|(ty, _)| *ty != COMP_CHARGED_PROJECTILES);
        }
    }
    let (x, y, z, yaw, pitch, _g) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
    let yr = (yaw as f64).to_radians();
    let pr = (pitch as f64).to_radians();
    let multi = enchant_level(&held.2, 23) > 0;
    let rocket_item = name_to_item("firework_rocket").unwrap_or(-1);
    let launch = |proj: &Stack, spread: f64| {
        let yr2 = yr + spread;
        let (dx, dy, dz) = (
            -yr2.sin() * pr.cos() * 3.15,
            -pr.sin() * 3.15,
            yr2.cos() * pr.cos() * 3.15,
        );
        if proj.0 == rocket_item {
            let (flight, explosions) = comp_get(&proj.2, COMP_FIREWORKS)
                .and_then(decode_fireworks)
                .unwrap_or((1, 0));
            fireworks().lock().unwrap_or_else(|e| e.into_inner()).push(Firework {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                owner: me.eid,
                x,
                y: y + 1.5,
                z,
                dx,
                dy,
                dz,
                ttl: server_ticks((flight.max(0) * 10 + 11) as u64) as i32,
                explosions,
            });
        } else {
            arrows().lock().unwrap_or_else(|e| e.into_inner()).push(Arrow {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                owner: me.eid,
                x,
                y: y + 1.5,
                z,
                dx,
                dy,
                dz,
                ttl: server_ticks(ARROW_TTL_GT) as i32,
                dmg: 9.0,
                fire: 0,
                punch: 0,
                kind: 0,
                loy: 0,
                ret_item: proj.0,
                ret_comps: proj.2.clone(),
                pierce: enchant_level(&held.2, 24).clamp(0, 4) as i32,
                last_hit: -1,
                stuck: false,
            });
        }
    };
    for (i, proj) in projectiles.iter().enumerate() {
        let spread = if projectiles.len() > 1 { (i as f64 - 1.0) * 0.1745 } else { 0.0 };
        launch(proj, spread);
    }
    if multi && projectiles.len() == 1 {
        launch(&projectiles[0], -0.1745);
        launch(&projectiles[0], 0.1745);
    }
    if damage_held(me) {
        let snd = sound_pkt_named("entity.item.break", x, y + 1.0, z);
        broadcast_near(
            *me.dim.lock().unwrap_or_else(|e| e.into_inner()),
            x,
            y + 1.0,
            z,
            -1,
            |_c| (0x68, snd.clone()),
        );
    }
    held_slot_resync(me);
}

/// `instrument` on a goat horn: broadcast the component's sound event and run
/// the vanilla 7-second cooldown.
fn goat_horn_use(me: &Arc<Client>, held_slot: i16) {
    static GOAT_HORN_CD: Registry<i32, Instant> = OnceLock::new();
    let cd = GOAT_HORN_CD.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let cds = cd.lock().unwrap_or_else(|e| e.into_inner());
        if cds
            .get(&me.eid)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(7))
        {
            return;
        }
    }
    let selected = 36 + held_slot;
    let held = me
        .inv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&selected)
        .cloned()
        .unwrap_or_default();
    let sound = comp_get(&held.2, COMP_INSTRUMENT)
        .and_then(instrument_sound)
        .unwrap_or_else(|| "minecraft:item.goat_horn.sound.0".to_string());
    cd.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(me.eid, Instant::now());
    {
        let mut cp = Vec::new();
        write_varint(&mut cp, held.0.max(1));
        write_varint(&mut cp, 140);
        let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x17, cp);
    }
    let (x, y, z, ..) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
    let snd = sound_pkt_named(&sound, x, y + 1.0, z);
    broadcast_near(
        *me.dim.lock().unwrap_or_else(|e| e.into_inner()),
        x,
        y + 1.0,
        z,
        -1,
        |_c| (0x68, snd.clone()),
    );
}

/// `recipes` on a knowledge book: grant each recipe id to the reader and
/// consume the book, like `KnowledgeBookItem.use`.
fn knowledge_book_use(me: &Arc<Client>, held_slot: i16) {
    let selected = 36 + held_slot;
    let held = me
        .inv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&selected)
        .cloned()
        .unwrap_or_default();
    let recipes = knowledge_book_recipes(&held);
    if !recipes.is_empty() {
        let mut learned = learned_recipes().lock().unwrap_or_else(|e| e.into_inner());
        learned.entry(me.eid).or_default().extend(recipes);
    }
    if *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1 {
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(st) = inv.get_mut(&selected) {
            st.1 = st.1.saturating_sub(1);
            if st.1 == 0 {
                inv.remove(&selected);
            }
        }
    }
    held_slot_resync(me);
}

/// `fireworks` on a rocket item: launch a firework entity whose flight time
/// and explosions come from the component.
fn firework_rocket_use(me: &Arc<Client>, held_slot: i16) {
    let selected = 36 + held_slot;
    let held = me
        .inv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&selected)
        .cloned()
        .unwrap_or_default();
    if held.1 == 0 {
        return;
    }
    let (flight, explosions) = comp_get(&held.2, COMP_FIREWORKS)
        .and_then(decode_fireworks)
        .unwrap_or((3, 0));
    let (x, y, z, yaw, pitch, _g) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
    let yr = (yaw as f64).to_radians();
    let pr = (pitch as f64).to_radians();
    fireworks().lock().unwrap_or_else(|e| e.into_inner()).push(Firework {
        eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
        owner: me.eid,
        x,
        y: y + 1.5,
        z,
        dx: -yr.sin() * pr.cos() * 1.5,
        dy: -pr.sin() * 1.5 + 0.1,
        dz: yr.cos() * pr.cos() * 1.5,
        ttl: server_ticks((flight.max(1) * 10 + 11) as u64) as i32
            + (world_rng::next_u64() % 6) as i32,
        explosions,
    });
    if *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1 {
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(st) = inv.get_mut(&selected) {
            st.1 = st.1.saturating_sub(1);
            if st.1 == 0 {
                inv.remove(&selected);
            }
        }
    }
    held_slot_resync(me);
}

/// Shapeless `armor_dye`: exactly one dyeable armor piece plus at least one dye
/// item produces the color-mixed result through `dyed_craft_result`.
fn dyed_craft_any<'a>(stacks: impl Iterator<Item = &'a Stack>) -> Option<Stack> {
    let mut armor: Option<&Stack> = None;
    let mut dyes: Vec<i64> = Vec::new();
    for st in stacks.filter(|st| st.1 > 0) {
        if dyeable_item(st.0) && armor.is_none() {
            armor = Some(st);
        } else if dye_item_rgb(st.0).is_some() {
            dyes.push(st.0);
        } else {
            return None;
        }
    }
    let armor = armor?;
    dyed_craft_result(armor, &dyes)
}

/// The saved-NBT form for book-content components. Written books keep their
/// fields; pages are stored as raw wire bytes so `to_stack` round-trips every
/// entry byte-for-byte (a `pages` list still feeds `Lectern::page_count`).
fn book_content_nbt(
    ty: i64,
    data: &[u8],
) -> Option<BlockEntityNbtFields> {
    use crate::nbt::{NbtString, Tag};
    let mut p = 0;
    let mut fields = BlockEntityNbtFields::new();
    if ty == COMP_WRITABLE_BOOK_CONTENT {
        let n = slot_read_count(data, &mut p, 100)?;
        let mut pages = Vec::with_capacity(n);
        for _ in 0..n {
            let start = p;
            slot_read_string(data, &mut p)?;
            slot_read_optional_string(data, &mut p)?;
            pages.push(Tag::ByteArray(data.get(start..p)?.to_vec()));
        }
        fields.insert(
            NbtString::text("pages"),
            Tag::List(if pages.is_empty() { 0 } else { 7 }, pages),
        );
        return (p == data.len()).then_some(fields);
    }
    // written_book_content: raw_title, optional filtered_title, author,
    // generation, pages (text + optional filtered), resolved.
    let title = comp_read_string(data, &mut p)?;
    fields.insert(
        NbtString::text("title"),
        Tag::String(NbtString::text(&title)),
    );
    if slot_read_bool(data, &mut p)? {
        let filtered = comp_read_string(data, &mut p)?;
        fields.insert(
            NbtString::text("filtered_title"),
            Tag::String(NbtString::text(&filtered)),
        );
    }
    let author = comp_read_string(data, &mut p)?;
    fields.insert(
        NbtString::text("author"),
        Tag::String(NbtString::text(&author)),
    );
    let generation = slot_read_varint(data, &mut p)?;
    fields.insert(NbtString::text("generation"), Tag::Int(generation as i32));
    let n = slot_read_count(data, &mut p, 100)?;
    let mut pages = Vec::with_capacity(n);
    for _ in 0..n {
        let start = p;
        slot_read_text(data, &mut p)?;
        slot_read_optional_nbt(data, &mut p)?;
        pages.push(Tag::ByteArray(data.get(start..p)?.to_vec()));
    }
    fields.insert(
        NbtString::text("pages"),
        Tag::List(if pages.is_empty() { 0 } else { 7 }, pages),
    );
    let resolved = slot_read_bool(data, &mut p)?;
    fields.insert(NbtString::text("resolved"), Tag::Byte(i8::from(resolved)));
    (p == data.len()).then_some(fields)
}

fn book_content_data(ty: i64, tag: &crate::nbt::Tag) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    let pages = tag.get("pages").and_then(|t| t.list())?;
    if ty == COMP_WRITABLE_BOOK_CONTENT {
        if pages.len() > 100 {
            return None;
        }
        write_varint(&mut data, pages.len() as i64);
        for page in pages {
            let crate::nbt::Tag::ByteArray(entry) = page else {
                return None;
            };
            data.extend_from_slice(entry);
        }
        return Some(data);
    }
    let title = tag
        .get("title")
        .and_then(|t| t.string())
        .and_then(|s| s.to_utf8().ok())?;
    comp_write_string(&mut data, &title);
    match tag
        .get("filtered_title")
        .and_then(|t| t.string())
        .and_then(|s| s.to_utf8().ok())
    {
        Some(filtered) => {
            data.push(1);
            comp_write_string(&mut data, &filtered);
        }
        None => data.push(0),
    }
    let author = tag
        .get("author")
        .and_then(|t| t.string())
        .and_then(|s| s.to_utf8().ok())?;
    comp_write_string(&mut data, &author);
    let generation = tag.get("generation").and_then(nbt_int_val).unwrap_or(0);
    write_varint(&mut data, generation);
    if pages.len() > 100 {
        return None;
    }
    write_varint(&mut data, pages.len() as i64);
    for page in pages {
        let crate::nbt::Tag::ByteArray(entry) = page else {
            return None;
        };
        data.extend_from_slice(entry);
    }
    let resolved = match tag.get("resolved") {
        Some(crate::nbt::Tag::Byte(b)) => *b != 0,
        _ => true,
    };
    data.push(u8::from(resolved));
    Some(data)
}

/// Saved-NBT form for `jukebox_playable`: `{song, show_in_tooltip}` for the
/// direct-song form; the holder form stores nothing and falls back to the
/// raw-byte preservation arm.
fn jukebox_playable_nbt(data: &[u8]) -> Option<crate::nbt::Tag> {
    use crate::nbt::{NbtString, Tag};
    let mut p = 0;
    if slot_read_bool(data, &mut p)? {
        return None;
    }
    let song = comp_read_string(data, &mut p)?;
    let show = slot_read_bool(data, &mut p)?;
    if p != data.len() {
        return None;
    }
    let mut fields = BlockEntityNbtFields::new();
    fields.insert(
        NbtString::text("song"),
        Tag::String(NbtString::text(&song)),
    );
    fields.insert(
        NbtString::text("show_in_tooltip"),
        Tag::Byte(i8::from(show)),
    );
    Some(Tag::Compound(fields))
}

fn jukebox_playable_data(tag: &crate::nbt::Tag) -> Option<Vec<u8>> {
    let song = tag
        .get("song")
        .and_then(|t| t.string())
        .and_then(|s| s.to_utf8().ok())?;
    let show = match tag.get("show_in_tooltip") {
        Some(crate::nbt::Tag::Byte(b)) => *b != 0,
        _ => true,
    };
    let mut data = vec![0u8];
    comp_write_string(&mut data, &song);
    data.push(u8::from(show));
    Some(data)
}

#[cfg(test)]
mod v1211_comp_tests {
    use super::*;

    fn vi(n: i64) -> Vec<u8> {
        let mut v = Vec::new();
        write_varint(&mut v, n);
        v
    }

    /// TAG_String NBT form used by `custom_name`/`lock` wire payloads.
    fn nbt_str(text: &str) -> Vec<u8> {
        let mut v = vec![0x08u8];
        v.extend_from_slice(&(text.len() as u16).to_be_bytes());
        v.extend_from_slice(text.as_bytes());
        v
    }

    fn item(name: &str) -> i64 {
        name_to_item(name).unwrap_or_else(|| panic!("unknown item {name}"))
    }

    /// `minecraft:food` payload: nutrition, saturation, can_always_eat,
    /// eat_seconds, empty converts_to stack, then `(effect, probability)` rows.
    fn food_payload(nutrition: i64, saturation: f32, effects: &[(i64, f32)]) -> Vec<u8> {
        let mut d = vi(nutrition);
        d.extend_from_slice(&saturation.to_be_bytes());
        d.push(1); // can_always_eat
        d.extend_from_slice(&1.6f32.to_be_bytes());
        d.extend_from_slice(&vi(0)); // converts_to: empty stack
        d.extend_from_slice(&vi(effects.len() as i64));
        for (id, prob) in effects {
            d.extend_from_slice(&vi(*id));
            d.extend_from_slice(&prob.to_be_bytes());
        }
        d
    }

    /// `minecraft:tool` payload with one explicit-block rule.
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn tool_payload(block_id: i64, speed: f32, correct: bool, dpb: i64) -> Vec<u8> {
        let mut d = vi(1); // rules
        d.extend_from_slice(&vi(2)); // holder set of 1 explicit block id
        d.extend_from_slice(&vi(block_id));
        d.push(1); // speed present
        d.extend_from_slice(&speed.to_be_bytes());
        d.push(1); // correct_for_drops present
        d.push(u8::from(correct));
        d.extend_from_slice(&1.0f32.to_be_bytes()); // default speed
        d.extend_from_slice(&vi(dpb));
        d
    }

    /// `minecraft:can_break`/`can_place_on` payload naming one explicit block.
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn predicate_payload(block_id: i64) -> Vec<u8> {
        let mut d = vi(1); // predicates
        d.push(1); // blocks present
        d.extend_from_slice(&vi(2)); // holder set of 1 block id
        d.extend_from_slice(&vi(block_id));
        d.push(0); // no properties
        d.push(0); // no nbt matcher
        d.push(0); // show_in_tooltip
        d
    }

    #[test]
    fn v1211_comp_max_stack_size_caps_stacking() {
        let stone = item("stone");
        let no_comps: Comps = Vec::new();
        assert_eq!(item_stack_max(stone, &no_comps), 64);
        let comps = vec![(COMP_MAX_STACK_SIZE, vi(16))];
        assert_eq!(item_stack_max(stone, &comps), 16);
        // The component clamp flows into stack_max_of, which click/pickup
        // merge paths use for the real cap.
        let st = (stone, 10, comps.clone());
        assert_eq!(stack_max_of(&st), 16);
        // And into creative count clamping (99-domain wire count).
        let mut st = (stone, 99, comps);
        creative_slot_clamp(&mut st);
        assert_eq!(st.1, 16);
    }

    #[test]
    fn v1211_comp_unbreakable_skips_durability() {
        let pick = item("diamond_pickaxe");
        let mut st = (pick, 1, vec![(COMP_UNBREAKABLE, vec![1])]);
        assert!(!damage_stack(&mut st, 0.5));
        assert!(comp_get(&st.2, COMP_DAMAGE).is_none());
        // Without unbreakable the same call consumes durability.
        let mut plain = (pick, 1, Vec::new());
        assert!(!damage_stack(&mut plain, 0.5));
        assert_eq!(comp_varint(&plain.2, COMP_DAMAGE), Some(1));
    }

    #[test]
    fn v1211_comp_fire_resistant_survives_lava() {
        let stone = item("stone");
        let no_comps: Comps = Vec::new();
        assert!(!drop_fireproof(stone, &no_comps));
        let comps = vec![(COMP_FIRE_RESISTANT, Vec::new())];
        assert!(drop_fireproof(stone, &comps));
        assert!(drop_fireproof(item("netherite_ingot"), &no_comps));
        assert!(drop_burns_at("minecraft:lava"));
        assert!(drop_burns_at("minecraft:fire"));
        assert!(!drop_burns_at("minecraft:water"));
    }

    #[test]
    fn v1211_comp_food_custom_nutrition_and_effects() {
        let carrot = item("carrot");
        let data = food_payload(20, 0.9, &[(1, 1.0), (5, 0.5)]);
        let st = (carrot, 1, vec![(COMP_FOOD, data)]);
        let fc = stack_food(&st).unwrap();
        assert_eq!(fc.nutrition, 20);
        assert!((fc.saturation - 0.9).abs() < 1e-6);
        assert!(fc.can_always_eat);
        assert_eq!(fc.effects, vec![(1, 1.0), (5, 0.5)]);
        // Effects surface through the eat path in folded (prob%, id, amp, ticks).
        let eff = stack_food_effects(&st);
        assert!(eff.iter().any(|&(p, id, _, _)| p == 100 && id == 1));
        assert!(eff.iter().any(|&(p, id, _, _)| p == 50 && id == 5));
        // Fallback keeps the server's vanilla item table values.
        let plain = (carrot, 1, Vec::new());
        assert_eq!(stack_food(&plain).unwrap().nutrition, i64::from(food_value(carrot)));
    }

    #[test]
    fn v1211_comp_ominous_bottle_amplifier_bad_omen() {
        let bottle = item("ominous_bottle");
        let st = (
            bottle,
            1,
            vec![(COMP_OMINOUS_BOTTLE_AMPLIFIER, vi(3))],
        );
        assert_eq!(ominous_bottle_amplifier(&st), Some(3));
        // Bad omen (id 30) at the stored amplifier, ~2 hours of ticks.
        let eff = stack_food_effects(&st);
        assert!(eff.iter().any(|&(_, id, amp, _)| id == 30 && amp == 3));
        // Vanilla `OminousBottleAmplifier` clamps out-of-domain values to 0..=4.
        assert_eq!(
            apply_item_component_behavior(COMP_OMINOUS_BOTTLE_AMPLIFIER, &vi(9)),
            Some(ComponentEffect::Amplifier(4))
        );
        assert!(matches!(
            apply_item_component_behavior(COMP_OMINOUS_BOTTLE_AMPLIFIER, &vi(4)),
            Some(ComponentEffect::Amplifier(4))
        ));
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn v1211_comp_tool_custom_speed_and_damage() {
        let reg = breg().unwrap();
        let stone_state = reg.state_of("minecraft:stone", None);
        let stone_id = i64::from(reg.block_id_of(stone_state).unwrap());
        let stick = item("stick");
        let fast = (
            stick,
            1,
            vec![(COMP_TOOL, tool_payload(stone_id, 32.0, true, 2))],
        );
        let plain = (stick, 1, Vec::new());
        // The rule's speed replaces the (absent) vanilla tier speed.
        let with_tool = dig_seconds(stone_state as i64, &fast).unwrap();
        let without = dig_seconds(stone_state as i64, &plain).unwrap();
        assert!(with_tool < without);
        // correct_for_drops drives the harvestable multiplier path.
        let tool = tool_component_for(&fast).unwrap();
        assert!(tool_component_correct(&tool, stone_state, reg));
        assert_eq!(tool_damage_per_block(&fast), 2);
        // damage_per_block feeds durability: one break consumes 2 points.
        let mut st = (
            stick,
            1,
            vec![
                (COMP_TOOL, tool_payload(stone_id, 32.0, true, 2)),
                (COMP_MAX_DAMAGE, vi(10)),
            ],
        );
        assert!(!damage_stack(&mut st, 0.5));
        assert_eq!(comp_varint(&st.2, COMP_DAMAGE), Some(2));
    }

    #[test]
    fn v1211_comp_bundle_weight_insert_extract() {
        let bundle_item = item("bundle");
        let stone = item("stone");
        let mut contents: Vec<Stack> = Vec::new();
        // A full 64-stack fills the whole 64-weight budget.
        let (data, remaining) = bundle_try_insert(&mut contents, &(stone, 64, Vec::new()));
        assert_eq!(remaining, 0);
        let bundle = (bundle_item, 1, vec![(COMP_BUNDLE_CONTENTS, data.clone())]);
        assert_eq!(bundle_total_weight(&bundle), 64);
        // Nothing more fits.
        let before = contents.len();
        let (_data2, remaining) = bundle_try_insert(&mut contents, &(stone, 8, Vec::new()));
        assert_eq!(remaining, 8);
        assert_eq!(contents.len(), before);
        // A lighter stack occupies proportional weight: 16-count items weigh 4 each.
        let pearl = item("ender_pearl");
        let mut contents2: Vec<Stack> = Vec::new();
        let (_d, remaining) = bundle_try_insert(&mut contents2, &(pearl, 16, Vec::new()));
        assert_eq!(remaining, 0);
        // 16 pearls * (64/16) = the full budget again.
        let bundle2 = (bundle_item, 1, vec![(COMP_BUNDLE_CONTENTS, encode_stack_list(&contents2))]);
        assert_eq!(bundle_total_weight(&bundle2), 64);
        // Extraction is LIFO and conserves the stack.
        let (out, data3) = bundle_try_extract(&mut contents2).unwrap();
        assert_eq!((out.0, out.1), (pearl, 16));
        assert_eq!(decode_stack_list(&data3, 256).unwrap().iter().map(|s| s.1).sum::<u8>(), 0);
    }

    #[test]
    fn v1211_comp_lock_blocks_without_named_key() {
        // `lock` stored as a bare NBT string = `custom_name` key predicate.
        let chest_st = (
            item("chest"),
            1,
            vec![(COMP_LOCK, nbt_str("skeleton key"))],
        );
        let pred = stack_lock_predicate(&chest_st).unwrap();
        let key = (
            item("stick"),
            1,
            vec![(COMP_CUSTOM_NAME, nbt_str("skeleton key"))],
        );
        let wrong_key = (
            item("stick"),
            1,
            vec![(COMP_CUSTOM_NAME, nbt_str("other key"))],
        );
        assert!(container_unlocked(Some(&pred), &key));
        assert!(!container_unlocked(Some(&pred), &wrong_key));
        assert!(!container_unlocked(Some(&pred), &(item("stick"), 1, Vec::new())));
        // No lock means open.
        assert!(container_unlocked(None, &wrong_key));
    }

    #[test]
    fn v1211_comp_charged_projectiles_load_release() {
        let crossbow = item("crossbow");
        let arrow = item("arrow");
        let payload = encode_stack_list(&[(arrow, 3, Vec::new())]);
        let st = (
            crossbow,
            1,
            vec![(COMP_CHARGED_PROJECTILES, payload)],
        );
        let loaded = charged_projectiles_of(&st).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!((loaded[0].0, loaded[0].1), (arrow, 3));
        // The validator routes through the same decoder.
        assert!(matches!(
            apply_item_component_behavior(
                COMP_CHARGED_PROJECTILES,
                &encode_stack_list(&[(arrow, 1, Vec::new())])
            ),
            Some(ComponentEffect::StackList(_))
        ));
        // An unloaded crossbow reports nothing.
        assert!(charged_projectiles_of(&(crossbow, 1, Vec::new())).is_none());
    }

    #[test]
    fn v1211_comp_dyed_color_and_container_roundtrip() {
        use crate::nbt::Tag;
        let shulker = item("white_shulker_box");
        let apple = item("apple");
        let st = (
            shulker,
            1,
            vec![
                (COMP_CONTAINER, encode_stack_list(&[(apple, 5, Vec::new())])),
                (COMP_DYED_COLOR, dyed_color_data(0x00ff00)),
            ],
        );
        let entry = stack_to_nbt(0, &st).unwrap();
        let back = items_nbt_to_stacks(&Tag::List(10, vec![entry])).unwrap();
        let restored = back.get(&0).unwrap();
        assert_eq!((restored.0, restored.1), (shulker, 1));
        // `container` contents survive the save/load roundtrip.
        let slots = container_slots(restored).unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!((slots[0].0, slots[0].1), (apple, 5));
        // `dyed_color` survives byte-for-byte and decodes to the stored RGB.
        assert_eq!(stack_dyed_rgb(restored), Some(0x00ff00));
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn v1211_comp_can_place_can_break_predicates() {
        let reg = breg().unwrap();
        let stone_state = reg.state_of("minecraft:stone", None);
        let dirt_state = reg.state_of("minecraft:dirt", None);
        let stone_id = i64::from(reg.block_id_of(stone_state).unwrap());
        let st = (
            item("stick"),
            1,
            vec![
                (COMP_CAN_BREAK, predicate_payload(stone_id)),
                (COMP_CAN_PLACE_ON, predicate_payload(stone_id)),
            ],
        );
        assert!(stack_can_break(&st, stone_state, reg));
        assert!(!stack_can_break(&st, dirt_state, reg));
        assert!(stack_can_place_on(&st, stone_state, reg));
        assert!(!stack_can_place_on(&st, dirt_state, reg));
        // Missing components default to "allowed everywhere" (survival rules).
        let plain = (item("stick"), 1, Vec::new());
        assert!(stack_can_break(&plain, dirt_state, reg));
        assert!(stack_can_place_on(&plain, dirt_state, reg));
        // The block-level evaluator answers the same question directly.
        assert!(block_predicates_allow(&predicate_payload(stone_id), stone_state, reg));
        assert!(!block_predicates_allow(&predicate_payload(stone_id), dirt_state, reg));
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn v1211_comp_block_state_override() {
        let reg = breg().unwrap();
        let base = reg.state_of("minecraft:furnace", None);
        assert_eq!(reg.get_prop(base, "lit").as_deref(), Some("false"));
        let mut payload = vi(1);
        comp_write_string(&mut payload, "lit");
        comp_write_string(&mut payload, "true");
        let st = (
            item("furnace"),
            1,
            vec![(COMP_BLOCK_STATE, payload)],
        );
        let resolved = item_block_state_override(&st, base);
        assert_eq!(reg.get_prop(resolved, "lit").as_deref(), Some("true"));
        // No component: the base state is untouched.
        assert_eq!(item_block_state_override(&(item("furnace"), 1, Vec::new()), base), base);
    }

    #[test]
    fn v1211_comp_behavior_validator_rejects_malformed() {
        // Truncated payload, out-of-domain value, and unknown id all reject.
        assert_eq!(apply_item_component_behavior(COMP_MAX_STACK_SIZE, &[]), None);
        assert_eq!(apply_item_component_behavior(COMP_MAX_STACK_SIZE, &vi(100)), None);
        assert_eq!(apply_item_component_behavior(COMP_RARITY, &vi(4)), None);
        assert_eq!(apply_item_component_behavior(9999, &[]), None);
        assert!(matches!(
            apply_item_component_behavior(COMP_MAX_STACK_SIZE, &vi(99)),
            Some(ComponentEffect::StackLimit(99))
        ));
        assert!(apply_item_component_behavior(COMP_LOCK, &nbt_str("key")).is_some());
        assert!(apply_item_component_behavior(
            COMP_FOOD,
            &food_payload(6, 0.3, &[])
        ).is_some());
    }

    #[test]
    fn v1211_comp_suspicious_stew_and_firework_effects() {
        // suspicious_stew_effects: (effect id, duration ticks) list.
        let stew = item("suspicious_stew");
        let mut data = vi(1);
        data.extend_from_slice(&vi(1)); // speed
        data.extend_from_slice(&vi(160));
        let st = (stew, 1, vec![(COMP_SUSPICIOUS_STEW_EFFECTS, data)]);
        let eff = stack_food_effects(&st);
        assert!(eff.iter().any(|&(_, id, _, _)| id == 1));
        // fireworks component: flight duration + explosion count decode.
        let rocket = item("firework_rocket");
        let mut fw = vec![0u8]; // flight_duration byte
        fw.extend_from_slice(&vi(0)); // no explosions
        let st = (rocket, 1, vec![(COMP_FIREWORKS, fw)]);
        assert!(matches!(
            comp_get(&st.2, COMP_FIREWORKS).and_then(decode_fireworks),
            Some((0, 0))
        ));
    }
}

/// MEGA-SCALE §3.2 fixture: a 2,000-orb spawn burst collapses through the
/// pending merge before the pickup scan, conserving every XP point, and the
/// whole batch collects in one pass.
#[cfg(test)]
mod mega_scale_xp_tests {
    use super::*;
    use crate::effect_tests::client_pair;

    /// Save/restore for every shared store the XP merge/collection fixture
    /// touches. Restored on `Drop`, so a failed assertion still leaves the
    /// next test a clean world.
    struct MegaXpWorld {
        clients: Vec<Arc<Client>>,
        drops: Vec<Drop>,
        drop_pending: HashSet<i32>,
        xps: Vec<Xp>,
        xp_pending: HashSet<i32>,
        sessions: HashMap<i32, PlayerMechanismSession>,
        region: RegionSets,
    }

    impl MegaXpWorld {
        fn take() -> Self {
            Self {
                clients: std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner())),
                drops: std::mem::take(&mut *drops().lock().unwrap_or_else(|e| e.into_inner())),
                drop_pending: std::mem::take(
                    &mut *drop_pending().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                xps: std::mem::take(&mut *xps().lock().unwrap_or_else(|e| e.into_inner())),
                xp_pending: std::mem::take(
                    &mut *xp_pending().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                sessions: std::mem::take(
                    &mut *player_mechanism_sessions()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                ),
                region: test_region(),
            }
        }
    }

    impl std::ops::Drop for MegaXpWorld {
        fn drop(&mut self) {
            *clients().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.clients);
            *drops().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.drops);
            *drop_pending().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.drop_pending);
            *xps().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.xps);
            *xp_pending().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.xp_pending);
            *player_mechanism_sessions()
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.sessions);
            set_test_region(self.region.0.clone(), self.region.1.clone());
        }
    }

    /// 2,000 orbs spawned in one burst merge into the handful the wire cap
    /// allows — over-valued survivors spill bounded remainder orbs — and a
    /// nearby player then collects the entire conserved total in a single
    /// `drop_tick` pass.
    #[test]
    fn mega_scale_xp_2000_orbs_merge_and_collect_conserving_exact_xp() {
        let _guard = world_guard();
        let _w = MegaXpWorld::take();
        const N: usize = 2000;
        const VAL: i64 = 100;
        let total_expected: u64 = N as u64 * VAL as u64;
        let at = (10.0, 80.0, 10.0);
        for i in 0..N {
            // A tight farm cluster: every orb inside merge radius of every
            // other, all well inside the 1.6-block pickup reach.
            xp_spawn(
                at.0 + (i % 4) as f64 * 0.1,
                at.1,
                at.2 + ((i / 4) % 4) as f64 * 0.1,
                VAL as i16,
            );
        }
        drop_tick();
        {
            let xs = xps().lock().unwrap_or_else(|e| e.into_inner());
            let total: u64 = xs.iter().map(|x| u64::from(x.val as u16)).sum();
            assert_eq!(total, total_expected, "merging must conserve every XP point");
            assert!(
                xs.len() <= 16,
                "2,000 clustered orbs must collapse to a bounded few, got {}",
                xs.len()
            );
            assert!(
                xs.iter().all(|x| x.val > 0),
                "no zero-value tombstone may survive the merge"
            );
        }
        // A live client on the cluster collects the merged batch next tick.
        let (mut client, _rx) = client_pair();
        client.eid = 777_001;
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (at.0, at.1, at.2, 0.0, 0.0, true);
        let client = Arc::new(client);
        player_mechanism_sessions()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                client.eid,
                PlayerMechanismSession {
                    xp_bar: (0, 0.0),
                    food_timer: 0,
                    menus: Arc::new(LiveMenuConnection {
                        active: Mutex::new(None),
                        next_id: std::sync::atomic::AtomicU8::new(4),
                        enchantment_seed: Mutex::new(0),
                    }),
                },
            );
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(client.clone());
        drop_tick();
        assert!(
            xps().lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "every merged orb inside pickup reach must be collected"
        );
        assert_eq!(
            *client.xp.lock().unwrap_or_else(|e| e.into_inner()),
            total_expected as i64,
            "the player must receive the exact conserved XP total"
        );
    }
}

// ---------- S-MENUS-MULTIDIM-OPT tests ----------
//
// Multi-dim container keys, the drop-dimension side index, the per-chunk
// live-block-entity index and the borrow-diffed chest viewer tick. Every
// block read goes through the numeric-range fallbacks, so these tests run
// registry-free (`breg() == None` is the supported no-fixture path).

#[cfg(test)]
mod s5_multidim_tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    /// A `DimCells` twin for tests that run without the vanilla data fixture:
    /// writes a raw state into the overlay and restores the cell on drop.
    struct OverlayCells(Vec<(DimId, i32, i32, i32, Option<u32>)>);
    impl OverlayCells {
        fn new() -> Self {
            Self(Vec::new())
        }
        fn set(&mut self, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
            let key = (dim, x >> 4, z >> 4);
            let cell = local_idx(x, y, z);
            let prev = {
                let mut w = world().write(&key);
                let chunk = w.entry(key).or_default();
                let prev = chunk.get(&cell).copied();
                chunk.insert(cell, st);
                prev
            };
            self.0.push((dim, x, y, z, prev));
        }
    }
    impl std::ops::Drop for OverlayCells {
        fn drop(&mut self) {
            let mut w = world().write_all();
            for (dim, x, y, z, prev) in self.0.drain(..).rev() {
                if let Some(chunk) = w.get_mut(&(dim, x >> 4, z >> 4)) {
                    match prev {
                        Some(st) => {
                            chunk.insert(local_idx(x, y, z), st);
                        }
                        None => {
                            chunk.remove(&local_idx(x, y, z));
                        }
                    }
                }
            }
        }
    }

    /// Drain every frame the socket received; `read` hits the fixture's 1s
    /// timeout once the batch is exhausted.
    fn read_frames(rx: &mut TcpStream) -> Vec<(i64, Vec<u8>)> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = rx.read(&mut chunk) {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let mut frames = Vec::new();
        let mut rest = buf.as_slice();
        while !rest.is_empty() {
            let mut view = rest;
            let Some(len) = read_varint(&mut view) else { break };
            let len = len as usize;
            if view.len() < len {
                break;
            }
            let mut body = &view[..len];
            rest = &view[len..];
            let Some(id) = read_varint(&mut body) else { break };
            frames.push((id, body.to_vec()));
        }
        frames
    }

    /// One `(x, y, z)`, three dimensions, three containers: the dim inside
    /// `ContainerKey::Block` keeps a Nether chest and an Overworld chest at
    /// the same coordinates as separate maps — and an unprovisioned
    /// dimension reads empty rather than the Overworld's contents.
    #[test]
    fn container_keys_isolate_dimensions() {
        let _guard = world_guard();
        let pos = (8_700_001, 64, -8_700_001);
        let stack: Stack = (803, 7, Vec::new());
        {
            let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
            all.insert(
                ContainerKey::at(DimId::Nether, pos),
                HashMap::from([(0i16, stack.clone())]),
            );
            all.insert(
                ContainerKey::ow(pos),
                HashMap::from([(1i16, stack.clone())]),
            );
        }
        {
            let all = chests().lock().unwrap_or_else(|e| e.into_inner());
            let nether = all.get(&ContainerKey::at(DimId::Nether, pos)).unwrap();
            assert_eq!(nether.get(&0).map(|s| s.1), Some(7));
            assert!(nether.get(&1).is_none(), "the Overworld slot leaked into the Nether key");
            let ow = all.get(&ContainerKey::ow(pos)).unwrap();
            assert_eq!(ow.get(&1).map(|s| s.1), Some(7));
            assert!(ow.get(&0).is_none(), "the Nether slot leaked into the Overworld key");
            assert!(
                all.get(&ContainerKey::at(DimId::End, pos)).is_none(),
                "a dimension with no container must not read the Overworld's"
            );
        }
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        all.remove(&ContainerKey::at(DimId::Nether, pos));
        all.remove(&ContainerKey::ow(pos));
    }

    /// `window_live` resolves `container_exists` against the key's own
    /// dimension: a chest placed in the Overworld backs the Overworld window
    /// and no other.
    #[test]
    fn window_live_checks_the_keys_own_dimension() {
        let _guard = world_guard();
        let pos = (8_700_003, 64, -8_700_003);
        let mut cells = OverlayCells::new();
        cells.set(DimId::Overworld, pos.0, pos.1, pos.2, 2954); // chest, no-registry fallback
        assert!(window_live((ContainerKey::at(DimId::Overworld, pos), None)));
        assert!(
            !window_live((ContainerKey::at(DimId::Nether, pos), None)),
            "the Nether must not see the Overworld chest at the same coords"
        );
        assert!(
            !window_live((ContainerKey::at(DimId::End, pos), None)),
            "the End must not see the Overworld chest at the same coords"
        );
    }

    /// `hopper_push_dim` ejects into the container its own dimension holds:
    /// a Nether hopper above a Nether chest moves items between the two
    /// Nether keys and never materialises the Overworld keys.
    #[test]
    fn hopper_push_dim_moves_between_dim_keyed_containers() {
        let _guard = world_guard();
        let (hx, hy, hz) = (8_700_005, 64, -8_700_005);
        let dst = (hx + 1, hy, hz);
        let mut cells = OverlayCells::new();
        cells.set(DimId::Nether, dst.0, dst.1, dst.2, 2954); // chest fallback range
        {
            let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
            all.insert(
                ContainerKey::at(DimId::Nether, (hx, hy, hz)),
                HashMap::from([(0i16, (803i64, 5u8, Vec::new()) as Stack)]),
            );
        }
        assert!(hopper_push_dim(DimId::Nether, hx, hy, hz, dst));
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        let own = all.get(&ContainerKey::at(DimId::Nether, (hx, hy, hz))).unwrap();
        assert_eq!(own.get(&0).map(|s| s.1), Some(4), "one item per activation");
        let into = all.get(&ContainerKey::at(DimId::Nether, dst)).unwrap();
        assert_eq!(into.get(&0).map(|s| s.1), Some(1));
        assert!(
            all.get(&ContainerKey::ow((hx, hy, hz))).is_none(),
            "the Nether push must not create an Overworld hopper map"
        );
        all.remove(&ContainerKey::at(DimId::Nether, (hx, hy, hz)));
        all.remove(&ContainerKey::at(DimId::Nether, dst));
    }

    /// The drop side index scopes suction to the spawn dimension: a drop
    /// spawned in the Nether is a candidate for the Nether hopper at the same
    /// coords and invisible to the Overworld hopper there.
    #[test]
    fn drop_spawn_dim_scopes_hopper_suction() {
        let _guard = world_guard();
        let (hx, hy, hz) = (8_700_007, 64, -8_700_007);
        let eid = drop_spawn_in_dim(
            DimId::Nether,
            hx as f64 + 0.5,
            hy as f64 + 1.2,
            hz as f64 + 0.5,
            803,
            3,
            Vec::new(),
            0,
            None,
            None,
        );
        assert_eq!(drop_dim_of(eid), DimId::Nether);
        assert!(suction_nonempty_in(DimId::Nether, hx, hy, hz));
        assert!(
            !suction_nonempty_in(DimId::Overworld, hx, hy, hz),
            "the Nether drop must not enter the Overworld suction cell"
        );
        assert!(
            !hopper_suck_in_dim(DimId::Overworld, hx, hy, hz),
            "the Overworld hopper must not take the Nether drop"
        );
        assert!(hopper_suck_in_dim(DimId::Nether, hx, hy, hz));
        {
            let all = chests().lock().unwrap_or_else(|e| e.into_inner());
            let own = all.get(&ContainerKey::at(DimId::Nether, (hx, hy, hz))).unwrap();
            assert_eq!(own.get(&0).map(|s| s.1), Some(3));
            assert!(all.get(&ContainerKey::ow((hx, hy, hz))).is_none());
        }
        assert!(drops().lock().unwrap_or_else(|e| e.into_inner()).iter().all(|d| d.eid != eid));
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        all.remove(&ContainerKey::at(DimId::Nether, (hx, hy, hz)));
        drop_dims().lock().unwrap_or_else(|e| e.into_inner()).remove(&eid);
        drop_pending().lock().unwrap_or_else(|e| e.into_inner()).remove(&eid);
    }

    /// A Nether container change leaves the Overworld sleep-probe fingerprint
    /// untouched; the matching Overworld write moves it.
    #[test]
    fn hopper_probe_epoch_fingerprints_its_own_dimension() {
        let _guard = world_guard();
        let (hx, hy, hz) = (8_700_009, 64, -8_700_009);
        let dst = (hx + 1, hy, hz);
        let e1 = hopper_probe_epoch_in_dim(DimId::Overworld, hx, hy, hz, dst);
        {
            let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
            all.insert(
                ContainerKey::at(DimId::Nether, (hx, hy, hz)),
                HashMap::from([(0i16, (803i64, 5u8, Vec::new()) as Stack)]),
            );
        }
        let e2 = hopper_probe_epoch_in_dim(DimId::Overworld, hx, hy, hz, dst);
        assert_eq!(e1, e2, "a Nether write must not move the Overworld probe");
        {
            let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
            all.insert(
                ContainerKey::ow((hx, hy, hz)),
                HashMap::from([(0i16, (803i64, 5u8, Vec::new()) as Stack)]),
            );
        }
        let e3 = hopper_probe_epoch_in_dim(DimId::Overworld, hx, hy, hz, dst);
        assert_ne!(e2, e3, "an Overworld write must move the Overworld probe");
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        all.remove(&ContainerKey::at(DimId::Nether, (hx, hy, hz)));
        all.remove(&ContainerKey::ow((hx, hy, hz)));
    }

    /// `entries_in_chunk`: insert, update, remove and drift cases across
    /// dimensions and negative chunk coordinates (`div_euclid(16)`).
    #[test]
    fn live_store_entries_in_chunk_tracks_mutation() {
        let mut store = LiveBlockEntityStore::default();
        let entity = || (0, LiveBlockEntity::ChiseledBookshelf(ChiseledBookshelf::default()));
        // insert: same coords in two dims stay in separate chunk buckets.
        store.insert_entry((DimId::Overworld, 3, 70, 5), entity());
        store.insert_entry((DimId::Nether, 3, 70, 5), entity());
        // negative coords: -1 and -16 land in chunk -1, -17 in chunk -2.
        store.insert_entry((DimId::Overworld, -1, 70, -1), entity());
        store.insert_entry((DimId::Overworld, -16, 70, -16), entity());
        store.insert_entry((DimId::Overworld, -17, 70, 0), entity());
        assert_eq!(
            store.entries_in_chunk(DimId::Overworld, 0, 0),
            vec![(DimId::Overworld, 3, 70, 5)]
        );
        assert_eq!(
            store.entries_in_chunk(DimId::Nether, 0, 0),
            vec![(DimId::Nether, 3, 70, 5)]
        );
        let mut neg = store.entries_in_chunk(DimId::Overworld, -1, -1);
        neg.sort();
        assert_eq!(
            neg,
            vec![
                (DimId::Overworld, -16, 70, -16),
                (DimId::Overworld, -1, 70, -1)
            ]
        );
        assert_eq!(
            store.entries_in_chunk(DimId::Overworld, -2, 0),
            vec![(DimId::Overworld, -17, 70, 0)]
        );
        // update: replacing an entry does not duplicate the index position.
        store.insert_entry((DimId::Overworld, 3, 70, 5), entity());
        assert_eq!(store.entries_in_chunk(DimId::Overworld, 0, 0).len(), 1);
        // remove: the position leaves its chunk bucket.
        store.remove_entry((DimId::Overworld, 3, 70, 5));
        assert!(store.entries_in_chunk(DimId::Overworld, 0, 0).is_empty());
        assert_eq!(
            store.entries_in_chunk(DimId::Nether, 0, 0),
            vec![(DimId::Nether, 3, 70, 5)],
            "removing the Overworld entry leaves the Nether bucket alone"
        );
        // bypass writes: `entries` reached without the indexed mutators can
        // never make a removed cell report — re-validation filters it — and
        // `rebuild_index` picks a missed cell back up.
        store.insert_entry((DimId::Overworld, 9, 70, 9), entity());
        store.entries.remove(&(DimId::Overworld, 9, 70, 9)); // bypassed remove
        assert!(
            store.entries_in_chunk(DimId::Overworld, 0, 0).is_empty(),
            "a bypassed remove must still read as removed"
        );
        store.entries.insert((DimId::Overworld, 10, 70, 10), entity()); // bypassed insert
        assert!(
            store.entries_in_chunk(DimId::Overworld, 0, 0).is_empty(),
            "a bypassed insert is invisible until the bucket is rebuilt"
        );
        store.rebuild_index();
        assert_eq!(
            store.entries_in_chunk(DimId::Overworld, 0, 0),
            vec![(DimId::Overworld, 10, 70, 10)]
        );
        // clear: wiping `entries` leaves every query empty — the index can
        // never outlive the store it describes.
        store.entries.clear();
        assert!(store.entries_in_chunk(DimId::Overworld, 0, 0).is_empty());
        assert!(store.entries_in_chunk(DimId::Nether, 0, 0).is_empty());
        assert!(store.entries_in_chunk(DimId::Overworld, -1, -1).is_empty());
    }

    /// The borrow-diff: an untouched window produces no `0x15` on the wire,
    /// a single-slot change produces exactly one, and removing a stack
    /// produces the empty-slot update for just that slot.
    #[test]
    fn chest_viewer_tick_sends_only_changed_slots() {
        let _guard = world_guard();
        let pos = (8_700_011, 64, -8_700_011);
        let mut cells = OverlayCells::new();
        cells.set(DimId::Overworld, pos.0, pos.1, pos.2, 2954); // chest fallback range
        let (client, mut rx) = crate::effect_tests::client_pair();
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (
            pos.0 as f64 + 0.5,
            pos.1 as f64 + 1.0,
            pos.2 as f64 + 0.5,
            0.0,
            0.0,
            true,
        );
        let c = Arc::new(client);
        let win = (ContainerKey::at(DimId::Overworld, pos), None);
        *c.open_chest.lock().unwrap_or_else(|e| e.into_inner()) = Some(win);
        {
            let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
            all.insert(win.0, HashMap::from([(0i16, (803i64, 3u8, Vec::new()) as Stack)]));
        }
        // First visit surfaces the seeded slot: one SetSlot frame.
        chest_viewer_tick_on(std::slice::from_ref(&c));
        let first = read_frames(&mut rx);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, 0x15);
        {
            let sync = c.chest_sync.lock().unwrap_or_else(|e| e.into_inner());
            let sync = sync.as_ref().unwrap();
            assert_eq!(sync.last_slots[0], (803, 3, Vec::new()));
            assert_eq!(sync.last_slots.len(), 27);
        }
        // Unchanged: zero frames — the diff did no work.
        chest_viewer_tick_on(std::slice::from_ref(&c));
        assert!(read_frames(&mut rx).is_empty(), "an unchanged window must send nothing");
        // One slot changed: exactly one frame.
        {
            let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
            all.get_mut(&win.0)
                .unwrap()
                .insert(5i16, (804i64, 2u8, Vec::new()));
        }
        chest_viewer_tick_on(std::slice::from_ref(&c));
        let changed = read_frames(&mut rx);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, 0x15);
        // Slot removed: one frame carrying the empty slot.
        {
            let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
            all.get_mut(&win.0).unwrap().remove(&0i16);
        }
        chest_viewer_tick_on(std::slice::from_ref(&c));
        let removed = read_frames(&mut rx);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].0, 0x15);
        // Idle again.
        chest_viewer_tick_on(std::slice::from_ref(&c));
        assert!(read_frames(&mut rx).is_empty());
        *c.open_chest.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        all.remove(&win.0);
    }
}

/// XMOD1-HOPPER: vanilla hoppers against `BlockCapabilityBus` endpoints.
/// `live_cap_register_slot_inventory` stands in for "any mod machine on the
/// bus" — the hopper code cannot tell it apart from a Create depot or a
/// cooking pot, which is exactly the seam under test. These tests need the
/// bus and the flat container map only; no block registry is required
/// because an unplaced (air) cell is never a vanilla container.
#[cfg(all(test, feature = "mod-suite"))]
mod xmod1_hopper_cap_tests {
    use super::*;

    fn hopper_put(dim: DimId, pos: BlockPos, slot: i16, stack: Stack) {
        chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(ContainerKey::at(dim, pos))
            .or_default()
            .insert(slot, stack);
    }

    fn hopper_stacks(dim: DimId, pos: BlockPos) -> Vec<(i16, Stack)> {
        let mut stacks: Vec<_> = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ContainerKey::at(dim, pos))
            .into_iter()
            .flat_map(|slots| slots.iter())
            .filter(|(_, stack)| stack.1 > 0)
            .map(|(slot, stack)| (*slot, stack.clone()))
            .collect();
        stacks.sort_by_key(|(slot, _)| *slot);
        stacks
    }

    fn hopper_clear(dim: DimId, pos: BlockPos) {
        chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ContainerKey::at(dim, pos));
    }

    /// One vanilla hopper ejects a single item into a registered bus
    /// endpoint in every dimension flavour — Overworld, Nether and an
    /// unregistered custom `DimId` — with strict item conservation, and the
    /// vanilla eight-tick transfer cooldown lands through the shared
    /// scheduler on success.
    #[test]
    fn hopper_pushes_into_capability_endpoint_across_dims() {
        let _g = world_guard();
        live_cap_reset();
        let iron = name_to_item("iron_ingot").expect("iron ingot item");
        for (i, dim) in [
            DimId::Overworld,
            DimId::Nether,
            DimId::from_raw(7),
        ]
        .into_iter()
        .enumerate()
        {
            let hpos = (8_800_100 + i as i32 * 16, 70, -8_800_100);
            let dst = (hpos.0, hpos.1 - 1, hpos.2); // hopper faces down
            live_cap_register_slot_inventory(dim, dst.0, dst.1, dst.2, 4, 64);
            hopper_put(dim, hpos, 0, (iron, 5, Vec::new()));
            // A neighbouring cell with no endpoint answers `0` — the push
            // leaves the hopper untouched, exactly like a vanilla air cell.
            assert!(!hopper_push_dim(dim, hpos.0, hpos.1, hpos.2, (hpos.0 + 1, hpos.1, hpos.2)));
            assert_eq!(hopper_stacks(dim, hpos), vec![(0, (iron, 5, Vec::new()))]);

            assert!(hopper_push_dim(dim, hpos.0, hpos.1, hpos.2, dst));
            assert_eq!(
                hopper_stacks(dim, hpos),
                vec![(0, (iron, 4, Vec::new()))],
                "one item per activation, dim {dim:?}"
            );
            let landed = live_cap_extract_item(dim, dst.0, dst.1, dst.2, None, 64, false)
                .expect("the endpoint received the ejected item");
            assert_eq!(landed.item_id, "minecraft:iron_ingot");
            assert_eq!(landed.count, 1, "zero duplication/loss across the hop");
            assert!(landed.raw_components.is_empty());

            // A successful move arms the vanilla 8-tick transfer cooldown.
            hopper_sched_result(dim, hpos, true);
            let now = TICK_IDX.load(Ordering::Relaxed);
            assert_eq!(
                backoff()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&(dim, hpos.0, hpos.1, hpos.2))
                    .copied(),
                Some((now + server_ticks(HOPPER_TRANSFER_COOLDOWN as u64), 0)),
                "a successful push arms the 8-tick vanilla cooldown"
            );
            TICK_IDX.fetch_add(server_ticks(HOPPER_TRANSFER_COOLDOWN as u64 - 1), Ordering::Relaxed);
            assert!(
                !sched_ready_dim(dim, hpos.0, hpos.1, hpos.2),
                "tick 7 of the cooldown must still block the hopper"
            );
            TICK_IDX.fetch_add(1, Ordering::Relaxed);
            assert!(sched_ready_dim(dim, hpos.0, hpos.1, hpos.2));

            backoff()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(dim, hpos.0, hpos.1, hpos.2));
            live_cap_unregister(dim, dst.0, dst.1, dst.2);
            hopper_clear(dim, hpos);
        }
        live_cap_reset();
    }

    /// A vanilla hopper under a bus endpoint pulls finished items out —
    /// simulate-then-commit means a full hopper (or a stack the internal
    /// item table cannot name) never deletes or duplicates cargo, and a
    /// foreign `raw_components` blob rides the hopper's slots back onto the
    /// bus byte-identical.
    #[test]
    fn hopper_pulls_from_capability_endpoint_simulate_first() {
        let _g = world_guard();
        live_cap_reset();
        let dim = DimId::Nether;
        let hpos = (8_800_200, 70, -8_800_200);
        let src = (hpos.0, hpos.1 + 1, hpos.2);
        let beef = name_to_item("cooked_beef").expect("cooked beef item");
        let stone = name_to_item("stone").expect("stone item");
        let iron = name_to_item("iron_ingot").expect("iron ingot item");
        live_cap_register_slot_inventory(dim, src.0, src.1, src.2, 4, 64);
        assert_eq!(
            live_cap_insert_item(dim, src.0, src.1, src.2, None, "minecraft:cooked_beef", 3, &[], false),
            3
        );
        // A foreign mod item with no internal id is not pullable — the
        // probe sees it but the commit never runs, so nothing is lost.
        assert_eq!(
            live_cap_insert_item(dim, src.0, src.1, src.2, None, "mod_a:focus_gem", 1, &[0xAB, 0xCD], false),
            1
        );
        // Five full slots of stone: the pull must report no room WITHOUT
        // committing — the endpoint keeps every item.
        for slot in 0..5 {
            hopper_put(dim, hpos, slot, (stone, 64, Vec::new()));
        }
        assert!(!hopper_pull_dim(dim, hpos.0, hpos.1, hpos.2));
        assert_eq!(
            hopper_stacks(dim, hpos).iter().map(|(_, s)| (s.0, s.1)).collect::<Vec<_>>(),
            vec![(stone, 64); 5],
            "a refused pull leaves the hopper untouched"
        );
        let held = live_cap_extract_item(dim, src.0, src.1, src.2, None, 64, true)
            .expect("endpoint still holds cargo after the refused pull");
        assert_eq!(held.item_id, "minecraft:cooked_beef");
        assert_eq!(held.count, 3, "simulate-before-commit: nothing was consumed");

        // Room for one stack: the cooked beef lands, the foreign gem stays.
        hopper_clear(dim, hpos);
        for want in 1..=3u8 {
            assert!(hopper_pull_dim(dim, hpos.0, hpos.1, hpos.2));
            assert_eq!(
                hopper_stacks(dim, hpos),
                vec![(0, (beef, want, Vec::new()))],
                "pulled beef merges onto the hopper's own stack"
            );
        }
        // Only the unresolvable gem is left: the probe sees it, the commit
        // never runs — the pull reports nothing and the endpoint keeps it.
        assert!(!hopper_pull_dim(dim, hpos.0, hpos.1, hpos.2));
        let mut drained = Vec::new();
        while let Some(stack) =
            live_cap_extract_item(dim, src.0, src.1, src.2, None, 64, false)
        {
            drained.push((stack.item_id, stack.count, stack.raw_components));
        }
        assert_eq!(
            drained,
            vec![("mod_a:focus_gem".to_string(), 1, vec![0xAB, 0xCD])],
            "no duplication or loss across the refused and accepted pulls"
        );

        // A resolvable item carrying a foreign components blob keeps it
        // verbatim through the pull AND a later re-push.
        assert_eq!(
            live_cap_insert_item(dim, src.0, src.1, src.2, None, "minecraft:iron_ingot", 1, &[7, 7, 7], false),
            1
        );
        assert!(hopper_pull_dim(dim, hpos.0, hpos.1, hpos.2));
        let wrapped = vec![(HOPPER_CAP_FOREIGN_COMP, vec![7, 7, 7])];
        assert_eq!(
            hopper_stacks(dim, hpos),
            vec![(0, (beef, 3, Vec::new())), (1, (iron, 1, wrapped.clone()))]
        );
        let other = (hpos.0 + 1, hpos.1, hpos.2);
        live_cap_register_slot_inventory(dim, other.0, other.1, other.2, 4, 64);
        // Beef is the first non-empty slot — it ejects one per activation
        // until it drains, then the component-wrapped iron goes, its
        // foreign blob leaving the hopper byte-identical.
        for _ in 0..3 {
            assert!(hopper_push_mod_target(dim, hpos.0, hpos.1, hpos.2, other));
        }
        assert!(hopper_push_mod_target(dim, hpos.0, hpos.1, hpos.2, other));
        assert!(hopper_stacks(dim, hpos).is_empty(), "the hopper drained");
        let mut back = Vec::new();
        while let Some(stack) =
            live_cap_extract_item(dim, other.0, other.1, other.2, None, 64, false)
        {
            back.push((stack.item_id, stack.count, stack.raw_components));
        }
        assert_eq!(
            back,
            vec![
                ("minecraft:cooked_beef".to_string(), 3, Vec::new()),
                ("minecraft:iron_ingot".to_string(), 1, vec![7, 7, 7]),
            ],
            "a foreign blob round-trips through the hopper byte-identical"
        );

        live_cap_unregister(dim, src.0, src.1, src.2);
        live_cap_unregister(dim, other.0, other.1, other.2);
        hopper_clear(dim, hpos);
        live_cap_reset();
    }

    /// The `Comps` <-> `raw_components` codec: a real component set encodes
    /// behind the tag and decodes losslessly, the foreign wrapper unwraps
    /// verbatim, and a blob that merely borrows the prefix stays opaque.
    #[test]
    fn hopper_cap_comps_codec_roundtrip() {
        let comps: Comps = vec![(COMP_CUSTOM_NAME, vec![1, 2, 3]), (COMP_LORE, vec![4, 5])];
        let raw = hopper_cap_comps_encode(&comps);
        assert_eq!(&raw[..4], HOPPER_CAP_COMPS_TAG);
        assert_eq!(hopper_cap_comps_decode(&raw), comps);
        // Empty stays empty so the Create duct fast path still applies.
        assert!(hopper_cap_comps_encode(&Vec::new()).is_empty());
        assert_eq!(hopper_cap_comps_decode(&[]), Vec::<(i64, Vec<u8>)>::new());
        // Foreign wrapper unwraps verbatim.
        let wrapped: Comps = vec![(HOPPER_CAP_FOREIGN_COMP, vec![9, 9])];
        assert_eq!(hopper_cap_comps_encode(&wrapped), vec![9, 9]);
        // A blob that only borrows the prefix but is not our encoding stays
        // opaque instead of misparsing.
        let mut fake = HOPPER_CAP_COMPS_TAG.to_vec();
        fake.extend_from_slice(&[0xFF, 0xFF, 0x7F]);
        let decoded = hopper_cap_comps_decode(&fake);
        assert_eq!(decoded, vec![(HOPPER_CAP_FOREIGN_COMP, fake.clone())]);
        assert_eq!(hopper_cap_comps_encode(&decoded), fake);
    }
}

/// `find_chorus_teleport_spot_dim` — `LivingEntity.randomTeleport` as
/// `ChorusFruitItem.finishUsingItem` drives it, in the player's own dimension.
#[cfg(all(test, megu_vanilla_data))]
mod chorus_dim_tests {
    use super::*;

    /// Restores every dimensioned cell a fixture wrote, in reverse order.
    struct DimCells(Vec<(DimId, i32, i32, i32, u32)>);
    impl DimCells {
        fn new() -> Self {
            DimCells(Vec::new())
        }
        fn set(&mut self, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
            self.0.push((dim, x, y, z, block_at_dim(dim, x, y, z)));
            set_block_dim(dim, x, y, z, st);
        }
    }
    impl std::ops::Drop for DimCells {
        fn drop(&mut self) {
            for (dim, x, y, z, st) in self.0.drain(..).rev() {
                set_block_dim(dim, x, y, z, st);
            }
        }
    }

    /// Writes air through `y0..=y1` at `(x,z)` in `dim` so generated terrain
    /// cannot leak into a landing check.
    fn clear_column(cells: &mut DimCells, dim: DimId, x: i32, y0: i32, y1: i32, z: i32) {
        for y in y0..=y1 {
            cells.set(dim, x, y, z, 0);
        }
    }

    /// An End-stone island in the void: the roll commits on top of it, a roll
    /// inside the floor fails, and a void column fails instead of dropping the
    /// player into the void.
    #[test]
    fn chorus_end_roll_lands_on_island_and_refuses_void() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let mut cells = DimCells::new();
        let end_stone = name_to_state("end_stone").unwrap();
        // A lone column: void below, end_stone at y=60, air above.
        clear_column(&mut cells, DimId::End, 10, 0, 72, 10);
        cells.set(DimId::End, 10, 60, 10, end_stone);
        assert_eq!(
            find_chorus_teleport_spot_dim(DimId::End, (10.5, 62.0, 10.5), (0.5, 0, 0.5)),
            Some((10.5, 61.0, 10.5)),
            "the roll descends onto the island and stands on its top"
        );
        // A second storey: the feet cell inside end_stone rejects the landing.
        cells.set(DimId::End, 10, 61, 10, end_stone);
        assert_eq!(
            find_chorus_teleport_spot_dim(DimId::End, (10.5, 62.0, 10.5), (0.5, -1, 0.5)),
            None,
            "a roll landing inside the floor fails instead of embedding the player"
        );
        // A truly void column is a failed roll, never a void landing.
        let mut cells2 = DimCells::new();
        clear_column(&mut cells2, DimId::End, 20, 0, 72, 20);
        assert_eq!(
            find_chorus_teleport_spot_dim(DimId::End, (20.5, 62.0, 20.5), (0.5, 0, 0.5)),
            None,
            "no floor below the roll means no teleport"
        );
    }

    /// In the Nether the roll lands under the bedrock roof: the y roll stays
    /// inside the dimension's own window and the downward scan stops at the
    /// cave floor — for a player below the roof there is no path onto it.
    #[test]
    fn chorus_nether_roll_lands_under_the_roof() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let mut cells = DimCells::new();
        let stone = name_to_state("stone").unwrap();
        let bedrock = name_to_state("bedrock").unwrap();
        clear_column(&mut cells, DimId::Nether, 5, 61, 125, 5);
        cells.set(DimId::Nether, 5, 60, 5, stone);
        cells.set(DimId::Nether, 5, 126, 5, bedrock);
        cells.set(DimId::Nether, 5, 127, 5, bedrock);
        // The largest upward roll still resolves below the roof.
        let spot = find_chorus_teleport_spot_dim(DimId::Nether, (5.5, 64.0, 5.5), (0.5, 7, 0.5));
        assert_eq!(spot, Some((5.5, 61.0, 5.5)), "the roll lands on the cave floor");
        assert!(
            spot.is_some_and(|(_, y, _)| y < 126.0),
            "a player below the roof never lands on top of it"
        );
        // `randomTeleport` only checks the cell below the cursor: a roll that
        // lands the cursor in the roof's lower bedrock cell sees air below and
        // walks down out of the roof to the cave floor.
        let under = find_chorus_teleport_spot_dim(DimId::Nether, (5.5, 120.0, 5.5), (0.5, 6, 0.5));
        assert_eq!(
            under,
            Some((5.5, 61.0, 5.5)),
            "a roll into the roof's lower cell still comes out under it"
        );
        // But a roll onto y=127 stops there — the cell below is bedrock — and
        // the standing box collides, so the roll is wasted exactly as vanilla
        // wastes it (`randomTeleport` returns false for that attempt).
        assert_eq!(
            find_chorus_teleport_spot_dim(DimId::Nether, (5.5, 120.0, 5.5), (0.5, 7, 0.5)),
            None,
            "a roll whose scan stops inside the roof fails that attempt"
        );
    }

    /// Overworld cave and surface: the roll descends into a pocket and stands
    /// on its floor, needs two clear cells above the floor, and on open ground
    /// lands on the top block.
    #[test]
    fn chorus_overworld_roll_lands_in_cave_and_on_surface() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let mut cells = DimCells::new();
        let stone = name_to_state("stone").unwrap();
        // A pocket: stone floor at 200, air 201..=202, stone cap 203..=204.
        cells.set(DimId::Overworld, 3, 200, 3, stone);
        cells.set(DimId::Overworld, 3, 201, 3, 0);
        cells.set(DimId::Overworld, 3, 202, 3, 0);
        cells.set(DimId::Overworld, 3, 203, 3, stone);
        cells.set(DimId::Overworld, 3, 204, 3, stone);
        assert_eq!(
            find_chorus_teleport_spot_dim(DimId::Overworld, (3.5, 202.0, 3.5), (0.5, 0, 0.5)),
            Some((3.5, 201.0, 3.5)),
            "the roll descends into the pocket and stands on its floor"
        );
        // One block of headroom is not enough: the standing box needs two
        // clear cells above the floor.
        cells.set(DimId::Overworld, 3, 202, 3, stone);
        assert_eq!(
            find_chorus_teleport_spot_dim(DimId::Overworld, (3.5, 202.0, 3.5), (0.5, 0, 0.5)),
            None,
            "a one-cell pocket rejects the landing"
        );
        // Open ground: the roll stands on the top block.
        let mut cells2 = DimCells::new();
        cells2.set(DimId::Overworld, 8, 300, 8, stone);
        cells2.set(DimId::Overworld, 8, 301, 8, 0);
        cells2.set(DimId::Overworld, 8, 302, 8, 0);
        cells2.set(DimId::Overworld, 8, 303, 8, 0);
        assert_eq!(
            find_chorus_teleport_spot_dim(DimId::Overworld, (8.5, 302.0, 8.5), (0.5, 0, 0.5)),
            Some((8.5, 301.0, 8.5)),
            "the roll stands on the surface block"
        );
    }

    /// The horizontal roll stays inside vanilla's +-8 box around the player.
    #[test]
    fn chorus_roll_box_stays_within_eight_blocks() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let mut cells = DimCells::new();
        let stone = name_to_state("stone").unwrap();
        for x in 20..=38 {
            for z in 20..=38 {
                cells.set(DimId::Overworld, x, 250, z, stone);
                cells.set(DimId::Overworld, x, 251, z, 0);
                cells.set(DimId::Overworld, x, 252, z, 0);
                cells.set(DimId::Overworld, x, 253, z, 0);
            }
        }
        for roll in [0.0f64, 0.25, 0.5, 0.75, 0.999] {
            let Some((nx, _, nz)) = find_chorus_teleport_spot_dim(
                DimId::Overworld,
                (29.5, 252.0, 29.5),
                (roll, 0, 0.5),
            ) else {
                panic!("a floored roll must land (roll={roll})");
            };
            assert!(
                (29.5 - 8.0..=29.5 + 8.0).contains(&nx),
                "x scatter stays inside +-8, got {nx} for roll {roll}"
            );
            assert_eq!(nz, 29.5, "z follows the neutral roll");
        }
    }
}
