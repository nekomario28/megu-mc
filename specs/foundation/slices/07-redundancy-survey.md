# 07 — 冗長実装の調査（クローン検出）

Status: 調査完了。検出した上位から統合済み（下記）。

## 方法

関数本体を取り出し、**識別子・数値・文字列を潰して正規化**し、
4-gram shingle の Jaccard 類似度で比較する。25〜90トークンに限定する。

上限を入れないと**偽陽性**が出る：大きな本体は識別子を潰すと
どの長い直線コードも似るため、`load_datapacks` と `jblock` が J=0.96 になった。

```
245 idiom-sized functions (25-90 tokens); 29 clone pairs (J>=0.5)
```

## 検出結果と判定

| 対 | J | 判定 |
|---|---|---|
| **`client_snapshot` idiom ×35** | (同一式) | **統合済み** |
| **`broadcast` / `broadcast_seen_ex` / `broadcast_chunk`** | 0.90 / 0.73 | **統合済み** |
| **`requeue_water` / `requeue_hopper` (+`requeue_lava`)** | 1.00 | **統合済み** |
| `set_if_can_replace` / `set_if_can_replace_or_log` / `is_air_or_leaves` | 0.91 | **統合済み** |
| `name_to_state` / `name_to_item` / `name_to_etype` / `name_to_effect` | 1.00 | 未着手（slice 03 そのもの） |
| `spawn_arrow_pkt` / `spawn_drop_pkt` | 1.00 | 未着手（slice 04） |
| `spawn_player` / `spawn_mob` | 0.77 | 未着手（slice 04） |
| `harvest_level_required` / `enchant_max_level` | 0.76 | 未着手 |
| `door_base` / `gate_base` | 1.00 | 未着手（小さく、害が無い） |
| `planks_for_log` / `sign_base_for_item` / `item_to_state` / `attr_key_id` | 1.00 / 0.69 | 未着手（表引き idiom） |
| `top_ws` / `top_oc` | 1.00 | worldgen。害なし |
| `lerp2` / `lerp3`、`grad` / `grad3` | 1.00 / 0.95 | worldgen。**アリティ・用途が違う。統合しない** |
| `new` / `new_l`（worldgen） | 1.00 | 未判定 |
| `resolve_cond` / `resolve_ref` | 1.00 | worldgen。未判定 |

## 統合済み1：`client_snapshot()` — 35箇所 → 1箇所

```rust
let cls: Vec<Arc<Client>> = { clients().lock().unwrap().iter().cloned().collect() };
```

この式が**35回**書かれていた。`clients()` を保持したまま送信すると
**自己デッドロック**する（コメントによれば2回踏んでいる）。

統合により、ロック順序の規則が**関数**になった。コメントではなく構造で守られる。
手書きの残りは `client_snapshot()` 自身の定義内1箇所のみ。

## 統合済み2：`fanout` — 4つのbroadcastを1つのループに

`broadcast` / `broadcast_chunk` / `broadcast_seen` / `broadcast_seen_ex` が
それぞれ同じループを書いていた。違うのは**述語だけ**。

```rust
fn fanout<F, P>(interested: P, mk: F) { for c in client_snapshot().iter() { if !interested(c) { continue } ... } }
fn broadcast(me_eid, mk)          { fanout(|c| c.eid != me_eid, mk) }
fn broadcast_chunk(cx, cz, mk)    { fanout(|c| c.chunks.lock().unwrap().contains(&(cx, cz)), mk) }
fn broadcast_seen_ex(eid, ex, mk) { fanout(|c| c.eid != ex && c.seen.lock().unwrap().contains(&eid), mk) }
```

`broadcast*` の呼び出しは**94箇所**あるので、効果範囲は広い。

### ゲート

`the_broadcast_helpers_still_select_the_same_clients`。実ソケット対で
3クライアントを作り、**述語が選ぶ相手だけが受信する**ことを検査する
（sender除外・chunk所持・seen所持・excl）。
`interested` を無視して全員に送る変異で**exit=101**。

## 未着手の上位候補と、そう判断した理由

**統合済み3：`is_replaceable(reg, cur, tags)` — 述語3つ → 1つ**

`set_if_can_replace` / `set_if_can_replace_or_log` / `is_air_or_leaves` は
**同じ質問**（この位置に書いてよいか）を、タグだけ変えて別々に書いていた。

```rust
fn is_replaceable(reg: &BReg, cur: u32, tags: &[&str]) -> bool {
    reg.is_air(cur) || tags.iter().any(|t| reg.in_tag(cur, t))
}
```

タグが**引数になった**ので、どの placer が何を置換可能と見なすかが
呼び出し側に現れる。以前は3箇所に散っていた。

述語が `&mut FC` ではなく**現在状態**を受け取る形にしたので、
**FC を構築せずにテストできる**。

ゲート `the_replaceable_predicate_says_what_each_placer_used_to_say`：
空気はすべての placer で置換可能、石はどれでも不可、原木は
`logs` を名乗る placer でのみ可、加えて3つのタグ集合すべてで
定義（is_air ∨ in_tag）と一致。タグ引数を無視する変異で**exit=101**
（[tree-negative.log](../../../evidence/foundation/fanout/tree-negative.log)）。

**注意**：`tree.rs` には元々テストが無かった。この統合は条件を逐語的に
保存しているが、**placer 全体の挙動は依然としてテストされていない**。

**`name_to_*` の4つ組（J=1.00）** — どれも「静的表を名前で引いて
無ければNone」。slice 03 の registry そのもの。**表ごとに統合すると
slice 03 と重複する**ので、registry として一度に行う。

**`door_base` / `gate_base`（J=1.00）** — 小さく、害が無い（間違えても
ドアの向きが違うだけ）。優先度低。

**`lerp2`/`lerp3`、`grad`/`grad3`** — **統合しない**。アリティが違い、
潰すと分岐が増えて速くならない。クローン検出の偽陽性側。

## この調査の限界

- **正規化が強い**ので、J が高くても意味のある統合とは限らない
  （`lerp2`/`lerp3` がその例）。Jは候補を挙げるだけで、判定は人間がする。
- **本体が90トークンを超える関数は対象外**。大きな関数の中の重複は見えない。
- 挙動の重複（同じ規則を別々に実装）は**テキストが似ていなければ検出できない**。
  slice 26 の Snowball のdrag欠落はこの型で、この検出器では見つからない。
