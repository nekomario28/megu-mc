# 接続ライフサイクルを protogx で検査した（prism より前）

`gxkit` の `protogx` はプロトコル/メッセージ状態機械を検査する
（到達不能状態・未処理メッセージ・デッドロック・終端に到達できない状態）。
**実クライアントを起動する前に、接続が進んで終われるかを証明できる。**

## 結果：**0 problems**（構造的に健全）

```
protocol: 6 states, 15 transitions
  14 messages, 0 requests
```
`inspect` は問題行を出さない（= 0 件）。

**状態**：handshake / status / login / configuration / play / closed。
`src/main.rs` の実装を読んで写した：

| フェーズ | 実装 | 行 |
|---|---|---|
| handshake | `handshake_next_state()` が status(1) / login(2) を選ぶ | 11757 |
| login | Login Start(0x00) → Login Success(0x02) → **Login Acknowledged(0x03)** | 11801–11808 |
| configuration | `load_config_packets()` → **Select Known Packs(0x07) を15秒待つ** → Finish Configuration → **ack(0x03) を15秒待つ** | 11810–11841 |
| play | `'conn` ループ | 11973– |

**重要な事実**：configuration の2つのループは
**`Some(_) => continue`** で**未知のパケットを無視する**（切断しない）。

```rust
Some((0x07, _)) => { got_selpacks = true; break }
Some(_) => continue,
```

## 私のモデルは3回間違っており、毎回コードの方が寛容だった

| 私が書いたこと | 実際 |
|---|---|
| `configuration` は **ack(0x03) 以外で切断する** | `Some(_) => continue` — **無視する** |
| `Select Known Packs` は**未処理** | **専用の待ちループがある**（0x07 を15秒待つ） |
| `status` が**デッドロック** | 私が遷移を書き忘れただけ |

**この順序は重要である**：protogx の指摘は**まず自分のモデルの誤り**であり、
**コードの欠陥と報告する前に、コードを読んで確かめる必要があった**。
「ツールが赤を出した」は「コードが悪い」ではない。

## 残る本当のギャップ（プロトコル破壊ではない）

`Some(_) => continue` は**無視**であって処理ではない。したがって
**`Client Information`（view distance・locale・skin parts）は捨てられている。**
vanilla は view distance を使ってチャンク送信距離を決めるので、
**これは挙動のギャップ**である（接続は壊れない）。PLAN に記録した。

## これが「prism より前」である理由

**実クライアントが繋がらない原因は、接続の状態機械ではない**と分かった。
したがって次に疑うべきは**パケットのフィールド符号化・チャンクデータ・レジストリの中身**であり、
クライアントを起動する前にサーバー側で潰せる候補が絞られた。
