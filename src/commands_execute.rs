// `/execute in <dim> run ...` overrides the dimension for the subcommand only. A
// thread-local keeps the existing four-field CmdCtx (and every test that builds one)
// unchanged while still letting setblock/tp know which world the command names.
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
fn pred_eval(body: &str, ctx: &CmdCtx, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    let b = body.trim();
    if b.starts_with('[') {
        if let Some(inner) = b.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            return jobjects(inner).iter().all(|o| pred_eval(o, ctx, depth + 1));
        }
        return false;
    }
    let cond = jstr(b, "condition")
        .unwrap_or("")
        .trim_start_matches("minecraft:");
    match cond {
        "inverted" => jblock(b, "term")
            .map(|t| !pred_eval(t, ctx, depth + 1))
            .unwrap_or(false),
        "any_of" | "alternative" => jarray(b, "terms")
            .map(|a| jobjects(a).iter().any(|o| pred_eval(o, ctx, depth + 1)))
            .unwrap_or(false),
        "all_of" => jarray(b, "terms")
            .map(|a| jobjects(a).iter().all(|o| pred_eval(o, ctx, depth + 1)))
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
        "entity_properties" => {
            // only "this" + type check; context entity is a player when present
            match jblock(b, "predicate").and_then(|p| jstr(p, "type")) {
                Some(t) => t.trim_start_matches("minecraft:") == "player" && ctx.me.is_some(),
                None => ctx.me.is_some(),
            }
        }
        "location_check" => {
            let st = block_at(ctx.x as i32, ctx.y as i32 - 1, ctx.z as i32);
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
// execute subset: as/at/positioned/if|unless block|entity|score ... run <cmd>
fn exec_result(ctx: &CmdCtx, cmd: &str) -> i64 {
    // result value of a nested `execute if ...` chain (used by store result score)
    let a: Vec<&str> = cmd.split_whitespace().collect();
    let mut i = if a.first() == Some(&"execute") { 1 } else { 0 };
    let mut last = 0i64;
    while i < a.len() {
        match a[i] {
            "if" => match a.get(i + 1).copied().unwrap_or("") {
                "entity" => {
                    last = resolve_selector(a.get(i + 2).copied().unwrap_or(""), ctx.me.as_ref())
                        .len() as i64;
                    i += 3;
                    continue;
                }
                "block" => {
                    let (x, y, z) = (
                        coord(a.get(i + 2).copied().unwrap_or("~"), ctx.x) as i32,
                        coord(a.get(i + 3).copied().unwrap_or("~"), ctx.y) as i32,
                        coord(a.get(i + 4).copied().unwrap_or("~"), ctx.z) as i32,
                    );
                    let want = a.get(i + 5).copied().unwrap_or("");
                    last = block_pred_match(block_at(x, y, z), want) as i64;
                    i += 6;
                    continue;
                }
                _ => {}
            },
            "run" => break,
            _ => {}
        }
        i += 1;
    }
    last
}
// Nested `/execute run execute ...` recursion is depth-gated: each level
// re-parses args and holds a stream borrow — uncapped nesting exhausts the
// stack before any reply (audit: no bound on the recurse via run_command_ctx).
thread_local! { static EXEC_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) } }
fn run_execute(ctx: &CmdCtx, args: &[&str], s: &mut Option<&Client>) -> String {
    let depth = EXEC_DEPTH.get();
    if depth >= 16 {
        return "execute depth limit".into();
    }
    let saved_dim = EXEC_DIM.with(|d| d.get());
    EXEC_DEPTH.set(depth + 1);
    let r = run_execute_inner(ctx, args, s);
    EXEC_DIM.with(|d| d.set(saved_dim));
    EXEC_DEPTH.set(depth);
    r
}
fn run_execute_inner(ctx: &CmdCtx, args: &[&str], s: &mut Option<&Client>) -> String {
    let mut i = 0;
    let mut cur = ctx.clone();
    let mut targets: Vec<String> = vec![]; // empty = self/console
    let mut passed = true;
    let mut store_score: Option<(String, String)> = None;
    while i < args.len() {
        match args[i] {
            "run" => break,
            "in" => {
                // `/execute in <overworld|the_nether|the_end> run <subcommand>`
                let raw = args.get(i + 1).copied().unwrap_or("");
                let name = if raw.contains(':') {
                    raw.to_string()
                } else {
                    format!("minecraft:{raw}")
                };
                if let Some(dim) = DimId::parse_dimension_id(&name) {
                    EXEC_DIM.with(|d| d.set(Some(dim)));
                }
                i += 1;
            }
            "store" => {
                // store result score <entry> <objective>
                if args.get(i + 1).copied() == Some("result")
                    && args.get(i + 2).copied() == Some("score")
                {
                    store_score = Some((
                        args.get(i + 3).copied().unwrap_or("").to_string(),
                        args.get(i + 4).copied().unwrap_or("").to_string(),
                    ));
                    i += 4;
                }
            }
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
                            let p = *p.pos.lock().unwrap();
                            cur.x = p.0;
                            cur.y = p.1;
                            cur.z = p.2;
                        }
                    } else if let Ok(eid) = t.parse::<i32>() {
                        let cls: Vec<Arc<Client>> = client_snapshot();
                        if let Some(c) = cls.iter().find(|c| c.eid == eid) {
                            let p = *c.pos.lock().unwrap();
                            cur.x = p.0;
                            cur.y = p.1;
                            cur.z = p.2;
                        }
                    }
                }
            }
            "positioned" => {
                cur.x = coord(args.get(i + 1).copied().unwrap_or("0"), cur.x);
                cur.y = coord(args.get(i + 2).copied().unwrap_or("0"), cur.y);
                cur.z = coord(args.get(i + 3).copied().unwrap_or("0"), cur.z);
                i += 3;
            }
            "if" | "unless" => {
                let neg = args[i] == "unless";
                i += 1;
                let ok = match args.get(i).copied().unwrap_or("") {
                    "block" => {
                        let (x, y, z) = (
                            coord(args.get(i + 1).copied().unwrap_or("~"), cur.x) as i32,
                            coord(args.get(i + 2).copied().unwrap_or("~"), cur.y) as i32,
                            coord(args.get(i + 3).copied().unwrap_or("~"), cur.z) as i32,
                        );
                        i += 3;
                        let want = args.get(i + 1).copied().unwrap_or("");
                        let st = block_at(x, y, z);
                        block_pred_match(st, want)
                    }
                    "entity" => {
                        i += 1;
                        !resolve_selector(args.get(i).copied().unwrap_or(""), cur.me.as_ref())
                            .is_empty()
                    }
                    "data" if args.get(i + 1).copied() == Some("entity") => {
                        // if data entity <name> <path>
                        let nm = args.get(i + 2).copied().unwrap_or("");
                        let path = args.get(i + 3).copied().unwrap_or("");
                        i += 3;
                        if path.starts_with("Inventory[{") {
                            let idm = path
                                .split("id:\"")
                                .nth(1)
                                .and_then(|r| r.split('\"').next())
                                .unwrap_or("");
                            let item = name_to_item(idm.trim_start_matches("minecraft:"));
                            actors()
                                .lock()
                                .unwrap()
                                .get(nm)
                                .map(|a| {
                                    a.inv
                                        .lock()
                                        .unwrap()
                                        .values()
                                        .any(|(it, _, _)| Some(*it) == item)
                                })
                                .unwrap_or(false)
                        } else {
                            false
                        }
                    }
                    "data" => {
                        // if data block x y z <path>
                        let bx = coord(args.get(i + 2).copied().unwrap_or("~"), cur.x) as i32;
                        let by = coord(args.get(i + 3).copied().unwrap_or("~"), cur.y) as i32;
                        let bz = coord(args.get(i + 4).copied().unwrap_or("~"), cur.z) as i32;
                        let path = args.get(i + 5).copied().unwrap_or("");
                        i += 5;
                        let slots = if is_furnace_at(bx, by, bz) {
                            furnace_slots((bx, by, bz))
                        } else {
                            chests()
                                .lock()
                                .unwrap()
                                .get(&ContainerKey::ow((bx, by, bz)))
                                .cloned()
                                .unwrap_or_default()
                        };
                        match path {
                            "Items" => !slots.is_empty(),
                            "Items[0]" => slots.contains_key(&0i16),
                            p if p.starts_with("Items[{") => {
                                let idm = p
                                    .split("id:\"")
                                    .nth(1)
                                    .and_then(|r| r.split('\"').next())
                                    .unwrap_or("");
                                let ctm = p
                                    .split("count:")
                                    .nth(1)
                                    .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
                                    .and_then(|r| r.parse::<i64>().ok());
                                let item = name_to_item(idm.trim_start_matches("minecraft:"));
                                let have: i64 = slots
                                    .values()
                                    .filter(|(it, _, _)| Some(*it) == item)
                                    .map(|(_, c, _)| *c as i64)
                                    .sum();
                                match ctm {
                                    Some(c) => have == c,
                                    None => have > 0,
                                }
                            }
                            _ => false,
                        }
                    }
                    "predicate" => {
                        let id = args.get(i + 1).copied().unwrap_or("");
                        i += 1;
                        let id = if id.contains(':') {
                            id.to_string()
                        } else {
                            format!("minecraft:{}", id)
                        };
                        predicates()
                            .lock()
                            .unwrap()
                            .get(&id)
                            .map(|b| pred_eval(b, &cur, 0))
                            .unwrap_or(false)
                    }
                    "score" => {
                        // if score <target> <obj> matches <range>
                        let t = args.get(i + 1).copied().unwrap_or("");
                        let obj = args.get(i + 2).copied().unwrap_or("");
                        i += 2;
                        if args.get(i + 1).copied() == Some("matches") {
                            i += 1;
                            let range = args.get(i + 1).copied().unwrap_or("0");
                            let entry = match t {
                                "@s" => cur.me.as_ref().map(|c| c.name.clone()).unwrap_or_default(),
                                n => n.to_string(),
                            };
                            let value = scoreboard().lock().unwrap().get(&entry, obj).unwrap_or(0);
                            in_range(i64::from(value), range)
                        } else {
                            false
                        }
                    }
                    _ => false,
                };
                if ok == neg {
                    passed = false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if i >= args.len() {
        return if passed {
            "Test passed".into()
        } else {
            "Test failed".into()
        };
    }
    let cmd = args[i + 1..].join(" ");
    if !passed {
        return "Test failed".into();
    }
    if let Some((en, ob)) = &store_score {
        let r = exec_result(&cur, &cmd);
        let entry = match en.as_str() {
            "@s" => cur.me.as_ref().map(|c| c.name.clone()).unwrap_or_default(),
            n => n.to_string(),
        };
        let mut board = scoreboard().lock().unwrap();
        if !scoreboard_entry_available(&board, &entry, ob) {
            return "Too many scores for objective".into();
        }
        if let Err(error) = board.set(&entry, ob, r as i32) {
            return scoreboard_command_error(error);
        }
        broadcast_score(&board, &entry, ob);
        return format!("Stored {} in {} for {}", r, ob, entry);
    }
    let mut n = 0;
    if targets.is_empty() {
        run_command_ctx(&cur, &format!("/{}", cmd), s);
        n += 1;
    } else {
        for t in &targets {
            if t.starts_with("m:") {
                continue;
            } // mob-as-executor unsupported yet
            let eid: i32 = t.parse().unwrap_or(-1);
            let cls: Vec<Arc<Client>> = client_snapshot();
            if let Some(c) = cls.iter().find(|c| c.eid == eid).cloned() {
                let p = *c.pos.lock().unwrap();
                let cc = CmdCtx {
                    me: Some(c),
                    x: p.0,
                    y: p.1,
                    z: p.2,
                    };
                let _ = run_command_ctx(&cc, &format!("/{}", cmd), s);
                n += 1;
            }
        }
    }
    format!("executed {} command(s)", n)
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
fn in_range(v: i64, r: &str) -> bool {
    if let Some((a, b)) = r.split_once("..") {
        let lo = a.parse::<i64>().unwrap_or(i64::MIN);
        let hi = b.parse::<i64>().unwrap_or(i64::MAX);
        v >= lo && v <= hi
    } else {
        v == r.parse().unwrap_or(i64::MIN + 1)
    }
}
