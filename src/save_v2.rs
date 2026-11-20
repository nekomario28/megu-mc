/// A saved player position with its look direction *and* the dimension they logged out in. On disk
/// the dimension lives in the footer; the legacy six fields stay in their original wire layout.
type DimPosRot = (DimId, f64, f64, f64, f32, f32);

/// MET4 stores durations in server ticks. MET2 and MET3 stored the same counters as
/// 4TPS server ticks, which span five times the wall time, so a legacy value read
/// directly at 20TPS expires five times early. Dividing by TICK_MS converts the
/// legacy millisecond-equivalent budget, and TICK_MS went 250 to 50, so the factor
/// is exactly the cadence ratio.
fn legacy_ticks(raw: u64, legacy: bool) -> u64 {
    if legacy {
        raw / TICK_MS
    } else {
        raw
    }
}
/// The tick period MET2 and MET3 files were written at. It is a property of the
/// file format, not of the running server, so it is a fixed historical value and
/// must not be derived from TICK_MS.
const LEGACY_TICK_MS: u64 = 250;
/// Counters the legacy format stored as a number of 4TPS server ticks, not as
/// milliseconds: arrow and pearl ttl, primed TNT left, furnace burn_left and
/// progress, and the weather counters. A legacy tick spans 250ms, so it is five
/// 20TPS ticks; dividing would shorten the budget fivefold, which is the very
/// bug the legacy conversion exists to prevent.
fn legacy_tick_count(raw: u64, legacy: bool) -> u64 {
    if legacy {
        raw * (LEGACY_TICK_MS / TICK_MS)
    } else {
        raw
    }
}

static WORLD_SEED: AtomicU64 = AtomicU64::new(0);

static INV_STORE: OnceLock<Mutex<HashMap<String, HashMap<i16, Stack>>>> = OnceLock::new();
fn inv_store() -> &'static Mutex<HashMap<String, HashMap<i16, Stack>>> {
    INV_STORE.get_or_init(|| Mutex::new(HashMap::new()))
}
// name -> (x,y,z,yaw,pitch): vanilla respawns a player where they left
static POS_STORE: Registry<String, DimPosRot> = OnceLock::new();
/// The position record a client leaves behind: its live dimension plus the six legacy
/// fields. Both the logout write and the periodic world save call this, so a test can prove
/// the persisted dimension comes from `Client::dim` rather than a hard-coded Overworld.
fn client_dim_pos(c: &Client) -> DimPosRot {
    let dim = c.dim.lock().unwrap();
    let p = *c.pos.lock().unwrap();
    (*dim, p.0, p.1, p.2, p.3, p.4)
}
fn pos_store() -> &'static LockedMap<String, DimPosRot> {
    POS_STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn rebuild_derived_indexes() {
    // Restart continuity: waters/hoppers/crops/plugin-cells are indexes OF
    // block state — a restart must rebuild them from the overlay exactly
    // as set_block would have registered them, or a reboot silently
    // freezes every machine vanilla would keep running.
    let owned = GENERATED_FLUID_OWNER
        .get()
        .map(|owner| owner.lock().unwrap().owned.clone())
        .unwrap_or_default();
    let w = world().lock().unwrap();
    let mut wtr = waters().lock().unwrap();
    let mut hh = hoppers().lock().unwrap();
    let mut cg = crops().lock().unwrap();
    let mut cells = plugin_cells().lock().unwrap();
    let lamp_on = !plugins().lock().unwrap().is_empty();
    // The indexes below hold positions, not `(dimension, position)`, so they describe the
    // Overworld: `rebuild_derived_indexes` walks the Overworld's edits and leaves another
    // dimension's alone until the indexes name a dimension themselves (S3).
    for (&(dim, cx, cz), c) in w.iter() {
        if dim != DimId::Overworld {
            continue;
        }
        for (idx, st) in c.iter() {
            let x = cx * 16 + (*idx & 15) as i32;
            let z = cz * 16 + ((*idx >> 4) & 15) as i32;
            let y = (*idx >> 8) as i32 + MIN_Y;
            if (80..=95).contains(st) {
                wtr.insert((x, y, z));
                if !owned.contains(&(x, y, z)) {
                    water_queue().lock().unwrap().schedule((x, y, z), 1);
                }
            }
            if (96..=111).contains(st) {
                lavas().lock().unwrap().insert((x, y, z));
                if !owned.contains(&(x, y, z)) {
                    lava_queue().lock().unwrap().schedule((x, y, z), 1);
                }
            }
            if (9225..=9234).contains(st) {
                hh.insert((x, y, z));
                hopper_queue().lock().unwrap().schedule((x, y, z), 1);
            }
            if (4278..=4285).contains(st) {
                cg.insert((DimId::Overworld, x, y, z), (*st - 4278) as u8);
            }
            if lamp_on && *st == ST_GROWTH_LAMP {
                cells
                    .entry((DimId::Overworld, cx, cz))
                    .or_default()
                    .push(PluginCell {
                    pi: 0,
                    x,
                    y,
                    z,
                    key: 0,
                    warm_ok: true,
                });
            }
        }
    }
    eprintln!(
        "[spine] rebuilt indexes: {} water, {} hoppers, {} crops, {} cells",
        wtr.len(),
        hh.len(),
        cg.len(),
        cells.len()
    );
}

// ---------- persistence (overlay only, no pregen) ----------
const WORLD_FILE: &str = "world_overlay.bin";
const WORLD_SEED_MAGIC: &[u8; 4] = b"SED1";
/// The companion overlay file of a dimension that is not the Overworld, or `WORLD_FILE` itself.
///
/// The legacy name and the legacy bytes stay the Overworld's: a server that only ever edits the
/// Overworld writes exactly the one file it always did, and none of the companions.
fn dim_overlay_name(dim: DimId) -> &'static str {
    match dim {
        DimId::Overworld => WORLD_FILE,
        DimId::Nether => "world_overlay_nether.bin",
        DimId::End => "world_overlay_end.bin",
    }
}

fn dim_overlay_path(dir: &std::path::Path, dim: DimId) -> std::path::PathBuf {
    dir.join(dim_overlay_name(dim))
}

/// The directory the world save lives in: `WORLD_FILE`'s own, so the default bare file name means
/// the process's working directory — the same place the legacy save has always been read from.
fn world_save_dir() -> std::path::PathBuf {
    std::path::Path::new(WORLD_FILE)
        .parent()
        .map_or_else(std::path::PathBuf::new, std::path::Path::to_path_buf)
}

/// One dimension's overlay, as a save payload: `count`, then per chunk `cx, cz, count` and its
/// `idx, state` pairs. Returns how many chunks the table holds (0 = this dimension has no edits).
///
/// There is deliberately one writer for both the legacy file's leading table and a companion
/// file, and one reader for both, so "the Overworld's table" and "the same format for another
/// dimension" cannot drift into two formats.
fn save_dim_overlay_into(dim: DimId, f: &mut Vec<u8>) -> usize {
    let w = world().lock().unwrap();
    let chunks: Vec<(&DimChunkKey, &HashMap<u32, u32>)> =
        w.iter().filter(|(key, _)| key.0 == dim).collect();
    let count = chunks.len();
    write_varint(&mut *f, count as i64);
    for (key, c) in chunks {
        let (_, cx, cz) = *key;
        (*f).extend_from_slice(&cx.to_be_bytes());
        (*f).extend_from_slice(&cz.to_be_bytes());
        write_varint(&mut *f, c.len() as i64);
        for (&idx, &st) in c.iter() {
            (*f).extend_from_slice(&idx.to_be_bytes());
            write_varint(&mut *f, st as i64);
        }
    }
    count
}

/// Read one dimension's chunk table, starting at `p`, and apply its edits when requested.
///
/// The cursor form exists because the legacy file carries META sections after this table, so
/// `load_world_from` has to continue parsing where the table ended. A companion file is only the
/// table, which is what [`load_dim_overlay_from`] reads.
fn read_dim_overlay_at(dim: DimId, payload: &[u8], p: &mut usize, apply: bool) {
    let n = rv_varint(payload, p);
    let mut w = apply.then(|| world().lock().unwrap());
    for _ in 0..n {
        if *p + 8 > payload.len() {
            break;
        }
        let cx = i32::from_be_bytes(payload[*p..*p + 4].try_into().unwrap());
        *p += 4;
        let cz = i32::from_be_bytes(payload[*p..*p + 4].try_into().unwrap());
        *p += 4;
        let m = rv_varint(payload, p);
        let mut c = w.as_mut().map(|world| world.entry((dim, cx, cz)).or_default());
        for _ in 0..m {
            if *p + 4 > payload.len() {
                break;
            }
            let idx = u32::from_be_bytes(payload[*p..*p + 4].try_into().unwrap());
            *p += 4;
            let st = rv_varint(payload, p) as u32;
            if let Some(c) = c.as_deref_mut() {
                c.insert(idx, st);
            }
        }
        // `bump_world_epoch` moves the Overworld chunk's band/section caches, so only an
        // Overworld chunk may move it: a Nether cell is not part of what they describe.
        if apply && dim == DimId::Overworld {
            bump_world_epoch(cx << 4, cz << 4);
        }
    }
}

/// A companion file's payload: exactly one dimension's chunk table, nothing after it.
fn load_dim_overlay_from(dim: DimId, payload: &[u8]) -> Result<(), &'static str> {
    let mut p = 0usize;
    read_dim_overlay_at(dim, payload, &mut p, true);
    if p != payload.len() {
        return Err("trailing bytes after a dimension's overlay table");
    }
    Ok(())
}
fn write_comps(f: &mut Vec<u8>, c: &Comps) {
    write_varint(f, c.len() as i64);
    for (ty, d) in c {
        write_varint(f, *ty);
        f.extend_from_slice(&(d.len() as u16).to_be_bytes());
        f.extend_from_slice(d);
    }
}
fn read_comps(b: &[u8], p: &mut usize) -> Comps {
    let n = rv_varint(b, p);
    let mut c = Vec::new();
    for _ in 0..n {
        // The type is a variable-width VarInt, so the 2-byte length field can only
        // be checked after it has been consumed. Checking first and then indexing
        // panicked on a truncated record.
        if *p >= b.len() {
            break;
        }
        let ty = rv_varint(b, p);
        // The VarInt itself may consume all remaining bytes.
        if b.len().saturating_sub(*p) < 2 {
            break;
        }
        let l = u16::from_be_bytes(b[*p..*p + 2].try_into().unwrap()) as usize;
        *p += 2;
        if b.len().saturating_sub(*p) < l {
            break;
        }
        c.push((ty, b[*p..*p + l].to_vec()));
        *p += l;
    }
    c
}
/// The one wire encoder for a container's slot table. The legacy chest section and the DIM2
/// footer both call this, so a future change to item components cannot make one format drift.
fn write_stack_table(out: &mut Vec<u8>, slots: &HashMap<i16, Stack>) {
    write_varint(out, slots.len() as i64);
    for (&slot, (item, cnt, comps)) in slots.iter() {
        out.extend_from_slice(&slot.to_be_bytes());
        write_varint(out, *item);
        out.push(*cnt);
        write_comps(out, comps);
    }
}
/// The matching decoder. `has_comps` is false for MET2 (the pre-component wire form); the lenient
/// `break` on truncation is the legacy loader's behaviour, and DIM2 adds strictness by requiring
/// that the footer's length-delimited body be consumed exactly.
fn read_stack_table(payload: &[u8], p: &mut usize, has_comps: bool) -> HashMap<i16, Stack> {
    let n = rv_varint(payload, p);
    let mut slots = HashMap::new();
    for _ in 0..n {
        if p.saturating_add(3) > payload.len() {
            break;
        }
        let slot = i16::from_be_bytes(payload[*p..*p + 2].try_into().unwrap());
        *p += 2;
        let item = rv_varint(payload, p);
        if *p >= payload.len() {
            break;
        }
        let cnt = payload[*p];
        *p += 1;
        let comps = if has_comps {
            read_comps(payload, p)
        } else {
            Vec::new()
        };
        slots.insert(slot, (item, cnt, comps));
    }
    slots
}
/// A byte-for-byte copy of the fields the footer serialises. The caches are deliberately fresh:
/// they are derived state and are rebuilt on load, never persisted.
fn clone_furnace_for_footer(fr: &Furnace) -> Furnace {
    Furnace {
        input: fr.input.clone(),
        fuel: fr.fuel.clone(),
        output: fr.output.clone(),
        burn_left: fr.burn_left,
        progress: fr.progress,
        dirty: false,
        xp: 0.0,
        recipe_cache: CachedLookup::default(),
        fuel_cache: CachedLookup::default(),
    }
}
/// One Furnace's three stacks and two counters, in the legacy furnace-section layout.
fn write_furnace_state(out: &mut Vec<u8>, fr: &Furnace) {
    write_varint(out, fr.input.0);
    out.push(fr.input.1);
    write_comps(out, &fr.input.2);
    write_varint(out, fr.fuel.0);
    out.push(fr.fuel.1);
    write_comps(out, &fr.fuel.2);
    write_varint(out, fr.output.0);
    out.push(fr.output.1);
    write_comps(out, &fr.output.2);
    out.extend_from_slice(&fr.burn_left.to_be_bytes());
    out.extend_from_slice(&fr.progress.to_be_bytes());
}
fn read_furnace_state(
    payload: &[u8],
    p: &mut usize,
    has_comps: bool,
    legacy_ms_ages: bool,
) -> Option<Furnace> {
    let rd = |p: &mut usize| -> Stack {
        let it = rv_varint(payload, p);
        let c = if *p < payload.len() { payload[*p] } else { 0 };
        *p += 1;
        let comps = if has_comps {
            read_comps(payload, p)
        } else {
            Vec::new()
        };
        (it, c, comps)
    };
    let input = rd(p);
    let fuel = rd(p);
    let output = rd(p);
    if p.saturating_add(8) > payload.len() {
        return None;
    }
    let burn_left = legacy_tick_count(
        i32::from_be_bytes(payload[*p..*p + 4].try_into().unwrap()) as u64,
        legacy_ms_ages,
    ) as i32;
    *p += 4;
    let progress = legacy_tick_count(
        i32::from_be_bytes(payload[*p..*p + 4].try_into().unwrap()) as u64,
        legacy_ms_ages,
    ) as i32;
    *p += 4;
    Some(Furnace {
        input,
        fuel,
        output,
        burn_left,
        progress,
        dirty: false,
        xp: 0.0,
        recipe_cache: CachedLookup::default(),
        fuel_cache: CachedLookup::default(),
    })
}
fn rv_varint(b: &[u8], p: &mut usize) -> i64 {
    let mut v = 0i64;
    let mut sh = 0;
    loop {
        if *p >= b.len() {
            break;
        }
        let x = b[*p];
        *p += 1;
        v |= ((x & 0x7f) as i64) << sh;
        if x & 0x80 == 0 {
            break;
        }
        sh += 7;
        if sh > 63 {
            break;
        }
    }
    v
}
// Envelope protects new saves from silent partial-load and overwrite after a
// truncated or torn write. Legacy MET2/MET3 files remain readable for migration.
// SHA-1 detects accidental damage; it does not authenticate untrusted files.
const SAVE_MAGIC: &[u8; 4] = b"SPN4";
fn wrap_world_payload(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 32);
    out.extend_from_slice(SAVE_MAGIC);
    out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    out.extend_from_slice(&sha1(payload));
    out.extend_from_slice(payload);
    out
}
fn unwrap_world_payload(raw: Vec<u8>) -> Result<Vec<u8>, &'static str> {
    if raw.starts_with(SAVE_MAGIC) {
        if raw.len() < 32 {
            return Err("truncated save header");
        }
        let len = u64::from_be_bytes(raw[4..12].try_into().unwrap());
        if len != (raw.len() - 32) as u64 {
            return Err("save length mismatch");
        }
        if sha1(&raw[32..]) != raw[12..32] {
            return Err("save checksum mismatch");
        }
        return Ok(raw[32..].to_vec());
    }
    if raw.starts_with(b"SPN") {
        return Err("unsupported or corrupted save format");
    }
    Ok(raw) // existing unwrapped v2.37 format
}
/// Serialises saves. The tick autosave and the SIGTERM/SIGINT handler both call `save_world`,
/// and they share one temporary path; two writers would interleave in it and the envelope would
/// reject the result.
static SAVE_LOCK: Mutex<()> = Mutex::new(());
thread_local! { static SAVE_GUARD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) } }
/// Run a world/entity ownership transition under `SAVE_LOCK`, so the signal-thread save cannot
/// observe the block removed but the entity not yet added, or a landing placed but its entity
/// still queued. Re-entrant per thread: `spawn_fall` is reached from `falling_tick`'s placement
/// and from another `spawn_fall`, and only the outermost call locks.
fn with_save_guard<R>(f: impl FnOnce() -> R) -> R {
    if SAVE_GUARD.with(|g| g.get()) {
        return f();
    }
    let _save = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    SAVE_GUARD.with(|g| g.set(true));
    struct Reset;
    impl std::ops::Drop for Reset {
        fn drop(&mut self) {
            SAVE_GUARD.with(|g| g.set(false));
        }
    }
    let _reset = Reset;
    f()
}
fn save_world() {
    with_save_guard(save_world_guarded);
}
fn save_world_guarded() {
    save_world_guarded_in(&world_save_dir());
}
/// [`save_world_guarded`] against one directory: the legacy Overworld file, then one companion
/// per dimension that has edits.
///
/// Caller owns `SAVE_LOCK` (or is a single-threaded fixture), exactly as for `save_world_into`.
fn save_world_guarded_in(dir: &std::path::Path) {
    // save_world_into only builds the payload; the write happens here so the
    // snapshot stays observational and there is exactly one disk path.
    let mut f = Vec::new();
    save_world_into(&mut f);
    // The envelope lets load reject a torn or truncated write, and writing via a
    // temporary file plus a retained backup means a failed write cannot destroy
    // the previous save.
    let framed = wrap_world_payload(&f);
    if let Err(error) = write_world_file(
        &dim_overlay_path(dir, DimId::Overworld),
        &framed,
        connection_limits::backup_interval(),
    ) {
        eprintln!("[spine] WORLD SAVE FAILED: {error}");
    }
    for dim in [DimId::Nether, DimId::End] {
        if let Err(error) = save_dim_overlay_file(dir, dim) {
            eprintln!("[spine] WORLD SAVE FAILED: {error}");
        }
    }
}

/// Write one dimension's companion overlay, or remove the stale file when it has no edits.
///
/// A dimension with no overlay chunks must leave nothing behind: an Overworld-only server would
/// otherwise grow a pair of files on its first save, and a dimension whose last edit was undone
/// would keep a file that resurrects that edit on the next load.
fn save_dim_overlay_file(dir: &std::path::Path, dim: DimId) -> Result<(), String> {
    if dim == DimId::Overworld {
        // The Overworld's file is the whole save — chunk table plus META — written by the caller.
        return Ok(());
    }
    let mut f = Vec::new();
    let chunks = save_dim_overlay_into(dim, &mut f);
    let path = dim_overlay_path(dir, dim);
    if chunks == 0 {
        return remove_world_save(&path);
    }
    write_world_file(
        &path,
        &wrap_world_payload(&f),
        connection_limits::backup_interval(),
    )
}

/// Remove a save and every sibling `write_world_file` may have left beside it.
fn remove_world_save(path: &std::path::Path) -> Result<(), String> {
    for suffix in ["", ".tmp", ".bak", ".bak.1", ".bak.2"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let target = std::path::PathBuf::from(name);
        match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot remove {}: {error}", target.display())),
        }
    }
    Ok(())
}
/// Write `framed` to `path` via `path.tmp` (fsynced), then rename over it.
///
/// Backups are `path.bak` (the previous save, so at most one autosave old),
/// `path.bak.1` and `path.bak.2`. The older two only advance when `bak.1` is at
/// least `interval` old, so three generations span more than a few autosaves.
/// Every failure is returned with the path involved; the live file is only
/// replaced after the tmp file is complete.
fn write_world_file(
    path: &std::path::Path,
    framed: &[u8],
    interval: Duration,
) -> Result<(), String> {
    let with = |suffix: &str| {
        let mut o = path.as_os_str().to_owned();
        o.push(suffix);
        std::path::PathBuf::from(o)
    };
    let (tmp, bak, bak1, bak2) = (with(".tmp"), with(".bak"), with(".bak.1"), with(".bak.2"));
    {
        let mut f = std::fs::File::create(&tmp)
            .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
        f.write_all(framed)
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    }
    if path.exists() {
        // Backup trouble is reported but never blocks the save itself.
        let age = |p: &std::path::Path| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
        };
        if bak.exists() && age(&bak1).is_none_or(|a| a >= interval) {
            if bak1.exists() {
                if let Err(e) = std::fs::rename(&bak1, &bak2) {
                    eprintln!("[spine] backup rotate {} failed: {e}", bak1.display());
                }
            }
            if let Err(e) = std::fs::copy(&bak, &bak1) {
                eprintln!("[spine] backup rotate {} failed: {e}", bak.display());
            }
        }
        if let Err(e) = std::fs::copy(path, &bak) {
            eprintln!("[spine] backup {} failed: {e}", bak.display());
        }
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        format!(
            "cannot rename {} over {}: {e}",
            tmp.display(),
            path.display()
        )
    })
}
// Caller owns SAVE_LOCK (or is a single-threaded fixture). Some existing
// callers hold the mutex directly, without the re-entrant thread-local guard.
fn save_world_into(f: &mut Vec<u8>) {
    let world_spawn = *WORLD_SPAWN.lock().unwrap();
    let (pending, sources, spider_flags) = worldgen::with_gen(|g| (
        g.take_staged_payloads(), g.payload_sources.clone(), g.mineshaft_spawners.clone()
    )).unwrap_or_default();
    materialize_generation_payloads_inner(pending);
    let mut content = generated_content().lock().unwrap().clone();
    {
        let ch = chests().lock().unwrap();
        for cart in &mut content.carts {
            if !cart.has_loot_table() {
                if let Ok(uuid) = cart.uuid() {
                    let slots = ch.get(&ContainerKey::Cart(uuid)).cloned().unwrap_or_default();
                    if let crate::nbt::Tag::Compound(fields) = &mut cart.data {
                        fields.insert(crate::nbt::NbtString::text("Items"), stacks_to_items_nbt(&slots));
                    }
                }
            }
        }
    }
    let generated = generated_content::Snapshot {
        content, sources, spider_flags,
    };
    // The leading chunk table is the Overworld's, which is what the legacy file has always held:
    // its count, its keys and its bytes are unchanged, and another dimension's edits go to that
    // dimension's own companion file (`save_world_guarded_in`).
    save_dim_overlay_into(DimId::Overworld, f);
    // META section: mobs + world time + inventories (offline store + live).
    // MET4 = stacks carry component blobs and entity ages are server ticks.
    // MET2/MET3 stored those ages in milliseconds, which the loader converts.
    let mut drop_owners: Vec<(u32, Option<[u8; 16]>, Option<[u8; 16]>)> = Vec::new();
    // Non-Overworld signs and player dimensions ride in a footer (see `append_dim_footer`), so the
    // legacy sections below stay byte-for-byte identical for an Overworld-only save.
    let dim_signs: Vec<(DimId, i32, i32, i32, u8, [String; 4])>;
    let mut dim_players: Vec<(String, DimId)> = Vec::new();
    // Chests, furnaces, crops and pending loot outside the Overworld go to their own footer; the
    // legacy sections below carry the Overworld's entries in the old wire layout. Crops are
    // collected before any container lock: the growth tick holds `crops` while writing a block,
    // which can take `chests`, so taking them in the opposite order here would invert the locks.
    let mut dim2 = Dim2Entries::default();
    dim2.crops = crops()
        .lock()
        .unwrap()
        .iter()
        .filter(|(pos, _)| pos.0 != DimId::Overworld)
        .map(|(pos, age)| (*pos, *age))
        .collect();
    (*f).extend_from_slice(b"MET4");
    let mob_dimensions;
    {
        let ms = mobs().lock().unwrap();
        mob_dimensions = ms.iter().map(|mob| mob.dim).collect::<Vec<_>>();
        write_varint(&mut *f, ms.len() as i64);
        for m in ms.iter() {
            let (x, y, z, ..) = {
                let p = m.pos.lock().unwrap();
                *p
            };
            write_varint(&mut *f, m.etype);
            (*f).extend_from_slice(&x.to_be_bytes());
            (*f).extend_from_slice(&y.to_be_bytes());
            (*f).extend_from_slice(&z.to_be_bytes());
            (*f).extend_from_slice(
                &{
                    let h = m.hp.lock().unwrap();
                    *h
                }
                .to_be_bytes(),
            );
        }
    }
    (*f).extend_from_slice(&WORLD_TIME.load(Ordering::Relaxed).to_be_bytes());
    {
        let mut all: HashMap<String, HashMap<i16, Stack>> = inv_store().lock().unwrap().clone();
        for c in clients().lock().unwrap().iter() {
            all.insert(c.name.clone(), c.inv.lock().unwrap().clone());
        }
        write_varint(&mut *f, all.len() as i64);
        for (name, inv) in all.iter() {
            (*f).extend_from_slice(&(name.len() as u16).to_be_bytes());
            (*f).extend_from_slice(name.as_bytes());
            write_varint(&mut *f, inv.len() as i64);
            for (&slot, (item, cnt, ic)) in inv.iter() {
                (*f).extend_from_slice(&slot.to_be_bytes());
                write_varint(&mut *f, *item);
                (*f).push(*cnt);
                write_varint(&mut *f, ic.len() as i64);
                for (ty, d) in ic {
                    write_varint(&mut *f, *ty);
                    (*f).extend_from_slice(&(d.len() as u16).to_be_bytes());
                    (*f).extend_from_slice(d);
                }
            }
        }
        let ch = chests().lock().unwrap();
        // The legacy chest table holds the Overworld's block containers only — including the
        // synthetic per-player Ender keys, which always live under the Overworld — in the exact
        // old `(x, y, z)` wire layout. Anything in another dimension goes to DIM2.
        let ow_chests: Vec<_> = ch
            .iter()
            .filter_map(|(key, slots)| match key {
                ContainerKey::Block((DimId::Overworld, x, y, z)) => Some(((*x, *y, *z), slots)),
                ContainerKey::Block(pos) => {
                    dim2.chests.push((*pos, slots.clone()));
                    None
                }
                ContainerKey::Cart(_) => None,
            })
            .collect();
        write_varint(&mut *f, ow_chests.len() as i64);
        for ((x, y, z), slots) in ow_chests {
            (*f).extend_from_slice(&x.to_be_bytes());
            (*f).extend_from_slice(&y.to_be_bytes());
            (*f).extend_from_slice(&z.to_be_bytes());
            write_stack_table(&mut *f, slots);
        }
        let sg = signs().lock().unwrap();
        // The legacy signs section carries the Overworld's signs only, in the exact old wire
        // layout. Another dimension's sign goes to the footer, so its presence cannot shift a
        // single byte of an Overworld-only save.
        let ow_signs: Vec<_> = sg.iter().filter(|(k, _)| k.0 == DimId::Overworld).collect();
        write_varint(&mut *f, ow_signs.len() as i64);
        for (k, lines) in &ow_signs {
            let &(_dim, x, y, z, front) = *k;
            (*f).extend_from_slice(&x.to_be_bytes());
            (*f).extend_from_slice(&y.to_be_bytes());
            (*f).extend_from_slice(&z.to_be_bytes());
            (*f).push(front);
            for l in lines.iter() {
                (*f).extend_from_slice(&(l.len() as u16).to_be_bytes());
                (*f).extend_from_slice(l.as_bytes());
            }
        }
        dim_signs = sg
            .iter()
            .filter(|(k, _)| k.0 != DimId::Overworld)
            .map(|(k, v)| (k.0, k.1, k.2, k.3, k.4, v.clone()))
            .collect();
        // forceload tail — vanilla keeps forced chunks across restarts;
        // losing them would silently freeze fixture+machine regions.
        let fl = forceloaded().lock().unwrap();
        write_varint(&mut *f, fl.len() as i64);
        for &(cx, cz) in fl.iter() {
            (*f).extend_from_slice(&cx.to_be_bytes());
            (*f).extend_from_slice(&cz.to_be_bytes());
        }
        // player positions — vanilla respawns you where you logged out. The legacy six fields are
        // written for every player (so an Overworld-only save is byte-identical); the dimension of
        // a player who logged out elsewhere is corrected from the footer on load.
        let mut allp: HashMap<String, DimPosRot> = pos_store().lock().unwrap().clone();
        for c in clients().lock().unwrap().iter() {
            allp.insert(c.name.clone(), client_dim_pos(c));
        }
        write_varint(&mut *f, allp.len() as i64);
        for (name, &(dim, x, y, z, yaw, pitch)) in allp.iter() {
            (*f).extend_from_slice(&(name.len() as u16).to_be_bytes());
            (*f).extend_from_slice(name.as_bytes());
            (*f).extend_from_slice(&x.to_be_bytes());
            (*f).extend_from_slice(&y.to_be_bytes());
            (*f).extend_from_slice(&z.to_be_bytes());
            (*f).extend_from_slice(&yaw.to_be_bytes());
            (*f).extend_from_slice(&pitch.to_be_bytes());
            if dim != DimId::Overworld {
                dim_players.push((name.clone(), dim));
            }
        }
        // dropped items are world entities in vanilla — they survive
        // restart inside their despawn budget
        // A trident that came to rest is persisted as a ground drop. Saving must be
        // observational: the conversion is written into the payload instead of
        // being applied to the live world, where it also lost the new drop because
        // the drop list had already been serialized.
        let tridents: Vec<(f64, f64, f64, i64, Comps)> = arrows()
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.kind == 1 && a.ret_item > 0)
            .map(|a| (a.x, a.y, a.z, a.ret_item, a.ret_comps.clone()))
            .collect();
        let mut drop_ages: Vec<u32> = Vec::new();
        {
            let ds = drops().lock().unwrap();
            write_varint(&mut *f, (ds.len() + tridents.len()) as i64);
            for (idx, d) in ds.iter().enumerate() {
                (*f).extend_from_slice(&d.eid.to_be_bytes());
                (*f).extend_from_slice(&d.x.to_be_bytes());
                (*f).extend_from_slice(&d.y.to_be_bytes());
                (*f).extend_from_slice(&d.z.to_be_bytes());
                write_varint(&mut *f, d.item);
                (*f).push(d.cnt);
                write_comps(&mut *f, &d.comps);
                // `as u32` is already a bit-preserving two's complement cast, so a
                // negative pickup-delay marker round-trips; only the load-side
                // clamp used to destroy it.
                drop_ages.push(d.age as u32); // server ticks, signed
                if d.target.is_some() || d.thrower.is_some() {
                    drop_owners.push((idx as u32, d.target, d.thrower));
                }
            }
            for (x, y, z, item, comps) in tridents.iter() {
                // eid 0 is remapped on load and age 0 grants a full pickup delay.
                (*f).extend_from_slice(&0i32.to_be_bytes());
                (*f).extend_from_slice(&x.to_be_bytes());
                (*f).extend_from_slice(&y.to_be_bytes());
                (*f).extend_from_slice(&z.to_be_bytes());
                write_varint(&mut *f, *item);
                (*f).push(1);
                write_comps(&mut *f, comps);
                drop_ages.push(0);
            }
        }
        // Furnaces are block entities — contents and smelting state are durable in vanilla. The
        // legacy furnace table holds the Overworld's entries in the old wire layout; another
        // dimension's furnace goes to DIM2.
        let fs = furnaces().lock().unwrap();
        let ow_furnaces: Vec<_> = fs
            .iter()
            .filter_map(|(pos, fr)| match pos.0 {
                DimId::Overworld => Some((pos, fr)),
                _ => {
                    dim2.furnaces.push((*pos, clone_furnace_for_footer(fr)));
                    None
                }
            })
            .collect();
        write_varint(&mut *f, ow_furnaces.len() as i64);
        for ((_, x, y, z), fr) in ow_furnaces {
            (*f).extend_from_slice(&x.to_be_bytes());
            (*f).extend_from_slice(&y.to_be_bytes());
            (*f).extend_from_slice(&z.to_be_bytes());
            write_furnace_state(&mut *f, fr);
        }
        // sheared-mob indices (eids are re-issued on load — persist by
        // mob order, not eid)
        {
            let ms = mobs().lock().unwrap();
            let sh = sheared().lock().unwrap();
            let idx: Vec<i64> = ms
                .iter()
                .enumerate()
                .filter(|(_, m)| sh.contains(&m.eid))
                .map(|(i, _)| i as i64)
                .collect();
            write_varint(&mut *f, idx.len() as i64);
            for i in idx {
                write_varint(&mut *f, i);
            }
        }
        // Keep the legacy scoreboard layout. Both pieces describe one snapshot.
        let (scoreboard_objectives, scoreboard_sidebar) = {
            let board = scoreboard().lock().unwrap();
            write_varint(&mut *f, board.objectives.len() as i64);
            for obj in board.objectives.keys() {
                (*f).extend_from_slice(&(obj.len() as u16).to_be_bytes());
                (*f).extend_from_slice(obj.as_bytes());
                let entries = board.scores.get(obj);
                write_varint(&mut *f, entries.map_or(0, HashMap::len) as i64);
                for (entry, score) in entries.into_iter().flatten() {
                    (*f).extend_from_slice(&(entry.len() as u16).to_be_bytes());
                    (*f).extend_from_slice(entry.as_bytes());
                    (*f).extend_from_slice(&i64::from(score.value).to_be_bytes());
                }
            }
            (
                board.objectives.values().map(|obj| (obj.name.clone(), obj.criteria)).collect::<Vec<_>>(),
                board.display(ScoreboardDisplaySlot::Sidebar).unwrap_or_default().to_string(),
            )
        };
        // XP orbs are world entities too — same persistence class as drops
        {
            let xs = xps().lock().unwrap();
            write_varint(&mut *f, xs.len() as i64);
            for x in xs.iter() {
                (*f).extend_from_slice(&x.eid.to_be_bytes());
                (*f).extend_from_slice(&x.x.to_be_bytes());
                (*f).extend_from_slice(&x.y.to_be_bytes());
                (*f).extend_from_slice(&x.z.to_be_bytes());
                (*f).extend_from_slice(&x.val.to_be_bytes());
                (*f).extend_from_slice(&x.age.to_be_bytes());
            }
        }
        // drop despawn-budget residue: elapsed ms per drop, by order
        for a in drop_ages {
            (*f).extend_from_slice(&a.to_be_bytes());
        }
        // projectiles + primed tnt are world entities in vanilla —
        // they survive restart mid-flight
        {
            let ar = arrows().lock().unwrap();
            // ARW2: stuck flag persists so saved arrows resume embedded
            (*f).extend_from_slice(b"ARW2");
            // Tridents travel in the drop list above, so they are excluded here
            // rather than removed from the live arrow list.
            let persisted: Vec<&Arrow> = ar
                .iter()
                .filter(|a| !(a.kind == 1 && a.ret_item > 0))
                .collect();
            write_varint(&mut *f, persisted.len() as i64);
            for a in persisted {
                (*f).extend_from_slice(&a.eid.to_be_bytes());
                (*f).extend_from_slice(&a.owner.to_be_bytes());
                (*f).extend_from_slice(&a.x.to_be_bytes());
                (*f).extend_from_slice(&a.y.to_be_bytes());
                (*f).extend_from_slice(&a.z.to_be_bytes());
                (*f).extend_from_slice(&a.dx.to_be_bytes());
                (*f).extend_from_slice(&a.dy.to_be_bytes());
                (*f).extend_from_slice(&a.dz.to_be_bytes());
                (*f).extend_from_slice(&a.ttl.to_be_bytes());
                (*f).push(a.stuck as u8);
            }
            let bo = bobbers().lock().unwrap();
            write_varint(&mut *f, bo.len() as i64);
            for a in bo.iter() {
                (*f).extend_from_slice(&a.eid.to_be_bytes());
                (*f).extend_from_slice(&a.owner.to_be_bytes());
                (*f).extend_from_slice(&a.x.to_be_bytes());
                (*f).extend_from_slice(&a.y.to_be_bytes());
                (*f).extend_from_slice(&a.z.to_be_bytes());
                (*f).extend_from_slice(&a.dx.to_be_bytes());
                (*f).extend_from_slice(&a.dy.to_be_bytes());
                (*f).extend_from_slice(&a.dz.to_be_bytes());
                (*f).extend_from_slice(&a.age.to_be_bytes());
                (*f).push(a.bit as u8);
            }
            let pe = pearls().lock().unwrap();
            write_varint(&mut *f, pe.len() as i64);
            for a in pe.iter() {
                (*f).extend_from_slice(&a.eid.to_be_bytes());
                (*f).extend_from_slice(&a.owner.to_be_bytes());
                (*f).extend_from_slice(&a.x.to_be_bytes());
                (*f).extend_from_slice(&a.y.to_be_bytes());
                (*f).extend_from_slice(&a.z.to_be_bytes());
                (*f).extend_from_slice(&a.dx.to_be_bytes());
                (*f).extend_from_slice(&a.dy.to_be_bytes());
                (*f).extend_from_slice(&a.dz.to_be_bytes());
                (*f).extend_from_slice(&a.ttl.to_be_bytes());
            }
            let pr = primed().lock().unwrap();
            write_varint(&mut *f, pr.len() as i64);
            for a in pr.iter() {
                (*f).extend_from_slice(&a.eid.to_be_bytes());
                (*f).extend_from_slice(&a.x.to_be_bytes());
                (*f).extend_from_slice(&a.y.to_be_bytes());
                (*f).extend_from_slice(&a.z.to_be_bytes());
                (*f).extend_from_slice(&a.left.to_be_bytes());
            }
        }
        // weather is world state in vanilla — rain budget survives restart
        (*f).extend_from_slice(&RAINING.load(Ordering::Relaxed).to_be_bytes());
        (*f).extend_from_slice(&THUNDERING.load(Ordering::Relaxed).to_be_bytes());
        // gamerules are world state too (level.dat GameRules)
        for r in [
            &GR_KEEP_INVENTORY,
            &GR_DAYLIGHT_CYCLE,
            &GR_WEATHER_CYCLE,
            &GR_MOB_SPAWNING,
            &GR_MOB_GRIEFING,
            &GR_NATURAL_REGEN,
            &GR_MOB_LOOT,
        ] {
            (*f).push(r.load(Ordering::Relaxed));
        }
        (*f).extend_from_slice(&world_spawn.0.to_be_bytes());
        (*f).extend_from_slice(&world_spawn.1.to_be_bytes());
        (*f).extend_from_slice(&world_spawn.2.to_be_bytes());
        (*f).push(DIFFICULTY.load(Ordering::Relaxed));
        // entity tags: players by name, mobs by ordinal (eids re-issue)
        {
            let tm = tags().lock().unwrap();
            let players: Vec<(&String, &HashSet<String>)> =
                tm.iter().filter(|(k, _)| k.starts_with("p:")).collect();
            write_varint(&mut *f, players.len() as i64);
            for (k, st) in players {
                let nm = &k[2..];
                (*f).extend_from_slice(&(nm.len() as u16).to_be_bytes());
                (*f).extend_from_slice(nm.as_bytes());
                write_varint(&mut *f, st.len() as i64);
                for t in st.iter() {
                    (*f).extend_from_slice(&(t.len() as u16).to_be_bytes());
                    (*f).extend_from_slice(t.as_bytes());
                }
            }
            let ms = mobs().lock().unwrap();
            let mobe: Vec<(i64, &HashSet<String>)> = ms
                .iter()
                .enumerate()
                .filter_map(|(i, m)| tm.get(&format!("e:{}", m.eid)).map(|st| (i as i64, st)))
                .collect();
            write_varint(&mut *f, mobe.len() as i64);
            for (i, st) in mobe {
                write_varint(&mut *f, i);
                write_varint(&mut *f, st.len() as i64);
                for t in st.iter() {
                    (*f).extend_from_slice(&(t.len() as u16).to_be_bytes());
                    (*f).extend_from_slice(t.as_bytes());
                }
            }
        }
        // scheduled functions: stored as REMAINING ticks (TICK is not
        // persisted — re-anchor on load)
        {
            let t_now = TICK.load(Ordering::Relaxed);
            let sf = sched_fns().lock().unwrap();
            let n_sched = sf.len();
            write_varint(&mut *f, n_sched as i64);
            for (f2, due) in sf.iter() {
                (*f).extend_from_slice(&(f2.len() as u16).to_be_bytes());
                (*f).extend_from_slice(f2.as_bytes());
                (*f).extend_from_slice(&(due.saturating_sub(t_now)).to_be_bytes());
            }
            let ag = adv_done().lock().unwrap();
            write_varint(&mut *f, ag.len() as i64);
            for (pn, ids) in ag.iter() {
                (*f).extend_from_slice(&(pn.len() as u16).to_be_bytes());
                (*f).extend_from_slice(pn.as_bytes());
                write_varint(&mut *f, ids.len() as i64);
                for id in ids {
                    (*f).extend_from_slice(&(id.len() as u16).to_be_bytes());
                    (*f).extend_from_slice(id.as_bytes());
                }
            }
            // player attributes (p: keys) — e: keys are transient by design
            let at = attrs().lock().unwrap();
            let pa: Vec<(&String, &HashMap<String, f64>)> =
                at.iter().filter(|(k, _)| k.starts_with("p:")).collect();
            write_varint(&mut *f, pa.len() as i64);
            for (k, m) in pa {
                let nm = &k[2..];
                (*f).extend_from_slice(&(nm.len() as u16).to_be_bytes());
                (*f).extend_from_slice(nm.as_bytes());
                write_varint(&mut *f, m.len() as i64);
                for (an, v) in m.iter() {
                    (*f).extend_from_slice(&(an.len() as u16).to_be_bytes());
                    (*f).extend_from_slice(an.as_bytes());
                    (*f).extend_from_slice(&v.to_be_bytes());
                }
            }
            // Legacy objective criteria and sidebar selection.
            write_varint(&mut *f, scoreboard_objectives.len() as i64);
            for (obj, criterion) in scoreboard_objectives {
                let name = scoreboard_criterion_name(criterion);
                (*f).extend_from_slice(&(obj.len() as u16).to_be_bytes());
                (*f).extend_from_slice(obj.as_bytes());
                (*f).extend_from_slice(&(name.len() as u16).to_be_bytes());
                (*f).extend_from_slice(name.as_bytes());
            }
            (*f).extend_from_slice(&(scoreboard_sidebar.len() as u16).to_be_bytes());
            (*f).extend_from_slice(scoreboard_sidebar.as_bytes());
        }
    }

    // FAL2: falling-block entities (id, position, velocity, block state, age). FAL1 stored a
    // cell the old cell-moving model held; a save carrying it is converted on load.
    let mut in_flight = falling().lock().unwrap().clone();
    in_flight.sort_by(|a, b| {
        (a.x.to_bits(), a.y.to_bits(), a.z.to_bits(), a.eid).cmp(&(
            b.x.to_bits(),
            b.y.to_bits(),
            b.z.to_bits(),
            b.eid,
        ))
    });
    (*f).extend_from_slice(b"FAL2");
    write_varint(&mut *f, in_flight.len() as i64);
    for fa in in_flight {
        (*f).extend_from_slice(&fa.eid.to_be_bytes());
        (*f).extend_from_slice(&fa.x.to_be_bytes());
        (*f).extend_from_slice(&fa.y.to_be_bytes());
        (*f).extend_from_slice(&fa.z.to_be_bytes());
        (*f).extend_from_slice(&fa.dy.to_be_bytes());
        (*f).extend_from_slice(&fa.state.to_be_bytes());
        (*f).extend_from_slice(&fa.time.to_be_bytes());
    }

    // RNG1: the world random stream's state. Without it a reload restarts the stream
    // from the seed, so two runs of the same seed diverge after a restart.
    (*f).extend_from_slice(b"RNG1");
    (*f).extend_from_slice(&world_rng::saved_state().to_be_bytes());
    let max_cramming = GR_MAX_ENTITY_CRAMMING.load(Ordering::Relaxed);
    if max_cramming != ENTITY_CRAMMING_DEFAULT {
        (*f).extend_from_slice(b"GCR1");
        (*f).extend_from_slice(&max_cramming.to_be_bytes());
    }
    // BRD1 stores the current interpolated extent and remaining transition time, so the
    // monotonic clock's origin is never persisted across process restarts.
    {
        let now_ms = border_now_ms();
        let border = *world_border().lock().unwrap();
        (*f).extend_from_slice(b"BRD1");
        (*f).extend_from_slice(&border.center_x.to_be_bytes());
        (*f).extend_from_slice(&border.center_z.to_be_bytes());
        (*f).extend_from_slice(&border.current_size(now_ms).to_be_bytes());
        (*f).extend_from_slice(&border.new_size.to_be_bytes());
        (*f).extend_from_slice(&border.lerp_remaining_ms(now_ms).max(0).to_be_bytes());
        (*f).extend_from_slice(&border.damage_per_block.to_be_bytes());
        (*f).extend_from_slice(&border.damage_safe_zone.to_be_bytes());
        (*f).extend_from_slice(&border.warning_blocks.to_be_bytes());
        (*f).extend_from_slice(&border.warning_time_secs.to_be_bytes());
        (*f).extend_from_slice(&border.absolute_max_size.to_be_bytes());
    }
    // Legacy world records have no orientation and default to zero.
    if world_spawn.3 != 0.0 {
        f.extend_from_slice(b"SPA1");
        f.extend_from_slice(&world_spawn.3.to_be_bytes());
    }
    generated.append(f);
    let (sources, generated_blocks, generated_fluids) = worldgen::with_gen(|g| {
        (
            g.tick_sources(),
            g.generated_leaf_ticks().to_vec(),
            g.generated_fluid_ticks(),
        )
    })
    .unwrap_or_default();
    let blocks = block_ticks().lock().unwrap().clone();
    let (fluid_order, fluids, owned) = GENERATED_FLUID_OWNER
        .get()
        .map(|owner| {
            let owner = owner.lock().unwrap();
            (owner.next_order, owner.queue.clone(), owner.owned.clone())
        })
        .unwrap_or_else(|| (0, scheduled_ticks::ScheduledTicks::new(), HashSet::new()));
    tick_persistence::TickState {
        now: TICK_IDX.load(Ordering::Relaxed),
        block_order: BLOCK_TICK_ORDER.load(Ordering::Relaxed),
        blocks,
        fluid_order,
        fluids,
        owned,
        sources,
        generated_blocks,
        generated_fluids,
    }
    .append(f);
    if !drop_owners.is_empty() {
        append_drop_owners_footer(f, &drop_owners);
    }
    let mut block_loot: Vec<(BlockPos, PendingBlockLoot)> = Vec::new();
    for (&(dim, x, y, z), entry) in pending_block_loot().lock().unwrap().iter() {
        if dim == DimId::Overworld {
            block_loot.push(((x, y, z), entry.clone()));
        } else {
            dim2.loot.push(((dim, x, y, z), entry.clone()));
        }
    }
    block_loot.sort_by_key(|(pos, _)| *pos);
    if !block_loot.is_empty() {
        append_block_loot_footer(f, &block_loot);
    }
    // Written last so it is read first: an Overworld-only save emits nothing here, leaving every
    // legacy byte (and every other footer's position) exactly where it was.
    if !dim_signs.is_empty() || !dim_players.is_empty() {
        append_dim_footer(f, &dim_signs, &dim_players);
    }
    // DIM2 is written after DIM1 and is therefore stripped before it on load. Emitted only when
    // something outside the Overworld actually exists.
    if !dim2.is_empty() {
        append_dim2_footer(f, &dim2);
    }
    append_brewing_footer(f);
    append_mob_dimensions_footer(f, &mob_dimensions);
    append_world_seed_footer(f, WORLD_SEED.load(Ordering::Relaxed));
}

fn append_world_seed_footer(out: &mut Vec<u8>, seed: u64) {
    out.extend_from_slice(&seed.to_be_bytes());
    out.extend_from_slice(WORLD_SEED_MAGIC);
}

fn read_world_seed_footer(payload: &[u8]) -> Result<(&[u8], Option<u64>), &'static str> {
    if !payload.ends_with(WORLD_SEED_MAGIC) {
        return Ok((payload, None));
    }
    let end = payload
        .len()
        .checked_sub(12)
        .ok_or("truncated world seed footer")?;
    let seed = u64::from_be_bytes(
        payload[end..end + 8]
            .try_into()
            .map_err(|_| "truncated world seed footer")?,
    );
    Ok((&payload[..end], Some(seed)))
}

fn load_world_seed_in(dir: &std::path::Path) -> Result<Option<u64>, &'static str> {
    let path = dim_overlay_path(dir, DimId::Overworld);
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("failed to read world save"),
    };
    let payload = unwrap_world_payload(raw)?;
    let (_, seed) = read_world_seed_footer(&payload)?;
    Ok(seed)
}

fn resolve_world_seed(requested: Option<u64>, saved: Option<u64>) -> Result<u64, &'static str> {
    match (requested, saved) {
        (Some(requested), Some(saved)) if requested != saved => {
            Err("MC_SPINE_SEED does not match the saved world seed")
        }
        (Some(requested), _) => Ok(requested),
        (None, Some(saved)) => Ok(saved),
        (None, None) => Ok(0),
    }
}

// One dimension byte per legacy mob record in the same ordinal order.
const MOB_DIM_MAGIC: &[u8; 4] = b"MDM1";

fn append_mob_dimensions_footer(out: &mut Vec<u8>, dimensions: &[DimId]) {
    if dimensions.iter().all(|dim| *dim == DimId::Overworld) {
        return;
    }
    out.extend(dimensions.iter().map(|dim| dim.index() as u8));
    out.extend_from_slice(&(dimensions.len() as u64).to_be_bytes());
    out.extend_from_slice(MOB_DIM_MAGIC);
}

fn read_mob_dimensions_footer(payload: &[u8]) -> Result<(&[u8], Vec<DimId>), &'static str> {
    if !payload.ends_with(MOB_DIM_MAGIC) {
        return Ok((payload, Vec::new()));
    }
    let end = payload
        .len()
        .checked_sub(12)
        .ok_or("truncated mob dimension footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid mob dimension footer size")?;
    if !(1..=MOB_CAP).contains(&size) {
        return Err("invalid mob dimension footer size");
    }
    let start = end
        .checked_sub(size)
        .ok_or("invalid mob dimension footer size")?;
    let dimensions = payload[start..end]
        .iter()
        .map(|byte| DimId::from_index(usize::from(*byte)).ok_or("invalid mob dimension"))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((&payload[..start], dimensions))
}

// Non-Overworld signs and player dimensions. The body is `[sign count][signs][player count][players]`,
// framed by `[body size u64][DIM1]` exactly like the drop-owners footer. A `DimId` byte is validated on
// read with `DimId::from_index`, so an unknown dimension fails closed instead of being silently
// mapped to the Overworld.
const DIM_FOOTER_LEN: usize = 12;
const DIM_MAGIC: &[u8; 4] = b"DIM1";
fn append_dim_footer(
    out: &mut Vec<u8>,
    signs: &[(DimId, i32, i32, i32, u8, [String; 4])],
    players: &[(String, DimId)],
) {
    let start = out.len();
    out.extend_from_slice(&(signs.len() as u32).to_be_bytes());
    for &(dim, x, y, z, front, ref lines) in signs {
        out.push(dim.index() as u8);
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(&z.to_be_bytes());
        out.push(front);
        for l in lines.iter() {
            out.extend_from_slice(&(l.len() as u16).to_be_bytes());
            out.extend_from_slice(l.as_bytes());
        }
    }
    out.extend_from_slice(&(players.len() as u32).to_be_bytes());
    for (name, dim) in players {
        out.push(dim.index() as u8);
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
    }
    let size = (out.len() - start) as u64;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(DIM_MAGIC);
}
fn read_dim_footer(
    payload: &[u8],
) -> Result<
    (
        &[u8],
        Vec<(DimId, i32, i32, i32, u8, [String; 4])>,
        Vec<(String, DimId)>,
    ),
    &'static str,
> {
    if !payload.ends_with(DIM_MAGIC) {
        return Ok((payload, Vec::new(), Vec::new()));
    }
    let end = payload
        .len()
        .checked_sub(DIM_FOOTER_LEN)
        .ok_or("truncated dimension footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid dimension footer size")?;
    // Two u32 counts are the smallest possible body.
    if size < 8 {
        return Err("invalid dimension footer size");
    }
    let start = end.checked_sub(size).ok_or("invalid dimension footer size")?;
    let mut input = &payload[start..end];
    let read_dim = |input: &mut &[u8]| -> Result<DimId, &'static str> {
        let b = *input.first().ok_or("truncated dimension tag")?;
        *input = &input[1..];
        DimId::from_index(b as usize).ok_or("invalid dimension tag")
    };
    let n = u32::from_be_bytes(
        input
            .get(..4)
            .ok_or("truncated dimension sign count")?
            .try_into()
            .unwrap(),
    ) as usize;
    input = &input[4..];
    // Each sign has at least 1 dim + 13 position/front + 4 * 2 empty line lengths = 22 bytes.
    if n > input.len() / 22 {
        return Err("invalid dimension sign count");
    }
    let mut signs = Vec::with_capacity(n);
    for _ in 0..n {
        let dim = read_dim(&mut input)?;
        let pos = input.get(..13).ok_or("truncated dimension sign position")?;
        let x = i32::from_be_bytes(pos[..4].try_into().unwrap());
        let y = i32::from_be_bytes(pos[4..8].try_into().unwrap());
        let z = i32::from_be_bytes(pos[8..12].try_into().unwrap());
        let front = pos[12];
        input = &input[13..];
        let mut lines: [String; 4] = Default::default();
        for l in lines.iter_mut() {
            let nl = u16::from_be_bytes(
                input
                    .get(..2)
                    .ok_or("truncated dimension sign line length")?
                    .try_into()
                    .unwrap(),
            ) as usize;
            input = &input[2..];
            if input.len() < nl {
                return Err("truncated dimension sign line");
            }
            *l = String::from_utf8_lossy(&input[..nl]).to_string();
            input = &input[nl..];
        }
        signs.push((dim, x, y, z, front, lines));
    }
    let pn = u32::from_be_bytes(
        input
            .get(..4)
            .ok_or("truncated dimension player count")?
            .try_into()
            .unwrap(),
    ) as usize;
    input = &input[4..];
    // Each player has at least 1 dim + 2 name length + 0 name bytes = 3 bytes.
    if pn > input.len() / 3 {
        return Err("invalid dimension player count");
    }
    let mut players = Vec::with_capacity(pn);
    for _ in 0..pn {
        let dim = read_dim(&mut input)?;
        let nl = u16::from_be_bytes(
            input
                .get(..2)
                .ok_or("truncated dimension player name length")?
                .try_into()
                .unwrap(),
        ) as usize;
        input = &input[2..];
        if input.len() < nl {
            return Err("truncated dimension player name");
        }
        let name = String::from_utf8_lossy(&input[..nl]).to_string();
        input = &input[nl..];
        players.push((name, dim));
    }
    if !input.is_empty() {
        return Err("invalid dimension footer tail");
    }
    Ok((&payload[..start], signs, players))
}

/// Non-Overworld block containers, furnaces, crops (and their pending loot) ride in this footer.
/// The legacy sections keep their exact wire layout and hold the Overworld's entries only, so an
/// Overworld-only save emits nothing here and stays byte-for-byte identical to the old file.
const DIM2_FOOTER_LEN: usize = 12;
const DIM2_MAGIC: &[u8; 4] = b"DIM2";
#[derive(Default)]
struct Dim2Entries {
    chests: Vec<(DimBlockPos, HashMap<i16, Stack>)>,
    furnaces: Vec<(DimBlockPos, Furnace)>,
    crops: Vec<(DimBlockPos, u8)>,
    loot: Vec<(DimBlockPos, PendingBlockLoot)>,
}
impl Dim2Entries {
    fn is_empty(&self) -> bool {
        self.chests.is_empty()
            && self.furnaces.is_empty()
            && self.crops.is_empty()
            && self.loot.is_empty()
    }
}
fn append_dim2_footer(out: &mut Vec<u8>, entries: &Dim2Entries) {
    use crate::generation_payload::ContainerKind;
    let start = out.len();
    out.extend_from_slice(&(entries.chests.len() as u32).to_be_bytes());
    for ((dim, x, y, z), slots) in &entries.chests {
        out.push(dim.index() as u8);
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(&z.to_be_bytes());
        write_stack_table(out, slots);
    }
    out.extend_from_slice(&(entries.furnaces.len() as u32).to_be_bytes());
    for ((dim, x, y, z), fr) in &entries.furnaces {
        out.push(dim.index() as u8);
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(&z.to_be_bytes());
        write_furnace_state(out, fr);
    }
    out.extend_from_slice(&(entries.crops.len() as u32).to_be_bytes());
    for ((dim, x, y, z), age) in &entries.crops {
        out.push(dim.index() as u8);
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(&z.to_be_bytes());
        out.push(*age);
    }
    out.extend_from_slice(&(entries.loot.len() as u32).to_be_bytes());
    for ((dim, x, y, z), entry) in &entries.loot {
        out.push(dim.index() as u8);
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(&z.to_be_bytes());
        match entry.kind {
            ContainerKind::Chest => out.push(0),
            ContainerKind::Furnace => out.push(1),
            ContainerKind::BlockContainer(size) => {
                out.push(2);
                out.push(size);
            }
        }
        let table = entry.loot_table.as_bytes();
        let tlen = table.len().min(u16::MAX as usize);
        out.extend_from_slice(&(tlen as u16).to_be_bytes());
        out.extend_from_slice(&table[..tlen]);
        if let Some(seed) = entry.loot_seed {
            out.push(1);
            out.extend_from_slice(&seed.to_be_bytes());
        } else {
            out.push(0);
        }
    }
    let size = (out.len() - start) as u64;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(DIM2_MAGIC);
}
/// `DimId::from_index` is the fail-closed gate: an unknown tag, or an Overworld tag in a footer
/// that is only supposed to carry the other dimensions, is an error rather than a silent
/// relocation into the Overworld.
fn read_dim2_dim(body: &[u8], p: &mut usize) -> Result<DimId, &'static str> {
    let b = *body.get(*p).ok_or("truncated DIM2 dimension tag")?;
    *p += 1;
    let dim = DimId::from_index(b as usize).ok_or("invalid DIM2 dimension tag")?;
    if dim == DimId::Overworld {
        return Err("Overworld entry in DIM2 footer");
    }
    Ok(dim)
}
fn read_dim2_pos(body: &[u8], p: &mut usize) -> Result<DimBlockPos, &'static str> {
    let dim = read_dim2_dim(body, p)?;
    let raw = body
        .get(*p..*p + 12)
        .ok_or("truncated DIM2 position")?;
    *p += 12;
    Ok((
        dim,
        i32::from_be_bytes(raw[0..4].try_into().unwrap()),
        i32::from_be_bytes(raw[4..8].try_into().unwrap()),
        i32::from_be_bytes(raw[8..12].try_into().unwrap()),
    ))
}
fn read_dim2_count(body: &[u8], p: &mut usize, min_entry: usize) -> Result<usize, &'static str> {
    let n = u32::from_be_bytes(
        body.get(*p..*p + 4)
            .ok_or("truncated DIM2 count")?
            .try_into()
            .unwrap(),
    ) as usize;
    *p += 4;
    if n > body.len().saturating_sub(*p) / min_entry {
        return Err("invalid DIM2 count");
    }
    Ok(n)
}
fn read_dim2_footer(payload: &[u8]) -> Result<(&[u8], Dim2Entries), &'static str> {
    if !payload.ends_with(DIM2_MAGIC) {
        return Ok((payload, Dim2Entries::default()));
    }
    // Four u32 counts make the smallest possible body.
    let end = payload
        .len()
        .checked_sub(DIM2_FOOTER_LEN)
        .ok_or("truncated DIM2 footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid DIM2 footer size")?;
    if size < 16 {
        return Err("invalid DIM2 footer size");
    }
    let start = end.checked_sub(size).ok_or("invalid DIM2 footer size")?;
    let body = &payload[start..end];
    let mut p = 0usize;
    let mut out = Dim2Entries::default();
    let n = read_dim2_count(body, &mut p, 14)?;
    for _ in 0..n {
        let pos = read_dim2_pos(body, &mut p)?;
        let slots = read_stack_table(body, &mut p, true);
        out.chests.push((pos, slots));
    }
    let n = read_dim2_count(body, &mut p, 14)?;
    for _ in 0..n {
        let pos = read_dim2_pos(body, &mut p)?;
        let fr = read_furnace_state(body, &mut p, true, false)
            .ok_or("truncated DIM2 furnace")?;
        out.furnaces.push((pos, fr));
    }
    let n = read_dim2_count(body, &mut p, 14)?;
    for _ in 0..n {
        let pos = read_dim2_pos(body, &mut p)?;
        let age = *body.get(p).ok_or("truncated DIM2 crop age")?;
        p += 1;
        out.crops.push((pos, age));
    }
    {
        use crate::generation_payload::ContainerKind;
        let n = read_dim2_count(body, &mut p, 17)?;
        for _ in 0..n {
            let pos = read_dim2_pos(body, &mut p)?;
            let tag = *body.get(p).ok_or("truncated DIM2 loot kind")?;
            p += 1;
            let kind = match tag {
                0 => ContainerKind::Chest,
                1 => ContainerKind::Furnace,
                2 => {
                    let sz = *body.get(p).ok_or("truncated DIM2 loot size")?;
                    p += 1;
                    ContainerKind::BlockContainer(sz)
                }
                _ => return Err("invalid DIM2 loot kind"),
            };
            let tlen = u16::from_be_bytes(
                body.get(p..p + 2)
                    .ok_or("truncated DIM2 loot table length")?
                    .try_into()
                    .unwrap(),
            ) as usize;
            p += 2;
            let table = body.get(p..p + tlen).ok_or("truncated DIM2 loot table")?;
            p += tlen;
            let loot_table = String::from_utf8_lossy(table).to_string();
            let has_seed = *body.get(p).ok_or("truncated DIM2 loot seed flag")?;
            p += 1;
            let loot_seed = match has_seed {
                0 => None,
                1 => {
                    let s = i64::from_be_bytes(
                        body.get(p..p + 8)
                            .ok_or("truncated DIM2 loot seed")?
                            .try_into()
                            .unwrap(),
                    );
                    p += 8;
                    Some(s)
                }
                _ => return Err("invalid DIM2 loot seed flag"),
            };
            out.loot.push((
                pos,
                PendingBlockLoot {
                    kind,
                    loot_table,
                    loot_seed,
                },
            ));
        }
    }
    if p != body.len() {
        return Err("invalid DIM2 footer tail");
    }
    Ok((&payload[..start], out))
}
const BLOCK_LOOT_FOOTER_LEN: usize = 12;
const BLOCK_LOOT_MAGIC: &[u8; 4] = b"BLT1";
fn append_block_loot_footer(out: &mut Vec<u8>, entries: &[(BlockPos, PendingBlockLoot)]) {
    use crate::generation_payload::ContainerKind;
    let start = out.len();
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for &(pos, ref entry) in entries {
        out.extend_from_slice(&pos.0.to_be_bytes());
        out.extend_from_slice(&pos.1.to_be_bytes());
        out.extend_from_slice(&pos.2.to_be_bytes());
        match entry.kind {
            ContainerKind::Chest => out.push(0),
            ContainerKind::Furnace => out.push(1),
            ContainerKind::BlockContainer(size) => {
                out.push(2);
                out.push(size);
            }
        }
        let table = entry.loot_table.as_bytes();
        let tlen = table.len().min(u16::MAX as usize);
        out.extend_from_slice(&(tlen as u16).to_be_bytes());
        out.extend_from_slice(&table[..tlen]);
        if let Some(seed) = entry.loot_seed {
            out.push(1);
            out.extend_from_slice(&seed.to_be_bytes());
        } else {
            out.push(0);
        }
    }
    let size = (out.len() - start) as u64;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(BLOCK_LOOT_MAGIC);
}
fn read_block_loot_footer(
    payload: &[u8],
) -> Result<(&[u8], HashMap<BlockPos, PendingBlockLoot>), &'static str> {
    use crate::generation_payload::ContainerKind;
    if !payload.ends_with(BLOCK_LOOT_MAGIC) {
        return Ok((payload, HashMap::new()));
    }
    let end = payload
        .len()
        .checked_sub(BLOCK_LOOT_FOOTER_LEN)
        .ok_or("truncated block loot footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid block loot footer size")?;
    if size == 0 {
        return Err("invalid block loot footer size");
    }
    let start = end
        .checked_sub(size)
        .ok_or("invalid block loot footer size")?;
    let mut input = &payload[start..end];
    if input.len() < 4 {
        return Err("truncated block loot count");
    }
    let count = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
    input = &input[4..];
    if count > input.len() / 16 {
        return Err("invalid block loot count");
    }
    let mut out = HashMap::with_capacity(count);
    for _ in 0..count {
        if input.len() < 13 {
            return Err("truncated block loot entry");
        }
        let x = i32::from_be_bytes(input[..4].try_into().unwrap());
        let y = i32::from_be_bytes(input[4..8].try_into().unwrap());
        let z = i32::from_be_bytes(input[8..12].try_into().unwrap());
        let tag = input[12];
        input = &input[13..];
        let kind = match tag {
            0 => ContainerKind::Chest,
            1 => ContainerKind::Furnace,
            2 => {
                if input.is_empty() {
                    return Err("truncated block loot size");
                }
                let sz = input[0];
                input = &input[1..];
                ContainerKind::BlockContainer(sz)
            }
            _ => return Err("invalid block loot kind"),
        };
        if input.len() < 2 {
            return Err("truncated block loot table length");
        }
        let tlen = u16::from_be_bytes(input[..2].try_into().unwrap()) as usize;
        input = &input[2..];
        if input.len() < tlen + 1 {
            return Err("truncated block loot table");
        }
        let loot_table = String::from_utf8_lossy(&input[..tlen]).to_string();
        input = &input[tlen..];
        let has_seed = input[0];
        input = &input[1..];
        let loot_seed = match has_seed {
            0 => None,
            1 => {
                if input.len() < 8 {
                    return Err("truncated block loot seed");
                }
                let s = i64::from_be_bytes(input[..8].try_into().unwrap());
                input = &input[8..];
                Some(s)
            }
            _ => return Err("invalid block loot seed flag"),
        };
        out.insert(
            (x, y, z),
            PendingBlockLoot {
                kind,
                loot_table,
                loot_seed,
            },
        );
    }
    if !input.is_empty() {
        return Err("invalid block loot tail");
    }
    Ok((&payload[..start], out))
}
const DROP_OWNERS_FOOTER_LEN: usize = 12;
const DROP_OWNERS_MAGIC: &[u8; 4] = b"DRO1";
fn append_drop_owners_footer(
    out: &mut Vec<u8>,
    owners: &[(u32, Option<[u8; 16]>, Option<[u8; 16]>)],
) {
    let start = out.len();
    out.extend_from_slice(&(owners.len() as u32).to_be_bytes());
    for &(idx, target, thrower) in owners {
        out.extend_from_slice(&idx.to_be_bytes());
        let flags = u8::from(target.is_some()) | (u8::from(thrower.is_some()) << 1);
        out.push(flags);
        if let Some(t) = target {
            out.extend_from_slice(&t);
        }
        if let Some(th) = thrower {
            out.extend_from_slice(&th);
        }
    }
    let size = (out.len() - start) as u64;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(DROP_OWNERS_MAGIC);
}
fn read_drop_owners_footer(
    payload: &[u8],
) -> Result<(&[u8], Vec<(usize, Option<[u8; 16]>, Option<[u8; 16]>)>), &'static str> {
    if !payload.ends_with(DROP_OWNERS_MAGIC) {
        return Ok((payload, Vec::new()));
    }
    let end = payload
        .len()
        .checked_sub(DROP_OWNERS_FOOTER_LEN)
        .ok_or("truncated drop owners footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid drop owners size")?;
    if size == 0 {
        return Err("invalid drop owners size");
    }
    let start = end.checked_sub(size).ok_or("invalid drop owners size")?;
    let mut input = &payload[start..end];
    if input.len() < 4 {
        return Err("truncated drop owners count");
    }
    let count = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
    input = &input[4..];
    if count > input.len() / 5 {
        return Err("invalid drop owners count");
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if input.len() < 5 {
            return Err("truncated drop owner entry");
        }
        let idx = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
        let flags = input[4];
        input = &input[5..];
        if flags == 0 || (flags & !0b11) != 0 {
            return Err("invalid drop owner flags");
        }
        let target = if flags & 1 != 0 {
            if input.len() < 16 {
                return Err("truncated drop target uuid");
            }
            let u: [u8; 16] = input[..16].try_into().unwrap();
            input = &input[16..];
            Some(u)
        } else {
            None
        };
        let thrower = if flags & 2 != 0 {
            if input.len() < 16 {
                return Err("truncated drop thrower uuid");
            }
            let u: [u8; 16] = input[..16].try_into().unwrap();
            input = &input[16..];
            Some(u)
        } else {
            None
        };
        out.push((idx, target, thrower));
    }
    if !input.is_empty() {
        return Err("invalid drop owners tail");
    }
    Ok((&payload[..start], out))
}
fn load_world() -> Result<(), &'static str> {
    load_world_in(&world_save_dir())
}

/// [`load_world`] against one directory: the legacy file, then the companion of every dimension
/// that has one.
fn load_world_in(dir: &std::path::Path) -> Result<(), &'static str> {
    let path = dim_overlay_path(dir, DimId::Overworld);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("failed to read world save"),
    };
    load_world_from(&unwrap_world_payload(raw)?)?;
    for dim in [DimId::Nether, DimId::End] {
        load_dim_overlay_file(dir, dim)?;
    }
    Ok(())
}

/// Load `dim`'s companion overlay when the file exists; a dimension with no file has no edits.
fn load_dim_overlay_file(dir: &std::path::Path, dim: DimId) -> Result<(), &'static str> {
    if dim == DimId::Overworld {
        // The Overworld's edits are the legacy file's leading table, read by `load_world_from`.
        return Ok(());
    }
    let path = dim_overlay_path(dir, dim);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("failed to read world save"),
    };
    load_dim_overlay_from(dim, &unwrap_world_payload(raw)?)
}

fn load_world_from(payload: &[u8]) -> Result<(), &'static str> {
    // Footers are stripped last-written-first.
    let (payload, _) = read_world_seed_footer(payload)?;
    let (payload, mob_dimensions) = read_mob_dimensions_footer(payload)?;
    let (payload, brewing) = read_brewing_footer(payload)?;
    // A payload with no DIM2 footer is the legacy (or DIM1-only) form and parses exactly as before.
    let (payload, dim2) = read_dim2_footer(payload)?;
    let (payload, dim_signs, dim_players) = read_dim_footer(payload)?;
    let (payload, block_loot) = read_block_loot_footer(payload)?;
    let (payload, drop_owners) = read_drop_owners_footer(payload)?;
    let tick_state = tick_persistence::TickState::read(payload)?;
    let generated = generated_content::Snapshot::read(tick_persistence::prefix(payload)?)?;
    if generated.as_ref().is_some_and(|s| (!s.sources.is_empty() || !s.spider_flags.is_empty())
        && worldgen::with_gen(|_| ()).is_none()) {
        return Err("generated content requires initialized world generator");
    }
    if tick_state.as_ref().is_some_and(|s| {
        (!s.sources.is_empty() || !s.generated_blocks.is_empty() || !s.generated_fluids.is_empty())
            && worldgen::with_gen(|_| ()).is_none()
    }) {
        return Err("tick state requires initialized world generator");
    }
    let mut p = 0usize;
    let b = payload;
    // bounds-safe varint read: truncated input stops the parse instead of
    // panicking; continuation bytes beyond i64's 70 bits are ignored.
    let rv = rv_varint;
    // The leading table is the Overworld's: the legacy file stores `(cx, cz)` chunks, and they
    // load into `(DimId::Overworld, cx, cz)`. `p` continues past the table into the META tail.
    let has_mob_dimensions = !mob_dimensions.is_empty();
    // When the footer links to legacy mob ordinals, check the link before publishing any edits.
    read_dim_overlay_at(DimId::Overworld, payload, &mut p, !has_mob_dimensions);
    let has_meta = matches!(payload.get(p..p + 4), Some(b"MET2" | b"MET3" | b"MET4"));
    if has_mob_dimensions && !has_meta {
        return Err("mob dimension count does not match mob records");
    }
    // optional META tail
    let mut restored_mobs = Vec::new();
    let mut restored_sheared = Vec::new();
    let mut restored_mob_tags = Vec::new();
    if has_meta {
        let magic = &payload[p..p + 4];
        let has_comps = magic != b"MET2";
        // MET4 stores entity ages as server ticks. MET2 and MET3 stored them as
        // milliseconds, and one millisecond is not one tick, so a legacy file
        // read as ticks would despawn its drops immediately.
        let legacy_ms_ages = magic != b"MET4";
        p += 4;
        let mn = rv(payload, &mut p);
        for _ in 0..mn {
            let et = rv(payload, &mut p);
            if p + 28 > payload.len() {
                break;
            }
            let x = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
            p += 8;
            let y = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
            p += 8;
            let z = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
            p += 8;
            let hp = f32::from_be_bytes(b[p..p + 4].try_into().unwrap());
            p += 4;
            restored_mobs.push((et, x, y, z, hp));
        }
        if has_mob_dimensions {
            if restored_mobs.len() != mob_dimensions.len() {
                return Err("mob dimension count does not match mob records");
            }
            read_dim_overlay_at(DimId::Overworld, payload, &mut 0, true);
        }
        if p + 8 <= payload.len() {
            WORLD_TIME.store(
                u64::from_be_bytes(b[p..p + 8].try_into().unwrap()),
                Ordering::Relaxed,
            );
            p += 8;
        }
        if p < payload.len() {
            let pn = rv(payload, &mut p);
            let mut store = inv_store().lock().unwrap();
            for _ in 0..pn {
                if p + 2 > payload.len() {
                    break;
                }
                let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                p += 2;
                if p + nl > payload.len() {
                    break;
                }
                let name = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                p += nl;
                let en = rv(payload, &mut p);
                let mut inv = HashMap::new();
                for _ in 0..en {
                    if p + 3 > payload.len() {
                        break;
                    }
                    let slot = i16::from_be_bytes(b[p..p + 2].try_into().unwrap());
                    p += 2;
                    let item = rv(payload, &mut p);
                    if p >= payload.len() {
                        break;
                    }
                    let cnt = b[p];
                    p += 1;
                    let comps = if has_comps {
                        read_comps(payload, &mut p)
                    } else {
                        Vec::new()
                    };
                    inv.insert(slot, (item, cnt, comps));
                }
                store.insert(name, inv);
            }
        }
        if p < payload.len() {
            let cn = rv(payload, &mut p);
            let mut ch = chests().lock().unwrap();
            for _ in 0..cn {
                if p + 12 > payload.len() {
                    break;
                }
                let x = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let y = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let z = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let slots = read_stack_table(payload, &mut p, has_comps);
                // The legacy chest section has no dimension byte: every entry in it is Overworld.
                ch.insert(ContainerKey::Block((DimId::Overworld, x, y, z)), slots);
            }
        }
        if p < payload.len() {
            let sn = rv(payload, &mut p);
            let mut sg = signs().lock().unwrap();
            for _ in 0..sn {
                if p + 13 > payload.len() {
                    break;
                }
                let x = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let y = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let z = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let front = b[p];
                p += 1;
                let mut lines: [String; 4] = Default::default();
                for l in lines.iter_mut() {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + nl > payload.len() {
                        break;
                    }
                    *l = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                    p += nl;
                }
                // The legacy sign section has no dimension byte: every sign in it is Overworld.
                sg.insert((DimId::Overworld, x, y, z, front), lines);
            }
        }
        if p < payload.len() {
            let fn_ = rv(payload, &mut p);
            let mut fl = forceloaded().lock().unwrap();
            for _ in 0..fn_ {
                if p + 8 > payload.len() {
                    break;
                }
                let cx = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let cz = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                fl.insert((cx, cz));
            }
        }
        if p < payload.len() {
            let pn = rv(payload, &mut p);
            let mut ps = pos_store().lock().unwrap();
            for _ in 0..pn {
                if p + 2 > payload.len() {
                    break;
                }
                let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                p += 2;
                if p + nl + 28 > payload.len() {
                    break;
                }
                let name = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                p += nl;
                let x = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let y = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let z = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let yaw = f32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let pitch = f32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                // The legacy position section has no dimension byte: every player defaulted to
                // the Overworld; a footer entry below corrects the few who logged out elsewhere.
                ps.insert(name, (DimId::Overworld, x, y, z, yaw, pitch));
            }
        }
        if p < payload.len() {
            let dn = rv(payload, &mut p);
            let mut ds = drops().lock().unwrap();
            for _ in 0..dn {
                if p + 30 > payload.len() {
                    break;
                }
                let eid = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let x = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let y = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let z = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let item = rv(payload, &mut p);
                if p >= payload.len() {
                    break;
                }
                let cnt = b[p];
                p += 1;
                let comps = if has_comps {
                    read_comps(payload, &mut p)
                } else {
                    Vec::new()
                };
                if eid >= NEXT_EID.load(Ordering::Relaxed) {
                    NEXT_EID.store(eid + 1, Ordering::SeqCst);
                }
                ds.push(Drop {
                    eid,
                    x,
                    y,
                    z,
                    item,
                    cnt,
                    comps,
                    age: 0,
                    target: None,
                    thrower: None,
                });
            }
            for &(idx, target, thrower) in &drop_owners {
                if let Some(d) = ds.get_mut(idx) {
                    d.target = target;
                    d.thrower = thrower;
                }
            }
        }
        if p < payload.len() {
            let fn_ = rv(payload, &mut p);
            let mut fs = furnaces().lock().unwrap();
            for _ in 0..fn_ {
                if p + 12 > payload.len() {
                    break;
                }
                let x = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let y = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let z = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let Some(fr) = read_furnace_state(payload, &mut p, has_comps, legacy_ms_ages)
                else {
                    break;
                };
                // The legacy furnace section has no dimension byte: every entry is Overworld.
                fs.insert((DimId::Overworld, x, y, z), fr);
            }
            eprintln!("[spine] loaded {} furnaces", fs.len());
        }
        if p < payload.len() {
            // sheared mob indices -> mark by load order (fresh eids)
            let sn = rv(payload, &mut p);
            for _ in 0..sn {
                restored_sheared.push(rv(payload, &mut p) as usize);
            }
        }
        if p < payload.len() {
            let on = rv(payload, &mut p);
            let mut board = scoreboard().lock().unwrap();
            for _ in 0..on {
                if p + 2 > payload.len() {
                    break;
                }
                let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                p += 2;
                if p + nl > payload.len() {
                    break;
                }
                let obj = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                p += nl;
                let en = rv(payload, &mut p);
                let mut entries = HashMap::new();
                for _ in 0..en {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let el = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + el + 8 > payload.len() {
                        break;
                    }
                    let entry = String::from_utf8_lossy(&payload[p..p + el]).to_string();
                    p += el;
                    let val = i64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                    p += 8;
                    entries.insert(entry, ScoreboardScore {
                        value: val.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                        trigger_enabled: false,
                    });
                }
                board.scores.insert(obj.clone(), entries);
                board.objectives.insert(obj.clone(), ScoreboardObjective {
                    display_name: obj.clone(),
                    name: obj,
                    criteria: ScoreboardCriteria::Dummy,
                    render_type: ScoreboardRenderType::Integer,
                });
            }
        }
        if p + 1 < payload.len() {
            // XP orbs (eid,x,y,z,val,age_ms)
            let xn = rv(payload, &mut p);
            let mut xs = xps().lock().unwrap();
            let mut mx: i32 = 0;
            for _ in 0..xn {
                if p + 34 > payload.len() {
                    break;
                }
                let eid = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let x = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let y = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let z = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let val = i16::from_be_bytes(b[p..p + 2].try_into().unwrap());
                p += 2;
                let ams = u32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let age = legacy_ticks(u64::from(ams), legacy_ms_ages);
                xs.push(Xp {
                    eid,
                    x,
                    y,
                    z,
                    val,
                    age: age.min(server_ticks(XP_LIFE_GT)) as i32,
                });
                if eid > mx {
                    mx = eid;
                }
            }
            if mx > 0 {
                NEXT_EID.fetch_max(mx + 1, Ordering::SeqCst);
            }
            if !xs.is_empty() {
                eprintln!("[spine] loaded {} xp orbs", xs.len());
            }
        }
        if p < payload.len() {
            // drop ages by order — restore remaining despawn budget
            let mut ds = drops().lock().unwrap();
            for d in ds.iter_mut() {
                if p + 4 > payload.len() {
                    break;
                }
                let raw = u32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let aticks = legacy_ticks(u64::from(raw), legacy_ms_ages);
                // Reinterpret as signed so a negative pickup-delay marker survives
                // instead of wrapping to a value that clamps to a full lifespan.
                d.age = aticks as i32;
            }
        }
        if p < payload.len() {
            let rf3 = |b: &[u8], p: &mut usize| -> f64 {
                let v = f64::from_be_bytes(b[*p..*p + 8].try_into().unwrap());
                *p += 8;
                v
            };
            let ri4 = |b: &[u8], p: &mut usize| -> i32 {
                let v = i32::from_be_bytes(b[*p..*p + 4].try_into().unwrap());
                *p += 4;
                v
            };
            let mut mx = 0i32;
            let bump = |eid: i32, mx: &mut i32| {
                if eid > *mx {
                    *mx = eid;
                }
            };
            let arw2 = b[p..].starts_with(b"ARW2");
            if arw2 {
                p += 4;
            }
            let n = rv(payload, &mut p);
            {
                let mut ar = arrows().lock().unwrap();
                let rec = if arw2 { 61 } else { 60 };
                for _ in 0..n {
                    if p + rec > payload.len() {
                        break;
                    }
                    let eid = ri4(payload, &mut p);
                    let owner = ri4(payload, &mut p);
                    let (x, y, z) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let (dx, dy, dz) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let ttl = legacy_tick_count(ri4(payload, &mut p) as u64, legacy_ms_ages) as i32;
                    let stuck = if arw2 {
                        let v = b[p] != 0;
                        p += 1;
                        v
                    } else {
                        false
                    };
                    ar.push(Arrow {
                        eid,
                        owner,
                        x,
                        y,
                        z,
                        dx,
                        dy,
                        dz,
                        ttl,
                        dmg: 6.0,
                        fire: 0,
                        punch: 0,
                        kind: 0,
                        loy: 0,
                        ret_item: 0,
                        ret_comps: Vec::new(),
                        pierce: 0,
                        last_hit: -1,
                        stuck,
                    });
                    bump(eid, &mut mx);
                }
            }
            let n = rv(payload, &mut p);
            {
                let mut bo = bobbers().lock().unwrap();
                for _ in 0..n {
                    if p + 61 > payload.len() {
                        break;
                    }
                    let eid = ri4(payload, &mut p);
                    let owner = ri4(payload, &mut p);
                    let (x, y, z) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let (dx, dy, dz) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let age = ri4(payload, &mut p);
                    let bit = b[p] != 0;
                    p += 1;
                    bo.push(Bobber {
                        eid,
                        owner,
                        x,
                        y,
                        z,
                        dx,
                        dy,
                        dz,
                        age,
                        bit,
                    });
                    bump(eid, &mut mx);
                }
            }
            let n = rv(payload, &mut p);
            {
                let mut pe = pearls().lock().unwrap();
                for _ in 0..n {
                    if p + 60 > payload.len() {
                        break;
                    }
                    let eid = ri4(payload, &mut p);
                    let owner = ri4(payload, &mut p);
                    let (x, y, z) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let (dx, dy, dz) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let ttl = legacy_tick_count(ri4(payload, &mut p) as u64, legacy_ms_ages) as i32;
                    pe.push(Pearl {
                        eid,
                        owner,
                        x,
                        y,
                        z,
                        dx,
                        dy,
                        dz,
                        ttl,
                    });
                    bump(eid, &mut mx);
                }
            }
            let n = rv(payload, &mut p);
            {
                let mut pr = primed().lock().unwrap();
                for _ in 0..n {
                    if p + 32 > payload.len() {
                        break;
                    }
                    let eid = ri4(payload, &mut p);
                    let (x, y, z) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let left = legacy_tick_count(
                        i32::from_be_bytes(b[p..p + 4].try_into().unwrap()) as u64,
                        legacy_ms_ages,
                    ) as i32;
                    p += 4;
                    pr.push(Primed { eid, x, y, z, left });
                    bump(eid, &mut mx);
                }
            }
            if mx > 0 {
                NEXT_EID.fetch_max(mx + 1, Ordering::SeqCst);
            }
        }
        if p + 8 <= payload.len() {
            RAINING.store(
                legacy_tick_count(
                    u64::from_be_bytes(b[p..p + 8].try_into().unwrap()),
                    legacy_ms_ages,
                ),
                Ordering::Relaxed,
            );
            p += 8;
        }
        if p + 8 <= payload.len() {
            THUNDERING.store(
                legacy_tick_count(
                    u64::from_be_bytes(b[p..p + 8].try_into().unwrap()),
                    legacy_ms_ages,
                ),
                Ordering::Relaxed,
            );
            p += 8;
        }
        if p + 4 <= payload.len() {
            for r in [
                &GR_KEEP_INVENTORY,
                &GR_DAYLIGHT_CYCLE,
                &GR_WEATHER_CYCLE,
                &GR_MOB_SPAWNING,
                &GR_MOB_GRIEFING,
                &GR_NATURAL_REGEN,
                &GR_MOB_LOOT,
            ] {
                r.store(b[p], Ordering::Relaxed);
                p += 1;
            }
        }
        if p + 25 <= payload.len() {
            let (x, y, z) = (
                f64::from_be_bytes(b[p..p + 8].try_into().unwrap()),
                f64::from_be_bytes(b[p + 8..p + 16].try_into().unwrap()),
                f64::from_be_bytes(b[p + 16..p + 24].try_into().unwrap()),
            );
            *WORLD_SPAWN.lock().unwrap() = (x, y, z, 0.0);
            p += 24;
            DIFFICULTY.store(b[p], Ordering::Relaxed);
            p += 1;
        }
        if p < payload.len() {
            let mut tm = tags().lock().unwrap();
            let pn = rv(payload, &mut p);
            for _ in 0..pn {
                if p + 2 > payload.len() {
                    break;
                }
                let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                p += 2;
                if p + nl > payload.len() {
                    break;
                }
                let nm = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                p += nl;
                let tc = rv(payload, &mut p);
                let mut st = HashSet::new();
                for _ in 0..tc {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let tl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + tl > payload.len() {
                        break;
                    }
                    st.insert(String::from_utf8_lossy(&payload[p..p + tl]).to_string());
                    p += tl;
                }
                if !st.is_empty() {
                    tm.insert(format!("p:{}", nm), st);
                }
            }
            let mn2 = rv(payload, &mut p);
            let mut mtag: Vec<(usize, HashSet<String>)> = Vec::new();
            for _ in 0..mn2 {
                let i = rv(payload, &mut p) as usize;
                let tc = rv(payload, &mut p);
                let mut st = HashSet::new();
                for _ in 0..tc {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let tl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + tl > payload.len() {
                        break;
                    }
                    st.insert(String::from_utf8_lossy(&payload[p..p + tl]).to_string());
                    p += tl;
                }
                if !st.is_empty() {
                    mtag.push((i, st));
                }
            }
            restored_mob_tags = mtag;
            if p < payload.len() {
                let sn = rv(payload, &mut p);
                let mut sf = sched_fns().lock().unwrap();
                let t_now = TICK.load(Ordering::Relaxed);
                for _ in 0..sn {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + nl > payload.len() {
                        break;
                    }
                    let f2 = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                    p += nl;
                    if p + 8 > payload.len() {
                        break;
                    }
                    let rem = u64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                    p += 8;
                    sf.push((f2, t_now + rem));
                }
            }
            if p < payload.len() {
                let gn = rv(payload, &mut p);
                let mut ag = adv_done().lock().unwrap();
                for _ in 0..gn {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + nl > payload.len() {
                        break;
                    }
                    let pn = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                    p += nl;
                    let ic = rv(payload, &mut p);
                    let mut ids = HashSet::new();
                    for _ in 0..ic {
                        if p + 2 > payload.len() {
                            break;
                        }
                        let il = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                        p += 2;
                        if p + il > payload.len() {
                            break;
                        }
                        ids.insert(String::from_utf8_lossy(&payload[p..p + il]).to_string());
                        p += il;
                    }
                    if !ids.is_empty() {
                        ag.insert(pn, ids);
                    }
                }
            }
            // player attributes
            if p < payload.len() {
                let an = rv(payload, &mut p);
                let mut at = attrs().lock().unwrap();
                for _ in 0..an {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + nl > payload.len() {
                        break;
                    }
                    let nm = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                    p += nl;
                    let cnt = rv(payload, &mut p);
                    let mut m = HashMap::new();
                    for _ in 0..cnt {
                        if p + 2 > payload.len() {
                            break;
                        }
                        let kl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                        p += 2;
                        if p + kl + 8 > payload.len() {
                            break;
                        }
                        let kn = String::from_utf8_lossy(&payload[p..p + kl]).to_string();
                        p += kl;
                        let v = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                        p += 8;
                        m.insert(kn, v);
                    }
                    if !m.is_empty() {
                        at.insert(format!("p:{}", nm), m);
                    }
                }
                // objective types + sidebar
                if p < payload.len() {
                    let oc = rv(payload, &mut p);
                    let mut board = scoreboard().lock().unwrap();
                    for _ in 0..oc {
                        if p + 2 > payload.len() {
                            break;
                        }
                        let ol = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                        p += 2;
                        if p + ol + 2 > payload.len() {
                            break;
                        }
                        let on = String::from_utf8_lossy(&payload[p..p + ol]).to_string();
                        p += ol;
                        let tl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                        p += 2;
                        if p + tl > payload.len() {
                            break;
                        }
                        let criterion = String::from_utf8_lossy(&payload[p..p + tl]);
                        if let (Some(objective), Some(criterion)) =
                            (board.objectives.get_mut(&on), ScoreboardCriteria::parse(&criterion))
                        {
                            objective.criteria = criterion;
                            if criterion == ScoreboardCriteria::Health {
                                objective.render_type = ScoreboardRenderType::Hearts;
                            }
                        }
                        p += tl;
                    }
                    if p + 2 <= payload.len() {
                        let sl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                        p += 2;
                        if p + sl <= payload.len() && sl > 0 {
                            let obj = String::from_utf8_lossy(&payload[p..p + sl]);
                            let _ = board.set_display(ScoreboardDisplaySlot::Sidebar, Some(&obj));
                        }
                    }
                }
            }
        }
        eprintln!(
            "[spine] loaded meta: {} mobs, {} invs, {} chests",
            mn,
            inv_store().lock().unwrap().len(),
            chests().lock().unwrap().len()
        );
    }

    // Apply the non-Overworld overrides after the legacy sections have loaded. A sign whose key
    // already exists in the Overworld section stays independent: the footer's key names its own
    // dimension. A player's coordinates came from the legacy section; only the dimension changes.
    if !dim_signs.is_empty() {
        let mut sg = signs().lock().unwrap();
        for (dim, x, y, z, front, lines) in dim_signs {
            sg.insert((dim, x, y, z, front), lines);
        }
    }
    if !dim_players.is_empty() {
        let mut ps = pos_store().lock().unwrap();
        for (name, dim) in dim_players {
            if let Some(entry) = ps.get_mut(&name) {
                entry.0 = dim;
            }
        }
    }

    // Falling queue, when the save carries one. Absent sections are the normal case for saves
    // written before this existed, so a missing tag leaves the queue empty rather than failing.
    if p + 4 <= payload.len() && &payload[p..p + 4] == b"FAL2" {
        p += 4;
        let n = rv(payload, &mut p);
        for _ in 0..n {
            if p + 44 > payload.len() {
                break;
            }
            let eid = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            let x = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap());
            p += 8;
            let y = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap());
            p += 8;
            let z = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap());
            p += 8;
            let dy = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap());
            p += 8;
            let state = u32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            let time = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            NEXT_EID.fetch_max(eid + 1, Ordering::SeqCst);
            falling().lock().unwrap().push(Fall {
                eid,
                x,
                y,
                z,
                dy,
                state,
                time,
            });
        }
    } else if p + 4 <= payload.len() && &payload[p..p + 4] == b"FAL1" {
        // The old cell-moving model kept the block in the world and stored its cell. Convert to
        // an entity and remove the block, so it does not remain after the entity lands.
        p += 4;
        let n = rv(payload, &mut p);
        for _ in 0..n {
            if p + 12 > payload.len() {
                break;
            }
            let fx = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            let fy = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            let fz = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            let st = block_at(fx, fy, fz);
            if st == 0 {
                continue;
            }
            // An absent overlay entry falls back to generated terrain, so removing the entry
            // would leave a generated block where the migrating block used to be. Record air
            // explicitly: the block is now the entity, not whatever terrain is underneath it.
            world()
                .lock()
                .unwrap()
                .entry((DimId::Overworld, fx >> 4, fz >> 4))
                .or_default()
                .insert(local_idx(fx, fy, fz), 0);
            let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
            falling().lock().unwrap().push(Fall {
                eid,
                x: fx as f64 + 0.5,
                y: fy as f64,
                z: fz as f64 + 0.5,
                dy: 0.0,
                state: st,
                time: 0,
            });
        }
    }

    GR_MAX_ENTITY_CRAMMING.store(ENTITY_CRAMMING_DEFAULT, Ordering::Relaxed);
    // World random state, when the save carries one. A save without it (written before
    // this section existed) leaves the stream at the seed-derived start.
    if p + 4 <= payload.len() && &payload[p..p + 4] == b"RNG1" {
        p += 4;
        if p + 8 <= payload.len() {
            world_rng::restore(u64::from_be_bytes(payload[p..p + 8].try_into().unwrap()));
            p += 8;
        }
    }
    if p + 4 <= payload.len() && &payload[p..p + 4] == b"GCR1" {
        p += 4;
        if p + 4 <= payload.len() {
            GR_MAX_ENTITY_CRAMMING.store(
                i32::from_be_bytes(payload[p..p + 4].try_into().unwrap()),
                Ordering::Relaxed,
            );
            p += 4;
        }
    }
    if p + 4 <= payload.len() && &payload[p..p + 4] == b"BRD1" {
        p += 4;
        const BORDER_SAVE_LEN: usize = 8 * 7 + 4 * 3;
        if p + BORDER_SAVE_LEN <= payload.len() {
            let center_x = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let center_z = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let old_size = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let new_size = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let duration = i64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let damage_per_block = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let damage_safe_zone = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let warning_blocks = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap()); p += 4;
            let warning_time_secs = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap()); p += 4;
            let absolute_max_size = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            if [center_x, center_z, old_size, new_size, damage_per_block, damage_safe_zone]
                .iter().all(|value| value.is_finite())
            {
                *world_border().lock().unwrap() = WorldBorderState {
                    center_x, center_z, old_size, new_size,
                    lerp_start_ms: border_now_ms(),
                    lerp_duration_ms: duration.max(0),
                    damage_per_block, damage_safe_zone, warning_blocks,
                    warning_time_secs, absolute_max_size,
                };
            }
        }
    }
    if let Some(bytes) = payload.get(p..p + 8).filter(|bytes| &bytes[..4] == b"SPA1") {
        let angle = f32::from_be_bytes(bytes[4..].try_into().unwrap());
        if angle.is_finite() {
            WORLD_SPAWN.lock().unwrap().3 = angle;
        }
    }
    if let Some(state) = tick_state {
        TICK_IDX.store(state.now, Ordering::Relaxed);
        BLOCK_TICK_ORDER.store(state.block_order, Ordering::Relaxed);
        *block_ticks().lock().unwrap() = state.blocks;
        let mut owner = GeneratedFluidOwner::new();
        owner.queue = state.fluids;
        owner.next_order = state.fluid_order;
        owner.owned = state.owned;
        *GENERATED_FLUID_OWNER
            .get_or_init(|| Mutex::new(GeneratedFluidOwner::new()))
            .lock()
            .unwrap() = owner;
        worldgen::with_gen(|g| {
            g.restore_tick_sources(state.sources);
            g.restore_generated_ticks(state.generated_blocks, state.generated_fluids);
        });
    }
    if let Some(mut generated) = generated {
        let mut cart_inventories = Vec::new();
        for cart in &mut generated.content.carts {
            cart.eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
            if !cart.has_loot_table() {
                let uuid = cart.uuid()?;
                if let crate::nbt::Tag::Compound(fields) = &mut cart.data {
                    if let Some(items) = fields.remove(&crate::nbt::NbtString::text("Items")) {
                        cart_inventories.push((ContainerKey::Cart(uuid), items_nbt_to_stacks(&items)?));
                    }
                }
            }
        }
        for frame in &mut generated.content.frames {
            frame.eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
        }
        *generated_content().lock().unwrap() = generated.content;
        if !cart_inventories.is_empty() {
            let mut ch = chests().lock().unwrap();
            for (key, slots) in cart_inventories {
                ch.insert(key, slots);
            }
        }
        worldgen::with_gen(|g| {
            g.payload_sources = generated.sources;
            g.mineshaft_spawners = generated.spider_flags;
        });
    }
    // The legacy block-loot footer is Overworld-only; its positions load back with that dimension.
    *pending_block_loot().lock().unwrap() = block_loot
        .into_iter()
        .map(|(pos, entry)| (ow_block(pos), entry))
        .collect();
    // DIM2 entries restore into their own dimensions. They are applied after the legacy sections
    // so a mixed save cannot let an old Overworld entry overwrite a new one at the same coordinate.
    if !dim2.is_empty() {
        {
            let mut ch = chests().lock().unwrap();
            for (pos, slots) in dim2.chests {
                ch.insert(ContainerKey::Block(pos), slots);
            }
        }
        {
            let mut fs = furnaces().lock().unwrap();
            for (pos, fr) in dim2.furnaces {
                fs.insert(pos, fr);
            }
        }
        {
            let mut cg = crops().lock().unwrap();
            for (pos, age) in dim2.crops {
                cg.insert(pos, age);
            }
        }
        {
            let mut all = pending_block_loot().lock().unwrap();
            for (pos, entry) in dim2.loot {
                all.insert(pos, entry);
            }
        }
    }
    // Persisted drop, projectile, TNT and falling-block IDs must reserve their range
    // before mobs receive fresh IDs. Keep mob ordinals stable for shearing and tags.
    if !restored_mobs.is_empty() {
        let mut ms = mobs().lock().unwrap();
        let first = ms.len();
        for (index, (etype, x, y, z, hp)) in restored_mobs.into_iter().enumerate() {
            ms.push(Arc::new(Mob {
                dim: mob_dimensions.get(index).copied().unwrap_or(DimId::Overworld),
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                uuid: [0u8; 16],
                etype,
                pos: Mutex::new((x, y, z, 0.0, 0.0)),
                hp: Mutex::new(hp),
                last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
                hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
                last_hurt_tick: Mutex::new(None),
                vel: Mutex::new([0.0; 3]),
                fuse: Mutex::new(0),
                fire_ticks: Mutex::new(0),
                love: Mutex::new(0),
                air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            }));
        }
        let mut sh = sheared().lock().unwrap();
        for index in restored_sheared {
            if let Some(m) = ms.get(first + index) {
                sh.insert(m.eid);
            }
        }
        drop(sh);
        let mut tm = tags().lock().unwrap();
        for (index, state) in restored_mob_tags {
            if let Some(m) = ms.get(first + index) {
                tm.insert(format!("e:{}", m.eid), state);
            }
        }
    }
    *live_brewing_stands().lock().unwrap() = brewing;
    eprintln!(
        "[spine] loaded {} dirty chunks",
        world().lock().unwrap().len()
    );
    rebuild_derived_indexes();
    Ok(())
}

const BREWING_SAVE_MAGIC: &[u8; 4] = b"BRS1";

fn append_brewing_footer(out: &mut Vec<u8>) {
    let stands = live_brewing_stands().lock().unwrap();
    let mut entries: Vec<_> = stands
        .iter()
        .filter(|(_, stand)| {
            stand.stand.fuel > 0 || stand.menu.slots.iter().any(|stack| stack.1 > 0)
        })
        .collect();
    if entries.is_empty() {
        return;
    }
    entries.sort_by_key(|(&(dim, x, y, z), _)| (dim.index(), x, y, z));
    let start = out.len();
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (&(dim, x, y, z), entry) in entries {
        out.push(dim.index() as u8);
        for coordinate in [x, y, z] {
            out.extend_from_slice(&coordinate.to_be_bytes());
        }
        let slots = entry
            .menu
            .slots
            .iter()
            .enumerate()
            .filter(|(_, stack)| stack.1 > 0)
            .map(|(i, stack)| (i as i16, stack.clone()))
            .collect();
        write_stack_table(out, &slots);
        out.push(entry.stand.fuel);
        // If the ingredient was swapped since the last tick, the outstanding
        // recipe is already invalid. The loaded entity can start a new one.
        let current = item_to_name(entry.menu.slots[3].0).and_then(BrewingIngredient::from_item);
        let remaining = if current == entry.stand.active_ingredient {
            entry.stand.brew_time
        } else {
            0
        };
        out.extend_from_slice(&remaining.to_be_bytes());
    }
    out.extend_from_slice(&((out.len() - start) as u64).to_be_bytes());
    out.extend_from_slice(BREWING_SAVE_MAGIC);
}

fn read_brewing_footer(
    payload: &[u8],
) -> Result<(&[u8], HashMap<DimBlockPos, LiveBrewingStand>), &'static str> {
    let mut stands = HashMap::new();
    if !payload.ends_with(BREWING_SAVE_MAGIC) {
        return Ok((payload, stands));
    }
    let end = payload
        .len()
        .checked_sub(12)
        .ok_or("truncated brewing footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid brewing footer size")?;
    let start = end.checked_sub(size).ok_or("invalid brewing footer size")?;
    let body = &payload[start..end];
    let mut p = 0;
    let count = read_dim2_count(body, &mut p, 17).map_err(|_| "invalid brewing stand count")?;
    for _ in 0..count {
        let header = body
            .get(p..p + 13)
            .ok_or("truncated brewing stand position")?;
        p += header.len();
        let dim = DimId::from_index(header[0] as usize).ok_or("invalid brewing dimension")?;
        let coordinate = |i| i32::from_be_bytes(header[i..i + 4].try_into().unwrap());
        let key = (dim, coordinate(1), coordinate(5), coordinate(9));
        let slots = read_stack_table(body, &mut p, true);
        let timers = body.get(p..p + 3).ok_or("truncated brewing stand timers")?;
        p += timers.len();
        let mut entry = LiveBrewingStand::default();
        for (index, stack) in slots {
            *entry
                .menu
                .slots
                .get_mut(index as usize)
                .ok_or("invalid brewing slot")? = stack;
        }
        entry.stand.fuel = timers[0];
        entry.stand.brew_time = u16::from_be_bytes(timers[1..3].try_into().unwrap());
        if entry.stand.fuel > BREW_FUEL_PER_POWDER || entry.stand.brew_time > BREW_TIME_TICKS {
            return Err("invalid brewing stand timers");
        }
        if entry.stand.brew_time > 0 {
            entry.stand.active_ingredient =
                item_to_name(entry.menu.slots[3].0).and_then(BrewingIngredient::from_item);
            if entry.stand.active_ingredient.is_none() {
                return Err("brewing stand has a timer without an ingredient");
            }
        }
        if stands.insert(key, entry).is_some() {
            return Err("duplicate brewing stand position");
        }
    }
    if p != body.len() {
        return Err("trailing bytes in brewing footer");
    }
    Ok((&payload[..start], stands))
}

#[cfg(test)]
mod world_seed_persistence_tests {
    use super::*;

    #[test]
    fn world_seed_footer_round_trips_and_preserves_legacy_payloads() {
        let legacy = b"legacy save";
        assert_eq!(read_world_seed_footer(legacy), Ok((legacy.as_slice(), None)));

        let mut payload = legacy.to_vec();
        append_world_seed_footer(&mut payload, u64::MAX);
        assert_eq!(
            read_world_seed_footer(&payload),
            Ok((legacy.as_slice(), Some(u64::MAX)))
        );
    }

    #[test]
    fn saved_seed_is_default_and_mismatched_override_is_rejected() {
        assert_eq!(resolve_world_seed(None, Some(123)), Ok(123));
        assert_eq!(resolve_world_seed(Some(123), Some(123)), Ok(123));
        assert_eq!(resolve_world_seed(None, None), Ok(0));
        assert_eq!(
            resolve_world_seed(Some(456), Some(123)),
            Err("MC_SPINE_SEED does not match the saved world seed")
        );
    }
}

#[cfg(test)]
mod save_integrity_tests {
    use super::*;
    #[test]
    fn wrapped_world_roundtrip_and_legacy_acceptance() {
        let legacy = vec![0u8, b'M', b'E', b'T', b'3', 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(unwrap_world_payload(legacy.clone()), Ok(legacy.clone()));
        assert_eq!(
            unwrap_world_payload(wrap_world_payload(&legacy)),
            Ok(legacy)
        );
    }
    #[test]
    fn truncated_and_damaged_new_saves_are_rejected() {
        let original = wrap_world_payload(b"world-data");
        for n in 0..original.len() {
            let short = original[..n].to_vec();
            if n >= 3 {
                assert!(unwrap_world_payload(short).is_err(), "truncated to {n}");
            }
        }
        let mut corrupt = original;
        *corrupt.last_mut().unwrap() ^= 1;
        assert_eq!(unwrap_world_payload(corrupt), Err("save checksum mismatch"));
    }
}

#[cfg(test)]
mod save_backup_tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spine-save-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saves_keep_three_backup_generations_and_no_tmp_left() {
        let dir = scratch("gens");
        let world = dir.join("world.bin");
        // interval 0: every save rotates, so the generations are easy to read
        for n in 1..=5u8 {
            write_world_file(&world, &[n], Duration::ZERO).unwrap();
        }
        let read = |name: &str| std::fs::read(dir.join(name)).unwrap();
        assert_eq!(read("world.bin"), [5]);
        assert_eq!(read("world.bin.bak"), [4]);
        assert_eq!(read("world.bin.bak.1"), [3]);
        assert_eq!(read("world.bin.bak.2"), [2]);
        assert!(!dir.join("world.bin.bak.3").exists());
        assert!(!dir.join("world.bin.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn older_generations_do_not_advance_within_the_interval() {
        let dir = scratch("interval");
        let world = dir.join("world.bin");
        for n in 1..=5u8 {
            write_world_file(&world, &[n], Duration::from_secs(3600)).unwrap();
        }
        let read = |name: &str| std::fs::read(dir.join(name)).unwrap();
        assert_eq!(read("world.bin"), [5]);
        assert_eq!(
            read("world.bin.bak"),
            [4],
            ".bak is always the previous save"
        );
        // bak.1 was created on the second save and is far younger than an hour
        assert_eq!(read("world.bin.bak.1"), [1]);
        assert!(!dir.join("world.bin.bak.2").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_save_is_reported_with_its_path_and_keeps_the_old_world() {
        let dir = scratch("fail");
        let world = dir.join("world.bin");
        write_world_file(&world, b"good", Duration::ZERO).unwrap();
        // a directory squatting on the tmp path makes the tmp write fail
        std::fs::create_dir(dir.join("world.bin.tmp")).unwrap();
        let err = write_world_file(&world, b"new", Duration::ZERO).unwrap_err();
        assert!(err.contains("world.bin.tmp"), "{err}");
        assert_eq!(std::fs::read(&world).unwrap(), b"good");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
