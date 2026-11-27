// See the pinned 1.21.1 server's PotentialCalculator, NaturalSpawner.createState,
// getRoughBiome and SpawnState.canSpawn/afterSpawn (tools/bytecode_probe.sh).

struct CheckedSpawnCost<'a> {
    entity_type: &'a str,
    pos: NaturalSpawnPos,
    charge: f64,
}

/// The energy budget for one dimension's spawn pass. The supplied biome lookup
/// uses the chunk's noise biome at the block position's quart coordinates.
pub(crate) struct SpawnCostState<'a, F> {
    potential: PotentialCalculator,
    rough_biome_at: F,
    last_checked: Option<CheckedSpawnCost<'a>>,
}

impl<'a, F, B> SpawnCostState<'a, F>
where
    F: FnMut(NaturalSpawnPos, (i32, i32)) -> Option<B>,
    B: AsRef<str>,
{
    /// Feed census-eligible mobs in order, using namespaced entity IDs and block positions.
    /// The biome lookup returns `None` for unavailable chunks.
    pub(crate) fn from_existing(
        mobs: impl IntoIterator<Item = (&'a str, NaturalSpawnPos)>,
        rough_biome_at: F,
    ) -> Self {
        let mut state = Self {
            potential: PotentialCalculator::default(),
            rough_biome_at,
            last_checked: None,
        };
        for (entity_type, pos) in mobs {
            if let Some(cost) = state.cost_at(entity_type, pos, (pos.x >> 4, pos.z >> 4)) {
                state.potential.add_charge(pos, cost.charge);
            }
        }
        state
    }

    pub(crate) fn can_spawn(
        &mut self,
        entity_type: &'a str,
        pos: NaturalSpawnPos,
        chunk: (i32, i32),
    ) -> bool {
        let cost = self.cost_at(entity_type, pos, chunk);
        // Every check replaces the cached charge, including when the biome has no cost.
        self.last_checked = Some(CheckedSpawnCost {
            entity_type,
            pos,
            charge: cost.map_or(0.0, |cost| cost.charge),
        });
        cost.is_none_or(|cost| self.potential.can_spawn(pos, cost))
    }

    pub(crate) fn after_spawn(
        &mut self,
        entity_type: &str,
        pos: NaturalSpawnPos,
        chunk: (i32, i32),
    ) {
        let charge = match &self.last_checked {
            Some(last) if last.pos == pos && last.entity_type == entity_type => last.charge,
            _ => self
                .cost_at(entity_type, pos, chunk)
                .map_or(0.0, |cost| cost.charge),
        };
        self.potential.add_charge(pos, charge);
    }

    fn cost_at(
        &mut self,
        entity_type: &str,
        pos: NaturalSpawnPos,
        chunk: (i32, i32),
    ) -> Option<MobSpawnCost> {
        let biome = (self.rough_biome_at)(pos, chunk)?;
        feature::spawn_cost_for(biome.as_ref(), entity_type)
    }
}

#[cfg(test)]
mod spawn_costs_tests {
    use super::*;
    use std::cell::Cell;

    const SOUL_SAND_VALLEY: &str = "minecraft:soul_sand_valley";
    #[cfg(megu_vanilla_data)]
    const WARPED_FOREST: &str = "minecraft:warped_forest";
    const ENDERMAN: &str = "minecraft:enderman";
    const SKELETON: &str = "minecraft:skeleton";

    fn pos(x: i32, y: i32, z: i32) -> NaturalSpawnPos {
        NaturalSpawnPos { x, y, z }
    }

    #[test]
    fn potential_sums_inverse_distances_and_accepts_exact_budget() {
        let mut potential = PotentialCalculator::default();
        potential.add_charge(pos(0, 0, 0), 4.0);
        potential.add_charge(pos(0, 3, 4), 6.0);
        let candidate = pos(0, 0, 4);
        assert_eq!(potential.get_potential_energy_change(candidate, 2.0), 6.0);
        let mut cost = MobSpawnCost {
            energy_budget: 6.0,
            charge: 2.0,
        };
        assert!(potential.can_spawn(candidate, cost));
        cost.energy_budget = 5.999;
        assert!(!potential.can_spawn(candidate, cost));
    }

    #[test]
    fn colocated_charge_has_infinite_potential() {
        let site = pos(-1, 64, 2);
        let mut potential = PotentialCalculator::default();
        potential.add_charge(site, 0.0);
        assert_eq!(potential.get_potential_energy_change(site, 1.0), 0.0);
        potential.add_charge(site, -1.0);
        assert_eq!(
            potential.get_potential_energy_change(site, 1.0),
            f64::INFINITY
        );
        assert_eq!(potential.get_potential_energy_change(site, 0.0), 0.0);
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn existing_mobs_seed_only_costs_in_their_own_biome() {
        let uncharged = pos(0, 64, 0);
        let charged = pos(-16, 64, 0);
        let unloaded = pos(16, 64, 0);
        let mut state = SpawnCostState::from_existing(
            [
                (SKELETON, uncharged),
                (SKELETON, charged),
                (ENDERMAN, unloaded),
            ],
            |_, chunk| match chunk {
                (0, 0) => Some(WARPED_FOREST),
                (-1, 0) => Some(SOUL_SAND_VALLEY),
                _ => None,
            },
        );
        assert!(state.can_spawn(ENDERMAN, uncharged, (0, 0)));
        assert!(!state.can_spawn(SKELETON, charged, (-1, 0)));
        assert!(state.can_spawn(ENDERMAN, unloaded, (0, 0)));
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn every_check_refreshes_the_charge_including_when_there_is_no_cost() {
        let site = pos(0, 64, 0);
        let mut state = SpawnCostState::from_existing([], |_, chunk| match chunk {
            (0, 0) => Some(WARPED_FOREST),
            (1, 0) => Some(SOUL_SAND_VALLEY),
            _ => Some("minecraft:plains"),
        });
        assert!(state.can_spawn(ENDERMAN, site, (0, 0)));
        assert!(state.can_spawn(ENDERMAN, site, (2, 0)));
        state.after_spawn(ENDERMAN, site, (0, 0));
        assert!(state.can_spawn(ENDERMAN, site, (0, 0)));
        state.after_spawn(ENDERMAN, site, (1, 0));
        // 1.0 / 7 exceeds the warped forest budget. A fresh lookup would add 0.7.
        assert!(!state.can_spawn(ENDERMAN, pos(7, 64, 0), (0, 0)));
    }

    #[test]
    fn after_spawn_reuses_only_the_last_checked_type_and_position() {
        let lookups = Cell::new(0);
        let site = pos(0, 64, 0);
        let other = pos(8, 64, 0);
        let mut state = SpawnCostState::from_existing([], |_, _| {
            lookups.set(lookups.get() + 1);
            Some(SOUL_SAND_VALLEY)
        });
        let chunk = (0, 0);
        assert!(state.can_spawn(ENDERMAN, site, chunk));
        state.after_spawn(SKELETON, site, chunk);
        state.after_spawn(ENDERMAN, other, chunk);
        assert_eq!(lookups.get(), 3);
        state.after_spawn(ENDERMAN, site, chunk);
        assert_eq!(lookups.get(), 3);
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn after_spawn_accumulates_charges_across_entity_types() {
        let mut state = SpawnCostState::from_existing([], |_, _| Some(SOUL_SAND_VALLEY));
        let chunk = (0, 0);
        let first = pos(0, 64, 0);
        let second = pos(8, 64, 0);
        let between = pos(4, 64, 0);
        assert!(state.can_spawn(ENDERMAN, first, chunk));
        state.after_spawn(ENDERMAN, first, chunk);
        assert!(state.can_spawn(SKELETON, between, chunk));
        assert!(state.can_spawn(SKELETON, second, chunk));
        state.after_spawn(SKELETON, second, chunk);
        assert!(!state.can_spawn(SKELETON, between, chunk));
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn pinned_biomes_only_charge_for_soul_sand_valley_and_warped_forest() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/datapacks/vanilla/data/minecraft/worldgen/biome"
        );
        let mut nonempty = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if !path.extension().is_some_and(|ext| ext == "json") {
                continue;
            }
            let json = jparse(&std::fs::read(&path).unwrap()).unwrap();
            let J::Obj(costs) = json.get("spawn_costs").unwrap() else {
                panic!("expected a spawn_costs object in {}", path.display());
            };
            if !costs.is_empty() {
                let biome = path.file_stem().unwrap().to_str().unwrap();
                nonempty.push((format!("minecraft:{biome}"), costs.len()));
            }
        }
        nonempty.sort_unstable();
        assert_eq!(
            nonempty,
            [
                (SOUL_SAND_VALLEY.to_owned(), 4),
                (WARPED_FOREST.to_owned(), 1)
            ]
        );

        let valley_cost = MobSpawnCost {
            energy_budget: 0.15,
            charge: 0.7,
        };
        for entity in [SKELETON, "minecraft:ghast", ENDERMAN, "minecraft:strider"] {
            assert_eq!(
                feature::spawn_cost_for(SOUL_SAND_VALLEY, entity),
                Some(valley_cost)
            );
        }
        assert_eq!(
            feature::spawn_cost_for(WARPED_FOREST, ENDERMAN),
            Some(MobSpawnCost {
                energy_budget: 0.12,
                charge: 1.0
            }),
        );
        assert_eq!(feature::spawn_cost_for(WARPED_FOREST, SKELETON), None);
    }
}
