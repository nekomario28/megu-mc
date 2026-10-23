// mc-spine v0.6 — joinable 1.21.1 (protocol 767) flat server:
// handshake -> status | login -> config (vanilla blob replay) -> play
// (join_game + 9x9 chunks + teleport + health) -> consume loop with
// creative block dig/place backed by a shared overlay world.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

// ---------- varint / wire ----------
fn read_varint(b: &mut &[u8]) -> Option<i64> {
    let mut v = 0i64; let mut i = 0;
    loop {
        if b.is_empty() || i > 9 { return None }
        let byte = b[0]; *b = &b[1..];
        v |= ((byte & 0x7f) as i64) << (7 * i);
        if byte & 0x80 == 0 { return Some(v) }
        i += 1;
    }
}
fn write_varint(out: &mut Vec<u8>, v: i64) {
    // i32-range negatives encode as VarInt (5-byte u32 LEB128); wider as VarLong.
    // (arithmetic `>>=` on a negative i64 loops forever -> unbounded Vec growth)
    let mut v = if v < 0 && v >= i32::MIN as i64 { v as u32 as u64 } else { v as u64 };
    loop {
        let mut b = (v & 0x7f) as u8; v >>= 7;
        if v != 0 { b |= 0x80 } out.push(b);
        if v == 0 { break }
    }
}
fn write_str(out: &mut Vec<u8>, s: &str) {
    write_varint(out, s.len() as i64); out.extend_from_slice(s.as_bytes());
}
fn write_pos(out: &mut Vec<u8>, x: i32, y: i32, z: i32) {
    let v = ((x as i64 & 0x3ffffff) << 38) | ((z as i64 & 0x3ffffff) << 12) | (y as i64 & 0xfff);
    out.extend_from_slice(&v.to_be_bytes());
}
fn read_pos(b: &[u8]) -> (i32, i32, i32) {
    let v = i64::from_be_bytes(b[..8].try_into().unwrap());
    let x = (v >> 38) as i32;
    let z = ((v >> 12) & 0x3ffffff) as i32;
    let y = (v & 0xfff) as i32;
    ((x << 6) >> 6, (y << 20) >> 20, (z << 6) >> 6) // sign-extend, (x,y,z) order
}

fn read_packet(s: &mut TcpStream) -> Option<(i64, Vec<u8>)> {
    // single-shot read (login phase only; consume loop uses RxBuf)
    let mut len = 0i64; let mut i = 0;
    loop {
        let mut b = [0u8; 1];
        if s.read(&mut b).ok()? != 1 { return None }
        len |= ((b[0] & 0x7f) as i64) << (7 * i); i += 1;
        if b[0] & 0x80 == 0 { break }
        if i > 5 { return None }
    }
    if len < 0 || len > (16 << 20) {
        let _ = s.shutdown(std::net::Shutdown::Both);
        return None
    }
    let mut body = vec![0u8; len as usize];
    s.read_exact(&mut body).ok()?;
    let mut sl = &body[..];
    let id = read_varint(&mut sl)?;
    Some((id, sl.to_vec()))
}
// Buffered frame reader for the consume loop: a mid-frame timeout must NOT
// drop already-read bytes. Dropped length bytes desync the stream -> garbage
// length fields -> repeated <=16MiB allocs -> allocator arena churn -> the OOM.
struct RxBuf { buf: std::collections::VecDeque<u8> }
enum RxOut { Closed, Idle, Frame(i64, Vec<u8>) }
impl RxBuf {
    fn new() -> Self { RxBuf { buf: std::collections::VecDeque::new() } }
    fn try_frame(&mut self) -> Option<Option<RxOut>> {
        let n = self.buf.len();
        let mut len = 0i64; let mut i = 0usize;
        loop {
            if i >= n { return Some(None) }
            let b = self.buf[i];
            len |= ((b & 0x7f) as i64) << (7 * i); i += 1;
            if b & 0x80 == 0 { break }
            if i > 5 { return None }
        }
        if len < 0 || len > (16 << 20) { return None }
        let total = i + len as usize;
        if n < total { return Some(None) }
        let body: Vec<u8> = self.buf.iter().skip(i).take(len as usize).copied().collect();
        self.buf.drain(..total);
        let mut sl = &body[..];
        match read_varint(&mut sl) {
            Some(id) => Some(Some(RxOut::Frame(id, sl.to_vec()))),
            None => Some(Some(RxOut::Idle)),
        }
    }
    fn next(&mut self, s: &mut TcpStream) -> RxOut {
        match self.try_frame() {
            None => { let _ = s.shutdown(std::net::Shutdown::Both); return RxOut::Closed }
            Some(Some(f)) => return f,
            Some(None) => {}
        }
        let mut tmp = [0u8; 8192];
        match s.read(&mut tmp) {
            Ok(0) => RxOut::Closed,
            Ok(n) => { self.buf.extend(&tmp[..n]); RxOut::Idle }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => RxOut::Idle,
            Err(_) => RxOut::Closed,
        }
    }
}
fn send_packet(s: &mut TcpStream, id: i64, payload: Vec<u8>) -> std::io::Result<()> {
    let mut body = Vec::new();
    write_varint(&mut body, id); body.extend_from_slice(&payload);
    let mut out = Vec::new();
    write_varint(&mut out, body.len() as i64); out.extend_from_slice(&body);
    s.write_all(&out)
}
fn send_raw(s: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    let mut out = Vec::new();
    write_varint(&mut out, body.len() as i64); out.extend_from_slice(body);
    s.write_all(&out)
}

// ---------- clients (multiplayer visibility) ----------
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

struct Client {
    eid: i32,
    uuid: [u8; 16],
    name: String,
    pos: Mutex<(f64, f64, f64, f32, f32, bool)>,
    hp: Mutex<f32>,
    sock: Mutex<TcpStream>,
    inv: Mutex<HashMap<i16, (i64, u8)>>,
    cursor: Mutex<(i64, u8)>,
    open_chest: Mutex<Option<(i32, i32, i32)>>,
    open_furnace: Mutex<Option<(i32, i32, i32)>>,
    craft: Mutex<HashMap<i16, (i64, u8)>>,
    craft9: Mutex<HashMap<i16, (i64, u8)>>,
    open_table: Mutex<bool>,
    open_merchant: Mutex<bool>,
    trade_sel: Mutex<i32>,
    merchant_in: Mutex<[(i64, u8); 2]>, bow_draw: Mutex<bool>, blocking: Mutex<bool>, sprinting: Mutex<bool>,
    xp: Mutex<i64>,
    gm: Mutex<u8>,          // 0=survival 1=creative 3=spectator
    fall_from: Mutex<f64>,
    food: Mutex<i32>,
    air: Mutex<i32>,
    armor_pts: Mutex<i32>,
    sat: Mutex<f32>,
    walk_acc: Mutex<f64>,
    lastp: Mutex<(f64, f64, f64)>,
    spawn_pt: Mutex<Option<(f64, f64, f64)>>,
    held: Mutex<i16>,
}
static CLIENTS: OnceLock<Mutex<Vec<Arc<Client>>>> = OnceLock::new();
static NEXT_EID: AtomicI32 = AtomicI32::new(1000);
fn clients() -> &'static Mutex<Vec<Arc<Client>>> {
    CLIENTS.get_or_init(|| Mutex::new(Vec::new()))
}
fn angle(deg: f32) -> i8 { (deg / 360.0 * 256.0) as i8 }

fn player_info_add(c: &Client) -> Vec<u8> {
    // actions: add_player|update_game_mode|update_listed|update_latency = 0x1d
    let mut p = vec![0x1du8];
    write_varint(&mut p, 1);
    p.extend_from_slice(&c.uuid);
    write_str(&mut p, &c.name);
    write_varint(&mut p, 0); // properties
    write_varint(&mut p, 1); // gamemode creative
    write_varint(&mut p, 1); // listed
    write_varint(&mut p, 0); // latency
    p
}
fn spawn_player(c: &Client) -> Vec<u8> {
    let (x, y, z, yaw, pitch, _g) = *c.pos.lock().unwrap();
    let mut p = Vec::new();
    write_varint(&mut p, c.eid as i64);
    p.extend_from_slice(&c.uuid);
    write_varint(&mut p, 128); // entity_type minecraft:player (1.21.1)
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.push(angle(pitch) as u8); p.push(angle(yaw) as u8); p.push(angle(yaw) as u8);
    write_varint(&mut p, 0);
    p.extend_from_slice(&[0u8; 6]); // velocity
    p
}
fn arrow_meta(eid: i32) -> Vec<u8> { let mut p = Vec::new(); write_varint(&mut p, eid as i64); p.push(0xff); p }
fn entity_teleport(eid: i32, x: f64, y: f64, z: f64, yaw: f32, pitch: f32, g: bool) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.push(angle(yaw) as u8); p.push(angle(pitch) as u8);
    p.push(g as u8);
    p
}
fn head_rot(eid: i32, yaw: f32) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64); p.push(angle(yaw) as u8); p
}
fn broadcast<F: Fn(&Client) -> (i64, Vec<u8>)>(me_eid: i32, mk: F) {
    // snapshot so callers may hold their own locks without risking a
    // lock-order cycle against clients()
    let cls: Vec<Arc<Client>> = { clients().lock().unwrap().iter().cloned().collect() };
    for c in cls.iter() {
        if c.eid == me_eid { continue }
        let pl = mk(c);
        if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut *w, pl.0, pl.1); }
    }
}
// broadcast helpers return (packet_id, payload)

// ---------- arrows ----------
struct Arrow { eid: i32, owner: i32, x: f64, y: f64, z: f64, dx: f64, dy: f64, dz: f64, ttl: i32 }
static ARROWS: OnceLock<Mutex<Vec<Arrow>>> = OnceLock::new();
fn arrows() -> &'static Mutex<Vec<Arrow>> { ARROWS.get_or_init(|| Mutex::new(Vec::new())) }
struct Primed { eid: i32, x: f64, y: f64, z: f64, lit: Instant }
static PRIMED: OnceLock<Mutex<Vec<Primed>>> = OnceLock::new();
fn primed() -> &'static Mutex<Vec<Primed>> { PRIMED.get_or_init(|| Mutex::new(Vec::new())) }
fn spawn_arrow_pkt(a: &Arrow) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, a.eid as i64);
    p.extend_from_slice(&[0u8; 16]);
    write_varint(&mut p, 4); // arrow
    p.extend_from_slice(&a.x.to_be_bytes());
    p.extend_from_slice(&a.y.to_be_bytes());
    p.extend_from_slice(&a.z.to_be_bytes());
    p.extend_from_slice(&[0u8; 3]);
    write_varint(&mut p, 0);
    p.extend_from_slice(&[0u8; 6]);
    p
}

struct Bobber { eid: i32, owner: i32, x: f64, y: f64, z: f64, dx: f64, dy: f64, dz: f64, age: i32, bit: bool }
static BOBBERS: OnceLock<Mutex<Vec<Bobber>>> = OnceLock::new();
fn bobbers() -> &'static Mutex<Vec<Bobber>> { BOBBERS.get_or_init(|| Mutex::new(Vec::new())) }
struct Pearl { eid: i32, owner: i32, x: f64, y: f64, z: f64, dx: f64, dy: f64, dz: f64, ttl: i32 }
static PEARLS: OnceLock<Mutex<Vec<Pearl>>> = OnceLock::new();
fn pearls() -> &'static Mutex<Vec<Pearl>> { PEARLS.get_or_init(|| Mutex::new(Vec::new())) }
struct Snowball { eid: i32, owner: i32, x: f64, y: f64, z: f64, dx: f64, dy: f64, dz: f64, ttl: i32 }
static SNOWBALLS: OnceLock<Mutex<Vec<Snowball>>> = OnceLock::new();
fn snowballs() -> &'static Mutex<Vec<Snowball>> { SNOWBALLS.get_or_init(|| Mutex::new(Vec::new())) }
fn spawn_typed_pkt(eid: i32, etype: i64, x: f64, y: f64, z: f64) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64); p.extend_from_slice(&[0u8; 16]);
    write_varint(&mut p, etype);
    p.extend_from_slice(&x.to_be_bytes()); p.extend_from_slice(&y.to_be_bytes()); p.extend_from_slice(&z.to_be_bytes());
    p.extend_from_slice(&[0u8; 3]); write_varint(&mut p, 0); p.extend_from_slice(&[0u8; 6]); p
}

// ---------- xp orbs ----------
struct Xp { eid: i32, x: f64, y: f64, z: f64, val: i16, age: Instant }
static XPS: OnceLock<Mutex<Vec<Xp>>> = OnceLock::new();
fn xps() -> &'static Mutex<Vec<Xp>> { XPS.get_or_init(|| Mutex::new(Vec::new())) }

// ---------- item drops ----------
struct Drop { eid: i32, x: f64, y: f64, z: f64, item: i64, age: Instant }
static DROPS: OnceLock<Mutex<Vec<Drop>>> = OnceLock::new();
fn drops() -> &'static Mutex<Vec<Drop>> { DROPS.get_or_init(|| Mutex::new(Vec::new())) }

// blockstate -> dropped item id (vanilla: stone->cobblestone, grass->dirt)
fn state_to_item(st: u32) -> Option<i64> {
    Some(match st { 1 => 35, 9 => 28, 10 => 28, 14 => 35, 15 => 36, 112 => 57,
        79 => 56, 131 => 132, 519 => 188, 2092 => 88, 4276 => 91, 2354 => 290, 2355 => 291,
        _ => return None })
}
fn spawn_drop_pkt(d: &Drop) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, d.eid as i64);
    p.extend_from_slice(&[0u8; 16]);
    write_varint(&mut p, 58); // item entity
    p.extend_from_slice(&d.x.to_be_bytes());
    p.extend_from_slice(&d.y.to_be_bytes());
    p.extend_from_slice(&d.z.to_be_bytes());
    p.extend_from_slice(&[0u8; 3]);
    write_varint(&mut p, 0);
    p.extend_from_slice(&[0u8; 6]);
    p
}
fn drop_meta_pkt(d: &Drop) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, d.eid as i64);
    p.push(8);                    // metadata key 8 = item
    write_varint(&mut p, 7);      // type 7 = item_stack
    write_varint(&mut p, 1);          // slot: count
    write_varint(&mut p, d.item);     // item id
    write_varint(&mut p, 0); write_varint(&mut p, 0); // components
    p.push(0xff);
    p
}
fn collect_pkt(drop_eid: i32, collector: i32) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, drop_eid as i64);
    write_varint(&mut p, collector as i64);
    write_varint(&mut p, 1);
    p
}
fn xp_level(total: i64) -> (i32, f32) {
    let mut l = 0i32; let mut acc = 0i64; let mut next = 7i64;
    while total >= acc + next {
        acc += next; l += 1;
        next = if l < 16 { 2 * l as i64 + 7 } else if l < 31 { 5 * l as i64 - 38 } else { 9 * l as i64 - 158 };
    }
    (l, ((total - acc) as f32 / next as f32).clamp(0.0, 1.0))
}
fn xp_orb_pkt(eid: i32, x: f64, y: f64, z: f64, count: i16) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.extend_from_slice(&count.to_be_bytes());
    p
}
fn velocity_pkt(eid: i32, vx: i16, vy: i16, vz: i16) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.extend_from_slice(&vx.to_be_bytes());
    p.extend_from_slice(&vy.to_be_bytes());
    p.extend_from_slice(&vz.to_be_bytes());
    p
}
fn explosion_pkt(x: f64, y: f64, z: f64, records: &[(i8,i8,i8)]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.extend_from_slice(&3.0f32.to_be_bytes());
    write_varint(&mut p, records.len() as i64);
    for &(ox, oy, oz) in records { p.push(ox as u8); p.push(oy as u8); p.push(oz as u8); }
    p.extend_from_slice(&[0u8; 12]); // player motion xyz f32
    write_varint(&mut p, 0);          // block_interaction = destroy
    write_varint(&mut p, 53);         // small particle: poof
    write_varint(&mut p, 22);         // large particle: explosion
    write_varint(&mut p, 577);        // sound: entity.generic.explode
    p
}
fn mob_loot(etype: i64) -> Vec<i64> {
    let names: &[&str] = match etype {
        124 => &["rotten_flesh"],
        22 => &["beef", "leather"],
        23 => &["gunpowder"],
        91 => &["bone", "arrow"],
        19 => &["chicken", "feather"],
        77 => &["porkchop"],
        87 => &["mutton", "white_wool"],
        _ => &[],
    };
    names.iter().filter_map(|n| name_to_item(n)).collect()
}

fn armor_mult(c: &Client) -> f32 {
    let pts = *c.armor_pts.lock().unwrap();
    (1.0 - 0.04 * pts as f32).max(0.2)
}
fn armor_pts_of(inv: &HashMap<i16, (i64, u8)>) -> i32 {
    let mut pts = 0i32;
    let slots: [(i16, &[(&str, i32)]); 4] = [
        (5, &[("leather_helmet",1),("golden_helmet",2),("chainmail_helmet",2),("iron_helmet",2),("diamond_helmet",3),("netherite_helmet",3),("turtle_helmet",2)]),
        (6, &[("leather_chestplate",3),("golden_chestplate",5),("chainmail_chestplate",5),("iron_chestplate",6),("diamond_chestplate",8),("netherite_chestplate",8),("elytra",0)]),
        (7, &[("leather_leggings",2),("golden_leggings",3),("chainmail_leggings",4),("iron_leggings",5),("diamond_leggings",6),("netherite_leggings",6)]),
        (8, &[("leather_boots",1),("golden_boots",1),("chainmail_boots",1),("iron_boots",2),("diamond_boots",3),("netherite_boots",3)])];
    for (slot, mats) in slots {
        if let Some(&(item, _)) = inv.get(&slot) {
            for (n, p) in mats { if name_to_item(n) == Some(item) { pts += p } }
        }
    }
    pts
}

// ---------- mobs ----------
struct Mob {
    eid: i32,
    uuid: [u8; 16],
    etype: i64,
    pos: Mutex<(f64, f64, f64, f32, f32)>,
    hp: Mutex<f32>,
    last_hit: Mutex<Instant>,
    fuse: Mutex<i32>,
    love: Mutex<i32>,
}
static MOBS: OnceLock<Mutex<Vec<Arc<Mob>>>> = OnceLock::new();
fn mobs() -> &'static Mutex<Vec<Arc<Mob>>> {
    MOBS.get_or_init(|| Mutex::new(Vec::new()))
}
static SHEARED: OnceLock<Mutex<std::collections::HashSet<i32>>> = OnceLock::new();
fn sheared() -> &'static Mutex<std::collections::HashSet<i32>> { SHEARED.get_or_init(|| Mutex::new(std::collections::HashSet::new())) }
fn spawn_mob(m: &Mob) -> Vec<u8> {
    let (x, y, z, yaw, pitch) = *m.pos.lock().unwrap();
    let mut p = Vec::new();
    write_varint(&mut p, m.eid as i64);
    p.extend_from_slice(&m.uuid);
    write_varint(&mut p, m.etype);
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.push(angle(pitch) as u8); p.push(angle(yaw) as u8); p.push(angle(yaw) as u8);
    write_varint(&mut p, 0);
    p.extend_from_slice(&[0u8; 6]);
    p
}
fn spawn_entity_pkt(eid: i32, etype: u32, x: f64, y: f64, z: f64, yaw: f32, pitch: f32) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.extend_from_slice(&[0u8; 16]);
    write_varint(&mut p, etype as i64);
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.push(angle(pitch) as u8); p.push(angle(yaw) as u8); p.push(angle(yaw) as u8);
    write_varint(&mut p, 0);
    p.extend_from_slice(&[0u8; 6]);
    p
}
fn surface_y(x: i32, z: i32) -> f64 {
    // top non-air block + 1 (flat world => 16 unless overlay edits)
    let mut y = 100i32;
    while y > MIN_Y && block_at(x, y - 1, z) == 0 { y -= 1; }
    y as f64
}
static TICK: AtomicU64 = AtomicU64::new(0);
static WORLD_TIME: AtomicU64 = AtomicU64::new(1000); // morning
static RAINING: AtomicU64 = AtomicU64::new(0); // ticks of rain remaining
fn keepalive_time_tick() {
    let t = TICK.fetch_add(1, Ordering::Relaxed);
    // world clock advances every tick (20 tps => +20/s)
    let wt = WORLD_TIME.fetch_add(5, Ordering::Relaxed) % 24000;
    if t % 4 == 0 {
        broadcast(0, |_c| {
            let mut p = Vec::new();
            p.extend_from_slice(&t.to_be_bytes());      // world age (i64)
            p.extend_from_slice(&(wt as i64).to_be_bytes());
            (0x64, p)
        });
    }
    if t % 40 == 0 {
        broadcast(0, |_c| { (0x26, t.to_be_bytes().to_vec()) });
    }
    if t % 20 == 0 { // container size watch (leak hunt)
        let rss = std::fs::read_to_string("/proc/self/status").unwrap_or_default()
            .lines().find(|l| l.starts_with("VmRSS")).map(|l| l.to_string()).unwrap_or_default();
        eprintln!("[sz] t={} {} | mobs={} drops={} xps={} arrows={} bob={} pearl={} primed={} chests={} furn={} crops={} world={} inv={} clients={}",
            t, rss.trim(),
            mobs().lock().unwrap().len(), drops().lock().unwrap().len(), xps().lock().unwrap().len(),
            arrows().lock().unwrap().len(), bobbers().lock().unwrap().len(), pearls().lock().unwrap().len(),
            primed().lock().unwrap().len(), chests().lock().unwrap().len(), furnaces().lock().unwrap().len(),
            crops().lock().unwrap().len(), world().lock().unwrap().len(), inv_store().lock().unwrap().len(),
            clients().lock().unwrap().len());
    }
    // weather: occasional rain
    {
        let rt = RAINING.load(Ordering::Relaxed);
        if rt > 0 {
            if RAINING.fetch_sub(1, Ordering::Relaxed) == 1 {
                broadcast(-1, |_c| (0x22, vec![2u8, 0, 0, 0, 0]));
            }
        } else if t.wrapping_mul(2654435761) % 6000 == 0 {
            RAINING.store(2400, Ordering::Relaxed);
            broadcast(-1, |_c| (0x22, vec![2u8, 0, 0, 0, 0]));
        }
    }
    // hunger: drain by distance, regen at high food, starve at 0
    {
        let cls: Vec<Arc<Client>> = { clients().lock().unwrap().iter().cloned().collect() };
        for c in cls.iter() {
            if *c.gm.lock().unwrap() == 1 { continue }
            let (px, py, pz, ..) = { let p = c.pos.lock().unwrap(); *p };
            let mut lp = c.lastp.lock().unwrap();
            let d = ((px - lp.0).powi(2) + (pz - lp.2).powi(2)).sqrt();
            *lp = (px, py, pz);
            let mut fchg = false;
            if d < 10.0 {
                let mut wa = c.walk_acc.lock().unwrap();
                *wa += d * if *c.sprinting.lock().unwrap() { 4.0 } else { 1.0 };
                if *wa > 50.0 { *wa = 0.0; let mut f = c.food.lock().unwrap(); *f = (*f - 1).max(0); fchg = true; }
            }
            let food = *c.food.lock().unwrap();
            let mut h = c.hp.lock().unwrap();
            let mut dirty = fchg;
            if food >= 18 && *h < 20.0 && *h > 0.0 && t % 16 == 0 {
                *h = (*h + 1.0).min(20.0); dirty = true;
                let mut st = c.sat.lock().unwrap(); *st -= 0.8;
                if *st <= 0.0 { *st = 0.0; let mut f = c.food.lock().unwrap(); *f = (*f - 1).max(0); }
            }
            if food <= 0 && *h > 1.0 && t % 16 == 8 { *h = (*h - 1.0).max(1.0); dirty = true; }
            {   // drowning: head in water drains air, then 2hp/s
                let wst = name_to_state("water").unwrap_or(u32::MAX);
                let submerged = block_at(px.floor() as i32, (py + 1.62).floor() as i32, pz.floor() as i32) == wst;
                let mut air = c.air.lock().unwrap();
                if !submerged { *air = 60; }
                else if *air > 0 { *air -= 1; }
                else if t % 4 == 0 { *h = (*h - 2.0).max(0.0); dirty = true; }
            }
            {   // lava: feet in lava -> 4hp/tick-ish (vanilla 4hp/0.5s; ours every 2 ticks)
                let lst = name_to_state("lava").unwrap_or(u32::MAX);
                let in_lava = block_at(px.floor() as i32, (py - 0.2).floor() as i32, pz.floor() as i32) == lst
                    || block_at(px.floor() as i32, py.floor() as i32, pz.floor() as i32) == lst
                    || block_at(px.floor() as i32, (py + 1.0).floor() as i32, pz.floor() as i32) == lst;
                if in_lava && t % 2 == 0 { *h = (*h - 4.0).max(0.0); dirty = true; }
            }
            if dirty { if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut *w, 0x5d, health_pkt_c(*h, c)); } }
        }
    }
    // crop growth: ~1 stage per 60s average (1/240 chance per 250ms tick)
    {
        let mut cg = crops().lock().unwrap();
        let mut ripe: Vec<(i32,i32,i32)> = Vec::new();
        let keys: Vec<(i32,i32,i32)> = cg.keys().copied().collect();
        for p in keys {
            let age = cg.get(&p).copied().unwrap_or(0);
            let hh = ((p.0 as i64).wrapping_mul(31) ^ (p.2 as i64).wrapping_mul(37) ^ (p.1 as i64) ^ t as i64) as u64;
            if hh % 120 == 0 {
                if age < 7 {
                    cg.insert(p, age + 1);
                    let st = 4278 + (age as u32) + 1;
                    set_block(p.0, p.1, p.2, st);
                    ripe.push(p);
                }
            }
        }
        drop(cg);
        for (x, y, z) in ripe {
            let bc = block_change(x, y, z, block_at(x, y, z));
            broadcast(-1, |_c| (0x09, bc.clone()));
        }
    }
    if t % 120 == 119 { save_world(); }
    // night spawn cycle: dark hours -> zombies/creepers near players (cap 12)
    if t % 40 == 20 && wt >= 13000 && wt <= 23000 {
        let mcount = mobs().lock().unwrap().iter().filter(|m| matches!(m.etype, 124 | 23)).count();
        let cls = clients().lock().unwrap();
        if mcount < 12 && !cls.is_empty() {
            let idx = (t / 40) as usize;
            if let Some(c) = cls.get(idx % cls.len()) {
                let (px, py, pz, ..) = { let p = c.pos.lock().unwrap(); *p };
                let ang = (t as f64) * 2.399;
                let (nx, nz) = (px + ang.cos() * 14.0, pz + ang.sin() * 14.0);
                let ny = surface_y(nx.round() as i32, nz.round() as i32);
                let et = match (t / 40) % 5 { 0 => 23, 1 => 91, 2 => 100, _ => 124 };
                let m = Arc::new(Mob { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), uuid: [0u8;16],
                    etype: et, pos: Mutex::new((nx, ny, nz, 0.0, 0.0)), hp: Mutex::new(20.0),
                    last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)), fuse: Mutex::new(0), love: Mutex::new(0) });
                let sp = spawn_mob(&m);
                mobs().lock().unwrap().push(m);
                drop(cls);
                broadcast(-1, |_c| (0x01, sp.clone()));
            } else { drop(cls); }
        } else { drop(cls); }
    }
    { // fishing bobbers: fly, land on water, bite after 3-7 s
        let mut fb = bobbers().lock().unwrap();
        for b in fb.iter_mut() {
            b.age += 1;
            let wst = name_to_state("water").unwrap_or(u32::MAX);
            let in_water = block_at(b.x.floor() as i32, b.y.floor() as i32, b.z.floor() as i32) == wst;
            if !in_water && b.dy.abs() + b.dx.abs() + b.dz.abs() > 0.01 {
                b.x += b.dx * 0.6; b.y += b.dy * 0.6; b.z += b.dz * 0.6;
                b.dy -= 0.04;
            } else {
                b.dx = 0.0; b.dy = 0.0; b.dz = 0.0;
                if !b.bit && b.age > 15 { b.bit = ((b.age as u64).wrapping_mul(2654435761) % 11) < 6; }
            }
            if b.age % 3 == 0 {
                let (ax, ay, az, eid) = (b.x, b.y, b.z, b.eid);
                broadcast(-1, |_c| (0x70, entity_teleport(eid, ax, ay, az, 0.0, 0.0, true)));
            }
        }
    }
    { // thrown pearls: fly, on expiry teleport owner
        let mut pl = pearls().lock().unwrap();
        let mut done: Vec<(i32, f64, f64, f64)> = vec![];
        for p0 in pl.iter_mut() {
            p0.ttl -= 1;
            let hit = block_at(p0.x.floor() as i32, p0.y.floor() as i32, p0.z.floor() as i32) != 0;
            if hit || p0.ttl <= 0 { done.push((p0.owner, p0.x, p0.y, p0.z)); }
            else { p0.x += p0.dx * 0.7; p0.y += p0.dy * 0.7; p0.z += p0.dz * 0.7; p0.dy -= 0.05; }
            let (ax, ay, az, eid) = (p0.x, p0.y, p0.z, p0.eid);
            broadcast(-1, |_c| (0x70, entity_teleport(eid, ax, ay, az, 0.0, 0.0, true)));
        }
        for (owner, tx, ty, tz) in done {
            let dead_pearl = pl.iter().find(|p0| p0.owner == owner).map(|p0| p0.eid);
            pl.retain(|p0| p0.owner != owner);
            if let Some(peid) = dead_pearl {
                let mut dp = Vec::new(); write_varint(&mut dp, 1); write_varint(&mut dp, peid as i64);
                broadcast(-1, |_c| (0x42, dp.clone()));
            }
            let cls = clients().lock().unwrap();
            if let Some(c) = cls.iter().find(|c| c.eid == owner) {
                { let mut p = c.pos.lock().unwrap(); *p = (tx, ty, tz, p.3, p.4, true); }
                if let Ok(mut w) = c.sock.lock() {
                    let _ = send_packet(&mut *w, 0x40, teleport_pkt(tx, ty, tz, 0.0, 0.0, 100));
                    let mut h = c.hp.lock().unwrap(); *h = (*h - 5.0).max(0.5);
                    let hh = *h; drop(h);
                    let _ = send_packet(&mut *w, 0x5d, health_pkt_c(hh, c));
                }
            }
        }
    }
    { // snowballs: fly, hit mob -> hurt flash
        let mut sl2 = snowballs().lock().unwrap();
        let mut done: Vec<(i32, Option<i32>)> = vec![];
        for p0 in sl2.iter_mut() {
            p0.ttl -= 1;
            let mut hit_mob: Option<i32> = None;
            { let ms = mobs().lock().unwrap();
                for m in ms.iter() {
                    let mp = m.pos.lock().unwrap();
                    if (mp.0 - p0.x).abs() < 0.6 && (mp.2 - p0.z).abs() < 0.6 && p0.y > mp.1 - 0.3 && p0.y < mp.1 + 2.0 { hit_mob = Some(m.eid); break; }
                }
            }
            let blocked = block_at(p0.x.floor() as i32, p0.y.floor() as i32, p0.z.floor() as i32) != 0;
            if hit_mob.is_some() || blocked || p0.ttl <= 0 { done.push((p0.eid, hit_mob)); }
            else {
                p0.x += p0.dx; p0.y += p0.dy; p0.z += p0.dz; p0.dy -= 0.05;
                broadcast(-1, |_c| (0x70, entity_teleport(p0.eid, p0.x, p0.y, p0.z, 0.0, 0.0, true)));
            }
        }
        for (seid, hit) in done {
            sl2.retain(|p0| p0.eid != seid);
            let mut dp = Vec::new(); write_varint(&mut dp, 1); write_varint(&mut dp, seid as i64);
            broadcast(-1, |_c| (0x42, dp.clone()));
            if let Some(meid) = hit { broadcast(-1, |_c| (0x24, hurt_anim(meid))); }
        }
    }
    { // despawn xp orbs after 5 min
        let mut xl = xps().lock().unwrap();
        let exp: Vec<i32> = xl.iter().filter(|x| x.age.elapsed() > Duration::from_secs(300)).map(|x| x.eid).collect();
        for eid in &exp {
            xl.retain(|x| x.eid != *eid);
            let mut p = Vec::new(); write_varint(&mut p, 1); write_varint(&mut p, *eid as i64);
            broadcast(-1, |_c| (0x42, p.clone()));
        }
    }
    // #minecraft:tick functions (approx rate: our tick = 250ms, vanilla = 50ms)
    let ticks: Vec<String> = packs().lock().unwrap().iter().flat_map(|p| p.tick.clone()).collect();
    for f in ticks { run_function(&f, None, &mut None); }
}
fn arrow_tick() {
    let mut dead: Vec<i32> = vec![];
    let mut hits: Vec<i32> = vec![]; // victim eids
    let mut mob_dead: Vec<(i32, i64, f64, f64, f64)> = vec![];
    {
        let mut ar = arrows().lock().unwrap();
        for a in ar.iter_mut() {
            a.x += a.dx * 0.9; a.y += a.dy * 0.9; a.z += a.dz * 0.9;
            a.ttl -= 1;
            // gravity-lite
            a.dy -= 0.02;
            let (ax, ay, az, eid, ttl) = (a.x, a.y, a.z, a.eid, a.ttl);
            broadcast(-1, |_c| (0x70, entity_teleport(eid, ax, ay, az, 0.0, 0.0, true)));
            if ttl <= 0 { dead.push(eid); continue }
            if a.owner >= 0 {
                // player-shot arrow: hit mobs (sub-step to avoid tunneling)
                let ml = mobs().lock().unwrap();
                'outer: for m in ml.iter() {
                    let (mx, my, mz, ..) = *m.pos.lock().unwrap();
                    for st in 1..=4 {
                        let t = st as f64 / 4.0;
                        let (sx, sy, sz) = (ax - a.dx*0.9*(1.0-t), ay - a.dy*0.9*(1.0-t), az - a.dz*0.9*(1.0-t));
                        if (mx-sx).abs() < 1.2 && (mz-sz).abs() < 1.2 && sy > my - 0.4 && sy < my + 2.1 {
                            let mut h = m.hp.lock().unwrap(); *h -= 6.0;
                            let dh = *h; drop(h);
                            broadcast(-1, |_c| (0x24, hurt_anim(m.eid)));
                            if dh <= 0.0 { mob_dead.push((m.eid, m.etype, mx, my, mz)); }
                            dead.push(eid); break 'outer;
                        }
                    }
                }
            } else {
                let cls = clients().lock().unwrap();
                for c in cls.iter() {
                    let (px, py, pz, ..) = { let p = c.pos.lock().unwrap(); *p };
                    // player hitbox ~0.6 wide, 1.8 tall
                    if (px-ax).abs() < 0.7 && (pz-az).abs() < 0.7 && ay > py - 0.3 && ay < py + 2.0 {
                        hits.push(c.eid); dead.push(eid); break;
                    }
                }
            }
        }
        ar.retain(|a| !dead.contains(&a.eid));
    }
    for (deid, etype, mx, my, mz) in mob_dead {
        mobs().lock().unwrap().retain(|m| m.eid != deid);
        let mut p = Vec::new(); write_varint(&mut p, 1); write_varint(&mut p, deid as i64);
        broadcast(-1, |_c| (0x42, p.clone()));
        let was_sheared = sheared().lock().unwrap().remove(&deid);
        for item in mob_loot(etype).into_iter().filter(|i| !(was_sheared && *i == 202)) {
            let d = Drop { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), x: mx, y: my, z: mz, item, age: Instant::now() };
            let sp = spawn_drop_pkt(&d); let md = drop_meta_pkt(&d);
            drops().lock().unwrap().push(d);
            broadcast(-1, |_c| (0x01, sp.clone()));
            broadcast(-1, |_c| (0x58, md.clone()));
        }
        let xeid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
        let xp = xp_orb_pkt(xeid, mx, my, mz, 3);
        xps().lock().unwrap().push(Xp { eid: xeid, x: mx, y: my, z: mz, val: 3, age: Instant::now() });
        broadcast(-1, |_c| (0x02, xp.clone()));
    }
    for eid in &dead {
        let mut dp = Vec::new(); write_varint(&mut dp, 1); write_varint(&mut dp, *eid as i64);
        broadcast(-1, |_c| (0x42, dp.clone()));
    }
    for veid in hits {
        let cls = clients().lock().unwrap();
        if let Some(c) = cls.iter().find(|c| c.eid == veid) {
            if *c.gm.lock().unwrap() == 1 { continue }
            let mut h = c.hp.lock().unwrap();
            *h = (*h - 3.0 * armor_mult(c)).max(0.0);
            let hh = *h; drop(h);
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut *w, 0x24, hurt_anim(veid));
                let _ = send_packet(&mut *w, 0x5d, health_pkt_c(hh, c));
            }
            let vname = c.name.clone();
            drop(cls);
            broadcast(veid, move |_c| (0x24, hurt_anim(veid)));
            if hh <= 0.0 {
                let line = system_chat(&format!("{} was shot by Skeleton", vname));
                broadcast(-1, |_c| (0x6c, line.clone()));
            }
        }
    }
}
fn explode_at(ex: f64, ey: f64, ez: f64, power: f64) {
    let (bx, by, bz) = (ex.floor() as i32, ey.floor() as i32, ez.floor() as i32);
    let mut offs: Vec<(i8,i8,i8)> = Vec::new();
    for dx in -2..=2 { for dy in -1..=2 { for dz in -2..=2 {
        if dx*dx+dy*dy+dz*dz <= 5 {
            offs.push((dx as i8, dy as i8, dz as i8));
            set_block(bx+dx as i32, by+dy as i32, bz+dz as i32, 0);
        }
    }}}
    broadcast(-1, |_c| (0x20, explosion_pkt(ex, ey, ez, &offs)));
    let cs: Vec<Arc<Client>> = { clients().lock().unwrap().iter().cloned().collect() };
    for cl in cs {
        let (px,py,pz,..) = { let p = cl.pos.lock().unwrap(); *p };
        let dd = ((px-ex).powi(2)+(py-ey).powi(2)+(pz-ez).powi(2)).sqrt();
        if dd < power*1.5 && *cl.gm.lock().unwrap() != 1 {
            let mut h = cl.hp.lock().unwrap();
            *h = (*h - ((power*1.5-dd)*3.0) as f32 * armor_mult(&*cl)).max(0.0);
            let hh = *h; drop(h);
            if let Ok(mut w) = cl.sock.lock() {
                let _ = send_packet(&mut *w, 0x24, hurt_anim(cl.eid));
                let _ = send_packet(&mut *w, 0x5d, health_pkt_c(hh, &cl));
            }
        }
    }
}

fn primed_tick() {
    let mut boom: Vec<Primed> = Vec::new();
    {
        let mut pl = primed().lock().unwrap();
        let mut i = 0;
        while i < pl.len() {
            if pl[i].lit.elapsed() > Duration::from_millis(4000) { boom.push(pl.remove(i)); }
            else { i += 1; }
        }
    }
    for p in boom {
        let mut dp = Vec::new(); write_varint(&mut dp, 1); write_varint(&mut dp, p.eid as i64);
        broadcast(-1, |_c| (0x42, dp.clone()));
        explode_at(p.x, p.y, p.z, 4.0);
    }
}

fn furnace_tick() {
    let mut fs = furnaces().lock().unwrap();
    let mut dirty_pos: Vec<(i32,i32,i32)> = Vec::new();
    for (pos, f) in fs.iter_mut() {
        let can_smelt = f.input.1 > 0 && smelt_result(f.input.0).is_some()
            && (f.output.1 == 0 || (f.output.0 == smelt_result(f.input.0).unwrap() && f.output.1 < 64));
        if f.burn_left <= 0 && can_smelt && f.fuel.1 > 0 {
            let t = fuel_ticks(f.fuel.0);
            if t > 0 {
                f.burn_left = t;
                f.fuel.1 -= 1;
                if f.fuel.1 == 0 { f.fuel = (0, 0); }
                f.dirty = true;
            }
        }
        if f.burn_left > 0 && can_smelt {
            f.burn_left -= 1;
            f.progress += 1;
            if f.progress >= 40 {
                f.progress = 0;
                let r = smelt_result(f.input.0).unwrap();
                f.output = (r, f.output.1 + 1);
                f.input.1 -= 1;
                if f.input.1 == 0 { f.input = (0, 0); }
                f.dirty = true;
            }
        } else if f.progress > 0 {
            f.progress -= 1;
        }
        if f.dirty { f.dirty = false; dirty_pos.push(*pos); }
    }
    drop(fs);
    if dirty_pos.is_empty() { return }
    let cs: Vec<Arc<Client>> = { clients().lock().unwrap().iter().cloned().collect() };
    for c in cs {
        let fp = *c.open_furnace.lock().unwrap();
        if let Some(fp) = fp {
            if dirty_pos.contains(&fp) {
                let inv = c.inv.lock().unwrap().clone();
                if let Ok(mut w) = c.sock.lock() { furnace_push_items(&mut *w, fp, &inv); }
            }
        }
    }
}

// minimal redstone: lever powers 6 adjacent cells -> iron door opens/closes, piston pushes one block
fn redstone_update(x: i32, y: i32, z: i32, powered: bool) {
    for (dx, dy, dz) in [(1i32,0,0),(-1,0,0),(0,1,0),(0,-1,0),(0,0,1),(0,0,-1)] {
        let (bx, by, bz) = (x + dx, y + dy, z + dz);
        let st = block_at(bx, by, bz);
        if (5652..=5715).contains(&st) { // iron door: open while powered (both halves)
            for oy in [-1i32, 0, 1] {
                let hst = block_at(bx, by + oy, bz);
                if !(5652..=5715).contains(&hst) { continue }
                let open_now = (hst - 5652) & 2 == 0;
                if open_now != powered {
                    let nst = if powered { hst & !2 } else { hst | 2 };
                    set_block(bx, by + oy, bz, nst);
                    broadcast(-1, |_c| (0x09, block_change(bx, by + oy, bz, nst)));
                }
            }
            continue;
        }
        if (2011..=2022).contains(&st) { // piston
            let facing = ((st - 2011) % 6) as i32; // 0 n,1 e,2 s,3 w,4 up,5 down
            let extended = (st - 2011) & 6 == 0;
            let (fx, fy, fz) = match facing { 0 => (0,0,-1), 1 => (1,0,0), 2 => (0,0,1), 3 => (-1,0,0), 4 => (0,1,0), _ => (0,-1,0) };
            let (px, py, pz) = (bx + fx, by + fy, bz + fz);
            if powered && !extended {
                let nst = st ^ 6;
                set_block(bx, by, bz, nst);
                broadcast(-1, |_c| (0x09, block_change(bx, by, bz, nst)));
                let front = block_at(px, py, pz);
                let (qx, qy, qz) = (bx + fx * 2, by + fy * 2, bz + fz * 2);
                let dest = block_at(qx, qy, qz);
                let head = 2023 + facing as u32 * 4; // piston_head, normal type
                if front != 0 && dest == 0 {
                    set_block(qx, qy, qz, front);
                    set_block(px, py, pz, head);
                    broadcast(-1, |_c| (0x09, block_change(qx, qy, qz, front)));
                    broadcast(-1, |_c| (0x09, block_change(px, py, pz, head)));
                } else if front == 0 {
                    set_block(px, py, pz, head);
                    broadcast(-1, |_c| (0x09, block_change(px, py, pz, head)));
                }
            } else if !powered && extended {
                let nst = st ^ 6;
                set_block(bx, by, bz, nst);
                broadcast(-1, |_c| (0x09, block_change(bx, by, bz, nst)));
                if (2023..=2046).contains(&block_at(px, py, pz)) {
                    set_block(px, py, pz, 0);
                    broadcast(-1, |_c| (0x09, block_change(px, py, pz, 0)));
                }
            }
            continue;
        }
    }
}
fn drop_tick() {
    // player within 1.3 blocks picks up the drop
    let mut taken: Vec<(i32, i32, i64)> = vec![]; // (drop_eid, collector_eid, item)
    {
        let ds: Vec<(i32,f64,f64,f64,i64)> = drops().lock().unwrap().iter().map(|d| (d.eid,d.x,d.y,d.z,d.item)).collect();
        let cls = clients().lock().unwrap();
        for (d_eid, dx, dy, dz, d_item) in ds.iter() {
            let d = &Drop { eid: *d_eid, x: *dx, y: *dy, z: *dz, item: *d_item, age: Instant::now() };
            for c in cls.iter() {
                let (px, py, pz, ..) = { let p = c.pos.lock().unwrap(); *p };
                if (px-d.x).powi(2)+(py-d.y).powi(2)+(pz-d.z).powi(2) < 1.69 {
                    taken.push((d.eid, c.eid, d.item));
                    break;
                }
            }
        }
    }
    // xp orbs float to nearby players and grant xp
    {
        let mut collected: Vec<(i32, i32, i16)> = vec![];
        {
            let xs = xps().lock().unwrap();
            let cls = clients().lock().unwrap();
            for x in xs.iter() {
                for c in cls.iter() {
                    let (px, py, pz, ..) = { let p = c.pos.lock().unwrap(); *p };
                    if (px-x.x).powi(2)+(py-x.y).powi(2)+(pz-x.z).powi(2) < 2.56 {
                        collected.push((x.eid, c.eid, x.val));
                        break;
                    }
                }
            }
        }
        for (xeid, ceid, val) in collected {
            xps().lock().unwrap().retain(|x| x.eid != xeid);
            let cls = clients().lock().unwrap();
            if let Some(c) = cls.iter().find(|c| c.eid == ceid) {
                let mut xp = c.xp.lock().unwrap();
                *xp += val as i64;
                let total = *xp;
                drop(xp);
                let (lvl, bar) = xp_level(total);
                let mut p = Vec::new();
                p.extend_from_slice(&bar.to_be_bytes());
                write_varint(&mut p, lvl as i64);
                write_varint(&mut p, total);
                if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut *w, 0x5c, p); }
                drop(cls);
                let mut dp = Vec::new(); write_varint(&mut dp, 1); write_varint(&mut dp, xeid as i64);
                broadcast(-1, |_c| (0x42, dp.clone()));
            }
        }
    }
    for (deid, ceid, item) in taken {
        drops().lock().unwrap().retain(|d| d.eid != deid);
        let cls = clients().lock().unwrap();
        if let Some(c) = cls.iter().find(|c| c.eid == ceid) {
            // first free inventory slot 9..=44
            let mut inv = c.inv.lock().unwrap();
            let slot = inv.iter().find(|(_, &(it, cnt))| it == item && cnt < 64)
                .map(|(&sl, _)| sl)
                .unwrap_or_else(|| (9..=44).find(|sl| !inv.contains_key(sl)).unwrap_or(9));
            let cnt = inv.get(&slot).map(|e| e.1).unwrap_or(0) + 1;
            inv.insert(slot, (item, cnt));
            let mut p = vec![0u8];
            write_varint(&mut p, 0);
            p.extend_from_slice(&slot.to_be_bytes());
            write_varint(&mut p, cnt as i64);
            write_varint(&mut p, item);
            write_varint(&mut p, 0); write_varint(&mut p, 0);
            if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut *w, 0x15, p); }
            drop(inv);
            drop(cls);
            broadcast(-1, |_c| (0x6f, collect_pkt(deid, ceid)));
            let mut dp = Vec::new(); write_varint(&mut dp, 1); write_varint(&mut dp, deid as i64);
            broadcast(-1, |_c| (0x42, dp.clone()));
        }
    }
    { // despawn drops after 5 min (vanilla)
        let mut dp = drops().lock().unwrap();
        let exp: Vec<i32> = dp.iter().filter(|d| d.age.elapsed() > Duration::from_secs(300)).map(|d| d.eid).collect();
        for eid in &exp {
            dp.retain(|d| d.eid != *eid);
            let mut p = Vec::new(); write_varint(&mut p, 1); write_varint(&mut p, *eid as i64);
            broadcast(-1, |_c| (0x42, p.clone()));
        }
    }
}
fn mob_tick() {
    let cls: Vec<(i32, f64, f64, f64)> = {
        let c = clients().lock().unwrap();
        c.iter().map(|c| { let p = c.pos.lock().unwrap(); (c.eid, p.0, p.1, p.2) }).collect()
    };
    let wheat_holders: Vec<(f64, f64, f64)> = {
        let c = clients().lock().unwrap();
        c.iter().filter(|c| {
            let hs = 36 + *c.held.lock().unwrap();
            c.inv.lock().unwrap().get(&hs).map(|v| v.0).unwrap_or(0) == 854
        }).map(|c| { let p = c.pos.lock().unwrap(); (p.0, p.1, p.2) }).collect()
    };
    if cls.is_empty() { return }
    let mobs_snap: Vec<Arc<Mob>> = mobs().lock().unwrap().iter().cloned().collect();
    let wt = WORLD_TIME.load(Ordering::Relaxed) % 24000;
    let day = wt < 13000 || wt > 23000;
    for m in mobs_snap.iter() {
        let mut mp = m.pos.lock().unwrap();
        // undead burn in daylight with clear sky
        if day && matches!(m.etype, 124 | 91) {
            let (bx, by, bz) = (mp.0.round() as i32, mp.1.round() as i32, mp.2.round() as i32);
            let exposed = by + 1 >= height_at(bx, bz) && block_at(bx, by + 2, bz) == 0;
            let mut burn = m.fuse.lock().unwrap();
            if exposed {
                *burn += 1;
                if *burn >= 8 {
                    *burn = 0;
                    drop(burn); drop(mp);
                    // on-fire flag: entity flags byte idx0 bit0x01
                    let mut md = Vec::new(); write_varint(&mut md, m.eid as i64);
                    md.push(0); md.push(0); md.push(1); md.push(0xff);
                    broadcast(-1, |_c| (0x58, md.clone()));
                    let mut hp = m.hp.lock().unwrap();
                    *hp -= 1.0;
                    let dead = *hp <= 0.0; drop(hp);
                    broadcast(-1, |_c| (0x24, hurt_anim(m.eid)));
                    if dead {
                        let (mx, my, mz) = (m.pos.lock().unwrap().0, m.pos.lock().unwrap().1, m.pos.lock().unwrap().2);
                        mobs().lock().unwrap().retain(|x| x.eid != m.eid);
                        let mut dp = Vec::new(); write_varint(&mut dp, 1); write_varint(&mut dp, m.eid as i64);
                        broadcast(-1, |_c| (0x42, dp.clone()));
                        for item in mob_loot(m.etype) {
                            let d = Drop { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), x: mx, y: my, z: mz, item, age: Instant::now() };
                            let sp = spawn_drop_pkt(&d); let mdd = drop_meta_pkt(&d);
                            drops().lock().unwrap().push(d);
                            broadcast(-1, |_c| (0x01, sp.clone()));
                            broadcast(-1, |_c| (0x58, mdd.clone()));
                        }
                    }
                    continue;
                }
            } else if *burn != 0 {
                *burn = 0;
                drop(burn); drop(mp);
                let mut md = Vec::new(); write_varint(&mut md, m.eid as i64);
                md.push(0); md.push(0); md.push(0); md.push(0xff);
                broadcast(-1, |_c| (0x58, md.clone()));
                continue;
            }
        }
        // creeper: approach then fuse+explode
        if m.etype == 23 {
            let mut best: Option<(f64,f64,f64)> = None; let mut bd = f64::MAX;
            for &(_eid, px, py, pz) in &cls {
                let d = (px-mp.0).powi(2)+(pz-mp.2).powi(2);
                if d < bd { bd = d; best = Some((px,py,pz)); }
            }
            if let Some((px,_py,pz)) = best {
                let mut fuse = m.fuse.lock().unwrap();
                if bd < 9.0 {
                    *fuse += 1;
                    if *fuse >= 6 {
                        // explode: crater radius 3 (skip bedrock), damage nearby players
                        let (cx, cy, cz) = (mp.0.round() as i32, mp.1.round() as i32, mp.2.round() as i32);
                        let mut recs: Vec<(i8,i8,i8)> = vec![];
                        for dx in -3i32..=3 { for dy in -3i32..=3 { for dz in -3i32..=3 {
                            if dx*dx+dy*dy+dz*dz > 9 { continue }
                            let (bx, by, bz) = (cx+dx, cy+dy, cz+dz);
                            let st = block_at(bx, by, bz);
                            if st == 0 || st == 79 { continue }
                            set_block(bx, by, bz, 0);
                            recs.push((dx as i8, dy as i8, dz as i8));
                            broadcast(-1, |_c| (0x09, block_change(bx, by, bz, 0)));
                        }}}
                        broadcast(-1, |_c| (0x20, explosion_pkt(mp.0, mp.1, mp.2, &recs)));
                        for &(_eid, px, py, pz) in &cls {
                            let dd = ((px-mp.0).powi(2)+(py-mp.1).powi(2)+(pz-mp.2).powi(2)).sqrt();
                            if dd < 5.0 {
                                let c = clients().lock().unwrap();
                                if let Some(cl) = c.iter().find(|c| c.eid == _eid) {
                                    if *cl.gm.lock().unwrap() == 1 { continue }
                                    let mut h = cl.hp.lock().unwrap();
                                    *h = (*h - ((5.0-dd)*3.0) as f32 * armor_mult(&*cl)).max(0.0);
                                    let hh = *h; drop(h);
                                    if let Ok(mut w) = cl.sock.lock() {
                                        let _ = send_packet(&mut *w, 0x24, hurt_anim(cl.eid));
                                        let _ = send_packet(&mut *w, 0x5d, health_pkt_c(hh, cl));
                                    }
                                }
                            }
                        }
                        drop(fuse); drop(mp);
                        let deid = m.eid;
                        mobs().lock().unwrap().retain(|m| m.eid != deid);
                        let mut p = Vec::new(); write_varint(&mut p, 1); write_varint(&mut p, deid as i64);
                        broadcast(-1, |_c| (0x42, p.clone()));
                        return;
                    }
                } else if bd < 24.0*24.0 {
                    *fuse = 0;
                    drop(fuse);
                    let dx = px-mp.0; let dz = pz-mp.2; let len = (dx*dx+dz*dz).sqrt();
                    mp.0 += dx/len * 0.25; mp.2 += dz/len * 0.25;
                    mp.1 = surface_y(mp.0.round() as i32, mp.2.round() as i32);
                    mp.3 = (dx.atan2(dz) * 180.0 / std::f64::consts::PI) as f32 - 180.0;
                    drop(mp);
                    let (x,y,z,yaw,pitch) = *m.pos.lock().unwrap();
                    broadcast(0, |_c| (0x70, entity_teleport(m.eid, x, y, z, yaw, pitch, true)));
                    broadcast(0, |_c| (0x48, head_rot(m.eid, yaw)));
                } else { drop(fuse); }
            }
            continue;
        }
        // sheared sheep regrow wool by eating grass
        if m.etype == 87 && sheared().lock().unwrap().contains(&m.eid) {
            let (bx, by, bz) = (mp.0.round() as i32, mp.1.round() as i32, mp.2.round() as i32);
            if block_at(bx, by - 1, bz) == name_to_state("grass_block").unwrap_or(u32::MAX)
                && ((m.eid as u64).wrapping_mul(1103515245) ^ TICK.load(Ordering::Relaxed)) % 400 == 0 {
                set_block(bx, by - 1, bz, name_to_state("dirt").unwrap_or(0));
                sheared().lock().unwrap().remove(&m.eid);
                let mut p = Vec::new(); write_varint(&mut p, ((bx as i64) << 38) | ((bz as i64 & 0x3ffffff) << 12) | (by - 1) as i64 & 0xfff);
                write_varint(&mut p, name_to_state("dirt").unwrap_or(0) as i64);
                broadcast(-1, |_c| (0x09, p.clone()));
            }
        }
        // passive mobs: slow wander + breeding (two loved animals -> baby)
        if matches!(m.etype, 22 | 77 | 87 | 19) {
            let mut lov = m.love.lock().unwrap();
            if *lov > 0 {
                *lov -= 1;
                let partner = mobs_snap.iter().find(|p| p.eid != m.eid && p.etype == m.etype
                    && *p.love.lock().unwrap() > 0
                    && { let q = p.pos.lock().unwrap(); (q.0-mp.0).powi(2)+(q.2-mp.2).powi(2) < 16.0 });
                if let Some(p) = partner {
                    *lov = 0; *p.love.lock().unwrap() = 0;
                    let q = *p.pos.lock().unwrap();
                    let (bx, bz) = ((mp.0+q.0)/2.0, (mp.2+q.2)/2.0);
                    let by = surface_y(bx.round() as i32, bz.round() as i32);
                    drop(lov); drop(mp);
                    let baby = Arc::new(Mob { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), uuid: [0u8;16],
                        etype: m.etype, pos: Mutex::new((bx, by, bz, 0.0, 0.0)), hp: Mutex::new(20.0),
                        last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
                        fuse: Mutex::new(0), love: Mutex::new(0) });
                    let sp = spawn_mob(&baby);
                    mobs().lock().unwrap().push(baby);
                    broadcast(-1, |_c| (0x01, sp.clone()));
                    let xeid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                    let xp = xp_orb_pkt(xeid, bx, by, bz, 3);
                    xps().lock().unwrap().push(Xp { eid: xeid, x: bx, y: by, z: bz, val: 3, age: Instant::now() });
                    broadcast(-1, |_c| (0x02, xp.clone()));
                    continue;
                }
            }
            drop(lov);
            // follow nearest player holding wheat within 9 blocks
            let mut tgt: Option<(f64, f64)> = None; let mut bd = f64::MAX;
            for &(hx, _hy, hz) in &wheat_holders {
                let d = (hx - mp.0).powi(2) + (hz - mp.2).powi(2);
                if d < 81.0 && d < bd { bd = d; tgt = Some((hx, hz)); }
            }
            if let Some((tx, tz)) = tgt {
                if bd > 2.0 {
                    let dx = tx - mp.0; let dz = tz - mp.2; let len = (dx*dx + dz*dz).sqrt().max(0.01);
                    mp.0 += dx / len * 0.28; mp.2 += dz / len * 0.28;
                    mp.1 = surface_y(mp.0.round() as i32, mp.2.round() as i32);
                }
                mp.3 = ((-((tx - mp.0).atan2(tz - mp.2))) as f32).to_degrees();
                let (x,y,z,yaw,pitch) = *mp;
                drop(mp);
                broadcast(0, |_c| (0x70, entity_teleport(m.eid, x, y, z, yaw, pitch, true)));
            } else if (m.eid as i64 + TICK.load(Ordering::Relaxed) as i64) % 32 == 0 {
                let t = ((m.eid as u32).wrapping_mul(2654435761u32) ^ (TICK.load(Ordering::Relaxed) as u32)) as f64 / 4294967295.0 * std::f64::consts::TAU;
                mp.0 += t.cos() * 2.0; mp.2 += t.sin() * 2.0;
                mp.1 = surface_y(mp.0.round() as i32, mp.2.round() as i32);
                drop(mp);
                let (x,y,z,yaw,pitch) = *m.pos.lock().unwrap();
                broadcast(0, |_c| (0x70, entity_teleport(m.eid, x, y, z, yaw, pitch, true)));
            } else { drop(mp); }
            continue;
        }
        // skeleton: hold 6-12 b range, shoot arrows every ~3 s
        if m.etype == 91 {
            let mut best: Option<(f64,f64,f64)> = None; let mut bd = f64::MAX;
            for &(_eid, px, py, pz) in &cls {
                let d = (px-mp.0).powi(2)+(pz-mp.2).powi(2);
                if d < bd { bd = d; best = Some((px,py,pz)); }
            }
            if let Some((px,py,pz)) = best {
                let dist = bd.sqrt();
                // reposition: back off if <5, approach if >12
                if dist > 12.0 || dist < 5.0 {
                    let dir = if dist > 12.0 { 1.0 } else { -1.0 };
                    let dx = px-mp.0; let dz = pz-mp.2; let len = (dx*dx+dz*dz).sqrt().max(0.01);
                    mp.0 += dx/len * 0.22 * dir; mp.2 += dz/len * 0.22 * dir;
                    mp.1 = surface_y(mp.0.round() as i32, mp.2.round() as i32);
                    let (x,y,z,yaw,pitch) = *mp;
                    broadcast(0, |_c| (0x70, entity_teleport(m.eid, x, y, z, yaw, pitch, true)));
                }
                // fire every ~3 s when in range
                let mut lh = m.last_hit.lock().unwrap();
                if lh.elapsed() > Duration::from_millis(3000) && dist < 16.0 {
                    *lh = Instant::now();
                    drop(lh);
                    let (sx, sy, sz) = (mp.0, mp.1 + 1.6, mp.2);
                    drop(mp);
                    let (tx, ty, tz) = (px, py + 1.2, pz);
                    let (mut vx, mut vy, mut vz) = (tx-sx, ty-sy, tz-sz);
                    let vl = (vx*vx+vy*vy+vz*vz).sqrt().max(0.01);
                    vx /= vl; vy /= vl; vz /= vl;
                    let a = Arrow { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), owner: -1, x: sx, y: sy, z: sz,
                        dx: vx, dy: vy, dz: vz, ttl: 40 };
                    let sp = spawn_arrow_pkt(&a);
                    arrows().lock().unwrap().push(a);
                    broadcast(-1, |_c| (0x01, sp.clone()));
                }
            }
            continue;
        }
        // chase nearest player within 24 blocks (zombie=124 only)
        if m.etype == 124 {
            let mut best: Option<(f64,f64,f64)> = None; let mut bd = f64::MAX;
            for &(_eid, px, py, pz) in &cls {
                let d = (px-mp.0).powi(2)+(pz-mp.2).powi(2);
                if d < bd { bd = d; best = Some((px,py,pz)); }
            }
            if let Some((px,_py,pz)) = best {
                // melee: within 2.5 blocks, 1s cooldown -> damage that player
                if bd < 2.5*2.5 {
                    let mut lh = m.last_hit.lock().unwrap();
                    if lh.elapsed() > Duration::from_millis(1000) {
                        *lh = Instant::now();
                        drop(lh); drop(mp);
                        // find which client is nearest & hurt them
                        let mut hurt_eid = None;
                        {
                            let c = clients().lock().unwrap();
                            for cl in c.iter() {
                                let (cp0, _cp1, cp2) = { let cp = cl.pos.lock().unwrap(); (cp.0, cp.1, cp.2) };
                                let (mx, _my, mz) = { let mp2 = m.pos.lock().unwrap(); (mp2.0, mp2.1, mp2.2) };
                                let dd = (cp0-mx).powi(2)+(cp2-mz).powi(2);
                                if dd < 6.0 && *cl.gm.lock().unwrap() != 1 {
                                    let mut h = cl.hp.lock().unwrap();
                                    let dmg = if *cl.blocking.lock().unwrap() { 0.0 } else { 2.0 * armor_mult(&*cl) };
                                    *h = (*h - dmg).max(0.0);
                                    let hh = *h;
                                    drop(h);
                                    if let Ok(mut w) = cl.sock.lock() {
                                        let _ = send_packet(&mut *w, 0x24, hurt_anim(cl.eid));
                                        let _ = send_packet(&mut *w, 0x5d, health_pkt_c(hh, cl));
                                    }
                                    hurt_eid = Some((cl.eid, cl.name.clone(), hh));
                                    break;
                                }
                            }
                        }
                        if let Some((eid, vname, hh)) = hurt_eid {
                            broadcast(eid, |_c| (0x24, hurt_anim(eid)));
                            if hh <= 0.0 {
                                let mn = match m.etype { 124 => "Zombie", 91 => "Skeleton", 23 => "Creeper", _ => "mob" };
                                let line = system_chat(&format!("{} was slain by {}", vname, mn));
                                broadcast(-1, |_c| (0x6c, line.clone()));
                            }
                        }
                        continue;
                    }
                }
                if bd < 24.0*24.0 && bd > 1.0 {
                    let dx = px-mp.0; let dz = pz-mp.2; let len = (dx*dx+dz*dz).sqrt();
                    mp.0 += dx/len * 0.25; mp.2 += dz/len * 0.25;
                    mp.1 = surface_y(mp.0.round() as i32, mp.2.round() as i32);
                    mp.3 = (dx.atan2(dz) * 180.0 / std::f64::consts::PI) as f32 - 180.0;
                    drop(mp);
                    let (x,y,z,yaw,pitch) = *m.pos.lock().unwrap();
                    broadcast(0, |_c| (0x70, entity_teleport(m.eid, x, y, z, yaw, pitch, true)));
                    broadcast(0, |_c| (0x48, head_rot(m.eid, yaw)));
                    continue;
                }
            }
        }
    }
}

// ---------- world ----------
// overlay edits: (cx,cz) -> { packed_local_idx -> blockstate }
static WORLD: OnceLock<Mutex<HashMap<(i32, i32), HashMap<u32, u32>>>> = OnceLock::new();
fn world() -> &'static Mutex<HashMap<(i32, i32), HashMap<u32, u32>>> {
    WORLD.get_or_init(|| Mutex::new(HashMap::new()))
}
struct Furnace {
    input: (i64, u8), fuel: (i64, u8), output: (i64, u8),
    burn_left: i32, progress: i32, dirty: bool,
}
static FURNACES: OnceLock<Mutex<HashMap<(i32, i32, i32), Furnace>>> = OnceLock::new();
static CROPS: OnceLock<Mutex<HashMap<(i32, i32, i32), u8>>> = OnceLock::new();
fn crops() -> &'static Mutex<HashMap<(i32, i32, i32), u8>> { CROPS.get_or_init(|| Mutex::new(HashMap::new())) }
fn furnaces() -> &'static Mutex<HashMap<(i32, i32, i32), Furnace>> {
    FURNACES.get_or_init(|| Mutex::new(HashMap::new()))
}
fn smelt_result(item: i64) -> Option<i64> {
    for (a, b) in [("beef","cooked_beef"),("porkchop","cooked_porkchop"),("chicken","cooked_chicken"),
                   ("mutton","cooked_mutton"),("sand","glass"),("cobblestone","stone"),
                   ("potato","baked_potato"),("oak_log","charcoal"),("raw_iron","iron_ingot"),("raw_copper","copper_ingot"),("raw_gold","gold_ingot"),("rabbit","cooked_rabbit"),("cod","cooked_cod")] {
        if name_to_item(a) == Some(item) { return name_to_item(b) }
    }
    None
}
fn fuel_ticks(item: i64) -> i32 {
    for (n, t) in [("coal",320),("charcoal",320),("oak_log",60),("oak_planks",60),("stick",20),("coal_block",3200)] {
        if name_to_item(n) == Some(item) { return t }
    }
    0
}
static CHESTS: OnceLock<Mutex<HashMap<(i32, i32, i32), HashMap<i16, (i64, u8)>>>> = OnceLock::new();
fn chests() -> &'static Mutex<HashMap<(i32, i32, i32), HashMap<i16, (i64, u8)>>> {
    CHESTS.get_or_init(|| Mutex::new(HashMap::new()))
}
static INV_STORE: OnceLock<Mutex<HashMap<String, HashMap<i16, (i64, u8)>>>> = OnceLock::new();
fn inv_store() -> &'static Mutex<HashMap<String, HashMap<i16, (i64, u8)>>> {
    INV_STORE.get_or_init(|| Mutex::new(HashMap::new()))
}
const MIN_Y: i32 = -64;

// ---- procedural terrain (deterministic value-noise heightmap + trees) ----
fn hash01(x: i32, z: i32) -> f64 {
    let mut h = (x as u32).wrapping_mul(374761393) ^ (z as u32).wrapping_mul(668265263);
    h = (h ^ (h >> 13)).wrapping_mul(1274126177);
    ((h ^ (h >> 16)) & 0xffff) as f64 / 65535.0
}
fn smooth(t: f64) -> f64 { t * t * (3.0 - 2.0 * t) }
fn vnoise(x: f64, z: f64, cell: i32) -> f64 {
    let gx = (x / cell as f64).floor() as i32;
    let gz = (z / cell as f64).floor() as i32;
    let fx = smooth(x / cell as f64 - gx as f64);
    let fz = smooth(z / cell as f64 - gz as f64);
    let a = hash01(gx, gz); let b = hash01(gx + 1, gz);
    let c = hash01(gx, gz + 1); let d = hash01(gx + 1, gz + 1);
    a + (b - a) * fx + (c - a) * fz + (a - b - c + d) * fx * fz
}
// surface height: gentle hills, roughly 8..=22
fn height_at(x: i32, z: i32) -> i32 {
    (8.0 + vnoise(x as f64, z as f64, 48) * 10.0
        + vnoise(x as f64 + 1000.0, z as f64 - 500.0, 12) * 4.0) as i32
}
fn tree_at(x: i32, z: i32) -> bool {
    hash01(x ^ 0x5d3f, z ^ 0x7a1b) < 0.006
}
fn column_state(x: i32, y: i32, z: i32, h: i32) -> u32 {
    if y >= h {
        // trunk / leaves live above ground
        if tree_at(x, z) && y < h + 5 {
            return name_to_state("oak_log").unwrap_or(98);
        }
        if y <= 10 && h <= 10 {
            return name_to_state("water").unwrap_or(98);
        }
        if y <= h + 6 {
            for dx in -2i32..=2 { for dz in -2i32..=2 {
                if dx == 0 && dz == 0 { continue }
                let (tx, tz) = (x + dx, z + dz);
                if tree_at(tx, tz) {
                    let th = height_at(tx, tz);
                    if y >= th + 3 && y <= th + 5 {
                        return name_to_state("oak_leaves").unwrap_or(98);
                    }
                }
            }}
        }
        return 0;
    }
    if h <= 11 { // shore floors under/near waterline
        if y < h - 2 { ore_state(x, y, z) } else { name_to_state("sand").unwrap_or(10) }
    } else if y < h - 4 { ore_state(x, y, z) }
    else if y < h - 1 { name_to_state("dirt").unwrap_or(10) }
    else { name_to_state("grass_block").unwrap_or(9) }
}
fn door_base(st: u32) -> Option<u32> {
    for base in [4590u32, 11822, 11886, 11950, 12014, 12078, 12142, 12206, 12270,
                 19148, 19212, 23652, 23716, 23780, 23844, 23908, 23972, 24036, 24100] {
        if (base..base + 64).contains(&st) { return Some(base) }
    }
    None
}
fn gate_base(st: u32) -> Option<u32> {
    for base in [6997u32, 11310, 11342, 11374, 11406, 11438, 11470, 11502, 11534, 18876, 18908] {
        if (base..base + 32).contains(&st) { return Some(base) }
    }
    None
}
fn ore_state(x: i32, y: i32, z: i32) -> u32 {
    let oh = ((x as i64).wrapping_mul(0x9E3779B1) ^ (z as i64).wrapping_mul(0x85EBCA77) ^ (y as i64).wrapping_mul(0xC2B2AE3D)) as u64;
    let oh = oh.wrapping_mul(0x27D4EB2F) ^ (oh >> 15);
    if y <= -52 && y >= -60 && oh % 23 == 0 { return name_to_state("lava").unwrap_or(1) }
    if y <= -32 && oh % 601 == 0 { name_to_state("diamond_ore").unwrap_or(1) }
    else if y <= 4 && oh % 197 == 0 { name_to_state("iron_ore").unwrap_or(1) }
    else if oh % 89 == 0 { name_to_state("coal_ore").unwrap_or(1) }
    else { name_to_state("stone").unwrap_or(1) }
}
fn base_state(x: i32, y: i32, z: i32) -> u32 { column_state(x, y, z, height_at(x, z)) }
fn local_idx(x: i32, y: i32, z: i32) -> u32 {
    (((y - MIN_Y) as u32) << 8) | (((z & 15) as u32) << 4) | (x & 15) as u32
}
fn block_at(x: i32, y: i32, z: i32) -> u32 {
    let w = world().lock().unwrap();
    w.get(&(x >> 4, z >> 4))
        .and_then(|c| c.get(&local_idx(x, y, z)).copied())
        .unwrap_or_else(|| base_state(x, y, z))
}
fn set_block(x: i32, y: i32, z: i32, st: u32) {
    let mut w = world().lock().unwrap();
    let c = w.entry((x >> 4, z >> 4)).or_default();
    if st == base_state(x, y, z) { c.remove(&local_idx(x, y, z)); } else { c.insert(local_idx(x, y, z), st); }
}

// ---------- persistence (overlay only, no pregen) ----------
const WORLD_FILE: &str = "world_overlay.bin";
fn save_world() {
    let w = world().lock().unwrap();
    let mut f = Vec::new();
    write_varint(&mut f, w.len() as i64);
    for (&(cx, cz), c) in w.iter() {
        f.extend_from_slice(&cx.to_be_bytes());
        f.extend_from_slice(&cz.to_be_bytes());
        write_varint(&mut f, c.len() as i64);
        for (&idx, &st) in c.iter() {
            f.extend_from_slice(&idx.to_be_bytes());
            write_varint(&mut f, st as i64);
        }
    }
    drop(w);
    // META section: mobs + world time + inventories (offline store + live)
    f.extend_from_slice(b"MET2");
    {
        let ms = mobs().lock().unwrap();
        write_varint(&mut f, ms.len() as i64);
        for m in ms.iter() {
            let (x, y, z, ..) = { let p = m.pos.lock().unwrap(); *p };
            write_varint(&mut f, m.etype);
            f.extend_from_slice(&x.to_be_bytes());
            f.extend_from_slice(&y.to_be_bytes());
            f.extend_from_slice(&z.to_be_bytes());
            f.extend_from_slice(&{ let h = m.hp.lock().unwrap(); *h }.to_be_bytes());
        }
    }
    f.extend_from_slice(&WORLD_TIME.load(Ordering::Relaxed).to_be_bytes());
    {
        let mut all: HashMap<String, HashMap<i16, (i64, u8)>> = inv_store().lock().unwrap().clone();
        for c in clients().lock().unwrap().iter() {
            all.insert(c.name.clone(), c.inv.lock().unwrap().clone());
        }
        write_varint(&mut f, all.len() as i64);
        for (name, inv) in all.iter() {
            f.extend_from_slice(&(name.len() as u16).to_be_bytes());
            f.extend_from_slice(name.as_bytes());
            write_varint(&mut f, inv.len() as i64);
            for (&slot, &(item, cnt)) in inv.iter() {
                f.extend_from_slice(&slot.to_be_bytes());
                write_varint(&mut f, item);
                f.push(cnt);
            }
        }
        let ch = chests().lock().unwrap();
        write_varint(&mut f, ch.len() as i64);
        for (&(x, y, z), slots) in ch.iter() {
            f.extend_from_slice(&x.to_be_bytes());
            f.extend_from_slice(&y.to_be_bytes());
            f.extend_from_slice(&z.to_be_bytes());
            write_varint(&mut f, slots.len() as i64);
            for (&slot, &(item, cnt)) in slots.iter() {
                f.extend_from_slice(&slot.to_be_bytes());
                write_varint(&mut f, item);
                f.push(cnt);
            }
        }
    }
    let _ = std::fs::write(WORLD_FILE, &f);
}
fn load_world() {
    let Ok(b) = std::fs::read(WORLD_FILE) else { return };
    let mut p = 0usize;
    let rv = |b: &[u8], p: &mut usize| -> i64 {
        let mut v = 0i64; let mut s = 0;
        loop { let x = b[*p]; *p += 1; v |= ((x & 0x7f) as i64) << s; if x & 0x80 == 0 { break } s += 7; }
        v
    };
    let n = rv(&b, &mut p);
    let mut w = world().lock().unwrap();
    for _ in 0..n {
        let cx = i32::from_be_bytes(b[p..p+4].try_into().unwrap()); p += 4;
        let cz = i32::from_be_bytes(b[p..p+4].try_into().unwrap()); p += 4;
        let m = rv(&b, &mut p);
        let c = w.entry((cx, cz)).or_default();
        for _ in 0..m {
            let idx = u32::from_be_bytes(b[p..p+4].try_into().unwrap()); p += 4;
            let st = rv(&b, &mut p) as u32;
            c.insert(idx, st);
        }
    }
    eprintln!("[spine] loaded {} dirty chunks", w.len());
    // optional META tail
    if p + 4 <= b.len() && &b[p..p+4] == b"MET2" {
        p += 4;
        let mn = rv(&b, &mut p);
        let mut ms = mobs().lock().unwrap();
        for _ in 0..mn {
            let et = rv(&b, &mut p);
            let x = f64::from_be_bytes(b[p..p+8].try_into().unwrap()); p += 8;
            let y = f64::from_be_bytes(b[p..p+8].try_into().unwrap()); p += 8;
            let z = f64::from_be_bytes(b[p..p+8].try_into().unwrap()); p += 8;
            let hp = f32::from_be_bytes(b[p..p+4].try_into().unwrap()); p += 4;
            ms.push(Arc::new(Mob { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), uuid: [0u8;16],
                etype: et, pos: Mutex::new((x, y, z, 0.0, 0.0)), hp: Mutex::new(hp),
                last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
                fuse: Mutex::new(0), love: Mutex::new(0) }));
        }
        drop(ms);
        if p + 8 <= b.len() {
            WORLD_TIME.store(u64::from_be_bytes(b[p..p+8].try_into().unwrap()), Ordering::Relaxed); p += 8;
        }
        if p < b.len() {
            let pn = rv(&b, &mut p);
            let mut store = inv_store().lock().unwrap();
            for _ in 0..pn {
                let nl = u16::from_be_bytes(b[p..p+2].try_into().unwrap()) as usize; p += 2;
                let name = String::from_utf8_lossy(&b[p..p+nl]).to_string(); p += nl;
                let en = rv(&b, &mut p);
                let mut inv = HashMap::new();
                for _ in 0..en {
                    let slot = i16::from_be_bytes(b[p..p+2].try_into().unwrap()); p += 2;
                    let item = rv(&b, &mut p);
                    let cnt = b[p]; p += 1;
                    inv.insert(slot, (item, cnt));
                }
                store.insert(name, inv);
            }
        }
        if p < b.len() {
            let cn = rv(&b, &mut p);
            let mut ch = chests().lock().unwrap();
            for _ in 0..cn {
                let x = i32::from_be_bytes(b[p..p+4].try_into().unwrap()); p += 4;
                let y = i32::from_be_bytes(b[p..p+4].try_into().unwrap()); p += 4;
                let z = i32::from_be_bytes(b[p..p+4].try_into().unwrap()); p += 4;
                let en = rv(&b, &mut p);
                let mut slots = HashMap::new();
                for _ in 0..en {
                    let slot = i16::from_be_bytes(b[p..p+2].try_into().unwrap()); p += 2;
                    let item = rv(&b, &mut p);
                    let cnt = b[p]; p += 1;
                    slots.insert(slot, (item, cnt));
                }
                ch.insert((x, y, z), slots);
            }
        }
        eprintln!("[spine] loaded meta: {} mobs, {} invs, {} chests", mn, inv_store().lock().unwrap().len(), chests().lock().unwrap().len());
    }
}

// ---------- name -> id tables (generated from minecraft-data 1.21.1) ----------
fn state_from_arg(arg: &str) -> Option<u32> {
    let (name, props) = match arg.split_once('[') {
        Some((n, rest)) => (n, rest.trim_end_matches(']').to_string()),
        None => return name_to_state(arg),
    };
    let base = name_to_state(name)?;
    let get = |k: &str| -> Option<String> {
        props.split(',').find_map(|kv| kv.trim().strip_prefix(k).and_then(|v| v.strip_prefix('=').map(|s| s.to_string())))
    };
    if name == "piston" || name == "sticky_piston" {
        let lo = if name == "piston" { 2011u32 } else { 1992u32 };
        if let Some(f) = get("facing") {
            let fi = ["north","east","south","west","up","down"].iter().position(|&v| v == f).unwrap_or(0) as u32;
            return Some(lo + 6 + fi); // extended=false
        }
    }
    if name == "lever" {
        let mut face = 1u32; let mut face_i = 0u32; let mut pow = 1u32; // defaults: wall,north,unpowered
        if let Some(v) = get("face") { face_i = ["floor","wall","ceiling"].iter().position(|&x| x == v).unwrap_or(1) as u32; }
        if let Some(v) = get("facing") { face = ["north","south","west","east"].iter().position(|&x| x == v).unwrap_or(0) as u32; }
        if let Some(v) = get("powered") { pow = if v == "true" { 0 } else { 1 }; }
        return Some(5626 + face_i * 8 + face * 2 + pow);
    }
    if let Some(b) = door_base(base) {
        let mut st = base;
        if let Some(v) = get("open") { st = if v == "true" { st & !2 } else { st | 2 }; }
        if let Some(v) = get("half") { st = if v == "upper" { (st - b) & !8 | 8 + b } else { (st - b) & !8 | b }; }
        return Some(st);
    }
    Some(base)
}
fn name_to_state(n: &str) -> Option<u32> {
    let n = n.strip_prefix("minecraft:").unwrap_or(n);
    Some(match n {
        "air" => 0,
        "stone" => 1,
        "granite" => 2,
        "polished_granite" => 3,
        "diorite" => 4,
        "polished_diorite" => 5,
        "andesite" => 6,
        "polished_andesite" => 7,
        "grass_block" => 9,
        "dirt" => 10,
        "coarse_dirt" => 11,
        "podzol" => 13,
        "cobblestone" => 14,
        "oak_planks" => 15,
        "spruce_planks" => 16,
        "birch_planks" => 17,
        "jungle_planks" => 18,
        "acacia_planks" => 19,
        "cherry_planks" => 20,
        "dark_oak_planks" => 21,
        "mangrove_planks" => 22,
        "bamboo_planks" => 23,
        "bamboo_mosaic" => 24,
        "oak_sapling" => 25,
        "spruce_sapling" => 27,
        "birch_sapling" => 29,
        "jungle_sapling" => 31,
        "acacia_sapling" => 33,
        "cherry_sapling" => 35,
        "dark_oak_sapling" => 37,
        "mangrove_propagule" => 44,
        "bedrock" => 79,
        "water" => 80,
        "lava" => 96,
        "sand" => 112,
        "suspicious_sand" => 113,
        "red_sand" => 117,
        "gravel" => 118,
        "suspicious_gravel" => 119,
        "gold_ore" => 123,
        "deepslate_gold_ore" => 124,
        "iron_ore" => 125,
        "deepslate_iron_ore" => 126,
        "coal_ore" => 127,
        "deepslate_coal_ore" => 128,
        "nether_gold_ore" => 129,
        "oak_log" => 131,
        "spruce_log" => 134,
        "birch_log" => 137,
        "jungle_log" => 140,
        "acacia_log" => 143,
        "cherry_log" => 146,
        "dark_oak_log" => 149,
        "mangrove_log" => 152,
        "mangrove_roots" => 155,
        "muddy_mangrove_roots" => 157,
        "bamboo_block" => 160,
        "stripped_spruce_log" => 163,
        "stripped_birch_log" => 166,
        "stripped_jungle_log" => 169,
        "stripped_acacia_log" => 172,
        "stripped_cherry_log" => 175,
        "stripped_dark_oak_log" => 178,
        "stripped_oak_log" => 181,
        "stripped_mangrove_log" => 184,
        "stripped_bamboo_block" => 187,
        "oak_wood" => 190,
        "spruce_wood" => 193,
        "birch_wood" => 196,
        "jungle_wood" => 199,
        "acacia_wood" => 202,
        "cherry_wood" => 205,
        "dark_oak_wood" => 208,
        "mangrove_wood" => 211,
        "stripped_oak_wood" => 214,
        "stripped_spruce_wood" => 217,
        "stripped_birch_wood" => 220,
        "stripped_jungle_wood" => 223,
        "stripped_acacia_wood" => 226,
        "stripped_cherry_wood" => 229,
        "stripped_dark_oak_wood" => 232,
        "stripped_mangrove_wood" => 235,
        "oak_leaves" => 264,
        "spruce_leaves" => 292,
        "birch_leaves" => 320,
        "jungle_leaves" => 348,
        "acacia_leaves" => 376,
        "cherry_leaves" => 404,
        "dark_oak_leaves" => 432,
        "mangrove_leaves" => 460,
        "azalea_leaves" => 488,
        "flowering_azalea_leaves" => 516,
        "sponge" => 517,
        "wet_sponge" => 518,
        "glass" => 519,
        "lapis_ore" => 520,
        "deepslate_lapis_ore" => 521,
        "lapis_block" => 522,
        "dispenser" => 524,
        "sandstone" => 535,
        "chiseled_sandstone" => 536,
        "cut_sandstone" => 537,
        "note_block" => 539,
        "white_bed" => 1691,
        "orange_bed" => 1707,
        "magenta_bed" => 1723,
        "light_blue_bed" => 1739,
        "yellow_bed" => 1755,
        "lime_bed" => 1771,
        "pink_bed" => 1787,
        "gray_bed" => 1803,
        "light_gray_bed" => 1819,
        "cyan_bed" => 1835,
        "purple_bed" => 1851,
        "blue_bed" => 1867,
        "brown_bed" => 1883,
        "green_bed" => 1899,
        "red_bed" => 1915,
        "black_bed" => 1931,
        "powered_rail" => 1957,
        "detector_rail" => 1981,
        "sticky_piston" => 1998,
        "cobweb" => 2004,
        "short_grass" => 2005,
        "fern" => 2006,
        "dead_bush" => 2007,
        "seagrass" => 2008,
        "tall_seagrass" => 2010,
        "piston" => 2017,
        "piston_head" => 2025,
        "white_wool" => 2047,
        "orange_wool" => 2048,
        "magenta_wool" => 2049,
        "light_blue_wool" => 2050,
        "yellow_wool" => 2051,
        "lime_wool" => 2052,
        "pink_wool" => 2053,
        "gray_wool" => 2054,
        "light_gray_wool" => 2055,
        "cyan_wool" => 2056,
        "purple_wool" => 2057,
        "blue_wool" => 2058,
        "brown_wool" => 2059,
        "green_wool" => 2060,
        "red_wool" => 2061,
        "black_wool" => 2062,
        "moving_piston" => 2063,
        "dandelion" => 2075,
        "torchflower" => 2076,
        "poppy" => 2077,
        "blue_orchid" => 2078,
        "allium" => 2079,
        "azure_bluet" => 2080,
        "red_tulip" => 2081,
        "orange_tulip" => 2082,
        "white_tulip" => 2083,
        "pink_tulip" => 2084,
        "oxeye_daisy" => 2085,
        "cornflower" => 2086,
        "wither_rose" => 2087,
        "lily_of_the_valley" => 2088,
        "brown_mushroom" => 2089,
        "red_mushroom" => 2090,
        "gold_block" => 2091,
        "iron_block" => 2092,
        "bricks" => 2093,
        "tnt" => 2095,
        "bookshelf" => 2096,
        "chiseled_bookshelf" => 2160,
        "mossy_cobblestone" => 2353,
        "obsidian" => 2354,
        "torch" => 2355,
        "wall_torch" => 2356,
        "fire" => 2391,
        "soul_fire" => 2872,
        "spawner" => 2873,
        "oak_stairs" => 2885,
        "chest" => 2955,
        "redstone_wire" => 4138,
        "diamond_ore" => 4274,
        "deepslate_diamond_ore" => 4275,
        "diamond_block" => 4276,
        "crafting_table" => 4277,
        "wheat" => 4278,
        "farmland" => 4286,
        "furnace" => 4295,
        "oak_sign" => 4303,
        "spruce_sign" => 4335,
        "birch_sign" => 4367,
        "acacia_sign" => 4399,
        "cherry_sign" => 4431,
        "jungle_sign" => 4463,
        "dark_oak_sign" => 4495,
        "mangrove_sign" => 4527,
        "bamboo_sign" => 4559,
        "oak_door" => 4601,
        "ladder" => 4655,
        "rail" => 4663,
        "cobblestone_stairs" => 4693,
        "oak_wall_sign" => 4763,
        "spruce_wall_sign" => 4771,
        "birch_wall_sign" => 4779,
        "acacia_wall_sign" => 4787,
        "cherry_wall_sign" => 4795,
        "jungle_wall_sign" => 4803,
        "dark_oak_wall_sign" => 4811,
        "mangrove_wall_sign" => 4819,
        "bamboo_wall_sign" => 4827,
        "oak_hanging_sign" => 4867,
        "spruce_hanging_sign" => 4931,
        "birch_hanging_sign" => 4995,
        "acacia_hanging_sign" => 5059,
        "cherry_hanging_sign" => 5123,
        "jungle_hanging_sign" => 5187,
        "dark_oak_hanging_sign" => 5251,
        "crimson_hanging_sign" => 5315,
        "warped_hanging_sign" => 5379,
        "mangrove_hanging_sign" => 5443,
        "bamboo_hanging_sign" => 5507,
        "oak_wall_hanging_sign" => 5539,
        "spruce_wall_hanging_sign" => 5547,
        "birch_wall_hanging_sign" => 5555,
        "acacia_wall_hanging_sign" => 5563,
        "cherry_wall_hanging_sign" => 5571,
        "jungle_wall_hanging_sign" => 5579,
        "dark_oak_wall_hanging_sign" => 5587,
        "mangrove_wall_hanging_sign" => 5595,
        "crimson_wall_hanging_sign" => 5603,
        "warped_wall_hanging_sign" => 5611,
        "bamboo_wall_hanging_sign" => 5619,
        "lever" => 5635,
        "stone_pressure_plate" => 5651,
        "iron_door" => 5663,
        "oak_pressure_plate" => 5717,
        "spruce_pressure_plate" => 5719,
        "birch_pressure_plate" => 5721,
        "jungle_pressure_plate" => 5723,
        "acacia_pressure_plate" => 5725,
        "cherry_pressure_plate" => 5727,
        "dark_oak_pressure_plate" => 5729,
        "mangrove_pressure_plate" => 5731,
        "bamboo_pressure_plate" => 5733,
        "redstone_ore" => 5735,
        "deepslate_redstone_ore" => 5737,
        "redstone_torch" => 5738,
        "redstone_wall_torch" => 5740,
        "stone_button" => 5757,
        "snow" => 5772,
        "ice" => 5780,
        "snow_block" => 5781,
        "cactus" => 5782,
        "clay" => 5798,
        "sugar_cane" => 5799,
        "jukebox" => 5816,
        "oak_fence" => 5848,
        "netherrack" => 5849,
        "soul_sand" => 5850,
        "soul_soil" => 5851,
        "basalt" => 5853,
        "polished_basalt" => 5856,
        "soul_torch" => 5858,
        "soul_wall_torch" => 5859,
        "glowstone" => 5863,
        "nether_portal" => 5864,
        "carved_pumpkin" => 5866,
        "jack_o_lantern" => 5870,
        "cake" => 5874,
        "repeater" => 5884,
        "white_stained_glass" => 5945,
        "orange_stained_glass" => 5946,
        "magenta_stained_glass" => 5947,
        "light_blue_stained_glass" => 5948,
        "yellow_stained_glass" => 5949,
        "lime_stained_glass" => 5950,
        "pink_stained_glass" => 5951,
        "gray_stained_glass" => 5952,
        "light_gray_stained_glass" => 5953,
        "cyan_stained_glass" => 5954,
        "purple_stained_glass" => 5955,
        "blue_stained_glass" => 5956,
        "brown_stained_glass" => 5957,
        "green_stained_glass" => 5958,
        "red_stained_glass" => 5959,
        "black_stained_glass" => 5960,
        "oak_trapdoor" => 5976,
        "spruce_trapdoor" => 6040,
        "birch_trapdoor" => 6104,
        "jungle_trapdoor" => 6168,
        "acacia_trapdoor" => 6232,
        "cherry_trapdoor" => 6296,
        "dark_oak_trapdoor" => 6360,
        "mangrove_trapdoor" => 6424,
        "bamboo_trapdoor" => 6488,
        "stone_bricks" => 6537,
        "mossy_stone_bricks" => 6538,
        "cracked_stone_bricks" => 6539,
        "chiseled_stone_bricks" => 6540,
        "packed_mud" => 6541,
        "mud_bricks" => 6542,
        "infested_stone" => 6543,
        "infested_cobblestone" => 6544,
        "infested_stone_bricks" => 6545,
        "infested_mossy_stone_bricks" => 6546,
        "infested_cracked_stone_bricks" => 6547,
        "infested_chiseled_stone_bricks" => 6548,
        "brown_mushroom_block" => 6549,
        "red_mushroom_block" => 6613,
        "mushroom_stem" => 6677,
        "iron_bars" => 6772,
        "chain" => 6776,
        "glass_pane" => 6810,
        "pumpkin" => 6811,
        "melon" => 6812,
        "attached_pumpkin_stem" => 6813,
        "attached_melon_stem" => 6817,
        "pumpkin_stem" => 6821,
        "melon_stem" => 6829,
        "vine" => 6868,
        "glow_lichen" => 6996,
        "oak_fence_gate" => 7004,
        "brick_stairs" => 7040,
        "stone_brick_stairs" => 7120,
        "mud_brick_stairs" => 7200,
        "mycelium" => 7270,
        "lily_pad" => 7271,
        "nether_bricks" => 7272,
        "nether_brick_fence" => 7304,
        "nether_brick_stairs" => 7316,
        "nether_wart" => 7385,
        "enchanting_table" => 7389,
        "brewing_stand" => 7397,
        "cauldron" => 7398,
        "water_cauldron" => 7399,
        "lava_cauldron" => 7402,
        "powder_snow_cauldron" => 7403,
        "end_portal" => 7406,
        "end_portal_frame" => 7411,
        "end_stone" => 7415,
        "dragon_egg" => 7416,
        "redstone_lamp" => 7418,
        "cocoa" => 7419,
        "sandstone_stairs" => 7442,
        "emerald_ore" => 7511,
        "deepslate_emerald_ore" => 7512,
        "ender_chest" => 7514,
        "tripwire_hook" => 7530,
        "tripwire" => 7664,
        "emerald_block" => 7665,
        "spruce_stairs" => 7677,
        "birch_stairs" => 7757,
        "jungle_stairs" => 7837,
        "command_block" => 7912,
        "beacon" => 7918,
        "cobblestone_wall" => 7922,
        "mossy_cobblestone_wall" => 8246,
        "flower_pot" => 8567,
        "potted_torchflower" => 8568,
        "potted_oak_sapling" => 8569,
        "potted_spruce_sapling" => 8570,
        "potted_birch_sapling" => 8571,
        "potted_jungle_sapling" => 8572,
        "potted_acacia_sapling" => 8573,
        "potted_cherry_sapling" => 8574,
        "potted_dark_oak_sapling" => 8575,
        "potted_mangrove_propagule" => 8576,
        "potted_fern" => 8577,
        "potted_dandelion" => 8578,
        "potted_poppy" => 8579,
        "potted_blue_orchid" => 8580,
        "potted_allium" => 8581,
        "potted_azure_bluet" => 8582,
        "potted_red_tulip" => 8583,
        "potted_orange_tulip" => 8584,
        "potted_white_tulip" => 8585,
        "potted_pink_tulip" => 8586,
        "potted_oxeye_daisy" => 8587,
        "potted_cornflower" => 8588,
        "potted_lily_of_the_valley" => 8589,
        "potted_wither_rose" => 8590,
        "potted_red_mushroom" => 8591,
        "potted_brown_mushroom" => 8592,
        "potted_dead_bush" => 8593,
        "potted_cactus" => 8594,
        "carrots" => 8595,
        "potatoes" => 8603,
        "oak_button" => 8620,
        "spruce_button" => 8644,
        "birch_button" => 8668,
        "jungle_button" => 8692,
        "acacia_button" => 8716,
        "cherry_button" => 8740,
        "dark_oak_button" => 8764,
        "mangrove_button" => 8788,
        "bamboo_button" => 8812,
        "skeleton_skull" => 8843,
        "skeleton_wall_skull" => 8860,
        "wither_skeleton_skull" => 8883,
        "wither_skeleton_wall_skull" => 8900,
        "zombie_head" => 8923,
        "zombie_wall_head" => 8940,
        "player_head" => 8963,
        "player_wall_head" => 8980,
        "creeper_head" => 9003,
        "creeper_wall_head" => 9020,
        "dragon_head" => 9043,
        "dragon_wall_head" => 9060,
        "piglin_head" => 9083,
        "piglin_wall_head" => 9100,
        "anvil" => 9107,
        "chipped_anvil" => 9111,
        "damaged_anvil" => 9115,
        "trapped_chest" => 9120,
        "light_weighted_pressure_plate" => 9143,
        "heavy_weighted_pressure_plate" => 9159,
        "comparator" => 9176,
        "daylight_detector" => 9207,
        "redstone_block" => 9223,
        "nether_quartz_ore" => 9224,
        "hopper" => 9225,
        "quartz_block" => 9235,
        "chiseled_quartz_block" => 9236,
        "quartz_pillar" => 9238,
        "quartz_stairs" => 9251,
        "activator_rail" => 9333,
        "dropper" => 9345,
        "white_terracotta" => 9356,
        "orange_terracotta" => 9357,
        "magenta_terracotta" => 9358,
        "light_blue_terracotta" => 9359,
        "yellow_terracotta" => 9360,
        "lime_terracotta" => 9361,
        "pink_terracotta" => 9362,
        "gray_terracotta" => 9363,
        "light_gray_terracotta" => 9364,
        "cyan_terracotta" => 9365,
        "purple_terracotta" => 9366,
        "blue_terracotta" => 9367,
        "brown_terracotta" => 9368,
        "green_terracotta" => 9369,
        "red_terracotta" => 9370,
        "black_terracotta" => 9371,
        "white_stained_glass_pane" => 9403,
        "orange_stained_glass_pane" => 9435,
        "magenta_stained_glass_pane" => 9467,
        "light_blue_stained_glass_pane" => 9499,
        "yellow_stained_glass_pane" => 9531,
        "lime_stained_glass_pane" => 9563,
        "pink_stained_glass_pane" => 9595,
        "gray_stained_glass_pane" => 9627,
        "light_gray_stained_glass_pane" => 9659,
        "cyan_stained_glass_pane" => 9691,
        "purple_stained_glass_pane" => 9723,
        "blue_stained_glass_pane" => 9755,
        "brown_stained_glass_pane" => 9787,
        "green_stained_glass_pane" => 9819,
        "red_stained_glass_pane" => 9851,
        "black_stained_glass_pane" => 9883,
        "acacia_stairs" => 9895,
        "cherry_stairs" => 9975,
        "dark_oak_stairs" => 10055,
        "mangrove_stairs" => 10135,
        "bamboo_stairs" => 10215,
        "bamboo_mosaic_stairs" => 10295,
        "slime_block" => 10364,
        "barrier" => 10366,
        "light" => 10398,
        "iron_trapdoor" => 10414,
        "prismarine" => 10463,
        "prismarine_bricks" => 10464,
        "dark_prismarine" => 10465,
        "prismarine_stairs" => 10477,
        "prismarine_brick_stairs" => 10557,
        "dark_prismarine_stairs" => 10637,
        "prismarine_slab" => 10709,
        "prismarine_brick_slab" => 10715,
        "dark_prismarine_slab" => 10721,
        "sea_lantern" => 10724,
        "hay_block" => 10726,
        "white_carpet" => 10728,
        "orange_carpet" => 10729,
        "magenta_carpet" => 10730,
        "light_blue_carpet" => 10731,
        "yellow_carpet" => 10732,
        "lime_carpet" => 10733,
        "pink_carpet" => 10734,
        "gray_carpet" => 10735,
        "light_gray_carpet" => 10736,
        "cyan_carpet" => 10737,
        "purple_carpet" => 10738,
        "blue_carpet" => 10739,
        "brown_carpet" => 10740,
        "green_carpet" => 10741,
        "red_carpet" => 10742,
        "black_carpet" => 10743,
        "terracotta" => 10744,
        "coal_block" => 10745,
        "packed_ice" => 10746,
        "sunflower" => 10748,
        "lilac" => 10750,
        "rose_bush" => 10752,
        "peony" => 10754,
        "tall_grass" => 10756,
        "large_fern" => 10758,
        "white_banner" => 10759,
        "orange_banner" => 10775,
        "magenta_banner" => 10791,
        "light_blue_banner" => 10807,
        "yellow_banner" => 10823,
        "lime_banner" => 10839,
        "pink_banner" => 10855,
        "gray_banner" => 10871,
        "light_gray_banner" => 10887,
        "cyan_banner" => 10903,
        "purple_banner" => 10919,
        "blue_banner" => 10935,
        "brown_banner" => 10951,
        "green_banner" => 10967,
        "red_banner" => 10983,
        "black_banner" => 10999,
        "white_wall_banner" => 11015,
        "orange_wall_banner" => 11019,
        "magenta_wall_banner" => 11023,
        "light_blue_wall_banner" => 11027,
        "yellow_wall_banner" => 11031,
        "lime_wall_banner" => 11035,
        "pink_wall_banner" => 11039,
        "gray_wall_banner" => 11043,
        "light_gray_wall_banner" => 11047,
        "cyan_wall_banner" => 11051,
        "purple_wall_banner" => 11055,
        "blue_wall_banner" => 11059,
        "brown_wall_banner" => 11063,
        "green_wall_banner" => 11067,
        "red_wall_banner" => 11071,
        "black_wall_banner" => 11075,
        "red_sandstone" => 11079,
        "chiseled_red_sandstone" => 11080,
        "cut_red_sandstone" => 11081,
        "red_sandstone_stairs" => 11093,
        "oak_slab" => 11165,
        "spruce_slab" => 11171,
        "birch_slab" => 11177,
        "jungle_slab" => 11183,
        "acacia_slab" => 11189,
        "cherry_slab" => 11195,
        "dark_oak_slab" => 11201,
        "mangrove_slab" => 11207,
        "bamboo_slab" => 11213,
        "bamboo_mosaic_slab" => 11219,
        "stone_slab" => 11225,
        "smooth_stone_slab" => 11231,
        "sandstone_slab" => 11237,
        "cut_sandstone_slab" => 11243,
        "petrified_oak_slab" => 11249,
        "cobblestone_slab" => 11255,
        "brick_slab" => 11261,
        "stone_brick_slab" => 11267,
        "mud_brick_slab" => 11273,
        "nether_brick_slab" => 11279,
        "quartz_slab" => 11285,
        "red_sandstone_slab" => 11291,
        "cut_red_sandstone_slab" => 11297,
        "purpur_slab" => 11303,
        "smooth_stone" => 11306,
        "smooth_sandstone" => 11307,
        "smooth_quartz" => 11308,
        "smooth_red_sandstone" => 11309,
        "spruce_fence_gate" => 11317,
        "birch_fence_gate" => 11349,
        "jungle_fence_gate" => 11381,
        "acacia_fence_gate" => 11413,
        "cherry_fence_gate" => 11445,
        "dark_oak_fence_gate" => 11477,
        "mangrove_fence_gate" => 11509,
        "bamboo_fence_gate" => 11541,
        "spruce_fence" => 11597,
        "birch_fence" => 11629,
        "jungle_fence" => 11661,
        "acacia_fence" => 11693,
        "cherry_fence" => 11725,
        "dark_oak_fence" => 11757,
        "mangrove_fence" => 11789,
        "bamboo_fence" => 11821,
        "spruce_door" => 11833,
        "birch_door" => 11897,
        "jungle_door" => 11961,
        "acacia_door" => 12025,
        "cherry_door" => 12089,
        "dark_oak_door" => 12153,
        "mangrove_door" => 12217,
        "bamboo_door" => 12281,
        "end_rod" => 12338,
        "chorus_plant" => 12403,
        "chorus_flower" => 12404,
        "purpur_block" => 12410,
        "purpur_pillar" => 12412,
        "purpur_stairs" => 12425,
        "end_stone_bricks" => 12494,
        "torchflower_crop" => 12495,
        "pitcher_crop" => 12498,
        "pitcher_plant" => 12508,
        "beetroots" => 12509,
        "dirt_path" => 12513,
        "end_gateway" => 12514,
        "repeating_command_block" => 12521,
        "chain_command_block" => 12533,
        "frosted_ice" => 12539,
        "magma_block" => 12543,
        "nether_wart_block" => 12544,
        "red_nether_bricks" => 12545,
        "bone_block" => 12547,
        "structure_void" => 12549,
        "observer" => 12555,
        "shulker_box" => 12566,
        "white_shulker_box" => 12572,
        "orange_shulker_box" => 12578,
        "magenta_shulker_box" => 12584,
        "light_blue_shulker_box" => 12590,
        "yellow_shulker_box" => 12596,
        "lime_shulker_box" => 12602,
        "pink_shulker_box" => 12608,
        "gray_shulker_box" => 12614,
        "light_gray_shulker_box" => 12620,
        "cyan_shulker_box" => 12626,
        "purple_shulker_box" => 12632,
        "blue_shulker_box" => 12638,
        "brown_shulker_box" => 12644,
        "green_shulker_box" => 12650,
        "red_shulker_box" => 12656,
        "black_shulker_box" => 12662,
        "white_glazed_terracotta" => 12664,
        "orange_glazed_terracotta" => 12668,
        "magenta_glazed_terracotta" => 12672,
        "light_blue_glazed_terracotta" => 12676,
        "yellow_glazed_terracotta" => 12680,
        "lime_glazed_terracotta" => 12684,
        "pink_glazed_terracotta" => 12688,
        "gray_glazed_terracotta" => 12692,
        "light_gray_glazed_terracotta" => 12696,
        "cyan_glazed_terracotta" => 12700,
        "purple_glazed_terracotta" => 12704,
        "blue_glazed_terracotta" => 12708,
        "brown_glazed_terracotta" => 12712,
        "green_glazed_terracotta" => 12716,
        "red_glazed_terracotta" => 12720,
        "black_glazed_terracotta" => 12724,
        "white_concrete" => 12728,
        "orange_concrete" => 12729,
        "magenta_concrete" => 12730,
        "light_blue_concrete" => 12731,
        "yellow_concrete" => 12732,
        "lime_concrete" => 12733,
        "pink_concrete" => 12734,
        "gray_concrete" => 12735,
        "light_gray_concrete" => 12736,
        "cyan_concrete" => 12737,
        "purple_concrete" => 12738,
        "blue_concrete" => 12739,
        "brown_concrete" => 12740,
        "green_concrete" => 12741,
        "red_concrete" => 12742,
        "black_concrete" => 12743,
        "white_concrete_powder" => 12744,
        "orange_concrete_powder" => 12745,
        "magenta_concrete_powder" => 12746,
        "light_blue_concrete_powder" => 12747,
        "yellow_concrete_powder" => 12748,
        "lime_concrete_powder" => 12749,
        "pink_concrete_powder" => 12750,
        "gray_concrete_powder" => 12751,
        "light_gray_concrete_powder" => 12752,
        "cyan_concrete_powder" => 12753,
        "purple_concrete_powder" => 12754,
        "blue_concrete_powder" => 12755,
        "brown_concrete_powder" => 12756,
        "green_concrete_powder" => 12757,
        "red_concrete_powder" => 12758,
        "black_concrete_powder" => 12759,
        "kelp" => 12760,
        "kelp_plant" => 12786,
        "dried_kelp_block" => 12787,
        "turtle_egg" => 12788,
        "sniffer_egg" => 12800,
        "dead_tube_coral_block" => 12803,
        "dead_brain_coral_block" => 12804,
        "dead_bubble_coral_block" => 12805,
        "dead_fire_coral_block" => 12806,
        "dead_horn_coral_block" => 12807,
        "tube_coral_block" => 12808,
        "brain_coral_block" => 12809,
        "bubble_coral_block" => 12810,
        "fire_coral_block" => 12811,
        "horn_coral_block" => 12812,
        "dead_tube_coral" => 12813,
        "dead_brain_coral" => 12815,
        "dead_bubble_coral" => 12817,
        "dead_fire_coral" => 12819,
        "dead_horn_coral" => 12821,
        "tube_coral" => 12823,
        "brain_coral" => 12825,
        "bubble_coral" => 12827,
        "fire_coral" => 12829,
        "horn_coral" => 12831,
        "dead_tube_coral_fan" => 12833,
        "dead_brain_coral_fan" => 12835,
        "dead_bubble_coral_fan" => 12837,
        "dead_fire_coral_fan" => 12839,
        "dead_horn_coral_fan" => 12841,
        "tube_coral_fan" => 12843,
        "brain_coral_fan" => 12845,
        "bubble_coral_fan" => 12847,
        "fire_coral_fan" => 12849,
        "horn_coral_fan" => 12851,
        "dead_tube_coral_wall_fan" => 12853,
        "dead_brain_coral_wall_fan" => 12861,
        "dead_bubble_coral_wall_fan" => 12869,
        "dead_fire_coral_wall_fan" => 12877,
        "dead_horn_coral_wall_fan" => 12885,
        "tube_coral_wall_fan" => 12893,
        "brain_coral_wall_fan" => 12901,
        "bubble_coral_wall_fan" => 12909,
        "fire_coral_wall_fan" => 12917,
        "horn_coral_wall_fan" => 12925,
        "sea_pickle" => 12933,
        "blue_ice" => 12941,
        "conduit" => 12942,
        "bamboo_sapling" => 12944,
        "bamboo" => 12945,
        "potted_bamboo" => 12957,
        "void_air" => 12958,
        "cave_air" => 12959,
        "bubble_column" => 12960,
        "polished_granite_stairs" => 12973,
        "smooth_red_sandstone_stairs" => 13053,
        "mossy_stone_brick_stairs" => 13133,
        "polished_diorite_stairs" => 13213,
        "mossy_cobblestone_stairs" => 13293,
        "end_stone_brick_stairs" => 13373,
        "stone_stairs" => 13453,
        "smooth_sandstone_stairs" => 13533,
        "smooth_quartz_stairs" => 13613,
        "granite_stairs" => 13693,
        "andesite_stairs" => 13773,
        "red_nether_brick_stairs" => 13853,
        "polished_andesite_stairs" => 13933,
        "diorite_stairs" => 14013,
        "polished_granite_slab" => 14085,
        "smooth_red_sandstone_slab" => 14091,
        "mossy_stone_brick_slab" => 14097,
        "polished_diorite_slab" => 14103,
        "mossy_cobblestone_slab" => 14109,
        "end_stone_brick_slab" => 14115,
        "smooth_sandstone_slab" => 14121,
        "smooth_quartz_slab" => 14127,
        "granite_slab" => 14133,
        "andesite_slab" => 14139,
        "red_nether_brick_slab" => 14145,
        "polished_andesite_slab" => 14151,
        "diorite_slab" => 14157,
        "brick_wall" => 14163,
        "prismarine_wall" => 14487,
        "red_sandstone_wall" => 14811,
        "mossy_stone_brick_wall" => 15135,
        "granite_wall" => 15459,
        "stone_brick_wall" => 15783,
        "mud_brick_wall" => 16107,
        "nether_brick_wall" => 16431,
        "andesite_wall" => 16755,
        "red_nether_brick_wall" => 17079,
        "sandstone_wall" => 17403,
        "end_stone_brick_wall" => 17727,
        "diorite_wall" => 18051,
        "scaffolding" => 18403,
        "loom" => 18404,
        "barrel" => 18409,
        "smoker" => 18421,
        "blast_furnace" => 18429,
        "cartography_table" => 18436,
        "fletching_table" => 18437,
        "grindstone" => 18442,
        "lectern" => 18453,
        "smithing_table" => 18466,
        "stonecutter" => 18467,
        "bell" => 18472,
        "lantern" => 18506,
        "soul_lantern" => 18510,
        "campfire" => 18514,
        "soul_campfire" => 18546,
        "sweet_berry_bush" => 18575,
        "warped_stem" => 18580,
        "stripped_warped_stem" => 18583,
        "warped_hyphae" => 18586,
        "stripped_warped_hyphae" => 18589,
        "warped_nylium" => 18591,
        "warped_fungus" => 18592,
        "warped_wart_block" => 18593,
        "warped_roots" => 18594,
        "nether_sprouts" => 18595,
        "crimson_stem" => 18597,
        "stripped_crimson_stem" => 18600,
        "crimson_hyphae" => 18603,
        "stripped_crimson_hyphae" => 18606,
        "crimson_nylium" => 18608,
        "crimson_fungus" => 18609,
        "shroomlight" => 18610,
        "weeping_vines" => 18611,
        "weeping_vines_plant" => 18637,
        "twisting_vines" => 18638,
        "twisting_vines_plant" => 18664,
        "crimson_roots" => 18665,
        "crimson_planks" => 18666,
        "warped_planks" => 18667,
        "crimson_slab" => 18671,
        "warped_slab" => 18677,
        "crimson_pressure_plate" => 18681,
        "warped_pressure_plate" => 18683,
        "crimson_fence" => 18715,
        "warped_fence" => 18747,
        "crimson_trapdoor" => 18763,
        "warped_trapdoor" => 18827,
        "crimson_fence_gate" => 18883,
        "warped_fence_gate" => 18915,
        "crimson_stairs" => 18951,
        "warped_stairs" => 19031,
        "crimson_button" => 19109,
        "warped_button" => 19133,
        "crimson_door" => 19159,
        "warped_door" => 19223,
        "crimson_sign" => 19277,
        "warped_sign" => 19309,
        "crimson_wall_sign" => 19341,
        "warped_wall_sign" => 19349,
        "structure_block" => 19357,
        "jigsaw" => 19370,
        "composter" => 19372,
        "target" => 19381,
        "bee_nest" => 19397,
        "beehive" => 19421,
        "honey_block" => 19445,
        "honeycomb_block" => 19446,
        "netherite_block" => 19447,
        "ancient_debris" => 19448,
        "crying_obsidian" => 19449,
        "respawn_anchor" => 19450,
        "potted_crimson_fungus" => 19455,
        "potted_warped_fungus" => 19456,
        "potted_crimson_roots" => 19457,
        "potted_warped_roots" => 19458,
        "lodestone" => 19459,
        "blackstone" => 19460,
        "blackstone_stairs" => 19472,
        "blackstone_wall" => 19544,
        "blackstone_slab" => 19868,
        "polished_blackstone" => 19871,
        "polished_blackstone_bricks" => 19872,
        "cracked_polished_blackstone_bricks" => 19873,
        "chiseled_polished_blackstone" => 19874,
        "polished_blackstone_brick_slab" => 19878,
        "polished_blackstone_brick_stairs" => 19892,
        "polished_blackstone_brick_wall" => 19964,
        "gilded_blackstone" => 20285,
        "polished_blackstone_stairs" => 20297,
        "polished_blackstone_slab" => 20369,
        "polished_blackstone_pressure_plate" => 20373,
        "polished_blackstone_button" => 20383,
        "polished_blackstone_wall" => 20401,
        "chiseled_nether_bricks" => 20722,
        "cracked_nether_bricks" => 20723,
        "quartz_bricks" => 20724,
        "candle" => 20728,
        "white_candle" => 20744,
        "orange_candle" => 20760,
        "magenta_candle" => 20776,
        "light_blue_candle" => 20792,
        "yellow_candle" => 20808,
        "lime_candle" => 20824,
        "pink_candle" => 20840,
        "gray_candle" => 20856,
        "light_gray_candle" => 20872,
        "cyan_candle" => 20888,
        "purple_candle" => 20904,
        "blue_candle" => 20920,
        "brown_candle" => 20936,
        "green_candle" => 20952,
        "red_candle" => 20968,
        "black_candle" => 20984,
        "candle_cake" => 20998,
        "white_candle_cake" => 21000,
        "orange_candle_cake" => 21002,
        "magenta_candle_cake" => 21004,
        "light_blue_candle_cake" => 21006,
        "yellow_candle_cake" => 21008,
        "lime_candle_cake" => 21010,
        "pink_candle_cake" => 21012,
        "gray_candle_cake" => 21014,
        "light_gray_candle_cake" => 21016,
        "cyan_candle_cake" => 21018,
        "purple_candle_cake" => 21020,
        "blue_candle_cake" => 21022,
        "brown_candle_cake" => 21024,
        "green_candle_cake" => 21026,
        "red_candle_cake" => 21028,
        "black_candle_cake" => 21030,
        "amethyst_block" => 21031,
        "budding_amethyst" => 21032,
        "amethyst_cluster" => 21042,
        "large_amethyst_bud" => 21054,
        "medium_amethyst_bud" => 21066,
        "small_amethyst_bud" => 21078,
        "tuff" => 21081,
        "tuff_slab" => 21085,
        "tuff_stairs" => 21099,
        "tuff_wall" => 21171,
        "polished_tuff" => 21492,
        "polished_tuff_slab" => 21496,
        "polished_tuff_stairs" => 21510,
        "polished_tuff_wall" => 21582,
        "chiseled_tuff" => 21903,
        "tuff_bricks" => 21904,
        "tuff_brick_slab" => 21908,
        "tuff_brick_stairs" => 21922,
        "tuff_brick_wall" => 21994,
        "chiseled_tuff_bricks" => 22315,
        "calcite" => 22316,
        "tinted_glass" => 22317,
        "powder_snow" => 22318,
        "sculk_sensor" => 22320,
        "calibrated_sculk_sensor" => 22416,
        "sculk" => 22799,
        "sculk_vein" => 22927,
        "sculk_catalyst" => 22929,
        "sculk_shrieker" => 22937,
        "copper_block" => 22938,
        "exposed_copper" => 22939,
        "weathered_copper" => 22940,
        "oxidized_copper" => 22941,
        "copper_ore" => 22942,
        "deepslate_copper_ore" => 22943,
        "oxidized_cut_copper" => 22944,
        "weathered_cut_copper" => 22945,
        "exposed_cut_copper" => 22946,
        "cut_copper" => 22947,
        "oxidized_chiseled_copper" => 22948,
        "weathered_chiseled_copper" => 22949,
        "exposed_chiseled_copper" => 22950,
        "chiseled_copper" => 22951,
        "waxed_oxidized_chiseled_copper" => 22952,
        "waxed_weathered_chiseled_copper" => 22953,
        "waxed_exposed_chiseled_copper" => 22954,
        "waxed_chiseled_copper" => 22955,
        "oxidized_cut_copper_stairs" => 22967,
        "weathered_cut_copper_stairs" => 23047,
        "exposed_cut_copper_stairs" => 23127,
        "cut_copper_stairs" => 23207,
        "oxidized_cut_copper_slab" => 23279,
        "weathered_cut_copper_slab" => 23285,
        "exposed_cut_copper_slab" => 23291,
        "cut_copper_slab" => 23297,
        "waxed_copper_block" => 23300,
        "waxed_weathered_copper" => 23301,
        "waxed_exposed_copper" => 23302,
        "waxed_oxidized_copper" => 23303,
        "waxed_oxidized_cut_copper" => 23304,
        "waxed_weathered_cut_copper" => 23305,
        "waxed_exposed_cut_copper" => 23306,
        "waxed_cut_copper" => 23307,
        "waxed_oxidized_cut_copper_stairs" => 23319,
        "waxed_weathered_cut_copper_stairs" => 23399,
        "waxed_exposed_cut_copper_stairs" => 23479,
        "waxed_cut_copper_stairs" => 23559,
        "waxed_oxidized_cut_copper_slab" => 23631,
        "waxed_weathered_cut_copper_slab" => 23637,
        "waxed_exposed_cut_copper_slab" => 23643,
        "waxed_cut_copper_slab" => 23649,
        "copper_door" => 23663,
        "exposed_copper_door" => 23727,
        "oxidized_copper_door" => 23791,
        "weathered_copper_door" => 23855,
        "waxed_copper_door" => 23919,
        "waxed_exposed_copper_door" => 23983,
        "waxed_oxidized_copper_door" => 24047,
        "waxed_weathered_copper_door" => 24111,
        "copper_trapdoor" => 24179,
        "exposed_copper_trapdoor" => 24243,
        "oxidized_copper_trapdoor" => 24307,
        "weathered_copper_trapdoor" => 24371,
        "waxed_copper_trapdoor" => 24435,
        "waxed_exposed_copper_trapdoor" => 24499,
        "waxed_oxidized_copper_trapdoor" => 24563,
        "waxed_weathered_copper_trapdoor" => 24627,
        "copper_grate" => 24677,
        "exposed_copper_grate" => 24679,
        "weathered_copper_grate" => 24681,
        "oxidized_copper_grate" => 24683,
        "waxed_copper_grate" => 24685,
        "waxed_exposed_copper_grate" => 24687,
        "waxed_weathered_copper_grate" => 24689,
        "waxed_oxidized_copper_grate" => 24691,
        "copper_bulb" => 24695,
        "exposed_copper_bulb" => 24699,
        "weathered_copper_bulb" => 24703,
        "oxidized_copper_bulb" => 24707,
        "waxed_copper_bulb" => 24711,
        "waxed_exposed_copper_bulb" => 24715,
        "waxed_weathered_copper_bulb" => 24719,
        "waxed_oxidized_copper_bulb" => 24723,
        "lightning_rod" => 24743,
        "pointed_dripstone" => 24753,
        "dripstone_block" => 24768,
        "cave_vines" => 24770,
        "cave_vines_plant" => 24822,
        "spore_blossom" => 24823,
        "azalea" => 24824,
        "flowering_azalea" => 24825,
        "moss_carpet" => 24826,
        "pink_petals" => 24827,
        "moss_block" => 24843,
        "big_dripleaf" => 24845,
        "big_dripleaf_stem" => 24877,
        "small_dripleaf" => 24887,
        "hanging_roots" => 24901,
        "rooted_dirt" => 24902,
        "mud" => 24903,
        "deepslate" => 24905,
        "cobbled_deepslate" => 24907,
        "cobbled_deepslate_stairs" => 24919,
        "cobbled_deepslate_slab" => 24991,
        "cobbled_deepslate_wall" => 24997,
        "polished_deepslate" => 25318,
        "polished_deepslate_stairs" => 25330,
        "polished_deepslate_slab" => 25402,
        "polished_deepslate_wall" => 25408,
        "deepslate_tiles" => 25729,
        "deepslate_tile_stairs" => 25741,
        "deepslate_tile_slab" => 25813,
        "deepslate_tile_wall" => 25819,
        "deepslate_bricks" => 26140,
        "deepslate_brick_stairs" => 26152,
        "deepslate_brick_slab" => 26224,
        "deepslate_brick_wall" => 26230,
        "chiseled_deepslate" => 26551,
        "cracked_deepslate_bricks" => 26552,
        "cracked_deepslate_tiles" => 26553,
        "infested_deepslate" => 26555,
        "smooth_basalt" => 26557,
        "raw_iron_block" => 26558,
        "raw_copper_block" => 26559,
        "raw_gold_block" => 26560,
        "potted_azalea_bush" => 26561,
        "potted_flowering_azalea_bush" => 26562,
        "ochre_froglight" => 26564,
        "verdant_froglight" => 26567,
        "pearlescent_froglight" => 26570,
        "frogspawn" => 26572,
        "reinforced_deepslate" => 26573,
        "decorated_pot" => 26583,
        "crafter" => 26635,
        "trial_spawner" => 26644,
        "vault" => 26654,
        "heavy_core" => 26683,
        _ => return None,
    })
}

fn name_to_item(n: &str) -> Option<i64> {
    let n = n.strip_prefix("minecraft:").unwrap_or(n);
    Some(match n {
        "air" => 0,
        "stone" => 1,
        "granite" => 2,
        "polished_granite" => 3,
        "diorite" => 4,
        "polished_diorite" => 5,
        "andesite" => 6,
        "polished_andesite" => 7,
        "deepslate" => 8,
        "cobbled_deepslate" => 9,
        "polished_deepslate" => 10,
        "calcite" => 11,
        "tuff" => 12,
        "tuff_slab" => 13,
        "tuff_stairs" => 14,
        "tuff_wall" => 15,
        "chiseled_tuff" => 16,
        "polished_tuff" => 17,
        "polished_tuff_slab" => 18,
        "polished_tuff_stairs" => 19,
        "polished_tuff_wall" => 20,
        "tuff_bricks" => 21,
        "tuff_brick_slab" => 22,
        "tuff_brick_stairs" => 23,
        "tuff_brick_wall" => 24,
        "chiseled_tuff_bricks" => 25,
        "dripstone_block" => 26,
        "grass_block" => 27,
        "dirt" => 28,
        "coarse_dirt" => 29,
        "podzol" => 30,
        "rooted_dirt" => 31,
        "mud" => 32,
        "crimson_nylium" => 33,
        "warped_nylium" => 34,
        "cobblestone" => 35,
        "oak_planks" => 36,
        "spruce_planks" => 37,
        "birch_planks" => 38,
        "jungle_planks" => 39,
        "acacia_planks" => 40,
        "cherry_planks" => 41,
        "dark_oak_planks" => 42,
        "mangrove_planks" => 43,
        "bamboo_planks" => 44,
        "crimson_planks" => 45,
        "warped_planks" => 46,
        "bamboo_mosaic" => 47,
        "oak_sapling" => 48,
        "spruce_sapling" => 49,
        "birch_sapling" => 50,
        "jungle_sapling" => 51,
        "acacia_sapling" => 52,
        "cherry_sapling" => 53,
        "dark_oak_sapling" => 54,
        "mangrove_propagule" => 55,
        "bedrock" => 56,
        "sand" => 57,
        "suspicious_sand" => 58,
        "suspicious_gravel" => 59,
        "red_sand" => 60,
        "gravel" => 61,
        "coal_ore" => 62,
        "deepslate_coal_ore" => 63,
        "iron_ore" => 64,
        "deepslate_iron_ore" => 65,
        "copper_ore" => 66,
        "deepslate_copper_ore" => 67,
        "gold_ore" => 68,
        "deepslate_gold_ore" => 69,
        "redstone_ore" => 70,
        "deepslate_redstone_ore" => 71,
        "emerald_ore" => 72,
        "deepslate_emerald_ore" => 73,
        "lapis_ore" => 74,
        "deepslate_lapis_ore" => 75,
        "diamond_ore" => 76,
        "deepslate_diamond_ore" => 77,
        "nether_gold_ore" => 78,
        "nether_quartz_ore" => 79,
        "ancient_debris" => 80,
        "coal_block" => 81,
        "raw_iron_block" => 82,
        "raw_copper_block" => 83,
        "raw_gold_block" => 84,
        "heavy_core" => 85,
        "amethyst_block" => 86,
        "budding_amethyst" => 87,
        "iron_block" => 88,
        "copper_block" => 89,
        "gold_block" => 90,
        "diamond_block" => 91,
        "netherite_block" => 92,
        "exposed_copper" => 93,
        "weathered_copper" => 94,
        "oxidized_copper" => 95,
        "chiseled_copper" => 96,
        "exposed_chiseled_copper" => 97,
        "weathered_chiseled_copper" => 98,
        "oxidized_chiseled_copper" => 99,
        "cut_copper" => 100,
        "exposed_cut_copper" => 101,
        "weathered_cut_copper" => 102,
        "oxidized_cut_copper" => 103,
        "cut_copper_stairs" => 104,
        "exposed_cut_copper_stairs" => 105,
        "weathered_cut_copper_stairs" => 106,
        "oxidized_cut_copper_stairs" => 107,
        "cut_copper_slab" => 108,
        "exposed_cut_copper_slab" => 109,
        "weathered_cut_copper_slab" => 110,
        "oxidized_cut_copper_slab" => 111,
        "waxed_copper_block" => 112,
        "waxed_exposed_copper" => 113,
        "waxed_weathered_copper" => 114,
        "waxed_oxidized_copper" => 115,
        "waxed_chiseled_copper" => 116,
        "waxed_exposed_chiseled_copper" => 117,
        "waxed_weathered_chiseled_copper" => 118,
        "waxed_oxidized_chiseled_copper" => 119,
        "waxed_cut_copper" => 120,
        "waxed_exposed_cut_copper" => 121,
        "waxed_weathered_cut_copper" => 122,
        "waxed_oxidized_cut_copper" => 123,
        "waxed_cut_copper_stairs" => 124,
        "waxed_exposed_cut_copper_stairs" => 125,
        "waxed_weathered_cut_copper_stairs" => 126,
        "waxed_oxidized_cut_copper_stairs" => 127,
        "waxed_cut_copper_slab" => 128,
        "waxed_exposed_cut_copper_slab" => 129,
        "waxed_weathered_cut_copper_slab" => 130,
        "waxed_oxidized_cut_copper_slab" => 131,
        "oak_log" => 132,
        "spruce_log" => 133,
        "birch_log" => 134,
        "jungle_log" => 135,
        "acacia_log" => 136,
        "cherry_log" => 137,
        "dark_oak_log" => 138,
        "mangrove_log" => 139,
        "mangrove_roots" => 140,
        "muddy_mangrove_roots" => 141,
        "crimson_stem" => 142,
        "warped_stem" => 143,
        "bamboo_block" => 144,
        "stripped_oak_log" => 145,
        "stripped_spruce_log" => 146,
        "stripped_birch_log" => 147,
        "stripped_jungle_log" => 148,
        "stripped_acacia_log" => 149,
        "stripped_cherry_log" => 150,
        "stripped_dark_oak_log" => 151,
        "stripped_mangrove_log" => 152,
        "stripped_crimson_stem" => 153,
        "stripped_warped_stem" => 154,
        "stripped_oak_wood" => 155,
        "stripped_spruce_wood" => 156,
        "stripped_birch_wood" => 157,
        "stripped_jungle_wood" => 158,
        "stripped_acacia_wood" => 159,
        "stripped_cherry_wood" => 160,
        "stripped_dark_oak_wood" => 161,
        "stripped_mangrove_wood" => 162,
        "stripped_crimson_hyphae" => 163,
        "stripped_warped_hyphae" => 164,
        "stripped_bamboo_block" => 165,
        "oak_wood" => 166,
        "spruce_wood" => 167,
        "birch_wood" => 168,
        "jungle_wood" => 169,
        "acacia_wood" => 170,
        "cherry_wood" => 171,
        "dark_oak_wood" => 172,
        "mangrove_wood" => 173,
        "crimson_hyphae" => 174,
        "warped_hyphae" => 175,
        "oak_leaves" => 176,
        "spruce_leaves" => 177,
        "birch_leaves" => 178,
        "jungle_leaves" => 179,
        "acacia_leaves" => 180,
        "cherry_leaves" => 181,
        "dark_oak_leaves" => 182,
        "mangrove_leaves" => 183,
        "azalea_leaves" => 184,
        "flowering_azalea_leaves" => 185,
        "sponge" => 186,
        "wet_sponge" => 187,
        "glass" => 188,
        "tinted_glass" => 189,
        "lapis_block" => 190,
        "sandstone" => 191,
        "chiseled_sandstone" => 192,
        "cut_sandstone" => 193,
        "cobweb" => 194,
        "short_grass" => 195,
        "fern" => 196,
        "azalea" => 197,
        "flowering_azalea" => 198,
        "dead_bush" => 199,
        "seagrass" => 200,
        "sea_pickle" => 201,
        "white_wool" => 202,
        "orange_wool" => 203,
        "magenta_wool" => 204,
        "light_blue_wool" => 205,
        "yellow_wool" => 206,
        "lime_wool" => 207,
        "pink_wool" => 208,
        "gray_wool" => 209,
        "light_gray_wool" => 210,
        "cyan_wool" => 211,
        "purple_wool" => 212,
        "blue_wool" => 213,
        "brown_wool" => 214,
        "green_wool" => 215,
        "red_wool" => 216,
        "black_wool" => 217,
        "dandelion" => 218,
        "poppy" => 219,
        "blue_orchid" => 220,
        "allium" => 221,
        "azure_bluet" => 222,
        "red_tulip" => 223,
        "orange_tulip" => 224,
        "white_tulip" => 225,
        "pink_tulip" => 226,
        "oxeye_daisy" => 227,
        "cornflower" => 228,
        "lily_of_the_valley" => 229,
        "wither_rose" => 230,
        "torchflower" => 231,
        "pitcher_plant" => 232,
        "spore_blossom" => 233,
        "brown_mushroom" => 234,
        "red_mushroom" => 235,
        "crimson_fungus" => 236,
        "warped_fungus" => 237,
        "crimson_roots" => 238,
        "warped_roots" => 239,
        "nether_sprouts" => 240,
        "weeping_vines" => 241,
        "twisting_vines" => 242,
        "sugar_cane" => 243,
        "kelp" => 244,
        "moss_carpet" => 245,
        "pink_petals" => 246,
        "moss_block" => 247,
        "hanging_roots" => 248,
        "big_dripleaf" => 249,
        "small_dripleaf" => 250,
        "bamboo" => 251,
        "oak_slab" => 252,
        "spruce_slab" => 253,
        "birch_slab" => 254,
        "jungle_slab" => 255,
        "acacia_slab" => 256,
        "cherry_slab" => 257,
        "dark_oak_slab" => 258,
        "mangrove_slab" => 259,
        "bamboo_slab" => 260,
        "bamboo_mosaic_slab" => 261,
        "crimson_slab" => 262,
        "warped_slab" => 263,
        "stone_slab" => 264,
        "smooth_stone_slab" => 265,
        "sandstone_slab" => 266,
        "cut_sandstone_slab" => 267,
        "petrified_oak_slab" => 268,
        "cobblestone_slab" => 269,
        "brick_slab" => 270,
        "stone_brick_slab" => 271,
        "mud_brick_slab" => 272,
        "nether_brick_slab" => 273,
        "quartz_slab" => 274,
        "red_sandstone_slab" => 275,
        "cut_red_sandstone_slab" => 276,
        "purpur_slab" => 277,
        "prismarine_slab" => 278,
        "prismarine_brick_slab" => 279,
        "dark_prismarine_slab" => 280,
        "smooth_quartz" => 281,
        "smooth_red_sandstone" => 282,
        "smooth_sandstone" => 283,
        "smooth_stone" => 284,
        "bricks" => 285,
        "bookshelf" => 286,
        "chiseled_bookshelf" => 287,
        "decorated_pot" => 288,
        "mossy_cobblestone" => 289,
        "obsidian" => 290,
        "torch" => 291,
        "end_rod" => 292,
        "chorus_plant" => 293,
        "chorus_flower" => 294,
        "purpur_block" => 295,
        "purpur_pillar" => 296,
        "purpur_stairs" => 297,
        "spawner" => 298,
        "chest" => 299,
        "crafting_table" => 300,
        "farmland" => 301,
        "furnace" => 302,
        "ladder" => 303,
        "cobblestone_stairs" => 304,
        "snow" => 305,
        "ice" => 306,
        "snow_block" => 307,
        "cactus" => 308,
        "clay" => 309,
        "jukebox" => 310,
        "oak_fence" => 311,
        "spruce_fence" => 312,
        "birch_fence" => 313,
        "jungle_fence" => 314,
        "acacia_fence" => 315,
        "cherry_fence" => 316,
        "dark_oak_fence" => 317,
        "mangrove_fence" => 318,
        "bamboo_fence" => 319,
        "crimson_fence" => 320,
        "warped_fence" => 321,
        "pumpkin" => 322,
        "carved_pumpkin" => 323,
        "jack_o_lantern" => 324,
        "netherrack" => 325,
        "soul_sand" => 326,
        "soul_soil" => 327,
        "basalt" => 328,
        "polished_basalt" => 329,
        "smooth_basalt" => 330,
        "soul_torch" => 331,
        "glowstone" => 332,
        "infested_stone" => 333,
        "infested_cobblestone" => 334,
        "infested_stone_bricks" => 335,
        "infested_mossy_stone_bricks" => 336,
        "infested_cracked_stone_bricks" => 337,
        "infested_chiseled_stone_bricks" => 338,
        "infested_deepslate" => 339,
        "stone_bricks" => 340,
        "mossy_stone_bricks" => 341,
        "cracked_stone_bricks" => 342,
        "chiseled_stone_bricks" => 343,
        "packed_mud" => 344,
        "mud_bricks" => 345,
        "deepslate_bricks" => 346,
        "cracked_deepslate_bricks" => 347,
        "deepslate_tiles" => 348,
        "cracked_deepslate_tiles" => 349,
        "chiseled_deepslate" => 350,
        "reinforced_deepslate" => 351,
        "brown_mushroom_block" => 352,
        "red_mushroom_block" => 353,
        "mushroom_stem" => 354,
        "iron_bars" => 355,
        "chain" => 356,
        "glass_pane" => 357,
        "melon" => 358,
        "vine" => 359,
        "glow_lichen" => 360,
        "brick_stairs" => 361,
        "stone_brick_stairs" => 362,
        "mud_brick_stairs" => 363,
        "mycelium" => 364,
        "lily_pad" => 365,
        "nether_bricks" => 366,
        "cracked_nether_bricks" => 367,
        "chiseled_nether_bricks" => 368,
        "nether_brick_fence" => 369,
        "nether_brick_stairs" => 370,
        "sculk" => 371,
        "sculk_vein" => 372,
        "sculk_catalyst" => 373,
        "sculk_shrieker" => 374,
        "enchanting_table" => 375,
        "end_portal_frame" => 376,
        "end_stone" => 377,
        "end_stone_bricks" => 378,
        "dragon_egg" => 379,
        "sandstone_stairs" => 380,
        "ender_chest" => 381,
        "emerald_block" => 382,
        "oak_stairs" => 383,
        "spruce_stairs" => 384,
        "birch_stairs" => 385,
        "jungle_stairs" => 386,
        "acacia_stairs" => 387,
        "cherry_stairs" => 388,
        "dark_oak_stairs" => 389,
        "mangrove_stairs" => 390,
        "bamboo_stairs" => 391,
        "bamboo_mosaic_stairs" => 392,
        "crimson_stairs" => 393,
        "warped_stairs" => 394,
        "command_block" => 395,
        "beacon" => 396,
        "cobblestone_wall" => 397,
        "mossy_cobblestone_wall" => 398,
        "brick_wall" => 399,
        "prismarine_wall" => 400,
        "red_sandstone_wall" => 401,
        "mossy_stone_brick_wall" => 402,
        "granite_wall" => 403,
        "stone_brick_wall" => 404,
        "mud_brick_wall" => 405,
        "nether_brick_wall" => 406,
        "andesite_wall" => 407,
        "red_nether_brick_wall" => 408,
        "sandstone_wall" => 409,
        "end_stone_brick_wall" => 410,
        "diorite_wall" => 411,
        "blackstone_wall" => 412,
        "polished_blackstone_wall" => 413,
        "polished_blackstone_brick_wall" => 414,
        "cobbled_deepslate_wall" => 415,
        "polished_deepslate_wall" => 416,
        "deepslate_brick_wall" => 417,
        "deepslate_tile_wall" => 418,
        "anvil" => 419,
        "chipped_anvil" => 420,
        "damaged_anvil" => 421,
        "chiseled_quartz_block" => 422,
        "quartz_block" => 423,
        "quartz_bricks" => 424,
        "quartz_pillar" => 425,
        "quartz_stairs" => 426,
        "white_terracotta" => 427,
        "orange_terracotta" => 428,
        "magenta_terracotta" => 429,
        "light_blue_terracotta" => 430,
        "yellow_terracotta" => 431,
        "lime_terracotta" => 432,
        "pink_terracotta" => 433,
        "gray_terracotta" => 434,
        "light_gray_terracotta" => 435,
        "cyan_terracotta" => 436,
        "purple_terracotta" => 437,
        "blue_terracotta" => 438,
        "brown_terracotta" => 439,
        "green_terracotta" => 440,
        "red_terracotta" => 441,
        "black_terracotta" => 442,
        "barrier" => 443,
        "light" => 444,
        "hay_block" => 445,
        "white_carpet" => 446,
        "orange_carpet" => 447,
        "magenta_carpet" => 448,
        "light_blue_carpet" => 449,
        "yellow_carpet" => 450,
        "lime_carpet" => 451,
        "pink_carpet" => 452,
        "gray_carpet" => 453,
        "light_gray_carpet" => 454,
        "cyan_carpet" => 455,
        "purple_carpet" => 456,
        "blue_carpet" => 457,
        "brown_carpet" => 458,
        "green_carpet" => 459,
        "red_carpet" => 460,
        "black_carpet" => 461,
        "terracotta" => 462,
        "packed_ice" => 463,
        "dirt_path" => 464,
        "sunflower" => 465,
        "lilac" => 466,
        "rose_bush" => 467,
        "peony" => 468,
        "tall_grass" => 469,
        "large_fern" => 470,
        "white_stained_glass" => 471,
        "orange_stained_glass" => 472,
        "magenta_stained_glass" => 473,
        "light_blue_stained_glass" => 474,
        "yellow_stained_glass" => 475,
        "lime_stained_glass" => 476,
        "pink_stained_glass" => 477,
        "gray_stained_glass" => 478,
        "light_gray_stained_glass" => 479,
        "cyan_stained_glass" => 480,
        "purple_stained_glass" => 481,
        "blue_stained_glass" => 482,
        "brown_stained_glass" => 483,
        "green_stained_glass" => 484,
        "red_stained_glass" => 485,
        "black_stained_glass" => 486,
        "white_stained_glass_pane" => 487,
        "orange_stained_glass_pane" => 488,
        "magenta_stained_glass_pane" => 489,
        "light_blue_stained_glass_pane" => 490,
        "yellow_stained_glass_pane" => 491,
        "lime_stained_glass_pane" => 492,
        "pink_stained_glass_pane" => 493,
        "gray_stained_glass_pane" => 494,
        "light_gray_stained_glass_pane" => 495,
        "cyan_stained_glass_pane" => 496,
        "purple_stained_glass_pane" => 497,
        "blue_stained_glass_pane" => 498,
        "brown_stained_glass_pane" => 499,
        "green_stained_glass_pane" => 500,
        "red_stained_glass_pane" => 501,
        "black_stained_glass_pane" => 502,
        "prismarine" => 503,
        "prismarine_bricks" => 504,
        "dark_prismarine" => 505,
        "prismarine_stairs" => 506,
        "prismarine_brick_stairs" => 507,
        "dark_prismarine_stairs" => 508,
        "sea_lantern" => 509,
        "red_sandstone" => 510,
        "chiseled_red_sandstone" => 511,
        "cut_red_sandstone" => 512,
        "red_sandstone_stairs" => 513,
        "repeating_command_block" => 514,
        "chain_command_block" => 515,
        "magma_block" => 516,
        "nether_wart_block" => 517,
        "warped_wart_block" => 518,
        "red_nether_bricks" => 519,
        "bone_block" => 520,
        "structure_void" => 521,
        "shulker_box" => 522,
        "white_shulker_box" => 523,
        "orange_shulker_box" => 524,
        "magenta_shulker_box" => 525,
        "light_blue_shulker_box" => 526,
        "yellow_shulker_box" => 527,
        "lime_shulker_box" => 528,
        "pink_shulker_box" => 529,
        "gray_shulker_box" => 530,
        "light_gray_shulker_box" => 531,
        "cyan_shulker_box" => 532,
        "purple_shulker_box" => 533,
        "blue_shulker_box" => 534,
        "brown_shulker_box" => 535,
        "green_shulker_box" => 536,
        "red_shulker_box" => 537,
        "black_shulker_box" => 538,
        "white_glazed_terracotta" => 539,
        "orange_glazed_terracotta" => 540,
        "magenta_glazed_terracotta" => 541,
        "light_blue_glazed_terracotta" => 542,
        "yellow_glazed_terracotta" => 543,
        "lime_glazed_terracotta" => 544,
        "pink_glazed_terracotta" => 545,
        "gray_glazed_terracotta" => 546,
        "light_gray_glazed_terracotta" => 547,
        "cyan_glazed_terracotta" => 548,
        "purple_glazed_terracotta" => 549,
        "blue_glazed_terracotta" => 550,
        "brown_glazed_terracotta" => 551,
        "green_glazed_terracotta" => 552,
        "red_glazed_terracotta" => 553,
        "black_glazed_terracotta" => 554,
        "white_concrete" => 555,
        "orange_concrete" => 556,
        "magenta_concrete" => 557,
        "light_blue_concrete" => 558,
        "yellow_concrete" => 559,
        "lime_concrete" => 560,
        "pink_concrete" => 561,
        "gray_concrete" => 562,
        "light_gray_concrete" => 563,
        "cyan_concrete" => 564,
        "purple_concrete" => 565,
        "blue_concrete" => 566,
        "brown_concrete" => 567,
        "green_concrete" => 568,
        "red_concrete" => 569,
        "black_concrete" => 570,
        "white_concrete_powder" => 571,
        "orange_concrete_powder" => 572,
        "magenta_concrete_powder" => 573,
        "light_blue_concrete_powder" => 574,
        "yellow_concrete_powder" => 575,
        "lime_concrete_powder" => 576,
        "pink_concrete_powder" => 577,
        "gray_concrete_powder" => 578,
        "light_gray_concrete_powder" => 579,
        "cyan_concrete_powder" => 580,
        "purple_concrete_powder" => 581,
        "blue_concrete_powder" => 582,
        "brown_concrete_powder" => 583,
        "green_concrete_powder" => 584,
        "red_concrete_powder" => 585,
        "black_concrete_powder" => 586,
        "turtle_egg" => 587,
        "sniffer_egg" => 588,
        "dead_tube_coral_block" => 589,
        "dead_brain_coral_block" => 590,
        "dead_bubble_coral_block" => 591,
        "dead_fire_coral_block" => 592,
        "dead_horn_coral_block" => 593,
        "tube_coral_block" => 594,
        "brain_coral_block" => 595,
        "bubble_coral_block" => 596,
        "fire_coral_block" => 597,
        "horn_coral_block" => 598,
        "tube_coral" => 599,
        "brain_coral" => 600,
        "bubble_coral" => 601,
        "fire_coral" => 602,
        "horn_coral" => 603,
        "dead_brain_coral" => 604,
        "dead_bubble_coral" => 605,
        "dead_fire_coral" => 606,
        "dead_horn_coral" => 607,
        "dead_tube_coral" => 608,
        "tube_coral_fan" => 609,
        "brain_coral_fan" => 610,
        "bubble_coral_fan" => 611,
        "fire_coral_fan" => 612,
        "horn_coral_fan" => 613,
        "dead_tube_coral_fan" => 614,
        "dead_brain_coral_fan" => 615,
        "dead_bubble_coral_fan" => 616,
        "dead_fire_coral_fan" => 617,
        "dead_horn_coral_fan" => 618,
        "blue_ice" => 619,
        "conduit" => 620,
        "polished_granite_stairs" => 621,
        "smooth_red_sandstone_stairs" => 622,
        "mossy_stone_brick_stairs" => 623,
        "polished_diorite_stairs" => 624,
        "mossy_cobblestone_stairs" => 625,
        "end_stone_brick_stairs" => 626,
        "stone_stairs" => 627,
        "smooth_sandstone_stairs" => 628,
        "smooth_quartz_stairs" => 629,
        "granite_stairs" => 630,
        "andesite_stairs" => 631,
        "red_nether_brick_stairs" => 632,
        "polished_andesite_stairs" => 633,
        "diorite_stairs" => 634,
        "cobbled_deepslate_stairs" => 635,
        "polished_deepslate_stairs" => 636,
        "deepslate_brick_stairs" => 637,
        "deepslate_tile_stairs" => 638,
        "polished_granite_slab" => 639,
        "smooth_red_sandstone_slab" => 640,
        "mossy_stone_brick_slab" => 641,
        "polished_diorite_slab" => 642,
        "mossy_cobblestone_slab" => 643,
        "end_stone_brick_slab" => 644,
        "smooth_sandstone_slab" => 645,
        "smooth_quartz_slab" => 646,
        "granite_slab" => 647,
        "andesite_slab" => 648,
        "red_nether_brick_slab" => 649,
        "polished_andesite_slab" => 650,
        "diorite_slab" => 651,
        "cobbled_deepslate_slab" => 652,
        "polished_deepslate_slab" => 653,
        "deepslate_brick_slab" => 654,
        "deepslate_tile_slab" => 655,
        "scaffolding" => 656,
        "redstone" => 657,
        "redstone_torch" => 658,
        "redstone_block" => 659,
        "repeater" => 660,
        "comparator" => 661,
        "piston" => 662,
        "sticky_piston" => 663,
        "slime_block" => 664,
        "honey_block" => 665,
        "observer" => 666,
        "hopper" => 667,
        "dispenser" => 668,
        "dropper" => 669,
        "lectern" => 670,
        "target" => 671,
        "lever" => 672,
        "lightning_rod" => 673,
        "daylight_detector" => 674,
        "sculk_sensor" => 675,
        "calibrated_sculk_sensor" => 676,
        "tripwire_hook" => 677,
        "trapped_chest" => 678,
        "tnt" => 679,
        "redstone_lamp" => 680,
        "note_block" => 681,
        "stone_button" => 682,
        "polished_blackstone_button" => 683,
        "oak_button" => 684,
        "spruce_button" => 685,
        "birch_button" => 686,
        "jungle_button" => 687,
        "acacia_button" => 688,
        "cherry_button" => 689,
        "dark_oak_button" => 690,
        "mangrove_button" => 691,
        "bamboo_button" => 692,
        "crimson_button" => 693,
        "warped_button" => 694,
        "stone_pressure_plate" => 695,
        "polished_blackstone_pressure_plate" => 696,
        "light_weighted_pressure_plate" => 697,
        "heavy_weighted_pressure_plate" => 698,
        "oak_pressure_plate" => 699,
        "spruce_pressure_plate" => 700,
        "birch_pressure_plate" => 701,
        "jungle_pressure_plate" => 702,
        "acacia_pressure_plate" => 703,
        "cherry_pressure_plate" => 704,
        "dark_oak_pressure_plate" => 705,
        "mangrove_pressure_plate" => 706,
        "bamboo_pressure_plate" => 707,
        "crimson_pressure_plate" => 708,
        "warped_pressure_plate" => 709,
        "iron_door" => 710,
        "oak_door" => 711,
        "spruce_door" => 712,
        "birch_door" => 713,
        "jungle_door" => 714,
        "acacia_door" => 715,
        "cherry_door" => 716,
        "dark_oak_door" => 717,
        "mangrove_door" => 718,
        "bamboo_door" => 719,
        "crimson_door" => 720,
        "warped_door" => 721,
        "copper_door" => 722,
        "exposed_copper_door" => 723,
        "weathered_copper_door" => 724,
        "oxidized_copper_door" => 725,
        "waxed_copper_door" => 726,
        "waxed_exposed_copper_door" => 727,
        "waxed_weathered_copper_door" => 728,
        "waxed_oxidized_copper_door" => 729,
        "iron_trapdoor" => 730,
        "oak_trapdoor" => 731,
        "spruce_trapdoor" => 732,
        "birch_trapdoor" => 733,
        "jungle_trapdoor" => 734,
        "acacia_trapdoor" => 735,
        "cherry_trapdoor" => 736,
        "dark_oak_trapdoor" => 737,
        "mangrove_trapdoor" => 738,
        "bamboo_trapdoor" => 739,
        "crimson_trapdoor" => 740,
        "warped_trapdoor" => 741,
        "copper_trapdoor" => 742,
        "exposed_copper_trapdoor" => 743,
        "weathered_copper_trapdoor" => 744,
        "oxidized_copper_trapdoor" => 745,
        "waxed_copper_trapdoor" => 746,
        "waxed_exposed_copper_trapdoor" => 747,
        "waxed_weathered_copper_trapdoor" => 748,
        "waxed_oxidized_copper_trapdoor" => 749,
        "oak_fence_gate" => 750,
        "spruce_fence_gate" => 751,
        "birch_fence_gate" => 752,
        "jungle_fence_gate" => 753,
        "acacia_fence_gate" => 754,
        "cherry_fence_gate" => 755,
        "dark_oak_fence_gate" => 756,
        "mangrove_fence_gate" => 757,
        "bamboo_fence_gate" => 758,
        "crimson_fence_gate" => 759,
        "warped_fence_gate" => 760,
        "powered_rail" => 761,
        "detector_rail" => 762,
        "rail" => 763,
        "activator_rail" => 764,
        "saddle" => 765,
        "minecart" => 766,
        "chest_minecart" => 767,
        "furnace_minecart" => 768,
        "tnt_minecart" => 769,
        "hopper_minecart" => 770,
        "carrot_on_a_stick" => 771,
        "warped_fungus_on_a_stick" => 772,
        "elytra" => 773,
        "oak_boat" => 774,
        "oak_chest_boat" => 775,
        "spruce_boat" => 776,
        "spruce_chest_boat" => 777,
        "birch_boat" => 778,
        "birch_chest_boat" => 779,
        "jungle_boat" => 780,
        "jungle_chest_boat" => 781,
        "acacia_boat" => 782,
        "acacia_chest_boat" => 783,
        "cherry_boat" => 784,
        "cherry_chest_boat" => 785,
        "dark_oak_boat" => 786,
        "dark_oak_chest_boat" => 787,
        "mangrove_boat" => 788,
        "mangrove_chest_boat" => 789,
        "bamboo_raft" => 790,
        "bamboo_chest_raft" => 791,
        "structure_block" => 792,
        "jigsaw" => 793,
        "turtle_helmet" => 794,
        "turtle_scute" => 795,
        "armadillo_scute" => 796,
        "wolf_armor" => 797,
        "flint_and_steel" => 798,
        "bowl" => 799,
        "apple" => 800,
        "bow" => 801,
        "arrow" => 802,
        "coal" => 803,
        "charcoal" => 804,
        "diamond" => 805,
        "emerald" => 806,
        "lapis_lazuli" => 807,
        "quartz" => 808,
        "amethyst_shard" => 809,
        "raw_iron" => 810,
        "iron_ingot" => 811,
        "raw_copper" => 812,
        "copper_ingot" => 813,
        "raw_gold" => 814,
        "gold_ingot" => 815,
        "netherite_ingot" => 816,
        "netherite_scrap" => 817,
        "wooden_sword" => 818,
        "wooden_shovel" => 819,
        "wooden_pickaxe" => 820,
        "wooden_axe" => 821,
        "wooden_hoe" => 822,
        "stone_sword" => 823,
        "stone_shovel" => 824,
        "stone_pickaxe" => 825,
        "stone_axe" => 826,
        "stone_hoe" => 827,
        "golden_sword" => 828,
        "golden_shovel" => 829,
        "golden_pickaxe" => 830,
        "golden_axe" => 831,
        "golden_hoe" => 832,
        "iron_sword" => 833,
        "iron_shovel" => 834,
        "iron_pickaxe" => 835,
        "iron_axe" => 836,
        "iron_hoe" => 837,
        "diamond_sword" => 838,
        "diamond_shovel" => 839,
        "diamond_pickaxe" => 840,
        "diamond_axe" => 841,
        "diamond_hoe" => 842,
        "netherite_sword" => 843,
        "netherite_shovel" => 844,
        "netherite_pickaxe" => 845,
        "netherite_axe" => 846,
        "netherite_hoe" => 847,
        "stick" => 848,
        "mushroom_stew" => 849,
        "string" => 850,
        "feather" => 851,
        "gunpowder" => 852,
        "wheat_seeds" => 853,
        "wheat" => 854,
        "bread" => 855,
        "leather_helmet" => 856,
        "leather_chestplate" => 857,
        "leather_leggings" => 858,
        "leather_boots" => 859,
        "chainmail_helmet" => 860,
        "chainmail_chestplate" => 861,
        "chainmail_leggings" => 862,
        "chainmail_boots" => 863,
        "iron_helmet" => 864,
        "iron_chestplate" => 865,
        "iron_leggings" => 866,
        "iron_boots" => 867,
        "diamond_helmet" => 868,
        "diamond_chestplate" => 869,
        "diamond_leggings" => 870,
        "diamond_boots" => 871,
        "golden_helmet" => 872,
        "golden_chestplate" => 873,
        "golden_leggings" => 874,
        "golden_boots" => 875,
        "netherite_helmet" => 876,
        "netherite_chestplate" => 877,
        "netherite_leggings" => 878,
        "netherite_boots" => 879,
        "flint" => 880,
        "porkchop" => 881,
        "cooked_porkchop" => 882,
        "painting" => 883,
        "golden_apple" => 884,
        "enchanted_golden_apple" => 885,
        "oak_sign" => 886,
        "spruce_sign" => 887,
        "birch_sign" => 888,
        "jungle_sign" => 889,
        "acacia_sign" => 890,
        "cherry_sign" => 891,
        "dark_oak_sign" => 892,
        "mangrove_sign" => 893,
        "bamboo_sign" => 894,
        "crimson_sign" => 895,
        "warped_sign" => 896,
        "oak_hanging_sign" => 897,
        "spruce_hanging_sign" => 898,
        "birch_hanging_sign" => 899,
        "jungle_hanging_sign" => 900,
        "acacia_hanging_sign" => 901,
        "cherry_hanging_sign" => 902,
        "dark_oak_hanging_sign" => 903,
        "mangrove_hanging_sign" => 904,
        "bamboo_hanging_sign" => 905,
        "crimson_hanging_sign" => 906,
        "warped_hanging_sign" => 907,
        "bucket" => 908,
        "water_bucket" => 909,
        "lava_bucket" => 910,
        "powder_snow_bucket" => 911,
        "snowball" => 912,
        "leather" => 913,
        "milk_bucket" => 914,
        "pufferfish_bucket" => 915,
        "salmon_bucket" => 916,
        "cod_bucket" => 917,
        "tropical_fish_bucket" => 918,
        "axolotl_bucket" => 919,
        "tadpole_bucket" => 920,
        "brick" => 921,
        "clay_ball" => 922,
        "dried_kelp_block" => 923,
        "paper" => 924,
        "book" => 925,
        "slime_ball" => 926,
        "egg" => 927,
        "compass" => 928,
        "recovery_compass" => 929,
        "bundle" => 930,
        "fishing_rod" => 931,
        "clock" => 932,
        "spyglass" => 933,
        "glowstone_dust" => 934,
        "cod" => 935,
        "salmon" => 936,
        "tropical_fish" => 937,
        "pufferfish" => 938,
        "cooked_cod" => 939,
        "cooked_salmon" => 940,
        "ink_sac" => 941,
        "glow_ink_sac" => 942,
        "cocoa_beans" => 943,
        "white_dye" => 944,
        "orange_dye" => 945,
        "magenta_dye" => 946,
        "light_blue_dye" => 947,
        "yellow_dye" => 948,
        "lime_dye" => 949,
        "pink_dye" => 950,
        "gray_dye" => 951,
        "light_gray_dye" => 952,
        "cyan_dye" => 953,
        "purple_dye" => 954,
        "blue_dye" => 955,
        "brown_dye" => 956,
        "green_dye" => 957,
        "red_dye" => 958,
        "black_dye" => 959,
        "bone_meal" => 960,
        "bone" => 961,
        "sugar" => 962,
        "cake" => 963,
        "white_bed" => 964,
        "orange_bed" => 965,
        "magenta_bed" => 966,
        "light_blue_bed" => 967,
        "yellow_bed" => 968,
        "lime_bed" => 969,
        "pink_bed" => 970,
        "gray_bed" => 971,
        "light_gray_bed" => 972,
        "cyan_bed" => 973,
        "purple_bed" => 974,
        "blue_bed" => 975,
        "brown_bed" => 976,
        "green_bed" => 977,
        "red_bed" => 978,
        "black_bed" => 979,
        "cookie" => 980,
        "crafter" => 981,
        "filled_map" => 982,
        "shears" => 983,
        "melon_slice" => 984,
        "dried_kelp" => 985,
        "pumpkin_seeds" => 986,
        "melon_seeds" => 987,
        "beef" => 988,
        "cooked_beef" => 989,
        "chicken" => 990,
        "cooked_chicken" => 991,
        "rotten_flesh" => 992,
        "ender_pearl" => 993,
        "blaze_rod" => 994,
        "ghast_tear" => 995,
        "gold_nugget" => 996,
        "nether_wart" => 997,
        "potion" => 998,
        "glass_bottle" => 999,
        "spider_eye" => 1000,
        "fermented_spider_eye" => 1001,
        "blaze_powder" => 1002,
        "magma_cream" => 1003,
        "brewing_stand" => 1004,
        "cauldron" => 1005,
        "ender_eye" => 1006,
        "glistering_melon_slice" => 1007,
        "armadillo_spawn_egg" => 1008,
        "allay_spawn_egg" => 1009,
        "axolotl_spawn_egg" => 1010,
        "bat_spawn_egg" => 1011,
        "bee_spawn_egg" => 1012,
        "blaze_spawn_egg" => 1013,
        "bogged_spawn_egg" => 1014,
        "breeze_spawn_egg" => 1015,
        "cat_spawn_egg" => 1016,
        "camel_spawn_egg" => 1017,
        "cave_spider_spawn_egg" => 1018,
        "chicken_spawn_egg" => 1019,
        "cod_spawn_egg" => 1020,
        "cow_spawn_egg" => 1021,
        "creeper_spawn_egg" => 1022,
        "dolphin_spawn_egg" => 1023,
        "donkey_spawn_egg" => 1024,
        "drowned_spawn_egg" => 1025,
        "elder_guardian_spawn_egg" => 1026,
        "ender_dragon_spawn_egg" => 1027,
        "enderman_spawn_egg" => 1028,
        "endermite_spawn_egg" => 1029,
        "evoker_spawn_egg" => 1030,
        "fox_spawn_egg" => 1031,
        "frog_spawn_egg" => 1032,
        "ghast_spawn_egg" => 1033,
        "glow_squid_spawn_egg" => 1034,
        "goat_spawn_egg" => 1035,
        "guardian_spawn_egg" => 1036,
        "hoglin_spawn_egg" => 1037,
        "horse_spawn_egg" => 1038,
        "husk_spawn_egg" => 1039,
        "iron_golem_spawn_egg" => 1040,
        "llama_spawn_egg" => 1041,
        "magma_cube_spawn_egg" => 1042,
        "mooshroom_spawn_egg" => 1043,
        "mule_spawn_egg" => 1044,
        "ocelot_spawn_egg" => 1045,
        "panda_spawn_egg" => 1046,
        "parrot_spawn_egg" => 1047,
        "phantom_spawn_egg" => 1048,
        "pig_spawn_egg" => 1049,
        "piglin_spawn_egg" => 1050,
        "piglin_brute_spawn_egg" => 1051,
        "pillager_spawn_egg" => 1052,
        "polar_bear_spawn_egg" => 1053,
        "pufferfish_spawn_egg" => 1054,
        "rabbit_spawn_egg" => 1055,
        "ravager_spawn_egg" => 1056,
        "salmon_spawn_egg" => 1057,
        "sheep_spawn_egg" => 1058,
        "shulker_spawn_egg" => 1059,
        "silverfish_spawn_egg" => 1060,
        "skeleton_spawn_egg" => 1061,
        "skeleton_horse_spawn_egg" => 1062,
        "slime_spawn_egg" => 1063,
        "sniffer_spawn_egg" => 1064,
        "snow_golem_spawn_egg" => 1065,
        "spider_spawn_egg" => 1066,
        "squid_spawn_egg" => 1067,
        "stray_spawn_egg" => 1068,
        "strider_spawn_egg" => 1069,
        "tadpole_spawn_egg" => 1070,
        "trader_llama_spawn_egg" => 1071,
        "tropical_fish_spawn_egg" => 1072,
        "turtle_spawn_egg" => 1073,
        "vex_spawn_egg" => 1074,
        "villager_spawn_egg" => 1075,
        "vindicator_spawn_egg" => 1076,
        "wandering_trader_spawn_egg" => 1077,
        "warden_spawn_egg" => 1078,
        "witch_spawn_egg" => 1079,
        "wither_spawn_egg" => 1080,
        "wither_skeleton_spawn_egg" => 1081,
        "wolf_spawn_egg" => 1082,
        "zoglin_spawn_egg" => 1083,
        "zombie_spawn_egg" => 1084,
        "zombie_horse_spawn_egg" => 1085,
        "zombie_villager_spawn_egg" => 1086,
        "zombified_piglin_spawn_egg" => 1087,
        "experience_bottle" => 1088,
        "fire_charge" => 1089,
        "wind_charge" => 1090,
        "writable_book" => 1091,
        "written_book" => 1092,
        "mace" => 1093,
        "item_frame" => 1094,
        "glow_item_frame" => 1095,
        "flower_pot" => 1096,
        "carrot" => 1097,
        "potato" => 1098,
        "baked_potato" => 1099,
        "poisonous_potato" => 1100,
        "map" => 1101,
        "golden_carrot" => 1102,
        "skeleton_skull" => 1103,
        "wither_skeleton_skull" => 1104,
        "player_head" => 1105,
        "zombie_head" => 1106,
        "creeper_head" => 1107,
        "dragon_head" => 1108,
        "piglin_head" => 1109,
        "nether_star" => 1110,
        "pumpkin_pie" => 1111,
        "firework_rocket" => 1112,
        "firework_star" => 1113,
        "enchanted_book" => 1114,
        "nether_brick" => 1115,
        "prismarine_shard" => 1116,
        "prismarine_crystals" => 1117,
        "rabbit" => 1118,
        "cooked_rabbit" => 1119,
        "rabbit_stew" => 1120,
        "rabbit_foot" => 1121,
        "rabbit_hide" => 1122,
        "armor_stand" => 1123,
        "iron_horse_armor" => 1124,
        "golden_horse_armor" => 1125,
        "diamond_horse_armor" => 1126,
        "leather_horse_armor" => 1127,
        "lead" => 1128,
        "name_tag" => 1129,
        "command_block_minecart" => 1130,
        "mutton" => 1131,
        "cooked_mutton" => 1132,
        "white_banner" => 1133,
        "orange_banner" => 1134,
        "magenta_banner" => 1135,
        "light_blue_banner" => 1136,
        "yellow_banner" => 1137,
        "lime_banner" => 1138,
        "pink_banner" => 1139,
        "gray_banner" => 1140,
        "light_gray_banner" => 1141,
        "cyan_banner" => 1142,
        "purple_banner" => 1143,
        "blue_banner" => 1144,
        "brown_banner" => 1145,
        "green_banner" => 1146,
        "red_banner" => 1147,
        "black_banner" => 1148,
        "end_crystal" => 1149,
        "chorus_fruit" => 1150,
        "popped_chorus_fruit" => 1151,
        "torchflower_seeds" => 1152,
        "pitcher_pod" => 1153,
        "beetroot" => 1154,
        "beetroot_seeds" => 1155,
        "beetroot_soup" => 1156,
        "dragon_breath" => 1157,
        "splash_potion" => 1158,
        "spectral_arrow" => 1159,
        "tipped_arrow" => 1160,
        "lingering_potion" => 1161,
        "shield" => 1162,
        "totem_of_undying" => 1163,
        "shulker_shell" => 1164,
        "iron_nugget" => 1165,
        "knowledge_book" => 1166,
        "debug_stick" => 1167,
        "music_disc_13" => 1168,
        "music_disc_cat" => 1169,
        "music_disc_blocks" => 1170,
        "music_disc_chirp" => 1171,
        "music_disc_creator" => 1172,
        "music_disc_creator_music_box" => 1173,
        "music_disc_far" => 1174,
        "music_disc_mall" => 1175,
        "music_disc_mellohi" => 1176,
        "music_disc_stal" => 1177,
        "music_disc_strad" => 1178,
        "music_disc_ward" => 1179,
        "music_disc_11" => 1180,
        "music_disc_wait" => 1181,
        "music_disc_otherside" => 1182,
        "music_disc_relic" => 1183,
        "music_disc_5" => 1184,
        "music_disc_pigstep" => 1185,
        "music_disc_precipice" => 1186,
        "disc_fragment_5" => 1187,
        "trident" => 1188,
        "phantom_membrane" => 1189,
        "nautilus_shell" => 1190,
        "heart_of_the_sea" => 1191,
        "crossbow" => 1192,
        "suspicious_stew" => 1193,
        "loom" => 1194,
        "flower_banner_pattern" => 1195,
        "creeper_banner_pattern" => 1196,
        "skull_banner_pattern" => 1197,
        "mojang_banner_pattern" => 1198,
        "globe_banner_pattern" => 1199,
        "piglin_banner_pattern" => 1200,
        "flow_banner_pattern" => 1201,
        "guster_banner_pattern" => 1202,
        "goat_horn" => 1203,
        "composter" => 1204,
        "barrel" => 1205,
        "smoker" => 1206,
        "blast_furnace" => 1207,
        "cartography_table" => 1208,
        "fletching_table" => 1209,
        "grindstone" => 1210,
        "smithing_table" => 1211,
        "stonecutter" => 1212,
        "bell" => 1213,
        "lantern" => 1214,
        "soul_lantern" => 1215,
        "sweet_berries" => 1216,
        "glow_berries" => 1217,
        "campfire" => 1218,
        "soul_campfire" => 1219,
        "shroomlight" => 1220,
        "honeycomb" => 1221,
        "bee_nest" => 1222,
        "beehive" => 1223,
        "honey_bottle" => 1224,
        "honeycomb_block" => 1225,
        "lodestone" => 1226,
        "crying_obsidian" => 1227,
        "blackstone" => 1228,
        "blackstone_slab" => 1229,
        "blackstone_stairs" => 1230,
        "gilded_blackstone" => 1231,
        "polished_blackstone" => 1232,
        "polished_blackstone_slab" => 1233,
        "polished_blackstone_stairs" => 1234,
        "chiseled_polished_blackstone" => 1235,
        "polished_blackstone_bricks" => 1236,
        "polished_blackstone_brick_slab" => 1237,
        "polished_blackstone_brick_stairs" => 1238,
        "cracked_polished_blackstone_bricks" => 1239,
        "respawn_anchor" => 1240,
        "candle" => 1241,
        "white_candle" => 1242,
        "orange_candle" => 1243,
        "magenta_candle" => 1244,
        "light_blue_candle" => 1245,
        "yellow_candle" => 1246,
        "lime_candle" => 1247,
        "pink_candle" => 1248,
        "gray_candle" => 1249,
        "light_gray_candle" => 1250,
        "cyan_candle" => 1251,
        "purple_candle" => 1252,
        "blue_candle" => 1253,
        "brown_candle" => 1254,
        "green_candle" => 1255,
        "red_candle" => 1256,
        "black_candle" => 1257,
        "small_amethyst_bud" => 1258,
        "medium_amethyst_bud" => 1259,
        "large_amethyst_bud" => 1260,
        "amethyst_cluster" => 1261,
        "pointed_dripstone" => 1262,
        "ochre_froglight" => 1263,
        "verdant_froglight" => 1264,
        "pearlescent_froglight" => 1265,
        "frogspawn" => 1266,
        "echo_shard" => 1267,
        "brush" => 1268,
        "netherite_upgrade_smithing_template" => 1269,
        "sentry_armor_trim_smithing_template" => 1270,
        "dune_armor_trim_smithing_template" => 1271,
        "coast_armor_trim_smithing_template" => 1272,
        "wild_armor_trim_smithing_template" => 1273,
        "ward_armor_trim_smithing_template" => 1274,
        "eye_armor_trim_smithing_template" => 1275,
        "vex_armor_trim_smithing_template" => 1276,
        "tide_armor_trim_smithing_template" => 1277,
        "snout_armor_trim_smithing_template" => 1278,
        "rib_armor_trim_smithing_template" => 1279,
        "spire_armor_trim_smithing_template" => 1280,
        "wayfinder_armor_trim_smithing_template" => 1281,
        "shaper_armor_trim_smithing_template" => 1282,
        "silence_armor_trim_smithing_template" => 1283,
        "raiser_armor_trim_smithing_template" => 1284,
        "host_armor_trim_smithing_template" => 1285,
        "flow_armor_trim_smithing_template" => 1286,
        "bolt_armor_trim_smithing_template" => 1287,
        "angler_pottery_sherd" => 1288,
        "archer_pottery_sherd" => 1289,
        "arms_up_pottery_sherd" => 1290,
        "blade_pottery_sherd" => 1291,
        "brewer_pottery_sherd" => 1292,
        "burn_pottery_sherd" => 1293,
        "danger_pottery_sherd" => 1294,
        "explorer_pottery_sherd" => 1295,
        "flow_pottery_sherd" => 1296,
        "friend_pottery_sherd" => 1297,
        "guster_pottery_sherd" => 1298,
        "heart_pottery_sherd" => 1299,
        "heartbreak_pottery_sherd" => 1300,
        "howl_pottery_sherd" => 1301,
        "miner_pottery_sherd" => 1302,
        "mourner_pottery_sherd" => 1303,
        "plenty_pottery_sherd" => 1304,
        "prize_pottery_sherd" => 1305,
        "scrape_pottery_sherd" => 1306,
        "sheaf_pottery_sherd" => 1307,
        "shelter_pottery_sherd" => 1308,
        "skull_pottery_sherd" => 1309,
        "snort_pottery_sherd" => 1310,
        "copper_grate" => 1311,
        "exposed_copper_grate" => 1312,
        "weathered_copper_grate" => 1313,
        "oxidized_copper_grate" => 1314,
        "waxed_copper_grate" => 1315,
        "waxed_exposed_copper_grate" => 1316,
        "waxed_weathered_copper_grate" => 1317,
        "waxed_oxidized_copper_grate" => 1318,
        "copper_bulb" => 1319,
        "exposed_copper_bulb" => 1320,
        "weathered_copper_bulb" => 1321,
        "oxidized_copper_bulb" => 1322,
        "waxed_copper_bulb" => 1323,
        "waxed_exposed_copper_bulb" => 1324,
        "waxed_weathered_copper_bulb" => 1325,
        "waxed_oxidized_copper_bulb" => 1326,
        "trial_spawner" => 1327,
        "trial_key" => 1328,
        "ominous_trial_key" => 1329,
        "vault" => 1330,
        "ominous_bottle" => 1331,
        "breeze_rod" => 1332,
        _ => return None,
    })
}

fn name_to_etype(n: &str) -> Option<i64> {
    let n = n.strip_prefix("minecraft:").unwrap_or(n);
    Some(match n {
        "allay" => 0,
        "area_effect_cloud" => 1,
        "armadillo" => 2,
        "armor_stand" => 3,
        "arrow" => 4,
        "axolotl" => 5,
        "bat" => 6,
        "bee" => 7,
        "blaze" => 8,
        "block_display" => 9,
        "boat" => 10,
        "bogged" => 11,
        "breeze" => 12,
        "breeze_wind_charge" => 13,
        "camel" => 14,
        "cat" => 15,
        "cave_spider" => 16,
        "chest_boat" => 17,
        "chest_minecart" => 18,
        "chicken" => 19,
        "cod" => 20,
        "command_block_minecart" => 21,
        "cow" => 22,
        "creeper" => 23,
        "dolphin" => 24,
        "donkey" => 25,
        "dragon_fireball" => 26,
        "drowned" => 27,
        "egg" => 28,
        "elder_guardian" => 29,
        "end_crystal" => 30,
        "ender_dragon" => 31,
        "ender_pearl" => 32,
        "enderman" => 33,
        "endermite" => 34,
        "evoker" => 35,
        "evoker_fangs" => 36,
        "experience_bottle" => 37,
        "experience_orb" => 38,
        "eye_of_ender" => 39,
        "falling_block" => 40,
        "firework_rocket" => 41,
        "fox" => 42,
        "frog" => 43,
        "furnace_minecart" => 44,
        "ghast" => 45,
        "giant" => 46,
        "glow_item_frame" => 47,
        "glow_squid" => 48,
        "goat" => 49,
        "guardian" => 50,
        "hoglin" => 51,
        "hopper_minecart" => 52,
        "horse" => 53,
        "husk" => 54,
        "illusioner" => 55,
        "interaction" => 56,
        "iron_golem" => 57,
        "item" => 58,
        "item_display" => 59,
        "item_frame" => 60,
        "ominous_item_spawner" => 61,
        "fireball" => 62,
        "leash_knot" => 63,
        "lightning_bolt" => 64,
        "llama" => 65,
        "llama_spit" => 66,
        "magma_cube" => 67,
        "marker" => 68,
        "minecart" => 69,
        "mooshroom" => 70,
        "mule" => 71,
        "ocelot" => 72,
        "painting" => 73,
        "panda" => 74,
        "parrot" => 75,
        "phantom" => 76,
        "pig" => 77,
        "piglin" => 78,
        "piglin_brute" => 79,
        "pillager" => 80,
        "polar_bear" => 81,
        "potion" => 82,
        "pufferfish" => 83,
        "rabbit" => 84,
        "ravager" => 85,
        "salmon" => 86,
        "sheep" => 87,
        "shulker" => 88,
        "shulker_bullet" => 89,
        "silverfish" => 90,
        "skeleton" => 91,
        "skeleton_horse" => 92,
        "slime" => 93,
        "small_fireball" => 94,
        "sniffer" => 95,
        "snow_golem" => 96,
        "snowball" => 97,
        "spawner_minecart" => 98,
        "spectral_arrow" => 99,
        "spider" => 100,
        "squid" => 101,
        "stray" => 102,
        "strider" => 103,
        "tadpole" => 104,
        "text_display" => 105,
        "tnt" => 106,
        "tnt_minecart" => 107,
        "trader_llama" => 108,
        "trident" => 109,
        "tropical_fish" => 110,
        "turtle" => 111,
        "vex" => 112,
        "villager" => 113,
        "vindicator" => 114,
        "wandering_trader" => 115,
        "warden" => 116,
        "wind_charge" => 117,
        "witch" => 118,
        "wither" => 119,
        "wither_skeleton" => 120,
        "wither_skull" => 121,
        "wolf" => 122,
        "zoglin" => 123,
        "zombie" => 124,
        "zombie_horse" => 125,
        "zombie_villager" => 126,
        "zombified_piglin" => 127,
        "player" => 128,
        "fishing_bobber" => 129,
        _ => return None,
    })
}

// ---------- resource pack (embedded http + sha1) ----------
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let ml = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 { msg.push(0); }
    msg.extend_from_slice(&ml.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 { w[i] = u32::from_be_bytes(chunk[i*4..i*4+4].try_into().unwrap()); }
        for i in 16..80 { w[i] = (w[i-3] ^ w[i-8] ^ w[i-14] ^ w[i-16]).rotate_left(1); }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(w[i]);
            e = d; d = c; c = b.rotate_left(30); b = a; a = t;
        }
        h[0]=h[0].wrapping_add(a); h[1]=h[1].wrapping_add(b); h[2]=h[2].wrapping_add(c);
        h[3]=h[3].wrapping_add(d); h[4]=h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for i in 0..5 { out[i*4..i*4+4].copy_from_slice(&h[i].to_be_bytes()); }
    out
}
fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{:02x}", x)).collect() }

static PACK_INFO: OnceLock<Option<(String, String)>> = OnceLock::new(); // (url, sha1hex)
fn pack_info() -> &'static Option<(String, String)> { PACK_INFO.get_or_init(|| load_pack_info()) }
fn load_pack_info() -> Option<(String, String)> {
    let data = std::fs::read("resourcepack.zip").ok()?;
    let h = hex(&sha1(&data));
    Some((format!("http://{}:25598/pack.zip", local_ip()), h))
}
fn local_ip() -> String { "127.0.0.1".into() } // override via env later if needed

fn http_server() {
    let Ok(data) = std::fs::read("resourcepack.zip") else { return };
    let Ok(l) = TcpListener::bind("0.0.0.0:25598") else { return };
    eprintln!("[spine] resource pack http :25598 ({} bytes)", data.len());
    thread::spawn(move || {
        for st in l.incoming().flatten() {
            let mut st = st;
            st.set_read_timeout(Some(Duration::from_secs(5))).ok();
            st.set_write_timeout(Some(Duration::from_secs(10))).ok();
            let mut buf = [0u8; 1024];
            let _ = st.read(&mut buf);
            let head = format!("HTTP/1.1 200 OK
Content-Type: application/zip
Content-Length: {}
Connection: close

", data.len());
            let _ = st.write_all(head.as_bytes());
            let _ = st.write_all(&data);
        }
    });
}
fn resource_pack_pkt() -> Option<Vec<u8>> {
    let (url, hash) = pack_info().as_ref()?;
    let mut p = Vec::new();
    p.extend_from_slice(&[0xabu8; 16]); // uuid
    write_str(&mut p, url);
    write_str(&mut p, hash);
    p.push(0); // forced=false
    p.push(0); // no prompt
    Some(p)
}

// ---------- datapacks (folder packs under datapacks/) ----------
struct Datapack { name: String, functions: HashMap<String, Vec<String>>, load: Vec<String>, tick: Vec<String> }
static PACKS: OnceLock<Mutex<Vec<Datapack>>> = OnceLock::new();
fn packs() -> &'static Mutex<Vec<Datapack>> { PACKS.get_or_init(|| Mutex::new(Vec::new())) }

fn load_datapacks() {
    let base = std::path::Path::new("datapacks");
    let Ok(rd) = std::fs::read_dir(base) else { return };
    for e in rd.flatten() {
        let dir = e.path();
        if !dir.is_dir() { continue }
        let mut pk = Datapack { name: e.file_name().to_string_lossy().into(), functions: HashMap::new(), load: vec![], tick: vec![] };
        // functions: data/<ns>/function/**/*.mcfunction (1.21 layout: 'function' singular)
        if let Ok(data) = std::fs::read_dir(dir.join("data")) {
            for ns in data.flatten() {
                let nsn = ns.file_name().to_string_lossy().to_string();
                for tag in ["function", "functions"] {
                    let fdir = ns.path().join(tag);
                    let mut stack = vec![fdir];
                    while let Some(d) = stack.pop() {
                        if let Ok(rr) = std::fs::read_dir(&d) {
                            for f in rr.flatten() {
                                let p = f.path();
                                if p.is_dir() { stack.push(p); continue }
                                if p.extension().map(|x| x == "mcfunction").unwrap_or(false) {
                                    let rel = p.strip_prefix(&ns.path().join(tag)).unwrap().to_string_lossy().replace('\\', "/");
                                    let key = format!("{}:{}", nsn, rel.trim_end_matches(".mcfunction"));
                                    if let Ok(body) = std::fs::read_to_string(&p) {
                                        pk.functions.insert(key, body.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty() && !l.starts_with('#')).collect());
                                    }
                                }
                            }
                        }
                    }
                }
                // tags: data/<ns>/tags/function/{load,tick}.json
                for tag in ["function", "functions"] {
                    for (jf, dst) in [("load", &mut pk.load), ("tick", &mut pk.tick)] {
                        let jp = ns.path().join("tags").join(tag).join(format!("{}.json", jf));
                        if let Ok(body) = std::fs::read_to_string(&jp) {
                            // tiny json: grab quoted strings inside "values" array
                            if let Some(va) = body.split("\"values\"").nth(1) {
                                for m in va.split('"').skip(1).step_by(2) {
                                    if m.starts_with('#') || m.contains(':') {
                                        let m = m.trim_start_matches('#');
                                        let m = if m.contains(':') { m.to_string() } else { format!("minecraft:{}", m) };
                                        dst.push(m);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if !pk.functions.is_empty() || !pk.load.is_empty() {
            eprintln!("[spine] datapack {}: {} fn, load={:?}, tick={:?}", pk.name, pk.functions.len(), pk.load, pk.tick);
            packs().lock().unwrap().push(pk);
        }
    }
}

fn lookup_function(name: &str) -> Option<Vec<String>> {
    let name = if name.contains(':') { name.to_string() } else { format!("minecraft:{}", name) };
    packs().lock().unwrap().iter().find_map(|p| p.functions.get(&name).cloned())
}
fn run_function(name: &str, me: Option<Arc<Client>>, s: &mut Option<&mut TcpStream>) -> usize {
    run_function_depth(name, me, s, 0)
}
fn run_function_depth(name: &str, me: Option<Arc<Client>>, s: &mut Option<&mut TcpStream>, depth: usize) -> usize {
    if depth > 16 { return 0 }
    let Some(lines) = lookup_function(name) else { return 0 };
    let mut n = 0;
    for line in lines {
        if let Some(rest) = line.strip_prefix("function ") {
            n += run_function_depth(rest.trim(), me.clone(), s, depth + 1);
        } else {
            run_command(me.as_ref(), &format!("/{}", line), s);
            n += 1;
        }
    }
    n
}

// ---------- item -> blockstate (1.21.1 ids) ----------
fn item_to_state(item: i64) -> Option<u32> {
    Some(match item {
        1 => 1, 27 => 9, 28 => 10, 35 => 14, 36 => 15, 57 => 112,
        88 => 2092, 91 => 4276, 132 => 131, 188 => 519, 290 => 2354, 291 => 2355,
        _ => return None,
    })
}

// ---------- config blob replay ----------
fn load_config_packets() -> Vec<(i64, Vec<u8>)> {
    let data = match std::fs::read("config_packets.bin") { Ok(d) => d, Err(_) => return vec![] };
    let mut out = Vec::new();
    let mut sl = &data[..];
    while !sl.is_empty() {
        let len = match read_varint(&mut sl) { Some(l) => l as usize, None => break };
        if sl.len() < len { break }
        let pkt = sl[..len].to_vec(); sl = &sl[len..];
        let mut p2 = &pkt[..];
        let id = read_varint(&mut p2).unwrap_or(-1);
        out.push((id, pkt));
    }
    out
}

// ---------- chunks ----------
fn section_encode(states: &[u32; 4096], biome: i64) -> Vec<u8> {
    let mut v = Vec::new();
    // palette
    let mut pal: Vec<u32> = Vec::new();
    let mut idx = vec![0u16; 4096];
    for (i, &s) in states.iter().enumerate() {
        match pal.iter().position(|&p| p == s) {
            Some(p) => idx[i] = p as u16,
            None => { idx[i] = pal.len() as u16; pal.push(s); }
        }
    }
    let bc = states.iter().filter(|&&s| s != 0).count() as i16;
    v.extend_from_slice(&bc.to_be_bytes());
    if pal.len() == 1 {
        v.push(0); write_varint(&mut v, pal[0] as i64); write_varint(&mut v, 0);
    } else {
        let bits = 64 - (pal.len() as u64 - 1).leading_zeros();
        let bpe = bits.max(4) as usize;
        v.push(bpe as u8);
        write_varint(&mut v, pal.len() as i64);
        for &p in &pal { write_varint(&mut v, p as i64); }
        let per = 64 / bpe;
        let nlongs = 4096 / per + if 4096 % per != 0 { 1 } else { 0 };
        write_varint(&mut v, nlongs as i64);
        let mut longs = vec![0u64; nlongs];
        for i in 0..4096 {
            let li = i / per; let off = (i % per) * bpe;
            longs[li] |= (idx[i] as u64) << off;
        }
        for l in longs { v.extend_from_slice(&l.to_be_bytes()); }
    }
    // biome: always single plains (id 1)
    v.push(0); write_varint(&mut v, biome); write_varint(&mut v, 0);
    v
}

fn chunk_data(cx: i32, cz: i32) -> Vec<u8> {

    let mut data = Vec::new();
    let overlay_empty = {
        let w = world().lock().unwrap();
        w.get(&(cx, cz)).map_or(true, |c| c.is_empty())
    };
    let mut hs = [0i32; 256];
    let (mut hmin, mut hmax) = (i32::MAX, i32::MIN);
    for lz in 0..16 { for lx in 0..16 {
        let h = height_at(cx * 16 + lx, cz * 16 + lz);
        hs[((lz << 4) | lx) as usize] = h;
        hmin = hmin.min(h); hmax = hmax.max(h);
    }}
    for sec in 0..24 {
        let sy = sec * 16 + MIN_Y;
        // uniform fast path only when overlay empty AND terrain can't reach the section
        if overlay_empty && (sy + 15 < hmin - 4 || sy > hmax + 6) {
            let st = if sy + 15 < hmin - 4 { 1 } else { 0 };
            let mut arr = [0u32; 4096];
            if st != 0 { arr = [st; 4096]; }
            data.extend_from_slice(&section_encode(&arr, 1));
        } else {
            let mut arr = [0u32; 4096];
            for ly in 0..16 {
                for lz in 0..16 {
                    for lx in 0..16 {
                        let wx = cx * 16 + lx as i32; let wz = cz * 16 + lz as i32; let wy = sy + ly as i32;
                        arr[(ly << 8) | (lz << 4) | lx] = if overlay_empty {
                            column_state(wx, wy, wz, hs[((lz << 4) | lx) as usize])
                        } else {
                            block_at(wx, wy, wz)
                        };
                    }
                }
            }
            data.extend_from_slice(&section_encode(&arr, 1));
        }
    }
    data
}

fn nbt_str(v: &mut Vec<u8>, s: &str) {
    v.extend_from_slice(&(s.len() as u16).to_be_bytes()); v.extend_from_slice(s.as_bytes());
}
fn heightmaps_nbt(top_plus1: i64) -> Vec<u8> {
    let mut v = Vec::new();
    v.push(10); // TAG_Compound — anonymousNbt: contents directly, no root name
    v.push(12); // TAG_Long_Array
    nbt_str(&mut v, "MOTION_BLOCKING");
    v.extend_from_slice(&37i32.to_be_bytes());
    let per = 64 / 9;
    let mut longs = vec![0u64; 37];
    for i in 0..256usize {
        let li = i / per; let off = (i % per) * 9;
        if li < 37 { longs[li] |= ((top_plus1 as u64) & 0x1ff) << off; }
    }
    for l in longs { v.extend_from_slice(&l.to_be_bytes()); }
    v.push(0);
    v
}

fn map_chunk(cx: i32, cz: i32) -> Vec<u8> {

    let mut p = Vec::new();
    p.extend_from_slice(&cx.to_be_bytes());
    p.extend_from_slice(&cz.to_be_bytes());
    p.extend_from_slice(&heightmaps_nbt(16)); // flat top y=15 -> 16
    let data = chunk_data(cx, cz);
    write_varint(&mut p, data.len() as i64);
    p.extend_from_slice(&data);
    write_varint(&mut p, 0); // block entities
    write_varint(&mut p, 1); p.extend_from_slice(&0x3ffffffi64.to_be_bytes()); // skyLightMask
    write_varint(&mut p, 0); // blockLightMask
    write_varint(&mut p, 0); // emptySky
    write_varint(&mut p, 1); p.extend_from_slice(&0x3ffffffi64.to_be_bytes()); // emptyBlock
    write_varint(&mut p, 26);
    for _ in 0..26 { write_varint(&mut p, 2048); p.extend_from_slice(&[0xffu8; 2048]); }
    write_varint(&mut p, 0);
    p
}

fn stream_chunks(s: &mut TcpStream, sent: &mut std::collections::HashSet<(i32, i32)>, ccx: i32, ccz: i32) {
    let mut vp = Vec::new(); write_varint(&mut vp, ccx as i64); write_varint(&mut vp, ccz as i64);
    let _ = send_packet(s, 0x54, vp);
    let mut news = Vec::new();
    for dx in -4..=4i32 { for dz in -4..=4i32 {
        let c = (ccx + dx, ccz + dz);
        if sent.len() > 4096 { sent.clear(); }
        if !sent.contains(&c) { news.push(c); sent.insert(c); }
    }}
    if news.is_empty() { return }
    let _ = send_packet(s, 0x0d, vec![]);
    for (cx, cz) in &news {
        if send_packet(s, 0x27, map_chunk(*cx, *cz)).is_err() { return }
    }
    let mut bf = Vec::new(); write_varint(&mut bf, news.len() as i64);
    let _ = send_packet(s, 0x0c, bf);
}

fn join_game() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&1i32.to_be_bytes());
    p.push(0);
    write_varint(&mut p, 1); write_str(&mut p, "minecraft:overworld");
    write_varint(&mut p, 20);
    write_varint(&mut p, 8);
    write_varint(&mut p, 8);
    p.push(0); p.push(1); p.push(0);
    p.extend_from_slice(&spawn_info());
    p.push(0);   // secure chat off
    p
}
fn spawn_info() -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, 0);
    write_str(&mut p, "minecraft:overworld");
    p.extend_from_slice(&0i64.to_be_bytes());
    p.push(1);   // creative
    p.push(255); // prev -1
    p.push(0); p.push(1); // isDebug=false, isFlat=true
    p.push(0);   // no death loc
    write_varint(&mut p, 0);
    p
}
fn hurt_anim(eid: i32) -> Vec<u8> {
    let mut p = Vec::new(); write_varint(&mut p, eid as i64); p.extend_from_slice(&0f32.to_be_bytes()); p
}
fn health_pkt(hp: f32, food: i32, sat: f32) -> Vec<u8> {
    let mut p = Vec::new(); p.extend_from_slice(&hp.to_be_bytes()); write_varint(&mut p, food as i64); p.extend_from_slice(&sat.to_be_bytes()); p
}
fn slot_wire(cnt: u8, item: i64) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, cnt as i64);
    if cnt > 0 { write_varint(&mut p, item); write_varint(&mut p, 0); write_varint(&mut p, 0); }
    p
}
// villager trades: (input item name, count, optional second input, output name, count)
fn trades() -> Vec<(i64, u8, Option<(i64, u8)>, i64, u8)> {
    let mut v = Vec::new();
    for (a, ac, b, c, cc) in [
        ("wheat", 20u8, None, "emerald", 1u8),
        ("rotten_flesh", 22, None, "emerald", 1),
        ("coal", 15, None, "emerald", 1),
        ("emerald", 1, None, "bread", 1),
        ("emerald", 3, None, "iron_ingot", 1),
        ("emerald", 5, None, "diamond", 1),
        ("iron_ingot", 4, None, "emerald", 1),
        ("emerald", 1, None, "arrow", 8),
    ] {
        let (ai, ci) = (name_to_item(a), name_to_item(c));
        if let (Some(ai), Some(ci)) = (ai, ci) {
            v.push((ai, ac, b.and_then(name_to_item).map(|i| (i, 1u8)), ci, cc));
        }
    }
    v
}
fn reduced_slot(p: &mut Vec<u8>, item: i64, cnt: u8) {
    write_varint(p, item); write_varint(p, cnt as i64); write_varint(p, 0);
}
fn inv_consume(inv: &mut HashMap<i16, (i64, u8)>, item: i64, need: i64) -> bool {
    let have: i64 = inv.values().filter(|&&(it, _)| it == item).map(|&(_, c)| c as i64).sum();
    if have < need { return false }
    let mut left = need;
    let keys: Vec<i16> = inv.iter().filter(|(_, &(it, _))| it == item).map(|(&k, _)| k).collect();
    for k in keys { if left <= 0 { break }
        let (it, c) = inv[&k];
        let take = (c as i64).min(left); left -= take;
        if take >= c as i64 { inv.remove(&k); } else { inv.insert(k, (it, c - take as u8)); }
    }
    true
}
fn trade_list_pkt(wid: u8) -> Vec<u8> {
    let mut p = vec![wid];
    let ts = trades();
    write_varint(&mut p, ts.len() as i64);
    for (a, ac, b, c, cc) in ts {
        reduced_slot(&mut p, a, ac);
        p.extend_from_slice(&slot_wire(cc, c));
        match b { Some((bi, bc)) => { p.push(1); reduced_slot(&mut p, bi, bc); } None => p.push(0) }
        p.push(0);                                  // tradeDisabled
        p.extend_from_slice(&0i32.to_be_bytes());   // uses
        p.extend_from_slice(&12i32.to_be_bytes());  // max uses
        p.extend_from_slice(&1i32.to_be_bytes());   // xp
        p.extend_from_slice(&0i32.to_be_bytes());   // specialPrice
        p.extend_from_slice(&0.0f32.to_be_bytes()); // priceMultiplier
        p.extend_from_slice(&0i32.to_be_bytes());   // demand
    }
    write_varint(&mut p, 1); // villagerLevel
    write_varint(&mut p, 0); // experience
    p.push(1); p.push(1);    // regular + canRestock
    p
}
fn open_chest_window(s: &mut TcpStream, pos: (i32,i32,i32), inv: &HashMap<i16,(i64,u8)>) {
    let mut p = Vec::new();
    write_varint(&mut p, 1);        // windowId
    write_varint(&mut p, 2);        // generic_9x3
    p.extend_from_slice(&chat_nbt("Chest"));
    let _ = send_packet(s, 0x33, p);
    // window_items: 27 chest + 27 inv(9-35) + 9 hotbar(36-44)
    let mut q = vec![1u8];
    write_varint(&mut q, 0);
    write_varint(&mut q, 63);
    let ch = chests().lock().unwrap().get(&pos).cloned().unwrap_or_default();
    for i in 0..27i16 {
        let (it, c) = ch.get(&i).copied().unwrap_or((0, 0));
        q.extend_from_slice(&slot_wire(c, it));
    }
    for wsl in 27..63i16 {
        let islot = wsl - 18;
        let (it, c) = inv.get(&islot).copied().unwrap_or((0, 0));
        q.extend_from_slice(&slot_wire(c, it));
    }
    q.extend_from_slice(&slot_wire(0, 0)); // carried
    let _ = send_packet(s, 0x13, q);
}
fn click_slot(m: &mut HashMap<i16, (i64, u8)>, cur: &mut (i64, u8), slot: i16, btn: i8) {
    let sv = m.get(&slot).copied();
    let cv = *cur;
    if btn == 0 {
        if cv.1 == 0 && sv.is_some() { *cur = sv.unwrap(); m.remove(&slot); }
        else if cv.1 > 0 && sv.is_none() { m.insert(slot, cv); *cur = (0, 0); }
        else if cv.1 > 0 && sv.is_some() {
            let (si, sc) = sv.unwrap();
            if si == cv.0 && sc + cv.1 <= 64 { m.insert(slot, (si, sc + cv.1)); *cur = (0, 0); }
            else { m.insert(slot, cv); *cur = (si, sc); }
        }
    } else if btn == 1 {
        if cv.1 == 0 && sv.is_some() {
            let (si, sc) = sv.unwrap();
            let take = sc - sc / 2;
            *cur = (si, take);
            if sc - take == 0 { m.remove(&slot); } else { m.insert(slot, (si, sc - take)); }
        } else if cv.1 > 0 {
            if sv.is_none() { m.insert(slot, (cv.0, 1)); *cur = (cv.0, cv.1 - 1); }
            else { let (si, sc) = sv.unwrap();
                if si == cv.0 && sc < 64 { m.insert(slot, (si, sc + 1)); *cur = (cv.0, cv.1 - 1); } }
        }
    }
}
fn craft_result(grid: &HashMap<i16, (i64, u8)>) -> Option<(i64, u8)> {
    let planks = name_to_item("oak_planks")?;
    let log = name_to_item("oak_log")?;
    let stick = name_to_item("stick")?;
    let table = name_to_item("crafting_table")?;
    let vals: Vec<(i16, i64)> = (1..=4).filter_map(|i| grid.get(&i).map(|&(it, _)| (i, it))).collect();
    if vals.len() == 1 && vals[0].1 == log { return Some((planks, 4)) }
    if vals.len() == 4 && vals.iter().all(|v| v.1 == planks) { return Some((table, 1)) }
    // sticks: two planks vertical (1&3 or 2&4)
    if vals.len() == 2 && vals.iter().all(|v| v.1 == planks)
        && ((vals[0].0 == 1 && vals[1].0 == 3) || (vals[0].0 == 2 && vals[1].0 == 4)) {
        return Some((stick, 4))
    }
    None
}
fn open_furnace_window(s: &mut TcpStream, pos: (i32,i32,i32), inv: &HashMap<i16,(i64,u8)>) {
    let mut p = Vec::new();
    write_varint(&mut p, 2);        // windowId 2 (chest uses 1)
    write_varint(&mut p, 14);       // furnace menu
    p.extend_from_slice(&chat_nbt("Furnace"));
    let _ = send_packet(s, 0x33, p);
    furnace_push_items(s, pos, inv);
}
fn furnace_push_items(s: &mut TcpStream, pos: (i32,i32,i32), inv: &HashMap<i16,(i64,u8)>) {
    let mut q = vec![2u8];
    write_varint(&mut q, 0);
    write_varint(&mut q, 39);
    let (fi, fu, fo) = {
        let fs = furnaces().lock().unwrap();
        let f = fs.get(&pos);
        (f.map(|f| f.input).unwrap_or((0,0)), f.map(|f| f.fuel).unwrap_or((0,0)), f.map(|f| f.output).unwrap_or((0,0)))
    };
    q.extend_from_slice(&slot_wire(fi.1, fi.0));
    q.extend_from_slice(&slot_wire(fu.1, fu.0));
    q.extend_from_slice(&slot_wire(fo.1, fo.0));
    for wsl in 3..39i16 {
        let (it, c) = inv.get(&(wsl + 6)).copied().unwrap_or((0, 0));
        q.extend_from_slice(&slot_wire(c, it));
    }
    q.extend_from_slice(&slot_wire(0, 0));
    let _ = send_packet(s, 0x13, q);
}
fn craft3_result(g: &HashMap<i16, (i64, u8)>) -> Option<(i64, u8)> {
    let at = |i: i16| g.get(&i).map(|e| e.0).unwrap_or(0);
    let is = |i: i16, n: &str| name_to_item(n) == Some(at(i));
    let cnt = |n: &str| -> i64 { let id = name_to_item(n).unwrap_or(-1); (1..=9).filter(|&i| at(i) == id).count() as i64 };
    let air = |i: i16| at(i) == 0;
    // chest / furnace: full ring around empty center
    for (mat, res) in [("oak_planks", "chest"), ("cobblestone", "furnace")] {
        let ok = [1,2,3,4,6,7,8,9].iter().all(|&i| is(i, mat)) && air(5);
        if ok { return name_to_item(res).map(|r| (r, 1)) }
    }
    // torch: coal directly above stick
    for (c, st) in [(1,4),(2,5),(3,6),(4,7),(5,8),(6,9)] {
        if is(c, "coal") && is(st, "stick") && (1..=9).filter(|&i| !air(i)).count() == 2 {
            return name_to_item("torch").map(|r| (r, 4))
        }
    }
    // bread: 3 wheat in a row
    for (a, b, c) in [(1i16,2,3),(4,5,6),(7,8,9)] {
        if is(a,"wheat") && is(b,"wheat") && is(c,"wheat")
            && (1..=9).filter(|&i| !air(i)).count() == 3 {
            return name_to_item("bread").map(|r| (r, 1))
        }
    }
    // generic tool/armor recipes per material
    for (mat, prefix) in [("oak_planks","wooden"),("cobblestone","stone"),("iron_ingot","iron"),("gold_ingot","golden"),("diamond","diamond")] {
        let stick = |i: i16| is(i, "stick");
        let m = |i: i16| is(i, mat);
        // pickaxe: mat mat mat / _ stick _ / _ stick _
        if m(1)&&m(2)&&m(3)&&stick(5)&&stick(8)&&air(4)&&air(6)&&air(7)&&air(9) {
            return name_to_item(&format!("{}_pickaxe", prefix)).map(|r| (r,1))
        }
        // axe: mat mat / mat stick / _ stick (either column)
        if ((m(1)&&m(2)&&m(4)&&stick(5)&&stick(8)&&air(3)&&air(6)&&air(7)&&air(9))
         || (m(2)&&m(3)&&m(6)&&stick(5)&&stick(8)&&air(1)&&air(4)&&air(7)&&air(9))) {
            return name_to_item(&format!("{}_axe", prefix)).map(|r| (r,1))
        }
        // shovel: mat / stick / stick, any column
        for col in [0i16,1,2] {
            if m(1+col)&&stick(4+col)&&stick(7+col)
                && (1..=9).filter(|&i| !air(i)).count() == 3 {
                return name_to_item(&format!("{}_shovel", prefix)).map(|r| (r,1))
            }
        }
        // sword: mat / mat / stick, any column
        for col in [0i16,1,2] {
            if m(1+col)&&m(4+col)&&stick(7+col)
                && (1..=9).filter(|&i| !air(i)).count() == 3 {
                return name_to_item(&format!("{}_sword", prefix)).map(|r| (r,1))
            }
        }
        // hoe: mat mat / _ stick / _ stick (or mirrored)
        if ((m(1)&&m(2)&&stick(5)&&stick(8)&&air(3)&&air(4)&&air(6)&&air(7)&&air(9))
         || (m(2)&&m(3)&&stick(5)&&stick(8)&&air(1)&&air(4)&&air(6)&&air(7)&&air(9))) {
            return name_to_item(&format!("{}_hoe", prefix)).map(|r| (r,1))
        }
        // armor: helmet=5, chest=8, leggings=7, boots=4
        if mat != "cobblestone" {
            if m(1)&&m(2)&&m(3)&&m(4)&&m(6)&&air(5)&&air(7)&&air(8)&&air(9) {
                return name_to_item(&format!("{}_helmet", prefix)).map(|r| (r,1))
            }
            if m(1)&&m(3)&&m(4)&&m(5)&&m(6)&&m(7)&&m(8)&&m(9)&&air(2) {
                return name_to_item(&format!("{}_chestplate", prefix)).map(|r| (r,1))
            }
            if m(1)&&m(2)&&m(3)&&m(4)&&m(6)&&m(7)&&m(9)&&air(5)&&air(8) {
                return name_to_item(&format!("{}_leggings", prefix)).map(|r| (r,1))
            }
            if m(4)&&m(6)&&m(7)&&m(9)&&(1..=9).filter(|&i| !air(i)).count()==4 {
                return name_to_item(&format!("{}_boots", prefix)).map(|r| (r,1))
            }
        }
    }
    // leather armor pieces on their own
    {
        let l = |i: i16| is(i, "leather");
        if l(1)&&l(2)&&l(3)&&l(4)&&l(6)&&air(5)&&air(7)&&air(8)&&air(9) { return name_to_item("leather_helmet").map(|r|(r,1)) }
        if l(1)&&l(3)&&l(4)&&l(5)&&l(6)&&l(7)&&l(8)&&l(9)&&air(2) { return name_to_item("leather_chestplate").map(|r|(r,1)) }
        if l(1)&&l(2)&&l(3)&&l(4)&&l(6)&&l(7)&&l(9)&&air(5)&&air(8) { return name_to_item("leather_leggings").map(|r|(r,1)) }
        if l(4)&&l(6)&&l(7)&&l(9)&&(1..=9).filter(|&i| !air(i)).count()==4 { return name_to_item("leather_boots").map(|r|(r,1)) }
    }
    // bucket: iron V-shape
    if is(4,"iron_ingot")&&is(6,"iron_ingot")&&is(8,"iron_ingot")&&(1..=9).filter(|&i| !air(i)).count()==3 {
        return name_to_item("bucket").map(|r| (r,1))
    }
    // shears: 2 iron diagonal-ish
    if ((is(5,"iron_ingot")&&is(8,"iron_ingot"))||(is(5,"iron_ingot")&&is(9,"iron_ingot")))
        && (1..=9).filter(|&i| !air(i)).count()==2 {
        return name_to_item("shears").map(|r| (r,1))
    }
    // shield: planks across + iron center-top
    if is(1,"oak_planks")&&is(2,"iron_ingot")&&is(3,"oak_planks")&&is(4,"oak_planks")&&is(5,"oak_planks")&&is(6,"oak_planks")&&is(8,"oak_planks")&&air(7)&&air(9) {
        return name_to_item("shield").map(|r| (r,1))
    }
    // fishing rod: 3 sticks diagonal + 2 string
    if is(3,"stick")&&is(5,"stick")&&is(7,"stick")&&is(6,"string")&&is(9,"string")
        && (1..=9).filter(|&i| !air(i)).count()==5 {
        return name_to_item("fishing_rod").map(|r| (r,1))
    }
    // bow: sticks + strings
    if is(2,"stick")&&is(4,"stick")&&is(8,"stick")&&is(3,"string")&&is(6,"string")&&is(9,"string")&&air(1)&&air(5)&&air(7) {
        return name_to_item("bow").map(|r| (r,1))
    }
    // also accept 2x2-style recipes in the big grid
    let g2: HashMap<i16,(i64,u8)> = g.clone();
    if cnt("oak_log") == 1 && (1..=9).filter(|&i| !air(i)).count() == 1 {
        return name_to_item("oak_planks").map(|r| (r, 4))
    }
    if cnt("oak_planks") == 4 && (1..=9).filter(|&i| !air(i)).count() == 4 {
        // any 2x2 block of planks -> crafting_table
        for (a,b,c,d) in [(1i16,2,4,5),(2,3,5,6),(4,5,7,8),(5,6,8,9)] {
            if is(a,"oak_planks")&&is(b,"oak_planks")&&is(c,"oak_planks")&&is(d,"oak_planks") {
                return name_to_item("crafting_table").map(|r| (r,1))
            }
        }
    }
    let _ = g2;
    None
}
fn health_pkt_c(hp: f32, c: &Client) -> Vec<u8> {
    health_pkt(hp, *c.food.lock().unwrap(), *c.sat.lock().unwrap())
}
fn food_value(item: i64) -> i32 {
    for (n, v) in [("cooked_beef",8),("cooked_porkchop",8),("cooked_chicken",6),("cooked_mutton",6),
                   ("bread",5),("baked_potato",5),("cooked_rabbit",5),("apple",4),("rotten_flesh",4),
                   ("carrot",3),("beef",3),("porkchop",3),("rabbit",3),("mutton",2),("melon_slice",2),
                   ("cookie",2),("chicken",1),("potato",1),("beetroot",1),("dried_kelp",1)] {
        if name_to_item(n) == Some(item) { return v }
    }
    0
}

fn chat_nbt(text: &str) -> Vec<u8> {
    // {"text":"..."} as anonymousNbt compound
    let mut v = vec![10u8];
    v.push(8); // TAG_String
    nbt_str(&mut v, "text");
    nbt_str(&mut v, text);
    v.push(0);
    v
}
fn teleport_pkt(x: f64, y: f64, z: f64, yaw: f32, pitch: f32, tid: i64) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.extend_from_slice(&yaw.to_be_bytes());
    p.extend_from_slice(&pitch.to_be_bytes());
    p.push(0);
    write_varint(&mut p, tid);
    p
}

// resolve @selector -> player eids or mob eids ("m:<eid>" prefixed for mobs)
fn resolve_selector(sel: &str, ctx: Option<&Arc<Client>>) -> Vec<String> {
    if let Some(inner) = sel.strip_prefix("@e[type=").and_then(|x| x.strip_suffix(']')) {
        if inner == "player" {
            return clients().lock().unwrap().iter().map(|c| c.eid.to_string()).collect();
        }
        let et = name_to_etype(inner).unwrap_or(-1);
        return mobs().lock().unwrap().iter().filter(|m| m.etype == et).map(|m| format!("m:{}", m.eid)).collect();
    }
    let cls = clients().lock().unwrap();
    if sel == "@p" {
        if let Some(c) = ctx {
            let pp = *c.pos.lock().unwrap();
            let mut best: Option<(f64, i32)> = None;
            for cl in cls.iter() {
                let cp = *cl.pos.lock().unwrap();
                let d = (cp.0-pp.0).powi(2) + (cp.2-pp.2).powi(2);
                if best.map(|b| d < b.0).unwrap_or(true) { best = Some((d, cl.eid)); }
            }
            if let Some((_, e)) = best { return vec![e.to_string()] }
        }
        return cls.first().map(|c| vec![c.eid.to_string()]).unwrap_or_default();
    }
    if sel == "@a" || sel == "@e" {
        return cls.iter().map(|c| c.eid.to_string()).collect();
    }
    if sel == "@s" {
        return ctx.map(|c| vec![c.eid.to_string()]).unwrap_or_default();
    }
    vec![]
}

// execution context: who + where (for ~ coords)
#[derive(Clone)]
struct CmdCtx { me: Option<Arc<Client>>, x: f64, y: f64, z: f64 }
fn coord(tok: &str, base: f64) -> f64 {
    if let Some(off) = tok.strip_prefix('~') { base + off.parse::<f64>().unwrap_or(0.0) }
    else { tok.parse::<f64>().unwrap_or(0.0) }
}

// ---------- scoreboard ----------
static SCORES: OnceLock<Mutex<HashMap<String, HashMap<String, i64>>>> = OnceLock::new();
fn scores() -> &'static Mutex<HashMap<String, HashMap<String, i64>>> {
    SCORES.get_or_init(|| Mutex::new(HashMap::new()))
}

// execute subset: as/at/positioned/if|unless block|entity|score ... run <cmd>
fn run_execute(ctx: &CmdCtx, args: &[&str], s: &mut Option<&mut TcpStream>) -> String {
    let mut i = 0;
    let mut cur = ctx.clone();
    let mut targets: Vec<String> = vec![]; // empty = self/console
    let mut passed = true;
    while i < args.len() {
        match args[i] {
            "run" => break,
            "as" => {
                i += 1;
                targets = resolve_selector(args.get(i).copied().unwrap_or(""), cur.me.as_ref());
            }
            "at" => {
                i += 1;
                let ts = resolve_selector(args.get(i).copied().unwrap_or(""), cur.me.as_ref());
                if let Some(t) = ts.first() {
                    if let Some(eid) = t.strip_prefix("m:").and_then(|e| e.parse::<i32>().ok()) {
                        if let Some(p) = mobs().lock().unwrap().iter().find(|m| m.eid == eid) {
                            let p = *p.pos.lock().unwrap(); cur.x = p.0; cur.y = p.1; cur.z = p.2;
                        }
                    } else if let Ok(eid) = t.parse::<i32>() {
                        let cls = clients().lock().unwrap();
                        if let Some(c) = cls.iter().find(|c| c.eid == eid) {
                            let p = *c.pos.lock().unwrap(); cur.x = p.0; cur.y = p.1; cur.z = p.2;
                        }
                    }
                }
            }
            "positioned" => {
                cur.x = coord(args.get(i+1).copied().unwrap_or("0"), cur.x);
                cur.y = coord(args.get(i+2).copied().unwrap_or("0"), cur.y);
                cur.z = coord(args.get(i+3).copied().unwrap_or("0"), cur.z);
                i += 3;
            }
            "if" | "unless" => {
                let neg = args[i] == "unless";
                i += 1;
                let ok = match args.get(i).copied().unwrap_or("") {
                    "block" => {
                        let (x, y, z) = (coord(args.get(i+1).copied().unwrap_or("~"), cur.x) as i32,
                                         coord(args.get(i+2).copied().unwrap_or("~"), cur.y) as i32,
                                         coord(args.get(i+3).copied().unwrap_or("~"), cur.z) as i32);
                        i += 3;
                        let want = args.get(i+1).copied().unwrap_or("");
                        let st = block_at(x, y, z);
                        name_to_state(want).map(|w| st == w).unwrap_or(false)
                    }
                    "entity" => {
                        i += 1;
                        !resolve_selector(args.get(i+1).copied().unwrap_or(""), cur.me.as_ref()).is_empty()
                    }
                    "score" => {
                        // if score <target> <obj> matches <range>
                        let t = args.get(i+1).copied().unwrap_or("");
                        let obj = args.get(i+2).copied().unwrap_or("");
                        i += 2;
                        if args.get(i+1).copied() == Some("matches") {
                            i += 1;
                            let range = args.get(i+1).copied().unwrap_or("0");
                            let entry = match t { "@s" => cur.me.as_ref().map(|c| c.name.clone()).unwrap_or_default(), n => n.to_string() };
                            let v = scores().lock().unwrap().get(obj).and_then(|m| m.get(&entry)).copied().unwrap_or(0);
                            in_range(v, range)
                        } else { false }
                    }
                    _ => false,
                };
                if ok == neg { passed = false; }
            }
            _ => {}
        }
        i += 1;
    }
    if i >= args.len() { return "missing 'run'".into() }
    let cmd = args[i+1..].join(" ");
    if !passed { return "Test failed".into() }
    let mut n = 0;
    if targets.is_empty() {
        run_command_ctx(&cur, &format!("/{}", cmd), s);
        n += 1;
    } else {
        for t in &targets {
            if t.starts_with("m:") { continue } // mob-as-executor unsupported yet
            let eid: i32 = t.parse().unwrap_or(-1);
            let cls = clients().lock().unwrap();
            if let Some(c) = cls.iter().find(|c| c.eid == eid).cloned() {
                let p = *c.pos.lock().unwrap();
                let cc = CmdCtx { me: Some(c), x: p.0, y: p.1, z: p.2 };
                run_command_ctx(&cc, &format!("/{}", cmd), s);
                n += 1;
            }
        }
    }
    format!("executed {} command(s)", n)
}
fn in_range(v: i64, r: &str) -> bool {
    if let Some((a, b)) = r.split_once("..") {
        let lo = a.parse::<i64>().unwrap_or(i64::MIN);
        let hi = b.parse::<i64>().unwrap_or(i64::MAX);
        v >= lo && v <= hi
    } else { v == r.parse().unwrap_or(i64::MIN + 1) }
}

// minimal brigadier seed: /time /tp /say /kill /gamemode /list /seed
fn run_command(me: Option<&Arc<Client>>, msg: &str, s: &mut Option<&mut TcpStream>) {
    let ctx = match me {
        Some(c) => { let p = *c.pos.lock().unwrap(); CmdCtx { me: Some(c.clone()), x: p.0, y: p.1, z: p.2 } }
        None => CmdCtx { me: None, x: 0.5, y: 16.0, z: 0.5 },
    };
    run_command_ctx(&ctx, msg, s)
}
fn run_command_ctx(ctx: &CmdCtx, msg: &str, s: &mut Option<&mut TcpStream>) {
    let me = ctx.me.as_ref();
    let mut it = msg[1..].split_whitespace();
    let cmd = it.next().unwrap_or("");

    let args: Vec<&str> = it.collect();
    let reply = match cmd {
        "execute" => run_execute(ctx, &args, s),
        "time" => {
            if args.first() == Some(&"set") {
                let v = match args.get(1).copied().unwrap_or("") {
                    "day" => 1000, "night" => 13000, "noon" => 6000, "midnight" => 18000,
                    n => n.parse::<u64>().unwrap_or(1000),
                };
                WORLD_TIME.store(v, Ordering::Relaxed);
                format!("Set time to {}", v)
            } else { "usage: /time set <n|day|night>".into() }
        }
        "tp" | "teleport" => {
            let (targets, xyz): (Vec<String>, &[&str]) = if args.first().map(|a| a.starts_with('@')).unwrap_or(false) {
                (resolve_selector(args[0], me), &args[1..])
            } else if let Some(me) = me { (vec![me.eid.to_string()], &args[..]) } else { (vec![], &args[..]) };
            if xyz.len() < 3 { "usage: /tp [@target] x y z".into() } else {
                let (x, y, z) = (coord(xyz[0], ctx.x), coord(xyz[1], ctx.y), coord(xyz[2], ctx.z));
                let mut n = 0;
                let cls = clients().lock().unwrap();
                for t in &targets {
                    if let Some(c) = cls.iter().find(|c| c.eid.to_string() == *t) {
                        { let mut p = c.pos.lock().unwrap(); *p = (x, y, z, p.3, p.4, true); }
                        if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut *w, 0x40, teleport_pkt(x, y, z, 0.0, 0.0, 100)); }
                        n += 1;
                    }
                }
                format!("Teleported {} target(s)", n)
            }
        }
        "say" => {
            let line = format!("[Server] {}", args.join(" "));
            let pl = system_chat(&line);
            broadcast(-1, |_c| (0x6c, pl.clone()));
            return;
        }
        "kill" => {
            let mut killed = 0;
            let targets = match args.first() {
                Some(sel) if sel.starts_with('@') => resolve_selector(sel, me),
                _ => me.map(|c| vec![c.eid.to_string()]).unwrap_or_default(),
            };
            for t in targets {
                if let Some(eid) = t.strip_prefix("m:").and_then(|e| e.parse::<i32>().ok()) {
                    mobs().lock().unwrap().retain(|m| m.eid != eid);
                    let mut p = Vec::new(); write_varint(&mut p, 1); write_varint(&mut p, eid as i64);
                    broadcast(-1, |_c| (0x42, p.clone()));
                    killed += 1;
                } else if let Ok(eid) = t.parse::<i32>() {
                    let cls: Vec<Arc<Client>> = { clients().lock().unwrap().iter().cloned().collect() };
                    if let Some(c) = cls.iter().find(|c| c.eid == eid) {
                        *c.hp.lock().unwrap() = 0.0;
                        if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut *w, 0x5d, health_pkt_c(0.0, c)); }
                        killed += 1;
                    }
                }
            }
            format!("Killed {}", killed)
        }
        "gamemode" => {
            if let Some(s) = s.as_deref_mut() {
                let gm = args.first().copied().unwrap_or("");
                let (v, flags) = match gm { "survival"|"0" => (0f32, 0x00u8), "creative"|"1" => (1.0, 0x0f), "spectator"|"3" => (3.0, 0x00), _ => (1.0, 0x0f) };
                if let Some(me) = me { *me.gm.lock().unwrap() = v as u8; }
                let mut g = vec![3u8]; g.extend_from_slice(&v.to_be_bytes());
                let _ = send_packet(s, 0x22, g);
                let mut ab = vec![flags]; ab.extend_from_slice(&0.05f32.to_be_bytes()); ab.extend_from_slice(&0.1f32.to_be_bytes());
                format!("Set gamemode to {}", gm)
            } else { "player only".into() }
        }
        "setblock" => {
            if args.len() >= 4 {
                let (x, y, z) = (coord(args[0], ctx.x) as i32, coord(args[1], ctx.y) as i32, coord(args[2], ctx.z) as i32);
                let st = state_from_arg(args[3]).unwrap_or(0);
                set_block(x, y, z, st);
                broadcast(-1, |_c| (0x09, block_change(x, y, z, st)));
                format!("Set block at {} {} {}", x, y, z)
            } else { "usage: /setblock x y z <block>".into() }
        }
        "summon" => {
            if args.len() >= 4 {
                let et = name_to_etype(args[0]).unwrap_or(124);
                let (x, y, z) = (coord(args[1], ctx.x), coord(args[2], ctx.y), coord(args[3], ctx.z));
                if args[0] == "tnt" {
                    let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                    primed().lock().unwrap().push(Primed { eid, x, y, z, lit: Instant::now() });
                    broadcast(-1, |_c| (0x01, spawn_entity_pkt(eid, et as u32, x, y, z, 0.0, 0.0)));
                    format!("Summoned tnt")
                } else {
                    let m = Arc::new(Mob { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), uuid: [0u8;16],
                        etype: et, pos: Mutex::new((x, y, z, 0.0, 0.0)), hp: Mutex::new(20.0),
                        last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
                        fuse: Mutex::new(0), love: Mutex::new(0) });
                    let sp = spawn_mob(&m);
                    mobs().lock().unwrap().push(m);
                    broadcast(-1, |_c| (0x01, sp.clone()));
                    format!("Summoned {}", args[0])
                }
            } else { "usage: /summon <entity> x y z".into() }
        }
        "give" => {
            let (targets, item_name) = if args.first().map(|a| a.starts_with('@')).unwrap_or(false) {
                (resolve_selector(args[0], me), args.get(1).copied().unwrap_or(""))
            } else if args.first().map(|a| name_to_item(a).is_some()).unwrap_or(false) {
                (me.map(|c| vec![c.eid.to_string()]).unwrap_or_default(), args.first().copied().unwrap_or(""))
            } else {
                let name = args.first().copied().unwrap_or("");
                let t = { clients().lock().unwrap().iter().find(|c| c.name == name).map(|c| c.eid.to_string()) };
                (t.into_iter().collect(), args.get(1).copied().unwrap_or(""))
            };
            {
            let cls = clients().lock().unwrap();
            let mut gave = 0;
            for t in &targets {
                if let Some(c) = cls.iter().find(|c| c.eid.to_string() == *t) {
                if let Some(item) = name_to_item(item_name) {
                    let cnt = args.iter().rev().find_map(|a| a.parse::<u8>().ok()).unwrap_or(1);
                    let mut inv = c.inv.lock().unwrap();
                    let slot: i16 = (9..=44).find(|sl| !inv.contains_key(sl)).unwrap_or(9);
                    inv.insert(slot, (item, cnt));
                    let mut p = vec![0u8]; write_varint(&mut p, 0);
                    p.extend_from_slice(&slot.to_be_bytes());
                    write_varint(&mut p, cnt as i64); write_varint(&mut p, item);
                    write_varint(&mut p, 0); write_varint(&mut p, 0);
                    if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut *w, 0x15, p); }
                    gave += 1;
                }
                }
            }
            format!("Gave {} x{}", item_name, gave)
            }
        }
        "weather" => {
            let w = args.first().copied().unwrap_or("clear");
            let (code, ticks) = match w { "rain" => (1u8, 2400i32), "thunder" => (1, 2400), _ => (2u8, 0) };
            RAINING.store(ticks.max(0) as u64, Ordering::Relaxed);
            broadcast(-1, move |_c| { let mut p = vec![code]; p.extend_from_slice(&ticks.to_be_bytes()); (0x22, p.clone()) });
            format!("Weather: {}", w)
        }
        "function" => {
            if let Some(name) = args.first() {
                format!("ran {} commands", run_function(name, me.cloned(), s))
            } else { "usage: /function <ns:name>".into() }
        }
        "datapack" => {
            if args.first() == Some(&"list") {
                let ps = packs().lock().unwrap();
                let names: Vec<String> = ps.iter().map(|p| p.name.clone()).collect();
                format!("{} packs: {}", names.len(), names.join(", "))
            } else { "usage: /datapack list".into() }
        }
        "scoreboard" => {
            match args.first().copied().unwrap_or("") {
                "objectives" => {
                    match args.get(1).copied().unwrap_or("") {
                        "add" => {
                            let obj: String = args.get(2).copied().unwrap_or("obj").chars().take(64).collect();
                            let mut sc = scores().lock().unwrap();
                            if sc.len() < 1024 || sc.contains_key(&obj) { sc.entry(obj.clone()).or_default(); }
                            format!("Added objective {}", obj)
                        }
                        "remove" => {
                            let obj: String = args.get(2).copied().unwrap_or("").chars().take(64).collect();
                            scores().lock().unwrap().remove(&obj);
                            format!("Removed {}", obj)
                        }
                        "list" => {
                            let sc = scores().lock().unwrap();
                            format!("{} objectives: {}", sc.len(), sc.keys().cloned().collect::<Vec<_>>().join(", "))
                        }
                        _ => "usage: /scoreboard objectives add|remove|list".into(),
                    }
                }
                "players" => {
                    let op = args.get(1).copied().unwrap_or("");
                    let entry = args.get(2).copied().unwrap_or("");
                    let obj = args.get(3).copied().unwrap_or("");
                    let name: String = match entry { "@s" => ctx.me.as_ref().map(|c| c.name.clone()).unwrap_or_default(), "@p" => ctx.me.as_ref().map(|c| c.name.clone()).unwrap_or_default(), n => n.chars().take(64).collect() };
                    let obj: String = obj.chars().take(64).collect();
                    let mut sc = scores().lock().unwrap();
                    match op {
                        "set" => { let v = args.get(4).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
                            let full = sc.get(&obj).map(|m| m.len()).unwrap_or(0) >= 4096 && sc.get(&obj).and_then(|m| m.get(&name)).is_none();
                            if !full { sc.entry(obj.clone()).or_default().insert(name.clone(), v); }
                            format!("Set {} = {}", name, v) }
                        "add" => { let v = args.get(4).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
                            let full = sc.get(&obj).map(|m| m.len()).unwrap_or(0) >= 4096 && sc.get(&obj).and_then(|m| m.get(&name)).is_none();
                            if full { format!("{} = (cap)", name) } else {
                            let e = sc.entry(obj.clone()).or_default().entry(name.clone()).or_insert(0); *e += v; format!("{} = {}", name, e) } }
                        "get" => { let v = sc.get(&obj).and_then(|m| m.get(&name)).copied().unwrap_or(0); format!("{} = {}", name, v) }
                        "reset" => { if let Some(m) = sc.get_mut(&obj) { m.remove(&name); } format!("Reset {}", name) }
                        _ => "usage: /scoreboard players set|add|get|reset".into(),
                    }
                }
                _ => "usage: /scoreboard objectives|players".into(),
            }
        }
        "list" => {
            let c = clients().lock().unwrap();
            let names: Vec<String> = c.iter().map(|x| x.name.clone()).collect();
            format!("{} players: {}", names.len(), names.join(", "))
        }
        "seed" => "0".into(),
        "" => "".into(),
        _ => format!("Unknown command: {}", cmd),
    };
    if !reply.is_empty() {
        if let Some(s) = s.as_deref_mut() {
            let _ = send_packet(s, 0x6c, system_chat(&reply));
        } else {
            eprintln!("[cmd] {}", reply);
        }
    }
}

fn system_chat(text: &str) -> Vec<u8> {
    let mut p = chat_nbt(text); p.push(0); p
}
fn ack(seq: i64) -> Vec<u8> { let mut v = Vec::new(); write_varint(&mut v, seq); v }
fn block_change(x: i32, y: i32, z: i32, st: u32) -> Vec<u8> {
    let mut v = Vec::new(); write_pos(&mut v, x, y, z); write_varint(&mut v, st as i64); v
}

const FACE_OFF: [(i32, i32, i32); 6] = [(0, -1, 0), (0, 1, 0), (0, 0, -1), (0, 0, 1), (-1, 0, 0), (1, 0, 0)];

fn handle(mut s: TcpStream) {
    s.set_nodelay(true).ok();
    let (id, hs) = match read_packet(&mut s) { Some(x) => x, None => return };
    if id != 0 { return }
    let mut p = &hs[..];
    let _proto = read_varint(&mut p);
    let alen = read_varint(&mut p).unwrap_or(0) as usize;
    if p.len() < alen + 3 { return }
    let next = p[alen + 2] as i64;
    if next == 1 {
        if let Some((0, _)) = read_packet(&mut s) {
            let json = "{\"version\":{\"name\":\"1.21.1\",\"protocol\":767},\"players\":{\"max\":20,\"online\":0},\"description\":{\"text\":\"mc-spine\"}}";
            let mut pl = Vec::new(); write_str(&mut pl, json);
            let _ = send_packet(&mut s, 0, pl);
        }
        if let Some((1, ping)) = read_packet(&mut s) { let _ = send_packet(&mut s, 1, ping); }
        return;
    }
    if next != 2 { return }
    // ---- login ----
    let (lid, lbody) = match read_packet(&mut s) { Some(x) => x, None => return };
    if lid != 0 { return }
    let mut lp = &lbody[..];
    let nlen = read_varint(&mut lp).unwrap_or(0) as usize;
    let name: String = String::from_utf8_lossy(&lp[..nlen.min(lp.len())]).chars().take(64).collect();
    let uuid_off = nlen.min(lp.len());
    let uuid = if lp.len() >= uuid_off + 16 { &lp[uuid_off..uuid_off + 16] } else { &[0u8; 16] };
    let mut ls = Vec::new();
    ls.extend_from_slice(uuid); write_str(&mut ls, &name);
    write_varint(&mut ls, 0); ls.push(0);
    if send_packet(&mut s, 2, ls).is_err() { return }
    match read_packet(&mut s) { Some((3, _)) => {}, _ => return }
    // ---- config ----
    let cfg = load_config_packets();
    let mut sent_selpacks = false;
    for (pid, pkt) in &cfg {
        if *pid == 0x0e { let _ = send_raw(&mut s, pkt); sent_selpacks = true; break; }
        let _ = send_raw(&mut s, pkt);
    }
    if !sent_selpacks { return }
    s.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let t0 = Instant::now();
    let mut got_selpacks = false;
    while Instant::now() - t0 < Duration::from_secs(15) {
        match read_packet(&mut s) {
            Some((0x07, _)) => { got_selpacks = true; break }
            Some(_) => continue,
            None => continue,
        }
    }
    if !got_selpacks { return }
    for (pid, pkt) in &cfg {
        if matches!(*pid, 0x01 | 0x0c | 0x0e | 0x03) { continue }
        let _ = send_raw(&mut s, pkt);
    }
    if send_packet(&mut s, 0x03, vec![]).is_err() { return }
    let t0 = Instant::now();
    loop {
        if Instant::now() - t0 > Duration::from_secs(15) { return }
        match read_packet(&mut s) {
            Some((0x03, _)) => break,
            Some(_) => continue,
            None => continue,
        }
    }
    // ---- play ----
    if send_packet(&mut s, 0x2b, join_game()).is_err() { return }
    let _ = send_packet(&mut s, 0x53, vec![0]); // held slot 0
    let mut vp = Vec::new(); write_varint(&mut vp, 0); write_varint(&mut vp, 0);
    let _ = send_packet(&mut s, 0x54, vp);
    let mut sp = Vec::new(); write_pos(&mut sp, 0, 16, 0); sp.extend_from_slice(&0f32.to_be_bytes());
    let _ = send_packet(&mut s, 0x56, sp);
    let mut gs = vec![13u8]; gs.extend_from_slice(&0f32.to_be_bytes());
    let _ = send_packet(&mut s, 0x22, gs);
    let mut sent: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
    stream_chunks(&mut s, &mut sent, 0, 0);
    let mut tm = Vec::new(); tm.extend_from_slice(&0i64.to_be_bytes()); tm.extend_from_slice(&6000i64.to_be_bytes());
    let _ = send_packet(&mut s, 0x64, tm);
    let mut ps = Vec::new();
    ps.extend_from_slice(&0.5f64.to_be_bytes());
    ps.extend_from_slice(&16f64.to_be_bytes());
    ps.extend_from_slice(&0.5f64.to_be_bytes());
    ps.extend_from_slice(&0f32.to_be_bytes());
    ps.extend_from_slice(&0f32.to_be_bytes());
    ps.push(0);
    write_varint(&mut ps, 1);
    let _ = send_packet(&mut s, 0x40, ps);
    let mut ab = vec![0x0du8]; ab.extend_from_slice(&0.05f32.to_be_bytes()); ab.extend_from_slice(&0.1f32.to_be_bytes());
    let _ = send_packet(&mut s, 0x38, ab);
    let _ = send_packet(&mut s, 0x0b, vec![1u8, 0u8]);
    let mut xp = Vec::new(); xp.extend_from_slice(&0f32.to_be_bytes()); write_varint(&mut xp, 0); write_varint(&mut xp, 0);
    let _ = send_packet(&mut s, 0x5c, xp);
    let mut hp = Vec::new(); hp.extend_from_slice(&20f32.to_be_bytes()); write_varint(&mut hp, 20); hp.extend_from_slice(&5f32.to_be_bytes());
    let _ = send_packet(&mut s, 0x5d, hp);
    if let Some(rp) = resource_pack_pkt() { let _ = send_packet(&mut s, 0x46, rp); }
    // restore saved inventory for this name
    let saved_inv = inv_store().lock().unwrap().get(&name).cloned();
    eprintln!("[spine] {} joined", name);
    // ---- multiplayer: register self, tell others, learn others ----
    let me = Arc::new(Client {
        eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
        uuid: {
            let mut u = [0u8; 16];
            let n = uuid.len().min(16); u[..n].copy_from_slice(&uuid[..n]); u
        },
        name: name.clone(),
        pos: Mutex::new((0.5, surface_y(0, 0), 0.5, 0.0, 0.0, true)),
        hp: Mutex::new(20.0),
        sock: Mutex::new(s.try_clone().unwrap()),
        inv: Mutex::new(saved_inv.unwrap_or_default()),
        cursor: Mutex::new((0, 0)),
        open_chest: Mutex::new(None),
        open_furnace: Mutex::new(None),
        craft: Mutex::new(HashMap::new()),
        craft9: Mutex::new(HashMap::new()),
        open_table: Mutex::new(false),
        open_merchant: Mutex::new(false),
        trade_sel: Mutex::new(-1),
        merchant_in: Mutex::new([(0, 0), (0, 0)]), bow_draw: Mutex::new(false), blocking: Mutex::new(false), sprinting: Mutex::new(false),
        xp: Mutex::new(0),
        gm: Mutex::new(1),
        fall_from: Mutex::new(surface_y(0, 0)),
        food: Mutex::new(20),
        air: Mutex::new(60),
        armor_pts: Mutex::new(0),
        sat: Mutex::new(5.0),
        walk_acc: Mutex::new(0.0),
        lastp: Mutex::new((0.0, 0.0, 0.0)),
        spawn_pt: Mutex::new(None),
    held: Mutex::new(0),
    });
    {
        let mut cls = clients().lock().unwrap();
        for c in cls.iter() {
            // teach ME about each existing client
            let _ = send_packet(&mut s, 0x3e, player_info_add(c));
            let _ = send_packet(&mut s, 0x01, spawn_player(c));
            // teach THEM about me
            let pi = player_info_add(&me); let sp = spawn_player(&me);
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut *w, 0x3e, pi.clone());
                let _ = send_packet(&mut *w, 0x01, sp.clone());
            }
        }
        cls.push(me.clone());
    }
    // refresh armor points from restored inventory
    { let iv = me.inv.lock().unwrap(); *me.armor_pts.lock().unwrap() = armor_pts_of(&iv); }
    // echo restored inventory slots so the client sees them
    {
        let inv = me.inv.lock().unwrap();
        for (&slot, &(item, cnt)) in inv.iter() {
            let mut p = vec![0u8];
            write_varint(&mut p, 0);
            p.extend_from_slice(&slot.to_be_bytes());
            write_varint(&mut p, cnt as i64);
            write_varint(&mut p, item);
            write_varint(&mut p, 0); write_varint(&mut p, 0);
            let _ = send_packet(&mut s, 0x15, p);
        }
    }
    {
        let ms = mobs().lock().unwrap();
        for m in ms.iter() { let _ = send_packet(&mut s, 0x01, spawn_mob(m)); }
        drop(ms);
        for d in drops().lock().unwrap().iter() {
            let _ = send_packet(&mut s, 0x01, spawn_drop_pkt(d));
            let _ = send_packet(&mut s, 0x58, drop_meta_pkt(d));
        }
    }
    let me_cleanup = me.clone();
    // ---- consume loop ----
    s.set_read_timeout(Some(Duration::from_secs(2))).ok();
    s.set_write_timeout(Some(Duration::from_secs(2))).ok();
    let mut last_ka = Instant::now();
    let mut held_slot: i16 = 0;

    let mut player_chunk: (i32, i32) = (0, 0);
    let mut rx = RxBuf::new();
    'conn: loop {
        if last_ka.elapsed() > Duration::from_secs(8) {
            let mut ka = Vec::new(); ka.extend_from_slice(&1i64.to_be_bytes());
            if send_packet(&mut s, 0x26, ka).is_err() { break 'conn }
            last_ka = Instant::now();
        }
        let (pid, b) = match rx.next(&mut s) {
            RxOut::Closed => break 'conn,
            RxOut::Idle => continue,
            RxOut::Frame(pid, b) => (pid, b),
        };

        match pid {
            0x1a | 0x1b => { // position / position_look: f64 x,y,z first
                if b.len() >= 24 {
                    let px = f64::from_be_bytes(b[..8].try_into().unwrap());
                    let py = f64::from_be_bytes(b[8..16].try_into().unwrap());
                    let pz = f64::from_be_bytes(b[16..24].try_into().unwrap());
                    let (mut yaw, mut pitch, mut gnd) = { let p = me.pos.lock().unwrap(); (p.3, p.4, p.5) };
                    if pid == 0x1b && b.len() >= 33 {
                        yaw = f32::from_be_bytes(b[24..28].try_into().unwrap());
                        pitch = f32::from_be_bytes(b[28..32].try_into().unwrap());
                        gnd = b[32] != 0;
                    } else if pid == 0x1a && b.len() >= 25 {
                        gnd = b[24] != 0;
                    }
                    {
                        let prev_gnd = { let p = me.pos.lock().unwrap(); p.5 };
                        *me.pos.lock().unwrap() = (px, py, pz, yaw, pitch, gnd);
                        let mut ff = me.fall_from.lock().unwrap();
                        if !gnd && py > *ff { *ff = py; }
                        if gnd && !prev_gnd {
                            let fall = *ff - py;
                            *ff = py;
                            let wst = name_to_state("water").unwrap_or(u32::MAX);
                            let wet_land = block_at(px.floor() as i32, (py - 0.5).floor() as i32, pz.floor() as i32) == wst
                                || block_at(px.floor() as i32, py.floor() as i32, pz.floor() as i32) == wst;
                            if fall > 3.0 && !wet_land && *me.gm.lock().unwrap() != 1 {
                                drop(ff);
                                let mut h = me.hp.lock().unwrap();
                                *h = (*h - (fall - 3.0).floor() as f32).max(0.0);
                                let hh = *h; drop(h);
                                let _ = send_packet(&mut s, 0x24, hurt_anim(me.eid));
                                let _ = send_packet(&mut s, 0x5d, health_pkt_c(hh, &me));
                            }
                        }
                    }
                    let (ccx, ccz) = ((px / 16.0).floor() as i32, (pz / 16.0).floor() as i32);
                    if (ccx, ccz) != player_chunk {
                        player_chunk = (ccx, ccz);

                        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| stream_chunks(&mut s, &mut sent, ccx, ccz))).is_ok();

                        let _ = ok;
                    }
                    broadcast(me.eid, |c| (0x70, entity_teleport(me.eid, px, py, pz, yaw, pitch, gnd)));
                    if pid == 0x1b { broadcast(me.eid, |c| (0x48, head_rot(me.eid, yaw))); }
                }
            }
            0x1c => { // look: yaw,pitch only
                if b.len() >= 9 {
                    let yaw = f32::from_be_bytes(b[..4].try_into().unwrap());
                    let pitch = f32::from_be_bytes(b[4..8].try_into().unwrap());
                    let mut p = me.pos.lock().unwrap();
                    p.3 = yaw; p.4 = pitch; p.5 = b[8] != 0;
                    let (px, py, pz, g) = (p.0, p.1, p.2, p.5);
                    drop(p);
                    broadcast(me.eid, |c| (0x70, entity_teleport(me.eid, px, py, pz, yaw, pitch, g)));
                    broadcast(me.eid, |c| (0x48, head_rot(me.eid, yaw)));
                }
            }
            0x25 => { // entity_action: sprint start/stop
                let mut sl = &b[..];
                let _eid = read_varint(&mut sl);
                if let Some(act) = read_varint(&mut sl) {
                    if act == 3 { *me.sprinting.lock().unwrap() = true; }
                    if act == 4 { *me.sprinting.lock().unwrap() = false; }
                }
            }
            0x2f => { // held_item_slot: i16
                if b.len() >= 2 { held_slot = i16::from_be_bytes(b[..2].try_into().unwrap()).clamp(0, 8); *me.held.lock().unwrap() = held_slot; }
            }
            0x1d => { // flying: onGround only
                if !b.is_empty() {
                    let mut p = me.pos.lock().unwrap(); p.5 = b[0] != 0;
                }
            }
            0x24 => { // block_dig
                let mut sl = &b[..];
                let status = read_varint(&mut sl).unwrap_or(-1);
                let (x, y, z) = if sl.len() >= 8 { read_pos(&sl[..8]) } else { (0, 0, 0) };
                let seq = if sl.len() >= 9 { let mut s2 = &sl[9..]; read_varint(&mut s2).unwrap_or(0) } else { 0 };
                if status == 5 { // finish using item (bow release / lower shield)
                    *me.blocking.lock().unwrap() = false;
                    if *me.bow_draw.lock().unwrap() {
                        *me.bow_draw.lock().unwrap() = false;
                        let arrow_slot = me.inv.lock().unwrap().iter()
                            .find(|(_, &(i, c))| i == 802 && c > 0).map(|(&sl, _)| sl);
                        let (hit, _) = { let inv = me.inv.lock().unwrap();
                            (inv.get(&(36 + held_slot)).map(|v| v.0).unwrap_or(0), 0) };
                        if hit == 801 && arrow_slot.is_some() {
                            let asl = arrow_slot.unwrap();
                            { let mut inv = me.inv.lock().unwrap();
                              let n = inv[&asl].1 - 1;
                              if n == 0 { inv.remove(&asl); } else { inv.get_mut(&asl).unwrap().1 = n; } }
                            let (x, y, z, yaw, pitch, _g) = *me.pos.lock().unwrap();
                            let yr = (yaw as f64).to_radians(); let pr = (pitch as f64).to_radians();
                            let a = Arrow { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), owner: me.eid,
                                x, y: y + 1.5, z,
                                dx: -yr.sin() * pr.cos() * 2.5, dy: -pr.sin() * 2.5, dz: yr.cos() * pr.cos() * 2.5, ttl: 60 };
                            let sp = spawn_arrow_pkt(&a);
                            arrows().lock().unwrap().push(a);
                            broadcast(-1, |_c| (0x01, sp.clone()));
                            { let inv = me.inv.lock().unwrap().get(&asl).copied();
                              let mut p = vec![4u8]; write_varint(&mut p, 0);
                              p.extend_from_slice(&(36i16 + held_slot as i16).to_be_bytes());
                              let held_now = { let i2 = me.inv.lock().unwrap(); i2.get(&(36 + held_slot)).copied().unwrap_or((0,0)) };
                              p.extend_from_slice(&slot_wire(held_now.1, held_now.0));
                              let _ = send_packet(&mut s, 0x15, p);
                              let mut p2 = vec![4u8]; write_varint(&mut p2, 0);
                              p2.extend_from_slice(&asl.to_be_bytes());
                              let iv = inv.unwrap_or((0, 0));
                              p2.extend_from_slice(&slot_wire(iv.1, iv.0));
                              let _ = send_packet(&mut s, 0x15, p2); }
                        }
                    }
                }
                if status == 0 || status == 2 { // creative instant / finish
                    let old = block_at(x, y, z);
                    set_block(x, y, z, 0);
                    let bc = block_change(x, y, z, 0);
                    let _ = send_packet(&mut s, 0x09, bc.clone());
                    broadcast(me.eid, |_c| (0x09, bc.clone()));
                    if *me.gm.lock().unwrap() != 1 {
                    let needs_pick = matches!(old, 1 | 14 | 125 | 127 | 4274 | 126 | 128 | 4275);
                    let held = { let inv = me.inv.lock().unwrap(); inv.get(&(36 + held_slot)).copied().unwrap_or((0, 0)) };
                    let is_pick = held.0 >= 820 && held.0 <= 845 && (held.0 - 820) % 5 == 0;
                    let drop_item = match old {
                        127 | 128 => Some(803),   // coal_ore -> coal
                        125 | 126 => Some(810),   // iron_ore -> raw_iron
                        4274 | 4275 => Some(805), // diamond_ore -> diamond
                        4278..=4285 => Some(853), // wheat -> seeds (+wheat if ripe)
                        _ => state_to_item(old),
                    };
                    crops().lock().unwrap().remove(&(x, y, z));
                    if old == 4285 { // ripe: extra wheat drop
                        let d = Drop { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                            x: x as f64 + 0.5, y: y as f64 + 0.25, z: z as f64 + 0.5, item: 854, age: Instant::now() };
                        let sp = spawn_drop_pkt(&d); let md = drop_meta_pkt(&d);
                        drops().lock().unwrap().push(d);
                        let _ = send_packet(&mut s, 0x01, sp.clone());
                        let _ = send_packet(&mut s, 0x58, md.clone());
                        broadcast(me.eid, |_c| (0x01, sp.clone()));
                        broadcast(me.eid, |_c| (0x58, md.clone()));
                    }
                    if let Some(item) = if needs_pick && !is_pick { None } else { drop_item } {
                        let d = Drop { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                            x: x as f64 + 0.5, y: y as f64 + 0.25, z: z as f64 + 0.5, item, age: Instant::now() };
                        let sp = spawn_drop_pkt(&d);
                        let md = drop_meta_pkt(&d);
                        drops().lock().unwrap().push(d);
                        let _ = send_packet(&mut s, 0x01, sp.clone());
                        let _ = send_packet(&mut s, 0x58, md.clone());
                        broadcast(me.eid, |_c| (0x01, sp.clone()));
                        broadcast(me.eid, |_c| (0x58, md.clone()));
                    }
                    }
                }
                let _ = send_packet(&mut s, 0x05, ack(seq));
            }
            0x38 => { // block_place
                let mut sl = &b[..];
                let _hand = read_varint(&mut sl);
                let (x, y, z) = if sl.len() >= 8 { read_pos(&sl[..8]) } else { (0, 0, 0) };
                let mut s2 = &sl[8.min(sl.len())..];
                let face = read_varint(&mut s2).unwrap_or(1);
                let _ = &s2[..13.min(s2.len())]; // cursor+inside
                let seq = if s2.len() >= 13 { let mut s3 = &s2[13..]; read_varint(&mut s3).unwrap_or(0) } else { 0 };
                let (dx, dy, dz) = FACE_OFF[(face.clamp(0, 5)) as usize];
                let (tx, ty, tz) = (x + dx, y + dy, z + dz);
                // chest: open container instead of placing
                if block_at(x, y, z) == name_to_state("chest").unwrap_or(u32::MAX) {
                    *me.open_chest.lock().unwrap() = Some((x, y, z));
                    let inv = me.inv.lock().unwrap().clone();
                    open_chest_window(&mut s, (x, y, z), &inv);
                    let _ = send_packet(&mut s, 0x05, ack(seq));
                    continue;
                }
                if block_at(x, y, z) == name_to_state("crafting_table").unwrap_or(u32::MAX) {
                    *me.open_table.lock().unwrap() = true;
                    let mut p = Vec::new();
                    write_varint(&mut p, 3);        // windowId 3
                    write_varint(&mut p, 12);       // crafting menu
                    p.extend_from_slice(&chat_nbt("Crafting"));
                    let _ = send_packet(&mut s, 0x33, p);
                    let mut q = vec![3u8];
                    write_varint(&mut q, 0);
                    write_varint(&mut q, 46);
                    for _ in 0..46 { q.extend_from_slice(&slot_wire(0, 0)); }
                    q.extend_from_slice(&slot_wire(0, 0));
                    let _ = send_packet(&mut s, 0x13, q);
                    // now fill: grid empty, inv slots
                    let inv = me.inv.lock().unwrap().clone();
                    let mut r = vec![3u8];
                    write_varint(&mut r, 0);
                    write_varint(&mut r, 46);
                    for _ in 0..10 { r.extend_from_slice(&slot_wire(0, 0)); }
                    for wsl in 10..46i16 {
                        let (it, c) = inv.get(&(wsl - 1)).copied().unwrap_or((0, 0));
                        r.extend_from_slice(&slot_wire(c, it));
                    }
                    r.extend_from_slice(&slot_wire(0, 0));
                    let _ = send_packet(&mut s, 0x13, r);
                    let _ = send_packet(&mut s, 0x05, ack(seq));
                    continue;
                }
                if block_at(x, y, z) == name_to_state("furnace").unwrap_or(u32::MAX) {
                    *me.open_furnace.lock().unwrap() = Some((x, y, z));
                    let inv = me.inv.lock().unwrap().clone();
                    open_furnace_window(&mut s, (x, y, z), &inv);
                    let _ = send_packet(&mut s, 0x05, ack(seq));
                    continue;
                }
                // buckets: empty fills from source; filled places liquid
                let held_b = { let inv = me.inv.lock().unwrap(); inv.get(&(36 + held_slot)).copied() };
                if let Some((item, cnt)) = held_b {
                    let src = block_at(x, y, z);
                    if item == 908 && (src == 80 || src == 96) {
                        let fi = if src == 80 { 909 } else { 910 };
                        set_block(x, y, z, 0);
                        let bc = block_change(x, y, z, 0);
                        let _ = send_packet(&mut s, 0x09, bc.clone());
                        broadcast(me.eid, |_c| (0x09, bc.clone()));
                        let mut inv = me.inv.lock().unwrap();
                        if cnt > 1 {
                            inv.insert(36 + held_slot, (908, cnt - 1));
                            let free = (9..=44).find(|sl| !inv.contains_key(sl)).unwrap_or(9);
                            inv.insert(free, (fi, 1));
                        } else {
                            inv.insert(36 + held_slot, (fi, 1));
                        }
                        let (i2, c2) = inv.get(&(36 + held_slot)).copied().unwrap_or((0, 0));
                        drop(inv);
                        let mut p = vec![0u8];
                        write_varint(&mut p, 0);
                        p.extend_from_slice(&(36 + held_slot).to_be_bytes());
                        p.extend_from_slice(&slot_wire(c2, i2));
                        let _ = send_packet(&mut s, 0x15, p);
                        let _ = send_packet(&mut s, 0x05, ack(seq));
                        continue;
                    }
                    let replaceable = matches!(block_at(tx, ty, tz), 0 | 2005 | 2006 | 2007 | 2075 | 2077 | 10756 | 5772..=5779);
                    if (item == 909 || item == 910) && replaceable {
                        let st = if item == 909 { 80 } else { 96 };
                        set_block(tx, ty, tz, st);
                        let bc = block_change(tx, ty, tz, st);
                        let _ = send_packet(&mut s, 0x09, bc.clone());
                        broadcast(me.eid, |_c| (0x09, bc.clone()));
                        if *me.gm.lock().unwrap() != 1 {
                            let mut inv = me.inv.lock().unwrap();
                            inv.insert(36 + held_slot, (908, 1));
                            drop(inv);
                            let mut p = vec![0u8];
                            write_varint(&mut p, 0);
                            p.extend_from_slice(&(36 + held_slot).to_be_bytes());
                            p.extend_from_slice(&slot_wire(1, 908));
                            let _ = send_packet(&mut s, 0x15, p);
                        }
                        let _ = send_packet(&mut s, 0x05, ack(seq));
                        continue;
                    }
                }
                // bed: right-click at night skips to morning
                if (1688..=1943).contains(&block_at(x, y, z)) {
                    let wt = WORLD_TIME.load(Ordering::Relaxed) % 24000;
                    if wt >= 13000 && wt <= 23500 {
                        WORLD_TIME.store(0, Ordering::Relaxed);
                        *me.spawn_pt.lock().unwrap() = Some((x as f64 + 0.5, y as f64 + 1.0, z as f64 + 0.5));
                        let mut sp = Vec::new();
                        write_pos(&mut sp, x, y, z);
                        sp.extend_from_slice(&0.0f32.to_be_bytes());
                        let _ = send_packet(&mut s, 0x56, sp);
                        let line = system_chat(&format!("{} slept through the night", me.name));
                        broadcast(-1, |_c| (0x6c, line.clone()));
                    }
                    let _ = send_packet(&mut s, 0x05, ack(seq));
                    continue;
                }
                // door toggle: flip open bit on both halves
                if let Some(base) = door_base(block_at(x, y, z)) {
                    for dy in [-1i32, 0, 1] {
                        let st = block_at(x, y + dy, z);
                        if door_base(st) == Some(base) {
                            set_block(x, y + dy, z, st ^ 2);
                            let bc = block_change(x, y + dy, z, st ^ 2);
                            let _ = send_packet(&mut s, 0x09, bc.clone());
                            broadcast(me.eid, |_c| (0x09, bc.clone()));
                        }
                    }
                    let _ = send_packet(&mut s, 0x05, ack(seq));
                    continue;
                }
                // lever toggle: flip powered bit, then update neighbors
                if (5626..=5649).contains(&block_at(x, y, z)) {
                    let st = block_at(x, y, z);
                    let nst = st ^ 1; // powered is stride1 (true=0, false=1)
                    set_block(x, y, z, nst);
                    let bc = block_change(x, y, z, nst);
                    let _ = send_packet(&mut s, 0x09, bc.clone());
                    broadcast(me.eid, |_c| (0x09, bc.clone()));
                    let powered = (nst - 5626) & 1 == 0;
                    redstone_update(x, y, z, powered);
                    let _ = send_packet(&mut s, 0x05, ack(seq));
                    continue;
                }
                // fence gate toggle (open bit = stride 2)
                if gate_base(block_at(x, y, z)).is_some() {
                    let st = block_at(x, y, z);
                    set_block(x, y, z, st ^ 2);
                    let bc = block_change(x, y, z, st ^ 2);
                    let _ = send_packet(&mut s, 0x09, bc.clone());
                    broadcast(me.eid, |_c| (0x09, bc.clone()));
                    let _ = send_packet(&mut s, 0x05, ack(seq));
                    continue;
                }
                // hotbar slots are window slots 36..=44
                let held = { let inv = me.inv.lock().unwrap(); inv.get(&(36 + held_slot)).copied() };
                // bone meal on crops: +2..4 stages, consume 1
                if let Some((960, _)) = held {
                    let cst = block_at(x, y, z);
                    if (4278..=4285).contains(&cst) {
                        let age = (cst - 4278) as u8;
                        let bump = 2 + ((x.wrapping_mul(7) ^ z.wrapping_mul(13) ^ y) & 1) as u8;
                        let nage = (age + bump).min(7);
                        set_block(x, y, z, 4278 + nage as u32);
                        crops().lock().unwrap().insert((x, y, z), nage);
                        let bc = block_change(x, y, z, 4278 + nage as u32);
                        let _ = send_packet(&mut s, 0x09, bc.clone());
                        broadcast(me.eid, |_c| (0x09, bc.clone()));
                        if *me.gm.lock().unwrap() != 1 {
                            let mut inv = me.inv.lock().unwrap();
                            if let Some(&(it, c)) = inv.get(&(36 + held_slot)) {
                                if c <= 1 { inv.remove(&(36 + held_slot)); } else { inv.insert(36 + held_slot, (it, c - 1)); }
                            }
                            let (it, c) = inv.get(&(36 + held_slot)).copied().unwrap_or((0, 0));
                            drop(inv);
                            let mut p = vec![0u8];
                            write_varint(&mut p, 0);
                            p.extend_from_slice(&(36 + held_slot).to_be_bytes());
                            p.extend_from_slice(&slot_wire(c, it));
                            let _ = send_packet(&mut s, 0x15, p);
                        }
                        let _ = send_packet(&mut s, 0x05, ack(seq));
                        continue;
                    }
                }
                // hoe: convert dirt-family clicked block to farmland
                if let Some((item, _)) = held {
                    let is_hoe = (822..=847).contains(&item) && (item - 822) % 5 == 0;
                    let tillable = matches!(block_at(x, y, z), 9 | 10 | 12513);
                    if is_hoe && tillable && face == 1 {
                        let fm = name_to_state("farmland").unwrap_or(0);
                        set_block(x, y, z, fm);
                        let bc = block_change(x, y, z, fm);
                        let _ = send_packet(&mut s, 0x09, bc.clone());
                        broadcast(me.eid, |_c| (0x09, bc.clone()));
                        let _ = send_packet(&mut s, 0x05, ack(seq));
                        continue;
                    }
                    // wheat seeds on farmland -> wheat age0 + consume seed
                    if item == 853 && block_at(x, y, z) == name_to_state("farmland").unwrap_or(u32::MAX) {
                        set_block(tx, ty, tz, 4278);
                        crops().lock().unwrap().insert((tx, ty, tz), 0);
                        let bc = block_change(tx, ty, tz, 4278);
                        let _ = send_packet(&mut s, 0x09, bc.clone());
                        broadcast(me.eid, |_c| (0x09, bc.clone()));
                        if *me.gm.lock().unwrap() != 1 {
                            let mut inv = me.inv.lock().unwrap();
                            if let Some(&(it, c)) = inv.get(&(36 + held_slot)) {
                                if c <= 1 { inv.remove(&(36 + held_slot)); } else { inv.insert(36 + held_slot, (it, c - 1)); }
                            }
                            let (it, c) = inv.get(&(36 + held_slot)).copied().unwrap_or((0, 0));
                            drop(inv);
                            let mut p = vec![0u8];
                            write_varint(&mut p, 0);
                            p.extend_from_slice(&(36 + held_slot).to_be_bytes());
                            p.extend_from_slice(&slot_wire(c, it));
                            let _ = send_packet(&mut s, 0x15, p);
                        }
                        let _ = send_packet(&mut s, 0x05, ack(seq));
                        continue;
                    }
                }
                if let Some((item, _cnt)) = held {
                    if let Some(st) = item_to_state(item) {
                        set_block(tx, ty, tz, st);
                        let bc = block_change(tx, ty, tz, st);
                        let _ = send_packet(&mut s, 0x09, bc.clone());
                        broadcast(me.eid, |_c| (0x09, bc.clone()));
                    }
                }
                let _ = send_packet(&mut s, 0x05, ack(seq));
            }
            0x36 => { // arm_animation: relay swing to others
                broadcast(me.eid, |c| (0x03, {
                    let mut p = Vec::new();
                    write_varint(&mut p, me.eid as i64);
                    p.push(0);
                    p
                }));
            }
            0x39 => { // use_item: ack + eat + bow draw
                let mut sl = &b[..];
                let _h = read_varint(&mut sl);
                let seq = read_varint(&mut sl).unwrap_or(0);
                let _ = send_packet(&mut s, 0x05, ack(seq));
                { let inv = me.inv.lock().unwrap();
                  let main = inv.get(&(36 + held_slot)).map(|v| v.0).unwrap_or(0);
                  if main == 931 { // fishing rod: cast or reel
                    drop(inv);
                    let mut fb = bobbers().lock().unwrap();
                    if let Some(ix) = fb.iter().position(|b| b.owner == me.eid) {
                            let b = fb.remove(ix);
                            let mut dp = Vec::new(); write_varint(&mut dp, 1); write_varint(&mut dp, b.eid as i64);
                            broadcast(-1, |_c| (0x42, dp.clone()));
                            if b.bit { // catch!
                                let fish = if (b.age as u64) % 5 == 0 { 936 } else { 935 };
                                let mut inv = me.inv.lock().unwrap();
                                let slot = (9..=44).find(|sl| !inv.contains_key(sl)).unwrap_or(9);
                                inv.insert(slot, (fish, 1));
                                let mut p = vec![4u8]; write_varint(&mut p, 0);
                                p.extend_from_slice(&slot.to_be_bytes());
                                p.extend_from_slice(&slot_wire(1, fish));
                                let _ = send_packet(&mut s, 0x15, p);
                            }
                    } else {
                        let (x, y, z, yaw, pitch, _g) = *me.pos.lock().unwrap();
                        let yr = (yaw as f64).to_radians(); let pr = (pitch as f64).to_radians();
                        let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                        let b = Bobber { eid, owner: me.eid, x, y: y + 1.5, z,
                            dx: -yr.sin() * pr.cos() * 1.2, dy: -pr.sin() * 1.2 + 0.2, dz: yr.cos() * pr.cos() * 1.2, age: 0, bit: false };
                        let sp = spawn_typed_pkt(eid, 129, b.x, b.y, b.z);
                        fb.push(b);
                        broadcast(-1, |_c| (0x01, sp.clone()));
                    }
                    continue;
                  }
                  if main == 993 { // ender pearl: throw
                    drop(inv);
                    let (x, y, z, yaw, pitch, _g) = *me.pos.lock().unwrap();
                    let yr = (yaw as f64).to_radians(); let pr = (pitch as f64).to_radians();
                    let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                    let pe = Pearl { eid, owner: me.eid, x, y: y + 1.5, z,
                        dx: -yr.sin() * pr.cos() * 2.0, dy: -pr.sin() * 2.0 + 0.3, dz: yr.cos() * pr.cos() * 2.0, ttl: 50 };
                    let sp = spawn_typed_pkt(eid, 32, x, y + 1.5, z);
                    pearls().lock().unwrap().push(pe);
                    broadcast(-1, |_c| (0x01, sp.clone()));
                    if *me.gm.lock().unwrap() != 1 {
                        let mut inv = me.inv.lock().unwrap();
                        let hs = 36 + held_slot;
                        if let Some(&(it, c)) = inv.get(&hs) { if it == 993 {
                            if c <= 1 { inv.remove(&hs); } else { inv.insert(hs, (it, c - 1)); }
                        } }
                    }
                    continue;
                  }
                  if main == 912 { // snowball: throw
                    drop(inv);
                    let (x, y, z, yaw, pitch, _g) = *me.pos.lock().unwrap();
                    let yr = (yaw as f64).to_radians(); let pr = (pitch as f64).to_radians();
                    let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                    let sb = Snowball { eid, owner: me.eid, x, y: y + 1.5, z,
                        dx: -yr.sin() * pr.cos() * 1.5, dy: -pr.sin() * 1.5 + 0.25, dz: yr.cos() * pr.cos() * 1.5, ttl: 60 };
                    let sp = spawn_typed_pkt(eid, 97, x, y + 1.5, z);
                    snowballs().lock().unwrap().push(sb);
                    broadcast(-1, |_c| (0x01, sp.clone()));
                    if *me.gm.lock().unwrap() != 1 {
                        let mut inv2 = me.inv.lock().unwrap();
                        let hs = 36 + held_slot;
                        if let Some(&(it, c)) = inv2.get(&hs) { if it == 912 {
                            if c <= 1 { inv2.remove(&hs); } else { inv2.insert(hs, (it, c - 1)); }
                        } }
                    }
                    continue;
                  }
                  if main == 1162 || (_h == Some(1) && inv.get(&45).map(|v| v.0).unwrap_or(0) == 1162) {
                    *me.blocking.lock().unwrap() = true;
                    continue;
                  }
                  if main == 801 {
                    *me.bow_draw.lock().unwrap() = true;
                    continue;
                  } }
                if *me.gm.lock().unwrap() != 1 {
                    // prefer held slot; fall back to first food item in inventory
                    let (slot, it) = {
                        let inv = me.inv.lock().unwrap();
                        let hs = 36 + held_slot;
                        match inv.get(&hs) {
                            Some(&(it, _)) if food_value(it) > 0 => (hs, it),
                            _ => inv.iter().find(|(_, &(i, _))| food_value(i) > 0)
                                    .map(|(&sl, &(i, _))| (sl, i)).unwrap_or((hs, 0)),
                        }
                    };
                    let fv = food_value(it);
                    let mut f = me.food.lock().unwrap();
                    if fv > 0 && *f < 20 {
                        *f = (*f + fv).min(20); drop(f);
                        let ncnt = {
                            let mut inv = me.inv.lock().unwrap();
                            let nc = inv.get(&slot).map(|e| e.1).unwrap_or(1).saturating_sub(1);
                            if nc == 0 { inv.remove(&slot); } else { inv.insert(slot, (it, nc)); }
                            nc
                        };
                        let mut p = vec![0u8];
                        write_varint(&mut p, 0);
                        p.extend_from_slice(&slot.to_be_bytes());
                        write_varint(&mut p, ncnt as i64);
                        if ncnt > 0 { write_varint(&mut p, it); write_varint(&mut p, 0); write_varint(&mut p, 0); }
                        let _ = send_packet(&mut s, 0x15, p);
                        let hh = *me.hp.lock().unwrap();
                        let _ = send_packet(&mut s, 0x5d, health_pkt_c(hh, &me));
                    }
                }
            }
            0x04 | 0x05 => { // chat_command(+signed): command string (no leading /)
                let mut sl = &b[..];

                if let Some(mlen) = read_varint(&mut sl) {
                    let m = String::from_utf8_lossy(&sl[..(mlen as usize).min(sl.len())]).to_string();

                    run_command(Some(&me), &format!("/{}", m), &mut Some(&mut s));
                }
            }
            0x06 => { // chat_message: first field = message string
                let mut sl = &b[..];
                if let Some(mlen) = read_varint(&mut sl) {
                    let m = String::from_utf8_lossy(&sl[..(mlen as usize).min(sl.len())]).to_string();
                    if m.starts_with('/') { run_command(Some(&me), &m, &mut Some(&mut s)); continue; }
                    let line = format!("<{}> {}", me.name, m);
                    let pl = system_chat(&line);
                    let _ = send_packet(&mut s, 0x6c, pl.clone());
                    broadcast(me.eid, |_c| (0x6c, pl.clone()));
                }
            }
            0x16 => { // use_entity: attack mobs
                let mut sl = &b[..];
                let target = read_varint(&mut sl).unwrap_or(-1) as i32;
                let mouse = read_varint(&mut sl).unwrap_or(-1);
                if mouse == 0 || mouse == 2 { // interact: villagers open trade UI; wheat breeds animals
                    let is_villager = { let ms = mobs().lock().unwrap();
                        ms.iter().any(|m| m.eid == target && m.etype == 113) };
                    let held_it = me.inv.lock().unwrap().get(&(36 + held_slot)).copied().unwrap_or((0, 0)).0;
                    if held_it == 983 && !is_villager { // shears on sheep -> wool
                        let is_sheep = { let ms = mobs().lock().unwrap();
                            ms.iter().any(|m| m.eid == target && m.etype == 87) };
                        if is_sheep && !sheared().lock().unwrap().contains(&target) {
                            sheared().lock().unwrap().insert(target);
                            let (mx, my, mz) = { let ms = mobs().lock().unwrap();
                                ms.iter().find(|m| m.eid == target).map(|m| *m.pos.lock().unwrap()).map(|p| (p.0, p.1, p.2)).unwrap_or((0.0,0.0,0.0)) };
                            let n = 1 + ((target as u64).wrapping_mul(2654435761) % 3) as u8;
                            for _ in 0..n {
                                let deid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                                let d = Drop { eid: deid, x: mx, y: my + 0.5, z: mz, item: 202, age: Instant::now() };
                                let sp = spawn_drop_pkt(&d); let md = drop_meta_pkt(&d);
                                drops().lock().unwrap().push(d);
                                broadcast(-1, |_c| (0x01, sp.clone()));
                                broadcast(-1, |_c| (0x58, md.clone()));
                            }
                            continue;
                        }
                    }
                    if held_it == 908 && !is_villager { // bucket on cow/sheep -> milk
                        let milkable = { let ms = mobs().lock().unwrap();
                            ms.iter().any(|m| m.eid == target && [22i64, 77].contains(&m.etype)) };
                        if milkable {
                            let k = 36 + held_slot;
                            { let mut inv = me.inv.lock().unwrap();
                                let c = inv.get(&k).map(|v| v.1).unwrap_or(0);
                                if c <= 1 { inv.insert(k, (914, 1)); }
                                else { inv.insert(k, (908, c - 1));
                                    let fs = (9..=44i16).find(|sl| !inv.contains_key(sl)).unwrap_or(9);
                                    inv.insert(fs, (914, 1));
                                }
                            }
                            let (it2, c2) = me.inv.lock().unwrap().get(&k).copied().unwrap_or((0, 0));
                            let mut p = vec![0u8]; write_varint(&mut p, 0);
                            p.extend_from_slice(&k.to_be_bytes());
                            p.extend_from_slice(&slot_wire(c2, it2));
                            let _ = send_packet(&mut s, 0x15, p);
                            continue;
                        }
                    }
                    if held_it == 854 && !is_villager { // wheat -> love mode on farm animals
                        let mut loved = false;
                        { let ms = mobs().lock().unwrap();
                            if let Some(m) = ms.iter().find(|m| m.eid == target && [19i64, 22, 77, 87].contains(&m.etype)) {
                                *m.love.lock().unwrap() = 80; loved = true;
                            }
                        }
                        if loved {
                            if *me.gm.lock().unwrap() != 1 {
                                let mut inv = me.inv.lock().unwrap();
                                let k = 36 + held_slot;
                                if let Some((it, c)) = inv.get(&k).copied() {
                                    if c > 1 { inv.insert(k, (it, c - 1)); } else { inv.remove(&k); }
                                }
                                let (it, c) = inv.get(&k).copied().unwrap_or((0, 0));
                                drop(inv);
                                let mut p = vec![0u8];
                                write_varint(&mut p, 0);
                                p.extend_from_slice(&k.to_be_bytes());
                                p.extend_from_slice(&slot_wire(c, it));
                                let _ = send_packet(&mut s, 0x15, p);
                            }
                            continue;
                        }
                    }
                    if is_villager {
                        *me.open_merchant.lock().unwrap() = true;
                        *me.trade_sel.lock().unwrap() = 0;
                        let mut p = Vec::new();
                        write_varint(&mut p, 4);        // windowId 4
                        write_varint(&mut p, 19);       // merchant menu
                        p.extend_from_slice(&chat_nbt("Villager"));
                        let _ = send_packet(&mut s, 0x33, p);
                        let _ = send_packet(&mut s, 0x2d, trade_list_pkt(4));
                        // window items: 3 trade slots + inv (wslot 3..39 -> inv 9..44)
                        let inv = me.inv.lock().unwrap().clone();
                        let mut q = vec![4u8];
                        write_varint(&mut q, 0);
                        write_varint(&mut q, 39);
                        for _ in 0..3 { q.extend_from_slice(&slot_wire(0, 0)); }
                        for wsl in 3..39i16 {
                            let (it, c) = inv.get(&(wsl + 6)).copied().unwrap_or((0, 0));
                            q.extend_from_slice(&slot_wire(c, it));
                        }
                        q.extend_from_slice(&slot_wire(0, 0));
                        let _ = send_packet(&mut s, 0x13, q);
                        continue;
                    }
                }
                if mouse == 1 { // attack
                    // PvP: target may be another player
                    if target != me.eid {
                        let mut found: Option<(i32, String, f32)> = None;
                        {
                            let cls = clients().lock().unwrap();
                            if let Some(v) = cls.iter().find(|c| c.eid == target) {
                                if *v.gm.lock().unwrap() == 1 { }
                                else {
                                let mut h = v.hp.lock().unwrap();
                                *h = (*h - 1.0 * armor_mult(v)).max(0.0);
                                let hh = *h; drop(h);
                                if let Ok(mut w) = v.sock.lock() {
                                    let _ = send_packet(&mut *w, 0x24, hurt_anim(v.eid));
                                    let _ = send_packet(&mut *w, 0x5d, health_pkt_c(hh, v));
                                }
                                let (pp, vp) = ({ let p = me.pos.lock().unwrap(); *p }, { let p = v.pos.lock().unwrap(); *p });
                                let (mut kx, mut kz) = (vp.0 - pp.0, vp.2 - pp.2);
                                let kl = (kx*kx + kz*kz).sqrt().max(0.01);
                                kx /= kl; kz /= kl;
                                let vel = velocity_pkt(v.eid, (kx*3000.0) as i16, 2500, (kz*3000.0) as i16);
                                if let Ok(mut w) = v.sock.lock() { let _ = send_packet(&mut *w, 0x5a, vel.clone()); }
                                found = Some((v.eid, v.name.clone(), hh));
                                }
                            }
                        }
                        if let Some((veid, vname, hh)) = found {
                            broadcast(veid, move |_c| (0x24, hurt_anim(veid)));
                            if hh <= 0.0 {
                                let line = system_chat(&format!("{} was slain by {}", vname, me.name));
                                broadcast(-1, |_c| (0x6c, line.clone()));
                            }
                            continue;
                        }
                    }
                    let mut dead: Option<(i32, i64, f64, f64, f64)> = None;
                    {
                        let ms = mobs().lock().unwrap();
                        if let Some(m) = ms.iter().find(|m| m.eid == target) {
                            let mut h = m.hp.lock().unwrap();
                            *h -= 4.0;
                            if *h <= 0.0 {
                                let (mx, my, mz, ..) = { let p = m.pos.lock().unwrap(); *p };
                                dead = Some((m.eid, m.etype, mx, my, mz));
                            }
                            let ha = hurt_anim(m.eid);
                            let _ = send_packet(&mut s, 0x24, ha.clone());
                            broadcast(me.eid, |_c| (0x24, ha.clone()));
                            // knockback away from attacker
                            let (pp, mp) = ({ let p = me.pos.lock().unwrap(); *p }, { let p = m.pos.lock().unwrap(); *p });
                            let (mut kx, mut kz) = (mp.0 - pp.0, mp.2 - pp.2);
                            let kl = (kx*kx + kz*kz).sqrt().max(0.01);
                            kx /= kl; kz /= kl;
                            let vp = velocity_pkt(m.eid, (kx*3200.0) as i16, 2800, (kz*3200.0) as i16);
                            broadcast(-1, |_c| (0x5a, vp.clone()));
                            let (nx, nz) = (mp.0 + kx*0.6, mp.2 + kz*0.6);
                            { let mut p = m.pos.lock().unwrap(); p.0 = nx; p.2 = nz; }
                            let tp = entity_teleport(m.eid, nx, mp.1, nz, mp.3, mp.4, true);
                            broadcast(-1, |_c| (0x70, tp.clone()));
                        }
                    }
                    if let Some((deid, etype, mx, my, mz)) = dead {
                        let mn = match etype { 124 => "Zombie", 22 => "Cow", 23 => "Creeper", 91 => "Skeleton", 19=>"Chicken", 77=>"Pig", 87=>"Sheep", 100=>"Spider", _ => "mob" };
                        let line = system_chat(&format!("{} was slain by {}", mn, me.name));
                        broadcast(-1, |_c| (0x6c, line.clone()));
                        mobs().lock().unwrap().retain(|m| m.eid != deid);
                        let mut p = Vec::new(); write_varint(&mut p, 1); write_varint(&mut p, deid as i64);
                        broadcast(-1, |_c| (0x42, p.clone()));
                        let was_sheared = sheared().lock().unwrap().remove(&deid);
                        for item in mob_loot(etype).into_iter().filter(|i| !(was_sheared && *i == 202)) {
                            let d = Drop { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), x: mx, y: my, z: mz, item, age: Instant::now() };
                            let sp = spawn_drop_pkt(&d); let md = drop_meta_pkt(&d);
                            drops().lock().unwrap().push(d);
                            broadcast(-1, |_c| (0x01, sp.clone()));
                            broadcast(-1, |_c| (0x58, md.clone()));
                        }
                        let xeid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                        let xp = xp_orb_pkt(xeid, mx, my, mz, 3);
                        xps().lock().unwrap().push(Xp { eid: xeid, x: mx, y: my, z: mz, val: 3, age: Instant::now() });
                        broadcast(-1, |_c| (0x02, xp.clone()));
                    }
                }
            }
            0x09 => { // client_command: perform_respawn
                let mut sl = &b[..];
                if read_varint(&mut sl) == Some(0) {
                    *me.hp.lock().unwrap() = 20.0;
                    let mut rp = spawn_info(); rp.push(0);
                    let _ = send_packet(&mut s, 0x47, rp);
                    let (rx, ry, rz) = me.spawn_pt.lock().unwrap().unwrap_or((0.5, 16.0, 0.5));
                    { let mut p = me.pos.lock().unwrap(); p.0 = rx; p.1 = ry; p.2 = rz; }
                    let mut ps = Vec::new();
                    ps.extend_from_slice(&rx.to_be_bytes());
                    ps.extend_from_slice(&ry.to_be_bytes());
                    ps.extend_from_slice(&rz.to_be_bytes());
                    ps.extend_from_slice(&0f32.to_be_bytes());
                    ps.extend_from_slice(&0f32.to_be_bytes());
                    ps.push(0); write_varint(&mut ps, 2);
                    let _ = send_packet(&mut s, 0x40, ps);
                    *me.food.lock().unwrap() = 20; *me.sat.lock().unwrap() = 5.0;
                    { let iv = me.inv.lock().unwrap(); *me.armor_pts.lock().unwrap() = armor_pts_of(&iv); }
                    let _ = send_packet(&mut s, 0x5d, health_pkt_c(20.0, &me));
                }
            }
            0x2b => { // resource_pack_receive: status varint
                let mut sl = &b[..];
                eprintln!("[spine] resource_pack status {:?}", read_varint(&mut sl));
            }
            0x2f => { held_slot = i16::from_be_bytes(b[..2].try_into().unwrap_or([0; 2])); *me.held.lock().unwrap() = held_slot; }
            0x32 => { // set_creative_slot: slot i16 + slot data
                if b.len() >= 3 {
                    let slot = i16::from_be_bytes(b[..2].try_into().unwrap());
                    let mut sl = &b[2..];
                    let cnt = read_varint(&mut sl).unwrap_or(0);
                    let mut item: i64 = 0;
                    if cnt > 0 {
                        if let Some(it) = read_varint(&mut sl) { me.inv.lock().unwrap().insert(slot, (it, cnt as u8)); item = it; }
                    } else { me.inv.lock().unwrap().remove(&slot); }
                    { let iv = me.inv.lock().unwrap(); *me.armor_pts.lock().unwrap() = armor_pts_of(&iv); }
                    // echo container set_slot (0x15): window 0, stateId 0
                    let mut p = vec![0u8];
                    write_varint(&mut p, 0);
                    p.extend_from_slice(&slot.to_be_bytes());
                    write_varint(&mut p, cnt);
                    if cnt > 0 { write_varint(&mut p, item); write_varint(&mut p, 0); write_varint(&mut p, 0); }
                    let _ = send_packet(&mut s, 0x15, p);
                }
            }
            0x2d => { // select_trade: echo input/output into merchant slots 0/1/2
                let mut sl = &b[..];
                let sel = read_varint(&mut sl).unwrap_or(0) as i32;

                *me.trade_sel.lock().unwrap() = sel;
                if *me.open_merchant.lock().unwrap() {
                    let ts = trades();
                    if sel >= 0 && (sel as usize) < ts.len() {
                        let (ai, ac, bin, ci, cc) = ts[sel as usize];
                        // move inputs from inventory into trade slots 0/1
                        let mut inv = me.inv.lock().unwrap();
                        let mut ok = inv_consume(&mut inv, ai, ac as i64);
                        if ok { if let Some((bi, bc)) = bin { ok = inv_consume(&mut inv, bi, bc as i64); } }
                        if ok {
                            let mut mi = me.merchant_in.lock().unwrap();
                            mi[0] = (ai, ac);
                            mi[1] = bin.map(|b| b).unwrap_or((0, 0));
                            drop(mi);
                            let snap = inv.clone();
                            drop(inv);
                            for wsl in 3..39i16 {
                                let (it, c) = snap.get(&(wsl + 6)).copied().unwrap_or((0, 0));
                                let mut p = vec![4u8];
                                write_varint(&mut p, 0);
                                p.extend_from_slice(&wsl.to_be_bytes());
                                p.extend_from_slice(&slot_wire(c, it));
                                let _ = send_packet(&mut s, 0x15, p);
                            }
                            for (wsl, it, c) in [(0i16, ai, ac), (1, bin.map(|b| b.0).unwrap_or(0), bin.map(|b| b.1).unwrap_or(0)), (2, ci, cc)] {
                                let mut p = vec![4u8];
                                write_varint(&mut p, 0);
                                p.extend_from_slice(&wsl.to_be_bytes());
                                p.extend_from_slice(&slot_wire(c, it));
                                let _ = send_packet(&mut s, 0x15, p);
                            }
                        } else {
                            drop(inv);
                            // can't afford: clear the trade slots
                            for wsl in 0..3i16 {
                                let mut p = vec![4u8];
                                write_varint(&mut p, 0);
                                p.extend_from_slice(&wsl.to_be_bytes());
                                p.extend_from_slice(&slot_wire(0, 0));
                                let _ = send_packet(&mut s, 0x15, p);
                            }
                        }
                    }
                }
            }
            0x0f => { // close_window
                *me.open_chest.lock().unwrap() = None;
                *me.open_furnace.lock().unwrap() = None;
                *me.open_table.lock().unwrap() = false;
                if *me.open_merchant.lock().unwrap() {
                    let mi = *me.merchant_in.lock().unwrap();
                    let mut inv = me.inv.lock().unwrap();
                    for (it, c) in mi { if c > 0 {
                        let slot = (9..=44).find(|sl| !inv.contains_key(sl)).unwrap_or(9);
                        inv.insert(slot, (it, c));
                    } }
                    *me.merchant_in.lock().unwrap() = [(0, 0), (0, 0)];
                    drop(inv);
                }
                *me.open_merchant.lock().unwrap() = false;
                *me.trade_sel.lock().unwrap() = -1;
            }
            0x0e => { // window_click: window u8, stateId varint, slot i16, button i8, mode varint, ...
                if b.len() >= 6 {
                    let mut sl = &b[..];
                    let _wid = sl[0]; sl = &sl[1..];
                    let _st = read_varint(&mut sl);
                    if sl.len() >= 4 {
                        let slot = i16::from_be_bytes(sl[..2].try_into().unwrap());
                        let btn = sl[2];
                        sl = &sl[3..];
                        let mode = read_varint(&mut sl).unwrap_or(0);
                        if mode == 0 {
                            let chest_pos = *me.open_chest.lock().unwrap();
                            if let Some(cp) = chest_pos {
                                // chest window: 0-26 chest, 27-62 player (inv wsl-18)
                                let mut all = chests().lock().unwrap();
                                let ch = all.entry(cp).or_default();
                                let mut inv = me.inv.lock().unwrap();
                                let mut cur = me.cursor.lock().unwrap();
                                if slot == -999 {
                                    if cur.1 > 0 {
                                        let it = *cur; *cur = (0, 0);
                                        let (px, py, pz, ..) = { let p = me.pos.lock().unwrap(); *p };
                                        let d = Drop { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), x: px, y: py + 1.3, z: pz, item: it.0, age: Instant::now() };
                                        let sp = spawn_drop_pkt(&d); let md = drop_meta_pkt(&d);
                                        drops().lock().unwrap().push(d);
                                        broadcast(-1, |_c| (0x01, sp.clone()));
                                        broadcast(-1, |_c| (0x58, md.clone()));
                                    }
                                } else if slot >= 0 && slot < 63 {
                                    let (gslot, gmap): (i16, &mut HashMap<i16,(i64,u8)>) = if slot < 27 { (slot, ch) } else { (slot - 18, &mut inv) };
                                    let sv = gmap.get(&gslot).copied();
                                    let cv = *cur;
                                    if btn == 0 {
                                        if cv.1 == 0 && sv.is_some() { *cur = sv.unwrap(); gmap.remove(&gslot); }
                                        else if cv.1 > 0 && sv.is_none() { gmap.insert(gslot, cv); *cur = (0, 0); }
                                        else if cv.1 > 0 && sv.is_some() {
                                            let (si, sc) = sv.unwrap();
                                            if si == cv.0 && sc + cv.1 <= 64 { gmap.insert(gslot, (si, sc + cv.1)); *cur = (0, 0); }
                                            else { gmap.insert(gslot, cv); *cur = (si, sc); }
                                        }
                                    } else if btn == 1 {
                                        if cv.1 == 0 && sv.is_some() {
                                            let (si, sc) = sv.unwrap();
                                            let take = sc - sc / 2;
                                            *cur = (si, take);
                                            if sc - take == 0 { gmap.remove(&gslot); } else { gmap.insert(gslot, (si, sc - take)); }
                                        } else if cv.1 > 0 {
                                            if sv.is_none() { gmap.insert(gslot, (cv.0, 1)); *cur = (cv.0, cv.1 - 1); }
                                            else { let (si, sc) = sv.unwrap();
                                                if si == cv.0 && sc < 64 { gmap.insert(gslot, (si, sc + 1)); *cur = (cv.0, cv.1 - 1); } }
                                        }
                                    }
                                    let (it, c2) = gmap.get(&gslot).copied().unwrap_or((0, 0));
                                    let mut p = vec![1u8];
                                    write_varint(&mut p, 0);
                                    p.extend_from_slice(&slot.to_be_bytes());
                                    p.extend_from_slice(&slot_wire(c2, it));
                                    let _ = send_packet(&mut s, 0x15, p);
                                }
                                continue;
                            }
                            let furnace_pos = *me.open_furnace.lock().unwrap();
                            if let Some(fp) = furnace_pos {
                                let mut fs = furnaces().lock().unwrap();
                                let f = fs.entry(fp).or_insert(Furnace { input: (0,0), fuel: (0,0), output: (0,0), burn_left: 0, progress: 0, dirty: false });
                                let mut inv = me.inv.lock().unwrap();
                                let mut cur = me.cursor.lock().unwrap();
                                if slot == -999 {
                                    if cur.1 > 0 {
                                        let it = *cur; *cur = (0, 0);
                                        let (px, py, pz, ..) = { let p = me.pos.lock().unwrap(); *p };
                                        let d = Drop { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), x: px, y: py + 1.3, z: pz, item: it.0, age: Instant::now() };
                                        let sp = spawn_drop_pkt(&d); let md = drop_meta_pkt(&d);
                                        drops().lock().unwrap().push(d);
                                        broadcast(-1, |_c| (0x01, sp.clone()));
                                        broadcast(-1, |_c| (0x58, md.clone()));
                                    }
                                } else if slot >= 0 && slot < 39 {
                                    if slot == 2 {
                                        // output: take only, merge onto cursor
                                        if f.output.1 > 0 && (cur.1 == 0 || (cur.0 == f.output.0 && cur.1 + f.output.1 <= 64)) {
                                            *cur = (f.output.0, cur.1 + f.output.1);
                                            f.output = (0, 0); f.dirty = true;
                                        }
                                    } else if slot == 0 || slot == 1 {
                                        let m = if slot == 0 { &mut f.input } else { &mut f.fuel };
                                        let sv = if m.1 == 0 { None } else { Some(*m) };
                                        let cv = *cur;
                                        if btn == 0 {
                                            if cv.1 == 0 && sv.is_some() { *cur = sv.unwrap(); *m = (0, 0); }
                                            else if cv.1 > 0 && sv.is_none() { *m = cv; *cur = (0, 0); }
                                            else if cv.1 > 0 && sv.is_some() {
                                                let (si, sc) = sv.unwrap();
                                                if si == cv.0 && sc + cv.1 <= 64 { *m = (si, sc + cv.1); *cur = (0, 0); }
                                                else { *m = cv; *cur = (si, sc); }
                                            }
                                        } else if btn == 1 {
                                            if cv.1 == 0 && sv.is_some() {
                                                let (si, sc) = sv.unwrap();
                                                let take = sc - sc / 2;
                                                *cur = (si, take);
                                                if sc - take == 0 { *m = (0, 0); } else { *m = (si, sc - take); }
                                            } else if cv.1 > 0 {
                                                if sv.is_none() { *m = (cv.0, 1); *cur = (cv.0, cv.1 - 1); }
                                                else { let (si, sc) = sv.unwrap();
                                                    if si == cv.0 && sc < 64 { *m = (si, sc + 1); *cur = (cv.0, cv.1 - 1); } }
                                            }
                                        }
                                        f.dirty = true;
                                    } else {
                                        let islot = slot + 6;
                                        click_slot(&mut inv, &mut cur, islot, btn as i8);
                                        *me.armor_pts.lock().unwrap() = armor_pts_of(&inv);
                                    }
                                    // echo whole furnace window (cheap, keeps client honest)
                                    let (fi, fu, fo) = (f.input, f.fuel, f.output);
                                    for (wsl, it, c) in [(0i16, fi.0, fi.1), (1, fu.0, fu.1), (2, fo.0, fo.1)] {
                                        let mut p = vec![2u8];
                                        write_varint(&mut p, 0);
                                        p.extend_from_slice(&wsl.to_be_bytes());
                                        p.extend_from_slice(&slot_wire(c, it));
                                        let _ = send_packet(&mut s, 0x15, p);
                                    }
                                    if slot >= 3 {
                                        let (it, c) = inv.get(&(slot + 6)).copied().unwrap_or((0, 0));
                                        let mut p = vec![2u8];
                                        write_varint(&mut p, 0);
                                        p.extend_from_slice(&slot.to_be_bytes());
                                        p.extend_from_slice(&slot_wire(c, it));
                                        let _ = send_packet(&mut s, 0x15, p);
                                    }
                                }
                                continue;
                            }
                            if *me.open_merchant.lock().unwrap() {

                                let sel = *me.trade_sel.lock().unwrap() as usize;
                                let ts = trades();
                                if slot == 2 && sel < ts.len() {
                                    let (ai, ac, bin, ci, cc) = ts[sel];
                                    let mut cur = me.cursor.lock().unwrap();
                                    let mut mi = me.merchant_in.lock().unwrap();
                                    let have1 = mi[0].0 == ai && mi[0].1 >= ac;
                                    let have2 = bin.map(|(bi, bc)| mi[1].0 == bi && mi[1].1 >= bc).unwrap_or(true);
                                    if have1 && have2 && (cur.1 == 0 || (cur.0 == ci && cur.1 + cc <= 64)) {
                                        *cur = (ci, cur.1 + cc);
                                        mi[0].1 -= ac; if mi[0].1 == 0 { mi[0] = (0, 0); }
                                        if let Some((_, bc)) = bin { if mi[1].1 >= bc { mi[1].1 -= bc; if mi[1].1 == 0 { mi[1] = (0, 0); } } }
                                        let (s0, s1) = (mi[0], mi[1]);
                                        drop(mi); drop(cur);
                                        // auto-refill from inventory
                                        let mut inv = me.inv.lock().unwrap();
                                        if s0.1 == 0 && inv_consume(&mut inv, ai, ac as i64) {
                                            let mut mi = me.merchant_in.lock().unwrap();
                                            mi[0] = (ai, ac);
                                        }
                                        if s1.1 == 0 { if let Some((bi, bc)) = bin { if inv_consume(&mut inv, bi, bc as i64) {
                                            let mut mi = me.merchant_in.lock().unwrap();
                                            mi[1] = (bi, bc);
                                        } } }
                                        let mi2 = *me.merchant_in.lock().unwrap();
                                        let can2 = bin.map(|(bi, bc)| mi2[1].0 == bi && mi2[1].1 >= bc).unwrap_or(true);
                                        let snap = inv.clone(); drop(inv);
                                        for wsl in 3..39i16 {
                                            let (it, c) = snap.get(&(wsl + 6)).copied().unwrap_or((0, 0));
                                            let mut p = vec![4u8];
                                            write_varint(&mut p, 0);
                                            p.extend_from_slice(&wsl.to_be_bytes());
                                            p.extend_from_slice(&slot_wire(c, it));
                                            let _ = send_packet(&mut s, 0x15, p);
                                        }
                                        let out2 = if mi2[0].0 == ai && mi2[0].1 >= ac && can2 { (cc, ci) } else { (0, 0) };
                                        for (wsl, it, c) in [(0i16, mi2[0].0, mi2[0].1), (1, mi2[1].0, mi2[1].1), (2, out2.1, out2.0)] {
                                            let mut p = vec![4u8];
                                            write_varint(&mut p, 0);
                                            p.extend_from_slice(&wsl.to_be_bytes());
                                            p.extend_from_slice(&slot_wire(c, it));
                                            let _ = send_packet(&mut s, 0x15, p);
                                        }
                                    }
                                } else if slot >= 3 && slot <= 38 {
                                    let mut inv = me.inv.lock().unwrap();
                                    let mut cur = me.cursor.lock().unwrap();
                                    click_slot(&mut inv, &mut cur, slot + 6, btn as i8);
                                    let (it, c) = inv.get(&(slot + 6)).copied().unwrap_or((0, 0));
                                    drop(cur); drop(inv);
                                    let mut p = vec![4u8];
                                    write_varint(&mut p, 0);
                                    p.extend_from_slice(&slot.to_be_bytes());
                                    p.extend_from_slice(&slot_wire(c, it));
                                    let _ = send_packet(&mut s, 0x15, p);
                                }
                                continue;
                            }
                            if *me.open_table.lock().unwrap() {
                                let mut grid = me.craft9.lock().unwrap();
                                let mut inv = me.inv.lock().unwrap();
                                let mut cur = me.cursor.lock().unwrap();
                                if slot == -999 {
                                    if cur.1 > 0 {
                                        let it = *cur; *cur = (0, 0);
                                        let (px, py, pz, ..) = { let p = me.pos.lock().unwrap(); *p };
                                        let d = Drop { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), x: px, y: py + 1.3, z: pz, item: it.0, age: Instant::now() };
                                        let sp = spawn_drop_pkt(&d); let md = drop_meta_pkt(&d);
                                        drops().lock().unwrap().push(d);
                                        drop(cur); drop(grid); drop(inv);
                                        broadcast(-1, |_c| (0x01, sp.clone()));
                                        broadcast(-1, |_c| (0x58, md.clone()));
                                        continue;
                                    }
                                } else if slot == 0 {
                                    if let Some((ri, rc)) = craft3_result(&grid) {
                                        if cur.1 == 0 || (cur.0 == ri && cur.1 + rc <= 64) {
                                            for i in 1..=9i16 {
                                                if let Some(&(it, c)) = grid.get(&i) {
                                                    if c <= 1 { grid.remove(&i); } else { grid.insert(i, (it, c - 1)); }
                                                }
                                            }
                                            *cur = (ri, cur.1 + rc);
                                        }
                                    }
                                } else if (1..=9).contains(&slot) {
                                    click_slot(&mut grid, &mut cur, slot, btn as i8);
                                } else if slot >= 10 && slot <= 45 {
                                    click_slot(&mut inv, &mut cur, slot - 1, btn as i8);
                                    *me.armor_pts.lock().unwrap() = armor_pts_of(&inv);
                                }
                                // echo changed slot + result
                                let (it, c) = if slot == 0 { (0, 0) }
                                    else if (1..=9).contains(&slot) { grid.get(&slot).copied().unwrap_or((0, 0)) }
                                    else if slot >= 10 { inv.get(&(slot - 1)).copied().unwrap_or((0, 0)) }
                                    else { (0, 0) };
                                let out = craft3_result(&grid).unwrap_or((0, 0));
                                drop(grid); drop(inv); drop(cur);
                                if slot >= 0 {
                                    let mut p = vec![3u8];
                                    write_varint(&mut p, 0);
                                    p.extend_from_slice(&slot.to_be_bytes());
                                    p.extend_from_slice(&slot_wire(c, it));
                                    let _ = send_packet(&mut s, 0x15, p);
                                }
                                let mut p0 = vec![3u8];
                                write_varint(&mut p0, 0);
                                p0.extend_from_slice(&0i16.to_be_bytes());
                                p0.extend_from_slice(&slot_wire(out.1, out.0));
                                let _ = send_packet(&mut s, 0x15, p0);
                                continue;
                            }
                            if slot == -999 {
                                let cur = { let c = me.cursor.lock().unwrap(); *c };
                                if cur.1 > 0 {
                                    *me.cursor.lock().unwrap() = (0, 0);
                                    let (px, py, pz, ..) = { let p = me.pos.lock().unwrap(); *p };
                                    let d = Drop { eid: NEXT_EID.fetch_add(1, Ordering::SeqCst), x: px, y: py + 1.3, z: pz, item: cur.0, age: Instant::now() };
                                    let sp = spawn_drop_pkt(&d); let md = drop_meta_pkt(&d);
                                    drops().lock().unwrap().push(d);
                                    broadcast(-1, |_c| (0x01, sp.clone()));
                                    broadcast(-1, |_c| (0x58, md.clone()));
                                }
                            } else if slot == 0 {
                                // craft output: take result, consume one of each ingredient
                                let mut cur = me.cursor.lock().unwrap();
                                let mut grid = me.craft.lock().unwrap();
                                if let Some((ri, rc)) = craft_result(&grid) {
                                    if cur.1 == 0 || (cur.0 == ri && cur.1 + rc <= 64) {
                                        for i in 1..=4i16 {
                                            if let Some(&(it, c)) = grid.get(&i) {
                                                if c <= 1 { grid.remove(&i); } else { grid.insert(i, (it, c - 1)); }
                                            }
                                        }
                                        *cur = (ri, cur.1 + rc);
                                        for i in 0..=4i16 {
                                            let (it, c) = if i == 0 { craft_result(&grid).unwrap_or((0, 0)) } else { grid.get(&i).copied().unwrap_or((0, 0)) };
                                            let mut p = vec![0u8];
                                            write_varint(&mut p, 0);
                                            p.extend_from_slice(&i.to_be_bytes());
                                            p.extend_from_slice(&slot_wire(c, it));
                                            let _ = send_packet(&mut s, 0x15, p);
                                        }
                                    }
                                }
                            } else if (1..=4).contains(&slot) {
                                let mut grid = me.craft.lock().unwrap();
                                let mut cur = me.cursor.lock().unwrap();
                                click_slot(&mut grid, &mut cur, slot, btn as i8);
                                let ns = grid.get(&slot).copied();
                                let out = craft_result(&grid).unwrap_or((0, 0));
                                drop(grid); drop(cur);
                                let mut p = vec![0u8];
                                write_varint(&mut p, 0);
                                p.extend_from_slice(&slot.to_be_bytes());
                                p.extend_from_slice(&slot_wire(ns.map(|e| e.1).unwrap_or(0), ns.map(|e| e.0).unwrap_or(0)));
                                let _ = send_packet(&mut s, 0x15, p);
                                let mut p0 = vec![0u8];
                                write_varint(&mut p0, 0);
                                p0.extend_from_slice(&0i16.to_be_bytes());
                                p0.extend_from_slice(&slot_wire(out.1, out.0));
                                let _ = send_packet(&mut s, 0x15, p0);
                            } else if slot >= 9 && slot <= 44 {
                                let mut inv = me.inv.lock().unwrap();
                                let mut cur = me.cursor.lock().unwrap();
                                click_slot(&mut inv, &mut cur, slot, btn as i8);
                                *me.armor_pts.lock().unwrap() = armor_pts_of(&inv);
                                let ns = inv.get(&slot).copied();
                                drop(inv); drop(cur);
                                let mut p = vec![0u8];
                                write_varint(&mut p, 0);
                                p.extend_from_slice(&slot.to_be_bytes());
                                p.extend_from_slice(&slot_wire(ns.map(|e| e.1).unwrap_or(0), ns.map(|e| e.0).unwrap_or(0)));
                                let _ = send_packet(&mut s, 0x15, p);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    {
        eprintln!("[spine] {} left", name);
        {
            // remove owned projectile/bobber entities
            let mut fb = bobbers().lock().unwrap();
            let gone_b: Vec<i32> = fb.iter().filter(|b| b.owner == me_cleanup.eid).map(|b| b.eid).collect();
            fb.retain(|b| b.owner != me_cleanup.eid);
            let mut pq = pearls().lock().unwrap();
            let gone_p: Vec<i32> = pq.iter().filter(|p| p.owner == me_cleanup.eid).map(|p| p.eid).collect();
            pq.retain(|p| p.owner != me_cleanup.eid);
            for eid in gone_b.iter().chain(gone_p.iter()) {
                let mut p = Vec::new(); write_varint(&mut p, 1); write_varint(&mut p, *eid as i64);
                broadcast(-1, |_c| (0x42, p.clone()));
            }
        }
        {
            // return merchant trade inputs before persisting
            let mi = *me_cleanup.merchant_in.lock().unwrap();
            let mut inv = me_cleanup.inv.lock().unwrap();
            for (it, c) in mi { if c > 0 {
                let slot = (9..=44).find(|sl| !inv.contains_key(sl)).unwrap_or(9);
                inv.insert(slot, (it, c));
            } }
            inv_store().lock().unwrap().insert(name.clone(), inv.clone());
        }
        let mut cls = clients().lock().unwrap();
        cls.retain(|c| c.eid != me_cleanup.eid);
        drop(cls);
        broadcast(me_cleanup.eid, |c| (0x42, {
            let mut p = Vec::new(); write_varint(&mut p, 1); write_varint(&mut p, me_cleanup.eid as i64); p
        }));
        let mut p = Vec::new(); write_varint(&mut p, 1); p.extend_from_slice(&me_cleanup.uuid);
        broadcast(me_cleanup.eid, |_c| (0x3d, p.clone()));
    }
}

fn main() {
    load_world();
    http_server();
    load_datapacks();
    {
        let loads: Vec<String> = packs().lock().unwrap().iter().flat_map(|p| p.load.clone()).collect();
        for f in loads { run_function(&f, None, &mut None); }
    }
    // seed a few mobs near spawn (only if none restored from save)
    if mobs().lock().unwrap().is_empty() {
        let mut ms = mobs().lock().unwrap();
        // (etype, x, z): 22=cow, 124=zombie (1.21.1 entity ids)
        for (i, (et, x, z)) in [(22i64, 5.5f64, 2.5f64), (22, -3.5, 4.5), (124, 8.5, 6.5), (124, -8.5, -6.5)].iter().enumerate() {
            ms.push(Arc::new(Mob {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                uuid: [i as u8; 16],
                etype: *et,
                pos: Mutex::new((*x, surface_y(*x as i32, *z as i32), *z, 0.0, 0.0)),
                hp: Mutex::new(20.0),
                last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
                fuse: Mutex::new(0),
                love: Mutex::new(0),
            }));
        }
    }
    thread::spawn(|| loop {
        std::thread::sleep(Duration::from_millis(250));
        keepalive_time_tick();
        drop_tick();
        arrow_tick();
        primed_tick();
        furnace_tick();
        mob_tick();
    });
    let port: u16 = std::env::var("MC_SPINE_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(25599);
    let l = TcpListener::bind(("0.0.0.0", port)).unwrap();
    eprintln!("mc-spine v0.6 listening :{}", port);
    static ACTIVE: AtomicUsize = AtomicUsize::new(0);
    for st in l.incoming() {
        if let Ok(s) = st {
            // connection cap: refuse above 64 live handler threads
            if ACTIVE.fetch_add(1, Ordering::SeqCst) >= 64 {
                ACTIVE.fetch_sub(1, Ordering::SeqCst);
                drop(s);
                continue;
            }
            thread::spawn(move || {
                handle(s);
                ACTIVE.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }
}
