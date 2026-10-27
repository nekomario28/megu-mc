//! StructureTemplate's typed load and block ordering. Placement/processor effects
//! are separate; loading a template does not place its cells into a world.
use crate::block_registry::BReg;
use crate::nbt::{NbtString, Tag};
use std::collections::BTreeMap;

type Pos = (i32, i32, i32);
#[derive(Clone, Debug)]
pub(crate) struct BlockInfo {
    pub pos: Pos,
    pub state: u32,
    pub nbt: Option<Tag>,
}
#[derive(Clone, Debug)]
pub(crate) struct EntityInfo {
    pub pos: [f64; 3],
    pub block_pos: Pos,
    pub nbt: Tag,
}
#[derive(Clone, Debug)]
pub(crate) struct Template {
    pub size: Pos,
    pub palettes: Vec<Vec<BlockInfo>>,
    pub entities: Vec<EntityInfo>,
}

fn integer_pos(value: &Tag) -> Result<Pos, &'static str> {
    let values = value
        .list()
        .ok_or("template position must be an int list")?;
    if values.len() != 3 {
        return Err("template position must have three coordinates");
    }
    Ok((
        values[0].int().ok_or("template coordinate must be int")?,
        values[1].int().ok_or("template coordinate must be int")?,
        values[2].int().ok_or("template coordinate must be int")?,
    ))
}
fn field<'a>(value: &'a Tag, key: &str) -> Result<&'a Tag, &'static str> {
    value.get(key).ok_or("missing template field")
}
fn list<'a>(value: &'a Tag, key: &str) -> Result<&'a [Tag], &'static str> {
    field(value, key)?
        .list()
        .ok_or("template field must be list")
}
fn state_from_tag(value: &Tag, reg: &BReg) -> Result<u32, &'static str> {
    let name = field(value, "Name")?
        .string()
        .ok_or("palette name must be string")?
        .to_utf8()
        .map_err(|_| "invalid palette name UTF-16")?;
    let mut properties = Vec::new();
    if let Some(value) = value.get("Properties") {
        let Tag::Compound(values) = value else {
            return Err("palette properties must be compound");
        };
        for (key, value) in values {
            properties.push((
                key.to_utf8().map_err(|_| "invalid property key")?,
                value
                    .string()
                    .ok_or("property value must be string")?
                    .to_utf8()
                    .map_err(|_| "invalid property value")?,
            ));
        }
    }
    reg.checked_state(&name, &properties)
        .ok_or("unknown palette block or property")
}

impl Template {
    /// Transformation-only preview. Processors and world placement side effects
    /// must be applied by the structure pipeline, not inferred from this list.
    pub(crate) fn transformed_blocks(
        &self,
        palette: usize,
        mirror: crate::structure_transform::Mirror,
        rotation: crate::structure_transform::Rotation,
        pivot: Pos,
        origin: Pos,
    ) -> Result<Vec<BlockInfo>, &'static str> {
        self.palettes
            .get(palette)
            .ok_or("palette outside template")?
            .iter()
            .map(|b| {
                let p = crate::structure_transform::block(b.pos, mirror, rotation, pivot);
                Ok(BlockInfo {
                    pos: (
                        p.0.wrapping_add(origin.0),
                        p.1.wrapping_add(origin.1),
                        p.2.wrapping_add(origin.2),
                    ),
                    state: crate::structure_transform::state(b.state, mirror, rotation)
                        .ok_or("unknown template state transform")?,
                    nbt: b.nbt.clone(),
                })
            })
            .collect()
    }
    pub(crate) fn load(root: &Tag, reg: &BReg) -> Result<Self, &'static str> {
        let size = integer_pos(field(root, "size")?)?;
        if size.0 < 0 || size.1 < 0 || size.2 < 0 {
            return Err("negative template size");
        }
        let raw_blocks = list(root, "blocks")?;
        let raw_palettes: Vec<&[Tag]> = if let Some(palettes) = root.get("palettes") {
            palettes
                .list()
                .ok_or("palettes must be list")?
                .iter()
                .map(|p| p.list().ok_or("palette must be list"))
                .collect::<Result<_, _>>()?
        } else {
            vec![list(root, "palette")?]
        };
        let mut palettes = Vec::new();
        for raw_palette in raw_palettes {
            let states = raw_palette
                .iter()
                .map(|p| state_from_tag(p, reg))
                .collect::<Result<Vec<_>, _>>()?;
            let mut blocks = Vec::new();
            for info in raw_blocks {
                let pos = integer_pos(field(info, "pos")?)?;
                let index = field(info, "state")?
                    .int()
                    .ok_or("palette index must be int")?;
                let state = *states
                    .get(usize::try_from(index).map_err(|_| "negative palette index")?)
                    .ok_or("palette index outside template palette")?;
                let nbt = match info.get("nbt") {
                    None => None,
                    Some(v @ Tag::Compound(_)) => Some(v.clone()),
                    _ => return Err("block entity data must be compound"),
                };
                let category = if nbt.is_some() {
                    2
                } else if reg
                    .is_template_solid(state)
                    .ok_or("missing native template-solid predicate")?
                {
                    0
                } else {
                    1
                };
                blocks.push((category, BlockInfo { pos, state, nbt }));
            }
            // Native addToLists/buildInfoList: full collision blocks first,
            // non-full next, then block entities; stable Y/X/Z order per group.
            blocks.sort_by_key(|(category, b)| (*category, b.pos.1, b.pos.0, b.pos.2));
            palettes.push(blocks.into_iter().map(|(_, block)| block).collect());
        }
        let mut entities = Vec::new();
        for info in list(root, "entities")? {
            let Some(nbt) = info.get("nbt") else {
                continue;
            };
            if !matches!(nbt, Tag::Compound(_)) {
                return Err("template entity NBT must be compound");
            }
            let raw = list(info, "pos")?;
            if raw.len() != 3 {
                return Err("entity position requires three doubles");
            }
            let mut pos = [0.0; 3];
            for (i, value) in raw.iter().enumerate() {
                let Tag::Double(bits) = value else {
                    return Err("entity position requires doubles");
                };
                pos[i] = f64::from_bits(*bits);
            }
            let block_pos = integer_pos(field(info, "blockPos")?)?;
            entities.push(EntityInfo {
                pos,
                block_pos,
                nbt: nbt.clone(),
            });
        }
        Ok(Self {
            size,
            palettes,
            entities,
        })
    }

    /// Exact canonical state/block-entity representation used by the native
    /// loader oracle. This covers every loaded palette's placement order.
    pub(crate) fn block_layout(&self, reg: &BReg) -> Result<Vec<u8>, &'static str> {
        fn state_tag(state: u32, reg: &BReg) -> Result<Tag, &'static str> {
            let mut out = BTreeMap::from([(
                NbtString::text("Name"),
                Tag::String(NbtString::text(reg.name_of(state))),
            )]);
            let properties = reg
                .properties_of(state)
                .ok_or("unknown state in template")?;
            if !properties.is_empty() {
                out.insert(
                    NbtString::text("Properties"),
                    Tag::Compound(
                        properties
                            .into_iter()
                            .map(|(k, v)| (NbtString::text(&k), Tag::String(NbtString::text(&v))))
                            .collect(),
                    ),
                );
            }
            Ok(Tag::Compound(out))
        }
        let mut out = Vec::new();
        out.extend_from_slice(&(self.palettes.len() as i32).to_be_bytes());
        for palette in &self.palettes {
            out.extend_from_slice(&(palette.len() as i32).to_be_bytes());
            for info in palette {
                for v in [info.pos.0, info.pos.1, info.pos.2] {
                    out.extend_from_slice(&v.to_be_bytes());
                }
                out.extend(crate::nbt::write_named(
                    &NbtString::text(""),
                    &state_tag(info.state, reg)?,
                )?);
                out.push(u8::from(info.nbt.is_some()));
                if let Some(nbt) = &info.nbt {
                    out.extend(crate::nbt::write_named(&NbtString::text(""), nbt)?);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation_math::sha256;
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|v| format!("{v:02x}")).collect()
    }
    #[test]
    fn every_pinned_template_matches_native_typed_nbt_and_loaded_order() {
        let reg = crate::block_registry::breg().unwrap();
        let mut count = 0;
        for row in include_str!("../tests/data/structure_templates_1211.tsv").lines() {
            let f: Vec<_> = row.split('\t').collect();
            assert_eq!(f.len(), 5);
            let path =
                std::path::Path::new("datapacks/vanilla/data/minecraft/structure").join(f[0]);
            let bytes = std::fs::read(&path).unwrap();
            let (name, root) = crate::nbt::read_gzip(&bytes, 16 * 1024 * 1024).unwrap();
            let canonical = crate::nbt::write_named(&name, &root).unwrap();
            assert_eq!(hex(&sha256(&canonical)), f[1], "native typed NBT: {}", f[0]);
            let template = Template::load(&root, reg).unwrap_or_else(|e| panic!("{}: {e}", f[0]));
            assert_eq!(
                template.palettes.len(),
                f[2].parse::<usize>().unwrap(),
                "{}",
                f[0]
            );
            assert_eq!(
                template.entities.len(),
                f[3].parse::<usize>().unwrap(),
                "{}",
                f[0]
            );
            assert_eq!(
                hex(&sha256(&template.block_layout(reg).unwrap())),
                f[4],
                "native template load: {}",
                f[0]
            );
            count += 1;
        }
        assert_eq!(count, 1180);
    }
}
