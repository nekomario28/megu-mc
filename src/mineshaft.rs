//! Procedural structure admission and decoration. Layout, placement primitives,
//! biome lookup and block writes keep their existing owners.
#[cfg(test)]
#[path = "mineshaft_tests.rs"]
mod tests;
use crate::generation_context::FC;
use crate::generation_math::{fastutil_mix64, Lcg, Xor};
use crate::json::{jparse, J};
use crate::mineshaft_layout::{self, Piece, Variant};
use crate::structure_placement::{FrequencyReduction, RandomSpread, Spread};
use crate::worldgen::Gen;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

#[derive(Clone, Debug)]
struct Entry {
    id: String,
    variant: Variant,
    weight: i32,
    biomes: HashSet<String>,
    step: usize,
    slot: usize,
}
struct Config {
    spread: RandomSpread,
    salt: i32,
    frequency: f32,
    reducer: FrequencyReduction,
    entries: Vec<Entry>,
    blocking: HashSet<String>,
}
pub(crate) struct Start {
    entry: usize,
    pub(crate) pieces: Vec<Piece>,
}
fn field<'a>(j: &'a J, name: &str) -> &'a J {
    j.get(name)
        .unwrap_or_else(|| panic!("mineshafts: missing {name}"))
}
fn ns(s: &str) -> String {
    if s.contains(':') {
        s.into()
    } else {
        format!("minecraft:{s}")
    }
}
fn resource(kind: &str, id: &str) -> J {
    let b = Gen::dp_resource(kind, id, "json")
        .unwrap_or_else(|| panic!("mineshafts: missing {kind}/{id}"));
    jparse(&b).unwrap_or_else(|| panic!("mineshafts: invalid {kind}/{id}"))
}
fn biomes(value: &J, visiting: &mut HashSet<String>) -> HashSet<String> {
    if let Some(values) = value.as_arr() {
        return values.iter().flat_map(|v| biomes(v, visiting)).collect();
    }
    let s = value.str();
    if let Some(tag) = s.strip_prefix('#') {
        let tag = ns(tag);
        assert!(
            visiting.insert(tag.clone()),
            "mineshafts: cyclic biome tag {tag}"
        );
        let j = resource("tags/worldgen/biome", &tag);
        let result = biomes(field(&j, "values"), visiting);
        visiting.remove(&tag);
        result
    } else {
        assert!(
            !s.is_empty(),
            "mineshafts: unsupported biome holder {value:?}"
        );
        HashSet::from([ns(s)])
    }
}
fn step(s: &str) -> usize {
    [
        "raw_generation",
        "lakes",
        "local_modifications",
        "underground_structures",
        "surface_structures",
        "strongholds",
        "underground_ores",
        "underground_decoration",
        "fluid_springs",
        "vegetal_decoration",
        "top_layer_modification",
    ]
    .iter()
    .position(|v| *v == s)
    .unwrap_or_else(|| panic!("mineshafts: unknown step {s}"))
}
fn config() -> Option<&'static Config> {
    static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let bytes = Gen::dp_resource("worldgen/structure_set", "minecraft:mineshafts", "json")?;
            let j = jparse(&bytes).expect("mineshafts: invalid structure set");
            let p = field(&j, "placement");
            assert_eq!(
                field(p, "type").str(),
                "minecraft:random_spread",
                "mineshafts: unsupported placement"
            );
            assert!(
                p.get("exclusion_zone").is_none(),
                "mineshafts: exclusion zone not implemented"
            );
            let salt = field(p, "salt").num() as i32;
            let spread = match p.get("spread_type").map(J::str).unwrap_or("linear") {
                "linear" => Spread::Linear,
                "triangular" => Spread::Triangular,
                other => panic!("mineshafts: unsupported spread {other}"),
            };
            let reducer = match p
                .get("frequency_reduction_method")
                .map(J::str)
                .unwrap_or("default")
            {
                "default" => FrequencyReduction::Default,
                "legacy_type_1" => FrequencyReduction::LegacyType1,
                "legacy_type_2" => FrequencyReduction::LegacyType2,
                "legacy_type_3" => FrequencyReduction::LegacyType3,
                other => panic!("mineshafts: unknown frequency reducer {other}"),
            };
            // RegistryDataLoader enumerates resource locations before registry step
            // grouping; absent starts never renumber the slots within a step.
            let mut structures: Vec<_> = Gen::dp_files("structure").into_iter().collect();
            structures.sort_by(|a, b| {
                let key = |s: &str| {
                    let (n, p) = s.split_once(':').unwrap();
                    (p.to_owned(), n.to_owned())
                };
                key(&a.0).cmp(&key(&b.0))
            });
            let mut counts = [0; 11];
            let mut slots = HashMap::new();
            for (id, bytes) in structures {
                let s =
                    jparse(&bytes).unwrap_or_else(|| panic!("mineshafts: invalid structure {id}"));
                let st = step(field(&s, "step").str());
                slots.insert(id, (st, counts[st]));
                counts[st] += 1;
            }
            let entries: Vec<_> = field(&j, "structures")
                .as_arr()
                .expect("mineshafts: entries")
                .iter()
                .map(|v| {
                    let id = ns(field(v, "structure").str());
                    let s = resource("worldgen/structure", &id);
                    assert_eq!(
                        field(&s, "type").str(),
                        "minecraft:mineshaft",
                        "mineshafts: unsupported entry {id}"
                    );
                    let variant = match field(&s, "mineshaft_type").str() {
                        "normal" => Variant::Normal,
                        "mesa" => Variant::Mesa,
                        other => panic!("mineshafts: unsupported variant {other}"),
                    };
                    let weight = field(v, "weight").num() as i32;
                    assert!(weight > 0, "mineshafts: nonpositive weight");
                    let (step, slot) = *slots.get(&id).expect("mineshafts: missing registry slot");
                    Entry {
                        id,
                        variant,
                        weight,
                        step,
                        slot,
                        biomes: biomes(field(&s, "biomes"), &mut HashSet::new()),
                    }
                })
                .collect();
            let frequency = p.get("frequency").map(J::num).unwrap_or(1.) as f32;
            assert!(
                (0.0..=1.0).contains(&frequency),
                "mineshafts: invalid frequency"
            );
            assert!(!entries.is_empty(), "mineshafts: empty entries");
            Some(Config {
                spread: RandomSpread::new(
                    field(p, "spacing").num() as i32,
                    field(p, "separation").num() as i32,
                    salt,
                    spread,
                )
                .expect("mineshafts: spread bounds"),
                salt,
                frequency,
                reducer,
                entries,
                blocking: biomes(
                    &J::Str("#minecraft:mineshaft_blocking".into()),
                    &mut HashSet::new(),
                ),
            })
        })
        .as_ref()
}

/// An attempt owns its fresh context RNG. The independently seeded selector is
/// advanced only by weighted selection, never by failed layout/biome attempts.
pub(crate) fn admit(g: &mut Gen, cx: i32, cz: i32) -> Option<Start> {
    admit_with_biome(g, cx, cz, |g, (x, y, z)| g.biome_name_at(x, y, z))
}

fn admit_with_biome(
    g: &mut Gen,
    cx: i32,
    cz: i32,
    mut noise_biome: impl FnMut(&mut Gen, (i32, i32, i32)) -> Option<String>,
) -> Option<Start> {
    let c = config()?;
    if c.spread.candidate(g.wseed(), cx, cz) != (cx, cz)
        || !c.reducer.allows(g.wseed(), c.salt, cx, cz, c.frequency)
    {
        return None;
    }
    let mut choices: Vec<_> = (0..c.entries.len()).collect();
    let mut random = Lcg::for_large_feature(g.wseed(), cx, cz);
    while !choices.is_empty() {
        let weight = choices.iter().map(|&i| c.entries[i].weight).sum();
        let selected = if choices.len() == 1 && c.entries.len() == 1 {
            0
        } else {
            let mut draw = random.next_bound(weight);
            choices
                .iter()
                .position(|&i| {
                    draw -= c.entries[i].weight;
                    draw < 0
                })
                .unwrap()
        };
        let entry = choices[selected];
        let e = &c.entries[entry];
        let layout = match e.variant {
            Variant::Normal => mineshaft_layout::normal_layout(g.wseed(), cx, cz, g.sea, g.min_y),
            Variant::Mesa => {
                mineshaft_layout::mesa_layout(g.wseed(), cx, cz, g.sea, g.min_y, |x, z| {
                    g.top_ws(x, z)
                })
            }
        };
        let point = (
            cx.wrapping_shl(4).wrapping_add(8),
            50 + layout.vertical_shift,
            cz.wrapping_shl(4).wrapping_add(8),
        );
        if noise_biome(g, point).is_some_and(|b| e.biomes.contains(&b)) {
            return Some(Start {
                entry,
                pieces: layout.pieces,
            });
        }
        choices.remove(selected);
    }
    None
}

/// Native LongOpenHashSet encounter order, including rehash and the zero slot.
/// Reference insertion is X-major/Z-minor across the native +/-8 chunk window.
fn reference_order(keys: impl IntoIterator<Item = u64>) -> Vec<u64> {
    fn insert(table: &mut [u64], key: u64) -> bool {
        let mut i = fastutil_mix64(key) as usize & (table.len() - 1);
        while table[i] != 0 {
            if table[i] == key {
                return false;
            }
            i = (i + 1) & (table.len() - 1);
        }
        table[i] = key;
        true
    }
    let mut table = vec![0; 32];
    let mut zero = false;
    let mut count = 0;
    for key in keys {
        let added = if key == 0 {
            let old = zero;
            zero = true;
            !old
        } else {
            insert(&mut table, key)
        };
        if !added {
            continue;
        }
        count += 1;
        if count > table.len() * 3 / 4 {
            let mut next = vec![0; table.len() * 2];
            for &k in table.iter().rev() {
                if k != 0 {
                    insert(&mut next, k);
                }
            }
            table = next;
        }
    }
    let mut out = Vec::new();
    if zero {
        out.push(0);
    }
    out.extend(table.into_iter().rev().filter(|&k| k != 0));
    out
}
/// ChunkAccess has a distinct LongSet for each structure, not one set whose
/// capacity is shared by NORMAL and Mesa references.
fn reference_order_by_type(keys: impl IntoIterator<Item = (usize, u64)>) -> Vec<(usize, u64)> {
    let mut groups = std::collections::BTreeMap::<usize, Vec<u64>>::new();
    for (entry, key) in keys {
        groups.entry(entry).or_default().push(key);
    }
    groups
        .into_iter()
        .flat_map(|(entry, keys)| {
            reference_order(keys)
                .into_iter()
                .map(move |key| (entry, key))
        })
        .collect()
}
fn packed(x: i32, z: i32) -> u64 {
    u64::from(x as u32) | (u64::from(z as u32) << 32)
}

fn decoration_entries(entries: &[Entry], step: usize) -> Vec<(usize, &Entry)> {
    let mut result: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.step == step)
        .collect();
    result.sort_by_key(|(_, e)| e.slot);
    result
}

pub(crate) struct References {
    starts: Vec<(u64, Start)>,
    #[cfg(test)]
    rng_after: HashMap<(usize, usize), (u64, u64)>,
}
impl References {
    pub(crate) fn step_count(&self) -> usize {
        config().map_or(0, |c| {
            self.starts
                .iter()
                .map(|(_, s)| c.entries[s.entry].step + 1)
                .max()
                .unwrap_or(0)
        })
    }

    pub(crate) fn collect(g: &mut Gen, cx: i32, cz: i32) -> Self {
        if g.preset != crate::worldgen::NoisePreset::Overworld || config().is_none() {
            return Self {
                starts: Vec::new(),
                #[cfg(test)]
                rng_after: HashMap::new(),
            };
        }
        let mut found = HashMap::new();
        let mut keys = Vec::new();
        for x in cx - 8..=cx + 8 {
            for z in cz - 8..=cz + 8 {
                let Some(start) = admit(g, x, z) else {
                    continue;
                };
                let mut b = [i32::MAX; 6];
                b[3..].fill(i32::MIN);
                for p in &start.pieces {
                    for a in 0..3 {
                        b[a] = b[a].min(p.bounds[a]);
                        b[a + 3] = b[a + 3].max(p.bounds[a + 3]);
                    }
                }
                if b[0] <= (cx << 4) + 15
                    && b[3] >= cx << 4
                    && b[2] <= (cz << 4) + 15
                    && b[5] >= cz << 4
                {
                    let key = packed(x, z);
                    keys.push(key);
                    found.insert(key, start);
                }
            }
        }
        Self {
            starts: reference_order_by_type(keys.into_iter().map(|k| (found[&k].entry, k)))
                .into_iter()
                .map(|(_, k)| (k, found.remove(&k).unwrap()))
                .collect(),
            #[cfg(test)]
            rng_after: HashMap::new(),
        }
    }
    pub(crate) fn place_step(
        &mut self,
        ctx: &mut FC,
        step: usize,
        population_seed: u64,
        cx: i32,
        cz: i32,
    ) {
        let Some(c) = config() else {
            return;
        };
        for (entry, e) in decoration_entries(&c.entries, step) {
            let seed = population_seed
                .wrapping_add(e.slot as u64)
                .wrapping_add(10_000u64.wrapping_mul(step as u64));
            let mut random = Xor::seed_worldgen(seed);
            crate::generation_math::XTRACE_FEAT.store(
                ((e.slot as i64) << 32) | step as i64,
                std::sync::atomic::Ordering::Relaxed,
            );
            for (key, start) in self.starts.iter_mut().filter(|(_, s)| s.entry == entry) {
                let sx = *key as i32;
                let sz = (*key >> 32) as i32;
                if crate::trace_env::env_flag!("MINESHAFT_TRACE") {
                    eprintln!(
                        "MSTART {cx} {cz} {} {step} {} {sx} {sz} {}",
                        e.id,
                        e.slot,
                        start.pieces.len()
                    );
                }
                crate::mineshaft_piece::place(
                    ctx,
                    &mut random,
                    cx,
                    cz,
                    sx,
                    sz,
                    e.variant,
                    &mut start.pieces,
                    &c.blocking,
                );
            }
            #[cfg(test)]
            self.rng_after.insert((step, e.slot), random.state128());
        }
    }
}
