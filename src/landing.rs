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
    if !ground
        && (block_containing(x, y, z) == water
            || block_containing(x, y + 1.0, z) == water
            || gliding)
    {
        *peak = y;
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
    if distance > 3.0
        && !wet
        && *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1
        && GR_FALL_DAMAGE.load(Ordering::Relaxed) != 0
    {
        let was_alive = *me.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0;
        let damage = (distance - 3.0).ceil() as f32;
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
