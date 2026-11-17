# fixture suite — 既存比較器を呼ぶ薄い実行入口（方針B）

Status: 実装済み（ローカル）。このページは入口の使い方と受け入れ規則だけを持ち、
測定値・未実行の一覧は実行ごとの `result.json` / `summary.md` を正本とする。

[project-convergence README](README.md) の方針B（比較の入口と結果の集計を共通化する）を、
新しい比較器を作らずに既存の2つへ接続する。差分計算・正規化・参照実行をこの入口へ複製しない。

| 役割 | 既存の道具 | この入口が足すもの |
|---|---|---|
| chunk全域の比較 | `tools/chunk_triage.py`（`--port-bin --n --fail-on-diff --json-out`） | manifest 1枚から行ごとに起動し、子プロセスの生 stdout/stderr/exit と JSON を保存 |
| sculk family の比較 | `tools/sculk_gate.py`（常に `--require-match --json-out`） | 同上 |
| 検証台帳・集計 | なし（既存は手作業） | 行ごとの status、入力sha256、stage、writer、exit code を1つの JSON と Markdown に |

```
tools/fixture_suite.py <manifest.json> --out <empty-dir> [--timeout SECONDS]
```

`--out` は必須で、入力（manifest・fixture・world・region・dump・trace）と重なる場所、
および前回の実行結果が入っているディレクトリを拒否する。子プロセスは引数リスト +
`shell=False` で起動し、Pythonの任意コマンド実行・プラグイン・タスクフレームワークは持たない。

## manifest（version 1）

```jsonc
{
  "version": 1,
  "rows": [
    {
      "id": "ow-4242-features-3-2",          // 重複不可。[A-Za-z0-9][A-Za-z0-9._-]*
      "kind": "chunk",                        // chunk | sculk
      "fixture": "captures/nat-.../",         // boundary.json を持つ capture ディレクトリ
      "world": "captures/nat-.../world",      // その dimension の world（level.dat がある方）
      "port": "captures/nat-.../port-dump.bin",
      "origin": [3, -2],
      "n": 3,
      "grid": { "min_y": -64, "height": 384 },
      "seed": 4242,
      "stage": { "reference": "features", "port": "features" },
      "inputs": {
        "sha256": {
          "captures/nat-.../world/level.dat": "<sha256>",
          "captures/nat-.../world/region/r.0.-1.mca": "<sha256>",
          "captures/nat-.../port-dump.bin": "<sha256>"
        },
        // 任意。runtime書き込みを持つ capture でのみ使う: 宣言はピンであって分類ではない。
        "expected_runtime_writes": { "count": 16,
                                     "blocks": {"nether_brick_fence[east=true,north=true,south=true,waterlogged=false,west=true]": 16},
                                     "writes_sha256": "<sorted RUNTIME payload lines の sha256>" },
        // 任意。無ければ writer は UNKNOWN のまま（block名から推定しない）。
        "writer_trace": { "path": "captures/nat-.../sculk-trace.txt", "sha256": "<sha256>",
                          "attribution": "manifest が宣言する帰属（任意の文字列）" }
      }
    }
  ]
}
```

- 相対パスは **manifest があるディレクトリ** を基準に解決する（cwd ではない）。
- `kind: chunk` は `chunk_triage` に `--n/--bin-ox/--bin-oz/--bin-n/--min-y/--height/--preset`
  を渡す。`kind: sculk` は Overworld の 24 section（`min_y=-64, height=384`）だけを受け付け、
  `sculk_gate` に `--require-match` を必ず渡す。
- `inputs.sha256` は、比較が実際に読む入力（`level.dat`、window の各 region ファイル、
  port dump）をすべて覆う必要がある。1つでも欠ける・一致しない行は INVALID。
  値は `sha256sum <file>` の結果。window の region ファイルは
  `r.<cx>>5>.<cz>>5>.mca`（`kind: sculk` も同じ）。
- 例は上記の構造のみ。実在worldや非公開artifactをリポジトリへ置かない。テストは
  `tests/fixture_suite_test.py` が一時ディレクトリに生成した小さな fixture を使う。

## 受け入れ（緑の条件）

行が MATCH になるのは、次のすべてを満たしたときだけである。

1. `boundary.json`（`tools/carve-agent/native_fixture.sh` が書く既存のprovenance）が
   `mode: frozen`、`runtime_writes`、`stage`、`seed`、`dimension`、`min_y`/`height`/`chunks`
   を記録している。runtime書き込みがある保存worldは、宣言した生成stageの比較ではない —
   **ただし行が `inputs.expected_runtime_writes` = `{count, blocks, writes_sha256}` を宣言し、
   capture自身の `run.log` がそれを再現する場合を除く**: `wrote=true` 行数 == count、
   canonical state census（`assets/blocks_1211.json` のstate名。比較器と同じowner）== blocks、
   並べ替えたRUNTIME行payloadのsha256 == writes_sha256。3点すべて完全一致のみ MATCH であり、
   hash無しのcount単独・未知のmember・1セルでも異なるcensus/hash は INVALID。
   宣言は分類ではなくピンであり、`boundary.json` の runtime_writes と `run.log` の
   `wrote=true` 数も一致しなければならない。未宣言で書き込みがある行は従来どおり INVALID。
   各フィールドは使う前に型を検査する（`dimension` が object/list、`runtime_writes` が bool
   のような値は INVALID であり、suite を `TypeError` で落とさない）。
2. 宣言した `stage.reference` と `stage.port` が一致し、`reference` が fixture の記録と一致する
   （stage は比較の同一性の一部。block名からwriterや最初の分岐stageを推定しない）。
3. `level.dat` の `Data.WorldGenSettings.seed` が宣言seedと一致し、`grid` が fixture の
   `min_y`/`height`、`n*n` が fixture の `chunks` と一致する。
4. `world`（と `region_world`）が **fixture 自身の world** である。`boundary.json` は fixture に
   あり `world_subdirectory` を記録しているので、`region_world` は
   `fixture/<world_subdirectory>` と一致し、`world` は fixture の `world` かその subdirectory
   でなければならない（`world_subdirectory` が無い場合は `fixture/world` のみ）。
   同じバイト列のworldを別の場所へ複製し、sha256 をその複製へ貼り直しても INVALID である
   — provenance は別のツリーを説明していることになる。
5. `inputs.sha256` の全入力が実ファイルと一致する（差し替え・古い入力を緑にしない）。
6. 子プロセスが `n*n` chunkすべてを比較し、skip 0、比較セル数 > 0。`kind: sculk` は参照側の
   sculk セル数 > 0。exit 0 と子の JSON が矛盾する（差分数>0、比較0件、部分grid、
   切り詰めJSON、exit 2/3）行は INVALID。

writer trace を束縛しない行は `writer.status = "UNKNOWN"` のまま MATCH になり得る。
traceを束縛した行は `DECLARED` とその宣言値をそのまま記録する（推定しない）。

## status と終了コード

| status | 意味 |
|---|---|
| MATCH | 上記を満たし、子のJSONで差分0・正の比較 |
| DIFF | 上記を満たし、子のJSONで差分>0 |
| INVALID | 入力・provenance・子の契約のいずれかが受け入れ不能 |
| NOT_RUN | 宣言されたが実行に到達しなかった行（出力先の拒否などで実行自体が始まらない場合。stderr に一覧を出す） |

- 0: 行が1つ以上あり、全行が MATCH
- 1: 有効な行に差分がある（INVALID/NOT_RUN が無い）
- 2: INVALID/NOT_RUN・子のクラッシュ・比較0件・行0件

bad input の行があっても残りの行は実行・記録する。行を落とさない。

## scope（主張しないこと）

- 対象は **渡した manifest の全行だけ**。リポジトリ内の他のfixtureを自動発見しない。
- `kind: chunk` は宣言windowの全セルの canonical block state 名（air/cave_air/void_air を区別）を
  比較する。heightmap・block entity・構造物payloadの受け入れは子の報告であって、この入口の
  合格主張ではない（whole canonical state の証明ではない）。
- `kind: sculk` は sculk family のセルだけを見る。whole-world parity ではない。
- この入口は capture・dump生成・world再生成・network・jar取得を行わない。保存済みartifactだけを比較する。
- trace の writer 帰属は、manifest が宣言した場合のみ `DECLARED` として転記する。

## sculk_gate の既存欠陥（この入口より先に修正した）

`tools/sculk_gate.py` は block **名** で比較していた（`elif gn != wn`）ため、
`sculk_vein` の `waterlogged`/方向、`sculk_catalyst` の `bloom` のような
同名・別stateを一致として通していた。あわせて、

- 非`full`chunkを `continue` で飛ばしていた（部分gridがPASSし得た）。
- dump長を「4096の倍数」でしか検査せず、24 section 未満の切り詰めを受け入れていた。
- `n <= 0` でクラッシュまたは0件比較になり得た。
- 保存worldが無い、または `region/r.*.mca` が無い場合に `FileNotFoundError` が
  そのまま外へ出ていた（`vanilla_chunk` の周囲が `SystemExit`/`ValueError` だけを
  受けていた）。現在は `Path(world).is_dir()` と `OSError` の両方を受け、
  `Usage`（exit 2）として理由を述べる。

現在は canonical state id（`assets/blocks_1211.json` の並び。参照側と `carver_diff` が同じ
keyspaceを通る）で比較し、`n*n` chunkすべての存在と `full`、24 section のdump、
範囲外state idを入力エラー（exit 2）として拒否し、`--json-out` に実際の計数を書く。
`--require-match` と report-only の従来契約は変えていない。
