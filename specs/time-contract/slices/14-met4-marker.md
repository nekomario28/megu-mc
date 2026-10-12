# 14 — MET4マーカー

## なぜ

20TPS切替の直前にDrop/Xp age をtickへ直したが、旧MET3はmsで書いていることを
受け入れなかった。切替で1tick=50msになったため、旧ファイルのageをtickとして読むと
寿命を超えて全ドロップが即座に消滅する。これは切替が作った後退である。

## 契約

- magicを `MET4` に上げる。`MET4` のageはserver tick。
- `MET2` / `MET3` のageはミリ秒。`raw / TICK_MS` でtickへ変換する。
- MET2は従来通りair fieldを持たない。mobは満タンの空気供給。
- 周期を変えても旧ファイルの実時間が保存される。

## ゲート

本番encoderを使った3つのround-trip。負の対照3種すべて検出。
負の対照には「修正前の実際のバグ」を含める。
serial/parallel全テスト、Fabric/NeoForge checkを通す。

## 残る永続化の欠落

mob air が永続化されない（MET2でもMET4でも）。vanillaは持つ。
追加するにはformatにfieldを足す必要がある。
