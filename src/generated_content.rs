//! Typed NBT for generated non-block content and its save section. Runtime IDs
//! are assigned by the entity owner; UUID, loot and mutable NBT survive restart.
use crate::nbt::{self, NbtString, Tag};
use std::collections::{BTreeMap, HashSet};
type Pos = (i32, i32, i32);
pub(crate) type SpiderFlag = (i32, i32, usize);
fn compound(values: impl IntoIterator<Item = (&'static str, Tag)>) -> Tag {
    Tag::Compound(
        values
            .into_iter()
            .map(|(k, v)| (NbtString::text(k), v))
            .collect(),
    )
}
fn string(s: &str) -> Tag {
    Tag::String(NbtString::text(s))
}
pub(crate) fn spawner_nbt_for(p: Pos, entity_id: &str) -> Tag {
    let entity_id = if entity_id.contains(':') {
        entity_id.to_owned()
    } else {
        format!("minecraft:{entity_id}")
    };
    compound([
        ("id", string("minecraft:mob_spawner")),
        ("x", Tag::Int(p.0)),
        ("y", Tag::Int(p.1)),
        ("z", Tag::Int(p.2)),
        ("Delay", Tag::Short(20)),
        ("MinSpawnDelay", Tag::Short(200)),
        ("MaxSpawnDelay", Tag::Short(800)),
        ("SpawnCount", Tag::Short(4)),
        ("MaxNearbyEntities", Tag::Short(6)),
        ("RequiredPlayerRange", Tag::Short(16)),
        ("SpawnRange", Tag::Short(4)),
        ("SpawnPotentials", Tag::List(0, vec![])),
        (
            "SpawnData",
            compound([(
                "entity",
                compound([("id", string(&entity_id))]),
            )]),
        ),
    ])
}
pub(crate) fn spawner_nbt(p: Pos) -> Tag {
    spawner_nbt_for(p, "minecraft:cave_spider")
}
pub(crate) fn cart_loot_nbt(seed: i64) -> Tag {
    let mut values = vec![("LootTable", string("minecraft:chests/abandoned_mineshaft"))];
    // ContainerEntity writes a seed only when nonzero.
    if seed != 0 {
        values.push(("LootTableSeed", Tag::Long(seed)));
    }
    compound(values)
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Cart {
    pub eid: i32,
    pub data: Tag,
}
impl Cart {
    pub fn new(eid: i32, uuid: [u8; 16], p: Pos, seed: i64) -> Self {
        let Tag::Compound(mut data) = cart_loot_nbt(seed) else {
            unreachable!()
        };
        for (k, v) in [
            ("id", string("minecraft:chest_minecart")),
            (
                "UUID",
                Tag::IntArray(
                    uuid.chunks_exact(4)
                        .map(|v| i32::from_be_bytes(v.try_into().unwrap()))
                        .collect(),
                ),
            ),
            (
                "Pos",
                Tag::List(
                    6,
                    [p.0, p.1, p.2]
                        .map(|v| Tag::Double((v as f64 + 0.5).to_bits()))
                        .to_vec(),
                ),
            ),
        ] {
            data.insert(NbtString::text(k), v);
        }
        Self {
            eid,
            data: Tag::Compound(data),
        }
    }
    pub fn uuid(&self) -> Result<[u8; 16], &'static str> {
        let Some(Tag::IntArray(v)) = self.data.get("UUID") else {
            return Err("cart UUID missing");
        };
        if v.len() != 4 {
            return Err("invalid cart UUID");
        }
        let mut bytes = [0; 16];
        for (i, v) in v.iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        Ok(bytes)
    }
    pub fn pos(&self) -> Result<[f64; 3], &'static str> {
        let Some(Tag::List(6, values)) = self.data.get("Pos") else {
            return Err("cart position missing");
        };
        if values.len() != 3 {
            return Err("invalid cart position");
        }
        let mut pos = [0.; 3];
        for (i, v) in values.iter().enumerate() {
            let Tag::Double(v) = v else {
                return Err("invalid cart coordinate");
            };
            pos[i] = f64::from_bits(*v);
            if !pos[i].is_finite() {
                return Err("nonfinite cart coordinate");
            }
        }
        Ok(pos)
    }
    pub fn set_pos(&mut self, p: [f64; 3]) {
        if let Tag::Compound(fields) = &mut self.data {
            fields.insert(
                NbtString::text("Pos"),
                Tag::List(6, p.into_iter().map(|v| Tag::Double(v.to_bits())).collect()),
            );
        }
    }
    pub fn motion(&self) -> [f64; 3] {
        let Some(Tag::List(6, values)) = self.data.get("Motion") else {
            return [0.0; 3];
        };
        if values.len() != 3 {
            return [0.0; 3];
        }
        let mut out = [0.0; 3];
        for (i, v) in values.iter().enumerate() {
            if let Tag::Double(bits) = v {
                let x = f64::from_bits(*bits);
                if x.is_finite() {
                    out[i] = x;
                }
            }
        }
        out
    }
    pub fn set_motion(&mut self, m: [f64; 3]) {
        if let Tag::Compound(fields) = &mut self.data {
            fields.insert(
                NbtString::text("Motion"),
                Tag::List(6, m.into_iter().map(|v| Tag::Double(v.to_bits())).collect()),
            );
        }
    }
    pub fn on_ground(&self) -> bool {
        matches!(self.data.get("OnGround"), Some(Tag::Byte(b)) if *b != 0)
    }
    pub fn set_on_ground(&mut self, ground: bool) {
        if let Tag::Compound(fields) = &mut self.data {
            fields.insert(NbtString::text("OnGround"), Tag::Byte(i8::from(ground)));
        }
    }
    pub fn has_loot_table(&self) -> bool {
        self.data.get("LootTable").is_some()
    }
    pub fn take_loot(&mut self) -> Option<(String, i64)> {
        let Tag::Compound(fields) = &mut self.data else {
            return None;
        };
        let table = match fields.remove(&NbtString::text("LootTable"))? {
            Tag::String(s) => s.to_utf8().ok()?,
            _ => return None,
        };
        let seed = match fields.remove(&NbtString::text("LootTableSeed")) {
            Some(Tag::Long(s)) => s,
            _ => 0,
        };
        Some((table, seed))
    }
}
/// A generated hanging entity (`minecraft:item_frame` / `minecraft:glow_item_frame`).
///
/// Same shape as `Cart`: the native save compound plus a runtime entity id. The
/// fields kept are the ones the save and the client need — `Facing`
/// (`HangingEntity.addAdditionalSaveData`, a `Direction.get3DDataValue()` byte),
/// the framed `Item` stack and `ItemRotation` (`ItemFrame.addAdditionalSaveData`).
/// The entity's own position is derived: a hanging entity hangs *in* the block its
/// `Pos` names, so the block center is stored and the facing decides which face.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Frame {
    pub eid: i32,
    pub data: Tag,
}
impl Frame {
    pub fn new(
        eid: i32,
        uuid: [u8; 16],
        p: Pos,
        glow: bool,
        facing: u8,
        item: Option<(&str, u8)>,
    ) -> Self {
        let mut values = vec![
            (
                "id",
                string(if glow {
                    "minecraft:glow_item_frame"
                } else {
                    "minecraft:item_frame"
                }),
            ),
            (
                "UUID",
                Tag::IntArray(
                    uuid.chunks_exact(4)
                        .map(|v| i32::from_be_bytes(v.try_into().unwrap()))
                        .collect(),
                ),
            ),
            (
                "Pos",
                Tag::List(
                    6,
                    [p.0, p.1, p.2]
                        .map(|v| Tag::Double((v as f64 + 0.5).to_bits()))
                        .to_vec(),
                ),
            ),
            ("Facing", Tag::Byte(facing as i8)),
            ("ItemRotation", Tag::Byte(0)),
        ];
        if let Some((name, count)) = item {
            values.push((
                "Item",
                compound([("id", string(name)), ("count", Tag::Int(i32::from(count)))]),
            ));
        }
        Self {
            eid,
            data: compound(values),
        }
    }
    pub fn uuid(&self) -> Result<[u8; 16], &'static str> {
        let Some(Tag::IntArray(v)) = self.data.get("UUID") else {
            return Err("frame UUID missing");
        };
        if v.len() != 4 {
            return Err("invalid frame UUID");
        }
        let mut bytes = [0u8; 16];
        for (i, v) in v.iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        Ok(bytes)
    }
    pub fn pos(&self) -> Result<[f64; 3], &'static str> {
        let Some(Tag::List(6, values)) = self.data.get("Pos") else {
            return Err("frame position missing");
        };
        if values.len() != 3 {
            return Err("invalid frame position");
        }
        let mut pos = [0.; 3];
        for (i, v) in values.iter().enumerate() {
            let Tag::Double(v) = v else {
                return Err("invalid frame coordinate");
            };
            pos[i] = f64::from_bits(*v);
            if !pos[i].is_finite() {
                return Err("nonfinite frame coordinate");
            }
        }
        Ok(pos)
    }
    /// `HangingEntity`'s direction as `Direction.get3DDataValue` writes it.
    pub fn facing(&self) -> Result<u8, &'static str> {
        match self.data.get("Facing") {
            Some(Tag::Byte(v)) if (0..=5).contains(v) => Ok(*v as u8),
            _ => Err("invalid frame facing"),
        }
    }
    pub fn glow(&self) -> bool {
        is_id(&self.data, "minecraft:glow_item_frame")
    }
    /// The framed stack's `id` and `count`, as `ItemFrame.addAdditionalSaveData`
    /// writes them. `None` is an empty frame.
    pub fn item(&self) -> Option<(String, u8)> {
        let stack = self.data.get("Item")?;
        let id = stack.get("id").and_then(Tag::string)?.to_utf8().ok()?;
        let count = stack.get("count").and_then(Tag::int).unwrap_or(1);
        let count = u8::try_from(count).ok()?;
        (count > 0).then_some((id, count))
    }
    pub fn rotation(&self) -> i32 {
        match self.data.get("ItemRotation") {
            Some(Tag::Byte(v)) => i32::from(*v),
            _ => 0,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Content {
    pub spawners: BTreeMap<Pos, Tag>,
    pub carts: Vec<Cart>,
    pub frames: Vec<Frame>,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Snapshot {
    pub content: Content,
    pub sources: HashSet<(i32, i32)>,
    pub spider_flags: HashSet<SpiderFlag>,
}
fn position(tag: &Tag) -> Result<Pos, &'static str> {
    Ok((
        tag.get("x").and_then(Tag::int).ok_or("spawner x missing")?,
        tag.get("y").and_then(Tag::int).ok_or("spawner y missing")?,
        tag.get("z").and_then(Tag::int).ok_or("spawner z missing")?,
    ))
}
fn is_id(tag: &Tag, expected: &str) -> bool {
    tag.get("id").and_then(Tag::string) == Some(&NbtString::text(expected))
}
impl Snapshot {
    pub fn append(&self, out: &mut Vec<u8>) {
        let mut sources: Vec<_> = self
            .sources
            .iter()
            .map(|&(x, z)| ((x as u32 as u64) | ((z as u32 as u64) << 32)) as i64)
            .collect();
        sources.sort_unstable();
        let mut flags: Vec<_> = self.spider_flags.iter().copied().collect();
        flags.sort_unstable();
        let root = compound([
            (
                "spawners",
                Tag::List(10, self.content.spawners.values().cloned().collect()),
            ),
            (
                "carts",
                Tag::List(
                    10,
                    self.content.carts.iter().map(|c| c.data.clone()).collect(),
                ),
            ),
            (
                "frames",
                Tag::List(
                    10,
                    self.content.frames.iter().map(|f| f.data.clone()).collect(),
                ),
            ),
            ("sources", Tag::LongArray(sources)),
            (
                "spider_flags",
                Tag::List(
                    11,
                    flags
                        .into_iter()
                        .map(|(x, z, i)| {
                            Tag::IntArray(vec![
                                x,
                                z,
                                i32::try_from(i).expect("piece index fits native int"),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]);
        let data = nbt::write_named(&NbtString::text(""), &root).expect("owned generated NBT");
        out.extend_from_slice(&data);
        out.extend_from_slice(&(data.len() as u64).to_be_bytes());
        out.extend_from_slice(b"GEN1");
    }
    /// Called on the prefix preceding TIK1; decode/validate before mutating any world owner.
    pub fn read(payload: &[u8]) -> Result<Option<Self>, &'static str> {
        if !payload.ends_with(b"GEN1") {
            return Ok(None);
        }
        let end = payload
            .len()
            .checked_sub(12)
            .ok_or("truncated generated content footer")?;
        let size = usize::try_from(u64::from_be_bytes(
            payload[end..end + 8].try_into().unwrap(),
        ))
        .map_err(|_| "generated content size overflow")?;
        let start = end
            .checked_sub(size)
            .ok_or("invalid generated content size")?;
        let (_, root) = nbt::read_named(&payload[start..end])?;
        let mut out = Self::default();
        let list = |key| match root.get(key) {
            Some(Tag::List(_, v)) => Ok(v),
            _ => Err("generated content list missing"),
        };
        for tag in list("spawners")? {
            if !is_id(tag, "minecraft:mob_spawner") {
                return Err("invalid generated spawner id");
            }
            if out
                .content
                .spawners
                .insert(position(tag)?, tag.clone())
                .is_some()
            {
                return Err("duplicate generated spawner");
            }
        }
        let mut uuids = HashSet::new();
        for tag in list("carts")? {
            if !is_id(tag, "minecraft:chest_minecart") {
                return Err("invalid generated cart id");
            }
            let cart = Cart {
                eid: 0,
                data: tag.clone(),
            };
            cart.pos()?;
            if !uuids.insert(cart.uuid()?) {
                return Err("duplicate generated cart UUID");
            }
            out.content.carts.push(cart);
        }
        // A save written before hanging entities were materialized has no `frames`
        // list at all; an empty section is not a corrupt one.
        for tag in match root.get("frames") {
            Some(Tag::List(_, v)) => v.as_slice(),
            _ => &[],
        } {
            if !is_id(tag, "minecraft:item_frame") && !is_id(tag, "minecraft:glow_item_frame") {
                return Err("invalid generated frame id");
            }
            let frame = Frame {
                eid: 0,
                data: tag.clone(),
            };
            frame.pos()?;
            frame.facing()?;
            if !uuids.insert(frame.uuid()?) {
                return Err("duplicate generated frame UUID");
            }
            out.content.frames.push(frame);
        }
        let Some(Tag::LongArray(sources)) = root.get("sources") else {
            return Err("generated sources missing");
        };
        for &key in sources {
            if !out.sources.insert((key as i32, (key >> 32) as i32)) {
                return Err("duplicate generated source");
            }
        }
        for flag in list("spider_flags")? {
            let Tag::IntArray(v) = flag else {
                return Err("invalid spider flag");
            };
            if v.len() != 3 || v[2] < 0 || !out.spider_flags.insert((v[0], v[1], v[2] as usize)) {
                return Err("invalid or duplicate spider flag");
            }
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_spawner_and_cart_loot_nbt_are_typed_and_lossless() {
        let (_, expected) =
            nbt::read_named(include_bytes!("../tests/data/mineshaft_spawner_1211.nbt")).unwrap();
        assert_eq!(spawner_nbt((7, 24, -19)), expected);
        let (_, expected) = nbt::read_named(include_bytes!(
            "../tests/data/mineshaft_chest_minecart_1211.nbt"
        ))
        .unwrap();
        assert_eq!(cart_loot_nbt(-9223372036854775807), expected);
    }
    #[test]
    fn snapshot_preserves_mutated_nbt_identity_and_source_claims() {
        let mut state = Snapshot::default();
        let mut tag = spawner_nbt((7, 24, -19));
        if let Tag::Compound(v) = &mut tag {
            v.insert(NbtString::text("Delay"), Tag::Short(137));
        }
        state.content.spawners.insert((7, 24, -19), tag);
        state
            .content
            .carts
            .push(Cart::new(0, [27; 16], (-7, -32, 40), i64::MIN));
        state.sources.insert((-13, 2));
        state.spider_flags.insert((-9, 4, 185));
        let mut bytes = vec![1, 2, 3];
        state.append(&mut bytes);
        assert_eq!(Snapshot::read(&bytes).unwrap().unwrap(), state);
        let footer = bytes.len() - 12;
        bytes[footer..][..8].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(Snapshot::read(&bytes).is_err());
    }
    #[test]
    fn spawner_nbt_for_produces_target_entity_id() {
        for (input, expected_id) in [
            ("minecraft:blaze", "minecraft:blaze"),
            ("minecraft:silverfish", "minecraft:silverfish"),
            ("blaze", "minecraft:blaze"),
            ("minecraft:cave_spider", "minecraft:cave_spider"),
        ] {
            let tag = spawner_nbt_for((1, 64, -2), input);
            assert_eq!(tag.get("id").and_then(Tag::string), Some(&NbtString::text("minecraft:mob_spawner")));
            assert_eq!(tag.get("x").and_then(Tag::int), Some(1));
            assert_eq!(tag.get("y").and_then(Tag::int), Some(64));
            assert_eq!(tag.get("z").and_then(Tag::int), Some(-2));
            let spawn_id = tag
                .get("SpawnData")
                .and_then(|d| d.get("entity"))
                .and_then(|e| e.get("id"))
                .and_then(Tag::string);
            assert_eq!(spawn_id, Some(&NbtString::text(expected_id)));
        }
    }
}
