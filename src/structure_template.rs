//! StructureTemplate load, processing and bounded full-cube terrain placement.
//! Load/processing remain read-only; solid placement writes through the generation context.
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

/// Coordinates are transformed before processing, states only during placement.
#[derive(Clone, Copy)]
pub(crate) struct Placement {
    pub origin: Pos,
    pub pivot: Pos,
    pub mirror: crate::structure_transform::Mirror,
    pub rotation: crate::structure_transform::Rotation,
}

fn state_from_json(value: &crate::json::J, reg: &BReg) -> Result<u32, &'static str> {
    use crate::json::J;
    let Some(J::Str(name)) = value.get("Name") else {
        return Err("missing template state name");
    };
    let mut properties = Vec::new();
    if let Some(props) = value.get("Properties") {
        let J::Obj(props) = props else {
            return Err("template state properties must be an object");
        };
        for (key, val) in props {
            let J::Str(val) = val else {
                return Err("template state property must be a string");
            };
            properties.push((key.clone(), val.clone()));
        }
    }
    reg.checked_state(name, &properties)
        .ok_or("unknown template state")
}

#[derive(Clone, Debug)]
enum RuleTest {
    Always,
    Block(String),
}
impl RuleTest {
    fn parse(value: &crate::json::J, reg: &BReg) -> Result<Self, &'static str> {
        match value.get("predicate_type").map(crate::json::J::str) {
            Some("minecraft:always_true") => Ok(Self::Always),
            Some("minecraft:block_match") => {
                let name = value.get("block").ok_or("missing rule block")?.str();
                let state = reg.checked_state(name, &[]).ok_or("unknown rule block")?;
                Ok(Self::Block(reg.name_of(state).to_owned()))
            }
            _ => Err("unsupported template rule test"),
        }
    }
    fn matches(&self, state: u32, reg: &BReg) -> bool {
        match self {
            Self::Always => true,
            Self::Block(name) => reg.name_of(state) == name,
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct BlockRule {
    input: RuleTest,
    location: RuleTest,
    output: u32,
}

/// Supported processor shapes, not a claim that every native processor is ported.
#[derive(Clone, Debug)]
pub(crate) enum Processor {
    BlockRot {
        integrity: f32,
        rottable: Option<Vec<String>>,
    },
    Protected(String),
    Rules(Vec<BlockRule>),
}
impl Processor {
    /// Reject an unsupported required child rather than shorten a processor list.
    pub(crate) fn list(value: &crate::json::J, reg: &BReg) -> Result<Vec<Self>, &'static str> {
        let processors = value
            .get("processors")
            .and_then(crate::json::J::as_arr)
            .ok_or("missing template processor list")?;
        processors.iter().map(|p| Self::parse(p, reg)).collect()
    }
    fn parse(value: &crate::json::J, reg: &BReg) -> Result<Self, &'static str> {
        use crate::json::J;
        match value.get("processor_type").map(J::str) {
            Some("minecraft:block_rot") => {
                let Some(J::Num(integrity)) = value.get("integrity") else {
                    return Err("missing block rot integrity");
                };
                let integrity = *integrity as f32;
                // Native floatRange compares boxed floats: -0.0 sorts below +0.0.
                if !integrity.is_finite() || integrity.is_sign_negative() || integrity > 1.0 {
                    return Err("block rot integrity outside 0..=1");
                }
                let rottable = match value.get("rottable_blocks") {
                    None => None,
                    Some(J::Arr(values)) => Some(
                        values
                            .iter()
                            .map(|v| {
                                let J::Str(name) = v else {
                                    return Err("rottable block must be an identity");
                                };
                                let state = reg
                                    .checked_state(name, &[])
                                    .ok_or("unknown rottable block")?;
                                Ok(reg.name_of(state).to_owned())
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                    _ => return Err("unsupported rottable holder set (explicit block lists only)"),
                };
                Ok(Self::BlockRot {
                    integrity,
                    rottable,
                })
            }
            Some("minecraft:protected_blocks") => {
                let Some(J::Str(name)) = value.get("value") else {
                    return Err("missing protected block identity");
                };
                let tag = name
                    .strip_prefix('#')
                    .filter(|tag| !tag.is_empty())
                    .ok_or("protected blocks requires a tag identity")?;
                let tag = crate::block_registry::ns_name(tag);
                let (namespace, path) = tag
                    .split_once(':')
                    .ok_or("invalid protected tag identity")?;
                if namespace.is_empty()
                    || !namespace.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b)
                    })
                    || !path.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b"/_.-".contains(&b)
                    })
                {
                    return Err("invalid protected tag identity");
                }
                Ok(Self::Protected(tag))
            }
            Some("minecraft:rule") => {
                let rules = value
                    .get("rules")
                    .and_then(J::as_arr)
                    .ok_or("missing template rules")?;
                Ok(Self::Rules(
                    rules
                        .iter()
                        .map(|r| {
                            // More general position tests and NBT modifiers need their own native cases.
                            if r.get("position_predicate").is_some()
                                || r.get("block_entity_modifier").is_some()
                            {
                                return Err(
                                    "unsupported template rule position test or NBT modifier",
                                );
                            }
                            let output =
                                r.get("output_state").ok_or("missing rule output state")?;
                            Ok(BlockRule {
                                input: RuleTest::parse(
                                    r.get("input_predicate").ok_or("missing rule input test")?,
                                    reg,
                                )?,
                                location: RuleTest::parse(
                                    r.get("location_predicate")
                                        .ok_or("missing rule location test")?,
                                    reg,
                                )?,
                                output: state_from_json(output, reg)?,
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                ))
            }
            _ => Err("unsupported template processor type"),
        }
    }
}

impl Template {
    /// Native processBlockInfos stage: no writes, clipping, state rotation,
    /// palette selection, entities, fluids, or shape updates. The caller supplies
    /// settings.getRandom(current_world_pos).nextFloat, preserving shared versus
    /// position-seeded RNG ownership. Rules here have deterministic tests only.
    pub(crate) fn process_blocks(
        &self,
        palette: usize,
        placement: Placement,
        processors: &[Processor],
        reg: &BReg,
        mut world: impl FnMut(Pos) -> u32,
        mut draw: impl FnMut(Pos) -> f32,
    ) -> Result<Vec<BlockInfo>, &'static str> {
        let mut result = Vec::new();
        for original in self
            .palettes
            .get(palette)
            .ok_or("palette outside template")?
        {
            let p = crate::structure_transform::block(
                original.pos,
                placement.mirror,
                placement.rotation,
                placement.pivot,
            );
            let mut current = original.clone();
            current.pos = (
                p.0.wrapping_add(placement.origin.0),
                p.1.wrapping_add(placement.origin.1),
                p.2.wrapping_add(placement.origin.2),
            );
            let mut keep = true;
            for processor in processors {
                match processor {
                    Processor::BlockRot {
                        integrity,
                        rottable,
                    } => {
                        // Membership uses the source snapshot, not preceding rule output.
                        if rottable.as_ref().is_none_or(|names| {
                            names.iter().any(|n| n == reg.name_of(original.state))
                        }) && draw(current.pos) > *integrity
                        {
                            keep = false;
                        }
                    }
                    Processor::Protected(tag) => {
                        if reg.in_tag(world(current.pos), tag) {
                            keep = false;
                        }
                    }
                    Processor::Rules(rules) => {
                        let existing = world(current.pos);
                        if let Some(rule) = rules.iter().find(|r| {
                            r.input.matches(current.state, reg) && r.location.matches(existing, reg)
                        }) {
                            current.state = rule.output;
                        }
                    }
                }
                if !keep {
                    break;
                }
            }
            if keep {
                result.push(current);
            }
        }
        Ok(result)
    }

    /// The one template NBT contract this crate accepts today: a container's `Items`.
    ///
    /// Native loads the tag through the live block entity with the level's registry
    /// access, and a container that also holds a `LootTable` saves that table *instead
    /// of* its items (`tests/data/template_init_1211.tsv` has both cases). Anything
    /// else is refused rather than stored and silently shadowed later.
    pub(crate) fn container_items(tag: &Tag) -> Result<Vec<(i16, i64, u8)>, &'static str> {
        let Tag::Compound(values) = tag else {
            return Err("template block NBT must be a compound");
        };
        for key in values.keys() {
            let key = key.to_utf8().map_err(|_| "template NBT key is not UTF-8")?;
            if key != "Items" {
                return Err("unsupported template block NBT: only container Items are handled");
            }
        }
        let Some(Tag::List(_, entries)) = tag.get("Items") else {
            return Err("template container Items must be a list");
        };
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let Tag::Compound(_) = entry else {
                return Err("template container item must be a compound");
            };
            let slot = match entry.get("Slot") {
                Some(Tag::Byte(v)) => *v as i16,
                _ => return Err("template container item needs a byte Slot"),
            };
            if !(0..=255).contains(&slot) {
                return Err("template container slot is out of range");
            }
            let count = match entry.get("count") {
                Some(Tag::Int(v)) => *v,
                _ => return Err("template container item needs an int count"),
            };
            if !(1..=64).contains(&count) {
                return Err("template container count is out of range");
            }
            let id = match entry.get("id") {
                Some(Tag::String(v)) => v.to_utf8().map_err(|_| "item id is not UTF-8")?,
                _ => return Err("template container item needs a string id"),
            };
            let item = crate::name_to_item(&id).ok_or("unknown item id in template NBT")?;
            out.push((slot, item, count as u8));
        }
        Ok(out)
    }

    /// A sign side's four lines, decoded from the game's own message form.
    ///
    /// Native stores each message as a JSON string component (`'"Hi"'`); older or edited
    /// data may carry a text object. Anything else is refused rather than shown blank.
    pub(crate) fn sign_lines(tag: &Tag) -> Result<[String; 4], &'static str> {
        let Tag::Compound(_) = tag else {
            return Err("template block NBT must be a compound");
        };
        let front = tag
            .get("front_text")
            .ok_or("a sign template needs front_text")?;
        let Tag::Compound(_) = front else {
            return Err("front_text must be a compound");
        };
        let Some(Tag::List(_, messages)) = front.get("messages") else {
            return Err("front_text.messages must be a list");
        };
        if messages.len() != 4 {
            return Err("a sign side has exactly four messages");
        }
        let mut lines: [String; 4] = Default::default();
        for (slot, message) in messages.iter().enumerate() {
            let Tag::String(text) = message else {
                return Err("a sign message must be a string component");
            };
            let raw = text.to_utf8().map_err(|_| "sign message is not UTF-8")?;
            let parsed = crate::json::jparse(raw.as_bytes()).ok_or("sign message is not JSON")?;
            lines[slot] = match &parsed {
                crate::json::J::Str(s) => s.clone(),
                crate::json::J::Obj(_) => match parsed.get("text") {
                    Some(crate::json::J::Str(s)) => s.clone(),
                    _ => return Err("unsupported sign component"),
                },
                _ => return Err("unsupported sign component"),
            };
        }
        Ok(lines)
    }

    /// Which of the two supported block-entity payloads a tag carries.
    ///
    /// `Items` and `front_text` do not share a cell in native data: a container stores its
    /// table *or* its items, and a sign stores a side. A tag with neither is refused.
    pub(crate) fn staged_payload(
        tag: &Tag,
        state: u32,
        reg: &BReg,
    ) -> Result<crate::generation_payload::GenerationPayload, &'static str> {
        use crate::generation_payload::{ContainerKind, GenerationPayload};
        if tag.get("front_text").is_some() {
            return Ok(GenerationPayload::Sign(Self::sign_lines(tag)?));
        }
        let kind = match reg.name_of(state) {
            "minecraft:chest" | "minecraft:trapped_chest" | "minecraft:barrel" => {
                ContainerKind::Chest
            }
            "minecraft:furnace" | "minecraft:blast_furnace" | "minecraft:smoker" => {
                ContainerKind::Furnace
            }
            other => {
                let _ = other;
                return Err("template container NBT on a block with no supported store");
            }
        };
        Ok(GenerationPayload::Container {
            kind,
            slots: Self::container_items(tag)?,
        })
    }

    /// The general template contract: no template entities yet, and NBT limited to
    /// a container's `Items` or a sign's `front_text`. Callers add their own restrictions on top (the fossil path
    /// requires full cubes), so admission is not baked into placement and one owner
    /// serves every caller.
    pub(crate) fn validate_placement(&self) -> Result<(), &'static str> {
        let reg = crate::block_registry::breg().ok_or("missing block registry")?;
        if !self.entities.is_empty() {
            return Err("template placement does not handle template entities");
        }
        for info in self.palettes.iter().flatten() {
            if let Some(tag) = &info.nbt {
                Self::staged_payload(tag, info.state, reg)?;
            }
        }
        Ok(())
    }

    /// The fossil palette contract: no entities/NBT, and only full-cube outputs.
    /// This is not the general template entity/fluid/shape-finalization pipeline.
    pub(crate) fn validate_solid_placement(
        &self,
        processors: &[Processor],
        reg: &BReg,
    ) -> Result<(), &'static str> {
        if !self.entities.is_empty()
            || self
                .palettes
                .iter()
                .flatten()
                .any(|b| b.nbt.is_some() || reg.is_template_solid(b.state) != Some(true))
        {
            return Err("solid template placement requires full cubes without entities or NBT");
        }
        for processor in processors {
            if let Processor::Rules(rules) = processor {
                if rules
                    .iter()
                    .any(|r| reg.is_template_solid(r.output) != Some(true))
                {
                    return Err("solid template rule has a non-full output");
                }
            }
        }
        Ok(())
    }

    /// The fossil entry point. It keeps the full-cube/no-NBT admission that
    /// `validate_solid_placement` states, then runs the shared placement owner, so
    /// loosening placement for other callers cannot loosen the fossil contract.
    pub(crate) fn place_solid_blocks(
        &self,
        ctx: &mut crate::generation_context::FC,
        rng: &mut crate::generation_math::Xor,
        placement: Placement,
        processors: &[Processor],
        clip: [i32; 6],
    ) -> Result<bool, &'static str> {
        let reg = crate::block_registry::breg().ok_or("missing block registry")?;
        self.validate_solid_placement(processors, reg)?;
        self.place_blocks(ctx, rng, placement, processors, clip)
    }

    /// Shared-RNG placement for any admitted template: processors, the coordinate
    /// transform, clipping after processing, writes, the bounded edge pass and container
    /// staging. A whole processor pass reads the world before this template's writes;
    /// the next template sees those writes. Clipping is AFTER processing, including
    /// draws for discarded coordinates.
    pub(crate) fn place_blocks(
        &self,
        ctx: &mut crate::generation_context::FC,
        rng: &mut crate::generation_math::Xor,
        placement: Placement,
        processors: &[Processor],
        clip: [i32; 6],
    ) -> Result<bool, &'static str> {
        if self.palettes.is_empty() {
            return Ok(false);
        }
        let palette = rng.next_bound(self.palettes.len() as i32) as usize;
        if self.palettes[palette].is_empty()
            || self.size.0 < 1
            || self.size.1 < 1
            || self.size.2 < 1
        {
            return Ok(false);
        }
        let reg = crate::block_registry::breg().ok_or("missing block registry")?;
        let blocks = self.process_blocks(
            palette,
            placement,
            processors,
            reg,
            |p| ctx.get(p.0, p.1, p.2),
            |_| rng.next_f32(),
        )?;
        let mut written = Vec::new();
        for block in blocks {
            let p = block.pos;
            if p.0 < clip[0]
                || p.1 < clip[1]
                || p.2 < clip[2]
                || p.0 > clip[3]
                || p.1 > clip[4]
                || p.2 > clip[5]
            {
                continue;
            }
            let state = crate::structure_transform::state(
                block.state,
                placement.mirror,
                placement.rotation,
            )
            .ok_or("unknown template state transform")?;
            if p.1 >= ctx.g.min_y && p.1 < ctx.g.min_y + ctx.g.height {
                ctx.set(p.0, p.1, p.2, state);
                written.push(p);
                if let Some(tag) = &block.nbt {
                    // Admission checked this tag; the write itself still fails closed
                    // rather than staging data the caller never validated.
                    ctx.payloads
                        .push((p, Self::staged_payload(tag, state, reg)?));
                }
            }
        }
        // Native updateShapeAtEdge visits exposed voxel faces after ALL writes.
        // Reuse the native-tested tree leaf scheduling owner and the same face
        // visitor as the existing fluid proto-tick path. A bounded positive
        // state transition is supported for an existing fence next to a newly
        // placed ordinary full cube. Other block-specific shape updates remain
        // outside this fossil-only placement contract.
        for (direction, p, q) in crate::voxel_faces::exposed_faces(written) {
            let inside = ctx.get(p.0, p.1, p.2);
            let outside = ctx.get(q.0, q.1, q.2);
            crate::generation_context::schedule_leaf_edge_state(ctx, reg, p, inside, outside);
            crate::generation_context::schedule_leaf_edge_state(ctx, reg, q, outside, inside);
            // Full-cube positive connections have one registry-owned contract.
            // The wall case reads the cell above; stair shape uses a live
            // read-through view of the just-placed template, not old terrain.
            let updated = reg.fence_after_full_neighbor(outside, inside, direction ^ 1)
                .or_else(|| reg.pane_after_full_neighbor(outside, inside, direction ^ 1))
                .or_else(|| {
                    if reg.name_of(outside).ends_with("_wall") {
                        let above = ctx.get(q.0, q.1.wrapping_add(1), q.2);
                        reg.wall_after_full_neighbor(outside, inside, above, direction ^ 1)
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    if reg.name_of(outside).ends_with("_stairs") {
                        reg.stair_after_neighbors(outside, q, |p| ctx.get(p.0, p.1, p.2))
                    } else {
                        None
                    }
                });
            if let Some(state) = updated {
                ctx.set(q.0, q.1, q.2, state);
            }
            // A waterlogged pane/stair/fence reports the same source
            // FluidState as a water block. Native ProtoChunkTicks preserves
            // identity/position, not the scheduled delay; unpack happens on
            // chunk activation. This is NOT the 5-GT live-world update path.
            match reg.fluid_name(outside) {
                "minecraft:water" => ctx.g.schedule_generated_fluid(q, 0),
                "minecraft:lava" => ctx.g.schedule_generated_fluid(q, 2),
                _ => {}
            }
        }
        Ok(true)
    }

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
    fn fossil_template_admission_rejects_unhandled_data() {
        let reg = crate::block_registry::breg().unwrap();
        let bone = reg.state_of("minecraft:bone_block", None);
        for kind in 0..4 {
            let mut template = Template {
                size: (1, 1, 1),
                palettes: vec![vec![BlockInfo {
                    pos: (0, 0, 0),
                    state: bone,
                    nbt: None,
                }]],
                entities: vec![],
            };
            let mut processors = Vec::new();
            match kind {
                0=>template.palettes[0][0].nbt=Some(Tag::Compound(BTreeMap::new())),
                1=>template.entities.push(EntityInfo {pos:[0.0;3],block_pos:(0,0,0),nbt:Tag::Compound(BTreeMap::new())}),
                2=>template.palettes[0][0].state=reg.state_of("minecraft:torch",None),
                _=>processors=Processor::list(&crate::json::jparse(br#"{"processors":[{"processor_type":"minecraft:rule","rules":[{"input_predicate":{"predicate_type":"minecraft:always_true"},"location_predicate":{"predicate_type":"minecraft:always_true"},"output_state":{"Name":"minecraft:torch"}}]}]}"#).unwrap(),reg).unwrap(),
            }
            assert!(
                template.validate_solid_placement(&processors, reg).is_err(),
                "unhandled template input {kind} passed fossil admission"
            );
        }
    }

    /// The native fixture is the expectation. The same two-cell template (a chest with
    /// explicit contents and a stone block) placed at 10,80,10 with no rotation and with
    /// 90 degrees must write the same cells, the same rotated state and the same staged
    /// contents as the captured native rows. Reading the fixture instead of restating it
    /// is the point: drift fails against what the game actually did.
    #[test]
    fn template_container_items_match_native_placement() {
        let reg = crate::block_registry::breg().unwrap();
        let chest = reg.state_of("minecraft:chest", None);
        let stone = reg.state_of("minecraft:stone", None);
        let mut items = std::collections::BTreeMap::new();
        items.insert(
            NbtString::text("Items"),
            Tag::List(
                10,
                vec![
                    container_entry(0, "minecraft:redstone", 3),
                    container_entry(2, "minecraft:diamond", 1),
                ],
            ),
        );
        let template = Template {
            // Same two cells as the fixture rows compared below: the items chest at the
            // template origin and the stone block at (1,0,1).
            size: (2, 1, 2),
            palettes: vec![vec![
                BlockInfo { pos: (0, 0, 0), state: chest, nbt: Some(Tag::Compound(items)) },
                BlockInfo { pos: (1, 0, 1), state: stone, nbt: None },
            ]],
            entities: Vec::new(),
        };
        assert!(template.validate_placement().is_ok());

        for (case, rotation) in [
            ("none", crate::structure_transform::Rotation::None),
            ("clockwise_90", crate::structure_transform::Rotation::Clockwise90),
        ] {
            let rows: Vec<Vec<String>> = include_str!("../tests/data/template_init_1211.tsv")
                .lines()
                .map(|line| line.split('\t').map(str::to_owned).collect())
                .filter(|f: &Vec<String>| f[0] == case)
                // This template is the items chest and the stone block. The fixture's other
                // rows are the loot-table chest (refused) and the sign (not staged yet).
                .filter(|f: &Vec<String>| f[4] == "absent" || f[5] != "-")
                .collect();
            let cells: Vec<(i32, i32, i32)> = rows.iter().map(|f| parse_cell(&f[2])).collect();
            let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
            let mut ctx = crate::generation_context::FC::new(&mut g);
            ctx.test_no_terrain = true;
            let air = reg.state_of("minecraft:air", None);
            // The bounded edge pass reads the six neighbours of every written cell, and
            // a scripted world panics rather than inventing terrain for an unprovided one.
            for cell in &cells {
                ctx.test_base_cell(*cell, air);
                for (dx, dy, dz) in [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)] {
                    ctx.test_base_cell((cell.0 + dx, cell.1 + dy, cell.2 + dz), air);
                }
            }
            let mut rng = crate::generation_math::Xor::seed_worldgen(7);
            let placement = Placement {
                origin: (10, 80, 10),
                pivot: (0, 0, 0),
                mirror: crate::structure_transform::Mirror::None,
                rotation,
            };
            let clip = [0, 0, 0, 31, 255, 31];
            assert!(template
                .place_blocks(&mut ctx, &mut rng, placement, &[], clip)
                .is_ok());

            for field in &rows {
                let cell = parse_cell(&field[2]);
                let placed = ctx.get(cell.0, cell.1, cell.2);
                // A state with no properties is written without brackets.
                let (name, props) = match field[3].split_once('[') {
                    Some((name, props)) => (name, props.trim_end_matches(']')),
                    None => (field[3].as_str(), ""),
                };
                assert_eq!(reg.name_of(placed), name, "{case} {} state", field[2]);
                for property in props.split(',').filter(|p| !p.is_empty()) {
                    let (key, value) = property.split_once('=').unwrap();
                    assert_eq!(
                        reg.get_prop(placed, key).as_deref(),
                        Some(value),
                        "{case} {} property {key}",
                        field[2]
                    );
                }
                let staged: Vec<_> = ctx
                    .payloads
                    .iter()
                    .filter(|(p, _)| *p == cell)
                    .filter_map(|(_, v)| match v {
                        crate::generation_payload::GenerationPayload::Container { slots, .. } => {
                            Some(slots.clone())
                        }
                        _ => None,
                    })
                    .next()
                    .unwrap_or_default();
                assert_eq!(
                    staged,
                    parse_native_contents(&field[5]),
                    "{case} {} contents",
                    field[2]
                );
            }
            assert_eq!(ctx.payloads.len(), 1, "{case}: one NBT cell stages contents");
        }
    }

    /// The handoff the fixture cannot show: staged contents must reach the one live
    /// container store, and survive a save/reload, or a placed chest is right in the
    /// generation layer and empty in game.
    #[test]
    fn staged_container_contents_reach_the_store_and_survive_a_save() {
        let reg = crate::block_registry::breg().unwrap();
        let chest = reg.state_of("minecraft:chest", None);
        let mut items = std::collections::BTreeMap::new();
        items.insert(
            NbtString::text("Items"),
            Tag::List(10, vec![container_entry(0, "minecraft:redstone", 3)]),
        );
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo { pos: (0, 0, 0), state: chest, nbt: Some(Tag::Compound(items)) }]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        {
            let mut ctx = crate::generation_context::FC::new(&mut g);
            ctx.test_no_terrain = true;
            let cell = (2, 70, 2);
            ctx.test_base_cell(cell, reg.state_of("minecraft:air", None));
            for (dx, dy, dz) in [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)] {
                ctx.test_base_cell((cell.0 + dx, cell.1 + dy, cell.2 + dz), reg.state_of("minecraft:air", None));
            }
            let mut rng = crate::generation_math::Xor::seed_worldgen(3);
            let placement = Placement {
                origin: cell,
                pivot: (0, 0, 0),
                mirror: crate::structure_transform::Mirror::None,
                rotation: crate::structure_transform::Rotation::None,
            };
            assert!(template
                .place_blocks(&mut ctx, &mut rng, placement, &[], [0, 0, 0, 31, 255, 31])
                .is_ok());
            assert_eq!(ctx.payloads.len(), 1, "placement stages the contents");
            let mut staged = std::mem::take(&mut ctx.payloads);
            g.stage_payloads((0, 0), &mut staged);
        }
        let staged = g.take_staged_payloads();
        assert_eq!(staged.len(), 1, "the generator holds the staged contents once");
        assert!(g.take_staged_payloads().is_empty(), "taking them twice yields nothing");
        crate::materialize_generation_payloads(staged);
        let redstone = crate::name_to_item("minecraft:redstone").unwrap() as i64;
        assert_eq!(
            crate::chests().lock().unwrap().get(&crate::ContainerKey::Block((2, 70, 2))).cloned(),
            Some([(0i16, (redstone, 3u8, Vec::new()))].into_iter().collect()),
            "the live store holds what the template staged"
        );
        // Save and reload: the contents must come back, not just the block.
        let mut buffer = Vec::new();
        crate::save_world_into(&mut buffer);
        crate::chests().lock().unwrap().clear();
        crate::load_world_from(&buffer).unwrap();
        assert_eq!(
            crate::chests().lock().unwrap().get(&crate::ContainerKey::Block((2, 70, 2))).cloned(),
            Some([(0i16, (redstone, 3u8, Vec::new()))].into_iter().collect()),
            "a saved container comes back with its contents"
        );
        crate::chests().lock().unwrap().remove(&crate::ContainerKey::Block((2, 70, 2)));
    }

    /// The window a player opens reads the same store the handoff writes, so the packet
    /// carries what the template staged. This is the claim "packet NBT" made concrete for
    /// a container; if a later change copies the contents somewhere else, this fails.
    #[test]
    fn a_placed_chest_shows_its_staged_contents_in_the_open_window() {
        use crate::effect_tests::client_pair;
        let reg = crate::block_registry::breg().unwrap();
        let redstone = crate::name_to_item("minecraft:redstone").unwrap();
        let pos = crate::ContainerKey::Block((2, 70, 2));
        crate::chests().lock().unwrap().insert(
            pos,
            [(0i16, (redstone, 3u8, Vec::new()))].into_iter().collect(),
        );
        let (client, mut receiver) = client_pair();
        let inv = client.inv.lock().unwrap().clone();
        crate::open_chest_window(
            &mut client.sock.lock().unwrap(),
            pos,
            None,
            &inv,
            "Chest",
        );
        // 0x33 opens the window, 0x13 carries the slots.
        let (id, _) = crate::read_packet(&mut receiver, std::time::Instant::now() + std::time::Duration::from_secs(2))
            .expect("the open-window packet");
        assert_eq!(id, 0x33, "an open-window packet precedes the items");
        let (id, payload) = crate::read_packet(&mut receiver, std::time::Instant::now() + std::time::Duration::from_secs(2))
            .expect("the window-items packet");
        assert_eq!(id, 0x13, "window items come after the screen");
        let mut at = 0usize;
        let mut varint = || {
            let mut value = 0u32;
            let mut shift = 0;
            loop {
                let byte = payload[at];
                at += 1;
                value |= ((byte & 0x7f) as u32) << shift;
                if byte & 0x80 == 0 {
                    return value;
                }
                shift += 7;
            }
        };
        assert_eq!(varint(), 1, "window id");
        let _state = varint();
        assert_eq!(varint(), 27 + 36, "chest slots plus the player inventory");
        assert_eq!(varint(), 3, "slot 0 holds the staged count");
        assert_eq!(varint(), redstone as u32, "slot 0 holds the staged item");
        assert_eq!(varint(), 0, "the staged stack carries no components");

        // The same window over an empty store: the packet follows the store, not a copy.
        crate::chests().lock().unwrap().remove(&pos);
        crate::open_chest_window(
            &mut client.sock.lock().unwrap(),
            pos,
            None,
            &inv,
            "Chest",
        );
        let _ = crate::read_packet(&mut receiver, std::time::Instant::now() + std::time::Duration::from_secs(2));
        let (id, payload) = crate::read_packet(&mut receiver, std::time::Instant::now() + std::time::Duration::from_secs(2))
            .expect("the second window-items packet");
        assert_eq!(id, 0x13);
        let mut at = 0usize;
        let mut varint = || {
            let mut value = 0u32;
            let mut shift = 0;
            loop {
                let byte = payload[at];
                at += 1;
                value |= ((byte & 0x7f) as u32) << shift;
                if byte & 0x80 == 0 {
                    return value;
                }
                shift += 7;
            }
        };
        let _ = varint();
        let _ = varint();
        assert_eq!(varint(), 27 + 36);
        assert_eq!(
            varint(),
            0,
            "an empty store must show an empty slot, so the window is not a stale copy"
        );
        let _ = reg;
    }

    /// The fixture's sign row, end to end: the template's `front_text` decodes to four
    /// lines, placement stages them, and the handoff writes them into the sign store so
    /// the chunk packet carries the text.
    #[test]
    fn template_sign_text_matches_native_and_reaches_the_sign_store() {
        use crate::generation_payload::GenerationPayload;
        let reg = crate::block_registry::breg().unwrap();
        let sign_state = reg.state_of("minecraft:oak_sign", None);
        let stone = reg.state_of("minecraft:stone", None);
        // The native fixture's raw form for the first line, so this test starts from the
        // captured shape rather than from what the port happens to emit.
        let rows: Vec<Vec<String>> = include_str!("../tests/data/template_init_1211.tsv")
            .lines()
            .map(|l| l.split('\t').map(str::to_owned).collect())
            .filter(|f: &Vec<String>| f[0] == "none" && f[6] != "-")
            .collect();
        assert_eq!(rows.len(), 1, "the fixture carries exactly one sign row");
        let native_text = rows[0][6].clone();
        let first_message = native_text.split('|').next().unwrap().to_string();
        let mut front = std::collections::BTreeMap::new();
        front.insert(
            NbtString::text("messages"),
            Tag::List(
                8,
                vec![
                    Tag::String(NbtString::text(&first_message)),
                    Tag::String(NbtString::text("\"\"")),
                    Tag::String(NbtString::text("\"\"")),
                    Tag::String(NbtString::text("\"\"")),
                ],
            ),
        );
        let mut tag = std::collections::BTreeMap::new();
        tag.insert(
            NbtString::text("front_text"),
            Tag::Compound(front),
        );
        let template = Template {
            size: (2, 2, 2),
            palettes: vec![vec![
                BlockInfo { pos: (1, 0, 1), state: stone, nbt: None },
                BlockInfo { pos: (1, 1, 1), state: sign_state, nbt: Some(Tag::Compound(tag)) },
            ]],
            entities: Vec::new(),
        };
        assert!(template.validate_placement().is_ok());

        let cell = parse_cell(&rows[0][2]);
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        {
            let mut ctx = crate::generation_context::FC::new(&mut g);
            ctx.test_no_terrain = true;
            let air = reg.state_of("minecraft:air", None);
            for base in [cell, (cell.0, cell.1 - 1, cell.2)] {
                ctx.test_base_cell(base, air);
                for (dx, dy, dz) in
                    [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)]
                {
                    ctx.test_base_cell((base.0 + dx, base.1 + dy, base.2 + dz), air);
                }
            }
            let mut rng = crate::generation_math::Xor::seed_worldgen(11);
            let placement = Placement {
                origin: (10, 80, 10),
                pivot: (0, 0, 0),
                mirror: crate::structure_transform::Mirror::None,
                rotation: crate::structure_transform::Rotation::None,
            };
            assert!(template
                .place_blocks(&mut ctx, &mut rng, placement, &[], [0, 0, 0, 31, 255, 31])
                .is_ok());
            let staged = ctx
                .payloads
                .iter()
                .find(|(p, _)| *p == cell)
                .map(|(_, v)| v.clone())
                .expect("the sign cell stages a payload");
            assert_eq!(
                staged,
                GenerationPayload::Sign(["Hi".into(), String::new(), String::new(), String::new()]),
                "the template's front_text decodes to four lines"
            );
            assert_eq!(
                reg.name_of(ctx.get(cell.0, cell.1, cell.2)),
                "minecraft:oak_sign"
            );
            let mut staged_all = std::mem::take(&mut ctx.payloads);
            g.stage_payloads((0, 0), &mut staged_all);
        }
        crate::materialize_generation_payloads(g.take_staged_payloads());
        assert_eq!(
            crate::signs().lock().unwrap().get(&(cell.0, cell.1, cell.2, 1)).cloned(),
            Some(["Hi".to_string(), String::new(), String::new(), String::new()]),
            "the sign store holds the front lines the template carried"
        );
        crate::signs().lock().unwrap().remove(&(cell.0, cell.1, cell.2, 1));
    }

    /// A furnace's `Items` belong in `furnaces()`, not `chests()`: the furnace window reads
    /// that store, so a misplaced staging would look like an empty furnace forever.
    #[test]
    fn template_furnace_contents_reach_the_furnace_store() {
        use crate::generation_payload::{ContainerKind, GenerationPayload};
        let reg = crate::block_registry::breg().unwrap();
        let furnace = reg.state_of("minecraft:furnace", None);
        let mut items = std::collections::BTreeMap::new();
        items.insert(
            NbtString::text("Items"),
            Tag::List(
                10,
                vec![
                    container_entry(0, "minecraft:iron_ore", 2),
                    container_entry(1, "minecraft:coal", 5),
                ],
            ),
        );
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo {
                pos: (0, 0, 0),
                state: furnace,
                nbt: Some(Tag::Compound(items)),
            }]],
            entities: Vec::new(),
        };
        assert!(template.validate_placement().is_ok());
        let cell = (3, 71, 3);
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        {
            let mut ctx = crate::generation_context::FC::new(&mut g);
            ctx.test_no_terrain = true;
            let air = reg.state_of("minecraft:air", None);
            ctx.test_base_cell(cell, air);
            for (dx, dy, dz) in [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)] {
                ctx.test_base_cell((cell.0 + dx, cell.1 + dy, cell.2 + dz), air);
            }
            let mut rng = crate::generation_math::Xor::seed_worldgen(13);
            let placement = Placement {
                origin: cell,
                pivot: (0, 0, 0),
                mirror: crate::structure_transform::Mirror::None,
                rotation: crate::structure_transform::Rotation::None,
            };
            assert!(template
                .place_blocks(&mut ctx, &mut rng, placement, &[], [0, 0, 0, 31, 255, 31])
                .is_ok());
            assert_eq!(
                ctx.payloads.first().map(|(_, v)| v.clone()),
                Some(GenerationPayload::Container {
                    kind: ContainerKind::Furnace,
                    slots: vec![
                        (0, crate::name_to_item("minecraft:iron_ore").unwrap(), 2),
                        (1, crate::name_to_item("minecraft:coal").unwrap(), 5),
                    ],
                }),
                "a furnace is staged with the furnace kind"
            );
            let mut staged = std::mem::take(&mut ctx.payloads);
            g.stage_payloads((0, 0), &mut staged);
        }
        crate::materialize_generation_payloads(g.take_staged_payloads());
        let ore = crate::name_to_item("minecraft:iron_ore").unwrap();
        let coal = crate::name_to_item("minecraft:coal").unwrap();
        {
            let store = crate::furnaces().lock().unwrap();
            let f = store.get(&cell).expect("the furnace store holds the placed furnace");
            assert_eq!(f.input, (ore, 2, Vec::new()), "slot 0 is the input");
            assert_eq!(f.fuel, (coal, 5, Vec::new()), "slot 1 is the fuel");
            assert_eq!(f.output, (0, 0, Vec::new()), "slot 2 is empty");
            assert!(f.dirty, "a placed furnace needs a resync");
        }
        assert!(
            crate::chests().lock().unwrap().get(&crate::ContainerKey::Block(cell)).is_none(),
            "a furnace must not be staged into the chest store"
        );
        crate::furnaces().lock().unwrap().remove(&cell);
    }

    /// A container the port has no store for is refused rather than staged where nothing
    /// would read it.
    #[test]
    fn unsupported_container_states_are_refused() {
        let reg = crate::block_registry::breg().unwrap();
        let dispenser = reg.state_of("minecraft:dispenser", None);
        let mut items = std::collections::BTreeMap::new();
        items.insert(
            NbtString::text("Items"),
            Tag::List(10, vec![container_entry(0, "minecraft:arrow", 1)]),
        );
        assert_eq!(
            Template::staged_payload(&Tag::Compound(items), dispenser, reg),
            Err("template container NBT on a block with no supported store")
        );
    }

    /// A sign tag with a shape the port cannot represent is refused, not blanked.
    #[test]
    fn unsupported_sign_components_are_refused() {
        let reg = crate::block_registry::breg().unwrap();
        let sign_state = reg.state_of("minecraft:oak_sign", None);
        let mut front = std::collections::BTreeMap::new();
        front.insert(
            NbtString::text("messages"),
            Tag::List(1, vec![Tag::Byte(1), Tag::Byte(1), Tag::Byte(1), Tag::Byte(1)]),
        );
        let mut tag = std::collections::BTreeMap::new();
        tag.insert(NbtString::text("front_text"), Tag::Compound(front));
        assert_eq!(
            Template::staged_payload(&Tag::Compound(tag), sign_state, reg),
            Err("a sign message must be a string component")
        );
    }

    /// The sign NBT the port emits must be the shape the game writes, or a client shows
    /// blank lines: `messages` are JSON string components and a `filtered_messages` list
    /// is present. The expectation is the encoding native produced in
    /// `sign-probe6.log`, kept here as the contract this owner has to match.
    #[test]
    fn sign_side_nbt_matches_the_shape_the_game_writes() {
        let lines = [
            "Hi".to_string(),
            String::new(),
            String::new(),
            String::new(),
        ];
        let mut out = Vec::new();
        crate::sign_side_nbt(&mut out, "front_text", &lines);
        // `sign_side_nbt` writes a named compound, so read it as one.
        let (name, tag) = crate::nbt::read_named(&out).expect("sign side parses");
        assert_eq!(name.to_utf8().unwrap(), "front_text");
        assert_eq!(tag.get("color").cloned(), Some(crate::nbt::Tag::String(crate::nbt::NbtString::text("white"))));
        assert_eq!(tag.get("has_glowing_text").cloned(), Some(crate::nbt::Tag::Byte(0)));
        let messages = match tag.get("messages") {
            Some(crate::nbt::Tag::List(kind, items)) => {
                assert_eq!(*kind, 8, "messages are a string list");
                items.clone()
            }
            other => panic!("messages must be a string list, got {other:?}"),
        };
        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[0],
            crate::nbt::Tag::String(crate::nbt::NbtString::text("\"Hi\"")),
            "a literal is a JSON string component, not a text object"
        );
        let filtered = match tag.get("filtered_messages") {
            Some(crate::nbt::Tag::List(_, items)) => items.clone(),
            other => panic!("filtered_messages must be present, got {other:?}"),
        };
        assert_eq!(filtered.len(), 4, "native always writes four filtered slots");
    }

    /// The fossil contract must not move: a chest is still refused there, even though the
    /// shared placement owner now handles one for other callers.
    #[test]
    fn the_fossil_contract_still_refuses_a_container() {
        let reg = crate::block_registry::breg().unwrap();
        let mut items = std::collections::BTreeMap::new();
        items.insert(NbtString::text("Items"), Tag::List(10, vec![container_entry(0, "minecraft:redstone", 3)]));
        let mut plain = std::collections::BTreeMap::new();
        plain.insert(
            NbtString::text("LootTable"),
            Tag::String(NbtString::text("minecraft:chests/simple_dungeon")),
        );
        let chest = reg.state_of("minecraft:chest", None);
        let with_items = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo { pos: (0, 0, 0), state: chest, nbt: Some(Tag::Compound(items)) }]],
            entities: Vec::new(),
        };
        assert_eq!(
            with_items.validate_placement(),
            Ok(()),
            "the general contract admits container contents"
        );
        assert_eq!(
            with_items.validate_solid_placement(&[], reg),
            Err("solid template placement requires full cubes without entities or NBT"),
            "the fossil contract still refuses a container"
        );
        let with_loot = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo { pos: (0, 0, 0), state: chest, nbt: Some(Tag::Compound(plain)) }]],
            entities: Vec::new(),
        };
        assert_eq!(
            with_loot.validate_placement(),
            Err("unsupported template block NBT: only container Items are handled"),
            "a loot table is refused rather than stored and shadowed"
        );
    }

    fn container_entry(slot: i8, id: &str, count: i32) -> Tag {
        let mut entry = std::collections::BTreeMap::new();
        entry.insert(NbtString::text("Slot"), Tag::Byte(slot));
        entry.insert(NbtString::text("id"), Tag::String(NbtString::text(id)));
        entry.insert(NbtString::text("count"), Tag::Int(count));
        Tag::Compound(entry)
    }

    fn parse_cell(text: &str) -> (i32, i32, i32) {
        let mut it = text.split(',');
        (
            it.next().unwrap().parse().unwrap(),
            it.next().unwrap().parse().unwrap(),
            it.next().unwrap().parse().unwrap(),
        )
    }

    /// `0=3 minecraft:redstone;2=1 minecraft:diamond` -> [(0, redstone, 3), (2, diamond, 1)]
    fn parse_native_contents(text: &str) -> Vec<(i16, i64, u8)> {
        if text == "-" {
            return Vec::new();
        }
        text.split(';')
            .map(|entry| {
                let (slot, stack) = entry.split_once('=').unwrap();
                let (count, id) = stack.split_once(' ').unwrap();
                (
                    slot.parse().unwrap(),
                    crate::name_to_item(id).unwrap(),
                    count.parse().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn fence_full_cube_edge_changes_only_the_facing_connection() {
        let reg = crate::block_registry::breg().unwrap();
        let bone = reg.state_of("minecraft:bone_block", None);
        let fence = reg.state_of("minecraft:oak_fence", None);
        assert_eq!(reg.is_template_solid(bone), Some(true));
        for (direction, property) in [
            (2, "north"),
            (3, "south"),
            (4, "west"),
            (5, "east"),
        ] {
            let updated = reg
                .fence_after_full_neighbor(fence, bone, direction)
                .expect("full bone-block face must connect an oak fence");
            assert_eq!(reg.get_prop(updated, property).as_deref(), Some("true"));
            for other in ["north", "south", "west", "east"] {
                if other != property {
                    assert_eq!(
                        reg.get_prop(updated, other),
                        reg.get_prop(fence, other),
                        "neighbor update changed the wrong fence side"
                    );
                }
            }
            assert!(
                reg.fence_after_full_neighbor(updated, bone, direction).is_none(),
                "already-connected fence must not be written again"
            );
        }
        assert!(reg.fence_after_full_neighbor(fence, bone, 0).is_none());
        assert!(reg.fence_after_full_neighbor(fence, bone, 1).is_none());
        assert!(reg.fence_after_full_neighbor(fence, reg.state_of("minecraft:air", None), 4).is_none());
        assert!(reg.fence_after_full_neighbor(bone, bone, 4).is_none());
    }

    #[test]
    fn solid_template_edges_update_fences_and_reserve_native_leaf_ticks() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let bone = reg.state_of("minecraft:bone_block", None);
        let fence = reg.state_of("minecraft:oak_fence", None);
        let leaves = reg.with_prop(
            reg.state_of("minecraft:oak_leaves", None),
            "distance",
            "7",
        );
        let air = reg.state_of("minecraft:air", None);
        let center = (8, 72, 8);
        let east = (9, 72, 8);
        let north = (8, 72, 7);
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo {
                pos: (0, 0, 0),
                state: bone,
                nbt: None,
            }]],
            entities: Vec::new(),
        };
        assert!(template.validate_solid_placement(&[], reg).is_ok());
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_writes = Some(Vec::new());
        for (pos, state) in [
            (center, air),
            ((7, 72, 8), air),
            (east, fence),
            (north, leaves),
            ((8, 72, 9), air),
            ((8, 71, 8), air),
            ((8, 73, 8), air),
        ] {
            ctx.test_base_cell(pos, state);
        }
        let mut rng = Xor::seed_worldgen(91);
        assert!(
            template
                .place_solid_blocks(
                    &mut ctx,
                    &mut rng,
                    Placement {
                        origin: center,
                        pivot: (0, 0, 0),
                        mirror: Mirror::None,
                        rotation: Rotation::None,
                    },
                    &[],
                    [8, 72, 8, 8, 72, 8],
                )
                .unwrap()
        );
        assert_eq!(ctx.get(center.0, center.1, center.2), bone);
        let updated_fence = ctx.get(east.0, east.1, east.2);
        assert_eq!(reg.get_prop(updated_fence, "west").as_deref(), Some("true"));
        assert_eq!(
            ctx.g.generated_leaf_ticks(),
            vec![(north, reg.state_of("minecraft:oak_leaves", None))],
            "template finalization must use tree's native tick owner"
        );
        assert!(ctx.g.generated_fluid_ticks().is_empty());
        let writes = ctx.test_writes.as_ref().unwrap();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0], (center, bone));
        assert_eq!(writes[1], (east, updated_fence));
    }

    #[test]
    fn pane_wall_and_waterlogged_connections_preserve_unrelated_properties() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let bone = reg.state_of("minecraft:bone_block", None);
        let air = reg.state_of("minecraft:air", None);
        let pane = reg.state_of("minecraft:glass_pane", None);
        let bars = reg.with_prop(reg.state_of("minecraft:iron_bars", None), "waterlogged", "true");
        let wall = reg.state_of("minecraft:cobblestone_wall", None);
        let stair = reg.state_of("minecraft:oak_stairs", None);
        let p = (8, 72, 8);
        let north = (8, 72, 7);
        let south = (8, 72, 9);
        let west = (7, 72, 8);
        let east = (9, 72, 8);
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo { pos: (0, 0, 0), state: bone, nbt: None }]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_writes = Some(Vec::new());
        for (at, st) in [
            (p, air), (north, wall), (south, stair), (west, bars),
            (east, pane), ((8, 71, 8), air), ((8, 73, 8), air),
            ((8, 73, 7), air), ((8, 72, 10), air),
        ] {
            ctx.test_base_cell(at, st);
        }
        let mut rng = Xor::seed_worldgen(0);
        assert!(template.place_solid_blocks(
            &mut ctx,
            &mut rng,
            Placement {
                origin: p, pivot: (0, 0, 0),
                mirror: Mirror::None, rotation: Rotation::None,
            },
            &[], [8, 72, 8, 8, 72, 8],
        ).unwrap());
        assert_eq!(reg.get_prop(ctx.get(north.0, north.1, north.2), "south").as_deref(), Some("low"));
        assert_eq!(reg.get_prop(ctx.get(north.0, north.1, north.2), "up").as_deref(), Some("true"));
        assert_eq!(reg.get_prop(ctx.get(east.0, east.1, east.2), "west").as_deref(), Some("true"));
        assert_eq!(reg.get_prop(ctx.get(west.0, west.1, west.2), "east").as_deref(), Some("true"));
        assert_eq!(
            reg.get_prop(ctx.get(west.0, west.1, west.2), "waterlogged").as_deref(),
            Some("true"),
            "shape updates must not lose the waterlogged state"
        );
        assert_eq!(ctx.get(south.0, south.1, south.2), stair,
            "an ordinary cube cannot change the corner of a straight stair");
        let writes = ctx.test_writes.as_ref().unwrap();
        assert_eq!(writes.first(), Some(&(p, bone)));
        assert_eq!(writes.len(), 4, "only the center and three connectors change");
        // The existing fluid engine cannot execute waterlogged block-state
        // water. Do not enqueue a phantom ordinary water block tick here.
        assert_eq!(
            ctx.g.generated_fluid_ticks(),
            vec![(west, 0)],
            "waterlogged bars reserve native water source via the proto queue"
        );
    }

    #[test]
    fn stairs_recompute_outer_inner_and_preserve_waterlogged_on_neighbor_changes() {
        use std::collections::HashMap;
        let reg = crate::block_registry::breg().unwrap();
        let stair = reg.with_prop(
            reg.state_of("minecraft:oak_stairs", None), "waterlogged", "true",
        );
        let west_stair = reg.with_prop(stair, "facing", "west");
        let top_stair = reg.with_prop(west_stair, "half", "top");
        let p = (0, 70, 0);
        let air = reg.state_of("minecraft:air", None);
        let eval = |neighbors: HashMap<(i32, i32, i32), u32>| {
            reg.stair_after_neighbors(stair, p, |q| neighbors.get(&q).copied().unwrap_or(air))
        };
        let outer = eval(HashMap::from([((0, 70, -1), west_stair)]))
            .expect("front turn");
        assert_eq!(reg.get_prop(outer, "shape").as_deref(), Some("outer_left"));
        assert_eq!(reg.get_prop(outer, "waterlogged").as_deref(), Some("true"));
        let inner = eval(HashMap::from([((0, 70, 1), west_stair)]))
            .expect("rear turn");
        assert_eq!(reg.get_prop(inner, "shape").as_deref(), Some("inner_left"));
        assert!(eval(HashMap::from([((0, 70, -1), top_stair)])).is_none(),
            "different stair halves do not make a corner");
        assert!(eval(HashMap::from([
            ((0, 70, -1), west_stair), ((1, 70, 0), stair),
        ])).is_none(), "aligned flank blocks the outer corner");
        assert!(eval(HashMap::new()).is_none(), "already-straight state is stable");
    }

    #[test]
    fn every_pinned_template_matches_native_typed_nbt_and_loaded_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reg = crate::block_registry::breg().unwrap();
        let rows: Vec<&str> = include_str!("../tests/data/structure_templates_1211.tsv")
            .lines()
            .collect();
        assert_eq!(rows.len(), 1180);
        let workers = std::thread::available_parallelism()
            .map(|n| n.get().clamp(1, 8))
            .unwrap_or(1);
        let next = AtomicUsize::new(0);
        let count = std::thread::scope(|s| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    s.spawn(|| {
                        let mut count = 0usize;
                        loop {
                            let idx = next.fetch_add(1, Ordering::Relaxed);
                            let Some(row) = rows.get(idx) else {
                                break;
                            };
                            let f: Vec<_> = row.split('\t').collect();
                            assert_eq!(f.len(), 5);
                            let path =
                                std::path::Path::new("datapacks/vanilla/data/minecraft/structure")
                                    .join(f[0]);
                            let bytes = std::fs::read(&path).unwrap();
                            let (name, root) =
                                crate::nbt::read_gzip(&bytes, 16 * 1024 * 1024).unwrap();
                            let canonical = crate::nbt::write_named(&name, &root).unwrap();
                            assert_eq!(hex(&sha256(&canonical)), f[1], "native typed NBT: {}", f[0]);
                            let template =
                                Template::load(&root, reg).unwrap_or_else(|e| panic!("{}: {e}", f[0]));
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
                        count
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum::<usize>()
        });
        assert_eq!(count, 1180);
    }

    #[test]
    fn processor_stage_matches_native_cells_and_rng() {
        use crate::generation_math::{hash_block_pos, Lcg, Xor};
        use crate::json::{jparse, J};
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let cases = jparse(include_bytes!(
            "../tests/data/template_processor_cases_1211.json"
        ))
        .unwrap();
        let cases = cases.as_arr().unwrap();
        let expected: Vec<_> = include_str!("../tests/data/template_processors_1211.tsv")
            .lines()
            .collect();
        assert_eq!(cases.len(), expected.len());
        assert_eq!(cases.len(), 270);
        let pos = |j: &J| {
            let a = j.as_arr().unwrap();
            (a[0].num() as i32, a[1].num() as i32, a[2].num() as i32)
        };
        for (case, row) in cases.iter().zip(expected) {
            let f: Vec<_> = row.split('\t').collect();
            let id = case.get("id").unwrap().str();
            assert_eq!(f[0], id);
            let empty_blocks = J::Arr(Vec::new());
            let blocks = case
                .get("blocks")
                .unwrap_or(&empty_blocks)
                .as_arr()
                .unwrap()
                .iter()
                .map(|b| BlockInfo {
                    pos: pos(b.get("pos").unwrap()),
                    state: state_from_json(b.get("state").unwrap(), reg).unwrap(),
                    nbt: b.get("marker").map(|m| {
                        Tag::Compound(BTreeMap::from([(
                            NbtString::text("marker"),
                            Tag::Int(m.num() as i32),
                        )]))
                    }),
                })
                .collect();
            let mut template = if let Some(path) = case.get("template") {
                let data = std::fs::read(path.str()).unwrap();
                let (_, root) = crate::nbt::read_gzip(&data, 16 * 1024 * 1024).unwrap();
                Template::load(&root, reg).unwrap()
            } else {
                Template {
                    size: (8, 8, 8),
                    palettes: vec![blocks],
                    entities: Vec::new(),
                }
            };
            let original = template.block_layout(reg).unwrap();
            let processors = Processor::list(case, reg).unwrap();
            let world: Vec<_> = case
                .get("world")
                .unwrap()
                .as_arr()
                .unwrap()
                .iter()
                .map(|s| state_from_json(s, reg).unwrap())
                .collect();
            let mut rng = Xor::seed_worldgen(case.get("seed").unwrap().num() as i64 as u64);
            let mode = case.get("random").unwrap().str();
            let mut shared_draws = 0;
            let mut reads = 0;
            let processed = template
                .process_blocks(
                    0,
                    Placement {
                        origin: pos(case.get("origin").unwrap()),
                        pivot: pos(case.get("pivot").unwrap()),
                        mirror: Mirror::from_index(case.get("mirror").unwrap().num() as u8)
                            .unwrap(),
                        rotation: Rotation::from_index(case.get("rotation").unwrap().num() as u8)
                            .unwrap(),
                    },
                    &processors,
                    reg,
                    |p| {
                        reads += 1;
                        let key =
                            p.0.wrapping_mul(31)
                                .wrapping_add(p.1.wrapping_mul(7))
                                .wrapping_add(p.2.wrapping_mul(13));
                        world[key.rem_euclid(world.len() as i32) as usize]
                    },
                    |p| {
                        if mode == "positional" {
                            Lcg::new(hash_block_pos(p.0, p.1, p.2) as u64).next_f32()
                        } else {
                            shared_draws += 1;
                            if mode == "zero" {
                                0.0
                            } else {
                                rng.next_f32()
                            }
                        }
                    },
                )
                .unwrap();
            assert_eq!(
                template.block_layout(reg).unwrap(),
                original,
                "source mutated: {id}"
            );
            assert_eq!(
                processed.len(),
                f[1].parse::<usize>().unwrap(),
                "count: {id}"
            );
            template.palettes = vec![processed];
            assert_eq!(
                hex(&sha256(&template.block_layout(reg).unwrap())),
                f[2],
                "full typed cells: {id}"
            );
            assert_eq!(
                rng.next_bound(1000003),
                f[3].parse::<i32>().unwrap(),
                "following RNG: {id}"
            );
            assert_eq!(
                shared_draws,
                f[4].parse::<usize>().unwrap(),
                "shared float calls: {id}"
            );
            assert_eq!(reads, f[5].parse::<usize>().unwrap(), "world reads: {id}");
        }
    }

    #[test]
    fn processor_lists_reject_unsupported_required_children() {
        use crate::json::jparse;
        let reg = crate::block_registry::breg().unwrap();
        let reject = |body: &str| {
            let json = format!("{{\"processors\":[{body}]}}");
            assert!(
                Processor::list(&jparse(json.as_bytes()).unwrap(), reg).is_err(),
                "accepted {body}"
            );
        };
        for bad in [
            r#"{"processor_type":"minecraft:gravity"}"#,
            r#"{"processor_type":"minecraft:protected_blocks","value":"bedrock"}"#,
            r##"{"processor_type":"minecraft:protected_blocks","value":"#BadTag"}"##,
            r#"{"processor_type":"minecraft:block_rot","integrity":-0.0}"#,
            r#"{"processor_type":"minecraft:block_rot","integrity":-1e-60}"#,
            r#"{"processor_type":"minecraft:block_rot","integrity":1.1}"#,
            r#"{"processor_type":"minecraft:block_rot","integrity":0.1,"rottable_blocks":["no_such_block"]}"#,
            r#"{"processor_type":"minecraft:rule","rules":[{}]}"#,
        ] {
            reject(bad);
        }
        // A supported sibling does not make a required unsupported child optional.
        reject(
            r#"{"processor_type":"minecraft:block_rot","integrity":1},{"processor_type":"minecraft:gravity"}"#,
        );
        for extra in [
            r#""position_predicate":{"predicate_type":"minecraft:always_true"},"#,
            r#""block_entity_modifier":{"type":"minecraft:passthrough"},"#,
        ] {
            reject(&format!(
                r#"{{"processor_type":"minecraft:rule","rules":[{{{extra}"input_predicate":{{"predicate_type":"minecraft:always_true"}},"location_predicate":{{"predicate_type":"minecraft:always_true"}},"output_state":{{"Name":"minecraft:stone"}}}}]}}"#
            ));
        }
    }
}
