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
        base.wrapping_add(demand_diff.max(0))
            .wrapping_add(self.special_price_diff)
            .clamp(
                1,
                i32::from(items.max_stack_size(self.cost_a.item, &self.cost_a.components)),
            )
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
            Self::MinorPositive => (1, 25, 1, 5),
            Self::MajorPositive => (5, 20, 0, 20),
            Self::Trading => (1, 25, 2, 20),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
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
                    && (self.profession == VillagerProfession::None
                        || self.profession == profession) =>
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

    #[cfg_attr(not(test), allow(dead_code))]
    fn tick(
        &mut self,
        pois: &PoiRegistry,
        position: BlockPos,
        world_time: u64,
    ) -> (VillagerActivity, Option<BlockPos>) {
        self.refresh_pois(pois);
        let day = world_time / VILLAGER_DAY_TICKS;
        if self.day.is_none_or(|previous| day > previous) {
            if let Some(previous) = self.day {
                self.gossip.decay(day - previous);
            }
            self.day = Some(day);
            self.restocks_today = 0;
        }

        let activity = VillagerActivity::at(world_time);
        let target = match activity {
            VillagerActivity::Idle => None,
            VillagerActivity::Meet => self.meeting_point,
            VillagerActivity::Rest => self.home,
            VillagerActivity::Work => {
                // The workstation occupies its block. Work from an adjacent block.
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
                }
                self.job_site
            }
        };
        self.sleeping = activity == VillagerActivity::Rest && target == Some(position);
        (activity, target)
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
        Ok(TradeExecutionOutcome {
            result,
            consumed_a,
            consumed_b,
            villager_xp: self.villager_xp,
            level_before,
            level_after: self.level(),
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

#[cfg_attr(not(test), allow(dead_code))]
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
    fn adjusted_cost_is_clamped_by_the_cost_items_effective_stack_limit() {
        let mut offer = offer();
        offer.cost_a.item = ENDER_PEARL;
        offer.cost_a.count = 5;
        offer.price_multiplier = 0.5;
        offer.demand = 9;
        assert_eq!(offer.cost_a_count(&items()), 16);
        offer.cost_a.components = vec![(1, vec![7])];
        assert_eq!(offer.cost_a_count(&items()), 7);
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
}
