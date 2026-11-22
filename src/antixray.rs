// Anti-xray engine-mode-1 masking and its runtime switch.
//
// This unit is `include!`d by `effects` rather than declared as a root module:
// root `mod` declarations belong to `main.rs` alone. Its API is reachable as
// `crate::effects::` until a root declaration can name it directly.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

/// The runtime switch, default on so existing behaviour and tests are
/// unchanged. [`set_anti_xray_enabled`] flips it for the whole process;
/// [`init_anti_xray_from_env`] seeds it from `MEGU_ANTI_XRAY`.
#[cfg_attr(not(test), allow(dead_code))]
static ANTI_XRAY_ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether the masking pass runs: the runtime switch read with one relaxed
/// load, so the hot path costs nothing extra.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn anti_xray_enabled() -> bool {
    ANTI_XRAY_ENABLED.load(Ordering::Relaxed)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn set_anti_xray_enabled(on: bool) {
    ANTI_XRAY_ENABLED.store(on, Ordering::Relaxed);
}

/// Seed the runtime switch from `MEGU_ANTI_XRAY`: `0`/`false`/`off`/`no`
/// disable; `1`/`true`/`on`/`yes` enable; unset or unrecognised keeps the
/// default, on.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn init_anti_xray_from_env() {
    let on = std::env::var("MEGU_ANTI_XRAY")
        .map(|v| super::anti_xray_value_enabled(&v))
        .unwrap_or(true);
    ANTI_XRAY_ENABLED.store(on, Ordering::Relaxed);
}

/// Rewrite buried hidden states in one 16³ section, the Paper engine-mode-1
/// contract: a hidden block with no `exposes` neighbour — air or any other
/// non-occluding cell — cannot be told apart from the host rock around it, so
/// the wire carries `filler` instead. `origin` is the section's lowest world
/// corner, `outside` reads world states past the section's edge, and the
/// section is indexed `(y << 8) | (z << 4) | x`. Returns the number of masked
/// cells. With anti-xray off this is one atomic load and nothing else: no
/// neighbour reads, no writes.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn mask_chunk(
    origin: [i32; 3],
    section: &mut [u32; 4096],
    hidden: &HashSet<u32>,
    mut outside: impl FnMut(i32, i32, i32) -> u32,
    exposes: impl Fn(u32) -> bool,
    mut filler: impl FnMut(i32, i32, i32) -> u32,
) -> usize {
    if !anti_xray_enabled() {
        return 0;
    }
    let mut masked = 0;
    for ly in 0..16i32 {
        for lz in 0..16i32 {
            for lx in 0..16i32 {
                let index = ((ly << 8) | (lz << 4) | lx) as usize;
                let st = section[index];
                if !hidden.contains(&st) {
                    continue;
                }
                let (wx, wy, wz) = (origin[0] + lx, origin[1] + ly, origin[2] + lz);
                let mut buried = true;
                for (dx, dy, dz) in [
                    (1, 0, 0),
                    (-1, 0, 0),
                    (0, 1, 0),
                    (0, -1, 0),
                    (0, 0, 1),
                    (0, 0, -1),
                ] {
                    let (nx, ny, nz) = (lx + dx, ly + dy, lz + dz);
                    let neighbour = if (0..16).contains(&nx)
                        && (0..16).contains(&ny)
                        && (0..16).contains(&nz)
                    {
                        section[((ny << 8) | (nz << 4) | nx) as usize]
                    } else {
                        outside(wx + dx, wy + dy, wz + dz)
                    };
                    if exposes(neighbour) {
                        buried = false;
                        break;
                    }
                }
                if buried {
                    section[index] = filler(wx, wy, wz);
                    masked += 1;
                }
            }
        }
    }
    masked
}

/// The live entry point's name for [`mask_chunk`]; same signature, same work.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn obfuscate_chunk(
    origin: [i32; 3],
    section: &mut [u32; 4096],
    hidden: &HashSet<u32>,
    outside: impl FnMut(i32, i32, i32) -> u32,
    exposes: impl Fn(u32) -> bool,
    filler: impl FnMut(i32, i32, i32) -> u32,
) -> usize {
    mask_chunk(origin, section, hidden, outside, exposes, filler)
}

#[cfg(test)]
mod antixray_tests {
    use super::*;
    use std::collections::HashSet;

    const AIR: u32 = 0;
    const STONE: u32 = 1;
    const ORE: u32 = 2;
    const DECOY: u32 = 3;

    /// An all-stone section with a buried ore at (5,5,5) and a second ore at
    /// (8,8,8) exposed to an air pocket at (8,8,9).
    fn ore_fixture() -> ([u32; 4096], HashSet<u32>) {
        let mut section = [STONE; 4096];
        section[(5 << 8) | (5 << 4) | 5] = ORE;
        section[(8 << 8) | (8 << 4) | 8] = ORE;
        section[(8 << 8) | (8 << 4) | 9] = AIR;
        (section, HashSet::from([ORE]))
    }

    #[test]
    fn p5b_parity_anti_xray_masks_buried_ore_and_keeps_exposed_ore() {
        let _guard = crate::world_guard();
        set_anti_xray_enabled(true);
        let (mut section, hidden) = ore_fixture();
        let masked = mask_chunk(
            [0, 0, 0],
            &mut section,
            &hidden,
            |_, _, _| STONE,
            |st| st == AIR,
            |_, _, _| DECOY,
        );
        assert_eq!(masked, 1);
        assert_eq!(section[(5 << 8) | (5 << 4) | 5], DECOY);
        assert_eq!(section[(8 << 8) | (8 << 4) | 8], ORE);
        assert_eq!(obfuscate_chunk([0, 0, 0], &mut section, &hidden, |_, _, _| STONE, |st| st == AIR, |_, _, _| DECOY), 0);
    }

    #[test]
    fn p5b_parity_anti_xray_disabled_is_a_clean_passthrough() {
        let _guard = crate::world_guard();
        set_anti_xray_enabled(false);
        let (mut section, hidden) = ore_fixture();
        let snapshot = section;
        // The outside reader must never run when the pass is off; panic if it does.
        let outside = |_: i32, _: i32, _: i32| -> u32 { panic!("disabled mask read the world") };
        assert_eq!(mask_chunk([0, 0, 0], &mut section, &hidden, outside, |st| st == AIR, |_, _, _| DECOY), 0);
        assert_eq!(section, snapshot);
        assert_eq!(obfuscate_chunk([0, 0, 0], &mut section, &hidden, outside, |st| st == AIR, |_, _, _| DECOY), 0);
        assert_eq!(section, snapshot);
        set_anti_xray_enabled(true);
    }

    #[test]
    fn p5b_parity_anti_xray_env_switch_disables_and_defaults_on() {
        let _guard = crate::world_guard();
        for (value, expected) in [
            ("0", false),
            ("false", false),
            ("off", false),
            ("no", false),
            ("1", true),
            ("true", true),
            ("on", true),
            ("yes", true),
        ] {
            std::env::set_var("MEGU_ANTI_XRAY", value);
            init_anti_xray_from_env();
            assert_eq!(anti_xray_enabled(), expected, "{value}");
        }
        std::env::remove_var("MEGU_ANTI_XRAY");
        init_anti_xray_from_env();
        assert!(anti_xray_enabled(), "unset defaults on");
        std::env::set_var("MEGU_ANTI_XRAY", "bogus");
        init_anti_xray_from_env();
        assert!(anti_xray_enabled(), "unrecognised defaults on");
        std::env::remove_var("MEGU_ANTI_XRAY");
        set_anti_xray_enabled(true);
    }
}
