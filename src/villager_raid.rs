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
#[derive(Clone, Copy, Debug, PartialEq)]
struct MerchantOffer {
    base_cost_a_count: i32,
    cost_b_count: i32,
    result_id: u32,
    result_count: i32,
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
    fn cost_a(&self) -> i32 {
        let demand_product = self.base_cost_a_count.wrapping_mul(self.demand);
        let scaled = demand_product as f32 * self.price_multiplier;
        let truncated = scaled as i32;
        // Mth.floor casts to a Java int before its corrective decrement.
        let demand_diff = if scaled < truncated as f32 {
            truncated.wrapping_sub(1)
        } else {
            truncated
        };
        self.base_cost_a_count
            .wrapping_add(demand_diff.max(0))
            .wrapping_add(self.special_price_diff)
            .clamp(1, 64)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn for_customer(mut self, reputation: i32, hero_level: Option<u8>) -> Self {
        self.special_price_diff =
            ((reputation as f32 * self.price_multiplier).floor() as i32).saturating_neg();
        if let Some(level) = hero_level.filter(|&level| (1..=5).contains(&level)) {
            let reduction = 0.3 + 0.0625 * f64::from(level - 1);
            let discount = (f64::from(self.base_cost_a_count) * reduction).floor() as i32;
            self.special_price_diff = self.special_price_diff.saturating_sub(discount.max(1));
        }
        self
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
    fn offer_for_customer(
        &mut self,
        pois: &PoiRegistry,
        index: usize,
        player: VillageUuid,
        hero_level: Option<u8>,
    ) -> Option<MerchantOffer> {
        self.refresh_pois(pois);
        if matches!(
            self.profession,
            VillagerProfession::None | VillagerProfession::Nitwit
        ) {
            return None;
        }
        Some(
            self.offers
                .get(index)?
                .for_customer(self.gossip.reputation(player), hero_level),
        )
    }

    // The caller supplies counts from stacks whose item identities it has validated.
    #[cfg_attr(not(test), allow(dead_code))]
    fn trade(
        &mut self,
        pois: &PoiRegistry,
        index: usize,
        player: VillageUuid,
        hero_level: Option<u8>,
        cost_a: &mut i32,
        cost_b: &mut i32,
    ) -> Option<(u32, i32)> {
        let offer = self.offer_for_customer(pois, index, player, hero_level)?;
        let required_a = offer.cost_a();
        if offer.uses >= offer.max_uses
            || offer.result_count <= 0
            || offer.cost_b_count < 0
            || *cost_a < required_a
            || *cost_b < offer.cost_b_count
        {
            return None;
        }
        *cost_a -= required_a;
        *cost_b -= offer.cost_b_count;
        self.offers[index].uses += 1;
        self.villager_xp = self.villager_xp.saturating_add(offer.xp_reward.max(0));
        self.gossip.add(player, GossipType::Trading, 2);
        Some((offer.result_id, offer.result_count))
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
const RAID_CELEBRATION_TICKS: u16 = 600;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RaidOmen {
    bad_omen_level: u8,
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

        if living_villagers == 0
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
        let wave = RaidWave {
            number: self.waves_spawned,
            counts: RAID_BASE_WAVES[row],
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

    fn offer() -> MerchantOffer {
        MerchantOffer {
            base_cost_a_count: 20,
            cost_b_count: 2,
            result_id: 42,
            result_count: 3,
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
        assert_eq!(
            villager.trade(&pois, 0, PLAYER, None, &mut 20, &mut 2),
            None
        );
        assert_eq!(villager.profession, VillagerProfession::None);
        assert!(villager.offers.is_empty());
        assert!(villager.claim_poi(&mut pois, JOB_SITE));
        villager.offers.push(offer());
        assert_eq!(
            villager.trade(&pois, 0, PLAYER, None, &mut 20, &mut 2),
            Some((42, 3))
        );

        pois.register(JOB_SITE, PoiKind::JobSite(VillagerProfession::Farmer));
        villager.tick(&pois, AT_WORK, 4_000);
        assert_eq!(villager.job_site, None);
        assert_eq!(villager.profession, VillagerProfession::Librarian);
        assert_eq!(villager.offers[0].uses, 1);
        assert!(!villager.claim_poi(&mut pois, JOB_SITE));
        assert!(villager
            .offer_for_customer(&pois, 0, PLAYER, None)
            .is_some());
        pois.register(JOB_SITE, PoiKind::JobSite(VillagerProfession::Librarian));
        assert!(villager.claim_poi(&mut pois, JOB_SITE));
        assert_eq!(villager.offers[0].uses, 1);
    }

    #[test]
    fn successful_trades_consume_both_costs_and_unlock_levels_at_the_xp_boundaries() {
        let (pois, mut villager) = village();
        villager.offers.push(offer());
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
            let (mut cost_a, mut cost_b) = (21, 1);
            assert_eq!(
                villager.trade(&pois, 0, PLAYER, None, &mut cost_a, &mut cost_b),
                None
            );
            assert_eq!((cost_a, cost_b), (21, 1));
            cost_b = 3;
            assert_eq!(
                villager.trade(&pois, 0, PLAYER, None, &mut cost_a, &mut cost_b),
                Some((42, 3))
            );
            assert_eq!((cost_a, cost_b), (1, 1));
            assert_eq!(villager.villager_xp, xp);
            assert_eq!(villager.level(), level);
        }
        assert_eq!(villager.gossip.reputation(PLAYER), 10);
        assert_eq!(
            villager.trade(&pois, 0, PLAYER, None, &mut 64, &mut 2),
            None
        );
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
        assert_eq!(villager.offers[0].cost_a(), 27);
        assert_eq!(villager.offers[1].cost_a(), 20);
        assert_eq!(villager.restocks_today, 1);
        villager.tick(&pois, AT_WORK, 4_000);
        assert_eq!(villager.offers[0].demand, 7);
        assert_eq!(villager.restocks_today, 1);
    }

    #[test]
    fn persisted_large_demand_uses_wrapping_java_int_product() {
        let offer = MerchantOffer {
            base_cost_a_count: 4,
            demand: 1 << 30,
            price_multiplier: 0.05,
            special_price_diff: 0,
            ..offer()
        };
        assert_eq!(offer.cost_a(), 4);
        assert_eq!(
            MerchantOffer {
                demand: -(1 << 28),
                price_multiplier: 4.0,
                ..offer
            }
            .cost_a(),
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
            villager
                .offer_for_customer(&pois, 0, player, hero)
                .unwrap()
                .cost_a()
        };
        assert_eq!(price(&mut villager, PLAYER, None), 14);
        assert_eq!(price(&mut villager, OTHER_PLAYER, None), 20);
        for (level, expected) in [(1, 8), (2, 7), (3, 6), (4, 5), (5, 3)] {
            assert_eq!(price(&mut villager, PLAYER, Some(level)), expected);
        }
        assert_eq!(villager.offers[0].special_price_diff, 0);
        villager.offers[0].base_cost_a_count = 2;
        assert_eq!(price(&mut villager, OTHER_PLAYER, Some(1)), 1);
        assert_eq!(price(&mut villager, PLAYER, Some(5)), 1);
        villager.offers[0].base_cost_a_count = 64;
        villager.offers[0].demand = 99;
        villager
            .gossip
            .add(OTHER_PLAYER, GossipType::MajorNegative, 100);
        assert_eq!(price(&mut villager, OTHER_PLAYER, Some(5)), 64);
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
        let mut omen = Some(RaidOmen { bad_omen_level: 2 });
        for (spot, kind) in [
            (west, PoiKind::Bed),
            (east, PoiKind::Bell),
            (far, PoiKind::JobSite(VillagerProfession::Mason)),
        ] {
            pois.register(spot, kind);
        }
        assert!(Raid::try_start(&pois, pos, PLAYER, &mut omen, RaidDifficulty::Hard).is_none());
        assert_eq!(omen, Some(RaidOmen { bad_omen_level: 2 }));
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
}
