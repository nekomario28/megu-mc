//! General 1.21.1 `BaseSpawner.serverTick`: NBT is authoritative; no parallel timer.
//! World collision/light, players and inserted mobs belong to the runtime.
use crate::{generation_math::Lcg, nbt::{NbtString, Tag}};
type Pos=(i32,i32,i32);
#[derive(Clone, Copy, Debug)]
pub(crate) struct LightRules {pub block:[i32;2],pub sky:[i32;2]}
pub(crate) trait World {
    fn active(&mut self,origin:Pos,range:i32)->bool;
    fn no_collision(&mut self,pos:[f64;3])->bool;
    fn rules(&mut self,pos:[f64;3],custom:Option<LightRules>,random:&mut Lcg)->bool;
    fn nearby(&mut self,origin:Pos,range:i32)->usize;
    fn unobstructed(&mut self,pos:[f64;3])->bool;
    fn insert(&mut self,pos:[f64;3],yaw:f32)->bool;
    fn effects(&mut self,origin:Pos,pos:[f64;3]);
}

/// Placement family used by `NaturalSpawner` when checking a generated creature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreaturePlacement { Ground, Water, Lava, Air }

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CreatureSpawnerData {
    pub kind: String,
    pub min_count: i32,
    pub max_count: i32,
    pub weight: i32,
}

/// World queries needed by the initial chunk-generation creature pass.
pub(crate) trait ChunkGenerationWorld {
    fn spawn_y(&mut self, kind: &str, x: i32, z: i32) -> i32;
    fn ground_spawnable(&mut self, kind: &str, pos: Pos) -> bool;
    fn water_spawnable(&mut self, kind: &str, pos: Pos) -> bool;
    fn lava_spawnable(&mut self, kind: &str, pos: Pos) -> bool;
    fn air_spawnable(&mut self, kind: &str, pos: Pos) -> bool;
    fn add_creature(&mut self, kind: &str, pos: Pos);
}

fn creature_placement(kind: &str) -> CreaturePlacement {
    match kind {
        "minecraft:cod" | "minecraft:salmon" | "minecraft:tropical_fish"
        | "minecraft:pufferfish" | "minecraft:squid" | "minecraft:glow_squid"
        | "minecraft:dolphin" | "minecraft:turtle" | "minecraft:axolotl"
        | "minecraft:guardian" | "minecraft:elder_guardian" => CreaturePlacement::Water,
        "minecraft:strider" => CreaturePlacement::Lava,
        "minecraft:bat" | "minecraft:parrot" => CreaturePlacement::Air,
        _ => CreaturePlacement::Ground,
    }
}

fn choose_creature<'a>(entries: &'a [CreatureSpawnerData], random: &mut Lcg) -> Option<&'a CreatureSpawnerData> {
    let total = entries.iter().try_fold(0i32, |sum, entry| {
        sum.checked_add(entry.weight).filter(|_| entry.weight >= 0)
    })?;
    if total <= 0 { return None; }
    let mut roll = random.next_bound(total);
    entries.iter().find(|entry| {
        roll -= entry.weight;
        roll < 0
    })
}

/// 1.21.1 `NaturalSpawner.spawnMobsForChunkGeneration` for creature spawning.
/// An omitted biome probability uses `MobSpawnSettings`' default of 0.1.
/// The lookup callback supplies the biome's `creature` category without adding
/// a dependency from this lower-level module to the feature registry.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn spawn_mobs_for_chunk_generation(
    chunk_x: i32,
    chunk_z: i32,
    center_biome: &str,
    creature_probability: Option<f32>,
    mut spawners_for: impl FnMut(&str, &str) -> Vec<CreatureSpawnerData>,
    world: &mut impl ChunkGenerationWorld,
    random: &mut Lcg,
) {
    let probability = creature_probability.unwrap_or(0.1);
    let entries = spawners_for(center_biome, "creature");
    if entries.is_empty() { return; }
    while random.next_f32() < probability {
        let Some(entry) = choose_creature(&entries, random) else { return; };
        let mut x = chunk_x * 16 + random.next_bound(16);
        let mut z = chunk_z * 16 + random.next_bound(16);
        let count = entry.min_count + random.next_bound((entry.max_count - entry.min_count + 1).max(1));
        for _ in 0..count {
            x += random.next_bound(5) - 2;
            z += random.next_bound(5) - 2;
            let pos = (x, world.spawn_y(&entry.kind, x, z), z);
            let accepted = match creature_placement(&entry.kind) {
                CreaturePlacement::Ground => world.ground_spawnable(&entry.kind, pos),
                CreaturePlacement::Water => world.water_spawnable(&entry.kind, pos),
                CreaturePlacement::Lava => world.lava_spawnable(&entry.kind, pos),
                CreaturePlacement::Air => world.air_spawnable(&entry.kind, pos),
            };
            if accepted { world.add_creature(&entry.kind, pos); }
        }
    }
}

fn int(tag:Option<&Tag>)->i32 {
    match tag {
        Some(Tag::Byte(v))=>i32::from(*v),Some(Tag::Short(v))=>i32::from(*v),Some(Tag::Int(v))=>*v,
        Some(Tag::Long(v))=>*v as i32,Some(Tag::Float(v))=>f32::from_bits(*v) as i32,
        Some(Tag::Double(v))=>f64::from_bits(*v) as i32,_=>0,
    }
}
fn short(tag:&Tag,key:&str)->i32 {
    // CompoundTag.getShort accepts any numeric tag, truncating through short.
    let value=match tag.get(key) {Some(Tag::Long(v))=>*v as i16,_=>int(tag.get(key)) as i16};
    i32::from(value)
}
fn set_delay(tag:&mut Tag,value:i32) {
    let Tag::Compound(fields)=tag else {unreachable!()};
    fields.insert(NbtString::text("Delay"),Tag::Short(value as i16));
}
fn reset(tag:&mut Tag,random:&mut Lcg) {
    let (min,max)=if tag.get("MinSpawnDelay").is_some(){(short(tag,"MinSpawnDelay"),short(tag,"MaxSpawnDelay"))}else{(200,800)};
    set_delay(tag,if max<=min {min}else{min+random.next_bound(max-min)});
}
fn limits(tag:&Tag,key:&str)->Result<[i32;2],&'static str>{
    let Some(v)=tag.get(key) else{return Ok([0,15])};
    let (min,max)=if matches!(v,Tag::Int(_)){let n=int(Some(v));(n,n)}else{
        let lo=v.get("min_inclusive").ok_or("spawner light minimum missing")?;
        let hi=v.get("max_inclusive").ok_or("spawner light maximum missing")?;
        (int(Some(lo)),int(Some(hi)))
    };
    if !(0..=15).contains(&min)||!(min..=15).contains(&max){return Err("invalid spawner custom light interval");}
    Ok([min,max])
}
/// 1.21.1 `BaseSpawner` rejects edited spawners by malformed potentials: this
/// is the rejection for entries that are not valid weighted spawn data.
const UNSUPPORTED_SPAWN_POTENTIALS: &str = "malformed spawner potentials";

/// 1.21.1 `BaseSpawner` rejects an entity id that is not a `ResourceLocation`.
const MALFORMED_SPAWN_ENTITY: &str = "invalid spawner entity id";

/// `true` when `id` is shaped like a `ResourceLocation`: an optional `namespace:`
/// prefix (`[a-z0-9_.-]`) and a path (`[a-z0-9/._-]`), neither empty. A missing
/// namespace means `minecraft`, as `ResourceLocation.parse` reads it.
///
/// The spawner checks the id's shape and nothing else, which is what vanilla does:
/// `BaseSpawner` keeps whatever `entity.id` names, and `EntityType.create` simply
/// yields nothing for a type no registry holds. So a modded `mymod:shadow_knight`
/// spawner round-trips, and a spawner naming an entity this process never
/// registered stays inert instead of refusing to load.
fn valid_entity_id(id: &str) -> bool {
    fn shaped(value: &str, path: bool) -> bool {
        !value.is_empty()
            && value.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'_' | b'.' | b'-')
                    || (path && b == b'/')
            })
    }
    let (namespace, path) = match id.split_once(':') {
        Some((namespace, path)) => (namespace, path),
        None => ("minecraft", id),
    };
    // `split_once` took the first `:`, so a second one stays in `path` and fails there.
    shaped(namespace, false) && shaped(path, true)
}

/// Validates the `entity` compound of a `SpawnData` entry: a well-formed entity id,
/// and nothing but `id`/`Pos` fields in it.
fn validate_spawn_data_entity(entity: &Tag) -> Result<(), &'static str> {
    let Some(id) = entity.get("id").and_then(Tag::string) else {
        return Err("spawner entity id missing");
    };
    if !id.to_utf8().is_ok_and(|name| valid_entity_id(&name)) {
        return Err(MALFORMED_SPAWN_ENTITY);
    }
    if let Tag::Compound(fields) = entity {
        if fields
            .keys()
            .any(|k| *k != NbtString::text("id") && *k != NbtString::text("Pos"))
        {
            return Err("edited spawner entity data is not implemented");
        }
    }
    Ok(())
}

/// One validated 1.21.1 `SpawnPotentials` entry: a positive integer `weight`
/// and the entry's `SpawnData` `data: Compound` (its `entity` is validated).
struct SpawnerPotential {
    weight: i32,
    data: Tag,
}

/// Parses and validates the 1.21.1 `SpawnPotentials` weighted list. Absent or
/// empty potentials keep the generated-content behavior: the stored
/// `SpawnData` is authoritative and no selection happens.
fn parse_spawner_potentials(tag: &Tag) -> Result<Vec<SpawnerPotential>, &'static str> {
    let Some(entries) = tag.get("SpawnPotentials").and_then(Tag::list) else {
        return Ok(Vec::new());
    };
    let mut potentials = Vec::with_capacity(entries.len());
    for entry in entries {
        let Tag::Compound(fields) = entry else {
            return Err(UNSUPPORTED_SPAWN_POTENTIALS);
        };
        let weight = match fields.get(&NbtString::text("weight")) {
            Some(Tag::Int(w)) if *w > 0 => *w,
            _ => return Err(UNSUPPORTED_SPAWN_POTENTIALS),
        };
        let data = fields
            .get(&NbtString::text("data"))
            .ok_or(UNSUPPORTED_SPAWN_POTENTIALS)?;
        let entity = data
            .get("entity")
            .ok_or(UNSUPPORTED_SPAWN_POTENTIALS)?;
        validate_spawn_data_entity(entity)?;
        potentials.push(SpawnerPotential {
            weight,
            data: data.clone(),
        });
    }
    Ok(potentials)
}

/// Vanilla `WeightedRandom.getWeightedRandom` over the validated potentials:
/// one `nextInt(total)` draw picks the entry whose cumulative weight range
/// covers it.
fn weighted_pick<'a>(potentials: &'a [SpawnerPotential], random: &mut Lcg) -> Option<&'a SpawnerPotential> {
    let total: i32 = potentials.iter().map(|p| p.weight).sum();
    if total <= 0 {
        return None;
    }
    let mut roll = random.next_bound(total);
    for potential in potentials {
        roll -= potential.weight;
        if roll < 0 {
            return Some(potential);
        }
    }
    None
}

/// Generated spawners use a single entry and empty potentials; edited spawners
/// fail closed on a malformed entity id or malformed potentials. An entity id that
/// is well formed but names nothing registered — a modded `mymod:shadow_knight`, or
/// a vanilla type this port does not simulate — keeps the spawner's own
/// `custom_spawn_rules` (or the runtime default) and simply finds nothing to insert.
pub(crate) fn tick(tag:&mut Tag,origin:Pos,world:&mut impl World,random:&mut Lcg)->Result<(),&'static str>{
    if !matches!(tag,Tag::Compound(_)){return Err("spawner NBT is not a compound");}
    let player_range=if tag.get("MaxNearbyEntities").is_some(){short(tag,"RequiredPlayerRange")}else{16};
    if !world.active(origin,player_range){return Ok(());}
    // Malformed 1.21.1 potentials fail closed before anything mutates the tag.
    let potentials=parse_spawner_potentials(tag)?;
    let mut delay=short(tag,"Delay");
    if delay==-1{reset(tag,random);delay=short(tag,"Delay");}
    if delay>0{set_delay(tag,delay-1);return Ok(());}
    let data=tag.get("SpawnData").ok_or("spawner SpawnData missing")?;
    let entity=data.get("entity").ok_or("spawner entity missing")?;
    validate_spawn_data_entity(entity)?;
    let custom=data.get("custom_spawn_rules").map(|v|Ok(LightRules{block:limits(v,"block_light_limit")?,sky:limits(v,"sky_light_limit")?})).transpose()?;
    let fixed=match entity.get("Pos") {Some(Tag::List(6,v))=>v.as_slice(),_=>&[]};
    if fixed.iter().any(|v|!matches!(v,Tag::Double(bits) if f64::from_bits(*bits).is_finite())){
        return Err("invalid spawner position");
    }
    let count=if tag.get("MinSpawnDelay").is_some(){short(tag,"SpawnCount")}else{4};
    let range=if tag.get("SpawnRange").is_some(){short(tag,"SpawnRange")}else{4};
    let max=if tag.get("MaxNearbyEntities").is_some(){short(tag,"MaxNearbyEntities")}else{6};
    let mut success=false;
    for _ in 0..count {
        let mut p=[0.;3];
        // Native order is X (two doubles), Y (bounded int), Z (two doubles).
        for (i, coordinate) in p.iter_mut().enumerate() {
            *coordinate=if let Some(Tag::Double(v))=fixed.get(i){f64::from_bits(*v)}else{match i {
                0=>origin.0 as f64+(random.next_f64()-random.next_f64())*range as f64+0.5,
                1=>(origin.1+random.next_bound(3)-1) as f64,
                _=>origin.2 as f64+(random.next_f64()-random.next_f64())*range as f64+0.5,
            }};
        }
        if !world.no_collision(p)||!world.rules(p,custom,random){continue;}
        if world.nearby(origin,range) as i32>=max{reset(tag,random);return Ok(());}
        let yaw=random.next_f32()*360.;
        if !world.unobstructed(p){continue;}
        if !world.insert(p,yaw){reset(tag,random);return Ok(());}
        world.effects(origin,p);success=true;
    }
    if success{reset(tag,random);}
    // When the spawner holds weighted potentials, vanilla re-picks
    // `spawnData` after the cycle: write the selected entry's data back as
    // the active `SpawnData`.
    if let Some(next)=weighted_pick(&potentials,random){
        if let Tag::Compound(map)=tag{
            map.insert(NbtString::text("SpawnData"),next.data.clone());
        }
    }
    Ok(())
}

#[cfg(all(test, megu_vanilla_data))]
#[path = "spawner_tests.rs"]
mod tests;

#[cfg(test)]
mod mod_spawner_entity_tests {
    use super::*;

    /// A world where every query passes, so a `tick` reaching the insert stage is
    /// observable as `Ok`: the entity id is the only thing under test.
    struct PermissiveWorld;

    impl World for PermissiveWorld {
        fn active(&mut self, _: Pos, _: i32) -> bool {
            true
        }
        fn no_collision(&mut self, _: [f64; 3]) -> bool {
            true
        }
        fn rules(&mut self, _: [f64; 3], _: Option<LightRules>, _: &mut Lcg) -> bool {
            true
        }
        fn nearby(&mut self, _: Pos, _: i32) -> usize {
            0
        }
        fn unobstructed(&mut self, _: [f64; 3]) -> bool {
            true
        }
        fn insert(&mut self, _: [f64; 3], _: f32) -> bool {
            true
        }
        fn effects(&mut self, _: Pos, _: [f64; 3]) {}
    }

    /// One edited spawner holding `entity_id` as its `SpawnData.entity.id`.
    fn spawner_for(entity_id: &str) -> Tag {
        let mut entity = std::collections::BTreeMap::new();
        entity.insert(
            NbtString::text("id"),
            Tag::String(NbtString::text(entity_id)),
        );
        let mut data = std::collections::BTreeMap::new();
        data.insert(NbtString::text("entity"), Tag::Compound(entity));
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(NbtString::text("SpawnData"), Tag::Compound(data));
        fields.insert(NbtString::text("Delay"), Tag::Short(0));
        fields.insert(NbtString::text("MinSpawnDelay"), Tag::Short(200));
        fields.insert(NbtString::text("MaxSpawnDelay"), Tag::Short(800));
        fields.insert(NbtString::text("SpawnCount"), Tag::Short(1));
        fields.insert(NbtString::text("MaxNearbyEntities"), Tag::Short(6));
        Tag::Compound(fields)
    }

    fn ticks(entity_id: &str) -> Result<(), &'static str> {
        let mut tag = spawner_for(entity_id);
        let mut world = PermissiveWorld;
        let mut rng = Lcg::new(7);
        tick(&mut tag, (0, 0, 0), &mut world, &mut rng)
    }

    #[test]
    fn accepts_vanilla_entities_the_closed_allowlist_rejected() {
        // None of these are on the twelve-name list this port used to enforce.
        for id in [
            "minecraft:zombie_villager",
            "minecraft:wither_skeleton",
            "minecraft:pillager",
            "minecraft:vex",
            "minecraft:enderman",
            "minecraft:allay",
            "minecraft:sniffer",
        ] {
            assert_eq!(ticks(id), Ok(()), "{id} spawns");
        }
    }

    #[test]
    fn accepts_modded_namespaced_entities() {
        for id in [
            "mymod:shadow_knight",
            "mymod:deep/shadow_king",
            "othermod:boss.golem",
            // A bare path is the `minecraft` namespace, as `ResourceLocation` reads it.
            "custom_mob",
            "minecraft:cow",
        ] {
            assert_eq!(ticks(id), Ok(()), "{id} spawns");
        }
    }

    #[test]
    fn rejects_malformed_entity_ids() {
        for id in ["", "mymod:", ":", "Upper:case", "two:colons:here", "bad id"] {
            assert_eq!(ticks(id), Err(MALFORMED_SPAWN_ENTITY), "{id} is rejected");
        }
    }

    #[test]
    fn entity_id_grammar_matches_resource_location() {
        assert!(valid_entity_id("minecraft:zombie"));
        assert!(valid_entity_id("mymod:shadow.knight_v2"));
        assert!(valid_entity_id("mymod:deep/nested/path"));
        assert!(valid_entity_id("zombie")); // no namespace => minecraft
        assert!(!valid_entity_id(""));
        assert!(!valid_entity_id("minecraft:"));
        assert!(!valid_entity_id(":zombie"));
        assert!(!valid_entity_id("Mymod:zombie"));
        assert!(!valid_entity_id("mymod:Zombie"));
        assert!(!valid_entity_id("mymod:shadow knight"));
    }
}
