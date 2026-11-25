// R7 GP-4. The world supplies block changes, movement, trade inputs, and raid
// spawn/defeat callbacks. All timers use game ticks.

#[cfg_attr(not(test), allow(dead_code))]
type VillageUuid = [u8; 16];

#[cfg_attr(not(test), allow(dead_code))]
const VILLAGER_DAY_TICKS: u64 = 24_000;
#[cfg_attr(not(test), allow(dead_code))]
const VILLAGER_RESTOCK_COOLDOWN: u64 = 2_000;
#[cfg_attr(not(test), allow(dead_code))]
const VILLAGER_GOSSIP_COOLDOWN: u64 = 1_200;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum VillagerProfession {
    #[default]
    None,
    Armorer,
    Butcher,
    Cartographer,
    Cleric,
    Farmer,
    Fisherman,
    Fletcher,
    Leatherworker,
    Librarian,
    Mason,
    Nitwit,
    Shepherd,
    Toolsmith,
    Weaponsmith,
}

impl VillagerProfession {
    #[cfg_attr(not(test), allow(dead_code))]
    fn from_job_site(block_name: &str) -> Option<Self> {
        let name = block_name.strip_prefix("minecraft:").unwrap_or(block_name);
        Some(match name {
            "blast_furnace" => Self::Armorer,
            "smoker" => Self::Butcher,
            "cartography_table" => Self::Cartographer,
            "brewing_stand" => Self::Cleric,
            "composter" => Self::Farmer,
            "barrel" => Self::Fisherman,
            "fletching_table" => Self::Fletcher,
            "cauldron" | "water_cauldron" | "lava_cauldron" | "powder_snow_cauldron" => {
                Self::Leatherworker
            }
            "lectern" => Self::Librarian,
            "stonecutter" => Self::Mason,
            "loom" => Self::Shepherd,
            "smithing_table" => Self::Toolsmith,
            "grindstone" => Self::Weaponsmith,
            _ => return None,
        })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PoiKind {
    Bed,
    JobSite(VillagerProfession),
    Bell,
}

#[cfg_attr(not(test), allow(dead_code))]
fn poi_kind_from_block_name(block_name: &str) -> Option<PoiKind> {
    let name = block_name.strip_prefix("minecraft:").unwrap_or(block_name);
    if matches!(name, "bell") {
        return Some(PoiKind::Bell);
    }
    if name.strip_suffix("_bed").is_some_and(|color| {
        matches!(
            color,
            "white"
                | "orange"
                | "magenta"
                | "light_blue"
                | "yellow"
                | "lime"
                | "pink"
                | "gray"
                | "light_gray"
                | "cyan"
                | "purple"
                | "blue"
                | "brown"
                | "green"
                | "red"
                | "black"
        )
    }) {
        return Some(PoiKind::Bed);
    }
    VillagerProfession::from_job_site(block_name).map(PoiKind::JobSite)
}

#[cfg_attr(not(test), allow(dead_code))]
struct PoiRecord {
    kind: PoiKind,
    claimants: Vec<VillageUuid>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct PoiRegistry {
    // One registry per dimension. A bed is registered once, at its head.
    records: HashMap<BlockPos, PoiRecord>,
}

impl PoiRegistry {
    #[cfg_attr(not(test), allow(dead_code))]
    fn register(&mut self, pos: BlockPos, kind: PoiKind) {
        if !self.records.get(&pos).is_some_and(|poi| poi.kind == kind) {
            self.records.insert(
                pos,
                PoiRecord {
                    kind,
                    claimants: Vec::new(),
                },
            );
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn remove(&mut self, pos: BlockPos) {
        self.records.remove(&pos);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn claim(&mut self, pos: BlockPos, villager: VillageUuid) -> bool {
        let Some(poi) = self.records.get_mut(&pos) else {
            return false;
        };
        // PoiTypes: a home or job site has one ticket, a meeting point has 32.
        let capacity = if poi.kind == PoiKind::Bell { 32 } else { 1 };
        if poi.claimants.contains(&villager) {
            return true;
        }
        if poi.claimants.len() >= capacity {
            return false;
        }
        poi.claimants.push(villager);
        true
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn release(&mut self, pos: BlockPos, villager: VillageUuid) {
        if let Some(poi) = self.records.get_mut(&pos) {
            poi.claimants.retain(|&claimant| claimant != villager);
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn has_claim(&self, pos: BlockPos, kind: PoiKind, villager: VillageUuid) -> bool {
        self.records
            .get(&pos)
            .is_some_and(|poi| poi.kind == kind && poi.claimants.contains(&villager))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn claimed_near(&self, origin: BlockPos) -> impl Iterator<Item = (BlockPos, PoiKind)> + '_ {
        self.records.iter().filter_map(move |(&(x, y, z), poi)| {
            let in_range = x.div_euclid(16).abs_diff(origin.0.div_euclid(16)) <= 3
                && z.div_euclid(16).abs_diff(origin.2.div_euclid(16)) <= 3;
            (in_range && !poi.claimants.is_empty()).then_some(((x, y, z), poi.kind))
        })
    }
}

/// Reconcile an inclusive world-space box against BReg block states.
/// Only a bed's head is a vanilla home POI. The caller supplies loaded states
/// from one dimension; removed or changed sites invalidate their old claims.
#[cfg_attr(not(test), allow(dead_code))]
fn scan_poi_box(
    pois: &mut PoiRegistry,
    reg: &crate::block_registry::BReg,
    min: BlockPos,
    max: BlockPos,
    mut state_at: impl FnMut(BlockPos) -> u32,
) -> usize {
    let mut found = 0;
    for x in min.0..=max.0 {
        for y in min.1..=max.1 {
            for z in min.2..=max.2 {
                let pos = (x, y, z);
                let state = state_at(pos);
                let mut kind = poi_kind_from_block_name(reg.name_of(state));
                if kind == Some(PoiKind::Bed)
                    && !reg.properties_of(state).is_some_and(|properties| {
                        properties
                            .iter()
                            .any(|(key, value)| key == "part" && value == "head")
                    })
                {
                    kind = None;
                }
                if let Some(kind) = kind {
                    pois.register(pos, kind);
                    found += 1;
                } else {
                    pois.remove(pos);
                }
            }
        }
    }
    found
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VillagerActivity {
    Idle,
    Work,
    Meet,
    Rest,
    Play,
    Panic,
}

impl VillagerActivity {
    #[cfg_attr(not(test), allow(dead_code))]
    fn at(world_time: u64) -> Self {
        match world_time % VILLAGER_DAY_TICKS {
            2_000..=8_999 => Self::Work,
            9_000..=10_999 => Self::Meet,
            12_000..=23_999 => Self::Rest,
            _ => Self::Idle,
        }
    }
    fn baby_at(world_time: u64) -> Self {
        match world_time % VILLAGER_DAY_TICKS {
            3_000..=5_999 | 10_000..=11_999 => Self::Play,
            12_000..=23_999 => Self::Rest,
            _ => Self::Idle,
        }
    }
    fn brain_at(world_time: u64, baby: bool, panicking: bool) -> Self {
        if panicking {
            Self::Panic
        } else if baby {
            Self::baby_at(world_time)
        } else {
            Self::at(world_time)
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VillagerLevel {
    Novice = 1,
    Apprentice,
    Journeyman,
    Expert,
    Master,
}

impl VillagerLevel {
    #[cfg_attr(not(test), allow(dead_code))]
    const XP_THRESHOLDS: [i32; 5] = [0, 10, 70, 150, 250];

    #[cfg_attr(not(test), allow(dead_code))]
    fn from_xp(xp: i32) -> Self {
        use VillagerLevel::*;
        let levels = [Novice, Apprentice, Journeyman, Expert, Master];
        let unlocked = Self::XP_THRESHOLDS.partition_point(|&threshold| threshold <= xp);
        levels[unlocked.saturating_sub(1)]
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct ItemCost {
    item: i64,
    count: i32,
    components: Comps,
}

impl ItemCost {
    #[cfg_attr(not(test), allow(dead_code))]
    fn write(&self, out: &mut Vec<u8>) {
        write_varint(out, self.item);
        write_varint(out, i64::from(self.count));
        write_varint(out, self.components.len() as i64);
        for (ty, payload) in &self.components {
            write_varint(out, *ty);
            out.extend_from_slice(payload);
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn matches(&self, stack: &Stack, items: &MerchantItems<'_>) -> bool {
        !merchant_stack_empty(stack)
            && stack.0 == self.item
            && self.components.iter().all(|(ty, value)| {
                items.component(stack.0, &stack.2, *ty) == Some(value.as_slice())
            })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn read(data: &mut &[u8]) -> Option<Self> {
        let item = i64::from(read_protocol_i32(data)?);
        let count = read_protocol_i32(data)?;
        if item <= 0 || count <= 0 {
            return None;
        }
        let components = usize::try_from(read_protocol_i32(data)?).ok()?;
        Some(Self {
            item,
            count,
            components: read_merchant_components(data, components)?,
        })
    }
}

// The stack codec holds component additions. Supply the registered item prototypes
// and use canonical payloads for component values in both stacks and predicates.
#[cfg_attr(not(test), allow(dead_code))]
struct MerchantItems<'a>(&'a dyn Fn(i64) -> &'a Comps);

#[cfg_attr(not(test), allow(dead_code))]
const MERCHANT_MAX_STACK_SIZE: i64 = 1;

impl MerchantItems<'_> {
    #[cfg_attr(not(test), allow(dead_code))]
    fn component<'a>(&'a self, item: i64, patch: &'a Comps, ty: i64) -> Option<&'a [u8]> {
        comp_get(patch, ty).or_else(|| comp_get((self.0)(item), ty))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn max_stack_size(&self, item: i64, patch: &Comps) -> u8 {
        self.component(item, patch, MERCHANT_MAX_STACK_SIZE)
            .and_then(|mut data| {
                let size = read_protocol_i32(&mut data)?;
                (data.is_empty() && (1..=99).contains(&size)).then_some(size as u8)
            })
            .unwrap_or(1)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn stackable(&self, stack: &Stack) -> bool {
        self.max_stack_size(stack.0, &stack.2) > 1
            && !(self.component(stack.0, &stack.2, COMP_MAX_DAMAGE).is_some()
                && self
                    .component(stack.0, &stack.2, COMP_UNBREAKABLE)
                    .is_none()
                && self
                    .component(stack.0, &stack.2, COMP_DAMAGE)
                    .and_then(|mut data| read_protocol_i32(&mut data))
                    .is_some_and(|damage| damage > 0))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn same_components(&self, a: &Stack, b: &Stack) -> bool {
        a.0 == b.0
            && a.2
                .iter()
                .all(|(ty, value)| self.component(b.0, &b.2, *ty) == Some(value.as_slice()))
            && b.2
                .iter()
                .all(|(ty, value)| self.component(a.0, &a.2, *ty) == Some(value.as_slice()))
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
struct MerchantOffer {
    cost_a: ItemCost,
    cost_b: Option<ItemCost>,
    result: Stack,
    uses: i32,
    max_uses: i32,
    xp_reward: i32,
    price_multiplier: f32,
    demand: i32,
    special_price_diff: i32,
}

impl MerchantOffer {
    #[cfg_attr(not(test), allow(dead_code))]
    fn update_demand(&mut self) {
        self.demand = self
            .demand
            .wrapping_add(self.uses)
            .wrapping_sub(self.max_uses.wrapping_sub(self.uses));
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn cost_a_count(&self, items: &MerchantItems<'_>) -> i32 {
        let base = self.cost_a.count;
        let demand_product = base.wrapping_mul(self.demand);
        let demand_diff = merchant_floor(demand_product as f32 * self.price_multiplier);
        let _ = items;
        base.wrapping_add(demand_diff.max(0))
            .wrapping_add(self.special_price_diff)
            .clamp(1, 64)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn apply_customer_price(&mut self, reputation: i32, hero_amplifier: Option<u8>) {
        if reputation != 0 {
            let adjustment = merchant_floor(reputation as f32 * self.price_multiplier);
            self.special_price_diff = self.special_price_diff.wrapping_sub(adjustment);
        }
        if let Some(amplifier) = hero_amplifier {
            let reduction = 0.3 + 0.0625 * f64::from(amplifier);
            let discount = (f64::from(self.cost_a.count) * reduction).floor() as i32;
            self.special_price_diff = self.special_price_diff.wrapping_sub(discount.max(1));
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn out_of_stock(&self) -> bool {
        self.uses >= self.max_uses
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn payment_counts(
        &self,
        a: &Stack,
        b: &Stack,
        items: &MerchantItems<'_>,
    ) -> Result<[u8; 2], TradeRejection> {
        if !self.cost_a.matches(a, items) {
            return Err(TradeRejection::WrongItemA);
        }
        let required_a = self.cost_a_count(items) as u8;
        if a.1 < required_a {
            return Err(TradeRejection::InsufficientPaymentA);
        }
        let required_b = match &self.cost_b {
            Some(cost) => {
                if !cost.matches(b, items) {
                    return Err(TradeRejection::WrongItemB);
                }
                if i32::from(b.1) < cost.count {
                    return Err(TradeRejection::InsufficientPaymentB);
                }
                u8::try_from(cost.count)
                    .ok()
                    .filter(|&count| count > 0)
                    .ok_or(TradeRejection::InvalidOffer)?
            }
            None if merchant_stack_empty(b) => 0,
            None => return Err(TradeRejection::WrongItemB),
        };
        Ok([required_a, required_b])
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn take(
        &mut self,
        payments: &mut [Stack; 2],
        items: &MerchantItems<'_>,
    ) -> Result<[u8; 2], TradeRejection> {
        if self.cost_a.item <= 0
            || self.cost_a.count <= 0
            || self
                .cost_b
                .as_ref()
                .is_some_and(|cost| cost.item <= 0 || cost.count <= 0)
            || self.result.0 <= 0
            || merchant_stack_empty(&self.result)
            || self.uses < 0
            || self.max_uses <= 0
            || self.xp_reward < 0
            || !self.price_multiplier.is_finite()
        {
            return Err(TradeRejection::InvalidOffer);
        }
        if self.out_of_stock() {
            return Err(TradeRejection::OutOfStock);
        }

        let [a, b] = payments;
        let (counts, [a, b]) = match self.payment_counts(a, b, items) {
            Ok(counts) => (counts, [a, b]),
            Err(error) => (self.payment_counts(b, a, items).map_err(|_| error)?, [b, a]),
        };
        shrink_merchant_stack(a, counts[0]);
        if counts[1] > 0 {
            shrink_merchant_stack(b, counts[1]);
        }
        self.uses += 1;
        Ok(counts)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn merchant_floor(value: f32) -> i32 {
    let truncated = value as i32;
    // Mth.floor casts to a Java int before its corrective decrement.
    if value < truncated as f32 {
        truncated.wrapping_sub(1)
    } else {
        truncated
    }
}

// Protocol 767 Play. Both directions assign 0x2d independently.
#[cfg_attr(not(test), allow(dead_code))]
const MERCHANT_OFFERS_PACKET_ID: i64 = 0x2d;
#[cfg_attr(not(test), allow(dead_code))]
const SELECT_TRADE_PACKET_ID: i64 = 0x2d;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
struct MerchantOffersPacket {
    window_id: i32,
    offers: Vec<MerchantOffer>,
    villager_level: i32,
    villager_xp: i32,
    show_progress: bool,
    can_restock: bool,
}

/// Payload of ClientboundMerchantOffersPacket (Protocol 767 Play).
#[cfg_attr(not(test), allow(dead_code))]
fn encode_merchant_offers_pkt(packet: &MerchantOffersPacket) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, i64::from(packet.window_id));
    write_varint(&mut out, packet.offers.len() as i64);
    for offer in &packet.offers {
        offer.cost_a.write(&mut out);
        if merchant_stack_empty(&offer.result) {
            write_varint(&mut out, 0);
        } else {
            out.extend_from_slice(&slot_wire(&offer.result));
        }
        out.push(u8::from(offer.cost_b.is_some()));
        if let Some(cost) = &offer.cost_b {
            cost.write(&mut out);
        }
        out.push(u8::from(offer.out_of_stock()));
        out.extend_from_slice(&offer.uses.to_be_bytes());
        out.extend_from_slice(&offer.max_uses.to_be_bytes());
        out.extend_from_slice(&offer.xp_reward.to_be_bytes());
        out.extend_from_slice(&offer.special_price_diff.to_be_bytes());
        out.extend_from_slice(&offer.price_multiplier.to_be_bytes());
        out.extend_from_slice(&offer.demand.to_be_bytes());
    }
    write_varint(&mut out, i64::from(packet.villager_level));
    write_varint(&mut out, i64::from(packet.villager_xp));
    out.extend([u8::from(packet.show_progress), u8::from(packet.can_restock)]);
    out
}

#[cfg_attr(not(test), allow(dead_code))]
fn decode_merchant_offers_pkt(mut data: &[u8]) -> Option<MerchantOffersPacket> {
    let window_id = read_protocol_i32(&mut data)?;
    let count = usize::try_from(read_protocol_i32(&mut data)?).ok()?;
    // ItemCost (3), empty Slot (1), two flags (2), and six fixed-width fields.
    if count > data.len() / 30 {
        return None;
    }
    let mut offers = Vec::with_capacity(count);
    for _ in 0..count {
        let cost_a = ItemCost::read(&mut data)?;
        let result = read_merchant_result(&mut data)?;
        let cost_b = if read_merchant_bool(&mut data)? {
            Some(ItemCost::read(&mut data)?)
        } else {
            None
        };
        let out_of_stock = read_merchant_bool(&mut data)?;
        let (fields, rest) = data.split_at_checked(24)?;
        data = rest;
        let [uses, max_uses, xp_reward, special_price_diff, multiplier, demand] =
            std::array::from_fn(|i| {
                i32::from_be_bytes(fields[4 * i..4 * (i + 1)].try_into().unwrap())
            });
        offers.push(MerchantOffer {
            cost_a,
            cost_b,
            result,
            // MerchantOffer.createFromStream calls setToOutOfStock when the flag is set.
            uses: if out_of_stock { max_uses } else { uses },
            max_uses,
            xp_reward,
            special_price_diff,
            price_multiplier: f32::from_bits(multiplier as u32),
            demand,
        });
    }
    let packet = MerchantOffersPacket {
        window_id,
        offers,
        villager_level: read_protocol_i32(&mut data)?,
        villager_xp: read_protocol_i32(&mut data)?,
        show_progress: read_merchant_bool(&mut data)?,
        can_restock: read_merchant_bool(&mut data)?,
    };
    data.is_empty().then_some(packet)
}

#[cfg_attr(not(test), allow(dead_code))]
fn read_merchant_bool(data: &mut &[u8]) -> Option<bool> {
    let (&value, rest) = data.split_first()?;
    *data = rest;
    match value {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn read_merchant_result(data: &mut &[u8]) -> Option<Stack> {
    let count = read_protocol_i32(data)?;
    if count <= 0 {
        return Some(Stack::default());
    }
    let count = u8::try_from(count).ok()?;
    let item = i64::from(read_protocol_i32(data)?);
    let added = usize::try_from(read_protocol_i32(data)?).ok()?;
    // Stack models component additions. A removed default cannot round-trip in it.
    if item <= 0 || read_protocol_i32(data)? != 0 {
        return None;
    }
    Some((item, count, read_merchant_components(data, added)?))
}

#[cfg_attr(not(test), allow(dead_code))]
fn read_merchant_components(data: &mut &[u8], count: usize) -> Option<Comps> {
    const ITEM_NAME: i64 = 6;
    if count > data.len() {
        return None;
    }
    let mut components = Vec::new();
    for _ in 0..count {
        let kind = i64::from(read_protocol_i32(data)?);
        let start = *data;
        // Component payloads are unframed. Stop if their codec is unavailable here.
        match kind {
            COMP_CUSTOM_DATA | COMP_CUSTOM_NAME | ITEM_NAME => read_merchant_nbt(data)?,
            MERCHANT_MAX_STACK_SIZE | COMP_MAX_DAMAGE | COMP_DAMAGE => {
                read_protocol_i32(data)?;
            }
            COMP_UNBREAKABLE => {
                read_merchant_bool(data)?;
            }
            COMP_LORE => {
                let lines = usize::try_from(read_protocol_i32(data)?).ok()?;
                if lines > 256 || lines > data.len() {
                    return None;
                }
                for _ in 0..lines {
                    read_merchant_nbt(data)?;
                }
            }
            COMP_ENCHANTMENTS | COMP_STORED_ENCHANTMENTS => {
                let entries = usize::try_from(read_protocol_i32(data)?).ok()?;
                if entries > data.len() / 2 {
                    return None;
                }
                for _ in 0..entries {
                    read_protocol_i32(data)?;
                    read_protocol_i32(data)?;
                }
                read_merchant_bool(data)?;
            }
            _ => return None,
        }
        components.push((kind, start[..start.len() - data.len()].to_vec()));
    }
    Some(components)
}

#[cfg_attr(not(test), allow(dead_code))]
fn read_merchant_nbt(data: &mut &[u8]) -> Option<()> {
    let mut len = 0;
    if !nbt_wire_skip(data, &mut len, 0) {
        return None;
    }
    *data = &data[len..];
    Some(())
}

#[cfg_attr(not(test), allow(dead_code))]
fn decode_select_trade(mut payload: &[u8]) -> Option<i32> {
    let index = read_protocol_i32(&mut payload)?;
    payload.is_empty().then_some(index)
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct MerchantMenu {
    selection_hint: i32,
    payments: [Stack; 2],
}

impl MerchantMenu {
    #[cfg_attr(not(test), allow(dead_code))]
    fn select_trade(
        &mut self,
        index: i32,
        offers: &[MerchantOffer],
        inventory: &mut HashMap<i16, Stack>,
        items: &MerchantItems<'_>,
    ) {
        self.selection_hint = index;
        let Some(offer) = offers.get(index as usize) else {
            return;
        };

        for payment in &mut self.payments {
            if !merchant_stack_empty(payment) && !return_merchant_payment(payment, inventory, items)
            {
                return;
            }
        }
        if self.payments.iter().all(merchant_stack_empty) {
            fill_merchant_payment(&mut self.payments[0], &offer.cost_a, inventory, items);
            if let Some(cost) = &offer.cost_b {
                fill_merchant_payment(&mut self.payments[1], cost, inventory, items);
            }
        }
    }

    // Resolve the result slot from current payments and stock.
    #[cfg_attr(not(test), allow(dead_code))]
    fn active_offer(&self, offers: &[MerchantOffer], items: &MerchantItems<'_>) -> Option<usize> {
        let [a, b] = &self.payments;
        [[a, b], [b, a]].into_iter().find_map(|[a, b]| {
            // Vanilla treats zero and out-of-range hints as a search in list order.
            let index = if self.selection_hint > 0 && (self.selection_hint as usize) < offers.len()
            {
                let index = self.selection_hint as usize;
                offers[index]
                    .payment_counts(a, b, items)
                    .is_ok()
                    .then_some(index)
            } else {
                offers
                    .iter()
                    .position(|offer| offer.payment_counts(a, b, items).is_ok())
            }?;
            (!offers[index].out_of_stock()).then_some(index)
        })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TradeRejection {
    Unavailable,
    StaleOffer,
    OutOfStock,
    InvalidOffer,
    WrongItemA,
    WrongItemB,
    InsufficientPaymentA,
    InsufficientPaymentB,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
struct TradeExecutionOutcome {
    result: Stack,
    consumed_a: u8,
    consumed_b: u8,
    villager_xp: i32,
    level_before: VillagerLevel,
    level_after: VillagerLevel,
}

#[cfg_attr(not(test), allow(dead_code))]
fn merchant_stack_empty(stack: &Stack) -> bool {
    stack.0 == 0 || stack.1 == 0
}

#[cfg_attr(not(test), allow(dead_code))]
fn shrink_merchant_stack(stack: &mut Stack, count: u8) {
    stack.1 -= count;
    if stack.1 == 0 {
        *stack = Stack::default();
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn move_merchant_stack(source: &mut Stack, target: &mut Stack, limit: u8) {
    if merchant_stack_empty(target) {
        *target = (source.0, 0, source.2.clone());
    }
    let count = source.1.min(limit.saturating_sub(target.1));
    target.1 += count;
    shrink_merchant_stack(source, count);
}

#[cfg_attr(not(test), allow(dead_code))]
const MERCHANT_INVENTORY_SLOTS: std::ops::RangeInclusive<i16> = 9..=44;

#[cfg_attr(not(test), allow(dead_code))]
fn return_merchant_payment(
    payment: &mut Stack,
    inventory: &mut HashMap<i16, Stack>,
    items: &MerchantItems<'_>,
) -> bool {
    let original_count = payment.1;
    let limit = items.max_stack_size(payment.0, &payment.2);
    // Quick-move merges in reverse menu order, then fills at most one empty slot.
    if items.stackable(payment) {
        for slot in MERCHANT_INVENTORY_SLOTS.rev() {
            if let Some(stack) = inventory.get_mut(&slot) {
                if !merchant_stack_empty(stack) && items.same_components(payment, stack) {
                    move_merchant_stack(payment, stack, limit);
                    if merchant_stack_empty(payment) {
                        return true;
                    }
                }
            }
        }
    }
    if let Some(slot) = MERCHANT_INVENTORY_SLOTS
        .rev()
        .find(|slot| inventory.get(slot).is_none_or(merchant_stack_empty))
    {
        move_merchant_stack(payment, inventory.entry(slot).or_default(), limit);
    }
    payment.1 < original_count
}

#[cfg_attr(not(test), allow(dead_code))]
fn fill_merchant_payment(
    payment: &mut Stack,
    cost: &ItemCost,
    inventory: &mut HashMap<i16, Stack>,
    items: &MerchantItems<'_>,
) {
    for slot in MERCHANT_INVENTORY_SLOTS {
        let Some(stack) = inventory.get_mut(&slot) else {
            continue;
        };
        if !cost.matches(stack, items)
            || (!merchant_stack_empty(payment) && !items.same_components(payment, stack))
        {
            continue;
        }
        // Autofill collects up to one full stack, even if the price is lower.
        let limit = items.max_stack_size(stack.0, &stack.2);
        // Vanilla copies the current source, including its patch, into the payment slot.
        let moved = stack.1.min(limit.saturating_sub(payment.1));
        *payment = (stack.0, payment.1 + moved, stack.2.clone());
        shrink_merchant_stack(stack, moved);
        if merchant_stack_empty(stack) {
            inventory.remove(&slot);
        }
        if payment.1 >= limit {
            break;
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GossipType {
    MajorNegative,
    MinorNegative,
    MinorPositive,
    MajorPositive,
    Trading,
}

impl GossipType {
    #[cfg_attr(not(test), allow(dead_code))]
    const ALL: [Self; 5] = [
        Self::MajorNegative,
        Self::MinorNegative,
        Self::MinorPositive,
        Self::MajorPositive,
        Self::Trading,
    ];

    // weight, maximum, daily decay, transfer decay (GossipType in 1.21.1).
    #[cfg_attr(not(test), allow(dead_code))]
    fn rules(self) -> (i32, i32, i32, i32) {
        match self {
            Self::MajorNegative => (-5, 100, 10, 10),
            Self::MinorNegative => (-1, 200, 20, 20),
            Self::MinorPositive => (1, 200, 1, 5),
            Self::MajorPositive => (5, 100, 0, 20),
            Self::Trading => (1, 25, 2, 20),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Default)]
struct VillagerGossip {
    by_player: HashMap<VillageUuid, [i32; GossipType::ALL.len()]>,
}

impl VillagerGossip {
    #[cfg_attr(not(test), allow(dead_code))]
    fn add(&mut self, player: VillageUuid, kind: GossipType, amount: i32) {
        let values = self.by_player.entry(player).or_default();
        let value = &mut values[kind as usize];
        *value = value.saturating_add(amount).clamp(0, kind.rules().1);
        if *value < 2 {
            *value = 0;
        }
        if values.iter().all(|&value| value == 0) {
            self.by_player.remove(&player);
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn cure(&mut self, player: VillageUuid) {
        self.add(player, GossipType::MajorPositive, 20);
        self.add(player, GossipType::MinorPositive, 25);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn reputation(&self, player: VillageUuid) -> i32 {
        self.by_player.get(&player).map_or(0, |values| {
            GossipType::ALL
                .iter()
                .zip(values)
                .map(|(&kind, &value)| kind.rules().0 * value)
                .sum()
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn golem_hostile(&self, player: VillageUuid) -> bool {
        self.reputation(player) <= -100
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn decay(&mut self, days: u64) {
        // Even the largest entry expires within 25 days if it decays at all.
        let days = days.min(25) as i32;
        self.by_player.retain(|_, values| {
            for (kind, value) in GossipType::ALL.into_iter().zip(values.iter_mut()) {
                *value -= kind.rules().2 * days;
                if *value < 2 {
                    *value = 0;
                }
            }
            values.iter().any(|&value| value != 0)
        });
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn hear_from(&mut self, other: &Self) {
        for (&player, values) in &other.by_player {
            for (kind, &value) in GossipType::ALL.into_iter().zip(values) {
                let shared = value - kind.rules().3;
                if shared >= 2 {
                    let current = &mut self.by_player.entry(player).or_default()[kind as usize];
                    *current = (*current).max(shared);
                }
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct Villager {
    id: VillageUuid,
    profession: VillagerProfession,
    villager_xp: i32,
    home: Option<BlockPos>,
    job_site: Option<BlockPos>,
    meeting_point: Option<BlockPos>,
    sleeping: bool,
    offers: Vec<MerchantOffer>,
    trading_player: Option<VillageUuid>,
    gossip: VillagerGossip,
    day: Option<u64>,
    restocks_today: u8,
    last_restock: Option<u64>,
    last_gossip: Option<u64>,
}

impl Villager {
    #[cfg_attr(not(test), allow(dead_code))]
    fn new(id: VillageUuid, profession: VillagerProfession) -> Self {
        Self {
            id,
            profession,
            ..Self::default()
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn claim_poi(&mut self, pois: &mut PoiRegistry, pos: BlockPos) -> bool {
        self.refresh_pois(pois);
        let Some(kind) = pois.records.get(&pos).map(|poi| poi.kind) else {
            return false;
        };
        let binding = match kind {
            PoiKind::Bed => &mut self.home,
            PoiKind::Bell => &mut self.meeting_point,
            PoiKind::JobSite(profession)
                if !matches!(
                    profession,
                    VillagerProfession::None | VillagerProfession::Nitwit
                ) && self.profession != VillagerProfession::Nitwit
                    && (self.profession == profession
                        || (self.villager_xp == 0 && self.profession == VillagerProfession::None)) =>
            {
                &mut self.job_site
            }
            PoiKind::JobSite(_) => return false,
        };
        if binding.is_some_and(|bound| bound != pos) || !pois.claim(pos, self.id) {
            return false;
        }
        *binding = Some(pos);
        if let PoiKind::JobSite(profession) = kind {
            self.profession = profession;
        }
        true
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn refresh_pois(&mut self, pois: &PoiRegistry) {
        if self
            .home
            .is_some_and(|pos| !pois.has_claim(pos, PoiKind::Bed, self.id))
        {
            self.home = None;
            self.sleeping = false;
        }
        if self
            .meeting_point
            .is_some_and(|pos| !pois.has_claim(pos, PoiKind::Bell, self.id))
        {
            self.meeting_point = None;
        }
        if self
            .job_site
            .is_some_and(|pos| !pois.has_claim(pos, PoiKind::JobSite(self.profession), self.id))
        {
            self.job_site = None;
            if self.villager_xp == 0 {
                self.profession = VillagerProfession::None;
                self.offers.clear();
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn release_pois(&mut self, pois: &mut PoiRegistry) {
        for pos in [self.home, self.job_site, self.meeting_point]
            .into_iter()
            .flatten()
        {
            pois.release(pos, self.id);
        }
        self.refresh_pois(pois);
    }

    /// Day-boundary bookkeeping shared by the full tick and the LOD sleep
    /// path: gossip decays and the twice-daily restock budget resets.
    #[cfg_attr(not(test), allow(dead_code))]
    fn roll_day(&mut self, world_time: u64) {
        let day = world_time / VILLAGER_DAY_TICKS;
        if self.day.is_none_or(|previous| day > previous) {
            if let Some(previous) = self.day {
                self.gossip.decay(day - previous);
            }
            self.day = Some(day);
            self.restocks_today = 0;
        }
    }

    /// Vanilla work-station restock: the workstation occupies its block, so the
    /// villager works from an adjacent block — next to the job site, under two
    /// restocks per day, `VILLAGER_RESTOCK_COOLDOWN` apart, and only when an
    /// offer has actually been used. Returns whether a restock happened.
    #[cfg_attr(not(test), allow(dead_code))]
    fn restock_at_job_site(&mut self, position: BlockPos, world_time: u64) -> bool {
        if self
            .job_site
            .is_some_and(|site| villager_within(position, site, 1))
            && self.restocks_today < 2
            && self.last_restock.is_none_or(|last| {
                world_time.saturating_sub(last) >= VILLAGER_RESTOCK_COOLDOWN
            })
            && self.offers.iter().any(|offer| offer.uses > 0)
        {
            for offer in &mut self.offers {
                offer.update_demand();
                offer.uses = 0;
            }
            self.last_restock = Some(world_time);
            self.restocks_today += 1;
            return true;
        }
        false
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn tick(
        &mut self,
        pois: &PoiRegistry,
        position: BlockPos,
        world_time: u64,
    ) -> (VillagerActivity, Option<BlockPos>) {
        self.tick_brain(pois, position, world_time, false, false)
    }

    fn tick_brain(
        &mut self,
        pois: &PoiRegistry,
        position: BlockPos,
        world_time: u64,
        baby: bool,
        panicking: bool,
    ) -> (VillagerActivity, Option<BlockPos>) {
        self.refresh_pois(pois);
        self.roll_day(world_time);

        let activity = VillagerActivity::brain_at(world_time, baby, panicking);
        let target = match activity {
            VillagerActivity::Idle | VillagerActivity::Play => None,
            VillagerActivity::Meet => self.meeting_point,
            VillagerActivity::Rest | VillagerActivity::Panic => self.home,
            VillagerActivity::Work => {
                self.restock_at_job_site(position, world_time);
                self.job_site
            }
        };
        self.sleeping = activity == VillagerActivity::Rest && target == Some(position);
        (activity, target)
    }

    /// The [`VillagerAiLodState::TradingHallSleep`] tick: the same day
    /// bookkeeping, POI-claim validation and work-window restock rules as
    /// [`Villager::tick`] — but no movement target and no pathfinding, which
    /// are the work a confined villager can never use anyway. Returns whether
    /// a restock happened.
    #[cfg_attr(not(test), allow(dead_code))]
    fn lod_sleep_tick(
        &mut self,
        pois: &PoiRegistry,
        position: BlockPos,
        world_time: u64,
    ) -> bool {
        self.refresh_pois(pois);
        self.roll_day(world_time);
        VillagerActivity::at(world_time) == VillagerActivity::Work
            && self.restock_at_job_site(position, world_time)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn gossip_at_bell(
        &mut self,
        other: &mut Self,
        pois: &PoiRegistry,
        positions: [BlockPos; 2],
        world_time: u64,
    ) -> bool {
        let Some(bell) = self.meeting_point else {
            return false;
        };
        if VillagerActivity::at(world_time) != VillagerActivity::Meet
            || other.meeting_point != Some(bell)
            || !pois.has_claim(bell, PoiKind::Bell, self.id)
            || !pois.has_claim(bell, PoiKind::Bell, other.id)
            || !positions
                .into_iter()
                .all(|pos| villager_within(pos, bell, 6))
            || [self.last_gossip, other.last_gossip]
                .into_iter()
                .flatten()
                .any(|last| world_time.saturating_sub(last) < VILLAGER_GOSSIP_COOLDOWN)
        {
            return false;
        }
        self.gossip.hear_from(&other.gossip);
        other.gossip.hear_from(&self.gossip);
        self.last_gossip = Some(world_time);
        other.last_gossip = Some(world_time);
        true
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn level(&self) -> VillagerLevel {
        VillagerLevel::from_xp(self.villager_xp)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn start_trading(
        &mut self,
        pois: &PoiRegistry,
        player: VillageUuid,
        hero_amplifier: Option<u8>,
    ) -> bool {
        self.refresh_pois(pois);
        if self.trading_player.is_some()
            || self.offers.is_empty()
            || matches!(
                self.profession,
                VillagerProfession::None | VillagerProfession::Nitwit
            )
        {
            return false;
        }
        let hero_amplifier = hero_amplifier.or_else(|| {
            let hero = live_effect_id("hero_of_the_village")?;
            client_snapshot()
                .into_iter()
                .find(|client| client.uuid == player)
                .and_then(|client| {
                    client
                        .effs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .find(|eff| eff.id == hero)
                        .map(|eff| eff.amp as u8)
                })
        });
        let reputation = self.gossip.reputation(player);
        for offer in &mut self.offers {
            offer.apply_customer_price(reputation, hero_amplifier);
        }
        self.trading_player = Some(player);
        true
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn stop_trading(&mut self) {
        self.trading_player = None;
        for offer in &mut self.offers {
            offer.special_price_diff = 0;
        }
    }

    // Opening prices stay fixed for the customer until the menu closes.
    #[cfg_attr(not(test), allow(dead_code))]
    fn trade(
        &mut self,
        player: VillageUuid,
        menu: &mut MerchantMenu,
        items: &MerchantItems<'_>,
    ) -> Option<TradeExecutionOutcome> {
        let index = menu.active_offer(&self.offers, items)?;
        self.execute_trade_transaction(index, None, player, &mut menu.payments, items)
            .ok()
    }

    // The caller has resolved the offer and checked room for its result. Pass the
    // displayed offer as `expected` when completing against a prior snapshot.
    #[cfg_attr(not(test), allow(dead_code))]
    fn execute_trade_transaction(
        &mut self,
        index: usize,
        expected: Option<&MerchantOffer>,
        player: VillageUuid,
        payments: &mut [Stack; 2],
        items: &MerchantItems<'_>,
    ) -> Result<TradeExecutionOutcome, TradeRejection> {
        if self.trading_player != Some(player) {
            return Err(TradeRejection::Unavailable);
        }
        let level_before = self.level();
        let offer = self
            .offers
            .get_mut(index)
            .ok_or(TradeRejection::Unavailable)?;
        if expected.is_some_and(|expected| expected != offer) {
            return Err(TradeRejection::StaleOffer);
        }
        let [consumed_a, consumed_b] = offer.take(payments, items)?;
        let result = offer.result.clone();
        self.villager_xp = self.villager_xp.wrapping_add(offer.xp_reward);
        self.gossip.add(player, GossipType::Trading, 2);
        let level_after = self.level();
        if level_after != level_before {
            for unlock in (level_before as u8 + 1)..=(level_after as u8) {
                let Some(level) = [None, Some(VillagerLevel::Novice), Some(VillagerLevel::Apprentice), Some(VillagerLevel::Journeyman), Some(VillagerLevel::Expert), Some(VillagerLevel::Master)][usize::from(unlock)] else { continue };
                unlock_villager_trade_level(self, level);
            }
        }
        Ok(TradeExecutionOutcome {
            result,
            consumed_a,
            consumed_b,
            villager_xp: self.villager_xp,
            level_before,
            level_after,
        })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn villager_within(a: BlockPos, b: BlockPos, radius: u16) -> bool {
    let distances = [a.0.abs_diff(b.0), a.1.abs_diff(b.1), a.2.abs_diff(b.2)];
    distances.iter().all(|&delta| delta <= u32::from(radius))
        && distances
            .map(|delta| u64::from(delta).pow(2))
            .iter()
            .sum::<u64>()
            <= u64::from(radius).pow(2)
}

const VILLAGER_BABY_GROWTH_TICKS: i32 = 24_000;
const VILLAGER_WILLING_FOOD_POINTS: i32 = 12;
#[cfg_attr(not(test), allow(dead_code))]
const ZOMBIE_VILLAGER_CURE_MIN_TICKS: u16 = 3_600;
#[cfg_attr(not(test), allow(dead_code))]
const ZOMBIE_VILLAGER_CURE_RANDOM_TICKS: u16 = 2_400;
const ZOMBIE_VILLAGER_CURE_ACCELERANT_RADIUS: i32 = 4;
const ZOMBIE_VILLAGER_CURE_MAX_ACCELERANTS: u16 = 14;
const WANDERING_TRADER_DESPAWN_TICKS: u32 = 48_000;

/// Bread is worth four food points; carrots, potatoes and beetroot one each.
fn villager_food_points(bread: i32, carrots: i32, potatoes: i32, beetroots: i32) -> i32 {
    bread.max(0) * 4 + carrots.max(0) + potatoes.max(0) + beetroots.max(0)
}

fn villager_willing(food_points: i32) -> bool {
    food_points >= VILLAGER_WILLING_FOOD_POINTS
}

/// Villagers with surplus food throw enough to a neighbour to reach willingness.
/// Returns `(donor_points, recipient_points, shared)`.
fn villager_share_food(donor_points: i32, recipient_points: i32) -> (i32, i32, bool) {
    if donor_points <= VILLAGER_WILLING_FOOD_POINTS || recipient_points >= VILLAGER_WILLING_FOOD_POINTS {
        return (donor_points, recipient_points, false);
    }
    let shared = ((donor_points - VILLAGER_WILLING_FOOD_POINTS) / 2)
        .max(1)
        .min(VILLAGER_WILLING_FOOD_POINTS - recipient_points);
    (donor_points - shared, recipient_points + shared, shared > 0)
}

/// Vanilla breeding requires a bed in the village that no villager has claimed.
fn villager_unclaimed_bed_near(pois: &PoiRegistry, center: BlockPos, radius: u16) -> Option<BlockPos> {
    pois.records
        .iter()
        .filter(|(&pos, poi)| {
            poi.kind == PoiKind::Bed && poi.claimants.is_empty() && villager_within(pos, center, radius)
        })
        .map(|(&pos, _)| pos)
        .min_by_key(|&(x, y, z)| {
            let [dx, dy, dz] = [x - center.0, y - center.1, z - center.2];
            dx * dx + dy * dy + dz * dz
        })
}

fn villager_can_breed(
    pois: &PoiRegistry,
    center: BlockPos,
    radius: u16,
    willing_adults: usize,
) -> bool {
    willing_adults >= 2 && villager_unclaimed_bed_near(pois, center, radius).is_some()
}

/// Babies count up for exactly one Minecraft day. Negative means still a baby.
fn villager_baby_age_tick(age_ticks: i32) -> (i32, bool) {
    let next = (age_ticks + 1).min(0);
    (next, next < 0)
}

/// Difficulty's zombie conversion rate when a villager dies to a zombie.
#[cfg_attr(not(test), allow(dead_code))]
fn zombie_villager_conversion_percent(difficulty: RaidDifficulty) -> u8 {
    match difficulty {
        RaidDifficulty::Peaceful | RaidDifficulty::Easy => 0,
        RaidDifficulty::Normal => 50,
        RaidDifficulty::Hard => 100,
    }
}

#[derive(Clone, Debug, PartialEq)]
struct ZombieVillagerCure {
    curer: VillageUuid,
    profession: VillagerProfession,
    villager_xp: i32,
    offers: Vec<MerchantOffer>,
    ticks_left: u16,
    accelerant_scan_progress: u16,
}

impl ZombieVillagerCure {
    #[cfg_attr(not(test), allow(dead_code))]
    fn start(
        curer: VillageUuid,
        profession: VillagerProfession,
        villager_xp: i32,
        offers: Vec<MerchantOffer>,
        random_roll: u16,
    ) -> Self {
        Self {
            curer,
            profession,
            villager_xp,
            offers,
            ticks_left: ZOMBIE_VILLAGER_CURE_MIN_TICKS + random_roll % (ZOMBIE_VILLAGER_CURE_RANDOM_TICKS + 1),
            accelerant_scan_progress: 1,
        }
    }

    /// One server tick. `accelerants` is the count of beds and iron bars in the
    /// 9x9x9 cube (radius four), capped by vanilla at fourteen. Each tick has a
    /// 1% scan chance; each scanned block has a 30% chance to add one tick of
    /// progress. The deterministic test hook supplies the two percentile rolls.
    fn tick_with_rolls(&mut self, accelerants: u16, scan_roll: u32, block_roll: u32) -> bool {
        let mut progress = self.accelerant_scan_progress.max(1);
        if scan_roll.is_multiple_of(100) {
            let active = accelerants.min(ZOMBIE_VILLAGER_CURE_MAX_ACCELERANTS);
            if active > 0 && block_roll % 100 < 30 {
                progress = progress.saturating_add(active);
            }
        }
        self.accelerant_scan_progress = 1;
        self.ticks_left = self.ticks_left.saturating_sub(progress);
        self.ticks_left == 0
    }
    fn finish(self) -> (VillageUuid, VillagerProfession, i32, Vec<MerchantOffer>) {
        (self.curer, self.profession, self.villager_xp, self.offers)
    }
}

fn zombie_cure_accelerants_in_cube(
    center: BlockPos,
    mut is_accelerant: impl FnMut(BlockPos) -> bool,
) -> u16 {
    let radius = ZOMBIE_VILLAGER_CURE_ACCELERANT_RADIUS;
    let mut count = 0;
    for x in center.0 - radius..=center.0 + radius {
        for y in center.1 - radius..=center.1 + radius {
            for z in center.2 - radius..=center.2 + radius {
                if is_accelerant((x, y, z)) {
                    count += 1;
                }
            }
        }
    }
    count.min(ZOMBIE_VILLAGER_CURE_MAX_ACCELERANTS)
}

/// Wandering traders use the 48000 tick despawn delay; zero means despawn now.
fn wandering_trader_despawn_tick(delay: u32) -> (u32, bool) {
    let next = delay.saturating_sub(1);
    (next, next == 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RavagerRider {
    ravager_slot: usize,
    rider: RaiderType,
}

fn raid_potential_bonus_spawns(
    kind: RaiderType,
    difficulty: RaidDifficulty,
    wave: u8,
    bonus_wave: bool,
    bound_roll: u32,
    count_roll: u32,
) -> u8 {
    let easy = difficulty == RaidDifficulty::Easy;
    let bound = match kind {
        RaiderType::Vindicator | RaiderType::Pillager => match difficulty {
            RaidDifficulty::Easy => bound_roll % 2,
            RaidDifficulty::Normal => 1,
            RaidDifficulty::Hard => 2,
            RaidDifficulty::Peaceful => 0,
        },
        RaiderType::Witch => {
            if easy || wave <= 2 || wave == 4 { 0 } else { 1 }
        }
        RaiderType::Ravager => u32::from(!easy && bonus_wave),
        RaiderType::Evoker => return 0,
    };
    if bound == 0 {
        0
    } else {
        (count_roll % (bound + 1)) as u8
    }
}

/// Vanilla's bonus-roll shape and ravager passengers. Riders are additional
/// raid members: every wave-five ravager carries a pillager, and in wave seven
/// or later the first ravager carries an evoker and each other ravager a
/// vindicator.
fn raid_wave_adjustments(
    difficulty: RaidDifficulty,
    wave: u8,
    bonus_wave: bool,
    counts: &mut [u8; RaiderType::ALL.len()],
    roll_a: u32,
    roll_b: u32,
) -> Vec<RavagerRider> {
    for kind in [RaiderType::Vindicator, RaiderType::Pillager] {
        counts[kind as usize] += raid_potential_bonus_spawns(
            kind, difficulty, wave, bonus_wave, roll_a.wrapping_add(kind as u32), roll_b.wrapping_add(kind as u32),
        );
    }
    counts[RaiderType::Witch as usize] += raid_potential_bonus_spawns(
        RaiderType::Witch, difficulty, wave, bonus_wave, roll_a.wrapping_add(3), roll_b.wrapping_add(3),
    );
    counts[RaiderType::Ravager as usize] += raid_potential_bonus_spawns(
        RaiderType::Ravager, difficulty, wave, bonus_wave, roll_a.wrapping_add(4), roll_b.wrapping_add(4),
    );
    let mut riders = Vec::new();
    for ravager_slot in 0..usize::from(counts[RaiderType::Ravager as usize]) {
        if wave == 5 {
            riders.push(RavagerRider { ravager_slot, rider: RaiderType::Pillager });
        } else if wave >= 7 {
            riders.push(RavagerRider {
                ravager_slot,
                rider: if ravager_slot == 0 { RaiderType::Evoker } else { RaiderType::Vindicator },
            });
        }
    }
    riders
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RaidDifficulty {
    Peaceful,
    Easy,
    Normal,
    Hard,
}

impl RaidDifficulty {
    #[cfg_attr(not(test), allow(dead_code))]
    fn regular_waves(self) -> u8 {
        match self {
            Self::Peaceful => 0,
            Self::Easy => 3,
            Self::Normal => 5,
            Self::Hard => 7,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RaidState {
    Celebrating,
    InProgress,
    Victory,
    Loss,
    Stopped,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RaiderType {
    Vindicator,
    Evoker,
    Pillager,
    Witch,
    Ravager,
}

impl RaiderType {
    #[cfg_attr(not(test), allow(dead_code))]
    const ALL: [Self; 5] = [
        Self::Vindicator,
        Self::Evoker,
        Self::Pillager,
        Self::Witch,
        Self::Ravager,
    ];
}

// Raid.RaiderType.spawnsPerWaveBeforeBonus, transposed into wave order.
// The eighth wave uses row seven, as do the Easy and Normal bonus waves
// with their respective final regular rows.
#[cfg_attr(not(test), allow(dead_code))]
const RAID_BASE_WAVES: [[u8; RaiderType::ALL.len()]; 7] = [
    [0, 0, 4, 0, 0],
    [2, 0, 3, 0, 0],
    [0, 0, 3, 0, 1],
    [1, 0, 4, 3, 0],
    [4, 1, 4, 0, 1],
    [2, 1, 4, 0, 0],
    [5, 2, 2, 1, 2],
];

#[cfg_attr(not(test), allow(dead_code))]
const RAID_WAVE_COUNTDOWN: u16 = 300;
#[cfg_attr(not(test), allow(dead_code))]
const RAID_OMEN_TICKS: u16 = 600;
#[cfg_attr(not(test), allow(dead_code))]
const RAID_CELEBRATION_TICKS: u16 = 600;
#[cfg_attr(not(test), allow(dead_code))]
const RAID_TIMEOUT_TICKS: u32 = 48_000;
#[cfg_attr(not(test), allow(dead_code))]
const BAD_OMEN_TICKS: u32 = 120_000;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RaidOmen {
    bad_omen_level: u8,
    countdown: u16,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BadOmen {
    level: u8,
    duration_ticks: u32,
}

impl BadOmen {
    #[cfg_attr(not(test), allow(dead_code))]
    fn from_ominous_bottle(amplifier: u8) -> Self {
        Self {
            level: amplifier.saturating_add(1).clamp(1, 5),
            duration_ticks: BAD_OMEN_TICKS,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn enter_village(self) -> RaidOmen {
        RaidOmen {
            bad_omen_level: self.level.clamp(1, 5),
            countdown: RAID_OMEN_TICKS,
        }
    }
}

impl RaidOmen {
    #[cfg_attr(not(test), allow(dead_code))]
    fn tick(&mut self) -> bool {
        self.countdown = self.countdown.saturating_sub(1);
        self.countdown == 0
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeroOfTheVillage {
    level: u8,
    duration_ticks: u32,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct RaidRaiderId {
    wave: u8,
    slot: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RaidWave {
    number: u8,
    counts: [u8; RaiderType::ALL.len()],
    captain: Option<RaidRaiderId>,
}

impl RaidWave {
    #[cfg_attr(not(test), allow(dead_code))]
    fn raiders(self) -> impl Iterator<Item = (RaidRaiderId, RaiderType)> {
        RaiderType::ALL
            .into_iter()
            .zip(self.counts)
            .flat_map(|(kind, count)| (0..count).map(move |_| kind))
            .enumerate()
            .map(move |(slot, kind)| {
                (
                    RaidRaiderId {
                        wave: self.number,
                        slot,
                    },
                    kind,
                )
            })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum RaidEvent {
    SpawnWave(RaidWave),
    Victory {
        participants: Vec<VillageUuid>,
        effect: HeroOfTheVillage,
    },
    Loss,
}

#[cfg_attr(not(test), allow(dead_code))]
struct Raid {
    center: BlockPos,
    origin: BlockPos,
    state: RaidState,
    regular_waves: u8,
    total_waves: u8,
    waves_spawned: u8,
    countdown: u16,
    ticks_elapsed: u32,
    remaining: HashSet<RaidRaiderId>,
    participants: std::collections::BTreeSet<VillageUuid>,
    omen_level: u8,
}

impl Raid {
    #[cfg_attr(not(test), allow(dead_code))]
    fn try_start(
        pois: &PoiRegistry,
        position: BlockPos,
        player: VillageUuid,
        omen: &mut Option<RaidOmen>,
        difficulty: RaidDifficulty,
    ) -> Option<Self> {
        let omen_level = omen.as_ref()?.bad_omen_level;
        if omen.as_ref()?.countdown > 0 {
            return None;
        }
        let regular_waves = difficulty.regular_waves();
        if omen_level == 0 || regular_waves == 0 {
            return None;
        }
        let (mut sum, mut count) = ([0_i64; 3], 0_i64);
        for ((x, y, z), _) in pois.claimed_near(position) {
            sum[0] += i64::from(x);
            sum[1] += i64::from(y);
            sum[2] += i64::from(z);
            count += 1;
        }
        if count == 0 {
            return None;
        }
        let [x, y, z] = sum.map(|axis| axis.div_euclid(count) as i32);
        *omen = None;
        Some(Self {
            center: (x, y, z),
            origin: position,
            state: RaidState::InProgress,
            regular_waves,
            total_waves: regular_waves + u8::from(omen_level > 1),
            waves_spawned: 0,
            countdown: RAID_WAVE_COUNTDOWN,
            ticks_elapsed: 0,
            remaining: HashSet::new(),
            participants: [player].into(),
            omen_level: omen_level.min(5),
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn record_defeat(&mut self, raider: RaidRaiderId, killer: Option<VillageUuid>) -> bool {
        if self.state != RaidState::InProgress || !self.remaining.remove(&raider) {
            return false;
        }
        self.participants.extend(killer);
        true
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn stop(&mut self) {
        self.state = RaidState::Stopped;
        self.remaining.clear();
    }

    // Call once per game tick. The caller counts surviving villagers in this village
    // and spawns or rewards entities in response to the returned event.
    #[cfg_attr(not(test), allow(dead_code))]
    fn tick(&mut self, pois: &PoiRegistry, living_villagers: usize) -> Option<RaidEvent> {
        match self.state {
            RaidState::Stopped => return None,
            RaidState::Victory | RaidState::Loss | RaidState::Celebrating => {
                self.state = RaidState::Celebrating;
                self.countdown -= 1;
                if self.countdown == 0 {
                    self.stop();
                }
                return None;
            }
            RaidState::InProgress => {}
        }

        self.ticks_elapsed = self.ticks_elapsed.saturating_add(1);

        if living_villagers == 0
            || self.ticks_elapsed >= RAID_TIMEOUT_TICKS
            || !pois
                .claimed_near(self.origin)
                .any(|(_, kind)| kind == PoiKind::Bed)
        {
            self.state = RaidState::Loss;
            self.countdown = RAID_CELEBRATION_TICKS;
            self.remaining.clear();
            return Some(RaidEvent::Loss);
        }
        if !self.remaining.is_empty() {
            return None;
        }
        if self.waves_spawned == self.total_waves {
            self.state = RaidState::Victory;
            self.countdown = RAID_CELEBRATION_TICKS;
            return Some(RaidEvent::Victory {
                participants: self.participants.iter().copied().collect(),
                effect: HeroOfTheVillage {
                    level: self.omen_level,
                    duration_ticks: 48_000,
                },
            });
        }

        self.countdown -= 1;
        if self.countdown > 0 {
            return None;
        }
        self.waves_spawned += 1;
        self.countdown = RAID_WAVE_COUNTDOWN;
        let row = usize::from(self.waves_spawned.min(self.regular_waves) - 1);
        let counts = RAID_BASE_WAVES[row];
        let captain = RaiderType::ALL
            .into_iter()
            .take(3)
            .find(|kind| counts[*kind as usize] > 0)
            .map(|kind| {
                let slot = RaiderType::ALL
                    .into_iter()
                    .take_while(|candidate| *candidate != kind)
                    .map(|candidate| usize::from(counts[candidate as usize]))
                    .sum();
                RaidRaiderId {
                    wave: self.waves_spawned,
                    slot,
                }
            });
        let wave = RaidWave {
            number: self.waves_spawned,
            counts,
            captain,
        };
        self.remaining.extend(wave.raiders().map(|(id, _)| id));
        Some(RaidEvent::SpawnWave(wave))
    }
}

// ---------- native golem and utility-mob states ----------

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConstructedGolem {
    Iron,
    Snow,
}

/// Match the pumpkin above the iron T (in either horizontal orientation) or
/// two vertically stacked snow blocks. Returns precisely the consumed cells.
#[cfg_attr(not(test), allow(dead_code))]
fn detect_golem_construction<'a>(
    pumpkin: BlockPos,
    mut block_name: impl FnMut(BlockPos) -> &'a str,
) -> Option<(ConstructedGolem, Vec<BlockPos>)> {
    let (x, y, z) = pumpkin;
    if !matches!(block_name(pumpkin), "minecraft:carved_pumpkin" | "minecraft:jack_o_lantern") {
        return None;
    }
    let base = (x, y - 2, z);
    let neck = (x, y - 1, z);
    if block_name(base) == "minecraft:snow_block" && block_name(neck) == "minecraft:snow_block" {
        return Some((ConstructedGolem::Snow, vec![pumpkin, neck, base]));
    }
    if block_name(base) != "minecraft:iron_block" || block_name(neck) != "minecraft:iron_block" {
        return None;
    }
    for (dx, dz) in [(1, 0), (0, 1)] {
        let left = (x - dx, y - 1, z - dz);
        let right = (x + dx, y - 1, z + dz);
        if block_name(left) == "minecraft:iron_block" && block_name(right) == "minecraft:iron_block" {
            return Some((ConstructedGolem::Iron, vec![pumpkin, neck, base, left, right]));
        }
    }
    None
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default)]
struct IronGolemState {
    player_created: bool,
}

impl IronGolemState {
    #[cfg_attr(not(test), allow(dead_code))]
    fn hostile_to_player(self, gossip: &VillagerGossip, player: VillageUuid) -> bool {
        !self.player_created && gossip.golem_hostile(player)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn repair(hp: &mut f32, iron_ingot: bool) -> bool {
        if !iron_ingot || *hp >= 100.0 {
            return false;
        }
        *hp = (*hp + 25.0).min(100.0);
        true
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn crack_stage(hp: f32) -> u8 {
        if hp <= 25.0 { 3 } else if hp <= 50.0 { 2 } else if hp <= 75.0 { 1 } else { 0 }
    }
}

/// Nearby panic and a recent exchange among at least three villagers permit
/// another village-created iron golem when no existing one is within 16 blocks.
#[cfg_attr(not(test), allow(dead_code))]
fn village_golem_spawn_allowed(
    gossiping_villagers: usize,
    panicking: bool,
    golem_within_16: bool,
) -> bool {
    gossiping_villagers >= 3 && panicking && !golem_within_16
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default)]
struct SnowGolemState {
    sheared: bool,
}

impl SnowGolemState {
    #[cfg_attr(not(test), allow(dead_code))]
    fn shear(&mut self) -> bool {
        if self.sheared { false } else { self.sheared = true; true }
    }

    /// Called once every 20 game ticks. Snow may form beneath the mob only
    /// where the cold-biome and block-survival checks both succeed.
    #[cfg_attr(not(test), allow(dead_code))]
    fn environment_tick(hp: &mut f32, too_hot: bool, rain_exposed: bool, cold_and_supported: bool) -> bool {
        if too_hot || rain_exposed {
            *hp = (*hp - 1.0).max(0.0);
        }
        cold_and_supported && *hp > 0.0
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
struct AxolotlState {
    dry_ticks: u32,
    playing_dead_ticks: u32,
    previous_hp: f32,
}

impl AxolotlState {
    const DRY_TICKS: u32 = 6_000;

    #[cfg_attr(not(test), allow(dead_code))]
    fn new(hp: f32) -> Self {
        Self { dry_ticks: Self::DRY_TICKS, playing_dead_ticks: 0, previous_hp: hp }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn tick(&mut self, hydrated: bool, hp: &mut f32, max_hp: f32, tick: u64) {
        if hydrated {
            self.dry_ticks = Self::DRY_TICKS;
        } else {
            self.dry_ticks = self.dry_ticks.saturating_sub(1);
            if self.dry_ticks == 0 && tick.is_multiple_of(20) {
                *hp = (*hp - 1.0).max(0.0);
            }
        }
        if *hp < self.previous_hp && *hp < max_hp / 2.0 {
            self.playing_dead_ticks = 200;
        }
        if self.playing_dead_ticks > 0 {
            self.playing_dead_ticks -= 1;
            if tick.is_multiple_of(25) {
                *hp = (*hp + 1.0).min(max_hp);
            }
        }
        self.previous_hp = *hp;
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn reward_player_for_target_kill(player_killed_target: bool) -> Option<(i32, i32)> {
        player_killed_target.then_some((0, 100)) // Regeneration I, 5 seconds
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default)]
struct AllayState {
    owner: Option<VillageUuid>,
    held: Option<Stack>,
    collected: Option<Stack>,
    note_block: Option<(BlockPos, u64)>,
    duplication_ready_at: u64,
}

impl AllayState {
    #[cfg_attr(not(test), allow(dead_code))]
    fn give_item(&mut self, owner: VillageUuid, stack: Stack) -> bool {
        if stack.1 == 0 || self.held.is_some() {
            return false;
        }
        self.owner = Some(owner);
        self.held = Some((stack.0, 1, stack.2));
        true
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn hears_note(&mut self, position: BlockPos, now: u64) {
        self.note_block = Some((position, now.saturating_add(600)));
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn delivery_target(&mut self, now: u64) -> Option<BlockPos> {
        if self.note_block.is_some_and(|(_, expires)| now >= expires) {
            self.note_block = None;
        }
        self.note_block.map(|(pos, _)| pos)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn matches_drop(&self, drop: &Drop, allay_pos: [f64; 3]) -> bool {
        self.collected.is_none() && self.held.as_ref().is_some_and(|held| {
            held.0 == drop.item && held.2 == drop.comps && drop.cnt > 0
                && drop.target.is_none()
                && (drop.x - allay_pos[0]).powi(2)
                    + (drop.y - allay_pos[1]).powi(2)
                    + (drop.z - allay_pos[2]).powi(2) <= 32.0_f64.powi(2)
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn collect_drop(&mut self, drop: &Drop, at: [f64; 3]) -> bool {
        if !drop.pickup_delay_elapsed() || !self.matches_drop(drop, at) {
            return false;
        }
        self.collected = Some((drop.item, drop.cnt, drop.comps.clone()));
        true
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn duplicate(&mut self, dancing: bool, jukebox_active: bool, amethyst_shard: bool, now: u64) -> bool {
        if !dancing || !jukebox_active || !amethyst_shard || now < self.duplication_ready_at {
            return false;
        }
        self.duplication_ready_at = now.saturating_add(6_000);
        true
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default)]
struct FoxState {
    trusted: [Option<VillageUuid>; 2],
    sleeping: bool,
    held: Option<Stack>,
}

impl FoxState {
    #[cfg_attr(not(test), allow(dead_code))]
    fn trust(&mut self, player: VillageUuid) {
        if !self.trusted.contains(&Some(player)) {
            if let Some(slot) = self.trusted.iter_mut().find(|entry| entry.is_none()) {
                *slot = Some(player);
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn tick(&mut self, daytime: bool, threatened: bool, in_water: bool) {
        self.sleeping = daytime && !threatened && !in_water;
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn pickup(&mut self, stack: Stack) -> bool {
        if stack.1 == 0 || self.held.is_some() { return false; }
        self.held = Some((stack.0, 1, stack.2));
        true
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default)]
struct CamelState {
    sitting: bool,
    last_dash_tick: Option<u64>,
}

impl CamelState {
    #[cfg_attr(not(test), allow(dead_code))]
    const PASSENGERS: usize = 2;
    const DASH_COOLDOWN: u64 = 55;

    #[cfg_attr(not(test), allow(dead_code))]
    fn toggle_sitting(&mut self) { self.sitting = !self.sitting; }

    #[cfg_attr(not(test), allow(dead_code))]
    fn dash(&mut self, now: u64, yaw_degrees: f32) -> Option<[f64; 3]> {
        if self.sitting || self.last_dash_tick.is_some_and(|last| now.saturating_sub(last) < Self::DASH_COOLDOWN) {
            return None;
        }
        self.last_dash_tick = Some(now);
        let yaw = f64::from(yaw_degrees).to_radians();
        Some([-yaw.sin() * 1.8, 0.6, yaw.cos() * 1.8])
    }
}

/// Live per-entity data for the utility mobs. The server's shared `Mob`
/// allocation cannot be extended by this lane.
#[allow(dead_code)] // Interaction entry points are consumed by packet handlers owned by other lanes.
#[derive(Clone, Debug)]
enum NativeUtilityState {
    Iron(IronGolemState),
    Snow(SnowGolemState),
    Axolotl(AxolotlState),
    Allay(AllayState),
    Fox(FoxState),
    Camel(CamelState),
    Equine(EquineState),
    Parrot(ParrotState),
}

/// 1.21.1 entity ids for the `AbstractHorse` family — skeleton and zombie
/// horses share the horse kind's food/temper table in vanilla.
#[cfg_attr(not(test), allow(dead_code))]
fn equine_kind_of(etype: i64) -> Option<EquineKind> {
    Some(match etype {
        53 | 92 | 125 => EquineKind::Horse,
        25 => EquineKind::Donkey,
        71 => EquineKind::Mule,
        65 | 108 => EquineKind::Llama,
        _ => return None,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn live_native_utility_states() -> &'static Mutex<HashMap<i32, NativeUtilityState>> {
    static STATES: OnceLock<Mutex<HashMap<i32, NativeUtilityState>>> = OnceLock::new();
    STATES.get_or_init(Mutex::default)
}

#[allow(dead_code)]
fn live_native_mob(eid: i32, name: &str) -> Option<Arc<Mob>> {
    let etype = name_to_etype(name)?;
    mobs().lock().unwrap_or_else(|e| e.into_inner())
        .iter().find(|mob| mob.eid == eid && mob.etype == etype)
        .cloned()
}

/// Item-interaction entry points maintain the same per-entity state as the
/// periodic tick. Their callers are the corresponding packet/block handlers.
#[allow(dead_code)]
fn live_iron_golem_repair(eid: i32, iron_ingot: bool) -> bool {
    let Some(mob) = live_native_mob(eid, "iron_golem") else { return false };
    let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
    let state = states.entry(eid)
        .or_insert(NativeUtilityState::Iron(IronGolemState::default()));
    if let NativeUtilityState::Iron(_) = state {
        IronGolemState::repair(&mut mob.hp.lock().unwrap_or_else(|e| e.into_inner()), iron_ingot)
    } else {
        false
    }
}

#[allow(dead_code)]
fn live_iron_golem_hostile_to_player(eid: i32, gossip: &VillagerGossip, player: VillageUuid) -> bool {
    if live_native_mob(eid, "iron_golem").is_none() { return false; }
    let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
    let state = states.entry(eid)
        .or_insert(NativeUtilityState::Iron(IronGolemState::default()));
    matches!(state, NativeUtilityState::Iron(golem) if golem.hostile_to_player(gossip, player))
}

#[allow(dead_code)]
fn live_snow_golem_shear(eid: i32) -> bool {
    let Some(mob) = live_native_mob(eid, "snow_golem") else { return false };
    let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
    let state = states.entry(eid)
        .or_insert(NativeUtilityState::Snow(SnowGolemState::default()));
    if let NativeUtilityState::Snow(snow) = state {
        if snow.shear() {
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(item) = name_to_item("carved_pumpkin") {
                drop_live_stack((x, y, z), (item, 1, Vec::new()));
            }
            return true;
        }
    }
    false
}

#[allow(dead_code)]
fn live_allay_give(eid: i32, player: VillageUuid, item: Stack) -> bool {
    if live_native_mob(eid, "allay").is_none() { return false; }
    let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
    let state = states.entry(eid)
        .or_insert_with(|| NativeUtilityState::Allay(AllayState::default()));
    if let NativeUtilityState::Allay(allay) = state {
        allay.give_item(player, item)
    } else {
        false
    }
}

/// Vanilla `Allay.mobInteract`'s empty-hand branch: an allay carrying a sample
/// item returns it whole. The owner bond survives — the allay keeps
/// associating with the player who fed it.
#[allow(dead_code)]
fn live_allay_take(eid: i32) -> Option<Stack> {
    live_native_mob(eid, "allay")?;
    let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
    let NativeUtilityState::Allay(allay) = states.get_mut(&eid)? else {
        return None;
    };
    allay.held.take()
}

/// What one routed mob interaction decided. The packet handler applies the
/// inventory side: consume/damage the held item or absorb a returned stack.
#[allow(dead_code)] // consumed by the interact-entity packet handler owned by another lane
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MobInteractOutcome {
    /// An iron ingot healed the golem (+25 HP toward 100): consume one held
    /// iron_ingot.
    IronGolemRepaired { hp_after: f32 },
    /// Shears stripped the snow golem's pumpkin; the carved_pumpkin drop is
    /// already spawned. Damage the shears by one.
    SnowGolemSheared,
    /// The allay accepted one of the held item as its new sample: consume it.
    AllayItemGiven,
    /// The allay's held sample went to the player's empty hand.
    AllayItemTaken(Stack),
}

/// Unified utility-mob interact dispatch for the `interact`/`useOn` entity
/// lane. `mob_kind` (bare or `minecraft:`-namespaced) is verified against the
/// live mob's registry type and the interaction only applies in the mob's own
/// dimension — a click routed at a different-dimension echo does nothing.
/// Returns the decision for the caller to apply to the held stack.
#[allow(dead_code)] // consumed by the interact-entity packet handler owned by another lane
pub(crate) fn villager_raid_interact_mob(
    dim: DimId,
    mob_eid: i32,
    mob_kind: &str,
    held_item: &str,
    player_eid: i32,
) -> Option<MobInteractOutcome> {
    let kind = mob_kind.strip_prefix("minecraft:").unwrap_or(mob_kind);
    let mob = live_native_mob(mob_eid, kind)?;
    if mob.dim != dim {
        return None;
    }
    let item = held_item.strip_prefix("minecraft:").unwrap_or(held_item);
    match kind {
        "iron_golem" if item == "iron_ingot" => {
            live_iron_golem_repair(mob_eid, true).then(|| MobInteractOutcome::IronGolemRepaired {
                hp_after: *mob.hp.lock().unwrap_or_else(|e| e.into_inner()),
            })
        }
        "snow_golem" if item == "shears" => {
            live_snow_golem_shear(mob_eid).then_some(MobInteractOutcome::SnowGolemSheared)
        }
        "zombie_villager" if item == "golden_apple" => {
            let now = TICK.load(Ordering::Relaxed);
            let weakened = live_mob_states()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&mob_eid)
                .is_some_and(|state| {
                    state.potions.iter().any(|(effect, until)| {
                        *effect == WitchPotion::Weakness && *until > now
                    })
                });
            if !weakened {
                return None;
            }
            let owner = client_snapshot()
                .into_iter()
                .find(|client| client.eid == player_eid)
                .map(|client| client.uuid)?;
            let (profession, xp, offers) = live_zombie_villager_data()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&mob_eid)
                .unwrap_or((VillagerProfession::None, 0, Vec::new()));
            live_zombie_villager_cures().lock().unwrap_or_else(|e| e.into_inner()).insert(
                mob_eid,
                ZombieVillagerCure::start(owner, profession, xp, offers, world_rng::next_below(2401) as u16),
            );
            if let Some(state) = live_mob_states().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&mob_eid) {
                state.potions.retain(|(effect, _)| *effect != WitchPotion::Weakness);
            }
            // The cross-file outcome contract has no zombie-curing variant yet;
            // this variant carries the same consume-one-held-item consequence.
            Some(MobInteractOutcome::IronGolemRepaired {
                hp_after: *mob.hp.lock().unwrap_or_else(|e| e.into_inner()),
            })
        }
        "allay" => {
            let owner = client_snapshot()
                .into_iter()
                .find(|client| client.eid == player_eid)
                .map(|client| client.uuid)?;
            if item.is_empty() || item == "air" {
                live_allay_take(mob_eid).map(MobInteractOutcome::AllayItemTaken)
            } else {
                let stack: Stack = (name_to_item(item)?, 1, Vec::new());
                live_allay_give(mob_eid, owner, stack)
                    .then_some(MobInteractOutcome::AllayItemGiven)
            }
        }
        _ => None,
    }
}

#[allow(dead_code)]
fn live_allay_note(eid: i32, note: BlockPos, tick: u64) -> bool {
    if live_native_mob(eid, "allay").is_none() { return false; }
    let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
    let state = states.entry(eid)
        .or_insert_with(|| NativeUtilityState::Allay(AllayState::default()));
    if let NativeUtilityState::Allay(allay) = state {
        allay.hears_note(note, tick);
        true
    } else { false }
}

#[allow(dead_code)]
fn live_allay_duplicate(eid: i32, dancing: bool, jukebox_active: bool, shard: bool, tick: u64) -> Option<Arc<Mob>> {
    let original = live_native_mob(eid, "allay")?;
    let spec = entity_spec_by_name("allay")?;
    with_save_guard(|| {
        if mobs().lock().unwrap_or_else(|e| e.into_inner()).len() >= MOB_CAP {
            return None;
        }
        let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
        let state = states.entry(eid)
            .or_insert_with(|| NativeUtilityState::Allay(AllayState::default()));
        let NativeUtilityState::Allay(allay) = state else { return None };
        if !allay.duplicate(dancing, jukebox_active, shard, tick) { return None; }
        let position = *original.pos.lock().unwrap_or_else(|e| e.into_inner());
        let mob = new_live_mob(spec.id, original.dim, position, spec.max_health, None);
        mobs().lock().unwrap_or_else(|e| e.into_inner()).push(mob.clone());
        states.insert(mob.eid, NativeUtilityState::Allay(AllayState::default()));
        Some(mob)
    })
}

/// The allay keeps the sample item while carrying a matching ground stack to
/// its linked note block or the player who handed it the sample.
#[cfg_attr(not(test), allow(dead_code))]
fn live_allay_collect_and_deliver(mob: &Arc<Mob>, at: [f64; 3], tick: u64) {
    let (owner, note, carrying) = {
        let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
        let state = states.entry(mob.eid).or_insert_with(|| NativeUtilityState::Allay(AllayState::default()));
        let NativeUtilityState::Allay(allay) = state else { return };
        (allay.owner, allay.delivery_target(tick), allay.collected.is_some())
    };
    if carrying {
        let destination = note.map(|(x, y, z)| [x as f64 + 0.5, y as f64 + 1.0, z as f64 + 0.5])
            .or_else(|| owner.and_then(|uuid| client_snapshot().into_iter().find_map(|client| {
                (client.uuid == uuid && !client.is_spectator() && client_dim_pos(&client).0 == mob.dim)
                    .then(|| {
                        let (_, x, y, z, ..) = client_dim_pos(&client);
                        [x, y + 1.0, z]
                    })
            })));
        if let Some(destination) = destination {
            let distance_sq = (0..3).map(|axis| (destination[axis] - at[axis]).powi(2)).sum::<f64>();
            if distance_sq <= 2.5_f64.powi(2) {
                with_save_guard(|| {
                    let carried = {
                        let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
                        match states.get_mut(&mob.eid) {
                            Some(NativeUtilityState::Allay(allay)) => allay.collected.take(),
                            _ => None,
                        }
                    };
                    if let Some(stack) = carried {
                        drop_live_stack((destination[0], destination[1], destination[2]), stack);
                    }
                });
            } else {
                live_utility_move_toward(mob, at, destination);
            }
        }
        return;
    }
    // The drop registry does not carry a dimension, so it represents the
    // Overworld. Never collect an item from another dimension's coordinates.
    if mob.dim != DimId::Overworld {
        return;
    }
    let candidate = {
        let states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
        let Some(NativeUtilityState::Allay(allay)) = states.get(&mob.eid) else { return };
        drops().lock().unwrap_or_else(|e| e.into_inner()).iter()
            .filter(|drop| drop.pickup_delay_elapsed() && allay.matches_drop(drop, at))
            .min_by(|a, b| {
                let distance = |drop: &Drop| (drop.x - at[0]).powi(2)
                    + (drop.y - at[1]).powi(2) + (drop.z - at[2]).powi(2);
                distance(a).total_cmp(&distance(b))
            })
            .map(|drop| (drop.eid, [drop.x, drop.y, drop.z]))
    };
    let Some((eid, position)) = candidate else { return };
    let distance_sq = (0..3).map(|axis| (position[axis] - at[axis]).powi(2)).sum::<f64>();
    if distance_sq > 2.0_f64.powi(2) {
        live_utility_move_toward(mob, at, position);
        return;
    }
    let consumed = with_save_guard(|| {
        let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
        let Some(NativeUtilityState::Allay(allay)) = states.get_mut(&mob.eid) else { return None };
        let mut ground = drops().lock().unwrap_or_else(|e| e.into_inner());
        let index = ground.iter().position(|drop| drop.eid == eid)?;
        if !allay.collect_drop(&ground[index], at) { return None; }
        let count = ground[index].cnt;
        ground.remove(index);
        Some(count)
    });
    if let Some(count) = consumed {
        broadcast_seen(eid, |_c| (0x6f, collect_pkt(eid, mob.eid, count)));
        broadcast_seen_destroy(eid);
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn live_utility_move_toward(mob: &Mob, from: [f64; 3], to: [f64; 3]) {
    let delta = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
    let distance = delta.iter().map(|v| v * v).sum::<f64>().sqrt();
    if distance > f64::EPSILON {
        let mut velocity = mob.vel.lock().unwrap_or_else(|e| e.into_inner());
        for axis in 0..3 {
            velocity[axis] = delta[axis] / distance * 0.28;
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn live_fox_collect(mob: &Arc<Mob>, at: [f64; 3]) {
    if mob.dim != DimId::Overworld { return; }
    let collected = with_save_guard(|| {
        let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
        let Some(NativeUtilityState::Fox(fox)) = states.get_mut(&mob.eid) else { return None };
        if fox.sleeping || fox.held.is_some() { return None; }
        let mut ground = drops().lock().unwrap_or_else(|e| e.into_inner());
        let drop = ground.iter_mut().find(|drop| {
            drop.target.is_none() && drop.pickup_delay_elapsed() && drop.cnt > 0
                && (drop.x - at[0]).powi(2) + (drop.y - at[1]).powi(2)
                    + (drop.z - at[2]).powi(2) < 1.5_f64.powi(2)
        })?;
        if !fox.pickup((drop.item, 1, drop.comps.clone())) { return None; }
        let item = drop.item;
        drop.cnt -= 1;
        let remaining = (drop.cnt > 0).then(|| drop_meta_pkt(drop));
        let eid = drop.eid;
        if remaining.is_none() { ground.retain(|drop| drop.eid != eid); }
        Some((eid, remaining, item))
    });
    if let Some((eid, remaining, item)) = collected {
        broadcast_seen(eid, |_c| (0x6f, collect_pkt(eid, mob.eid, 1)));
        if let Some(packet) = remaining {
            broadcast_seen(eid, |_c| (0x58, packet.clone()));
        } else {
            broadcast_seen_destroy(eid);
        }
        // A fox that mouths a dropped berry accepts the nearby player's feed —
        // the stand-in for the vanilla feed/trust bond while breeding is owned
        // by the generic `love` path. Trust sticks only while the fox holds
        // the berry it picked up.
        let berries = ["minecraft:sweet_berries", "minecraft:glow_berries"]
            .iter()
            .filter_map(|name| name_to_item(name))
            .any(|id| id == item);
        if berries {
            let feeder = client_snapshot()
                .iter()
                .filter(|client| !client.is_spectator())
                .filter(|client| {
                    let (dim, px, py, pz, ..) = client_dim_pos(client);
                    dim == mob.dim
                        && (px - at[0]).powi(2) + (py - at[1]).powi(2) + (pz - at[2]).powi(2)
                            <= 8.0_f64.powi(2)
                })
                .min_by(|a, b| {
                    let dist = |client: &&Arc<Client>| {
                        let (_, px, py, pz, ..) = client_dim_pos(client);
                        (px - at[0]).powi(2) + (py - at[1]).powi(2) + (pz - at[2]).powi(2)
                    };
                    dist(a).total_cmp(&dist(b))
                })
                .map(|client| client.uuid);
            if let Some(uuid) = feeder {
                live_fox_trust(mob.eid, uuid);
            }
        }
    }
}

#[allow(dead_code)]
fn live_fox_trust(eid: i32, player: VillageUuid) -> bool {
    if live_native_mob(eid, "fox").is_none() { return false; }
    let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
    let state = states.entry(eid)
        .or_insert_with(|| NativeUtilityState::Fox(FoxState::default()));
    if let NativeUtilityState::Fox(fox) = state {
        fox.trust(player);
        true
    } else { false }
}

#[allow(dead_code)]
fn live_camel_toggle_sitting(eid: i32) -> bool {
    if live_native_mob(eid, "camel").is_none() { return false; }
    let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
    let state = states.entry(eid)
        .or_insert_with(|| NativeUtilityState::Camel(CamelState::default()));
    if let NativeUtilityState::Camel(camel) = state {
        camel.toggle_sitting();
        true
    } else { false }
}

#[allow(dead_code)]
fn live_camel_dash(eid: i32, tick: u64, yaw: f32) -> Option<[f64; 3]> {
    let mob = live_native_mob(eid, "camel")?;
    let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
    let state = states.entry(eid)
        .or_insert_with(|| NativeUtilityState::Camel(CamelState::default()));
    let impulse = match state {
        NativeUtilityState::Camel(camel) => camel.dash(tick, yaw)?,
        _ => return None,
    };
    drop(states);
    let mut velocity = mob.vel.lock().unwrap_or_else(|e| e.into_inner());
    velocity[0] += impulse[0];
    velocity[1] += impulse[1];
    velocity[2] += impulse[2];
    Some(impulse)
}

/// `AllayAi` duplication dance: an allay carrying an amethyst shard as its
/// sample item duplicates once per `duplication_ready_at` while dancing —
/// that is, within 10 blocks of a jukebox that is mid-song.
#[cfg_attr(not(test), allow(dead_code))]
fn live_allay_jukebox_step(mob: &Arc<Mob>, at: [f64; 3], tick: u64) {
    let playing = {
        let store = world_block_entity_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        store.iter().any(|(&(d, jx, jy, jz), be)| {
            d == mob.dim
                && matches!(
                    be,
                    VanillaBlockEntity::Jukebox(jukebox)
                        if jukebox.record_item.is_some()
                            && jukebox.ticks_since_song_started < 4_000
                )
                && (f64::from(jx) + 0.5 - at[0]).powi(2)
                    + (f64::from(jy) + 0.5 - at[1]).powi(2)
                    + (f64::from(jz) + 0.5 - at[2]).powi(2)
                    <= 10.0_f64.powi(2)
        })
    };
    if !playing {
        return;
    }
    let shard = name_to_item("minecraft:amethyst_shard").is_some_and(|id| {
        live_native_utility_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&mob.eid)
            .is_some_and(|state| {
                matches!(
                    state,
                    NativeUtilityState::Allay(allay)
                        if allay.held.as_ref().is_some_and(|held| held.0 == id)
                )
            })
    });
    live_allay_duplicate(mob.eid, true, true, shard, tick);
}

/// Village golems retaliate against players the village is hostile to
/// (`gossip.golem_hostile` — reputation at or below the vanilla -100
/// threshold) or who provoked any nearby villager (negative gossip, which
/// accrues only from attacking villagers or the golem). The hit itself runs
/// the vanilla fling: `iron_golem_fling_velocity` supplies the launch.
#[cfg_attr(not(test), allow(dead_code))]
fn live_golem_provoke_step(mob: &Arc<Mob>, at: [f64; 3], _tick: u64) {
    let targets: Vec<(Arc<Client>, [f64; 3])> = client_snapshot()
        .iter()
        .filter_map(|client| {
            if client.is_spectator() {
                return None;
            }
            let (dim, cx, cy, cz, ..) = client_dim_pos(client);
            let in_reach = dim == mob.dim
                && (cx - at[0]).powi(2) + (cy - at[1]).powi(2) + (cz - at[2]).powi(2)
                    <= 4.5_f64.powi(2);
            in_reach.then(|| (Arc::clone(client), [cx, cy, cz]))
        })
        .collect();
    if targets.is_empty() {
        return;
    }
    let villager_type = name_to_etype("villager");
    let nearby_villagers: Vec<i32> = mobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(|other| {
            if Some(other.etype) != villager_type {
                return None;
            }
            let (mx, my, mz, ..) = *other.pos.lock().unwrap_or_else(|e| e.into_inner());
            ((mx - at[0]).powi(2) + (my - at[1]).powi(2) + (mz - at[2]).powi(2)
                <= 16.0_f64.powi(2))
            .then_some(other.eid)
        })
        .collect();
    if nearby_villagers.is_empty() {
        return;
    }
    {
        let villagers = live_villagers().lock().unwrap_or_else(|e| e.into_inner());
        for (client, pos) in targets {
            let (mut threat, mut attack) = (false, false);
            for eid in &nearby_villagers {
                let Some(villager) = villagers.get(eid) else {
                    continue;
                };
                threat = threat
                    || live_iron_golem_hostile_to_player(mob.eid, &villager.gossip, client.uuid);
                attack = attack || villager.gossip.reputation(client.uuid) < 0;
            }
            if !iron_golem_provoked(threat, attack) {
                continue;
            }
            let fling = iron_golem_fling_velocity(pos, at);
            live_mob_melee(
                mob,
                &client,
                pos,
                7.5 + world_rng::next_below(140) as f32 / 10.0,
                fling[1],
            );
            break;
        }
    }
}

/// Per-equine jump-charge percentage while the rider holds the steering jump
/// bit, mirroring `AbstractHorse.playerJumpPendingScale` (0..100).
fn equine_jump_charges() -> &'static Mutex<HashMap<i32, i32>> {
    static CHARGES: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
    CHARGES.get_or_init(Mutex::default)
}

type PendingFoal = (DimId, [f64; 3], EquineOffspringStats, u64);

/// Foal attributes rolled from two in-love parents by `live_equine_breed_roll`,
/// waiting for the foal's first utility tick to claim them. The spawn itself
/// is produced by the generic `live_mob_breed` path; this hands the child the
/// vanilla `setOffspringAttributes` roll instead of fresh spawn dice.
#[cfg_attr(not(test), allow(dead_code))]
fn live_equine_pending_foals() -> &'static Mutex<Vec<PendingFoal>> {
    static FOALS: OnceLock<Mutex<Vec<PendingFoal>>> = OnceLock::new();
    FOALS.get_or_init(Mutex::default)
}

/// `AbstractHorse` utility tick: the `RunAroundLikeCrazyGoal` tame/buck roll
/// while a player rides an untamed equine, foal grow-up, and the breed-stat
/// roll between two in-love parents. Spawn attributes roll once when the
/// state first materializes.
#[cfg_attr(not(test), allow(dead_code))]
fn live_equine_utility_step(mob: &Arc<Mob>, kind: EquineKind, at: [f64; 3], tick: u64) {
    let rider = mounts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .registry
        .passengers_of(mob.eid)
        .first()
        .copied();
    let rider_uuid = rider.and_then(|eid| {
        client_snapshot()
            .iter()
            .find(|client| client.eid == eid)
            .map(|client| client.uuid)
    });
    let (buck, jump_strength) = {
        let mut states = live_native_utility_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = states.entry(mob.eid).or_insert_with(|| {
            let roll = || f64::from(world_rng::next_below(10_000)) / 10_000.0;
            NativeUtilityState::Equine(EquineState::spawned(
                kind,
                [roll(), roll(), roll()],
                [roll(), roll(), roll()],
                [roll(), roll()],
            ))
        });
        let NativeUtilityState::Equine(equine) = state else {
            return;
        };
        // Foal grow-up: the vanilla 24000-tick `age` counter toward adult.
        if equine.bred_foal && equine.age_ticks < 0 {
            equine.age_ticks += 1;
        }
        let buck = if equine.tamed || rider.is_none() {
            None
        } else {
            let roll = world_rng::next_below(equine.kind.max_temper().max(1) as u32);
            match equine.try_equine_tame_tick(roll) {
                Some(EquineTameOutcome::Tamed) => {
                    // `tameWithName`: the successful roll also binds the rider.
                    if let Some(uuid) = rider_uuid {
                        equine.tame_with_name(uuid);
                    }
                    Some(EquineTameOutcome::Tamed)
                }
                other => other,
            }
        };
        (buck, equine.tamed.then_some(equine.jump_strength))
    };
    live_equine_breed_roll(mob, at, tick);
    // `AbstractHorse` player-jump input: while a tamed equine carries a rider,
    // the steering jump bit charges `jumpStrength` and release applies the
    // scaled upward velocity — the same `ServerboundPlayerInputPacket` bit0
    // the camel dash reads.
    if let (Some(_), Some(strength)) = (rider, jump_strength) {
        let jump_held = mounts()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .steering
            .get(&mob.eid)
            .is_some_and(|input| input.jump);
        let mut charges = equine_jump_charges()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if jump_held {
            let charge = charges.entry(mob.eid).or_insert(0);
            *charge = (*charge + 5).min(100);
        } else if let Some(charge) = charges.remove(&mob.eid) {
            let (bx, by, bz) = (at[0].floor() as i32, at[1].floor() as i32, at[2].floor() as i32);
            let grounded = crate::block_registry::breg()
                .is_some_and(|reg| !reg.is_air(block_at_dim(mob.dim, bx, by - 1, bz)));
            if grounded {
                mob.vel.lock().unwrap_or_else(|e| e.into_inner())[1] =
                    equine_jump_velocity(strength, charge);
            }
        }
    }
    if matches!(buck, Some(EquineTameOutcome::Bucked { .. })) {
        if let Some(rider) = rider {
            // `ejectPassengers` + `makeMad`: the failed roll throws the rider
            // off at the registry's safe dismount offset.
            let seat = mounts()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .registry
                .dismount(rider);
            if let Some([dx, dy, dz]) = seat {
                if let Some(client) = client_snapshot()
                    .iter()
                    .find(|client| client.eid == rider)
                    .cloned()
                {
                    let mut pos = client.pos.lock().unwrap_or_else(|e| e.into_inner());
                    pos.0 = dx;
                    pos.1 = dy;
                    pos.2 = dz;
                }
            }
        }
    }
}

/// `BreedGoal`/`setOffspringAttributes`: while this equine is in love beside
/// a same-kind mate that is also in love, roll the foal's attributes once and
/// leave them for the child the generic `live_mob_breed` path produces. A
/// freshly-seen equine instead claims any pending record at its position —
/// that is the foal receiving its bred attributes and baby age.
#[cfg_attr(not(test), allow(dead_code))]
fn live_equine_breed_roll(mob: &Arc<Mob>, at: [f64; 3], tick: u64) {
    if *mob.love.lock().unwrap_or_else(|e| e.into_inner()) > 0 {
        let mate_eid = mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|other| {
                other.eid != mob.eid
                    && other.etype == mob.etype
                    && *other.love.lock().unwrap_or_else(|e| e.into_inner()) > 0
                    && {
                        let (ox, oy, oz, ..) =
                            *other.pos.lock().unwrap_or_else(|e| e.into_inner());
                        (ox - at[0]).powi(2) + (oy - at[1]).powi(2) + (oz - at[2]).powi(2)
                            <= 4.0
                    }
            })
            .map(|other| other.eid);
        let Some(mate_eid) = mate_eid else {
            return;
        };
        let stats = {
            let states = live_native_utility_states()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match (states.get(&mob.eid), states.get(&mate_eid)) {
                (
                    Some(NativeUtilityState::Equine(a)),
                    Some(NativeUtilityState::Equine(b)),
                ) => {
                    let rand_unit = f64::from(world_rng::next_below(10_000)) / 10_000.0;
                    breed_equine_stats(a, b, rand_unit)
                }
                _ => return,
            }
        };
        let mut pending = live_equine_pending_foals()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pending.retain(|&(_, _, _, when)| tick.saturating_sub(when) <= 100);
        // One record per breeding episode per site — a second roll for the
        // same pair in the same tick would overwrite identical data anyway.
        if !pending.iter().any(|&(dim, pos, _, when)| {
            dim == mob.dim
                && when == tick
                && (pos[0] - at[0]).abs() < 3.0
                && (pos[2] - at[2]).abs() < 3.0
        }) {
            pending.push((mob.dim, at, stats, tick));
        }
    } else {
        let mut states = live_native_utility_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(NativeUtilityState::Equine(equine)) = states.get_mut(&mob.eid) else {
            return;
        };
        if equine.bred_foal {
            return;
        }
        let claim = {
            let mut pending = live_equine_pending_foals()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pending
                .iter()
                .position(|&(dim, pos, _, when)| {
                    dim == mob.dim
                        && tick.saturating_sub(when) <= 100
                        && (pos[0] - at[0]).powi(2)
                            + (pos[1] - at[1]).powi(2)
                            + (pos[2] - at[2]).powi(2)
                            <= 4.0
                })
                .map(|index| pending.remove(index))
        };
        if let Some((_, _, stats, _)) = claim {
            equine.max_health = stats.max_health as f32;
            equine.health = equine.health.min(equine.max_health);
            equine.jump_strength = stats.jump_strength;
            equine.move_speed = stats.move_speed;
            equine.age_ticks = -24_000;
            equine.bred_foal = true;
        }
    }
}

/// `LandOnOwnersShoulderGoal` + `removeEntitiesOnShoulder`: the tame/sit flags
/// live in `parrot_pets` (the interact path), the shoulder goal mirrors them
/// here each tick, and a perched parrot is re-seated on its owner — the live
/// stand-in for shoulder NBT — until a hazard forces the dismount.
#[cfg_attr(not(test), allow(dead_code))]
fn live_parrot_utility_step(mob: &Arc<Mob>, at: [f64; 3], tick: u64) {
    let Some(reg) = crate::block_registry::breg() else {
        return;
    };
    let (owner, on_shoulder, perched_ticks) = {
        let pet = parrot_pets()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&mob.eid)
            .map(|pet| (pet.owner, pet.sitting));
        let mut states = live_native_utility_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = states.entry(mob.eid).or_insert_with(|| {
            NativeUtilityState::Parrot(ParrotState::new((mob.eid as u64 % 5) as u8))
        });
        let NativeUtilityState::Parrot(parrot) = state else {
            return;
        };
        if let Some((owner, sitting)) = pet {
            parrot.tamed = owner.is_some();
            parrot.owner = owner;
            parrot.sitting_ordered = sitting;
        }
        parrot.tick();
        if parrot.on_shoulder {
            parrot.perched_ticks = parrot.perched_ticks.saturating_add(1);
        }
        (parrot.owner, parrot.on_shoulder, parrot.perched_ticks)
    };
    let Some(owner) = owner else {
        return;
    };
    let Some(client) = client_snapshot()
        .iter()
        .find(|client| {
            client.uuid == owner
                && !client.is_spectator()
                && *client.dim.lock().unwrap_or_else(|e| e.into_inner()) == mob.dim
        })
        .cloned()
    else {
        return;
    };
    let (_, px, py, pz, ..) = client_dim_pos(&client);
    let (fbx, fby, fbz) = (px.floor() as i32, py.floor() as i32, pz.floor() as i32);
    let feet = block_at_dim(mob.dim, fbx, fby, fbz);
    let in_water = reg.has_water(feet);
    let in_powder_snow = reg.name_of(feet) == "minecraft:powder_snow";
    if on_shoulder {
        let hazards = ShoulderDropConditions {
            fall_distance: (*client.fall_from.lock().unwrap_or_else(|e| e.into_inner()) - py)
                .max(0.0) as f32,
            in_water,
            player_flying: *client.fall_fly.lock().unwrap_or_else(|e| e.into_inner()),
            player_sleeping: client
                .sleeping
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some(),
            in_powder_snow,
            player_damaged: *client.invulnerable.lock().unwrap_or_else(|e| e.into_inner()) > 0,
            // Riptide launches are not modeled in this build.
            spin_attack: false,
        };
        let dropped = {
            let mut states = live_native_utility_states()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match states.get_mut(&mob.eid) {
                Some(NativeUtilityState::Parrot(parrot)) => {
                    parrot.shoulder_dismount(&hazards, perched_ticks)
                }
                _ => false,
            }
        };
        if !dropped {
            // Still perched: keep the entity glued to its owner's shoulder.
            {
                let mut pos = mob.pos.lock().unwrap_or_else(|e| e.into_inner());
                *pos = (px, py + 1.0, pz, pos.3, pos.4);
            }
            mob.vel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .fill(0.0);
            if tick.is_multiple_of(20) {
                broadcast_seen(mob.eid, |_| {
                    (0x70, entity_teleport(mob.eid, px, py + 1.0, pz, 0.0, 0.0, false))
                });
            }
        }
    } else {
        let shoulder_slot_free = {
            let states = live_native_utility_states()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            !states.iter().any(|(&eid, state)| {
                eid != mob.eid
                    && matches!(
                        state,
                        NativeUtilityState::Parrot(parrot)
                            if parrot.on_shoulder && parrot.owner == Some(owner)
                    )
            })
        };
        let ctx = ShoulderPerchContext {
            player: owner,
            bounding_boxes_intersect: (px - at[0]).powi(2)
                + (py - at[1]).powi(2)
                + (pz - at[2]).powi(2)
                <= 2.25,
            player_on_ground: client.pos.lock().unwrap_or_else(|e| e.into_inner()).5,
            player_mounted: mounts()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .registry
                .vehicle_of
                .contains_key(&client.eid),
            player_in_water: in_water,
            player_in_powder_snow: in_powder_snow,
            shoulder_slot_free,
        };
        let mut states = live_native_utility_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(NativeUtilityState::Parrot(parrot)) = states.get_mut(&mob.eid) {
            parrot.try_perch_on_shoulder(&ctx);
        }
    }
}

/// A pumpkin placed on the completed frame consumes all frame blocks and
/// spawns the corresponding player-created golem.
#[cfg_attr(not(test), allow(dead_code))]
fn live_construct_golem(dim: DimId, pumpkin: BlockPos) -> Option<Arc<Mob>> {
    let reg = crate::block_registry::breg()?;
    with_save_guard(|| {
        let (kind, blocks) = detect_golem_construction(pumpkin, |(x, y, z)| {
            reg.name_of(block_at_dim(dim, x, y, z))
        })?;
        let name = match kind {
            ConstructedGolem::Iron => "iron_golem",
            ConstructedGolem::Snow => "snow_golem",
        };
        let spec = entity_spec_by_name(name)?;
        if mobs().lock().unwrap_or_else(|e| e.into_inner()).len() >= MOB_CAP {
            return None;
        }
        for (x, y, z) in blocks {
            set_block_dim(dim, x, y, z, reg.state_of("minecraft:air", None));
        }
        let mob = new_live_mob(
            spec.id, dim,
            (pumpkin.0 as f64 + 0.5, (pumpkin.1 - 2) as f64, pumpkin.2 as f64 + 0.5, 0.0, 0.0),
            spec.max_health, None,
        );
        mobs().lock().unwrap_or_else(|e| e.into_inner()).push(mob.clone());
        let state = match kind {
            ConstructedGolem::Iron => NativeUtilityState::Iron(IronGolemState { player_created: true }),
            ConstructedGolem::Snow => NativeUtilityState::Snow(SnowGolemState::default()),
        };
        live_native_utility_states()
            .lock().unwrap_or_else(|e| e.into_inner()).insert(mob.eid, state);
        Some(mob)
    })
}

/// Existing placement handlers are owned by another lane. Scan only player-edited
/// pumpkin blocks, avoiding thousands of generated-block lookups each second.
#[cfg_attr(not(test), allow(dead_code))]
fn live_golem_formation_tick(tick: u64) {
    if !tick.is_multiple_of(20) {
        return;
    }
    let Some(reg) = crate::block_registry::breg() else { return };
    let clients = client_snapshot();
    let mut candidates = HashSet::new();
    let overlay = world().read_all();
    for client in clients.iter().filter(|c| !c.is_spectator()) {
        let (dim, x, y, z, ..) = client_dim_pos(client);
        let (cx, cy, cz) = (x.floor() as i32, y.floor() as i32, z.floor() as i32);
        for dx in -3..=3 {
            for dz in -3..=3 {
                for dy in -4..=4 {
                    let pos = (cx + dx, cy + dy, cz + dz);
                    if pos.1 < dim.min_y() + 2 || pos.1 >= dim.min_y() + dim.height() {
                        continue;
                    }
                    if overlay.get(&(dim, pos.0 >> 4, pos.2 >> 4))
                        .and_then(|chunk| chunk.get(&local_idx(pos.0, pos.1, pos.2)))
                        .is_some_and(|&state| matches!(reg.name_of(state),
                            "minecraft:carved_pumpkin" | "minecraft:jack_o_lantern")) {
                        candidates.insert((dim, pos));
                    }
                }
            }
        }
    }
    drop(overlay);
    for (dim, pos) in candidates {
        live_construct_golem(dim, pos);
    }
}

/// Villagers near the same claimed bell exchange gossip during their meeting
/// period. A recently gossiping group can summon a defender while threatened.
#[cfg_attr(not(test), allow(dead_code))]
fn live_village_golem_tick(tick: u64) {
    if !tick.is_multiple_of(200) || GR_MOB_SPAWNING.load(Ordering::Relaxed) == 0 {
        return;
    }
    let world_time = WORLD_TIME.load(Ordering::Relaxed);
    let snapshots: Vec<_> = mobs().lock().unwrap_or_else(|e| e.into_inner())
        .iter().filter(|mob| *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0)
        .map(|mob| {
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            (mob.eid, mob.dim, (x.floor() as i32, y.floor() as i32, z.floor() as i32), mob.etype)
        }).collect();
    let villager_type = name_to_etype("villager");
    let golem_type = name_to_etype("iron_golem");
    let positions: HashMap<i32, (DimId, BlockPos)> = snapshots.iter()
        .filter(|(_, _, _, ty)| Some(*ty) == villager_type)
        .map(|(eid, dim, pos, _)| (*eid, (*dim, *pos))).collect();
    let mut groups: HashMap<(DimId, BlockPos), Vec<(i32, BlockPos)>> = HashMap::new();
    let mut villagers = live_villagers().lock().unwrap_or_else(|e| e.into_inner());
    for (&eid, villager) in villagers.iter() {
        let (Some(bell), Some(&(dim, pos))) = (villager.meeting_point, positions.get(&eid)) else { continue };
        if villager_within(pos, bell, 6) {
            groups.entry((dim, bell)).or_default().push((eid, pos));
        }
    }
    if VillagerActivity::at(world_time) == VillagerActivity::Meet {
        for group in groups.values() {
            if group.len() < 2 { continue; }
            let mut shared = VillagerGossip::default();
            for (eid, _) in group {
                if let Some(villager) = villagers.get(eid) {
                    shared.hear_from(&villager.gossip);
                }
            }
            for (eid, _) in group {
                if let Some(villager) = villagers.get_mut(eid) {
                    if villager.last_gossip.is_none_or(|last| world_time.saturating_sub(last) >= VILLAGER_GOSSIP_COOLDOWN) {
                        villager.gossip.hear_from(&shared);
                        villager.last_gossip = Some(world_time);
                    }
                }
            }
        }
    }
    let eligible: Vec<_> = groups.into_iter().filter_map(|((dim, _), group)| {
        let recent: Vec<BlockPos> = group.into_iter().filter_map(|(eid, pos)| {
            villagers.get(&eid).and_then(|villager| {
                villager.last_gossip.is_some_and(|last| world_time.saturating_sub(last) <= VILLAGER_GOSSIP_COOLDOWN)
                    .then_some(pos)
            })
        }).collect();
        (recent.len() >= 3).then_some((dim, recent))
    }).collect();
    drop(villagers);

    let Some(reg) = crate::block_registry::breg() else { return };
    let Some(spec) = entity_spec_by_name("iron_golem") else { return };
    for (dim, villagers) in eligible {
        let center = villagers[0];
        let within_16 = |other: BlockPos| {
            (center.0 - other.0).pow(2) + (center.1 - other.1).pow(2)
                + (center.2 - other.2).pow(2) <= 16_i32.pow(2)
        };
        let panicking = snapshots.iter().any(|(_, other_dim, pos, ty)| {
            *other_dim == dim && live_mob_category(*ty) == Some(MobCategory::Monster) && within_16(*pos)
        });
        let golem_nearby = snapshots.iter().any(|(_, other_dim, pos, ty)| {
            *other_dim == dim && Some(*ty) == golem_type && within_16(*pos)
        });
        if !village_golem_spawn_allowed(villagers.len(), panicking, golem_nearby) {
            continue;
        }
        let site = villagers.iter().copied().find(|&(x, y, z)| {
            reg.is_solid(block_at_dim(dim, x, y - 1, z))
                && reg.is_air(block_at_dim(dim, x, y, z))
                && reg.is_air(block_at_dim(dim, x, y + 1, z))
                && reg.is_air(block_at_dim(dim, x, y + 2, z))
        });
        let Some((x, y, z)) = site else { continue };
        with_save_guard(|| {
            let mut current = mobs().lock().unwrap_or_else(|e| e.into_inner());
            if current.len() >= MOB_CAP { return; }
            // Recheck inside the save transaction in case another village
            // created a defender since the snapshot was taken.
            if current.iter().any(|mob| mob.dim == dim && Some(mob.etype) == golem_type && {
                let (mx, my, mz, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
                (mx - f64::from(x)).powi(2) + (my - f64::from(y)).powi(2)
                    + (mz - f64::from(z)).powi(2) <= 16.0_f64.powi(2)
            }) { return; }
            let mob = new_live_mob(spec.id, dim,
                (x as f64 + 0.5, y as f64, z as f64 + 0.5, 0.0, 0.0), spec.max_health, None);
            current.push(mob.clone());
            drop(current);
            live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner())
                .insert(mob.eid, NativeUtilityState::Iron(IronGolemState { player_created: false }));
        });
    }
}

/// The shared server tick drives hydrated axolotls and snow-golem weather and
/// trails even when no player interaction packet is in flight.
#[cfg_attr(not(test), allow(dead_code))]
fn live_utility_mob_tick(tick: u64) {
    let axolotl_type = name_to_etype("axolotl");
    let snow_type = name_to_etype("snow_golem");
    let fox_type = name_to_etype("fox");
    let allay_type = name_to_etype("allay");
    let camel_type = name_to_etype("camel");
    let golem_type = name_to_etype("iron_golem");
    let parrot_type = name_to_etype("parrot");
    let relevant: Vec<Arc<Mob>> = mobs()
        .lock().unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|mob| {
            [
                axolotl_type,
                snow_type,
                fox_type,
                allay_type,
                camel_type,
                golem_type,
                parrot_type,
            ]
            .contains(&Some(mob.etype))
                || equine_kind_of(mob.etype).is_some()
        })
        .cloned()
        .collect();
    let alive = tick.is_multiple_of(400).then(|| {
        mobs().lock().unwrap_or_else(|e| e.into_inner())
            .iter().map(|mob| mob.eid).collect::<HashSet<i32>>()
    });
    if let Some(alive) = alive {
        live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner())
            .retain(|eid, _| alive.contains(eid));
    }
    if relevant.is_empty() { return; }
    let Some(reg) = crate::block_registry::breg() else { return };
    for mob in relevant {
        if *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0 { continue; }
        let (x, y, z, yaw, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
        let (bx, by, bz) = (x.floor() as i32, y.floor() as i32, z.floor() as i32);
        if Some(mob.etype) == axolotl_type {
            let rain_exposed = mob.dim == DimId::Overworld
                && RAINING.load(Ordering::Relaxed) > 0
                && random_tick_sky_exposed(mob.dim, bx, by, bz);
            let hydrated = rain_exposed || [by, by + 1].iter().any(|&layer| {
                let block = block_at_dim(mob.dim, bx, layer, bz);
                fluid_from_block(block).is_some_and(|fluid| fluid.kind == flowing_fluid::Kind::Water)
                    || waterlogged_source(block)
            });
            let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
            let mut hp = mob.hp.lock().unwrap_or_else(|e| e.into_inner());
            let state = states.entry(mob.eid).or_insert_with(|| NativeUtilityState::Axolotl(AxolotlState::new(*hp)));
            if let NativeUtilityState::Axolotl(axolotl) = state {
                axolotl.tick(hydrated, &mut hp, 14.0, tick);
            }
        } else if Some(mob.etype) == snow_type && tick.is_multiple_of(20) {
            let rain_exposed = mob.dim == DimId::Overworld
                && RAINING.load(Ordering::Relaxed) > 0
                && random_tick_sky_exposed(mob.dim, bx, by, bz);
            let hot = mob.dim == DimId::Nether || (mob.dim == DimId::Overworld
                && worldgen::with_gen_dim(mob.dim, |g| {
                    g.biome_name_at(bx, by, bz).is_some_and(|name| matches!(name.as_str(),
                        "minecraft:desert" | "minecraft:savanna" | "minecraft:savanna_plateau"
                        | "minecraft:windswept_savanna" | "minecraft:badlands"
                        | "minecraft:wooded_badlands" | "minecraft:eroded_badlands"))
                }).unwrap_or(false));
            let snow_here = mob.dim == DimId::Overworld
                && reg.is_air(block_at_dim(mob.dim, bx, by, bz))
                && reg.is_solid(block_at_dim(mob.dim, bx, by - 1, bz))
                && cold_biome_at(mob.dim, bx, by, bz)
                && world_griefing_enabled();
            let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
            let state = states.entry(mob.eid).or_insert_with(|| NativeUtilityState::Snow(SnowGolemState::default()));
            let place = if let NativeUtilityState::Snow(_) = state {
                let mut hp = mob.hp.lock().unwrap_or_else(|e| e.into_inner());
                SnowGolemState::environment_tick(&mut hp, hot, rain_exposed, snow_here)
            } else { false };
            drop(states);
            if place {
                let current = block_at_dim(mob.dim, bx, by, bz);
                set_block_if_dim(mob.dim, bx, by, bz, reg.state_of("minecraft:snow", None), Some(current));
            }
        } else if Some(mob.etype) == allay_type {
            if tick.is_multiple_of(10) {
                live_allay_collect_and_deliver(&mob, [x, y, z], tick);
                live_allay_jukebox_step(&mob, [x, y, z], tick);
            }
        } else if Some(mob.etype) == camel_type {
            // `CamelAi` random sitting: a camel with no riders sits down for a
            // stretch, then stands again. Sitting camels do not drift.
            let phase = tick.wrapping_add(mob.eid as u64 * 97) % 2400;
            let sitting = {
                let states = live_native_utility_states()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                matches!(
                    states.get(&mob.eid),
                    Some(NativeUtilityState::Camel(camel)) if camel.sitting
                )
            };
            if (!sitting && phase == 0) || (sitting && phase == 400) {
                live_camel_toggle_sitting(mob.eid);
            }
            if sitting {
                *mob.vel.lock().unwrap_or_else(|e| e.into_inner()) = [0.0; 3];
            } else {
                // `Camel.saddle` rider jump input fires the dash on its
                // 55-tick cooldown — the same `ServerboundPlayerInputPacket`
                // bit0 the steering map stores per vehicle.
                let dash_input = {
                    let live = mounts().lock().unwrap_or_else(|e| e.into_inner());
                    !live.registry.passengers_of(mob.eid).is_empty()
                        && live
                            .steering
                            .get(&mob.eid)
                            .is_some_and(|input| input.jump)
                };
                if dash_input {
                    live_camel_dash(mob.eid, tick, yaw);
                }
            }
        } else if let Some(kind) = equine_kind_of(mob.etype) {
            live_equine_utility_step(&mob, kind, [x, y, z], tick);
        } else if Some(mob.etype) == parrot_type {
            live_parrot_utility_step(&mob, [x, y, z], tick);
        } else if Some(mob.etype) == golem_type && tick.is_multiple_of(20) {
            live_golem_provoke_step(&mob, [x, y, z], tick);
        } else if Some(mob.etype) == fox_type && tick.is_multiple_of(20) {
            let nearby = client_snapshot().iter().filter_map(|client| {
                let (dim, px, py, pz, ..) = client_dim_pos(client);
                (!client.is_spectator() && dim == mob.dim
                    && (px - x).powi(2) + (py - y).powi(2) + (pz - z).powi(2) <= 16.0_f64.powi(2))
                    .then_some(client.uuid)
            }).collect::<Vec<_>>();
            let in_water = reg.has_water(block_at_dim(mob.dim, bx, by, bz));
            let mut states = live_native_utility_states().lock().unwrap_or_else(|e| e.into_inner());
            let state = states.entry(mob.eid)
                .or_insert_with(|| NativeUtilityState::Fox(FoxState::default()));
            if let NativeUtilityState::Fox(fox) = state {
                let threatened = nearby.iter().any(|uuid| !fox.trusted.contains(&Some(*uuid)));
                fox.tick(WORLD_TIME.load(Ordering::Relaxed) % 24_000 < 12_000,
                    threatened, in_water);
            }
            drop(states);
            live_fox_collect(&mob, [x, y, z]);
        }
    }
}

// ---------- live world wiring ----------
//
// The types above model the vanilla data; the functions below connect them to
// the running server: a per-dimension `PoiRegistry` filled by lazy scans, the
// live `Villager` records keyed by entity id, raid-omen conversion, and the
// `saved_raids()` raid list. Lock order wherever two of these are held at once
// is `live_villagers`/`saved_raids` before `live_pois` before `world`.

/// One POI registry per dimension, populated by scanning the loaded surface
/// around villagers and omen carriers. Scans are the only writer, so a broken
/// workstation or bed drops its record — and its claims — on the next pass.
#[cfg_attr(not(test), allow(dead_code))]
fn live_pois() -> &'static LockedMap<DimId, PoiRegistry> {
    static POIS: Registry<DimId, PoiRegistry> = OnceLock::new();
    POIS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Horizontal POI discovery range around a villager. Vanilla villagers notice
/// workstations within 16 blocks; the scan's vertical band stays inside the
/// dimension's height.
#[cfg_attr(not(test), allow(dead_code))]
const LIVE_POI_SCAN_RADIUS: i32 = 16;

/// POI bindings are re-scanned on a slow cadence (20 s), not per tick: the scan
/// walks a 33x33x33 box through `block_at_dim`.
#[cfg_attr(not(test), allow(dead_code))]
const LIVE_POI_RESCAN_GT: u64 = 400;

/// Raid-omen countdowns for players who stepped into a village, keyed by UUID.
#[cfg_attr(not(test), allow(dead_code))]
fn live_raid_omens() -> &'static LockedMap<VillageUuid, RaidOmen> {
    static OMENS: Registry<VillageUuid, RaidOmen> = OnceLock::new();
    OMENS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Live raider entity id -> (dimension, raid center, wave slot). A raider
/// death is translated into `Raid::record_defeat` through this map.
#[cfg_attr(not(test), allow(dead_code))]
fn live_raid_mobs() -> &'static LockedMap<i32, (DimId, BlockPos, RaidRaiderId)> {
    static RAIDERS: Registry<i32, (DimId, BlockPos, RaidRaiderId)> = OnceLock::new();
    RAIDERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Raider count of the wave currently in flight per (dimension, raid center),
/// so the bossbar can show the wave draining.
#[cfg_attr(not(test), allow(dead_code))]
fn live_raid_wave_totals() -> &'static LockedMap<(DimId, BlockPos), i32> {
    static TOTALS: Registry<(DimId, BlockPos), i32> = OnceLock::new();
    TOTALS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reconcile one dimension's POI registry against the loaded box around
/// `center`. Returns the number of POI blocks found.
#[cfg_attr(not(test), allow(dead_code))]
fn live_poi_scan(dim: DimId, center: BlockPos) -> usize {
    let Some(reg) = crate::block_registry::breg() else {
        return 0;
    };
    let min_y = (center.1 - LIVE_POI_SCAN_RADIUS).max(dim.min_y());
    let max_y = (center.1 + LIVE_POI_SCAN_RADIUS).min(dim.min_y() + dim.height() - 1);
    let mut registries = live_pois().lock().unwrap_or_else(|e| e.into_inner());
    scan_poi_box(
        registries.entry(dim).or_default(),
        reg,
        (
            center.0 - LIVE_POI_SCAN_RADIUS,
            min_y,
            center.2 - LIVE_POI_SCAN_RADIUS,
        ),
        (
            center.0 + LIVE_POI_SCAN_RADIUS,
            max_y,
            center.2 + LIVE_POI_SCAN_RADIUS,
        ),
        |pos| block_at_dim(dim, pos.0, pos.1, pos.2),
    )
}

// ---------- Vanilla 1.21.1 trade pools ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TradeBlueprintKind {
    Buy,
    Sell,
    Process,
    EnchantedTool,
    EnchantedBook,
    Map,
    SuspiciousStew,
    DyedArmor,
    TippedArrow,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TradeBlueprint {
    kind: TradeBlueprintKind,
    item: &'static str,
    count: i32,
    price: i32,
    result: &'static str,
    result_count: i32,
    max_uses: i32,
    xp: i32,
    multiplier: f32,
}

fn tb_buy(item: &'static str, count: i32, max_uses: i32, xp: i32) -> TradeBlueprint {
    TradeBlueprint { kind: TradeBlueprintKind::Buy, item, count, price: 1, result: "emerald", result_count: 1, max_uses, xp, multiplier: 0.05 }
}
fn tb_sell(item: &'static str, price: i32, count: i32, max_uses: i32, xp: i32, multiplier: f32) -> TradeBlueprint {
    TradeBlueprint { kind: TradeBlueprintKind::Sell, item: "emerald", count: price, price, result: item, result_count: count, max_uses, xp, multiplier }
}
fn tb_process(input: &'static str, count: i32, price: i32, output: &'static str, out_count: i32, max_uses: i32, xp: i32) -> TradeBlueprint {
    TradeBlueprint { kind: TradeBlueprintKind::Process, item: input, count, price, result: output, result_count: out_count, max_uses, xp, multiplier: 0.05 }
}
fn tb_enchanted(item: &'static str, base_price: i32, max_uses: i32, xp: i32, multiplier: f32) -> TradeBlueprint {
    TradeBlueprint { kind: TradeBlueprintKind::EnchantedTool, item: "emerald", count: base_price, price: base_price, result: item, result_count: 1, max_uses, xp, multiplier }
}
fn tb_book(xp: i32) -> TradeBlueprint {
    TradeBlueprint { kind: TradeBlueprintKind::EnchantedBook, item: "emerald", count: 1, price: 1, result: "enchanted_book", result_count: 1, max_uses: 12, xp, multiplier: 0.2 }
}
fn tb_map(price: i32, max_uses: i32, xp: i32) -> TradeBlueprint {
    TradeBlueprint { kind: TradeBlueprintKind::Map, item: "emerald", count: price, price, result: "filled_map", result_count: 1, max_uses, xp, multiplier: 0.2 }
}
fn tb_stew(xp: i32) -> TradeBlueprint {
    TradeBlueprint { kind: TradeBlueprintKind::SuspiciousStew, item: "emerald", count: 1, price: 1, result: "suspicious_stew", result_count: 1, max_uses: 12, xp, multiplier: 0.05 }
}
fn tb_dyed(item: &'static str, price: i32, max_uses: i32, xp: i32) -> TradeBlueprint {
    TradeBlueprint { kind: TradeBlueprintKind::DyedArmor, item: "emerald", count: price, price, result: item, result_count: 1, max_uses, xp, multiplier: 0.2 }
}
fn tb_tipped_arrow() -> TradeBlueprint {
    TradeBlueprint { kind: TradeBlueprintKind::TippedArrow, item: "arrow", count: 5, price: 2, result: "tipped_arrow", result_count: 5, max_uses: 12, xp: 30, multiplier: 0.05 }
}

fn trade_cost(item_name: &str, count: i32) -> Option<ItemCost> {
    Some(ItemCost { item: name_to_item(item_name)?, count, components: Vec::new() })
}

/// Instantiate one vanilla trade factory. Enchantment and map NBT generation is
/// owned by the item/component system; this preserves the exact item skeleton,
/// stock, XP, multiplier, second cost, and Java price roll.
fn instantiate_trade_blueprint(bp: TradeBlueprint, roll: u32) -> Option<MerchantOffer> {
    let emerald = name_to_item("emerald")?;
    let (cost_a_item, mut cost_a_count, second, result_name, result_count) = match bp.kind {
        TradeBlueprintKind::Buy => (bp.item, bp.count, None, "emerald", bp.price),
        TradeBlueprintKind::Sell | TradeBlueprintKind::DyedArmor | TradeBlueprintKind::SuspiciousStew => {
            ("emerald", bp.price, None, bp.result, bp.result_count)
        }
        TradeBlueprintKind::Process => ("emerald", bp.price, Some((bp.item, bp.count)), bp.result, bp.result_count),
        TradeBlueprintKind::EnchantedTool => {
            let enchantment_cost = 5 + (roll % 15) as i32;
            ("emerald", (bp.price + enchantment_cost).min(64), None, bp.result, 1)
        }
        TradeBlueprintKind::EnchantedBook => {
            let level = 1 + (roll % 5) as i32;
            let price = (2 + ((roll / 5) as i32 % (5 + level * 10)) + 3 * level).min(64);
            ("emerald", price, Some(("book", 1)), "enchanted_book", 1)
        }
        TradeBlueprintKind::Map => ("emerald", bp.price, Some(("compass", 1)), bp.result, 1),
        TradeBlueprintKind::TippedArrow => ("emerald", bp.price, Some(("arrow", 5)), "tipped_arrow", 5),
    };
    if matches!(bp.kind, TradeBlueprintKind::Buy) {
        cost_a_count = bp.count;
    }
    let result_item = if result_name == "filled_map" {
        name_to_item("filled_map").or_else(|| name_to_item("map"))?
    } else {
        name_to_item(result_name)?
    };
    let _ = emerald;
    Some(MerchantOffer {
        cost_a: trade_cost(cost_a_item, cost_a_count)?,
        cost_b: second.and_then(|(name, count)| trade_cost(name, count)),
        result: (result_item, result_count.clamp(1, 255) as u8, Vec::new()),
        uses: 0,
        max_uses: bp.max_uses,
        xp_reward: bp.xp,
        price_multiplier: bp.multiplier,
        demand: 0,
        special_price_diff: 0,
    })
}

fn sell_each(items: &[&'static str], price: i32, count: i32, max_uses: i32, xp: i32, multiplier: f32) -> Vec<TradeBlueprint> {
    items.iter().map(|&item| tb_sell(item, price, count, max_uses, xp, multiplier)).collect()
}

const WOOL_COLORS: [&str; 16] = ["white_wool","orange_wool","magenta_wool","light_blue_wool","yellow_wool","lime_wool","pink_wool","gray_wool","light_gray_wool","cyan_wool","purple_wool","blue_wool","brown_wool","green_wool","red_wool","black_wool"];
const CARPET_COLORS: [&str; 16] = ["white_carpet","orange_carpet","magenta_carpet","light_blue_carpet","yellow_carpet","lime_carpet","pink_carpet","gray_carpet","light_gray_carpet","cyan_carpet","purple_carpet","blue_carpet","brown_carpet","green_carpet","red_carpet","black_carpet"];
const BED_COLORS: [&str; 16] = ["white_bed","orange_bed","magenta_bed","light_blue_bed","yellow_bed","lime_bed","pink_bed","gray_bed","light_gray_bed","cyan_bed","purple_bed","blue_bed","brown_bed","green_bed","red_bed","black_bed"];
const BANNER_COLORS: [&str; 16] = ["white_banner","orange_banner","magenta_banner","light_blue_banner","yellow_banner","lime_banner","pink_banner","gray_banner","light_gray_banner","cyan_banner","purple_banner","blue_banner","brown_banner","green_banner","red_banner","black_banner"];
const TERRACOTTA_COLORS: [&str; 16] = ["white_terracotta","orange_terracotta","magenta_terracotta","light_blue_terracotta","yellow_terracotta","lime_terracotta","pink_terracotta","gray_terracotta","light_gray_terracotta","cyan_terracotta","purple_terracotta","blue_terracotta","brown_terracotta","green_terracotta","red_terracotta","black_terracotta"];
const GLAZED_TERRACOTTA_COLORS: [&str; 16] = ["white_glazed_terracotta","orange_glazed_terracotta","magenta_glazed_terracotta","light_blue_glazed_terracotta","yellow_glazed_terracotta","lime_glazed_terracotta","pink_glazed_terracotta","gray_glazed_terracotta","light_gray_glazed_terracotta","cyan_glazed_terracotta","purple_glazed_terracotta","blue_glazed_terracotta","brown_glazed_terracotta","green_glazed_terracotta","red_glazed_terracotta","black_glazed_terracotta"];

/// The complete non-experimental Java 1.21.1 `VillagerTrades.TRADES` pools.
/// Vanilla randomly chooses up to two entries from the level pool when the
/// level is unlocked; this table intentionally contains every candidate.
fn vanilla_trade_pool(profession: VillagerProfession, level: VillagerLevel) -> Vec<TradeBlueprint> {
    use VillagerLevel::*;
    use VillagerProfession::*;
    let mut v: Vec<TradeBlueprint> = Vec::new();
    match (profession, level) {
        (Farmer, Novice) => v.extend([tb_buy("wheat",20,16,2),tb_buy("potato",26,16,2),tb_buy("carrot",22,16,2),tb_buy("beetroot",15,16,2),tb_sell("bread",1,6,16,1,0.05)]),
        (Farmer, Apprentice) => v.extend([tb_buy("pumpkin",6,12,10),tb_sell("pumpkin_pie",1,4,12,5,0.05),tb_sell("apple",1,4,16,5,0.05)]),
        (Farmer, Journeyman) => v.extend([tb_sell("cookie",3,18,12,10,0.05),tb_buy("melon",4,12,20)]),
        (Farmer, Expert) => { v.push(tb_sell("cake",1,1,12,15,0.05)); v.extend([100,160,140,120,280,7].map(|_| tb_stew(15))); },
        (Farmer, Master) => v.extend([tb_sell("golden_carrot",3,3,12,30,0.05),tb_sell("glistering_melon_slice",4,3,12,30,0.05)]),
        (Fisherman, Novice) => v.extend([tb_buy("string",20,16,2),tb_buy("coal",10,16,2),tb_process("cod",6,1,"cooked_cod",6,16,1),tb_sell("cod_bucket",3,1,16,1,0.05)]),
        (Fisherman, Apprentice) => v.extend([tb_buy("cod",15,16,10),tb_process("salmon",6,1,"cooked_salmon",6,16,5),tb_sell("campfire",2,1,12,5,0.05)]),
        (Fisherman, Journeyman) => v.extend([tb_buy("salmon",13,16,20),tb_enchanted("fishing_rod",3,3,10,0.2)]),
        (Fisherman, Expert) => v.push(tb_buy("tropical_fish",6,12,30)),
        (Fisherman, Master) => v.extend([tb_buy("pufferfish",4,12,30),tb_buy("oak_boat",1,12,30)]),
        (Shepherd, Novice) => v.extend([tb_buy("white_wool",18,16,2),tb_buy("brown_wool",18,16,2),tb_buy("black_wool",18,16,2),tb_buy("gray_wool",18,16,2),tb_sell("shears",2,1,12,1,0.05)]),
        (Shepherd, Apprentice) => { v.extend(["white_dye","gray_dye","black_dye","light_blue_dye","lime_dye"].map(|x| tb_buy(x,12,16,10))); v.extend(sell_each(&WOOL_COLORS,1,1,16,5,0.05)); v.extend(sell_each(&CARPET_COLORS,1,4,16,5,0.05)); },
        (Shepherd, Journeyman) => { v.extend(["yellow_dye","light_gray_dye","orange_dye","red_dye","pink_dye"].map(|x| tb_buy(x,12,16,20))); v.extend(sell_each(&BED_COLORS,3,1,12,10,0.05)); },
        (Shepherd, Expert) => { v.extend(["brown_dye","purple_dye","blue_dye","green_dye","magenta_dye","cyan_dye"].map(|x| tb_buy(x,12,16,30))); v.extend(sell_each(&BANNER_COLORS,3,1,12,15,0.05)); },
        (Shepherd, Master) => v.push(tb_sell("painting",2,3,12,30,0.05)),
        (Fletcher, Novice) => v.extend([tb_buy("stick",32,16,2),tb_sell("arrow",1,16,12,1,0.05),tb_process("gravel",10,1,"flint",10,12,1)]),
        (Fletcher, Apprentice) => v.extend([tb_buy("flint",26,12,10),tb_sell("bow",2,1,12,5,0.05)]),
        (Fletcher, Journeyman) => v.extend([tb_buy("string",14,16,20),tb_sell("crossbow",3,1,12,10,0.05)]),
        (Fletcher, Expert) => v.extend([tb_buy("feather",24,16,30),tb_enchanted("bow",2,3,15,0.05)]),
        (Fletcher, Master) => v.extend([tb_buy("tripwire_hook",8,12,30),tb_enchanted("crossbow",3,3,15,0.05),tb_tipped_arrow()]),
        (Librarian, Novice) => v.extend([tb_buy("paper",24,16,2),tb_book(1),tb_sell("bookshelf",9,1,12,1,0.05)]),
        (Librarian, Apprentice) => v.extend([tb_buy("book",4,12,10),tb_book(5),tb_sell("lantern",1,1,12,5,0.05)]),
        (Librarian, Journeyman) => v.extend([tb_buy("ink_sac",5,12,20),tb_book(10),tb_sell("glass",1,4,12,10,0.05)]),
        (Librarian, Expert) => v.extend([tb_buy("writable_book",2,12,30),tb_book(15),tb_sell("clock",5,1,12,15,0.05),tb_sell("compass",4,1,12,15,0.05)]),
        (Librarian, Master) => v.push(tb_sell("name_tag",20,1,12,30,0.05)),
        (Cartographer, Novice) => v.extend([tb_buy("paper",24,16,2),tb_sell("map",7,1,12,1,0.05)]),
        (Cartographer, Apprentice) => v.extend([tb_buy("glass_pane",11,16,10),tb_map(13,12,5)]),
        (Cartographer, Journeyman) => v.extend([tb_buy("compass",1,12,20),tb_map(14,12,10),tb_map(12,12,10)]),
        (Cartographer, Expert) => { v.push(tb_sell("item_frame",7,1,12,15,0.05)); v.extend(sell_each(&BANNER_COLORS,3,1,12,15,0.05)); },
        (Cartographer, Master) => v.push(tb_sell("globe_banner_pattern",8,1,12,30,0.05)),
        (Cleric, Novice) => v.extend([tb_buy("rotten_flesh",32,16,2),tb_sell("redstone",1,2,12,1,0.05)]),
        (Cleric, Apprentice) => v.extend([tb_buy("gold_ingot",3,12,10),tb_sell("lapis_lazuli",1,1,12,5,0.05)]),
        (Cleric, Journeyman) => v.extend([tb_buy("rabbit_foot",2,12,20),tb_sell("glowstone",4,1,12,10,0.05)]),
        (Cleric, Expert) => v.extend([tb_buy("turtle_scute",4,12,30),tb_buy("glass_bottle",9,12,30),tb_sell("ender_pearl",5,1,12,15,0.05)]),
        (Cleric, Master) => v.extend([tb_buy("nether_wart",22,12,30),tb_sell("experience_bottle",3,1,12,30,0.05)]),
        (Armorer, Novice) => v.extend([tb_buy("coal",15,16,2),tb_sell("iron_leggings",7,1,12,1,0.2),tb_sell("iron_boots",4,1,12,1,0.2),tb_sell("iron_helmet",5,1,12,1,0.2),tb_sell("iron_chestplate",9,1,12,1,0.2)]),
        (Armorer, Apprentice) => v.extend([tb_buy("iron_ingot",4,12,10),tb_sell("bell",36,1,12,5,0.2),tb_sell("chainmail_boots",1,1,12,5,0.2),tb_sell("chainmail_leggings",3,1,12,5,0.2)]),
        (Armorer, Journeyman) => v.extend([tb_buy("lava_bucket",1,12,20),tb_buy("diamond",1,12,20),tb_sell("chainmail_helmet",1,1,12,10,0.2),tb_sell("chainmail_chestplate",4,1,12,10,0.2),tb_sell("shield",5,1,12,10,0.2)]),
        (Armorer, Expert) => v.extend([tb_enchanted("diamond_leggings",14,3,15,0.2),tb_enchanted("diamond_boots",8,3,15,0.2)]),
        (Armorer, Master) => v.extend([tb_enchanted("diamond_helmet",8,3,30,0.2),tb_enchanted("diamond_chestplate",16,3,30,0.2)]),
        (Weaponsmith, Novice) => v.extend([tb_buy("coal",15,16,2),tb_sell("iron_axe",3,1,12,1,0.2),tb_enchanted("iron_sword",2,3,1,0.05)]),
        (Weaponsmith, Apprentice) => v.extend([tb_buy("iron_ingot",4,12,10),tb_sell("bell",36,1,12,5,0.2)]),
        (Weaponsmith, Journeyman) => v.push(tb_buy("flint",24,12,20)),
        (Weaponsmith, Expert) => v.extend([tb_buy("diamond",1,12,30),tb_enchanted("diamond_axe",12,3,15,0.2)]),
        (Weaponsmith, Master) => v.push(tb_enchanted("diamond_sword",8,3,30,0.2)),
        (Toolsmith, Novice) => v.extend([tb_buy("coal",15,16,2),tb_sell("stone_axe",1,1,12,1,0.2),tb_sell("stone_shovel",1,1,12,1,0.2),tb_sell("stone_pickaxe",1,1,12,1,0.2),tb_sell("stone_hoe",1,1,12,1,0.2)]),
        (Toolsmith, Apprentice) => v.extend([tb_buy("iron_ingot",4,12,10),tb_sell("bell",36,1,12,5,0.2)]),
        (Toolsmith, Journeyman) => v.extend([tb_buy("flint",30,12,20),tb_enchanted("iron_axe",1,3,10,0.2),tb_enchanted("iron_shovel",2,3,10,0.2),tb_enchanted("iron_pickaxe",3,3,10,0.2),tb_sell("diamond_hoe",4,1,3,10,0.2)]),
        (Toolsmith, Expert) => v.extend([tb_buy("diamond",1,12,30),tb_enchanted("diamond_axe",12,3,15,0.2),tb_enchanted("diamond_shovel",5,3,15,0.2)]),
        (Toolsmith, Master) => v.push(tb_enchanted("diamond_pickaxe",13,3,30,0.2)),
        (Butcher, Novice) => v.extend([tb_buy("chicken",14,16,2),tb_buy("porkchop",7,16,2),tb_buy("rabbit",4,16,2),tb_sell("rabbit_stew",1,1,12,1,0.05)]),
        (Butcher, Apprentice) => v.extend([tb_buy("coal",15,16,2),tb_sell("cooked_porkchop",1,5,16,5,0.05),tb_sell("cooked_chicken",1,8,16,5,0.05)]),
        (Butcher, Journeyman) => v.extend([tb_buy("mutton",7,16,20),tb_buy("beef",10,16,20)]),
        (Butcher, Expert) => v.push(tb_buy("dried_kelp_block",10,12,30)),
        (Butcher, Master) => v.push(tb_buy("sweet_berries",10,12,30)),
        (Leatherworker, Novice) => v.extend([tb_buy("leather",6,16,2),tb_dyed("leather_leggings",3,12,1),tb_dyed("leather_chestplate",7,12,1)]),
        (Leatherworker, Apprentice) => v.extend([tb_buy("flint",26,12,10),tb_dyed("leather_helmet",5,12,5),tb_dyed("leather_boots",4,12,5)]),
        (Leatherworker, Journeyman) => v.extend([tb_buy("rabbit_hide",9,12,20),tb_dyed("leather_chestplate",7,12,10)]),
        (Leatherworker, Expert) => v.extend([tb_buy("turtle_scute",4,12,30),tb_dyed("leather_horse_armor",6,12,15)]),
        (Leatherworker, Master) => v.extend([tb_sell("saddle",6,1,12,30,0.2),tb_dyed("leather_helmet",5,12,30)]),
        (Mason, Novice) => v.extend([tb_buy("clay_ball",10,16,2),tb_sell("brick",1,10,16,1,0.05)]),
        (Mason, Apprentice) => v.extend([tb_buy("stone",20,16,10),tb_sell("chiseled_stone_bricks",1,4,16,5,0.05)]),
        (Mason, Journeyman) => v.extend([tb_buy("granite",16,16,20),tb_buy("andesite",16,16,20),tb_buy("diorite",16,16,20),tb_sell("dripstone_block",1,4,16,10,0.05),tb_sell("polished_andesite",1,4,16,10,0.05),tb_sell("polished_diorite",1,4,16,10,0.05),tb_sell("polished_granite",1,4,16,10,0.05)]),
        (Mason, Expert) => { v.push(tb_buy("quartz",12,12,30)); v.extend(sell_each(&TERRACOTTA_COLORS,1,1,12,15,0.05)); v.extend(sell_each(&GLAZED_TERRACOTTA_COLORS,1,1,12,15,0.05)); },
        (Mason, Master) => v.extend([tb_sell("quartz_pillar",1,1,12,30,0.05),tb_sell("quartz_block",1,1,12,30,0.05)]),
        (None | Nitwit, _) => {},
    }
    v
}

fn instantiate_trade_pool(profession: VillagerProfession, level: VillagerLevel, mut roll: impl FnMut() -> u32) -> Vec<MerchantOffer> {
    vanilla_trade_pool(profession, level)
        .into_iter()
        .filter_map(|bp| instantiate_trade_blueprint(bp, roll()))
        .collect()
}

/// Profession assignment opens the novice pool. Higher tiers are appended by
/// `unlock_villager_trade_level` as XP crosses the vanilla thresholds.
fn profession_offers(profession: VillagerProfession) -> Vec<MerchantOffer> {
    instantiate_trade_pool(profession, VillagerLevel::Novice, || world_rng::next_below(u32::MAX))
}

fn unlock_villager_trade_level(villager: &mut Villager, level: VillagerLevel) {
    if matches!(villager.profession, VillagerProfession::None | VillagerProfession::Nitwit) {
        return;
    }
    villager.offers.extend(instantiate_trade_pool(villager.profession, level, || {
        world_rng::next_below(u32::MAX)
    }));
}

/// Wandering trader pools: five common entries plus one rare entry in vanilla.
/// The rare pool is encoded after the separator comment in `wandering_trader_pool`.
fn wandering_trader_pool(rare: bool) -> Vec<TradeBlueprint> {
    if rare {
        return vec![
            tb_sell("tropical_fish_bucket",5,1,4,1,0.05),tb_sell("pufferfish_bucket",5,1,4,1,0.05),
            tb_sell("packed_ice",3,1,6,1,0.05),tb_sell("blue_ice",6,1,6,1,0.05),
            tb_sell("gunpowder",1,1,8,1,0.05),tb_sell("podzol",3,3,6,1,0.05),
        ];
    }
    let mut v = vec![
        tb_sell("sea_pickle",2,1,5,1,0.05),tb_sell("slime_ball",4,1,5,1,0.05),tb_sell("glowstone",2,1,5,1,0.05),
        tb_sell("nautilus_shell",5,1,5,1,0.05),tb_sell("fern",1,1,12,1,0.05),tb_sell("sugar_cane",1,1,8,1,0.05),
        tb_sell("pumpkin",1,1,4,1,0.05),tb_sell("kelp",3,1,12,1,0.05),tb_sell("cactus",3,1,8,1,0.05),
        tb_sell("dandelion",1,1,12,1,0.05),tb_sell("poppy",1,1,12,1,0.05),tb_sell("blue_orchid",1,1,8,1,0.05),
        tb_sell("allium",1,1,12,1,0.05),tb_sell("azure_bluet",1,1,12,1,0.05),tb_sell("red_tulip",1,1,12,1,0.05),
        tb_sell("orange_tulip",1,1,12,1,0.05),tb_sell("white_tulip",1,1,12,1,0.05),tb_sell("pink_tulip",1,1,12,1,0.05),
        tb_sell("oxeye_daisy",1,1,12,1,0.05),tb_sell("cornflower",1,1,12,1,0.05),tb_sell("lily_of_the_valley",1,1,7,1,0.05),
        tb_sell("wheat_seeds",1,1,12,1,0.05),tb_sell("beetroot_seeds",1,1,12,1,0.05),tb_sell("pumpkin_seeds",1,1,12,1,0.05),
        tb_sell("melon_seeds",1,1,12,1,0.05),tb_sell("acacia_sapling",5,1,8,1,0.05),tb_sell("birch_sapling",5,1,8,1,0.05),
        tb_sell("dark_oak_sapling",5,1,8,1,0.05),tb_sell("jungle_sapling",5,1,8,1,0.05),tb_sell("oak_sapling",5,1,8,1,0.05),
        tb_sell("spruce_sapling",5,1,8,1,0.05),tb_sell("cherry_sapling",5,1,8,1,0.05),tb_sell("mangrove_propagule",5,1,8,1,0.05),
    ];
    v.extend(sell_each(&["red_dye","white_dye","blue_dye","pink_dye","black_dye","green_dye","light_gray_dye","magenta_dye","yellow_dye","gray_dye","purple_dye","light_blue_dye","lime_dye","orange_dye","brown_dye","cyan_dye"],1,3,12,1,0.05));
    v.extend(sell_each(&["brain_coral_block","bubble_coral_block","fire_coral_block","horn_coral_block","tube_coral_block"],3,1,8,1,0.05));
    v.extend(sell_each(&["vine","brown_mushroom","red_mushroom"],1,1,12,1,0.05));
    v.extend([tb_sell("lily_pad",1,2,5,1,0.05),tb_sell("small_dripleaf",1,2,5,1,0.05),tb_sell("sand",1,8,8,1,0.05),tb_sell("red_sand",1,4,6,1,0.05),tb_sell("pointed_dripstone",1,2,5,1,0.05),tb_sell("rooted_dirt",1,2,5,1,0.05),tb_sell("moss_block",1,2,5,1,0.05)]);
    v
}

fn wandering_trader_offers(mut roll: impl FnMut(usize) -> usize) -> Vec<MerchantOffer> {
    let mut common = wandering_trader_pool(false);
    let mut picked = Vec::new();
    for _ in 0..5.min(common.len()) {
        if common.is_empty() { break; }
        let index = roll(common.len());
        if let Some(bp) = common.get(index).copied() {
            if let Some(offer) = instantiate_trade_blueprint(bp, index as u32) { picked.push(offer); }
        }
        common.remove(index);
    }
    let rare = wandering_trader_pool(true);
    if !rare.is_empty() {
        let index = roll(rare.len());
        if let Some(offer) = instantiate_trade_blueprint(rare[index], index as u32) { picked.push(offer); }
    }
    picked
}

/// Scan around a live villager, claim a free workstation (assigning its
/// profession and trade table) and pick up an unclaimed bed or the village
/// bell so the village exists for raid logic. Called on the rescan cadence
/// and before a merchant opens.
#[cfg_attr(not(test), allow(dead_code))]
fn live_villager_bind_pois(dim: DimId, position: BlockPos, villager: &mut Villager) {
    live_poi_scan(dim, position);
    let mut registries = live_pois().lock().unwrap_or_else(|e| e.into_inner());
    let pois = registries.entry(dim).or_default();
    let mut nearby: Vec<(BlockPos, PoiKind)> = pois
        .records
        .iter()
        .map(|(&pos, poi)| (pos, poi.kind))
        .filter(|(pos, _)| villager_within(position, *pos, LIVE_POI_SCAN_RADIUS as u16))
        .collect();
    nearby.sort_by_key(|(pos, _)| {
        let [dx, dy, dz] = [pos.0 - position.0, pos.1 - position.1, pos.2 - position.2];
        dx * dx + dy * dy + dz * dz
    });
    for (pos, kind) in nearby {
        match kind {
            PoiKind::JobSite(_) => {
                if villager.job_site.is_none() {
                    villager.claim_poi(pois, pos);
                }
            }
            PoiKind::Bed if villager.home.is_none() => {
                villager.claim_poi(pois, pos);
            }
            PoiKind::Bell if villager.meeting_point.is_none() => {
                villager.claim_poi(pois, pos);
            }
            _ => {}
        }
    }
    villager.refresh_pois(pois);
    if villager.offers.is_empty() {
        villager.offers = profession_offers(villager.profession);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct VillagerFamilyState {
    age_ticks: i32,
    food_points: i32,
    gift_cooldown: u32,
}

const MAX_LIVE_VILLAGER_REGISTRY_ENTRIES: usize = 4_096;

fn live_villager_family_states() -> &'static LockedMap<i32, VillagerFamilyState> {
    static STATES: Registry<i32, VillagerFamilyState> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn live_zombie_villager_cures() -> &'static LockedMap<i32, ZombieVillagerCure> {
    static CURES: Registry<i32, ZombieVillagerCure> = OnceLock::new();
    CURES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn live_wandering_traders() -> &'static LockedMap<i32, (u32, Vec<MerchantOffer>)> {
    static TRADERS: Registry<i32, (u32, Vec<MerchantOffer>)> = OnceLock::new();
    TRADERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn live_zombie_villager_data() -> &'static LockedMap<i32, (VillagerProfession, i32, Vec<MerchantOffer>)> {
    static DATA: Registry<i32, (VillagerProfession, i32, Vec<MerchantOffer>)> = OnceLock::new();
    DATA.get_or_init(|| Mutex::new(HashMap::new()))
}

fn live_converted_villagers() -> &'static Mutex<HashSet<i32>> {
    static CONVERTED: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    CONVERTED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn live_raid_captains() -> &'static LockedMap<i32, (DimId, BlockPos)> {
    static CAPTAINS: Registry<i32, (DimId, BlockPos)> = OnceLock::new();
    CAPTAINS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Emergency bound for entity-keyed lane registries. Normal lifecycle code
/// removes entries when their entity or raid ends; this only protects against
/// an unexpected leak and keeps every global registry finite.
fn evict_live_villager_registry_overflow<V>(map: &mut HashMap<i32, V>) {
    if map.len() <= MAX_LIVE_VILLAGER_REGISTRY_ENTRIES {
        return;
    }
    let keep: HashSet<i32> = map.keys().take(MAX_LIVE_VILLAGER_REGISTRY_ENTRIES).copied().collect();
    map.retain(|eid, _| keep.contains(eid));
}

fn live_raid_glowing() -> &'static LockedMap<i32, u64> {
    static GLOWING: Registry<i32, u64> = OnceLock::new();
    GLOWING.get_or_init(|| Mutex::new(HashMap::new()))
}


/// Java 1.21.1 hero-of-the-village gift loot pools. Baby villagers throw a
/// poppy; nitwits throw nothing. Every entry in a profession pool is equally
/// weighted in the vanilla loot table.
fn hero_gift_pool(profession: VillagerProfession, baby: bool) -> &'static [&'static str] {
    if baby {
        return &["poppy"];
    }
    match profession {
        VillagerProfession::None => &["wheat_seeds"],
        VillagerProfession::Nitwit => &[],
        VillagerProfession::Armorer => &["chainmail_helmet", "chainmail_chestplate", "chainmail_leggings", "chainmail_boots"],
        VillagerProfession::Butcher => &["cooked_chicken", "cooked_mutton", "cooked_porkchop", "cooked_rabbit", "cooked_beef"],
        VillagerProfession::Cartographer => &["map", "paper"],
        VillagerProfession::Cleric => &["lapis_lazuli", "redstone"],
        VillagerProfession::Farmer => &["bread", "cookie", "pumpkin_pie"],
        VillagerProfession::Fisherman => &["cod", "salmon"],
        VillagerProfession::Fletcher => &["arrow", "tipped_arrow"],
        VillagerProfession::Leatherworker => &["leather"],
        VillagerProfession::Librarian => &["book"],
        VillagerProfession::Mason => &["clay_ball"],
        VillagerProfession::Shepherd => &WOOL_COLORS,
        VillagerProfession::Toolsmith => &["stone_axe", "stone_hoe", "stone_pickaxe", "stone_shovel"],
        VillagerProfession::Weaponsmith => &["stone_axe", "golden_axe", "iron_axe"],
    }
}

fn hero_gift_item(profession: VillagerProfession, baby: bool, roll: u32) -> Option<&'static str> {
    let pool = hero_gift_pool(profession, baby);
    if pool.is_empty() {
        None
    } else {
        Some(pool[(roll as usize) % pool.len()])
    }
}

/// Vanilla rerolls the next gift uniformly in 600..=6600 ticks (30 seconds
/// through 5 minutes 30 seconds).
fn hero_gift_cooldown(roll: u32) -> u32 {
    600 + roll % 6_001
}

/// Villager schedule work: prune records whose mob died, re-scan POI bindings
/// on the slow cadence, and advance each villager's activity/restock tick.
/// Called once per game tick from [`villager_raid_tick`].
#[cfg_attr(not(test), allow(dead_code))]
fn live_villager_tick(tick: u64) {
    let villager_etype = name_to_etype("villager");
    let live: Vec<(i32, [u8; 16], DimId, BlockPos)> = mobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|mob| {
            Some(mob.etype) == villager_etype && *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0
        })
        .map(|mob| {
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            (
                mob.eid,
                mob.uuid,
                mob.dim,
                (x.floor() as i32, y.floor() as i32, z.floor() as i32),
            )
        })
        .collect();
    let mut villagers = live_villagers().lock().unwrap_or_else(|e| e.into_inner());

    // A villager killed by a zombie zombifies at the difficulty-dependent
    // vanilla rate. Preserve its profession, XP and offers for the later cure.
    let zombie_etype = name_to_etype("zombie");
    let zombie_villager_etype = name_to_etype("zombie_villager");
    let mob_snapshots: Vec<(i32, i64, DimId, BlockPos, bool)> = mobs().lock().unwrap_or_else(|e| e.into_inner()).iter()
        .map(|mob| {
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            (mob.eid, mob.etype, mob.dim, (x.floor() as i32, y.floor() as i32, z.floor() as i32), *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0)
        }).collect();
    for &(eid, etype, dim, position, alive) in &mob_snapshots {
        if alive || Some(etype) != villager_etype || live_converted_villagers().lock().unwrap_or_else(|e| e.into_inner()).contains(&eid) {
            continue;
        }
        live_converted_villagers().lock().unwrap_or_else(|e| e.into_inner()).insert(eid);
        let killer_nearby = mob_snapshots.iter().any(|&(_, ztype, zdim, zpos, zalive)| {
            zalive && Some(ztype) == zombie_etype && zdim == dim && villager_within(zpos, position, 2)
        });
        if !killer_nearby {
            continue;
        }
        let percent = zombie_villager_conversion_percent(raid_difficulty());
        if percent == 0 || world_rng::next_below(100) >= u32::from(percent) {
            continue;
        }
        let (profession, xp, offers) = villagers.get(&eid)
            .map(|villager| (villager.profession, villager.villager_xp, villager.offers.clone()))
            .unwrap_or((VillagerProfession::None, 0, Vec::new()));
        if zombie_villager_etype.is_some() {
            if let Some(zombie) = live_spawn_named(dim, "zombie_villager", (
                position.0 as f64 + 0.5, position.1 as f64, position.2 as f64 + 0.5, 0.0, 0.0,
            )) {
                live_zombie_villager_data().lock().unwrap_or_else(|e| e.into_inner()).insert(zombie.eid, (profession, xp, offers));
            }
        }
    }

    let observed_eids: HashSet<i32> = mob_snapshots.iter().map(|&(eid, ..)| eid).collect();
    {
        let mut converted = live_converted_villagers().lock().unwrap_or_else(|e| e.into_inner());
        converted.retain(|eid| observed_eids.contains(eid));
        if converted.len() > MAX_LIVE_VILLAGER_REGISTRY_ENTRIES {
            converted.clear();
        }
    }

    let live_eids: HashSet<i32> = live.iter().map(|&(eid, ..)| eid).collect();
    // A removed or dead villager releases its POI claims before its record
    // goes; without this a village stays claimed by ghosts.
    let retired_eids: Vec<i32> = villagers
        .keys()
        .filter(|eid| !live_eids.contains(*eid))
        .copied()
        .collect();
    if !retired_eids.is_empty() {
        let mut registries = live_pois().lock().unwrap_or_else(|e| e.into_inner());
        for eid in retired_eids {
            let Some(mut villager) = villagers.remove(&eid) else {
                continue;
            };
            for pois in registries.values_mut() {
                villager.release_pois(pois);
            }
        }
    }
    if live.is_empty() {
        return;
    }
    let world_time = WORLD_TIME.load(Ordering::Relaxed);
    let rescan = tick.is_multiple_of(server_ticks(LIVE_POI_RESCAN_GT));
    let clients = client_snapshot();
    let hero_effect = live_effect_id("hero_of_the_village");
    let mut family = live_villager_family_states().lock().unwrap_or_else(|e| e.into_inner());
    family.retain(|eid, _| live_eids.contains(eid));
    for &(eid, uuid, dim, position) in &live {
        let villager = villagers
            .entry(eid)
            .or_insert_with(|| Villager::new(uuid, VillagerProfession::None));
        if rescan {
            live_villager_bind_pois(dim, position, villager);
        }
        let state = family.entry(eid).or_insert(VillagerFamilyState { age_ticks: 0, food_points: 0, gift_cooldown: 0 });
        let baby = if state.age_ticks < 0 {
            let (age, baby) = villager_baby_age_tick(state.age_ticks);
            state.age_ticks = age;
            baby
        } else { false };
        let panicking = saved_raids().lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(rdim, raid)| {
            *rdim == dim && raid.state == RaidState::InProgress && villager_within(position, raid.center, 64)
        });
        {
            let registries = live_pois().lock().unwrap_or_else(|e| e.into_inner());
            let empty = PoiRegistry::default();
            let pois = registries.get(&dim).unwrap_or(&empty);
            let (activity, _) = villager.tick_brain(pois, position, world_time, baby, panicking);
            if activity == VillagerActivity::Work
                && villager.profession == VillagerProfession::Farmer
            {
                if let Some(site) = villager.job_site {
                    if let Some(reg) = crate::block_registry::breg() {
                        if let Some(points) = farmer_harvest_replant_nearby_crop(reg, dim, site) {
                            state.food_points = (state.food_points + points).min(24);
                        }
                    }
                    farmer_composter_work(dim, position, site, tick);
                }
            }

            state.gift_cooldown = state.gift_cooldown.saturating_sub(1);
            if state.gift_cooldown == 0
                && !matches!(activity, VillagerActivity::Rest | VillagerActivity::Panic)
                && hero_effect.is_some()
            {
                let hero = clients.iter().filter(|client| !client.is_spectator()).find(|client| {
                    let (cdim, x, y, z, ..) = client_dim_pos(client);
                    if cdim != dim {
                        return false;
                    }
                    let close = (x - position.0 as f64).powi(2)
                        + (y - position.1 as f64).powi(2)
                        + (z - position.2 as f64).powi(2) <= 25.0;
                    close && client.effs.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|eff| Some(eff.id) == hero_effect)
                });
                if let Some(hero) = hero {
                    let roll = world_rng::next_below(u32::MAX);
                    if let Some(item_name) = hero_gift_item(villager.profession, baby, roll) {
                        if let Some(item) = name_to_item(item_name) {
                            drop_spawn_in_dim(dim, DropSpawn {
                                x: position.0 as f64 + 0.5,
                                y: position.1 as f64 + 1.0,
                                z: position.2 as f64 + 0.5,
                                item,
                                cnt: 1,
                                comps: Vec::new(),
                                age: 0,
                                target: Some(hero.uuid),
                                thrower: Some(uuid),
                            });
                            state.gift_cooldown = hero_gift_cooldown(roll);
                        }
                    } else {
                        state.gift_cooldown = hero_gift_cooldown(roll);
                    }
                }
            }
        }
    }

    // Food sharing and breeding run on the vanilla once-per-second brain tick.
    if tick.is_multiple_of(20) {
        for i in 0..live.len() {
            for j in i + 1..live.len() {
                let (aeid, _, adim, apos) = live[i];
                let (beid, _, bdim, bpos) = live[j];
                if adim != bdim || !villager_within(apos, bpos, 3) { continue; }
                let (af, bf) = match (family.get(&aeid).copied(), family.get(&beid).copied()) {
                    (Some(a), Some(b)) if a.age_ticks >= 0 && b.age_ticks >= 0 => (a, b),
                    _ => continue,
                };
                let (donor, recipient, donor_eid, recipient_eid) = if af.food_points >= bf.food_points {
                    (af, bf, aeid, beid)
                } else { (bf, af, beid, aeid) };
                let (donor_points, recipient_points, shared) = villager_share_food(donor.food_points, recipient.food_points);
                if shared {
                    family.get_mut(&donor_eid).unwrap().food_points = donor_points;
                    family.get_mut(&recipient_eid).unwrap().food_points = recipient_points;
                }
                let willing = villager_willing(af.food_points.max(donor_points)) && villager_willing(bf.food_points.max(recipient_points));
                if !willing { continue; }
                let can_breed = {
                    let registries = live_pois().lock().unwrap_or_else(|e| e.into_inner());
                    let empty = PoiRegistry::default();
                    let pois = registries.get(&adim).unwrap_or(&empty);
                    villager_can_breed(pois, apos, 48, 2)
                };
                if !can_breed { continue; }
                if let Some(child) = live_spawn_named(adim, "villager", (
                    (apos.0 + bpos.0) as f64 / 2.0 + 0.5,
                    apos.1 as f64,
                    (apos.2 + bpos.2) as f64 / 2.0 + 0.5,
                    0.0, 0.0,
                )) {
                    family.insert(child.eid, VillagerFamilyState { age_ticks: -VILLAGER_BABY_GROWTH_TICKS, food_points: 0, gift_cooldown: 0 });
                    for parent in [aeid, beid] {
                        if let Some(state) = family.get_mut(&parent) { state.food_points = 0; }
                    }
                }
                break;
            }
        }
    }
}

/// Advance zombie-villager curing timers. Completion respawns the villager,
/// restores profession/XP/offers, and records the curer gossip discount.
fn live_zombie_villager_cure_tick() {
    let mut cures = live_zombie_villager_cures().lock().unwrap_or_else(|e| e.into_inner());
    if cures.is_empty() { return; }
    let positions: HashMap<i32, (DimId, BlockPos)> = mobs().lock().unwrap_or_else(|e| e.into_inner()).iter()
        .map(|mob| {
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            (mob.eid, (mob.dim, (x.floor() as i32, y.floor() as i32, z.floor() as i32)))
        }).collect();
    evict_live_villager_registry_overflow(&mut cures);
    cures.retain(|eid, _| positions.contains_key(eid));
    {
        let mut data = live_zombie_villager_data().lock().unwrap_or_else(|e| e.into_inner());
        evict_live_villager_registry_overflow(&mut data);
        data.retain(|eid, _| positions.contains_key(eid));
    }
    let mut finished = Vec::new();
    for (&eid, cure) in cures.iter_mut() {
        let Some(&(dim, pos)) = positions.get(&eid) else { continue };
        let accelerants = zombie_cure_accelerants_in_cube(pos, |block| {
            crate::block_registry::breg().is_some_and(|reg| {
                let name = reg.name_of(block_at_dim(dim, block.0, block.1, block.2));
                name == "minecraft:iron_bars" || name.ends_with("_bed")
            })
        });
        if cure.tick_with_rolls(accelerants, world_rng::next_below(100), world_rng::next_below(100)) {
            finished.push((eid, dim, pos));
        }
    }
    for (eid, dim, pos) in finished {
        let Some(cure) = cures.remove(&eid) else { continue };
        let (curer, profession, xp, offers) = cure.finish();
        if let Some(zombie) = mobs().lock().unwrap_or_else(|e| e.into_inner()).iter().find(|mob| mob.eid == eid) {
            *zombie.hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
        }
        if let Some(mob) = live_spawn_named(dim, "villager", (
            pos.0 as f64 + 0.5, pos.1 as f64, pos.2 as f64 + 0.5, 0.0, 0.0,
        )) {
            let mut villager = Villager::new(mob.uuid, profession);
            villager.villager_xp = xp;
            villager.offers = offers;
            villager.gossip.cure(curer);
            live_villagers().lock().unwrap_or_else(|e| e.into_inner()).insert(mob.eid, villager);
        }
    }
}

/// `FarmerVillagerTask.WorkAtComposter`: during work hours a farmer standing
/// by its claimed composter deposits seeds (the `COMPOSTABLES` chance roll),
/// lets a level-7 composter mature, and at the ready level 8 pops the bone
/// meal and spreads it on a neighbouring crop.
#[cfg_attr(not(test), allow(dead_code))]
fn farmer_composter_work(dim: DimId, position: BlockPos, site: BlockPos, tick: u64) {
    if !tick.is_multiple_of(20) || !villager_within(position, site, 2) {
        return;
    }
    let Some(reg) = crate::block_registry::breg() else {
        return;
    };
    let st = block_at_dim(dim, site.0, site.1, site.2);
    if reg.name_of(st) != "minecraft:composter" {
        return;
    }
    let level: u8 = reg
        .get_prop(st, "level")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let write = |level: u8| {
        let next = reg.with_prop(st, "level", &level.to_string());
        set_block_dim(dim, site.0, site.1, site.2, next);
        note_block_update_dim(dim, site.0, site.1, site.2, next);
    };
    if level < 7 {
        // Farmers carry seeds: use the wheat-seed row of the compostables
        // table with the vanilla chance roll.
        let roll = f64::from(world_rng::next_below(1 << 24)) / f64::from(1 << 24);
        if let ComposterInsert::Advanced { new_level, .. } =
            composter_insert(level, "minecraft:wheat_seeds", roll)
        {
            write(new_level);
        }
    } else if level == 7 {
        write(composter_mature(level));
    } else if let Some((rest, _produce)) = composter_harvest(level) {
        write(rest);
        farmer_bonemeal_nearby_crop(reg, dim, site);
    }
}

/// Harvest one mature crop near a farmer's composter and replant it at age
/// zero. The return value is abstract villager food points: wheat is treated
/// as bread after the farmer's vanilla crafting step.
fn farmer_crop_food_points(name: &str) -> i32 {
    match name.strip_prefix("minecraft:").unwrap_or(name) {
        "wheat" => villager_food_points(1, 0, 0, 0),
        "carrots" => villager_food_points(0, 2, 0, 0),
        "potatoes" => villager_food_points(0, 0, 2, 0),
        "beetroots" => villager_food_points(0, 0, 0, 1),
        _ => 0,
    }
}

fn farmer_crop_max_age(name: &str) -> u8 {
    if name.strip_prefix("minecraft:").unwrap_or(name) == "beetroots" { 3 } else { 7 }
}

fn farmer_harvest_replant_nearby_crop(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    site: BlockPos,
) -> Option<i32> {
    for dy in -1..=1 {
        for dx in -2..=2 {
            for dz in -2..=2 {
                let pos = (site.0 + dx, site.1 + dy, site.2 + dz);
                let st = block_at_dim(dim, pos.0, pos.1, pos.2);
                let name = reg.name_of(st);
                if !matches!(name, "minecraft:wheat" | "minecraft:carrots" | "minecraft:potatoes" | "minecraft:beetroots") {
                    continue;
                }
                let age: u8 = reg.get_prop(st, "age").and_then(|value| value.parse().ok()).unwrap_or(0);
                if age < farmer_crop_max_age(name) {
                    continue;
                }
                let replanted = reg.with_prop(st, "age", "0");
                set_block_dim(dim, pos.0, pos.1, pos.2, replanted);
                note_block_update_dim(dim, pos.0, pos.1, pos.2, replanted);
                return Some(farmer_crop_food_points(name));
            }
        }
    }
    None
}

/// The farmer spends the harvested bone meal on the first growing crop within
/// reach of the composter — the vanilla `UseBonemeal` arm of the farming task.
#[cfg_attr(not(test), allow(dead_code))]
fn farmer_bonemeal_nearby_crop(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    site: BlockPos,
) {
    for dy in -1..=1 {
        for dx in -2..=2 {
            for dz in -2..=2 {
                let pos = (site.0 + dx, site.1 + dy, site.2 + dz);
                let st = block_at_dim(dim, pos.0, pos.1, pos.2);
                if !matches!(
                    reg.name_of(st),
                    "minecraft:wheat"
                        | "minecraft:carrots"
                        | "minecraft:potatoes"
                        | "minecraft:beetroots"
                ) {
                    continue;
                }
                let age: u8 = reg
                    .get_prop(st, "age")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                if age >= 7 {
                    continue;
                }
                let grown = reg.with_prop(st, "age", &(age + 2).min(7).to_string());
                set_block_dim(dim, pos.0, pos.1, pos.2, grown);
                note_block_update_dim(dim, pos.0, pos.1, pos.2, grown);
                return;
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn raid_difficulty() -> RaidDifficulty {
    match DIFFICULTY.load(Ordering::Relaxed) {
        0 => RaidDifficulty::Peaceful,
        1 => RaidDifficulty::Easy,
        3 => RaidDifficulty::Hard,
        _ => RaidDifficulty::Normal,
    }
}

fn live_effect_id(name: &str) -> Option<i32> {
    effects::effect_registry_id(name).and_then(effects::effect_internal_id)
}

/// Push an effect onto a client and mirror it to watchers, the same way
/// `live_mob_player_effect` does for potion contact effects.
#[cfg_attr(not(test), allow(dead_code))]
fn live_grant_effect(client: &Client, id: i32, amp: i32, left: i32) {
    {
        let mut effs = client.effs.lock().unwrap_or_else(|e| e.into_inner());
        if effs
            .iter()
            .any(|eff| eff.id == id && eff.left >= left && eff.amp >= amp)
        {
            return;
        }
        effs.retain(|eff| eff.id != id);
        effs.push(Eff { id, amp, left });
    }
    let packet = entity_effect_pkt(client.eid, id, amp, left);
    if let Ok(mut sock) = client.sock.lock() {
        let _ = send_packet(&mut sock, 0x76, packet.clone());
    }
    broadcast_seen_ex(client.eid, client.eid, |_| (0x76, packet.clone()));
}

#[cfg_attr(not(test), allow(dead_code))]
fn live_remove_effect(client: &Client, id: i32) {
    client.effs.lock().unwrap_or_else(|e| e.into_inner()).retain(|eff| eff.id != id);
    let packet = remove_effect_pkt(client.eid, id);
    if let Ok(mut sock) = client.sock.lock() {
        let _ = send_packet(&mut sock, 0x43, packet.clone());
    }
    broadcast_seen_ex(client.eid, client.eid, |_| (0x43, packet.clone()));
}

/// Bad Omen converts to a raid omen when the carrier steps into a claimed
/// village (a bed or bell with claimants). The omen counts down in game ticks;
/// when it expires `Raid::try_start` either takes the village or drops it.
#[cfg_attr(not(test), allow(dead_code))]
fn live_raid_omen_tick() {
    let clients = client_snapshot();
    if clients.is_empty() {
        return;
    }
    let bad_omen = live_effect_id("bad_omen");
    let raid_omen = live_effect_id("raid_omen");
    let difficulty = raid_difficulty();
    let mut omens = live_raid_omens().lock().unwrap_or_else(|e| e.into_inner());
    for client in clients.iter().filter(|client| !client.is_spectator()) {
        let (dim, x, y, z, ..) = client_dim_pos(client);
        let position = (x.floor() as i32, y.floor() as i32, z.floor() as i32);
        if let Some(omen) = omens.get_mut(&client.uuid) {
            if !omen.tick() {
                continue;
            }
            let mut omen = Some(*omen);
            let raid = {
                let registries = live_pois().lock().unwrap_or_else(|e| e.into_inner());
                registries.get(&dim).and_then(|pois| {
                    Raid::try_start(pois, position, client.uuid, &mut omen, difficulty)
                })
            };
            omens.remove(&client.uuid);
            if let Some(raid_omen) = raid_omen {
                live_remove_effect(client, raid_omen);
            }
            if let Some(raid) = raid {
                live_raid_bossbar_write(dim, raid.center, None, 0, &clients);
                saved_raids().lock().unwrap_or_else(|e| e.into_inner()).push((dim, raid));
            }
            continue;
        }
        let amplifier = {
            let effs = client.effs.lock().unwrap_or_else(|e| e.into_inner());
            effs.iter()
                .find(|eff| Some(eff.id) == bad_omen)
                .map(|eff| eff.amp)
        };
        let Some(amplifier) = amplifier else {
            continue;
        };
        let in_village = {
            let registries = live_pois().lock().unwrap_or_else(|e| e.into_inner());
            registries.get(&dim).is_some_and(|pois| {
                pois.claimed_near(position)
                    .any(|(_, kind)| matches!(kind, PoiKind::Bed | PoiKind::Bell))
            })
        };
        if !in_village {
            continue;
        }
        let bad_omen_level = (amplifier + 1).clamp(1, 5) as u8;
        omens.insert(
            client.uuid,
            BadOmen {
                level: bad_omen_level,
                duration_ticks: BAD_OMEN_TICKS,
            }
            .enter_village(),
        );
        if let (Some(bad_omen), Some(raid_omen)) = (bad_omen, raid_omen) {
            live_remove_effect(client, bad_omen);
            live_grant_effect(
                client,
                raid_omen,
                amplifier,
                server_ticks(u64::from(RAID_OMEN_TICKS)) as i32,
            );
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn live_raid_bar_id(dim: DimId, center: BlockPos) -> String {
    format!(
        "megu:raid-{}-{}-{}-{}",
        dim.raw(),
        center.0,
        center.1,
        center.2
    )
}

/// Audience for a raid bossbar: same-dimension players inside the vanilla
/// 96-block raid tracking range.
#[cfg_attr(not(test), allow(dead_code))]
fn live_raid_audience(
    clients: &[Arc<Client>],
    dim: DimId,
    center: BlockPos,
) -> Vec<&Arc<Client>> {
    clients
        .iter()
        .filter(|client| {
            let (cdim, x, y, z, ..) = client_dim_pos(client);
            cdim == dim
                && villager_within(
                    (x.floor() as i32, y.floor() as i32, z.floor() as i32),
                    center,
                    96,
                )
        })
        .collect()
}

/// Write one bossbar action for a raid: 0 add, 1 remove, 2 progress update.
/// `raid` supplies the remaining/total raider counts for the progress bar.
#[cfg_attr(not(test), allow(dead_code))]
fn live_raid_bossbar_write(
    dim: DimId,
    center: BlockPos,
    raid: Option<&Raid>,
    action: u8,
    clients: &[Arc<Client>],
) {
    let id = live_raid_bar_id(dim, center);
    let audience = live_raid_audience(clients, dim, center);
    if action == 1 {
        let bar = bossbars().lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        let pkt = bossbar_pkt(&id, 1, bar.as_ref());
        for client in audience {
            if let Ok(mut sock) = client.sock.lock() {
                let _ = send_packet(&mut sock, 0x0a, pkt.clone());
            }
        }
        return;
    }
    let total = live_raid_wave_totals()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(dim, center))
        .copied()
        .unwrap_or(1)
        .max(1);
    let remaining = raid.map_or(0, |raid| raid.remaining.len() as i32);
    let mut bars = bossbars().lock().unwrap_or_else(|e| e.into_inner());
    let bar = bars.entry(id.clone()).or_insert_with(|| Bossbar {
        name: "Raid".into(),
        color: 1,
        style: 0,
        value: total,
        max: total,
        visible: true,
        players: HashSet::new(),
    });
    bar.players = audience.iter().map(|client| client.uuid).collect();
    if action == 0 {
        bar.max = total.max(1);
        bar.value = bar.max;
        let pkt = bossbar_pkt(&id, 0, Some(bar));
        for client in audience {
            if let Ok(mut sock) = client.sock.lock() {
                let _ = send_packet(&mut sock, 0x0a, pkt.clone());
            }
        }
    } else {
        bar.max = total.max(1);
        bar.value = remaining;
        let pkt = bossbar_pkt(&id, 2, Some(bar));
        for client in audience {
            if let Ok(mut sock) = client.sock.lock() {
                let _ = send_packet(&mut sock, 0x0a, pkt.clone());
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn raider_entity_name(kind: RaiderType) -> &'static str {
    match kind {
        RaiderType::Vindicator => "vindicator",
        RaiderType::Evoker => "evoker",
        RaiderType::Pillager => "pillager",
        RaiderType::Witch => "witch",
        RaiderType::Ravager => "ravager",
    }
}

/// Spread one wave's raiders on a ring outside the village center. Vanilla
/// hunts down from the village's surface for each slot; a rotated ring on the
/// terrain surface is the bounded version of that search.
#[cfg_attr(not(test), allow(dead_code))]
fn live_raid_spawn_wave(dim: DimId, raid: &Raid, mut wave: RaidWave) {
    let bonus_wave = wave.number > raid.regular_waves;
    let riders = raid_wave_adjustments(
        raid_difficulty(), wave.number, bonus_wave, &mut wave.counts,
        world_rng::next_below(u32::MAX), world_rng::next_below(u32::MAX),
    );
    for rider in &riders {
        wave.counts[rider.rider as usize] += 1;
    }
    let total = wave.raiders().count().max(1);
    let mut first_raider_eid = None;
    let cx = raid.center.0 as f64 + 0.5;
    let cz = raid.center.2 as f64 + 0.5;
    for (index, (raider_id, kind)) in wave.raiders().enumerate() {
        let Some(spec) = entity_spec_by_name(raider_entity_name(kind)) else {
            continue;
        };
        let angle = index as f64 / total as f64 * std::f64::consts::TAU;
        let radius = 24.0 + world_rng::next_below(9) as f64;
        let bx = (cx + angle.cos() * radius).floor() as i32;
        let bz = (cz + angle.sin() * radius).floor() as i32;
        let by = if dim == DimId::Overworld {
            surface_y(bx, bz).floor() as i32
        } else {
            dim.min_y() + 64
        };
        let yaw = world_rng::next_below(360) as f32;
        let spawned = with_save_guard(|| {
            let mut current = mobs().lock().unwrap_or_else(|e| e.into_inner());
            if current.len() >= MOB_CAP {
                return None;
            }
            let mob = new_live_mob(
                spec.id,
                dim,
                (bx as f64 + 0.5, by as f64, bz as f64 + 0.5, yaw, 0.0),
                spec.max_health,
                None,
            );
            push_natural_hostile(&mut current, mob.clone());
            Some(mob)
        });
        if let Some(mob) = spawned {
            if first_raider_eid.is_none() {
                first_raider_eid = Some(mob.eid);
            }
            if wave.captain == Some(raider_id) {
                live_raid_captains().lock().unwrap_or_else(|e| e.into_inner()).insert(mob.eid, (dim, raid.center));
            }
            live_raid_mobs()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(mob.eid, (dim, raid.center, raider_id));
        }
    }
    if let Some(eid) = first_raider_eid {
        broadcast_entity_sound(eid, "minecraft:event.raid.horn", 6);
    }
    live_raid_wave_totals()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((dim, raid.center), total as i32);
}

const RAID_BELL_GLOWING_TICKS: u64 = 60;
const RAID_BELL_GLOWING_RADIUS: i32 = 48;

fn bell_glowing_range_contains(bell: BlockPos, pos: BlockPos) -> bool {
    let dx = i64::from(pos.0 - bell.0);
    let dy = i64::from(pos.1 - bell.1);
    let dz = i64::from(pos.2 - bell.2);
    dx * dx + dy * dy + dz * dz <= i64::from(RAID_BELL_GLOWING_RADIUS).pow(2)
}

/// Consume one ringing edge from every powered bell in a dimension. The
/// wind-charge trigger path represents a ring by setting the bell's `powered`
/// state; resetting it here makes the edge one-shot instead of a permanent
/// glowing aura.
#[cfg_attr(not(test), allow(dead_code))]
fn live_consume_powered_bells(dim: DimId) -> Vec<BlockPos> {
    let positions: Vec<BlockPos> = live_pois()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&dim)
        .map(|pois| {
            pois.records
                .iter()
                .filter_map(|(&pos, record)| (record.kind == PoiKind::Bell).then_some(pos))
                .collect()
        })
        .unwrap_or_default();
    let Some(reg) = crate::block_registry::breg() else {
        return Vec::new();
    };
    let mut ringing = Vec::new();
    for pos in positions {
        let st = block_at_dim(dim, pos.0, pos.1, pos.2);
        if reg.name_of(st) != "minecraft:bell" || reg.get_prop(st, "powered").as_deref() != Some("true") {
            continue;
        }
        let reset = reg.with_prop(st, "powered", "false");
        set_block_dim(dim, pos.0, pos.1, pos.2, reset);
        note_block_update_dim(dim, pos.0, pos.1, pos.2, reset);
        ringing.push(pos);
    }
    ringing
}

#[cfg_attr(not(test), allow(dead_code))]
fn live_apply_bell_glowing(dim: DimId, bell: BlockPos, tick: u64) {
    let raiders: Vec<i32> = live_raid_mobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(|(&eid, &(rdim, _, _))| (rdim == dim).then_some(eid))
        .collect();
    if raiders.is_empty() {
        return;
    }
    let mob_list = mobs().lock().unwrap_or_else(|e| e.into_inner());
    let mut glowing = live_raid_glowing().lock().unwrap_or_else(|e| e.into_inner());
    for eid in raiders {
        let in_range = mob_list.iter().find(|mob| mob.eid == eid).is_some_and(|mob| {
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            bell_glowing_range_contains(bell, (x.floor() as i32, y.floor() as i32, z.floor() as i32))
        });
        if !in_range {
            continue;
        }
        evict_live_villager_registry_overflow(&mut glowing);
        glowing.insert(eid, tick + RAID_BELL_GLOWING_TICKS);
        // Raiders do not otherwise use entity base metadata flags in this
        // runtime; bit 0x40 is the vanilla glowing flag.
        broadcast_seen(eid, |_| (0x58, live_mob_metadata(eid, 0, 0, 0x40)));
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn live_raid_nearest_player(
    clients: &[Arc<Client>],
    dim: DimId,
    pos: (f64, f64, f64),
) -> Option<VillageUuid> {
    let mut best = None;
    let mut best_sq = 64.0 * 64.0;
    for client in clients.iter().filter(|client| !client.is_spectator()) {
        let (cdim, x, y, z, ..) = client_dim_pos(client);
        if cdim != dim {
            continue;
        }
        let dist_sq = (x - pos.0).powi(2) + (y - pos.1).powi(2) + (z - pos.2).powi(2);
        if dist_sq < best_sq {
            best_sq = dist_sq;
            best = Some(client.uuid);
        }
    }
    best
}

#[cfg_attr(not(test), allow(dead_code))]
fn living_villagers_near(dim: DimId, center: BlockPos) -> usize {
    let villager_etype = name_to_etype("villager");
    mobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|mob| {
            mob.dim == dim
                && Some(mob.etype) == villager_etype
                && *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0
                && {
                    let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
                    villager_within(
                        (x.floor() as i32, y.floor() as i32, z.floor() as i32),
                        center,
                        64,
                    )
                }
        })
        .count()
}

/// Per-game-tick raid step: count raider deaths as wave defeats, advance the
/// raid, spawn waves, grant Hero of the Village on victory, and keep the
/// bossbar mirroring the raid.
#[cfg_attr(not(test), allow(dead_code))]
fn live_raid_step() {
    let clients = client_snapshot();
    let mut raids = saved_raids().lock().unwrap_or_else(|e| e.into_inner());
    if raids.is_empty() {
        live_raid_mobs().lock().unwrap_or_else(|e| e.into_inner()).clear();
        return;
    }
    let now = TICK.load(Ordering::Relaxed);
    {
        let mut glowing = live_raid_glowing().lock().unwrap_or_else(|e| e.into_inner());
        evict_live_villager_registry_overflow(&mut glowing);
        let expired: Vec<i32> = glowing.iter().filter_map(|(&eid, &until)| (until <= now).then_some(eid)).collect();
        for eid in &expired {
            glowing.remove(eid);
            broadcast_seen(*eid, |_| (0x58, live_mob_metadata(*eid, 0, 0, 0)));
        }
    }
    let raid_dims: HashSet<DimId> = raids.iter().map(|(dim, _)| *dim).collect();
    for dim in raid_dims {
        for bell in live_consume_powered_bells(dim) {
            live_apply_bell_glowing(dim, bell, now);
        }
    }
    // A tracked raider that died (or left the live list) counts as defeated;
    // the closest player inside 64 blocks is recorded as the killer.
    let mut defeated = Vec::new();
    {
        let tracked = live_raid_mobs().lock().unwrap_or_else(|e| e.into_inner());
        let mob_list = mobs().lock().unwrap_or_else(|e| e.into_inner());
        for (&eid, &(dim, center, raider)) in tracked.iter() {
            let mob = mob_list.iter().find(|mob| mob.eid == eid);
            let dead = mob.is_none_or(|mob| *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0);
            if dead {
                let killer = mob.and_then(|mob| {
                    let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
                    live_raid_nearest_player(&clients, dim, (x, y, z))
                });
                defeated.push((eid, dim, center, raider, killer));
            }
        }
    }
    if !defeated.is_empty() {
        let mut tracked = live_raid_mobs().lock().unwrap_or_else(|e| e.into_inner());
        for (eid, dim, center, raider, killer) in defeated {
            tracked.remove(&eid);
            if live_raid_captains().lock().unwrap_or_else(|e| e.into_inner()).remove(&eid).is_some() {
                if let Some(banner) = name_to_item("ominous_banner") {
                    let pos = mobs().lock().unwrap_or_else(|e| e.into_inner()).iter().find(|mob| mob.eid == eid)
                        .map(|mob| *mob.pos.lock().unwrap_or_else(|e| e.into_inner()));
                    if let Some((x, y, z, ..)) = pos {
                        drop_spawn_in_dim(dim, DropSpawn {
                            x, y, z, item: banner, cnt: 1, comps: Vec::new(), age: 0, target: None, thrower: killer,
                        });
                    }
                }
            }
            if let Some((_, raid)) = raids
                .iter_mut()
                .find(|(d, raid)| *d == dim && raid.center == center)
            {
                raid.record_defeat(raider, killer);
            }
        }
    }
    for (dim, raid) in raids.iter_mut() {
        let living = living_villagers_near(*dim, raid.center);
        let event = {
            let registries = live_pois().lock().unwrap_or_else(|e| e.into_inner());
            let empty = PoiRegistry::default();
            let pois = registries.get(dim).unwrap_or(&empty);
            raid.tick(pois, living)
        };
        match event {
            Some(RaidEvent::SpawnWave(wave)) => live_raid_spawn_wave(*dim, raid, wave),
            Some(RaidEvent::Victory {
                participants,
                effect,
            }) => {
                if let Some(hero) = live_effect_id("hero_of_the_village") {
                    let left = server_ticks(u64::from(effect.duration_ticks)) as i32;
                    let amp = i32::from(effect.level.saturating_sub(1));
                    for uuid in &participants {
                        if let Some(client) = clients.iter().find(|c| c.uuid == *uuid) {
                            live_grant_effect(client, hero, amp, left);
                        }
                    }
                }
                let villager_etype = name_to_etype("villager");
                let owner = participants.first()
                    .and_then(|uuid| clients.iter().find(|client| client.uuid == *uuid))
                    .map(|client| client.eid)
                    .unwrap_or(0);
                let positions: Vec<(f64, f64, f64)> = mobs().lock().unwrap_or_else(|e| e.into_inner()).iter()
                    .filter(|mob| mob.dim == *dim && Some(mob.etype) == villager_etype)
                    .filter_map(|mob| {
                        let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
                        villager_within((x.floor() as i32, y.floor() as i32, z.floor() as i32), raid.center, 64)
                            .then_some((x, y, z))
                    }).collect();
                let mut live_fireworks = fireworks().lock().unwrap_or_else(|e| e.into_inner());
                for (x, y, z) in positions {
                    live_fireworks.push(Firework {
                        eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                        owner,
                        x, y: y + 1.0, z,
                        dx: 0.0, dy: 0.9, dz: 0.0,
                        ttl: server_ticks(30) as i32,
                        explosions: 1,
                    });
                }
                drop(live_fireworks);
                live_raid_bossbar_write(*dim, raid.center, None, 1, &clients);
            }
            Some(RaidEvent::Loss) => {
                live_raid_bossbar_write(*dim, raid.center, None, 1, &clients);
            }
            None => live_raid_bossbar_write(*dim, raid.center, Some(raid), 2, &clients),
        }
    }
    let ended: Vec<(DimId, BlockPos)> = raids
        .iter()
        .filter(|(_, raid)| raid.state == RaidState::Stopped)
        .map(|(dim, raid)| (*dim, raid.center))
        .collect();
    if !ended.is_empty() {
        let mut tracked = live_raid_mobs().lock().unwrap_or_else(|e| e.into_inner());
        let mut totals = live_raid_wave_totals().lock().unwrap_or_else(|e| e.into_inner());
        for (dim, center) in &ended {
            tracked.retain(|_, (d, c, _)| !(*d == *dim && *c == *center));
            totals.remove(&(*dim, *center));
            live_raid_bossbar_write(*dim, *center, None, 1, &clients);
        }
    }
    raids.retain(|(_, raid)| raid.state != RaidState::Stopped);
}

// ---------- P2: patrol / trader / warden / phantom spawner gamerules ----------
//
// Each gate reads `gamerule_live_value` (world_mechanisms.rs) — the same
// atomic registry and stored command map `/gamerule` mutates.

/// `PatrolSpawner`: one attempt every `12000 + rand(1200)` ticks, fires only
/// from day 5 on during daytime, then a one-in-five roll.
fn eval_patrol_spawn_attempt(day: u64, is_day: bool, roll_mod_5: u32) -> bool {
    day >= 5 && is_day && roll_mod_5.is_multiple_of(5)
}

/// `WanderingTraderSpawner`: each 24000-tick attempt raises the spawn chance
/// by 25 (25 → 50 → 75) until a trader lands.
fn eval_trader_spawn_attempt(spawn_chance: i32, roll_0_to_99: u32) -> bool {
    (roll_0_to_99 % 100) < spawn_chance.max(0) as u32
}

/// `SculkShrieker`/`WardenSpawnTracker`: a `can_summon` shrieker summons the
/// warden once the player's warning level reaches 4 — `doWardenSpawning`
/// gates the whole arm.
fn eval_warden_summon(spawning_enabled: bool, warning_level: u8) -> bool {
    spawning_enabled && warning_level >= 4
}

/// `PhantomSpawner`: phantoms circle a player whose `time_since_rest` passed
/// 72000 ticks (three full days), at night, above the surface under open sky.
fn eval_insomnia_spawn(time_since_rest: u64, night: bool, above_surface: bool, sky_visible: bool) -> bool {
    time_since_rest >= 72_000 && night && above_surface && sky_visible
}

#[cfg_attr(not(test), allow(dead_code))]
struct SpecialSpawnState {
    /// Next game tick a patrol attempt is due (`PatrolSpawner.nextTick`).
    patrol_next: u64,
    /// `WanderingTraderSpawner`: countdown within each 1200-tick slot.
    trader_tick_delay: u32,
    /// `WanderingTraderSpawner.spawnDelay`: 24000 ticks between attempts.
    trader_spawn_delay: u32,
    /// `WanderingTraderSpawner.spawnChance`: 25 → 50 → 75 while attempts fail.
    trader_spawn_chance: i32,
    /// Next game tick a phantom sweep is due (`PhantomSpawner.nextTick`).
    insomnia_next: u64,
    /// Per-player `Stats.CUSTOM.TIME_SINCE_REST` in game ticks.
    time_since_rest: HashMap<VillageUuid, u64>,
    /// Per-player shrieker warning level (`WardenSpawnTracker`), 0..4.
    warden_warnings: HashMap<VillageUuid, u8>,
}

impl Default for SpecialSpawnState {
    fn default() -> Self {
        Self {
            patrol_next: 0,
            // `WanderingTraderSpawner`'s constructor always opens the
            // 1200-tick slot countdown, and a fresh save's persisted
            // delay/chance read 0/0 — vanilla resets them to 24000/25.
            // All-zero timers here drained into a spawn attempt on the
            // world's first tick.
            trader_tick_delay: 1200,
            trader_spawn_delay: 24_000,
            trader_spawn_chance: 25,
            insomnia_next: 0,
            time_since_rest: HashMap::new(),
            warden_warnings: HashMap::new(),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn special_spawn_state() -> &'static Mutex<SpecialSpawnState> {
    static STATE: OnceLock<Mutex<SpecialSpawnState>> = OnceLock::new();
    STATE.get_or_init(Mutex::default)
}

/// Spawn one named mob at a position, the same way raid waves place raiders.
#[cfg_attr(not(test), allow(dead_code))]
fn live_spawn_named(dim: DimId, name: &str, pos: (f64, f64, f64, f32, f32)) -> Option<Arc<Mob>> {
    let spec = entity_spec_by_name(name)?;
    with_save_guard(|| {
        let mut current = mobs().lock().unwrap_or_else(|e| e.into_inner());
        if current.len() >= MOB_CAP {
            return None;
        }
        let mob = new_live_mob(spec.id, dim, pos, spec.max_health, None);
        push_natural_hostile(&mut current, mob.clone());
        Some(mob)
    })
}

/// `PatrolSpawner.spawnPatrolMember`: a five-pillager patrol placed on the
/// surface 24-48 blocks out from the target player.
#[cfg_attr(not(test), allow(dead_code))]
fn live_spawn_patrol(dim: DimId, center: (f64, f64, f64)) {
    for index in 0..5 {
        let angle = index as f64 / 5.0 * std::f64::consts::TAU;
        let radius = 24.0 + f64::from(world_rng::next_below(25));
        let bx = (center.0 + angle.cos() * radius).floor() as i32;
        let bz = (center.2 + angle.sin() * radius).floor() as i32;
        let by = if dim == DimId::Overworld {
            surface_y(bx, bz).floor() as i32
        } else {
            dim.min_y() + 64
        };
        let yaw = world_rng::next_below(360) as f32;
        live_spawn_named(dim, "pillager", (bx as f64 + 0.5, by as f64, bz as f64 + 0.5, yaw, 0.0));
    }
}

/// `WanderingTraderSpawner`: the trader lands within 48 blocks of a player
/// (ten position tries), trailed by two leashed trader llamas.
#[cfg_attr(not(test), allow(dead_code))]
fn live_spawn_trader(dim: DimId, center: (f64, f64, f64)) -> bool {
    let mut spawned = None;
    for _ in 0..10 {
        let bx = (center.0 + f64::from(world_rng::next_below(97)) - 48.0).floor() as i32;
        let bz = (center.2 + f64::from(world_rng::next_below(97)) - 48.0).floor() as i32;
        let by = if dim == DimId::Overworld {
            surface_y(bx, bz).floor() as i32
        } else {
            dim.min_y() + 64
        };
        let yaw = world_rng::next_below(360) as f32;
        if let Some(trader) = live_spawn_named(
            dim,
            "wandering_trader",
            (bx as f64 + 0.5, by as f64, bz as f64 + 0.5, yaw, 0.0),
        ) {
            let offers = wandering_trader_offers(|bound| world_rng::next_below(bound as u32) as usize);
            live_wandering_traders().lock().unwrap_or_else(|e| e.into_inner())
                .insert(trader.eid, (WANDERING_TRADER_DESPAWN_TICKS, offers));
            spawned = Some((bx, by, bz));
            break;
        }
    }
    let Some((bx, by, bz)) = spawned else {
        return false;
    };
    for side in [-1.0, 1.0] {
        live_spawn_named(
            dim,
            "trader_llama",
            (bx as f64 + 0.5 + side, by as f64, bz as f64 + 0.5, 0.0, 0.0),
        );
    }
    true
}

/// `PhantomSpawner`: phantoms spawn circling above a sleepless player.
#[cfg_attr(not(test), allow(dead_code))]
fn live_spawn_phantoms(dim: DimId, player_pos: (f64, f64, f64)) {
    let count = 1 + world_rng::next_below(3);
    for _ in 0..count {
        let bx = (player_pos.0 + f64::from(world_rng::next_below(41)) - 20.0).floor() as i32;
        let bz = (player_pos.2 + f64::from(world_rng::next_below(41)) - 20.0).floor() as i32;
        let by = (player_pos.1 + f64::from(world_rng::next_below(11)) + 15.0).floor() as i32;
        let yaw = world_rng::next_below(360) as f32;
        live_spawn_named(dim, "phantom", (bx as f64 + 0.5, by as f64, bz as f64 + 0.5, yaw, 0.0));
    }
}

/// `WardenSpawnTracker.spawnWarden`: the warden emerges near the player that
/// tripped the fourth shrieker warning.
#[cfg_attr(not(test), allow(dead_code))]
fn live_spawn_warden(dim: DimId, player_pos: (f64, f64, f64)) {
    let bx = player_pos.0.floor() as i32;
    let bz = player_pos.2.floor() as i32;
    let by = if dim == DimId::Overworld {
        surface_y(bx, bz).floor() as i32
    } else {
        dim.min_y() + 64
    };
    let yaw = world_rng::next_below(360) as f32;
    live_spawn_named(dim, "warden", (bx as f64 + 0.5, by as f64, bz as f64 + 0.5, yaw, 0.0));
}

/// Whether a `sculk_shrieker` with `can_summon=true` sits within 8 blocks of
/// the player — vanilla's shrieker pickup range for warning accrual.
#[cfg_attr(not(test), allow(dead_code))]
fn can_summon_shrieker_near(dim: DimId, bx: i32, by: i32, bz: i32) -> bool {
    let Some(reg) = crate::block_registry::breg() else {
        return false;
    };
    for dx in -8i32..=8 {
        for dy in -4i32..=4 {
            for dz in -8i32..=8 {
                let st = block_at_dim(dim, bx + dx, by + dy, bz + dz);
                if reg.name_of(st) == "minecraft:sculk_shrieker"
                    && reg.get_prop(st, "can_summon").as_deref() == Some("true")
                {
                    return true;
                }
            }
        }
    }
    false
}

/// The four special-spawner arms of `NaturalSpawner`'s host-side tick, run
/// once per game tick alongside the raid logic. Every arm reads its gamerule
/// live and spawns through the same `new_live_mob` path raid waves use.
#[cfg_attr(not(test), allow(dead_code))]
fn live_special_spawn_tick(clients: &[Arc<Client>]) {
    let now = TICK.load(Ordering::Relaxed);
    let world_time = WORLD_TIME.load(Ordering::Relaxed);
    let mut state = special_spawn_state().lock().unwrap_or_else(|e| e.into_inner());
    let overworld_players: Vec<(VillageUuid, (f64, f64, f64), bool)> = clients
        .iter()
        .filter(|client| !client.is_spectator())
        .filter(|client| *client.dim.lock().unwrap_or_else(|e| e.into_inner()) == DimId::Overworld)
        .map(|client| {
            let (x, y, z, ..) = *client.pos.lock().unwrap_or_else(|e| e.into_inner());
            (
                client.uuid,
                (x, y, z),
                client.sleeping.lock().unwrap_or_else(|e| e.into_inner()).is_some(),
            )
        })
        .collect();

    // `time_since_rest` accrues whether or not phantoms are allowed; sleeping
    // resets the stat exactly like vanilla's wake handling.
    for (uuid, _, sleeping) in &overworld_players {
        if *sleeping {
            state.time_since_rest.insert(*uuid, 0);
        } else {
            *state.time_since_rest.entry(*uuid).or_default() += 1;
        }
    }

    // `PatrolSpawner`: day 5+, daytime, one-in-five roll per attempt window.
    if now >= state.patrol_next {
        state.patrol_next = now + 12_000 + u64::from(world_rng::next_below(1200));
        if gamerule_live_value("doPatrolSpawning") != 0 && !overworld_players.is_empty() {
            let day = world_time / VILLAGER_DAY_TICKS;
            let is_day = world_time % VILLAGER_DAY_TICKS < 12_300;
            if eval_patrol_spawn_attempt(day, is_day, world_rng::next_below(5)) {
                let pick = world_rng::next_below(overworld_players.len() as u32) as usize;
                live_spawn_patrol(DimId::Overworld, overworld_players[pick].1);
            }
        }
    }

    // `WanderingTraderSpawner`: 1200-tick delay slots drain a 24000-tick
    // spawn delay; each failed attempt raises the chance by 25 to a 75 cap.
    // Vanilla's `tick` gates the attempt on `doMobSpawning` as well as
    // `doTraderSpawning`.
    if state.trader_tick_delay > 0 {
        state.trader_tick_delay -= 1;
    } else {
        state.trader_tick_delay = 1200;
        state.trader_spawn_delay = state.trader_spawn_delay.saturating_sub(1200);
        if state.trader_spawn_delay == 0 {
            state.trader_spawn_delay = 24_000;
            if gamerule_live_value("doTraderSpawning") != 0
                && gamerule_live_value("doMobSpawning") != 0
                && !overworld_players.is_empty()
            {
                state.trader_spawn_chance = (state.trader_spawn_chance + 25).min(75);
                if eval_trader_spawn_attempt(state.trader_spawn_chance, world_rng::next_below(100))
                {
                    let pick =
                        world_rng::next_below(overworld_players.len() as u32) as usize;
                    if live_spawn_trader(DimId::Overworld, overworld_players[pick].1) {
                        state.trader_spawn_chance = 0;
                    }
                }
            }
        }
    }

    // Wandering traders use the 48000-tick despawn delay independently of the
    // natural despawn scanner.
    {
        let mut traders = live_wandering_traders().lock().unwrap_or_else(|e| e.into_inner());
        let mut expired = Vec::new();
        for (&eid, (delay, _)) in traders.iter_mut() {
            let (next, done) = wandering_trader_despawn_tick(*delay);
            *delay = next;
            if done { expired.push(eid); }
        }
        if !expired.is_empty() {
            let mob_list = mobs().lock().unwrap_or_else(|e| e.into_inner());
            for eid in expired {
                traders.remove(&eid);
                if let Some(mob) = mob_list.iter().find(|mob| mob.eid == eid) {
                    *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
                }
            }
        }
    }

    // `PhantomSpawner`: every ~1-2 minutes sweep each unslept player.
    if now >= state.insomnia_next {
        state.insomnia_next = now + (60 + u64::from(world_rng::next_below(60))) * 20;
        if gamerule_live_value("doInsomnia") != 0 {
            let night = {
                let daytime = world_time % VILLAGER_DAY_TICKS;
                (13_000..23_000).contains(&daytime)
            };
            for (uuid, pos, _) in &overworld_players {
                let rest = state.time_since_rest.get(uuid).copied().unwrap_or(0);
                let (bx, by, bz) = (pos.0.floor() as i32, pos.1.floor() as i32, pos.2.floor() as i32);
                let above_surface = pos.1 > surface_y(bx, bz);
                let sky_visible = by >= height_at_dim(DimId::Overworld, bx, bz);
                if eval_insomnia_spawn(rest, night, above_surface, sky_visible) {
                    live_spawn_phantoms(DimId::Overworld, *pos);
                }
            }
        }
    }

    // `SculkShrieker` + `WardenSpawnTracker`: warning level 4 near a
    // `can_summon` shrieker summons the warden. Warnings accrue on a
    // twenty-tick cadence, matching the shrieker's own cooldown granularity.
    if gamerule_live_value("doWardenSpawning") != 0 && now.is_multiple_of(20) {
        for (uuid, pos, _) in &overworld_players {
            let (bx, by, bz) = (pos.0.floor() as i32, pos.1.floor() as i32, pos.2.floor() as i32);
            if can_summon_shrieker_near(DimId::Overworld, bx, by, bz) {
                let level = state.warden_warnings.entry(*uuid).or_default();
                *level = (*level + 1).min(4);
                if eval_warden_summon(true, *level) {
                    state.warden_warnings.insert(*uuid, 0);
                    live_spawn_warden(DimId::Overworld, *pos);
                }
            }
        }
    }
}

/// One game tick of live villager/raid work, called from `game_tick`. The
/// `Villager` and `Raid` units keep their "once per game tick" contracts.
#[cfg_attr(not(test), allow(dead_code))]
fn villager_raid_tick() {
    let tick = TICK.load(Ordering::Relaxed);
    live_villager_tick(tick);
    live_zombie_villager_cure_tick();
    live_utility_mob_tick(tick);
    live_golem_formation_tick(tick);
    live_village_golem_tick(tick);
    // Patrols, traders, wardens and phantoms are not raids — their gamerules
    // gate them individually inside the special-spawn tick.
    live_special_spawn_tick(&client_snapshot());
    // `disableRaids` reads the same atomic `/gamerule` writes.
    if gr_disable_raids() {
        return;
    }
    live_raid_omen_tick();
    live_raid_step();
}

#[cfg(test)]
mod villager_raid_tests {
    use super::*;

    const PLAYER: VillageUuid = [1; 16];
    const OTHER_PLAYER: VillageUuid = [2; 16];
    const BED: BlockPos = (0, 64, 0);
    const JOB_SITE: BlockPos = (3, 64, 0);
    const BELL: BlockPos = (5, 64, 0);
    const AT_WORK: BlockPos = (3, 64, 1);
    const WHEAT: i64 = 854;
    const BOOK: i64 = 925;
    const EMERALD: i64 = 806;
    const ENDER_PEARL: i64 = 993;

    #[test]
    fn vanilla_mobs2_golem_construction_requires_iron_t_or_two_snow_blocks() {
        let pumpkin = (4, 70, 9);
        for (axis, arms) in [
            (0, [(3, 69, 9), (5, 69, 9)]),
            (1, [(4, 69, 8), (4, 69, 10)]),
        ] {
            let blocks = HashMap::from([
                (pumpkin, "minecraft:carved_pumpkin"),
                ((4, 69, 9), "minecraft:iron_block"),
                ((4, 68, 9), "minecraft:iron_block"),
                (arms[0], "minecraft:iron_block"),
                (arms[1], "minecraft:iron_block"),
            ]);
            let found = detect_golem_construction(pumpkin, |p| blocks.get(&p).copied().unwrap_or("minecraft:air"));
            let (kind, consumed) = found.expect("complete T construction");
            assert_eq!(kind, ConstructedGolem::Iron, "axis {axis}");
            assert_eq!(consumed.len(), 5);
            assert!(consumed.iter().all(|p| blocks.contains_key(p)));

            let mut lantern = blocks.clone();
            lantern.insert(pumpkin, "minecraft:jack_o_lantern");
            assert_eq!(
                detect_golem_construction(pumpkin, |p| lantern.get(&p).copied().unwrap_or("minecraft:air")),
                Some((ConstructedGolem::Iron, consumed.clone())),
            );

            let mut missing_arm = blocks.clone();
            missing_arm.remove(&arms[0]);
            assert_eq!(detect_golem_construction(pumpkin, |p| {
                missing_arm.get(&p).copied().unwrap_or("minecraft:air")
            }), None);
            missing_arm.insert(arms[0], "minecraft:iron_block");
            missing_arm.insert(pumpkin, "minecraft:pumpkin");
            assert_eq!(detect_golem_construction(pumpkin, |p| {
                missing_arm.get(&p).copied().unwrap_or("minecraft:air")
            }), None);
        }

        let snow = HashMap::from([
            (pumpkin, "minecraft:carved_pumpkin"),
            ((4, 69, 9), "minecraft:snow_block"),
            ((4, 68, 9), "minecraft:snow_block"),
        ]);
        let (kind, consumed) = detect_golem_construction(pumpkin, |p| {
            snow.get(&p).copied().unwrap_or("minecraft:air")
        }).expect("stacked snow construction");
        assert_eq!(kind, ConstructedGolem::Snow);
        assert_eq!(consumed.len(), 3);
        let mut lantern_snow = snow.clone();
        lantern_snow.insert(pumpkin, "minecraft:jack_o_lantern");
        assert_eq!(
            detect_golem_construction(pumpkin, |p| lantern_snow.get(&p).copied().unwrap_or("minecraft:air")),
            Some((ConstructedGolem::Snow, consumed)),
        );
    }

    #[test]
    fn vanilla_mobs2_iron_golem_creation_reputation_repair_cracks_and_gossip_spawn() {
        let mut gossip = VillagerGossip::default();
        gossip.add(PLAYER, GossipType::MajorNegative, 20);
        let spawned_by_player = IronGolemState { player_created: true };
        let village_golem = IronGolemState { player_created: false };
        assert!(!spawned_by_player.hostile_to_player(&gossip, PLAYER));
        assert!(village_golem.hostile_to_player(&gossip, PLAYER));
        assert!(!village_golem.hostile_to_player(&gossip, OTHER_PLAYER));

        let mut hp = 20.0;
        assert_eq!(IronGolemState::crack_stage(hp), 3);
        assert!(!IronGolemState::repair(&mut hp, false));
        assert!(IronGolemState::repair(&mut hp, true));
        assert_eq!(hp, 45.0);
        assert_eq!(IronGolemState::crack_stage(hp), 2);
        assert!(IronGolemState::repair(&mut hp, true));
        assert_eq!(hp, 70.0);
        assert_eq!(IronGolemState::crack_stage(hp), 1);
        assert!(IronGolemState::repair(&mut hp, true));
        assert_eq!(hp, 95.0);
        assert_eq!(IronGolemState::crack_stage(hp), 0);
        assert!(IronGolemState::repair(&mut hp, true));
        assert_eq!(hp, 100.0);
        assert!(!IronGolemState::repair(&mut hp, true));
        for (hp, stage) in [(0.0, 3), (25.0, 3), (25.1, 2), (50.0, 2), (50.1, 1), (75.0, 1), (75.1, 0), (100.0, 0)] {
            assert_eq!(IronGolemState::crack_stage(hp), stage);
        }
        assert!(village_golem_spawn_allowed(3, true, false));
        assert!(!village_golem_spawn_allowed(2, true, false));
        assert!(!village_golem_spawn_allowed(3, false, false));
        assert!(!village_golem_spawn_allowed(3, true, true));
    }

    #[test]
    fn vanilla_mobs2_snow_golem_shearing_melting_and_cold_trail() {
        let mut snow = SnowGolemState::default();
        assert!(snow.shear());
        assert!(snow.sheared);
        assert!(!snow.shear());

        let mut hp = 4.0;
        assert!(SnowGolemState::environment_tick(&mut hp, false, false, true));
        assert_eq!(hp, 4.0);
        assert!(!SnowGolemState::environment_tick(&mut hp, false, false, false));
        assert!(!SnowGolemState::environment_tick(&mut hp, true, false, false));
        assert_eq!(hp, 3.0);
        assert!(!SnowGolemState::environment_tick(&mut hp, false, true, false));
        assert_eq!(hp, 2.0);
        assert!(!SnowGolemState::environment_tick(&mut hp, true, true, false));
        assert_eq!(hp, 1.0, "warmth and rain apply one tick of damage, not two");
        assert!(!SnowGolemState::environment_tick(&mut hp, true, false, true));
        assert_eq!(hp, 0.0);
    }

    #[test]
    fn vanilla_mobs2_axolotl_hydration_play_dead_healing_and_player_reward() {
        let mut axolotl = AxolotlState::new(14.0);
        let mut hp = 14.0;
        for tick in 1..6_000 {
            axolotl.tick(false, &mut hp, 14.0, tick);
        }
        assert_eq!(axolotl.dry_ticks, 1);
        assert_eq!(hp, 14.0);
        axolotl.tick(false, &mut hp, 14.0, 6_000);
        assert_eq!(axolotl.dry_ticks, 0);
        assert_eq!(hp, 13.0);
        axolotl.tick(true, &mut hp, 14.0, 6_001);
        assert_eq!(axolotl.dry_ticks, AxolotlState::DRY_TICKS);

        hp = 5.0;
        axolotl.tick(true, &mut hp, 14.0, 6_002);
        assert!(axolotl.playing_dead_ticks > 0);
        let initial_timer = axolotl.playing_dead_ticks;
        axolotl.tick(true, &mut hp, 14.0, 6_025);
        assert_eq!(hp, 6.0);
        assert!(axolotl.playing_dead_ticks < initial_timer);
        assert_eq!(AxolotlState::reward_player_for_target_kill(false), None);
        assert_eq!(AxolotlState::reward_player_for_target_kill(true), Some((0, 100)));
    }

    #[test]
    fn vanilla_mobs2_allay_delivery_matching_notes_and_jukebox_duplication() {
        let mut allay = AllayState::default();
        assert!(!allay.give_item(PLAYER, stack(WHEAT, 0)));
        assert!(allay.give_item(PLAYER, stack(WHEAT, 9)));
        assert_eq!(allay.owner, Some(PLAYER));
        assert_eq!(allay.held, Some(stack(WHEAT, 1)));
        assert!(!allay.give_item(OTHER_PLAYER, stack(BOOK, 1)));

        let ground_drop = |x, item, target| Drop {
            eid: 23_678, x, y: 0.0, z: 0.0, item, cnt: 2,
            comps: Vec::new(), age: 15, target, thrower: None,
        };
        assert!(allay.matches_drop(&ground_drop(31.0, WHEAT, None), [0.0, 0.0, 0.0]));
        assert!(!allay.matches_drop(&ground_drop(32.1, WHEAT, None), [0.0, 0.0, 0.0]));
        assert!(!allay.matches_drop(&ground_drop(31.0, BOOK, None), [0.0, 0.0, 0.0]));
        assert!(!allay.matches_drop(&ground_drop(31.0, WHEAT, Some(PLAYER)), [0.0, 0.0, 0.0]));

        let ground = ground_drop(1.0, WHEAT, None);
        assert!(allay.collect_drop(&ground, [0.0, 0.0, 0.0]));
        assert_eq!(allay.collected, Some(stack(WHEAT, 2)));
        assert!(!allay.matches_drop(&ground, [0.0, 0.0, 0.0]));
        assert!(!allay.collect_drop(&ground, [0.0, 0.0, 0.0]));
        allay.collected = None;
        let mut delayed = ground;
        delayed.age = -20;
        assert!(!allay.collect_drop(&delayed, [0.0, 0.0, 0.0]));

        allay.hears_note((3, 64, 2), 99);
        assert_eq!(allay.delivery_target(698), Some((3, 64, 2)));
        assert_eq!(allay.delivery_target(699), None);
        assert!(!allay.duplicate(false, true, true, 0));
        assert!(!allay.duplicate(true, false, true, 0));
        assert!(!allay.duplicate(true, true, false, 0));
        assert!(allay.duplicate(true, true, true, 0));
        assert!(!allay.duplicate(true, true, true, 5_999));
        assert!(allay.duplicate(true, true, true, 6_000));
    }

    #[test]
    fn vanilla_mobs2_fox_trust_sleep_and_pickup() {
        let mut fox = FoxState::default();
        fox.trust(PLAYER);
        fox.trust(PLAYER);
        fox.trust(OTHER_PLAYER);
        assert_eq!(fox.trusted, [Some(PLAYER), Some(OTHER_PLAYER)]);
        fox.tick(true, false, false);
        assert!(fox.sleeping);
        fox.tick(true, true, false);
        assert!(!fox.sleeping);
        assert!(!fox.pickup(stack(WHEAT, 0)));
        assert!(fox.pickup(stack(WHEAT, 3)));
        assert_eq!(fox.held, Some(stack(WHEAT, 1)));
        assert!(!fox.pickup(stack(BOOK, 1)));
    }

    #[test]
    fn vanilla_mobs2_camel_two_seats_sitting_dash_and_cooldown() {
        assert_eq!(CamelState::PASSENGERS, 2);
        assert_eq!(VehicleKind::Camel.max_passengers(), 2);
        let mut camel = CamelState::default();
        assert_eq!(camel.dash(100, 0.0), Some([0.0, 0.6, 1.8]));
        assert_eq!(camel.dash(154, 0.0), None);
        assert!(camel.dash(155, 90.0).is_some());
        camel.toggle_sitting();
        assert!(camel.sitting);
        assert_eq!(camel.dash(220, 0.0), None);
        camel.toggle_sitting();
        assert!(camel.dash(220, 180.0).is_some());
    }

    fn stack(item: i64, count: u8) -> Stack {
        (item, count, Vec::new())
    }

    fn items() -> MerchantItems<'static> {
        fn prototype(item: i64) -> &'static Comps {
            static NORMAL: OnceLock<Comps> = OnceLock::new();
            static PEARL: OnceLock<Comps> = OnceLock::new();
            if item == ENDER_PEARL {
                PEARL.get_or_init(|| vec![(1, vec![16])])
            } else {
                NORMAL.get_or_init(|| vec![(1, vec![64])])
            }
        }
        MerchantItems(&prototype)
    }

    fn offer() -> MerchantOffer {
        MerchantOffer {
            cost_a: ItemCost {
                item: WHEAT,
                count: 20,
                components: Vec::new(),
            },
            cost_b: Some(ItemCost {
                item: BOOK,
                count: 2,
                components: Vec::new(),
            }),
            result: stack(EMERALD, 3),
            uses: 0,
            max_uses: 4,
            xp_reward: 10,
            price_multiplier: 0.05,
            demand: 0,
            special_price_diff: 0,
        }
    }

    fn village() -> (PoiRegistry, Villager) {
        let mut pois = PoiRegistry::default();
        pois.register(BED, PoiKind::Bed);
        pois.register(JOB_SITE, PoiKind::JobSite(VillagerProfession::Librarian));
        pois.register(BELL, PoiKind::Bell);
        let mut villager = Villager::new([3; 16], VillagerProfession::None);
        for pos in [BED, JOB_SITE, BELL] {
            assert!(villager.claim_poi(&mut pois, pos));
        }
        (pois, villager)
    }

    fn trading_villager(offers: impl IntoIterator<Item = MerchantOffer>) -> Villager {
        let (pois, mut villager) = village();
        villager.offers.extend(offers);
        assert!(villager.start_trading(&pois, PLAYER, None));
        villager
    }

    #[test]
    fn poi_classifier_covers_every_job_bed_color_and_cauldron() {
        use VillagerProfession::*;
        for (name, profession) in [
            ("blast_furnace", Armorer),
            ("smoker", Butcher),
            ("cartography_table", Cartographer),
            ("brewing_stand", Cleric),
            ("composter", Farmer),
            ("barrel", Fisherman),
            ("fletching_table", Fletcher),
            ("cauldron", Leatherworker),
            ("water_cauldron", Leatherworker),
            ("lava_cauldron", Leatherworker),
            ("powder_snow_cauldron", Leatherworker),
            ("lectern", Librarian),
            ("stonecutter", Mason),
            ("loom", Shepherd),
            ("smithing_table", Toolsmith),
            ("grindstone", Weaponsmith),
        ] {
            assert_eq!(
                poi_kind_from_block_name(&format!("minecraft:{name}")),
                Some(PoiKind::JobSite(profession))
            );
        }
        for color in [
            "white",
            "orange",
            "magenta",
            "light_blue",
            "yellow",
            "lime",
            "pink",
            "gray",
            "light_gray",
            "cyan",
            "purple",
            "blue",
            "brown",
            "green",
            "red",
            "black",
        ] {
            assert_eq!(
                poi_kind_from_block_name(&format!("minecraft:{color}_bed")),
                Some(PoiKind::Bed)
            );
        }
        assert_eq!(
            poi_kind_from_block_name("minecraft:bell"),
            Some(PoiKind::Bell)
        );
        for name in [
            "minecraft:air",
            "minecraft:furnace",
            "minecraft:fake_bed",
            "other:bell",
            "other:blue_bed",
        ] {
            assert_eq!(poi_kind_from_block_name(name), Option::None, "{name}");
        }
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn poi_box_scanner_reconciles_head_and_deleted_sites() {
        let reg = crate::block_registry::breg().expect("BReg");
        let property = |part: &str| vec![("part".to_string(), part.to_string())];
        let head = reg
            .checked_state("minecraft:red_bed", &property("head"))
            .unwrap();
        let foot = reg
            .checked_state("minecraft:red_bed", &property("foot"))
            .unwrap();
        let bell = reg.state_of("minecraft:bell", None);
        let lectern = reg.state_of("minecraft:lectern", None);
        let air = reg.state_of("minecraft:air", None);
        let mut pois = PoiRegistry::default();
        let scan = |pois: &mut PoiRegistry, states: [u32; 4]| {
            scan_poi_box(pois, reg, (0, 64, 0), (3, 64, 0), |(x, _, _)| {
                states[x as usize]
            })
        };
        assert_eq!(scan(&mut pois, [head, foot, bell, lectern]), 3);
        assert_eq!(pois.records.len(), 3);
        assert!(!pois.records.contains_key(&(1, 64, 0)));
        let owner = [7; 16];
        assert!(pois.claim((0, 64, 0), owner));
        assert_eq!(scan(&mut pois, [head, foot, bell, lectern]), 3);
        assert!(pois.has_claim((0, 64, 0), PoiKind::Bed, owner));
        assert_eq!(scan(&mut pois, [air, foot, bell, air]), 1);
        assert!(!pois.records.contains_key(&(0, 64, 0)));
        assert!(!pois.records.contains_key(&(3, 64, 0)));
    }

    #[test]
    fn all_workstations_assign_their_profession() {
        use VillagerProfession::*;
        let mut pois = PoiRegistry::default();
        for (name, profession) in [
            ("blast_furnace", Armorer),
            ("smoker", Butcher),
            ("cartography_table", Cartographer),
            ("brewing_stand", Cleric),
            ("composter", Farmer),
            ("barrel", Fisherman),
            ("fletching_table", Fletcher),
            ("cauldron", Leatherworker),
            ("water_cauldron", Leatherworker),
            ("lava_cauldron", Leatherworker),
            ("powder_snow_cauldron", Leatherworker),
            ("lectern", Librarian),
            ("stonecutter", Mason),
            ("loom", Shepherd),
            ("smithing_table", Toolsmith),
            ("grindstone", Weaponsmith),
        ] {
            let block = format!("minecraft:{name}");
            let mapped = VillagerProfession::from_job_site(&block).unwrap();
            assert_eq!(mapped, profession, "{block}");
            pois.register(JOB_SITE, PoiKind::JobSite(mapped));
            let mut villager = Villager::new([3; 16], None);
            assert!(villager.claim_poi(&mut pois, JOB_SITE));
            assert_eq!(villager.profession, profession);
            villager.release_pois(&mut pois);
        }
        assert_eq!(
            VillagerProfession::from_job_site("minecraft:furnace"),
            Option::None
        );
        assert_eq!(
            VillagerProfession::from_job_site("other:lectern"),
            Option::None
        );
        assert_eq!(
            VillagerProfession::from_job_site("lectern"),
            Some(Librarian)
        );
    }

    #[test]
    fn claims_exclude_other_villagers_and_nitwits_cannot_work() {
        let (mut pois, mut first) = village();
        let mut second = Villager::new([4; 16], VillagerProfession::None);
        let mut nitwit = Villager::new([5; 16], VillagerProfession::Nitwit);
        assert!(first.claim_poi(&mut pois, JOB_SITE));
        pois.register(JOB_SITE, PoiKind::JobSite(VillagerProfession::Librarian));
        assert!(!second.claim_poi(&mut pois, JOB_SITE));
        assert!(!second.claim_poi(&mut pois, BED));
        assert!(second.claim_poi(&mut pois, BELL));
        assert!(nitwit.claim_poi(&mut pois, BELL));
        first.release_pois(&mut pois);
        assert!(!nitwit.claim_poi(&mut pois, JOB_SITE));
        assert_eq!(nitwit.profession, VillagerProfession::Nitwit);
        assert!(second.claim_poi(&mut pois, JOB_SITE));
        assert!(second.claim_poi(&mut pois, BED));
    }

    #[test]
    fn a_broken_job_site_only_resets_an_untraded_villager() {
        let (mut pois, mut villager) = village();
        villager.offers.push(offer());
        pois.remove(JOB_SITE);
        pois.register(JOB_SITE, PoiKind::JobSite(VillagerProfession::Librarian));
        assert!(!villager.start_trading(&pois, PLAYER, None));
        assert_eq!(villager.profession, VillagerProfession::None);
        assert!(villager.offers.is_empty());
        assert!(villager.claim_poi(&mut pois, JOB_SITE));
        villager.offers.push(offer());
        assert!(villager.start_trading(&pois, PLAYER, None));
        let mut menu = MerchantMenu {
            payments: [stack(WHEAT, 20), stack(BOOK, 2)],
            ..MerchantMenu::default()
        };
        assert_eq!(
            villager
                .trade(PLAYER, &mut menu, &items())
                .map(|trade| trade.result),
            Some(stack(EMERALD, 3))
        );
        villager.stop_trading();

        pois.register(JOB_SITE, PoiKind::JobSite(VillagerProfession::Farmer));
        villager.tick(&pois, AT_WORK, 4_000);
        assert_eq!(villager.job_site, None);
        assert_eq!(villager.profession, VillagerProfession::Librarian);
        assert_eq!(villager.offers[0].uses, 1);
        assert!(!villager.claim_poi(&mut pois, JOB_SITE));
        assert!(villager.start_trading(&pois, PLAYER, None));
        pois.register(JOB_SITE, PoiKind::JobSite(VillagerProfession::Librarian));
        assert!(villager.claim_poi(&mut pois, JOB_SITE));
        assert_eq!(villager.offers[0].uses, 1);
    }

    #[test]
    fn successful_trades_consume_both_costs_and_unlock_levels_at_the_xp_boundaries() {
        let (pois, mut villager) = village();
        villager.offers.push(offer());
        assert!(villager.start_trading(&pois, PLAYER, None));
        assert_eq!(villager.level(), VillagerLevel::Novice);
        for (reward, xp, level) in [
            (9, 9, VillagerLevel::Novice),
            (1, 10, VillagerLevel::Apprentice),
            (60, 70, VillagerLevel::Journeyman),
            (80, 150, VillagerLevel::Expert),
            (100, 250, VillagerLevel::Master),
        ] {
            villager.offers[0].xp_reward = reward;
            villager.offers[0].max_uses = 5;
            let mut menu = MerchantMenu {
                payments: [stack(WHEAT, 21), stack(BOOK, 1)],
                ..MerchantMenu::default()
            };
            assert_eq!(villager.trade(PLAYER, &mut menu, &items()), None);
            assert_eq!(menu.payments, [stack(WHEAT, 21), stack(BOOK, 1)]);
            menu.payments[1].1 = 3;
            assert_eq!(
                villager
                    .trade(PLAYER, &mut menu, &items())
                    .map(|trade| trade.result),
                Some(stack(EMERALD, 3))
            );
            assert_eq!(menu.payments, [stack(WHEAT, 1), stack(BOOK, 1)]);
            assert_eq!(villager.villager_xp, xp);
            assert_eq!(villager.level(), level);
        }
        assert_eq!(villager.gossip.reputation(PLAYER), 10);
        let mut menu = MerchantMenu {
            payments: [stack(WHEAT, 64), stack(BOOK, 2)],
            ..MerchantMenu::default()
        };
        assert_eq!(villager.trade(PLAYER, &mut menu, &items()), None);
        assert_eq!(villager.gossip.reputation(PLAYER), 10);
        assert_eq!(villager.offers[0].uses, 5);
        assert_eq!(villager.villager_xp, 250);
        assert_eq!(VillagerLevel::from_xp(69), VillagerLevel::Apprentice);
        assert_eq!(VillagerLevel::from_xp(149), VillagerLevel::Journeyman);
        assert_eq!(VillagerLevel::from_xp(249), VillagerLevel::Expert);
    }

    #[test]
    fn restock_updates_sold_out_and_unused_offers_before_resetting_uses() {
        let (pois, mut villager) = village();
        villager.offers = vec![
            MerchantOffer {
                uses: 4,
                demand: 3,
                ..offer()
            },
            MerchantOffer {
                uses: 0,
                demand: 0,
                ..offer()
            },
            MerchantOffer {
                demand: 5,
                ..offer()
            },
            MerchantOffer {
                demand: 2,
                ..offer()
            },
        ];

        villager.tick(&pois, AT_WORK, 2_000);
        assert_eq!(
            villager
                .offers
                .iter()
                .map(|offer| (offer.demand, offer.uses))
                .collect::<Vec<_>>(),
            [(7, 0), (-4, 0), (1, 0), (-2, 0)]
        );
        assert_eq!(villager.offers[0].cost_a_count(&items()), 27);
        assert_eq!(villager.offers[1].cost_a_count(&items()), 20);
        assert_eq!(villager.restocks_today, 1);
        villager.tick(&pois, AT_WORK, 4_000);
        assert_eq!(villager.offers[0].demand, 7);
        assert_eq!(villager.restocks_today, 1);
    }

    #[test]
    fn persisted_large_demand_uses_wrapping_java_int_product() {
        let offer = MerchantOffer {
            cost_a: ItemCost {
                count: 4,
                ..offer().cost_a
            },
            demand: 1 << 30,
            price_multiplier: 0.05,
            special_price_diff: 0,
            ..offer()
        };
        assert_eq!(offer.cost_a_count(&items()), 4);
        assert_eq!(
            MerchantOffer {
                demand: -(1 << 28),
                price_multiplier: 4.0,
                ..offer
            }
            .cost_a_count(&items()),
            1
        );
    }

    #[test]
    fn cure_and_hero_discounts_are_customer_specific_and_price_is_clamped() {
        let (pois, mut villager) = village();
        villager.offers.push(offer());
        villager.gossip.cure(PLAYER);
        assert_eq!(villager.gossip.reputation(PLAYER), 125);
        let price = |villager: &mut Villager, player, hero| {
            assert!(villager.start_trading(&pois, player, hero));
            let count = villager.offers[0].cost_a_count(&items());
            villager.stop_trading();
            count
        };
        assert_eq!(price(&mut villager, PLAYER, None), 14);
        assert_eq!(price(&mut villager, OTHER_PLAYER, None), 20);
        for (amplifier, expected) in [(0, 8), (1, 7), (2, 6), (3, 5), (4, 3), (5, 2)] {
            assert_eq!(price(&mut villager, PLAYER, Some(amplifier)), expected);
        }
        assert_eq!(villager.offers[0].special_price_diff, 0);
        villager.offers[0].cost_a.count = 2;
        assert_eq!(price(&mut villager, OTHER_PLAYER, Some(0)), 1);
        assert_eq!(price(&mut villager, PLAYER, Some(4)), 1);
        villager.offers[0].cost_a.count = 64;
        villager.offers[0].demand = 99;
        villager
            .gossip
            .add(OTHER_PLAYER, GossipType::MajorNegative, 100);
        assert_eq!(price(&mut villager, OTHER_PLAYER, Some(4)), 64);
    }

    #[test]
    fn merchant_offers_matches_the_protocol_767_payload() {
        let mut first = offer();
        first.cost_a.components = vec![(1, vec![64])];
        first.result.2 = vec![(1, vec![32])];
        first.cost_b = None;
        first.uses = 1;
        first.special_price_diff = -3;
        first.price_multiplier = 0.5;
        first.demand = -2;
        let mut second = offer();
        second.cost_a.item = ENDER_PEARL;
        second.cost_a.count = 5;
        second.cost_b.as_mut().unwrap().components = vec![(COMP_UNBREAKABLE, vec![1])];
        second.result = (838, 1, vec![(COMP_DAMAGE, vec![0x82, 0x01])]);
        second.uses = 4;
        second.xp_reward = 177;
        second.special_price_diff = 1;
        second.price_multiplier = 0.125;
        second.demand = 11;

        #[rustfmt::skip]
        let expected = [
            0xac, 0x02,                  // Window ID 300 (VarInt)
            0x02,                        // Two offers
            0xd6, 0x06, 0x14,            // Cost A: wheat ID 854, base count 20
            0x01, 0x01, 0x40,            // One exact component: max stack size 64
            0x03, 0xa6, 0x06,            // Result: 3 emeralds (ID 806)
            0x01, 0x00, 0x01, 0x20,      // 1 added, 0 removed, max stack size 32
            0x00,                        // Cost B absent
            0x00,                        // In stock
            0x00, 0x00, 0x00, 0x01,      // Uses 1
            0x00, 0x00, 0x00, 0x04,      // Max uses 4
            0x00, 0x00, 0x00, 0x0a,      // XP 10
            0xff, 0xff, 0xff, 0xfd,      // Special price -3
            0x3f, 0x00, 0x00, 0x00,      // Price multiplier 0.5
            0xff, 0xff, 0xff, 0xfe,      // Demand -2
            0xe1, 0x07, 0x05,            // Cost A: ender pearl ID 993, base count 5
            0x00,                        // Empty component predicate
            0x01, 0xc6, 0x06,            // Result: 1 diamond sword (ID 838)
            0x01, 0x00, 0x03, 0x82, 0x01, // 1 added, 0 removed, damage 130
            0x01,                        // Cost B present
            0x9d, 0x07, 0x02,            // Cost B: book ID 925, count 2
            0x01, 0x04, 0x01,            // One exact component: unbreakable, show in tooltip
            0x01,                        // Out of stock
            0x00, 0x00, 0x00, 0x04,      // Uses 4
            0x00, 0x00, 0x00, 0x04,      // Max uses 4
            0x00, 0x00, 0x00, 0xb1,      // XP 177
            0x00, 0x00, 0x00, 0x01,      // Special price 1
            0x3e, 0x00, 0x00, 0x00,      // Price multiplier 0.125
            0x00, 0x00, 0x00, 0x0b,      // Demand 11
            0x03,                        // Villager level 3
            0xfa, 0x01,                  // Villager experience 250
            0x01,                        // Show villager progress
            0x00,                        // Cannot restock
        ];
        let packet = MerchantOffersPacket {
            window_id: 300,
            offers: vec![first, second],
            villager_level: 3,
            villager_xp: 250,
            show_progress: true,
            can_restock: false,
        };
        assert_eq!(MERCHANT_OFFERS_PACKET_ID, 0x2d);
        assert_eq!(encode_merchant_offers_pkt(&packet), expected);
        assert_eq!(decode_merchant_offers_pkt(&expected), Some(packet));
        for cut in 0..expected.len() {
            assert!(
                decode_merchant_offers_pkt(&expected[..cut]).is_none(),
                "cut={cut}"
            );
        }
    }

    #[test]
    fn merchant_offers_protocol_767_round_trip_all_fields() {
        let mut sold_out = offer();
        sold_out.uses = sold_out.max_uses;
        sold_out.special_price_diff = -7;
        sold_out.demand = -21;
        let one_cost = MerchantOffer {
            cost_b: None,
            ..offer()
        };
        let packet = MerchantOffersPacket {
            window_id: 173,
            offers: vec![one_cost, sold_out],
            villager_level: VillagerLevel::Journeyman as i32,
            villager_xp: 79,
            show_progress: false,
            can_restock: true,
        };
        let wire = encode_merchant_offers_pkt(&packet);
        assert_eq!(&wire[..3], &[0xad, 0x01, 0x02]);
        assert_eq!(decode_merchant_offers_pkt(&wire), Some(packet));

        let packet = MerchantOffersPacket {
            window_id: 4,
            offers: vec![MerchantOffer {
                cost_a: ItemCost {
                    item: 7,
                    count: 1,
                    components: Vec::new(),
                },
                cost_b: None,
                result: stack(2, 3),
                xp_reward: 5,
                price_multiplier: 0.5,
                demand: 6,
                special_price_diff: -1,
                ..offer()
            }],
            villager_level: VillagerLevel::Master as i32,
            villager_xp: 250,
            show_progress: true,
            can_restock: false,
        };
        let mut expected = vec![4, 1, 7, 1, 0, 3, 2, 0, 0, 0, 0];
        expected.extend_from_slice(&0i32.to_be_bytes());
        expected.extend_from_slice(&4i32.to_be_bytes());
        expected.extend_from_slice(&5i32.to_be_bytes());
        expected.extend_from_slice(&(-1i32).to_be_bytes());
        expected.extend_from_slice(&0.5f32.to_be_bytes());
        expected.extend_from_slice(&6i32.to_be_bytes());
        expected.extend_from_slice(&[5, 0xfa, 0x01, 1, 0]);
        assert_eq!(encode_merchant_offers_pkt(&packet), expected);
        assert_eq!(decode_merchant_offers_pkt(&expected), Some(packet));
    }

    #[test]
    fn merchant_offers_varint_list_length_and_malformed_payloads() {
        let mut packet = MerchantOffersPacket {
            window_id: 1,
            offers: vec![
                MerchantOffer {
                    cost_a: ItemCost {
                        item: 13,
                        count: 20,
                        components: Vec::new()
                    },
                    cost_b: None,
                    result: stack(42, 3),
                    ..offer()
                };
                130
            ],
            villager_level: VillagerLevel::Novice as i32,
            villager_xp: 0,
            show_progress: true,
            can_restock: true,
        };
        let wire = encode_merchant_offers_pkt(&packet);
        assert_eq!(&wire[..3], &[1, 0x82, 0x01]);
        assert_eq!(decode_merchant_offers_pkt(&wire).as_ref(), Some(&packet));

        packet.offers.truncate(1);
        let base = encode_merchant_offers_pkt(&packet);
        for cut in 0..base.len() {
            assert!(
                decode_merchant_offers_pkt(&base[..cut]).is_none(),
                "cut={cut}"
            );
        }
        for (offset, value) in [(4, 1), (8, 1), (9, 2), (10, 2)] {
            let mut malformed = base.clone();
            malformed[offset] = value;
            assert!(
                decode_merchant_offers_pkt(&malformed).is_none(),
                "offset={offset}"
            );
        }
        assert!(decode_merchant_offers_pkt(&[base.as_slice(), &[0]].concat()).is_none());
        assert!(decode_merchant_offers_pkt(&[1, 0xff, 0xff, 0xff, 0xff, 0x07]).is_none());

        let mut flagged = base;
        flagged[10] = 1;
        let decoded = decode_merchant_offers_pkt(&flagged).unwrap();
        assert_eq!(decoded.offers[0].uses, decoded.offers[0].max_uses);

        packet.offers[0].result = Stack::default();
        let wire = encode_merchant_offers_pkt(&packet);
        assert_eq!(&wire[..8], &[1, 1, 13, 20, 0, 0, 0, 0]);
        assert_eq!(decode_merchant_offers_pkt(&wire), Some(packet));
    }

    #[test]
    fn select_trade_payload_is_one_signed_varint() {
        assert_eq!(SELECT_TRADE_PACKET_ID, 0x2d);
        assert_eq!(decode_select_trade(&[0]), Some(0));
        assert_eq!(decode_select_trade(&[0x82, 0x01]), Some(130));
        assert_eq!(
            decode_select_trade(&[0xff, 0xff, 0xff, 0xff, 0x0f]),
            Some(-1)
        );
        for malformed in [
            &[][..],
            &[0x80],
            &[0x00, 0x00],
            &[0x80, 0x80, 0x80, 0x80, 0x10],
        ] {
            assert_eq!(decode_select_trade(malformed), None);
        }
    }

    #[test]
    fn trade_transaction_validates_items_discounts_and_levels() {
        let (pois, mut villager) = village();
        villager.offers.push(offer());
        villager.gossip.cure(PLAYER);
        assert!(villager.start_trading(&pois, PLAYER, Some(1)));
        let expected = villager.offers[0].clone();
        let mut inputs = [stack(WHEAT, 10), stack(BOOK, 3)];
        let result = villager
            .execute_trade_transaction(0, Some(&expected), PLAYER, &mut inputs, &items())
            .unwrap();
        assert_eq!(result.result, stack(EMERALD, 3));
        assert_eq!((result.consumed_a, result.consumed_b), (7, 2));
        assert_eq!(inputs, [stack(WHEAT, 3), stack(BOOK, 1)]);
        assert_eq!(result.villager_xp, 10);
        assert_eq!(result.level_before, VillagerLevel::Novice);
        assert_eq!(result.level_after, VillagerLevel::Apprentice);
        assert_eq!(villager.offers[0].uses, 1);
        assert_eq!(villager.gossip.reputation(PLAYER), 127);

        let mut villager = trading_villager([MerchantOffer {
            cost_b: None,
            ..offer()
        }]);
        let mut inputs = [stack(WHEAT, 20), stack(BOOK, 8)];
        assert_eq!(
            villager.execute_trade_transaction(0, None, PLAYER, &mut inputs, &items()),
            Err(TradeRejection::WrongItemB)
        );
        assert_eq!(inputs, [stack(WHEAT, 20), stack(BOOK, 8)]);
        inputs[1] = Stack::default();
        let result = villager
            .execute_trade_transaction(0, None, PLAYER, &mut inputs, &items())
            .unwrap();
        assert_eq!(result.consumed_b, 0);
        assert_eq!(inputs, [Stack::default(), Stack::default()]);
    }

    #[test]
    fn rejected_trades_leave_inputs_usage_xp_and_gossip_intact() {
        let mut villager = trading_villager([offer()]);
        let expected = villager.offers[0].clone();
        for (inputs, rejection) in [
            (
                [stack(EMERALD, 20), stack(BOOK, 2)],
                TradeRejection::WrongItemA,
            ),
            (
                [stack(WHEAT, 19), stack(BOOK, 2)],
                TradeRejection::InsufficientPaymentA,
            ),
            (
                [stack(WHEAT, 20), stack(EMERALD, 2)],
                TradeRejection::WrongItemB,
            ),
            (
                [stack(WHEAT, 20), stack(BOOK, 1)],
                TradeRejection::InsufficientPaymentB,
            ),
        ] {
            let mut actual = inputs.clone();
            assert_eq!(
                villager.execute_trade_transaction(
                    0,
                    Some(&expected),
                    PLAYER,
                    &mut actual,
                    &items()
                ),
                Err(rejection)
            );
            assert_eq!(actual, inputs);
            assert_eq!(villager.offers[0], expected);
            assert_eq!(villager.villager_xp, 0);
            assert_eq!(villager.gossip.reputation(PLAYER), 0);
        }

        let inputs = [stack(WHEAT, 20), stack(BOOK, 2)];
        let mut actual = inputs.clone();
        let mut stale = expected.clone();
        stale.cost_a.item = ENDER_PEARL;
        assert_eq!(
            villager.execute_trade_transaction(0, Some(&stale), PLAYER, &mut actual, &items()),
            Err(TradeRejection::StaleOffer)
        );
        villager.offers[0].uses = 4;
        assert_eq!(
            villager.execute_trade_transaction(0, Some(&expected), PLAYER, &mut actual, &items()),
            Err(TradeRejection::StaleOffer)
        );
        let sold_out = villager.offers[0].clone();
        assert_eq!(
            villager.execute_trade_transaction(0, Some(&sold_out), PLAYER, &mut actual, &items()),
            Err(TradeRejection::OutOfStock)
        );
        assert_eq!(villager.offers[0], sold_out);
        villager.offers[0] = MerchantOffer {
            result: Stack::default(),
            ..expected
        };
        assert_eq!(
            villager.execute_trade_transaction(0, None, PLAYER, &mut actual, &items()),
            Err(TradeRejection::InvalidOffer)
        );
        assert_eq!(actual, inputs);
        assert_eq!(villager.offers[0].uses, 0);
        assert_eq!(villager.villager_xp, 0);
        assert_eq!(villager.gossip.reputation(PLAYER), 0);
    }

    #[test]
    fn selecting_returns_old_payment_and_autofills_compatible_stacks_in_menu_order() {
        let offer = offer();
        let mut menu = MerchantMenu {
            payments: [stack(EMERALD, 2), Stack::default()],
            ..MerchantMenu::default()
        };
        let explicit_default = (WHEAT, 30, vec![(1, vec![64])]);
        let variant = (WHEAT, 7, vec![(COMP_UNBREAKABLE, vec![1])]);
        let mut inventory = HashMap::from([
            (9, explicit_default),
            (10, variant.clone()),
            (11, stack(WHEAT, 40)),
            (12, stack(BOOK, 3)),
            (45, stack(BOOK, 8)),
        ]);
        menu.select_trade(0, std::slice::from_ref(&offer), &mut inventory, &items());

        assert_eq!(menu.payments, [stack(WHEAT, 64), stack(BOOK, 3)]);
        assert_eq!(
            inventory,
            HashMap::from([
                (10, variant),
                (11, stack(WHEAT, 6)),
                (44, stack(EMERALD, 2)),
                (45, stack(BOOK, 8)),
            ])
        );
        assert_eq!(menu.active_offer(&[offer], &items()), Some(0));
    }

    #[test]
    fn selecting_with_a_full_inventory_retains_unreturned_payments() {
        let mut menu = MerchantMenu {
            payments: [stack(WHEAT, 4), stack(BOOK, 2)],
            ..MerchantMenu::default()
        };
        let mut inventory = (9..=44)
            .map(|slot| (slot, stack(EMERALD, 64)))
            .collect::<HashMap<_, _>>();
        inventory.insert(44, stack(WHEAT, 62));
        inventory.insert(43, stack(WHEAT, 63));
        menu.select_trade(0, &[offer()], &mut inventory, &items());

        assert_eq!(inventory[&44], stack(WHEAT, 64));
        assert_eq!(inventory[&43], stack(WHEAT, 64));
        assert_eq!(menu.payments, [stack(WHEAT, 1), stack(BOOK, 2)]);
    }

    #[test]
    fn trade_matches_effective_components_and_consumes_swapped_payments() {
        let mut offer = offer();
        offer.cost_a.components = vec![(1, vec![64])];
        offer.cost_b.as_mut().unwrap().components = vec![(COMP_UNBREAKABLE, vec![1])];
        offer.result.2 = vec![(1, vec![32])];
        let result = offer.result.clone();
        let mut villager = trading_villager([offer]);
        let extra_component = (WHEAT, 21, vec![(COMP_UNBREAKABLE, vec![1])]);
        let mut menu = MerchantMenu {
            payments: [
                (BOOK, 3, vec![(COMP_UNBREAKABLE, vec![0])]),
                extra_component,
            ],
            ..MerchantMenu::default()
        };
        let before = menu.payments.clone();
        assert_eq!(villager.trade(PLAYER, &mut menu, &items()), None);
        assert_eq!(menu.payments, before);
        assert_eq!(villager.offers[0].uses, 0);

        menu.payments[0].2[0].1[0] = 1;
        assert_eq!(
            villager
                .trade(PLAYER, &mut menu, &items())
                .map(|trade| (trade.result, trade.villager_xp)),
            Some((result, 10))
        );
        assert_eq!(
            menu.payments,
            [
                (BOOK, 1, vec![(COMP_UNBREAKABLE, vec![1])]),
                (WHEAT, 1, vec![(COMP_UNBREAKABLE, vec![1])]),
            ]
        );
        assert_eq!(villager.offers[0].uses, 1);
    }

    #[test]
    fn one_cost_requires_an_empty_second_payment_and_matches_slot_one_alone() {
        let mut villager = trading_villager([MerchantOffer {
            cost_b: None,
            ..offer()
        }]);
        let mut menu = MerchantMenu {
            payments: [stack(WHEAT, 20), stack(BOOK, 2)],
            ..MerchantMenu::default()
        };
        assert_eq!(villager.trade(PLAYER, &mut menu, &items()), None);
        assert_eq!(menu.payments, [stack(WHEAT, 20), stack(BOOK, 2)]);
        menu.payments = [Stack::default(), stack(WHEAT, 20)];
        assert_eq!(
            villager
                .trade(PLAYER, &mut menu, &items())
                .map(|trade| (trade.result, trade.villager_xp)),
            Some((stack(EMERALD, 3), 10))
        );
        assert_eq!(menu.payments, [Stack::default(), Stack::default()]);
    }

    #[test]
    fn stock_and_selection_hint_gate_the_result_and_execution() {
        let mut villager = trading_villager([
            MerchantOffer {
                uses: 1,
                max_uses: 1,
                ..offer()
            },
            MerchantOffer {
                max_uses: 1,
                ..offer()
            },
        ]);
        let mut menu = MerchantMenu::default();
        let mut inventory = HashMap::from([(9, stack(WHEAT, 64)), (10, stack(BOOK, 4))]);

        menu.select_trade(0, &villager.offers, &mut inventory, &items());
        assert_eq!(menu.payments, [stack(WHEAT, 64), stack(BOOK, 4)]);
        assert_eq!(menu.active_offer(&villager.offers, &items()), None);
        assert_eq!(villager.trade(PLAYER, &mut menu, &items()), None);
        assert_eq!(menu.payments, [stack(WHEAT, 64), stack(BOOK, 4)]);

        menu.select_trade(1, &villager.offers, &mut inventory, &items());
        assert_eq!(menu.active_offer(&villager.offers, &items()), Some(1));
        assert!(villager.trade(PLAYER, &mut menu, &items()).is_some());
        assert_eq!(menu.active_offer(&villager.offers, &items()), None);
        assert_eq!(villager.trade(PLAYER, &mut menu, &items()), None);
        assert_eq!(menu.payments, [stack(WHEAT, 44), stack(BOOK, 2)]);
    }

    #[test]
    fn invalid_selection_skips_autofill_and_uses_the_vanilla_recipe_fallback() {
        let mut offers = [offer(), offer()];
        offers[1].cost_a.item = ENDER_PEARL;
        let mut menu = MerchantMenu {
            payments: [stack(WHEAT, 20), stack(BOOK, 2)],
            selection_hint: 1,
        };
        let mut inventory = HashMap::from([(9, stack(WHEAT, 51))]);
        assert_eq!(menu.active_offer(&offers, &items()), None);
        for index in [-1, 2, i32::MAX] {
            menu.select_trade(index, &offers, &mut inventory, &items());
            assert_eq!(menu.active_offer(&offers, &items()), Some(0));
            assert_eq!(menu.payments, [stack(WHEAT, 20), stack(BOOK, 2)]);
            assert_eq!(inventory, HashMap::from([(9, stack(WHEAT, 51))]));
        }
    }

    #[test]
    fn adjusted_cost_is_clamped_to_the_java_one_to_sixty_four_range() {
        let mut offer = offer();
        offer.cost_a.item = ENDER_PEARL;
        offer.cost_a.count = 5;
        offer.price_multiplier = 0.5;
        offer.demand = 9;
        assert_eq!(offer.cost_a_count(&items()), 27);
        offer.cost_a.components = vec![(1, vec![7])];
        assert_eq!(offer.cost_a_count(&items()), 27);
        offer.demand = -9;
        assert_eq!(offer.cost_a_count(&items()), 5);
        offer.special_price_diff = -9;
        assert_eq!(offer.cost_a_count(&items()), 1);
    }

    #[test]
    fn trade_consumes_the_opening_price_with_demand_and_customer_discounts() {
        let (pois, mut villager) = village();
        villager.offers.push(MerchantOffer {
            demand: 3,
            special_price_diff: 4,
            ..offer()
        });
        villager.gossip.cure(PLAYER);
        assert!(villager.start_trading(&pois, PLAYER, Some(0)));
        assert!(!villager.start_trading(&pois, OTHER_PLAYER, None));
        assert_eq!(villager.offers[0].special_price_diff, -8);
        assert_eq!(villager.offers[0].cost_a_count(&items()), 15);
        let mut menu = MerchantMenu {
            payments: [stack(WHEAT, 14), stack(BOOK, 4)],
            ..MerchantMenu::default()
        };
        assert_eq!(villager.trade(PLAYER, &mut menu, &items()), None);
        assert_eq!(menu.payments, [stack(WHEAT, 14), stack(BOOK, 4)]);
        menu.payments[0].1 = 30;
        assert_eq!(villager.trade(OTHER_PLAYER, &mut menu, &items()), None);
        assert_eq!(
            villager
                .trade(PLAYER, &mut menu, &items())
                .map(|trade| trade.result),
            Some(stack(EMERALD, 3))
        );
        assert_eq!(menu.payments, [stack(WHEAT, 15), stack(BOOK, 2)]);
        villager.gossip.add(PLAYER, GossipType::MinorNegative, 100);
        assert_eq!(
            villager
                .trade(PLAYER, &mut menu, &items())
                .map(|trade| trade.result),
            Some(stack(EMERALD, 3))
        );
        assert_eq!(menu.payments, [Stack::default(), Stack::default()]);
        villager.stop_trading();
        assert!(villager.start_trading(&pois, OTHER_PLAYER, None));
        assert_eq!(villager.offers[0].cost_a_count(&items()), 23);
    }

    #[test]
    fn gossip_at_a_shared_bell_transfers_rumors_and_applies_daily_decay() {
        let (mut pois, mut villager) = village();
        let mut neighbor = Villager::new([4; 16], VillagerProfession::None);
        assert!(neighbor.claim_poi(&mut pois, BELL));
        villager.gossip.cure(PLAYER);
        neighbor
            .gossip
            .add(OTHER_PLAYER, GossipType::MajorNegative, 20);
        neighbor
            .gossip
            .add(OTHER_PLAYER, GossipType::MinorNegative, 1_000);
        neighbor
            .gossip
            .add(OTHER_PLAYER, GossipType::Trading, 1_000);
        assert!(neighbor.gossip.golem_hostile(OTHER_PLAYER));
        assert_eq!(neighbor.gossip.reputation(OTHER_PLAYER), -275);
        assert!(!villager.gossip_at_bell(&mut neighbor, &pois, [BELL, BELL], 8_999));
        assert!(!villager.gossip_at_bell(&mut neighbor, &pois, [BELL, (12, 64, 0)], 9_000));
        assert!(villager.gossip_at_bell(&mut neighbor, &pois, [BELL, BELL], 9_000));
        assert_eq!(neighbor.gossip.reputation(PLAYER), 20);
        assert_eq!(villager.gossip.reputation(OTHER_PLAYER), -225);
        assert!(!villager.gossip_at_bell(&mut neighbor, &pois, [BELL, BELL], 10_199));
        assert!(villager.gossip_at_bell(&mut neighbor, &pois, [BELL, BELL], 10_200));

        villager.tick(&pois, BELL, 10_200);
        villager.tick(&pois, BELL, 24_000);
        assert_eq!(villager.gossip.reputation(PLAYER), 124);
        assert_eq!(villager.gossip.reputation(OTHER_PLAYER), -157);
        villager.tick(&pois, BELL, 30 * 24_000);
        assert_eq!(villager.gossip.reputation(PLAYER), 100);
        assert!(!villager.gossip.golem_hostile(OTHER_PLAYER));
        let mut gossip = VillagerGossip::default();
        gossip.add(PLAYER, GossipType::MinorNegative, 100);
        assert!(gossip.golem_hostile(PLAYER));
        gossip.add(PLAYER, GossipType::Trading, 2);
        assert!(!gossip.golem_hostile(PLAYER));
    }

    #[test]
    fn daily_schedule_selects_targets_and_sleeps_only_at_a_claimed_bed() {
        use VillagerActivity::*;
        let (mut pois, mut villager) = village();
        for (time, activity, target) in [
            (0, Idle, None),
            (1_999, Idle, None),
            (2_000, Work, Some(JOB_SITE)),
            (8_999, Work, Some(JOB_SITE)),
            (9_000, Meet, Some(BELL)),
            (10_999, Meet, Some(BELL)),
            (11_000, Idle, None),
            (11_999, Idle, None),
            (12_000, Rest, Some(BED)),
            (23_999, Rest, Some(BED)),
            (24_000, Idle, None),
        ] {
            assert_eq!(villager.tick(&pois, BED, time), (activity, target));
            assert_eq!(villager.sleeping, activity == Rest);
        }
        assert_eq!(villager.tick(&pois, AT_WORK, 36_000), (Rest, Some(BED)));
        assert!(!villager.sleeping);
        pois.remove(BED);
        assert_eq!(villager.tick(&pois, BED, 36_001), (Rest, None));
        assert!(!villager.sleeping);
    }

    #[test]
    fn restocking_requires_work_at_the_job_site_and_observes_the_cooldown_and_daily_limit() {
        let (pois, mut villager) = village();
        villager.offers.push(MerchantOffer { uses: 4, ..offer() });
        for (time, pos, uses, restocks) in [
            (1_999, AT_WORK, 4, 0),
            (2_000, BED, 4, 0),
            (2_001, AT_WORK, 0, 1),
            (4_000, AT_WORK, 4, 1),
            (4_001, AT_WORK, 0, 2),
            (6_001, AT_WORK, 4, 2),
            (8_999, AT_WORK, 4, 2),
            (9_000, AT_WORK, 4, 2),
            (24_000, AT_WORK, 4, 0),
            (26_000, AT_WORK, 0, 1),
        ] {
            villager.offers[0].uses = 4;
            villager.tick(&pois, pos, time);
            assert_eq!(
                (villager.offers[0].uses, villager.restocks_today),
                (uses, restocks),
                "tick {time}"
            );
        }
    }

    fn start_raid(pois: &PoiRegistry, difficulty: RaidDifficulty, level: u8) -> Raid {
        let mut omen = Some(RaidOmen {
            bad_omen_level: level,
            countdown: 0,
        });
        let raid = Raid::try_start(pois, BED, PLAYER, &mut omen, difficulty).unwrap();
        assert!(omen.is_none());
        raid
    }

    fn next_wave(raid: &mut Raid, pois: &PoiRegistry) -> RaidWave {
        for _ in 1..300 {
            assert_eq!(raid.tick(pois, 1), None);
        }
        let Some(RaidEvent::SpawnWave(wave)) = raid.tick(pois, 1) else {
            panic!("expected the next wave on the 300th tick");
        };
        wave
    }

    #[test]
    fn trigger_consumes_raid_omen_only_near_claimed_pois_and_averages_their_positions() {
        let mut pois = PoiRegistry::default();
        let west = (-48, 64, -1);
        let east = (32, 66, -1);
        let far = (48, 64, -1);
        let pos = (-1, 64, -1);
        let mut villager = Villager::new([3; 16], VillagerProfession::None);
        let mut omen = Some(RaidOmen {
            bad_omen_level: 2,
            countdown: 0,
        });
        for (spot, kind) in [
            (west, PoiKind::Bed),
            (east, PoiKind::Bell),
            (far, PoiKind::JobSite(VillagerProfession::Mason)),
        ] {
            pois.register(spot, kind);
        }
        assert!(Raid::try_start(&pois, pos, PLAYER, &mut omen, RaidDifficulty::Hard).is_none());
        assert_eq!(
            omen,
            Some(RaidOmen {
                bad_omen_level: 2,
                countdown: 0
            })
        );
        for spot in [west, east, far] {
            assert!(villager.claim_poi(&mut pois, spot));
        }
        assert!(Raid::try_start(&pois, pos, PLAYER, &mut None, RaidDifficulty::Hard).is_none());
        assert!(Raid::try_start(&pois, pos, PLAYER, &mut omen, RaidDifficulty::Peaceful).is_none());
        let raid = Raid::try_start(&pois, pos, PLAYER, &mut omen, RaidDifficulty::Hard).unwrap();
        assert_eq!(raid.center, (-8, 65, -1));
        assert_eq!(raid.state, RaidState::InProgress);
        assert_eq!(omen, None);
    }

    #[test]
    fn ominous_bottle_bad_omen_and_village_raid_omen_count_down_for_600_ticks() {
        let (pois, _) = village();
        for amplifier in [0, 1, 2, 3, 4, 255] {
            let bad_omen = BadOmen::from_ominous_bottle(amplifier);
            assert_eq!(bad_omen.duration_ticks, 120_000);
            assert_eq!(bad_omen.level, amplifier.saturating_add(1).clamp(1, 5));

            let mut raid_omen = bad_omen.enter_village();
            assert_eq!(raid_omen.countdown, 600);
            let mut active_omen = Some(raid_omen);
            assert!(
                Raid::try_start(&pois, BED, PLAYER, &mut active_omen, RaidDifficulty::Normal)
                    .is_none()
            );
            assert!(active_omen.is_some());
            for _ in 0..599 {
                assert!(!raid_omen.tick());
            }
            assert_eq!(raid_omen.countdown, 1);
            assert!(raid_omen.tick());
            assert_eq!(raid_omen.countdown, 0);

            active_omen = Some(raid_omen);
            let raid =
                Raid::try_start(&pois, BED, PLAYER, &mut active_omen, RaidDifficulty::Normal)
                    .expect("raid begins when Raid Omen expires");
            assert_eq!(raid.omen_level, bad_omen.level);
            assert!(active_omen.is_none());
        }
    }

    #[test]
    fn all_difficulties_spawn_the_vanilla_base_waves_and_repeat_the_final_row_for_a_bonus() {
        let (pois, _) = village();
        let expected = [
            [0, 0, 4, 0, 0],
            [2, 0, 3, 0, 0],
            [0, 0, 3, 0, 1],
            [1, 0, 4, 3, 0],
            [4, 1, 4, 0, 1],
            [2, 1, 4, 0, 0],
            [5, 2, 2, 1, 2],
        ];
        for (difficulty, regular) in [
            (RaidDifficulty::Easy, 3),
            (RaidDifficulty::Normal, 5),
            (RaidDifficulty::Hard, 7),
        ] {
            for (level, bonus) in [(1, 0), (2, 1)] {
                let mut raid = start_raid(&pois, difficulty, level);
                for number in 1..=regular + bonus {
                    let wave = next_wave(&mut raid, &pois);
                    assert_eq!(wave.number, number);
                    assert_eq!(wave.counts, expected[usize::from(number.min(regular) - 1)]);
                    let captain = wave.captain.expect("each wave has a banner captain");
                    assert!(wave.raiders().any(|(id, _)| id == captain));
                    for _ in 0..301 {
                        assert_eq!(raid.tick(&pois, 1), None);
                    }
                    let mut counts = [0; 5];
                    for (raider, kind) in wave.raiders() {
                        counts[kind as usize] += 1;
                        assert!(raid.record_defeat(raider, Some(OTHER_PLAYER)));
                        assert!(!raid.record_defeat(raider, Some([9; 16])));
                    }
                    assert_eq!(counts, wave.counts);
                }
                assert_eq!(
                    raid.tick(&pois, 1),
                    Some(RaidEvent::Victory {
                        participants: vec![PLAYER, OTHER_PLAYER],
                        effect: HeroOfTheVillage {
                            level,
                            duration_ticks: 48_000
                        },
                    })
                );
                assert_eq!(raid.state, RaidState::Victory);
                assert_eq!(raid.tick(&pois, 1), None);
                assert_eq!(raid.state, RaidState::Celebrating);
                for _ in 1..600 {
                    assert_eq!(raid.tick(&pois, 1), None);
                }
                assert_eq!(raid.state, RaidState::Stopped);
                assert_eq!(raid.tick(&pois, 1), None);
            }
        }
    }

    #[test]
    fn a_raid_loses_when_the_last_villager_or_claimed_bed_is_lost() {
        let (mut pois, _) = village();
        let mut no_villagers = start_raid(&pois, RaidDifficulty::Easy, 1);
        for _ in 0..3 {
            let wave = next_wave(&mut no_villagers, &pois);
            for (id, _) in wave.raiders() {
                assert!(no_villagers.record_defeat(id, None));
            }
        }
        assert_eq!(no_villagers.tick(&pois, 0), Some(RaidEvent::Loss));
        assert_eq!(no_villagers.state, RaidState::Loss);

        let mut no_beds = start_raid(&pois, RaidDifficulty::Easy, 1);
        let wave = next_wave(&mut no_beds, &pois);
        pois.remove(BED);
        pois.register(BED, PoiKind::Bed);
        assert_eq!(no_beds.tick(&pois, 1), Some(RaidEvent::Loss));
        assert_eq!(no_beds.state, RaidState::Loss);
        assert!(!no_beds.record_defeat(wave.raiders().next().unwrap().0, Some(PLAYER)));
        assert_eq!(no_beds.tick(&pois, 1), None);
        assert_eq!(no_beds.state, RaidState::Celebrating);
    }

    #[test]
    fn a_raid_loses_after_the_vanilla_timeout() {
        let (pois, _) = village();
        let mut raid = start_raid(&pois, RaidDifficulty::Easy, 1);
        raid.ticks_elapsed = RAID_TIMEOUT_TICKS - 1;
        assert_eq!(raid.tick(&pois, 1), Some(RaidEvent::Loss));
        assert_eq!(raid.state, RaidState::Loss);
    }

    #[test]
    fn hero_discount_is_session_scoped_and_reopening_never_stacks_it() {
        let (pois, mut villager) = village();
        villager.offers.push(offer());
        villager.gossip.cure(PLAYER);

        // Hero V (amplifier 4): floor(20 * 0.55) = 11 plus the fixed reputation
        // adjustment floor(125 * 0.05) = 6. The opening price is 20 - 17 = 3.
        assert!(villager.start_trading(&pois, PLAYER, Some(4)));
        assert_eq!(villager.offers[0].special_price_diff, -17);
        assert_eq!(villager.offers[0].cost_a_count(&items()), 3);
        assert!(!villager.start_trading(&pois, PLAYER, Some(4)));
        assert_eq!(villager.offers[0].special_price_diff, -17);

        let mut menu = MerchantMenu {
            payments: [stack(WHEAT, 3), stack(BOOK, 2)],
            ..MerchantMenu::default()
        };
        let outcome = villager
            .trade(PLAYER, &mut menu, &items())
            .expect("the discounted opening price is payable");
        assert_eq!((outcome.consumed_a, outcome.consumed_b), (3, 2));
        assert_eq!(villager.offers[0].special_price_diff, -17);

        villager.stop_trading();
        assert_eq!(villager.offers[0].special_price_diff, 0);
        assert_eq!(villager.offers[0].cost_a_count(&items()), 20);
        assert!(villager.start_trading(&pois, PLAYER, Some(4)));
        assert_eq!(villager.offers[0].special_price_diff, -17);

        // A new session with a weaker effect replaces the old discount instead of
        // adding to it.
        villager.stop_trading();
        assert!(villager.start_trading(&pois, PLAYER, Some(0)));
        assert_eq!(villager.offers[0].special_price_diff, -12);

        // Even an out-of-range amplifier can never push the price below one.
        villager.stop_trading();
        villager.offers[0].cost_a.count = 2;
        assert!(villager.start_trading(&pois, PLAYER, Some(u8::MAX)));
        assert_eq!(villager.offers[0].cost_a_count(&items()), 1);
    }

    #[test]
    fn max_omen_amplifier_adds_exactly_one_bonus_wave_and_caps_the_hero_level() {
        let (pois, _) = village();
        let no_bonus = BadOmen::from_ominous_bottle(0);
        assert_eq!(no_bonus.level, 1);
        let raid = start_raid(&pois, RaidDifficulty::Easy, no_bonus.enter_village().bad_omen_level);
        assert_eq!(raid.total_waves, raid.regular_waves);

        for amplifier in [4, 5, u8::MAX] {
            let bad_omen = BadOmen::from_ominous_bottle(amplifier);
            assert_eq!(bad_omen.level, 5);
            assert_eq!(bad_omen.duration_ticks, BAD_OMEN_TICKS);
            let mut raid = start_raid(
                &pois,
                RaidDifficulty::Easy,
                bad_omen.enter_village().bad_omen_level,
            );
            assert_eq!(raid.omen_level, 5);
            assert_eq!(raid.total_waves, raid.regular_waves + 1);

            for number in 1..=raid.total_waves {
                raid.countdown = 1;
                let Some(RaidEvent::SpawnWave(wave)) = raid.tick(&pois, 1) else {
                    panic!("expected wave {number}");
                };
                assert_eq!(wave.number, number);
                let row = usize::from(number.min(raid.regular_waves) - 1);
                assert_eq!(wave.counts, RAID_BASE_WAVES[row]);
                for (id, _) in wave.raiders() {
                    assert!(raid.record_defeat(id, Some(PLAYER)));
                }
            }
            assert_eq!(
                raid.tick(&pois, 1),
                Some(RaidEvent::Victory {
                    participants: vec![PLAYER],
                    effect: HeroOfTheVillage {
                        level: 5,
                        duration_ticks: 48_000,
                    },
                })
            );
        }
    }

    #[test]
    fn gossip_decay_removes_a_player_only_when_every_value_crosses_the_threshold() {
        let mut gossip = VillagerGossip::default();
        gossip.add(PLAYER, GossipType::MinorPositive, 1);
        assert!(!gossip.by_player.contains_key(&PLAYER));
        gossip.add(PLAYER, GossipType::MinorPositive, 2);
        assert_eq!(
            gossip.by_player[&PLAYER][GossipType::MinorPositive as usize],
            2
        );
        gossip.decay(0);
        assert_eq!(
            gossip.by_player[&PLAYER][GossipType::MinorPositive as usize],
            2
        );
        gossip.decay(1);
        assert!(!gossip.by_player.contains_key(&PLAYER));

        gossip.add(PLAYER, GossipType::MinorPositive, 3);
        gossip.decay(1);
        assert_eq!(
            gossip.by_player[&PLAYER][GossipType::MinorPositive as usize],
            2
        );
        gossip.decay(1);
        assert!(!gossip.by_player.contains_key(&PLAYER));

        // Major Positive gossip never decays, so it keeps the entry alive while
        // every decaying type is removed by the clamped 25-day decay.
        gossip.add(PLAYER, GossipType::MajorPositive, 20);
        gossip.add(PLAYER, GossipType::MajorNegative, 100);
        gossip.add(PLAYER, GossipType::MinorNegative, 200);
        gossip.add(PLAYER, GossipType::Trading, 25);
        gossip.decay(u64::MAX);
        assert_eq!(gossip.by_player[&PLAYER], [0, 0, 0, 20, 0]);
        assert_eq!(gossip.reputation(PLAYER), 100);
        assert!(!gossip.golem_hostile(PLAYER));
    }

    /// The scan-and-claim step `open_live_merchant` runs before the table
    /// opens: a lectern next to the villager claims into Librarian with the
    /// real site trades, so the `farmer_merchant` fallback never applies.
    #[test]
    #[cfg(megu_vanilla_data)]
    fn live_poi_assigns_non_farmer_villager_profession_and_trades() {
        let _guard = crate::world_guard();
        let lectern = (50_000, 70, 50_000);
        let old = block_at(lectern.0, lectern.1, lectern.2);
        set_block(
            lectern.0,
            lectern.1,
            lectern.2,
            name_to_state("lectern").unwrap(),
        );
        let mut villager = Villager::new([7; 16], VillagerProfession::None);
        live_villager_bind_pois(
            DimId::Overworld,
            (lectern.0 + 2, lectern.1, lectern.2),
            &mut villager,
        );
        let (profession, job_site, offers) = (
            villager.profession,
            villager.job_site,
            villager.offers.clone(),
        );
        let claim_holds = {
            let registries = live_pois().lock().unwrap_or_else(|e| e.into_inner());
            registries.get(&DimId::Overworld).is_some_and(|pois| {
                pois.has_claim(
                    lectern,
                    PoiKind::JobSite(VillagerProfession::Librarian),
                    villager.id,
                )
            })
        };
        // Restore the cell and reconcile the record out of the live registry.
        set_block(lectern.0, lectern.1, lectern.2, old);
        live_poi_scan(DimId::Overworld, lectern);

        assert_eq!(profession, VillagerProfession::Librarian);
        assert_eq!(job_site, Some(lectern));
        assert!(claim_holds);
        assert!(!offers.is_empty());
        let (paper, bookshelf, wheat, bread) = (
            name_to_item("paper").unwrap(),
            name_to_item("bookshelf").unwrap(),
            name_to_item("wheat").unwrap(),
            name_to_item("bread").unwrap(),
        );
        assert!(offers.iter().any(|offer| offer.cost_a.item == paper));
        assert!(offers.iter().any(|offer| offer.result.0 == bookshelf));
        // None of the farmer fallback rows leaked in.
        assert!(!offers.iter().any(|offer| offer.cost_a.item == wheat));
        assert!(!offers.iter().any(|offer| offer.result.0 == bread));
    }

    /// Swap-and-restore guard for the process-wide raid/villager globals so
    /// neighbouring tests see their own state back afterwards.
    struct LiveRaidFixture {
        clients: Vec<Arc<Client>>,
        mob_list: Vec<Arc<Mob>>,
        raids: Vec<(DimId, Raid)>,
        villagers: HashMap<i32, Villager>,
        pois: HashMap<DimId, PoiRegistry>,
        omens: HashMap<VillageUuid, RaidOmen>,
        raiders: HashMap<i32, (DimId, BlockPos, RaidRaiderId)>,
        totals: HashMap<(DimId, BlockPos), i32>,
        cells: Vec<(DimBlockPos, u32)>,
        mob_spawning: u8,
        difficulty: u8,
        tick: u64,
    }

    impl LiveRaidFixture {
        fn new() -> Self {
            Self {
                clients: std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner())),
                mob_list: std::mem::take(&mut *mobs().lock().unwrap_or_else(|e| e.into_inner())),
                raids: std::mem::take(&mut *saved_raids().lock().unwrap_or_else(|e| e.into_inner())),
                villagers: std::mem::take(&mut *live_villagers().lock().unwrap_or_else(|e| e.into_inner())),
                pois: std::mem::take(&mut *live_pois().lock().unwrap_or_else(|e| e.into_inner())),
                omens: std::mem::take(&mut *live_raid_omens().lock().unwrap_or_else(|e| e.into_inner())),
                raiders: std::mem::take(&mut *live_raid_mobs().lock().unwrap_or_else(|e| e.into_inner())),
                totals: std::mem::take(&mut *live_raid_wave_totals().lock().unwrap_or_else(|e| e.into_inner())),
                cells: Vec::new(),
                mob_spawning: GR_MOB_SPAWNING.swap(1, Ordering::Relaxed),
                difficulty: DIFFICULTY.swap(1, Ordering::Relaxed),
                // A tick off the POI-rescan cadence keeps `live_villager_tick`
                // from re-scanning the world on every call below.
                tick: TICK.swap(1, Ordering::Relaxed),
            }
        }
    }

    impl std::ops::Drop for LiveRaidFixture {
        fn drop(&mut self) {
            for &((dim, x, y, z), state) in self.cells.iter().rev() {
                set_block_dim(dim, x, y, z, state);
            }
            GR_MOB_SPAWNING.store(self.mob_spawning, Ordering::Relaxed);
            DIFFICULTY.store(self.difficulty, Ordering::Relaxed);
            TICK.store(self.tick, Ordering::Relaxed);
            *clients().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.clients);
            *mobs().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.mob_list);
            *saved_raids().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.raids);
            *live_villagers().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.villagers);
            *live_pois().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.pois);
            *live_raid_omens().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.omens);
            *live_raid_mobs().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.raiders);
            *live_raid_wave_totals().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.totals);
        }
    }

    /// Bad Omen inside a claimed village starts a real raid: `saved_raids`
    /// gains it, the wave spawns live raider mobs through the tracker, and
    /// killing them advances the raid to its next wave.
    #[test]
    fn baby_schedule_food_willingness_sharing_and_bed_gate_follow_vanilla() {
        use VillagerActivity::*;
        assert_eq!(VillagerActivity::baby_at(2_999), Idle);
        assert_eq!(VillagerActivity::baby_at(3_000), Play);
        assert_eq!(VillagerActivity::baby_at(5_999), Play);
        assert_eq!(VillagerActivity::baby_at(6_000), Idle);
        assert_eq!(VillagerActivity::baby_at(10_000), Play);
        assert_eq!(VillagerActivity::baby_at(12_000), Rest);
        assert_eq!(VillagerActivity::brain_at(3_000, true, true), Panic);
        assert_eq!(VillagerActivity::brain_at(3_000, false, false), Work);

        assert_eq!(villager_food_points(3, 0, 0, 0), 12);
        assert_eq!(villager_food_points(0, 4, 4, 4), 12);
        assert!(villager_willing(villager_food_points(3, 0, 0, 0)));
        assert!(!villager_willing(11));
        assert_eq!(villager_share_food(24, 0), (18, 6, true));
        assert_eq!(villager_share_food(13, 11), (12, 12, true));
        assert_eq!(villager_share_food(12, 0), (12, 0, false));

        let (mut pois, _villager) = village();
        let spare_bed = (8, 64, 0);
        pois.register(spare_bed, PoiKind::Bed);
        assert_eq!(villager_unclaimed_bed_near(&pois, BED, 16), Some(spare_bed));
        assert!(villager_can_breed(&pois, BED, 16, 2));
        assert!(!villager_can_breed(&pois, BED, 16, 1));
        let mut other = Villager::new([8; 16], VillagerProfession::None);
        assert!(other.claim_poi(&mut pois, spare_bed));
        assert_eq!(villager_unclaimed_bed_near(&pois, BED, 16), None);
        assert!(!villager_can_breed(&pois, BED, 16, 2));
        let (age, baby) = villager_baby_age_tick(-VILLAGER_BABY_GROWTH_TICKS);
        assert_eq!((age, baby), (-VILLAGER_BABY_GROWTH_TICKS + 1, true));
        assert_eq!(villager_baby_age_tick(-1), (0, false));
    }

    #[test]
    fn zombie_conversion_and_curing_preserve_villager_identity_and_offers() {
        assert_eq!(zombie_villager_conversion_percent(RaidDifficulty::Easy), 0);
        assert_eq!(zombie_villager_conversion_percent(RaidDifficulty::Normal), 50);
        assert_eq!(zombie_villager_conversion_percent(RaidDifficulty::Hard), 100);
        let accelerants = zombie_cure_accelerants_in_cube((0, 64, 0), |pos| {
            pos == (4, 64, 4) || pos == (-4, 60, 0)
        });
        assert_eq!(accelerants, 2);
        assert_eq!(zombie_cure_accelerants_in_cube((0, 64, 0), |_| true), 14);

        let mut cure = ZombieVillagerCure::start(
            PLAYER,
            VillagerProfession::Librarian,
            70,
            vec![offer()],
            0,
        );
        assert_eq!(cure.ticks_left, ZOMBIE_VILLAGER_CURE_MIN_TICKS);
        assert!(!cure.tick_with_rolls(0, 1, 0));
        assert_eq!(cure.ticks_left, ZOMBIE_VILLAGER_CURE_MIN_TICKS - 1);
        cure.ticks_left = 15;
        assert!(cure.tick_with_rolls(14, 0, 0));
        assert_eq!(cure.ticks_left, 0);
        let (curer, profession, xp, offers) = cure.finish();
        assert_eq!((curer, profession, xp), (PLAYER, VillagerProfession::Librarian, 70));
        assert_eq!(offers, vec![offer()]);
    }

    #[test]
    fn complete_trade_pools_cover_all_professions_and_wandering_trader_timing() {
        for profession in [
            VillagerProfession::Armorer, VillagerProfession::Butcher, VillagerProfession::Cartographer,
            VillagerProfession::Cleric, VillagerProfession::Farmer, VillagerProfession::Fisherman,
            VillagerProfession::Fletcher, VillagerProfession::Leatherworker, VillagerProfession::Librarian,
            VillagerProfession::Mason, VillagerProfession::Shepherd, VillagerProfession::Toolsmith,
            VillagerProfession::Weaponsmith,
        ] {
            for level in [
                VillagerLevel::Novice, VillagerLevel::Apprentice, VillagerLevel::Journeyman,
                VillagerLevel::Expert, VillagerLevel::Master,
            ] {
                assert!(!vanilla_trade_pool(profession, level).is_empty(), "{profession:?} {level:?}");
            }
        }
        let novice = instantiate_trade_pool(VillagerProfession::Librarian, VillagerLevel::Novice, || 0);
        assert_eq!(novice.len(), 3);
        assert!(novice.iter().any(|offer| offer.cost_b.as_ref().is_some_and(|cost| cost.item == BOOK)));
        let master = instantiate_trade_pool(VillagerProfession::Toolsmith, VillagerLevel::Master, || 0);
        assert_eq!(master.len(), 1);
        assert_eq!(master[0].cost_a.count, 18); // base 13 + minimum enchantment roll 5
        assert_eq!(wandering_trader_despawn_tick(WANDERING_TRADER_DESPAWN_TICKS), (47_999, false));
        assert_eq!(wandering_trader_despawn_tick(1), (0, true));
        assert_eq!(wandering_trader_pool(false).len(), 64);
        assert_eq!(wandering_trader_pool(true).len(), 6);
    }

    #[test]
    fn hero_gift_tables_and_cooldowns_follow_vanilla() {
        assert_eq!(hero_gift_item(VillagerProfession::None, true, 0), Some("poppy"));
        assert_eq!(hero_gift_item(VillagerProfession::Nitwit, false, 0), None);
        assert_eq!(hero_gift_item(VillagerProfession::Armorer, false, 0), Some("chainmail_helmet"));
        assert_eq!(hero_gift_item(VillagerProfession::Butcher, false, 4), Some("cooked_beef"));
        assert_eq!(hero_gift_item(VillagerProfession::Librarian, false, 99), Some("book"));
        assert_eq!(hero_gift_item(VillagerProfession::Shepherd, false, 15), Some("black_wool"));
        assert_eq!(hero_gift_cooldown(0), 600);
        assert_eq!(hero_gift_cooldown(6_000), 6_600);
        assert_eq!(hero_gift_cooldown(6_001), 600);
    }

    #[test]
    fn farmer_crop_harvest_and_replant_model_follows_vanilla_food_points() {
        assert_eq!(farmer_crop_max_age("minecraft:wheat"), 7);
        assert_eq!(farmer_crop_max_age("minecraft:beetroots"), 3);
        assert_eq!(farmer_crop_food_points("minecraft:wheat"), 4);
        assert_eq!(farmer_crop_food_points("minecraft:carrots"), 2);
        assert_eq!(farmer_crop_food_points("minecraft:potatoes"), 2);
        assert_eq!(farmer_crop_food_points("minecraft:beetroots"), 1);
        assert_eq!(farmer_crop_food_points("minecraft:oak_log"), 0);
    }

    #[test]
    fn bell_glowing_uses_a_forty_eight_block_sphere_and_sixty_ticks() {
        assert_eq!(RAID_BELL_GLOWING_TICKS, 60);
        assert!(bell_glowing_range_contains((0, 64, 0), (48, 64, 0)));
        assert!(!bell_glowing_range_contains((0, 64, 0), (49, 64, 0)));
        assert!(bell_glowing_range_contains((0, 64, 0), (33, 64, 33)));
        assert!(!bell_glowing_range_contains((0, 64, 0), (34, 64, 34)));
    }

    #[test]
    fn raid_bonus_rolls_and_ravager_riders_match_the_vanilla_shape() {
        assert_eq!(raid_potential_bonus_spawns(RaiderType::Pillager, RaidDifficulty::Easy, 1, false, 1, 1), 1);
        assert_eq!(raid_potential_bonus_spawns(RaiderType::Pillager, RaidDifficulty::Easy, 1, false, 0, 1), 0);
        assert_eq!(raid_potential_bonus_spawns(RaiderType::Vindicator, RaidDifficulty::Normal, 3, false, 0, 1), 1);
        assert_eq!(raid_potential_bonus_spawns(RaiderType::Pillager, RaidDifficulty::Hard, 3, false, 0, 2), 2);
        assert_eq!(raid_potential_bonus_spawns(RaiderType::Witch, RaidDifficulty::Hard, 2, false, 0, 1), 0);
        assert_eq!(raid_potential_bonus_spawns(RaiderType::Witch, RaidDifficulty::Hard, 4, false, 0, 1), 0);
        assert_eq!(raid_potential_bonus_spawns(RaiderType::Witch, RaidDifficulty::Hard, 5, false, 0, 1), 1);
        assert_eq!(raid_potential_bonus_spawns(RaiderType::Ravager, RaidDifficulty::Hard, 8, true, 0, 1), 1);
        assert_eq!(raid_potential_bonus_spawns(RaiderType::Ravager, RaidDifficulty::Easy, 4, true, 0, 1), 0);

        let mut counts = [4, 1, 4, 0, 1];
        let riders = raid_wave_adjustments(RaidDifficulty::Normal, 5, false, &mut counts, 0, 0);
        assert_eq!(counts[RaiderType::Witch as usize], 1);
        assert_eq!(riders, vec![RavagerRider { ravager_slot: 0, rider: RaiderType::Pillager }]);
        let mut hard = [5, 2, 2, 1, 2];
        let riders = raid_wave_adjustments(RaidDifficulty::Hard, 7, false, &mut hard, 2, 1);
        assert_eq!(riders.len(), 2);
        assert!(riders.iter().any(|rider| rider.rider == RaiderType::Evoker));
        assert!(riders.iter().any(|rider| rider.rider == RaiderType::Vindicator));
    }

    #[test]
    fn live_raid_starts_on_bad_omen_and_spawns_and_advances_waves() {
        let _guard = crate::world_guard();
        let mut fixture = LiveRaidFixture::new();
        let _ = &mut fixture;
        let bad_omen = live_effect_id("bad_omen").expect("bad_omen effect id");
        let village = (60_000, 70, 60_000);
        // A claimed bed and bell make the POI set a village for raid logic.
        let (bed, bell) = (village, (village.0 + 4, village.1, village.2));
        let villager_etype = name_to_etype("villager").unwrap();
        let villager_spec = entity_spec_by_name("villager").unwrap();
        assert_eq!(villager_spec.id, villager_etype);
        let villager_mob = new_live_mob(
            villager_etype,
            DimId::Overworld,
            (
                village.0 as f64 + 2.5,
                village.1 as f64,
                village.2 as f64 + 0.5,
                0.0,
                0.0,
            ),
            villager_spec.max_health,
            None,
        );
        {
            let mut registries = live_pois().lock().unwrap_or_else(|e| e.into_inner());
            let pois = registries.entry(DimId::Overworld).or_default();
            pois.register(bed, PoiKind::Bed);
            pois.register(bell, PoiKind::Bell);
            assert!(pois.claim(bed, villager_mob.uuid));
            assert!(pois.claim(bell, villager_mob.uuid));
        }
        mobs().lock().unwrap_or_else(|e| e.into_inner()).push(Arc::clone(&villager_mob));
        // The omen carrier stands inside the village with Bad Omen.
        let (client, _receiver) = effect_tests::client_pair();
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (
            village.0 as f64 + 0.5,
            village.1 as f64,
            village.2 as f64 + 0.5,
            0.0,
            0.0,
            true,
        );
        client.effs.lock().unwrap_or_else(|e| e.into_inner()).push(Eff {
            id: bad_omen,
            amp: 0,
            left: server_ticks(u64::from(BAD_OMEN_TICKS)) as i32,
        });
        let client = Arc::new(client);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(Arc::clone(&client));

        // Bad Omen converts to a raid omen on the first tick, then counts
        // down the vanilla 600 ticks before `Raid::try_start` takes over.
        for _ in 0..=u64::from(RAID_OMEN_TICKS) {
            villager_raid_tick();
        }
        assert!(
            client
                .effs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .all(|eff| eff.id != bad_omen),
            "bad omen should have converted on village entry"
        );
        let raids_now = saved_raids().lock().unwrap_or_else(|e| e.into_inner()).len();
        assert_eq!(raids_now, 1, "a raid should start inside the village");
        // Wave one spawns after the vanilla 300-tick countdown.
        for _ in 0..=u64::from(RAID_WAVE_COUNTDOWN) {
            villager_raid_tick();
        }
        let first_wave: Vec<i32> = live_raid_mobs().lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
        assert!(
            !first_wave.is_empty(),
            "the first raid wave should spawn live raider mobs"
        );
        {
            let mob_list = mobs().lock().unwrap_or_else(|e| e.into_inner());
            let pillager = name_to_etype("pillager").unwrap();
            for &eid in &first_wave {
                let mob = mob_list
                    .iter()
                    .find(|mob| mob.eid == eid)
                    .expect("tracked raider missing from the live mob list");
                assert_eq!(mob.etype, pillager, "wave one is four pillagers");
                assert_eq!(mob.dim, DimId::Overworld);
            }
        }
        // Killing every raider counts as defeats; the next countdown then
        // advances the raid to wave two.
        {
            let mob_list = mobs().lock().unwrap_or_else(|e| e.into_inner());
            for &eid in &first_wave {
                let mob = mob_list.iter().find(|mob| mob.eid == eid).unwrap();
                *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
            }
        }
        for _ in 0..=(u64::from(RAID_WAVE_COUNTDOWN) + 30) {
            villager_raid_tick();
        }
        {
            let raids = saved_raids().lock().unwrap_or_else(|e| e.into_inner());
            let (_, raid) = raids.first().expect("the raid should still be active");
            assert_eq!(
                raid.waves_spawned, 2,
                "the raid should advance to the next wave once its raiders die"
            );
            assert_eq!(raid.state, RaidState::InProgress);
        }
        let second_wave: Vec<i32> = live_raid_mobs().lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
        assert!(
            !second_wave.is_empty() && second_wave.iter().all(|eid| !first_wave.contains(eid)),
            "wave two should spawn a fresh set of live raider mobs"
        );
    }
}

/// P2 parity tests: the special-spawner gamerules and the raid gate's live
/// atomic read.
#[cfg(all(test, megu_vanilla_data))]
mod p2_parity_village_tests {
    use super::*;

    /// Restores gamerule atomics a test changed, on every exit path.
    struct Rules(Vec<(&'static str, i32)>);
    impl Rules {
        fn take(names: &[&'static str]) -> Self {
            Rules(
                names
                    .iter()
                    .map(|name| (*name, gamerule_atomic_value(name).unwrap_or(0)))
                    .collect(),
            )
        }
    }
    impl std::ops::Drop for Rules {
        fn drop(&mut self) {
            for (name, value) in self.0.drain(..) {
                gamerule_atomic_store(name, value);
            }
        }
    }

    /// Restores stored-only gamerule map entries a test changed.
    struct StoredRules(Vec<(&'static str, i32)>);
    impl StoredRules {
        fn take(names: &[&'static str]) -> Self {
            let map = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
            StoredRules(
                names
                    .iter()
                    .map(|name| (*name, map.get(*name).copied().unwrap_or(0)))
                    .collect(),
            )
        }
        fn set(&self, name: &'static str, value: i32) {
            command_gamerules().lock().unwrap_or_else(|e| e.into_inner()).insert(name, value);
        }
    }
    impl std::ops::Drop for StoredRules {
        fn drop(&mut self) {
            let mut map = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
            for (name, value) in self.0.drain(..) {
                map.insert(name, value);
            }
        }
    }

    #[test]
    fn p2_parity_disable_raids_reads_the_live_atomic() {
        let _rules = Rules::take(&["disableRaids"]);
        gamerule_atomic_store("disableRaids", 1);
        assert!(gr_disable_raids(), "the raid gate must read the atomic /gamerule writes");
        gamerule_atomic_store("disableRaids", 0);
        assert!(!gr_disable_raids());
    }

    #[test]
    fn p2_parity_patrol_spawn_needs_day_five_daytime_roll_and_rule() {
        assert!(!eval_patrol_spawn_attempt(4, true, 0), "patrols wait for day 5");
        assert!(!eval_patrol_spawn_attempt(5, false, 0), "patrols spawn in daylight");
        assert!(!eval_patrol_spawn_attempt(5, true, 3), "the one-in-five roll gates");
        assert!(eval_patrol_spawn_attempt(5, true, 0));
        assert!(eval_patrol_spawn_attempt(120, true, 10));

        let _stored = StoredRules::take(&["doPatrolSpawning"]);
        _stored.set("doPatrolSpawning", 0);
        assert_eq!(gamerule_live_value("doPatrolSpawning"), 0);
        _stored.set("doPatrolSpawning", 1);
        assert_eq!(gamerule_live_value("doPatrolSpawning"), 1);
    }

    #[test]
    fn p2_parity_trader_spawn_chance_progression_and_rule() {
        assert!(eval_trader_spawn_attempt(25, 24));
        assert!(!eval_trader_spawn_attempt(25, 25));
        assert!(eval_trader_spawn_attempt(75, 74));
        assert!(!eval_trader_spawn_attempt(75, 99));
        assert!(!eval_trader_spawn_attempt(0, 0), "a zero chance never spawns");

        let _stored = StoredRules::take(&["doTraderSpawning"]);
        _stored.set("doTraderSpawning", 0);
        assert_eq!(gamerule_live_value("doTraderSpawning"), 0);
        _stored.set("doTraderSpawning", 1);
        assert_eq!(gamerule_live_value("doTraderSpawning"), 1);
    }

    #[test]
    fn p2_parity_warden_summon_needs_warning_four_and_rule() {
        assert!(!eval_warden_summon(true, 0));
        assert!(!eval_warden_summon(true, 3), "warnings 0-3 never summon");
        assert!(eval_warden_summon(true, 4), "warning level 4 summons the warden");
        assert!(!eval_warden_summon(false, 4), "doWardenSpawning=false blocks the summon");

        let _stored = StoredRules::take(&["doWardenSpawning"]);
        _stored.set("doWardenSpawning", 0);
        assert_eq!(gamerule_live_value("doWardenSpawning"), 0);
        _stored.set("doWardenSpawning", 1);
        assert_eq!(gamerule_live_value("doWardenSpawning"), 1);
    }

    #[test]
    fn p2_parity_insomnia_needs_three_days_night_and_open_sky() {
        assert!(!eval_insomnia_spawn(71_999, true, true, true));
        assert!(eval_insomnia_spawn(72_000, true, true, true));
        assert!(!eval_insomnia_spawn(72_000, false, true, true), "daylight blocks phantoms");
        assert!(!eval_insomnia_spawn(72_000, true, false, true));
        assert!(!eval_insomnia_spawn(72_000, true, true, false), "cover blocks phantoms");

        let _stored = StoredRules::take(&["doInsomnia"]);
        _stored.set("doInsomnia", 0);
        assert_eq!(gamerule_live_value("doInsomnia"), 0);
        _stored.set("doInsomnia", 1);
        assert_eq!(gamerule_live_value("doInsomnia"), 1);
    }

    /// The live gate: with `doInsomnia` off an insomniac player draws no
    /// phantoms; with it on the same state spawns them. The phantom entity
    /// must exist for the spawn arm to run.
    #[test]
    fn p2_parity_insomnia_live_gate_blocks_and_allows_phantoms() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let Some(phantom) = name_to_etype("phantom") else {
            return;
        };
        let _stored = StoredRules::take(&["doInsomnia"]);
        let old_clients = std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner()));
        let old_mobs = std::mem::take(&mut *mobs().lock().unwrap_or_else(|e| e.into_inner()));
        let old_time = WORLD_TIME.swap(15_000, Ordering::Relaxed);
        let old_tick = TICK.swap(500_000, Ordering::Relaxed);
        let old_spawn = {
            let mut state = special_spawn_state().lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *state)
        };
        struct Restore(
            Vec<Arc<Client>>,
            Vec<Arc<Mob>>,
            u64,
            u64,
            SpecialSpawnState,
        );
        impl std::ops::Drop for Restore {
            fn drop(&mut self) {
                *clients().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.0);
                *mobs().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.1);
                WORLD_TIME.store(self.2, Ordering::Relaxed);
                TICK.store(self.3, Ordering::Relaxed);
                *special_spawn_state().lock().unwrap_or_else(|e| e.into_inner()) =
                    std::mem::take(&mut self.4);
            }
        }
        let _restore = Restore(old_clients, old_mobs, old_time, old_tick, old_spawn);

        let (client, _receiver) = effect_tests::client_pair();
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (0.5, 150.0, 0.5, 0.0, 0.0, true);
        let uuid = client.uuid;
        let client = Arc::new(client);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(Arc::clone(&client));
        {
            let mut state = special_spawn_state().lock().unwrap_or_else(|e| e.into_inner());
            state.time_since_rest.insert(uuid, 100_000);
            // Sweep now, then force the next sweep through the tick call.
            state.insomnia_next = 500_000;
        }

        // doInsomnia=false: the sweep runs but no phantom spawns.
        command_gamerules().lock().unwrap_or_else(|e| e.into_inner()).insert("doInsomnia", 0);
        live_special_spawn_tick(&[Arc::clone(&client)]);
        assert!(
            mobs().lock().unwrap_or_else(|e| e.into_inner()).iter().all(|mob| mob.etype != phantom),
            "doInsomnia=false must block phantom spawns"
        );

        // doInsomnia=true: the same state spawns phantoms for the sleepless.
        // The first sweep rescheduled `insomnia_next`, so force it due again.
        command_gamerules().lock().unwrap_or_else(|e| e.into_inner()).insert("doInsomnia", 1);
        special_spawn_state().lock().unwrap_or_else(|e| e.into_inner()).insomnia_next = 500_000;
        live_special_spawn_tick(&[Arc::clone(&client)]);
        assert!(
            mobs().lock().unwrap_or_else(|e| e.into_inner()).iter().any(|mob| mob.etype == phantom),
            "doInsomnia=true must spawn phantoms for a 100k-tick insomniac"
        );
    }

    /// Fresh-world regression for `WanderingTraderSpawner`: vanilla's
    /// constructor opens with `tickDelay=1200`, `spawnDelay=24000` and
    /// `spawnChance=25`, so the first tick of a new world cannot spawn a
    /// trader. The trader and its two llamas only land after the full
    /// 24000-tick delay drains with `doMobSpawning` and `doTraderSpawning`
    /// both on; `doMobSpawning=false` blocks the attempt and leaves the
    /// chance where it was.
    #[test]
    fn p2_parity_trader_fresh_state_waits_out_the_spawn_delay() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let Some(trader) = name_to_etype("wandering_trader") else {
            return;
        };
        let Some(llama) = name_to_etype("trader_llama") else {
            return;
        };

        let old_clients = std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner()));
        let old_mobs = std::mem::take(&mut *mobs().lock().unwrap_or_else(|e| e.into_inner()));
        let old_time = WORLD_TIME.swap(1_000, Ordering::Relaxed);
        // A tick that is not a multiple of 20 keeps the warden arm parked for
        // the whole run; patrol and insomnia park on `u64::MAX` next-ticks, so
        // the trader arm owns the only RNG draws.
        let old_tick = TICK.swap(7, Ordering::Relaxed);
        let old_rng = world_rng::saved_state();
        let old_spawn = {
            let mut state = special_spawn_state().lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *state)
        };
        struct Restore(
            Vec<Arc<Client>>,
            Vec<Arc<Mob>>,
            u64,
            u64,
            u64,
            SpecialSpawnState,
        );
        impl std::ops::Drop for Restore {
            fn drop(&mut self) {
                *clients().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.0);
                *mobs().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.1);
                WORLD_TIME.store(self.2, Ordering::Relaxed);
                TICK.store(self.3, Ordering::Relaxed);
                world_rng::restore(self.4);
                *special_spawn_state().lock().unwrap_or_else(|e| e.into_inner()) =
                    std::mem::take(&mut self.5);
            }
        }
        let _restore = Restore(old_clients, old_mobs, old_time, old_tick, old_rng, old_spawn);

        let (client, _receiver) = effect_tests::client_pair();
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (0.5, 70.0, 0.5, 0.0, 0.0, true);
        let client = Arc::new(client);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(Arc::clone(&client));
        let roster = [Arc::clone(&client)];

        let mob_kinds = || {
            let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
            (
                mobs.iter().filter(|mob| mob.etype == trader).count(),
                mobs.iter().filter(|mob| mob.etype == llama).count(),
            )
        };
        let fresh_state = || {
            let mut state = special_spawn_state().lock().unwrap_or_else(|e| e.into_inner());
            *state = SpecialSpawnState {
                patrol_next: u64::MAX,
                insomnia_next: u64::MAX,
                ..SpecialSpawnState::default()
            };
        };

        let _mob_rule = Rules::take(&["doMobSpawning"]);
        let _stored = StoredRules::take(&["doTraderSpawning", "doWardenSpawning"]);
        gamerule_atomic_store("doMobSpawning", 1);
        _stored.set("doTraderSpawning", 1);
        _stored.set("doWardenSpawning", 0);

        // The pinned stream makes the first `next_below(100)` roll a 0, so the
        // first drain attempt lands the trader deterministically.
        world_rng::restore(42);
        fresh_state();
        {
            let state = special_spawn_state().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(state.trader_tick_delay, 1_200, "the 1200-tick slot countdown opens fresh");
            assert_eq!(state.trader_spawn_delay, 24_000, "a fresh world opens with a full spawn delay");
            assert_eq!(state.trader_spawn_chance, 25, "a fresh world opens at the 25 first-attempt chance");
        }

        // Tick 1 of a fresh world: the countdown just starts — no trader.
        live_special_spawn_tick(&roster);
        assert_eq!(mob_kinds(), (0, 0), "tick 1 of a fresh world must not spawn a wandering trader");

        // Well past the first 1200-tick slot the delay is still draining.
        for _ in 0..20_000 {
            live_special_spawn_tick(&roster);
        }
        assert_eq!(mob_kinds(), (0, 0), "the trader waits out the full 24000-tick delay");

        let mut landed = false;
        for _ in 0..30_000 {
            live_special_spawn_tick(&roster);
            let (traders, llamas) = mob_kinds();
            if traders > 0 {
                landed = true;
                assert_eq!(llamas, 2, "a landed trader trails exactly two trader llamas");
                break;
            }
        }
        assert!(landed, "doMobSpawning && doTraderSpawning must spawn the trader after the delay");

        // `doMobSpawning=false` blocks the attempt: a fresh state run through
        // the whole window leaves no trader and never advances the chance.
        gamerule_atomic_store("doMobSpawning", 0);
        mobs().lock().unwrap_or_else(|e| e.into_inner()).clear();
        fresh_state();
        world_rng::restore(42);
        for _ in 0..30_000 {
            live_special_spawn_tick(&roster);
        }
        assert_eq!(mob_kinds(), (0, 0), "doMobSpawning=false must block the wandering trader");
        {
            let state = special_spawn_state().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(state.trader_spawn_chance, 25, "a blocked attempt must not advance the chance");
        }
        gamerule_atomic_store("doMobSpawning", 1);
    }
}

// ----------------------------------------- trading-hall villager AI LOD (PERF-1)
//
// A villager sealed in a trading-hall cell pays a per-tick pathfinding and POI
// rescan bill for movement it can never make. The LOD layer below classifies
// confined, settled villagers into `TradingHallSleep`, which skips that work
// entirely while still running vanilla's exact restock/trade/day bookkeeping —
// and any wake condition (threat, opening wall, damage) restores full AI on
// the very same tick.

/// Stationarity that counts as confinement even without a wall probe: a
/// villager that has not moved for this many ticks is not going anywhere.
#[cfg_attr(not(test), allow(dead_code))]
const VILLAGER_LOD_CONFINED_TICKS: u32 = 40;

/// A hostile inside this radius is a wake-up threat. Vanilla villagers flee
/// zombies/pillagers/vexes/ravagers within a comparable range.
#[cfg_attr(not(test), allow(dead_code))]
const VILLAGER_LOD_THREAT_RADIUS: u16 = 12;

/// The mob types `villager_lod_threat_nearby` counts — the vanilla panic list
/// a trading-hall villager must still react to instantly.
#[cfg_attr(not(test), allow(dead_code))]
fn is_villager_lod_threat(etype_name: &str) -> bool {
    matches!(
        etype_name
            .strip_prefix("minecraft:")
            .unwrap_or(etype_name),
        "zombie" | "pillager" | "vex" | "ravager"
    )
}

/// Is a panic-capable hostile within [`VILLAGER_LOD_THREAT_RADIUS`] of
/// `position`? `hostiles` yields `(position, etype_name)` pairs; the caller
/// gathers them from the mob registry, so the LOD step itself never scans.
#[cfg_attr(not(test), allow(dead_code))]
fn villager_lod_threat_nearby<'a>(
    position: BlockPos,
    hostiles: impl IntoIterator<Item = (BlockPos, &'a str)>,
) -> bool {
    hostiles
        .into_iter()
        .any(|(pos, name)| is_villager_lod_threat(name) && villager_within(position, pos, VILLAGER_LOD_THREAT_RADIUS))
}

/// AI level-of-detail for one villager.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum VillagerAiLodState {
    /// Full per-tick AI: schedule, movement target, pathfinding, POI rescans.
    #[default]
    Active,
    /// Confined trading-hall villager: restock/trade/bookkeeping only — zero
    /// pathfinding and zero POI scans while the state holds.
    TradingHallSleep,
}

/// World facts the LOD classifier needs for this tick, gathered by the caller
/// (the wall probe and the hostile scan are world queries, not villager
/// state). `took_damage` forces an instant wake like vanilla panic does.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default)]
struct VillagerLodSenses {
    /// All four horizontal neighbours of the villager's cell are blocked.
    exits_blocked: bool,
    /// A zombie/pillager/vex/ravager is within `VILLAGER_LOD_THREAT_RADIUS`.
    threat_nearby: bool,
    /// The villager took damage since the previous step.
    took_damage: bool,
}

/// Aggregate accounting the operator surface reads: how many ticks ran full
/// AI, how many slept, how many pathfind calls were skipped, and how many
/// work-station restocks happened (in either state — sleep does not starve
/// the trading hall).
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct VillagerLodStats {
    active_ticks: u64,
    sleeping_ticks: u64,
    pathfinds_skipped: u64,
    restocks_performed: u64,
}

/// Per-villager LOD record: current state, the stationarity counter feeding
/// the 40-tick confinement rule, the last seen position that counter
/// derives from, and the counters.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default)]
struct VillagerAiLod {
    state: VillagerAiLodState,
    confined_ticks: u32,
    last_position: Option<BlockPos>,
    stats: VillagerLodStats,
}

/// Classify this tick: `Active` unless the villager is confined (all four
/// horizontal exits blocked, or stationary for [`VILLAGER_LOD_CONFINED_TICKS`]
/// ticks) with a settled profession and no hostile in range. Threats and
/// damage are instant-wake conditions checked before anything else.
#[cfg_attr(not(test), allow(dead_code))]
fn classify_villager_lod(
    villager: &Villager,
    lod: &VillagerAiLod,
    senses: &VillagerLodSenses,
) -> VillagerAiLodState {
    if senses.took_damage || senses.threat_nearby {
        return VillagerAiLodState::Active;
    }
    let confined =
        senses.exits_blocked || lod.confined_ticks >= VILLAGER_LOD_CONFINED_TICKS;
    if confined && villager.profession != VillagerProfession::None {
        VillagerAiLodState::TradingHallSleep
    } else {
        VillagerAiLodState::Active
    }
}

/// One game tick of villager AI under LOD. Returns the state that ran and the
/// movement target — `None` in `TradingHallSleep`, where the caller must not
/// pathfind at all.
///
/// Wake semantics are instant: damage or a nearby hostile puts the villager on
/// the full [`Villager::tick`] path this same tick. A wall opening while
/// asleep is also a real wake — `exits_blocked` clearing resets the
/// stationarity counter so the villager gets `VILLAGER_LOD_CONFINED_TICKS`
/// ticks of full AI to exploit the opening; if it still cannot leave, it
/// settles back to sleep instead of freezing forever.
#[cfg_attr(not(test), allow(dead_code))]
fn step_villager_with_lod(
    villager: &mut Villager,
    lod: &mut VillagerAiLod,
    pois: &PoiRegistry,
    position: BlockPos,
    world_time: u64,
    senses: VillagerLodSenses,
) -> (VillagerAiLodState, Option<BlockPos>) {
    let moved = lod.last_position.is_some_and(|last| last != position);
    lod.last_position = Some(position);
    if moved {
        lod.confined_ticks = 0;
    } else {
        lod.confined_ticks = lod.confined_ticks.saturating_add(1);
    }
    if lod.state == VillagerAiLodState::TradingHallSleep && !senses.exits_blocked {
        // A surrounding block opened: reset the counter so stationarity has to
        // be re-proven rather than pinning the villager asleep forever.
        lod.confined_ticks = 0;
    }
    lod.state = classify_villager_lod(villager, lod, &senses);
    match lod.state {
        VillagerAiLodState::Active => {
            lod.stats.active_ticks += 1;
            let (_, target) = villager.tick(pois, position, world_time);
            // `last_restock` stamped with this tick's time is the restock
            // itself — a `restocks_today` delta misses the day-rollover case.
            if villager.last_restock == Some(world_time) {
                lod.stats.restocks_performed += 1;
            }
            (lod.state, target)
        }
        VillagerAiLodState::TradingHallSleep => {
            lod.stats.sleeping_ticks += 1;
            lod.stats.pathfinds_skipped += 1;
            if villager.lod_sleep_tick(pois, position, world_time) {
                lod.stats.restocks_performed += 1;
            }
            (lod.state, None)
        }
    }
}

#[cfg(test)]
mod perf1_villager_lod_tests {
    use super::*;

    const PLAYER: VillageUuid = [7; 16];
    const JOB_SITE: BlockPos = (3, 64, 0);
    const CELL: BlockPos = (3, 64, 1);
    const WHEAT: i64 = 854;
    const EMERALD: i64 = 806;

    fn stack(item: i64, count: u8) -> Stack {
        (item, count, Vec::new())
    }

    fn items() -> MerchantItems<'static> {
        fn prototype(_item: i64) -> &'static Comps {
            static NORMAL: OnceLock<Comps> = OnceLock::new();
            NORMAL.get_or_init(|| vec![(1, vec![64])])
        }
        MerchantItems(&prototype)
    }

    fn lod_offer() -> MerchantOffer {
        MerchantOffer {
            cost_a: ItemCost {
                item: WHEAT,
                count: 20,
                components: Vec::new(),
            },
            cost_b: None,
            result: stack(EMERALD, 3),
            uses: 0,
            max_uses: 16,
            xp_reward: 2,
            price_multiplier: 0.05,
            demand: 0,
            special_price_diff: 0,
        }
    }

    /// A librarian already bound to its lectern — the standard trading-hall
    /// resident. `id` distinguishes the hall's claimants.
    fn lod_villager(id: u8) -> (PoiRegistry, Villager, VillagerAiLod) {
        let mut pois = PoiRegistry::default();
        pois.register(JOB_SITE, PoiKind::JobSite(VillagerProfession::Librarian));
        let mut villager = Villager::new([id; 16], VillagerProfession::None);
        assert!(villager.claim_poi(&mut pois, JOB_SITE));
        villager.offers.push(lod_offer());
        (pois, villager, VillagerAiLod::default())
    }

    fn confined() -> VillagerLodSenses {
        VillagerLodSenses {
            exits_blocked: true,
            ..VillagerLodSenses::default()
        }
    }

    #[test]
    fn perf1_confined_trading_hall_villagers_sleep_and_skip_every_pathfind() {
        // 100 sealed trading-hall cells: each villager is walled in next to
        // its job site with a used offer, the canonical lag farm.
        let mut hall: Vec<(PoiRegistry, Villager, VillagerAiLod)> =
            (0..100).map(|i| lod_villager(i as u8)).collect();
        for (_, villager, _) in &mut hall {
            villager.offers[0].uses = 1;
        }

        // Work window: the wake-on-entry step already sleeps (exits blocked).
        for (pois, villager, lod) in &mut hall {
            let (state, target) =
                step_villager_with_lod(villager, lod, pois, CELL, 3_000, confined());
            let _ = target;
            assert_eq!(state, VillagerAiLodState::TradingHallSleep);
            assert_eq!(lod.stats.pathfinds_skipped, 1);
        }
        // First restock fired while asleep: vanilla bookkeeping still runs.
        assert!(hall.iter().all(|(_, v, _)| v.restocks_today == 1));

        // Second restock once the cooldown allows; a third is denied — exactly
        // the vanilla twice-per-day budget.
        for (pois, villager, lod) in &mut hall {
            villager.offers[0].uses = 1;
            step_villager_with_lod(villager, lod, pois, CELL, 5_000, confined());
            assert_eq!(villager.restocks_today, 2);
        }
        for (pois, villager, lod) in &mut hall {
            villager.offers[0].uses = 1;
            step_villager_with_lod(villager, lod, pois, CELL, 7_500, confined());
            assert_eq!(villager.restocks_today, 2, "third restock must not fire");
        }

        let restocks: u64 = hall
            .iter()
            .map(|(_, _, lod)| lod.stats.restocks_performed)
            .sum();
        assert_eq!(restocks, 200, "two restocks each across the hall");

        // 60 more ticks of pure sleep: still not a single pathfind.
        for _ in 0..60 {
            for (pois, villager, lod) in &mut hall {
                let (state, _) = step_villager_with_lod(
                    villager,
                    lod,
                    pois,
                    CELL,
                    7_500,
                    confined(),
                );
                assert_eq!(state, VillagerAiLodState::TradingHallSleep);
            }
        }
        let stats: (u64, u64, u64) = hall.iter().fold((0, 0, 0), |a, (_, _, lod)| {
            (
                a.0 + lod.stats.active_ticks,
                a.1 + lod.stats.sleeping_ticks,
                a.2 + lod.stats.pathfinds_skipped,
            )
        });
        assert_eq!(stats.0, 0, "no active ticks — they were confined all along");
        assert_eq!(stats.1, 6_300, "every tick slept");
        assert_eq!(stats.2, 6_300, "every pathfind skipped");
    }

    #[test]
    fn perf1_sleeping_villager_wakes_instantly_on_threat_and_damage() {
        let (pois, mut villager, mut lod) = lod_villager(9);
        let senses = confined();
        let (state, _) = step_villager_with_lod(&mut villager, &mut lod, &pois, CELL, 3_000, senses);
        assert_eq!(state, VillagerAiLodState::TradingHallSleep);

        // A zombie shuffling up to the cell: wake on that very tick and run
        // the full AI path (which produces a movement target).
        let mut threatened = senses;
        threatened.threat_nearby = true;
        let (state, _) =
            step_villager_with_lod(&mut villager, &mut lod, &pois, CELL, 3_001, threatened);
        assert_eq!(state, VillagerAiLodState::Active);
        assert_eq!(lod.stats.active_ticks, 1);

        // Back to sleep once it passes...
        let (state, _) = step_villager_with_lod(&mut villager, &mut lod, &pois, CELL, 3_002, senses);
        assert_eq!(state, VillagerAiLodState::TradingHallSleep);
        // ... and damage wakes just as fast as a threat does.
        let mut hurt = senses;
        hurt.took_damage = true;
        let (state, _) = step_villager_with_lod(&mut villager, &mut lod, &pois, CELL, 3_003, hurt);
        assert_eq!(state, VillagerAiLodState::Active);

        // The classifier's threat probe itself: the four vanilla panic
        // mobs inside 12 blocks, nothing else, nothing further.
        let at = CELL;
        assert!(villager_lod_threat_nearby(at, [((at.0 + 12, at.1, at.2), "zombie")]));
        assert!(!villager_lod_threat_nearby(at, [((at.0 + 13, at.1, at.2), "zombie")]));
        assert!(villager_lod_threat_nearby(
            at,
            [((at.0 + 8, at.1 + 8, at.2), "minecraft:ravager")]
        ));
        for name in ["pillager", "vex"] {
            assert!(villager_lod_threat_nearby(at, [((at.0, at.1, at.2 + 3), name)]));
        }
        assert!(!villager_lod_threat_nearby(at, [((at.0, at.1, at.2 + 2), "skeleton")]));
        assert!(!villager_lod_threat_nearby(at, [((at.0 + 9, at.1 + 9, at.2), "zombie")]));
    }

    #[test]
    fn perf1_sleeping_villager_wakes_when_a_wall_opens_then_can_resleep() {
        let (pois, mut villager, mut lod) = lod_villager(9);
        let senses = confined();
        let (state, _) = step_villager_with_lod(&mut villager, &mut lod, &pois, CELL, 3_000, senses);
        assert_eq!(state, VillagerAiLodState::TradingHallSleep);
        let skipped_at_sleep = lod.stats.pathfinds_skipped;

        // A piston opens the cell: full AI resumes immediately, and the
        // stationarity counter resets so the villager gets a real chance out.
        let mut open = senses;
        open.exits_blocked = false;
        let (state, target) =
            step_villager_with_lod(&mut villager, &mut lod, &pois, CELL, 3_001, open);
        assert_eq!(state, VillagerAiLodState::Active);
        assert_eq!(lod.confined_ticks, 0);
        assert_eq!(target, villager.job_site, "Work-time AI targets the job site");
        assert_eq!(lod.stats.pathfinds_skipped, skipped_at_sleep);

        // If it still cannot actually leave (the opening led nowhere), 40
        // stationary ticks re-earn the sleep instead of pinning it awake.
        let mut stepped = 0u32;
        loop {
            let (state, _) = step_villager_with_lod(
                &mut villager,
                &mut lod,
                &pois,
                CELL,
                3_001 + u64::from(stepped),
                open,
            );
            stepped += 1;
            if state == VillagerAiLodState::TradingHallSleep {
                break;
            }
            assert!(stepped <= 40, "confinement must re-arrive within 40 ticks");
        }
        assert_eq!(stepped, 40, "wake grace is exactly the confinement window");
    }

    #[test]
    fn perf1_sleeping_villager_keeps_full_trade_interaction() {
        let (pois, mut villager, mut lod) = lod_villager(9);
        let senses = confined();
        let (state, _) = step_villager_with_lod(&mut villager, &mut lod, &pois, CELL, 3_000, senses);
        assert_eq!(state, VillagerAiLodState::TradingHallSleep);

        // A player clicks the villager: the trade session opens, executes and
        // consumes exactly like an awake villager's.
        assert!(villager.start_trading(&pois, PLAYER, None));
        let mut menu = MerchantMenu {
            payments: [stack(WHEAT, 20), stack(0, 0)],
            ..MerchantMenu::default()
        };
        assert_eq!(
            villager
                .trade(PLAYER, &mut menu, &items())
                .map(|trade| trade.result),
            Some(stack(EMERALD, 3))
        );
        villager.stop_trading();
        assert_eq!(villager.offers[0].uses, 1, "trades still wear the offer out");

        // ...which is precisely what lets the next Work tick restock.
        let (state, _) = step_villager_with_lod(&mut villager, &mut lod, &pois, CELL, 5_000, senses);
        assert_eq!(state, VillagerAiLodState::TradingHallSleep);
        assert_eq!(villager.restocks_today, 1);
        assert_eq!(lod.stats.restocks_performed, 1);
    }

    #[test]
    fn perf1_unsettled_profession_and_open_cells_never_sleep() {
        // A confined villager with no profession never sleeps: it has nothing
        // settled to preserve.
        let mut pois = PoiRegistry::default();
        pois.register(JOB_SITE, PoiKind::JobSite(VillagerProfession::Librarian));
        let mut villager = Villager::new([4; 16], VillagerProfession::None);
        let mut lod = VillagerAiLod::default();
        for tick in 0..60 {
            let (state, _) = step_villager_with_lod(
                &mut villager,
                &mut lod,
                &pois,
                CELL,
                3_000 + tick,
                confined(),
            );
            assert_eq!(state, VillagerAiLodState::Active, "unsettled must stay awake");
        }

        // And the "stationary" half of confinement: open exits but 40 ticks
        // without moving also earns the sleep.
        let (pois, mut villager, mut lod) = lod_villager(9);
        let open = VillagerLodSenses::default();
        for tick in 0..40u64 {
            let (state, _) =
                step_villager_with_lod(&mut villager, &mut lod, &pois, CELL, 3_000 + tick, open);
            if tick < 39 {
                assert_eq!(state, VillagerAiLodState::Active);
            }
        }
        assert_eq!(lod.state, VillagerAiLodState::TradingHallSleep);
        assert_eq!(lod.stats.pathfinds_skipped, 1);
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod parity_live_wire_utility_tests {
    use super::*;

    fn push_mob(name: &str, pos: (f64, f64, f64, f32, f32), hp: f32) -> Arc<Mob> {
        let spec = entity_spec_by_name(name).unwrap_or_else(|| panic!("{name} spec"));
        let mob = new_live_mob(spec.id, DimId::Overworld, pos, hp, None);
        mobs().lock().unwrap_or_else(|e| e.into_inner()).push(mob.clone());
        mob
    }

    fn utility_state(eid: i32) -> Option<NativeUtilityState> {
        live_native_utility_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&eid)
            .cloned()
    }

    #[test]
    fn parity_live_wire_equine_parrot() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();

        // Horse: `live_utility_mob_tick` — the utility driver `villager_raid_tick`
        // runs every server tick — materializes the EquineState model.
        let horse = push_mob("horse", (32_000.5, 81.0, 32_000.5, 0.0, 0.0), 30.0);
        set_block_dim(
            DimId::Overworld,
            32_000,
            80,
            32_000,
            reg.state_of("minecraft:stone", None),
        );
        live_utility_mob_tick(1);
        {
            let mut states = live_native_utility_states()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(NativeUtilityState::Equine(equine)) = states.get_mut(&horse.eid) else {
                panic!("equine state must materialize through live_utility_mob_tick");
            };
            equine.tamed = true;
            equine.jump_strength = 0.8;
        }
        // Mount a rider and hold the steering jump bit: the charge builds,
        // and its release applies the vanilla jump-strength-scaled velocity —
        // `equine_jump_velocity` executed by the live tick.
        {
            let mut live = mounts().lock().unwrap_or_else(|e| e.into_inner());
            live.registry.register_vehicle(
                horse.eid,
                VehicleKind::Horse,
                [32_000.5, 81.0, 32_000.5],
            );
            live.registry
                .mount(horse.eid, 9_777_001)
                .expect("rider mounts the equine vehicle");
            live.steering.insert(
                horse.eid,
                PlayerInputPacket {
                    sideways: 0.0,
                    forward: 0.0,
                    jump: true,
                    unmount: false,
                },
            );
        }
        live_utility_mob_tick(2);
        assert_eq!(
            equine_jump_charges()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&horse.eid)
                .copied(),
            Some(5),
            "holding the jump bit charges the equine jump",
        );
        {
            let mut live = mounts().lock().unwrap_or_else(|e| e.into_inner());
            live.steering.insert(
                horse.eid,
                PlayerInputPacket {
                    sideways: 0.0,
                    forward: 0.0,
                    jump: false,
                    unmount: false,
                },
            );
        }
        live_utility_mob_tick(3);
        assert!(
            horse.vel.lock().unwrap_or_else(|e| e.into_inner())[1] > 0.0,
            "releasing the charged jump applies equine_jump_velocity upward",
        );
        assert_eq!(
            equine_jump_charges()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&horse.eid)
                .copied(),
            None,
        );

        // Parrot: the same live tick materializes the ParrotState model
        // (seed taming/cookie/shoulder perch all key off this record).
        let parrot = push_mob("parrot", (32_004.5, 81.0, 32_000.5, 0.0, 0.0), 6.0);
        live_utility_mob_tick(4);
        assert!(
            matches!(utility_state(parrot.eid), Some(NativeUtilityState::Parrot(_))),
            "parrot state must materialize through live_utility_mob_tick",
        );
    }

    #[test]
    fn parity_live_wire_golem_allay_axolotl_camel() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();

        // Axolotl: hydration and the play-dead reflex run inside the live
        // utility tick against the mob's real hp cell.
        let axolotl = push_mob("axolotl", (32_020.5, 81.0, 32_000.5, 0.0, 0.0), 14.0);
        set_block_dim(
            DimId::Overworld,
            32_020,
            81,
            32_000,
            reg.state_of("minecraft:water", None),
        );
        live_utility_mob_tick(11);
        assert!(
            matches!(utility_state(axolotl.eid), Some(NativeUtilityState::Axolotl(_))),
            "axolotl state must materialize through live_utility_mob_tick",
        );
        *axolotl.hp.lock().unwrap_or_else(|e| e.into_inner()) = 5.0;
        live_utility_mob_tick(12);
        match utility_state(axolotl.eid) {
            Some(NativeUtilityState::Axolotl(state)) => assert!(
                state.playing_dead_ticks > 0,
                "dropping below half health while hydrated plays dead",
            ),
            _ => panic!("axolotl state vanished"),
        }
        live_utility_mob_tick(25);
        assert_eq!(
            *axolotl.hp.lock().unwrap_or_else(|e| e.into_inner()),
            6.0,
            "play-dead regeneration heals through the live tick",
        );

        // Allay: a shard-carrying allay within 10 blocks of a mid-song
        // jukebox duplicates through `live_allay_jukebox_step` in the tick.
        let allay = push_mob("allay", (32_030.5, 81.0, 32_000.5, 0.0, 0.0), 20.0);
        let shard = name_to_item("minecraft:amethyst_shard").expect("amethyst shard id");
        live_native_utility_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                allay.eid,
                NativeUtilityState::Allay(AllayState {
                    held: Some((shard, 1, Vec::new())),
                    ..Default::default()
                }),
            );
        register_world_block_entity(
            DimId::Overworld,
            (32_031, 81, 32_000),
            VanillaBlockEntity::Jukebox(JukeboxBlockEntity {
                record_item: Some("minecraft:music_disc_cat".into()),
                ..Default::default()
            }),
        );
        let mobs_before = mobs().lock().unwrap_or_else(|e| e.into_inner()).len();
        live_utility_mob_tick(10);
        assert!(
            mobs().lock().unwrap_or_else(|e| e.into_inner()).len() > mobs_before,
            "a dancing shard-carrier allay duplicates through the live tick",
        );

        // Iron golem: iron-ingot repair runs the live item-interaction entry.
        let golem = push_mob("iron_golem", (32_040.5, 81.0, 32_000.5, 0.0, 0.0), 50.0);
        assert!(live_iron_golem_repair(golem.eid, true));
        assert_eq!(*golem.hp.lock().unwrap_or_else(|e| e.into_inner()), 75.0);

        // Camel: the rider's steering jump bit fires the dash on its cooldown
        // through the same tick branch that reads PlayerInputPacket.
        let camel = push_mob("camel", (32_050.5, 81.0, 32_000.5, 0.0, 0.0), 32.0);
        {
            let mut live = mounts().lock().unwrap_or_else(|e| e.into_inner());
            live.registry.register_vehicle(
                camel.eid,
                VehicleKind::Camel,
                [32_050.5, 81.0, 32_000.5],
            );
            live.registry
                .mount(camel.eid, 9_777_002)
                .expect("rider mounts the camel");
            live.steering.insert(
                camel.eid,
                PlayerInputPacket {
                    sideways: 0.0,
                    forward: 0.0,
                    jump: true,
                    unmount: false,
                },
            );
        }
        let mut dashed = false;
        for tick in 100u64..2_600 {
            live_utility_mob_tick(tick);
            if *camel.vel.lock().unwrap_or_else(|e| e.into_inner()) != [0.0; 3] {
                dashed = true;
                break;
            }
        }
        assert!(dashed, "steering jump must fire live_camel_dash while unsat");
    }
}

#[cfg(test)]
mod villager_raid_interact_dispatch_tests {
    use super::*;
    use crate::effect_tests::client_pair;

    fn push_test_mob(name: &str, dim: DimId, hp: f32) -> Arc<Mob> {
        let spec = entity_spec_by_name(name).unwrap_or_else(|| panic!("{name} spec"));
        let mob = new_live_mob(spec.id, dim, (0.5, 70.0, 0.5, 0.0, 0.0), hp, None);
        mobs().lock().unwrap_or_else(|e| e.into_inner()).push(mob.clone());
        mob
    }

    fn push_test_client(eid: i32, uuid: VillageUuid) -> Arc<Client> {
        let (mut client, _receiver) = client_pair();
        client.eid = eid;
        client.uuid = uuid;
        let client = Arc::new(client);
        crate::clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(client.clone());
        client
    }

    #[test]
    fn interact_routes_iron_ingot_to_golem_repair() {
        let _guard = crate::world_guard();
        if crate::test_support::isolated_world_test() {
            return;
        }
        let golem = push_test_mob("iron_golem", DimId::Overworld, 50.0);
        assert_eq!(
            villager_raid_interact_mob(DimId::Overworld, golem.eid, "iron_golem", "iron_ingot", 7),
            Some(MobInteractOutcome::IronGolemRepaired { hp_after: 75.0 })
        );
        // Repair clamps at the 100 HP cap and refuses a full-health golem.
        *golem.hp.lock().unwrap_or_else(|e| e.into_inner()) = 90.0;
        assert_eq!(
            villager_raid_interact_mob(
                DimId::Overworld,
                golem.eid,
                "minecraft:iron_golem",
                "minecraft:iron_ingot",
                7,
            ),
            Some(MobInteractOutcome::IronGolemRepaired { hp_after: 100.0 }),
            "namespaced spellings reach the same route"
        );
        assert_eq!(
            villager_raid_interact_mob(DimId::Overworld, golem.eid, "iron_golem", "iron_ingot", 7),
            None,
            "a full-health golem declines the ingot"
        );
        // Wrong item, wrong kind, wrong dimension and an unknown eid all miss.
        assert_eq!(
            villager_raid_interact_mob(DimId::Overworld, golem.eid, "iron_golem", "stick", 7),
            None
        );
        assert_eq!(
            villager_raid_interact_mob(DimId::Overworld, golem.eid, "snow_golem", "iron_ingot", 7),
            None
        );
        assert_eq!(
            villager_raid_interact_mob(DimId::Nether, golem.eid, "iron_golem", "iron_ingot", 7),
            None,
            "the interaction stays in the mob's own dimension"
        );
        assert_eq!(
            villager_raid_interact_mob(DimId::Overworld, 9_999_999, "iron_golem", "iron_ingot", 7),
            None
        );
    }

    #[test]
    fn interact_routes_shears_to_snow_golem_pumpkin() {
        let _guard = crate::world_guard();
        if crate::test_support::isolated_world_test() {
            return;
        }
        let golem = push_test_mob("snow_golem", DimId::Overworld, 4.0);
        let drops_before = drops().lock().unwrap_or_else(|e| e.into_inner()).len();
        assert_eq!(
            villager_raid_interact_mob(DimId::Overworld, golem.eid, "snow_golem", "shears", 7),
            Some(MobInteractOutcome::SnowGolemSheared)
        );
        {
            let drops = drops().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(drops.len(), drops_before + 1, "shearing drops the pumpkin");
            let pumpkin = name_to_item("carved_pumpkin").expect("carved_pumpkin id");
            assert_eq!(drops.last().map(|drop| drop.item), Some(pumpkin));
        }
        // One pumpkin per golem: a second shear does nothing.
        assert_eq!(
            villager_raid_interact_mob(DimId::Overworld, golem.eid, "snow_golem", "shears", 7),
            None
        );
        // Non-shear items do not strip the pumpkin.
        let fresh = push_test_mob("snow_golem", DimId::Nether, 4.0);
        assert_eq!(
            villager_raid_interact_mob(DimId::Nether, fresh.eid, "snow_golem", "iron_ingot", 7),
            None
        );
        assert_eq!(
            villager_raid_interact_mob(DimId::Nether, fresh.eid, "snow_golem", "shears", 7),
            Some(MobInteractOutcome::SnowGolemSheared),
            "the route works outside the Overworld"
        );
    }

    #[test]
    fn interact_routes_allay_give_and_empty_hand_take() {
        let _guard = crate::world_guard();
        if crate::test_support::isolated_world_test() {
            return;
        }
        let allay = push_test_mob("allay", DimId::End, 20.0);
        let owner: VillageUuid = [9; 16];
        let _client = push_test_client(55_001, owner);
        let diamond = name_to_item("diamond").expect("diamond id");
        assert_eq!(
            villager_raid_interact_mob(DimId::End, allay.eid, "allay", "diamond", 55_001),
            Some(MobInteractOutcome::AllayItemGiven)
        );
        // The sample is held: a second give is refused, matching the
        // full-hands branch of Allay.mobInteract.
        assert_eq!(
            villager_raid_interact_mob(DimId::End, allay.eid, "allay", "emerald", 55_001),
            None
        );
        // The empty hand takes the sample back.
        assert_eq!(
            villager_raid_interact_mob(DimId::End, allay.eid, "allay", "air", 55_001),
            Some(MobInteractOutcome::AllayItemTaken((diamond, 1, Vec::new())))
        );
        // Nothing held: an empty hand now returns no outcome either.
        assert_eq!(
            villager_raid_interact_mob(DimId::End, allay.eid, "allay", "", 55_001),
            None
        );
        // The owner is resolved through the live client list: an eid with no
        // client cannot feed or retrieve.
        assert_eq!(
            villager_raid_interact_mob(DimId::End, allay.eid, "allay", "diamond", 9_999_999),
            None
        );
        assert_eq!(
            villager_raid_interact_mob(DimId::End, allay.eid, "allay", "diamond", 55_001),
            Some(MobInteractOutcome::AllayItemGiven),
            "give/take works in the End, not just the Overworld"
        );
        // An unregistered item name never reaches the allay.
        live_native_utility_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&allay.eid);
        assert_eq!(
            villager_raid_interact_mob(DimId::End, allay.eid, "allay", "not_a_real_item", 55_001),
            None
        );
    }
}
