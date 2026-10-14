# コマンド権限を authzgx で検査した

`authzgx` は権限モデルの**昇格経路**（低い主体が sensitive な許可に到達するか）を検査する。
実装から2つのモデルを書き、**出荷既定**と**脱出口**を分けた。

## 1. 出荷既定（`auth.py`）→ **no problems**

```
2 principals, 2 roles, 39 permissions
inspect: no problems
```

**実装**（`src/main.rs:10437`）：

```rust
fn command_allowed_for_player(cmd: &str, args: &[&str]) -> bool {
    matches!(cmd, "help" | "list")
        || (cmd == "time" && matches!(args.first().copied(), Some("query" | "get")))
}
```

**allowlist（既定拒否）**である。**考えられていないコマンドは「許可」ではなく「拒否」になる。**
`run_command_ctx` は `ctx.me.is_some()`（= プレイヤー）のときだけこれを適用するので、
**RCON / コンソール（プレイヤー文脈なし）は全コマンドに到達する**。

**`player` は `help` / `list` / `time query` 以外に到達できない。** authzgx は昇格経路を1つも報告しない。

## 2. 脱出口（`escape.py`）→ **1 error（意図的）**

```
error perm:setblock escalation: low principal 'player_with_escape_flag'
                               reaches sensitive permission 'setblock'
```

`MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS=1` はプレイヤーに allowlist を**丸ごと飛ばさせる**。
コードのコメントは「**ローカル互換テスト用で、公開/LAN 配備向けではない**」と明記している。

**これは修正すべきバグではなく、意図的な穴である。** authzgx に**赤を出させたまま**このファイルを残すのは、
**穴の形と名前を、穴の無いモデルの隣に置くため**である。

## この検査の限界（正直に）

モデルは**コマンド名の許可**を写したもので、**引数の意味**は写していない。
`time query` は許可されているが、`time set` は許可されていない — この区別は
`command_allowed_for_player` が `args` を見ることで成立しており、モデルでは
`time query` と `time set` を**別の許可**として書いて表現している。
**引数まで含む検査は authzgx の範囲外**であり、**そこはテストで押さえる**。

**`wire_smoke.py` では見えない** — 権限判定はチャットコマンドの経路にあり、接続の検査を通らない。
