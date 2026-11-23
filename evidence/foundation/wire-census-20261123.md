# Wire census 2026-11-23 — actual S2C emission surface vs static audit

HEAD: `46c9e62d` (probe ran `target/release/megu` built 11:37; installer-only commits since).

## Method

`tools/` 相当のプローブ(/tmp/wire_census.py,要 promote 候補): megu を使い捨て
workdir で起動(`config_packets.bin`+`server.toml` をコピー、
`MEGU_ANVIL_WORLD_DIR=""`, `MEGU_WG=old`),`tests/protocol767.py` の Client で
login+config+play を全フェーズ計測し、送出 S2C id をヒストグラム化。
表は `proto/src/data_v767.rs` の Play/Clientbound 表と照合。

## Result — static audit false-negative rate is high

`tools/v767_total_parity_audit.py` は Play/Clientbound で **76 missing** を報告するが、
login+chunk+move+chat の単純セッションだけで **29 distinct** の送出を実測
(commands 0x11, initialize_border 0x25, update_advancements 0x74, server_data 0x4b,
recipe 0x41, update_recipes 0x77, set_carried_item 0x53, set_default_spawn_position 0x56,
set_simulation_distance 0x62, set_chunk_cache_radius 0x55, player_abilities 0x38,
change_difficulty 0x0b … — すべて静的監査では missing)。

原因は構造的: 静的監査は `send_packet(s, 0xNN, …)` の字面パターンを探すが、
実装は `commands_payload()`/`entity_teleport()`/`frame()` のような
ペイロードビルダ経由で送出する。**「missing」は≈実装不在ではなくパターン非検出**。

Config フェーズ実測 8 distinct: 0x01 custom_payload, 0x03 finish, 0x07 registry_data×11,
0x0c feature_flags, 0x0d update_tags, 0x0e select_known_packs (+ login 0x02/0x03)。

## Confirmed real gaps (code + wire両方で確認)

| ID | packet | status |
|---|---|---|
| 0x2e/0x2f/0x30 | move_entity_pos / pos_rot / rot | **never sent** — 全モブ位置更新が `entity_teleport` 0x70 (src/main.rs 14+ サイト、combat_and_health.rs も)。vanilla は ≤8ブロック差分を相対移動で送る。帯域 ~3x + 補間 semantics 差 + `rotate_head` 0x48 も未送出で頭 yaw 追従なし |
| 0x04 config keep_alive | config-phase keep_alive/ping | never sent (vanilla は config 滞留中も ping) |
| 0x69 start_configuration | mid-play config re-entry | never sent (resource pack 切替等に必要) |
| 0x0e chunks_biomes | biome update | never sent |

## AUDIT_WIRE P1–P4 recheck on HEAD

- **P1 scores()**: 実質修正済。コマンド文字列 `MAX_CHAT_CHARS=256`、objective毎
  `SCOREBOARD_ENTRY_LIMIT=4096`。残: objectives 総数自体に cap 無し(低速漏洩、~256B/cmd)。
- **P2 sent set**: 実質修正済。`ChunkDelivery::next_batch` が window に `retain` +
  ACKペーシング + tick 予算 (connection_limits.rs:229-265)。
- **P3 capacity pinning**: 未検証(D2 sweep に委譲)。
- **P4 login name**: `chat_text_valid` は chat 限定; login `nlen` 16MiB cap は要再検証。

## 計測の限界

- 単一セッション(login+chunk view=4+move+chat)のため「トリガ不要な packet」のみが
  upper bound。mob spawn, damage, container open, piston, weather 等のイベント駆動
  packet はこのセッションでは出ない = 95 never-observed は真の未実装とは言えない。
- 真の網羅には vanilla jar との差分セッションが必要(同一 trace を両サーバに流して
  S2C multiset を比較)。`tools/client_diff/parity_client.py` がその器だが
  `~/.cache/megu-audit/load/server.jar` (sha1 pinned) がローカルに無い。
