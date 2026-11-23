# L5/H8 — test-only dead-code triage: combat_and_health.rs + player_mechanisms.rs

Date: 2026-11-23. Method: stripped every `#[cfg_attr(not(test), allow(dead_code))]`
in both files, ran `cargo check --offline --locked -j2`, and recorded the exact
`dead_code` warning set (99 items), then restored and applied only the verified
dispositions below. `cargo check` is warning-free afterwards.

Constraint honoured: only `src/combat_and_health.rs` and
`src/player_mechanisms.rs` were touched. Every production call site that lives
in `src/main.rs`, `src/play_protocol.rs`, `src/worldgen.rs` (or any other file)
is out of bounds this slice, so those fns are **kept with their annotation**
and noted — wiring them is a follow-up lane, not speculative edits here.

## combat_and_health.rs

### (a) Wired — none needed beyond stale-annotation cleanup

### Stale annotations removed (items verified LIVE in non-test build)

These carried `#[cfg_attr(not(test), allow(dead_code))]` but have real
production callers; the attribute was leftover. Removed:

- `WindChargeKind`, `WindChargeProfile`, `wind_charge_profile`,
  `evaluate_wind_charge_burst`, `wind_charge_reset_fall`,
  `wind_charge_trigger_at`, `wind_charge_trigger_blocks`,
  `wind_charge_note_neighbors`, `wind_charge_burst`, `wind_charge_ages`,
  `wind_charge_projectile_step` — all reachable via
  `wind_charge_projectile_step` → `combat2_state_tick` → `arrow_tick` →
  `w1_arrow_tick` (main.rs).

### (c) Kept + documented — test-only, call site in a forbidden file

- `enchantment_targets_aquatic` — only caller is `typed_damage_bonus` (also
  dead). The enchantment damage-bonus composition belongs in the melee attack
  path at `play_protocol.rs` (~line 12405, `wep + sharp + strb + wkb`).
- `typed_damage_bonus` — same call site (play_protocol.rs melee damage).
- `bane_slowness_iv` — Bane of Arthropods post-hit Slowness IV rider; the
  post-attack effect application is in play_protocol.rs.
- `entity_is_living` — feeds `CritContext.target_is_living`; production crits
  go through `melee_critical_allowed` (called from play_protocol.rs:12449)
  which hardcodes `target_is_living: true`. Wiring needs a signature/call-site
  change in play_protocol.rs.
- `melee_hit_damage` — vanilla damage ordering helper; the live damage
  formula is inline in play_protocol.rs.
- `ignite_mob_fire_aspect` — Fire Aspect ignition. Live path writes
  `m.fire_ticks` directly at play_protocol.rs:~12530; this helper targets the
  parallel `MOB_FIRE_TICKS` map.
- `mob_fire_ticks_of` — reader for `MOB_FIRE_TICKS`; its consumer would be
  the mob env tick in main.rs (~line 5758) or a debug/status path.
- `tick_mob_fire` — damage applier for `MOB_FIRE_TICKS`. Production fire
  damage already runs through `m.fire_ticks` + `fire_tick_update` in the
  main.rs mob env tick; wiring this in would double-apply burn. Needs a
  main.rs call-site decision (adopt map vs field), not doable this slice.
- `SWEEP_KNOCKBACK` — sweep impulse magnitude for the `sweeping_attack_gate`/
  `sweeping_movement_ok` design; the live sweep uses `sweeping_attack`
  (player_mechanisms.rs) called from play_protocol.rs:12496.
- `sweeping_attack_gate` — note `charge > 0.9` vs live `attack_strength >=
  0.9` in `sweeping_attack`: NOT behavior-identical, so it was not substituted.
- `sweeping_movement_ok` — needs live horizontal-speed/movement-cap data only
  available at the play_protocol.rs call site.
- `spin_attack_ticks_left` — reader for the `riptide_spin` map (written at
  combat_and_health.rs:~2627 on Riptide launch). The spin-attack consumer
  belongs to the attack path in play_protocol.rs/main.rs. Kept both
  `not(test)` and `not(megu_vanilla_data)` attrs.
- `WindChargeProfile::direct_hit_damage` — written by live
  `wind_charge_profile`, read only by tests. The `onHitEntity` direct-hit
  damage is unimplemented; kept `not(test)` + `not(megu_vanilla_data)` attrs
  on the field (previously covered by the struct-level attr).
- `TotemDamageKind::Kill` — `generic_kill` bypass variant. No production
  constructor exists: `/kill` (commands_execute.rs) routes through `kill_mob`
  and never reaches `try_trigger_totem_of_undying`; the player-kill damage
  source is unimplemented. Kept as taxonomy documentation pinned by
  `vanilla_combat2_totem_void_and_kill_bypass`.

## player_mechanisms.rs

### (a) Wired

- `sweeping_target_in_range` — now called inside `sweeping_attack`
  (same file, live via play_protocol.rs:12496). `search_box` is computed once
  and shared between the eligibility check and the `SweepAttack` field;
  behaviour is bit-identical (`target_box.inflate(1.0, 0.25).intersects(...)`).

### (c) Kept + documented — test-only spec helpers awaiting forbidden call sites

Combat/enchantment evaluators — production equivalents are inline in
main.rs / play_protocol.rs / menus_and_items.rs; refactoring those call sites
is outside this slice:

- `knockback_strength`, `lure_wait_ticks`, `FishingLootWeights`,
  `luck_of_the_seas_weights` (fishing), `thorns_triggers`, `thorns_damage`,
  `respiration_air_ticks`, `respiration_preserves_air`,
  `depth_strider_underwater_factor`, `soul_speed_multiplier`,
  `swift_sneak_speed_multiplier`, `soul_speed_on_soul_block_bonus`,
  `frost_walker_freeze_radius`, `respiration_air_loss_probability`,
  `aqua_affinity_mining_factor`, `feather_falling_damage_reduction`,
  `quick_charge_crossbow_ticks`, `multishot_projectile_angles`,
  `piercing_max_entities`, `density_smash_bonus_damage`,
  `breach_effective_armor_fraction`, `wind_burst_bounce_strength`,
  `riptide_dash_velocity`, `channeling_strikes_lightning`,
  `v767_custom_stat_id`.

Mining cluster — the live dig path uses `mining_speed_with_modifiers`
(menus_and_items.rs:475,496; play_protocol.rs:11008); this is the
vanilla-accurate `getDestroySpeed`/`getDestroyProgress` reimplementation:

- `mining_damage_per_tick`, `instant_mining_break`, `MiningSpeedContext`,
  `mining_tool_speed`, `sword_efficient`, `vanilla_destroy_speed`,
  `vanilla_mining_ticks`, `preferred_mining_tool`.

Menu/XP helpers — consumers live outside the allowed files:

- `SmithingMenu::result` / `take_result` — menus_and_items.rs reimplements
  the same logic via `live_smithing_result` (line ~10405); deduplicating
  needs an edit there.
- `PlayerXpState::query_experience_points` — the `/xp` query path is in
  commands_core.rs.

Game-rule accessors — consumers are the player env tick (main.rs), respawn
logic, crafting checks (menus_and_items.rs) and the advancement broadcast:

- `announce_advancements_enabled`, `player_damage_rule_enabled`,
  `drowning_damage_enabled`, `fall_damage_enabled`, `fire_damage_enabled`,
  `freeze_damage_enabled`, `natural_regeneration_enabled`,
  `eval_environmental_damage`, `do_immediate_respawn_enabled`,
  `do_limited_crafting_enabled`, `eval_limited_crafting_allowed`,
  `spawn_radius_blocks`, `eval_spawn_offset`.

VANILLA-TRIALS lane (trial spawner / vault / brush / copper oxidation) — a
complete spec block whose world-tick and block-tick integration belongs to
main.rs / worldgen.rs:

- `TRIAL_SPAWNER_*` consts, `TrialSpawnerState` (+impl),
  `trial_spawner_reward_item`, `trial_spawner_reward_items`,
  `VAULT_*` consts, `VaultState`, `VaultRefusal`, `VaultReward` (+impl),
  `vault_reward_items`, `BRUSH_*` consts, `BrushableBlockState` (+impl),
  `brush_loot_item`, `CopperOxidationStage` (+impl), `CopperScrape`,
  `COPPER_*` consts, `CopperBlockState` (+impl),
  `copper_lightning_deoxidize_chance`.

## Verification

- `cargo check --offline --locked -j2` — clean, zero warnings.
- `cargo test --release --offline -j2` filtered runs:
  - `p1_combat p5a_parity vanilla_combat2 player_mechanisms p2_parity
    p5e_parity vanilla_trials mega_scale` — 156 passed, 0 failed.
  - `enchant health sweep melee fire_aspect totem knockback` — 54 passed,
    0 failed.

No commits made; changes left uncommitted as instructed.
