# エンティティ角度の byte 量子化

既存 `angle` は f32 を直接 i8 に cast していた。Rust のこの cast は飽和するため、
例えば head update の **180° が 127** となり、native の **128** に折り返さない。
負の小角の floor、複数周回、演算順も別の誤りを生む。

Valence の byte-angle という値境界を参考にしたが、同実装の丸め方は vanilla 1.21.1 の正本ではない。
ここで取ったのは **考え方（BORROW_CONCEPT）**であり、同じ式をコピーしたものではない。
実装は既存 main の angle/packet builders に置き、spawn の3 producer には同じ角度フィールド writer を使う。
新しい packet engine・ECS・依存・旧版 ID は加えない。

## 同じ「角度」でも constructor の契約が違う

公式 mappings と固定 jar の bytecode で次を確認した。

- `ClientboundAddEntityPacket` (`abv`) は body の pitch/yaw を **f32 の乗算→除算→Mth.floor** で詰める。
  spawn head yaw は **f64** の計算である。
- `ClientboundTeleportEntityPacket` (`afw`) は同じ f32 式を **int へ truncate** し、byte へ narrow する。
  spawn の floor を共用すると負の小角で違う byte になる。
- head update は `ServerEntity` 側の **Mth.floor(float)** の式で量子化する。

どれも float → int は Java の飽和、int → byte は下位8bitの折り返しである。
有限の入力でも乗算が infinity になり得るため、mod 360 や単純な Rust `.floor() as i8` への置換はしない。

## 参照と guard

[`tools/packet_angles_oracle.py`](../../../tools/packet_angles_oracle.py) は既存の authenticated Java runner を使う。
固定 1.21.1 jar を SHA-1 で認証してから実行する。ダウンロードやサーバー起動はしない。
[`vanilla.json`](vanilla.json) は f32 bit pattern を入力とし、0/負の角度/周回/量子化境界の隣接 f32/
大きい有限値を含む。ケースの正本は generator であり、手書きの別期待値表は持たない。

Java は実 `ClientboundAddEntityPacket` の通常 constructor を呼び、実 byte field を読む。
これらの field を packet writer が同じ順序で直接 `writeByte` することも bytecode で確認した。
teleport は実 `Marker(EntityType.MARKER, null Level)` を通常 constructor で作り、
実 `Entity.setYRot` / `setXRot` → 実 `ClientboundTeleportEntityPacket` constructor を呼ぶ。
Level 不在の許容経路は実行確認した。Level のメソッド・Marker の spawn/tracking メソッドは呼ばず、
constructor bypass / Unsafe allocation は使わない。
head update は実 `Mth.floor(float)` を実行するが、ServerEntity tracker 全体の実行ではない。
Guava が出す自身の Unsafe 警告は、この probe の constructor bypass を意味しない。

Rust は production の player/mob/generic spawn、teleport、head packet builders が出す角度 byte を照合する。
fixture の空・末尾欠落・追加・入力変更・結果変更・source/jar provenance の drift は Python guard で落とす。
日常の consumer test は JDK 不要で、既存 verify/CI に接続する。native の再実行・意図した記録は CLI の責任とする。

```sh
cargo test --release --offline --locked -j 1 entity_angles -- --nocapture
python3 tests/packet_angles_oracle_test.py
python3 tools/packet_angles_oracle.py
MUTATION_FILTER=entity_angles_ MUTATION_RELEASE=1 CARGO_BUILD_JOBS=1 \
  bash tools/mutation_check.sh --offline
```

## この区切りの結果

- native **123ケース**を実行し、再実行でも入力・結果・provenance 全体が一致した。
- production packet builder を通る Rust **3本**、新しい Python guard **3本**が通過。
  既存 surface/biome/container guard、gate self-tests、layer/debt checks、all-target clippy `-D warnings`、fmt も通過した。
- canonical の選択 mutation は **5 proved / 0 not a gate / 0 parked / 0 skipped**。
  byte wrap、floor、teleport truncate、subnormal での演算順、spawn head の f64 をそれぞれ壊して検出した。
  復元後の source hash を照合し、production binary を再ビルドして container/gameplay wire を両方確認した。
- full serial release suite は **188 passed / 24 failed / 1 ignored**。
  前回保存済みの surface run から失敗名は増減せず、新設3本だけが増えた。
  **24件中15件は `PoisonError`** で、24個の独立した欠陥という意味ではない。
- 独立 `codex review --uncommitted` は actionable な問題を指摘しなかった。
  最終 review は**静的のみ**で、Rust/Java 実行は実装側の記録である。

`red.txt` は subnormal 6入力を追加する前の **117ケース**での3本の失敗ログである。
corpus の拡張で case ID/pitch の組み合わせが変わったため、最終 fixture の ID と混同しない。
受け入れの根拠は最終123ケースの再実行と mutation controls である。

実行条件・source/fixture/binary identity と結果は [run.json](run.json) と各ログを参照。
`unit.jsonl` は journal から invocation・message・accounting の必要 field だけを抽出したもの。
ログの末尾空白だけは正規化し、raw/committed hash を `log-normalization.json` に残した。
Rust の検証 peak は **1,077,592,064 bytes**、mutation/build/wire は **1,217,200,128 bytes**。
fresh light admission と 2 GiB cap / jobs 1 で実行し、native/reference と review は 1 GiB cap を使った。

## 検証範囲の限界

この範囲は **角度フィールド**であり、spawn packet 全フィールドの vanilla 差分、独立した body/head pose、
client の補間、実 vanilla server 対 port の wire capture、entity physics/tracking/lifecycle の一致ではない。
データ移行の全 suite は別判定で、旧 capture の再記録で受け入れを作らない。
