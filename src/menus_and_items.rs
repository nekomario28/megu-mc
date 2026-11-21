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
impl ContainerKey {
    /// The Overworld block key every dimension-less caller means until S3 threads a dimension
    /// through it.
    fn ow(pos: BlockPos) -> Self {
        ContainerKey::Block(ow_block(pos))
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
// COMP_ARMOR_TRIM=35 / COMP_BANNER_PATTERNS=49 anchors come from.
const COMP_ATTRIBUTE_MODIFIERS: i64 = 12;
const COMP_STORED_ENCHANTMENTS: i64 = 23;

/// `AttributeModifier.Operation` varint enum (Protocol 767).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    if let Some(n) = comp_varint(comps, 1) {
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
    let can = can_harvest(block, held.0);
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
    let dmg = comp_varint(&st.2, COMP_DAMAGE).unwrap_or(0) + 1;
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
    write_varint(&mut p, 0);
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
}
static DROPS: OnceLock<Mutex<Vec<Drop>>> = OnceLock::new();
fn drops() -> &'static Mutex<Vec<Drop>> {
    DROPS.get_or_init(|| Mutex::new(Vec::new()))
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

/// The cell -> member index both merge scans use, sized once from the item count.
///
/// A `HashMap` is the wrong shape for this: there are no removals, the key is three
/// integers, every bucket is a short list, and the scan probes the own cell plus its
/// 26 neighbours for every item (16,200 probes for 600 entities). Measured on the
/// entity-heavy fixture, the SipHash probe was the whole cost of the merge - 84 us of
/// walking the neighbourhood plus 25 us of building the map, in a 127 us `drops` stage,
/// and 105 us of a 135 us `environment` stage - while the distance test it guards is
/// under a nanosecond. This is a chained table over pre-sized arrays: no
/// resizing, no per-cell allocation, a probe that is three multiplies and a mask, and
/// an exact key comparison so a collision changes speed and nothing else.
struct CellBuckets {
    cell: Vec<(i32, i32, i32)>,
    used: Vec<bool>,
    head: Vec<u32>,
    next: Vec<u32>,
    mask: usize,
}

const NO_MEMBER: u32 = u32::MAX;

impl CellBuckets {
    fn new(items: usize) -> Self {
        // At most `items` distinct cells, and a load factor of at most one half.
        let cap = (items.max(4) * 2).next_power_of_two();
        CellBuckets {
            cell: vec![(0, 0, 0); cap],
            used: vec![false; cap],
            head: vec![NO_MEMBER; cap],
            next: vec![NO_MEMBER; items],
            mask: cap - 1,
        }
    }

    #[inline]
    fn slot(&self, cell: (i32, i32, i32)) -> usize {
        let h = (cell.0 as u32)
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add((cell.1 as u32).wrapping_mul(0x85EB_CA77))
            .wrapping_add((cell.2 as u32).wrapping_mul(0xC2B2_AE3D));
        // xorshift mix: the multiply chain alone leaves low-bit structure that a mask
        // and a linear probe would turn into long clusters.
        let h = h ^ (h >> 15) ^ (h >> 7);
        (h as usize) & self.mask
    }

    /// Link `idx` into `cell`'s bucket. The chain runs newest first, so a bucket is
    /// walked in descending index order; both callers sort their candidates before
    /// comparing, because the merge contract is ascending index order.
    fn push(&mut self, cell: (i32, i32, i32), idx: u32) {
        let mut at = self.slot(cell);
        while self.used[at] && self.cell[at] != cell {
            at = (at + 1) & self.mask;
        }
        if !self.used[at] {
            self.used[at] = true;
            self.cell[at] = cell;
        }
        self.next[idx as usize] = self.head[at];
        self.head[at] = idx;
    }

    /// The members of `cell`, in no particular order; `None` when no item is there.
    #[inline]
    fn members(&self, cell: (i32, i32, i32)) -> Option<u32> {
        let mut at = self.slot(cell);
        while self.used[at] {
            if self.cell[at] == cell {
                return Some(self.head[at]);
            }
            at = (at + 1) & self.mask;
        }
        None
    }
}

/// Walk one cell's members into `out`, keeping only indices past `after`.
fn collect_members(
    buckets: &CellBuckets,
    cell: (i32, i32, i32),
    after: usize,
    out: &mut Vec<usize>,
) {
    let Some(mut member) = buckets.members(cell) else {
        return;
    };
    while member != NO_MEMBER {
        let idx = member as usize;
        if idx > after {
            out.push(idx);
        }
        member = buckets.next[idx];
    }
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
    let mut buckets = CellBuckets::new(dp.len());
    for (idx, d) in dp.iter().enumerate() {
        buckets.push(
            (d.x.floor() as i32, d.y.floor() as i32, d.z.floor() as i32),
            idx as u32,
        );
    }
    if let Some(t) = __t {
        tickprof::add("drops/merge_cells", t.elapsed());
    }
    __t = __prof.then(std::time::Instant::now);
    let mut merged: Vec<i32> = Vec::new();
    let mut candidates: Vec<usize> = Vec::new();
    for i in 0..dp.len() {
        let (cx, cy, cz) = (
            dp[i].x.floor() as i32,
            dp[i].y.floor() as i32,
            dp[i].z.floor() as i32,
        );
        candidates.clear();
        // checked arithmetic: a drop whose cell is at the i32 limit has no neighbour
        // there, and wrapping would invent a cell that aliases.
        for dx in -1..=1i32 {
            for dy in -1..=1i32 {
                for dz in -1..=1i32 {
                    let key = (cx.checked_add(dx), cy.checked_add(dy), cz.checked_add(dz));
                    if let (Some(kx), Some(ky), Some(kz)) = key {
                        collect_members(&buckets, (kx, ky, kz), i, &mut candidates);
                    }
                }
            }
        }
        candidates.sort_unstable();
        for &j in candidates.iter() {
            let (ax, ay, az, ait, acnt, aage) = {
                let a = &dp[i];
                (a.x, a.y, a.z, a.item, a.cnt, a.age)
            };
            let (bx, by, bz, bit, bcnt, bage) = {
                let b = &dp[j];
                (b.x, b.y, b.z, b.item, b.cnt, b.age)
            };
            // Cheap integer and distance tests first; the component blobs are a Vec
            // comparison, so they run only for a real candidate.
            if ait == bit
                && dp[i].target == dp[j].target
                && (ax - bx).powi(2) + (ay - by).powi(2) + (az - bz).powi(2) < 0.25
                && dp[i].comps == dp[j].comps
                && acnt as u16 + bcnt as u16 <= item_stack_max(ait, &dp[i].comps) as u16
            {
                let (keep, eat) = if aage <= bage { (i, j) } else { (j, i) };
                dp[keep].cnt = dp[keep].cnt.saturating_add(dp[eat].cnt);
                merged.push(dp[eat].eid);
                break;
            }
        }
    }
    if let Some(t) = __t {
        tickprof::add("drops/merge_scan", t.elapsed());
    }
    merged
}

fn furnace_tick() {
    let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
    if fs.is_empty() {
        return;
    }
    let mut dirty_pos: Vec<DimBlockPos> = Vec::new();
    // One region borrow for the whole loop. Acquiring the mutex per furnace was
    // what dominated this stage, not the set lookup itself.
    let region = region_guard();
    for (pos, f) in fs.iter_mut() {
        if !region.is_warm(pos.1, pos.3) {
            continue;
        }
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
        if f.dirty {
            f.dirty = false;
            dirty_pos.push(*pos);
        }
    }
    drop(region);
    drop(fs);
    if dirty_pos.is_empty() {
        return;
    }
    let cs: Vec<Arc<Client>> = client_snapshot();
    for c in cs {
        let fp = *c.open_furnace.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((dim, x, y, z)) = fp {
            if dirty_pos.contains(&(dim, x, y, z)) {
                let inv = c.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let slots = furnace_slot_snapshot_dim(dim, (x, y, z));
                if let Ok(mut w) = c.sock.lock() {
                    furnace_push_items_snap(&mut w, &inv, &slots);
                }
            }
        }
    }
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
    let mut h = 0xcbf29ce484222325u64;
    for (ty, data) in comps {
        h = (h ^ (*ty as u64)).wrapping_mul(0x100000001b3);
        h = (h ^ data.len() as u64).wrapping_mul(0x100000001b3);
        for &b in data {
            h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
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
    for d in dl.iter() {
        let snap = DropCellSnap {
            eid: d.eid,
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
fn suction_nonempty(hx: i32, hy: i32, hz: i32) -> bool {
    ensure_drop_cells();
    let dc = drop_cells().lock().unwrap_or_else(|e| e.into_inner());
    each_suction_cell(hx, hy, hz, |cx, cy, cz| {
        dc.map
            .get(&(cx, cy, cz))
            .is_some_and(|v| v.iter().any(|s| in_suction_cell(s.x, s.y, s.z, hx, hy, hz)))
    })
}
/// Drops resting in `pos`'s suction cell as spawn-time snapshots, sorted by
/// eid — the same order `hopper_suck` scans them in.
fn suction_snaps(hx: i32, hy: i32, hz: i32) -> Vec<DropCellSnap> {
    ensure_drop_cells();
    let dc = drop_cells().lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    each_suction_cell(hx, hy, hz, |cx, cy, cz| {
        if let Some(v) = dc.map.get(&(cx, cy, cz)) {
            out.extend(
                v.iter()
                    .filter(|s| in_suction_cell(s.x, s.y, s.z, hx, hy, hz))
                    .cloned(),
            );
        }
        false
    });
    out.sort_by_key(|s| s.eid);
    out
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
fn probe_partner(st: u32, pos: (i32, i32, i32)) -> Option<(i32, i32, i32)> {
    chest_pair_of_dim(DimId::Overworld, st, pos).map(|(right, left)| {
        if pos == right {
            left
        } else {
            right
        }
    })
}
/// The watched-state fingerprint for one hopper visit: everything
/// `hopper_push`, `hopper_pull` and `hopper_suck` can observe, and nothing
/// else. Block states cover container-ness and double-chest pairing; the
/// resolved partner positions stand in for the neighbour reads `chest_pair`
/// makes; container maps cover slot contents; pending-loot flags cover the
/// `unpack_block_loot_if_needed` materialization; the suction snaps cover the
/// drop window. `content_revision` rides along so a datapack reload can never
/// hide behind an unchanged world image.
fn hopper_probe_epoch(hx: i32, hy: i32, hz: i32, dst: (i32, i32, i32)) -> u128 {
    let own = (hx, hy, hz);
    let above = (hx, hy + 1, hz);
    let ast = block_at(above.0, above.1, above.2);
    let dst_st = block_at(dst.0, dst.1, dst.2);
    let ap = probe_partner(ast, above);
    let dp = probe_partner(dst_st, dst);
    let mut acc = mixv(content_revision() as u128, 0x5BD1_E995_27D4_EB2F);
    acc = mixv(acc, ast as u64);
    acc = mixv(acc, dst_st as u64);
    acc = mix_pos(acc, ap);
    acc = mix_pos(acc, dp);
    {
        let all = chests().lock().unwrap_or_else(|e| e.into_inner());
        acc = mix_container_map(acc, all.get(&ContainerKey::ow(own)));
        acc = mix_container_map(acc, all.get(&ContainerKey::ow(above)));
        if let Some(p) = ap {
            acc = mix_container_map(acc, all.get(&ContainerKey::ow(p)));
        }
        acc = mix_container_map(acc, all.get(&ContainerKey::ow(dst)));
        if let Some(p) = dp {
            acc = mix_container_map(acc, all.get(&ContainerKey::ow(p)));
        }
    }
    {
        let pend = pending_block_loot().lock().unwrap_or_else(|e| e.into_inner());
        for pos in [Some(own), Some(above), ap, Some(dst), dp]
            .into_iter()
            .flatten()
        {
            acc = mixv(acc, u64::from(pend.contains_key(&ow_block(pos))));
        }
    }
    for s in suction_snaps(hx, hy, hz) {
        acc = mixv(acc, s.eid as u64);
        acc = mixv(acc, s.item as u64);
        acc = mixv(acc, s.comps_hash);
        acc = mixv(acc, s.x.to_bits());
        acc = mixv(acc, s.y.to_bits());
        acc = mixv(acc, s.z.to_bits());
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
static HOPPER_SLEEP: OnceLock<Mutex<HashMap<(i32, i32, i32), HopperSleep>>> = OnceLock::new();
fn hopper_sleep() -> &'static Mutex<HashMap<(i32, i32, i32), HopperSleep>> {
    HOPPER_SLEEP.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Visit accounting the `bot_hopper_*` tests read: real scans versus
/// fingerprint-proven skips.
static HOPPER_SCAN_RUNS: AtomicU64 = AtomicU64::new(0);
static HOPPER_SCAN_SKIPS: AtomicU64 = AtomicU64::new(0);
/// Bound on the sleep map so positions whose hoppers were removed outside the
/// tick path cannot grow it without limit; a sweep only runs past this size.
const HOPPER_SLEEP_MAX: usize = 8192;

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
        if !(9225..=9234).contains(&st) {
            hoppers().lock().unwrap_or_else(|e| e.into_inner()).remove(&(hx, hy, hz));
            hopper_sleep().lock().unwrap_or_else(|e| e.into_inner()).remove(&(hx, hy, hz));
            continue;
        }
        if st >= 9230 {
            sched_result(hx, hy, hz, false);
            requeue_hopper(hx, hy, hz);
            continue;
        } // powered(disabled) hopper holds items
        let facing = st - 9225; // 0 down,1 n,2 s,3 w,4 e
        let (dx, dy, dz) = match facing {
            0 => (0, -1, 0),
            1 => (0, 0, -1),
            2 => (0, 0, 1),
            3 => (-1, 0, 0),
            _ => (1, 0, 0),
        };
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
        let mut did = hopper_push(hx, hy, hz, dst);
        if hopper_pull(hx, hy, hz) {
            did = true;
        } else if hopper_suck(hx, hy, hz) {
            did = true;
        }
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
        sched_result(hx, hy, hz, did);
        requeue_hopper(hx, hy, hz);
    }
}
/// The container slot count a hopper transfer may see at `st`: vanilla reads
/// any `Container` — chests, barrels and shulker boxes are 27 wide, a hopper is
/// `HOPPER_INVENTORY_SIZE` wide — so hopper-to-hopper chains transfer through
/// the same path as a hopper pushing into a chest.
const HOPPER_SLOTS: i16 = HOPPER_INVENTORY_SIZE as i16;
fn hopper_container_slots(st: u32) -> Option<i16> {
    if (9225..=9234).contains(&st) {
        Some(HOPPER_SLOTS)
    } else {
        container_kind(st).map(|_| 27)
    }
}
/// Vanilla `ejectItems`: one item from the hopper's first non-empty slot into
/// the facing container. Runs before any pull and independently of it.
fn hopper_push(hx: i32, hy: i32, hz: i32, dst: (i32, i32, i32)) -> bool {
    let Some(dst_slots) = hopper_container_slots(block_at(dst.0, dst.1, dst.2)) else {
        return false;
    };
    unpack_block_loot_if_needed(ow_block(dst));
    let partner = chest_partner(dst.0, dst.1, dst.2);
    if let Some(partner) = partner {
        unpack_block_loot_if_needed(ow_block(partner));
    }
    let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
    let own = all.get(&ContainerKey::ow((hx, hy, hz))).and_then(|m| {
        m.iter()
            .filter(|(_, v)| v.1 > 0)
            .map(|(k, v)| (*k, v.clone()))
            .min_by_key(|(k, _)| *k)
    });
    let Some((k, (it, _, _cc))) = own else { return false };
    let smax = item_stack_max(it, &_cc) as u8;
    // vanilla merges double chests — push spills to the
    // partner half when the facing half is full
    let dsts = [
        (dst, dst_slots),
        partner.map(|p| (p, 27)).unwrap_or((dst, dst_slots)),
    ];
    for (dp, limit) in dsts {
        let dstm = all.entry(ContainerKey::ow(dp)).or_default();
        let slot = dstm
            .iter()
            .filter(|(s, v)| **s < limit && v.0 == it && v.2 == _cc && v.1 < smax)
            .map(|(s2, _)| *s2)
            .min()
            .or_else(|| (0..limit).find(|sl| !dstm.contains_key(sl)));
        let Some(slot) = slot else { continue }; // full -> try partner
        let cur = dstm.get(&slot).map(|v| v.1).unwrap_or(0);
        dstm.insert(slot, (it, cur + 1, _cc.clone()));
        if let Some(sm) = all.get_mut(&ContainerKey::ow((hx, hy, hz))) {
            if let Some(v) = sm.get_mut(&k) {
                v.1 -= 1;
                if v.1 == 0 {
                    sm.remove(&k);
                }
            }
        }
        return true;
    }
    false
}
/// Vanilla `suckInItems`' container half: one item from the first extractable
/// slot of the container above into the hopper's own five slots — the pulled
/// item joins the hopper's buffer, it does not skip ahead to the push target.
fn hopper_pull(hx: i32, hy: i32, hz: i32) -> bool {
    let src = (hx, hy + 1, hz);
    let Some(src_slots) = hopper_container_slots(block_at(src.0, src.1, src.2)) else {
        return false;
    };
    unpack_block_loot_if_needed(ow_block(src));
    let partner = chest_partner(src.0, src.1, src.2);
    if let Some(partner) = partner {
        unpack_block_loot_if_needed(ow_block(partner));
    }
    let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
    // vanilla merges double chests into one container — a
    // hopper under either half can pull from both
    let srcs = [
        (src, src_slots),
        partner.map(|p| (p, 27)).unwrap_or((src, src_slots)),
    ];
    for (srcp, limit) in srcs {
        let stacks: std::collections::BTreeMap<i16, Stack> = all
            .get(&ContainerKey::ow(srcp))
            .map(|ch| {
                ch.iter()
                    .filter(|(s, v)| **s < limit && v.1 > 0)
                    .map(|(k, v)| (*k, v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (k, (it, c, _xc)) in stacks {
            let smax = item_stack_max(it, &_xc) as u8;
            {
                let own = all.entry(ContainerKey::ow((hx, hy, hz))).or_default();
                // vanilla `addItem` order: merge onto a matching partial slot
                // first, else take the first empty of the hopper's five slots
                let slot = own
                    .iter()
                    .filter(|(s, v)| {
                        **s < HOPPER_SLOTS && v.0 == it && v.2 == _xc && v.1 < smax
                    })
                    .map(|(s, _)| *s)
                    .min()
                    .or_else(|| (0..HOPPER_SLOTS).find(|sl| !own.contains_key(sl)));
                let Some(slot) = slot else { continue }; // hopper full -> next slot
                let cur = own.get(&slot).map(|v| v.1).unwrap_or(0);
                own.insert(slot, (it, cur + 1, _xc.clone()));
            }
            if let Some(srcm) = all.get_mut(&ContainerKey::ow(srcp)) {
                if c <= 1 {
                    srcm.remove(&k);
                } else {
                    srcm.insert(k, (it, c - 1, _xc.clone()));
                }
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
    // O(1) idle exit: the spatial index proves no drop rests in the suction
    // cell, so the vanilla scan below — a full `drops()` walk — cannot find a
    // candidate. When the index says otherwise the original scan runs
    // verbatim, so a stale-extra entry costs one scan and never a wrong take.
    if !suction_nonempty(hx, hy, hz) {
        return false;
    }
    let mut cell: Vec<(i32, i64, u8, Comps)> = {
        let dl = drops().lock().unwrap_or_else(|e| e.into_inner());
        dl.iter()
            .filter(|d| {
                (d.x - (hx as f64 + 0.5)).abs() < 0.7
                    && d.y > hy as f64 + 0.4
                    && d.y < hy as f64 + 1.6
                    && (d.z - (hz as f64 + 0.5)).abs() < 0.7
            })
            .map(|d| (d.eid, d.item, d.cnt, d.comps.clone()))
            .collect()
    };
    cell.sort_by_key(|(eid, ..)| *eid);
    for (deid, item, cnt, dcp) in cell {
        let smax = item_stack_max(item, &dcp) as u8;
        let mut remaining = cnt;
        let moved = {
            let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
            let own = all.entry(ContainerKey::ow((hx, hy, hz))).or_default();
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
        return true;
    }
    false
}
fn drop_tick() {
    // Split under `MC_SPINE_TICK_PROFILE=1`. The bucketed merge landed without a
    // split, so `drops` could not say whether the residual is the pickup scan, the
    // cell build, the neighbourhood walk or the despawn pass.
    let __prof = tickprof::enabled();
    let mut __t = __prof.then(std::time::Instant::now);
    let region = region_guard();
    // 1.21.1 Player.aiStep() touch box + !isSpectator() + ItemEntity.target match
    let mut taken: Vec<(i32, Arc<Client>)> = vec![];
    {
        let ds: Vec<(i32, f64, f64, Option<[u8; 16]>, EntityAabb)> = drops()
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
        let mut collected: Vec<(i32, i32, i16)> = vec![];
        {
            let xs = xps().lock().unwrap_or_else(|e| e.into_inner());
            let cls: Vec<Arc<Client>> = client_snapshot();
            for x in xs.iter() {
                for c in cls.iter() {
                    let (px, py, pz, ..) = {
                        let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
                        *p
                    };
                    if (px - x.x).powi(2) + (py - x.y).powi(2) + (pz - x.z).powi(2) < 2.56 {
                        collected.push((x.eid, c.eid, x.val));
                        break;
                    }
                }
            }
        }
        for (xeid, ceid, val) in collected {
            xps().lock().unwrap_or_else(|e| e.into_inner()).retain(|x| x.eid != xeid);
            let cls: Vec<Arc<Client>> = client_snapshot();
            if let Some(c) = cls.iter().find(|c| c.eid == ceid) {
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
                drop(cls);
                broadcast_seen_destroy(xeid);
            }
        }
    }
    if let Some(t) = __t {
        tickprof::add("drops/xp_pickup", t.elapsed());
    }
    __t = __prof.then(std::time::Instant::now);
    for (deid, c) in taken {
        let selected = *c.held.lock().unwrap();
        let creative = *c.gm.lock().unwrap() == 1;
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
            write_varint(&mut p, 0);
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
        let exp: Vec<i32> = dp.iter().filter(|d| d.age >= life).map(|d| d.eid).collect();
        for eid in &exp {
            dp.retain(|d| d.eid != *eid);
            death_slot_hint_forget(*eid);
            broadcast_seen_destroy(*eid);
        }
        while dp.len() > DROP_CAP {
            let d = dp.remove(0);
            death_slot_hint_forget(d.eid);
            broadcast_seen_destroy(d.eid);
        }
        if let Some(t) = __t {
            tickprof::add("drops/despawn", t.elapsed());
        }
        __t = __prof.then(std::time::Instant::now);
        let merged = merge_drops(&mut dp);
        __t = __prof.then(std::time::Instant::now);
        for eid in &merged {
            dp.retain(|d| d.eid != *eid);
            death_slot_hint_forget(*eid);
            broadcast_seen_destroy(*eid);
        }
        if let Some(t) = __t {
            tickprof::add("drops/merge_destroy", t.elapsed());
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
// furnace block states 4295..=4302 (facing x lit); one store for
// containers regardless of which interface touched them
fn is_furnace_at(x: i32, y: i32, z: i32) -> bool {
    (4295..=4302).contains(&block_at(x, y, z))
}
fn furnace_slots(pos: (i32, i32, i32)) -> HashMap<i16, Stack> {
    let fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
    fs.get(&ow_block(pos))
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
        xps().lock().unwrap_or_else(|e| e.into_inner()).push(Xp {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            x: px,
            y: py + 1.0,
            z: pz,
            val: whole,
            age: 0,
        });
    }
}
fn fuel_ticks(item: i64) -> i32 {
    for (n, t) in [
        ("coal", 1600),
        ("charcoal", 1600),
        ("oak_log", 300),
        ("oak_planks", 300),
        ("stick", 100),
        ("coal_block", 16000),
    ] {
        if name_to_item(n) == Some(item) {
            return server_ticks(t) as i32;
        }
    }
    0
}
static CHESTS: Registry<ContainerKey, HashMap<i16, Stack>> = OnceLock::new();

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
        if let Some(pools) = pools {
            let items = roll_loot_seeded(&pools, seed);
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
                    store.entries.remove(&pos);
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

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
fn loot_json_next_int(rng: &mut crate::generation_math::Lcg, min: i32, max: i32) -> i32 {
    if min >= max {
        min
    } else {
        rng.next_bound(max - min + 1) + min
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn loot_json_shuffle<T>(list: &mut [T], rng: &mut crate::generation_math::Lcg) {
    for i in (2..=list.len()).rev() {
        let j = rng.next_bound(i as i32) as usize;
        list.swap(i - 1, j);
    }
}

// The same split vanilla applies when a rolled stack does not fit the free
// slots: oversized stacks are divided at random, then the survivors shuffle.
#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
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
    let stacks: Vec<Stack> = items.iter().filter_map(loot_json_stack).collect();
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
    let key = ContainerKey::ow(pos);
    audience
        .iter()
        .filter(|c| {
            if c.is_spectator() {
                return false;
            }
            let open = match c.open_chest.lock() {
                Ok(g) => *g,
                Err(_) => return false,
            };
            let Some((a, b)) = open else {
                return false;
            };
            if a == key || b == Some(key) {
                return true;
            }
            if matches!(a, ContainerKey::Block(ek) if is_ender_key((ek.1, ek.2, ek.3))) && b.is_none() {
                return c
                    .chest_sync
                    .lock()
                    .ok()
                    .and_then(|s| s.as_ref().filter(|st| st.window == (a, b)).and_then(|st| st.anchor))
                    == Some(pos);
            }
            false
        })
        .count() as u32
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
    rdim == ldim && chest_pair(rx, ry, rz) == Some(((rx, ry, rz), (lx, ly, lz)))
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
        return anchor.is_some_and(|pos| (7513..=7520).contains(&block_at(pos.0, pos.1, pos.2)));
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
fn invalidate_ender_chest_windows(audience: &[Arc<Client>], pos: BlockPos) -> usize {
    let mut closed = 0;
    for c in audience {
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
            invalidate_ender_chest_windows(audience, pos);
            return;
        }
    }
    invalidate_container_windows(audience, open.0, &chest_window_halves(open));
}

fn container_kind(st: u32) -> Option<&'static str> {
    // 27-slot generic container blocks
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
/// The other half, or `None` when this chest is `single`.
fn chest_partner(x: i32, y: i32, z: i32) -> Option<BlockPos> {
    chest_pair(x, y, z).map(|(right, left)| if (x, y, z) == right { left } else { right })
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
fn client_window_halves(
    open: (ContainerKey, Option<ContainerKey>),
    anchor: Option<BlockPos>,
) -> Vec<BlockPos> {
    if matches!(open, (ContainerKey::Block(ek), None) if is_ender_key((ek.1, ek.2, ek.3))) {
        return anchor.into_iter().collect();
    }
    chest_window_halves(open)
}
fn client_open_window_and_anchor(
    c: &Client,
) -> Option<((ContainerKey, Option<ContainerKey>), Option<BlockPos>)> {
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
    chest_window_halves(window)
        .into_iter()
        .any(is_chest_blocked_at)
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
                anchor.is_some_and(|pos| {
                    (7513..=7520).contains(&block_at(pos.0, pos.1, pos.2))
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
            let halves = client_window_halves(win, anchor);
            let before = chest_opener_snapshot(audience, &halves);
            *c.open_chest.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *c.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x12, vec![1u8]);
            }
            sync_chest_openers(audience, &before);
            continue;
        }
        let current = chest_window_slots(win);
        let updates: Vec<(i16, i32, Stack)> = {
            let mut sync_guard = c.chest_sync.lock().unwrap_or_else(|e| e.into_inner());
            let sync = sync_guard.get_or_insert_with(|| ChestSyncState {
                window: win,
                anchor,
                state_id: 0,
                last_slots: vec![(0, 0, Vec::new()); current.len()],
            });
            if sync.window != win || sync.last_slots.len() != current.len() {
                sync.window = win;
                sync.anchor = anchor;
                sync.state_id = 0;
                sync.last_slots = vec![(0, 0, Vec::new()); current.len()];
            }
            let mut out = Vec::new();
            for (idx, stack) in current.iter().enumerate() {
                if sync.last_slots[idx] != *stack {
                    sync.state_id = (sync.state_id + 1) & 0x7fff;
                    sync.last_slots[idx] = stack.clone();
                    out.push((idx as i16, sync.state_id, stack.clone()));
                }
            }
            out
        };
        if !updates.is_empty() {
            if let Ok(mut w) = c.sock.lock() {
                for (slot, state_id, stack) in updates {
                    let mut p = vec![1u8];
                    write_varint(&mut p, i64::from(state_id));
                    p.extend_from_slice(&slot.to_be_bytes());
                    p.extend_from_slice(&slot_wire(&stack));
                    let _ = send_packet(&mut w, 0x15, p);
                }
            }
        }
    }
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
const ITEM_NAMES: [&str; 1333] = [
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
                1 => {
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
                    inner.insert(NbtString::text("show_in_tooltip"), Tag::Byte(i8::from(show)));
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
                    inner.insert(NbtString::text("show_in_tooltip"), Tag::Byte(i8::from(show)));
                    let key = if *ty == COMP_STORED_ENCHANTMENTS {
                        "minecraft:stored_enchantments"
                    } else {
                        "minecraft:enchantments"
                    };
                    comp_map.insert(NbtString::text(key), Tag::Compound(inner));
                }
                16 => {
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
                        inner.insert(
                            NbtString::text("show_in_tooltip"),
                            Tag::Byte(i8::from(amc.show_in_tooltip)),
                        );
                        comp_map.insert(
                            NbtString::text("minecraft:attribute_modifiers"),
                            Tag::Compound(inner),
                        );
                    }
                }
                _ => {}
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
                            comp_set_varint(&mut comps, 1, n);
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
                            comp_set_varint(&mut comps, 16, n);
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
                                    .unwrap_or_else(|| "mc-spine:custom".to_string());
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
                    _ => {}
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

#[derive(Debug, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ChiseledBookshelf {
    pub(crate) slots: [Option<BlockEntityItem>; 6],
    last_interacted_slot: Option<usize>,
}

impl Default for ChiseledBookshelf {
    fn default() -> Self {
        Self {
            slots: Default::default(),
            last_interacted_slot: None,
        }
    }
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
    for (bytes, word) in uuid.chunks_exact_mut(4).zip(parts) {
        bytes.copy_from_slice(&word.to_be_bytes());
    }
    Some(uuid)
}

fn block_entity_uuid_nbt(uuid: &BlockEntityUuid) -> Tag {
    Tag::IntArray(
        uuid.chunks_exact(4)
            .map(|bytes| i32::from_be_bytes(bytes.try_into().expect("four bytes per UUID word")))
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
            id: tag.get("id").and_then(block_entity_uuid),
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
        .filter_map(block_entity_uuid)
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
        if self.rewarded_players.len() > 128 {
            self.rewarded_players.remove(0);
        }
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
    Crafter(Crafter, Option<i64>),
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
            "minecraft:crafter" => Self::Crafter(Crafter::from_nbt(tag), None),
            "minecraft:jukebox" => Self::Jukebox(Jukebox::from_nbt(tag, live_jukebox_song)),
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
    chunk_versions: HashMap<DimChunkKey, u64>,
    scanned_at: u64,
}

fn live_block_entities() -> &'static Mutex<LiveBlockEntityStore> {
    static STORE: OnceLock<Mutex<LiveBlockEntityStore>> = OnceLock::new();
    STORE.get_or_init(Mutex::default)
}

impl LiveBlockEntityStore {
    fn at(&mut self, reg: &BReg, key: DimBlockPos, state: u32) -> Option<&mut LiveBlockEntity> {
        let block = reg.block_id_of(state)?;
        if self.entries.get(&key).is_some_and(|&(id, _)| id != block) {
            self.entries.remove(&key);
        }
        let id = live_block_entity_id(reg.name_of(state))?;
        Some(
            &mut self
                .entries
                .entry(key)
                .or_insert_with(|| (block, LiveBlockEntity::load(reg, state, id, &Tag::End)))
                .1,
        )
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
            let edits = world().lock().unwrap_or_else(|e| e.into_inner());
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
            .entries
            .insert(key, (block, entity));
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
    if live_block_entity_id(reg.name_of(state)).is_none() || c.is_spectator() {
        return false;
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
                    } else if box_.insert(&mut held, live_jukebox_song) {
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
                            store.entries.remove(&key);
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
            send_live_contents(&mut sock, 0, 0, &[], &inv, 0..46, &cursor);
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
        COMP_ARMOR_TRIM => {
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
}
fn write_open_chest_packets(
    s: &mut TcpStream,
    wide: bool,
    initial: &[Stack],
    inv: &HashMap<i16, Stack>,
    title: &str,
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
    write_varint(&mut q, 0);
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
fn open_chest_window(
    s: &mut TcpStream,
    first: ContainerKey,
    second: Option<ContainerKey>,
    inv: &HashMap<i16, Stack>,
    title: &str,
) {
    let initial = chest_window_slots((first, second));
    write_open_chest_packets(s, second.is_some(), &initial, inv, title);
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
    }
    let initial = chest_window_slots(window);
    *me.open_chest.lock().unwrap_or_else(|e| e.into_inner()) = Some(window);
    *me.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = Some(ChestSyncState {
        window,
        anchor,
        state_id: 0,
        last_slots: initial.clone(),
    });
    if let Ok(mut w) = me.sock.lock() {
        write_open_chest_packets(&mut w, window.1.is_some(), &initial, inv, title);
    }
}
fn armor_removal_blocked(slot: i16, stack: Option<&Stack>, creative: bool) -> bool {
    !creative
        && (5..=8).contains(&slot)
        && stack.is_some_and(|stack| stack.1 > 0 && enchant_level(&stack.2, 2) > 0)
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
    let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
    drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
        eid,
        x,
        y,
        z,
        item: stack.0,
        cnt: stack.1,
        comps: stack.2,
        age: -(server_ticks(30) as i32),
        target: None,
        thrower: None,
    });
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
        return;
    }
    let mut p = vec![wid];
    write_varint(&mut p, 0);
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
        if let Some(st) = m.get(&slot) {
            let amount = if btn == 0 { st.1 } else { st.1.div_ceil(2) };
            if let Some(taken) = map_take(m, slot, amount) {
                *cur = taken;
            }
        }
        return;
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
    None
}
fn open_furnace_window(s: &mut TcpStream, inv: &HashMap<i16, Stack>, slots: &[Stack; 3]) {
    let mut p = Vec::new();
    write_varint(&mut p, 2); // windowId 2 (chest uses 1)
    write_varint(&mut p, 14); // furnace menu
    p.extend_from_slice(&chat_nbt("Furnace"));
    let _ = send_packet(s, 0x33, p);
    furnace_push_items_snap(s, inv, slots);
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
fn furnace_push_items_snap(s: &mut TcpStream, inv: &HashMap<i16, Stack>, slots: &[Stack; 3]) {
    let mut q = vec![2u8];
    write_varint(&mut q, 0);
    write_varint(&mut q, 39);
    for slot in slots {
        q.extend_from_slice(&slot_wire(slot));
    }
    for wsl in 3..39i16 {
        let (it, c, _cc) = inv.get(&(wsl + 6)).cloned().unwrap_or((0, 0, Vec::new()));
        q.extend_from_slice(&slot_wire(&(it, c, _cc)));
    }
    q.extend_from_slice(&slot_wire(&(0, 0, Vec::new())));
    let _ = send_packet(s, 0x13, q);
}
// The frozen reference in furnace_cache_tests still exercises the original signature.
#[cfg(test)]
fn furnace_push_items(s: &mut TcpStream, pos: (i32, i32, i32), inv: &HashMap<i16, Stack>) {
    let slots = furnace_slot_snapshot_dim(DimId::Overworld, pos);
    furnace_push_items_snap(s, inv, &slots);
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
    if comp_get(&base.2, COMP_ARMOR_TRIM).and_then(|data| data.get(..2)) == Some(&trim[..2]) {
        return None;
    }
    let mut result = (base.0, 1, base.2.clone());
    comp_set(&mut result.2, COMP_ARMOR_TRIM, trim.to_vec());
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
                LiveBlockEntity::Jukebox(box_) => box_.comparator_signal(live_jukebox_song),
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
    None
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
    let Some(connection) = live_menu_connection(c) else {
        return;
    };
    close_live_menu(c, false, false);
    if let Some((window, old_anchor)) = client_open_window_and_anchor(c) {
        let audience = client_snapshot();
        let before = chest_opener_snapshot(&audience, &client_window_halves(window, old_anchor));
        *c.open_chest.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *c.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
        sync_chest_openers(&audience, &before);
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

fn anvil_hover_name(stack: &Stack) -> &str {
    if let Some([8, hi, lo, text @ ..]) = comp_get(&stack.2, COMP_CUSTOM_NAME) {
        if let Some(name) = text.get(..u16::from_be_bytes([*hi, *lo]) as usize) {
            return std::str::from_utf8(name).unwrap_or_default();
        }
    }
    item_to_name(stack.0).unwrap_or_default()
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
            // Holder streams reserve zero for inline definitions (COMP_ARMOR_TRIM).
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
    let map_id = comp_varint(&stack.2, WM_COMP_MAP_ID)?;
    Some(
        live_map_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&map_id)
            .copied()
            .unwrap_or(MapState {
                map_id,
                scale: 0,
                locked: false,
            }),
    )
}

fn sync_live_menu(c: &Client, session: &mut LiveMenuSession, send_offers: bool) {
    let mut properties = Vec::new();
    let mut offers = None;
    let slots = match &mut session.menu {
        LiveMenu::Anvil(menu) => {
            let result = menu.result(anvil_hover_name(&menu.left), *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1);
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
                .active_offer(&villager.offers, &MerchantItems(&merchant_item_prototype))
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
                    comp_set_varint(&mut result.2, WM_COMP_MAP_ID, state.map_id);
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
                    value: beacon.primary.map_or(-1, beacon_effect_wire_id),
                },
                WindowPropertyUpdate {
                    property: 2,
                    value: beacon.secondary.map_or(-1, beacon_effect_wire_id),
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
    drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
        eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
        x,
        y,
        z,
        item: stack.0,
        cnt: stack.1,
        comps: stack.2,
        age: -(server_ticks(30) as i32),
        target: None,
        thrower: None,
    });
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
        send_live_contents(&mut sock, 0, 0, &[], &inventory, 0..46, &Stack::default());
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
                    &MerchantItems(&merchant_item_prototype),
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
                let name = anvil_hover_name(&menu.left).to_owned();
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
                let items = MerchantItems(&merchant_item_prototype);
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
        xps().lock().unwrap_or_else(|e| e.into_inner()).push(Xp {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            x,
            y: y + 1.0,
            z,
            val: (3 + world_rng::next_u64() % 4) as i16,
            age: 0,
        });
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
        let mut updates = HashMap::new();
        for (&key, entry) in live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
            let (mut slots_changed, mut properties_changed, mut eject) = (false, false, false);
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
        if updates.is_empty() {
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
                    let (fish, fcnt, fcomps) = table
                        .and_then(|p| roll_loot(&p).into_iter().next())
                        .unwrap_or((
                            if (b.age as u64).is_multiple_of(5) {
                                936
                            } else {
                                935
                            },
                            1,
                            Vec::new(),
                        ));
                    let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                    let slot = (9..=44).find(|sl| !inv.contains_key(sl)).unwrap_or(9);
                    inv.insert(slot, (fish, fcnt, fcomps.clone()));
                    let mut p = vec![4u8];
                    write_varint(&mut p, 0);
                    p.extend_from_slice(&slot.to_be_bytes());
                    p.extend_from_slice(&slot_wire(&(fish, fcnt, fcomps)));
                    let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x15, p);
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
                write_varint(&mut p, 0);
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
        if main == 801 || main == 1192 || main == name_to_item("trident").unwrap_or(-9) {
            *me.bow_draw.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            return;
        }
    }
    if *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1 {
        // prefer held slot; fall back to first food item in inventory
        let (slot, it) = {
            let inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
            let hs = 36 + held_slot;
            match inv.get(&hs) {
                Some((it, _, _)) if food_value(*it) > 0 => (hs, *it),
                _ => inv
                    .iter()
                    .find(|(_, (i, _, _))| food_value(*i) > 0)
                    .map(|(&sl, (i, _, _))| (sl, *i))
                    .unwrap_or((hs, 0)),
            }
        };
        let fv = food_value(it);
        let cur_food = *me.food.lock().unwrap_or_else(|e| e.into_inner());
        let edible = fv > 0 && (cur_food < 20 || always_edible(it));
        if edible && !eat_release {
            // vanilla: eating holds use for 32 ticks; nothing consumed yet
            *me.eat_start.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }
        if edible && eat_release {
            let mut f = me.food.lock().unwrap_or_else(|e| e.into_inner());
            let mut st = me.sat.lock().unwrap_or_else(|e| e.into_inner());
            (*f, *st) = FoodData::values_after_eating(*f, *st, fv, sat_value(it));
            drop(st);
            drop(f);
            let ncnt = {
                let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                let (nc, cc2) = inv
                    .get(&slot)
                    .map(|e| (e.1.saturating_sub(1), e.2.clone()))
                    .unwrap_or((0, Vec::new()));
                if nc == 0 {
                    inv.remove(&slot);
                } else {
                    inv.insert(slot, (it, nc, cc2));
                }
                nc
            };
            let mut p = vec![0u8];
            write_varint(&mut p, 0);
            p.extend_from_slice(&slot.to_be_bytes());
            write_varint(&mut p, ncnt as i64);
            if ncnt > 0 {
                write_varint(&mut p, it);
                write_varint(&mut p, 0);
                write_varint(&mut p, 0);
            }
            let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x15, p);
            let hh = *me.hp.lock().unwrap_or_else(|e| e.into_inner());
            let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x5d, health_pkt_c(hh, me));
            // vanilla food side-effects (golden apple, pufferfish, ...)
            for &(ch, eid2, amp, ticks) in food_effects(it) {
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
            // chorus fruit: random teleport within +-8 blocks
            if it == name_to_item("chorus_fruit").unwrap_or(-9) {
                let (px, py, pz, ..) = {
                    let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
                    *p
                };
                for _ in 0..16 {
                    let nx = px + ((world_rng::next_u64() % 1600) as f64 / 100.0) - 8.0;
                    let nz = pz + ((world_rng::next_u64() % 1600) as f64 / 100.0) - 8.0;
                    let ny = surface_y(nx.round() as i32, nz.round() as i32);
                    if (ny - py).abs() < 8.0 {
                        let (_, _, _, yaw, pitch, _) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
                        teleport_in_dimension(
                            me,
                            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                            (nx, ny, nz, yaw, pitch),
                        );
                        break;
                    }
                }
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
            modifier_id: "mc-spine:test".to_string(),
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
    use crate::effect_tests::client_pair;
    use std::io::Read;

    /// Restores the block-entity store, placed cells, menu sessions, clients
    /// and drops around a live beacon-menu test.
    struct BeaconWorld {
        store: LiveBlockEntityStore,
        cells: Vec<(DimBlockPos, u32)>,
        sessions: HashMap<i32, PlayerMechanismSession>,
        players: Vec<Arc<Client>>,
    }

    impl BeaconWorld {
        fn new() -> Self {
            Self {
                store: std::mem::take(&mut *live_block_entities().lock().unwrap_or_else(|e| e.into_inner())),
                cells: Vec::new(),
                sessions: std::mem::take(&mut *player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner())),
                players: std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner())),
            }
        }

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

        hopper_tick();
        // Second activation: the pulled item is the lowest occupied slot, so
        // it is what gets pushed next while another item is pulled in.
        assert_eq!(stacks_at((0, hy - 1, 0)), vec![(pull_item, 1), (push_item, 1)]);
        assert_eq!(stacks_at((0, hy + 1, 0)), vec![(pull_item, 1)]);
        assert_eq!(stacks_at(HPOS), vec![(pull_item, 1)]);
        restore_world(saved);
    }

    #[test]
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
        hopper_tick();
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

#[cfg(test)]
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
        assert_eq!(comp_get(&result.2, COMP_ARMOR_TRIM), Some([10, 1, 1].as_slice()));
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
                ..Default::default()
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
            .entries
            .insert(
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
            if next() % 4 == 0 {
                comp_set_varint(&mut comps, COMP_DAMAGE, (next() % 8) as i64);
            }
            if next() % 16 == 0 {
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
    fn warm_chunks(x0: i32, z0: i32, x1: i32, z1: i32) -> (HashSet<(i32, i32)>, HashSet<(i32, i32)>) {
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
                90000 + i as i32,
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
