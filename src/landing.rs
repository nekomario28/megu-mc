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
    let fabric = std::env::var_os("MC_SPINE_FABRIC_CLASSPATH");
    let neoforge = std::env::var_os("MC_SPINE_NEOFORGE_CLASSPATH");
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
        let _ = (classpath, loader);
        Err("MC_SPINE_FABRIC_CLASSPATH requires --features fabric-probe".into())
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
    Ok(super::set_block_if(
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
    if mob.etype != 22 || *mob.hp.lock().unwrap() <= 0.0 {
        return false;
    }
    let state = block_at(position.0, position.1, position.2);
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
        position,
        state,
        distance,
    }) {
        Ok(true) => {
            note_block_update(position.0, position.1, position.2, 10);
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
    let (x, y, z, _, _, ground) = *me.pos.lock().unwrap();
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        return;
    }
    let water = name_to_state("water").unwrap_or(u32::MAX);
    let gliding = *me.fall_fly.lock().unwrap();
    let mut peak = me.fall_from.lock().unwrap();
    if !ground && y > *peak {
        *peak = y;
    }
    if !ground
        && (block_at(x.floor() as i32, y.floor() as i32, z.floor() as i32) == water
            || block_at(x.floor() as i32, (y + 1.0).floor() as i32, z.floor() as i32) == water
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
    let state = block_at(position.0, position.1, position.2);
    let wet = block_at(x.floor() as i32, (y - 0.5).floor() as i32, z.floor() as i32) == water
        || block_at(x.floor() as i32, y.floor() as i32, z.floor() as i32) == water;
    let height = if gliding {
        0.6
    } else if *me.sneaking.lock().unwrap() {
        1.5
    } else {
        1.8
    };
    if (4286..=4293).contains(&state) && !wet && *me.hp.lock().unwrap() > 0.0 {
        let random = (crate::world_rng::next_u64() >> 40) as f32 / 16777216.0;
        if eligible(distance as f32, random, true, true, true, 0.6, height) {
            match commit(Trample {
                position,
                state,
                distance: distance as f32,
            }) {
                Ok(true) => note_block_update(position.0, position.1, position.2, 10),
                Ok(false) => {}
                Err(error) => {
                    // Fail closed. Never apply an unreviewed vanilla fallback after callback failure.
                    eprintln!("[mod-compatibility] landing failed: {error}");
                    let _ = me.sock.lock().unwrap().shutdown(std::net::Shutdown::Both);
                    return;
                }
            }
        }
    }
    // Cancellation changes farmland conversion only; the existing damage path still runs.
    if distance > 3.0 && !wet && *me.gm.lock().unwrap() != 1 {
        let mut hp = me.hp.lock().unwrap();
        let damage = (distance - 3.0).ceil() as f32;
        *hp = (*hp - damage * (1.0 - 0.04 * enchant_epf(me, 1) as f32)).max(0.0);
        if *hp <= 0.0 {
            drop(hp);
            totem_check(me);
            hp = me.hp.lock().unwrap();
        }
        let current = *hp;
        drop(hp);
        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x24, hurt_anim(me.eid));
        let _ = send_packet(
            &mut me.sock.lock().unwrap(),
            0x5d,
            health_pkt_c(current, me),
        );
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
    #[test]
    fn stale_landing_does_not_replace_new_block() {
        // This is the only test here that writes to the shared world, so it must
        // hold the world fixture lock or it corrupts another test's world mid-run.
        // It also has to put the cell back afterwards.
        let _world_guard = super::super::WORLD_TEST_LOCK.lock().unwrap();
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
}
