//! Supported player landing operation. Java callbacks run synchronously on one owner.
use super::*;
#[cfg(feature = "java-compat")]
use crate::landing_types::Request;
use crate::landing_types::{Loader, Trample};
#[cfg(feature = "java-compat")]
use std::sync::mpsc::{self, SyncSender};
#[cfg(feature = "java-compat")]
static CALLBACKS: OnceLock<SyncSender<Request>> = OnceLock::new();

pub fn initialize() -> Result<(), String> {
    let fabric = std::env::var_os("MEGU_FABRIC_CLASSPATH");
    let neoforge = std::env::var_os("MEGU_NEOFORGE_CLASSPATH");
    if fabric.is_some() && neoforge.is_some() {
        return Err("Select only one Java loader adapter".into());
    }
    let (classpath, loader) = match (fabric, neoforge) {
        (Some(path), None) => {
            if !cfg!(feature = "fabric-probe") {
                return Err("Fabric adapter requires --features fabric-probe".into());
            }
            (path, Loader::Fabric)
        }
        (None, Some(path)) => {
            if !cfg!(feature = "neoforge-events") {
                return Err("NeoForge adapter requires --features neoforge-events".into());
            }
            (path, Loader::NeoForge)
        }
        _ => return Ok(()),
    };
    #[cfg(not(feature = "java-compat"))]
    {
        let _ = classpath;
        match loader {
            Loader::Fabric => Err("MEGU_FABRIC_CLASSPATH requires --features fabric-probe".into()),
            Loader::NeoForge => {
                Err("MEGU_NEOFORGE_CLASSPATH requires --features neoforge-events".into())
            }
        }
    }
    #[cfg(feature = "java-compat")]
    {
        let classpath = classpath
            .into_string()
            .map_err(|_| "Invalid classpath path")?;
        let (send, receive) = mpsc::sync_channel(64);
        let (ready_send, ready_receive) = mpsc::channel();
        thread::Builder::new()
            .name("java-game-owner".into())
            .spawn(move || {
                super::java_compat::serve(&classpath, loader, receive, ready_send);
            })
            .map_err(|e| e.to_string())?;
        ready_receive
            .recv()
            .map_err(|_| "Java startup thread terminated")??;
        CALLBACKS
            .set(send)
            .map_err(|_| "Java owner already initialized")?;
        Ok(())
    }
}

// Vanilla 1.21.1 div.a(dcw,dtc,jd,bsr,float), offsets 0..85.
// Random is supplied explicitly so boundary tests do not rely on lucky seeds.
pub fn eligible(
    distance: f32,
    random: f32,
    living: bool,
    player: bool,
    mob_griefing: bool,
    width: f32,
    height: f32,
) -> bool {
    distance.is_finite()
        && (0.0..1.0).contains(&random)
        && random < distance - 0.5
        && living
        && (player || mob_griefing)
        && width * width * height > 0.512
}

/// Vanilla 1.21.1 `LivingEntity.calculateFallDamage` safe fall distance. The
/// player attribute (`generic.safe_fall_distance`) defaults to this value; this
/// module only owns the landing path, so the default is pinned here rather than
/// re-reading the attribute table from another module.
const SAFE_FALL_DISTANCE: f32 = 3.0;

/// The `generic.fall_damage_multiplier` attribute total, which
/// `calculateFallDamage` folds in after the `damageMultiplier` argument. Nothing
/// on the landing path modifies it, so the default is pinned here.
const FALL_DAMAGE_MULTIPLIER: f32 = 1.0;

/// The three inputs a block's `fallOn` override may change before
/// `LivingEntity.calculateFallDamage` runs.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FallOn {
    /// Multiplies the fall distance *before* the safe distance is subtracted.
    /// `BedBlock.fallOn` forwards `fallDistance * 0.5` to `Block.fallOn`.
    distance_scale: f32,
    /// Added after the scale, as `PointedDripstoneBlock.fallOn` does.
    distance_offset: f32,
    /// The `damageMultiplier` argument passed to `causeFallDamage`.
    damage_multiplier: f32,
}

const DEFAULT_FALL_ON: FallOn = FallOn {
    distance_scale: 1.0,
    distance_offset: 0.0,
    damage_multiplier: 1.0,
};

/// Every `BedBlock` color in 1.21.1 shares the vanilla `fallOn` override.
const BED_BLOCKS: [&str; 16] = [
    "minecraft:white_bed",
    "minecraft:orange_bed",
    "minecraft:magenta_bed",
    "minecraft:light_blue_bed",
    "minecraft:yellow_bed",
    "minecraft:lime_bed",
    "minecraft:pink_bed",
    "minecraft:gray_bed",
    "minecraft:light_gray_bed",
    "minecraft:cyan_bed",
    "minecraft:purple_bed",
    "minecraft:blue_bed",
    "minecraft:brown_bed",
    "minecraft:green_bed",
    "minecraft:red_bed",
    "minecraft:black_bed",
];

fn is_bed_block(name: &str) -> bool {
    BED_BLOCKS.contains(&name)
}

/// Vanilla 1.21.1 `Block.fallOn` overrides that change fall damage. Other
/// `fallOn` side effects (sounds, bounce velocities) belong to the physics
/// path, not to this damage-only view.
fn fall_on_for_block(name: &str, suppressing_bounce: bool, dripstone_tip_up: bool) -> FallOn {
    match name {
        "minecraft:hay_block" | "minecraft:honey_block" => FallOn {
            distance_scale: 1.0,
            distance_offset: 0.0,
            damage_multiplier: 0.2,
        },
        "minecraft:pointed_dripstone" if dripstone_tip_up => FallOn {
            distance_scale: 1.0,
            distance_offset: 2.0,
            damage_multiplier: 2.0,
        },
        // `SlimeBlock.fallOn`: a sneak-suppressed bounce delegates to the
        // default `Block.fallOn`, otherwise the landing deals no damage.
        "minecraft:slime_block" => FallOn {
            distance_scale: 1.0,
            distance_offset: 0.0,
            damage_multiplier: if suppressing_bounce { 1.0 } else { 0.0 },
        },
        // `BedBlock.fallOn` halves the distance it hands to `Block.fallOn`, so
        // the full 3.0 safe distance still comes off the halved value. This is
        // not a 0.5 multiplier on the damage: a 5-block fall onto a bed is
        // ceil(2.5 - 3.0) = 0, while (5 - 3) * 0.5 would round up to 1.
        _ if is_bed_block(name) => FallOn {
            distance_scale: 0.5,
            distance_offset: 0.0,
            damage_multiplier: 1.0,
        },
        _ => DEFAULT_FALL_ON,
    }
}

/// `LivingEntity.calculateFallDamage`:
/// `Mth.ceil((distance - safeFallDistance) * damageMultiplier *
/// fallDamageMultiplier)`, where `safeFallDistance` is the attribute total
/// (3.0 by default plus the Jump Boost modifier). `Mth.ceil` yields an integer
/// that `causeFallDamage` casts back to a float, so the `f32` result here is
/// that integer; non-positive/NaN results never hurt in vanilla.
fn calculate_fall_damage(
    distance: f32,
    fall_on: FallOn,
    safe_fall_distance: f32,
    jump_boost_bonus: f32,
) -> f32 {
    let distance = distance * fall_on.distance_scale + fall_on.distance_offset;
    let damage = ((distance - safe_fall_distance - jump_boost_bonus)
        * fall_on.damage_multiplier
        * FALL_DAMAGE_MULTIPLIER)
        .ceil();
    if damage.is_finite() && damage > 0.0 {
        damage
    } else {
        0.0
    }
}

/// `MobEffects.JUMP_BOOST` carries an `ADD_VALUE` modifier of `+1.0` per level
/// on `generic.safe_fall_distance`, so every level raises the distance the
/// player can drop without damage by one block. `Eff.id` is the zero-based
/// `MobEffects` index, which the registry table maps back to `jump_boost`.
fn jump_boost_bonus(effects: &[Eff]) -> f32 {
    effects
        .iter()
        .filter(|eff| crate::effects::effect_registry_name(eff.id + 1) == Some("jump_boost"))
        .map(|eff| eff.amp as f32 + 1.0)
        .fold(0.0, f32::max)
}

/// Reads the block at the landing position and folds its `fallOn` override.
/// Pointed dripstone only uses its stalagmite branch for an upward tip.
fn fall_on_at_state(state: u32, suppressing_bounce: bool) -> FallOn {
    let Some(registry) = breg() else {
        return DEFAULT_FALL_ON;
    };
    let name = registry.name_of(state);
    let dripstone_tip_up = name == "minecraft:pointed_dripstone"
        && registry.get_prop(state, "vertical_direction").as_deref() == Some("up")
        && registry.get_prop(state, "thickness").as_deref() == Some("tip");
    fall_on_for_block(name, suppressing_bounce, dripstone_tip_up)
}

/// Blocks whose collision resets vanilla fall distance while the entity is
/// inside them (`makeStuckInBlock`), plus liquids and climbable blocks whose
/// `travel`/`onClimbable` paths reset it every tick.
fn resets_fall_distance(name: &str) -> bool {
    matches!(
        name,
        "minecraft:water"
            | "minecraft:lava"
            | "minecraft:cobweb"
            | "minecraft:powder_snow"
            | "minecraft:sweet_berry_bush"
            | "minecraft:ladder"
            | "minecraft:scaffolding"
            | "minecraft:vine"
            | "minecraft:twisting_vines"
            | "minecraft:twisting_vines_plant"
            | "minecraft:weeping_vines"
            | "minecraft:weeping_vines_plant"
            | "minecraft:cave_vines"
            | "minecraft:cave_vines_plant"
            | "minecraft:bubble_column"
    )
}

fn block_resets_fall_distance(state: u32) -> bool {
    breg().is_some_and(|registry| resets_fall_distance(registry.name_of(state)))
}

fn dim_from_slot(slot: u8) -> DimId {
    DimId::ALL
        .get(usize::from(slot))
        .copied()
        .unwrap_or(DimId::Overworld)
}

fn commit(trample: Trample) -> Result<bool, String> {
    #[cfg(feature = "java-compat")]
    if let Some(callbacks) = CALLBACKS.get() {
        let (send, receive) = mpsc::sync_channel(0);
        callbacks
            .send(Request {
                trample,
                reply: send,
            })
            .map_err(|_| "Java game owner terminated")?;
        return receive.recv().map_err(|_| "Java callback terminated")?;
    }
    Ok(super::set_block_if_dim(
        dim_from_slot(trample.dim),
        trample.position.0,
        trample.position.1,
        trample.position.2,
        10,
        Some(trample.state),
    ))
}

/// Adult cow dimensions are pinned from 1.21.1 bsx static initializer, offsets 948..968.
/// Other mobs are not advertised as supported until their dimensions and landing are validated.
pub fn cow_land(mob: &Arc<Mob>, position: (i32, i32, i32), distance: f32) -> bool {
    if mob.etype != 22 || *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0 {
        return false;
    }
    let dim = DimId::Overworld;
    let state = block_at_dim(dim, position.0, position.1, position.2);
    if !(4286..=4293).contains(&state) {
        return false;
    }
    let random = (crate::world_rng::next_u64() >> 40) as f32 / 16777216.0;
    if !eligible(
        distance,
        random,
        true,
        false,
        GR_MOB_GRIEFING.load(Ordering::Relaxed) != 0,
        0.9,
        1.4,
    ) {
        return false;
    }
    match commit(Trample {
        dim: dim.index() as u8,
        position,
        state,
        distance,
    }) {
        Ok(true) => {
            note_block_update_dim(dim, position.0, position.1, position.2, 10);
            true
        }
        Ok(false) => false,
        Err(error) => {
            eprintln!("[mod-compatibility] cow landing failed: {error}");
            false
        }
    }
}

pub fn player_move(me: &Arc<Client>, previous_ground: bool) {
    let (x, y, z, _, _, ground) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        return;
    }
    let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
    let water = name_to_state("water").unwrap_or(u32::MAX);
    // Block containing a point in the player's current dimension.
    let block_containing = |px: f64, py: f64, pz: f64| {
        block_at_dim(dim, px.floor() as i32, py.floor() as i32, pz.floor() as i32)
    };
    let gliding = *me.fall_fly.lock().unwrap_or_else(|e| e.into_inner());
    let mut peak = me.fall_from.lock().unwrap_or_else(|e| e.into_inner());
    if !ground && y > *peak {
        *peak = y;
    }
    if !ground {
        // Vanilla resets the fall distance every tick while the entity is in a
        // liquid, stuck in a speed-multiplying block, or on a climbable. The two
        // positions the existing liquid probes already sample cover the rest of
        // that set (cobweb/powder snow/sweet berries and the vine family).
        let feet = block_containing(x, y, z);
        let head = block_containing(x, y + 1.0, z);
        let resets = gliding
            || feet == water
            || head == water
            || block_resets_fall_distance(feet)
            || block_resets_fall_distance(head);
        if resets {
            *peak = y;
        }
    }
    if !ground || previous_ground {
        return;
    }
    let distance = *peak - y;
    *peak = y;
    drop(peak);
    let position = (x.floor() as i32, (y - 0.2).floor() as i32, z.floor() as i32);
    let state = block_at_dim(dim, position.0, position.1, position.2);
    let wet = block_containing(x, y - 0.5, z) == water || block_containing(x, y, z) == water;
    let height = if gliding {
        0.6
    } else if *me.sneaking.lock().unwrap_or_else(|e| e.into_inner()) {
        1.5
    } else {
        1.8
    };
    if (4286..=4293).contains(&state) && !wet && *me.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0 {
        let random = (crate::world_rng::next_u64() >> 40) as f32 / 16777216.0;
        if eligible(distance as f32, random, true, true, true, 0.6, height) {
            match commit(Trample {
                dim: dim.index() as u8,
                position,
                state,
                distance: distance as f32,
            }) {
                Ok(true) => note_block_update_dim(dim, position.0, position.1, position.2, 10),
                Ok(false) => {}
                Err(error) => {
                    // Fail closed. Never apply an unreviewed vanilla fallback after callback failure.
                    eprintln!("[mod-compatibility] landing failed: {error}");
                    let _ = me.sock.lock().unwrap_or_else(|e| e.into_inner()).shutdown(std::net::Shutdown::Both);
                    return;
                }
            }
        }
    }
    // Cancellation changes farmland conversion only; the existing damage path still runs.
    if !wet
        && *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1
        && GR_FALL_DAMAGE.load(Ordering::Relaxed) != 0
    {
        let suppressing_bounce = *me.sneaking.lock().unwrap_or_else(|e| e.into_inner());
        // Jump Boost's safe-fall-distance modifier is part of the vanilla
        // `calculateFallDamage` input, so read it before the damage pipeline
        // takes any of its own locks.
        let jump_boost_bonus = {
            let effects = me.effs.lock().unwrap_or_else(|e| e.into_inner());
            jump_boost_bonus(effects.as_slice())
        };
        let damage = calculate_fall_damage(
            distance as f32,
            fall_on_at_state(state, suppressing_bounce),
            SAFE_FALL_DISTANCE,
            jump_boost_bonus,
        );
        if damage <= 0.0 {
            return;
        }
        let was_alive = *me.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0;
        // Typed `fall` source: armor is bypassed, Feather Falling EPF applies,
        // and the hurt/health packets plus Damage Event go out through the
        // shared pipeline.
        let current = live_env_damage(me, damage, "fall").unwrap_or_else(|| *lk(&me.hp));
        // Vanilla `die` on a lethal `fall` source: the death sweep's
        // `lethal_damage_type` never sees this hit (it lands between ticks),
        // so the landing path broadcasts the vanilla fall message itself —
        // once, only when this impact actually killed (a totem save or an
        // already-dead player stays silent).
        if was_alive && current <= 0.0 {
            drop_player_items_on_death(me);
            player_death_broadcast(me.name.as_str(), "fall");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fall_damage(name: &str, distance: f32, sneaking: bool, dripstone_tip_up: bool) -> f32 {
        calculate_fall_damage(
            distance,
            fall_on_for_block(name, sneaking, dripstone_tip_up),
            SAFE_FALL_DISTANCE,
            0.0,
        )
    }

    fn jump_boost_effect(amplifier: i32) -> Eff {
        Eff {
            id: 7,
            amp: amplifier,
            left: 200,
        }
    }

    #[test]
    fn vanilla_1_21_1_fall_on_multipliers_match_bytecode() {
        // Default `Block.fallOn`: causeFallDamage(distance, 1.0), so the safe
        // distance comes off the raw distance.
        assert_eq!(fall_damage("minecraft:stone", 3.0, false, false), 0.0);
        assert_eq!(fall_damage("minecraft:stone", 4.0, false, false), 1.0);
        assert_eq!(fall_damage("minecraft:stone", 10.0, false, false), 7.0);

        // HayBlock.fallOn: 0.2x damage multiplier after the safe distance.
        // (8 - 3) * 0.2 is exactly 1.0; 6 * 0.2 rounds up to 2 like Mth.ceil.
        assert_eq!(fall_damage("minecraft:hay_block", 8.0, false, false), 1.0);
        assert_eq!(fall_damage("minecraft:hay_block", 9.0, false, false), 2.0);
        // 5 - 3 = 2 scaled by 0.2 is 0.4, which Mth.ceil rounds up to 1.
        assert_eq!(fall_damage("minecraft:hay_block", 5.0, false, false), 1.0);

        // HoneyBlock.fallOn: the same 0.2x multiplier as hay.
        assert_eq!(fall_damage("minecraft:honey_block", 8.0, false, false), 1.0);
        assert_eq!(fall_damage("minecraft:honey_block", 9.0, false, false), 2.0);

        // BedBlock.fallOn halves the *distance*, not the damage: every color
        // lands on `Block.fallOn(distance * 0.5)` with the default 1.0
        // multiplier, so the 3.0 safe distance comes off the halved value.
        for bed in BED_BLOCKS {
            assert_eq!(
                fall_damage(bed, 5.0, false, false),
                0.0,
                "{bed}: ceil(2.5 - 3.0) is not a hurt"
            );
            assert_eq!(
                fall_damage(bed, 8.0, false, false),
                1.0,
                "{bed}: ceil(4.0 - 3.0)"
            );
            assert_eq!(fall_damage(bed, 9.0, false, false), 2.0, "{bed}");
        }
        // The distinction matters: as a 0.5 damage multiplier a 5-block drop
        // would round (5 - 3) * 0.5 up to a full point of damage.
        assert_ne!(
            fall_damage("minecraft:red_bed", 5.0, false, false),
            (5.0f32 - SAFE_FALL_DISTANCE) * 0.5
        );

        // SlimeBlock.fallOn: no damage unless sneaking suppresses the bounce,
        // in which case the default 1.0 multiplier returns.
        assert_eq!(
            fall_damage("minecraft:slime_block", 100.0, false, false),
            0.0
        );
        assert_eq!(fall_damage("minecraft:slime_block", 5.0, true, false), 2.0);
        assert_eq!(fall_damage("minecraft:slime_block", 4.0, true, false), 1.0);
        assert_eq!(
            fall_damage("minecraft:slime_block", 100.0, true, false),
            97.0
        );

        // PointedDripstoneBlock.fallOn for an upward tip:
        // causeFallDamage(distance + 2.0, 2.0), i.e. ceil((d + 2 - 3) * 2).
        assert_eq!(
            fall_damage("minecraft:pointed_dripstone", 1.5, false, true),
            1.0
        );
        assert_eq!(
            fall_damage("minecraft:pointed_dripstone", 4.0, false, true),
            6.0
        );
        // A downward tip or a non-tip thickness uses `Block.fallOn` and never
        // picks up the stalagmite extra distance and multiplier.
        assert_eq!(
            fall_damage("minecraft:pointed_dripstone", 4.0, false, false),
            1.0
        );
        assert_eq!(
            fall_damage("minecraft:pointed_dripstone", 1.5, false, false),
            0.0
        );
    }

    #[test]
    fn jump_boost_raises_the_safe_fall_distance_by_one_block_per_level() {
        // `MobEffects.JUMP_BOOST` adds an ADD_VALUE modifier of +1.0 per level
        // on generic.safe_fall_distance, so it behaves exactly like a larger
        // safe distance (or, here, a smaller effective fall).
        assert_eq!(jump_boost_bonus(&[]), 0.0);
        assert_eq!(jump_boost_bonus(&[jump_boost_effect(0)]), 1.0);
        assert_eq!(jump_boost_bonus(&[jump_boost_effect(1)]), 2.0);
        // A non-Jump-Boost effect contributes nothing.
        assert_eq!(
            jump_boost_bonus(&[Eff {
                id: 0,
                amp: 3,
                left: 200,
            }]),
            0.0
        );
        // Duplicate instances (the server keeps one per effect, but a save or
        // a mod can supply more) use the strongest level.
        assert_eq!(
            jump_boost_bonus(&[jump_boost_effect(0), jump_boost_effect(2)]),
            3.0
        );

        let stone = |distance: f32, amplifier: i32| {
            calculate_fall_damage(
                distance,
                fall_on_for_block("minecraft:stone", false, false),
                SAFE_FALL_DISTANCE,
                jump_boost_bonus(&[jump_boost_effect(amplifier)]),
            )
        };
        // Level I turns a 4-block drop into a safe one: 4 - 3 - 1 = 0.
        assert_eq!(stone(4.0, 0), 0.0);
        assert_eq!(stone(5.0, 0), 1.0);
        assert_eq!(stone(6.0, 1), 1.0);
        assert_eq!(stone(6.0, 0), 2.0);

        // The modifier is part of the safe-distance total, so it applies after
        // a `fallOn` override has changed the distance: a bed's halved 8-block
        // drop is ceil(4 - 3 - 1) = 0 and hay's 9-block drop is
        // ceil((9 - 3 - 1) * 0.2) = 1.
        let with_jump_boost = |name: &str, distance: f32| {
            calculate_fall_damage(
                distance,
                fall_on_for_block(name, false, false),
                SAFE_FALL_DISTANCE,
                jump_boost_bonus(&[jump_boost_effect(0)]),
            )
        };
        assert_eq!(with_jump_boost("minecraft:red_bed", 8.0), 0.0);
        assert_eq!(with_jump_boost("minecraft:hay_block", 9.0), 1.0);
        // Stalagmite extra distance and multiplier fold in too:
        // ceil((8 + 2 - 3 - 1) * 2) = 12.
        assert_eq!(
            calculate_fall_damage(
                8.0,
                fall_on_for_block("minecraft:pointed_dripstone", false, true),
                SAFE_FALL_DISTANCE,
                jump_boost_bonus(&[jump_boost_effect(0)]),
            ),
            12.0
        );
    }

    #[test]
    fn fall_damage_is_never_negative_or_nan() {
        let stone = fall_on_for_block("minecraft:stone", false, false);
        assert_eq!(
            calculate_fall_damage(0.0, stone, SAFE_FALL_DISTANCE, 0.0),
            0.0
        );
        assert_eq!(
            calculate_fall_damage(2.5, stone, SAFE_FALL_DISTANCE, 0.0),
            0.0
        );
        assert_eq!(
            calculate_fall_damage(f32::NAN, stone, SAFE_FALL_DISTANCE, 0.0),
            0.0
        );
        assert_eq!(
            calculate_fall_damage(f32::INFINITY, stone, SAFE_FALL_DISTANCE, 0.0),
            0.0
        );
        // Exactly on the boundary Mth.ceil still returns the safe value, so a
        // 4.0 fall is the first point of damage.
        assert_eq!(
            calculate_fall_damage(SAFE_FALL_DISTANCE, stone, SAFE_FALL_DISTANCE, 0.0),
            0.0
        );
        assert_eq!(
            calculate_fall_damage(SAFE_FALL_DISTANCE + 0.5, stone, SAFE_FALL_DISTANCE, 0.0),
            1.0
        );
    }

    #[cfg(megu_vanilla_data)]
    #[test]
    fn pointed_dripstone_tip_up_is_read_from_state_properties() {
        let registry = breg().expect("block registry is available to the binary tests");
        let base = registry.state_of("minecraft:pointed_dripstone", None);
        let tip_up = registry.with_prop(
            registry.with_prop(base, "vertical_direction", "up"),
            "thickness",
            "tip",
        );
        let tip_down = registry.with_prop(
            registry.with_prop(base, "vertical_direction", "down"),
            "thickness",
            "tip",
        );
        let frustum_up = registry.with_prop(
            registry.with_prop(base, "vertical_direction", "up"),
            "thickness",
            "frustum",
        );
        assert_eq!(
            fall_on_at_state(tip_up, false),
            FallOn {
                distance_scale: 1.0,
                distance_offset: 2.0,
                damage_multiplier: 2.0,
            }
        );
        assert_eq!(fall_on_at_state(tip_down, false), DEFAULT_FALL_ON);
        assert_eq!(fall_on_at_state(frustum_up, false), DEFAULT_FALL_ON);
    }

    #[test]
    fn fall_distance_resetting_blocks_cover_1_21_1_set() {
        for name in [
            "minecraft:water",
            "minecraft:lava",
            "minecraft:cobweb",
            "minecraft:powder_snow",
            "minecraft:sweet_berry_bush",
            "minecraft:ladder",
            "minecraft:scaffolding",
            "minecraft:vine",
            "minecraft:twisting_vines",
            "minecraft:weeping_vines",
            "minecraft:cave_vines",
            "minecraft:bubble_column",
        ] {
            assert!(
                resets_fall_distance(name),
                "{name} must reset fall distance"
            );
        }
        assert!(!resets_fall_distance("minecraft:stone"));
        assert!(!resets_fall_distance("minecraft:hay_block"));
    }

    #[test]
    fn vanilla_eligibility_boundaries() {
        assert!(!eligible(0.5, 0.0, true, true, false, 0.6, 1.8));
        assert!(eligible(0.6, 0.09, true, true, false, 0.6, 1.8));
        assert!(!eligible(0.6, 0.11, true, true, false, 0.6, 1.8));
        assert!(!eligible(7.0, 0.99, true, false, false, 0.9, 1.4));
        assert!(eligible(7.0, 0.99, true, false, true, 0.9, 1.4));
        assert!(!eligible(7.0, 0.0, false, false, true, 1.0, 1.0));
        assert!(!eligible(7.0, 0.0, true, true, true, 0.6, 0.6));
        assert!(!eligible(f32::NAN, 0.0, true, true, true, 1.0, 1.0));
    }
    /// The lethal-landing broadcast goes through
    /// `player_death_broadcast(.., "fall")`, so the chat line is the vanilla
    /// 1.21.1 `death.fell.fall` message — `death.fell` fall_variants share
    /// the `fall`/`ender_pearl` message_id.
    #[test]
    fn fall_death_message_is_vanilla_formatted() {
        assert_eq!(
            format_vanilla_death_message("Steve", "fall", None, None),
            "Steve fell from a high place"
        );
        // A lethal pearl teleport reads the same fall_variants line.
        assert_eq!(
            format_vanilla_death_message("Steve", "ender_pearl", None, None),
            "Steve fell from a high place"
        );
    }
    #[test]
    fn stale_landing_does_not_replace_new_block() {
        // This is the only test here that writes to the shared world, so it must
        // hold the world fixture lock or it corrupts another test's world mid-run.
        // It also has to put the cell back afterwards.
        let _world_guard = super::super::WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let p = (811, 71, 811);
        let original = block_at(p.0, p.1, p.2);
        super::super::set_block(p.0, p.1, p.2, 4286);
        super::super::set_block(p.0, p.1, p.2, 1);
        let replaced = super::super::set_block_if(p.0, p.1, p.2, 10, Some(4286));
        let landed = block_at(p.0, p.1, p.2);
        super::super::set_block(p.0, p.1, p.2, original);
        assert!(!replaced, "a stale landing must not replace the new block");
        assert_eq!(landed, 1);
    }
    #[test]
    fn commit_trample_is_dimension_scoped() {
        let _world_guard = super::super::WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let p = (812, 71, 812);
        let orig_ow = block_at_dim(DimId::Overworld, p.0, p.1, p.2);
        let orig_ne = block_at_dim(DimId::Nether, p.0, p.1, p.2);
        super::super::set_block_dim(DimId::Overworld, p.0, p.1, p.2, 4286);
        super::super::set_block_dim(DimId::Nether, p.0, p.1, p.2, 4286);
        let changed = commit(Trample {
            dim: DimId::Nether.index() as u8,
            position: p,
            state: 4286,
            distance: 4.0,
        })
        .expect("nether trample commit");
        let after_ow = block_at_dim(DimId::Overworld, p.0, p.1, p.2);
        let after_ne = block_at_dim(DimId::Nether, p.0, p.1, p.2);
        super::super::set_block_dim(DimId::Overworld, p.0, p.1, p.2, orig_ow);
        super::super::set_block_dim(DimId::Nether, p.0, p.1, p.2, orig_ne);
        assert!(changed, "nether farmland must convert in the Nether");
        assert_eq!(after_ne, 10, "nether cell must become dirt");
        assert_eq!(
            after_ow, 4286,
            "nether trample must not mutate Overworld farmland at the same coordinates"
        );
    }
}
