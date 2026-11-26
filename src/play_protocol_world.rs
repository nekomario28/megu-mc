fn play_c2s_update_sign(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // update_sign: pos i64, front bool, 4 strings
    if b.len() >= 10 {
        let (sx, sy, sz) = read_pos(&b[..8]);
        let front = if b[8] != 0 { 1u8 } else { 0u8 };
        let mut sl = &b[9..];
        let mut lines: [String; 4] = Default::default();
        // MEGA-W2 §3.1: `ac_sanitize_sign_line` strips control
        // characters for everyone and `§` formatting for
        // non-operators; operators keep legitimate formatting.
        let perm = client_permission_level(&me);
        for l in lines.iter_mut() {
            let n = read_varint(&mut sl).unwrap_or(0).clamp(0, 256) as usize;
            let m = n.min(sl.len());
            *l = ac_sanitize_sign_line(&String::from_utf8_lossy(&sl[..m]), perm).line;
            sl = &sl[m..];
        }
        let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
        let pos = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
        // ServerPlayer's sign edit path checks mayInteract and the
        // eight-block squared distance before changing the block entity.
        if !matches!(*me.gm.lock().unwrap_or_else(|e| e.into_inner()), 0 | 1)
            || !sign_edit_in_range((pos.0, pos.1, pos.2), sx, sy, sz)
            || !is_sign_block_state(block_at_dim(dim, sx, sy, sz))
            // a waxed sign's text is frozen; vanilla drops the edit
            || sign_meta_at(dim, sx, sy, sz).is_waxed
            // Persistent claims: editing sign text modifies the
            // block.
            || !can_player_modify_block(dim, sx, sz, &me.name, perm)
        {
            return PlayPacketControl::Skip;
        }
        signs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((dim, sx, sy, sz, front), lines.clone());
        let sign_pkt = sign_entity_pkt(dim, sx, sy, sz);
        broadcast_chunk_dim_packet(dim, sx.div_euclid(16), sz.div_euclid(16), 0x07, &sign_pkt);
    }
    PlayPacketControl::Handled
}

fn play_c2s_block_entity_tag_query(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // block_entity_tag_query: the OP>=2 debug-stick query; the
    // block's live NBT answers on `tag_query` (S2C 0x6e).
    if let Some((tx_id, pos)) = decode_block_entity_tag_query(b) {
        if client_permission_level(&me) >= 2 {
            let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
            let nbt = live_block_entity_snapshot((dim, pos.0, pos.1, pos.2)).map(|(_, tag)| tag);
            let _ = send_packet(
                &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                0x6e,
                tag_query_reply(tx_id, nbt),
            );
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_edit_book(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    if let Some(book) = decode_edit_book(b) {
        c2s_apply_edit_book(&me, &book);
    }
    PlayPacketControl::Handled
}

fn play_c2s_entity_tag_query(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // entity_tag_query: OP>=2, answers with the target's
    // `id`/`Pos`/`Rotation` NBT on `tag_query` (S2C 0x6e).
    if let Some((tx_id, entity_id)) = decode_entity_tag_query(b) {
        if client_permission_level(&me) >= 2 {
            let _ = send_packet(
                &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                0x6e,
                tag_query_reply(tx_id, entity_tag_nbt(entity_id)),
            );
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_jigsaw_generate(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // jigsaw_generate: OP>=2 + creative; the request is recorded
    // against the block position like a structure edit.
    if let Some((pos, levels, keep_jigsaws)) = decode_jigsaw_generate(b) {
        if sender_gamemaster_permitted(&me) {
            let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
            jigsaw_generate_requests()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((dim, pos.0, pos.1, pos.2), (levels, keep_jigsaws));
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_set_command_block(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // set_command_block: the VANILLA-CMDBLK decoder + apply —
    // OP>=2, creative and `enable-command-block` are all enforced
    // inside `apply_set_command_block`.
    if let Some(update) = decode_set_command_block_c2s(b) {
        let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
        let _ = apply_set_command_block(
            client_permission_level(&me),
            *me.gm.lock().unwrap_or_else(|e| e.into_inner()),
            true,
            dim,
            update,
        );
    }
    PlayPacketControl::Handled
}

fn play_c2s_set_command_minecart(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // set_command_minecart: identical gate under
    // `apply_set_command_minecart`.
    if let Some(update) = decode_set_command_minecart_c2s(b) {
        let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
        let _ = apply_set_command_minecart(
            client_permission_level(&me),
            *me.gm.lock().unwrap_or_else(|e| e.into_inner()),
            true,
            dim,
            update,
        );
    }
    PlayPacketControl::Handled
}

fn play_c2s_set_jigsaw_block(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // set_jigsaw_block: OP>=2 + creative; the edit metadata is
    // recorded against the block position.
    if let Some((pos, update)) = decode_set_jigsaw_block(b) {
        if sender_gamemaster_permitted(&me) {
            let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
            jigsaw_block_updates()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((dim, pos.0, pos.1, pos.2), update);
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_set_structure_block(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // set_structure_block: OP>=2 + creative; same recorded-edit
    // contract as the jigsaw arm.
    if let Some((pos, update)) = decode_set_structure_block(b) {
        if sender_gamemaster_permitted(&me) {
            let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
            structure_block_updates()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((dim, pos.0, pos.1, pos.2), update);
        }
    }
    PlayPacketControl::Handled
}
