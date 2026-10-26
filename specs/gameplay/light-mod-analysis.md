# 軽量化mod の設計解析（光エンジン）

Status: 調査。**設計のみ参照。コードは写さない。**

## 出典とライセンス（重要）

| 対象 | 出典 | ライセンス | 扱い |
|---|---|---|---|
| **Starlight** | `github.com/PaperMC/Starlight`（branch `fabric`）、`TECHNICAL_DETAILS.md` | GitHub API で **NOASSERTION**（許容的と確認できない） | **設計のみ参照。コードは写さない** |
| **Lithium** | `github.com/CaffeineMC/lithium`（branch `develop`） | **LGPL-3.0** | 同上 |
| **FerriteCore** | `github.com/malte0811/FerriteCore` | README のみ取得 | 同上 |

**issue #3**（Pumpkin）で定めた扱いと同じ。
**高レベルの設計と「何が遅いか」の記述のみ**を参照し、実装は独立に書く。

## 最も重要な事実：vanilla 1.20+ は Starlight の方式を取り込んでいる

Starlight の文書冒頭の obsoletion notice：

> Given the 1.20 Vanilla light engine copies basically everything from Starlight in
> this document and then some, the comparisons to Vanilla and Phosphor are invalid.

**したがって我々の参照対象は「vanilla 1.21.1 自身の光エンジン」であり、
Starlight はその設計を説明する文献である。** 我々が vanilla と一致させたいなら、
Starlight の文書にある原則がそのまま vanilla の原則になっている。

## 我々の実装に直接効く3つの原則

### 1. 光データは「非空セクションの1セクション半径」にしか存在しない

> The general rule imposed is that only light data (nibblearrays) that exists within
> 1 square radius of a non-empty chunk section is going to be initialised - no other
> light data will exist.

**これが我々の費用の主因に効く。** 我々の `sky_light_sections` は
**常に26区間すべて**を計算して送る。地形が y=64..80 にしか無いなら、
光が要るのは**その周辺の数区間だけ**である。

### 2. 既に 15 の隣接を高さマップで除外する（キューが 2000 → 300）

> a whooping 27% of time was spent reading light levels for propagating light. This
> very significant difference made me think that most of the queued values were
> simply not propagating light at all, since their neighbours were most likely full
> 15 as well.
> … you can use the heightmap to figure out what neighbours are going to be level 15
> so you can exclude checking them, and in most cases exclude even queueing the
> position. For example, for lighting a desert … the queued levels to propagate were
> only about ~300, whereas on the Tuinity implementation they could be ~2000 or more.

**これは我々の天空光の水平伝播の設計にそのまま効く。**
我々は「暗領域に接する殻」を種にしたが、**その殻の大半も
「既に15の隣」である**。高さマップで除外すればキューはさらに減る。

**そしてこれは我々が z=9 が 6 になる理由を説明できなかった問題への示唆でもある：**
伝播の種の選び方と「既に15の領域をどう除外するか」が絡むと、
意図しない経路が生まれうる。

### 3. 光は「送信ごとに計算」ではなく「変化時に維持」する

> it maintains the light data in a hashtable (section->nibble array) and it updates
> this map via copy-on-write as the number of chunks generated grows, so does the
> data in this hashtable, and so does the time the copy-on-write operation takes.

**vanilla も Starlight も、光を維持（incremental）し、変化時に更新する。**
我々は**チャンク送信ごとに 475µs で計算し直している**。これは
「軽量」に反する構造であり、**測定値（475µs）の根本原因はここにある可能性が高い。**

ただし vanilla の copy-on-write は**生成チャンク数とともに遅くなる**という
欠点もあると文書が述べている。**維持方式を採るなら、その欠点を継承しない形**を選ぶ。

## 我々の現在地（比較）

| | vanilla/Starlight | この枝 |
|---|---|---|
| 光の保持 | セクションごとに維持、変化時に更新 | **チャンク送信ごとに全計算** |
| 計算範囲 | 非空セクションの1半径 | **常に26区間** |
| 隣接の除外 | 高さマップで既に15を除外 | なし |
| 水平伝播 | あり | **なし**（試作→撤回） |
| ブロックライト | propagator が1つ | 独自 BFS（密な配列、x1.41） |

**つまり我々は vanilla の設計に対して「維持をやめて毎回計算する」という
最も費用のかかる選択をしている。** 入射光の無い世界では 23.8µs で済むが、
これは「何も維持していない」ことの裏返しでもある。

## 次の一手（優先順）

1. **光を送信ごとに計算するのをやめる**（大きな構造変更）。
   まず**何を維持するか**を決める：セクションごとのnibble配列と、
   「非空セクションの1半径」ルール
2. **26区間固定をやめる**：非空セクションの周辺だけを計算・送信する
   （マスクは既にパケットにある。今は常に全区間を送っている）
3. **天空光の水平伝播を高さマップで除外して実装**（z=9 の件の再挑戦）
4. 各段階で**連続前後測定**、対照つき

## 参照した文書

- `TECHNICAL_DETAILS.md`（Starlight）: 伝播アルゴリズムの比較、光データ管理、
  chunk lighting、ベンチマーク
- 取得元 URL は上表。**ライセンスが許容的でないため、文章・コードの転載はしない。**

## 独立レビュー（flash）で確認したこと

`sky_spill` と `sky_light_sections_from` を `opencode-go/deepseek-v4.1-flash` でレビュー：

- **BFS は値を上げるだけ**（`lit` は既存 < 新値のときだけ insert、合成は `max`）。
  減衰 `level - max(1, opacity)` はブロックライトの BFS と同一。
- **`sky_light_sections_from` の3つの fast path は `pack_sky_section` と一致**（境界 `floor==sy`,
  `floor==sy+15` 含む）。インデックスにも問題なし。
- **seed の `dark()` ガードは死んだコード**（`y = floor+1` は `y <= floor` を満たさない）で、
  コメントも実装と食い違っていた。ガードを除去しコメントを修正。
- **マージンの floor を 3×3 化して修正**：当初 `dark()` は `x.rem_euclid(16)` で中央チャンクの
  `floors` を流用していたため、BFS が隣チャンクの margin に入るとその列の floor を正しく
  使えなかった。`sky_light_sections_world` が 3×3 の floors を計算して `floor_of` として渡す
  ようにし、各セルが自分の列の floor で判定されるようにした（3×3 は固定配列なので `dark()` の
  追加コストは小さい。floors の計算は 9 倍になるが、`height_at` はキャッシュ済み）。
