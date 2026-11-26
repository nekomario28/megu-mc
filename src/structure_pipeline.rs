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
enum SinglePieceKind {
    BuriedTreasure,
    SwampHut,
    DesertPyramid,
    JungleTemple,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wave3aKind {
    EndCity,
    Mansion,
    Fortress,
    Monument,
    Stronghold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kick {
    /// `minecraft:mineshaft` / `minecraft:mineshaft_mesa`: the existing, verified
    /// procedural-piece owner in `mineshaft.rs`.
    Mineshaft,
    /// `structure_template::find_generation_point` + `place_structure`.
    Template,
    /// `SinglePieceStructure` (`buried_treasure`, `swamp_hut`, `desert_pyramid`, `jungle_temple`).
    SinglePiece(SinglePieceKind),
    /// `JigsawStructure` (`village_*`, `pillager_outpost`, `ancient_city`, `trail_ruins`, `trial_chambers`, ...).
    Jigsaw,
    /// Wave 3A (`end_city`, `mansion`, `fortress`, `monument`, `stronghold`).
    Wave3a(Wave3aKind),
}

struct StructureDef {
    idx: usize,
    kick: Kick,
    template: Option<structure_template::StructureKind>,
    step: usize,
    slot: usize,
    adjustment: BeardAdjustment,
    biomes: HashSet<String>,
}

struct SetMeta {
    all_mineshaft: bool,
    exclusion_idx: Option<usize>,
    /// Loaded entries as `(structure_idx, weight)` in set order.
    resolved_entries: Vec<(usize, i32)>,
    total_weight: i32,
}

struct PresetMasks {
    /// Per `StructureDef.idx`: `None` if `biomes` is empty (matches all) or any
    /// leaf id >= 128; `Some(mask)` otherwise (`Some(0)` means impossible in this preset).
    struct_masks: Vec<Option<u128>>,
    /// Per set index: true iff at least one loaded entry can match a biome in this preset.
    set_has_any_valid: Vec<bool>,
    /// All sets with `set_has_any_valid[i]`.
    all_valid_sets: Vec<usize>,
    /// Adapting sets with `set_has_any_valid[i]`.
    adapting_valid_sets: Vec<usize>,
    /// Pre-resolved `#minecraft:required_ocean_monument_surrounding` leaf mask.
    monument_surrounding_mask: Option<u128>,
    /// Per set index: pre-resolved `PlacementDef::Rings::preferred_biomes` leaf mask.
    ring_preferred_masks: Vec<Option<u128>>,
}

fn build_biome_mask(g: &Gen, names: &HashSet<String>) -> Option<u128> {
    if names.is_empty() {
        return None;
    }
    let mut mask = 0u128;
    for name in names {
        if let Some(leaf) = g.biome_leaf_of_name(name) {
            if usize::from(leaf) >= 128 {
                return None;
            }
            mask |= 1u128 << leaf;
        }
    }
    Some(mask)
}

fn preset_masks<'a>(d: &'a Defs, g: &Gen) -> Option<&'a PresetMasks> {
    g.biome_name_of_leaf(0)?;
    let slot = &d.preset_masks[preset_tag(g.preset) as usize];
    Some(slot.get_or_init(|| {
        let mut struct_masks = vec![None; d.struct_ids.len()];
        for def in d.structures.values() {
            struct_masks[def.idx] = build_biome_mask(g, &def.biomes);
        }
        let mut set_has_any_valid = Vec::with_capacity(d.sets.len());
        let mut all_valid_sets = Vec::with_capacity(d.sets.len());
        let mut ring_preferred_masks = Vec::with_capacity(d.sets.len());
        for (i, meta) in d.set_meta.iter().enumerate() {
            let valid = meta
                .resolved_entries
                .iter()
                .any(|&(sidx, _)| struct_masks[sidx] != Some(0));
            set_has_any_valid.push(valid);
            if valid {
                all_valid_sets.push(i);
            }
            let rmask = match &d.sets[i].placement {
                PlacementDef::Rings {
                    preferred_biomes, ..
                } => build_biome_mask(g, preferred_biomes),
                _ => None,
            };
            ring_preferred_masks.push(rmask);
        }
        let adapting_valid_sets: Vec<usize> = d
            .adapting_sets
            .iter()
            .copied()
            .filter(|&i| set_has_any_valid[i])
            .collect();
        let monument_surrounding_mask = build_biome_mask(g, &d.monument_surrounding);
        PresetMasks {
            struct_masks,
            set_has_any_valid,
            all_valid_sets,
            adapting_valid_sets,
            monument_surrounding_mask,
            ring_preferred_masks,
        }
    }))
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
    /// `ConcentricRingsStructurePlacement`: strongholds.
    Rings {
        distance: i32,
        spread: i32,
        count: i32,
        preferred_biomes: HashSet<String>,
    },
}

struct SetDef {
    placement: PlacementDef,
    entries: Vec<(String, i32)>,
}

struct Defs {
    sets: Vec<SetDef>,
    set_meta: Vec<SetMeta>,
    #[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
    by_id: HashMap<String, usize>,
    structures: HashMap<String, StructureDef>,
    struct_ids: Vec<String>,
    /// Structures of each step, in registry (slot) order.
    by_step: Vec<Vec<String>>,
    /// All set indices `0..sets.len()`.
    all_sets: Vec<usize>,
    /// Sets containing `minecraft:fortress`.
    fortress_sets: Vec<usize>,
    /// Sets with at least one structure whose `terrain_adaptation` is not `none`.
    /// Only these can contribute a beardifier box, so a chunk whose neighbourhood
    /// admits none of them needs no beardifier at all.
    adapting_sets: Vec<usize>,
    /// The complement of `adapting_sets`, in the same (ascending) set-index order.
    /// Beard resolution must never generate these; the decoration path still needs
    /// them, so the two halves are evaluated separately and merged by set index.
    non_adapting_sets: Vec<usize>,
    /// `#minecraft:required_ocean_monument_surrounding` biome set.
    monument_surrounding: HashSet<String>,
    /// Lazily resolved biome-leaf bitmasks per `NoisePreset` (0=Overworld, 1=Nether, 2=End).
    preset_masks: [std::sync::OnceLock<PresetMasks>; 3],
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
    structure_files.sort_by_key(|a| registry_sort_key(&a.0));
    let mut set_files: Vec<(String, Vec<u8>)> = Gen::dp_files("structure_set").into_iter().collect();
    set_files.sort_by_key(|a| registry_sort_key(&a.0));
    load_defs_from_files(&structure_files, &set_files)
}

/// Parse one `worldgen/structure_set` JSON into a `SetDef`.
///
/// Kept separate from the file loop so a single malformed set can be skipped: the
/// old loader's `?` on `placement`/`structures` returned `None` from the whole
/// registry load, which silently disabled every structure in the seed.
fn parse_set(bytes: &[u8]) -> Result<SetDef, String> {
    let j = crate::json::jparse(bytes).ok_or("not valid JSON")?;
    let p = j.get("placement").ok_or("missing `placement`")?;
    let placement_type = p.get("type").map(|v| v.str()).unwrap_or_default();
    let placement = match placement_type {
        "minecraft:random_spread" => {
            let number = |k: &str, d: f64| p.get(k).map(|v| v.num()).unwrap_or(d);
            let reducer_name = p
                .get("frequency_reduction_method")
                .map(|v| v.str())
                .unwrap_or("default");
            let reducer = match reducer_name {
                "default" => 0,
                "legacy_type_1" => 1,
                "legacy_type_2" => 2,
                "legacy_type_3" => 3,
                other => return Err(format!("unknown frequency reducer {other}")),
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
        "minecraft:concentric_rings" => PlacementDef::Rings {
            distance: p.get("distance").map(|v| v.num()).unwrap_or(0.0) as i32,
            spread: p.get("spread").map(|v| v.num()).unwrap_or(0.0) as i32,
            count: p.get("count").map(|v| v.num()).unwrap_or(0.0) as i32,
            preferred_biomes: p
                .get("preferred_biomes")
                .map(|v| biome_set(v, &mut HashSet::new()))
                .unwrap_or_default(),
        },
        other => return Err(format!("unknown placement {other}")),
    };
    let entries: Vec<(String, i32)> = j
        .get("structures")
        .and_then(|v| v.as_arr())
        .ok_or("missing or invalid `structures`")?
        .iter()
        .filter_map(|e| {
            let structure = ns(e.get("structure")?.str());
            let weight = e.get("weight").map(|v| v.num()).unwrap_or(1.0) as i32;
            Some((structure, weight))
        })
        .collect();
    if entries.is_empty() {
        return Err("`structures` has no usable entries".into());
    }
    Ok(SetDef { placement, entries })
}

/// The loader proper, over already-enumerated `(id, bytes)` pairs so a test can hand
/// it a pack with one broken entry in it.
///
/// Every per-entry failure is a **skip**, never a `None`: native's
/// `RegistryDataLoader` reports the resource it cannot deserialize and keeps the
/// rest of the registry, and this port has to do the same. Before, one malformed
/// structure or structure_set JSON returned `None` from here, which disabled the
/// whole pipeline and was indistinguishable from "this seed has no structures".
///
/// A skipped structure also takes no **slot**: `counts[step]` only advances for an
/// entry that reached `by_step`, so `setFeatureSeed(pop, slot, step)` still numbers
/// the survivors exactly the way the registry enumerates them. With no malformed
/// file the result is bit-identical to the pre-skip loader (pinned by the
/// `vanilla_defs_load_identically` fingerprint test).
fn load_defs_from_files(
    structure_files: &[(String, Vec<u8>)],
    set_files: &[(String, Vec<u8>)],
) -> Option<Defs> {
    let mut counts = [0usize; STRUCTURE_STEPS.len()];
    let mut structures = HashMap::new();
    let mut struct_ids = Vec::new();
    let mut by_step: Vec<Vec<String>> = vec![Vec::new(); STRUCTURE_STEPS.len()];
    for (id, bytes) in structure_files {
        let Some(j) = crate::json::jparse(bytes) else {
            eprintln!("SPIPE skipping malformed structure {id}: not valid JSON");
            continue;
        };
        // 1.21.1 writes `step` on every structure; a missing or unknown one is a
        // parse failure rather than a default step.
        let Some(step) = j.get("step").map(|v| v.str()).and_then(step_index) else {
            eprintln!("SPIPE skipping malformed structure {id}: missing or unknown `step`");
            continue;
        };
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
            | "minecraft:ruined_portal" => match structure_template::parse_structure(&j) {
                Ok(kind) => (Kick::Template, Some(kind)),
                Err(why) => {
                    eprintln!("SPIPE skipping malformed structure {id}: template payload {why}");
                    continue;
                }
            },
            "minecraft:buried_treasure" => {
                (Kick::SinglePiece(SinglePieceKind::BuriedTreasure), None)
            }
            "minecraft:swamp_hut" => (Kick::SinglePiece(SinglePieceKind::SwampHut), None),
            "minecraft:desert_pyramid" => {
                (Kick::SinglePiece(SinglePieceKind::DesertPyramid), None)
            }
            "minecraft:jungle_temple" => (Kick::SinglePiece(SinglePieceKind::JungleTemple), None),
            "minecraft:jigsaw" => (Kick::Jigsaw, None),
            "minecraft:end_city" => (Kick::Wave3a(Wave3aKind::EndCity), None),
            "minecraft:woodland_mansion" => (Kick::Wave3a(Wave3aKind::Mansion), None),
            "minecraft:fortress" => (Kick::Wave3a(Wave3aKind::Fortress), None),
            "minecraft:ocean_monument" => (Kick::Wave3a(Wave3aKind::Monument), None),
            "minecraft:stronghold" => (Kick::Wave3a(Wave3aKind::Stronghold), None),
            other => {
                eprintln!("SPIPE skipping malformed structure {id}: unknown type {other}");
                continue;
            }
        };
        // The slot is taken last, so an entry that never made it into `by_step`
        // cannot shift the numbering of the ones after it.
        let slot = counts[step];
        counts[step] += 1;
        let idx = struct_ids.len();
        struct_ids.push(id.clone());
        by_step[step].push(id.clone());
        structures.insert(
            id.clone(),
            StructureDef {
                idx,
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
    let mut sets = Vec::new();
    let mut by_id = HashMap::new();
    for (id, bytes) in set_files {
        match parse_set(bytes) {
            Ok(set) => {
                by_id.insert(id.clone(), sets.len());
                sets.push(set);
            }
            Err(why) => eprintln!("SPIPE skipping malformed structure_set {id}: {why}"),
        }
    }
    if crate::trace_env::env_flag!("SPIPE_TRACE") {
        eprintln!(
            "SPIPE defs: {} sets, {} structures, by_step {:?}",
            sets.len(),
            structures.len(),
            by_step.iter().map(|v| v.len()).collect::<Vec<_>>()
        );
    }
    let is_adapting: Vec<bool> = sets
        .iter()
        .map(|set| {
            set.entries.iter().any(|(id, _)| {
                structures
                    .get(id)
                    .is_some_and(|def| def.adjustment != BeardAdjustment::None)
            })
        })
        .collect();
    let adapting_sets: Vec<usize> = is_adapting
        .iter()
        .enumerate()
        .filter_map(|(i, &yes)| yes.then_some(i))
        .collect();
    let non_adapting_sets: Vec<usize> = is_adapting
        .iter()
        .enumerate()
        .filter_map(|(i, &yes)| (!yes).then_some(i))
        .collect();
    let monument_surrounding =
        resource("tags/worldgen/biome", "minecraft:required_ocean_monument_surrounding")
            .and_then(|j| j.get("values").map(|v| biome_set(v, &mut HashSet::new())))
            .unwrap_or_default();
    let set_meta: Vec<SetMeta> = sets
        .iter()
        .map(|set| {
            let all_mineshaft = set.entries.iter().all(|(id, _)| {
                structures
                    .get(id)
                    .is_some_and(|def| def.kick == Kick::Mineshaft)
            });
            let exclusion_idx = match &set.placement {
                PlacementDef::Spread {
                    exclusion: Some((other, _)),
                    ..
                } => by_id.get(other).copied(),
                _ => None,
            };
            let resolved_entries: Vec<(usize, i32)> = set
                .entries
                .iter()
                .filter_map(|(id, w)| structures.get(id).map(|def| (def.idx, *w)))
                .collect();
            let total_weight = resolved_entries.iter().map(|(_, w)| *w).sum();
            SetMeta {
                all_mineshaft,
                exclusion_idx,
                resolved_entries,
                total_weight,
            }
        })
        .collect();
    let all_sets: Vec<usize> = (0..sets.len()).collect();
    let fortress_sets: Vec<usize> = sets
        .iter()
        .enumerate()
        .filter_map(|(index, set)| {
            set.entries
                .iter()
                .any(|(id, _)| id == "minecraft:fortress")
                .then_some(index)
        })
        .collect();
    Some(Defs {
        sets,
        set_meta,
        by_id,
        structures,
        struct_ids,
        by_step,
        all_sets,
        fortress_sets,
        adapting_sets,
        non_adapting_sets,
        monument_surrounding,
        preset_masks: [
            std::sync::OnceLock::new(),
            std::sync::OnceLock::new(),
            std::sync::OnceLock::new(),
        ],
    })
}

// ---------------------------------------------------------------- admission

fn preset_tag(preset: worldgen::NoisePreset) -> u8 {
    match preset {
        worldgen::NoisePreset::Overworld => 0,
        worldgen::NoisePreset::Nether => 1,
        worldgen::NoisePreset::End => 2,
    }
}

type RingSetMap = HashMap<(u64, u8, usize), std::sync::Arc<HashSet<(i32, i32)>>>;
type RingCandidateMap = HashMap<(u64, u8, usize), std::sync::Arc<Vec<((i32, i32), u64)>>>;

thread_local! {
    /// Cached relocated stronghold chunks per `(seed, preset_tag, set_index)` — the eager,
    /// all-128 view. Tests and any caller that must enumerate every ring use this; admission
    /// checks use the lazy per-index path below instead.
    static RING_CACHE: std::cell::RefCell<RingSetMap> =
        std::cell::RefCell::new(HashMap::new());
    /// The cheap, pure candidate list `(candidate chunk, child RNG state)` per ring set.
    /// Building it costs a few LCG draws and **no** biome lookups.
    static RING_CANDIDATES: std::cell::RefCell<RingCandidateMap> =
        std::cell::RefCell::new(HashMap::new());
    /// Lazily relocated chunk for one ring index, keyed by `(seed, preset_tag, set, index)`.
    static RING_I_CACHE: std::cell::RefCell<RingIndexCache> = std::cell::RefCell::new(RingIndexCache {
        map: HashMap::new(),
        ord: std::collections::VecDeque::new(),
    });
}

/// TLS retention for [`RING_I_CACHE`], bounded FIFO like the other caches.
struct RingIndexCache {
    map: HashMap<(u64, u8, usize, i32), (i32, i32)>,
    ord: std::collections::VecDeque<(u64, u8, usize, i32)>,
}

/// The ring placement parameters, or `None` outside an Overworld concentric-ring set.
fn ring_params<'a>(d: &'a Defs, g: &Gen, set: usize) -> Option<(i32, i32, i32, &'a HashSet<String>)> {
    if g.preset != worldgen::NoisePreset::Overworld {
        return None;
    }
    match &d.sets[set].placement {
        PlacementDef::Rings {
            distance,
            spread,
            count,
            preferred_biomes,
        } => Some((*distance, *spread, *count, preferred_biomes)),
        _ => None,
    }
}

/// The cheap candidate list for one ring set, cached by its pure key.
fn ring_candidates_cached(
    d: &Defs,
    g: &Gen,
    set: usize,
) -> std::sync::Arc<Vec<((i32, i32), u64)>> {
    let key = (g.wseed(), preset_tag(g.preset), set);
    if let Some(v) = RING_CANDIDATES.with(|c| c.borrow().get(&key).cloned()) {
        return v;
    }
    // `concentric_rings` delegates to `concentric_ring_candidates`; recording the child
    // state in its callback recovers the exact candidate list without a biome lookup.
    // Routing through `mineshaft`'s re-export keeps `feature`'s source-edge ratchet intact.
    let mut candidates = Vec::new();
    if let Some((distance, spread, count, _)) = ring_params(d, g, set) {
        let _ = mineshaft::concentric_rings(g.wseed(), distance, spread, count, |candidate, rng| {
            candidates.push((candidate, rng.state48()));
            None
        });
    }
    let arc = std::sync::Arc::new(candidates);
    RING_CANDIDATES.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() > 16 {
            map.clear();
        }
        map.insert(key, arc.clone());
    });
    arc
}

/// Resolve one ring index's biome relocation, without the eager all-128 scan.
///
/// The biome search is seeded with `last = None` for this index alone; the unit test
/// `lazy_ring_indices_match_eager_concentric_rings` checks all 128 against the eager
/// path (which threads one `last` across the whole loop).
fn ring_chunk_i_uncached(d: &Defs, g: &mut Gen, set: usize, i: i32) -> (i32, i32) {
    let Some((_, _, _, preferred_biomes)) = ring_params(d, g, set) else {
        return (i32::MIN, i32::MIN);
    };
    let Some(&(candidate, child_state)) = ring_candidates_cached(d, g, set).get(i as usize) else {
        return (i32::MIN, i32::MIN);
    };
    let mut child = crate::generation_math::Lcg::from_raw_state(child_state);
    if let Some(mask) = preset_masks(d, g).and_then(|pm| pm.ring_preferred_masks[set]) {
        return mineshaft::concentric_rings_target(
            candidate,
            &mut child,
            112,
            |qx, qy, qz| g.biome_quart(qx, qy, qz, None),
            |b| b.is_some_and(|(_, leaf)| usize::from(leaf) < 128 && (mask & (1u128 << leaf)) != 0),
        )
        .unwrap_or(candidate);
    }
    let preferred: HashSet<u16> = preferred_biomes
        .iter()
        .filter_map(|name| g.biome_leaf_of_name(name))
        .collect();
    mineshaft::concentric_rings_target(
        candidate,
        &mut child,
        112,
        |qx, qy, qz| g.biome_quart(qx, qy, qz, None),
        |b| b.is_some_and(|(_, leaf)| preferred.contains(&leaf)),
    )
    .unwrap_or(candidate)
}

/// One resolved ring index, single-flight inside a round and TLS-cached otherwise.
fn ring_chunk_i(d: &Defs, g: &mut Gen, set: usize, i: i32) -> (i32, i32) {
    let key = (g.wseed(), preset_tag(g.preset), set, i);
    if let Some(cache) = g.starts_round.and_then(round_cache) {
        let token = crate::worldgen::flight_token(RING_I_FLIGHT_DOMAIN, &key);
        return cache.ring_i.get_or_build(key, token, || ring_chunk_i_uncached(d, g, set, i));
    }
    if let Some(v) = RING_I_CACHE.with(|c| c.borrow().map.get(&key).copied()) {
        return v;
    }
    let v = ring_chunk_i_uncached(d, g, set, i);
    RING_I_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.map.insert(key, v);
        c.ord.push_back(key);
        if c.ord.len() > 4096 {
            if let Some(old) = c.ord.pop_front() {
                c.map.remove(&old);
            }
        }
    });
    v
}

/// Does any ring of `set` relocate to `(x, z)`? The candidate-to-relocation offset is
/// at most 7 chunks (radius 112 blocks = 28 quarts, `>> 4`), so a ±8 chunk window is
/// conservative. Only those few indices are resolved — the other ~120 never touch a biome.
fn ring_admit(d: &Defs, g: &mut Gen, set: usize, x: i32, z: i32) -> bool {
    let candidates = ring_candidates_cached(d, g, set);
    for (i, &((cx, cz), _)) in candidates.iter().enumerate() {
        if (cx - x).abs() <= 8 && (cz - z).abs() <= 8 {
            let (rx, rz) = ring_chunk_i(d, g, set, i as i32);
            if (rx, rz) == (x, z) {
                return true;
            }
        }
    }
    false
}

#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn ring_chunks(d: &Defs, g: &mut Gen, set: usize) -> std::sync::Arc<HashSet<(i32, i32)>> {
    let seed = g.wseed();
    let tag = preset_tag(g.preset);
    let key = (seed, tag, set);
    if let Some(cache) = g.starts_round.and_then(round_cache) {
        let token = crate::worldgen::flight_token(RINGS_FLIGHT_DOMAIN, &key);
        return cache
            .rings
            .get_or_build(key, token, || std::sync::Arc::new(ring_chunks_uncached(d, g, set)));
    }
    if let Some(cached) = RING_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return cached;
    }
    let arc = std::sync::Arc::new(ring_chunks_uncached(d, g, set));
    RING_CACHE.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() > 64 {
            map.clear();
        }
        map.insert(key, arc.clone());
    });
    arc
}

/// [`ring_chunks`]'s computation, without the caches: the eager concentric-ring scan,
/// which reads the biome quart sweep for all 128 candidates and threads one `last`
/// hint across them. Kept as the reference the lazy path is tested against.
#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn ring_chunks_uncached(d: &Defs, g: &mut Gen, set: usize) -> HashSet<(i32, i32)> {
    let seed = g.wseed();
    if g.preset != worldgen::NoisePreset::Overworld {
        HashSet::new()
    } else if let PlacementDef::Rings {
        distance,
        spread,
        count,
        preferred_biomes,
    } = &d.sets[set].placement
    {
        let preferred: HashSet<u16> = preferred_biomes
            .iter()
            .filter_map(|name| g.biome_leaf_of_name(name))
            .collect();
        let mut last: Option<u16> = None;
        mineshaft::concentric_rings(seed, *distance, *spread, *count, |candidate, rng| {
            mineshaft::concentric_rings_target(
                candidate,
                rng,
                112,
                |qx, qy, qz| {
                    let res = g.biome_quart(qx, qy, qz, last);
                    if let Some((_, leaf)) = res {
                        last = Some(leaf);
                    }
                    res
                },
                |b| b.is_some_and(|(_, leaf)| preferred.contains(&leaf)),
            )
        })
        .unwrap_or_default()
        .into_iter()
        .collect()
    } else {
        HashSet::new()
    }
}

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
        return false;
    };
    let Some(spread) = mineshaft::spread_placement(*spacing, *separation, *salt, *triangular) else {
        return false;
    };
    spread.candidate(seed, x, z) == (x, z)
}

#[allow(dead_code)]
fn candidate_admits_live(d: &Defs, g: &mut Gen, set: usize, seed: u64, x: i32, z: i32) -> bool {
    match &d.sets[set].placement {
        PlacementDef::Spread { .. } => candidate_admits(d, set, seed, x, z),
        PlacementDef::Rings { .. } => ring_admit(d, g, set, x, z),
    }
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
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
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
        Some((_, count)) => {
            let count = *count;
            let Some(other) = d.set_meta[set].exclusion_idx else {
                return false;
            };
            !mineshaft::exclusion_forbids(count, x, z, |ox, oz| {
                set_admits(d, other, seed, ox, oz, depth + 1)
            })
        }
    }
}

fn set_admits_live(
    d: &Defs,
    g: &mut Gen,
    set: usize,
    seed: u64,
    x: i32,
    z: i32,
    depth: u32,
) -> bool {
    if depth > 2 {
        return false;
    }
    match &d.sets[set].placement {
        PlacementDef::Rings { .. } => ring_admit(d, g, set, x, z),
        PlacementDef::Spread { exclusion, .. } => {
            if !placement_admits(d, set, seed, x, z) {
                return false;
            }
            match exclusion {
                None => true,
                Some((_, count)) => {
                    let count = *count;
                    let Some(other) = d.set_meta[set].exclusion_idx else {
                        return false;
                    };
                    !mineshaft::exclusion_forbids(count, x, z, |ox, oz| {
                        set_admits_live(d, g, other, seed, ox, oz, depth + 1)
                    })
                }
            }
        }
    }
}

/// The chunks in the ±8 window of `(cx, cz)` admitted by any of `sets`, as a (possibly
/// duplicated, unsorted) list. Callers must `sort_unstable(); dedup();` — sorting
/// `(x, z)` tuples recovers the exact row-major encounter order of the old
/// `for x { for z { .. } }` scan, which fixes floating-point summation order in
/// `compute_beardifier` and the reference insertion order in `References::collect`.
fn window_admitted_chunks(
    d: &Defs,
    g: &mut Gen,
    sets: &[usize],
    cx: i32,
    cz: i32,
) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    let seed = g.wseed();
    for &set in sets {
        match &d.sets[set].placement {
            PlacementDef::Spread {
                spacing,
                separation,
                salt,
                triangular,
                ..
            } => {
                if *spacing <= 0 {
                    continue;
                }
                let Some(spread) =
                    mineshaft::spread_placement(*spacing, *separation, *salt, *triangular)
                else {
                    continue;
                };
                // A spread candidate of cell (gx, gz) lies in
                // [gx*spacing, gx*spacing + spacing - 1]; div_euclid is Math.floorDiv,
                // so this is exactly the set of cells whose range meets the window.
                let gx0 = (cx - 8).div_euclid(*spacing);
                let gx1 = (cx + 8).div_euclid(*spacing);
                let gz0 = (cz - 8).div_euclid(*spacing);
                let gz1 = (cz + 8).div_euclid(*spacing);
                for gx in gx0..=gx1 {
                    for gz in gz0..=gz1 {
                        let (x, z) = spread.candidate(seed, gx * *spacing, gz * *spacing);
                        if (x - cx).abs() > 8 || (z - cz).abs() > 8 {
                            continue;
                        }
                        if set_admits_live(d, g, set, seed, x, z, 0) {
                            out.push((x, z));
                        }
                    }
                }
            }
            PlacementDef::Rings { .. } => {
                let candidates = ring_candidates_cached(d, g, set);
                for (i, &((rx_candidate, rz_candidate), _)) in candidates.iter().enumerate() {
                    // The relocation moves a candidate by at most 7 chunks (radius 112),
                    // so a ±15 window around the query is conservative.
                    if (rx_candidate - cx).abs() > 15 || (rz_candidate - cz).abs() > 15 {
                        continue;
                    }
                    let (rx, rz) = ring_chunk_i(d, g, set, i as i32);
                    if (rx - cx).abs() <= 8 && (rz - cz).abs() <= 8 {
                        out.push((rx, rz));
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- starts

pub(crate) enum StartPieces {
    Mineshaft(Box<mineshaft::Start>),
    Template { pieces: Vec<Piece> },
    BuriedTreasure(mineshaft::BuriedTreasurePiece),
    SwampHut(mineshaft::SwampHutPiece),
    DesertPyramid(mineshaft::DesertPyramidPiece),
    JungleTemple(mineshaft::JungleTemplePiece),
    Fortress {
        pieces: Vec<mineshaft::FortressPiece>,
        loots: mineshaft::LootMap,
    },
    Monument {
        layout: Box<mineshaft::MonumentLayout>,
        loots: mineshaft::LootMap,
    },
    Stronghold(Box<mineshaft::StrongholdStart>),
    EndCity {
        pieces: Vec<structure_template::TemplatePiece>,
    },
    Mansion {
        pieces: Vec<structure_template::TemplatePiece>,
    },
    Jigsaw {
        pieces: Vec<JigsawPiece>,
    },
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

fn union_bb(boxes: impl IntoIterator<Item = BB>) -> Option<BB> {
    let mut out = [i32::MAX; 6];
    out[3..].fill(i32::MIN);
    let mut any = false;
    for b in boxes {
        bb_encapsulate(&mut out, &b);
        any = true;
    }
    any.then_some(out)
}

thread_local! {
    /// `StructureTemplateManager.getOrCreate`: the native manager caches templates
    /// per world; this is the same cache, kept off the generator (whose layer may
    /// not name `Templates`). Held behind `Rc` so a nested generation reads the
    /// same cache: a structure's world query can resolve the beardifier, which
    /// generates the neighbouring chunks' starts and reaches this cache again
    /// (see the `Templates` note on the nether-decoration panic).
    static TEMPLATES: std::cell::RefCell<Option<std::rc::Rc<Templates>>> = const { std::cell::RefCell::new(None) };
}

fn with_templates<R>(f: impl FnOnce(&Templates) -> R) -> R {
    let templates = TEMPLATES.with(|slot| {
        let mut held = slot.borrow_mut();
        std::rc::Rc::clone(held.get_or_insert_with(|| {
            std::rc::Rc::new(Templates::new(|id| Gen::dp_resource("structure", id, "nbt")))
        }))
        // the cell borrow ends here, so a nested `with_templates` can take it
    });
    f(&templates)
}

#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
fn valid_biome(g: &mut Gen, allowed: &HashSet<String>, pos: (i32, i32, i32)) -> bool {
    valid_biome_fast(g, allowed, None, pos)
}

fn valid_biome_fast(
    g: &mut Gen,
    allowed: &HashSet<String>,
    mask: Option<u128>,
    pos: (i32, i32, i32),
) -> bool {
    if allowed.is_empty() {
        return true;
    }
    if mask == Some(0) {
        return false;
    }
    // Structure.isValidBiome samples the noise source at QuartPos.fromBlock,
    // without the Voronoi zoom used by LevelReader.getBiome.
    if let Some((_, leaf)) = g.biome_quart(pos.0 >> 2, pos.1 >> 2, pos.2 >> 2, None) {
        if let Some(m) = mask {
            if usize::from(leaf) < 128 {
                return (m & (1u128 << leaf)) != 0;
            }
        }
        return g
            .biome_name_of_leaf(leaf)
            .is_some_and(|name| allowed.contains(&name));
    }
    // Synthetic generators may have no noise-biome source.
    let Some(biome) = FC::new(g).biome(pos.0, pos.1, pos.2) else {
        return false;
    };
    allowed.contains(&biome)
}

fn min_corner_occupied_y(g: &mut Gen, x: i32, z: i32, dx: i32, dz: i32) -> i32 {
    let h0 = g.top_ws(x, z) - 1;
    let h1 = g.top_ws(x, z + dz) - 1;
    let h2 = g.top_ws(x + dx, z) - 1;
    let h3 = g.top_ws(x + dx, z + dz) - 1;
    h0.min(h1).min(h2).min(h3)
}

fn monument_surrounding_ok(
    g: &mut Gen,
    surrounding: &HashSet<String>,
    mask: Option<u128>,
    cx: i32,
    cz: i32,
) -> bool {
    if surrounding.is_empty() {
        return true;
    }
    if mask == Some(0) {
        return false;
    }
    let (x, y, z) = (cx * 16 + 9, g.sea, cz * 16 + 9);
    let Some((_, center_leaf)) = g.biome_quart(x >> 2, y >> 2, z >> 2, None) else {
        return false;
    };
    let (qx0, qx1) = ((x - 29) >> 2, (x + 29) >> 2);
    let (qy0, qy1) = ((y - 29) >> 2, (y + 29) >> 2);
    let (qz0, qz1) = ((z - 29) >> 2, (z + 29) >> 2);
    if let Some(m) = mask {
        if usize::from(center_leaf) >= 128 || (m & (1u128 << center_leaf)) == 0 {
            return false;
        }
        let mut last = Some(center_leaf);
        for qx in qx0..=qx1 {
            for qy in qy0..=qy1 {
                for qz in qz0..=qz1 {
                    let Some((_, leaf)) = g.biome_quart(qx, qy, qz, last) else {
                        return false;
                    };
                    last = Some(leaf);
                    if usize::from(leaf) >= 128 || (m & (1u128 << leaf)) == 0 {
                        return false;
                    }
                }
            }
        }
        return true;
    }
    let allowed: HashSet<u16> = surrounding
        .iter()
        .filter_map(|name| g.biome_leaf_of_name(name))
        .collect();
    if !allowed.contains(&center_leaf) {
        return false;
    }
    let mut last = Some(center_leaf);
    for qx in qx0..=qx1 {
        for qy in qy0..=qy1 {
            for qz in qz0..=qz1 {
                let Some((_, leaf)) = g.biome_quart(qx, qy, qz, last) else {
                    return false;
                };
                last = Some(leaf);
                if !allowed.contains(&leaf) {
                    return false;
                }
            }
        }
    }
    true
}

/// Generate the structure this set entry names, or `None` when it refuses.
#[expect(clippy::too_many_arguments)]
fn try_generate(
    d: &Defs,
    g: &mut Gen,
    id: &str,
    def: &StructureDef,
    pm: Option<&PresetMasks>,
    seed: u64,
    cx: i32,
    cz: i32,
) -> Option<Start> {
    let biome_mask = pm.and_then(|p| p.struct_masks[def.idx]);
    if biome_mask == Some(0) {
        return None;
    }
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
            let (stub, build) = found?;
            // The gate is `Structure.findValidGenerationPoint`, which filters on the
            // stub the structure's own `findGenerationPoint` returned - not on the
            // chunk centre. Reading the centre's biome at a fixed y=64 asks a different
            // question, and in the Nether it answers a *different biome*: seed 4242
            // chunk (-24,22) scans to (-382,88,367), whose noise biome is
            // `basalt_deltas` (native quart(-96,22,91) = basalt_deltas), while the
            // centre (-376,64,360) is `soul_sand_valley`. `#has_structure/nether_fossil`
            // holds only soul_sand_valley, so vanilla refuses the start and the port
            // used to place it (spurious beard at bb=[-384,88,366,-382,91,367]).
            if !valid_biome_fast(fc.g, &def.biomes, biome_mask, stub) {
                return None;
            }
            let pieces = with_templates(|templates| build(&mut fc, templates, &mut gen, reg).ok())?;
            let bbox = union_bb(pieces.iter().map(|p| p.bb))?;
            Some(Start {
                structure: id.to_owned(),
                key: (cx, cz),
                bbox,
                stub,
                pieces: StartPieces::Template { pieces },
            })
        }
        Kick::SinglePiece(kind) => match kind {
            SinglePieceKind::BuriedTreasure => {
                let stub = (cx * 16 + 8, g.top_oc(cx * 16 + 8, cz * 16 + 8) - 1, cz * 16 + 8);
                if !valid_biome_fast(g, &def.biomes, biome_mask, stub) {
                    return None;
                }
                let piece = mineshaft::BuriedTreasurePiece::new(cx, cz);
                let bbox = piece.bounds;
                Some(Start {
                    structure: id.to_owned(),
                    key: (cx, cz),
                    bbox,
                    stub,
                    pieces: StartPieces::BuriedTreasure(piece),
                })
            }
            SinglePieceKind::SwampHut => {
                let stub = (cx * 16 + 8, g.top_ws(cx * 16 + 8, cz * 16 + 8) - 1, cz * 16 + 8);
                if !valid_biome_fast(g, &def.biomes, biome_mask, stub) {
                    return None;
                }
                let mut rng = crate::generation_math::Lcg::for_large_feature(seed, cx, cz);
                let piece = mineshaft::SwampHutPiece::new(&mut rng, cx * 16, cz * 16);
                let bbox = piece.bounds;
                Some(Start {
                    structure: id.to_owned(),
                    key: (cx, cz),
                    bbox,
                    stub,
                    pieces: StartPieces::SwampHut(piece),
                })
            }
            SinglePieceKind::DesertPyramid => {
                let stub = (cx * 16 + 8, g.top_ws(cx * 16 + 8, cz * 16 + 8) - 1, cz * 16 + 8);
                if !valid_biome_fast(g, &def.biomes, biome_mask, stub) {
                    return None;
                }
                if min_corner_occupied_y(g, cx * 16, cz * 16, 21, 21) < g.sea {
                    return None;
                }
                let mut rng = crate::generation_math::Lcg::for_large_feature(seed, cx, cz);
                let piece = mineshaft::DesertPyramidPiece::new(&mut rng, cx * 16, cz * 16);
                let bbox = piece.bounds;
                Some(Start {
                    structure: id.to_owned(),
                    key: (cx, cz),
                    bbox,
                    stub,
                    pieces: StartPieces::DesertPyramid(piece),
                })
            }
            SinglePieceKind::JungleTemple => {
                let stub = (cx * 16 + 8, g.top_ws(cx * 16 + 8, cz * 16 + 8) - 1, cz * 16 + 8);
                if !valid_biome_fast(g, &def.biomes, biome_mask, stub) {
                    return None;
                }
                if min_corner_occupied_y(g, cx * 16, cz * 16, 12, 15) < g.sea {
                    return None;
                }
                let mut rng = crate::generation_math::Lcg::for_large_feature(seed, cx, cz);
                let piece = mineshaft::JungleTemplePiece::new(&mut rng, cx * 16, cz * 16);
                let bbox = piece.bounds;
                Some(Start {
                    structure: id.to_owned(),
                    key: (cx, cz),
                    bbox,
                    stub,
                    pieces: StartPieces::JungleTemple(piece),
                })
            }
        },
        Kick::Jigsaw => {
            let reg = crate::block_registry::breg()?;
            let (stub, pieces) =
                generate_jigsaw_pieces_live(g, id, &def.biomes, seed, cx, cz, reg)?;
            let bbox = union_bb(pieces.iter().map(|p| p.bb))?;
            Some(Start {
                structure: id.to_owned(),
                key: (cx, cz),
                bbox,
                stub,
                pieces: StartPieces::Jigsaw { pieces },
            })
        }
        Kick::Wave3a(kind) => match kind {
            Wave3aKind::Fortress => {
                let stub = (cx * 16, 64, cz * 16);
                if !valid_biome_fast(g, &def.biomes, biome_mask, stub) {
                    return None;
                }
                let mut rng = crate::generation_math::Lcg::for_large_feature(seed, cx, cz);
                let layout = mineshaft::generate_fortress_pieces(&mut rng, cx, cz);
                if layout.pieces.is_empty() {
                    return None;
                }
                let bbox = layout.bounds;
                Some(Start {
                    structure: id.to_owned(),
                    key: (cx, cz),
                    bbox,
                    stub,
                    pieces: StartPieces::Fortress {
                        pieces: layout.pieces,
                        loots: std::collections::BTreeMap::new(),
                    },
                })
            }
            Wave3aKind::Monument => {
                let mono_mask = pm.and_then(|p| p.monument_surrounding_mask);
                if !monument_surrounding_ok(g, &d.monument_surrounding, mono_mask, cx, cz) {
                    return None;
                }
                let stub = (cx * 16 + 8, g.top_oc(cx * 16 + 8, cz * 16 + 8) - 1, cz * 16 + 8);
                if !valid_biome_fast(g, &def.biomes, biome_mask, stub) {
                    return None;
                }
                let mut rng = crate::generation_math::Lcg::for_large_feature(seed, cx, cz);
                let layout = mineshaft::generate_monument_pieces(&mut rng, cx, cz);
                let bbox = union_bb(layout.pieces.iter().map(|p| p.bounds))?;
                Some(Start {
                    structure: id.to_owned(),
                    key: (cx, cz),
                    bbox,
                    stub,
                    pieces: StartPieces::Monument {
                        layout: Box::new(layout),
                        loots: std::collections::BTreeMap::new(),
                    },
                })
            }
            Wave3aKind::Stronghold => {
                let stub = (cx * 16, 0, cz * 16);
                if !valid_biome_fast(g, &def.biomes, biome_mask, stub) {
                    return None;
                }
                let (start, _rng) =
                    mineshaft::generate_stronghold_pieces_with_rng(seed, cx, cz, g.min_y, g.sea);
                let bbox = union_bb(start.pieces.iter().map(|p| p.bounds))?;
                Some(Start {
                    structure: id.to_owned(),
                    key: (cx, cz),
                    bbox,
                    stub,
                    pieces: StartPieces::Stronghold(Box::new(start)),
                })
            }
            Wave3aKind::EndCity => {
                let reg = crate::block_registry::breg()?;
                let stub_probe =
                    (cx * 16 + 8, g.top_ws(cx * 16 + 8, cz * 16 + 8) - 1, cz * 16 + 8);
                if !valid_biome_fast(g, &def.biomes, biome_mask, stub_probe) {
                    return None;
                }
                let mut gen = crate::generation_math::Lcg::for_large_feature(seed, cx, cz);
                let mut fc = FC::new(g);
                let (stub, build) =
                    structure_template::find_end_city_pieces(&mut fc, cx, cz, &mut gen)?;
                let pieces = with_templates(|templates| build(&mut fc, templates, &mut gen, reg));
                let bbox = union_bb(pieces.iter().map(|p| p.bb))?;
                Some(Start {
                    structure: id.to_owned(),
                    key: (cx, cz),
                    bbox,
                    stub,
                    pieces: StartPieces::EndCity { pieces },
                })
            }
            Wave3aKind::Mansion => {
                let reg = crate::block_registry::breg()?;
                let mut gen = crate::generation_math::Lcg::for_large_feature(seed, cx, cz);
                let mut fc = FC::new(g);
                let (stub, build) =
                    structure_template::find_mansion_pieces(&mut fc, cx, cz, &mut gen)?;
                if !valid_biome_fast(fc.g, &def.biomes, biome_mask, stub) {
                    return None;
                }
                let pieces = with_templates(|templates| build(&mut fc, templates, &mut gen, reg));
                let bbox = union_bb(pieces.iter().map(|p| p.bb))?;
                Some(Start {
                    structure: id.to_owned(),
                    key: (cx, cz),
                    bbox,
                    stub,
                    pieces: StartPieces::Mansion { pieces },
                })
            }
        },
    }
}

/// The starts a set places in one chunk, in vanilla's admission order.
///
/// `createStructures` walks each set once: an existing valid start for a member
/// short-circuits the set (the port recomputes starts per chunk, so nothing is
/// stored yet), the placement has to admit the chunk, and a multi-entry set picks
/// by weight with the set's own seeded random, dropping a refused entry and
/// re-drawing.
fn set_starts(
    d: &Defs,
    g: &mut Gen,
    set_idx: usize,
    pm: Option<&PresetMasks>,
    cx: i32,
    cz: i32,
) -> Vec<Start> {
    let set = &d.sets[set_idx];
    let meta = &d.set_meta[set_idx];
    // The mineshaft set owns its weighted selection (and its variant-specific biome
    // check) inside `mineshaft::admit`; re-drawing here would consume the selector
    // twice and admit a different entry.
    if meta.all_mineshaft {
        let id = &set.entries[0].0;
        return match d.structures.get(id) {
            Some(def) => try_generate(d, g, id, def, pm, g.wseed(), cx, cz)
                .into_iter()
                .collect(),
            None => Vec::new(),
        };
    }
    if set.entries.len() == 1 {
        let id = &set.entries[0].0;
        let Some(def) = d.structures.get(id) else {
            return Vec::new();
        };
        return try_generate(d, g, id, def, pm, g.wseed(), cx, cz)
            .into_iter()
            .collect();
    }
    // Multi-entry set: weighted selection over the candidates that are loaded.
    let mut candidates: Vec<(usize, i32)> = meta.resolved_entries.clone();
    let mut total: i32 = meta.total_weight;
    let seed = g.wseed();
    let mut random = crate::generation_math::Lcg::for_large_feature(seed, cx, cz);
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
        let (sidx, weight) = candidates[index];
        let id = &d.struct_ids[sidx];
        if let Some(def) = d.structures.get(id) {
            if let Some(start) = try_generate(d, g, id, def, pm, seed, cx, cz) {
                return vec![start];
            }
        }
        candidates.remove(index);
        total -= weight;
    }
    Vec::new()
}

/// Generate starts for a subset of sets, tagged with their `d.sets` index.
///
/// Sets are independent (each builds its own seeded RNG), so evaluating the adapting
/// and non-adapting halves separately produces the same starts, in the same order, as
/// one pass over all 20 sets.
fn starts_for_sets(
    d: &Defs,
    g: &mut Gen,
    sets: &[usize],
    cx: i32,
    cz: i32,
) -> Vec<(usize, Start)> {
    // M3 P1a M1: one admission scan per (chunk, cache). The owner/worker split is the point:
    // the round's cache is shared, so a scan here is duplicated work whenever two threads
    // compute the same key. P1b made that impossible for the round's own cache (the slot is a
    // single-flight claim, so the second asker waits instead of scanning), and this counter is
    // how that is measured: the count must not grow with the worker count any more.
    crate::worldgen::m3_inc_split(
        crate::worldgen::M3_CHUNK_STARTS_OWNER,
        crate::worldgen::M3_CHUNK_STARTS_WORKER,
    );
    let pm = preset_masks(d, g);
    let seed = g.wseed();
    let mut out = Vec::new();
    for &i in sets {
        let meta = &d.set_meta[i];
        if meta.resolved_entries.is_empty() {
            continue;
        }
        if pm.is_some_and(|p| !p.set_has_any_valid[i]) {
            continue;
        }
        // The mineshaft set owns its own admission (`mineshaft::admit`), which is
        // byte-for-byte the same placement/frequency/weighted/biome chain.
        if !meta.all_mineshaft && !set_admits_live(d, g, i, seed, cx, cz, 0) {
            continue;
        }
        out.extend(
            set_starts(d, g, i, pm, cx, cz)
                .into_iter()
                .map(|s| (i, s)),
        );
    }
    out
}

/// Merge set-tagged starts by their original `d.sets` index. `sort_by_key` is stable,
/// so starts from one set keep vanilla's within-set order.
fn merge_starts(mut parts: Vec<(usize, Start)>) -> Vec<Start> {
    parts.sort_by_key(|(i, _)| *i);
    parts.into_iter().map(|(_, s)| s).collect()
}

fn adapting_starts_uncached(d: &Defs, g: &mut Gen, cx: i32, cz: i32) -> Vec<(usize, Start)> {
    starts_for_sets(d, g, &d.adapting_sets, cx, cz)
}

fn non_adapting_starts_uncached(d: &Defs, g: &mut Gen, cx: i32, cz: i32) -> Vec<(usize, Start)> {
    starts_for_sets(d, g, &d.non_adapting_sets, cx, cz)
}

/// Starts of every set that admit this chunk, in `d.sets` order.
#[allow(dead_code)]
fn chunk_starts(g: &mut Gen, cx: i32, cz: i32) -> Vec<Start> {
    let Some(d) = defs() else {
        return Vec::new();
    };
    let mut parts = adapting_starts_uncached(d, g, cx, cz);
    parts.extend(non_adapting_starts_uncached(d, g, cx, cz));
    merge_starts(parts)
}

thread_local! {
    /// Starts per (seed, dimension, chunk), shared by the decoration pass so the
    /// admission scan runs once. Pure in its key.
    static STARTS: std::cell::RefCell<StartsCache> =
        std::cell::RefCell::new(StartsCache { seed: 0, preset: 0, map: std::collections::HashMap::new(), ord: std::collections::VecDeque::new() });
    /// Adapting-only starts per (seed, dimension, chunk), used by beard resolution.
    /// Kept separate so a beard read never generates non-adapting structures.
    static ADAPTING_STARTS: std::cell::RefCell<AdaptingStartsCache> =
        std::cell::RefCell::new(AdaptingStartsCache { seed: 0, preset: 0, map: std::collections::HashMap::new(), ord: std::collections::VecDeque::new() });
}

type StartsByChunk = HashMap<(i32, i32), std::sync::Arc<Vec<Start>>>;
type AdaptingStartsByChunk = HashMap<(i32, i32), std::sync::Arc<Vec<(usize, Start)>>>;

struct StartsCache {
    seed: u64,
    preset: u8,
    map: StartsByChunk,
    ord: std::collections::VecDeque<(i32, i32)>,
}

struct AdaptingStartsCache {
    seed: u64,
    preset: u8,
    map: AdaptingStartsByChunk,
    ord: std::collections::VecDeque<(i32, i32)>,
}

type StartsFlight =
    crate::worldgen::FlightCache<(u64, u8, i32, i32), std::sync::Arc<Vec<Start>>>;
type AdaptingStartsFlight =
    crate::worldgen::FlightCache<(u64, u8, i32, i32), std::sync::Arc<Vec<(usize, Start)>>>;
type RingsFlight =
    crate::worldgen::FlightCache<(u64, u8, usize), std::sync::Arc<HashSet<(i32, i32)>>>;
type RingIFlight = crate::worldgen::FlightCache<(u64, u8, usize, i32), (i32, i32)>;

/// One parallel terrain round's structure-start cache, shared by that round's workers
/// **and nobody else**.
///
/// The workers of one round scan overlapping 17x17 windows, so a per-thread cache makes
/// them recompute the same starts: measured on the 64-chunk full dump, `chunk_starts` ran
/// 610 times at 8 per-thread caches against 281 serial and the run cost 19.96 s instead of
/// 7.42 s. A process-wide cache fixes the duplication but also changes what unrelated work
/// sees (in the parallel suite it moved
/// `tick_tests::wall_clock_boundaries_are_gated_on_elapsed_time_not_tick_count` over its
/// wall-clock assertion), so the sharing is scoped to the round: created before the workers
/// spawn, dropped after they join, and every other caller keeps the thread-local cache.
static STARTS_ROUNDS: std::sync::OnceLock<std::sync::Mutex<HashMap<u64, std::sync::Arc<RoundCache>>>> =
    std::sync::OnceLock::new();

/// What one terrain round shares between its workers: the structure starts and the stronghold
/// ring sets, both pure in their keys. Both matter — with the ring set per thread, a round
/// that spawns fresh worker threads recomputed the whole concentric-ring scan once per
/// thread per round, and that alone accounted for most of the 22.8 s the 64-chunk full dump
/// cost at 5 workers (against 8.19 s serial).
///
/// Each map is a single-flight cache rather than a plain map because the old shape - look up,
/// drop the lock, compute, re-lock, insert - is what M3 P1a measured as duplicated work: two
/// workers that miss the same key at once both compute it, and the later insert loses. Measured
/// on the 64-chunk `full` dump, `chunk_starts` per round went from 281 serial to 454-462 at five
/// workers and 376-571 at eight, varying run to run because the count was a race, not a
/// difference in what was generated.
struct RoundCache {
    starts: StartsFlight,
    /// Adapting-only starts: beard resolution asks for these without ever generating a
    /// non-adapting structure. `starts_cached` takes its adapting half from this cache.
    adapting_starts: AdaptingStartsFlight,
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    rings: RingsFlight,
    /// One lazily relocated chunk per `(seed, preset, set, ring index)`.
    ring_i: RingIFlight,
    /// Test probe: `chunk_starts` scans this round actually ran. The M1 counters aggregate the
    /// whole process (a worker flushes into them on its way out), so a test that wants *this
    /// round's* duplicate count reads it here instead of subtracting two aggregates that other
    /// tests are also writing to.
    #[cfg(test)]
    scans: std::sync::atomic::AtomicU64,
}

impl Default for RoundCache {
    fn default() -> Self {
        RoundCache {
            // The round's lifetime is the retention: it is created before the workers spawn and
            // dropped after they join, so a bound would only evict work the round itself is
            // still asking for.
            starts: crate::worldgen::FlightCache::new(None),
            adapting_starts: crate::worldgen::FlightCache::new(None),
            rings: crate::worldgen::FlightCache::new(None),
            ring_i: crate::worldgen::FlightCache::new(None),
            #[cfg(test)]
            scans: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

/// Flight domains for this module's four caches (see `worldgen::flight_token`).
const STARTS_FLIGHT_DOMAIN: u64 = 0x3_0000_0001;
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
const RINGS_FLIGHT_DOMAIN: u64 = 0x3_0000_0002;
const ADAPTING_STARTS_FLIGHT_DOMAIN: u64 = 0x3_0000_0003;
const RING_I_FLIGHT_DOMAIN: u64 = 0x3_0000_0004;

static STARTS_NEXT_ROUND: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

type StartsRoundMap = HashMap<u64, std::sync::Arc<RoundCache>>;

fn starts_rounds() -> &'static std::sync::Mutex<StartsRoundMap> {
    STARTS_ROUNDS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Test probe: how many `chunk_starts` scans this round has run (see [`RoundCache::scans`]).
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn round_start_scans(round: u64) -> Option<u64> {
    round_cache(round).map(|c| c.scans.load(std::sync::atomic::Ordering::Relaxed))
}

/// Test probe: `(built, shared)` for the round's adapting-starts flight cache — the number
/// of distinct keys computed against the number of asks served from a flight or a slot.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn round_adapting_flight_counts(round: u64) -> Option<(u64, u64)> {
    round_cache(round).map(|c| c.adapting_starts.flight_counts())
}

/// The round's cache, cloned out of the map so a caller never holds the map's mutex while it
/// waits for (or builds) a value.
fn round_cache(round: u64) -> Option<std::sync::Arc<RoundCache>> {
    starts_rounds()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&round)
        .cloned()
}

/// Open a round-scoped start cache and return its id (`Gen::starts_round` selects it).
///
/// The same round also opens `worldgen`'s shared terrain caches: a round is the unit of both
/// the structure-start sharing and the column/beardifier sharing, so the two are opened and
/// dropped together and neither can outlive the generators that key on them.
pub(crate) fn starts_round_begin() -> u64 {
    let id = STARTS_NEXT_ROUND.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    starts_rounds()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, std::sync::Arc::new(RoundCache::default()));
    crate::worldgen::terrain_share_begin(id);
    id
}

/// Drop a round-scoped start cache; called after the round's workers have joined.
pub(crate) fn starts_round_end(id: u64) {
    starts_rounds()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    crate::worldgen::terrain_share_end(id);
}

/// The adapting-only starts for one chunk, tagged by `d.sets` index and in set order.
/// This is the beardifier's sole start source: a beard read never generates a
/// mineshaft, monument, shipwreck, ocean ruin, buried treasure or ruined portal.
fn adapting_starts_cached(g: &mut Gen, cx: i32, cz: i32) -> std::sync::Arc<Vec<(usize, Start)>> {
    let seed = g.wseed();
    let preset = preset_tag(g.preset);
    let build = |g: &mut Gen| {
        let Some(d) = defs() else {
            return std::sync::Arc::new(Vec::new());
        };
        std::sync::Arc::new(adapting_starts_uncached(d, g, cx, cz))
    };
    if let Some(cache) = g.starts_round.and_then(round_cache) {
        let key = (seed, preset, cx, cz);
        let token = crate::worldgen::flight_token(ADAPTING_STARTS_FLIGHT_DOMAIN, &key);
        // Build outside any cache lock: start generation reads neighbour columns and can
        // resolve the nested beardifier rule, which is itself an adapting-starts ask.
        return cache.adapting_starts.get_or_build(key, token, || {
            #[cfg(test)]
            cache
                .scans
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            build(g)
        });
    }
    if let Some(v) = ADAPTING_STARTS.with(|slot| {
        let mut cache = slot.borrow_mut();
        if cache.seed != seed || cache.preset != preset {
            cache.seed = seed;
            cache.preset = preset;
            cache.map.clear();
            cache.ord.clear();
        }
        cache.map.get(&(cx, cz)).cloned()
    }) {
        return v;
    }
    let starts = build(g);
    ADAPTING_STARTS.with(|slot| {
        let mut cache = slot.borrow_mut();
        if cache.seed != seed || cache.preset != preset {
            cache.seed = seed;
            cache.preset = preset;
            cache.map.clear();
            cache.ord.clear();
        }
        if let Some(existing) = cache.map.get(&(cx, cz)) {
            return existing.clone();
        }
        cache.ord.push_back((cx, cz));
        if cache.ord.len() > 1024 {
            if let Some(old) = cache.ord.pop_front() {
                cache.map.remove(&old);
            }
        }
        cache.map.insert((cx, cz), starts.clone());
        starts
    })
}

/// The full start list: reuse the already-cached adapting half and evaluate only the
/// non-adapting sets, then merge by original set index so the result is byte-for-byte the
/// `chunk_starts` order. The adapting starts are cloned out of the shared `Arc`; a chunk's
/// start count is small, and this avoids generating any adapting structure twice.
fn starts_uncached_reusing(g: &mut Gen, cx: i32, cz: i32) -> Vec<Start> {
    let Some(d) = defs() else {
        return Vec::new();
    };
    let adapting = adapting_starts_cached(g, cx, cz);
    let mut parts: Vec<(usize, Start)> = adapting
        .iter()
        .map(|(i, s)| (*i, clone_start(s)))
        .collect();
    parts.extend(non_adapting_starts_uncached(d, g, cx, cz));
    merge_starts(parts)
}

fn starts_cached(g: &mut Gen, cx: i32, cz: i32) -> std::sync::Arc<Vec<Start>> {
    let seed = g.wseed();
    let preset = preset_tag(g.preset);
    if let Some(cache) = g.starts_round.and_then(round_cache) {
        let key = (seed, preset, cx, cz);
        let token = crate::worldgen::flight_token(STARTS_FLIGHT_DOMAIN, &key);
        // The leader computes *outside* any cache lock: start generation reads neighbour
        // columns and can resolve the nested beardifier rule, so holding a lock across it
        // would serialise the round's workers. Waiters block on the flight's own condvar, so
        // what they wait for is one publication, not the whole structure layer.
        return cache.starts.get_or_build(key, token, || {
            #[cfg(test)]
            cache
                .scans
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::sync::Arc::new(starts_uncached_reusing(g, cx, cz))
        });
    }
    if let Some(v) = STARTS.with(|slot| {
        let mut cache = slot.borrow_mut();
        if cache.seed != seed || cache.preset != preset {
            cache.seed = seed;
            cache.preset = preset;
            cache.map.clear();
            cache.ord.clear();
        }
        cache.map.get(&(cx, cz)).cloned()
    }) {
        return v;
    }
    let starts = std::sync::Arc::new(starts_uncached_reusing(g, cx, cz));
    STARTS.with(|slot| {
        let mut cache = slot.borrow_mut();
        if cache.seed != seed || cache.preset != preset {
            cache.seed = seed;
            cache.preset = preset;
            cache.map.clear();
            cache.ord.clear();
        }
        if let Some(existing) = cache.map.get(&(cx, cz)) {
            return existing.clone();
        }
        cache.ord.push_back((cx, cz));
        if cache.ord.len() > 1024 {
            if let Some(old) = cache.ord.pop_front() {
                cache.map.remove(&old);
            }
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
        cache.preset = 0;
        cache.map.clear();
        cache.ord.clear();
    });
    ADAPTING_STARTS.with(|slot| {
        let mut cache = slot.borrow_mut();
        cache.seed = 0;
        cache.preset = 0;
        cache.map.clear();
        cache.ord.clear();
    });
    RING_CACHE.with(|c| c.borrow_mut().clear());
    RING_CANDIDATES.with(|c| c.borrow_mut().clear());
    RING_I_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.map.clear();
        c.ord.clear();
    });
    starts_rounds()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

// ---------------------------------------------------------------- references

/// `ChunkGenerator.createReferences` + `StructureManager.startsForStructure`.
///
/// The reference set of a structure is the starts whose bounding box intersects
/// this chunk, among the 17x17 chunk window vanilla scans, kept in the insertion
/// order the native `LongOpenHashSet` iterates.
pub(crate) struct References {
    /// (step, structure id, starts in reference order)
    items: ReferenceItems,
    step_count: usize,
    #[cfg(test)]
    rng_after: HashMap<(usize, usize), (u64, u64)>,
}

type ReferenceItems = Vec<(usize, String, Vec<(u64, Start)>)>;
type FoundStarts = HashMap<String, (usize, Vec<u64>, HashMap<u64, Start>)>;
type OrderedStarts = Vec<(usize, usize, String, Vec<(u64, Start)>)>;

/// The structure and piece boxes of each fortress referenced by this chunk.
pub(crate) fn fortress_spawn_bounds(g: &mut Gen, cx: i32, cz: i32) -> Vec<(BB, Vec<BB>)> {
    let Some(d) = defs() else {
        return Vec::new();
    };
    let mut bounds = Vec::new();
    for (x, z) in window_admitted_chunks(d, g, &d.fortress_sets, cx, cz) {
        for start in starts_cached(g, x, z).iter() {
            if let StartPieces::Fortress { pieces, .. } = &start.pieces {
                if start.intersects_chunk(cx, cz) {
                    bounds.push((
                        start.bbox,
                        pieces.iter().map(|piece| piece.bounds).collect(),
                    ));
                }
            }
        }
    }
    bounds
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
        // Only chunks admitted by some set can hold a start. Enumerate the grid cells that
        // can land in the window instead of scanning all 17x17 chunks; sort+dedup recovers
        // the exact row-major encounter order of the old full-window scan.
        let active_sets = preset_masks(d, g)
            .map(|p| p.all_valid_sets.as_slice())
            .unwrap_or(&d.all_sets);
        let mut interesting = window_admitted_chunks(d, g, active_sets, cx, cz);
        interesting.sort_unstable();
        interesting.dedup();
        if interesting.is_empty() {
            return Self {
                items: Vec::new(),
                step_count: 0,
                #[cfg(test)]
                rng_after: HashMap::new(),
            };
        }
        // key -> starts, per structure id, in the encounter order of the window.
        let mut found: FoundStarts = HashMap::new();
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
        if found.is_empty() {
            return Self {
                items: Vec::new(),
                step_count: 0,
                #[cfg(test)]
                rng_after: HashMap::new(),
            };
        }
        let mut ordered: OrderedStarts = found
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
        #[cfg(not(test))]
        if !self.items.iter().any(|(item_step, _, _)| *item_step == step) {
            return;
        }
        let Some(d) = defs() else { return };
        let Some(step_ids) = d.by_step.get(step) else {
            return;
        };
        for (index, id) in step_ids.iter().enumerate() {
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
                    if crate::trace_env::env_flag!("MINESHAFT_TRACE")
                        && start.structure.starts_with("minecraft:mineshaft")
                    {
                        eprintln!(
                            "MSTART {cx} {cz} {} {step} {index} {sx} {sz} {}",
                            start.structure,
                            match &start.pieces {
                                StartPieces::Mineshaft(m) => m.pieces.len(),
                                _ => 0,
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
            StartPieces::BuriedTreasure(p) => StartPieces::BuriedTreasure(p.clone()),
            StartPieces::SwampHut(p) => StartPieces::SwampHut(p.clone()),
            StartPieces::DesertPyramid(p) => StartPieces::DesertPyramid(p.clone()),
            StartPieces::JungleTemple(p) => StartPieces::JungleTemple(p.clone()),
            StartPieces::Fortress { pieces, loots } => StartPieces::Fortress {
                pieces: pieces.clone(),
                loots: loots.clone(),
            },
            StartPieces::Monument { layout, loots } => StartPieces::Monument {
                layout: layout.clone(),
                loots: loots.clone(),
            },
            StartPieces::Stronghold(s) => StartPieces::Stronghold(s.clone()),
            StartPieces::EndCity { pieces } => StartPieces::EndCity {
                pieces: pieces.clone(),
            },
            StartPieces::Mansion { pieces } => StartPieces::Mansion {
                pieces: pieces.clone(),
            },
            StartPieces::Jigsaw { pieces } => StartPieces::Jigsaw {
                pieces: pieces.clone(),
            },
        },
    }
}

fn log_single_piece_loots(label: &str, loots: &[mineshaft::SinglePieceLoot]) {
    if !crate::trace_env::env_flag!("SPIPE_PAYLOAD") {
        return;
    }
    for loot in loots {
        if loot.kind != mineshaft::LootKind::Chest {
            eprintln!(
                "SPIPE_PAYLOAD {label} {:?} @ {:?} table={} seed={}",
                loot.kind, loot.pos, loot.table, loot.seed
            );
        }
    }
}

/// `StructureStart.placeInChunk`: the pieces whose boxes meet the chunk, then the
/// structure's `afterPlace`.
fn place_start(ctx: &mut FC, start: &mut Start, random: &mut crate::generation_math::Xor, cx: i32, cz: i32) {
    let (x0, z0) = (cx << 4, cz << 4);
    // Vanilla's clip is `new BoundingBox(chunkPos.getMinBlockX(), level.getMinBuildHeight(),
    // ..., level.getMaxBuildHeight(), ...)`, i.e. the **world** window. Keeping the
    // generator window here would drop every piece cell above y=127 even when the
    // storage layer accepts it (the End City tower: 273 cells at 128..143).
    let mut chunk_bb: BB = [
        x0,
        ctx.g.world_min_y,
        z0,
        x0 + 15,
        ctx.g.world_min_y + ctx.g.world_height - 1,
        z0 + 15,
    ];
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
        StartPieces::BuriedTreasure(p) => {
            let res = p.post_process(ctx, random, chunk_bb);
            log_single_piece_loots("buried_treasure", &res.loots);
        }
        StartPieces::SwampHut(p) => {
            let res = p.post_process(ctx, chunk_bb);
            log_single_piece_loots("swamp_hut", &res.loots);
        }
        StartPieces::DesertPyramid(p) => {
            let seed = ctx.g.wseed();
            let mut level_rng = crate::generation_math::Xor::seed(seed)
                .splitter()
                .of_str("minecraft:worldgen_region_random")
                .splitter()
                .pos(cx << 4, 0, cz << 4);
            let mut res = p.post_process(ctx, random, &mut level_rng, seed as i64, chunk_bb);
            mineshaft::DesertPyramidStructure::after_place(
                std::slice::from_ref(p),
                ctx,
                seed as i64,
                chunk_bb,
                &mut res,
            );
            log_single_piece_loots("desert_pyramid", &res.loots);
        }
        StartPieces::JungleTemple(p) => {
            let res = p.post_process(ctx, random, chunk_bb);
            log_single_piece_loots("jungle_temple", &res.loots);
        }
        StartPieces::Fortress { pieces, loots } => {
            let Some(reg) = crate::block_registry::breg() else {
                return;
            };
            mineshaft::place_fortress_chunk(ctx, reg, pieces, random, chunk_bb, loots);
        }
        StartPieces::Monument { layout, loots } => {
            let Some(reg) = crate::block_registry::breg() else {
                return;
            };
            let sea = ctx.g.sea;
            let mut elders = Vec::new();
            mineshaft::place_monument_chunk(
                ctx,
                reg,
                layout,
                random,
                chunk_bb,
                sea,
                loots,
                &mut elders,
            );
            for ((x, y, z), id) in &elders {
                ctx.push_entity(id, [*x as f64 + 0.5, *y as f64, *z as f64 + 0.5], 0.0);
            }
            if crate::trace_env::env_flag!("SPIPE_PAYLOAD") && !elders.is_empty() {
                eprintln!("SPIPE_PAYLOAD monument elders={elders:?}");
            }
        }
        StartPieces::Stronghold(s) => {
            let out = mineshaft::place_stronghold_chunk(ctx, s, chunk_bb, random);
            for payload in &out.payloads {
                if let mineshaft::StrongholdPayload::SilverfishSpawner { pos } = payload {
                    ctx.push_spawner(*pos, "minecraft:silverfish");
                    if crate::trace_env::env_flag!("SPIPE_PAYLOAD") {
                        eprintln!("SPIPE_PAYLOAD stronghold silverfish_spawner @ {pos:?}");
                    }
                }
            }
        }
        StartPieces::EndCity { pieces } => {
            let Some(reg) = crate::block_registry::breg() else {
                return;
            };
            let seed = ctx.g.wseed();
            let out = with_templates(|templates| {
                structure_template::place_end_city_chunk(
                    ctx,
                    templates,
                    pieces,
                    random,
                    &chunk_bb,
                    seed,
                    reg,
                )
            });
            if let Ok(out) = out {
                crate::structure_template::stage_payloads(ctx, out);
            }
        }
        StartPieces::Mansion { pieces } => {
            let Some(reg) = crate::block_registry::breg() else {
                return;
            };
            let seed = ctx.g.wseed();
            let out = with_templates(|templates| {
                structure_template::place_mansion_chunk(
                    ctx,
                    templates,
                    pieces,
                    random,
                    &chunk_bb,
                    seed,
                    reg,
                )
            });
            if let Ok(out) = out {
                crate::structure_template::stage_payloads(ctx, out);
            }
        }
        StartPieces::Jigsaw { pieces } => {
            let Some(reg) = crate::block_registry::breg() else {
                return;
            };
            let seed = ctx.g.wseed() as i64;
            let _ = place_jigsaw_chunk_live(ctx, pieces, seed, random, chunk_bb, reg);
        }
    }
}

fn packed(x: i32, z: i32) -> u64 {
    u64::from(x as u32) | (u64::from(z as u32) << 32)
}

// ---------------------------------------------------------------- beardifier

/// Collect one start's beardifier contribution, verbatim from the original
/// `beard_chunk_data` loop. Extracted so the adapting-only path and the tests' full-start
/// reference share one implementation of the piece filtering and summation order.
fn beard_push_start(data: &mut BeardChunkData, d: &Defs, cx: i32, cz: i32, start: &Start) {
    let Some(def) = d.structures.get(&start.structure) else {
        return;
    };
    if def.adjustment == BeardAdjustment::None {
        return;
    }
    let (x0, z0) = (cx << 4, cz << 4);
    // Vanilla filters per *piece* (`StructurePiece.isCloseToChunk(chunk, 12)`),
    // not per start: a start's union box may reach this chunk through a piece
    // that does not.
    let mut push = |b: &BB| {
        if !bb_close_to_chunk(b, cx, cz, 12) {
            return;
        }
        // The ids are only needed by `BEARD_TRACE`: a rigid box that the saved
        // reference chunk does not justify has to name the start that produced it.
        if crate::trace_env::env_str!("BEARD_TRACE")
            .is_some_and(|want| want == "all" || want == format!("{cx},{cz}"))
        {
            eprintln!(
                "BEARDPIECE {cx},{cz} {} start={},{} bb={:?}",
                start.structure, start.key.0, start.key.1, b
            );
        }
        data.rigids.push(BeardRigid {
            min: [b[0], b[1], b[2]],
            max: [b[3], b[4], b[5]],
            adjustment: def.adjustment,
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
        StartPieces::BuriedTreasure(p) => push(&p.bounds),
        StartPieces::SwampHut(p) => push(&p.bounds),
        StartPieces::DesertPyramid(p) => push(&p.bounds),
        StartPieces::JungleTemple(p) => push(&p.bounds),
        StartPieces::Fortress { pieces, .. } => {
            for piece in pieces {
                push(&piece.bounds);
            }
        }
        StartPieces::Monument { layout, .. } => {
            for piece in &layout.pieces {
                push(&piece.bounds);
            }
        }
        StartPieces::Stronghold(s) => {
            for piece in &s.pieces {
                push(&piece.bounds);
            }
        }
        StartPieces::EndCity { pieces } | StartPieces::Mansion { pieces } => {
            for piece in pieces {
                push(&piece.bb);
            }
        }
        StartPieces::Jigsaw { pieces } => {
            for piece in pieces {
                if !bb_close_to_chunk(&piece.bb, cx, cz, 12) {
                    continue;
                }
                if piece.is_rigid() {
                    data.rigids.push(BeardRigid {
                        min: [piece.bb[0], piece.bb[1], piece.bb[2]],
                        max: [piece.bb[3], piece.bb[4], piece.bb[5]],
                        adjustment: def.adjustment,
                        ground_level_delta: piece.ground_level_delta(),
                    });
                }
                for j in &piece.junctions {
                    if j.source_x > x0 - 12
                        && j.source_z > z0 - 12
                        && j.source_x < x0 + 27
                        && j.source_z < z0 + 27
                    {
                        data.junctions.push(*j);
                    }
                }
            }
        }
    }
}

/// `Beardifier.forStructuresInChunk`: every start whose structure adapts terrain,
/// with each piece close to this chunk turned into a rigid box, plus the jigsaw
/// junctions whose source lies within 12 blocks of the chunk.
fn beard_chunk_data(g: &mut Gen, cx: i32, cz: i32) -> BeardChunkData {
    let Some(d) = defs() else {
        return BeardChunkData::default();
    };
    let mut data = BeardChunkData::default();
    // Generating a start is expensive (a template structure reads neighbour columns for
    // its heightmaps), so first ask the cheap question: does any terrain-adapting set
    // admit a chunk in the ±8 window? Enumerate only the spread grid cells and the nearby
    // stronghold indices that can land there, then sort+dedup to keep the row-major order.
    let active_adapting = preset_masks(d, g)
        .map(|p| p.adapting_valid_sets.as_slice())
        .unwrap_or(&d.adapting_sets);
    let mut interesting = window_admitted_chunks(d, g, active_adapting, cx, cz);
    interesting.sort_unstable();
    interesting.dedup();
    if interesting.is_empty() {
        return data;
    }
    let interesting_len = interesting.len();
    for (x, z) in interesting {
        // Adapting-only: a beard read never generates a mineshaft, monument, shipwreck,
        // ocean ruin, buried treasure or ruined portal.
        for (_, start) in adapting_starts_cached(g, x, z).iter() {
            beard_push_start(&mut data, d, cx, cz, start);
        }
    }
    // `BEARD_TRACE=<cx>,<cz>`: the beardifier input for one chunk, after every start in the
    // +/-8 window has been asked. This is the answer `NoiseChunk.fillFromNoise` shapes the
    // terrain with, so a feature-time column read that disagrees with the same chunk's
    // un-beardified dump is attributed here instead of inferred from a four-block height
    // difference (measured 2026-11-16 on the Nether `(-25,22)` delta: the port's beard was
    // non-empty while the saved reference chunk carries no structure start at all).
    if crate::trace_env::env_str!("BEARD_TRACE").is_some_and(|want| {
        want == "all" || want == format!("{cx},{cz}")
    }) {
        eprintln!(
            "BEARD {cx},{cz} rigids={} junctions={} interesting={}",
            data.rigids.len(),
            data.junctions.len(),
            interesting_len
        );
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

#[cfg(all(test, megu_vanilla_data))]
mod pipeline_tests {
    use super::*;

    #[test]
    fn valid_biome_samples_noise_quarts_without_voronoi_zoom() {
        let mut g = Gen::load_preset(4242, crate::name_to_state, worldgen::NoisePreset::Nether)
            .expect("nether gen");
        // A biome boundary distinguishes the structure's quart lookup from the
        // LevelReader lookup. Include negative coordinates to exercise >> 2.
        let (pos, noise, blended) = (-392..=-360)
            .find_map(|x| {
                (340..=380).find_map(|z| {
                    let pos = (x, 88, z);
                    let (_, leaf) = g.biome_quart(x >> 2, 88 >> 2, z >> 2, None)?;
                    let noise = g.biome_name_of_leaf(leaf)?;
                    let blended = FC::new(&mut g).biome(x, 88, z)?;
                    (noise != blended).then_some((pos, noise, blended))
                })
            })
            .expect("nether boundary with differing quart and blended biomes");
        assert!(valid_biome(&mut g, &HashSet::from([noise]), pos));
        assert!(!valid_biome(&mut g, &HashSet::from([blended]), pos));
    }

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

    /// The lazy per-index path and the eager all-128 path must pick the exact same
    /// relocated chunk for every stronghold index on seed 0 (the benchmark seed). The
    /// eager path threads one `biome_quart` `last` hint across all 128; the lazy path
    /// resolves each index with `last = None`. On this seed they agree for all 128.
    #[test]
    fn lazy_ring_indices_match_eager_concentric_rings() {
        reset_cache();
        let d = defs().expect("structure definitions load");
        let &set = d.by_id.get("minecraft:strongholds").expect("strongholds set");
        let mut g = Gen::load(0, crate::name_to_state).expect("overworld gen");
        let seed = g.wseed();
        let (distance, spread, count, preferred_biomes) =
            ring_params(d, &g, set).expect("stronghold set is concentric rings");
        let preferred: HashSet<u16> = preferred_biomes
            .iter()
            .filter_map(|name| g.biome_leaf_of_name(name))
            .collect();
        let mut last: Option<u16> = None;
        let eager = mineshaft::concentric_rings(seed, distance, spread, count, |candidate, rng| {
            mineshaft::concentric_rings_target(
                candidate,
                rng,
                112,
                |qx, qy, qz| {
                    let res = g.biome_quart(qx, qy, qz, last);
                    if let Some((_, leaf)) = res {
                        last = Some(leaf);
                    }
                    res
                },
                |b| b.is_some_and(|(_, leaf)| preferred.contains(&leaf)),
            )
        })
        .expect("rings resolve");
        assert_eq!(eager.len(), 128);
        let candidates = ring_candidates_cached(d, &g, set);
        assert_eq!(candidates.len(), 128, "the candidate list is pure in (seed, set)");
        let lazy: Vec<(i32, i32)> = (0..count).map(|i| ring_chunk_i(d, &mut g, set, i)).collect();
        assert_eq!(
            lazy, eager,
            "lazy per-index resolution must match the eager path index for index"
        );
        // Membership via the admission helper agrees with the eager set too.
        let mut eager_set = eager;
        eager_set.sort_unstable();
        eager_set.dedup();
        for &(cx, cz) in &eager_set {
            assert!(
                ring_admit(d, &mut g, set, cx, cz),
                "eager ring chunk ({cx},{cz}) was not admitted lazily"
            );
        }
    }

    /// The grid-cell enumeration must be exactly the brute-force 17x17 scan, in the same
    /// row-major order, for every set and for the adapting subset, including negative
    /// coordinates and a real stronghold chunk (which exercises the Rings branch).
    #[test]
    fn window_admitted_chunks_matches_full_window_scan() {
        let d = defs().expect("structure definitions load");
        let mut g = Gen::load(0, crate::name_to_state).expect("overworld gen");
        let seed = g.wseed();
        let all_sets: Vec<usize> = (0..d.sets.len()).collect();
        let adapting = d.adapting_sets.clone();
        let sh_set = *d.by_id.get("minecraft:strongholds").expect("strongholds set");
        let sh: Vec<(i32, i32)> = ring_chunks(d, &mut g, sh_set).iter().copied().collect();
        let sh_pos = sh[0];
        for (cx, cz) in [
            (0, 0),
            (-37, -19),
            (64, -8),
            (-5, 80),
            sh_pos,
            (sh_pos.0 + 1, sh_pos.1 - 1),
        ] {
            for sets in [&all_sets, &adapting] {
                let mut got = window_admitted_chunks(d, &mut g, sets, cx, cz);
                got.sort_unstable();
                got.dedup();
                // The brute-force scan is already in row-major (x outer, z inner) order.
                let mut want: Vec<(i32, i32)> = Vec::new();
                for x in cx - 8..=cx + 8 {
                    for z in cz - 8..=cz + 8 {
                        if sets
                            .iter()
                            .any(|&s| set_admits_live(d, &mut g, s, seed, x, z, 0))
                        {
                            want.push((x, z));
                        }
                    }
                }
                assert_eq!(got, want, "window ({cx},{cz}), {} sets", sets.len());
            }
        }
    }

    /// Beard resolution must produce exactly the same rigids/junctions as the original
    /// full-start path, while never generating a start from a non-adapting set.
    #[test]
    fn beard_chunk_data_matches_full_start_reference_without_non_adapting() {
        reset_cache();
        let d = defs().expect("structure definitions load");
        let mut g = Gen::load(4242, crate::name_to_state).expect("overworld gen");
        let sh_set = *d.by_id.get("minecraft:strongholds").expect("strongholds set");
        let mut sh: Vec<(i32, i32)> = ring_chunks(d, &mut g, sh_set).iter().copied().collect();
        sh.sort_unstable();
        let (cx, cz) = sh
            .into_iter()
            .find(|&(cx, cz)| {
                starts_cached(&mut g, cx, cz)
                    .iter()
                    .any(|s| s.structure == "minecraft:stronghold")
            })
            .expect("a stronghold chunk that generates a start");

        let got = beard_chunk_data(&mut g, cx, cz);
        assert!(!got.is_empty(), "the stronghold chunk must contribute a beard");

        // Reference: the original algorithm — 289-chunk candidate scan over adapting
        // sets, full start list, then the shared per-start beard collection.
        let seed = g.wseed();
        let mut reference = BeardChunkData::default();
        for x in cx - 8..=cx + 8 {
            for z in cz - 8..=cz + 8 {
                if d.adapting_sets
                    .iter()
                    .any(|&s| candidate_admits_live(d, &mut g, s, seed, x, z))
                {
                    for start in starts_cached(&mut g, x, z).iter() {
                        beard_push_start(&mut reference, d, cx, cz, start);
                    }
                }
            }
        }
        assert_eq!(got, reference, "beard data changed with the adapting-only path");

        // No start from a non-adapting set may appear on the beard path.
        let mut non_adapting_structures: HashSet<&str> = HashSet::new();
        for &s in &d.non_adapting_sets {
            for (id, _) in &d.sets[s].entries {
                non_adapting_structures.insert(id.as_str());
            }
        }
        let adapting = adapting_starts_cached(&mut g, cx, cz);
        assert!(!adapting.is_empty());
        for (_, start) in adapting.iter() {
            assert!(
                !non_adapting_structures.contains(start.structure.as_str()),
                "beard path generated non-adapting structure {}",
                start.structure
            );
        }
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
                    // Gen-less `admits` (`set_admits`) intentionally returns `false` for
                    // `PlacementDef::Rings`; live ring relocation is handled by `set_admits_live`
                    // and tested in `live_stronghold_rings_admit_generate_and_beardify`.
                    assert_eq!(
                        admits("minecraft:strongholds", number(1) as u64, number(2) as i32, number(3) as i32),
                        Some(false),
                        "gen-less set_admits does not resolve concentric rings"
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
        // The installed generator and the process-wide beard resolver this fixture
        // registers cannot be undone; run in a child like the other generated-world
        // fixtures so later tests still see an uninitialised worldgen.
        if crate::test_support::isolated_world_test() {
            return;
        }
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
        assert!(!d.monument_surrounding.is_empty());
        let strongholds = &d.sets[*d.by_id.get("minecraft:strongholds").expect("strongholds set")];
        assert!(matches!(
            &strongholds.placement,
            PlacementDef::Rings { distance: 32, spread: 3, count: 128, preferred_biomes }
            if !preferred_biomes.is_empty()
        ));
    }

    #[test]
    fn live_stronghold_rings_admit_generate_and_beardify() {
        reset_cache();
        let d = defs().expect("structure definitions load");
        let &set = d.by_id.get("minecraft:strongholds").expect("strongholds set");
        let step = d
            .structures
            .get("minecraft:stronghold")
            .expect("stronghold def")
            .step;
        let mut g = Gen::load(4242, crate::name_to_state).expect("overworld gen");
        let mut rings: Vec<(i32, i32)> = ring_chunks(d, &mut g, set).iter().copied().collect();
        rings.sort();
        assert_eq!(rings.len(), 128);

        let found = rings.into_iter().find(|&(cx, cz)| {
            starts_cached(&mut g, cx, cz)
                .iter()
                .any(|s| s.structure == "minecraft:stronghold")
        });
        let (cx, cz) = found.expect("at least one ring chunk generates a stronghold");
        assert!(set_admits_live(d, &mut g, set, 4242, cx, cz, 0));

        let beard = beard_chunk_data(&mut g, cx, cz);
        assert!(
            beard
                .rigids
                .iter()
                .any(|r| r.adjustment == BeardAdjustment::Bury),
            "stronghold should contribute bury rigid boxes to beardifier"
        );

        let pop = pop_seed(g.wseed(), cx * 16, cz * 16);
        let mut refs = References::collect(&mut g, cx, cz);
        let mut ctx = FC::new(&mut g);
        refs.place_step(&mut ctx, step, pop, cx, cz);
        assert!(!ctx.wr.is_empty(), "stronghold placement should write blocks");
    }

    #[test]
    fn live_jigsaw_beard_chunk_data_collects_rigids_and_junctions() {
        reset_cache();
        let d = defs().expect("structure definitions load");
        let &village_set = d.by_id.get("minecraft:villages").expect("villages set");
        let mut g = Gen::load(4242, crate::name_to_state).expect("overworld gen");

        let mut hit: Option<(i32, i32, usize)> = None;
        'scan: for cx in -40..=40 {
            for cz in -40..=40 {
                if !set_admits_live(d, &mut g, village_set, 4242, cx, cz, 0) {
                    continue;
                }
                for start in starts_cached(&mut g, cx, cz).iter() {
                    if let StartPieces::Jigsaw { pieces } = &start.pieces {
                        if pieces.iter().any(|p| p.is_rigid())
                            && pieces.iter().any(|p| !p.junctions.is_empty())
                        {
                            let step = d.structures[&start.structure].step;
                            hit = Some((cx, cz, step));
                            break 'scan;
                        }
                    }
                }
            }
        }
        let (cx, cz, step) = hit.expect("a village jigsaw start in seed 4242");
        let beard = beard_chunk_data(&mut g, cx, cz);
        assert!(!beard.rigids.is_empty(), "jigsaw start should emit rigid boxes");
        assert!(
            !beard.junctions.is_empty(),
            "jigsaw start should emit junctions within chunk window"
        );
        let (x0, z0) = (cx << 4, cz << 4);
        for j in &beard.junctions {
            assert!(
                j.source_x > x0 - 12
                    && j.source_z > z0 - 12
                    && j.source_x < x0 + 27
                    && j.source_z < z0 + 27
            );
        }

        let pop = pop_seed(g.wseed(), cx * 16, cz * 16);
        let mut refs = References::collect(&mut g, cx, cz);
        let mut ctx = FC::new(&mut g);
        refs.place_step(&mut ctx, step, pop, cx, cz);
        assert!(!ctx.wr.is_empty(), "village placement should write blocks");
    }

    #[test]
    fn live_pipeline_dispatches_single_piece_wave3a_and_seed_4242_chunk() {
        reset_cache();
        let d = defs().expect("structure definitions load");

        // 1. Seed 4242 Overworld chunk (3, -2) full populate() + SinglePiece start placement.
        let mut ow = Gen::load(4242, crate::name_to_state).expect("overworld gen");
        populate(&mut ow, 3, -2);

        let single_sets = [
            "minecraft:buried_treasures",
            "minecraft:desert_pyramids",
            "minecraft:swamp_huts",
            "minecraft:jungle_temples",
        ];
        let mut single_hit: Option<(i32, i32, usize)> = None;
        'ow_scan: for &set_name in &single_sets {
            let &set = d.by_id.get(set_name).expect("single piece set");
            for cx in -64..=64 {
                for cz in -64..=64 {
                    if !set_admits_live(d, &mut ow, set, 4242, cx, cz, 0) {
                        continue;
                    }
                    for start in starts_cached(&mut ow, cx, cz).iter() {
                        if matches!(
                            start.pieces,
                            StartPieces::BuriedTreasure(_)
                                | StartPieces::SwampHut(_)
                                | StartPieces::DesertPyramid(_)
                                | StartPieces::JungleTemple(_)
                        ) {
                            single_hit = Some((cx, cz, d.structures[&start.structure].step));
                            break 'ow_scan;
                        }
                    }
                }
            }
        }
        let (sx, sz, s_step) = single_hit.expect("at least one SinglePiece start in seed 4242");
        let mut s_refs = References::collect(&mut ow, sx, sz);
        let s_pop = pop_seed(ow.wseed(), sx * 16, sz * 16);
        let mut s_ctx = FC::new(&mut ow);
        s_refs.place_step(&mut s_ctx, s_step, s_pop, sx, sz);
        assert!(!s_ctx.wr.is_empty(), "SinglePiece placement should write blocks");

        // 2. Nether Fortress via Wave3A.
        let mut nether =
            Gen::load_preset(4242, crate::name_to_state, worldgen::NoisePreset::Nether)
                .expect("nether gen");
        let &nether_set = d
            .by_id
            .get("minecraft:nether_complexes")
            .expect("nether complexes");
        let mut fortress_hit: Option<(i32, i32, usize)> = None;
        'nether_scan: for cx in -40..=40 {
            for cz in -40..=40 {
                if !set_admits_live(d, &mut nether, nether_set, 4242, cx, cz, 0) {
                    continue;
                }
                for start in starts_cached(&mut nether, cx, cz).iter() {
                    if matches!(start.pieces, StartPieces::Fortress { .. }) {
                        fortress_hit = Some((cx, cz, d.structures[&start.structure].step));
                        break 'nether_scan;
                    }
                }
            }
        }
        let (fx, fz, f_step) = fortress_hit.expect("a Nether fortress start in seed 4242");
        let mut f_refs = References::collect(&mut nether, fx, fz);
        let f_pop = pop_seed(nether.wseed(), fx * 16, fz * 16);
        let mut f_ctx = FC::new(&mut nether);
        f_refs.place_step(&mut f_ctx, f_step, f_pop, fx, fz);
        assert!(!f_ctx.wr.is_empty(), "Fortress placement should write blocks");

        // 3. End City via Wave3A (outer End islands start beyond 1000 blocks).
        let mut end = Gen::load_preset(4242, crate::name_to_state, worldgen::NoisePreset::End)
            .expect("end gen");
        let &end_set = d.by_id.get("minecraft:end_cities").expect("end cities");
        let mut end_hit: Option<(i32, i32, usize)> = None;
        'end_scan: for cx in 64..=128 {
            for cz in -40..=40 {
                if !set_admits_live(d, &mut end, end_set, 4242, cx, cz, 0) {
                    continue;
                }
                for start in starts_cached(&mut end, cx, cz).iter() {
                    if matches!(start.pieces, StartPieces::EndCity { .. }) {
                        end_hit = Some((cx, cz, d.structures[&start.structure].step));
                        break 'end_scan;
                    }
                }
            }
        }
        let (ex, ez, e_step) = end_hit.expect("an End City start in seed 4242");
        let mut e_refs = References::collect(&mut end, ex, ez);
        let e_pop = pop_seed(end.wseed(), ex * 16, ez * 16);
        let mut e_ctx = FC::new(&mut end);
        e_refs.place_step(&mut e_ctx, e_step, e_pop, ex, ez);
        assert!(!e_ctx.wr.is_empty(), "EndCity placement should write blocks");
    }

    /// The block names one chunk's feature layer recorded, with counts.
    fn feature_blocks(
        g: &mut Gen,
        cx: i32,
        cz: i32,
    ) -> std::collections::BTreeMap<String, usize> {
        let reg = crate::block_registry::breg().expect("block registry");
        let mut out: std::collections::BTreeMap<String, usize> = Default::default();
        for x in cx * 16..cx * 16 + 16 {
            for z in cz * 16..cz * 16 + 16 {
                // the world window: a feature may write above the generator top
                for y in g.world_min_y..g.world_min_y + g.world_height {
                    if let Some(st) = g.feature_override(x, y, z) {
                        *out.entry(reg.name_of(st).to_string()).or_default() += 1;
                    }
                }
            }
        }
        out
    }

    /// One piece box per piece of a start (the beardifier's unit).
    fn piece_boxes(start: &Start) -> Vec<BB> {
        match &start.pieces {
            StartPieces::Mineshaft(_) => vec![start.bbox],
            StartPieces::Template { pieces } => pieces.iter().map(|p| p.bb).collect(),
            StartPieces::BuriedTreasure(p) => vec![p.bounds],
            StartPieces::SwampHut(p) => vec![p.bounds],
            StartPieces::DesertPyramid(p) => vec![p.bounds],
            StartPieces::JungleTemple(p) => vec![p.bounds],
            StartPieces::Fortress { pieces, .. } => pieces.iter().map(|p| p.bounds).collect(),
            StartPieces::Monument { layout, .. } => {
                layout.pieces.iter().map(|p| p.bounds).collect()
            }
            StartPieces::Stronghold(s) => s.pieces.iter().map(|p| p.bounds).collect(),
            StartPieces::EndCity { pieces } | StartPieces::Mansion { pieces } => {
                pieces.iter().map(|p| p.bb).collect()
            }
            StartPieces::Jigsaw { pieces } => pieces.iter().map(|p| p.bb).collect(),
        }
    }

    /// The chunk holding the most piece volume: the one chunk where `populate` is
    /// guaranteed to write the start's own blocks (a structure's bbox can span a dozen
    /// chunks while its pieces sit in two of them).
    fn densest_chunk(start: &Start) -> (i32, i32) {
        let mut by_chunk: HashMap<(i32, i32), i64> = HashMap::new();
        for bb in piece_boxes(start) {
            let volume = i64::from(bb[3] - bb[0] + 1)
                * i64::from(bb[4] - bb[1] + 1)
                * i64::from(bb[5] - bb[2] + 1);
            for cx in (bb[0] >> 4)..=(bb[3] >> 4) {
                for cz in (bb[2] >> 4)..=(bb[5] >> 4) {
                    *by_chunk.entry((cx, cz)).or_default() += volume;
                }
            }
        }
        by_chunk
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
            .map(|(chunk, _)| chunk)
            .unwrap_or((start.bbox[0] >> 4, start.bbox[2] >> 4))
    }

    /// `NetherFossilStructure.findGenerationPoint` scans `ChunkGenerator.getBaseColumn`:
    /// noise only, with no surface rule, no carvers, no features and no beardifier. Two
    /// defects lived in reading the live column instead. The scan was circular - the
    /// fossil's own `beard_thin` moves the local surface by ~20 blocks in the Nether's
    /// soft upper density - and it folded the stage order into the placement, because
    /// `feature::populate` pre-carves the 3x3 before `References::collect`.
    ///
    /// Native oracle: seed 4242, `world/DIM-1`, chunk (-22,20) holds
    /// `minecraft:nether_fossil` with one child at BB [-356, 55, 319, -350, 61, 324];
    /// chunk (-24,20) holds no fossil at all.
    #[test]
    fn nether_fossil_scans_the_noise_column_not_the_live_world() {
        reset_cache();
        let mut nether =
            Gen::load_preset(4242, crate::name_to_state, worldgen::NoisePreset::Nether)
                .expect("nether gen");
        let reg = crate::block_registry::breg().expect("block registry");
        let starts = starts_cached(&mut nether, -22, 20);
        let fossil = starts
            .iter()
            .find(|s| s.structure == "minecraft:nether_fossil")
            .expect("nether_fossil start in seed 4242 chunk (-22,20)");
        assert_eq!(fossil.bbox, [-356, 55, 319, -350, 61, 324], "native capture BB");

        // The scan column is the piece origin, not the box corner: `pos` is the box's
        // maximum x/z. The two cells the scan compares there are the noise surface
        // (`netherrack` at 55) and the air pocket above it (56).
        let (x, z) = (-350, 324);
        let mut base = |y: i32| {
            nether
                .base_column_state(x, y, z)
                .map(|st| reg.name_of(st).to_string())
        };
        assert_eq!(base(55).as_deref(), Some("minecraft:netherrack"));
        assert_eq!(base(56).as_deref(), Some("minecraft:air"));
        // The base column is the *noise* answer: one block lower the surface rule paints
        // `soul_soil` in the final chunk while the noise still says `netherrack`.
        assert_eq!(base(57).as_deref(), Some("minecraft:netherrack"));
        assert_eq!(
            reg.name_of(nether.terrain_state(x, 57, z)),
            "minecraft:soul_soil",
            "the base column must not carry the surface rule"
        );

        // Vanilla answers no fossil here; the pre-fix live scan invented one 23 blocks up.
        assert!(
            starts_cached(&mut nether, -24, 20)
                .iter()
                .all(|s| s.structure != "minecraft:nether_fossil"),
            "chunk (-24,20) has no nether_fossil in the native capture"
        );

        // Vanilla settles the start at `structure_starts`, so the carver and feature
        // stages cannot move it. `feature::populate` pre-carves the 3x3 before it
        // collects references, so a scan over live blocks answers differently after a
        // populate (measured pre-fix: (-24,20) had a fossil before any carver ran and
        // none after), while the noise column answers the same either way.
        populate(&mut nether, -22, 20);
        reset_cache();
        let after = starts_cached(&mut nether, -22, 20);
        assert_eq!(
            after
                .iter()
                .find(|s| s.structure == "minecraft:nether_fossil")
                .map(|s| s.bbox),
            Some([-356, 55, 319, -350, 61, 324]),
            "the fossil start must not depend on the carver/feature layer"
        );

        // The beardifier follows the start: the corrected box, at the noise surface.
        let beard = beard_chunk_data(&mut nether, -23, 20);
        assert_eq!(beard.rigids.len(), 1, "{:?}", beard.rigids);
        assert_eq!(beard.rigids[0].min, [-356, 55, 319]);
        assert_eq!(beard.rigids[0].max, [-350, 61, 324]);
        assert_eq!(beard.rigids[0].adjustment, BeardAdjustment::BeardThin);
    }

    /// `Structure.findValidGenerationPoint` gates on the position the structure's own
    /// `findGenerationPoint` returned, never on the chunk centre. Seed 4242's chunk
    /// (-24,22) is the measured case: the fossil scan walks the noise column at
    /// (-382,88,367) in `basalt_deltas` while the chunk centre (-376,64,360) is
    /// `soul_sand_valley`. `#has_structure/nether_fossil` holds only soul_sand_valley, so
    /// vanilla refuses the start (native `world/DIM-1` chunk (-24,22) has an empty
    /// `structures.starts`) where a centre-gated port placed `bb=[-384,88,366,-382,91,367]`
    /// and its `beard_thin` reshaped chunk (-25,22)'s +x columns.
    #[test]
    fn nether_fossil_biome_gate_reads_the_start_not_the_chunk_centre() {
        reset_cache();
        let mut nether =
            Gen::load_preset(4242, crate::name_to_state, worldgen::NoisePreset::Nether)
                .expect("nether gen");

        // The two starts the native 9x9 footprint holds, at their saved `Children[].BB`.
        for (cx, cz, bb) in [
            (-22, 20, [-356, 55, 319, -350, 61, 324]),
            (-22, 22, [-337, 76, 355, -333, 80, 357]),
        ] {
            let starts = starts_cached(&mut nether, cx, cz);
            let fossil = starts
                .iter()
                .find(|s| s.structure == "minecraft:nether_fossil")
                .unwrap_or_else(|| panic!("native nether_fossil start in chunk ({cx},{cz})"));
            assert_eq!(fossil.bbox, bb, "native child box for chunk ({cx},{cz})");
        }

        // (-24,22): the placement admits the chunk, the scan succeeds, and the biome the
        // gate has to read is the one at the start it produced.
        assert!(
            starts_cached(&mut nether, -24, 22)
                .iter()
                .all(|s| s.structure != "minecraft:nether_fossil"),
            "native chunk (-24,22) holds no nether_fossil start"
        );
        assert_eq!(
            nether.biome_name_at(-382, 88, 367).as_deref(),
            Some("minecraft:basalt_deltas"),
            "the scan's own position is outside #has_structure/nether_fossil"
        );
        assert_eq!(
            FC::new(&mut nether).biome(-382, 88, 367).as_deref(),
            Some("minecraft:basalt_deltas"),
            "the gate's own read of the start agrees with the quart answer"
        );
        assert_eq!(
            FC::new(&mut nether).biome(-376, 64, 360).as_deref(),
            Some("minecraft:soul_sand_valley"),
            "the centre's biome is the wrong question, which is why the start appeared"
        );

        // Its beard was the visible damage: chunk (-25,22) is the chunk it reshaped.
        let beard = beard_chunk_data(&mut nether, -25, 22);
        assert!(beard.rigids.is_empty(), "{:?}", beard.rigids);
    }

    #[test]
    fn dimension_populate_runs_that_dimensions_decorations_only() {
        reset_cache();
        let d = defs().expect("structure definitions load");

        // Offline full-decoration parity for the two non-Overworld dimensions. Until
        // now `MEGU_WG_PRESET=nether|end` refused to run the feature layer at all
        // (`main.rs`) and `tools/port_triage.py` refused `--mode full`, so no test could
        // reach this path. Seed 4242 was scanned for one start of each structure; the
        // chunk is the piece-densest chunk of that start.
        let mut nether =
            Gen::load_preset(4242, crate::name_to_state, worldgen::NoisePreset::Nether)
                .expect("nether gen");
        // Every one of the Nether's own structures: the start exists, and `populate`
        // writes its blocks into the chunk that holds them.
        for (id, at, marker) in [
            ("minecraft:fortress", (-25, 6), "minecraft:nether_bricks"),
            (
                "minecraft:bastion_remnant",
                (-37, -15),
                "minecraft:polished_blackstone_bricks",
            ),
            (
                "minecraft:ruined_portal_nether",
                (-39, -26),
                "minecraft:obsidian",
            ),
        ] {
            let starts = starts_cached(&mut nether, at.0, at.1);
            let start = starts
                .iter()
                .find(|s| s.structure == id)
                .unwrap_or_else(|| panic!("{id} start in seed 4242 chunk {at:?}"));
            let (cx, cz) = densest_chunk(start);
            populate(&mut nether, cx, cz);
            let blocks = feature_blocks(&mut nether, cx, cz);
            assert!(
                blocks.get(marker).copied().unwrap_or(0) > 0,
                "{id}: populate({cx},{cz}) must write {marker}, got {:?}",
                blocks.keys().take(12).collect::<Vec<_>>()
            );
        }

        // `nether_fossil` is the one Nether structure whose `findGenerationPoint` scans
        // block states. It scans `ChunkGenerator.getBaseColumn`, i.e. noise only, so the
        // start is settled at `structure_starts` and never sees the carvers, the
        // features or its own `beard_thin` mound. The native oracle (seed 4242, chunk
        // (-22,20), `world/DIM-1`) puts the fossil at exactly this box.
        let fossil_chunk = (-22, 20);
        let fossil_starts = starts_cached(&mut nether, fossil_chunk.0, fossil_chunk.1);
        let fossil = fossil_starts
            .iter()
            .find(|s| s.structure == "minecraft:nether_fossil")
            .expect("nether_fossil start in seed 4242 chunk (-22,20)");
        let step = d.structures[&fossil.structure].step;
        let (pop_at, fossil_bbox) = (fossil.key, fossil.bbox);
        let pop = pop_seed(nether.wseed(), pop_at.0 * 16, pop_at.1 * 16);
        let mut refs = References::collect(&mut nether, pop_at.0, pop_at.1);
        let mut ctx = FC::new(&mut nether);
        refs.place_step(&mut ctx, step, pop, pop_at.0, pop_at.1);
        let reg = crate::block_registry::breg().expect("block registry");
        let bones = ctx
            .wr
            .iter()
            .filter(|((x, _, z), st)| {
                reg.name_of(**st) == "minecraft:bone_block"
                    && *x >= fossil_bbox[0]
                    && *x <= fossil_bbox[3]
                    && *z >= fossil_bbox[2]
                    && *z <= fossil_bbox[5]
            })
            .count();
        assert!(bones > 0, "nether_fossil placement must write bone blocks");

        // The same populate must not reach across dimensions. Each set below is placed
        // in the Nether chunk space (the placement admits these chunks) and every entry
        // is an Overworld/End structure: the tag-resolved biome set is the only filter.
        for (set_id, cx, cz) in [
            ("minecraft:villages", 22, 18),
            ("minecraft:ocean_monuments", 16, 6),
            ("minecraft:end_cities", 1, 25),
        ] {
            let &set = d.by_id.get(set_id).expect("structure set");
            assert!(
                set_admits_live(d, &mut nether, set, 4242, cx, cz, 0),
                "{set_id} placement must admit nether chunk ({cx},{cz})"
            );
            for start in starts_cached(&mut nether, cx, cz).iter() {
                let def = d.structures.get(&start.structure).expect("structure def");
                // A member whose biome holder failed to resolve answers `true` for every
                // biome (`valid_biome` with an empty set), which is a cross-dimension
                // placement waiting to happen: require the resolved set to be non-empty.
                assert!(
                    !def.biomes.is_empty(),
                    "{} biome set did not resolve; it can fire in any dimension",
                    start.structure
                );
                assert!(
                    start.structure != "minecraft:end_city"
                        && !start.structure.starts_with("minecraft:village")
                        && start.structure != "minecraft:ocean_monument",
                    "{set_id} member {} fired in the Nether at ({cx},{cz})",
                    start.structure
                );
            }
        }

        // The End: an End City start on the outer islands, populated through the feature
        // layer, plus the End's own small feature (chorus) that the chunk carries.
        let mut end = Gen::load_preset(4242, crate::name_to_state, worldgen::NoisePreset::End)
            .expect("end gen");
        let end_starts = starts_cached(&mut end, 65, 45);
        let city = end_starts
            .iter()
            .find(|s| s.structure == "minecraft:end_city")
            .expect("end_city start in seed 4242 chunk (65,45)");
        let (ecx, ecz) = densest_chunk(city);
        populate(&mut end, ecx, ecz);
        let blocks = feature_blocks(&mut end, ecx, ecz);
        assert!(
            !blocks.is_empty(),
            "end_city populate({ecx},{ecz}) must write blocks"
        );
        assert!(
            blocks.keys().any(|k| k.starts_with("minecraft:purpur")),
            "end_city populate must write purpur, got {:?}",
            blocks.keys().take(12).collect::<Vec<_>>()
        );
        // No Nether/Overworld structure may appear in the End either.
        for (cx, cz) in [(ecx, ecz), (65, 45), (64, 44)] {
            for start in starts_cached(&mut end, cx, cz).iter() {
                assert!(
                    !matches!(
                        start.structure.as_str(),
                        "minecraft:fortress"
                            | "minecraft:bastion_remnant"
                            | "minecraft:nether_fossil"
                            | "minecraft:ruined_portal_nether"
                            | "minecraft:ocean_monument"
                            | "minecraft:stronghold"
                    ),
                    "{} fired in the End at ({cx},{cz})",
                    start.structure
                );
            }
        }
    }

    /// The world window, not the generator window, is what a chunk can *store*.
    ///
    /// 1.21.1 declares them separately: `data/minecraft/dimension_type/the_end.json` is
    /// `min_y 0 / height 256`, `worldgen/noise_settings/end.json` is `0/128`. Terrain,
    /// density, aquifers, carvers and surface rules live in the 128; chunk sections,
    /// heightmaps, the dump length and structure bounds live in the 256. `end_city`
    /// builds *up* with no ceiling of its own, so on the pinned capture
    /// (`captures/matrix-v1/nat-4242-end-city`, chunk (65,45)) vanilla stores 273
    /// non-air cells at y 128..143: `purpur_block` 168, `end_stone_bricks` 54,
    /// `purpur_pillar[axis=y]` 12, `magenta_stained_glass` 12, `purpur_stairs` 20,
    /// `end_rod` 4. Highest block y = 139, stored `WORLD_SURFACE` = 140.
    ///
    /// A port that uses the generator height as the storage bound drops all of them and
    /// still reports `0` differences, because they fall outside the window it compares;
    /// `chunk_triage` only sees the loss as an out-of-window exclusion count. This test
    /// pins the cells, not the window arithmetic.
    #[test]
    fn end_city_stores_cells_above_the_generator_top() {
        reset_cache();
        let mut end = Gen::load_preset(4242, crate::name_to_state, worldgen::NoisePreset::End)
            .expect("end gen");
        let starts = starts_cached(&mut end, 65, 45);
        assert!(
            starts.iter().any(|s| s.structure == "minecraft:end_city"),
            "seed 4242 chunk (65,45) must hold an end_city start"
        );
        populate(&mut end, 65, 45);
        let reg = crate::block_registry::breg().expect("block registry");
        // 128..143 is world section 8 and past the generator top, so nothing the terrain
        // layer produces can land here: every cell is a structure write the storage bound
        // has to accept. 144..255 must stay air (vanilla has nothing above 143 here).
        //
        // The read is the dump's own reader (`Gen::block_state_at`, which `terrain_section`
        // wraps), taken from this test's local generator: `worldgen::terrain_section` reads
        // the global `GEN`, which this test does not install.
        let mut census: std::collections::BTreeMap<String, usize> = Default::default();
        let mut above = 0usize;
        let mut higher = 0usize;
        for sy in (128..256).step_by(16) {
            let mut cells = 0usize;
            for ly in 0..16 {
                for lz in 0..16 {
                    for lx in 0..16 {
                        let st = end.block_state_at(
                            65 * 16 + lx,
                            sy + ly,
                            45 * 16 + lz,
                            crate::feature::populate,
                        );
                        if st == 0 || reg.is_air(st) {
                            continue;
                        }
                        cells += 1;
                        *census.entry(reg.canonical_state(st)).or_default() += 1;
                    }
                }
            }
            if sy == 128 {
                above = cells;
            } else {
                higher += cells;
            }
        }
        assert_eq!(
            higher, 0,
            "vanilla stores nothing above y=143 in this chunk; got {higher} cells in 144..255"
        );
        assert_eq!(
            above, 273,
            "the native capture stores 273 non-air cells at y 128..143 (chunk 65,45); \
             got {above} (census: {census:?}). Zero means the storage bound is still the \
             generator window, so every end_city cell above y=127 is dropped"
        );
        // The tower's own materials, so a count that happens to match cannot pass with a
        // different structure in the same section.
        let count = |name: &str| {
            census
                .iter()
                .filter(|(k, _)| k.starts_with(&format!("minecraft:{name}")))
                .map(|(_, v)| *v)
                .sum::<usize>()
        };
        assert_eq!(count("purpur_block"), 168, "census: {census:?}");
        assert_eq!(count("end_stone_bricks"), 54, "census: {census:?}");
        // The chunk's own heightmap has to follow the same window: the native capture stores
        // `Heightmaps.WORLD_SURFACE` top = 140 for this chunk (highest block y = 139), i.e.
        // above the generator top. A heightmap scan that stops at y=127 cannot answer that.
        let mut fc = FC::new(&mut end);
        let mut top = i32::MIN;
        for lz in 0..16 {
            for lx in 0..16 {
                top = top.max(fc.hm_ws(65 * 16 + lx, 45 * 16 + lz));
            }
        }
        assert_eq!(
            top, 140,
            "the native capture stores Heightmaps.WORLD_SURFACE top = 140 for this chunk; a \
             0..127 scan tops out at 128, so the two sides would disagree on every column \
             under the tower"
        );
    }

    #[test]
    fn structure_spawner_and_entity_payloads_reach_fc() {
        use crate::generation_payload::GenerationPayload;
        reset_cache();
        let d = defs().expect("structure definitions load");

        // 1. Fortress -> SpawnerEntity("minecraft:blaze")
        let mut nether =
            Gen::load_preset(4242, crate::name_to_state, worldgen::NoisePreset::Nether)
                .expect("nether gen");
        let &nether_set = d
            .by_id
            .get("minecraft:nether_complexes")
            .expect("nether complexes");
        let mut throne_chunks: Option<(usize, Vec<(i32, i32)>)> = None;
        'fort_scan: for cx in -40..=40 {
            for cz in -40..=40 {
                if !set_admits_live(d, &mut nether, nether_set, 4242, cx, cz, 0) {
                    continue;
                }
                for start in starts_cached(&mut nether, cx, cz).iter() {
                    if let StartPieces::Fortress { pieces, .. } = &start.pieces {
                        let mut chunks = Vec::new();
                        for p in pieces {
                            if p.kind == crate::mineshaft_piece::FortressKind::MonsterThrone {
                                for tx in (p.bounds[0] >> 4)..=(p.bounds[3] >> 4) {
                                    for tz in (p.bounds[2] >> 4)..=(p.bounds[5] >> 4) {
                                        chunks.push((tx, tz));
                                    }
                                }
                            }
                        }
                        if !chunks.is_empty() {
                            throne_chunks = Some((d.structures[&start.structure].step, chunks));
                            break 'fort_scan;
                        }
                    }
                }
            }
        }
        let (f_step, f_chunks) = throne_chunks.expect("a Fortress with MonsterThrone in seed 4242");
        let mut blaze_spawners = Vec::new();
        for (cx, cz) in f_chunks {
            let mut refs = References::collect(&mut nether, cx, cz);
            let pop = pop_seed(nether.wseed(), cx * 16, cz * 16);
            let mut ctx = FC::new(&mut nether);
            refs.place_step(&mut ctx, f_step, pop, cx, cz);
            for (pos, payload) in ctx.payloads {
                if matches!(&payload, GenerationPayload::SpawnerEntity(id) if id == "minecraft:blaze") {
                    blaze_spawners.push(pos);
                }
            }
        }
        assert!(
            !blaze_spawners.is_empty(),
            "Fortress MonsterThrone must push SpawnerEntity(\"minecraft:blaze\")"
        );

        // 2. Stronghold -> SpawnerEntity("minecraft:silverfish")
        let mut ow = Gen::load(4242, crate::name_to_state).expect("overworld gen");
        let &sh_set = d.by_id.get("minecraft:strongholds").expect("strongholds set");
        let sh_step = d.structures["minecraft:stronghold"].step;
        let mut rings: Vec<(i32, i32)> = ring_chunks(d, &mut ow, sh_set).iter().copied().collect();
        rings.sort();
        let mut portal_chunks = None;
        for (cx, cz) in rings {
            for start in starts_cached(&mut ow, cx, cz).iter() {
                if let StartPieces::Stronghold(s) = &start.pieces {
                    if let Some(idx) = s.portal_room {
                        let bb = s.pieces[idx].bounds;
                        let mut chunks = Vec::new();
                        for tx in (bb[0] >> 4)..=(bb[3] >> 4) {
                            for tz in (bb[2] >> 4)..=(bb[5] >> 4) {
                                chunks.push((tx, tz));
                            }
                        }
                        portal_chunks = Some(chunks);
                        break;
                    }
                }
            }
            if portal_chunks.is_some() {
                break;
            }
        }
        let sh_chunks = portal_chunks.expect("a Stronghold PortalRoom in seed 4242");
        let mut silverfish_spawners = Vec::new();
        for (cx, cz) in sh_chunks {
            let mut refs = References::collect(&mut ow, cx, cz);
            let pop = pop_seed(ow.wseed(), cx * 16, cz * 16);
            let mut ctx = FC::new(&mut ow);
            refs.place_step(&mut ctx, sh_step, pop, cx, cz);
            for (pos, payload) in ctx.payloads {
                if matches!(&payload, GenerationPayload::SpawnerEntity(id) if id == "minecraft:silverfish") {
                    silverfish_spawners.push(pos);
                }
            }
        }
        assert!(
            !silverfish_spawners.is_empty(),
            "Stronghold PortalRoom must push SpawnerEntity(\"minecraft:silverfish\")"
        );

        // 3. Monument -> Entity("minecraft:elder_guardian")
        let &mon_set = d.by_id.get("minecraft:ocean_monuments").expect("monuments set");
        let mut mon_hit: Option<(usize, [i32; 6])> = None;
        'mon_scan: for cx in -64..=64 {
            for cz in -64..=64 {
                if !set_admits_live(d, &mut ow, mon_set, 4242, cx, cz, 0) {
                    continue;
                }
                for start in starts_cached(&mut ow, cx, cz).iter() {
                    if matches!(start.pieces, StartPieces::Monument { .. }) {
                        mon_hit = Some((d.structures[&start.structure].step, start.bbox));
                        break 'mon_scan;
                    }
                }
            }
        }
        let (m_step, m_bb) = mon_hit.expect("an Ocean Monument in seed 4242");
        let mut elder_pos = Vec::new();
        for cx in (m_bb[0] >> 4)..=(m_bb[3] >> 4) {
            for cz in (m_bb[2] >> 4)..=(m_bb[5] >> 4) {
                let mut refs = References::collect(&mut ow, cx, cz);
                let pop = pop_seed(ow.wseed(), cx * 16, cz * 16);
                let mut ctx = FC::new(&mut ow);
                refs.place_step(&mut ctx, m_step, pop, cx, cz);
                for (_, payload) in ctx.payloads {
                    if let GenerationPayload::Entity { id, pos, .. } = payload {
                        if id == "minecraft:elder_guardian" {
                            elder_pos.push(pos);
                        }
                    }
                }
            }
        }
        assert!(
            !elder_pos.is_empty(),
            "Monument placement must push elder_guardian entities"
        );
        for pos in &elder_pos {
            assert_eq!(pos[0].rem_euclid(1.0), 0.5);
            assert_eq!(pos[2].rem_euclid(1.0), 0.5);
        }

        // 4. SwampHut -> Entity("minecraft:witch") & Entity("minecraft:cat")
        let &hut_set = d.by_id.get("minecraft:swamp_huts").expect("swamp_huts set");
        let mut hut_hit: Option<(usize, [i32; 6])> = None;
        'hut_scan: for cx in -512..=512 {
            for cz in -512..=512 {
                if !set_admits_live(d, &mut ow, hut_set, 4242, cx, cz, 0) {
                    continue;
                }
                for start in starts_cached(&mut ow, cx, cz).iter() {
                    if matches!(start.pieces, StartPieces::SwampHut(_)) {
                        hut_hit = Some((d.structures[&start.structure].step, start.bbox));
                        break 'hut_scan;
                    }
                }
            }
        }
        let (h_step, h_bb) = hut_hit.expect("a SwampHut in seed 4242");
        let mut hut_entities = Vec::new();
        for cx in (h_bb[0] >> 4)..=(h_bb[3] >> 4) {
            for cz in (h_bb[2] >> 4)..=(h_bb[5] >> 4) {
                let mut refs = References::collect(&mut ow, cx, cz);
                let pop = pop_seed(ow.wseed(), cx * 16, cz * 16);
                let mut ctx = FC::new(&mut ow);
                refs.place_step(&mut ctx, h_step, pop, cx, cz);
                for (_, payload) in ctx.payloads {
                    if let GenerationPayload::Entity { id, .. } = payload {
                        hut_entities.push(id);
                    }
                }
            }
        }
        assert!(hut_entities.iter().any(|id| id == "minecraft:witch"));
        assert!(hut_entities.iter().any(|id| id == "minecraft:cat"));
    }

    /// The container payloads the remaining structure families produce reach `FC` during
    /// placement, read at the chunks native generates them for seed 4242.
    #[test]
    fn structure_container_payloads_reach_fc() {
        use crate::generation_context::{ContainerKind, GenerationPayload};
        reset_cache();
        let d = defs().expect("structure definitions load");
        let mut ow = Gen::load(4242, crate::name_to_state).expect("overworld gen");

        fn payloads_at(
            g: &mut Gen,
            step: usize,
            cx: i32,
            cz: i32,
        ) -> Vec<((i32, i32, i32), crate::generation_context::GenerationPayload)> {
            let mut refs = References::collect(g, cx, cz);
            let pop = pop_seed(g.wseed(), cx * 16, cz * 16);
            let mut ctx = FC::new(g);
            refs.place_step(&mut ctx, step, pop, cx, cz);
            ctx.payloads
        }

        // `minecraft:buried_treasure`: the piece stages its own chest
        // (`StructurePiece.createChest`).
        let buried = payloads_at(
            &mut ow,
            d.structures["minecraft:buried_treasure"].step,
            -181,
            127,
        );
        assert_eq!(buried.len(), 1, "one buried treasure chest");
        assert_eq!(buried[0].0, (-2887, 52, 2041), "at the cell it was written to");
        match &buried[0].1 {
            GenerationPayload::LootContainer {
                kind,
                loot_table,
                loot_seed,
            } => {
                assert_eq!(*kind, ContainerKind::Chest);
                assert_eq!(loot_table, "minecraft:chests/buried_treasure");
                // `createChest` draws the seed it stores for a chest with no `LootTableSeed`.
                assert!(loot_seed.is_some(), "the piece draws the chest's loot seed");
            }
            other => panic!("a buried treasure chest stages a loot container, got {other:?}"),
        }

        // `minecraft:desert_pyramid`: the four treasure chests and the
        // archaeology suspicious-sand brushables stage their own payloads.
        let pyramid = payloads_at(
            &mut ow,
            d.structures["minecraft:desert_pyramid"].step,
            -573,
            -555,
        );
        let pyramid_chests: Vec<_> = pyramid
            .iter()
            .filter(|(_, p)| matches!(p, GenerationPayload::LootContainer { .. }))
            .collect();
        let pyramid_brushables: Vec<_> = pyramid
            .iter()
            .filter(|(_, p)| matches!(p, GenerationPayload::Brushable { .. }))
            .collect();
        assert_eq!(pyramid_chests.len(), 4, "four pyramid chests in this chunk");
        assert!(!pyramid_brushables.is_empty(), "pyramid archaeology brushables stage in this chunk");
        assert_eq!(
            pyramid_chests.len() + pyramid_brushables.len(),
            pyramid.len(),
            "only chests and brushables stage in the desert pyramid"
        );
        for (pos, payload) in &pyramid_chests {
            assert_eq!(
                payload,
                &GenerationPayload::LootContainer {
                    kind: ContainerKind::Chest,
                    loot_table: "minecraft:chests/desert_pyramid".to_owned(),
                    loot_seed: match payload {
                        GenerationPayload::LootContainer { loot_seed, .. } => *loot_seed,
                        _ => unreachable!(),
                    },
                },
                "{pos:?} carries the pyramid chest table"
            );
        }

        // `minecraft:jungle_pyramid`: two chests and two dispensers, each in the store that
        // reads that block.
        let jungle = payloads_at(
            &mut ow,
            d.structures["minecraft:jungle_pyramid"].step,
            -394,
            201,
        );
        let kind_of = |payload: &crate::generation_context::GenerationPayload| match payload {
            GenerationPayload::LootContainer { kind, .. } => Some(*kind),
            _ => None,
        };
        let jungle_chests = jungle
            .iter()
            .filter(|(_, p)| {
                kind_of(p) == Some(ContainerKind::Chest)
                    && matches!(p, GenerationPayload::LootContainer { loot_table, .. } if loot_table == "minecraft:chests/jungle_temple")
            })
            .count();
        let jungle_dispensers = jungle
            .iter()
            .filter(|(_, p)| {
                kind_of(p) == Some(ContainerKind::BlockContainer(9))
                    && matches!(p, GenerationPayload::LootContainer { loot_table, .. } if loot_table == "minecraft:chests/jungle_temple_dispenser")
            })
            .count();
        assert_eq!(jungle_chests, 2, "two jungle temple chests");
        assert_eq!(jungle_dispensers, 2, "two jungle temple dispensers");

        // A `minecraft:village_plains` chunk: a jigsaw piece's container states reach the same
        // stores — the weapon smith's chest as a loot container and the house furnaces as
        // container `Items` in the furnace store.
        let village = payloads_at(
            &mut ow,
            d.structures["minecraft:village_plains"].step,
            -191,
            -95,
        );
        assert!(
            village.iter().any(|(_, p)| matches!(
                p,
                GenerationPayload::Container { kind: ContainerKind::Furnace, .. }
            )),
            "a village furnace's `Items` belong in the furnace store"
        );
        assert!(
            village.iter().any(|(_, p)| matches!(
                p,
                GenerationPayload::LootContainer { kind: ContainerKind::Chest, loot_table, .. }
                    if loot_table == "minecraft:chests/village/village_weaponsmith"
            )),
            "the weapon smith's chest is staged as a chest loot container"
        );
    }
}

/// F1 window guards that belong to the *feature* layer's module scope: they call the
/// placement code directly, which is `pub(crate)`-invisible from another module.
///
/// (This file is `include!`d into `feature.rs`, so these tests are children of the
/// module that owns `mod_positions` / `freg` / `Pf`.)
#[cfg(test)]
mod window_tests {
    #[cfg(megu_vanilla_data)]
    use super::*;

    /// `height_range` samples the **generator** window, not the world window.
    ///
    /// It is the placement modifier of 23 Nether placed features (`ore_quartz_nether`,
    /// `glowstone`, `spring_lava`, the `patch_*` vegetation, ...) and 0 End ones, so a
    /// 128 -> 256 change here is loud and invisible at the same time: no Nether fixture
    /// row has a cell above 127, so nothing in the matrix would see the draws move.
    ///
    /// Vanilla resolves the anchors against `WorldGenerationContext`, i.e.
    /// `chunkGenerator.getMinY()/getGenDepth()` — the noise settings window. The
    /// measurement that fixes it: on the pinned Nether fixture chunk (-25,6)
    /// `nether_quartz_ore` reaches y=117 on both sides, which is exactly
    /// `BelowTop(10)` over a 0/128 window; with `hi = 245` roughly half of the ~150
    /// quartz cells would land in air above the roof.
    #[test]
    #[cfg(megu_vanilla_data)]
    fn height_range_positions_sample_the_generator_window() {
        let mut g = crate::worldgen::Gen::load_preset(
            4242,
            crate::name_to_state,
            crate::worldgen::NoisePreset::Nether,
        )
        .expect("nether gen");
        let gen_window = (g.min_y, g.height);
        let world_window = (g.world_min_y, g.world_height);
        assert_eq!(gen_window, (0, 128), "the noise settings window");
        assert_eq!(world_window, (0, 256), "the dimension_type window");
        let pf = freg()
            .placed
            .get("minecraft:ore_quartz_nether")
            .expect("placed feature ore_quartz_nether");
        let hr = pf
            .mods
            .iter()
            .find(|m| matches!(m, PM::HeightRange(_)))
            .expect("ore_quartz_nether is placed by height_range");
        let PM::HeightRange(hp) = hr else {
            unreachable!()
        };
        let mut ctx = FC::new(&mut g);
        let mut rng = Xor::seed(4242);
        let mut out = Vec::new();
        let mut got = Vec::new();
        for _ in 0..1024 {
            out.clear();
            mod_positions(hr, freg(), &mut ctx, &mut rng, (0, 64, 0), pf, &mut out);
            assert_eq!(out.len(), 1, "height_range emits exactly one position");
            got.push(out[0].1);
        }
        // The same draws, replayed straight from the generator window: the placement code
        // must be passing exactly these two numbers.
        let mut replay = Xor::seed(4242);
        let expected: Vec<i32> = (0..1024)
            .map(|_| hp.get(&mut replay, gen_window.0, gen_window.1))
            .collect();
        assert_eq!(
            got, expected,
            "height_range must sample the generator window ({gen_window:?})"
        );
        // The bounds the Nether's ores were measured inside.
        assert!(got.iter().all(|y| (10..=117).contains(y)), "{:?}", got.iter().min());
        assert_eq!(got.iter().max(), Some(&117), "BelowTop(10) over 0/128");
        // Negative control: the assertion above can fail. The world window is the same
        // range expression resolved against 0/256, and it does not produce these draws.
        let mut replay = Xor::seed(4242);
        let widened: Vec<i32> = (0..1024)
            .map(|_| hp.get(&mut replay, world_window.0, world_window.1))
            .collect();
        assert_ne!(
            got, widened,
            "widening the anchors must change the draws; otherwise this test pins nothing"
        );
        assert!(
            widened.iter().any(|y| *y > 127),
            "the world window must be able to draw above the roof, or the two windows are \\
             indistinguishable here"
        );
    }
}

#[cfg(test)]
mod defs_loader_tests {
    use super::*;

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn vanilla(kind: &str) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = Gen::dp_files(kind).into_iter().collect();
        files.sort_by_key(|a| registry_sort_key(&a.0));
        files
    }

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn fingerprint(d: &Defs) -> String {
        let mut s = String::new();
        let mut ids: Vec<_> = d.structures.keys().collect();
        ids.sort();
        for id in ids {
            let v = &d.structures[id];
            let mut biomes: Vec<_> = v.biomes.iter().collect();
            biomes.sort();
            s.push_str(&format!(
                "S|{id}|{:?}|{}|{}|{:?}|{:?}|{}\n",
                v.kick,
                v.step,
                v.slot,
                v.adjustment,
                v.template,
                biomes.into_iter().map(String::as_str).collect::<Vec<_>>().join(",")
            ));
        }
        for (i, step) in d.by_step.iter().enumerate() {
            s.push_str(&format!("P|{i}|{}\n", step.join(",")));
        }
        let mut set_ids: Vec<_> = d.by_id.iter().collect();
        set_ids.sort();
        for (id, &idx) in set_ids {
            let set = &d.sets[idx];
            let entries: Vec<String> = set
                .entries
                .iter()
                .map(|(e, w)| format!("{e}:{w}"))
                .collect();
            let placement = match &set.placement {
                PlacementDef::Spread {
                    spacing,
                    separation,
                    salt,
                    triangular,
                    frequency,
                    reducer,
                    exclusion,
                } => format!(
                    "spread|{spacing}|{separation}|{salt}|{triangular}|{}|{reducer}|{exclusion:?}",
                    frequency.to_bits()
                ),
                PlacementDef::Rings {
                    distance,
                    spread,
                    count,
                    preferred_biomes,
                } => {
                    let mut bs: Vec<_> = preferred_biomes.iter().collect();
                    bs.sort();
                    format!(
                        "rings|{distance}|{spread}|{count}|{}",
                        bs.into_iter().map(String::as_str).collect::<Vec<_>>().join(",")
                    )
                }
            };
            s.push_str(&format!("T|{id}|{idx}|{placement}|{}\n", entries.join(",")));
        }
        s.push_str(&format!("A|{:?}\n", d.adapting_sets));
        let mut mono: Vec<_> = d.monument_surrounding.iter().collect();
        mono.sort();
        s.push_str(&format!(
            "M|{}\n",
            mono.into_iter().map(String::as_str).collect::<Vec<_>>().join(",")
        ));
        s
    }

    /// SHA-256 of the fingerprint the pre-skip loader produced from the pinned
    /// vanilla datapack — measured at HEAD `8dad8c7` before `load_defs_from_files`
    /// existed. If a refactor moves one structure slot, template payload, set
    /// placement, biome list or adapting-set index, this digest moves with it.
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    const VANILLA_FP: &str = "915d86995cdbf21f7df2b7e40fb6d6152e19a683edc8b081c7146cb60a35f230";

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn digest(d: &Defs) -> String {
        let bytes = crate::generation_math::sha256(fingerprint(d).as_bytes());
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn vanilla_defs_load_identically() {
        let structures = vanilla("structure");
        let sets = vanilla("structure_set");
        let d = load_defs_from_files(&structures, &sets).expect("vanilla defs load");
        assert_eq!(d.structures.len(), 34);
        assert_eq!(d.sets.len(), 20);
        assert_eq!(d.by_step.iter().map(|v| v.len()).sum::<usize>(), 34);
        assert_eq!(
            digest(&d),
            VANILLA_FP,
            "the 0-malformed-file path must be identical to the pre-skip loader"
        );
        // The production entry point enumerates the same files and must agree with
        // the helper exactly, not just in count.
        assert_eq!(digest(&load_defs_inner().expect("defs")), VANILLA_FP);
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn malformed_structure_entries_are_skipped_without_shifting_slots() {
        let mut structures = vanilla("structure");
        // All three ids sort before `minecraft:ancient_city`, so an entry that
        // wrongly consumed a slot would shift every valid structure's number.
        structures.push(("minecraft:aaa_bad_json".into(), b"{not json".to_vec()));
        structures.push((
            "minecraft:aaa_bad_step".into(),
            br#"{"type":"minecraft:buried_treasure"}"#.to_vec(),
        ));
        structures.push((
            "minecraft:aaa_bad_template".into(),
            br#"{"type":"minecraft:nether_fossil","step":"underground_structures"}"#.to_vec(),
        ));
        let sets = vanilla("structure_set");
        let d = load_defs_from_files(&structures, &sets)
            .expect("the pipeline must survive malformed structures");
        assert_eq!(d.structures.len(), 34, "exactly the three injected entries drop");
        assert_eq!(d.sets.len(), 20);
        assert_eq!(
            digest(&d),
            VANILLA_FP,
            "every survivor keeps its exact slot, template and set"
        );
        for bad in ["aaa_bad_json", "aaa_bad_step", "aaa_bad_template"] {
            assert!(!d.structures.contains_key(&format!("minecraft:{bad}")));
        }
        // The valid igloo still carries its template and its surface slot.
        let igloo = &d.structures["minecraft:igloo"];
        assert_eq!(igloo.step, 4);
        assert_eq!(igloo.slot, 3);
        assert!(igloo.template.is_some());
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn malformed_structure_set_entries_are_skipped() {
        let structures = vanilla("structure");
        let mut sets = vanilla("structure_set");
        // These sort before the vanilla sets, so a bad set must not shift `by_id`.
        sets.push(("minecraft:aaa_bad_json".into(), b"{not json".to_vec()));
        sets.push((
            "minecraft:aaa_bad_placement".into(),
            br#"{"placement":{"type":"minecraft:no_such_placement"}}"#.to_vec(),
        ));
        sets.push((
            "minecraft:aaa_bad_structures".into(),
            br#"{"placement":{"type":"minecraft:random_spread","spacing":32}}"#.to_vec(),
        ));
        sets.push((
            "minecraft:aaa_empty_structures".into(),
            br#"{"placement":{"type":"minecraft:random_spread","spacing":32},"structures":[]}"#
                .to_vec(),
        ));
        let d = load_defs_from_files(&structures, &sets)
            .expect("the pipeline must survive malformed sets");
        assert_eq!(d.sets.len(), 20, "exactly the four injected sets drop");
        assert_eq!(d.structures.len(), 34);
        assert_eq!(
            digest(&d),
            VANILLA_FP,
            "the surviving sets keep their ids and indices"
        );
        for bad in [
            "aaa_bad_json",
            "aaa_bad_placement",
            "aaa_bad_structures",
            "aaa_empty_structures",
        ] {
            assert!(!d.by_id.contains_key(&format!("minecraft:{bad}")));
        }
        // The pillager exclusion zone and the stronghold rings still resolve.
        let pillager = &d.sets[*d.by_id.get("minecraft:pillager_outposts").expect("pillager")];
        assert!(matches!(
            &pillager.placement,
            PlacementDef::Spread { exclusion: Some((other, 10)), .. }
                if other == "minecraft:villages"
        ));
        let strongholds = &d.sets[*d.by_id.get("minecraft:strongholds").expect("strongholds")];
        assert!(matches!(
            &strongholds.placement,
            PlacementDef::Rings { distance: 32, spread: 3, count: 128, .. }
        ));
    }
}
