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
/// One vanilla 1.21.1 spawner entity: the server's entity type, the
/// `EntityType.<clinit>` bounding box, and the `MobCategory` serialized name
/// the runtime gates natural spawning by. Insertion of the specific mob is the
/// runtime world's `World::insert`; this table is the id allowlist and the
/// dimensions/category reference.
#[allow(dead_code)]
struct SpawnerMob {
    /// Mojang entity id, e.g. `minecraft:cave_spider`.
    id: &'static str,
    etype: i64,
    width: f32,
    height: f32,
    category: &'static str,
}

/// Vanilla 1.21.1 mob ids a spawner may legally hold: the dungeon
/// `monster_room` entries, the mineshaft cave-spider, the fortress blaze, the
/// bastion magma cube, the stronghold silverfish, and the trial/general
/// monsters. Anything outside this list is rejected, so an edited spawner can
/// never resurrect an id the server cannot simulate.
const SPAWNER_MOBS: &[SpawnerMob] = &[
    SpawnerMob { id: "minecraft:zombie", etype: 124, width: 0.6, height: 1.95, category: "monster" },
    SpawnerMob { id: "minecraft:skeleton", etype: 91, width: 0.6, height: 1.99, category: "monster" },
    SpawnerMob { id: "minecraft:spider", etype: 100, width: 1.4, height: 0.9, category: "monster" },
    SpawnerMob { id: "minecraft:cave_spider", etype: 16, width: 0.7, height: 0.5, category: "monster" },
    SpawnerMob { id: "minecraft:blaze", etype: 8, width: 0.5, height: 1.7, category: "monster" },
    SpawnerMob { id: "minecraft:magma_cube", etype: 67, width: 2.4, height: 0.9, category: "monster" },
    SpawnerMob { id: "minecraft:silverfish", etype: 90, width: 0.3, height: 0.5, category: "monster" },
    SpawnerMob { id: "minecraft:drowned", etype: 27, width: 0.6, height: 1.8, category: "monster" },
    SpawnerMob { id: "minecraft:stray", etype: 102, width: 0.6, height: 1.99, category: "monster" },
    SpawnerMob { id: "minecraft:husk", etype: 54, width: 0.6, height: 1.9, category: "monster" },
    SpawnerMob { id: "minecraft:breeze", etype: 12, width: 0.7, height: 0.9, category: "monster" },
    SpawnerMob { id: "minecraft:pig", etype: 77, width: 0.9, height: 0.9, category: "creature" },
];

/// 1.21.1 `BaseSpawner` rejects edited spawners by id: this is the rejection
/// for an entity id outside [`SPAWNER_MOBS`].
const UNSUPPORTED_SPAWN_ENTITY: &str = "unsupported spawner entity";

/// 1.21.1 `BaseSpawner` rejects edited spawners by malformed potentials: this
/// is the rejection for entries that are not valid weighted spawn data.
const UNSUPPORTED_SPAWN_POTENTIALS: &str = "malformed spawner potentials";

fn supported_spawner_mob(id: &NbtString) -> Option<&'static SpawnerMob> {
    let name = id.to_utf8().ok()?;
    SPAWNER_MOBS.iter().find(|m| m.id == name.as_str())
}

/// Validates the `entity` compound of a `SpawnData` entry: a supported 1.21.1
/// mob id, and nothing but `id`/`Pos` fields in it.
fn validate_spawn_data_entity(entity: &Tag) -> Result<(), &'static str> {
    let Some(id) = entity.get("id").and_then(Tag::string) else {
        return Err("spawner entity id missing");
    };
    if supported_spawner_mob(id).is_none() {
        return Err(UNSUPPORTED_SPAWN_ENTITY);
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
/// fail closed on unsupported entity ids or malformed potentials.
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
        for i in 0..3 {
            p[i]=if let Some(Tag::Double(v))=fixed.get(i){f64::from_bits(*v)}else{match i {
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

#[cfg(test)]
#[path = "spawner_tests.rs"]
mod tests;
