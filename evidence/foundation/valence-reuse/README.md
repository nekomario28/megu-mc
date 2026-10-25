# Valence を部品単位で使う

参照は [valence-rs/valence](https://github.com/valence-rs/valence/tree/c4177cab47ae109c848a431756d5d7c0207c05ed)
の `c4177cab47ae109c848a431756d5d7c0207c05ed`（main の確認時点）。
コードは **MIT / Copyright (c) 2022 Ryan Johnson**。
[ライセンス原本](../../../third_party/licenses/valence.txt)を保持する。
ロゴは別ライセンスなので取り込まない。確認した source は **1.20.1 / protocol 763** で、
mc-spine の **1.21.1 / protocol 767** のレジストリ・定数の根拠にはしない。
これは当該 revision の調査で、全 branch/history の対応版調査ではない。

Valence 自身は Bevy ECS 上のカスタムサーバー用 framework で、vanilla game mechanics は任意 plugin の領域とする。
「Valence が実装したもの」を「vanilla 互換と実証済みのもの」と読み替えない。
依存追加・Bevy 導入・Valence の build/execution は行わず、既存 owner に合う差分だけを選ぶ。

## 今回の判断

| 領域 | 判断と理由 |
|---|---|
| packet の byte-angle | **BORROW_CONCEPT**。`valence_protocol/src/byte_angle.rs` の量子化値と byte 表現を分離する考え方を使い、mc-spine の既存 angle owner を修復した。`rem_euclid + round` の実装は採らず、実 1.21.1 constructor の floor/truncate と精度の差を保持する。[検証](../../gameplay/entity-angles/README.md) |
| entity update/tracking | **REFERENCE_ONLY**。`valence_entity/src/query.rs` の position/look/head/metadata の差分送信は参考になる。現在の追跡・fanout owner を置換する判断には、1.21.1 の tracker と lifecycle の検証が要る。単なる packet-id の置換では採らない |
| AABB / raycast | **HOLD**。`valence_math/src/aabb.rs` と `valence_spatial/src/lib.rs` は軽い数学の候補。ただし face 接触、zero direction、inside-origin を含む ray と、vanilla の有限 segment / voxel shape の契約は別。既存の projectile sample を helper だけ置換して parity を主張しない |
| chunk palette | **REFERENCE_ONLY**。`valence_server/src/layer/chunk/paletted_container.rs` は single/indirect/direct と compact-u64 の実装候補。mc-spine に既存 encoder があるので、同じ入力で correctness/費用の差を測る前に二重化しない。古い state/biome ID は採らない |
| NBT / Java string | **HOLD**。`java_string/src/cesu8.rs` は modified UTF-8 と lone surrogate を表現できる。Rust `String` への lossy 変換では同じ契約にならないため、構造物・アイテム・保存データの表現と実クラス round-trip を先に揃える |
| generated registry / metadata | **REJECT（この版のデータ転用）**。1.20.1 の生成値は 1.21.1 の根拠ではない。extract → generate の構成は参照できるが、データは pinned jar から取る |

最初の差分は小さい entity encoding の基礎に限定した。衝突・属性・AI・構造物や worldgen 全体を
Valence に置き換えたという意味ではない。Horizon と同じく、donor は実装候補、固定 1.21.1 は観測可能な挙動の正本である。
