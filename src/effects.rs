use super::*;

pub(super) fn tick_effects(c: &Client) {
    adv_event(&c.name, "tick", None);
    let mut ef = c.effs.lock().unwrap();
    if ef.is_empty() { return }
    let mut dirty2 = false;
    let mut expired: Vec<i32> = Vec::new();
    let mut grant_totem = false;
    for e in ef.iter_mut() {
        e.left -= 1;
        if *c.gm.lock().unwrap() == 1 { if e.left <= 0 { expired.push(e.id); } continue }
        let mut h = c.hp.lock().unwrap();
        match e.id {
            9 => { let iv = (server_ticks(50) as i32 >> (e.amp.clamp(0, 6) as u32)).max(1); if e.left % iv == 0 { let mh = max_hp_of(&c.name); *h = (*h + 1.0).min(mh); dirty2 = true; } }
            18 => { let iv = (server_ticks(25) as i32 >> (e.amp.clamp(0, 6) as u32)).max(1); if e.left % iv == 0 { *h = (*h - 1.0).max(1.0); dirty2 = true; } }
            19 => { let iv = (server_ticks(40) as i32 >> (e.amp.clamp(0, 6) as u32)).max(1); if e.left % iv == 0 { *h = (*h - 1.0).max(0.0); dirty2 = true; }
                if *h <= 0.0 {
                    // The effect guard is held here; do not re-lock it through totem_check.
                    let mut inv2 = c.inv.lock().unwrap();
                    let hk = 36i16 + (*c.held.lock().unwrap()) as i16;
                    let mut saved = false;
                    let mut saved_k = -1i16;
                    let mut saved_now = (0i64, 0u8, Vec::new());
                    for k in [45i16, hk] {
                        if inv2.get(&k).map(|v| v.0) == Some(1163) {
                            let st = inv2.get(&k).unwrap().clone();
                            if st.1 <= 1 { inv2.remove(&k); } else { inv2.insert(k, (st.0, st.1 - 1, st.2)); }
                            saved = true; saved_k = k;
                            saved_now = inv2.get(&k).cloned().unwrap_or((0, 0, Vec::new()));
                            break
                        }
                    }
                    drop(inv2);
                    if saved {
                        *h = 1.0;
                        grant_totem = true;
                        resync_slot(c, 0, saved_k, &saved_now);
                        let mut st_pkt = Vec::new();
                        st_pkt.extend_from_slice(&c.eid.to_be_bytes());
                        st_pkt.push(35);
                        let _ = send_packet(&mut *c.sock.lock().unwrap(), 0x1f, st_pkt.clone());
                        broadcast_seen(c.eid, |_c| (0x1f, st_pkt.clone()));
                        let (px, py, pz, ..) = { let p = c.pos.lock().unwrap(); *p };
                        let snd = sound_pkt_named("item.totem.use", px, py, pz);
                        broadcast_seen(c.eid, |_c| (0x68, snd.clone()));
                    }
                } }
            16 => { drop(h); let mut f = c.food.lock().unwrap();
                if e.left % server_ticks(80) as i32 == 0 && *f > 0 { *f -= 1; dirty2 = true; }
                h = c.hp.lock().unwrap(); }
            _ => {}
        }
        drop(h);
        if e.left <= 0 { expired.push(e.id); }
        // Death protection removes the entire old set; later entries must not execute.
        if grant_totem { break }
    }
    if dirty2 { if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut *w, 0x5d, health_pkt_c(*c.hp.lock().unwrap(), c)); } }
    for id in &expired {
        ef.retain(|e| e.id != *id);
        if *id == 21 { *c.absorb.lock().unwrap() = 0.0; }
        let pk = remove_effect_pkt(c.eid, *id);
        if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut *w, 0x43, pk.clone()); }
        broadcast_seen(c.eid, |_c| (0x43, pk.clone()));
    }
    // Expired effects belong to the old set, not to the death-protection grant.
    if grant_totem { grant_totem_effects(c, &mut ef); }
}
