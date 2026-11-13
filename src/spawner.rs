//! Cave-spider BaseSpawner.serverTick. NBT is authoritative; no parallel timer.
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
/// Generated mineshafts use a single cave-spider entry and empty potentials.
/// Unsupported edited entity data is reported, never replaced by default mobs.
pub(crate) fn tick(tag:&mut Tag,origin:Pos,world:&mut impl World,random:&mut Lcg)->Result<(),&'static str>{
    if !matches!(tag,Tag::Compound(_)){return Err("spawner NBT is not a compound");}
    let player_range=if tag.get("MaxNearbyEntities").is_some(){short(tag,"RequiredPlayerRange")}else{16};
    if !world.active(origin,player_range){return Ok(());}
    if tag.get("SpawnPotentials").and_then(Tag::list).is_some_and(|v|!v.is_empty()){
        return Err("weighted spawner potentials are not implemented");
    }
    let mut delay=short(tag,"Delay");
    if delay==-1{reset(tag,random);delay=short(tag,"Delay");}
    if delay>0{set_delay(tag,delay-1);return Ok(());}
    let data=tag.get("SpawnData").ok_or("spawner SpawnData missing")?;
    let entity=data.get("entity").ok_or("spawner entity missing")?;
    if entity.get("id").and_then(Tag::string)!=Some(&NbtString::text("minecraft:cave_spider")){
        return Err("only cave-spider spawners are implemented");
    }
    if let Tag::Compound(fields)=entity {
        if fields.keys().any(|k|*k!=NbtString::text("id")&&*k!=NbtString::text("Pos")){
            return Err("edited cave-spider entity data is not implemented");
        }
    }
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
    Ok(())
}

#[cfg(test)]
#[path = "spawner_tests.rs"]
mod tests;
