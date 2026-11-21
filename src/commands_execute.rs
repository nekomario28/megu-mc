use crate::brigadier::{
    ExecuteAnchor, ExecuteBlocksMode, ExecuteBossbarField, ExecuteCheck, ExecuteClause,
    ExecuteCommand, ExecuteConditionKind, ExecuteConditional, ExecuteCoordinate, ExecuteDataTarget,
    ExecuteHeightmap, ExecuteModifier, ExecuteNbtType, ExecutePosition, ExecuteRelation,
    ExecuteScoreComparison, ExecuteScoreTest, ExecuteStore, ExecuteStoreKind, ExecuteStoreTarget,
};
use crate::nbt::{NbtString, Tag as ExecTag};

// Legacy command handlers read the branch dimension through cmd_dim. The
// override lives only for the duration of their dispatch.
thread_local! {
    static EXEC_DIM: std::cell::Cell<Option<DimId>> = const { std::cell::Cell::new(None) };
}
fn cmd_dim(ctx: &CmdCtx) -> DimId {
    EXEC_DIM
        .with(|d| d.get())
        .or_else(|| ctx.me.as_ref().map(|c| *c.dim.lock().unwrap()))
        .unwrap_or(DimId::Overworld)
}

// datapack predicate eval — subset: random_chance, inverted, any_of/all_of
// (alternative), time_check, weather_check, entity_properties(type only),
// location_check(block only); top-level array = all_of. Unknown -> false.
fn pred_eval(body: &str, source: &ExecuteSource, depth: usize) -> bool {
    let ctx = &source.ctx;
    if depth > 8 {
        return false;
    }
    let b = body.trim();
    if b.starts_with('[') {
        if let Some(inner) = b.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            return jobjects(inner)
                .iter()
                .all(|o| pred_eval(o, source, depth + 1));
        }
        return false;
    }
    let cond = jstr(b, "condition")
        .unwrap_or("")
        .trim_start_matches("minecraft:");
    match cond {
        "inverted" => jblock(b, "term")
            .map(|t| !pred_eval(t, source, depth + 1))
            .unwrap_or(false),
        "any_of" | "alternative" => jarray(b, "terms")
            .map(|a| jobjects(a).iter().any(|o| pred_eval(o, source, depth + 1)))
            .unwrap_or(false),
        "all_of" => jarray(b, "terms")
            .map(|a| jobjects(a).iter().all(|o| pred_eval(o, source, depth + 1)))
            .unwrap_or(false),
        "random_chance" => {
            let c = jnum(b, "chance")
                .map(|n| n as f64 / 100.0)
                .or_else(|| jstr(b, "chance").and_then(|x| x.parse::<f64>().ok()))
                .unwrap_or(0.0);
            let h = ctx.x.to_bits().wrapping_mul(2862933555777941757)
                ^ ctx.z.to_bits().wrapping_mul(3202034522624059733)
                ^ TICK.load(Ordering::Relaxed);
            (h % 10000) as f64 / 10000.0 < c
        }
        "time_check" => {
            let period = jnum(b, "period").filter(|p| *p > 0).unwrap_or(24000) as u64;
            let t = WORLD_TIME.load(Ordering::Relaxed) % period;
            match jblock(b, "value") {
                Some(v) => {
                    let lo = jnum(v, "min").unwrap_or(i64::MIN);
                    let hi = jnum(v, "max").unwrap_or(i64::MAX);
                    t as i64 >= lo && t as i64 <= hi
                }
                None => jnum(b, "value").map(|v| t as i64 == v).unwrap_or(false),
            }
        }
        "weather_check" => {
            let want_rain = jstr(b, "raining").map(|v| v == "true");
            want_rain
                .map(|w| (RAINING.load(Ordering::Relaxed) > 0) == w)
                .unwrap_or(true)
        }
        "entity_properties" => match jblock(b, "predicate").and_then(|p| jstr(p, "type")) {
            Some(t) => source.entity.as_ref().is_some_and(|entity| {
                t.trim_start_matches("minecraft:") == entity.kind().trim_start_matches("minecraft:")
            }),
            None => source.entity.is_some(),
        },
        "location_check" => {
            let (x, y, z) = execute_block_pos([ctx.x, ctx.y - 1.0, ctx.z]);
            let st = block_at_dim(source.dimension, x, y, z);
            jblock(b, "predicate")
                .and_then(|p| jblock(p, "block"))
                .map(|bb| {
                    if let Some(arr) = jarray(bb, "blocks") {
                        jstr_all(arr).iter().any(|n| block_pred_match(st, n))
                    } else {
                        jstr(bb, "blocks")
                            .map(|n| block_pred_match(st, n))
                            .unwrap_or(false)
                    }
                })
                .unwrap_or(false)
        }
        _ => false,
    }
}
fn jstr_all(arr: &str) -> Vec<String> {
    arr.split('"')
        .skip(1)
        .step_by(2)
        .map(|x| x.to_string())
        .collect()
}
const EXECUTE_DEPTH_LIMIT: u32 = 16;
const EXECUTE_FORK_LIMIT: usize = 65_536;

thread_local! {
    static EXEC_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[derive(Clone)]
enum ExecuteEntity {
    Player(Arc<Client>),
    Mob(Arc<Mob>),
    Other {
        id: i32,
        kind: &'static str,
        pos: [f64; 3],
        owner: Option<i32>,
        dimension: DimId,
    },
}

impl ExecuteEntity {
    fn id(&self) -> i32 {
        match self {
            Self::Player(player) => player.eid,
            Self::Mob(mob) => mob.eid,
            Self::Other { id, .. } => *id,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Player(_) => "minecraft:player",
            Self::Mob(mob) => {
                entity_spec_by_id(mob.etype).map_or("minecraft:unknown", |spec| spec.name)
            }
            Self::Other { kind, .. } => kind,
        }
    }

    fn dimension(&self) -> DimId {
        match self {
            Self::Player(player) => *player.dim.lock().unwrap(),
            Self::Mob(mob) => mob.dim,
            Self::Other { dimension, .. } => *dimension,
        }
    }

    fn location(&self) -> ([f64; 3], [f64; 2]) {
        let (x, y, z, yaw, pitch) = match self {
            Self::Player(player) => {
                let (x, y, z, yaw, pitch, _) = *player.pos.lock().unwrap();
                (x, y, z, yaw, pitch)
            }
            Self::Mob(mob) => *mob.pos.lock().unwrap(),
            Self::Other { pos, .. } => return (*pos, [0.0; 2]),
        };
        ([x, y, z], [f64::from(yaw), f64::from(pitch)])
    }

    fn eye_height(&self) -> f64 {
        f64::from(match self {
            Self::Player(player) => {
                let sneaking = *player.sneaking.lock().unwrap();
                let flying = *player.fall_fly.lock().unwrap();
                player_eye_height(sneaking, flying)
            }
            Self::Mob(mob) => entity_eye_height_1211(mob.etype),
            Self::Other { kind, .. } => {
                entity_spec_by_name(kind).map_or(0.0, |spec| spec.eye_height)
            }
        })
    }

    fn health(&self) -> Option<f32> {
        match self {
            Self::Player(player) => Some(*player.hp.lock().unwrap()),
            Self::Mob(mob) => Some(*mob.hp.lock().unwrap()),
            Self::Other { .. } => None,
        }
    }

    fn tag_key(&self) -> String {
        match self {
            Self::Player(player) => format!("p:{}", player.name),
            Self::Mob(mob) => format!("e:{}", mob.eid),
            Self::Other { id, kind, .. } => {
                let prefix = match *kind {
                    "minecraft:item" => 'd',
                    "minecraft:tnt" => 't',
                    "minecraft:arrow" => 'a',
                    "minecraft:ender_pearl" => 'p',
                    "minecraft:fishing_bobber" => 'b',
                    "minecraft:experience_orb" => 'x',
                    _ => 'e',
                };
                format!("{prefix}:{id}")
            }
        }
    }

    fn score_holder(&self) -> String {
        let uuid = match self {
            Self::Player(player) => return player.name.clone(),
            Self::Mob(mob) => u128::from_be_bytes(mob.uuid),
            Self::Other { id, .. } => *id as u128,
        };
        format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            uuid >> 96,
            (uuid >> 80) & 0xffff,
            (uuid >> 64) & 0xffff,
            (uuid >> 48) & 0xffff,
            uuid & 0xffff_ffff_ffff
        )
    }

    fn related(&self, relation: ExecuteRelation) -> Option<i32> {
        match (self, relation) {
            (Self::Other { owner, .. }, ExecuteRelation::Origin) => *owner,
            (Self::Mob(mob), ExecuteRelation::Owner) => {
                let owner = {
                    let states = live_mob_states().lock().unwrap();
                    match states.get(&mob.eid)?.behavior.as_ref()? {
                        MobBehaviorState::Wolf(pet) | MobBehaviorState::Cat(pet) => pet.owner?,
                        _ => return None,
                    }
                };
                client_snapshot()
                    .iter()
                    .find(|player| player.uuid == owner)
                    .map(|player| player.eid)
            }
            (Self::Mob(mob), ExecuteRelation::Target) => {
                let states = live_mob_states().lock().unwrap();
                match states.get(&mob.eid)?.behavior.as_ref()? {
                    MobBehaviorState::Warden(warden) => warden.target,
                    _ => None,
                }
            }
            // Ride, leash and last-attacker identities have no live owner here.
            _ => None,
        }
    }
}

fn execute_entities() -> Vec<ExecuteEntity> {
    let mut entities: Vec<_> = client_snapshot()
        .into_iter()
        .map(ExecuteEntity::Player)
        .collect();
    entities.extend(
        mobs()
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .map(ExecuteEntity::Mob),
    );
    macro_rules! add_entities {
        ($registry:ident, $kind:literal, $owner:expr) => {
            entities.extend(
                $registry()
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|e| ExecuteEntity::Other {
                        id: e.eid,
                        kind: concat!("minecraft:", $kind),
                        pos: [e.x, e.y, e.z],
                        owner: ($owner)(e),
                        dimension: DimId::Overworld,
                    }),
            );
        };
    }
    add_entities!(drops, "item", |_: &Drop| None);
    add_entities!(primed, "tnt", |_: &Primed| None);
    add_entities!(arrows, "arrow", |a: &Arrow| Some(a.owner));
    add_entities!(pearls, "ender_pearl", |p: &Pearl| Some(p.owner));
    add_entities!(bobbers, "fishing_bobber", |b: &Bobber| Some(b.owner));
    add_entities!(xps, "experience_orb", |_: &Xp| None);
    entities.extend(
        live_mob_projectiles()
            .lock()
            .unwrap()
            .iter()
            .map(|projectile| ExecuteEntity::Other {
                id: projectile.eid,
                kind: entity_spec_by_id(projectile.entity_type())
                    .map_or("minecraft:unknown", |spec| spec.name),
                pos: projectile.pos,
                owner: Some(projectile.owner),
                dimension: projectile.dim,
            }),
    );
    entities
}

#[derive(Clone)]
struct ExecuteSource {
    ctx: CmdCtx,
    entity: Option<ExecuteEntity>,
    dimension: DimId,
    rotation: [f64; 2],
    anchor: ExecuteAnchor,
}

impl ExecuteSource {
    fn new(ctx: &CmdCtx) -> Self {
        let entity = ctx.me.clone().map(ExecuteEntity::Player);
        let rotation = entity
            .as_ref()
            .map_or([0.0; 2], |entity| entity.location().1);
        Self {
            ctx: ctx.clone(),
            entity,
            rotation,
            dimension: cmd_dim(ctx),
            anchor: ExecuteAnchor::Feet,
        }
    }

    fn position(&self) -> [f64; 3] {
        [self.ctx.x, self.ctx.y, self.ctx.z]
    }

    fn set_position(&mut self, [x, y, z]: [f64; 3]) {
        (self.ctx.x, self.ctx.y, self.ctx.z) = (x, y, z);
    }

    fn set_entity(&mut self, entity: ExecuteEntity) {
        self.ctx.me = match &entity {
            ExecuteEntity::Player(player) => Some(player.clone()),
            _ => None,
        };
        self.entity = Some(entity);
    }

    fn anchor_position(&self) -> [f64; 3] {
        let [x, mut y, z] = self.position();
        if self.anchor == ExecuteAnchor::Eyes {
            y += self.entity.as_ref().map_or(0.0, ExecuteEntity::eye_height);
        }
        [x, y, z]
    }

    fn resolve_position(&self, pos: ExecutePosition) -> Result<[f64; 3], String> {
        use ExecuteCoordinate::*;
        let coords = [pos.x, pos.y, pos.z];
        let out = if let [Local(left), Local(up), Local(forward)] = coords {
            let [yaw, pitch] = self.rotation.map(f64::to_radians);
            let (sy, cy) = yaw.sin_cos();
            let (sp, cp) = pitch.sin_cos();
            let [x, y, z] = self.anchor_position();
            [
                x + left * cy - up * sy * sp - forward * sy * cp,
                y + up * cp - forward * sp,
                z + left * sy + up * cy * sp + forward * cy * cp,
            ]
        } else {
            let [x, y, z] = self.position();
            [
                execute_world_coordinate(pos.x, x)?,
                execute_world_coordinate(pos.y, y)?,
                execute_world_coordinate(pos.z, z)?,
            ]
        };
        if !out.into_iter().all(f64::is_finite) {
            return Err("Invalid execute position".into());
        }
        Ok(out)
    }

    fn block_position(&self, pos: ExecutePosition) -> Result<BlockPos, String> {
        Ok(execute_block_pos(self.resolve_position(pos)?))
    }

    fn face(&mut self, target: [f64; 3]) {
        let [x, y, z] = self.anchor_position();
        let [dx, dy, dz] = [target[0] - x, target[1] - y, target[2] - z];
        self.rotation = [
            -dx.atan2(dz).to_degrees(),
            -dy.atan2(dx.hypot(dz)).to_degrees(),
        ];
    }

    fn select(&self, target: &str) -> Result<Vec<ExecuteEntity>, String> {
        if target == "@s" {
            return Ok(self.entity.iter().cloned().collect());
        }
        if !target.starts_with('@') {
            return Ok(client_snapshot()
                .into_iter()
                .find(|player| player.name == target)
                .map(ExecuteEntity::Player)
                .into_iter()
                .collect());
        }
        let spec =
            brigadier::SelectorSpec::parse(target, |kind| entity_spec_by_name(kind).is_some())?;
        let mut entities = execute_entities();
        if let Some(entity) = self
            .entity
            .as_ref()
            .filter(|entity| !entities.iter().any(|other| other.id() == entity.id()))
        {
            entities.push(entity.clone());
        }

        let tag_snapshot = tags().lock().unwrap().clone();
        let entity_tags: Vec<Vec<&str>> = entities
            .iter()
            .map(|entity| {
                tag_snapshot
                    .get(&entity.tag_key())
                    .into_iter()
                    .flatten()
                    .map(String::as_str)
                    .collect()
            })
            .collect();
        let score_filters: Vec<_> = spec
            .filters()
            .iter()
            .filter_map(|filter| match filter {
                brigadier::SelectorFilter::Scores(scores) => Some(scores.keys()),
                _ => None,
            })
            .flatten()
            .collect();
        let score_snapshot: Vec<HashMap<&str, i32>> = {
            let board = scoreboard().lock().unwrap();
            entities
                .iter()
                .map(|entity| {
                    let holder = entity.score_holder();
                    score_filters
                        .iter()
                        .filter_map(|objective| {
                            board
                                .get(&holder, objective)
                                .map(|score| (objective.as_str(), score))
                        })
                        .collect()
                })
                .collect()
        };

        let entity_nbt: Vec<_> = if spec
            .filters()
            .iter()
            .any(|filter| matches!(filter, brigadier::SelectorFilter::Nbt { .. }))
        {
            entities
                .iter()
                .map(|entity| {
                    brigadier::SelectorNbt::parse(&execute_tag_snbt(&execute_entity_data(entity)))
                        .ok()
                })
                .collect()
        } else {
            Vec::new()
        };
        // Vanilla `@a[team=red]` reads the scoreboard team that owns each
        // candidate's score-holder name; `p3b_team_of` is that registry.
        let entity_teams: Vec<Option<String>> = entities
            .iter()
            .map(|entity| p3b_team_of(&entity.score_holder()))
            .collect();
        let candidates: Vec<_> = entities
            .iter()
            .zip(&entity_tags)
            .zip(score_snapshot)
            .enumerate()
            .map(|(index, ((entity, tags), scores))| {
                let (position, [yaw, pitch]) = entity.location();
                let (name, level, gamemode) = match entity {
                    ExecuteEntity::Player(player) => {
                        use brigadier::SelectorGameMode::*;
                        let mode = match *player.gm.lock().unwrap() {
                            1 => Creative,
                            2 => Adventure,
                            3 => Spectator,
                            _ => Survival,
                        };
                        (
                            player.name.as_str(),
                            Some(
                                player_mechanism_sessions()
                                    .lock()
                                    .unwrap()
                                    .get(&player.eid)
                                    .map_or(0, |session| session.xp_bar.0),
                            ),
                            Some(mode),
                        )
                    }
                    _ => (entity.kind().trim_start_matches("minecraft:"), None, None),
                };
                let bounds = entity_spec_by_name(entity.kind()).map(|spec| {
                    let [x, y, z] = position;
                    let r = f64::from(spec.width) / 2.0;
                    [
                        [x - r, y, z - r],
                        [x + r, y + f64::from(spec.height), z + r],
                    ]
                });
                let health = entity.health();
                brigadier::SelectorCandidate {
                    id: entity.id(),
                    name,
                    entity_type: entity.kind(),
                    position,
                    dimension: entity.dimension().dimension_id(),
                    bounds,
                    alive: health.is_none_or(|hp| hp > 0.0),
                    living: health.is_some(),
                    yaw: yaw as f32,
                    pitch: pitch as f32,
                    level,
                    gamemode,
                    tags,
                    scores,
                    nbt: entity_nbt.get(index).and_then(Option::as_ref),
                    team: entity_teams
                        .get(index)
                        .and_then(|team| team.as_deref()),
                    ..Default::default()
                }
            })
            .collect();
        let source = brigadier::SelectorSource {
            position: self.position(),
            dimension: self.dimension.dimension_id(),
            executor: self.entity.as_ref().map(ExecuteEntity::id),
        };
        let by_id: HashMap<_, _> = entities
            .iter()
            .map(|entity| (entity.id(), entity))
            .collect();
        Ok(brigadier::select_entities(
            &spec,
            &source,
            &candidates,
            |candidate, id| {
                let mut source = self.clone();
                source.set_entity((*by_id.get(&candidate.id)?).clone());
                source.set_position(candidate.position);
                source.dimension = source.entity.as_ref()?.dimension();
                execute_predicate(id, &source)
            },
            |len| (world_rng::next_u64() % len as u64) as usize,
        )
        .into_iter()
        .filter_map(|candidate| by_id.get(&candidate.id).map(|entity| (*entity).clone()))
        .collect())
    }

    fn score_holders(&self, target: &str, objective: &str) -> Result<Vec<String>, String> {
        if target.starts_with('@') {
            return Ok(self
                .select(target)?
                .iter()
                .map(ExecuteEntity::score_holder)
                .collect());
        }
        if target == "*" {
            let board = scoreboard().lock().unwrap();
            let mut holders: Vec<_> = board
                .scores
                .get(objective)
                .into_iter()
                .flat_map(|scores| scores.keys().cloned())
                .collect();
            holders.sort_unstable();
            return Ok(holders);
        }
        Ok(scoreboard_holders(target, self.ctx.me.as_ref()))
    }

    fn modify(mut self, modifier: &ExecuteModifier) -> Result<Vec<Self>, String> {
        use ExecuteModifier::*;
        match modifier {
            As { targets }
            | At { targets }
            | PositionedAs { targets }
            | RotatedAs { targets }
            | FacingEntity { targets, .. } => {
                return self.select(targets).map(|entities| {
                    entities
                        .into_iter()
                        .map(|entity| {
                            let mut source = self.clone();
                            let (mut position, rotation) = entity.location();
                            match modifier {
                                As { .. } => source.set_entity(entity),
                                At { .. } => {
                                    source.set_position(position);
                                    source.rotation = rotation;
                                    source.dimension = entity.dimension();
                                }
                                PositionedAs { .. } => source.set_position(position),
                                RotatedAs { .. } => source.rotation = rotation,
                                FacingEntity { anchor, .. } => {
                                    if *anchor == ExecuteAnchor::Eyes {
                                        position[1] += entity.eye_height();
                                    }
                                    source.face(position);
                                }
                                _ => unreachable!(),
                            }
                            source
                        })
                        .collect()
                });
            }
            Positioned { pos } => {
                self.set_position(self.resolve_position(*pos)?);
                self.anchor = ExecuteAnchor::Feet;
            }
            PositionedOver { heightmap } => {
                use crate::block_registry::HeightmapKind;
                let kind = match heightmap {
                    ExecuteHeightmap::WorldSurface => HeightmapKind::WorldSurface,
                    ExecuteHeightmap::OceanFloor => HeightmapKind::OceanFloor,
                    ExecuteHeightmap::MotionBlocking => HeightmapKind::MotionBlocking,
                    ExecuteHeightmap::MotionBlockingNoLeaves => {
                        HeightmapKind::MotionBlockingNoLeaves
                    }
                };
                let (x, _, z) = execute_block_pos(self.position());
                if !execute_chunk_loaded(self.dimension, x, z) {
                    return Err("Execute position is not loaded".into());
                }
                let reg = breg().ok_or("Block registry unavailable")?;
                let min = self.dimension.min_y();
                self.ctx.y = f64::from(
                    (min..min + self.dimension.height())
                        .rev()
                        .find(|&y| {
                            reg.heightmap_counts(block_at_dim(self.dimension, x, y, z), kind)
                        })
                        .map_or(min, |y| y + 1),
                );
            }
            Rotated { yaw, pitch } => {
                self.rotation = [
                    execute_world_coordinate(*yaw, self.rotation[0])?,
                    execute_world_coordinate(*pitch, self.rotation[1])?,
                ];
            }
            Facing { pos } => self.face(self.resolve_position(*pos)?),
            Align { axes } => {
                let mut position = self.position();
                for (value, align) in position.iter_mut().zip([axes.x, axes.y, axes.z]) {
                    if align {
                        *value = value.floor();
                    }
                }
                self.set_position(position);
            }
            Anchored { anchor } => self.anchor = *anchor,
            In { dimension } => {
                let dimension =
                    DimId::parse_dimension_id(dimension).ok_or("Unknown execute dimension")?;
                // Vanilla's `execute in` rescales x/z by the ratio of the two
                // dimension_types' coordinate_scale — 8 for the Nether, 1 for the
                // Overworld and the End, and whatever a custom type declares.
                let ratio = self.dimension.coordinate_scale() / dimension.coordinate_scale();
                self.ctx.x *= ratio;
                self.ctx.z *= ratio;
                self.dimension = dimension;
            }
            Summon { entity } => {
                let spec = entity_spec_by_name(entity)
                    .filter(|spec| spec.summonable)
                    .ok_or_else(|| format!("Unknown summonable entity {entity}"))?;
                let [x, y, z] = self.position();
                if ![x, y, z].into_iter().all(f64::is_finite)
                    || x.abs() >= 30_000_000.0
                    || z.abs() >= 30_000_000.0
                    || y.abs() > 30_000_000.0
                {
                    return Err("Invalid summon position".into());
                }
                let mut live = mobs().lock().unwrap();
                if live.len() >= MOB_CAP {
                    return Err("mob cap reached".into());
                }
                let mob = new_live_mob(
                    spec.id,
                    self.dimension,
                    (x, y, z, 0.0, 0.0),
                    default_mob_hp(entity),
                    None,
                );
                self.set_entity(ExecuteEntity::Mob(mob.clone()));
                live.push(mob);
            }
            On { relation } => {
                let Some(id) = self
                    .entity
                    .as_ref()
                    .and_then(|entity| entity.related(*relation))
                else {
                    return Ok(Vec::new());
                };
                let Some(entity) = execute_entities()
                    .into_iter()
                    .find(|entity| entity.id() == id)
                else {
                    return Ok(Vec::new());
                };
                self.set_entity(entity);
            }
        }
        Ok(vec![self])
    }
}

fn execute_world_coordinate(value: ExecuteCoordinate, base: f64) -> Result<f64, String> {
    match value {
        ExecuteCoordinate::Absolute(value) => Ok(value),
        ExecuteCoordinate::Relative(value) => Ok(base + value),
        ExecuteCoordinate::Local(_) => {
            Err("Local coordinates require a full local position".into())
        }
    }
}

fn execute_block_pos([x, y, z]: [f64; 3]) -> BlockPos {
    (x.floor() as i32, y.floor() as i32, z.floor() as i32)
}

fn execute_chunk_loaded(dim: DimId, x: i32, z: i32) -> bool {
    let chunk = (x >> 4, z >> 4);
    // The warm ring and the forceload set are dimension-agnostic server state: they
    // mark the chunks this server is actively ticking, whichever dimension asked.
    // Everything below is keyed by the queried dimension, so a Nether or End chunk
    // reports loaded on the same evidence an Overworld chunk does.
    region_guard().is_warm(x, z)
        || forceloaded().lock().unwrap().contains(&chunk)
        || world()
            .lock()
            .unwrap()
            .get(&(dim, chunk.0, chunk.1))
            .is_some_and(|cells| !cells.is_empty())
        || band_for(dim, chunk.0, chunk.1).is_some()
        || {
            let store = section_store().lock().unwrap();
            store.generation == current_gen_seq()
                && store
                    .map
                    .get(&(dim, chunk.0, chunk.1))
                    .is_some_and(|sections| sections.iter().any(Option::is_some))
        }
        || worldgen::with_gen_dim(dim, |g| g.pop_done.contains(&chunk)).unwrap_or(false)
        || client_snapshot().iter().any(|client| {
            *client.dim.lock().unwrap() == dim && client.chunks.lock().unwrap().contains(&chunk)
        })
}

fn execute_predicate(id: &str, source: &ExecuteSource) -> Option<bool> {
    let body = predicates().lock().unwrap().get(id).cloned()?;
    Some(pred_eval(&body, source, 0))
}

fn execute_condition(
    source: &ExecuteSource,
    condition: &ExecuteConditional,
) -> Result<Option<i32>, String> {
    use ExecuteConditionKind::*;
    let count = match &condition.kind {
        Block { pos, predicate } => {
            let (x, y, z) = source.block_position(*pos)?;
            check_modification_bounds(source.dimension, (x, y, z))?;
            let (predicate, nbt) = predicate
                .split_once('{')
                .map_or((predicate.as_str(), None), |(pred, nbt)| {
                    (pred, Some(format!("{{{nbt}")))
                });
            let state = block_at_dim(source.dimension, x, y, z);
            let nbt_matches = if let Some(nbt) = nbt {
                let (_, pattern) = execute_nbt_matcher(&nbt)?;
                execute_block_data(source.dimension, (x, y, z))
                    .is_some_and(|data| execute_nbt_matches(&pattern, &data))
            } else {
                true
            };
            (block_pred_match(state, predicate) && nbt_matches).then_some(1)
        }
        Blocks {
            start,
            end,
            destination,
            mode,
        } => execute_compare_blocks(
            source.dimension,
            source.block_position(*start)?,
            source.block_position(*end)?,
            source.block_position(*destination)?,
            *mode,
        )?,
        Entity { targets } => {
            let count = source.select(targets)?.len() as i32;
            (count > 0).then_some(count)
        }
        Score {
            target,
            objective,
            test,
        } => {
            let holder = execute_single_score_holder(source, target, objective)?;
            let other = if let ExecuteScoreTest::Compare(_, rhs) = test {
                Some(execute_single_score_holder(
                    source,
                    &rhs.target,
                    &rhs.objective,
                )?)
            } else {
                None
            };
            let board = scoreboard().lock().unwrap();
            let get = |holder: Option<String>, objective: &str| board.get(&holder?, objective);
            let value = get(holder, objective);
            let matched = match test {
                ExecuteScoreTest::Matches(range) => value.is_some_and(|value| {
                    range.min.is_none_or(|min| value >= min)
                        && range.max.is_none_or(|max| value <= max)
                }),
                ExecuteScoreTest::Compare(op, rhs) => value
                    .zip(get(other.flatten(), &rhs.objective))
                    .is_some_and(|(a, b)| match op {
                        ExecuteScoreComparison::LessThan => a < b,
                        ExecuteScoreComparison::LessEquals => a <= b,
                        ExecuteScoreComparison::Equals => a == b,
                        ExecuteScoreComparison::GreaterEquals => a >= b,
                        ExecuteScoreComparison::GreaterThan => a > b,
                    }),
            };
            matched.then_some(1)
        }
        Biome { pos, biome } => {
            let (x, y, z) = source.block_position(*pos)?;
            let actual = worldgen::with_gen_dim(source.dimension, |gen| {
                let (_, leaf) = gen.biome_blended(x, y, z)?;
                gen.biome_name_of_leaf(leaf)
            })
            .flatten()
            .unwrap_or_else(|| "minecraft:plains".into());
            (actual.trim_start_matches("minecraft:") == biome.trim_start_matches("minecraft:"))
                .then_some(1)
        }
        Dimension { dimension } => (source.dimension.dimension_id() == dimension).then_some(1),
        Loaded { pos } => {
            let (x, _, z) = source.block_position(*pos)?;
            execute_chunk_loaded(source.dimension, x, z).then_some(1)
        }
        Predicate { id } => execute_predicate(id, source).unwrap_or(false).then_some(1),
        Data { target, path, nbt } => {
            let path = ExecuteNbtPath::parse(path)?;
            let data = execute_read_data(source, target)?;
            let values = data.as_ref().map_or_else(Vec::new, |data| path.get(data));
            let predicate = nbt.as_deref().map(execute_nbt_matcher).transpose()?;
            let count = values
                .iter()
                .filter(|value| {
                    predicate.as_ref().is_none_or(|(negated, pattern)| {
                        execute_nbt_matches(pattern, value) != *negated
                    })
                })
                .count() as i32;
            (count > 0).then_some(count)
        }
    };
    Ok(match condition.check {
        ExecuteCheck::If => count,
        ExecuteCheck::Unless => count.is_none().then_some(1),
    })
}

fn execute_single_score_holder(
    source: &ExecuteSource,
    target: &str,
    objective: &str,
) -> Result<Option<String>, String> {
    let mut holders = source.score_holders(target, objective)?;
    if holders.len() > 1 {
        return Err("Expected a single score holder".into());
    }
    Ok(holders.pop())
}

fn execute_compare_blocks(
    dim: DimId,
    start: BlockPos,
    end: BlockPos,
    dest: BlockPos,
    mode: ExecuteBlocksMode,
) -> Result<Option<i32>, String> {
    let bounds = ModificationBox::between(start, end);
    check_modification_bounds(dim, bounds.min)?;
    check_modification_bounds(dim, bounds.max)?;
    check_modification_bounds(dim, dest)?;
    let (x0, y0, z0) = bounds.min;
    let (x1, y1, z1) = bounds.max;
    let volume = (i64::from(x1 - x0) + 1) * (i64::from(y1 - y0) + 1) * (i64::from(z1 - z0) + 1);
    if volume > 32_768 {
        return Err(format!(
            "Too many blocks to compare ({volume}, limit 32768)"
        ));
    }
    check_modification_bounds(dim, (dest.0 + x1 - x0, dest.1 + y1 - y0, dest.2 + z1 - z0))?;
    let mut count = 0;
    for z in z0..=z1 {
        for y in y0..=y1 {
            for x in x0..=x1 {
                let state = block_at_dim(dim, x, y, z);
                if mode == ExecuteBlocksMode::Masked
                    && breg().map_or(state == 0, |reg| reg.is_air(state))
                {
                    continue;
                }
                let to = (dest.0 + x - x0, dest.1 + y - y0, dest.2 + z - z0);
                if block_at_dim(dim, to.0, to.1, to.2) != state
                    || execute_block_data(dim, (x, y, z)) != execute_block_data(dim, to)
                {
                    return Ok(None);
                }
                count += 1;
            }
        }
    }
    Ok(Some(count))
}

#[derive(Clone)]
enum ExecuteNbtNode {
    Key(NbtString),
    Index(i32),
    All,
    Match(ExecTag, bool),
}

#[derive(Clone)]
struct ExecuteNbtPath(Vec<ExecuteNbtNode>);

impl ExecuteNbtPath {
    fn parse(path: &str) -> Result<Self, String> {
        use ExecuteNbtNode::*;
        let bad_path = || format!("Invalid NBT path '{path}'");
        let mut reader = brigadier::StringReader::new(path);
        let mut nodes = Vec::new();
        let mut need_key = true;
        while let Some(token) = reader.peek() {
            let node = match token {
                '.' if !need_key => {
                    reader.skip();
                    need_key = true;
                    continue;
                }
                '[' if !need_key => {
                    reader.skip();
                    let node = match reader.peek() {
                        Some(']') => All,
                        Some('{') => Match(
                            execute_read_snbt(&mut reader, 0).ok_or_else(bad_path)?,
                            true,
                        ),
                        _ => Index(reader.read_int().map_err(|_| bad_path())?),
                    };
                    reader.expect(']').map_err(|_| bad_path())?;
                    node
                }
                '{' if !need_key || nodes.is_empty() => Match(
                    execute_read_snbt(&mut reader, 0).ok_or_else(bad_path)?,
                    false,
                ),
                _ if need_key => {
                    let name = if matches!(token, '"' | '\'') {
                        reader.read_quoted_string().map_err(|_| bad_path())?
                    } else {
                        let start = reader.cursor();
                        while reader
                            .peek()
                            .is_some_and(|c| !c.is_whitespace() && !".[]{}\"'".contains(c))
                        {
                            reader.skip();
                        }
                        reader.read_text()[start..].to_owned()
                    };
                    if name.is_empty() {
                        return Err(bad_path());
                    }
                    Key(NbtString::text(&name))
                }
                _ => return Err(bad_path()),
            };
            nodes.push(node);
            if nodes.len() > 64 {
                return Err(bad_path());
            }
            need_key = false;
        }
        if need_key {
            return Err(bad_path());
        }
        Ok(Self(nodes))
    }

    fn get<'a>(&self, root: &'a ExecTag) -> Vec<std::borrow::Cow<'a, ExecTag>> {
        use std::borrow::Cow;
        use ExecuteNbtNode::*;
        let mut values = vec![Cow::Borrowed(root)];
        for node in &self.0 {
            values = values
                .into_iter()
                .flat_map(|value| {
                    let Cow::Borrowed(value) = value else {
                        return Vec::new();
                    };
                    match (node, value) {
                        (Key(key), ExecTag::Compound(fields)) => {
                            fields.get(key).into_iter().map(Cow::Borrowed).collect()
                        }
                        (Index(index), ExecTag::List(_, entries)) => {
                            execute_nbt_index(*index, entries.len())
                                .map(|i| Cow::Borrowed(&entries[i]))
                                .into_iter()
                                .collect()
                        }
                        (All, ExecTag::List(_, entries)) => {
                            entries.iter().map(Cow::Borrowed).collect()
                        }
                        (Index(index), ExecTag::IntArray(entries)) => {
                            execute_nbt_index(*index, entries.len())
                                .map(|i| Cow::Owned(ExecTag::Int(entries[i])))
                                .into_iter()
                                .collect()
                        }
                        (Index(index), ExecTag::ByteArray(entries)) => {
                            execute_nbt_index(*index, entries.len())
                                .map(|i| Cow::Owned(ExecTag::Byte(entries[i] as i8)))
                                .into_iter()
                                .collect()
                        }
                        (Index(index), ExecTag::LongArray(entries)) => {
                            execute_nbt_index(*index, entries.len())
                                .map(|i| Cow::Owned(ExecTag::Long(entries[i])))
                                .into_iter()
                                .collect()
                        }
                        (All, ExecTag::IntArray(entries)) => entries
                            .iter()
                            .map(|&v| Cow::Owned(ExecTag::Int(v)))
                            .collect(),
                        (All, ExecTag::ByteArray(entries)) => entries
                            .iter()
                            .map(|&v| Cow::Owned(ExecTag::Byte(v as i8)))
                            .collect(),
                        (All, ExecTag::LongArray(entries)) => entries
                            .iter()
                            .map(|&v| Cow::Owned(ExecTag::Long(v)))
                            .collect(),
                        (Match(pattern, true), ExecTag::List(_, entries)) => entries
                            .iter()
                            .filter(|value| execute_nbt_matches(pattern, value))
                            .map(Cow::Borrowed)
                            .collect(),
                        (Match(pattern, false), value) if execute_nbt_matches(pattern, value) => {
                            vec![Cow::Borrowed(value)]
                        }
                        _ => Vec::new(),
                    }
                })
                .collect();
        }
        values
    }

    fn set(&self, root: &mut ExecTag, value: &ExecTag) -> bool {
        fn set_at(root: &mut ExecTag, path: &[ExecuteNbtNode], value: &ExecTag) -> bool {
            use ExecuteNbtNode::*;
            let Some((node, rest)) = path.split_first() else {
                *root = value.clone();
                return true;
            };
            match (node, root) {
                (Key(key), ExecTag::Compound(fields)) => {
                    if rest.is_empty() {
                        fields.insert(key.clone(), value.clone());
                        return true;
                    }
                    let child = fields.entry(key.clone()).or_insert_with(|| match rest[0] {
                        Index(_) | All | Match(_, true) => ExecTag::List(0, Vec::new()),
                        _ => ExecTag::Compound(Default::default()),
                    });
                    set_at(child, rest, value)
                }
                (Index(index), ExecTag::List(kind, entries))
                    if !rest.is_empty() || *kind == value.id() =>
                {
                    execute_nbt_index(*index, entries.len())
                        .is_some_and(|i| set_at(&mut entries[i], rest, value))
                }
                (All, ExecTag::List(kind, entries)) if rest.is_empty() => {
                    entries.fill(value.clone());
                    if entries.is_empty() {
                        entries.push(value.clone());
                    }
                    *kind = value.id();
                    true
                }
                (All, ExecTag::List(_, entries)) => {
                    entries.iter_mut().fold(false, |changed, entry| {
                        set_at(entry, rest, value) || changed
                    })
                }
                (Match(pattern, true), ExecTag::List(_, entries)) if !rest.is_empty() => entries
                    .iter_mut()
                    .filter(|entry| execute_nbt_matches(pattern, entry))
                    .fold(false, |changed, entry| {
                        set_at(entry, rest, value) || changed
                    }),
                (Match(pattern, false), entry)
                    if !rest.is_empty() && execute_nbt_matches(pattern, entry) =>
                {
                    set_at(entry, rest, value)
                }
                _ => false,
            }
        }
        set_at(root, &self.0, value)
    }
}

fn execute_nbt_index(index: i32, len: usize) -> Option<usize> {
    let index = if index < 0 {
        len.checked_sub(index.unsigned_abs() as usize)?
    } else {
        index as usize
    };
    (index < len).then_some(index)
}

fn execute_nbt_compound(values: impl IntoIterator<Item = (&'static str, ExecTag)>) -> ExecTag {
    ExecTag::Compound(
        values
            .into_iter()
            .map(|(name, value)| (NbtString::text(name), value))
            .collect(),
    )
}

fn execute_nbt_matcher(input: &str) -> Result<(bool, ExecTag), String> {
    let (negated, text) = input
        .strip_prefix('!')
        .map_or((false, input), |text| (true, text));
    let mut reader = brigadier::StringReader::new(text);
    execute_read_snbt(&mut reader, 0)
        .filter(|_| !reader.can_read())
        .map(|tag| (negated, tag))
        .ok_or_else(|| format!("Invalid NBT predicate '{input}'"))
}

fn execute_read_snbt(reader: &mut brigadier::StringReader, depth: u32) -> Option<ExecTag> {
    if depth >= 64 {
        return None;
    }
    reader.skip_whitespace();
    match reader.peek()? {
        '{' => {
            reader.skip();
            reader.skip_whitespace();
            let mut fields = std::collections::BTreeMap::new();
            while reader.peek()? != '}' {
                let name = reader.read_string().ok()?;
                if name.is_empty() {
                    return None;
                }
                reader.skip_whitespace();
                reader.expect(':').ok()?;
                fields.insert(
                    NbtString::text(&name),
                    execute_read_snbt(reader, depth + 1)?,
                );
                reader.skip_whitespace();
                if reader.peek() != Some(',') {
                    break;
                }
                reader.skip();
                reader.skip_whitespace();
            }
            reader.expect('}').ok()?;
            Some(ExecTag::Compound(fields))
        }
        '[' => {
            reader.skip();
            reader.skip_whitespace();
            let array_kind = match (reader.peek(), reader.peek_at(1)) {
                (Some(kind @ ('B' | 'I' | 'L')), Some(';')) => {
                    reader.skip();
                    reader.skip();
                    reader.skip_whitespace();
                    Some(kind)
                }
                _ => None,
            };
            let mut entries = Vec::new();
            while reader.peek()? != ']' {
                let entry = execute_read_snbt(reader, depth + 1)?;
                if entries
                    .first()
                    .is_some_and(|first: &ExecTag| first.id() != entry.id())
                {
                    return None;
                }
                entries.push(entry);
                reader.skip_whitespace();
                if reader.peek() != Some(',') {
                    break;
                }
                reader.skip();
                reader.skip_whitespace();
            }
            reader.expect(']').ok()?;
            match array_kind {
                Some('B') => entries
                    .into_iter()
                    .map(|tag| {
                        if let ExecTag::Byte(v) = tag {
                            Some(v as u8)
                        } else {
                            None
                        }
                    })
                    .collect::<Option<_>>()
                    .map(ExecTag::ByteArray),
                Some('I') => entries
                    .into_iter()
                    .map(|tag| tag.int())
                    .collect::<Option<_>>()
                    .map(ExecTag::IntArray),
                Some('L') => entries
                    .into_iter()
                    .map(|tag| {
                        if let ExecTag::Long(v) = tag {
                            Some(v)
                        } else {
                            None
                        }
                    })
                    .collect::<Option<_>>()
                    .map(ExecTag::LongArray),
                _ => Some(ExecTag::List(
                    entries.first().map_or(0, ExecTag::id),
                    entries,
                )),
            }
        }
        '"' | '\'' => Some(ExecTag::String(NbtString::text(
            &reader.read_quoted_string().ok()?,
        ))),
        _ => {
            let value = reader.read_unquoted_string();
            if value.is_empty() {
                return None;
            }
            let (number, suffix) = value.split_at(value.len() - 1);
            let number = match suffix.to_ascii_lowercase().as_str() {
                "b" => number.parse().ok().map(ExecTag::Byte),
                "s" => number.parse().ok().map(ExecTag::Short),
                "l" => number.parse().ok().map(ExecTag::Long),
                "f" => number
                    .parse::<f32>()
                    .ok()
                    .map(|n| ExecTag::Float(n.to_bits())),
                "d" => number
                    .parse::<f64>()
                    .ok()
                    .map(|n| ExecTag::Double(n.to_bits())),
                _ if value.eq_ignore_ascii_case("true") => Some(ExecTag::Byte(1)),
                _ if value.eq_ignore_ascii_case("false") => Some(ExecTag::Byte(0)),
                _ => value.parse().ok().map(ExecTag::Int).or_else(|| {
                    value
                        .parse::<f64>()
                        .ok()
                        .map(|n| ExecTag::Double(n.to_bits()))
                }),
            };
            Some(number.unwrap_or_else(|| ExecTag::String(NbtString::text(&value))))
        }
    }
}

fn execute_tag_snbt(tag: &ExecTag) -> String {
    fn quoted(text: &NbtString) -> String {
        let text = String::from_utf16_lossy(&text.0);
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }
    match tag {
        ExecTag::End => String::new(),
        ExecTag::Byte(n) => format!("{n}b"),
        ExecTag::Short(n) => format!("{n}s"),
        ExecTag::Int(n) => n.to_string(),
        ExecTag::Long(n) => format!("{n}l"),
        ExecTag::Float(n) => format!("{}f", f32::from_bits(*n)),
        ExecTag::Double(n) => format!("{}d", f64::from_bits(*n)),
        ExecTag::String(text) => quoted(text),
        ExecTag::List(_, tags) => format!(
            "[{}]",
            tags.iter()
                .map(execute_tag_snbt)
                .collect::<Vec<_>>()
                .join(",")
        ),
        ExecTag::ByteArray(values) => format!(
            "[B;{}]",
            values
                .iter()
                .map(|n| format!("{}b", *n as i8))
                .collect::<Vec<_>>()
                .join(",")
        ),
        ExecTag::IntArray(values) => format!(
            "[I;{}]",
            values
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
        ExecTag::LongArray(values) => format!(
            "[L;{}]",
            values
                .iter()
                .map(|n| format!("{n}l"))
                .collect::<Vec<_>>()
                .join(",")
        ),
        ExecTag::Compound(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, value)| { format!("{}:{}", quoted(key), execute_tag_snbt(value)) })
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

fn execute_nbt_matches(pattern: &ExecTag, value: &ExecTag) -> bool {
    match (pattern, value) {
        (ExecTag::Compound(pattern), ExecTag::Compound(value)) => {
            pattern.iter().all(|(key, pattern)| {
                value
                    .get(key)
                    .is_some_and(|actual| execute_nbt_matches(pattern, actual))
            })
        }
        (ExecTag::List(_, pattern), ExecTag::List(_, actual)) if !pattern.is_empty() => {
            pattern.len() <= actual.len()
                && pattern.iter().all(|expected| {
                    actual
                        .iter()
                        .any(|value| execute_nbt_matches(expected, value))
                })
        }
        _ => pattern == value,
    }
}

fn execute_storage() -> &'static LockedMap<String, ExecTag> {
    static STORAGE: Registry<String, ExecTag> = OnceLock::new();
    STORAGE.get_or_init(Mutex::default)
}

fn execute_block_data(dim: DimId, (x, y, z): BlockPos) -> Option<ExecTag> {
    if dim == DimId::Overworld {
        let generated = generated_content().lock().unwrap();
        if let Some(tag) = generated.spawners.get(&(x, y, z)) {
            let mut tag = tag.clone();
            if let ExecTag::Compound(fields) = &mut tag {
                for key in ["x", "y", "z"] {
                    fields.remove(&NbtString::text(key));
                }
            }
            return Some(tag);
        }
    }
    let key = (dim, x, y, z);
    if let Some(furnace) = furnaces().lock().unwrap().get(&key) {
        let slots = HashMap::from([
            (0, furnace.input.clone()),
            (1, furnace.fuel.clone()),
            (2, furnace.output.clone()),
        ]);
        return Some(execute_nbt_compound([
            ("Items", stacks_to_items_nbt(&slots)),
            (
                "BurnTime",
                ExecTag::Short((furnace.burn_left * GAME_TICKS_PER_TICK as i32) as i16),
            ),
            (
                "CookTime",
                ExecTag::Short((furnace.progress * GAME_TICKS_PER_TICK as i32) as i16),
            ),
        ]));
    }
    if let Some(slots) = chests().lock().unwrap().get(&ContainerKey::Block(key)) {
        return Some(execute_nbt_compound([(
            "Items",
            stacks_to_items_nbt(slots),
        )]));
    }
    let state = block_at_dim(dim, x, y, z);
    // Live block entities (beacon, lectern, banner, ...) snapshot through the same
    // store chunk packets and saves use; it is keyed by dimension already.
    if let Some((_, tag)) = live_block_entity_snapshot(key) {
        return Some(tag);
    }
    if is_sign_block_state(state) {
        return Some(execute_sign_block_data(dim, (x, y, z)));
    }
    let state_name = breg()?.canonical_state(state);
    let name = state_name
        .split('[')
        .next()?
        .trim_start_matches("minecraft:");
    matches!(
        name,
        "chest"
            | "trapped_chest"
            | "barrel"
            | "hopper"
            | "dispenser"
            | "dropper"
            | "furnace"
            | "blast_furnace"
            | "smoker"
            | "shulker_box"
    )
    .then(|| execute_nbt_compound([("Items", ExecTag::List(0, Vec::new()))]))
}

/// The sign's stored text projected into the same compound shape the chunk packet's
/// `front_text`/`back_text`/`is_waxed` carries, so `if data`/`store` see what a client
/// would. Lines are JSON-string components, matching `sign_side_nbt`.
fn execute_sign_block_data(dim: DimId, (x, y, z): BlockPos) -> ExecTag {
    let store = signs().lock().unwrap();
    let side = |front: u8| {
        let lines = store.get(&(dim, x, y, z, front)).cloned().unwrap_or_default();
        execute_nbt_compound([
            ("color", ExecTag::String(NbtString::text("white"))),
            ("has_glowing_text", ExecTag::Byte(0)),
            (
                "filtered_messages",
                ExecTag::List(
                    8,
                    [""; 4]
                        .map(|line| ExecTag::String(NbtString::text(line)))
                        .into(),
                ),
            ),
            (
                "messages",
                ExecTag::List(
                    8,
                    lines
                        .iter()
                        .map(|line| ExecTag::String(NbtString::text(&json_text(line))))
                        .collect(),
                ),
            ),
        ])
    };
    execute_nbt_compound([
        ("front_text", side(1)),
        ("back_text", side(0)),
        ("is_waxed", ExecTag::Byte(0)),
    ])
}

fn execute_entity_data(entity: &ExecuteEntity) -> ExecTag {
    let (pos, rotation) = entity.location();
    let mut values = vec![
        ("id", ExecTag::String(NbtString::text(entity.kind()))),
        (
            "Pos",
            ExecTag::List(6, pos.map(|n| ExecTag::Double(n.to_bits())).into()),
        ),
        (
            "Rotation",
            ExecTag::List(
                5,
                rotation
                    .map(|n| ExecTag::Float((n as f32).to_bits()))
                    .into(),
            ),
        ),
    ];
    if let Some(hp) = entity.health() {
        values.push(("Health", ExecTag::Float(hp.to_bits())));
    }
    match entity {
        ExecuteEntity::Player(player) => {
            values.push((
                "Inventory",
                stacks_to_items_nbt(&player.inv.lock().unwrap()),
            ));
        }
        ExecuteEntity::Mob(mob) => {
            values.push((
                "Air",
                ExecTag::Short((*mob.air.lock().unwrap() * GAME_TICKS_PER_TICK as i32) as i16),
            ));
            values.push((
                "Fire",
                ExecTag::Short(
                    (*mob.fire_ticks.lock().unwrap() * GAME_TICKS_PER_TICK as i32) as i16,
                ),
            ));
        }
        ExecuteEntity::Other {
            id,
            kind: "minecraft:item",
            ..
        } => {
            if let Some(item) = drops()
                .lock()
                .unwrap()
                .iter()
                .find(|item| item.eid == *id)
                .and_then(|item| stack_to_nbt(0, &(item.item, item.cnt, item.comps.clone())))
            {
                values.push(("Item", item));
            }
        }
        ExecuteEntity::Other { .. } => {}
    }
    execute_nbt_compound(values)
}

fn execute_read_data(
    source: &ExecuteSource,
    target: &ExecuteDataTarget,
) -> Result<Option<ExecTag>, String> {
    Ok(match target {
        ExecuteDataTarget::Block(pos) => {
            let (x, y, z) = source.block_position(*pos)?;
            let mut data = execute_block_data(source.dimension, (x, y, z));
            if let Some(ExecTag::Compound(fields)) = &mut data {
                for (key, value) in [("x", x), ("y", y), ("z", z)] {
                    fields.insert(NbtString::text(key), ExecTag::Int(value));
                }
            }
            data
        }
        ExecuteDataTarget::Entity(target) => {
            let selected = source.select(target)?;
            match selected.as_slice() {
                [entity] => Some(execute_entity_data(entity)),
                [] => actors().lock().unwrap().get(target).map(|actor| {
                    execute_nbt_compound([(
                        "Inventory",
                        stacks_to_items_nbt(&actor.inv.lock().unwrap()),
                    )])
                }),
                _ => return Err("Expected a single data entity".into()),
            }
        }
        ExecuteDataTarget::Storage(id) => execute_storage().lock().unwrap().get(id).cloned(),
    })
}

#[derive(Clone)]
enum BoundExecuteTarget {
    Score(Vec<String>, String),
    Bossbar(String, ExecuteBossbarField),
    Block(DimId, BlockPos, ExecuteNbtPath, ExecuteNbtType, f64),
    Entity(Arc<Mob>, ExecuteNbtPath, ExecuteNbtType, f64),
    Storage(String, ExecuteNbtPath, ExecuteNbtType, f64),
}

#[derive(Clone)]
struct BoundExecuteStore {
    kind: ExecuteStoreKind,
    target: BoundExecuteTarget,
}

impl BoundExecuteStore {
    fn bind(source: &ExecuteSource, store: &ExecuteStore) -> Result<Self, String> {
        let target = match &store.target {
            ExecuteStoreTarget::Score { targets, objective } => {
                let holders = source.score_holders(targets, objective)?;
                if holders.is_empty() {
                    return Err("No score holders found".into());
                }
                scoreboard()
                    .lock()
                    .unwrap()
                    .require_writable(objective)
                    .map_err(scoreboard_command_error)?;
                BoundExecuteTarget::Score(holders, objective.clone())
            }
            ExecuteStoreTarget::Bossbar { id, field } => {
                if !bossbars().lock().unwrap().contains_key(id) {
                    return Err(format!("Unknown bossbar '{id}'"));
                }
                BoundExecuteTarget::Bossbar(id.clone(), *field)
            }
            ExecuteStoreTarget::Block {
                pos,
                path,
                nbt_type,
                scale,
            } => {
                let pos = source.block_position(*pos)?;
                if execute_block_data(source.dimension, pos).is_none() {
                    return Err("No block entity found".into());
                }
                BoundExecuteTarget::Block(
                    source.dimension,
                    pos,
                    ExecuteNbtPath::parse(path)?,
                    *nbt_type,
                    *scale,
                )
            }
            ExecuteStoreTarget::Entity {
                target,
                path,
                nbt_type,
                scale,
            } => {
                let selected = source.select(target)?;
                match selected.as_slice() {
                    [ExecuteEntity::Mob(mob)] => BoundExecuteTarget::Entity(
                        mob.clone(),
                        ExecuteNbtPath::parse(path)?,
                        *nbt_type,
                        *scale,
                    ),
                    [ExecuteEntity::Player(_)] => return Err("Cannot modify player data".into()),
                    _ => return Err("Expected a single writable data entity".into()),
                }
            }
            ExecuteStoreTarget::Storage {
                id,
                path,
                nbt_type,
                scale,
            } => BoundExecuteTarget::Storage(
                id.clone(),
                ExecuteNbtPath::parse(path)?,
                *nbt_type,
                *scale,
            ),
        };
        Ok(Self {
            kind: store.kind,
            target,
        })
    }

    fn complete(&self, result: Option<i32>) -> Result<(), String> {
        let value = match self.kind {
            ExecuteStoreKind::Result => result.unwrap_or(0),
            ExecuteStoreKind::Success => i32::from(result.is_some()),
        };
        match &self.target {
            BoundExecuteTarget::Score(holders, objective) => {
                let mut board = scoreboard().lock().unwrap();
                let available = board
                    .scores
                    .get(objective)
                    .map_or(SCOREBOARD_ENTRY_LIMIT, |scores| {
                        SCOREBOARD_ENTRY_LIMIT.saturating_sub(scores.len())
                    });
                let new_holders = holders
                    .iter()
                    .filter(|holder| board.get(holder, objective).is_none())
                    .count();
                if new_holders > available {
                    return Err("Too many scores for objective".into());
                }
                for holder in holders {
                    board
                        .set(holder, objective, value)
                        .map_err(scoreboard_command_error)?;
                    broadcast_score(&board, holder, objective);
                }
            }
            BoundExecuteTarget::Bossbar(id, field) => {
                let mut bars = bossbars().lock().unwrap();
                let bar = bars
                    .get_mut(id)
                    .ok_or_else(|| format!("Unknown bossbar '{id}'"))?;
                match field {
                    ExecuteBossbarField::Value => bar.value = value,
                    ExecuteBossbarField::Max => bar.max = value,
                }
                let packet = bossbar_pkt(id, 2, Some(bar));
                for player in client_snapshot()
                    .iter()
                    .filter(|player| bar.shown_to(player))
                {
                    let _ = send_packet(&mut player.sock.lock().unwrap(), 0x0a, packet.clone());
                }
            }
            BoundExecuteTarget::Storage(id, path, ty, scale) => {
                let mut storage = execute_storage().lock().unwrap();
                let mut data = storage
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| execute_nbt_compound([]));
                if !path.set(&mut data, &execute_scaled_tag(value, *ty, *scale)) {
                    return Err("Could not set storage NBT path".into());
                }
                storage.insert(id.clone(), data);
            }
            BoundExecuteTarget::Block(dim, pos, path, ty, scale) => {
                execute_write_block_data(
                    *dim,
                    *pos,
                    path,
                    &execute_scaled_tag(value, *ty, *scale),
                )?;
            }
            BoundExecuteTarget::Entity(mob, path, ty, scale) => {
                execute_write_mob_data(mob, path, &execute_scaled_tag(value, *ty, *scale))?;
            }
        }
        Ok(())
    }
}

fn execute_scaled_tag(result: i32, nbt_type: ExecuteNbtType, scale: f64) -> ExecTag {
    let value = f64::from(result) * scale;
    match nbt_type {
        ExecuteNbtType::Byte => ExecTag::Byte(value as i32 as i8),
        ExecuteNbtType::Short => ExecTag::Short(value as i32 as i16),
        ExecuteNbtType::Int => ExecTag::Int(value as i32),
        ExecuteNbtType::Long => ExecTag::Long(value as i64),
        ExecuteNbtType::Float => ExecTag::Float((value as f32).to_bits()),
        ExecuteNbtType::Double => ExecTag::Double(value.to_bits()),
    }
}

fn execute_nbt_number(value: &ExecTag) -> Option<f64> {
    match value {
        ExecTag::Float(bits) => Some(f64::from(f32::from_bits(*bits))),
        ExecTag::Double(bits) => Some(f64::from_bits(*bits)),
        _ => nbt_int_val(value).map(|value| value as f64),
    }
}

fn execute_write_mob_data(mob: &Mob, path: &ExecuteNbtPath, value: &ExecTag) -> Result<(), String> {
    use ExecuteNbtNode::*;
    let number = execute_nbt_number(value)
        .filter(|number| number.is_finite())
        .ok_or("Invalid entity data value")?;
    let unsupported = || "Unsupported writable entity NBT path".to_owned();
    with_save_guard(|| match path.0.as_slice() {
        [Key(key)] => match key.to_utf8().map_err(|_| unsupported())?.as_str() {
            "Health" => {
                let max = entity_spec_by_id(mob.etype).map_or(f32::MAX, |spec| spec.max_health);
                *mob.hp.lock().unwrap() = (number as f32).clamp(0.0, max);
                Ok(())
            }
            "Air" => {
                *mob.air.lock().unwrap() = number as i16 as i32 / GAME_TICKS_PER_TICK as i32;
                Ok(())
            }
            "Fire" => {
                *mob.fire_ticks.lock().unwrap() = number as i16 as i32 / GAME_TICKS_PER_TICK as i32;
                Ok(())
            }
            _ => Err(unsupported()),
        },
        [Key(key), Index(index)] => {
            let mut pos = mob.pos.lock().unwrap();
            match (
                key.to_utf8().map_err(|_| unsupported())?.as_str(),
                *index,
                value,
            ) {
                ("Pos", 0 | -3, ExecTag::Double(_)) => {
                    pos.0 = number.clamp(-30_000_000.0, 30_000_000.0)
                }
                ("Pos", 1 | -2, ExecTag::Double(_)) => {
                    pos.1 = number.clamp(-20_000_000.0, 20_000_000.0)
                }
                ("Pos", 2 | -1, ExecTag::Double(_)) => {
                    pos.2 = number.clamp(-30_000_000.0, 30_000_000.0)
                }
                ("Rotation", 0 | -2, ExecTag::Float(_)) => pos.3 = number as f32,
                ("Rotation", 1 | -1, ExecTag::Float(_)) => pos.4 = number as f32,
                _ => return Err(unsupported()),
            }
            Ok(())
        }
        _ => Err(unsupported()),
    })
}

fn execute_write_block_data(
    dim: DimId,
    pos: BlockPos,
    path: &ExecuteNbtPath,
    value: &ExecTag,
) -> Result<(), String> {
    let unsupported = || "Unsupported writable block NBT path".to_owned();
    with_save_guard(|| {
        let Some(ExecuteNbtNode::Key(key)) = path.0.first() else {
            return Err(unsupported());
        };
        let key = key.to_utf8().map_err(|_| unsupported())?;
        if matches!(key.as_str(), "id" | "x" | "y" | "z") {
            return Err(unsupported());
        }
        if dim == DimId::Overworld {
            let mut generated = generated_content().lock().unwrap();
            if let Some(data) = generated.spawners.get_mut(&pos) {
                let mut updated = data.clone();
                if !path.set(&mut updated, value) {
                    return Err(unsupported());
                }
                *data = updated;
                return Ok(());
            }
        }
        let (x, y, z) = pos;
        if let Some(furnace) = furnaces().lock().unwrap().get_mut(&(dim, x, y, z)) {
            match (key.as_str(), path.0.len()) {
                ("BurnTime", 1) | ("CookTime", 1) => {
                    let ticks = execute_nbt_number(value).ok_or_else(unsupported)? as i16;
                    let field = if key == "BurnTime" {
                        &mut furnace.burn_left
                    } else {
                        &mut furnace.progress
                    };
                    *field = i32::from(ticks) / GAME_TICKS_PER_TICK as i32;
                }
                ("Items", _) => {
                    let mut slots = HashMap::from([
                        (0, furnace.input.clone()),
                        (1, furnace.fuel.clone()),
                        (2, furnace.output.clone()),
                    ]);
                    execute_write_items(&mut slots, path, value)?;
                    furnace.input = slots.remove(&0).unwrap_or_default();
                    furnace.fuel = slots.remove(&1).unwrap_or_default();
                    furnace.output = slots.remove(&2).unwrap_or_default();
                }
                _ => return Err(unsupported()),
            }
            furnace.dirty = true;
            return Ok(());
        }
        if key == "Items" {
            let mut containers = chests().lock().unwrap();
            if let Some(slots) = containers.get_mut(&ContainerKey::Block((dim, x, y, z))) {
                return execute_write_items(slots, path, value);
            }
        }
        // Live block entities (beacon, lectern, banner, ...) round-trip through the
        // same snapshot/restore pair chunk packets and saves use, in whichever
        // dimension the block lives.
        if let Some((_, mut data)) = live_block_entity_snapshot((dim, x, y, z)) {
            if path.set(&mut data, value) && restore_live_block_entity((dim, x, y, z), &data) {
                return Ok(());
            }
            return Err(unsupported());
        }
        Err(unsupported())
    })
}

fn execute_write_items(
    slots: &mut HashMap<i16, Stack>,
    path: &ExecuteNbtPath,
    value: &ExecTag,
) -> Result<(), String> {
    let original = stacks_to_items_nbt(slots);
    let mut data = execute_nbt_compound([("Items", original.clone())]);
    if !path.set(&mut data, value) {
        return Err("Could not set Items NBT path".into());
    }
    let items = data.get("Items").ok_or("Missing Items tag")?;
    let mut updated = items_nbt_to_stacks(items)?;
    let decoded = items_nbt_to_stacks(&original)?;
    // The NBT projection represents a subset of wire components. A count or
    // slot edit must preserve the original components even when a stack moves.
    let slot = |entry: &ExecTag| {
        entry
            .get("Slot")
            .and_then(nbt_int_val)
            .map(|slot| slot as i16)
    };
    for (before, after) in original
        .list()
        .unwrap_or_default()
        .iter()
        .zip(items.list().unwrap_or_default())
    {
        let (Some(from), Some(to)) = (slot(before), slot(after)) else {
            continue;
        };
        let (Some(previous), Some(stack)) = (slots.get(&from), updated.get_mut(&to)) else {
            continue;
        };
        if before.get("components") == after.get("components") {
            stack.2.clone_from(&previous.2);
        } else if let Some(projected) = decoded.get(&from) {
            for (id, bytes) in &previous.2 {
                if !projected.2.iter().any(|(old, _)| old == id)
                    && !stack.2.iter().any(|(new, _)| new == id)
                {
                    stack.2.push((*id, bytes.clone()));
                }
            }
        }
    }
    *slots = updated;
    Ok(())
}

struct ExecuteReport {
    result: Option<i32>,
    feedback: String,
}

fn run_execute(ctx: &CmdCtx, args: &[&str], s: &mut Option<&Client>) -> String {
    run_execute_inner(ExecuteSource::new(ctx), &args.join(" "), s).feedback
}

fn run_execute_inner(source: ExecuteSource, input: &str, s: &mut Option<&Client>) -> ExecuteReport {
    let evaluated = with_execute_depth(|| {
        let mut command = parse_execute_pipeline(input)?;
        let terminal_feedback = command.run.is_none();
        let mut depth = EXEC_DEPTH.get();
        // A nested execute is another redirect on the same command chain.
        // Stores registered before the redirect still observe each fork.
        while let Some(tail) = command
            .run
            .as_deref()
            .and_then(|run| execute_command_tail(run, "execute"))
        {
            if depth >= EXECUTE_DEPTH_LIMIT {
                return Err("execute depth limit".into());
            }
            depth += 1;
            let nested = parse_execute_pipeline(tail)?;
            command.clauses.extend(nested.clauses);
            command.run = nested.run;
        }
        execute_pipeline(source, &command, terminal_feedback, s)
    });
    evaluated.unwrap_or_else(|feedback| ExecuteReport {
        result: None,
        feedback,
    })
}

fn parse_execute_pipeline(input: &str) -> Result<ExecuteCommand, String> {
    let error = match ExecuteCommand::parse(input) {
        Ok(command) => return Ok(command),
        Err(error) => error,
    };
    // Greedy commands can contain literal quotes or brackets. Retry with a
    // placeholder to let the parser identify the actual run boundary.
    for (index, _) in input.match_indices("run") {
        let (prefix, rest) = input.split_at(index);
        let tail = &rest[3..];
        if (!prefix.is_empty() && !prefix.ends_with(char::is_whitespace))
            || !tail.starts_with(char::is_whitespace)
            || tail.trim().is_empty()
        {
            continue;
        }
        if let Ok(mut command) = ExecuteCommand::parse(&format!("{prefix}run say")) {
            if command.run.as_deref() == Some("say") {
                command.run = Some(tail.trim().to_owned());
                return Ok(command);
            }
        }
    }
    Err(error)
}

fn execute_command_tail<'a>(command: &'a str, name: &str) -> Option<&'a str> {
    let tail = command.trim_start_matches('/').strip_prefix(name)?;
    (tail.is_empty() || tail.starts_with(char::is_whitespace)).then(|| tail.trim_start())
}

fn with_execute_depth<T>(run: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let depth = EXEC_DEPTH.get();
    if depth >= EXECUTE_DEPTH_LIMIT {
        return Err("execute depth limit".into());
    }
    struct RestoreDepth(u32);
    impl std::ops::Drop for RestoreDepth {
        fn drop(&mut self) {
            EXEC_DEPTH.set(self.0);
        }
    }
    let _restore = RestoreDepth(EXEC_DEPTH.replace(depth + 1));
    run()
}

fn execute_pipeline(
    source: ExecuteSource,
    command: &ExecuteCommand,
    terminal_feedback: bool,
    s: &mut Option<&Client>,
) -> Result<ExecuteReport, String> {
    let mut branches = vec![(source, Vec::new())];
    let mut first_error = None;
    let mut tested = false;
    let mut succeeded = 0;
    let mut result = 0i32;
    let mut stored_score = None;
    let terminal = command.run.is_none();
    for (index, clause) in command.clauses.iter().enumerate() {
        let mut next = Vec::new();
        for (source, mut stores) in branches {
            match clause {
                ExecuteClause::Modifier(modifier) => match source.modify(modifier) {
                    Ok(sources) => {
                        next.extend(sources.into_iter().map(|source| (source, stores.clone())))
                    }
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                },
                ExecuteClause::Store(store) => match BoundExecuteStore::bind(&source, store) {
                    Ok(store) => {
                        stores.push(Arc::new(store));
                        next.push((source, stores));
                    }
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                },
                ExecuteClause::Condition(condition) => {
                    match execute_condition(&source, condition) {
                        Ok(value) if terminal && index + 1 == command.clauses.len() => {
                            if let Some(value) = value {
                                succeeded += 1;
                                result = result.wrapping_add(value);
                            }
                            complete_execute_stores(
                                &stores,
                                value,
                                &mut first_error,
                                &mut stored_score,
                            );
                        }
                        Ok(Some(_)) => next.push((source, stores)),
                        Ok(None) => {
                            tested = true;
                        }
                        Err(error) => {
                            if terminal && index + 1 == command.clauses.len() {
                                complete_execute_stores(
                                    &stores,
                                    None,
                                    &mut first_error,
                                    &mut stored_score,
                                );
                            }
                            first_error.get_or_insert(error);
                        }
                    }
                }
            }
            if next.len() > EXECUTE_FORK_LIMIT {
                return Err("execute fork limit".into());
            }
        }
        branches = next;
    }

    if let Some(command) = command.run.as_deref() {
        for (source, stores) in branches {
            let value = match execute_run(&source, command, s) {
                Ok(value) => {
                    succeeded += 1;
                    result = result.wrapping_add(value);
                    Some(value)
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                    None
                }
            };
            complete_execute_stores(&stores, value, &mut first_error, &mut stored_score);
        }
    }

    let feedback = if let Some(error) = first_error {
        error
    } else if terminal_feedback && succeeded > 0 {
        if result > 1 {
            format!("Test passed, count: {result}")
        } else {
            "Test passed".into()
        }
    } else if (terminal || tested) && succeeded == 0 {
        "Test failed".into()
    } else {
        match (succeeded, stored_score) {
            (1, Some(stored_score)) => stored_score,
            _ => format!("executed {succeeded} command(s)"),
        }
    };
    Ok(ExecuteReport {
        result: (succeeded > 0).then_some(result),
        feedback,
    })
}

fn complete_execute_stores(
    stores: &[Arc<BoundExecuteStore>],
    result: Option<i32>,
    first_error: &mut Option<String>,
    stored_score: &mut Option<String>,
) {
    for store in stores {
        match store.complete(result) {
            Err(error) => {
                first_error.get_or_insert(error);
            }
            Ok(()) if stored_score.is_none() && store.kind == ExecuteStoreKind::Result => {
                if let BoundExecuteTarget::Score(holders, objective) = &store.target {
                    let value = result.unwrap_or(0);
                    *stored_score = Some(format!(
                        "Stored {value} in {objective} for {}",
                        holders.join(", ")
                    ));
                }
            }
            Ok(()) => {}
        }
    }
}

fn execute_run(
    source: &ExecuteSource,
    command: &str,
    s: &mut Option<&Client>,
) -> Result<i32, String> {
    let command = command.trim_start_matches('/');
    let (name, tail) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    match name {
        "execute" => {
            let report = run_execute_inner(source.clone(), tail, s);
            return report.result.ok_or(report.feedback);
        }
        "function" => {
            return with_execute_depth(|| {
                let lines = lookup_function(tail.trim()).ok_or("Unknown function")?;
                let mut ran = 0;
                for line in lines {
                    let _ = execute_run(source, &line, s);
                    ran += 1;
                    // `/return` ends the calling function's remaining lines.
                    if line == "return" || line.starts_with("return ") {
                        break;
                    }
                }
                Ok(ran)
            });
        }
        _ => {}
    }
    // #98 — execute tails tokenize with the same quote/bracket-aware splitter
    // as top-level commands, so `execute run setblock x y z chest{...}` keeps
    // its SNBT and quoted arguments intact. Greedy tails (say/tellraw-style
    // malformed quotes) fall back to whitespace: the tokens only feed the
    // coordinate pre-resolution below, and the dispatched command string is
    // re-tokenized by `run_command_ctx`, which reports strict errors itself.
    let mut args: Vec<String> = split_command_args(tail)
        .unwrap_or_else(|_| tail.split_whitespace().map(str::to_owned).collect());
    let parts: Vec<&str> = args.iter().map(String::as_str).collect();
    match (name, parts.as_slice()) {
        ("scoreboard", ["players", _, _, ..]) => return execute_run_scoreboard(source, &parts, s),
        ("tag", [target, action @ ("add" | "remove"), tag]) => {
            let entities = source.select(target)?;
            let mut tags = tags().lock().unwrap();
            let count = entities
                .iter()
                .filter(|entity| {
                    let key = entity.tag_key();
                    if *action == "add" {
                        tags.entry(key).or_default().insert((*tag).into())
                    } else {
                        tags.get_mut(&key).is_some_and(|entry| entry.remove(*tag))
                    }
                })
                .count() as i32;
            return if count > 0 {
                Ok(count)
            } else {
                Err("No entity tags changed".into())
            };
        }
        ("kill", [] | ["@s"]) => {
            if let Some(ExecuteEntity::Mob(mob)) = &source.entity {
                let (x, y, z, ..) = *mob.pos.lock().unwrap();
                kill_mob(mob.eid, mob.etype, x, y, z);
                return Ok(1);
            }
        }
        ("data", ["get", target @ .., path]) => {
            let target = match target {
                ["storage", id] => ExecuteDataTarget::Storage(block_registry::ns_name(id)),
                ["entity", selector] => ExecuteDataTarget::Entity((*selector).into()),
                ["block", x, y, z] => ExecuteDataTarget::Block(ExecutePosition {
                    x: ExecuteCoordinate::parse(x)?,
                    y: ExecuteCoordinate::parse(y)?,
                    z: ExecuteCoordinate::parse(z)?,
                }),
                _ => return Err("Expected a data target and path".into()),
            };
            let data = execute_read_data(source, &target)?.ok_or("No data target found")?;
            return execute_data_result(&ExecuteNbtPath::parse(path)?, &data);
        }
        _ => {}
    }

    // Legacy handlers take world coordinates. Resolve their position arguments
    // here so local coordinates see the branch's rotation and eye anchor.
    let positions: &[(usize, bool)] = match name {
        "setblock" => &[(0, true)],
        "summon" => &[(1, false)],
        "fill" => &[(0, true), (3, true)],
        "clone" => &[(0, true), (3, true), (6, true)],
        "tp" | "teleport" if args.len() >= 4 && ExecuteCoordinate::parse(&args[0]).is_err() => {
            &[(1, false)]
        }
        "tp" | "teleport" => &[(0, false)],
        "data" if parts.get(1) == Some(&"block") => &[(2, true)],
        _ => &[],
    };
    for &(start, block) in positions {
        if let Some(tokens) = args.get_mut(start..start + 3) {
            let position = source.resolve_position(ExecutePosition {
                x: ExecuteCoordinate::parse(&tokens[0])?,
                y: ExecuteCoordinate::parse(&tokens[1])?,
                z: ExecuteCoordinate::parse(&tokens[2])?,
            })?;
            if name == "setblock" {
                check_modification_bounds(source.dimension, execute_block_pos(position))?;
            }
            for (token, value) in tokens.iter_mut().zip(position) {
                *token = if block {
                    value.floor().to_string()
                } else {
                    value.to_string()
                };
            }
        }
    }
    let mut ctx = source.ctx.clone();
    // The outer execute command already passed the permission check. Commands
    // that use only the position can use the legacy console entry point.
    if matches!(name, "setblock" | "fill" | "clone" | "summon" | "say") {
        ctx.me = None;
    }
    struct RestoreDimension(Option<DimId>);
    impl std::ops::Drop for RestoreDimension {
        fn drop(&mut self) {
            EXEC_DIM.set(self.0);
        }
    }
    let _restore = RestoreDimension(EXEC_DIM.replace(Some(source.dimension)));
    let command = if positions.is_empty() {
        format!("/{command}")
    } else {
        format!("/{name} {}", args.join(" "))
    };
    let reply = run_command_ctx(&ctx, &command, s);
    execute_legacy_result(name, &reply).ok_or(reply)
}

fn execute_run_scoreboard(
    source: &ExecuteSource,
    args: &[&str],
    s: &mut Option<&Client>,
) -> Result<i32, String> {
    let [_, action, target, tail @ ..] = args else {
        return Err("Expected a scoreboard player action and target".into());
    };
    let objective = tail.first().copied().unwrap_or("");
    let holders = source.score_holders(target, objective)?;
    let sources = match args {
        ["players", "operation", _, _, _, other, objective] => {
            source.score_holders(other, objective)?
        }
        _ => vec![String::new()],
    };
    if holders.is_empty() || sources.is_empty() {
        return Err("No score holders found".into());
    }
    let mut board = scoreboard().lock().unwrap();
    let mut result = 0i32;
    let mut replies = Vec::new();
    let mut resolved = args.to_vec();
    for holder in &holders {
        resolved[2] = holder;
        for other in &sources {
            if *action == "operation" && resolved.len() == 7 {
                resolved[5] = other;
            }
            let reply = apply_scoreboard_command(&mut board, &resolved)?;
            let value = match *action {
                "set" | "add" | "remove" | "get" | "operation" => {
                    board.get(holder, objective).unwrap_or(0)
                }
                _ => execute_legacy_result("scoreboard", &reply).unwrap_or(1),
            };
            result = result.wrapping_add(value);
            replies.push(reply);
        }
    }
    drop(board);
    if let Some(client) = s {
        let _ = send_packet(
            &mut client.sock.lock().unwrap(),
            0x6c,
            system_chat(&replies.join("; ")),
        );
    }
    Ok(result)
}

fn execute_data_result(path: &ExecuteNbtPath, data: &ExecTag) -> Result<i32, String> {
    let values = path.get(data);
    let [value] = values.as_slice() else {
        return Err("Expected one NBT value".into());
    };
    Ok(match value.as_ref() {
        ExecTag::List(_, values) => values.len() as i32,
        ExecTag::Compound(values) => values.len() as i32,
        ExecTag::ByteArray(values) => values.len() as i32,
        ExecTag::IntArray(values) => values.len() as i32,
        ExecTag::LongArray(values) => values.len() as i32,
        ExecTag::String(value) => value.0.len() as i32,
        number => execute_nbt_number(number)
            .ok_or("Unsupported NBT result")?
            .floor() as i32,
    })
}

// Commands outside the execute evaluator still return their chat feedback.
// Decode the numeric results exposed by that interface and keep success
// distinct from a zero-valued query.
fn execute_legacy_result(name: &str, reply: &str) -> Option<i32> {
    let lower = reply.to_ascii_lowercase();
    if [
        "usage:",
        "unknown ",
        "invalid ",
        "no ",
        "could not ",
        "cannot ",
        "unsupported ",
        "you do not have permission",
        "too many ",
        "mob cap ",
        "expected ",
        "error:",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
        || (name == "bossbar"
            && (reply.ends_with(" already exists") || reply.contains(" must be an integer")))
    {
        return None;
    }
    let parse_first = |text: &str| text.split_whitespace().next()?.parse::<i32>().ok();
    let numeric = match name {
        "time" if reply.starts_with("The ") => {
            reply.rsplit_once(" is ").and_then(|(_, n)| parse_first(n))
        }
        "time" => reply.strip_prefix("Set time to ").and_then(parse_first),
        "fill" | "clone" => reply.split_once(' ').and_then(|(_, n)| parse_first(n)),
        "give" | "experience" | "xp" | "effect" => reply
            .strip_suffix(" target(s)")
            .and_then(|reply| reply.rsplit_once(' '))
            .and_then(|(_, n)| parse_first(n))
            .filter(|&n| n > 0),
        "kill" | "tp" | "teleport" => reply
            .split_once(' ')
            .and_then(|(_, n)| parse_first(n))
            .filter(|&n| n > 0),
        "list" | "seed" => parse_first(reply),
        "bossbar" => reply
            .rsplit_once(" is ")
            .and_then(|(_, n)| match n {
                "true" => Some(1),
                "false" => Some(0),
                _ => parse_first(n),
            })
            .or_else(|| parse_first(reply)),
        "scoreboard" => reply
            .strip_prefix("Reset ")
            .and_then(parse_first)
            .or_else(|| parse_first(reply)),
        // `/forceload query <x> <z>` reports 1 when the chunk is force-loaded,
        // 0 when it is not; the bare query reports the chunk count.
        "forceload" if reply.contains(" is not force loaded") => Some(0),
        "forceload" if reply.contains(" is force loaded") => Some(1),
        "forceload" => parse_first(reply),
        _ => None,
    };
    if matches!(
        name,
        "kill" | "tp" | "teleport" | "fill" | "clone" | "give" | "experience" | "xp" | "effect"
    ) {
        numeric
    } else {
        Some(numeric.unwrap_or(1))
    }
}

// datapack predicates: id -> raw json (parsed at eval — files are small)
fn predicates() -> &'static Mutex<HashMap<String, String>> {
    static R: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn block_tag_has(tag: &str, st: u32, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    let members = block_tags()
        .lock()
        .unwrap()
        .get(tag)
        .cloned()
        .unwrap_or_default();
    for m in members {
        if let Some(inner) = m.strip_prefix('#') {
            if block_tag_has(inner, st, depth + 1) {
                return true;
            }
        } else if let Some((lo, hi)) = block_family_range(&m) {
            if (lo..=hi).contains(&st) {
                return true;
            }
        } else if name_to_state(&m).map(|w| w == st).unwrap_or(false) {
            return true;
        }
    }
    false
}
fn block_pred_match(st: u32, pred: &str) -> bool {
    if let Some(tag) = pred.strip_prefix('#') {
        let tag = if tag.contains(':') {
            tag.to_string()
        } else {
            format!("minecraft:{}", tag)
        };
        return block_tag_has(&tag, st, 0);
    }
    let (name, props) = match pred.split_once('[') {
        Some((n, r)) => (n, r.trim_end_matches(']')),
        None => (pred, ""),
    };
    let name = name.trim_start_matches("minecraft:");
    if let Some(reg) = breg().filter(|reg| reg.name_of(st).trim_start_matches("minecraft:") == name)
    {
        return props.split(',').filter(|p| !p.is_empty()).all(|p| {
            p.split_once('=').is_some_and(|(key, value)| {
                reg.get_prop(st, key.trim()).as_deref() == Some(value.trim())
            })
        });
    }
    match block_family_range(name) {
        Some((lo, hi)) => {
            if !(lo..=hi).contains(&st) {
                return false;
            }
            for kv in props.split(',').filter(|x| !x.is_empty()) {
                if let Some((k, v)) = kv.split_once('=') {
                    // single-prop families like wheat[age=N]: age == offset
                    if name == "wheat" && k.trim() == "age" {
                        if st - 4278 != v.trim().parse().unwrap_or(99) {
                            return false;
                        }
                        continue;
                    }
                    if name == "carrots" && k.trim() == "age" {
                        if st - 4294 != v.trim().parse().unwrap_or(99) {
                            return false;
                        }
                        continue;
                    }
                    if name == "water" && k.trim() == "level" {
                        if st - 80 != v.trim().parse().unwrap_or(99) {
                            return false;
                        }
                        continue;
                    }
                    if !prop_match(name, st, k.trim(), v.trim()) {
                        return false;
                    }
                }
            }
            true
        }
        None => name_to_state(name).map(|w| st == w).unwrap_or(false),
    }
}

#[cfg(test)]
mod execute_dimension_tests {
    use super::*;

    fn execute_console(command: &str) -> String {
        let source = crate::CmdCtx {
            me: None,
            x: 0.5,
            y: 80.0,
            z: 0.5,
        };
        crate::run_command_ctx(&source, command, &mut None)
    }

    #[test]
    fn execute_in_nether_and_end_if_block_reads_dimension_state() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        for (dimension, block_x) in [
            ("minecraft:the_nether", 48),
            ("minecraft:the_end", 52),
        ] {
            assert_eq!(
                execute_console(&format!(
                    "/execute in {dimension} run setblock {block_x} 80 40 minecraft:gold_block"
                )),
                "executed 1 command(s)"
            );
            assert_eq!(
                execute_console(&format!(
                    "/execute in {dimension} if block {block_x} 80 40 minecraft:gold_block"
                )),
                "Test passed"
            );
            assert_eq!(
                execute_console(&format!(
                    "/execute in {dimension} if block {block_x} 80 40 minecraft:stone"
                )),
                "Test failed"
            );
        }
    }

    #[test]
    fn execute_in_nether_if_loaded_and_data_block_use_dimension_state() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        assert_eq!(
            execute_console(
                "/execute in minecraft:the_nether run setblock 56 80 40 minecraft:oak_sign"
            ),
            "executed 1 command(s)"
        );
        // `if loaded` must see the dimension-scoped edit, not just Overworld state.
        assert_eq!(
            execute_console("/execute in minecraft:the_nether if loaded 56 80 40"),
            "Test passed"
        );
        // Sign text lives in the dimension-keyed sign store; the execute data
        // projection reads it the way the chunk packet writes it.
        let mut lines: SignLines = Default::default();
        lines[0] = "Hello".to_owned();
        signs()
            .lock()
            .unwrap()
            .insert((DimId::Nether, 56, 80, 40, 1), lines);
        assert_eq!(
            execute_console(
                "/execute in minecraft:the_nether if data block 56 80 40 front_text.messages[0]"
            ),
            "Test passed"
        );
        assert_eq!(
            execute_console(
                "/execute in minecraft:the_nether if data block 56 80 40 front_text {color:\"white\"}"
            ),
            "Test passed"
        );
        assert_eq!(
            execute_console(
                "/execute in minecraft:the_nether if data block 56 80 40 front_text.messages[7]"
            ),
            "Test failed"
        );
    }
}
