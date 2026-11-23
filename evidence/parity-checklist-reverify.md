# M39-M43 re-verification (2026-11-23, second pass)

- M39 (P1-4 real-chunk Fossil acceptance): **still open** — checklist item unchecked.
- M40 (P0-2 zero-substitution PR #32 follow-through): **still open** — checklist states Rust build / full suite / nested-value runs / native compare all unrun.
- M41 (P2-2 configured features + population order): **still open, narrowed** — bonus_chest + end_spike gated (4 mutations), sculk_patch fixed at ebd19fd with 0/0/0 diff; remaining unhandled features and latest population-order sweep continue.
- M42 (spawn_costs + NaturalSpawner real-class comparison): **still open** — P0-2 explicitly lists it as the remaining closure item.
- M43 (protocol matrix): **major narrowing** — `tools/v767_total_parity_audit.py` now reports Play/Clientbound **server-covered 120/124** (was 82/124): only custom_chat_completions, disguised_chat, player_look_at, start_configuration unsent. Configuration/Clientbound 5/17 (cookie_request, disconnect, keep_alive, ping, reset_chat, resource_pack family, store_cookie, transfer, update_tags, custom_report_details, server_links) and Login cookie_request remain. The **client** side is now the bigger gap: 73/124 covered, 52 missing including player_chat, explode, ticking_state/step, update_attributes, commands.
