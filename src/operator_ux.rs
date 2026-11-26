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
fn safe_spawn_or_fallback_with_relaxed(
    surface: &dyn Fn(i32, i32) -> i32,
    block: &dyn Fn(i32, i32, i32) -> u32,
    min_feet_y: i32,
    max_feet_y: i32,
    relaxed_min_feet_y: i32,
) -> (f64, f64, f64) {
    let found = find_safe_spawn_column(
        surface,
        block,
        min_feet_y,
        max_feet_y,
        SPAWN_SEARCH_RADIUS,
        SPAWN_SEARCH_STEP,
    )
    .or_else(|| {
        (relaxed_min_feet_y < min_feet_y).then(|| {
            find_safe_spawn_column(
                surface,
                block,
                relaxed_min_feet_y,
                max_feet_y,
                SPAWN_SEARCH_RADIUS,
                SPAWN_SEARCH_STEP,
            )
        })?
    });
    match found {
        Some((x, feet, z)) => (x as f64 + 0.5, feet as f64, z as f64 + 0.5),
        None => (0.5, surface(0, 0) as f64 + 1.0, 0.5),
    }
}

#[cfg(all(test, megu_vanilla_data))]
fn safe_spawn_or_fallback(
    surface: &dyn Fn(i32, i32) -> i32,
    block: &dyn Fn(i32, i32, i32) -> u32,
    min_feet_y: i32,
    max_feet_y: i32,
) -> (f64, f64, f64) {
    safe_spawn_or_fallback_with_relaxed(
        surface,
        block,
        min_feet_y,
        max_feet_y,
        min_feet_y,
    )
}

/// A safe initial spawn in `dim`, searched over the real world (`height_at_dim`
/// for the column, `block_at_dim` for the cells). Dry solid ground, two air
/// cells of headroom, no fluids or contact-damage blocks, and at or above the
/// Overworld's reasonable-elevation floor of y 62 (other dimensions only have
/// to be inside their own vertical bounds).
fn find_safe_world_spawn(dim: DimId) -> (f64, f64, f64) {
    let min_feet_y = if dim == DimId::Overworld {
        62
    } else {
        dim.min_y() + 1
    };
    let max_feet_y = dim.min_y() + dim.height() - 2;
    let surface = |x: i32, z: i32| height_at_dim(dim, x, z);
    let block = |x: i32, y: i32, z: i32| block_at_dim(dim, x, y, z);
    safe_spawn_or_fallback_with_relaxed(
        &surface,
        &block,
        min_feet_y,
        max_feet_y,
        dim.min_y() + 1,
    )
}

// ---------------------------------------------------------- server list ping

/// RFC 4648 base64, standard alphabet, `=` padding — written out so the favicon
/// path needs no external crate.
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
fn status_json_text(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` — the lowercase hyphenated form the
/// vanilla `players.sample` entries carry in `id`.
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
fn format_lan_announcement(motd: &str, port: u16) -> String {
    format!("[MOTD]{motd}[/MOTD][AD]{port}[/AD]")
}

/// Background thread multicasting the LAN payload until `stop()`ed. Sending is
/// best-effort — a network without a multicast route must not kill the pinger.
struct LanAnnouncer {
    halt: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

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
fn tps_of(mspt_ms: f64) -> f64 {
    if mspt_ms <= 0.0 {
        return 20.0;
    }
    (1000.0 / mspt_ms).min(20.0)
}

/// Resident memory from `/proc/self/statm` (Linux); `None` anywhere else.
fn memory_usage_mb() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // Linux reports statm in 4 KiB pages on every arch this crate targets.
    Some(resident_pages * 4096 / (1024 * 1024))
}

fn memory_line(mem: Option<u64>) -> String {
    match mem {
        Some(mb) => format!("{mb} MB"),
        None => "N/A".to_string(),
    }
}

/// The multi-line console/RCON `status` reply. Numbers are stated in their
/// everyday units (TPS, ms/tick, MB); nothing internal leaks through.
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
/// One tick this far past the target (3× the 50 ms budget) is a burst the
/// ~15-tick EWMA cannot answer in time — a player join streaming
/// worldgen+encode inside the tick measures ~1.9 s. Jump straight to the
/// background-starvation band instead of escalating one stage per tick.
const LOAD_CATASTROPHE_MS: f64 = 150.0;

/// Always-on throttle: feed it one tick's work and read the budget back. No
/// knobs, no wall clock — the caller passes `Duration`s.
struct AutoLoadBalancer {
    ewma_ms: f64,
    primed: bool,
    stage: u8,
}

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
        if ms > LOAD_CATASTROPHE_MS {
            self.stage = self.stage.max(LOAD_WARN_STAGE_MAX);
        } else if self.ewma_ms > LOAD_OVERLOAD_MS {
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

// ----------------------------------------------------- GameSpy4 (GS4) query
//
// Vanilla's UDP query protocol: every datagram opens with the magic bytes
// 0xFE 0xFD, a packet type, and a session id. A Handshake (0x09) issues a
// per-IP challenge token with a 30 second lease; a Stat request (0x00) must
// echo a live token back — 11 bytes asks for the basic stat line, 15 bytes
// for the full K/V block plus the online player list.

/// The two magic bytes every GS4 datagram opens with.
const GS4_MAGIC: [u8; 2] = [0xFE, 0xFD];
const GS4_TYPE_STAT: u8 = 0x00;
const GS4_TYPE_HANDSHAKE: u8 = 0x09;
/// Vanilla masks every session id down to 0x0F0F0F0F on the way in and echoes
/// the masked value back in each reply.
const GS4_SESSION_MASK: i32 = 0x0F0F0F0F;
/// A challenge token stays valid for 30 seconds per the vanilla spec.
const GS4_CHALLENGE_TTL: Duration = Duration::from_secs(30);
/// Requests larger than this are dropped unread — no valid query comes close.
const GS4_MAX_DATAGRAM: usize = 1460;
/// Bound on outstanding challenge entries so a handshake flood cannot grow
/// the map without limit; expired entries are swept before the cap applies.
const GS4_CHALLENGE_CAP: usize = 4096;

/// Everything a stat reply reports, gathered by the caller — the handler never
/// reaches into live server state itself.
#[derive(Clone, Debug, Default, PartialEq)]
struct Gs4StatusSnapshot {
    /// Server MOTD; the `hostname` key of the full reply.
    motd: String,
    /// Vanilla reports "SMP" for survival multiplayer.
    gametype: String,
    /// "MINECRAFT" in vanilla's reply.
    game_id: String,
    /// The server's release name, e.g. "1.21.1".
    version: String,
    /// Vanilla's own server reports an empty plugin list.
    plugins: String,
    /// The world/map name, e.g. "world".
    map: String,
    max_players: usize,
    /// Online player names; `numplayers` is their count.
    players: Vec<String>,
    host_port: u16,
    host_ip: String,
}

/// A vanilla-flavoured snapshot: gametype "SMP", game_id "MINECRAFT", no
/// plugins — the three fields a pure-vanilla server never varies.
fn gs4_vanilla_snapshot(
    motd: String,
    map: String,
    players: Vec<String>,
    max_players: usize,
    host_port: u16,
    host_ip: String,
) -> Gs4StatusSnapshot {
    Gs4StatusSnapshot {
        motd,
        gametype: "SMP".to_string(),
        game_id: "MINECRAFT".to_string(),
        version: "1.21.1".to_string(),
        plugins: String::new(),
        map,
        max_players,
        players,
        host_port,
        host_ip,
    }
}

/// One issued challenge token and when it was minted.
#[derive(Clone, Copy, Debug)]
struct Gs4Challenge {
    token: i32,
    issued_at: Instant,
}

/// The sans-IO GS4 core: per-IP challenge tokens and byte-exact replies. Every
/// clock read arrives as an `Instant` argument, so expiry is testable without
/// sleeping and the token stream is deterministic per handler salt — no
/// wall-clock read anywhere in the protocol path.
struct Gs4QueryHandler {
    challenges: HashMap<std::net::IpAddr, Gs4Challenge>,
    /// Monotone issuance counter: the moving input of the token hash, so a
    /// re-issued challenge for the same address mints a fresh value.
    issued: u64,
    /// Per-handler salt: a token minted by one server cannot be replayed on a
    /// differently seeded one.
    salt: u64,
}

impl Gs4QueryHandler {
    fn new(salt: u64) -> Self {
        Self {
            challenges: HashMap::new(),
            issued: 0,
            salt,
        }
    }

    /// FNV-1a over the salt, the client address and the issuance counter —
    /// deterministic per handler, different per issue, always a positive
    /// decimal string like vanilla's integer tokens.
    fn mint_token(&self, ip: std::net::IpAddr) -> i32 {
        let mut h = megu_core::hash::Fnv1a64::new();
        h.update(&self.salt.to_be_bytes());
        match ip {
            std::net::IpAddr::V4(v4) => h.update(&v4.octets()),
            std::net::IpAddr::V6(v6) => h.update(&v6.octets()),
        }
        h.update(&self.issued.to_be_bytes());
        (h.finish() & 0x7fff_ffff) as i32
    }

    /// The stored token for `ip` is honoured while it is younger than the TTL
    /// and matches `challenge` exactly. An expired entry is evicted on sight so
    /// the map cannot accumulate dead leases.
    fn challenge_valid(&mut self, ip: std::net::IpAddr, challenge: i32, now: Instant) -> bool {
        let Some(entry) = self.challenges.get(&ip).copied() else {
            return false;
        };
        if now.duration_since(entry.issued_at) >= GS4_CHALLENGE_TTL {
            self.challenges.remove(&ip);
            return false;
        }
        entry.token == challenge
    }

    /// Handshake (0x09): mint (or, inside the lease, reuse) the address's
    /// token and answer `[0x09][session_id][token\0]`. `None` when the
    /// challenge table is genuinely saturated — a handshake flood fails
    /// closed rather than growing the map forever.
    fn handle_handshake(
        &mut self,
        ip: std::net::IpAddr,
        session_id: i32,
        now: Instant,
    ) -> Option<Vec<u8>> {
        if self.challenges.len() >= GS4_CHALLENGE_CAP {
            self.challenges
                .retain(|_, c| now.duration_since(c.issued_at) < GS4_CHALLENGE_TTL);
            if self.challenges.len() >= GS4_CHALLENGE_CAP {
                return None;
            }
        }
        let token = match self.challenges.get(&ip) {
            Some(entry) if now.duration_since(entry.issued_at) < GS4_CHALLENGE_TTL => {
                entry.token
            }
            _ => {
                self.issued += 1;
                let token = self.mint_token(ip);
                self.challenges.insert(
                    ip,
                    Gs4Challenge {
                        token,
                        issued_at: now,
                    },
                );
                token
            }
        };
        let mut reply = Vec::with_capacity(16);
        reply.push(GS4_TYPE_HANDSHAKE);
        reply.extend_from_slice(&session_id.to_be_bytes());
        reply.extend_from_slice(token.to_string().as_bytes());
        reply.push(0);
        Some(reply)
    }

    /// One datagram in, maybe one reply out. `None` means dropped: bad magic,
    /// an unknown type, a truncated or oversized packet, a stat request of a
    /// size that is neither basic nor full, or a challenge check that failed.
    /// `snapshot` is consulted only after a stat request's token validates.
    fn handle_datagram(
        &mut self,
        src_ip: std::net::IpAddr,
        packet: &[u8],
        now: Instant,
        snapshot: &dyn Fn() -> Gs4StatusSnapshot,
    ) -> Option<Vec<u8>> {
        if packet.len() < 7 || packet.len() > GS4_MAX_DATAGRAM {
            return None;
        }
        if packet[0..2] != GS4_MAGIC {
            return None;
        }
        let session_id =
            i32::from_be_bytes(packet[3..7].try_into().unwrap()) & GS4_SESSION_MASK;
        match packet[2] {
            GS4_TYPE_HANDSHAKE => self.handle_handshake(src_ip, session_id, now),
            GS4_TYPE_STAT => {
                let full = match packet.len() {
                    11 => false,
                    15 => true,
                    _ => return None,
                };
                let challenge = i32::from_be_bytes(packet[7..11].try_into().unwrap());
                if !self.challenge_valid(src_ip, challenge, now) {
                    return None;
                }
                let snap = snapshot();
                Some(if full {
                    gs4_full_stat(session_id, &snap)
                } else {
                    gs4_basic_stat(session_id, &snap)
                })
            }
            _ => None,
        }
    }
}

/// The 11-byte basic stat: `[0x00][session_id]` then MOTD, gametype, map and
/// both player counts as `\0`-terminated text, the port as a little-endian
/// u16 (the one non-text field on the wire) and the host address.
fn gs4_basic_stat(session_id: i32, snap: &Gs4StatusSnapshot) -> Vec<u8> {
    let num = snap.players.len().to_string();
    let max = snap.max_players.to_string();
    let mut out = Vec::new();
    out.push(GS4_TYPE_STAT);
    out.extend_from_slice(&session_id.to_be_bytes());
    for field in [
        snap.motd.as_str(),
        snap.gametype.as_str(),
        snap.map.as_str(),
        num.as_str(),
        max.as_str(),
    ] {
        out.extend_from_slice(field.as_bytes());
        out.push(0);
    }
    out.extend_from_slice(&snap.host_port.to_le_bytes());
    out.extend_from_slice(snap.host_ip.as_bytes());
    out.push(0);
    out
}

/// The full stat: the `splitnum` K/V section in vanilla's fixed key order, then
/// the `player_` section with every online name.
fn gs4_full_stat(session_id: i32, snap: &Gs4StatusSnapshot) -> Vec<u8> {
    let num = snap.players.len().to_string();
    let max = snap.max_players.to_string();
    let port = snap.host_port.to_string();
    let mut out = Vec::new();
    out.push(GS4_TYPE_STAT);
    out.extend_from_slice(&session_id.to_be_bytes());
    out.extend_from_slice(b"splitnum\x00\x80\x00");
    for (key, value) in [
        ("hostname", snap.motd.as_str()),
        ("gametype", snap.gametype.as_str()),
        ("game_id", snap.game_id.as_str()),
        ("version", snap.version.as_str()),
        ("plugins", snap.plugins.as_str()),
        ("map", snap.map.as_str()),
        ("numplayers", num.as_str()),
        ("maxplayers", max.as_str()),
        ("hostport", port.as_str()),
        ("hostip", snap.host_ip.as_str()),
    ] {
        out.extend_from_slice(key.as_bytes());
        out.push(0);
        out.extend_from_slice(value.as_bytes());
        out.push(0);
    }
    out.push(0);
    out.extend_from_slice(b"\x01player_\x00\x00");
    for name in &snap.players {
        out.extend_from_slice(name.as_bytes());
        out.push(0);
    }
    out.push(0);
    out
}

/// A per-process salt for the live listener's token stream: the pid plus a
/// stack address — no clock read, and tokens minted here cannot be replayed
/// against a differently seeded handler.
fn gs4_runner_salt() -> u64 {
    let marker = 0u8;
    (u64::from(std::process::id()) << 32)
        | (&marker as *const u8 as usize as u64 & 0xffff_ffff)
}

/// Bind the GS4 listener on `bind_addr` and serve datagrams until `stop_flag`
/// goes true. Returns the bound address — the port actually taken when the
/// bind asked for `:0` — and the thread's join handle. The socket polls the
/// flag every ~50 ms so `stop` lands promptly; send errors never kill the
/// listener, exactly as the LAN announcer treats its multicast route.
fn spawn_gs4_query_thread(
    bind_addr: &str,
    snapshot_fn: Arc<dyn Fn() -> Gs4StatusSnapshot + Send + Sync>,
    stop_flag: Arc<AtomicBool>,
) -> std::io::Result<(std::net::SocketAddr, thread::JoinHandle<()>)> {
    let sock = std::net::UdpSocket::bind(bind_addr)?;
    sock.set_read_timeout(Some(Duration::from_millis(50)))?;
    let bound = sock.local_addr()?;
    let join = thread::Builder::new()
        .name("gs4-query".to_string())
        .spawn(move || {
            let mut handler = Gs4QueryHandler::new(gs4_runner_salt());
            // One byte past the cap: a larger datagram still lands (truncated
            // to the buffer) and is dropped by the size gate, never answered.
            let mut buf = [0u8; GS4_MAX_DATAGRAM + 1];
            while !stop_flag.load(Ordering::Relaxed) {
                match sock.recv_from(&mut buf) {
                    Ok((n, src)) if n <= GS4_MAX_DATAGRAM => {
                        if let Some(reply) = handler.handle_datagram(
                            src.ip(),
                            &buf[..n],
                            Instant::now(),
                            &*snapshot_fn,
                        ) {
                            let _ = sock.send_to(&reply, src);
                        }
                    }
                    Ok(_) => {} // oversized datagram: dropped
                    Err(_) => {} // timeout poll or a transient socket error
                }
            }
        })?;
    Ok((bound, join))
}

// ---------------------------------------------------------------- tests ----

#[cfg(test)]
mod operator_ux_tests {
    use super::*;

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn st(name: &str) -> u32 {
        breg().expect("block registry").state_of(name, None)
    }

    #[test]
    #[cfg(megu_vanilla_data)]
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
            .join(format!("megu-icon-{}", std::process::id()));
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
            format_lan_announcement("megu", 25599),
            "[MOTD]megu[/MOTD][AD]25599[/AD]"
        );
        // Lifecycle: start on an ephemeral UDP socket, stop joins the thread.
        if let Ok(announcer) = LanAnnouncer::start("megu", 25599) {
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

    #[test]
    fn auto_load_balancer_catastrophic_tick_jumps_to_background_band() {
        let mut bal = AutoLoadBalancer::new();
        for _ in 0..10 {
            bal.observe(Duration::from_millis(20));
        }
        // A single ~1.9 s join-storm tick: the EWMA barely moves, but the
        // stage must engage the background-starvation band immediately
        // instead of climbing one stage per tick for ~15 ticks.
        let bud = bal.observe(Duration::from_millis(1900));
        assert_eq!(bal.stage(), LOAD_WARN_STAGE_MAX);
        assert!(bud.chunk_send_scale < 1.0);
        assert!(bud.mob_spawn_scale < 1.0);
        // The visible stages stay untouched by one freak tick.
        assert_eq!(bud.entity_activation_range, 32);
        assert_eq!(bud.extra_view_trim, 0);
        // The burst keeps the EWMA over 48 ms for a while — the stage may
        // still climb during the tail — but healthy ticks recover it fully.
        for _ in 0..80 {
            bal.observe(Duration::from_millis(20));
        }
        assert_eq!(bal.stage(), 0);
    }

    // ------------------------------------------------------ GS4 query ----

    fn p4a_gs4_snapshot() -> Gs4StatusSnapshot {
        gs4_vanilla_snapshot(
            "A Minecraft Server".to_string(),
            "world".to_string(),
            vec!["Alice".to_string(), "Bob".to_string()],
            20,
            25565,
            "127.0.0.1".to_string(),
        )
    }

    fn p4a_gs4_handshake(session_id: i32) -> Vec<u8> {
        let mut p = vec![0xFE, 0xFD, 0x09];
        p.extend_from_slice(&session_id.to_be_bytes());
        p
    }

    fn p4a_gs4_stat(session_id: i32, challenge: i32, full: bool) -> Vec<u8> {
        let mut p = vec![0xFE, 0xFD, 0x00];
        p.extend_from_slice(&session_id.to_be_bytes());
        p.extend_from_slice(&challenge.to_be_bytes());
        if full {
            p.extend_from_slice(&[0, 0, 0, 0]);
        }
        p
    }

    /// The decimal token out of a handshake reply's `[0x09][sid][token\0]`.
    fn p4a_gs4_token_of(reply: &[u8]) -> i32 {
        assert_eq!(reply[0], 0x09, "handshake reply type");
        assert_eq!(reply.last(), Some(&0), "handshake reply terminator");
        std::str::from_utf8(&reply[5..reply.len() - 1])
            .unwrap()
            .parse()
            .unwrap()
    }

    fn p4a_no_snapshot() -> impl Fn() -> Gs4StatusSnapshot {
        || -> Gs4StatusSnapshot {
            panic!("a handshake must never need the status snapshot")
        }
    }

    /// The byte-exact basic-stat body for `p4a_gs4_snapshot` at `session_id`.
    fn p4a_expected_basic(session_id: i32) -> Vec<u8> {
        let mut out = vec![0x00];
        out.extend_from_slice(&session_id.to_be_bytes());
        for field in ["A Minecraft Server", "SMP", "world", "2", "20"] {
            out.extend_from_slice(field.as_bytes());
            out.push(0);
        }
        out.extend_from_slice(&25565u16.to_le_bytes());
        out.extend_from_slice(b"127.0.0.1");
        out.push(0);
        out
    }

    /// The byte-exact full-stat body for `p4a_gs4_snapshot` at `session_id`.
    fn p4a_expected_full(session_id: i32) -> Vec<u8> {
        let mut out = vec![0x00];
        out.extend_from_slice(&session_id.to_be_bytes());
        out.extend_from_slice(b"splitnum\x00\x80\x00");
        for (k, v) in [
            ("hostname", "A Minecraft Server"),
            ("gametype", "SMP"),
            ("game_id", "MINECRAFT"),
            ("version", "1.21.1"),
            ("plugins", ""),
            ("map", "world"),
            ("numplayers", "2"),
            ("maxplayers", "20"),
            ("hostport", "25565"),
            ("hostip", "127.0.0.1"),
        ] {
            out.extend_from_slice(k.as_bytes());
            out.push(0);
            out.extend_from_slice(v.as_bytes());
            out.push(0);
        }
        out.push(0);
        out.extend_from_slice(b"\x01player_\x00\x00");
        for name in ["Alice", "Bob"] {
            out.extend_from_slice(name.as_bytes());
            out.push(0);
        }
        out.push(0);
        out
    }

    #[test]
    fn p4a_gs4_handshake_issues_per_ip_challenge_and_echoes_masked_session() {
        let mut handler = Gs4QueryHandler::new(0x5eed);
        let now = Instant::now();
        let ip: std::net::IpAddr = "192.0.2.7".parse().unwrap();
        let no_snap = p4a_no_snapshot();

        // 0x7f7f7f7f masks down to 0x0f0f0f0f — the echoed bytes must be the
        // masked id, not what the client sent.
        let reply = handler
            .handle_datagram(ip, &p4a_gs4_handshake(0x7f7f_7f7f), now, &no_snap)
            .expect("handshake must be answered");
        assert_eq!(&reply[1..5], &0x0f0f_0f0fi32.to_be_bytes());
        let token = p4a_gs4_token_of(&reply);
        assert!(token > 0, "token text is a positive decimal: {token}");

        // A second handshake inside the lease reuses the live token verbatim.
        let again = handler
            .handle_datagram(
                ip,
                &p4a_gs4_handshake(0x7f7f_7f7f),
                now + Duration::from_secs(10),
                &no_snap,
            )
            .unwrap();
        assert_eq!(again, reply, "same IP inside the TTL keeps its token");

        // A different address gets its own token, not a shared one.
        let other: std::net::IpAddr = "192.0.2.8".parse().unwrap();
        let other_reply = handler
            .handle_datagram(other, &p4a_gs4_handshake(0x7f7f_7f7f), now, &no_snap)
            .unwrap();
        assert_ne!(
            p4a_gs4_token_of(&other_reply),
            token,
            "a second address must not share the first token"
        );

        // The token stream is deterministic per salt: a fresh handler with the
        // same salt mints the same first token for the same address.
        let mut twin = Gs4QueryHandler::new(0x5eed);
        let twin_reply = twin
            .handle_datagram(ip, &p4a_gs4_handshake(0x7f7f_7f7f), now, &no_snap)
            .unwrap();
        assert_eq!(twin_reply, reply, "deterministic per (salt, ip, issue)");

        // …and a different salt mints a different stream.
        let mut other_salt = Gs4QueryHandler::new(0xbad5);
        let other_salt_reply = other_salt
            .handle_datagram(ip, &p4a_gs4_handshake(0x7f7f_7f7f), now, &no_snap)
            .unwrap();
        assert_ne!(
            p4a_gs4_token_of(&other_salt_reply),
            token,
            "tokens must not replay across handler salts"
        );
    }

    #[test]
    fn p4a_gs4_basic_and_full_stat_replies_are_byte_exact() {
        let mut handler = Gs4QueryHandler::new(0x5eed);
        let now = Instant::now();
        let ip: std::net::IpAddr = "198.51.100.23".parse().unwrap();
        let snap_value = p4a_gs4_snapshot();
        let snap = || snap_value.clone();
        let no_snap = p4a_no_snapshot();

        let reply = handler
            .handle_datagram(ip, &p4a_gs4_handshake(0x0102_0304), now, &no_snap)
            .unwrap();
        let token = p4a_gs4_token_of(&reply);

        let basic = handler
            .handle_datagram(
                ip,
                &p4a_gs4_stat(0x1122_3344, token, false),
                now + Duration::from_secs(1),
                &snap,
            )
            .expect("a valid basic stat must be answered");
        assert_eq!(basic, p4a_expected_basic(0x0102_0304));

        let full = handler
            .handle_datagram(
                ip,
                &p4a_gs4_stat(0x1a2b_3c4d, token, true),
                now + Duration::from_secs(2),
                &snap,
            )
            .expect("a valid full stat must be answered");
        assert_eq!(full, p4a_expected_full(0x0a0b_0c0d));
    }

    #[test]
    fn p4a_gs4_fail_closed_on_bad_magic_truncation_oversize_and_tokens() {
        let mut handler = Gs4QueryHandler::new(0x5eed);
        let now = Instant::now();
        let ip_a: std::net::IpAddr = "203.0.113.10".parse().unwrap();
        let ip_b: std::net::IpAddr = "203.0.113.11".parse().unwrap();
        let snap_value = p4a_gs4_snapshot();
        let snap = || snap_value.clone();
        let no_snap = p4a_no_snapshot();

        // Wrong magic, truncation, an unknown type, oversize.
        assert_eq!(
            handler.handle_datagram(ip_a, &[0x00, 0x00, 0x09, 0, 0, 0, 1], now, &no_snap),
            None,
            "bad magic"
        );
        assert_eq!(
            handler.handle_datagram(ip_a, &[0xFE, 0xFD], now, &no_snap),
            None,
            "truncated below the session id"
        );
        assert_eq!(
            handler.handle_datagram(ip_a, &[0xFE, 0xFD, 0x09, 0, 0, 0], now, &no_snap),
            None,
            "a 6-byte datagram has no full session id"
        );
        assert!(
            handler.handle_datagram(
                ip_a,
                &p4a_gs4_handshake(7),
                now,
                &no_snap,
            )
            .is_some(),
            "control: a real handshake answers"
        );
        // Re-handshake above consumed the first issuance; mint a stable token
        // for the stat checks that follow.
        let token = p4a_gs4_token_of(
            &handler
                .handle_datagram(ip_a, &p4a_gs4_handshake(7), now, &no_snap)
                .unwrap(),
        );
        assert_eq!(
            handler.handle_datagram(ip_a, &p4a_gs4_handshake(7), now, &no_snap)
                .map(|r| p4a_gs4_token_of(&r)),
            Some(token),
            "control: same lease token"
        );
        let mut unknown = p4a_gs4_handshake(7);
        unknown[2] = 0x07;
        assert_eq!(
            handler.handle_datagram(ip_a, &unknown, now, &no_snap),
            None,
            "unknown packet type"
        );
        let mut big = vec![0xFE, 0xFD, 0x09];
        big.extend_from_slice(&7i32.to_be_bytes());
        big.resize(GS4_MAX_DATAGRAM + 1, 0);
        assert_eq!(
            handler.handle_datagram(ip_a, &big, now, &no_snap),
            None,
            "oversized datagram"
        );

        // Stat requests without a live token: wrong value, no lease at all
        // (from an address that never handshook), and a stolen-token replay
        // from a second address.
        assert_eq!(
            handler.handle_datagram(ip_a, &p4a_gs4_stat(7, token + 1, false), now, &snap),
            None,
            "wrong token value"
        );
        assert_eq!(
            handler.handle_datagram(ip_b, &p4a_gs4_stat(7, token, false), now, &snap),
            None,
            "token is bound to the handshaking address, not the value alone"
        );
        assert_eq!(
            handler.handle_datagram(ip_a, &p4a_gs4_stat(7, token, false), now, &snap),
            Some(p4a_expected_basic(7)),
            "control: the live token answers"
        );

        // Sizes around the two legal stat bodies are all rejected.
        for len in [8usize, 10, 12, 14, 16, 100] {
            let mut weird = vec![0xFE, 0xFD, 0x00];
            weird.extend_from_slice(&7i32.to_be_bytes());
            weird.extend_from_slice(&token.to_be_bytes());
            weird.resize(len, 0);
            assert_eq!(
                handler.handle_datagram(ip_a, &weird, now, &snap),
                None,
                "stat request of {len} bytes"
            );
        }

        // At the TTL boundary the lease is dead; past it a new handshake mints
        // a fresh token and the old one stays dead.
        assert_eq!(
            handler.handle_datagram(
                ip_a,
                &p4a_gs4_stat(7, token, false),
                now + GS4_CHALLENGE_TTL,
                &snap,
            ),
            None,
            "expired token at the TTL boundary"
        );
        let renewed = handler
            .handle_datagram(
                ip_a,
                &p4a_gs4_handshake(7),
                now + GS4_CHALLENGE_TTL + Duration::from_secs(1),
                &no_snap,
            )
            .unwrap();
        let new_token = p4a_gs4_token_of(&renewed);
        assert_ne!(new_token, token, "expiry mints a fresh token");
        assert_eq!(
            handler.handle_datagram(
                ip_a,
                &p4a_gs4_stat(7, token, false),
                now + GS4_CHALLENGE_TTL + Duration::from_secs(2),
                &snap,
            ),
            None,
            "the dead token stays dead"
        );
        assert!(
            handler
                .handle_datagram(
                    ip_a,
                    &p4a_gs4_stat(7, new_token, false),
                    now + GS4_CHALLENGE_TTL + Duration::from_secs(3),
                    &snap,
                )
                .is_some(),
            "the renewed lease answers"
        );
    }

    #[test]
    fn p4a_gs4_udp_loopback_serves_handshake_basic_full_and_drops() {
        let stop = Arc::new(AtomicBool::new(false));
        let snap_value = p4a_gs4_snapshot();
        let snapshot_fn: Arc<dyn Fn() -> Gs4StatusSnapshot + Send + Sync> =
            Arc::new(move || snap_value.clone());
        let Ok((addr, join)) = spawn_gs4_query_thread(
            "127.0.0.1:0",
            Arc::clone(&snapshot_fn),
            Arc::clone(&stop),
        ) else {
            return; // no UDP loopback in this sandbox: nothing to verify
        };
        let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 2048];

        // Handshake -> [0x09][masked sid][token\0].
        client
            .send_to(&p4a_gs4_handshake(0x7f7f_7f7f), addr)
            .unwrap();
        let (n, src) = client.recv_from(&mut buf).unwrap();
        assert_eq!(src.ip(), addr.ip());
        let reply = &buf[..n];
        assert_eq!(reply[0], 0x09);
        assert_eq!(&reply[1..5], &0x0f0f_0f0fi32.to_be_bytes());
        let token = p4a_gs4_token_of(reply);
        assert!(token > 0);

        // Basic stat over the wire, byte-compared against the known layout.
        client
            .send_to(&p4a_gs4_stat(0x1122_3344, token, false), addr)
            .unwrap();
        let (n, _) = client.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], p4a_expected_basic(0x0102_0304).as_slice());

        // Full stat: the K/V block and the player list, byte-exact.
        client
            .send_to(&p4a_gs4_stat(0x1a2b_3c4d, token, true), addr)
            .unwrap();
        let (n, _) = client.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], p4a_expected_full(0x0a0b_0c0d).as_slice());

        // Fail-closed over the wire: a wrong token and a bad-magic datagram
        // get no reply at all (recv times out).
        client
            .send_to(&p4a_gs4_stat(0x0102_0304, token + 1, false), addr)
            .unwrap();
        assert!(client.recv_from(&mut buf).is_err(), "wrong token is dropped");
        client.send_to(&[0xDE, 0xAD, 0x09, 0, 0, 0, 1], addr).unwrap();
        assert!(client.recv_from(&mut buf).is_err(), "bad magic is dropped");

        // Cross-IP theft: 127.0.0.2 presenting 127.0.0.1's token is refused.
        if let Ok(other) = std::net::UdpSocket::bind("127.0.0.2:0") {
            other
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            other
                .send_to(&p4a_gs4_stat(0x0102_0304, token, false), addr)
                .unwrap();
            assert!(
                other.recv_from(&mut buf).is_err(),
                "a token is bound to its handshaking address"
            );
        }

        stop.store(true, Ordering::Relaxed);
        join.join().unwrap();
    }
}

// ------------------------------------------- built-in MSPT hotspot profiler
//
// PERF-1: the zero-dependency Spark substitute. A fixed ring of per-tick
// durations gives mean/p50/p95/p99/max MSPT and exact TPS; a bounded per-chunk
// accumulator attributes work microseconds to `(dim, cx, cz)` and subsystem,
// so `/perf` style output can name the chunk that is actually eating the tick.

/// Ring capacity: 1,200 ticks is a 60-second window at the 20 TPS target.
const PROFILER_RING_CAPACITY: usize = 1_200;

/// Bound on the per-chunk accumulator. A server can touch more chunks than
/// this in a minute; when the map is full the coldest entry is evicted for a
/// new arrival, so the retained set tracks the hot chunks that matter.
#[allow(dead_code)]
const PROFILER_MAX_CHUNKS: usize = 8_192;

/// Subsystems chunk work is attributed to. The label is what operators see —
/// plain words, no internal names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ProfilerSubsystem {
    Hoppers,
    Villagers,
    Worldgen,
    Redstone,
    Mobs,
    Light,
    Network,
    Mods,
}

impl ProfilerSubsystem {
    const ALL: [Self; 8] = [
        Self::Hoppers,
        Self::Villagers,
        Self::Worldgen,
        Self::Redstone,
        Self::Mobs,
        Self::Light,
        Self::Network,
        Self::Mods,
    ];

    fn index(self) -> usize {
        self as usize
    }

    /// Lowercase label for human lines and JSON keys.
    fn label(self) -> &'static str {
        match self {
            Self::Hoppers => "hoppers",
            Self::Villagers => "villagers",
            Self::Worldgen => "worldgen",
            Self::Redstone => "redstone",
            Self::Mobs => "mobs",
            Self::Light => "light",
            Self::Network => "network",
            Self::Mods => "mods",
        }
    }
}

/// Per-chunk work ledger: microseconds per subsystem plus the running total
/// the hot-set eviction and top-N ranking read.
#[derive(Clone, Copy, Debug, Default)]
struct ChunkWorkLedger {
    per_subsystem: [u64; 8],
    total: u64,
}

/// One row of a [`ProfilerReport`]'s top-N list: the chunk, its block-center
/// coordinates (`cx*16+8`, `cz*16+8` — where a `/tp` lands), the total
/// microseconds attributed in the window, the mean cost per recorded tick,
/// and the subsystem that dominates it.
#[derive(Clone, Debug)]
struct ChunkHotspot {
    dim: DimId,
    chunk_x: i32,
    chunk_z: i32,
    block_x: i32,
    block_z: i32,
    total_us: u64,
    ms_per_tick: f64,
    dominant: ProfilerSubsystem,
}

/// A point-in-time readout from [`ServerHotspotProfiler::summary`]: exact TPS
/// and the MSPT percentiles, the subsystem percentage breakdown, and the
/// hottest chunks. `format_human` is the console/report rendering; `to_json`
/// is the same data for tooling.
#[derive(Clone, Debug)]
struct ProfilerReport {
    ticks_recorded: usize,
    tps: f64,
    mspt_mean: f64,
    mspt_p50: f64,
    mspt_p95: f64,
    mspt_p99: f64,
    mspt_max: f64,
    /// `(subsystem, window microseconds, share of attributed work)` per
    /// subsystem, in `ProfilerSubsystem::ALL` order.
    subsystem_us: [(ProfilerSubsystem, u64, f64); 8],
    #[allow(dead_code)]
    total_attributed_us: u64,
    top_chunks: Vec<ChunkHotspot>,
}

/// What an operator reads for a dimension: the friendly vanilla names, the
/// registry id for custom dimensions.
fn profiler_dim_name(dim: DimId) -> String {
    match dim {
        DimId::Overworld => "Overworld".to_string(),
        DimId::Nether => "Nether".to_string(),
        DimId::End => "End".to_string(),
        _ => dim.dimension_id().to_string(),
    }
}

#[allow(dead_code)]
impl ProfilerReport {
    /// Jargon-free operator output: one status line, one subsystem line, then
    /// the hottest chunks as `Overworld chunk (12, -4) [x=200, z=-56]:
    /// 14.2 ms/tick (hoppers)`.
    fn format_human(&self) -> String {
        let mut out = format!(
            "TPS {:.1} | MSPT mean {:.1} ms, p50 {:.1}, p95 {:.1}, p99 {:.1}, max {:.1} ms ({} ticks)\n",
            self.tps,
            self.mspt_mean,
            self.mspt_p50,
            self.mspt_p95,
            self.mspt_p99,
            self.mspt_max,
            self.ticks_recorded,
        );
        let breakdown: Vec<String> = self
            .subsystem_us
            .iter()
            .filter(|(_, us, _)| *us > 0)
            .map(|(sub, _, pct)| format!("{} {:.1}%", sub.label(), pct))
            .collect();
        if !breakdown.is_empty() {
            out.push_str(&format!("Work by subsystem: {}\n", breakdown.join(", ")));
        }
        if self.top_chunks.is_empty() {
            out.push_str("No chunk hotspots recorded.\n");
        } else {
            out.push_str("Top chunk hotspots:\n");
            for spot in &self.top_chunks {
                out.push_str(&format!(
                    "  {} chunk ({}, {}) [x={}, z={}]: {:.1} ms/tick ({})\n",
                    profiler_dim_name(spot.dim),
                    spot.chunk_x,
                    spot.chunk_z,
                    spot.block_x,
                    spot.block_z,
                    spot.ms_per_tick,
                    spot.dominant.label(),
                ));
            }
        }
        out
    }

    /// The same report as JSON — hand-built like the status reply, so nothing
    /// outside this file is needed.
    #[allow(dead_code)]
    fn to_json(&self) -> String {
        let subsystems = self
            .subsystem_us
            .iter()
            .map(|(sub, us, pct)| {
                format!(
                    "\"{}\":{{\"us\":{us},\"pct\":{pct:.2}}}",
                    sub.label()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let chunks = self
            .top_chunks
            .iter()
            .map(|spot| {
                format!(
                    "{{\"dim\":\"{}\",\"cx\":{},\"cz\":{},\"x\":{},\"z\":{},\
                     \"total_us\":{},\"ms_per_tick\":{:.2},\"subsystem\":\"{}\"}}",
                    status_json_text(spot.dim.dimension_id()),
                    spot.chunk_x,
                    spot.chunk_z,
                    spot.block_x,
                    spot.block_z,
                    spot.total_us,
                    spot.ms_per_tick,
                    spot.dominant.label(),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"ticks\":{},\"tps\":{:.2},\"mspt\":{{\"mean\":{:.2},\"p50\":{:.2},\
             \"p95\":{:.2},\"p99\":{:.2},\"max\":{:.2}}},\"subsystems\":{{{subsystems}}},\
             \"top_chunks\":[{chunks}]}}",
            self.ticks_recorded,
            self.tps,
            self.mspt_mean,
            self.mspt_p50,
            self.mspt_p95,
            self.mspt_p99,
            self.mspt_max,
        )
    }
}

/// The recorder itself: a fixed ring of tick durations plus the bounded
/// per-chunk work map. `record_*` calls are the only inputs; `summary` turns
/// the window into a report. All times are microseconds — `Instant`-side
/// callers convert once at the edge.
struct ServerHotspotProfiler {
    /// `ring[..ring_len]` is the valid window; before the first wrap that is
    /// `0..ring_next`, after it the whole array is valid.
    ring: Vec<u32>,
    ring_next: usize,
    ring_len: usize,
    chunks: HashMap<(DimId, i32, i32), ChunkWorkLedger>,
}

impl Default for ServerHotspotProfiler {
    fn default() -> Self {
        Self {
            ring: vec![0; PROFILER_RING_CAPACITY],
            ring_next: 0,
            ring_len: 0,
            chunks: HashMap::new(),
        }
    }
}

#[allow(dead_code)]
impl ServerHotspotProfiler {
    /// One tick's duration, in microseconds.
    fn record_tick_us(&mut self, us: u32) {
        self.ring[self.ring_next] = us;
        self.ring_next = (self.ring_next + 1) % PROFILER_RING_CAPACITY;
        self.ring_len = (self.ring_len + 1).min(PROFILER_RING_CAPACITY);
    }

    /// Attribute `us` microseconds of work inside `(dim, cx, cz)` to
    /// `subsystem`. Over the bound, the coldest chunk is evicted for a new key.
    fn record_chunk_work(
        &mut self,
        dim: DimId,
        cx: i32,
        cz: i32,
        subsystem: ProfilerSubsystem,
        us: u32,
    ) {
        let key = (dim, cx, cz);
        if !self.chunks.contains_key(&key) && self.chunks.len() >= PROFILER_MAX_CHUNKS {
            if let Some(coldest) = self
                .chunks
                .iter()
                .min_by_key(|(_, ledger)| ledger.total)
                .map(|(key, _)| *key)
            {
                self.chunks.remove(&coldest);
            }
        }
        let ledger = self.chunks.entry(key).or_default();
        ledger.per_subsystem[subsystem.index()] += u64::from(us);
        ledger.total += u64::from(us);
    }

    /// Drop everything: fresh ring, empty chunk map.
    fn reset(&mut self) {
        self.ring.fill(0);
        self.ring_next = 0;
        self.ring_len = 0;
        self.chunks.clear();
    }

    /// Snapshot the window. Percentiles are nearest-rank over the recorded
    /// ticks — `p`% of ticks were at or under `pP`. TPS is `1e6 / mean_us`
    /// clamped to the 20 TPS ceiling; an empty or zero-cost window reads 20.
    fn summary(&self, top_n: usize) -> ProfilerReport {
        let mut ticks: Vec<u32> = self.ring[..self.ring_len].to_vec();
        ticks.sort_unstable();
        let n = ticks.len();
        let mean_us = if n == 0 {
            0.0
        } else {
            ticks.iter().map(|&t| u64::from(t)).sum::<u64>() as f64 / n as f64
        };
        let tps = if mean_us <= 0.0 {
            20.0
        } else {
            (1_000_000.0 / mean_us).min(20.0)
        };
        let percentile = |p: usize| -> f64 {
            if n == 0 {
                return 0.0;
            }
            // nearest-rank: ceil(p/100 * n), 1-indexed.
            let rank = (p * n).div_ceil(100).max(1);
            f64::from(ticks[(rank - 1).min(n - 1)]) / 1000.0
        };
        let mspt_max = f64::from(ticks.last().copied().unwrap_or(0)) / 1000.0;

        let mut subsystem_us = ProfilerSubsystem::ALL.map(|sub| (sub, 0u64, 0.0f64));
        let mut total_attributed = 0u64;
        for ledger in self.chunks.values() {
            total_attributed += ledger.total;
            for entry in &mut subsystem_us {
                entry.1 += ledger.per_subsystem[entry.0.index()];
            }
        }
        for entry in &mut subsystem_us {
            entry.2 = if total_attributed == 0 {
                0.0
            } else {
                entry.1 as f64 * 100.0 / total_attributed as f64
            };
        }

        let mut ranked: Vec<ChunkHotspot> = self
            .chunks
            .iter()
            .map(|(&(dim, cx, cz), ledger)| {
                let dominant = ProfilerSubsystem::ALL
                    .iter()
                    .max_by_key(|sub| ledger.per_subsystem[sub.index()])
                    .copied()
                    .unwrap_or(ProfilerSubsystem::Mobs);
                ChunkHotspot {
                    dim,
                    chunk_x: cx,
                    chunk_z: cz,
                    block_x: cx.saturating_mul(16).saturating_add(8),
                    block_z: cz.saturating_mul(16).saturating_add(8),
                    total_us: ledger.total,
                    ms_per_tick: if n == 0 {
                        0.0
                    } else {
                        ledger.total as f64 / n as f64 / 1000.0
                    },
                    dominant,
                }
            })
            .collect();
        // Hottest first; ties break on coordinates so reports are stable.
        ranked.sort_by(|a, b| {
            b.total_us
                .cmp(&a.total_us)
                .then((a.dim, a.chunk_x, a.chunk_z).cmp(&(b.dim, b.chunk_x, b.chunk_z)))
        });
        ranked.truncate(top_n);

        ProfilerReport {
            ticks_recorded: n,
            tps,
            mspt_mean: mean_us / 1000.0,
            mspt_p50: percentile(50),
            mspt_p95: percentile(95),
            mspt_p99: percentile(99),
            mspt_max,
            subsystem_us,
            total_attributed_us: total_attributed,
            top_chunks: ranked,
        }
    }

    /// Human rendering of the current window, top five chunks.
    fn format_human(&self) -> String {
        self.summary(5).format_human()
    }

    /// JSON rendering of the current window, top five chunks.
    fn to_json(&self) -> String {
        self.summary(5).to_json()
    }
}

// -------------------------------------- 24/365 SRE primitives (OPS-SRE)
//
// Three small, dependency-free primitives the operator surface shares: a
// monotonic tick watchdog, a rolling world-backup rotator with a disk-space
// guard, and a Prometheus text formatter for a localhost scrape endpoint
// (Grafana, Uptime Kuma). Everything is sans-IO: callers hand in the
// readings, the code never sleeps, blocks on I/O or panics.

/// How long the tick heartbeat has been silent, classified against the
/// watchdog's warn/critical thresholds. `stall_ms` rides on the non-healthy
/// arms so the diagnostic needs no second clock read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum WatchdogVerdict {
    Healthy,
    Warning { stall_ms: u64 },
    CriticalStall { stall_ms: u64 },
}

impl WatchdogVerdict {
    /// The greppable one-liner for a non-healthy verdict. `MEGU-E-` is the
    /// operator-error tag family the crash reports already use; the 8-hex
    /// suffix is a small FNV-1a over the severity and stall so the same stall
    /// always renders the same tag. `Healthy` maps to `None`: a live tick
    /// emits nothing.
    #[allow(dead_code)]
    fn diagnostic(self) -> Option<String> {
        let (severity, stall_ms) = match self {
            Self::Healthy => return None,
            Self::Warning { stall_ms } => ("warning", stall_ms),
            Self::CriticalStall { stall_ms } => ("critical", stall_ms),
        };
        let mut h = megu_core::hash::Fnv1a64::new();
        h.update(severity.as_bytes());
        h.update(&stall_ms.to_be_bytes());
        Some(format!(
            "MEGU-E-{:08X} tick watchdog stall {}ms severity={severity}",
            h.finish() as u32, stall_ms
        ))
    }
}

/// Default stall budgets: a 5 s silence is worth an operator note, 30 s
/// means the tick loop is effectively dead.
#[allow(dead_code)]
const WATCHDOG_WARN_MS: u64 = 5_000;
#[allow(dead_code)]
const WATCHDOG_CRITICAL_MS: u64 = 30_000;

/// Monotonic heartbeat for the server tick loop. `beat()` stamps every
/// completed tick; `check_stall` classifies the silence since the last beat.
/// `now` always arrives as an argument — the same convention
/// `Gs4QueryHandler` uses — so tests never sleep and no wall clock is read.
#[allow(dead_code)]
struct TickWatchdog {
    last_beat: Instant,
    warn_ms: u64,
    critical_ms: u64,
}

#[allow(dead_code)]
impl TickWatchdog {
    fn new() -> Self {
        Self::with_thresholds(WATCHDOG_WARN_MS, WATCHDOG_CRITICAL_MS)
    }

    /// Custom thresholds; `critical` clamps up to `warn` and `warn` floors at
    /// 1 ms so the bands can never invert or fire on a zero stall.
    fn with_thresholds(warn_ms: u64, critical_ms: u64) -> Self {
        let warn_ms = warn_ms.max(1);
        Self {
            last_beat: Instant::now(),
            warn_ms,
            critical_ms: critical_ms.max(warn_ms),
        }
    }

    /// One completed tick: the heartbeat moves to now.
    fn beat(&mut self) {
        self.last_beat = Instant::now();
    }

    /// Classify the stall at `now`. `max_stall_ms` is the caller's own tick
    /// budget: `0` keeps the configured warn threshold, a nonzero value can
    /// only tighten it — the watchdog's ceiling is authoritative, so a caller
    /// may ask for an earlier warning but never a later one.
    fn check_stall(&self, now: Instant, max_stall_ms: u64) -> WatchdogVerdict {
        let stall_ms = now
            .saturating_duration_since(self.last_beat)
            .as_millis() as u64;
        let warn_ms = if max_stall_ms == 0 {
            self.warn_ms
        } else {
            self.warn_ms.min(max_stall_ms)
        };
        let critical_ms = self.critical_ms.max(warn_ms);
        if stall_ms > critical_ms {
            WatchdogVerdict::CriticalStall { stall_ms }
        } else if stall_ms > warn_ms {
            WatchdogVerdict::Warning { stall_ms }
        } else {
            WatchdogVerdict::Healthy
        }
    }
}

/// Retention bounds for world backups: keep at most `max_backups` generations
/// and never start a new one while free disk sits under `min_free_bytes`.
struct RollingBackupPolicy {
    max_backups: usize,
    min_free_bytes: u64,
}

/// The retention ceiling `max_backups` is validated against.
#[allow(dead_code)]
const BACKUP_MAX_BACKUPS_LIMIT: usize = 128;
/// Vanilla-faithful defaults: a dozen generations, half a GiB of headroom.
const BACKUP_DEFAULT_MAX_BACKUPS: usize = 12;
const BACKUP_DEFAULT_MIN_FREE_BYTES: u64 = 512 * 1024 * 1024;

impl RollingBackupPolicy {
    /// `max_backups` must sit in `1..=128`; `min_free_bytes` is the disk-space
    /// guard — a backup that would leave the volume under it is refused.
    #[allow(dead_code)]
    fn new(max_backups: usize, min_free_bytes: u64) -> Result<Self, String> {
        if !(1..=BACKUP_MAX_BACKUPS_LIMIT).contains(&max_backups) {
            return Err(format!(
                "backup max_backups must be in 1..={BACKUP_MAX_BACKUPS_LIMIT}"
            ));
        }
        Ok(Self {
            max_backups,
            min_free_bytes,
        })
    }
}

impl Default for RollingBackupPolicy {
    fn default() -> Self {
        Self {
            max_backups: BACKUP_DEFAULT_MAX_BACKUPS,
            min_free_bytes: BACKUP_DEFAULT_MIN_FREE_BYTES,
        }
    }
}

/// What the rotator decided for one backup cycle.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum BackupVerdict {
    /// Disk guard passed: write the new backup, then prune `prune` (oldest
    /// sequence first) so the set never exceeds `max_backups`.
    Proceed { prune: Vec<String> },
    /// `free_bytes` is under the policy floor — skip this cycle cleanly; the
    /// caller sees the numbers and nothing is deleted.
    SkippedLowDisk {
        free_bytes: u64,
        min_free_bytes: u64,
    },
    /// A listed name is not a plain `world-backup-<seq>.tar.gz` basename, so
    /// the directory listing cannot be trusted: fail closed and prune
    /// nothing this round.
    RejectedName { name: String },
}

/// The sequence number inside a strict `world-backup-<seq>.tar.gz` basename,
/// or `None` for anything else. The check is purely lexical — callers hand
/// basenames, never paths — so `..`, separators, a missing suffix, an empty
/// or non-digit sequence and sequence overflow all fail closed, and no
/// symlink or traversal entry can ever be expressed.
fn backup_seq_of(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("world-backup-")?;
    let digits = rest.strip_suffix(".tar.gz")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Decide one rotation. `existing` is the backup directory's basenames in any
/// order — the rotator sorts by embedded sequence itself, so "oldest" means
/// the lowest `<seq>`, never lexical order (`world-backup-10` sorts before
/// `world-backup-2` as text but is newer). On `Proceed`, `prune` lists exactly
/// the oldest files whose removal keeps the post-write count at
/// `max_backups`; the caller writes the new backup first, then deletes
/// `prune`.
#[allow(dead_code)]
fn plan_backup_rotation(
    policy: &RollingBackupPolicy,
    free_bytes: u64,
    existing: &[String],
) -> BackupVerdict {
    if free_bytes < policy.min_free_bytes {
        return BackupVerdict::SkippedLowDisk {
            free_bytes,
            min_free_bytes: policy.min_free_bytes,
        };
    }
    match backup_rotation_prune(existing, policy.max_backups) {
        Ok(prune) => BackupVerdict::Proceed { prune },
        Err(name) => BackupVerdict::RejectedName { name },
    }
}

/// One scrape of server vitals for a Prometheus text endpoint. Every field is
/// gathered by the caller — the same convention `OperatorHealthSnapshot` and
/// `Gs4StatusSnapshot` already follow — so the formatter reads fields and
/// never interrogates the server.
struct OperatorMetricsSnapshot {
    /// Observed ticks per second (20.0 nominal).
    tps: f64,
    mspt_p50: f64,
    mspt_p95: f64,
    mspt_p99: f64,
    players_online: u64,
    loaded_chunks: u64,
    /// Tick targets the fault quarantine currently holds frozen.
    quarantined_faults: u64,
    /// Resident set size in MiB.
    rss_mib: f64,
    uptime_seconds: f64,
}

impl OperatorMetricsSnapshot {
    /// Prometheus text exposition (0.0.4): a `# HELP`/`# TYPE` pair then one
    /// sample line per metric, every name `megu_`-prefixed for a
    /// localhost scrape. Non-finite floats render as `0` — a NaN sample makes
    /// some collectors reject the whole exposition, and `0` is the honest
    /// "no reading" anyway.
    fn format_prometheus(&self) -> String {
        fn gauge(v: f64) -> String {
            if v.is_finite() {
                format!("{v}")
            } else {
                "0".to_string()
            }
        }
        let rows: [(&str, &str, String); 9] = [
            (
                "megu_tps",
                "Server ticks per second, 20.0 nominal",
                gauge(self.tps),
            ),
            (
                "megu_mspt_p50",
                "Median tick time in milliseconds",
                gauge(self.mspt_p50),
            ),
            (
                "megu_mspt_p95",
                "95th-percentile tick time in milliseconds",
                gauge(self.mspt_p95),
            ),
            (
                "megu_mspt_p99",
                "99th-percentile tick time in milliseconds",
                gauge(self.mspt_p99),
            ),
            (
                "megu_players_online",
                "Players currently online",
                self.players_online.to_string(),
            ),
            (
                "megu_loaded_chunks",
                "World chunks currently loaded",
                self.loaded_chunks.to_string(),
            ),
            (
                "megu_quarantined_faults",
                "Tick targets frozen by the fault quarantine",
                self.quarantined_faults.to_string(),
            ),
            (
                "megu_rss_mib",
                "Resident memory in MiB",
                gauge(self.rss_mib),
            ),
            (
                "megu_uptime_seconds",
                "Process uptime in seconds",
                gauge(self.uptime_seconds),
            ),
        ];
        let mut out = String::with_capacity(1024);
        for (name, help, value) in rows {
            out.push_str("# HELP ");
            out.push_str(name);
            out.push(' ');
            out.push_str(help);
            out.push('\n');
            out.push_str("# TYPE ");
            out.push_str(name);
            out.push_str(" gauge\n");
            out.push_str(name);
            out.push(' ');
            out.push_str(&value);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod perf1_profiler_tests {
    use super::*;

    #[test]
    fn perf1_profiler_reports_exact_mspt_percentiles_and_tps() {
        let mut profiler = ServerHotspotProfiler::default();
        // A loaded minute: 1000 ticks at 10 ms, 180 at 50 ms, 20 at 100 ms.
        for _ in 0..1_000 {
            profiler.record_tick_us(10_000);
        }
        for _ in 0..180 {
            profiler.record_tick_us(50_000);
        }
        for _ in 0..20 {
            profiler.record_tick_us(100_000);
        }

        let report = profiler.summary(5);
        assert_eq!(report.ticks_recorded, 1_200);
        // mean = 21_000_000 / 1200 = 17_500 us = 17.5 ms; TPS clamps at 20.
        assert!((report.mspt_mean - 17.5).abs() < 1e-9, "{}", report.mspt_mean);
        assert!((report.tps - 20.0).abs() < 1e-9, "{}", report.tps);
        // nearest-rank: p50 lands inside the 10ms block, p95 in the 50ms
        // block, p99/max in the 100ms tail.
        assert!((report.mspt_p50 - 10.0).abs() < 1e-9, "{}", report.mspt_p50);
        assert!((report.mspt_p95 - 50.0).abs() < 1e-9, "{}", report.mspt_p95);
        assert!((report.mspt_p99 - 100.0).abs() < 1e-9, "{}", report.mspt_p99);
        assert!((report.mspt_max - 100.0).abs() < 1e-9, "{}", report.mspt_max);
    }

    #[test]
    fn perf1_profiler_tps_reflects_real_mspt_and_ring_wraps() {
        let mut profiler = ServerHotspotProfiler::default();
        // 1,400 writes over a 1,200-slot ring: the first 200 are evicted, the
        // surviving window is exactly the last 1,200 ticks at 40 ms.
        for _ in 0..200 {
            profiler.record_tick_us(5_000);
        }
        for _ in 0..1_200 {
            profiler.record_tick_us(40_000);
        }
        let report = profiler.summary(0);
        assert_eq!(report.ticks_recorded, 1_200);
        assert!((report.mspt_mean - 40.0).abs() < 1e-9);
        // TPS = 1e6 / 40_000 us = 25 -> clamped to the 20 TPS ceiling.
        assert!((report.tps - 20.0).abs() < 1e-9);

        profiler.reset();
        assert_eq!(profiler.ring_len, 0);
        assert!(profiler.chunks.is_empty());
        // 80 ms ticks: TPS = 12.5, unclamped — the slow server case.
        for _ in 0..100 {
            profiler.record_tick_us(80_000);
        }
        let report = profiler.summary(0);
        assert_eq!(report.ticks_recorded, 100);
        assert!((report.tps - 12.5).abs() < 1e-9, "{}", report.tps);
        assert!((report.mspt_p99 - 80.0).abs() < 1e-9);
    }

    #[test]
    fn perf1_profiler_ranks_chunk_hotspots_with_block_coords_and_dominant() {
        let mut profiler = ServerHotspotProfiler::default();
        for _ in 0..100 {
            profiler.record_tick_us(50_000);
        }
        // Overworld (12,-4) is the hoppers hotspot: 1420 us/tick over the
        // window -> 14.2 ms/tick at n=10? no: n=100 ticks -> total/100.
        profiler.record_chunk_work(DimId::Overworld, 12, -4, ProfilerSubsystem::Hoppers, 1_000_000);
        profiler.record_chunk_work(DimId::Overworld, 12, -4, ProfilerSubsystem::Redstone, 200_000);
        profiler.record_chunk_work(DimId::Overworld, 12, -4, ProfilerSubsystem::Hoppers, 220_000);
        profiler.record_chunk_work(DimId::Overworld, 0, 0, ProfilerSubsystem::Villagers, 500_000);
        profiler.record_chunk_work(DimId::Nether, -9, 9, ProfilerSubsystem::Worldgen, 50_000);
        profiler.record_chunk_work(DimId::Overworld, 3, 3, ProfilerSubsystem::Mobs, 10_000);

        let report = profiler.summary(2);
        assert_eq!(report.top_chunks.len(), 2);
        let hottest = &report.top_chunks[0];
        assert_eq!((hottest.dim, hottest.chunk_x, hottest.chunk_z), (DimId::Overworld, 12, -4));
        // block-center: cx*16+8, cz*16+8
        assert_eq!((hottest.block_x, hottest.block_z), (200, -56));
        assert_eq!(hottest.total_us, 1_420_000);
        // 1_420_000 us over 100 ticks = 14.2 ms/tick.
        assert!((hottest.ms_per_tick - 14.2).abs() < 1e-9, "{}", hottest.ms_per_tick);
        assert_eq!(hottest.dominant, ProfilerSubsystem::Hoppers);
        assert_eq!(report.top_chunks[1].chunk_x, 0);
        assert_eq!(report.top_chunks[1].dominant, ProfilerSubsystem::Villagers);

        // Subsystem percentages of the attributed 1_980_000 us.
        let pct = |sub: ProfilerSubsystem| {
            report
                .subsystem_us
                .iter()
                .find(|(s, _, _)| *s == sub)
                .map(|(_, us, pct)| (*us, *pct))
                .unwrap()
        };
        let (us, p) = pct(ProfilerSubsystem::Hoppers);
        assert_eq!(us, 1_220_000);
        assert!((p - 1_220_000.0 * 100.0 / 1_980_000.0).abs() < 1e-6, "{p}");
        let (_, p) = pct(ProfilerSubsystem::Worldgen);
        assert!((p - 50_000.0 * 100.0 / 1_980_000.0).abs() < 1e-6);
        let total_pct: f64 = report.subsystem_us.iter().map(|(_, _, p)| p).sum();
        assert!((total_pct - 100.0).abs() < 1e-6, "{total_pct}");
    }

    #[test]
    fn perf1_profiler_formats_human_line_and_valid_json() {
        let mut profiler = ServerHotspotProfiler::default();
        for _ in 0..100 {
            profiler.record_tick_us(50_000);
        }
        profiler.record_chunk_work(DimId::Overworld, 12, -4, ProfilerSubsystem::Hoppers, 1_420_000);

        let human = profiler.summary(5).format_human();
        assert!(human.contains("TPS 20.0"), "{human}");
        assert!(human.contains("p99"), "{human}");
        assert!(
            human.contains("Overworld chunk (12, -4) [x=200, z=-56]: 14.2 ms/tick (hoppers)"),
            "{human}"
        );

        let json = profiler.summary(5).to_json();
        let parsed = jparse(json.as_bytes()).expect("report JSON must parse");
        let _ = parsed;
        assert!(json.contains("\"p99\":80.00") || json.contains("\"p99\":50.00"), "{json}");
        assert!(json.contains("\"dim\":\"minecraft:overworld\""), "{json}");
        assert!(json.contains("\"x\":200,\"z\":-56"), "{json}");
        assert!(json.contains("\"subsystem\":\"hoppers\""), "{json}");

        // The profiler's own convenience renderings use top five.
        assert_eq!(profiler.format_human(), profiler.summary(5).format_human());
    }

    #[test]
    fn perf1_profiler_bound_evicts_the_coldest_chunk() {
        let mut profiler = ServerHotspotProfiler::default();
        profiler.record_tick_us(50_000);
        // Fill the map to capacity; chunk (0,0) is a small cold entry.
        profiler.record_chunk_work(DimId::Overworld, 0, 0, ProfilerSubsystem::Mobs, 1);
        for i in 1..PROFILER_MAX_CHUNKS as i32 {
            profiler.record_chunk_work(DimId::Overworld, i, 7, ProfilerSubsystem::Mobs, 100);
        }
        assert_eq!(profiler.chunks.len(), PROFILER_MAX_CHUNKS);
        // A hot new arrival evicts the coldest (the 1-us entry), not itself.
        profiler.record_chunk_work(DimId::Nether, 1, 1, ProfilerSubsystem::Hoppers, 9_999);
        assert_eq!(profiler.chunks.len(), PROFILER_MAX_CHUNKS);
        assert!(!profiler.chunks.contains_key(&(DimId::Overworld, 0, 0)));
        assert!(profiler.chunks.contains_key(&(DimId::Nether, 1, 1)));
    }
}

#[cfg(test)]
mod ops_sre_tests {
    use super::*;

    #[test]
    fn ops_sre_tick_watchdog_classifies_healthy_warning_and_critical() {
        let mut watchdog = TickWatchdog::new();
        let now = Instant::now();
        // A fresh beat is healthy; beat() itself re-arms the heartbeat.
        watchdog.last_beat = now - Duration::from_millis(100);
        assert_eq!(
            watchdog.check_stall(now, 0),
            WatchdogVerdict::Healthy
        );
        watchdog.beat();
        assert_eq!(
            watchdog.check_stall(Instant::now(), 0),
            WatchdogVerdict::Healthy
        );
        assert!(WatchdogVerdict::Healthy.diagnostic().is_none());

        // Past the 5 s warn budget but inside the 30 s critical budget.
        watchdog.last_beat = now - Duration::from_millis(5_001);
        let verdict = watchdog.check_stall(now, 0);
        assert_eq!(verdict, WatchdogVerdict::Warning { stall_ms: 5_001 });
        let diag = verdict.diagnostic().expect("warning emits a diagnostic");
        assert!(diag.starts_with("MEGU-E-"), "{diag}");
        assert!(diag.contains("stall 5001ms"), "{diag}");
        assert!(diag.contains("severity=warning"), "{diag}");
        assert_eq!(diag.len(), "MEGU-E-".len() + 8 + " tick watchdog stall 5001ms severity=warning".len(), "{diag}");
        // The tag is deterministic: the same stall renders the same MEGU-E id.
        assert_eq!(
            verdict.diagnostic(),
            WatchdogVerdict::Warning { stall_ms: 5_001 }.diagnostic()
        );

        // Past the 30 s critical budget.
        watchdog.last_beat = now - Duration::from_millis(30_001);
        let verdict = watchdog.check_stall(now, 0);
        assert_eq!(
            verdict,
            WatchdogVerdict::CriticalStall { stall_ms: 30_001 }
        );
        let diag = verdict.diagnostic().expect("critical emits a diagnostic");
        assert!(diag.starts_with("MEGU-E-"), "{diag}");
        assert!(diag.contains("severity=critical"), "{diag}");

        // A caller tick budget can tighten the warn band but never loosen it.
        watchdog.last_beat = now - Duration::from_millis(60);
        assert_eq!(
            watchdog.check_stall(now, 50),
            WatchdogVerdict::Warning { stall_ms: 60 }
        );
        watchdog.last_beat = now - Duration::from_millis(4_000);
        assert_eq!(
            watchdog.check_stall(now, 60_000),
            WatchdogVerdict::Healthy,
            "a loose caller budget cannot push the warn past the configured 5 s"
        );
        // Custom thresholds and the inverted-band clamp.
        let mut tight = TickWatchdog::with_thresholds(10, 5);
        tight.last_beat = now - Duration::from_millis(11);
        assert_eq!(
            tight.check_stall(now, 0),
            WatchdogVerdict::CriticalStall { stall_ms: 11 },
            "critical clamps up to warn, so 11 ms is already critical"
        );
    }

    #[test]
    fn ops_sre_backup_rotation_skips_low_disk_and_prunes_oldest() {
        let policy = RollingBackupPolicy::default();
        assert_eq!(policy.max_backups, 12);
        assert_eq!(policy.min_free_bytes, 512 * 1024 * 1024);
        assert!(RollingBackupPolicy::new(0, 0).is_err());
        assert!(RollingBackupPolicy::new(129, 0).is_err());
        assert!(RollingBackupPolicy::new(1, 0).is_ok());
        assert!(RollingBackupPolicy::new(128, 0).is_ok());

        let names = |seqs: &[u64]| -> Vec<String> {
            seqs.iter()
                .map(|s| format!("world-backup-{s}.tar.gz"))
                .collect()
        };

        // Below the 512 MiB floor the backup is skipped cleanly — nothing is
        // pruned and the verdict carries both numbers for the operator.
        let low = plan_backup_rotation(&policy, 512 * 1024 * 1024 - 1, &names(&[1, 2, 3]));
        assert_eq!(
            low,
            BackupVerdict::SkippedLowDisk {
                free_bytes: 512 * 1024 * 1024 - 1,
                min_free_bytes: 512 * 1024 * 1024,
            }
        );
        // Exactly at the floor is allowed.
        assert!(matches!(
            plan_backup_rotation(&policy, 512 * 1024 * 1024, &[]),
            BackupVerdict::Proceed { .. }
        ));

        // Under the cap nothing is pruned; at the cap the oldest seq goes.
        let small = RollingBackupPolicy::new(3, 0).unwrap();
        assert_eq!(
            plan_backup_rotation(&small, u64::MAX, &names(&[1, 2])),
            BackupVerdict::Proceed { prune: vec![] }
        );
        // 3 existing + 1 incoming = 4 > max 3 -> prune exactly the oldest.
        assert_eq!(
            plan_backup_rotation(&small, u64::MAX, &names(&[1, 2, 3])),
            BackupVerdict::Proceed {
                prune: vec!["world-backup-1.tar.gz".to_string()]
            }
        );
        // Input order is irrelevant: seq — not lexical text — decides age.
        let mut shuffled = names(&[10, 2, 3]);
        shuffled.reverse();
        assert_eq!(
            plan_backup_rotation(&small, u64::MAX, &shuffled),
            BackupVerdict::Proceed {
                prune: vec!["world-backup-2.tar.gz".to_string()]
            }
        );
        // A bigger excess prunes oldest-first, count exact: 4 existing + 1
        // incoming against a cap of 2 drops the three oldest.
        let tiny = RollingBackupPolicy::new(2, 0).unwrap();
        assert_eq!(
            plan_backup_rotation(&tiny, u64::MAX, &names(&[4, 1, 7, 2])),
            BackupVerdict::Proceed {
                prune: vec![
                    "world-backup-1.tar.gz".to_string(),
                    "world-backup-2.tar.gz".to_string(),
                    "world-backup-4.tar.gz".to_string(),
                ]
            }
        );
    }

    #[test]
    fn ops_sre_backup_rotation_rejects_unsafe_names_fail_closed() {
        let policy = RollingBackupPolicy::default();
        for bad in [
            "../world-backup-1.tar.gz",
            "world-backup-../x.tar.gz",
            "world-backup-1.tar.gz/../../evil",
            "sub/world-backup-1.tar.gz",
            "world-backup-1.tar.gz\\evil",
            "world-backup-.tar.gz",
            "world-backup-abc.tar.gz",
            "world-backup--1.tar.gz",
            "world-backup-1.tar.gz ",
            "world-backup-1.zip",
            "backup-1.tar.gz",
            "world-backup-18446744073709551616.tar.gz", // u64 overflow
            "",
        ] {
            let existing = vec![bad.to_string()];
            assert_eq!(
                plan_backup_rotation(&policy, u64::MAX, &existing),
                BackupVerdict::RejectedName {
                    name: bad.to_string()
                },
                "{bad:?} must fail closed"
            );
        }
        // One bad apple spoils the listing: valid siblings are not pruned.
        let mut mixed = vec!["world-backup-1.tar.gz".to_string(), "notes.txt".to_string()];
        mixed.push("world-backup-2.tar.gz".to_string());
        assert!(matches!(
            plan_backup_rotation(&policy, u64::MAX, &mixed),
            BackupVerdict::RejectedName { .. }
        ));
        // Leading zeros still parse to a sequence and stay prunable.
        assert_eq!(
            plan_backup_rotation(
                &RollingBackupPolicy::new(1, 0).unwrap(),
                u64::MAX,
                &["world-backup-007.tar.gz".to_string()],
            ),
            BackupVerdict::Proceed {
                prune: vec!["world-backup-007.tar.gz".to_string()]
            }
        );
    }

    #[test]
    fn ops_sre_prometheus_snapshot_emits_valid_lines_and_scrubs_non_finite() {
        let snap = OperatorMetricsSnapshot {
            tps: 19.98,
            mspt_p50: 12.5,
            mspt_p95: 41.25,
            mspt_p99: 87.5,
            players_online: 3,
            loaded_chunks: 225,
            quarantined_faults: 2,
            rss_mib: 142.0,
            uptime_seconds: 3600.5,
        };
        let text = snap.format_prometheus();
        for name in [
            "megu_tps",
            "megu_mspt_p50",
            "megu_mspt_p95",
            "megu_mspt_p99",
            "megu_players_online",
            "megu_loaded_chunks",
            "megu_quarantined_faults",
            "megu_rss_mib",
            "megu_uptime_seconds",
        ] {
            assert!(text.contains(&format!("# TYPE {name} gauge\n")), "{name} missing TYPE");
            assert!(text.contains(&format!("# HELP {name} ")), "{name} missing HELP");
            let sample = text
                .lines()
                .find(|line| line.starts_with(&format!("{name} ")))
                .unwrap_or_else(|| panic!("{name} sample line missing in:\n{text}"));
            let value = sample.rsplit(' ').next().unwrap();
            assert!(
                value.parse::<f64>().is_ok(),
                "{name} sample is not a number: {value}"
            );
        }
        assert!(text.contains("megu_tps 19.98\n"), "{text}");
        assert!(text.contains("megu_players_online 3\n"), "{text}");
        assert!(text.contains("megu_quarantined_faults 2\n"), "{text}");
        assert!(text.contains("megu_uptime_seconds 3600.5\n"), "{text}");
        // Every line is a comment, a TYPE/HELP header, or `name value` — no
        // stray prose can break a scraper.
        for line in text.lines() {
            assert!(
                line.starts_with('#')
                    || line.split_once(' ').is_some_and(|(name, _)| {
                        name.starts_with("megu_") && !name.contains(' ')
                    }),
                "non exposition line: {line}"
            );
        }

        // NaN and both infinities scrub to 0, never "NaN"/"inf" on the wire.
        let dirty = OperatorMetricsSnapshot {
            tps: f64::NAN,
            mspt_p50: f64::INFINITY,
            mspt_p95: f64::NEG_INFINITY,
            mspt_p99: 0.0,
            players_online: 0,
            loaded_chunks: 0,
            quarantined_faults: 0,
            rss_mib: f64::NAN,
            uptime_seconds: f64::INFINITY,
        };
        let text = dirty.format_prometheus();
        assert!(!text.contains("NaN"), "{text}");
        assert!(!text.contains("inf"), "{text}");
        for name in [
            "megu_tps",
            "megu_mspt_p50",
            "megu_mspt_p95",
            "megu_rss_mib",
            "megu_uptime_seconds",
        ] {
            assert!(
                text.contains(&format!("{name} 0\n")),
                "{name} must scrub to 0:\n{text}"
            );
        }
        assert!(text.contains("megu_mspt_p99 0\n"), "{text}");
    }
}

// ---------------------------------------------------------------------------
// Lane OPS-ADMIN-BUILTIN: the built-in server-administration surface that
// replaces the four external admin plugins — live block audit + OP-4 grief
// rollback, chunk land claims + spawn protection, rotating-backup status and
// a live Prometheus scrape — all reached through `/megu` subcommands. The
// command layer (`teams_and_commands.rs`) owns the permission gates; this
// section owns the state and the deterministic query/restore mechanics.
// ---------------------------------------------------------------------------

/// Bounded ring of live block edits. 8_192 entries covers several minutes of
/// heavy griefing at ~50 edits/sec while staying under ~1 MiB resident.
const LIVE_BLOCK_AUDIT_CAP: usize = 8_192;
static LIVE_BLOCK_AUDIT_SEQ: AtomicU64 = AtomicU64::new(1);

/// One recorded block transition. `old_state`/`new_state` are registry state
/// ids, exactly what `block_at_dim`/`set_block_dim` speak.
#[derive(Clone, Debug)]
struct LiveBlockAuditEntry {
    /// Monotonic sequence — the chronological order rollback walks back.
    #[allow(dead_code)]
    seq: u64,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    actor: String,
    old_state: u32,
    new_state: u32,
    tick: u64,
}

fn live_block_audit() -> &'static Mutex<std::collections::VecDeque<LiveBlockAuditEntry>> {
    static AUDIT: OnceLock<Mutex<std::collections::VecDeque<LiveBlockAuditEntry>>> =
        OnceLock::new();
    AUDIT.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

/// Record one live block edit. Called by the command-driven writers
/// (`/setblock`, `/fill`, `/clone`) with the executor's name — or `"Server"`
/// for console/RCON frames — so `/megu audit` and `/megu rollback` always see
/// who changed the world and when (server tick, never a wall clock).
fn record_live_block_audit(
    dim: DimId,
    pos: (i32, i32, i32),
    actor: &str,
    old_state: u32,
    new_state: u32,
    tick: u64,
) {
    if old_state == new_state {
        return;
    }
    let entry = LiveBlockAuditEntry {
        seq: LIVE_BLOCK_AUDIT_SEQ.fetch_add(1, Ordering::Relaxed),
        dim,
        x: pos.0,
        y: pos.1,
        z: pos.2,
        actor: actor.to_owned(),
        old_state,
        new_state,
        tick,
    };
    let mut audit = live_block_audit()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if audit.len() >= LIVE_BLOCK_AUDIT_CAP {
        audit.pop_front();
    }
    audit.push_back(entry);
}

/// Deterministic audit query: every recorded edit matching the filters,
/// returned oldest-first so callers can iterate or reverse for rollback.
/// `center`/`radius` is a 3-D Euclidean bound around a block position in
/// `center_dim`; `dim` alone restricts the dimension without a radius.
#[derive(Default)]
struct LiveBlockAuditQuery {
    actor: Option<String>,
    dim: Option<DimId>,
    center: Option<(DimId, i32, i32, i32)>,
    radius: i32,
    since_tick: Option<u64>,
}

fn query_live_block_audit(query: &LiveBlockAuditQuery) -> Vec<LiveBlockAuditEntry> {
    let audit = live_block_audit()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let radius_sq = i64::from(query.radius.max(0)) * i64::from(query.radius.max(0));
    audit
        .iter()
        .filter(|entry| {
            if let Some(actor) = &query.actor {
                if entry.actor != *actor {
                    return false;
                }
            }
            if let Some(dim) = query.dim {
                if entry.dim != dim {
                    return false;
                }
            }
            if let Some(since) = query.since_tick {
                if entry.tick < since {
                    return false;
                }
            }
            if let Some((dim, cx, cy, cz)) = query.center {
                if entry.dim != dim {
                    return false;
                }
                let dx = i64::from(entry.x) - i64::from(cx);
                let dy = i64::from(entry.y) - i64::from(cy);
                let dz = i64::from(entry.z) - i64::from(cz);
                if dx * dx + dy * dy + dz * dz > radius_sq {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect()
}

fn live_block_audit_len() -> usize {
    live_block_audit()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

/// What one `/megu rollback` pass did, persisted so operators can audit the
/// audit tool itself.
#[derive(Clone, Debug)]
#[allow(dead_code)]
struct LiveRollbackSummary {
    tick: u64,
    op: String,
    target: String,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    radius: i32,
    matched: usize,
    restored: usize,
    skipped: usize,
    truncated: bool,
}

const LIVE_ROLLBACK_CAP: usize = 256;

fn live_rollback_log() -> &'static Mutex<std::collections::VecDeque<LiveRollbackSummary>> {
    static LOG: OnceLock<Mutex<std::collections::VecDeque<LiveRollbackSummary>>> =
        OnceLock::new();
    LOG.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

fn record_live_rollback_summary(summary: LiveRollbackSummary) {
    let mut log = live_rollback_log()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if log.len() >= LIVE_ROLLBACK_CAP {
        log.pop_front();
    }
    log.push_back(summary);
}

#[allow(dead_code)]
fn live_rollback_summaries() -> Vec<LiveRollbackSummary> {
    live_rollback_log()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// Restore audited blocks newest-first. An entry is only rolled back while
/// the world still shows the state the audit recorded as `new_state` — a
/// later edit (by anyone) makes the cell diverge and it is skipped rather
/// than clobbered, which is what makes reverse-chronological rollback safe
/// against interleaved grief. Returns `(restored, skipped, applied)` where
/// `applied` counts the entries actually considered (bounded by `max_blocks`).
fn rollback_live_block_entries(
    entries_newest_first: &[LiveBlockAuditEntry],
    max_blocks: usize,
) -> (usize, usize, usize) {
    let mut restored = 0usize;
    let mut skipped = 0usize;
    let mut applied = 0usize;
    for entry in entries_newest_first.iter().take(max_blocks) {
        applied += 1;
        if block_at_dim(entry.dim, entry.x, entry.y, entry.z) != entry.new_state {
            skipped += 1;
            continue;
        }
        set_block_dim(entry.dim, entry.x, entry.y, entry.z, entry.old_state);
        note_block_update_dim(entry.dim, entry.x, entry.y, entry.z, entry.old_state);
        let mut body = Vec::with_capacity(12);
        write_pos(&mut body, entry.x, entry.y, entry.z);
        write_varint(
            &mut body,
            i64::from(visible_block_update_state(
                entry.dim,
                entry.x,
                entry.y,
                entry.z,
                entry.old_state,
            )),
        );
        broadcast_chunk_dim(entry.dim, entry.x >> 4, entry.z >> 4, |_c| {
            (0x09, body.clone())
        });
        restored += 1;
    }
    (restored, skipped, applied)
}

// ------------------------------------------------------------- land claims

use megu_server_files::claims::{
    ClaimAction, ClaimChunkKey, ClaimOwner, ClaimPermissionSet, ClaimsWorld, SubClaimId,
};

/// One chunk's claim: who owns it, who the owner trusts, and when (server
/// tick) the claim was staked. Keyed `(dim, chunk_x, chunk_z)`.
#[derive(Clone)]
struct LiveClaimRecord {
    owner: String,
    trusted: std::collections::BTreeSet<String>,
    created_tick: u64,
}

/// The live claim store: the name-keyed per-chunk view `/megu claim` and the
/// dig/place predicate read, backed by a persistent
/// [`ClaimsWorld`](megu_server_files::claims::ClaimsWorld) that owns
/// durability (`openpac_claims.json`/`openpac_parties.json` beside the world
/// save) and the richer OpenPAC-style enforcement matrix the cross-border
/// helpers below consult. Every mutation through this type mirrors into the
/// world and persists, so a restart restores exactly what was claimed.
struct LiveClaims {
    records: HashMap<(DimId, i32, i32), LiveClaimRecord>,
    world: ClaimsWorld,
    /// The directory claims persist into; `None` under `cfg(test)` so unit
    /// tests never write claim files into the checkout — persistence itself
    /// is exercised through `LiveClaims::load` on a scratch dir.
    dir: Option<std::path::PathBuf>,
}

/// The directory the global store loads from and saves into: the world save
/// directory in production. Unit tests get no directory — the whole point of
/// the split is that command-path tests stay in memory while the persistence
/// tests open scratch dirs explicitly.
#[cfg(not(test))]
fn live_claims_store_dir() -> Option<std::path::PathBuf> {
    Some(world_save_dir())
}

#[cfg(test)]
fn live_claims_store_dir() -> Option<std::path::PathBuf> {
    None
}

/// The [`ClaimChunkKey`] for the map key `(dim, cx, cz)`, or `None` when the
/// dimension cannot be named (`DimId` slots no datapack registered).
fn live_claim_world_key(dim: DimId, cx: i32, cz: i32) -> Option<ClaimChunkKey> {
    ClaimChunkKey::new(dim.dimension_id(), cx, cz).ok()
}

impl LiveClaims {
    fn empty(dir: Option<std::path::PathBuf>) -> Self {
        Self {
            records: HashMap::new(),
            world: ClaimsWorld::new(),
            dir,
        }
    }

    /// Load the persistent world from `dir` (a missing file is an empty
    /// store, never a crash) and rebuild the live map from it.
    fn load(dir: std::path::PathBuf) -> Self {
        let world = match ClaimsWorld::load_from_dir(&dir) {
            Ok(world) => world,
            Err(error) => {
                eprintln!(
                    "[spine] could not load claims from {}: {error}",
                    dir.display()
                );
                ClaimsWorld::new()
            }
        };
        let mut this = Self {
            records: HashMap::new(),
            world,
            dir: Some(dir),
        };
        this.rebuild_records();
        this
    }

    /// Regenerate `records` from the world after a load: player claims keep
    /// their owner; a permission override holding the full trusted set reads
    /// back as a trusted name; `created_tick` is session state and restarts
    /// at 0.
    fn rebuild_records(&mut self) {
        self.records = self
            .world
            .claims()
            .iter()
            .filter_map(|(wkey, claim)| {
                let dim = DimId::parse_dimension_id(&wkey.dimension)?;
                let ClaimOwner::Player(owner) = &claim.owner else {
                    return None;
                };
                let sub = SubClaimId::parse(&claim.sub_claim).unwrap_or_else(|_| SubClaimId::main());
                let trusted = self
                    .world
                    .protection_config(owner, &sub)
                    .player_overrides
                    .iter()
                    .filter(|(_, perms)| **perms == ClaimPermissionSet::trusted_builder())
                    .map(|(name, _)| name.clone())
                    .collect();
                Some((
                    (dim, wkey.cx, wkey.cz),
                    LiveClaimRecord {
                        owner: owner.clone(),
                        trusted,
                        created_tick: 0,
                    },
                ))
            })
            .collect();
    }

    /// Persist the world; `None` dir (unit-test builds) is a no-op. Save
    /// failures are reported, not propagated — losing durability must not
    /// crash the command path it serves.
    fn persist(&self) {
        if let Some(dir) = &self.dir {
            if let Err(error) = self.world.save_to_dir(dir) {
                eprintln!("[spine] could not save claims to {}: {error}", dir.display());
            }
        }
    }

    /// Mirror one map record into the world: the claim lands (or is retaken)
    /// under `record.owner` and each trusted name becomes a full-access
    /// per-player override. The live quota lives on the map, so the mirror's
    /// own quota is lifted to never refuse the sync.
    fn mirror_insert(&mut self, key: &(DimId, i32, i32), record: &LiveClaimRecord) {
        let Some(wkey) = live_claim_world_key(key.0, key.1, key.2) else {
            return;
        };
        let sub = SubClaimId::main();
        if self.world.claim_at(&wkey).is_some() {
            let _ = self.world.unclaim_chunk_as("", true, &wkey);
        }
        self.world
            .protection_config_mut(&record.owner, &sub)
            .max_claims = usize::MAX;
        if self
            .world
            .claim_chunk_as(&record.owner, true, wkey, &sub)
            .is_err()
        {
            return;
        }
        for name in &record.trusted {
            self.world.set_player_permission_override(
                &record.owner,
                &sub,
                name,
                ClaimPermissionSet::trusted_builder(),
            );
        }
    }

    fn get(&self, key: &(DimId, i32, i32)) -> Option<&LiveClaimRecord> {
        self.records.get(key)
    }

    fn values(&self) -> impl Iterator<Item = &LiveClaimRecord> {
        self.records.values()
    }

    fn insert(
        &mut self,
        key: (DimId, i32, i32),
        record: LiveClaimRecord,
    ) -> Option<LiveClaimRecord> {
        let old = self.records.insert(key, record.clone());
        self.mirror_insert(&key, &record);
        self.persist();
        old
    }

    fn remove(&mut self, key: &(DimId, i32, i32)) -> Option<LiveClaimRecord> {
        let old = self.records.remove(key);
        if old.is_some() {
            if let Some(wkey) = live_claim_world_key(key.0, key.1, key.2) {
                let _ = self.world.unclaim_chunk_as("", true, &wkey);
            }
            self.persist();
        }
        old
    }

    /// Set or clear `name`'s per-chunk trust; mirrored into the world as a
    /// full-access per-player override and persisted. `false` when the chunk
    /// is unclaimed.
    fn set_trusted(&mut self, key: &(DimId, i32, i32), name: &str, trusted: bool) -> bool {
        let Some(record) = self.records.get_mut(key) else {
            return false;
        };
        if trusted {
            record.trusted.insert(name.to_owned());
        } else {
            record.trusted.remove(name);
        }
        let owner = record.owner.clone();
        let sub = SubClaimId::main();
        if trusted {
            self.world.set_player_permission_override(
                &owner,
                &sub,
                name,
                ClaimPermissionSet::trusted_builder(),
            );
        } else {
            self.world
                .clear_player_permission_override(&owner, &sub, name);
        }
        self.persist();
        true
    }

    /// Test resets empty the whole store — map and world together — so no
    /// claim or roster entry can leak between fixtures.
    #[cfg(test)]
    fn clear(&mut self) {
        self.records.clear();
        self.world = ClaimsWorld::new();
        self.persist();
    }

    /// Mutate the backing world for an operation the map does not mirror
    /// (permission matrices, party create/join/leave, forceload), then
    /// persist — the one door that keeps every state change durable.
    fn update_world(&mut self, f: impl FnOnce(&mut ClaimsWorld)) {
        f(&mut self.world);
        self.persist();
    }

    /// Resolve a uuid (or an already-name string) to the claim-space
    /// identity: the roster's latest name for the uuid, else the string
    /// itself — which fails closed against name-keyed claims.
    fn actor_name(&self, uuid_or_name: &str) -> String {
        self.world
            .known_players()
            .get(uuid_or_name)
            .map(|record| record.name.clone())
            .unwrap_or_else(|| uuid_or_name.to_owned())
    }

    /// The shared verdict behind the player-facing helpers: a live-record
    /// owner or per-chunk trusted name always passes; everyone else takes the
    /// world's full matrix (per-player overrides, party/ally trust,
    /// `everyone_else`, the `protect_*` floor).
    fn player_allowed(
        &self,
        dim: DimId,
        bx: i32,
        bz: i32,
        actor_name: &str,
        action: ClaimAction,
    ) -> bool {
        let key = claim_chunk_key(dim, bx, bz);
        if let Some(record) = self.records.get(&key) {
            if record.owner == actor_name || record.trusted.contains(actor_name) {
                return true;
            }
        }
        match live_claim_world_key(key.0, key.1, key.2) {
            Some(wkey) => self
                .world
                .check_action(&wkey, Some(actor_name), action)
                .is_allowed(),
            None => !self.records.contains_key(&key),
        }
    }
}

fn live_claims() -> &'static Mutex<LiveClaims> {
    static CLAIMS: OnceLock<Mutex<LiveClaims>> = OnceLock::new();
    CLAIMS.get_or_init(|| {
        Mutex::new(match live_claims_store_dir() {
            Some(dir) => LiveClaims::load(dir),
            None => LiveClaims::empty(None),
        })
    })
}

/// Per-player claim quota in chunks. Generous enough for a normal base,
/// bounded so one player cannot wall off the world.
static LIVE_CLAIM_QUOTA: AtomicUsize = AtomicUsize::new(256);

fn live_claim_quota() -> usize {
    LIVE_CLAIM_QUOTA.load(Ordering::Relaxed)
}

/// Spawn-protection radius in blocks around the world spawn; `0` (the
/// vanilla-faithful default) disables it entirely.
static SPAWN_PROTECT_RADIUS: AtomicI32 = AtomicI32::new(0);
const SPAWN_PROTECT_RADIUS_MAX: i32 = 1024;

fn live_spawn_protect_radius() -> i32 {
    SPAWN_PROTECT_RADIUS.load(Ordering::Relaxed)
}

fn set_live_spawn_protect_radius(radius: i32) -> Result<(), String> {
    if !(0..=SPAWN_PROTECT_RADIUS_MAX).contains(&radius) {
        return Err(format!(
            "spawn protection radius must be in 0..={SPAWN_PROTECT_RADIUS_MAX}"
        ));
    }
    SPAWN_PROTECT_RADIUS.store(radius, Ordering::Relaxed);
    Ok(())
}

fn claim_chunk_key(dim: DimId, x: i32, z: i32) -> (DimId, i32, i32) {
    (dim, x.div_euclid(16), z.div_euclid(16))
}

/// May `player_name` at `op_level` modify the block column at (x, z) in `dim`?
/// OP level 4 always may (the admin override). Inside an armed spawn-protect
/// radius on the Overworld nobody below OP-4 may, claim or not — the vanilla
/// spawn-protection semantic. Otherwise a claimed chunk admits only its owner
/// and trusted allies; unclaimed chunks are open to everyone.
///
/// The live record answers first — it is the exact per-chunk trust view the
/// `/megu claim` commands write. A chunk the map does not key but the
/// persistent world does (a claim restored without a session record, or a
/// server/admin claim) falls back to the `ClaimsWorld` block verdict, so the
/// durable store is authoritative wherever the live view is silent.
fn can_player_modify_block(
    dim: DimId,
    x: i32,
    z: i32,
    player_name: &str,
    op_level: u8,
) -> bool {
    if op_level >= 4 {
        return true;
    }
    let radius = live_spawn_protect_radius();
    if radius > 0 && dim == DimId::Overworld {
        let spawn = *WORLD_SPAWN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dx = f64::from(x) - spawn.0;
        let dz = f64::from(z) - spawn.2;
        if dx * dx + dz * dz <= f64::from(radius) * f64::from(radius) {
            return false;
        }
    }
    let key = claim_chunk_key(dim, x, z);
    let claims = live_claims()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(record) = claims.get(&key) {
        return record.owner == player_name || record.trusted.contains(player_name);
    }
    match live_claim_world_key(key.0, key.1, key.2) {
        Some(wkey) => claims
            .world
            .check_action(&wkey, Some(player_name), ClaimAction::BlockBreak)
            .is_allowed(),
        None => true,
    }
}

/// The shared body of the player-facing claim helpers: an op bypasses like
/// the OP-4 override, the uuid↔name roster learns the caller, and the
/// claim-space name takes the owner/trust/matrix verdict.
fn live_claims_player_action(
    dim: DimId,
    bx: i32,
    bz: i32,
    player_uuid: &str,
    player_name: &str,
    is_op: bool,
    action: ClaimAction,
) -> bool {
    if is_op {
        return true;
    }
    let mut claims = live_claims()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !player_uuid.is_empty() {
        claims.world.record_player_seen(
            player_uuid,
            player_name,
            TICK.load(Ordering::Relaxed),
        );
    }
    claims.player_allowed(dim, bx, bz, player_name, action)
}

/// May the player open a container (chest, barrel, shulker, hopper, machine
/// UI) in the chunk holding `(bx, bz)`? Owner and per-chunk trusted names
/// always may; everyone else needs `containers` under the claim's matrix.
pub(crate) fn can_player_open_container(
    dim: DimId,
    bx: i32,
    bz: i32,
    player_uuid: &str,
    player_name: &str,
    is_op: bool,
) -> bool {
    live_claims_player_action(
        dim,
        bx,
        bz,
        player_uuid,
        player_name,
        is_op,
        ClaimAction::ContainerInteract,
    )
}

/// May the player right-click/leash/shear/ride/name an entity standing in
/// the chunk holding `(bx, bz)` (armor stands, item frames, villagers,
/// pets)?
pub(crate) fn can_player_interact_entity(
    dim: DimId,
    bx: i32,
    bz: i32,
    player_uuid: &str,
    player_name: &str,
    is_op: bool,
) -> bool {
    live_claims_player_action(
        dim,
        bx,
        bz,
        player_uuid,
        player_name,
        is_op,
        ClaimAction::EntityInteract,
    )
}

/// May the player attack or shoot an entity standing in the chunk holding
/// `(bx, bz)`?
pub(crate) fn can_player_damage_entity(
    dim: DimId,
    bx: i32,
    bz: i32,
    player_uuid: &str,
    player_name: &str,
    is_op: bool,
) -> bool {
    live_claims_player_action(
        dim,
        bx,
        bz,
        player_uuid,
        player_name,
        is_op,
        ClaimAction::EntityAttack,
    )
}

/// May an explosion whose blast originates at `(src_bx, src_bz)` damage the
/// block column at `(target_bx, target_bz)`? Same-owner blasts always cross;
/// a credited actor needs break/damage permission inside the target claim.
/// `actor_uuid` resolves through the uuid↔name roster before the matrix sees
/// it; a dimension the world cannot name falls back to "claimed ⇒ denied".
pub(crate) fn can_explosion_damage_block(
    dim: DimId,
    src_bx: i32,
    src_bz: i32,
    target_bx: i32,
    target_bz: i32,
    actor_uuid: Option<&str>,
) -> bool {
    let claims = live_claims()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let src_key = claim_chunk_key(dim, src_bx, src_bz);
    let target_key = claim_chunk_key(dim, target_bx, target_bz);
    let (Some(source), Some(target)) = (
        live_claim_world_key(src_key.0, src_key.1, src_key.2),
        live_claim_world_key(target_key.0, target_key.1, target_key.2),
    ) else {
        return claims.get(&target_key).is_none();
    };
    let actor = actor_uuid.map(|uuid| claims.actor_name(uuid));
    claims
        .world
        .check_action(
            &target,
            actor.as_deref(),
            ClaimAction::ExplosionFrom {
                source,
                actor_uuid: actor.clone(),
            },
        )
        .is_allowed()
}

/// May `actor_uuid` body-push, knockback-displace or (`is_lure`) tempt/lure
/// an entity out of the chunk holding `(bx, bz)`?
pub(crate) fn can_entity_be_pushed_or_lured(
    dim: DimId,
    bx: i32,
    bz: i32,
    actor_uuid: &str,
    is_lure: bool,
) -> bool {
    let claims = live_claims()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let actor = claims.actor_name(actor_uuid);
    let action = if is_lure {
        ClaimAction::EntityTemptOrLure
    } else {
        ClaimAction::EntityPushOrDisplace
    };
    claims.player_allowed(dim, bx, bz, &actor, action)
}

/// May a vehicle (boat, minecart, modded ride) move from `(src_bx, src_bz)`
/// into the chunk holding `(target_bx, target_bz)` — or may `actor_uuid`
/// force an entity to board one across that border?
pub(crate) fn can_vehicle_enter_claim(
    dim: DimId,
    src_bx: i32,
    src_bz: i32,
    target_bx: i32,
    target_bz: i32,
    actor_uuid: Option<&str>,
) -> bool {
    let claims = live_claims()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let src_key = claim_chunk_key(dim, src_bx, src_bz);
    let target_key = claim_chunk_key(dim, target_bx, target_bz);
    let (Some(source), Some(target)) = (
        live_claim_world_key(src_key.0, src_key.1, src_key.2),
        live_claim_world_key(target_key.0, target_key.1, target_key.2),
    ) else {
        return claims.get(&target_key).is_none();
    };
    let actor = actor_uuid.map(|uuid| claims.actor_name(uuid));
    claims
        .world
        .check_action(
            &target,
            actor.as_deref(),
            ClaimAction::VehicleEnterOrInject {
                source,
                actor_uuid: actor.clone(),
            },
        )
        .is_allowed()
}

/// The live-claims enforcement surface, exercised end to end so every helper
/// the `play_protocol`/`combat_and_health` lanes wire stays a live item —
/// `pub` keeps it out of dead-code analysis while the callers land.
#[doc(hidden)]
pub fn live_claims_enforcement_surface() -> bool {
    can_player_open_container(DimId::Overworld, 0, 0, "", "", true)
        && can_player_interact_entity(DimId::Overworld, 0, 0, "", "", true)
        && can_player_damage_entity(DimId::Overworld, 0, 0, "", "", true)
        && can_explosion_damage_block(DimId::Overworld, 0, 0, 0, 0, None)
        && can_entity_be_pushed_or_lured(DimId::Overworld, 0, 0, "", false)
        && can_vehicle_enter_claim(DimId::Overworld, 0, 0, 0, 0, None)
}

// ------------------------------------------------------- backup & metrics

/// The directory the rotating `world-backup-<seq>.tar.gz` generations live
/// in: `backups/` beside the live world save.
fn live_backup_dir() -> std::path::PathBuf {
    world_save_dir().join("backups")
}

/// Basenames in `dir` that look like backup generations, sorted. Only
/// `world-backup-*` names are considered — the rotation planner itself still
/// rejects any candidate that is not a strict `world-backup-<seq>.tar.gz`,
/// so a malformed name fails closed instead of being pruned blindly.
fn backup_dir_listing(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("world-backup-"))
        .collect();
    names.sort();
    names
}

/// Free bytes available to `dir`'s filesystem (`f_bavail * f_frsize`), or
/// `None` where the platform cannot report it — the caller then shows the
/// prune plan without a disk verdict rather than guessing.
#[cfg(target_os = "linux")]
fn backup_volume_free_bytes(dir: &std::path::Path) -> Option<u64> {
    #[repr(C)]
    struct Statvfs {
        f_bsize: u64,
        f_frsize: u64,
        f_blocks: u64,
        f_bfree: u64,
        f_bavail: u64,
        f_files: u64,
        f_ffree: u64,
        f_favail: u64,
        f_fsid: u64,
        f_flag: u64,
        f_namemax: u64,
    }
    unsafe extern "C" {
        fn statvfs(path: *const std::ffi::c_char, buf: *mut Statvfs) -> i32;
    }
    let c_path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    let mut stat: Statvfs = unsafe { std::mem::zeroed() };
    if unsafe { statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail.saturating_mul(stat.f_frsize))
}

#[cfg(not(target_os = "linux"))]
fn backup_volume_free_bytes(_dir: &std::path::Path) -> Option<u64> {
    None
}

/// The sequence the next `world-backup-<seq>.tar.gz` would take: one past
/// the highest well-formed existing name, or 1 for an empty/missing dir.
fn backup_next_seq(existing: &[String]) -> u64 {
    existing
        .iter()
        .filter_map(|name| backup_seq_of(name))
        .max()
        .map_or(1, |max| max.saturating_add(1))
}

/// The prune half of [`plan_backup_rotation`], split out so the status/plan
/// reports can show *what would be deleted* even when free disk space cannot
/// be measured. `Err(name)` names the first candidate that is not a strict
/// backup basename — the same fail-closed rejection the planner applies.
fn backup_rotation_prune(existing: &[String], max_backups: usize) -> Result<Vec<String>, String> {
    let mut ordered: Vec<(u64, &str)> = Vec::with_capacity(existing.len());
    for name in existing {
        match backup_seq_of(name) {
            Some(seq) => ordered.push((seq, name.as_str())),
            None => return Err(name.clone()),
        }
    }
    ordered.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    let excess = (ordered.len() + 1).saturating_sub(max_backups);
    Ok(ordered
        .iter()
        .take(excess)
        .map(|(_, name)| (*name).to_owned())
        .collect())
}

/// `/megu backup plan` — the live rotation decision for the default policy:
/// what the next generation would be named, which oldest files it would
/// prune, and whether the disk guard passes. Every number comes from the
/// real backup directory and filesystem, never a guess.
fn format_backup_plan_report() -> String {
    let policy = RollingBackupPolicy::default();
    let dir = live_backup_dir();
    let existing = backup_dir_listing(&dir);
    let next = backup_next_seq(&existing);
    let prune = match backup_rotation_prune(&existing, policy.max_backups) {
        Ok(prune) => prune,
        Err(name) => {
            return format!(
                "Backup plan refused: '{name}' is not a world-backup-<seq>.tar.gz name"
            )
        }
    };
    let free = backup_volume_free_bytes(&dir);
    let mut out = format!(
        "Next backup: world-backup-{next}.tar.gz; existing {} generation(s)\n",
        existing.len()
    );
    if prune.is_empty() {
        out.push_str("Prune: none (retention not exceeded)\n");
    } else {
        out.push_str(&format!("Prune {} oldest: {}\n", prune.len(), prune.join(", ")));
    }
    match free {
        Some(free_bytes) if free_bytes < policy.min_free_bytes => out.push_str(&format!(
            "Disk guard: SKIP — {} free bytes under the {} byte floor\n",
            free_bytes, policy.min_free_bytes
        )),
        Some(free_bytes) => out.push_str(&format!(
            "Disk guard: OK — {} free bytes (floor {})\n",
            free_bytes, policy.min_free_bytes
        )),
        None => out.push_str("Disk guard: free space unavailable on this platform\n"),
    }
    out
}

/// `/megu backup status` — policy bounds plus the live directory state, so an
/// operator sees retention config and reality in one reply.
fn format_backup_status_report() -> String {
    let policy = RollingBackupPolicy::default();
    let dir = live_backup_dir();
    let existing = backup_dir_listing(&dir);
    let free = backup_volume_free_bytes(&dir);
    let mut out = format!(
        "Backup dir: {}\nRetention: keep at most {} world-backup-*.tar.gz, min free {} bytes\n",
        dir.display(),
        policy.max_backups,
        policy.min_free_bytes
    );
    out.push_str(&format!("Existing backups: {}\n", existing.len()));
    for name in existing.iter().take(8) {
        out.push_str(&format!("  {name}\n"));
    }
    if existing.len() > 8 {
        out.push_str(&format!("  … and {} more\n", existing.len() - 8));
    }
    match free {
        Some(bytes) => out.push_str(&format!("Free disk: {bytes} bytes\n")),
        None => out.push_str("Free disk: unavailable on this platform\n"),
    }
    out.push_str(&format!(
        "Next sequence: world-backup-{}.tar.gz\n",
        backup_next_seq(&existing)
    ));
    out
}

/// `/megu metrics` — a live Prometheus text exposition built from the same
/// sources `/status` and `/profile` read: the hotspot profiler's tick
/// percentiles, the online player list, the hot chunk set and process RSS.
fn live_operator_metrics_prometheus() -> String {
    let report = live_hotspot_profiler()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .summary(0);
    let health = operator_health_snapshot();
    OperatorMetricsSnapshot {
        tps: report.tps,
        mspt_p50: report.mspt_p50,
        mspt_p95: report.mspt_p95,
        mspt_p99: report.mspt_p99,
        players_online: health.players_online as u64,
        loaded_chunks: health.loaded_chunks as u64,
        quarantined_faults: 0,
        rss_mib: memory_usage_mb().map_or(0.0, |mib| mib as f64),
        uptime_seconds: TICK.load(Ordering::Relaxed) as f64 / f64::from(TICKS_PER_SECOND),
    }
    .format_prometheus()
}

// ---------------------------------------------------------------------------
// Lane OPS-ADMIN-CORE: the server administration & lifecycle core.
//
//   * `ServerLifecycleController` — sleep-on-empty / optional auto-shutdown
//     state machine with wake-on-join. All times arrive as `Instant`
//     arguments, so tests drive the clock and production stays off
//     wall-clock reads entirely.
//   * `SupervisorPolicy` — crash auto-restart with exponential backoff and a
//     5-in-60-seconds crash-loop guard, plus the `MEGU_RESTART_EXIT_CODE`
//     clean-restart convention.
//   * `RemoteControlServer` — opt-in, zero-dependency 4-button mobile remote:
//     one non-blocking `poll_once` step over a plain `TcpListener`, scoped
//     bearer tokens compared constant-time by their SHA-256 digests.
//   * `SurvivalModRegistry` + `survival_mod_allows_command` — a moderation
//     role below OP that can police players but can never touch the world.
//   * `HubServerDirectory` / `MultiWorldRegistry` — the `[servers]` hub table
//     used for stop/restart player transfer, and managed extra worlds that
//     map onto the standard `dimensions/megu/<name>/region` layout.
//   * `format_operator_status_summary` — the one-screen `/megu status` and
//     startup summary.
// ---------------------------------------------------------------------------

// `admin_core_items!` stamps the dead-code allowance on every item below: the
// same per-item allowance the rest of this file carries, applied in one
// place so a wire-ready surface does not need the attribute restated on
// each item. Every expanded item still carries it verbatim.
macro_rules! admin_core_items {
    ($($item:item)*) => {
        $(#[allow(dead_code)] $item)*
    };
}

admin_core_items! {

// ----------------------------------------------------- lifecycle ----

/// Exit code a megu process returns when the operator asked for a restart
/// rather than a halt: the wrapper/supervisor relaunches immediately.
const MEGU_RESTART_EXIT_CODE: i32 = 42;

/// The MOTD the minimal sleeping listener answers pings with.
const SLEEPING_LISTEN_MOTD: &str = "Sleeping — Join to wake!";

/// Where the world server sits in its power lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LifecycleState {
    /// World is loaded and ticking normally.
    Running,
    /// World saved and RAM released; a minimal listener keeps the slot
    /// occupied and wakes the world on the next whitelisted join.
    Sleeping,
    /// Final state after `SaveAndShutdown` — nothing else happens.
    Stopped,
}

/// What the supervisor must do after a lifecycle observation. Decisions
/// stay here; the filesystem/network side effects belong to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LifecycleAction {
    /// Nothing to do.
    None,
    /// Save the world, release chunk RAM, keep the sleep listener.
    SaveAndHibernate,
    /// Save the world and terminate the process.
    SaveAndShutdown,
    /// Bring the world back up for an incoming player.
    WakeWorld,
    /// Refuse the handshake without touching the sleeping world.
    RejectLogin,
}

/// Sleep-on-empty controller. While `online_players == 0` a monotonic
/// timer runs: past `idle_sleep` the server hibernates, past the optional
/// `idle_shutdown` (measured from the same empty start) it stops. Any
/// player count above zero resets the clock.
struct ServerLifecycleController {
    state: LifecycleState,
    idle_sleep: Duration,
    idle_shutdown: Option<Duration>,
    empty_since: Option<Instant>,
}

impl ServerLifecycleController {
    fn new(idle_sleep_secs: u64, idle_shutdown_secs: Option<u64>) -> Self {
        Self {
            state: LifecycleState::Running,
            idle_sleep: Duration::from_secs(idle_sleep_secs),
            idle_shutdown: idle_shutdown_secs.map(Duration::from_secs),
            empty_since: None,
        }
    }

    fn state(&self) -> LifecycleState {
        self.state
    }

    /// MOTD for the minimal listener kept while `Sleeping`.
    fn sleeping_motd() -> &'static str {
        SLEEPING_LISTEN_MOTD
    }

    /// Feed one observation of the online count. Call it once per tick or
    /// per supervisor pass; `now` is the caller's monotonic clock.
    fn update(&mut self, now: Instant, online_players: usize) -> LifecycleAction {
        if online_players > 0 {
            self.empty_since = None;
            if self.state == LifecycleState::Sleeping {
                // Someone is back online without a login handshake we saw
                // (RCON session, proxy transfer): treat it as a wake.
                self.state = LifecycleState::Running;
                return LifecycleAction::WakeWorld;
            }
            return LifecycleAction::None;
        }
        if self.state == LifecycleState::Stopped {
            return LifecycleAction::None;
        }
        let empty_since = *self.empty_since.get_or_insert(now);
        let idle = now.saturating_duration_since(empty_since);
        if let Some(limit) = self.idle_shutdown {
            if idle >= limit {
                self.state = LifecycleState::Stopped;
                return LifecycleAction::SaveAndShutdown;
            }
        }
        if self.state == LifecycleState::Running && idle >= self.idle_sleep {
            self.state = LifecycleState::Sleeping;
            return LifecycleAction::SaveAndHibernate;
        }
        LifecycleAction::None
    }

    /// A login packet arrived (at the normal or the sleep listener). A
    /// non-whitelisted name is refused without waking anything; an
    /// authorized one pulls the world back up when sleeping.
    fn on_incoming_login_attempt(
        &mut self,
        _player_name: &str,
        whitelist_allowed: bool,
    ) -> LifecycleAction {
        if !whitelist_allowed {
            return LifecycleAction::RejectLogin;
        }
        self.empty_since = None;
        if self.state == LifecycleState::Sleeping {
            self.state = LifecycleState::Running;
            return LifecycleAction::WakeWorld;
        }
        LifecycleAction::None
    }
}

/// What the process supervisor does with an exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SupervisorDecision {
    /// Exit 0: intended stop, do not relaunch.
    Stop,
    /// `MEGU_RESTART_EXIT_CODE`: operator asked for restart, relaunch now.
    RestartImmediate,
    /// Crash: relaunch after the given backoff.
    RestartDelayed(Duration),
    /// Too many crashes in the window — give up instead of looping.
    HaltCrashLoop { crashes: usize, window_secs: u64 },
}

/// Crash auto-restart with exponential backoff (1s → 2s → 4s → 8s → 16s,
/// capped at 30s) and a hard crash-loop guard: five crashes inside sixty
/// seconds halt the supervisor instead of restart-looping forever. The
/// backoff ladder only relaxes on a clean stop or an explicit restart
/// request — a process that keeps dying keeps escalating to the cap.
struct SupervisorPolicy {
    crashes: VecDeque<Instant>,
    backoff_step: u32,
}

impl SupervisorPolicy {
    const CRASH_LOOP_COUNT: usize = 5;
    const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(60);
    const BACKOFF_BASE: Duration = Duration::from_secs(1);
    const BACKOFF_CAP: Duration = Duration::from_secs(30);

    fn new() -> Self {
        Self {
            crashes: VecDeque::new(),
            backoff_step: 0,
        }
    }

    /// Classify one child exit. `now` is the caller's monotonic clock.
    fn decide(&mut self, exit_code: i32, now: Instant) -> SupervisorDecision {
        if exit_code == 0 {
            self.reset();
            return SupervisorDecision::Stop;
        }
        if exit_code == MEGU_RESTART_EXIT_CODE {
            self.reset();
            return SupervisorDecision::RestartImmediate;
        }
        while let Some(&oldest) = self.crashes.front() {
            if now.saturating_duration_since(oldest) < Self::CRASH_LOOP_WINDOW {
                break;
            }
            self.crashes.pop_front();
        }
        self.crashes.push_back(now);
        if self.crashes.len() >= Self::CRASH_LOOP_COUNT {
            let crashes = self.crashes.len();
            // If an operator manually relaunches after a halt the window
            // starts clean rather than instantly re-tripping.
            self.crashes.clear();
            self.backoff_step = 0;
            return SupervisorDecision::HaltCrashLoop {
                crashes,
                window_secs: Self::CRASH_LOOP_WINDOW.as_secs(),
            };
        }
        let step = self.backoff_step.min(5);
        self.backoff_step = self.backoff_step.saturating_add(1);
        let delay = (Self::BACKOFF_BASE * (1u32 << step)).min(Self::BACKOFF_CAP);
        SupervisorDecision::RestartDelayed(delay)
    }

    fn reset(&mut self) {
        self.crashes.clear();
        self.backoff_step = 0;
    }
}

// ------------------------------------------------ remote control ----

/// Hard cap on one remote-control HTTP request, headers included.
const REMOTE_REQ_CAP: usize = 8 * 1024;

/// A side-effecting command the remote accepted; the caller runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteAction {
    Start,
    Restart,
    Stop,
}

/// Which token presented the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteScope {
    Admin,
    StartOnly,
}

/// One answered request: status line, body, and the action to relay.
struct RemoteReply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    action: Option<RemoteAction>,
}

impl RemoteReply {
    fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.as_bytes().to_vec(),
            action: None,
        }
    }

    fn json(status: u16, body: String, action: Option<RemoteAction>) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: body.into_bytes(),
            action,
        }
    }

    fn html(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.into_bytes(),
            action: None,
        }
    }
}

/// Opt-in minimal remote control. Binds loopback by default; the operator
/// chooses the address. Tokens are stored as SHA-256 digests only and
/// compared in constant time — the raw tokens never touch the struct.
///
/// `poll_once` is a single strictly-bounded step: it drains the pending
/// accept queue, answers each request, and returns the commands the
/// caller should act on. No threads are spawned anywhere.
struct RemoteControlServer {
    listener: TcpListener,
    admin_hash: [u8; 32],
    start_only_hash: Option<[u8; 32]>,
}

impl RemoteControlServer {
    fn bind(
        addr: &str,
        admin_token: &str,
        start_only_token: Option<&str>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            admin_hash: token_digest(admin_token),
            start_only_hash: start_only_token.map(token_digest),
        })
    }

    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// One non-blocking step: answer every connection already in the
    /// backlog and collect the authorized actions for the main loop.
    /// `status_json` renders the `/api/status` body on demand.
    fn poll_once(&mut self, status_json: &dyn Fn() -> String) -> Vec<RemoteAction> {
        let mut actions = Vec::new();
        loop {
            let (mut stream, _) = match self.listener.accept() {
                Ok(pair) => pair,
                Err(_) => break, // WouldBlock or a real error: step ends
            };
            // On macOS/BSD, accepted sockets inherit O_NONBLOCK from a
            // non-blocking listener; switch to blocking before the bounded
            // 250 ms read/write so requests do not fail with WouldBlock -> 400.
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
            let raw = read_http_request(&mut stream);
            let reply = self.handle_request(&raw, status_json);
            let head = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.status,
                http_reason(reply.status),
                reply.content_type,
                reply.body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&reply.body);
            let _ = stream.flush();
            let _ = stream.shutdown(std::net::Shutdown::Write);
            if let Some(action) = reply.action {
                actions.push(action);
            }
        }
        actions
    }

    /// Pure request → reply mapping, split out so the policy itself is
    /// testable without a socket.
    fn handle_request(
        &self,
        raw: &[u8],
        status_json: &dyn Fn() -> String,
    ) -> RemoteReply {
        let Some((method, target)) = parse_request_line(raw) else {
            return RemoteReply::text(400, "bad request");
        };
        let (path, query) = split_query(&target);
        let token = header_value(raw, "authorization")
            .and_then(|v| v.trim().strip_prefix("Bearer "))
            .map(str::trim)
            .or_else(|| query_param(query, "token"))
            .unwrap_or("");
        let Some(scope) = self.scope_for_token(token) else {
            return RemoteReply::text(401, "unauthorized");
        };
        let admin = scope == RemoteScope::Admin;
        let reply = match (method.as_str(), path.as_str()) {
            ("GET", "/") => {
                if !admin {
                    RemoteReply::text(403, "forbidden")
                } else {
                    RemoteReply::html(render_remote_page(token))
                }
            }
            ("GET", "/api/status") => RemoteReply::json(200, status_json(), None),
            ("POST", "/api/start") => RemoteReply::json(
                200,
                "{\"ok\":true,\"action\":\"start\"}".to_string(),
                Some(RemoteAction::Start),
            ),
            ("POST", "/api/restart") if admin => RemoteReply::json(
                200,
                "{\"ok\":true,\"action\":\"restart\"}".to_string(),
                Some(RemoteAction::Restart),
            ),
            ("POST", "/api/stop") if admin => RemoteReply::json(
                200,
                "{\"ok\":true,\"action\":\"stop\"}".to_string(),
                Some(RemoteAction::Stop),
            ),
            ("POST", "/api/restart") | ("POST", "/api/stop") => {
                RemoteReply::text(403, "forbidden")
            }
            _ => RemoteReply::text(404, "not found"),
        };
        reply
    }

    /// The presented token hashed and compared constant-time against both
    /// stored digests. Empty tokens never match.
    fn scope_for_token(&self, token: &str) -> Option<RemoteScope> {
        if token.is_empty() {
            return None;
        }
        let digest = token_digest(token);
        if ct_tag_eq(&digest, &self.admin_hash) {
            return Some(RemoteScope::Admin);
        }
        if let Some(start_only) = self.start_only_hash {
            if ct_tag_eq(&digest, &start_only) {
                return Some(RemoteScope::StartOnly);
            }
        }
        None
    }
}

/// SHA-256 of a token — compare digests, never raw strings.
fn token_digest(token: &str) -> [u8; 32] {
    crate::generation_math::sha256(token.as_bytes())
}

fn http_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    }
}

/// Read one request, bounded: stop once the header block is complete, at
/// the 8 KiB cap, or when the peer stalls past a short read timeout.
fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while buf.len() < REMOTE_REQ_CAP {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if headers_complete(&buf) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    buf.truncate(REMOTE_REQ_CAP);
    buf
}

fn headers_complete(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n")
        || buf.windows(2).any(|w| w == b"\n\n")
}

/// `METHOD SP request-target SP HTTP/x` from the first line.
fn parse_request_line(raw: &[u8]) -> Option<(String, String)> {
    let end = raw
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(raw.len());
    let line = std::str::from_utf8(&raw[..end]).ok()?.trim_end_matches('\r');
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    Some((method.to_string(), target.to_string()))
}

/// `(path, query)` split of a request-target; query stays raw.
fn split_query(target: &str) -> (String, &str) {
    match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query),
        None => (target.to_string(), ""),
    }
}

/// `?name=value` lookup; percent-decoding is unnecessary for the token
/// alphabet the CLI generates (hex/base64url).
fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == name {
                return Some(v);
            }
        }
    }
    None
}

/// Case-insensitive header lookup over the raw request.
fn header_value<'a>(raw: &'a [u8], name: &str) -> Option<&'a str> {
    let text = std::str::from_utf8(raw).ok()?;
    for line in text.lines().skip(1) {
        if line.trim().is_empty() {
            break; // end of the header block
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            return Some(value.trim());
        }
    }
    None
}

/// Minimal HTML-attribute escaping for the echoed token.
fn html_attr_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The whole UI: four large form buttons, no scripts, no frameworks. The
/// token travels in the form action so plain mobile browsers keep working
/// without an Authorization header.
fn render_remote_page(token: &str) -> String {
    let t = html_attr_escape(token);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Megu Remote</title><style>\
         body{{font-family:system-ui,sans-serif;background:#111;color:#eee;\
         max-width:22rem;margin:2rem auto;padding:0 1rem}}\
         h1{{font-size:1.4rem;text-align:center}}\
         form{{margin:.75rem 0}}button{{width:100%;font-size:1.25rem;\
         padding:1rem;border:0;border-radius:.75rem;background:#2d6;\
         color:#041;font-weight:700}}\
         form.stop button{{background:#e55;color:#fff}}\
         form.restart button{{background:#fa4;color:#210}}\
         form.status button{{background:#39c;color:#012}}\
         </style></head><body><h1>Megu Remote</h1>\
         <form class=\"status\" method=\"get\" action=\"/api/status?token={t}\">\
         <button type=\"submit\">Status</button></form>\
         <form method=\"post\" action=\"/api/start?token={t}\">\
         <button type=\"submit\">Start</button></form>\
         <form class=\"restart\" method=\"post\" action=\"/api/restart?token={t}\">\
         <button type=\"submit\">Restart</button></form>\
         <form class=\"stop\" method=\"post\" action=\"/api/stop?token={t}\">\
         <button type=\"submit\">Stop</button></form>\
         </body></html>"
    )
}

// ------------------------------------------------ survival mods ----

/// Commands a Survival Moderator may run. The role exists so trusted
/// players can police a survival server — kicks, bans, list/whitelist
/// upkeep, chat announcements and read-only audit/status — while every
/// world- or inventory-mutating command stays strictly out of reach.
const SURVIVAL_MOD_ALLOWED: &[&str] = &[
    "kick",
    "ban",
    "pardon",
    "ban-ip",
    "pardon-ip",
    "banlist",
    "whitelist",
    "say",
    "list",
];

/// `/megu` subcommands a Survival Moderator may run (read-only).
const SURVIVAL_MOD_ALLOWED_MEGU: &[&str] = &["audit", "status"];

/// Whether `cmd_name` is inside the Survival-Moderator allowlist. Anything
/// not listed is denied: `megu rollback`, `megu snapshot`, `megu claim`,
/// `gamemode`, `give`, `tp`, `teleport`, `fill`, `setblock`, `summon`,
/// `xp`, `effect`, `op`, `deop`, `stop`, `save-all` — no item dupes and
/// no world mutation from this role, ever. A leading `/` and extra
/// arguments are accepted; comparison is case-insensitive.
fn survival_mod_allows_command(cmd_name: &str) -> bool {
    let cmd = cmd_name.trim().trim_start_matches('/').trim();
    let mut parts = cmd.split_whitespace();
    let Some(head) = parts.next() else {
        return false;
    };
    if head.eq_ignore_ascii_case("megu") {
        return parts
            .next()
            .is_some_and(|sub| {
                SURVIVAL_MOD_ALLOWED_MEGU
                    .iter()
                    .any(|&allowed| sub.eq_ignore_ascii_case(allowed))
            });
    }
    SURVIVAL_MOD_ALLOWED
        .iter()
        .any(|&allowed| head.eq_ignore_ascii_case(allowed))
}

/// Persistent set of Survival Moderator player names — the zero-cheat
/// moderation role, deliberately separate from Vanilla OP levels 0–4.
/// Serializes as `{"players":[…]}` through the shared crate JSON codec.
struct SurvivalModRegistry {
    players: BTreeSet<String>,
}

impl SurvivalModRegistry {
    fn new() -> Self {
        Self {
            players: BTreeSet::new(),
        }
    }

    fn add(&mut self, name: &str) -> bool {
        self.players.insert(name.to_string())
    }

    fn remove(&mut self, name: &str) -> bool {
        self.players.remove(name)
    }

    fn contains(&self, name: &str) -> bool {
        self.players.contains(name)
    }

    fn len(&self) -> usize {
        self.players.len()
    }

    fn is_empty(&self) -> bool {
        self.players.is_empty()
    }

    fn names(&self) -> impl Iterator<Item = &str> {
        self.players.iter().map(String::as_str)
    }

    /// Deterministic JSON (names sorted by the `BTreeSet`).
    fn to_json(&self) -> String {
        let names = self
            .players
            .iter()
            .map(|n| format!("\"{}\"", status_json_text(n)))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"players\":[{names}]}}")
    }

    fn from_json(text: &str) -> Option<Self> {
        let doc = crate::json::jparse(text.as_bytes())?;
        let arr = doc.get("players")?.as_arr()?;
        let mut registry = Self::new();
        for entry in arr {
            let crate::json::J::Str(name) = entry else {
                return None;
            };
            registry.add(name);
        }
        Some(registry)
    }

    fn save_json(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, self.to_json())
    }

    /// Missing file means an empty registry; a malformed one is an error
    /// the operator sees instead of a silently dropped roster.
    fn load_json(path: &std::path::Path) -> std::io::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::new());
            }
            Err(e) => return Err(e),
        };
        Self::from_json(&text).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not a survival-mod registry", path.display()),
            )
        })
    }
}

// --------------------------------------------------- hub & worlds ----

/// One `[servers]` row: a friendly name pointing at host:port.
struct HubServerEntry {
    host: String,
    port: u16,
}

/// The hub `[servers]` directory. On `/stop` or `/megu restart` the
/// `fallback_server` names where online players are transferred before
/// the socket drops — typically the hub itself.
struct HubServerDirectory {
    servers: BTreeMap<String, HubServerEntry>,
    fallback_server: Option<String>,
}

impl HubServerDirectory {
    fn new() -> Self {
        Self {
            servers: BTreeMap::new(),
            fallback_server: None,
        }
    }

    /// Register or replace a server. Validation fails closed: bad names,
    /// bad hosts and port 0 are errors, never silently stored.
    fn add_server(&mut self, name: &str, host: &str, port: u16) -> Result<(), String> {
        if !valid_directory_name(name) {
            return Err(format!(
                "server name {name:?} must be 1-32 of [a-z0-9_-]"
            ));
        }
        if !valid_remote_host(host) {
            return Err(format!("host {host:?} is not a valid hostname or IP"));
        }
        if port == 0 {
            return Err("port 0 is not a connectable server address".to_string());
        }
        self.servers.insert(
            name.to_string(),
            HubServerEntry {
                host: host.to_string(),
                port,
            },
        );
        Ok(())
    }

    fn remove_server(&mut self, name: &str) -> bool {
        let removed = self.servers.remove(name).is_some();
        if self.fallback_server.as_deref() == Some(name) {
            self.fallback_server = None;
        }
        removed
    }

    /// `(host, port)` for a friendly name.
    fn resolve(&self, name: &str) -> Option<(&str, u16)> {
        self.servers.get(name).map(|e| (e.host.as_str(), e.port))
    }

    /// `(name, host, port)` players are transferred to on stop/restart.
    fn fallback(&self) -> Option<(&str, &str, u16)> {
        let name = self.fallback_server.as_deref()?;
        let (host, port) = self.resolve(name)?;
        Some((name, host, port))
    }

    /// The fallback must reference a registered server — a typo here would
    /// otherwise only surface while players are being kicked.
    fn set_fallback(&mut self, name: &str) -> Result<(), String> {
        if !self.servers.contains_key(name) {
            return Err(format!("no server named {name:?} is registered"));
        }
        self.fallback_server = Some(name.to_string());
        Ok(())
    }

    fn len(&self) -> usize {
        self.servers.len()
    }
}

/// `[a-z0-9_-]{1,32}` — the friendly-name shape for `[servers]` rows and
/// managed world ids alike.
fn valid_directory_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Fail-closed host validation: a literal `IpAddr`, or a DNS name of
/// `[a-zA-Z0-9-]` dot-separated labels (no leading/trailing hyphen).
fn valid_remote_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// Generator preset for a managed extra world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorldGenerator {
    Overworld,
    Flat,
    Void,
    Nether,
    End,
}

/// The gamemode players land in when entering a managed world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorldGameMode {
    Survival,
    Creative,
    Adventure,
    Spectator,
}

/// One `[[worlds]]` row. Worlds are plain dimensions on disk — the save
/// layout is the standard Vanilla/NeoForge
/// `dimensions/megu/<id>/region` tree, no custom format.
struct ManagedWorld {
    id: String,
    generator: WorldGenerator,
    seed: u64,
    pvp: bool,
    default_gamemode: WorldGameMode,
    spawn: [f64; 3],
}

impl ManagedWorld {
    /// The world directory relative to the save root.
    fn dimension_dir(&self) -> String {
        format!("dimensions/megu/{}", self.id)
    }

    /// The region directory holding this world's `.mca` files.
    fn region_dir(&self) -> String {
        format!("{}/region", self.dimension_dir())
    }
}

/// Declared extra worlds, keyed by id.
struct MultiWorldRegistry {
    worlds: BTreeMap<String, ManagedWorld>,
}

impl MultiWorldRegistry {
    fn new() -> Self {
        Self {
            worlds: BTreeMap::new(),
        }
    }

    /// Register a world. Duplicate or malformed ids are rejected.
    fn add(&mut self, world: ManagedWorld) -> Result<(), String> {
        if !valid_directory_name(&world.id) {
            return Err(format!(
                "world id {:?} must be 1-32 of [a-z0-9_-]",
                world.id
            ));
        }
        if self.worlds.contains_key(&world.id) {
            return Err(format!("world {:?} is already registered", world.id));
        }
        self.worlds.insert(world.id.clone(), world);
        Ok(())
    }

    fn remove(&mut self, id: &str) -> bool {
        self.worlds.remove(id).is_some()
    }

    fn get(&self, id: &str) -> Option<&ManagedWorld> {
        self.worlds.get(id)
    }

    fn len(&self) -> usize {
        self.worlds.len()
    }

    fn iter(&self) -> impl Iterator<Item = &ManagedWorld> {
        self.worlds.values()
    }
}

// ------------------------------------------------ status summary ----

/// Everything the one-screen `/megu status` and the startup summary need,
/// gathered by the caller.
struct OperatorStatusSummary {
    tps: f64,
    mspt_p50: f64,
    mspt_p95: f64,
    players_online: usize,
    players_max: usize,
    loaded_chunks: usize,
    rss_mb: Option<u64>,
    active_worlds: usize,
    hub_servers: usize,
    survival_mods: usize,
}

/// The one-screen operator summary: TPS, MSPT p50/p95, players, chunks,
/// RSS, worlds, hub servers and the Survival-Mod count. Plain units, no
/// internal vocabulary.
fn format_operator_status_summary(s: &OperatorStatusSummary) -> String {
    let rss = match s.rss_mb {
        Some(mb) => format!("{mb} MB"),
        None => "N/A".to_string(),
    };
    format!(
        "TPS: {:.1}   MSPT p50/p95: {:.1}/{:.1} ms\n\
         Players: {}/{}   Chunks: {}   RSS: {}\n\
         Worlds: {} active   Hub servers: {}   Survival mods: {}",
        s.tps,
        s.mspt_p50,
        s.mspt_p95,
        s.players_online,
        s.players_max,
        s.loaded_chunks,
        rss,
        s.active_worlds,
        s.hub_servers,
        s.survival_mods,
    )
}

// ------------------------------------------------------------- tests ----

#[cfg(test)]
mod operator_ux_admin_core_tests {
    use super::*;

    #[test]
    fn lifecycle_sleeps_after_idle_and_wakes_on_whitelisted_join() {
        let t0 = Instant::now();
        let mut ctl = ServerLifecycleController::new(60, None);
        assert_eq!(ctl.update(t0, 3), LifecycleAction::None);
        // The world empties at +10; hibernate fires 60s after that.
        assert_eq!(ctl.update(t0 + Duration::from_secs(10), 0), LifecycleAction::None);
        assert_eq!(ctl.update(t0 + Duration::from_secs(69), 0), LifecycleAction::None);
        assert_eq!(ctl.state(), LifecycleState::Running);
        assert_eq!(
            ctl.update(t0 + Duration::from_secs(70), 0),
            LifecycleAction::SaveAndHibernate
        );
        assert_eq!(ctl.state(), LifecycleState::Sleeping);
        assert_eq!(
            ServerLifecycleController::sleeping_motd(),
            "Sleeping — Join to wake!"
        );
        // A non-whitelisted attempt must not wake the world.
        assert_eq!(
            ctl.on_incoming_login_attempt("mallory", false),
            LifecycleAction::RejectLogin
        );
        assert_eq!(ctl.state(), LifecycleState::Sleeping);
        // A whitelisted join wakes it immediately.
        assert_eq!(
            ctl.on_incoming_login_attempt("alice", true),
            LifecycleAction::WakeWorld
        );
        assert_eq!(ctl.state(), LifecycleState::Running);
        // The idle clock restarted at the wake: a fresh empty run needs
        // a full 60s again before the next hibernate.
        assert_eq!(ctl.update(t0 + Duration::from_secs(80), 0), LifecycleAction::None);
        assert_eq!(ctl.update(t0 + Duration::from_secs(139), 0), LifecycleAction::None);
        assert_eq!(
            ctl.update(t0 + Duration::from_secs(140), 0),
            LifecycleAction::SaveAndHibernate
        );
        // Players observed online while sleeping also wakes (defensive).
        assert_eq!(ctl.update(t0 + Duration::from_secs(141), 1), LifecycleAction::WakeWorld);
        assert_eq!(ctl.state(), LifecycleState::Running);
    }

    #[test]
    fn lifecycle_shutdown_fires_after_longer_empty_window() {
        let t0 = Instant::now();
        let mut ctl = ServerLifecycleController::new(60, Some(300));
        assert_eq!(ctl.update(t0, 0), LifecycleAction::None); // empty clock starts
        assert_eq!(
            ctl.update(t0 + Duration::from_secs(60), 0),
            LifecycleAction::SaveAndHibernate
        );
        assert_eq!(
            ctl.update(t0 + Duration::from_secs(299), 0),
            LifecycleAction::None
        );
        assert_eq!(
            ctl.update(t0 + Duration::from_secs(300), 0),
            LifecycleAction::SaveAndShutdown
        );
        assert_eq!(ctl.state(), LifecycleState::Stopped);
        // Terminal: neither more idle time nor login attempts change it.
        assert_eq!(ctl.update(t0 + Duration::from_secs(600), 0), LifecycleAction::None);
        assert_eq!(
            ctl.on_incoming_login_attempt("alice", true),
            LifecycleAction::None
        );
    }

    #[test]
    fn lifecycle_no_shutdown_when_not_configured() {
        let t0 = Instant::now();
        let mut ctl = ServerLifecycleController::new(60, None);
        ctl.update(t0, 0); // empty clock starts
        assert_eq!(
            ctl.update(t0 + Duration::from_secs(60), 0),
            LifecycleAction::SaveAndHibernate
        );
        for extra in [120, 600, 86_400] {
            assert_eq!(
                ctl.update(t0 + Duration::from_secs(extra), 0),
                LifecycleAction::None
            );
            assert_eq!(ctl.state(), LifecycleState::Sleeping);
        }
    }

    #[test]
    fn supervisor_clean_exit_stops_and_code_42_restarts_immediately() {
        let t0 = Instant::now();
        let mut sup = SupervisorPolicy::new();
        // Even after crashes, a clean exit means a real operator stop.
        assert_eq!(
            sup.decide(1, t0),
            SupervisorDecision::RestartDelayed(Duration::from_secs(1))
        );
        assert_eq!(sup.decide(0, t0 + Duration::from_secs(1)), SupervisorDecision::Stop);
        assert_eq!(
            sup.decide(MEGU_RESTART_EXIT_CODE, t0 + Duration::from_secs(2)),
            SupervisorDecision::RestartImmediate
        );
    }

    #[test]
    fn supervisor_backoff_doubles_then_crash_loop_halts() {
        let t0 = Instant::now();
        let mut sup = SupervisorPolicy::new();
        for (i, expect_secs) in [1u64, 2, 4, 8].iter().enumerate() {
            let at = t0 + Duration::from_secs(10 * i as u64);
            assert_eq!(
                sup.decide(1, at),
                SupervisorDecision::RestartDelayed(Duration::from_secs(*expect_secs)),
                "crash {} should delay {expect_secs}s",
                i + 1
            );
        }
        // Fifth crash inside 60s: the loop guard fires, no more restarts.
        assert_eq!(
            sup.decide(1, t0 + Duration::from_secs(40)),
            SupervisorDecision::HaltCrashLoop {
                crashes: 5,
                window_secs: 60
            }
        );
    }

    #[test]
    fn supervisor_backoff_escalates_across_windows_to_cap() {
        let t0 = Instant::now();
        let mut sup = SupervisorPolicy::new();
        // Spaced crashes never reach 5-in-60, so restarts keep happening
        // while the ladder escalates 1→2→4→8→16 and caps at 30s.
        for (at, secs) in [
            (0u64, 1u64),
            (70, 2),
            (71, 4),
            (72, 8),
            (73, 16),
            (131, 30),
            (192, 30),
        ] {
            assert_eq!(
                sup.decide(1, t0 + Duration::from_secs(at)),
                SupervisorDecision::RestartDelayed(Duration::from_secs(secs)),
                "crash at +{at}s should delay {secs}s"
            );
        }
    }

    // --- remote control over a real loopback listener ---

    fn remote_request(
        srv: &mut RemoteControlServer,
        request: &str,
    ) -> (String, Vec<RemoteAction>) {
        let addr = srv.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let _ = client.shutdown(std::net::Shutdown::Write);
        let actions = srv.poll_once(&|| "{\"tps\":20.0}".to_string());
        let mut reply = String::new();
        match client.read_to_string(&mut reply) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset && !reply.is_empty() => {}
            Err(e) => panic!("remote_request read failed: {e}"),
        }
        (reply, actions)
    }

    fn http_status(reply: &str) -> u16 {
        reply
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    #[test]
    fn remote_control_token_scopes_and_actions() {
        let mut srv = RemoteControlServer::bind(
            "127.0.0.1:0",
            "admin-secret",
            Some("starter-secret"),
        )
        .unwrap();

        // No token at all → 401.
        let (reply, actions) =
            remote_request(&mut srv, "GET /api/status HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(http_status(&reply), 401, "{reply}");
        assert!(actions.is_empty());

        // Wrong token → 401.
        let (reply, _) = remote_request(
            &mut srv,
            "GET /api/status HTTP/1.1\r\nAuthorization: Bearer nope\r\n\r\n",
        );
        assert_eq!(http_status(&reply), 401, "{reply}");

        // start_only token: status and start work…
        let (reply, _) = remote_request(
            &mut srv,
            "GET /api/status HTTP/1.1\r\nAuthorization: Bearer starter-secret\r\n\r\n",
        );
        assert_eq!(http_status(&reply), 200, "{reply}");
        assert!(reply.contains("{\"tps\":20.0}"), "{reply}");

        let (reply, actions) = remote_request(
            &mut srv,
            "POST /api/start HTTP/1.1\r\nAuthorization: Bearer starter-secret\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(http_status(&reply), 200, "{reply}");
        assert_eq!(actions, vec![RemoteAction::Start]);

        // …but stop and restart are strictly forbidden for it.
        for path in ["/api/stop", "/api/restart"] {
            let (reply, actions) = remote_request(
                &mut srv,
                &format!(
                    "POST {path} HTTP/1.1\r\nAuthorization: Bearer starter-secret\r\nContent-Length: 0\r\n\r\n"
                ),
            );
            assert_eq!(http_status(&reply), 403, "{path}: {reply}");
            assert!(actions.is_empty(), "{path}");
        }
        // The page is admin-only too.
        let (reply, _) = remote_request(
            &mut srv,
            "GET / HTTP/1.1\r\nAuthorization: Bearer starter-secret\r\n\r\n",
        );
        assert_eq!(http_status(&reply), 403, "{reply}");

        // Admin token: every action lands.
        for (path, action) in [
            ("/api/start", RemoteAction::Start),
            ("/api/restart", RemoteAction::Restart),
            ("/api/stop", RemoteAction::Stop),
        ] {
            let (reply, actions) = remote_request(
                &mut srv,
                &format!(
                    "POST {path} HTTP/1.1\r\nAuthorization: Bearer admin-secret\r\nContent-Length: 0\r\n\r\n"
                ),
            );
            assert_eq!(http_status(&reply), 200, "{path}: {reply}");
            assert_eq!(actions, vec![action], "{path}");
        }

        // Query-token path (what the mobile page's forms use).
        let (reply, actions) = remote_request(
            &mut srv,
            "POST /api/stop?token=admin-secret HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(http_status(&reply), 200, "{reply}");
        assert_eq!(actions, vec![RemoteAction::Stop]);

        // Unknown route → 404 even with a good token.
        let (reply, _) = remote_request(
            &mut srv,
            "GET /api/nuke?token=admin-secret HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(http_status(&reply), 404, "{reply}");
    }

    #[test]
    fn remote_control_page_renders_four_buttons() {
        let mut srv =
            RemoteControlServer::bind("127.0.0.1:0", "admin-secret", None).unwrap();
        let (reply, _) = remote_request(
            &mut srv,
            "GET /?token=admin-secret HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(http_status(&reply), 200, "{reply}");
        assert!(reply.contains("Content-Type: text/html"), "{reply}");
        for button in [">Status<", ">Start<", ">Restart<", ">Stop<"] {
            assert!(reply.contains(button), "missing {button}:\n{reply}");
        }
        assert!(reply.contains("viewport"), "{reply}");
        assert!(!reply.contains("<script"), "{reply}");
    }

    #[test]
    fn remote_control_without_start_token_denies_start_scope() {
        let srv = RemoteControlServer::bind("127.0.0.1:0", "admin-secret", None).unwrap();
        assert_eq!(srv.scope_for_token("admin-secret"), Some(RemoteScope::Admin));
        assert_eq!(srv.scope_for_token("starter-secret"), None);
        assert_eq!(srv.scope_for_token(""), None);
    }

    // --- survival moderators ---

    #[test]
    fn survival_mod_allowlist_polices_without_cheating() {
        for allowed in [
            "/kick",
            "kick",
            "kick griefer reason",
            "/ban",
            "/pardon",
            "/ban-ip",
            "/pardon-ip",
            "/banlist",
            "/whitelist",
            "whitelist add alice",
            "/say",
            "/list",
            "/megu audit",
            "/megu status",
            "MEGU STATUS",
        ] {
            assert!(
                survival_mod_allows_command(allowed),
                "{allowed} should be allowed"
            );
        }
        for denied in [
            "/megu rollback",
            "/megu snapshot",
            "/megu claim",
            "/megu",
            "/gamemode",
            "/give",
            "/tp",
            "/teleport",
            "/fill",
            "/setblock",
            "/summon",
            "/xp",
            "/effect",
            "/op",
            "/deop",
            "/stop",
            "/save-all",
            "",
        ] {
            assert!(
                !survival_mod_allows_command(denied),
                "{denied} must be denied"
            );
        }
    }

    #[test]
    fn survival_mod_registry_json_roundtrip() {
        let mut reg = SurvivalModRegistry::new();
        assert!(reg.is_empty());
        assert!(reg.add("alice"));
        assert!(reg.add("bob"));
        assert!(!reg.add("alice"));
        assert!(reg.contains("alice"));
        assert_eq!(reg.len(), 2);

        let json = reg.to_json();
        assert_eq!(json, "{\"players\":[\"alice\",\"bob\"]}");
        let back = SurvivalModRegistry::from_json(&json).unwrap();
        assert_eq!(back.names().collect::<Vec<_>>(), vec!["alice", "bob"]);

        // Malformed documents fail instead of silently emptying.
        assert!(SurvivalModRegistry::from_json("{}").is_none());
        assert!(SurvivalModRegistry::from_json("{\"players\":[1]}").is_none());
        assert!(SurvivalModRegistry::from_json("not json").is_none());

        // Save/load through a real file.
        let dir = std::env::temp_dir().join(format!(
            "megu-survmod-{}-{}",
            std::process::id(),
            "roundtrip"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("survival-mods.json");
        reg.save_json(&path).unwrap();
        let loaded = SurvivalModRegistry::load_json(&path).unwrap();
        assert_eq!(loaded.names().collect::<Vec<_>>(), vec!["alice", "bob"]);
        // Missing file → empty registry, not an error.
        let missing = SurvivalModRegistry::load_json(&dir.join("gone.json")).unwrap();
        assert!(missing.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- hub directory & multi-world registry ---

    fn hub_world(id: &str) -> ManagedWorld {
        ManagedWorld {
            id: id.to_string(),
            generator: WorldGenerator::Flat,
            seed: 42,
            pvp: false,
            default_gamemode: WorldGameMode::Creative,
            spawn: [0.5, 64.0, 0.5],
        }
    }

    #[test]
    fn hub_directory_validates_and_resolves_fallback() {
        let mut dir = HubServerDirectory::new();
        dir.add_server("hub", "127.0.0.1", 25565).unwrap();
        dir.add_server("survival", "survival.example.net", 25566)
            .unwrap();
        assert_eq!(dir.len(), 2);
        assert_eq!(dir.resolve("hub"), Some(("127.0.0.1", 25565)));

        for (name, host, port) in [
            ("", "h", 1),                 // empty name
            ("Bad Name", "h", 1),         // illegal characters
            ("ok", "", 1),                // empty host
            ("ok", "-bad.example", 1),    // leading hyphen label
            ("ok", "bad host!", 1),       // illegal characters
            ("ok", "fine.example", 0),    // port zero
        ] {
            assert!(
                dir.add_server(name, host, port).is_err(),
                "{name}/{host}/{port} must be rejected"
            );
        }

        // Fallback must point at a real server.
        assert!(dir.set_fallback("missing").is_err());
        dir.set_fallback("hub").unwrap();
        assert_eq!(dir.fallback(), Some(("hub", "127.0.0.1", 25565)));
        // Removing the fallback target clears the pointer.
        assert!(dir.remove_server("hub"));
        assert_eq!(dir.fallback(), None);
    }

    #[test]
    fn multiworld_registry_maps_standard_dimension_paths() {
        let mut reg = MultiWorldRegistry::new();
        reg.add(hub_world("farm")).unwrap();
        assert!(reg.add(hub_world("farm")).is_err());
        assert!(reg.add(hub_world("Bad Id")).is_err());
        assert_eq!(reg.len(), 1);
        let world = reg.get("farm").unwrap();
        assert_eq!(world.dimension_dir(), "dimensions/megu/farm");
        assert_eq!(world.region_dir(), "dimensions/megu/farm/region");
        assert_eq!(world.generator, WorldGenerator::Flat);
        assert_eq!(world.default_gamemode, WorldGameMode::Creative);
        assert_eq!(world.spawn, [0.5, 64.0, 0.5]);
        assert!(reg.remove("farm"));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn operator_status_summary_is_one_screen() {
        let text = format_operator_status_summary(&OperatorStatusSummary {
            tps: 19.94,
            mspt_p50: 41.2,
            mspt_p95: 48.9,
            players_online: 3,
            players_max: 20,
            loaded_chunks: 1_842,
            rss_mb: Some(512),
            active_worlds: 4,
            hub_servers: 2,
            survival_mods: 5,
        });
        assert!(text.contains("TPS: 19.9"), "{text}");
        assert!(text.contains("p50/p95: 41.2/48.9"), "{text}");
        assert!(text.contains("Players: 3/20"), "{text}");
        assert!(text.contains("Chunks: 1842"), "{text}");
        assert!(text.contains("RSS: 512 MB"), "{text}");
        assert!(text.contains("Worlds: 4 active"), "{text}");
        assert!(text.contains("Hub servers: 2"), "{text}");
        assert!(text.contains("Survival mods: 5"), "{text}");
        let lines = text.lines().count();
        assert!((2..=4).contains(&lines), "one screen, not a wall:\n{text}");
    }
}
}
