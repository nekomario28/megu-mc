# 接続ライフサイクル：モデルと実通信を分ける

`spine_proto.py` は `protogx inspect` で検査する手書きモデル。
モデルが構造的に健全でも、実装のパケット符号化・タイムアウト・描画・
vanillaとの一致は証明できない。以前の「接続できない原因は状態機械ではない」
という結論は強すぎたため撤回する。

## 実装へ結びつける契約

protocol 767のClient Informationは、configurationでは `0x00`、playでは `0x0a`。
configurationの両方の待機ループと、接続後のplayループがview distanceを消費する。
localeは長さ制限付きUTF-8、長さは最大5バイトのVarInt、viewは**符号付きByte**。
接続が保持する有効距離をサーバー上限と最小距離で制限し、変更時に同じ窓を
送信にもアンロードにも使う。チャンク保持集合は既存の `Client.chunks` が唯一の所有者。

`tests/client_settings_wire.py` は使い捨て世界で実サーバーへ接続し、
両configuration段階からの設定、play中の拡大・縮小・符号付き入力・上限・
移動後の更新に対して、実際に受信したチャンク座標を検査する。
`mutation_check.sh` は各受信経路を無効化して、この試験が落ちることを要求する。
Cargoの出力先が変更されても、Cargoが報告した実行ファイルを使う。

この検査は合成クライアントによる通信の証拠であり、実ゲームクライアントの描画の
証拠ではない。またview以外の設定値をすべてゲームへ反映する契約でもない。
実クライアントの確認は `specs/close-defects/README.md` のCで別に追跡する。
