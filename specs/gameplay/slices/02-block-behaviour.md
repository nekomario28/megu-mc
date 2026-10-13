# 02 — ブロック挙動ディスパッチとランダムtick

Status: 設計。実装は未着手。**slice 03〜05 の前提**。

## 何が無いか

**ランダムtick の仕組みが存在しない。** vanilla では `randomTickSpeed`（既定3）で
chunk section ごとに毎tick ランダムな位置を選び、その位置のブロックの
`randomTick` を走らせる。これが**作物の成長・苗木の成長・葉の枯れ・草の伝播・
雪氷の生成**を駆動している。

現状：
- 作物は **独自のタイマー**（`crops()` の `HashMap<pos,u8>` を毎tick 走査）で育つ。
  vanilla のランダムtick ではない
- **苗木はブロックとして存在するが成長挙動が無い**（`name_to_state` の対応だけ）
- **サトウキビも同様**（状態IDはあるが成長しない）
- **葉の枯れが無い**
- 草の伝播・雪氷も無い

## 構造上の制約（これが設計を決める）

world は**疎なオーバーレイ**である：

```rust
static WORLD: Mutex<HashMap<(i32, i32), HashMap<u32, u32>>>;
// (cx,cz) -> { packed_local_idx -> blockstate }
fn block_at(x,y,z) -> u32 {
    w.get(&(x>>4, z>>4)).and_then(|c| c.get(&local_idx(x,y,z))).unwrap_or_else(|| base_state(x,y,z))
}
```

つまり**「読み込まれた chunk の一覧」というものが無い**。vanilla のランダムtick は
「読み込み済み chunk を列挙して、各 section でサンプルする」ので、
**列挙できる対象を決める必要がある**。

候補：
1. **オーバーレイのキー**（プレイヤーが触った chunk）— 狭すぎる（生成地形が入らない）
2. **hot region**（`region_guard()` が返す温かい領域）— シミュレーション対象の定義と一致する
3. **forceload された chunk**（`forceloaded()`）— 明示的な対象

**2 + 3 が正しい**：hot region は「シミュレーションする場所」の既存の定義であり、
forceload は明示的な指定。**新しい「読み込み済み chunk」概念を作らない。**

## 部品

```rust
/// vanilla: chunk section ごとに randomTickSpeed 個のランダム位置。
/// 位置は世界の RNG から引く（RNG 消費が挙動の一部である）。
fn random_tick_once() {
    for (cx, cz) in ticked_chunks() {
        for section_y in ... {
            for _ in 0..RANDOM_TICK_SPEED {
                let x = cx*16 + rnd(16); let y = ...; let z = cz*16 + rnd(16);
                on_random_tick(x, y, z);
            }
        }
    }
}

/// 1箇所のディスパッチ。vanilla はブロックの `randomTick` を呼ぶ。
/// ここは「ブロックの種類」で分岐する。手書きの数値範囲ではなく
/// 資産の `material` / タグ / 名前から種別を引く（slice 01 の規則）。
fn on_random_tick(x: i32, y: i32, z: i32) { ... }
```

**`on_random_tick` に置くもの（消費者）**：

| 挙動 | 条件 | 必要なデータ |
|---|---|---|
| サトウキビ成長 | 上 2 段が空・高さ < 3 | 状態ID（既にある） |
| 苗木成長 | 空間・（vanilla は光） | 樹木 feature（`tree.rs` がある） |
| 葉の枯れ | distance 7 かつ 4 以内に原木が無い | **状態の `distance` プロパティ** |
| 草の伝播 | 隣が草・（vanilla は光） | 光エンジン（slice 03） |
| 雪氷 | バイオーム温度 | バイオーム（既にある） |

## 検証の戦略（ここが難しい）

vanilla のランダムtick は **RNG を消費する**。つまり挙動の等価性は
「同じ RNG 列なら同じ結果」でしか言えない。

1. **RNG の消費順序を固定する**：`ticked_chunks()` の反復順、
   section の順、1 section 内の 3 サンプルの順。vanilla は
   `level.random` を共有するので、**自分の RNG 列を世界のものと一致させることは
   原理的にできない**（他システムも同じ RNG を引くため）。
2. したがって **vanilla との bit 一致は主張しない**。
   主張できるのは：
   - **分布の一致**（サンプル位置が一様、1 chunk あたりの回数が正しい）
   - **挙動の条件の一致**（枯れ/成長の条件式）
   - **RNG 消費が既存システムを乱さないこと**
3. 既存の作物タイマーを**どうするか**は別の決定：
   - (a) ランダムtick へ移す → **挙動が変わる**（タイミングが確率的になる）。
     vanilla に近づくが RNG 消費が増える
   - (b) そのまま残す → 新しいブロックだけがランダムtick を使う
   **測ってから決める。** 作物の成長速度分布を before/after で比較する。

## 実装前の前提：seed 付き world RNG が無い

**`rand_u64()` は `SystemTime` ベースである**（`src/main.rs:905`、18箇所で使用）。
つまり**サーバーに決定的な乱数列が無く、同じ seed・同じ入力でも実行ごとに結果が変わる**。

```rust
fn rand_u64() -> u64 {
    let x = SystemTime::now()...   // 時刻そのもの
```

これは2つの理由で**ランダムtick の前提を壊す**：

1. **テストできない。** ランダムtick は「同じ乱数列なら同じ結果」でしか
   等価性を言えない。時刻ベースでは再現できない
2. **vanilla と違う。** vanilla の `level.random` は seed 付きの状態機械で、
   保存もされる。時刻ベースは「保存される乱数状態」を持たない

**したがって順序の先頭に置く**：

0. **seed 付き world RNG（保存される状態）を導入する。**
   消費者はランダムtick（本slice）。`rand_u64()` の18箇所を
   いつ移すかは**別の決定**（移すと既存のドロップ挙動の分布が変わりうる）
   — まず**新規の消費者だけ**が新しい RNG を使う。

これは「消費者が居てから部品を作る」規則に合致する：
ランダムtick が最初の消費者であり、RNG はその前提である。

## 順序

1. **`world_rng()`**：seed 付き・状態を持つ・保存対象。テストで列を固定できる
2. **`ticked_chunks()`**（hot region + forceload。新しい概念を作らない）
2. **RNG サンプラ + 一様性のgate**（消費者なしでは作らない規則に従い、
   次と同時に入れる）
3. **サトウキビ成長**（最も単純な消費者。光も樹木も要らない）
4. **苗木成長**（`tree.rs` の feature を使う）
5. **葉の枯れ**（`distance` プロパティの読み取りが要る）
6. 草・雪氷（slice 03 の光・温度を待つ）
7. **作物タイマーの扱いを決める**（(a)/(b) を測ってから）

## 非目標

- vanilla との **bit 一致**。RNG を共有できない以上、主張しない。
- 新しい「読み込み済み chunk」概念。
- 光の無い段階で光依存の挙動を書くこと（slice 03 を待つ）。
