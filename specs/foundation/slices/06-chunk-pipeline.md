# 06 — Chunk送信 pipeline とビュー距離

## Why

`stream_chunks` は移動のたびに**同期的に9×9（81チャンク）を一括送信**する。

```rust
for dx in -4..=4i32 { for dz in -4..=4i32 { ... }}     // 窓は ±4 固定
if news.is_empty() { return }
let _ = send_packet(s, 0x0d, vec![]);                  // 一括開始
for (cx, cz) in &news {
    if send_packet(s, 0x27, map_chunk(*cx, *cz)).is_err() { return }
    ... signs ...
}
```

### 3つの問題

1. **同期的一括送信**。プレイヤーが移動すると、そのtickで81チャンクぶんの
   `map_chunk`（NBT・高さマップ・section符号化）を作って送る。
   tickが跳ねる。PR #6 が「avoid synchronous 9×9 bursts on player movement」
   として指摘している（issue #3 P1/P2）。
2. **ビュー距離が ±4 固定**。クライアントの描画距離設定を受けず、
   運営者も変えられない（[slice 28](../../time-contract/slices/28-playability-gaps.md) A4）。
   遠景が消え、描画距離MODも効かない。
3. **backpressure が無い**。相手が読み遅いと送信が詰まる。
   PR #6 はプロトコル767のACK（`0x08` float）で gate する設計にしている。

## 既存の作業との関係

**PR #6（`feat/queued-chunk-streaming`）が、この設計をほぼ実装している。**
ただし base は `fix/audit-1-security-runtime` で、このbranchとは別系統。
issue #3 の記載：

- near-first queue、移動でキャンセル、per-batch cap
- protocol-767 の ACK gate、stale window の拒否
- 実Javaログインでの ACK-gated 4→2→1 バッチ検証

**したがってこのsliceは「新規設計」ではなく「統合仕様」**。
PR #6 をこのbranchに取り込み、以下を**契約として固定**する。

## 契約

- **ビュー距離は設定可能**。既定は現状（±4）を保ち、
  運営者が環境変数か設定ファイルで上げ下げできる。
  クライアントの要求は**上限でclamp**する（要求どおりに送らない）。
- **1tickの送信量に予算**を置く。移動時に81チャンクを一度に作らない。
  near-first（近い順）で、予算ぶんだけ送る。
- **移動で古い予約をキャンセル**する。窓から出たチャンクは送らない
  （既存の `sent` からのunload と整合させる）。
- **ACK gate** でbackpressureを扱う。ACKが来ないまま先へ進まない。
- **パケット順序は 1.21.1 の仕様に従う**。順序を変えない。
- 予算・バッチ上限・タイムアウトは `TICK_MS` から導出する
  （実時間の値を生で書かない）。

## 非目標

- チャンク生成の並列化（issue #3 が「owner/epoch invariants が要る」としている）。
- Anvil/Linear の保存形式。
- ビュー距離に応じた hot/warm 半径の自動調整（slice 01 の non-goal）。
  ただし**整合は必要**：送る範囲 ⊃ 興味範囲、を検証する。

## ゲート

- **順序**: 1.21.1 のチャンク送信順（開始 `0x0d` → `0x27` → unload `0x21`）が
  保たれることを wire で確認する。
- **予算**: 移動時に1tickで送るチャンク数が上限以下であることを検証する。
- **キャンセル**: 窓から出たチャンクが送られないこと。
- **ACK gate**: ACKを返さない相手に対し、送信が無制限に進まないこと。
  タイムアウトで切断されること。
- **clamp**: クライアントが大きな半径を要求しても上限を超えないこと。
- **整合**: `stream_chunks` の窓が `hot`（slice 01）を包含すること。
- 実Javaクライアントでのログイン・移動・再ログイン（PR #6 が既に一部実施）。
- 全テスト、CI 4step、wire smoke。

## リスク

- **PR #6 との重複実装**。設計を先に固定し、PR #6 をそのまま取り込む。
  独自に書き直さない。
- base が異なるため統合時に衝突する。slice 03/04 と同じく
  **1コミットずつ取り込み、テスト数を確認する**。
- ビュー距離を上げると生成費用が跳ねる。既定は現状のままにし、
  上げた場合の費用は**測ってから**記録する（未測定のまま推奨しない）。
