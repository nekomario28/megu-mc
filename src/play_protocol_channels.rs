fn play_c2s_keepalive(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    if !cx.keepalive.reply(Instant::now(), b) {
        play_disconnect(
            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
            "Timed out",
        );
        return PlayPacketControl::Break;
    }
    // A reply that matched the pending challenge is a measured
    // round-trip: feed it into the OPS-2 lag-compensation envelope
    // so latency spikes do not read as movement cheats.
    if let Some(sent_at) = cx.keepalive_sent_at.take() {
        let rtt_ms = sent_at.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
        ac_note_player_rtt_ms(me.eid, rtt_ms);
    }
    PlayPacketControl::Handled
}

fn play_c2s_chunk_batch_received(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // Java 1.21.1 Chunk Batch Received: desired chunks per tick, four
    // big-endian f32 bytes. Answering it opens the next batch.
    let acked = {
        let mut ack = me.chunk_ack.lock().unwrap_or_else(|e| e.into_inner());
        ack.acknowledge(b)
    };
    if let Err(why) = acked {
        eprintln!("[spine] {} disconnected: {why}", me.name);
        return PlayPacketControl::Break;
    }
    *cx.retry_chunks = pump_client_chunks(&me, false);
    PlayPacketControl::Handled
}

fn play_c2s_custom_payload(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // A1-SERVER: the airship helm channel is a Create-offload
    // subworld input, not a mod payload — dispatch it before the
    // generic register/lod handler sees it.
    #[cfg(feature = "create-offload")]
    let helm = aeronautics_helm_payload_c2s(&me, b);
    #[cfg(not(feature = "create-offload"))]
    let helm = false;
    if !helm {
        custom_payload_c2s(&me, b);
    }
    PlayPacketControl::Handled
}

fn play_c2s_resource_pack_status(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // resource_pack_receive: status varint
    let mut sl = b;
    let status = read_varint(&mut sl);
    eprintln!("[spine] resource_pack status {status:?}");
    if pack_info().is_none() && status.is_some() {
        // A pack status with nothing advertised means the client
        // still holds a stale pack; `resource_pack_pop` with no id
        // clears the whole stack.
        let _ = send_play_resource_pack_pop(
            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
            None,
        );
    }
    if resource_pack_status_disconnects(status) {
        play_disconnect(
            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
            "This server requires the configured resource pack.",
        );
        return PlayPacketControl::Break;
    }
    PlayPacketControl::Handled
}

fn play_c2s_configuration_acknowledged(
    cx: &mut PlayPacketContext<'_>,
    b: &[u8],
) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // configuration_acknowledged: the client finished a mid-play
    // reconfigure exchange — record it; a stray empty ack is
    // harmless on a server that does not currently reconfigure.
    if b.is_empty() {
        reconfigure_acked()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(me.eid);
    }
    PlayPacketControl::Handled
}

fn play_c2s_cookie_response(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // cookie_response: per-connection store, 5 KiB cap enforced
    // in the decoder.
    if let Some((key, payload)) = decode_cookie_response(b) {
        client_cookies()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(me.eid)
            .or_default()
            .insert(key, payload);
    }
    PlayPacketControl::Handled
}

fn play_c2s_ping_request(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // ping_request: mirror the i64 straight back on `pong_response`
    // (S2C 0x36) — the client's play-phase latency readout.
    if let Some(payload) = decode_ping_request(b) {
        let _ = send_packet(
            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
            0x36,
            payload.to_be_bytes().to_vec(),
        );
    }
    PlayPacketControl::Handled
}

fn play_c2s_pong(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // pong: the client answered a `ClientboundPingPacket`; the id
    // lands on the per-connection ledger.
    if let Some(id) = decode_pong(b) {
        last_client_pongs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(me.eid, id);
    }
    PlayPacketControl::Handled
}
