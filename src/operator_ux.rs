// operator_ux.rs — operator- & player-facing surfaces: safe world-spawn search,
// the server-list ping payload (favicon + hover sample), LAN discovery, the
// human-readable console/RCON status report, and the always-on load smoother.
// `include!`d into `main.rs`; everything here speaks to admins and clients, so
// the strings are plain language — no internal tuning vocabulary ever leaves
// these functions.

// -------------------------------------------------------------- safe spawn

/// How far out the spawn search may wander, in blocks, and the lattice it walks.
const SPAWN_SEARCH_RADIUS: i32 = 256;
const SPAWN_SEARCH_STEP: i32 = 4;

/// What the ground/space cell at a candidate means for a spawning player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpawnCell {
    /// Standable headroom: any of the three air states.
    Air,
    /// Water, lava, bubble columns and waterlogged states: never spawnable.
    Fluid,
    /// A solid block that hurts on contact (lava-adjacent blocks are already
    /// `Fluid`): cactus, magma, powder snow, fire, berry thorns.
    Danger,
    /// A full, light-blocking block a player can stand on.
    Solid,
    /// Anything else (snow layers, plants, torches): not standable ground.
    Thin,
}

/// Blocks whose contact damages a fresh player; checked by registry name so the
/// list is data, not state-id arithmetic. Fluids are already handled separately.
const SPAWN_DANGER: &[&str] = &[
    "minecraft:cactus",
    "minecraft:magma_block",
    "minecraft:powder_snow",
    "minecraft:fire",
    "minecraft:soul_fire",
    "minecraft:sweet_berry_bush",
];

#[allow(dead_code)]
fn spawn_cell_class(st: u32) -> SpawnCell {
    if st == 0 {
        return SpawnCell::Air;
    }
    // The synthetic id ranges predate the registry load and stay valid after it.
    if fluid_from_block(st).is_some() {
        return SpawnCell::Fluid;
    }
    let Some(reg) = breg() else {
        // Without the registry the only trustworthy facts are air (0) and the
        // fluid ranges; every other nonzero state is treated as standable.
        return SpawnCell::Solid;
    };
    let name = reg.name_of(st);
    if matches!(
        name,
        "minecraft:air" | "minecraft:cave_air" | "minecraft:void_air"
    ) {
        return SpawnCell::Air;
    }
    if reg.is_liquid(st) {
        return SpawnCell::Fluid;
    }
    if SPAWN_DANGER.contains(&name) {
        return SpawnCell::Danger;
    }
    if reg.opacity_of(st) == 15 {
        SpawnCell::Solid
    } else {
        SpawnCell::Thin
    }
}

/// The standable feet y at column (x, z), if any. `surface` is allowed to answer
/// either convention in the tree (top solid y, or first air y), so both feet
/// heights are probed with real block reads: two air cells of headroom over one
/// `Solid` ground cell.
#[allow(dead_code)]
fn safe_feet_y(
    surface: &dyn Fn(i32, i32) -> i32,
    block: &dyn Fn(i32, i32, i32) -> u32,
    x: i32,
    z: i32,
    min_feet_y: i32,
    max_feet_y: i32,
) -> Option<i32> {
    let top = surface(x, z);
    for feet in [top + 1, top] {
        if feet < min_feet_y || feet > max_feet_y {
            continue;
        }
        if spawn_cell_class(block(x, feet, z)) != SpawnCell::Air {
            continue;
        }
        if spawn_cell_class(block(x, feet + 1, z)) != SpawnCell::Air {
            continue;
        }
        if spawn_cell_class(block(x, feet - 1, z)) == SpawnCell::Solid {
            return Some(feet);
        }
    }
    None
}

/// Outward square spiral on a `step` lattice from (0, 0): (0,0), then east,
/// turning left with legs of 1, 1, 2, 2, 3, 3 … cells until the column leaves
/// `radius`. The ring bound is the termination guarantee — it cannot wander off.
#[allow(dead_code)]
fn find_safe_spawn_column(
    surface: &dyn Fn(i32, i32) -> i32,
    block: &dyn Fn(i32, i32, i32) -> u32,
    min_feet_y: i32,
    max_feet_y: i32,
    radius: i32,
    step: i32,
) -> Option<(i32, i32, i32)> {
    const DIRS: [(i32, i32); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];
    let (mut x, mut z) = (0i32, 0i32);
    let mut dir = 0usize;
    let mut leg = 1i32; // cells remaining…
    let mut leg_pos = 0i32; // …walked on this leg
    let mut legs_done = 0u32; // legs at the current length (grows every two)
    loop {
        if let Some(feet) = safe_feet_y(surface, block, x, z, min_feet_y, max_feet_y) {
            return Some((x, feet, z));
        }
        x += DIRS[dir].0 * step;
        z += DIRS[dir].1 * step;
        leg_pos += 1;
        if leg_pos == leg {
            leg_pos = 0;
            dir = (dir + 1) & 3;
            legs_done += 1;
            if legs_done == 2 {
                legs_done = 0;
                leg += 1;
            }
        }
        if x.abs().max(z.abs()) > radius {
            return None;
        }
    }
}

/// The search, then the guaranteed exit: the centre column's surface + 1 when no
/// dry spot exists inside the radius (an all-ocean custom world still spawns the
/// player instead of hanging).
#[allow(dead_code)]
fn safe_spawn_or_fallback(
    surface: &dyn Fn(i32, i32) -> i32,
    block: &dyn Fn(i32, i32, i32) -> u32,
    min_feet_y: i32,
    max_feet_y: i32,
) -> (f64, f64, f64) {
    match find_safe_spawn_column(
        surface,
        block,
        min_feet_y,
        max_feet_y,
        SPAWN_SEARCH_RADIUS,
        SPAWN_SEARCH_STEP,
    ) {
        Some((x, feet, z)) => (x as f64 + 0.5, feet as f64, z as f64 + 0.5),
        None => (0.5, surface(0, 0) as f64 + 1.0, 0.5),
    }
}

/// A safe initial spawn in `dim`, searched over the real world (`height_at_dim`
/// for the column, `block_at_dim` for the cells). Dry solid ground, two air
/// cells of headroom, no fluids or contact-damage blocks, and at or above the
/// Overworld's reasonable-elevation floor of y 62 (other dimensions only have
/// to be inside their own vertical bounds).
#[allow(dead_code)]
fn find_safe_world_spawn(dim: DimId) -> (f64, f64, f64) {
    let min_feet_y = if dim == DimId::Overworld {
        62
    } else {
        dim.min_y() + 1
    };
    let max_feet_y = dim.min_y() + dim.height() - 2;
    let surface = |x: i32, z: i32| height_at_dim(dim, x, z);
    let block = |x: i32, y: i32, z: i32| block_at_dim(dim, x, y, z);
    safe_spawn_or_fallback(&surface, &block, min_feet_y, max_feet_y)
}

// ---------------------------------------------------------- server list ping

/// RFC 4648 base64, standard alphabet, `=` padding — written out so the favicon
/// path needs no external crate.
#[allow(dead_code)]
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The `favicon` payload for the server list, or `None` when no
/// `server-icon.png` sits next to the server. Errors are operator-readable:
/// wrong size, wrong dimensions and unreadable files say so in words.
#[allow(dead_code)]
fn load_server_icon_favicon(path: &std::path::Path) -> Result<Option<String>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!("{} could not be read: {e}", path.display()));
        }
    };
    let name = || path.display().to_string();
    if bytes.len() > 64 * 1024 {
        return Err(format!(
            "{} is {} bytes; a server icon must be at most 64 KiB",
            name(),
            bytes.len()
        ));
    }
    if bytes.len() < 24 || bytes[..8] != *b"\x89PNG\r\n\x1a\n" {
        return Err(format!("{} is not a PNG image", name()));
    }
    if bytes[12..16] != *b"IHDR" {
        return Err(format!("{} is missing its PNG header", name()));
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    if width != 64 || height != 64 {
        return Err(format!(
            "{} must be 64x64 pixels (found {width}x{height})",
            name()
        ));
    }
    Ok(Some(format!(
        "data:image/png;base64,{}",
        base64_encode(&bytes)
    )))
}

/// JSON string literal contents: the same escape set the existing status reply
/// already applies to the motd.
#[allow(dead_code)]
fn status_json_text(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` — the lowercase hyphenated form the
/// vanilla `players.sample` entries carry in `id`.
#[allow(dead_code)]
fn uuid_text(u: &[u8; 16]) -> String {
    let mut s = String::with_capacity(36);
    for (i, b) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The vanilla 1.21.1 (protocol 767) server-list ping body: version, player
/// counts, the hover `sample` (sorted by name, capped at 12 so the list stays
/// readable), the motd, and the icon when one loaded.
#[allow(dead_code)]
fn build_status_json_rich(
    motd: &str,
    max_players: usize,
    online_players: &[(String, [u8; 16])],
    favicon: Option<&str>,
) -> String {
    const SAMPLE_CAP: usize = 12;
    let mut sample: Vec<&(String, [u8; 16])> = online_players.iter().collect();
    sample.sort_by(|a, b| a.0.cmp(&b.0));
    sample.truncate(SAMPLE_CAP);
    let sample_json = sample
        .iter()
        .map(|(name, uuid)| {
            format!(
                "{{\"name\":\"{}\",\"id\":\"{}\"}}",
                status_json_text(name),
                uuid_text(uuid)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let mut json = format!(
        "{{\"version\":{{\"name\":\"1.21.1\",\"protocol\":767}},\
         \"players\":{{\"max\":{max_players},\"online\":{},\"sample\":[{sample_json}]}},\
         \"description\":{{\"text\":\"{}\"}}}}",
        online_players.len(),
        status_json_text(motd)
    );
    if let Some(icon) = favicon {
        json.insert_str(
            json.len() - 1,
            &format!(",\"favicon\":\"{}\"", status_json_text(icon)),
        );
    }
    json
}

// -------------------------------------------------------------- LAN announce

/// Minecraft Java's `LanServerPinger` multicast group and payload period.
const LAN_MULTICAST_ADDR: &str = "224.0.2.60:4445";
const LAN_PING_INTERVAL: Duration = Duration::from_millis(1500);

/// The exact wire payload vanilla's `LanServerPinger` multicasts.
#[allow(dead_code)]
fn format_lan_announcement(motd: &str, port: u16) -> String {
    format!("[MOTD]{motd}[/MOTD][AD]{port}[/AD]")
}

/// Background thread multicasting the LAN payload until `stop()`ed. Sending is
/// best-effort — a network without a multicast route must not kill the pinger.
#[allow(dead_code)]
struct LanAnnouncer {
    halt: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

#[allow(dead_code)]
impl LanAnnouncer {
    fn start(motd: &str, port: u16) -> std::io::Result<Self> {
        let sock = std::net::UdpSocket::bind(("0.0.0.0", 0))?;
        let halt = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&halt);
        let payload = format_lan_announcement(motd, port);
        let thread = thread::Builder::new()
            .name("lan-announcer".to_string())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    let _ = sock.send_to(payload.as_bytes(), LAN_MULTICAST_ADDR);
                    // Wake often enough that stop() lands within ~100 ms.
                    for _ in 0..15 {
                        if flag.load(Ordering::Relaxed) {
                            return;
                        }
                        thread::sleep(LAN_PING_INTERVAL / 15);
                    }
                }
            })?;
        Ok(Self {
            halt,
            thread: Some(thread),
        })
    }

    /// Signal the thread and wait for it to finish. Idempotent via `Drop`.
    fn stop(mut self) {
        self.halt.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl std::ops::Drop for LanAnnouncer {
    fn drop(&mut self) {
        self.halt.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ------------------------------------------------------ operator status line

/// Everything the `status` reply needs, already gathered — the formatter reads
/// fields, it does not interrogate the server.
#[allow(dead_code)]
struct OperatorHealthSnapshot {
    /// Rolling mean tick time in milliseconds.
    mspt_ms: f64,
    /// Observed ticks per second.
    tps: f64,
    players_online: usize,
    players_max: usize,
    player_names: Vec<String>,
    loaded_chunks: usize,
    /// `Some` pins the reported figure; `None` reads the live process memory.
    memory_mb: Option<u64>,
    native_mods: usize,
    compat_mods: usize,
}

/// The plain-language band an admin reads off one number. The thresholds are
/// the same bands the load smoother watches, phrased as words.
#[allow(dead_code)]
fn health_label(mspt_ms: f64) -> &'static str {
    if mspt_ms <= 35.0 {
        "Smooth"
    } else if mspt_ms <= 48.0 {
        "Busy"
    } else {
        "Overloaded"
    }
}

/// Observed TPS implied by a mean tick time: 20.0 while there is headroom.
#[allow(dead_code)]
fn tps_of(mspt_ms: f64) -> f64 {
    if mspt_ms <= 0.0 {
        return 20.0;
    }
    (1000.0 / mspt_ms).min(20.0)
}

/// Resident memory from `/proc/self/statm` (Linux); `None` anywhere else.
#[allow(dead_code)]
fn memory_usage_mb() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // Linux reports statm in 4 KiB pages on every arch this crate targets.
    Some(resident_pages * 4096 / (1024 * 1024))
}

#[allow(dead_code)]
fn memory_line(mem: Option<u64>) -> String {
    match mem {
        Some(mb) => format!("{mb} MB"),
        None => "N/A".to_string(),
    }
}

/// The multi-line console/RCON `status` reply. Numbers are stated in their
/// everyday units (TPS, ms/tick, MB); nothing internal leaks through.
#[allow(dead_code)]
fn format_operator_status(s: &OperatorHealthSnapshot) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "Status: {} ({:.1} TPS, {:.1} ms/tick)",
        health_label(s.mspt_ms),
        s.tps,
        s.mspt_ms
    ));
    let mut players = format!("Players: {}/{}", s.players_online, s.players_max);
    if !s.player_names.is_empty() {
        players.push_str(&format!(" ({})", s.player_names.join(", ")));
    }
    lines.push(players);
    lines.push(format!("Loaded chunks: {}", s.loaded_chunks));
    lines.push(format!(
        "Memory: {}",
        memory_line(s.memory_mb.or_else(memory_usage_mb))
    ));
    let mut mods = Vec::new();
    if s.native_mods > 0 {
        mods.push(format!("{} native", s.native_mods));
    }
    if s.compat_mods > 0 {
        mods.push(format!("{} compatibility mode", s.compat_mods));
    }
    if !mods.is_empty() {
        lines.push(format!("Mods: {}", mods.join(", ")));
    }
    lines.join("\n")
}

// --------------------------------------------------- automatic load throttle

/// What one stage of the smoother permits. `chunk_send_scale` and
/// `mob_spawn_scale` multiply the background work rates; `entity_activation_range`
/// is a block distance; `extra_view_trim` is chunks shaved off the view edge.
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)]
struct LoadBudget {
    chunk_send_scale: f64,
    mob_spawn_scale: f64,
    entity_activation_range: i32,
    extra_view_trim: i32,
}

/// Gentlest-to-tightest stages. The first two only starve work players never
/// watch (chunk stream rate, distant mob spawns); the last two reach what a
/// player could notice — view distance and activation range — and only run
/// while the server is genuinely overloaded.
#[allow(dead_code)]
const LOAD_BUDGETS: [LoadBudget; 5] = [
    LoadBudget {
        chunk_send_scale: 1.0,
        mob_spawn_scale: 1.0,
        entity_activation_range: 32,
        extra_view_trim: 0,
    },
    LoadBudget {
        chunk_send_scale: 0.7,
        mob_spawn_scale: 0.5,
        entity_activation_range: 32,
        extra_view_trim: 0,
    },
    LoadBudget {
        chunk_send_scale: 0.5,
        mob_spawn_scale: 0.25,
        entity_activation_range: 32,
        extra_view_trim: 0,
    },
    LoadBudget {
        chunk_send_scale: 0.4,
        mob_spawn_scale: 0.15,
        entity_activation_range: 24,
        extra_view_trim: 1,
    },
    LoadBudget {
        chunk_send_scale: 0.3,
        mob_spawn_scale: 0.1,
        entity_activation_range: 16,
        extra_view_trim: 2,
    },
];

#[allow(dead_code)]
impl LoadBudget {
    const FULL: LoadBudget = LOAD_BUDGETS[0];
}

/// Weight given to the newest tick in the rolling mean: the average settles
/// over roughly the last ~15 ticks rather than jumping on one spike.
const LOAD_EWMA_ALPHA: f64 = 0.15;
const LOAD_STAGE_MAX: u8 = 4;
/// Over 48 ms/tick the server is overloaded; under 32 ms there is room to give
/// work back. Between 32 and 48 the current stage holds — that deadband is what
/// keeps the budget from flapping.
const LOAD_OVERLOAD_MS: f64 = 48.0;
const LOAD_RECOVER_MS: f64 = 32.0;
/// Inside the 38..=48 warning band only the background stages engage.
const LOAD_WARN_MS: f64 = 38.0;
const LOAD_WARN_STAGE_MAX: u8 = 2;

/// Always-on throttle: feed it one tick's work and read the budget back. No
/// knobs, no wall clock — the caller passes `Duration`s.
#[allow(dead_code)]
struct AutoLoadBalancer {
    ewma_ms: f64,
    primed: bool,
    stage: u8,
}

#[allow(dead_code)]
impl AutoLoadBalancer {
    fn new() -> Self {
        Self {
            ewma_ms: 0.0,
            primed: false,
            stage: 0,
        }
    }

    /// Record one tick and return the budget now in force. One tick moves at
    /// most one stage in one direction, which is what makes the adaptation
    /// smooth rather than oscillating.
    fn observe(&mut self, work: Duration) -> LoadBudget {
        let ms = work.as_secs_f64() * 1000.0;
        self.ewma_ms = if self.primed {
            self.ewma_ms + (ms - self.ewma_ms) * LOAD_EWMA_ALPHA
        } else {
            self.primed = true;
            ms
        };
        if self.ewma_ms > LOAD_OVERLOAD_MS {
            self.stage = (self.stage + 1).min(LOAD_STAGE_MAX);
        } else if self.ewma_ms > LOAD_WARN_MS {
            if self.stage < LOAD_WARN_STAGE_MAX {
                self.stage += 1;
            }
        } else if self.ewma_ms < LOAD_RECOVER_MS {
            self.stage = self.stage.saturating_sub(1);
        }
        self.budget()
    }

    fn budget(&self) -> LoadBudget {
        LOAD_BUDGETS[self.stage as usize]
    }

    fn stage(&self) -> u8 {
        self.stage
    }

    fn mspt(&self) -> f64 {
        self.ewma_ms
    }
}

// ---------------------------------------------------------------- tests ----

#[cfg(test)]
mod operator_ux_tests {
    use super::*;

    fn st(name: &str) -> u32 {
        breg().expect("block registry").state_of(name, None)
    }

    #[test]
    fn safe_spawn_finder_avoids_ocean_and_lava_and_finds_dry_land() {
        let (grass, dirt, stone) = (st("grass_block"), st("dirt"), st("stone"));
        let (water, lava) = (st("water"), st("lava"));
        let (cactus, magma, powder) = (
            st("cactus"),
            st("magma_block"),
            st("powder_snow"),
        );

        // Columns default to open ocean: floor at 40, water to the surface at
        // 63, air above. Hazards planted on the spiral path before the only
        // land cell at (16, 0): a cactus, a lava crust, magma, powder snow.
        let kinds: HashMap<(i32, i32), u8> = [
            ((-4, 0), 3),  // cactus ground
            ((8, 0), 2),   // lava crust
            ((-8, 0), 4),  // magma ground
            ((12, 0), 5),  // powder-snow ground
            ((16, 0), 0),  // the one safe column
        ]
        .into_iter()
        .collect();
        let surface = |x: i32, z: i32| {
            match kinds.get(&(x, z)).copied().unwrap_or(1) {
                1 => 40, // ocean: top solid is the floor
                _ => 64, // everything else tops out at 64
            }
        };
        let block = |x: i32, y: i32, z: i32| -> u32 {
            match kinds.get(&(x, z)).copied().unwrap_or(1) {
                0 => {
                    if y <= 63 {
                        stone
                    } else if y == 64 {
                        grass
                    } else {
                        0
                    }
                }
                1 => {
                    if y <= 40 {
                        dirt
                    } else if y <= 63 {
                        water
                    } else {
                        0
                    }
                }
                2 => {
                    if y < 64 {
                        stone
                    } else if y == 64 {
                        lava
                    } else {
                        0
                    }
                }
                3 => {
                    if y < 64 {
                        stone
                    } else if y == 64 {
                        cactus
                    } else {
                        0
                    }
                }
                4 => {
                    if y < 64 {
                        stone
                    } else if y == 64 {
                        magma
                    } else {
                        0
                    }
                }
                _ => {
                    if y < 64 {
                        stone
                    } else if y == 64 {
                        powder
                    } else {
                        0
                    }
                }
            }
        };

        let found = find_safe_spawn_column(&surface, &block, 62, 250, 256, 4);
        assert_eq!(found, Some((16, 65, 0)), "spiral must land on the dry column");

        // An all-ocean world cannot satisfy the search but still answers fast.
        let ocean_surface = |_x: i32, _z: i32| 40;
        let ocean_block = |_x: i32, y: i32, _z: i32| {
            if y <= 40 {
                dirt
            } else if y <= 63 {
                water
            } else {
                0
            }
        };
        assert_eq!(
            find_safe_spawn_column(&ocean_surface, &ocean_block, 62, 250, 256, 4),
            None
        );
        let fallback = safe_spawn_or_fallback(&ocean_surface, &ocean_block, 62, 250);
        assert_eq!(fallback, (0.5, 41.0, 0.5));

        // The real entry point must always return a finite, in-bounds answer.
        let (x, y, z) = find_safe_world_spawn(DimId::Overworld);
        assert!(x.is_finite() && y.is_finite() && z.is_finite());
        assert!(x.abs() <= 256.5 && z.abs() <= 256.5, "{x},{z}");
    }

    #[test]
    fn server_icon_validates_64x64_png_and_encodes_status_json_with_player_sample() {
        // Hand-built minimal PNG: signature, IHDR length+tag, width, height.
        fn png(w: u32, h: u32) -> Vec<u8> {
            let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
            v.extend_from_slice(&13u32.to_be_bytes());
            v.extend_from_slice(b"IHDR");
            v.extend_from_slice(&w.to_be_bytes());
            v.extend_from_slice(&h.to_be_bytes());
            v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
            v
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("mc-spine-icon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let missing = dir.join("server-icon.png");
        assert_eq!(load_server_icon_favicon(&missing).unwrap(), None);

        std::fs::write(&missing, png(32, 32)).unwrap();
        let err = load_server_icon_favicon(&missing).unwrap_err();
        assert!(err.contains("64x64"), "{err}");
        assert!(err.contains("32x32"), "{err}");

        std::fs::write(&missing, b"not a png at all").unwrap();
        assert!(load_server_icon_favicon(&missing)
            .unwrap_err()
            .contains("not a PNG"));

        std::fs::write(&missing, png(64, 64)).unwrap();
        let favicon = load_server_icon_favicon(&missing)
            .unwrap()
            .expect("64x64 icon accepted");
        assert!(favicon.starts_with("data:image/png;base64,"), "{favicon}");
        let _ = std::fs::remove_dir_all(&dir);

        // The encoder itself: known RFC 4648 vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");

        // Status body: 15 players -> sorted 12-entry hover sample.
        let players: Vec<(String, [u8; 16])> = (0..15u8)
            .map(|i| (format!("P{i:02}"), [i; 16]))
            .collect();
        let json = build_status_json_rich(
            "A \"quoted\" motd",
            20,
            &players,
            Some("data:image/png;base64,AAAA"),
        );
        assert!(json.contains("\"protocol\":767"), "{json}");
        assert!(json.contains("\"max\":20,\"online\":15"), "{json}");
        assert_eq!(json.matches("\"name\":\"P").count(), 12, "{json}");
        assert!(json.contains("\"name\":\"P00\",\"id\":\"00000000-0000-0000-0000-000000000000\""), "{json}");
        assert!(json.contains("\"text\":\"A \\\"quoted\\\" motd\""), "{json}");
        assert!(json.contains("\"favicon\":\"data:image/png;base64,AAAA\""), "{json}");

        let bare = build_status_json_rich("plain", 8, &[], None);
        assert!(!bare.contains("favicon"), "{bare}");
        assert!(bare.contains("\"online\":0,\"sample\":[]"), "{bare}");
    }

    #[test]
    fn lan_announcement_matches_vanilla_motd_ad_format() {
        assert_eq!(
            format_lan_announcement("A Minecraft Server", 25565),
            "[MOTD]A Minecraft Server[/MOTD][AD]25565[/AD]"
        );
        assert_eq!(
            format_lan_announcement("mc-spine", 25599),
            "[MOTD]mc-spine[/MOTD][AD]25599[/AD]"
        );
        // Lifecycle: start on an ephemeral UDP socket, stop joins the thread.
        if let Ok(announcer) = LanAnnouncer::start("mc-spine", 25599) {
            announcer.stop();
        }
    }

    #[test]
    fn operator_status_is_human_readable_and_free_of_developer_jargon() {
        let snap = OperatorHealthSnapshot {
            mspt_ms: 8.4,
            tps: 20.0,
            players_online: 3,
            players_max: 20,
            player_names: vec!["Alice".into(), "Bob".into(), "Carol".into()],
            loaded_chunks: 225,
            memory_mb: Some(142),
            native_mods: 4,
            compat_mods: 1,
        };
        let text = format_operator_status(&snap);
        assert!(text.contains("Status: Smooth (20.0 TPS, 8.4 ms/tick)"), "{text}");
        assert!(text.contains("Players: 3/20 (Alice, Bob, Carol)"), "{text}");
        assert!(text.contains("Loaded chunks: 225"), "{text}");
        assert!(text.contains("Memory: 142 MB"), "{text}");
        assert!(text.contains("Mods: 4 native, 1 compatibility mode"), "{text}");

        // Nothing an operator sees may read like source internals.
        let lower = text.to_lowercase();
        for jargon in [
            "tier", "s1", "s2", "s3", "s4", "s5", "mutex", "slab", "occlu",
            "ewma", "mspt", "registry", "packet",
        ] {
            assert!(!lower.contains(jargon), "jargon {jargon:?} in {text}");
        }

        // Label thresholds: 35 smooth, above that busy, past 48 overloaded.
        assert_eq!(health_label(35.0), "Smooth");
        assert_eq!(health_label(36.0), "Busy");
        assert_eq!(health_label(48.0), "Busy");
        assert_eq!(health_label(49.0), "Overloaded");
        assert_eq!(memory_line(None), "N/A");

        // Pure vanilla: the Mods line disappears rather than printing zeroes.
        let vanilla = OperatorHealthSnapshot {
            mspt_ms: 55.0,
            tps: tps_of(55.0),
            players_online: 0,
            players_max: 20,
            player_names: vec![],
            loaded_chunks: 81,
            memory_mb: None,
            native_mods: 0,
            compat_mods: 0,
        };
        let text = format_operator_status(&vanilla);
        assert!(text.contains("Status: Overloaded"), "{text}");
        assert!(text.contains("Players: 0/20\n"), "{text}");
        assert!(!text.contains("Mods"), "{text}");
        assert!(text.contains("Memory:"), "{text}"); // live /proc read or N/A
    }

    #[test]
    fn auto_load_balancer_throttles_under_pressure_and_recovers_with_hysteresis() {
        let mut bal = AutoLoadBalancer::new();
        for _ in 0..40 {
            bal.observe(Duration::from_millis(20));
        }
        assert!(bal.mspt() < 25.0, "ewma settled near 20 ms: {}", bal.mspt());
        assert_eq!(bal.budget(), LoadBudget::FULL);

        // Warning band: background work trims first, view never touched.
        let mut trimmed_background = false;
        for _ in 0..60 {
            let bud = bal.observe(Duration::from_millis(43));
            assert_eq!(bud.extra_view_trim, 0, "view trim inside the warn band");
            assert_eq!(bud.entity_activation_range, 32);
            if bud.chunk_send_scale < 1.0 && bud.mob_spawn_scale < 1.0 {
                trimmed_background = true;
            }
        }
        assert!(trimmed_background, "background scales trimmed under load");
        assert_eq!(bal.stage(), 2, "warn band caps at the background stages");

        // Genuine overload: now the visible edge trims, one stage per tick.
        let mut saw_view_trim = false;
        for _ in 0..80 {
            if bal.observe(Duration::from_millis(60)).extra_view_trim > 0 {
                saw_view_trim = true;
            }
        }
        assert!(saw_view_trim, "overload reached the view-trim stages");
        let deep = bal.budget();
        assert!(deep.entity_activation_range < 32);
        assert!(deep.extra_view_trim <= 2);

        // Hysteresis deadband: hovering between 32 and 38 changes nothing.
        let held = bal.stage();
        for _ in 0..10 {
            bal.observe(Duration::from_millis(35));
        }
        assert_eq!(bal.stage(), held, "deadband must hold the stage");

        // Recovery below 32 ms restores smoothly: monotone, one step per tick.
        let mut last = held;
        loop {
            let stage_before = bal.stage();
            bal.observe(Duration::from_millis(20));
            let stage_after = bal.stage();
            assert!(stage_after <= stage_before, "never rises while recovering");
            assert!(stage_before - stage_after <= 1, "one step per tick");
            assert!(stage_after <= last);
            last = stage_after;
            if stage_after == 0 {
                break;
            }
        }
        assert_eq!(bal.budget(), LoadBudget::FULL);
    }
}
