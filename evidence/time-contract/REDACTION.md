# evidence/time-contract: 公開準備での redaction

公開前に、これらのログへ含まれていた作成者環境の絶対パス（ホームディレクトリ・作業ディレクトリ）を除いた。
証拠の内容（コンパイル結果・テスト結果）は変えていない。変更はビルドディレクトリの文字列だけ。

- 15 個のログ: ビルドディレクトリ `<絶対パス>/native` を `<WORKDIR>` へ置換。各 `receipt.json` の sha256 は置換後の内容で再計算した。
- 4 個の独立レビュー生ログ（`codex review` の標準出力）: 削除。ログに作成者のホーム配下のパス、
  エージェント用の個人メモ（memory ファイル）の抜粋、スキル定義ファイルの全文が含まれていたため。
  レビュー結論は各 `receipt.json` の `review` / `independent_review` と README に残る。receipt の該当 hash 行は削除した。

`receipt.json` を持たない `activation-java.log` は `activation-receipt.json` が名前だけで参照し、hash は持たない。

一部の receipt（`entity-fixture`、`game-tick-periods`、`stored-durations`）は、リポジトリに存在しないファイル名で
redaction 前のログと同じ hash を保持している。これらは元から参照先が無いので変更していない。

## 変更前後の sha256

| ファイル | 変更前 | 変更後 |
|---|---|---|
| `activation-java.log` | `11a984d15598045a9c98b88fd799ca600e22a988431f58564f7d292b3a16083a` | `bb7845fd0bd610f11b1e6ab137cf5ad1e0782c52cd04e5bf971ff543ab4fe472` |
| `cadence-owner/build.log` | `faeb413027f6f0bf57c4e86199aaa184644bd92d1a016599156204423768e0e1` | `771dc8235d5aa5f3f7124cb59f5b48221e91d92969afafc7e1e574a60b6b57e3` |
| `cadence-owner/time-contract-cadence-clock-negative.log` | `a8f96938bd7504610bf44e4dcbcaf4021cc43e7541e703c96a587e08a1117aa8` | `055db8600b015881dd13024e2ec63f90bb748c2a19f1abb12cd67d4c6cab2ee5` |
| `cadence-owner/time-contract-cadence-features.log` | `b6476168e3defde8fa371e3353d33d385edf30dacf0268b09d6b08a10208e65c` | `7c8a93de22b775c666a8d2cdb119823ee15d7019eb39650a7bd0ad41672fb0dd` |
| `cadence-owner/time-contract-cadence-review.log` | `802ba398cd81b7fe98c33cf80a6292af4a256b9ff8efc1d410320b9f56b53851` | 削除 |
| `cadence-owner/time-contract-cadence-suite.log` | `e6597a577697cec756d14cfe0f2a8d9d15548a0e54ae058a512212376c5c4332` | `bf4d3a0baa3e1c24c97d04e4b1f84119dcf6a5a4dbff7444830592153bcf0e96` |
| `cadence-owner/time-contract-cadence-wire-negative.log` | `5281844d9470b5af3e47bfd8342335e3cce907339c6c0c9a3832bec69d789667` | `3043346e905dfc1e119de11ddabfca3e0e3ca9b1647a1b2c1bc52e835beb37c1` |
| `region-expansion/after/build.log` | `273af9919d8e3d6ffd925ab73462ec9d549e82667ec0258a74a39fb211f680cc` | `037171cc292802448eb61b873808f5874003642b1fdae07c72829d86fc5e253e` |
| `region-expansion/before-test.log` | `61c79ebcd9ef2046648a046d8ed0ca7c0e9ba6431f2558828633ac4839db22ae` | `4851dba5572b2cbd60dca7702ab0d89e3c8f011e466ac4f16c8ba1f905196926` |
| `region-expansion/before/build.log` | `5c3435dc5c30d8fa7a8ef4feb0fac090a8d0f769378c864ca24b4d9db963c821` | `f0ceecd4c56136a844ce15eed95fcf8e7cbe2aca767ba684e3d66825e7509f50` |
| `region-expansion/features.log` | `5a40a3691af350a35735f370fd5f086be6ecde4c8af17a296b663bb2b8a89c99` | `987550adc465a203275505140159cd658227a4dda970d8e064a99595d4f87a50` |
| `region-expansion/negative-control.log` | `bae6c4e959d8c77f4d46c95e5910f34314bd3edf4570f77fceebd693f010da15` | `a317ba74b2a3e782b83f29c4eeb3a925c0500783989621a90d05c8025ff92185` |
| `region-expansion/parallel-suite.log` | `992a0d2db859dd5feef1a167725924d5e6a76eb369970d3b74ecbeb2b954938d` | `a7fcde5f6e7fb5acd3860310b4b8c62543a7fabbdc395342c151a73a6835e7f5` |
| `region-expansion/review-before.log` | `1cd55bd387b3c407f6639d243366cde97beba85d69f8f2704a289e93bf83f651` | 削除 |
| `region-expansion/review.log` | `cafa330ed740e8347a7ba5586e388befb25d4328877b8bc3373d9347724bae36` | 削除 |
| `tick-baseline/features.log` | `8f4ee20742966ff7ad02a5fc63b3bce755f987d3540caee846712ef6ea80b14d` | `bc40a77a0ed23710af4b1fbd64ec0a61eab0e2609288d5bb596943a403167101` |
| `tick-baseline/negative-control.log` | `ea953ce63fac323c0158e7a8a56b966416bc9eee3117b78ddfa33a59798ba1a2` | `0ea24cbc2a1059f953fdecc800f755444c0a0e636e6d7fdb1057c8a0ef5eea3a` |
| `tick-baseline/review.log` | `eea6cd4cc9e32b1d5cf0e7315dc1fa950ae247bd3fd3223d80f0946769038dbb` | 削除 |
| `tick-baseline/suite.log` | `3f0a24ddece65963b7fcb4b127c232dca99177dd10b39f3eb5619b2fd8eb0949` | `cb36421de970bac1f3fda395ed35b1db746e5c630171f44be8a471008b9a834e` |
