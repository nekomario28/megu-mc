struct PreplaySession {
    s: TcpStream,
    peer: String,
    name: String,
    uuid: [u8; 16],
    login_skin: Option<(String, String)>,
    authenticated: bool,
    login: connection_limits::LoginGuard,
    compression: Option<usize>,
    rx: RxBuf,
    link: ConfigLink,
    client_view: i32,
    cfg: Vec<(i64, Vec<u8>)>,
    cipher_guard: Option<SocketCipherGuard>,
    compression_registration: Option<LoginCompression>,
}

struct PlaySession {
    s: TcpStream,
    me: Arc<Client>,
    rx: RxBuf,
    compression: Option<usize>,
    retry_chunks: bool,
    view: i32,
    initial_held: i16,
    disconnect_guard: ConnectionCleanupGuard,
    cipher_guard: Option<SocketCipherGuard>,
    compression_registration: Option<LoginCompression>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlayPacketControl {
    Handled,
    Skip,
    Break,
}

struct PlayPacketContext<'a> {
    me: &'a Arc<Client>,
    retry_chunks: &'a mut bool,
    keepalive: &'a mut connection_limits::KeepaliveState,
    keepalive_sent_at: &'a mut Option<Instant>,
    view: &'a mut i32,
    held_slot: &'a mut i16,
    chat_throttle: &'a mut connection_limits::ChatThrottle,
    move_guard: &'a mut ingress_guard::MovementFloodGuard,
}

fn negotiate_preplay_session(mut s: TcpStream) -> Option<PreplaySession> {
    s.set_nodelay(true).ok();
    let mut peer = s
        .peer_addr()
        .map_or_else(|_| "unknown".to_string(), |a| a.ip().to_string());
    // Applied before handshake/login: idle unauthenticated sockets must expire.
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    s.set_write_timeout(Some(Duration::from_secs(10))).ok();
    // One wall-clock budget for handshake, login and configuration together.
    let login_deadline = Instant::now() + connection_limits::login_timeout();
    // This budget spans handshake, login and configuration, rather than
    // starting only after the client reaches play state.
    let mut preplay_rate = connection_limits::PacketRateLimit::new(Instant::now());
    let (id, hs) = read_preplay_frame(&mut s, login_deadline, None, &mut preplay_rate)?;
    if id != 0 {
        return None;
    }
    let (protocol, next) = handshake_parts(&hs)?;
    if protocol != 767 {
        let reason = if protocol < 767 {
            "Outdated client! Please use 1.21.1."
        } else {
            "Outdated server! Please use 1.21.1."
        };
        if next == 1 {
            status_version_mismatch(&mut s, reason, login_deadline);
        } else {
            login_disconnect(&mut s, reason);
        }
        return None;
    }
    if next == 1 {
        if let Some((0, _)) = read_preplay_frame(&mut s, login_deadline, None, &mut preplay_rate) {
            let online_players: Vec<(String, [u8; 16])> = clients()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|c| (c.name.clone(), c.uuid))
                .collect();
            let max = live_max_players();
            let motd = std::env::var("MEGU_MOTD").unwrap_or_else(|_| "megu".to_owned());
            let json = build_status_json_rich(&motd, max, &online_players, status_favicon());
            let mut pl = Vec::new();
            write_str(&mut pl, &json);
            let _ = send_packet(&mut s, 0, pl);
        }
        if let Some((1, ping)) = read_preplay_frame(&mut s, login_deadline, None, &mut preplay_rate)
        {
            let _ = send_packet(&mut s, 1, ping);
        }
        return None;
    }
    if next != 2 {
        return None;
    }
    // ---- login ----
    let (lid, lbody) = read_preplay_frame(&mut s, login_deadline, None, &mut preplay_rate)?;
    if lid != 0 {
        return None;
    }
    let (mut name, mut uuid) = login_identity(&lbody)?;
    // W4: identity-bearing login exchanges run before admission, so the ban and
    // whitelist checks judge the *verified* identity — the Velocity-forwarded
    // triple or the Mojang session profile — not whatever the socket claims.
    let mut login_skin = None;
    // `true` once an identity-bearing exchange verified this connection: every
    // privilege decision below may then trust the UUID, not just the claimed name.
    let mut authenticated = false;
    if velocity_forwarding_enabled() {
        match velocity_login(&mut s, login_deadline, &mut preplay_rate) {
            Ok(fwd) => {
                eprintln!(
                    "[spine] {peer} forwarded {} as {}",
                    fwd.forwarded_ip, fwd.username
                );
                peer = fwd.forwarded_ip;
                uuid = fwd.uuid;
                name = fwd.username;
                login_skin = forwarded_skin(&fwd.properties);
                authenticated = true;
            }
            Err(why) => {
                eprintln!("[security] velocity forwarding refused for {name} from {peer}: {why}");
                login_disconnect_text(
                    &mut s,
                    "This server requires connections through the proxy.",
                );
                return None;
            }
        }
    } else if online_mode_enabled() {
        match online_login(&mut s, &name, &peer, login_deadline, &mut preplay_rate) {
            Ok(profile) => {
                uuid = profile.uuid;
                name = profile.name;
                login_skin = forwarded_skin(&profile.properties);
                authenticated = true;
            }
            Err(why) => {
                eprintln!("[security] online-mode login refused for {name} from {peer}: {why}");
                login_disconnect(&mut s, "Failed to verify username!");
                return None;
            }
        }
    }
    // `LoginCompression` only guards the wire entry when a threshold was
    // negotiated; a cipher armed above must not outlive this connection, or a
    // later connection reusing the fd would read a dead keystream.
    let cipher_guard = conn_cipher(&s).map(|_| SocketCipherGuard(socket_key(&s)));
    if let Some(list) = connection_limits::allowlist() {
        if !list.permits(&name, &uuid) {
            eprintln!("[security] login refused for {name} from {peer}: not on the allowlist");
            login_disconnect(&mut s, "You are not white-listed on this server!");
            return None;
        }
    }
    // Vanilla `PlayerList.canPlayerLogin`: IP ban, player ban, whitelist, then
    // the player cap — evaluated before Login Success so the refusal arrives
    // as a Login-state Disconnect like Vanilla's.
    {
        let online = clients().lock().unwrap_or_else(|e| e.into_inner()).len();
        let mut live = live_admission().lock().unwrap_or_else(|e| e.into_inner());
        let live = live.get_or_insert_with(|| LiveAdmission::load(std::path::Path::new(".")));
        // A `/ban`, `/pardon`, `/whitelist` or `/op` issued since the last
        // login — or a file an operator edited by hand — lands on the next
        // admission check, not after a restart.
        live.refresh();
        if let Err(reason) = check_login_admission_verified(
            &name,
            &uuid_text(&uuid),
            &peer,
            online,
            live.max_players,
            &live.ops,
            &live.bans,
            &live.whitelist,
            authenticated,
        ) {
            eprintln!("[security] login refused for {name} from {peer}: {reason}");
            login_disconnect_text(&mut s, &reason);
            return None;
        }
    }
    // Held until this function returns, i.e. through the player's save on
    // logout: a second session under the same identity cannot start from stale
    // saved state or write over this one.
    let login = match LOGINS.reserve(&name, uuid) {
        Ok(g) => g,
        Err(why) => {
            eprintln!("[security] duplicate login refused for {name} from {peer}: {why:?}");
            login_disconnect(
                &mut s,
                "A player with that name or UUID is already logged in.",
            );
            return None;
        }
    };
    // Vanilla 1.21.1 compresses at 256 bytes by default; an explicit `-1`/`0`
    // in the env keeps framing uncompressed. In-process live-TCP fixtures
    // drive `handle` with a raw uncompressed client, so the test build keeps
    // the old disabled default unless the env names a threshold.
    let threshold = connection_limits::compression_threshold_env(|name| std::env::var(name).ok());
    let threshold = if cfg!(test) && std::env::var_os("MEGU_COMPRESSION_THRESHOLD").is_none() {
        -1
    } else {
        threshold
    };
    let Ok(compression_registration) = negotiate_compression(&mut s, threshold) else {
        return None;
    };
    let compression = usize::try_from(threshold).ok();
    let mut ls = Vec::new();
    ls.extend_from_slice(&uuid);
    write_str(&mut ls, &name);
    write_varint(&mut ls, 0);
    ls.push(0);
    if send_packet(&mut s, 2, ls).is_err() {
        return None;
    }
    match read_preplay_frame(&mut s, login_deadline, compression, &mut preplay_rate) {
        Some((3, _)) => {}
        _ => return None,
    }
    // ---- config ----
    let mut client_view: i32 = 8;
    let cfg = load_config_packets();
    // The phase runs on the shared exchange: the captured stream replays up
    // to `select_known_packs`, the `0x07`/`0x03` answers are awaited while the
    // `ConfigLink` keep-alive cadence, pong/cookie/pack-response servicing
    // and rate limiting run inline — vanilla's configuration listener, not a
    // bare replay. `rx` buffers partial frames across the poll timeouts.
    let mut rx = RxBuf::new(compression);
    let mut link = ConfigLink::new(client_view);
    if !run_config_exchange(
        &mut s,
        &mut rx,
        &cfg,
        compression,
        &mut preplay_rate,
        &mut link,
        false,
    ) {
        return None;
    }
    client_view = link.client_view;
    Some(PreplaySession {
        s,
        peer,
        name,
        uuid,
        login_skin,
        authenticated,
        login,
        compression,
        rx,
        link,
        client_view,
        cfg,
        cipher_guard,
        compression_registration,
    })
}

fn enter_play_session(preplay: PreplaySession, live_eid: Arc<AtomicI32>) -> Option<PlaySession> {
    let PreplaySession {
        mut s,
        peer,
        name,
        uuid,
        login_skin,
        authenticated,
        login,
        compression,
        rx,
        mut link,
        client_view,
        cfg,
        cipher_guard,
        compression_registration,
    } = preplay;
    // ---- play ----
    // P2-3-S4: read the saved dimension BEFORE 0x2b so Join Game and the first chunk
    // window both name the world the player actually logged out in. The 0x2b payload and
    // the camera position must agree or the client asks for chunks from the wrong generator.
    let player_uuid: [u8; 16] = uuid.as_slice().try_into().unwrap();
    let saved_player = saved_players()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&player_uuid)
        .cloned();
    let (saved_dim, sx, sy, sz, syaw, spitch) = saved_player
        .as_ref()
        .map(|save| save.position)
        .or_else(|| {
            pos_store()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&name)
                .copied()
        })
        .unwrap_or_else(|| {
            let w = *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner());
            let (x, y, z, yaw) = resolve_safe_world_spawn_pos(DimId::Overworld, w);
            (DimId::Overworld, x, y, z, yaw, 0.0)
        });
    let player_eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
    let initial_gm = saved_player.as_ref().map_or_else(
        default_game_mode,
        |save| save.gamemode,
    );
    let initial_held = saved_player
        .as_ref()
        .map_or(0, |save| save.held.clamp(0, 8));
    if send_packet(&mut s, 0x2b, join_game(saved_dim, initial_gm, player_eid)).is_err() {
        return None;
    }
    // vanilla `PlayerList.placeNewPlayer` follows Join Game with
    // `ClientboundServerDataPacket`: the MOTD component plus the optional
    // server icon (raw PNG bytes when `server-icon.png` validated).
    {
        let motd = std::env::var("MEGU_MOTD").unwrap_or_else(|_| "megu".to_owned());
        let icon = if status_favicon().is_some() {
            std::fs::read("server-icon.png").ok()
        } else {
            None
        };
        let _ = send_packet(
            &mut s,
            0x4b,
            encode_play_server_data(&motd, icon.as_deref()).1,
        );
    }
    send_view_distance(&mut s, client_view.clamp(2, live_view_distance()));
    let permission_level = verified_permission_level(&uuid_text(&uuid), &name, authenticated);
    if send_commands_at_level(&mut s, permission_level).is_err() {
        return None;
    }
    let _ = send_packet(&mut s, 0x53, vec![initial_held as u8]);
    let mut vp = Vec::new();
    write_varint(&mut vp, i64::from((sx.floor() as i32).div_euclid(16)));
    write_varint(&mut vp, i64::from((sz.floor() as i32).div_euclid(16)));
    let _ = send_packet(&mut s, 0x54, vp);
    let spawn = *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    let _ = send_packet(&mut s, 0x56, spawn_position_packet(spawn));
    let mut gs = vec![13u8];
    gs.extend_from_slice(&0f32.to_be_bytes());
    let _ = send_packet(&mut s, 0x22, gs);
    if RAINING.load(Ordering::Relaxed) > 0 {
        let mut rp = vec![1u8];
        rp.extend_from_slice(&0f32.to_be_bytes());
        let _ = send_packet(&mut s, 0x22, rp);
        if THUNDERING.load(Ordering::Relaxed) > 0 {
            let mut tp = vec![8u8];
            tp.extend_from_slice(&1f32.to_be_bytes());
            let _ = send_packet(&mut s, 0x22, tp);
        }
    }
    let mut sent: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
    let mut gen: GenSources = GenSources::new();
    let view = client_view.clamp(2, live_view_distance());
    let mut chunk_ack = connection_limits::chunk_feedback(window_batch(view));
    let mut chunk_delivery = connection_limits::ChunkDelivery::default();
    let initial_window = initial_chunk_window(sx, sz);
    // vanilla: a returning player resumes where they logged out — the chunk window,
    // spawn teleport and authoritative pos all agree. The dimension was read above,
    // before Join Game, and is threaded through the first stream as well. The join
    // lane starts occluded unless the saved position is already underground.
    let initial_occluded = should_occlude_underground(
        saved_dim,
        sy,
        height_at_dim(saved_dim, sx.floor() as i32, sz.floor() as i32),
        initial_gm,
        false,
    );
    stream_chunks(
        &mut s,
        &mut sent,
        &mut gen,
        &mut chunk_ack,
        saved_dim,
        initial_window,
        syaw,
        true,
        Some(&mut chunk_delivery),
        initial_occluded,
        initial_gm != 3 || gamerule_live_value("spectatorsGenerateChunks") != 0,
    );
    let mut tm = Vec::new();
    tm.extend_from_slice(&0i64.to_be_bytes());
    tm.extend_from_slice(&6000i64.to_be_bytes());
    let _ = send_packet(&mut s, 0x64, tm);
    let mut teleport = connection_limits::TeleportSync::default();
    let initial_id = teleport.begin();
    send_prepared_teleport(
        &mut s,
        &mut teleport,
        (sx, sy, sz, syaw, spitch),
        initial_id,
    );
    let _ = send_packet(&mut s, 0x38, player_abilities_payload(initial_gm, false));
    let _ = send_packet(&mut s, 0x1f, permission_event(player_eid, permission_level));
    let _ = send_packet(&mut s, 0x0b, vec![DIFFICULTY.load(Ordering::Relaxed), 0]);
    // `GameRules.sendGameRulesToPlayer` on `PlayerList.placeNewPlayer`: entity
    // event 22 on the joining player's own entity turns reduced F3 debug on,
    // 23 restores the full readout.
    let debug_status: i8 = if gamerule_live_value("reducedDebugInfo") != 0 {
        22
    } else {
        23
    };
    let _ = send_packet(&mut s, 0x1f, entity_event_pkt(player_eid, debug_status));
    let mut xp = Vec::new();
    xp.extend_from_slice(
        &saved_player
            .as_ref()
            .map_or(0.0, |save| save.xp_progress)
            .to_be_bytes(),
    );
    write_varint(
        &mut xp,
        saved_player.as_ref().map_or(0, |save| save.xp_level) as i64,
    );
    write_varint(
        &mut xp,
        saved_player.as_ref().map_or(0, |save| save.xp_total),
    );
    let _ = send_packet(&mut s, 0x5c, xp);
    let mut hp = Vec::new();
    hp.extend_from_slice(
        &saved_player
            .as_ref()
            .map_or_else(|| max_hp_of(&name), |save| save.health)
            .to_be_bytes(),
    );
    write_varint(
        &mut hp,
        saved_player.as_ref().map_or(20, |save| save.food) as i64,
    );
    hp.extend_from_slice(
        &saved_player
            .as_ref()
            .map_or(5.0, |save| save.saturation)
            .to_be_bytes(),
    );
    let _ = send_packet(&mut s, 0x5d, hp);
    // Live mod suite: the armed `megu:mod_sync_v1` (MSY1) manifest rides
    // the play-phase 0x19 custom payload. No-op unless an operator manifest
    // validated at boot (default builds send nothing new).
    live_mod_sync_on_join(&mut s);
    // Create (`mod-suite`): the current kinetic snapshot rides the same
    // play-phase 0x19 channel so late joiners see spinning wheels/shafts.
    live_create_sync_on_join(&mut s);
    // Live mod suite (`mod-suite`): the dynamic `megu:mod_registry_sync_v1`
    // (`MRS1`) block-state/item registry rides the same channel so clients
    // can name modded ids from the first chunk packet. No-op on vanilla.
    live_mod_registry_sync_on_join(&mut s);
    if let Some(rp) = resource_pack_pkt() {
        let _ = send_packet(&mut s, 0x46, rp);
    }
    // ---- configured session packets (all opt-in; a default server sends
    // none of them) ----
    // `cookie_request`/`store_cookie`: `MEGU_COOKIE_REQUEST` is a `;`-list
    // of keys to query and `MEGU_STORE_COOKIE` a `;`-list of `key=value`
    // pairs to persist — the answers land on the 0x11 cookie_response branch.
    if let Ok(keys) = std::env::var("MEGU_COOKIE_REQUEST") {
        for key in keys.split(';').map(str::trim).filter(|k| !k.is_empty()) {
            let _ = send_play_cookie_request(&mut s, key);
        }
    }
    if let Ok(pairs) = std::env::var("MEGU_STORE_COOKIE") {
        for pair in pairs.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            if let Some((key, value)) = pair.split_once('=') {
                let _ = send_play_store_cookie(&mut s, key, value.as_bytes());
            }
        }
    }
    // `custom_report_details` / `server_links` ride the play phase when
    // configured, the same as a mid-session vanilla push.
    if let Ok(details) = std::env::var("MEGU_REPORT_DETAILS") {
        let entries: Vec<(&str, &str)> = details
            .split(';')
            .filter_map(|pair| pair.split_once('='))
            .collect();
        if !entries.is_empty() {
            let (_, payload) = encode_play_custom_report_details(&entries);
            let _ = send_packet(&mut s, 0x7a, payload);
        }
    }
    if let Ok(links) = std::env::var("MEGU_SERVER_LINKS") {
        // `builtin:<id>=<url>` or `<text>=<url>`; text labels ride a component
        // built by the same `text_component_nbt` as /title.
        let mut parsed: ParsedServerLinks<'_> = Vec::new();
        for entry in links.split(';').map(str::trim).filter(|e| !e.is_empty()) {
            let Some((label, url)) = entry.split_once('=') else {
                continue;
            };
            if let Some(builtin) = label
                .strip_prefix("builtin:")
                .and_then(|v| v.trim().parse::<u32>().ok())
            {
                parsed.push((Some(builtin), None, url));
            } else {
                parsed.push((None, Some(text_component_nbt(label)), url));
            }
        }
        if !parsed.is_empty() {
            let wire: Vec<(PlayServerLinkLabel<'_>, &str)> = parsed
                .iter()
                .filter_map(|(builtin, custom, url)| match (builtin, custom) {
                    (Some(builtin), _) => Some((PlayServerLinkLabel::BuiltIn(*builtin), *url)),
                    (_, Some(nbt)) => Some((PlayServerLinkLabel::Custom(nbt.as_slice()), *url)),
                    _ => None,
                })
                .collect();
            let (_, payload) = encode_play_server_links(&wire);
            let _ = send_packet(&mut s, 0x7b, payload);
        }
    }
    // `PlayerList.placeNewPlayer` recipe-book order: UpdateRecipes, then the
    // recipe-book INIT carrying the player's settings and highlights; the
    // tab-list header/footer pair is opt-in like Vanilla's dedicated server
    // files rather than the single `header` key some forks expose.
    {
        let book = player_recipe_book(player_uuid);
        let _ = send_packet(
            &mut s,
            0x77,
            encode_update_recipes(&update_recipes_entries()),
        );
        let init = UpdateRecipesBook {
            action: RecipeBookAction::Init,
            settings: book.settings,
            recipes: book.unlocked.iter().cloned().collect(),
            to_highlight: book.to_highlight(),
        };
        let _ = send_packet(&mut s, 0x41, encode_update_recipes_book(&init));
        if let Ok(header) = std::env::var("MEGU_TABLIST_HEADER") {
            let footer = std::env::var("MEGU_TABLIST_FOOTER").unwrap_or_default();
            let _ = send_packet(&mut s, 0x6d, encode_tab_list(&header, &footer));
        }
    }
    // restore saved inventory for this name
    let saved_inv = saved_player
        .as_ref()
        .map(|save| save.inventory.clone())
        .or_else(|| {
            inv_store()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&name)
                .cloned()
        });
    eprintln!(
        "[spine] {} joined from {} (uuid {})",
        name,
        peer,
        hex(&uuid)
    );
    // ---- multiplayer: register self, tell others, learn others ----
    let Ok(writer) = s.try_clone() else {
        return None;
    };
    // The play writer is a second fd on the same socket; the cipher armed on
    // `s` during the encryption handshake must cover it or broadcasts to this
    // client would leave in the clear.
    if let Some(cipher) = conn_cipher(&s) {
        wire_state_or_insert(socket_key(&writer))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cipher = Some(cipher);
    }
    sync_player_flying(player_eid, initial_gm, None);
    let me = Arc::new(Client {
        eid: player_eid,
        uuid: player_uuid,
        name: name.clone(),
        player_attack_damage_id: player_attack_damage_wire_id(&cfg),
        pos: Mutex::new((sx, sy, sz, syaw, spitch, true)),
        teleport: Mutex::new(teleport),
        hp: Mutex::new(saved_player.as_ref().map_or(20.0, |save| save.health)),
        sock: Mutex::new(writer),
        inv: Mutex::new(saved_inv.unwrap_or_default()),
        cursor: Mutex::new((0, 0, Vec::new())),
        drag: Mutex::new((-1, Vec::new())),
        open_chest: Mutex::new(None),
        chest_sync: Mutex::new(None),
        open_furnace: Mutex::new(None),
        craft: Mutex::new(HashMap::new()),
        craft9: Mutex::new(HashMap::new()),
        open_table: Mutex::new(false),
        #[cfg(test)]
        open_merchant: Mutex::new(false),
        #[cfg(test)]
        trade_sel: Mutex::new(-1),
        #[cfg(test)]
        merchant_in: Mutex::new(Default::default()),
        bow_draw: Mutex::new(None),
        dig: Mutex::new(None),
        eat_start: Mutex::new(None),
        blocking: Mutex::new(false),
        sprinting: Mutex::new(false),
        sneaking: Mutex::new(false),
        last_melee: Mutex::new(Instant::now() - Duration::from_secs(10)),
        fall_fly: Mutex::new(false),
        invulnerable: Mutex::new(0),
        last_hurt: Mutex::new(0.0),
        shield_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
        absorb: Mutex::new(saved_player.as_ref().map_or(0.0, |save| save.absorption)),
        pearl_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
        seen: Mutex::new(HashSet::new()),
        chunks: Mutex::new(HashSet::new()),
        chunk_ack: Mutex::new(chunk_ack),
        gen_sources: Mutex::new(gen),
        xp: Mutex::new(saved_player.as_ref().map_or(0, |save| save.xp_total)),
        gm: Mutex::new(initial_gm),
        fall_from: Mutex::new(sy),
        food: Mutex::new(saved_player.as_ref().map_or(20, |save| save.food)),
        air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        armor_pts: Mutex::new(0),
        sat: Mutex::new(saved_player.as_ref().map_or(5.0, |save| save.saturation)),
        walk_acc: Mutex::new(saved_player.as_ref().map_or(0.0, |save| save.exhaustion)),
        lastp: Mutex::new((sx, sy, sz)),
        spawn_pt: Mutex::new(saved_player.as_ref().and_then(|save| {
            save.respawn.map(|point| {
                (
                    point.position.0,
                    point.position.1,
                    point.position.2,
                    point.angle,
                )
            })
        })),
        dim: Mutex::new(saved_dim),
        view: Mutex::new(view),
        spawn_dim: Mutex::new(
            saved_player
                .as_ref()
                .and_then(|save| save.respawn.map(|point| point.dim))
                .unwrap_or(saved_dim),
        ),
        in_portal: Mutex::new(false),
        portal_at: Mutex::new(None),
        skin: Mutex::new(login_skin.clone()),
        held: Mutex::new(initial_held),
        effs: Mutex::new(saved_player.as_ref().map_or_else(Vec::new, |save| {
            save.effects
                .iter()
                .map(|&(id, amp, left)| Eff { id, amp, left })
                .collect()
        })),
        lod_v1_enabled: AtomicBool::new(false),
        underground_revealed: AtomicBool::new(!initial_occluded),
        sleeping: Mutex::new(None),
        inv_state_id: AtomicI32::new(0),
        generic_kill_pending: AtomicBool::new(false),
        use_on_resend: Mutex::new(None),
    });
    // Published to the panic cleanup in `handle` before the client list sees
    // the player, so a panic anywhere below still unregisters this eid.
    live_eid.store(me.eid, Ordering::SeqCst);
    if authenticated {
        mark_authenticated(me.eid);
    }
    let retry_chunks = chunk_delivery.has_pending();
    if !me.enter_play_wire(compression, chunk_delivery) {
        return None;
    }
    // `PlayerList.placeNewPlayer` -> `inventoryMenu.sendAllDataToRemote`: the
    // joining client gets one full window-0 SetContainerContent whose stateId
    // consumes the first increment, so the first slot echo carries state 2.
    resend_container_content(&me, 0);
    {
        let border = world_border().lock().unwrap_or_else(|e| e.into_inner());
        let board = scoreboard().lock().unwrap_or_else(|e| e.into_inner());
        let mut tracker = player_advancements()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        restore_player_advancements(&mut tracker, &me.name);
        {
            let mut w = me.sock.lock().unwrap_or_else(|e| e.into_inner());
            let now = border_now_ms();
            let mut snapshot = *border;
            snapshot.finish_lerp(now);
            let (id, body) = snapshot.initialize_packet(now);
            let _ = send_packet(&mut w, id, body);
            let _ = sync_scoreboard(&mut w, &board);
        }
        send_advancement_delta(&mut tracker, &me, true);
        player_mechanism_sessions()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                me.eid,
                PlayerMechanismSession {
                    xp_bar: saved_player
                        .as_ref()
                        .map_or((0, 0.0), |save| (save.xp_level, save.xp_progress)),
                    food_timer: 0,
                    menus: Arc::new(LiveMenuConnection {
                        active: Mutex::new(None),
                        next_id: std::sync::atomic::AtomicU8::new(4),
                        enchantment_seed: Mutex::new(
                            saved_player_enchantment_seed(&me.uuid).unwrap_or_else(|| {
                                i32::from_be_bytes(me.uuid[..4].try_into().unwrap())
                            }),
                        ),
                    }),
                },
            );
    }
    if let Some(save) = saved_player.as_ref() {
        chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ender_container_key(&me.uuid), save.ender_chest.clone());
        for &(id, amp, left) in &save.effects {
            let _ = send_packet(
                &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                0x76,
                entity_effect_pkt(me.eid, id, amp, left),
            );
        }
    }
    // Java sees the login before the tick thread can snapshot this player.
    #[cfg(feature = "neoforge-events")]
    if java_compat::NEOFORGE.get().is_some() {
        let _ = neoforge_event(
            &me,
            java_compat::S1Event::PlayerLogin(me.eid, me.name.clone()),
        );
    }
    {
        // Match bossbar command lock order so a joining member receives one Add.
        let bars = bossbars().lock().unwrap_or_else(|e| e.into_inner());
        let mut cls = clients().lock().unwrap_or_else(|e| e.into_inner());
        *me.chunks.lock().unwrap_or_else(|e| e.into_inner()) = sent;
        for c in cls.iter() {
            // tab-list info is global (vanilla keeps it world-wide);
            // the ENTITY spawn is interest-gated — the sweep sends it
            // only once the other player enters visible range.
            let _ = send_packet(
                &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                0x3e,
                player_info_add(c),
            );
            let pi = player_info_add(&me);
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x3e, pi.clone());
            }
        }
        cls.push(me.clone());
        // Vanilla `PlayerList.placeNewPlayer` chat completions: the joiner
        // gets `add` for every online name, everyone else `add` for theirs.
        let names: Vec<String> = cls.iter().map(|c| c.name.clone()).collect();
        let (_, all_names) = encode_play_custom_chat_completions(0, &names);
        let _ = send_packet(
            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
            0x18,
            all_names,
        );
        let (_, joined_name) =
            encode_play_custom_chat_completions(0, std::slice::from_ref(&me.name));
        for c in cls.iter().filter(|c| c.eid != me.eid) {
            let _ = send_packet(
                &mut c.sock.lock().unwrap_or_else(|e| e.into_inner()),
                0x18,
                joined_name.clone(),
            );
        }
        for (id, bar) in bars.iter().filter(|(_, bar)| bar.shown_to(&me)) {
            let _ = send_packet(
                &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                0x0a,
                bossbar_pkt(id, 0, Some(bar)),
            );
        }
    }
    // Armed the moment the client is on the online list: the guard's `Drop`
    // runs the whole disconnect teardown when this function's scope ends —
    // a clean `'conn` break and a panic unwind take the same path. `handle`'s
    // `forget_live_client` then sweeps anything a mid-unwind teardown missed.
    let disconnect_guard = ConnectionCleanupGuard::new(me.clone(), &peer, login);
    // An operator change can happen during the initial chunk stream, before
    // the client is present in the online list used by /op and /reload.
    if permission_level != client_permission_level(&me) {
        update_client_commands(&me);
    }
    // A forwarded or session-verified skin is already on `me`; only run the
    // offline Mojang lookup when login left the player skinless.
    if login_skin.is_none() {
        kick_skin(name.clone(), &me);
    }
    // refresh armor points from restored inventory
    {
        let iv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        *me.armor_pts.lock().unwrap_or_else(|e| e.into_inner()) = armor_pts_of(&iv);
    }
    // Answers that landed during the configuration phase carry over: a
    // requested cookie response keyed by name and a pong probe keyed by eid,
    // matching how the play-phase `0x11`/`0x27` handlers store them.
    if !link.cookies.is_empty() {
        client_cookies()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(me.eid, std::mem::take(&mut link.cookies));
    }
    if let Some(pong) = link.last_pong {
        last_client_pongs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(me.eid, pong);
    }
    send_attrs(&me);
    // echo restored inventory slots so the client sees them
    {
        let inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        for (&slot, st) in inv.iter() {
            let mut p = vec![0u8];
            write_varint(&mut p, i64::from(inv_state_next(&me)));
            p.extend_from_slice(&slot.to_be_bytes());
            p.extend_from_slice(&slot_wire(st));
            let _ = send_packet(
                &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                0x15,
                p,
            );
        }
    }
    {
        let ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
        // mobs/drops stream via interest_tick (distance-gated)
        drop(ms);
        // drops/xp/mobs stream via interest_tick (distance-gated)
    }
    // Opt-in minimap fair-play (`MEGU_FAIR_PLAY` or `/megu policy`
    // `minimap_mode`, default off): the control code goes out once here, on
    // play join. Off by default — nothing is sent unless the operator armed
    // the flag or restricted the map.
    {
        let policy = map_fair_play_policy();
        if policy.fair_play {
            eprintln!(
                "[spine] {} map fair-play on: cave_map={} entity_radar={}",
                me.name, policy.allow_cave_map, policy.allow_entity_radar
            );
        }
        for chat in map_fair_play_join_chats(&policy) {
            let mut sock = me.sock.lock().unwrap_or_else(|e| e.into_inner());
            let _ = send_packet(&mut sock, 0x6c, chat);
        }
    }
    Some(PlaySession {
        s,
        me,
        rx,
        compression,
        retry_chunks,
        view,
        initial_held,
        disconnect_guard,
        cipher_guard,
        compression_registration,
    })
}

fn run_play_session(session: PlaySession) {
    let PlaySession {
        mut s,
        me,
        mut rx,
        compression,
        mut retry_chunks,
        mut view,
        initial_held,
        disconnect_guard: _disconnect_guard,
        cipher_guard: _cipher_guard,
        compression_registration: _compression_registration,
    } = session;
    // ---- consume loop ----
    s.set_write_timeout(Some(Duration::from_secs(2))).ok();
    let mut keepalive = connection_limits::KeepaliveState::new(Instant::now());
    let mut next_keepalive_id = 1i64;
    // The instant the outstanding keepalive challenge went out, mirrored so a
    // matching reply can be turned into a round-trip measurement.
    let mut keepalive_sent_at: Option<Instant> = None;
    let mut held_slot: i16 = initial_held;
    // `EntityEquipment.setChanged` baseline: every consumed packet may touch
    // the menu (hotbar switch, armor drag, plugin write), so the loop head
    // diffs the live view and ships one sparse SetEquipment.
    let mut last_equipment = player_equipment(&me);

    // Reuse the configuration-phase reader: a pipelining client can already
    // have play packets buffered behind its `0x03` finish acknowledgement,
    // and a fresh buffer would silently drop them.
    // Play-state packet inactivity is tracked separately from the keep-alive deadline.
    let mut last_rx = Instant::now();
    let mut chat_throttle = connection_limits::ChatThrottle::new(Instant::now());
    let mut packet_rate = connection_limits::PacketRateLimit::new(Instant::now());
    let mut move_guard = ingress_guard::MovementFloodGuard::new();
    let mut read_timeout = None;
    let mut last_chunk_retry_tick = TICK_IDX.load(Ordering::Relaxed);
    'conn: loop {
        drop_player_items_on_death(&me);
        {
            let equipment_now = player_equipment(&me);
            if let Some(packet) =
                encode_set_equipment_changes(me.eid, &last_equipment, &equipment_now)
            {
                last_equipment = equipment_now;
                broadcast_seen_packet(me.eid, 0x5b, &packet);
            }
        }
        use connection_limits::KeepaliveAction;
        match keepalive.tick(Instant::now(), next_keepalive_id) {
            KeepaliveAction::Send(id) => {
                if send_packet(
                    &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                    0x26,
                    id.to_be_bytes().to_vec(),
                )
                .is_err()
                {
                    break 'conn;
                }
                // vanilla `ServerGamePacketListenerImpl.tick` also probes the
                // client with ClientboundPingPacket on its connection-alive
                // cadence; the C2S 0x27 answer lands on `last_client_pongs`.
                if send_packet(
                    &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                    0x35,
                    encode_play_ping(id as i32).1,
                )
                .is_err()
                {
                    break 'conn;
                }
                keepalive_sent_at = Some(Instant::now());
                next_keepalive_id = next_keepalive_id.wrapping_add(1);
            }
            KeepaliveAction::TimedOut => {
                play_disconnect(
                    &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                    "Timed out",
                );
                break 'conn;
            }
            KeepaliveAction::Wait => {}
        }
        // ACKs and movement can wake delivery immediately. An eligible backlog
        // gets another chance on the next tick, even during continuous input.
        let tick = TICK_IDX.load(Ordering::Relaxed);
        if retry_chunks && tick != last_chunk_retry_tick {
            let __p0 = Instant::now();
            retry_chunks = pump_client_chunks(&me, false);
            let __pe = __p0.elapsed();
            if conn_prof() && __pe > std::time::Duration::from_millis(50) {
                eprintln!("[conn-prof] pump_client_chunks {:?}", __pe);
            }
            last_chunk_retry_tick = tick;
        }
        let timeout = if retry_chunks {
            Duration::from_millis(TICK_MS)
        } else {
            Duration::from_secs(2)
        };
        if read_timeout != Some(timeout) {
            s.set_read_timeout(Some(timeout)).ok();
            read_timeout = Some(timeout);
        }
        // A client that stopped answering chunk batches is gone, even on an idle socket.
        if me
            .chunk_ack
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .timed_out(Instant::now())
        {
            eprintln!(
                "[spine] {} disconnected: chunk batch acknowledgement timeout",
                me.name
            );
            break 'conn;
        }
        // A datapack publish that carries synced-registry content re-enters
        // the configuration phase mid-play (`0x69`), the same lane vanilla's
        // `startConfiguration` re-entry takes.
        if reconfigure_pending()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&me.eid)
        {
            eprintln!(
                "[spine] {} re-entering configuration after content revision",
                me.name
            );
            if !play_reconfigure_exchange(
                &me,
                &mut s,
                &mut rx,
                compression,
                &mut packet_rate,
                &mut view,
            ) {
                break 'conn;
            }
            // Vanilla's fresh common listener restarts the keep-alive clock on
            // re-entry; the exchange just consumed every in-flight answer.
            keepalive = connection_limits::KeepaliveState::new(Instant::now());
            keepalive_sent_at = None;
            last_rx = Instant::now();
            read_timeout = None;
        }
        let (pid, b) = match rx.next(&mut s) {
            RxOut::Closed => break 'conn,
            RxOut::Idle => {
                let idle_limit = connection_limits::idle_timeout();
                if !idle_limit.is_zero() && last_rx.elapsed() > idle_limit {
                    eprintln!(
                        "[spine] {} disconnected: no packet for {:?}",
                        me.name, idle_limit
                    );
                    play_disconnect(
                        &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                        "Timed out",
                    );
                    break 'conn;
                }
                // A dimension change initiated by another connection can
                // create a backlog even while this connection is idle.
                let tick = TICK_IDX.load(Ordering::Relaxed);
                if tick != last_chunk_retry_tick {
                    let __p0 = Instant::now();
                    retry_chunks = pump_client_chunks(&me, false);
                    let __pe = __p0.elapsed();
                    if conn_prof() && __pe > std::time::Duration::from_millis(50) {
                        eprintln!("[conn-prof] idle pump_client_chunks {:?}", __pe);
                    }
                    last_chunk_retry_tick = tick;
                }
                continue;
            }
            RxOut::Frame(pid, b) => {
                last_rx = Instant::now();
                (pid, b)
            }
        };
        drop_player_items_on_death(&me);

        // Count every decoded Play packet before any downstream parsing or
        // dispatch: a movement flood otherwise multiplies tracker fanout.
        let received_at = Instant::now();
        if !packet_rate.allow(received_at) {
            if packet_rate.should_disconnect(received_at) {
                eprintln!(
                    "[spine] {} disconnected after {} rate-limited play packets",
                    me.name,
                    packet_rate.dropped()
                );
                play_disconnect(
                    &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                    "Packet rate limit exceeded",
                );
                break 'conn;
            }
            continue 'conn;
        }

        // vanilla refuses a movement update whose position is not finite or is
        // outside the world; letting it through carries the value into worldgen,
        // visibility and every float-to-int conversion behind them.
        if !ingress_guard::valid_movement_frame(pid, &b) {
            eprintln!("[spine] {} disconnected: malformed movement frame", me.name);
            play_disconnect(
                &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                "Invalid move player packet received",
            );
            break 'conn;
        }

        let __d1 = Instant::now();
        let control = {
            let mut cx = PlayPacketContext {
                me: &me,
                retry_chunks: &mut retry_chunks,
                keepalive: &mut keepalive,
                keepalive_sent_at: &mut keepalive_sent_at,
                view: &mut view,
                held_slot: &mut held_slot,
                chat_throttle: &mut chat_throttle,
                move_guard: &mut move_guard,
            };
            match pid {
                0x00 => play_c2s_teleport_confirm(&mut cx, &b),
                0x18 => play_c2s_keepalive(&mut cx, &b),
                0x02 => play_c2s_change_difficulty(&mut cx, &b),
                0x08 => play_c2s_chunk_batch_received(&mut cx, &b),
                0x0a => play_c2s_client_information(&mut cx, &b),
                id if id == i64::from(brigadier_wire::SERVERBOUND_COMMAND_SUGGESTION) => {
                    play_c2s_command_suggestion(&mut cx, &b)
                }
                0x1a | 0x1b => play_c2s_player_movement(&mut cx, pid, &b),
                0x1c => play_c2s_player_rotation(&mut cx, &b),
                0x25 => play_c2s_entity_action(&mut cx, &b),
                0x2f => play_c2s_held_item_slot(&mut cx, &b),
                0x20 => play_c2s_pick_item(&mut cx, &b),
                0x1d => play_c2s_player_flying(&mut cx, &b),
                0x1e => play_c2s_move_vehicle(&mut cx, &b),
                0x1f => play_c2s_paddle_boat(&mut cx, &b),
                0x26 => play_c2s_player_input(&mut cx, &b),
                0x24 => play_c2s_world_interaction(&mut cx, pid, &b),
                0x38 => play_c2s_world_interaction(&mut cx, pid, &b),
                0x36 => play_c2s_arm_animation(&mut cx, &b),
                0x39 => play_c2s_use_item_for_hand(&mut cx, &b),
                0x04 | 0x05 => play_c2s_chat_command(&mut cx, &b),
                0x06 => play_c2s_chat_message(&mut cx, &b),
                0x12 => play_c2s_custom_payload(&mut cx, &b),
                0x16 => play_c2s_use_entity(&mut cx, &b),
                0x28 => play_c2s_recipe_book_change_settings(&mut cx, &b),
                0x29 => play_c2s_recipe_book_seen_recipe(&mut cx, &b),
                0x22 => play_c2s_place_recipe(&mut cx, &b),
                0x09 => play_c2s_client_command(&mut cx, &b),
                0x2b => play_c2s_resource_pack_status(&mut cx, &b),
                0x35 => play_c2s_update_sign(&mut cx, &b),
                0x32 => play_c2s_set_creative_slot(&mut cx, &b),
                0x0d => play_c2s_container_button_click(&mut cx, &b),
                0x2a => play_c2s_anvil_rename(&mut cx, &b),
                0x2d => play_c2s_select_trade(&mut cx, &b),
                0x2e => play_c2s_set_beacon_effect(&mut cx, &b),
                0x0f => play_c2s_close_window(&mut cx, &b),
                0x0e if b.len() >= 6 => play_c2s_window_click(&mut cx, &b),
                0x01 => play_c2s_block_entity_tag_query(&mut cx, &b),
                0x03 => play_c2s_chat_ack(&mut cx, &b),
                0x07 => play_c2s_chat_session_update(&mut cx, &b),
                0x0c => play_c2s_configuration_acknowledged(&mut cx, &b),
                0x10 => play_c2s_container_slot_state_changed(&mut cx, &b),
                0x11 => play_c2s_cookie_response(&mut cx, &b),
                0x13 => play_c2s_debug_sample_subscription(&mut cx, &b),
                0x14 => play_c2s_edit_book(&mut cx, &b),
                0x15 => play_c2s_entity_tag_query(&mut cx, &b),
                0x17 => play_c2s_jigsaw_generate(&mut cx, &b),
                0x19 => play_c2s_lock_difficulty(&mut cx, &b),
                0x21 => play_c2s_ping_request(&mut cx, &b),
                0x23 => play_c2s_player_abilities(&mut cx, &b),
                0x27 => play_c2s_pong(&mut cx, &b),
                0x2c => play_c2s_seen_advancements(&mut cx, &b),
                0x30 => play_c2s_set_command_block(&mut cx, &b),
                0x31 => play_c2s_set_command_minecart(&mut cx, &b),
                0x33 => play_c2s_set_jigsaw_block(&mut cx, &b),
                0x34 => play_c2s_set_structure_block(&mut cx, &b),
                0x37 => play_c2s_teleport_to_entity(&mut cx, &b),
                _ => PlayPacketControl::Handled,
            }
        };
        match control {
            PlayPacketControl::Handled => {}
            PlayPacketControl::Skip => continue,
            PlayPacketControl::Break => break 'conn,
        }
        let __dd = __d1.elapsed();
        if conn_prof() && __dd > std::time::Duration::from_millis(50) {
            eprintln!("[conn-prof] dispatch pid={pid:#x} {:?}", __dd);
        }
        let __d0 = Instant::now();
        apply_queued_block_updates();
        let __de = __d0.elapsed();
        if conn_prof() && __de > std::time::Duration::from_millis(50) {
            eprintln!("[conn-prof] post-dispatch flush pid={pid:#x} {:?}", __de);
        }
    }
    // The `_disconnect_guard` armed at registration runs `disconnect_cleanup`
    // here — and on any panic unwind — so every exit takes the same teardown.
}
