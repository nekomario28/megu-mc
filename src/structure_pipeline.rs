// structure_pipeline.rs — the unified 1.21.1 StructureSet / StructureStart pipeline.
//
// Included from `feature.rs`, which is the only layer that may see both
// `structure_template` and `mineshaft` (`tools/layers_check.sh`). `worldgen` (terrain)
// cannot import this module, so it reaches the beardifier input through the
// `BeardResolver` function pointer registered at the bottom of this file.
//
// Vanilla bytecode that fixes the order and the RNG (pinned jar):
//   `ChunkGenerator.createStructures` + its set lambda
//       - an existing valid start for any member of the set short-circuits the set
//       - `StructurePlacement.isStructureChunk` = placement chunk AND
//         `applyAdditionalChunkRestrictions` (frequency reducer) AND
//         `applyInteractionsWithOtherStructures` (exclusion zone), in that order
//       - one entry: generate directly (no selection draw)
//       - several entries: `WorldgenRandom(LegacyRandomSource(0))` +
//         `setLargeFeatureSeed(seed, x, z)`, then `nextInt(totalWeight)` selection,
//         retried with the weight removed until a start or no candidates remain
//   `StructureManager.createReferences` / `ChunkGenerator.createReferences`
//       - +/-8 chunk window, `start.getBoundingBox().intersects(chunk)` filter,
//         insertion order per structure, then the fastutil LongOpenHashSet order
//   `ChunkGenerator.applyBiomeDecoration`
//       - per `GenerationStep.Decoration`, for every structure of that step in
//         registry order: `setFeatureSeed(pop, index, step)` then
//         `startsForStructure` in reference order, `StructureStart.placeInChunk`
//   `Beardifier.forStructuresInChunk`
//       - only structures whose `terrain_adaptation != none`, pieces
//         `isCloseToChunk(chunk, 12)`, `Rigid(box, adjustment, groundLevelDelta)`
//         for RIGID-projection pool pieces and every non-pool piece; junctions
//         within the block window of the chunk
use crate::mineshaft;
use crate::structure_template::{self, BB, Piece, Templates};
use crate::worldgen::{self, BeardAdjustment, BeardChunkData, BeardRigid, Gen};

/// `GenerationStep.Decoration` names in ordinal order.
pub(crate) const STRUCTURE_STEPS: [&str; 11] = [
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
];

fn ns(s: &str) -> String {
    if s.contains(':') {
        s.into()
    } else {
        format!("minecraft:{s}")
    }
}

/// A structure id sorted the way `RegistryDataLoader` enumerates resource
/// locations (path first, then namespace) — the order that fixes each
/// structure's in-step index, i.e. its decoration RNG seed.
fn registry_sort_key(id: &str) -> (String, String) {
    let (namespace, path) = id.split_once(':').unwrap_or(("minecraft", id));
    (path.to_owned(), namespace.to_owned())
}

fn resource(kind: &str, id: &str) -> Option<crate::json::J> {
    let bytes = Gen::dp_resource(kind, id, "json")?;
    crate::json::jparse(&bytes)
}

/// `#minecraft:is_overworld` style biome holder → the explicit set.
fn biome_set(value: &crate::json::J, visiting: &mut HashSet<String>) -> HashSet<String> {
    if let Some(values) = value.as_arr() {
        return values.iter().flat_map(|v| biome_set(v, visiting)).collect();
    }
    let s = value.str();
    if let Some(tag) = s.strip_prefix('#') {
        let tag = ns(tag);
        if !visiting.insert(tag.clone()) {
            return HashSet::new();
        }
        let result = resource("tags/worldgen/biome", &tag)
            .map(|j| {
                j.get("values")
                    .map(|v| biome_set(v, visiting))
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        visiting.remove(&tag);
        result
    } else if s.is_empty() {
        HashSet::new()
    } else {
        HashSet::from([ns(s)])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kick {
    /// `minecraft:mineshaft` / `minecraft:mineshaft_mesa`: the existing, verified
    /// procedural-piece owner in `mineshaft.rs`.
    Mineshaft,
    /// `structure_template::find_generation_point` + `place_structure`.
    Template,
    /// Piece builders exist, but the `Structure.findGenerationPoint` adapter that
    /// turns them into a start is not wired here yet.
    SinglePiece,
    /// `jigsaw_structures` currently only exposes a scene-driven entry point.
    Jigsaw,
    /// Wave 3A (`end_city`, `mansion`, `fortress`, `monument`, `stronghold`).
    Wave3a,
}

struct StructureDef {
    kick: Kick,
    template: Option<structure_template::StructureKind>,
    step: usize,
    slot: usize,
    adjustment: BeardAdjustment,
    biomes: HashSet<String>,
}

enum PlacementDef {
    Spread {
        spacing: i32,
        separation: i32,
        salt: i32,
        triangular: bool,
        frequency: f32,
        reducer: u8,
        /// `ExclusionZone`: (other set id, chunk count).
        exclusion: Option<(String, i32)>,
    },
    /// `ConcentricRingsStructurePlacement`: strongholds (Wave 3A).
    Rings,
}

struct SetDef {
    placement: PlacementDef,
    entries: Vec<(String, i32)>,
}

struct Defs {
    sets: Vec<SetDef>,
    by_id: HashMap<String, usize>,
    structures: HashMap<String, StructureDef>,
    /// Structures of each step, in registry (slot) order.
    by_step: Vec<Vec<String>>,
    /// Sets with at least one structure whose `terrain_adaptation` is not `none`.
    /// Only these can contribute a beardifier box, so a chunk whose neighbourhood
    /// admits none of them needs no beardifier at all.
    adapting_sets: Vec<usize>,
}

fn step_index(name: &str) -> Option<usize> {
    STRUCTURE_STEPS.iter().position(|v| *v == name)
}

fn defs() -> Option<&'static Defs> {
    static DEFS: std::sync::OnceLock<Option<Defs>> = std::sync::OnceLock::new();
    DEFS.get_or_init(load_defs).as_ref()
}

fn load_defs() -> Option<Defs> {
    let loaded = load_defs_inner();
    if loaded.is_none() {
        // A structure pipeline with no definitions admits nothing, which looks exactly
        // like "no structures in this seed". Say so.
        eprintln!("SPIPE structure definitions did not load");
    }
    loaded
}

fn load_defs_inner() -> Option<Defs> {
    // Structures first: the slot of a structure inside its step is its index in the
    // registry enumeration, which is what `setFeatureSeed(pop, index, step)` uses.
    let mut structure_files: Vec<(String, Vec<u8>)> = Gen::dp_files("structure").into_iter().collect();
    structure_files.sort_by(|a, b| registry_sort_key(&a.0).cmp(&registry_sort_key(&b.0)));
    let mut counts = [0usize; STRUCTURE_STEPS.len()];
    let mut structures = HashMap::new();
    let mut by_step: Vec<Vec<String>> = vec![Vec::new(); STRUCTURE_STEPS.len()];
    for (id, bytes) in &structure_files {
        let j = crate::json::jparse(bytes)?;
        let step = step_index(
            j.get("step")
                .map(|v| v.str())
                .or_else(|| {
                    // 1.21.1 writes `step` on every structure; a missing one is a parse
                    // failure rather than a default step.
                    None
                })
                .as_deref()?,
        )?;
        let slot = counts[step];
        counts[step] += 1;
        let ty = j.get("type").map(|v| v.str().to_owned()).unwrap_or_default();
        let adjustment = j
            .get("terrain_adaptation")
            .and_then(|v| BeardAdjustment::parse(v.str()))
            .unwrap_or(BeardAdjustment::None);
        let biomes = j
            .get("biomes")
            .map(|v| biome_set(v, &mut HashSet::new()))
            .unwrap_or_default();
        let (kick, template) = match ty.as_str() {
            "minecraft:mineshaft" => (Kick::Mineshaft, None),
            "minecraft:igloo"
            | "minecraft:nether_fossil"
            | "minecraft:shipwreck"
            | "minecraft:ocean_ruin"
            | "minecraft:ruined_portal" => (
                Kick::Template,
                Some(structure_template::parse_structure(&j).ok()?),
            ),
            "minecraft:buried_treasure"
            | "minecraft:swamp_hut"
            | "minecraft:desert_pyramid"
            | "minecraft:jungle_temple" => (Kick::SinglePiece, None),
            "minecraft:jigsaw" => (Kick::Jigsaw, None),
            "minecraft:end_city"
            | "minecraft:woodland_mansion"
            | "minecraft:fortress"
            | "minecraft:ocean_monument"
            | "minecraft:stronghold" => (Kick::Wave3a, None),
            other => {
                eprintln!("SPIPE unknown structure type {other} for {id}");
                continue;
            }
        };
        by_step[step].push(id.clone());
        structures.insert(
            id.clone(),
            StructureDef {
                kick,
                template,
                step,
                slot,
                adjustment,
                biomes,
            },
        );
    }
    // Structure sets, in the same registry enumeration order.
    let mut set_files: Vec<(String, Vec<u8>)> = Gen::dp_files("structure_set").into_iter().collect();
    set_files.sort_by(|a, b| registry_sort_key(&a.0).cmp(&registry_sort_key(&b.0)));
    let mut sets = Vec::new();
    let mut by_id = HashMap::new();
    for (id, bytes) in &set_files {
        let j = crate::json::jparse(bytes)?;
        let p = j.get("placement")?;
        let placement_type = p.get("type").map(|v| v.str().to_owned()).unwrap_or_default();
        let placement = match placement_type.as_str() {
            "minecraft:random_spread" => {
                let number = |k: &str, d: f64| p.get(k).map(|v| v.num()).unwrap_or(d);
                let reducer_name = p
                    .get("frequency_reduction_method")
                    .map(|v| v.str().to_owned())
                    .unwrap_or_else(|| "default".into());
                let reducer = match reducer_name.as_str() {
                    "default" => 0,
                    "legacy_type_1" => 1,
                    "legacy_type_2" => 2,
                    "legacy_type_3" => 3,
                    other => {
                        eprintln!("SPIPE unknown frequency reducer {other} in {id}");
                        return None;
                    }
                };
                // The zone names another set by id; the index is resolved in a second
                // pass, because the sets are enumerated in registry order and the
                // referenced set may not have been parsed yet.
                let exclusion = p.get("exclusion_zone").map(|z| {
                    (
                        ns(z.get("other_set").map(|v| v.str()).unwrap_or_default()),
                        z.get("chunk_count").map(|v| v.num()).unwrap_or(0.0) as i32,
                    )
                });
                PlacementDef::Spread {
                    spacing: number("spacing", 0.0) as i32,
                    separation: number("separation", 0.0) as i32,
                    salt: number("salt", 0.0) as i32,
                    triangular: p.get("spread_type").map(|v| v.str()) == Some("triangular"),
                    frequency: number("frequency", 1.0) as f32,
                    reducer,
                    exclusion,
                }
            }
            "minecraft:concentric_rings" => PlacementDef::Rings,
            other => {
                eprintln!("SPIPE unknown placement {other} in {id}");
                return None;
            }
        };
        let entries: Vec<(String, i32)> = j
            .get("structures")?
            .as_arr()?
            .iter()
            .filter_map(|e| {
                let structure = ns(e.get("structure")?.str());
                let weight = e.get("weight").map(|v| v.num()).unwrap_or(1.0) as i32;
                Some((structure, weight))
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        by_id.insert(id.clone(), sets.len());
        sets.push(SetDef { placement, entries });
    }
    if crate::trace_env::env_flag!("SPIPE_TRACE") {
        eprintln!(
            "SPIPE defs: {} sets, {} structures, by_step {:?}",
            sets.len(),
            structures.len(),
            by_step.iter().map(|v| v.len()).collect::<Vec<_>>()
        );
    }
    let adapting_sets: Vec<usize> = sets
        .iter()
        .enumerate()
        .filter(|(_, set)| {
            set.entries.iter().any(|(id, _)| {
                structures
                    .get(id)
                    .is_some_and(|def| def.adjustment != BeardAdjustment::None)
            })
        })
        .map(|(i, _)| i)
        .collect();
    Some(Defs {
        sets,
        by_id,
        structures,
        by_step,
        adapting_sets,
    })
}

// ---------------------------------------------------------------- admission

/// `RandomSpreadStructurePlacement.isPlacementChunk`: is this chunk the set's own
/// candidate chunk? No frequency, no exclusion.
fn candidate_admits(d: &Defs, set: usize, seed: u64, x: i32, z: i32) -> bool {
    let PlacementDef::Spread {
        spacing,
        separation,
        salt,
        triangular,
        ..
    } = &d.sets[set].placement
    else {
        // `ConcentricRingsStructurePlacement` places from `generateRingPositions`, which
        // needs the biome relocation search. Wave 3A.
        return false;
    };
    let Some(spread) = mineshaft::spread_placement(*spacing, *separation, *salt, *triangular) else {
        return false;
    };
    spread.candidate(seed, x, z) == (x, z)
}

/// `isPlacementChunk` + `applyAdditionalChunkRestrictions`: the candidate test and the
/// frequency reducer, before any exclusion zone.
fn placement_admits(d: &Defs, set: usize, seed: u64, x: i32, z: i32) -> bool {
    let PlacementDef::Spread {
        salt,
        frequency,
        reducer,
        ..
    } = &d.sets[set].placement
    else {
        return false;
    };
    candidate_admits(d, set, seed, x, z)
        && mineshaft::frequency_allows(*reducer, seed, *salt, x, z, *frequency)
}

/// `StructurePlacement.isStructureChunk` for one set, including the exclusion zone.
fn set_admits(d: &Defs, set: usize, seed: u64, x: i32, z: i32, depth: u32) -> bool {
    if depth > 2 {
        // The exclusion graph is shallow in 1.21.1 (one set points at another); a
        // longer chain would be a data bug, and refusing is the fail-closed read.
        return false;
    }
    if !placement_admits(d, set, seed, x, z) {
        return false;
    }
    let PlacementDef::Spread { exclusion, .. } = &d.sets[set].placement else {
        return false;
    };
    match exclusion {
        None => true,
        Some((other, count)) => {
            let count = *count;
            // The zone names a set id; its index is resolved here because the sets are
            // enumerated in registry order and the referenced set may sort later.
            let Some(&other) = d.by_id.get(other) else {
                return false;
            };
            !mineshaft::exclusion_forbids(count, x, z, |ox, oz| {
                set_admits(d, other, seed, ox, oz, depth + 1)
            })
        }
    }
}

// ---------------------------------------------------------------- starts

pub(crate) enum StartPieces {
    Mineshaft(Box<mineshaft::Start>),
    Template { pieces: Vec<Piece> },
}

pub(crate) struct Start {
    pub(crate) structure: String,
    /// The chunk the start belongs to (`ChunkPos` of `StructureStart`).
    pub(crate) key: (i32, i32),
    pub(crate) bbox: BB,
    /// `Structure.GenerationStub.position()`: the piece placement anchor.
    pub(crate) stub: (i32, i32, i32),
    pub(crate) pieces: StartPieces,
}

impl Start {
    fn intersects_chunk(&self, cx: i32, cz: i32) -> bool {
        let (x0, z0) = (cx << 4, cz << 4);
        bb_intersects(&self.bbox, &[x0, i32::MIN / 2, z0, x0 + 15, i32::MAX / 2, z0 + 15])
    }

}

fn bb_intersects(a: &BB, b: &BB) -> bool {
    a[0] <= b[3] && a[3] >= b[0] && a[1] <= b[4] && a[4] >= b[1] && a[2] <= b[5] && a[5] >= b[2]
}

/// `StructurePiece.isCloseToChunk`: the box reaches within `distance` of the chunk.
fn bb_close_to_chunk(bb: &BB, cx: i32, cz: i32, distance: i32) -> bool {
    let (x0, z0) = (cx << 4, cz << 4);
    bb_intersects(
        bb,
        &[
            x0 - distance,
            i32::MIN / 2,
            z0 - distance,
            x0 + 15 + distance,
            i32::MAX / 2,
            z0 + 15 + distance,
        ],
    )
}

fn bb_encapsulate(a: &mut BB, b: &BB) {
    for i in 0..3 {
        a[i] = a[i].min(b[i]);
        a[i + 3] = a[i + 3].max(b[i + 3]);
    }
}

thread_local! {
    /// `StructureTemplateManager.getOrCreate`: the native manager caches templates
    /// per world; this is the same cache, kept off the generator (whose layer may
    /// not name `Templates`).
    static TEMPLATES: std::cell::RefCell<Option<Templates>> = const { std::cell::RefCell::new(None) };
}

fn with_templates<R>(f: impl FnOnce(&mut Templates) -> R) -> R {
    TEMPLATES.with(|slot| {
        let mut held = slot.borrow_mut();
        let templates = held.get_or_insert_with(|| {
            Templates::new(|id| Gen::dp_resource("structure", id, "nbt"))
        });
        f(templates)
    })
}

/// Generate the structure this set entry names, or `None` when it refuses.
fn try_generate(
    g: &mut Gen,
    id: &str,
    def: &StructureDef,
    seed: u64,
    cx: i32,
    cz: i32,
) -> Option<Start> {
    match def.kick {
        Kick::Mineshaft => {
            let start = mineshaft::admit(g, cx, cz)?;
            let bbox = mineshaft::start_bbox(&start);
            let (x0, z0) = (cx << 4, cz << 4);
            Some(Start {
                structure: mineshaft::start_id(&start).to_owned(),
                key: (cx, cz),
                bbox,
                stub: (x0 + 8, bbox[1], z0 + 8),
                pieces: StartPieces::Mineshaft(Box::new(start)),
            })
        }
        Kick::Template => {
            let kind = def.template.as_ref()?;
            let reg = crate::block_registry::breg()?;
            let mut fc = FC::new(g);
            if let Some(biome) = fc.biome(cx * 16 + 8, 64, cz * 16 + 8) {
                if !def.biomes.is_empty() && !def.biomes.contains(&biome) {
                    return None;
                }
            }
            let cold_biome = fc.biome_leaf(cx * 16 + 8, 64, cz * 16 + 8)
                .is_some_and(|leaf| fc.cold_enough_to_snow(leaf, cx * 16 + 8, 64, cz * 16 + 8));
            let mut gen = crate::generation_math::Lcg::for_large_feature(seed, cx, cz);
            let found = with_templates(|templates| {
                let mut find = structure_template::FindCtx {
                    cx,
                    cz,
                    cold_biome,
                    rng: &mut gen,
                };
                structure_template::find_generation_point(&mut fc, templates, kind, &mut find, reg)
                    .ok()
                    .flatten()
            });
            let Some((stub, build)) = found else {
                return None;
            };
            let pieces = with_templates(|templates| build(&mut fc, templates, &mut gen, reg).ok())?;
            if pieces.is_empty() {
                return None;
            }
            let mut bbox = [i32::MAX; 6];
            bbox[3..].fill(i32::MIN);
            for piece in &pieces {
                bb_encapsulate(&mut bbox, &piece.bb);
            }
            Some(Start {
                structure: id.to_owned(),
                key: (cx, cz),
                bbox,
                stub,
                pieces: StartPieces::Template { pieces },
            })
        }
        Kick::SinglePiece | Kick::Jigsaw | Kick::Wave3a => None,
    }
}

/// The starts a set places in one chunk, in vanilla's admission order.
///
/// `createStructures` walks each set once: an existing valid start for a member
/// short-circuits the set (the port recomputes starts per chunk, so nothing is
/// stored yet), the placement has to admit the chunk, and a multi-entry set picks
/// by weight with the set's own seeded random, dropping a refused entry and
/// re-drawing.
fn set_starts(d: &Defs, g: &mut Gen, set: &SetDef, cx: i32, cz: i32) -> Vec<Start> {
    // The mineshaft set owns its weighted selection (and its variant-specific biome
    // check) inside `mineshaft::admit`; re-drawing here would consume the selector
    // twice and admit a different entry.
    let all_mineshaft = set.entries.iter().all(|(id, _)| {
        d.structures
            .get(id)
            .is_some_and(|def| def.kick == Kick::Mineshaft)
    });
    if all_mineshaft {
        let id = &set.entries[0].0;
        return match d.structures.get(id) {
            Some(def) => try_generate(g, id, def, g.wseed(), cx, cz).into_iter().collect(),
            None => Vec::new(),
        };
    }
    if set.entries.len() == 1 {
        let id = &set.entries[0].0;
        let Some(def) = d.structures.get(id) else {
            return Vec::new();
        };
        return try_generate(g, id, def, g.wseed(), cx, cz).into_iter().collect();
    }
    // Multi-entry set: weighted selection over the candidates that are loaded.
    let mut candidates: Vec<(&str, i32)> = set
        .entries
        .iter()
        .filter(|(id, _)| d.structures.contains_key(id))
        .map(|(id, w)| (id.as_str(), *w))
        .collect();
    let mut total: i32 = candidates.iter().map(|(_, w)| *w).sum();
    let mut random = crate::generation_math::Lcg::for_large_feature(g.wseed(), cx, cz);
    while !candidates.is_empty() && total > 0 {
        let mut draw = random.next_bound(total);
        let mut index = 0usize;
        for (i, (_, weight)) in candidates.iter().enumerate() {
            draw -= *weight;
            index = i;
            if draw < 0 {
                break;
            }
        }
        let (id, weight) = candidates[index];
        if let Some(def) = d.structures.get(id) {
            if let Some(start) = try_generate(g, id, def, g.wseed(), cx, cz) {
                return vec![start];
            }
        }
        candidates.remove(index);
        total -= weight;
    }
    Vec::new()
}

/// Starts of every set that admit this chunk.
fn chunk_starts(g: &mut Gen, cx: i32, cz: i32) -> Vec<Start> {
    let Some(d) = defs() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, set) in d.sets.iter().enumerate() {
        if set.entries.len() == 1 && matches!(d.structures.get(&set.entries[0].0), None) {
            continue;
        }
        // The mineshaft set owns its own admission (`mineshaft::admit`), which is
        // byte-for-byte the same placement/frequency/weighted/biome chain.
        let mineshaft_set = set.entries.iter().all(|(id, _)| {
            d.structures
                .get(id)
                .is_some_and(|def| def.kick == Kick::Mineshaft)
        });
        if !mineshaft_set && !set_admits(d, i, g.wseed(), cx, cz, 0) {
            continue;
        }
        if mineshaft_set {
            out.extend(set_starts(d, g, set, cx, cz));
            continue;
        }
        out.extend(set_starts(d, g, set, cx, cz));
    }
    out
}

thread_local! {
    /// Starts per (seed, chunk), shared by the beardifier resolver and the
    /// decoration pass so the admission scan runs once. Pure in its key.
    static STARTS: std::cell::RefCell<StartsCache> =
        std::cell::RefCell::new(StartsCache { seed: 0, map: std::collections::HashMap::new(), ord: Vec::new() });
}

struct StartsCache {
    seed: u64,
    map: HashMap<(i32, i32), std::sync::Arc<Vec<Start>>>,
    ord: Vec<(i32, i32)>,
}

fn starts_cached(g: &mut Gen, cx: i32, cz: i32) -> std::sync::Arc<Vec<Start>> {
    STARTS.with(|slot| {
        let mut cache = slot.borrow_mut();
        let seed = g.wseed();
        if cache.seed != seed {
            cache.seed = seed;
            cache.map.clear();
            cache.ord.clear();
        }
        if let Some(v) = cache.map.get(&(cx, cz)) {
            return v.clone();
        }
        let starts = std::sync::Arc::new(chunk_starts(g, cx, cz));
        cache.ord.push((cx, cz));
        if cache.ord.len() > 1024 {
            let old = cache.ord.remove(0);
            cache.map.remove(&old);
        }
        cache.map.insert((cx, cz), starts.clone());
        starts
    })
}

/// Drop the cached starts. The cache keys on the level seed, so a new world already
/// starts cold; this exists for a caller that reuses a seed with different structure data.
#[allow(dead_code)]
pub(crate) fn reset_cache() {
    STARTS.with(|slot| {
        let mut cache = slot.borrow_mut();
        cache.seed = 0;
        cache.map.clear();
        cache.ord.clear();
    });
}

// ---------------------------------------------------------------- references

/// `ChunkGenerator.createReferences` + `StructureManager.startsForStructure`.
///
/// The reference set of a structure is the starts whose bounding box intersects
/// this chunk, among the 17x17 chunk window vanilla scans, kept in the insertion
/// order the native `LongOpenHashSet` iterates.
pub(crate) struct References {
    /// (step, structure id, starts in reference order)
    items: Vec<(usize, String, Vec<(u64, Start)>)>,
    step_count: usize,
    #[cfg(test)]
    rng_after: HashMap<(usize, usize), (u64, u64)>,
}

impl References {
    pub(crate) fn collect(g: &mut Gen, cx: i32, cz: i32) -> Self {
        let Some(d) = defs() else {
            return Self {
                items: Vec::new(),
                step_count: 0,
                #[cfg(test)]
                rng_after: HashMap::new(),
            };
        };
        // key -> starts, per structure id, in the encounter order of the window.
        let mut found: HashMap<String, (usize, Vec<u64>, HashMap<u64, Start>)> = HashMap::new();
        // Only chunks with a candidate of some set can hold a start. The candidate test is
        // a superset of admission (it skips the frequency reducer), so nothing is lost, and
        // the expensive per-chunk generation only runs where a set actually points.
        let seed = g.wseed();
        let mut interesting: Vec<(i32, i32)> = Vec::new();
        for x in cx - 8..=cx + 8 {
            for z in cz - 8..=cz + 8 {
                if d.sets
                    .iter()
                    .enumerate()
                    .any(|(set, _)| set_admits(d, set, seed, x, z, 0))
                {
                    interesting.push((x, z));
                }
            }
        }
        for (x, z) in interesting {
            for start in starts_cached(g, x, z).iter() {
                if !start.intersects_chunk(cx, cz) {
                    continue;
                }
                let Some(def) = d.structures.get(&start.structure) else {
                    continue;
                };
                let key = packed(start.key.0, start.key.1);
                let entry = found
                    .entry(start.structure.clone())
                    .or_insert_with(|| (def.step, Vec::new(), HashMap::new()));
                entry.1.push(key);
                entry.2.insert(key, clone_start(start));
            }
        }
        let mut ordered: Vec<(usize, usize, String, Vec<(u64, Start)>)> = found
            .into_iter()
            .map(|(id, (step, keys, starts))| {
                let slot = d.structures.get(&id).map(|s| s.slot).unwrap_or(0);
                (step, slot, id, keys, starts)
            })
            .map(|(step, slot, id, keys, mut starts)| {
                let ordered = mineshaft::reference_order_by_type(
                    keys.into_iter()
                        .map(|k| (0usize, k)),
                )
                .into_iter()
                .map(|(_, k)| (k, starts.remove(&k).expect("start")))
                .collect::<Vec<_>>();
                (step, slot, id, ordered)
            })
            .collect();
        ordered.sort_by_key(|(step, slot, _, _)| (*step, *slot));
        let step_count = ordered.iter().map(|(step, ..)| step + 1).max().unwrap_or(0);
        Self {
            items: ordered
                .into_iter()
                .map(|(step, _, id, starts)| (step, id, starts))
                .collect(),
            step_count,
            #[cfg(test)]
            rng_after: HashMap::new(),
        }
    }

    pub(crate) fn step_count(&self) -> usize {
        self.step_count
    }

    /// `ChunkGenerator.applyBiomeDecoration`'s structure half for one step:
    /// `setFeatureSeed(populationSeed, index, step)` for every structure of the
    /// step in registry order, then `StructureStart.placeInChunk` for each start
    /// of this chunk in reference order.
    pub(crate) fn place_step(
        &mut self,
        ctx: &mut FC,
        step: usize,
        population_seed: u64,
        cx: i32,
        cz: i32,
    ) {
        let Some(d) = defs() else { return };
        for (index, id) in d.by_step.get(step).cloned().unwrap_or_default().iter().enumerate() {
            let seed = population_seed
                .wrapping_add(index as u64)
                .wrapping_add(10_000u64.wrapping_mul(step as u64));
            let mut random = crate::generation_math::Xor::seed_worldgen(seed);
            crate::generation_math::XTRACE_FEAT.store(
                ((index as i64) << 32) | step as i64,
                std::sync::atomic::Ordering::Relaxed,
            );
            let mut mined = false;
            for (item_step, item_id, starts) in self.items.iter_mut() {
                if *item_step != step || item_id != id {
                    continue;
                }
                for (key, start) in starts.iter_mut() {
                    let (sx, sz) = (*key as i32, (*key >> 32) as i32);
                    if crate::trace_env::env_flag!("MINESHAFT_TRACE") && start.structure.starts_with("minecraft:mineshaft") {
                        eprintln!(
                            "MSTART {cx} {cz} {} {step} {index} {sx} {sz} {}",
                            start.structure,
                            match &start.pieces {
                                StartPieces::Mineshaft(m) => m.pieces.len(),
                                StartPieces::Template { pieces } => pieces.len(),
                            }
                        );
                    }
                    mined = true;
                    place_start(ctx, start, &mut random, cx, cz);
                }
            }
            let _ = mined;
            #[cfg(test)]
            self.rng_after.insert((step, index), random.state128());
        }
    }
}

fn clone_start(start: &Start) -> Start {
    Start {
        structure: start.structure.clone(),
        key: start.key,
        bbox: start.bbox,
        stub: start.stub,
        pieces: match &start.pieces {
            StartPieces::Mineshaft(m) => {
                StartPieces::Mineshaft(Box::new(mineshaft::clone_start(m)))
            }
            StartPieces::Template { pieces } => StartPieces::Template {
                pieces: pieces.clone(),
            },
        },
    }
}

/// `StructureStart.placeInChunk`: the pieces whose boxes meet the chunk, then the
/// structure's `afterPlace`.
fn place_start(ctx: &mut FC, start: &mut Start, random: &mut crate::generation_math::Xor, cx: i32, cz: i32) {
    let (x0, z0) = (cx << 4, cz << 4);
    let mut chunk_bb: BB = [x0, ctx.g.min_y, z0, x0 + 15, ctx.g.min_y + ctx.g.height - 1, z0 + 15];
    match &mut start.pieces {
        StartPieces::Mineshaft(m) => {
            mineshaft::place_start(ctx, random, cx, cz, start.key, m);
        }
        StartPieces::Template { pieces } => {
            let Some(reg) = crate::block_registry::breg() else {
                return;
            };
            let seed = ctx.g.wseed();
            let out = with_templates(|templates| {
                structure_template::place_structure(
                    ctx,
                    templates,
                    pieces,
                    random,
                    seed,
                    &mut chunk_bb,
                    start.stub,
                    reg,
                )
            });
            if let Ok(out) = out {
                crate::structure_template::stage_payloads(ctx, out);
            }
        }
    }
}

fn packed(x: i32, z: i32) -> u64 {
    u64::from(x as u32) | (u64::from(z as u32) << 32)
}

// ---------------------------------------------------------------- beardifier

/// `Beardifier.forStructuresInChunk`: every start whose structure adapts terrain,
/// with each piece close to this chunk turned into a rigid box, plus the jigsaw
/// junctions whose source lies within 12 blocks of the chunk.
fn beard_chunk_data(g: &mut Gen, cx: i32, cz: i32) -> BeardChunkData {
    let Some(d) = defs() else {
        return BeardChunkData::default();
    };
    let mut data = BeardChunkData::default();
    // Generating a start is expensive (a template structure reads neighbour columns for
    // its heightmaps), so first ask the cheap question: does any terrain-adapting set have
    // a *candidate* chunk in the ±8 window? For a chunk in the middle of the ocean that is
    // a few thousand LCG draws and no structure generation at all.
    let seed = g.wseed();
    let mut interesting: Vec<(i32, i32)> = Vec::new();
    for x in cx - 8..=cx + 8 {
        for z in cz - 8..=cz + 8 {
            if d.adapting_sets
                .iter()
                .any(|&set| candidate_admits(d, set, seed, x, z))
            {
                interesting.push((x, z));
            }
        }
    }
    if interesting.is_empty() {
        return data;
    }
    for (x, z) in interesting {
        for start in starts_cached(g, x, z).iter() {
                let Some(def) = d.structures.get(&start.structure) else {
                    continue;
                };
                if def.adjustment == BeardAdjustment::None {
                    continue;
                }
                // Vanilla filters per *piece* (`StructurePiece.isCloseToChunk(chunk, 12)`),
                // not per start: a start's union box may reach this chunk through a piece
                // that does not.
                let mut push = |b: &BB| {
                    if !bb_close_to_chunk(b, cx, cz, 12) {
                        return;
                    }
                    data.rigids.push(BeardRigid {
                        min: [b[0], b[1], b[2]],
                        max: [b[3], b[4], b[5]],
                        adjustment: def.adjustment,
                        // `StructurePiece.getGroundLevelDelta` is 0 for every non-pool
                        // piece; jigsaw pieces carry theirs with their junctions (Wave 3A).
                        ground_level_delta: 0,
                    });
                };
                match &start.pieces {
                    StartPieces::Mineshaft(m) => {
                        for piece in &m.pieces {
                            push(&piece.bounds);
                        }
                    }
                    StartPieces::Template { pieces } => {
                        for piece in pieces {
                            push(&piece.bb);
                        }
                    }
                }
            // Jigsaw junctions: no structure wired so far produces them (the pool-piece
            // path that would is the Wave 3A jigsaw hook), so the field stays empty. The
            // density function still reads it, and the scripted fixture pins that half.
        }
    }
    data
}

/// The resolver `worldgen` calls for a chunk's columns.
pub(crate) fn beard_resolver(g: &mut Gen, cx: i32, cz: i32) -> BeardChunkData {
    beard_chunk_data(g, cx, cz)
}

/// Register the pipeline's beardifier resolver with the terrain layer.
pub(crate) fn register_beard_resolver() {
    worldgen::register_beard_resolver(beard_resolver);
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod pipeline_tests {
    use super::*;

    /// `isStructureChunk` for a set id (null when the set did not load).
    fn admits(set_id: &str, seed: u64, x: i32, z: i32) -> Option<bool> {
        let d = defs()?;
        let &set = d.by_id.get(set_id)?;
        Some(set_admits(d, set, seed, x, z, 0))
    }

    /// `isPlacementChunk` alone: the column the exclusion fixture records separately from
    /// the frequency reducer and the zone.
    fn candidate_only(set_id: &str, seed: u64, x: i32, z: i32) -> Option<bool> {
        let d = defs()?;
        let &set = d.by_id.get(set_id)?;
        Some(candidate_admits(d, set, seed, x, z))
    }

    /// The native fixture: `SCENE`/`RIGID`/`JUNC`/`SAMPLE` (Beardifier densities),
    /// `ADMIT` (per-set admission), `EXCL` (the exclusion zone changing an answer) and
    /// `RING` (the concentric-ring chunks the port does not place yet).
    #[test]
    fn beardifier_density_and_structure_admission_match_1211_native() {
        const TSV: &str = include_str!("../tests/data/beardifier_pipeline_1211.tsv");
        let mut scenes: HashMap<String, BeardChunkData> = HashMap::new();
        let mut expected: HashMap<(String, i32, i32, i32), u64> = HashMap::new();
        let mut admits_rows = 0usize;
        let mut excl_rows = 0usize;
        let mut ring_rows = 0usize;
        let mut checked_admits = 0usize;
        let mut checked_excl = 0usize;
        let mut checked_density = 0usize;
        for line in TSV.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let number = |i: usize| f[i].parse::<i64>().unwrap();
            match f[0] {
                "SCENE" => {
                    scenes.insert(f[1].to_owned(), BeardChunkData::default());
                }
                "RIGID" => {
                    let data = scenes.get_mut(f[1]).expect("RIGID before SCENE");
                    data.rigids.push(BeardRigid {
                        min: [number(2) as i32, number(3) as i32, number(4) as i32],
                        max: [number(5) as i32, number(6) as i32, number(7) as i32],
                        adjustment: BeardAdjustment::parse(f[8]).expect("adjustment"),
                        ground_level_delta: number(9) as i32,
                    });
                }
                "JUNC" => {
                    let data = scenes.get_mut(f[1]).expect("JUNC before SCENE");
                    data.junctions.push(crate::worldgen::BeardJunction {
                        source_x: number(2) as i32,
                        source_ground_y: number(3) as i32,
                        source_z: number(4) as i32,
                        delta_y: number(5) as i32,
                        rigid: f[6] == "rigid",
                    });
                }
                "SAMPLE" => {
                    let bits = u64::from_str_radix(f[5], 16).expect("density bits");
                    expected.insert(
                        (f[1].to_owned(), number(2) as i32, number(3) as i32, number(4) as i32),
                        bits,
                    );
                }
                "SCENEMETA" => {}
                "ADMIT" => {
                    admits_rows += 1;
                    let want = f[5] == "1";
                    let seed = number(2) as u64;
                    let got = admits(f[1], seed, number(3) as i32, number(4) as i32)
                        .unwrap_or_else(|| panic!("set {} did not load", f[1]));
                    assert_eq!(got, want, "admission {} seed {seed} ({}, {})", f[1], f[3], f[4]);
                    checked_admits += 1;
                }
                "EXCL" => {
                    excl_rows += 1;
                    let seed = number(1) as u64;
                    let (x, z) = (number(2) as i32, number(3) as i32);
                    let want_placement = f[4] == "1";
                    let want_admitted = f[5] == "1";
                    assert_eq!(
                        candidate_only("minecraft:pillager_outposts", seed, x, z),
                        Some(want_placement),
                        "pillager candidate chunk seed {seed} ({x}, {z})"
                    );
                    assert_eq!(
                        admits("minecraft:pillager_outposts", seed, x, z),
                        Some(want_admitted),
                        "pillager exclusion seed {seed} ({x}, {z})"
                    );
                    checked_excl += 1;
                }
                "RING" => {
                    ring_rows += 1;
                    // The ring placement is a recorded unsupported arm: the port must not
                    // claim a stronghold chunk it cannot place.
                    assert_eq!(
                        admits("minecraft:strongholds", number(1) as u64, number(2) as i32, number(3) as i32),
                        Some(false),
                        "concentric rings are not placed yet"
                    );
                }
                other => panic!("unexpected fixture row {other}"),
            }
        }
        for ((scene, x, y, z), bits) in expected {
            let data = scenes.get(&scene).expect("scene");
            let got = compute_beardifier(data, x, y, z);
            assert_eq!(
                got.to_bits(),
                bits,
                "beardifier {scene} at ({x}, {y}, {z}): {got} vs {}",
                f64::from_bits(bits)
            );
            checked_density += 1;
        }
        assert_eq!(scenes.len(), 21);
        assert_eq!(checked_density, 420);
        assert_eq!(admits_rows, 3320);
        assert_eq!(checked_admits, admits_rows);
        assert_eq!(excl_rows, 73);
        assert_eq!(checked_excl, excl_rows);
        assert_eq!(ring_rows, 8);
    }

    /// The empty structure set must be the zero the port's build places: a chunk with no
    /// adapting structure adds nothing to the terrain density.
    #[test]
    fn an_empty_beard_set_adds_nothing() {
        let data = BeardChunkData::default();
        for (x, y, z) in [(0, 0, 0), (-20, 63, 7), (15, 320, -15)] {
            assert_eq!(compute_beardifier(&data, x, y, z).to_bits(), 0.0f64.to_bits());
        }
    }

    /// A column set built while the beardifier is resolving is the preliminary answer and
    /// must not enter the per-chunk cache: the same chunk's final columns need the data the
    /// nested build could not ask for.
    #[test]
    fn preliminary_columns_are_not_cached_as_final() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static NESTED: AtomicBool = AtomicBool::new(false);
        // A resolver for every chunk: the first call forces a nested terrain read (as a
        // structure's heightmap query does); afterwards every chunk carries empty data.
        fn resolver(g: &mut Gen, cx: i32, cz: i32) -> BeardChunkData {
            if !NESTED.swap(true, Ordering::SeqCst) {
                let _ = g.top_ws(cx * 16 + 24, cz * 16 + 8);
            }
            BeardChunkData::default()
        }
        let _guard = crate::worldgen::lock_gen_for_tests();
        assert!(crate::worldgen::init(7, crate::name_to_state));
        crate::worldgen::register_beard_resolver(resolver);
        // Chunk (0,0) asks for (1,0) while resolving, which used to leave a cache entry
        // without beard data; the direct read must rebuild it.
        let first = crate::worldgen::chunk_cols(0, 0).expect("columns");
        assert!(crate::worldgen::cols_have_beard(&first));
        let nested = crate::worldgen::chunk_cols(1, 0).expect("columns");
        assert!(
            crate::worldgen::cols_have_beard(&nested),
            "a preliminary column set was served from the cache"
        );
    }

    /// The pipeline must load every set and structure of the pinned data: a silently
    /// missing set admits nothing and looks like a seed without structures.
    #[test]
    fn all_pinned_sets_and_structures_load() {
        let d = defs().expect("structure definitions load");
        assert_eq!(d.sets.len(), 20);
        assert_eq!(d.structures.len(), 34);
        assert_eq!(d.by_step.iter().map(|v| v.len()).sum::<usize>(), 34);
        for set in &d.sets {
            for (id, _) in &set.entries {
                assert!(d.structures.contains_key(id), "{id} is not a loaded structure");
            }
        }
        // The exclusion zone is resolved by name; the pillager set must keep it.
        let pillager = &d.sets[*d.by_id.get("minecraft:pillager_outposts").expect("pillager set")];
        assert!(matches!(
            &pillager.placement,
            PlacementDef::Spread { exclusion: Some((other, 10)), .. } if other == "minecraft:villages"
        ));
    }
}
