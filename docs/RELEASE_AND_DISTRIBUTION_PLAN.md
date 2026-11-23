# Release & Distribution Plan — Megu (megu)

`v0.1.0-alpha.1` 公開に向けた、ライセンス・配布経路・公式サイト・
スポンサーシップ・ローンチチェックリストの実践ガイド。
対象読者は日本在住の学生開発者。英語の技術用語はそのまま使う。

関連ドキュメント:

- [release-checklist.md](release-checklist.md) — ゲート実行の正本（本書はその上位計画）
- [LICENSING.md](../LICENSING.md) — 現行ライセンス決定の記録
- [SECURITY.md](../.github/SECURITY.md) — 脆弱性報告ポリシー
- [website/](../website/) — 既存のランディングサイト（Cloudflare Pages 前提）

---

## 1. License Recommendation — `LGPL-3.0-or-later`

### 現状（2026-11-22 統一済み）

[LICENSING.md](../LICENSING.md) およびルート `Cargo.toml` を含む全マニフェストは、
2026-11-22 に **`LGPL-3.0-or-later`** 単一表記へ統一済みです（2026-11-20 時点の
トリプルライセンス移行経路を経て確定）。

### 推奨: 本体を `LGPL-3.0-or-later` に統一する

`megu` は **mod エコシステム上で動くエンジン**であり、配布形態としては
「単体サーバー/クライアント」だけでなく「外部 mod JAR を動的ロードする
ホスト」として振る舞う。ここで `GPL-3.0-or-later` を選ぶと、外部 mod 作者が
「自分の MIT/Apache-2.0 mod が GPL エンジン上で動くとライセンス感染するのでは」
と警戒し、Fabric/NeoForge エコシステム（Sodium=MIT、Lithium=LGPL-3.0、
Horizon=GPL-3.0、C2ME=MIT、NeoForge=LGPL-2.1、Fabric=Apache-2.0）との
相性が悪化する。

`LGPL-3.0-or-later` を推す理由:

1. **エコシステム整合** — Sodium (MIT), Lithium (LGPL-3.0), C2ME (MIT),
   Starlight, FerriteCore など主要 performance mod と同じ「weak copyleft」
   レイヤーに立てる。mod 作者の心理的・法的ハードルが最も低い。
2. **エンジン fork への強い copyleft は維持** — `megu` 本体や
   `megu-core` の改変・再配布には依然 LGPLv3 のソース公開義務がかかる。
   「クローズド商用 fork による持ち逃げ」は防げる（GPL との実質的差は
   「リンク/ロードされる側への波及」のみ）。
3. **外部 mod JAR への非波及をライセンス条文で明確化** — LGPLv3 §4/§5 と
   「mere aggregation / dynamic loading」の解釈により、MIT/Apache/BSD/ARR
   ライセンスの外部 mod がカスタム linking exception なしで実行可能。
   現行の「Fabric/NeoForge 互換レイヤ経由で動的ロードされる独立 JAR には
   コピーレフトを波及させない」という設計意図と条文レベルで一致する。
4. **GPLv3 との双方向互換** — LGPLv3 §3 により LGPL-3.0-or-later コードは
   GPLv3 プロジェクトへ取り込める。将来 GPL-3.0-or-later 側のコンポーネントと
   統合する場合も障害にならない（逆も LGPL 側が GPL を取り込む場合は
   GPL 側の条件が優先される点に注意）。
5. **将来の再ライセンス自由度** — `-or-later` により、コントリビューターの
   同意なしに LGPL-3.0 → GPL-3.0-or-later 等への移行判断が可能
   （トリプルライセンス寄稿と組み合わせて最大の柔軟性）。

### 注意点・やるべきこと（メンテナ判断事項）

- 現行 `LICENSE` は GPLv3 本文。LGPL への変更は `COPYING.LESSER` を
  `LICENSE` 相当に昇格させる作業であり、**本レーンのスコープ外**
  （`LICENSE`/`Cargo.toml`/`deny.toml`/`LICENSING.md` は編集禁止）。
  実施する場合は別レーンで `LICENSE` 差し替え + `Cargo.toml` license
  field + `LICENSING.md` 更新 + `NOTICE` 確認を一括で行う。
- `GPL-3.0-or-later` 据え置きも選択肢として残る。その場合は
  「外部 mod は別プロセス/別ライセンスのプラグインとして扱う」設計を
  ドキュメントでより明確にする必要がある。
- AGPL-3.0-or-later はマネージドホスティング（SaaS 的提供）まで
  ソース公開を求めるため、コミュニティ mod エコシステムには過剰。
  トリプルライセンスの選択肢として残すのが妥当。

---

## 2. Distribution Channels（配布経路）

### 2.1 Launcher Thin JAR Bootstrapper — **第一配布経路（#1）**

`tools/client_launcher/` の thin Java bootstrapper
（`megu-bootstrapper.jar`、**32 KiB 未満**・再現ビルド可能）は、
プレイヤーが普段使うランチャーの Microsoft/Minecraft アカウント
ログインと Mojang 正規アセットをそのまま使って `megu` / Megu を
起動する、Fabric / NeoForge と同じ「ワンクリック導入」UX を実現する
**最優先の配布経路**。対応ランチャー:

- **Official Minecraft Launcher** — `versions/<id>/<id>.json` プロファイル
  （`inheritsFrom` + `mainClass: net.megu.launcher.Main`）を生成。
  バニラと同じアカウント・アセット解決がそのまま効く。
- **Prism Launcher / MultiMC** — `patches/net.megu.bootstrapper.json`
  カスタムパッチで `mainClass` を差し替える**ネイティブ patch export**
  （`--bootstrap-jar`）。シェル wrapper 不要で Windows / macOS / Linux
  全てで動作。`mmc-pack.json` + `instance.cfg` の従来 export も維持。
- **Modrinth App** — `.mrpack`（`modrinth.index.json`）を export。
  `overrides/` に統合 JAR と native library を同梱。

対応 Minecraft バージョンは `1.21.1` と `26.3`（`--mc-version`）。

#### Operator / release 手順

```bash
# 0) OS 標準の .minecraft ディレクトリを確認（読み取りのみ・実ホームは変更しない）
python3 tools/client_launcher/gen_launcher_profile.py --detect-minecraft-dir
#   Linux:   ~/.minecraft
#   Windows: %APPDATA%\.minecraft
#   macOS:   ~/Library/Application Support/minecraft

# 1) 再現ビルド可能な bootstrapper JAR（<32 KiB をビルド時に強制）
for V in 1.21.1 26.3; do
  python3 tools/client_launcher/gen_launcher_profile.py \
    --out-dir dist/boot-$V --mc-version $V --build-jar
done

# 2) Official Minecraft Launcher 用プロファイル（versions/ + libraries/ に配置）
MC_DIR=$(python3 tools/client_launcher/gen_launcher_profile.py --detect-minecraft-dir)
for V in 1.21.1 26.3; do
  python3 tools/client_launcher/gen_launcher_profile.py \
    --out-dir "$MC_DIR" --mc-version $V --build-jar
  # 任意: --client-bin /<native client> --client-sha256 <sha256> で native を pin
  # 任意: --version-id megu-$V でプロファイル ID を変更
done
# → ランチャーのバージョン選択に "megu-1.21.1" / "megu-26.3" が出る

# 3) Prism Launcher インスタンス（ネイティブ patch・シェル wrapper 無し）
for V in 1.21.1 26.3; do
  python3 tools/client_launcher/export_launcher_bundle.py \
    --format prism --mc-version $V \
    --bootstrap-jar dist/boot-$V/megu-bootstrapper.jar \
    --client-bin target/release/megu-client \
    --client-sha256 "$(sha256sum target/release/megu-client | cut -d' ' -f1)" \
    --out-dir dist/prism-$V
done
# → Prism のインスタンスフォルダにコピーして起動（patches/ が mainClass を差し替え）
#   --bootstrap-jar 無しの従来 export は megu-wrapper 経由（互換維持）

# 4) Modrinth App 向け .mrpack（loader はバージョンごとに要確認）
python3 tools/client_launcher/export_launcher_bundle.py \
  --format mrpack --mc-version 1.21.1 \
  --loader fabric --loader-version 0.16.10 \
  --integrated-jar dist/megu-integrated.jar \
  --out-dir dist/mrpack-1.21.1
python3 tools/client_launcher/export_launcher_bundle.py \
  --format mrpack --mc-version 26.3 \
  --loader neoforge --loader-version <26.3 対応版> \
  --integrated-jar dist/megu-integrated.jar \
  --out-dir dist/mrpack-26.3
```

`--accessToken` 等の認証情報は argv に残す既定動作を維持しつつ、
`--megu-token-env` / `MEGU_TOKEN_ENV=1` 指定時は子プロセスへ
`MEGU_LAUNCHER_ACCESS_TOKEN` 環境変数経由で渡し、ログ・manifest に
一切残さない（`redact()` が `--accessToken` / `--authToken` /
`--clientToken` / `--token` と Bearer トークンを常時マスク）。
Windows `.exe` 名（`megu-client-gpu.exe` 等）も `--gameDir` /
`--gameDir/bin` / `PATH` から自動検出する。

### 2.2 GitHub Releases（一次配布）

[.github/workflows/release.yml](../.github/workflows/release.yml) が
既に 8 バイナリのクロスプラットフォームビルドとアーカイブ検証を持つ。
公開時の成果物セット:

| Platform | Artifact | 備考 |
|---|---|---|
| Linux x86_64 | `megu-<ver>-x86_64-unknown-linux-gnu.tar.gz` | `tools/release_bundle.py` の決定的 tarball |
| Linux arm64 | `megu-<ver>-aarch64-unknown-linux-gnu.tar.gz` | 同上 |
| Linux x86_64 | `megu-<ver>-x86_64.AppImage` | AppImage 化（任意・後追い可） |
| Windows x86_64 | `megu-<ver>-windows-x86_64.zip` | `.msi` は WiX/インストーラ整備後に検討 |
| macOS Universal | `megu-<ver>-macos-universal.dmg` | arm64+x86_64 universal binary |

- 各リリースに `SHA256SUMS` と `release-manifest.json` を必ず添付
  （`release_bundle.py verify` が bytes レベルで再検証する）。
- **SBOM**: `cargo-sbom` / `cargo-cyclonedx` で CycloneDX SBOM を生成し
  `megu-<ver>.cdx.json` として添付する（サプライチェーン透明性、
  `deny.toml` の advisory 情報と整合）。
- Companion binaries（`megu-client`, `megu-mod-check`,
  `megu-world-migrate`, `megu-init`, `megu-admin`,
  `megu-client-gpu`, `megu-installer`）は
  `--extra-binary` で同一 tarball に同梱する（H18 で対応済み）。
  ※同梱 8 バイナリの削減ポリシーは zero-bloat 監査 #4 の未決事項。
- コンテナイメージ `megu:<ver>` は GHCR (`ghcr.io/<owner>/megu`)
  へ push。Docker Hub は学生アカウントでも可だが GHCR が無料・連携が楽。

### 2.3 Modrinth

- **Server software listing**: Modrinth は server-side software も
  ホストできる。`megu` を "server software" として登録し、
  GitHub Releases の各プラットフォーム artifact を versions として紐付け。
  launcher からワンクリック導入できる導線になる。
- **Modpack companion**: `megu` 対応を謳う modpack は
  `megu-mod-check` の pre-flight レポートを description に貼る。
- **Launcher listing**: Modrinth App / ATLauncher 系 launcher での
  一覧掲載を目指す。掲載要件（changelog, license 表記, provenance）は
  事前に Modrinth のガイドラインを確認。

### 2.4 Package managers

| Channel | Package | 状態 |
|---|---|---|
| AUR | `megu-bin` | Arch Linux ユーザーに最も刺さる。PKGBUILD は GitHub Releases tarball を fetch して `SHA256SUMS` 検証するだけの薄いラッパーでよい |
| Homebrew | `homebrew-tap` (`<owner>/homebrew-megu`) | macOS/Linux。`brew install <owner>/megu/megu` 形式。tap リポジトリは別 repo でよい |
| Winget | `microsoft/winget-pkgs` への manifest PR | Windows ユーザー向け。`winget install <Publisher>.megu`。manifest は YAML で portable + installer 両対応 |
| Scoop | `ScoopInstaller/Extras` or 独自 bucket | Windows 代替。AUR と同様に tarball fetch 型で簡単 |
| Flathub | `dev.<domain>.Megu.yml` metainfo | Flatpak 化は `org.freedesktop.Platform` runtime + バイナリ同梱。審査に数週間かかるため後追いでよい |

優先順位: **Launcher Thin JAR Bootstrapper（§2.1、全ランチャー共通の
第一導線）→ GitHub Releases → Modrinth → AUR/Homebrew → Winget/Scoop →
Flatpak**。bootstrapper がプレイヤー導線の入口、binary release と
package manager はその裏側の配布網として機能する。

---

## 3. Website Plan — 0 円運用の公式サイト

### ホスティング選定

| Option | Cost | CDN | Custom domain | 備考 |
|---|---|---|---|---|
| **Cloudflare Pages** | ¥0 | Global | ✅ free | `website/wrangler.toml` が既に Pages 前提。Functions で `/api/report` crash relay も実装済み。**推奨** |
| **GitHub Pages** | ¥0 | Global | ✅ free | Jekyll 不要・静的 HTML で十分。ただし crash relay API は別途 Worker が必要 |

- ドメインは任意。`megu.pages.dev` / `megu-site.pages.dev` で十分。
  こだわるなら `megu.gg` / `megu.dev` 等を Cloudflare Registrar
  （原価販売）で年 ¥1,500 前後。
- HTTPS/証明書/CDN は両者とも自動。アクセス解析は Cloudflare Web
  Analytics（無料・Cookie なし）で十分。

### 推奨ページ構成（既存 `website/index.html` の延長）

1. **Hero + instant download matrix** — OS/arch を選ぶと該当 artifact
   へのリンクと SHA-256 が出る。「Download for Linux x86_64」ボタンを
   ファーストビューに。
2. **Aesthetics gallery** — Vanilla+ / 和風ビジュアルのスクリーンショット
   ギャラリー。テーマ案: 『陰翳礼讃』(In Praise of Shadows) の陰影表現、
   侘び寂び、桜・紅葉・四季の光。`assets/` の screenshot を整備して
   `<figure>` + caption で並べる。EN/JA キャプション両対応。
3. **Mod compatibility checker showcase** — `megu-mod-check --json`
   の出力例（`native_bundle` / `auto_skipped` / `tier2_lifted_shim` /
   `tier3_fallback_required`）をインタラクティブに見せるデモ。
   「あなたの modpack は動くか?」を 30 秒で体験させる導線。
4. **EN/JA toggle** — `<html lang>` 切替 or URL `?lang=ja`。
   既存 `site-config.js` の単一ソース設定パターンを踏襲し、
   文言は `site-config.js` に集約する。
5. **Docs & sponsor CTA** — `docs/` へのリンク、GitHub Sponsors /
   Ko-fi / BOOTH へのバナー（`.github/FUNDING.yml` と整合）。

### 技術方針

- **Astro または plain HTML+CSS+JS** — ビルド不要の plain HTML で十分
  （現行 `website/` がまさにそれ）。Astro を選ぶなら islands 構成で
  gallery の lightbox 程度に留める。
- 既存 `website/functions/api/` の crash relay (`/api/report`,
  `/api/latest`, KV `CRASH_INDEX`, Turnstile) はそのまま活用。
  `wrangler pages deploy website` でデプロイ。
- SEO: `sitemap.xml`, OGP 画像, `<title>`/`description` を日英両対応で。

---

## 4. Sponsorship Plan — 日本在住学生の支援経路

OSS を続けるための支援を、日本から学生でも低手数料で受け取れる
現実的な経路を比較する。**全て初期費用 ¥0**。

### 4.1 手数料・受取方法の比較

| Service | Platform fee | 決済手数料 | 出金方法 | 向いている用途 |
|---|---|---|---|---|
| **GitHub Sponsors** | **0%** (個人) | Stripe Express / Wise 経由 | 日本の銀行口座, PayPal | OSS 開発者向け最安・最信頼。repo に Sponsor ボタンが出る |
| **Ko-fi** | **0%** | PayPal/Stripe 約 3.6–4.1% + ¥40 | PayPal or Stripe 経由で銀行 | 海外（特に英語圏 Minecraft コミュニティ）からの単発投げ銭 |
| **BOOTH** | **5.6% + ¥45**/件 | 決済手数料込み | **日本の銀行口座へ直接出金** | 国内ファン・「応援購入」。無料配布+BOOST も可能 |
| **pixiv FANBOX** | 約 10% 前後 | プランにより変動 | 日本の銀行口座 | 国内クリエイター・月額支援。手数料は高め |
| **OFUSE** | 約 10% | 同上 | 日本の銀行口座 | pixiv 系の投げ銭。単発向き |
| **Buy Me a Coffee** | 5% | 決済手数料別 | Stripe 経由 | Ko-fi の代替。GitHub Sponsors が主軸なら不要 |
| **Patreon** | 約 8–12% | 同上 | PayPal/銀行 | 海外月額。初期は GitHub Sponsors で十分 |

### 4.2 推奨構成（最小 2〜3 本立て）

```
GitHub Sponsors  ← メイン。手数料 0%、repo に自然に溶け込む
Ko-fi           ← 海外単発投げ銭。Ko-fi Shop で壁紙/称号も売れる
BOOTH BOOST     ← 国内向け。無料配布 mod/resourcepack に「BOOST」ボタン
```

- GitHub Sponsors が個人 0% なのは圧倒的。まずここを開設。
- Ko-fi は PayPal/Stripe 連携で海外ユーザーが摩擦なく投げられる。
- BOOTH は国内の「推し活」文化に合い、銀行口座直結で学生にも優しい。
  BOOST 機能で「無料で配布しつつ投げ銭だけ受け取る」形が取れる。

### 4.3 日本在住学生の実務チェックリスト

- [ ] **年齢要件**: GitHub Sponsors は 18 歳以上 + Stripe アカウントが
      必要。18 歳未満または学生は保護者の同意・名義での口座開設が必要な
      場合がある。PayPal も 18 歳以上が原則。
- [ ] **口座名義**: 受取口座は原則「本人名義」。親名義の口座を使うと
      プラットフォームの KYC で止まる可能性がある。親の協力が必要なら
      最初に確認。
- [ ] **本人確認 (KYC)**: Stripe / PayPal / BOOTH 全てで身分証明書
      （免許証・マイナンバーカード・パスポート等）のアップロードが
      求められる。早めに済ませておく。
- [ ] **税金・扶養の目安**: 日本では雑所得の年間合計が **20 万円を超える**
      と確定申告が必要になるのが一般的な目安。また親の扶養控除
      （特定扶養親族は所得 48 万円以下が目安）に影響する可能性がある。
      ※これは一般的な情報であり、最終的な判断は**必ず税務署または
      保護者・税理士に確認**すること。
- [ ] **開設タイミング**: GitHub Sponsors は repo に `.github/FUNDING.yml`
      を置くだけでボタンが出る。コードが動く段階（v0.1.0-alpha.1）で
      開設し、「開発中です」と正直に書く。
- [ ] **特商法表記**: 継続的に販売（BOOTH での有料物販）を行う場合は
      特定商取引法に基づく表記が必要になる場合がある。投げ銭・BOOST・
      寄付のみなら不要なケースが多いが、売上が出始めたら確認。

### 4.4 文面テンプレート（寄付ページ用）

```
Megu (megu) は日本の学生が一人で開発している
Minecraft 互換エンジン（Rust 製）です。

サーバー運営・mod 開発・配布の費用（ドメイン・CI・テスト環境）を
支援していただけると幸いです。寄付は任意で、
機能やロードマップへの影響はありません。

If you enjoy Megu, consider supporting development.
Donation is optional and does not affect features or roadmap.
```

---

## 5. v0.1.0-alpha.1 Launch Checklist

[release-checklist.md](release-checklist.md) の全工程に加え、
**タグを打つ前**に以下を全て green にする。全て fail-closed。

### 5.1 Pre-tag gates（同一 release-candidate SHA で実行）

- [ ] `bash tests/verify.sh` — 全ゲート PASS
      （`--without-mutations` は mutation campaign のみ skip、
      他は全て実行される）
- [ ] `python3 tools/wiring_audit.py --strict --tolerated-unwired
      tools/wiring_audit_unwired.json` — **17/17 wiring contracts wired,
      0 pending** は達成済み。残る条件は unwired モジュールが全て
      tolerated list 内であること（staged モジュールの新規 unwired
      または consumer 着地による stale exemption で fail しないこと）
- [ ] `python3 tests/vanilla_live_diff_test.py` — live oracle **18/18 MATCH**
- [ ] Clippy 全クレート 0 warnings（[release-checklist.md §1](release-checklist.md) の
      standalone loop、`-D warnings`）
- [ ] `python3 tools/provenance_check.py --strict-public` — clean export
      で Mojang 派生物ゼロを証明
- [ ] `cargo check --bins --offline --locked` — stripped public export で
      バイナリがビルドできることを確認
- [ ] License 整合: `LICENSE` / `COPYING.LESSER` / `COPYING.AGPL` /
      `Cargo.toml` license field / `NOTICE` / `deny.toml` が
      LICENSING.md の決定と一致（LGPL 移行を行う場合は本レーン外の
      別レーンで実施済みであること）
- [ ] `cargo deny check` — advisory + license + source 全パス
- [ ] Security audit sign-off — `.github/SECURITY.md` 記載スコープ
      （protocol DoS / mod sandbox / save corruption / crash redaction）
      の自己レビュー完了

### 5.2 Build & package

- [ ] `python3 tools/export_public_branch.sh --out ../megu-public --verify`
- [ ] `python3 tools/release_bundle.py build --binary target/release/megu
      --out-dir dist --version 0.1.0-alpha.1 --target <triple>`
      + companion `--extra-binary`（client/mod-check/world-migrate/init/gpu）
- [ ] `python3 tools/release_bundle.py verify` — manifest + SHA256SUMS +
      per-member records 一致、purity guard 通過
- [ ] CycloneDX SBOM 生成・添付
- [ ] `docker build -f docker/Dockerfile -t megu:0.1.0-alpha.1 .`
      + EULA negative test 2 件

### 5.3 Publish

- [ ] `git tag -s v0.1.0-alpha.1`（署名推奨）→ push
- [ ] GitHub Release 作成（draft → 公開）。Notes に:
  - clean-room statement（Mojang assets 非同梱、operator が
    `tools/vanilla_data/gen_vanilla_data.sh` で再生成）
  - 対応 MC version（1.21.1 LTS / 26.3）
  - 既知の制限（bounded parity audit 結果、Wave 3 未検証など
    [release-checklist.md §7](release-checklist.md) の表現をそのまま使用）
  - SBOM + SHA256SUMS + manifest リンク
- [ ] GHCR へコンテナイメージ push
- [ ] Modrinth server software listing 作成（GitHub Releases と紐付け）
- [ ] AUR `megu-bin` PKGBUILD 提出（任意タイミング）
- [ ] Homebrew tap / Winget manifest / Scoop bucket（漸進的）

### 5.4 Website & community

- [ ] `wrangler pages deploy website`（Cloudflare Pages）で本番デプロイ
- [ ] GitHub Sponsors 開設 + `.github/FUNDING.yml` の `github:` コメント解除
- [ ] Ko-fi 開設 + `ko_fi:` コメント解除
- [ ] BOOTH 開設（BOOST 用）
- [ ] Discussions カテゴリ整備（Help / Mod compat / Ideas）
- [ ] `.github/ISSUE_TEMPLATE/*` が意図通りレンダリングされるか確認
- [ ] 日本語 README セクション or `README.ja.md` 追加検討

### 5.5 Post-launch

- [ ] 初週の issue triage 方針を決める（label: triage → bug/feature/mod-compat）
- [ ] `megu-mod-check` の出力を issue template で収集し、
      adapter の対応範囲を data-driven に拡張
- [ ] analytics（Cloudflare Web Analytics）で download 数・流入元を確認
- [ ] 月次で `cargo audit` / `cargo deny` を回し、advisory に対応

---

## 付録: 用語対訳

| English | 日本語 |
|---|---|
| Distribution channel | 配布経路 |
| Release bundle | リリースバンドル |
| Clean-room | クリーンルーム（Mojang 資産を含まない独立実装） |
| Copyleft | コピーレフト（派生物にも同一ライセンスを要求） |
| Dynamic loading | 動的ロード |
| Link exception | リンク例外条項 |
| Wire protocol | ワイヤプロトコル |
| Parity audit | パリティ監査（Vanilla との一致度検証） |
| Sponsorship / donation | 支援 / 寄付 |
| Throwaway account | 使い捨てアカウント（非推奨） |
| KYC (Know Your Customer) | 本人確認 |
| Withholding tax | 源泉徴収 |
| Flatpak / Flathub | Linux サンドボックスパッケージ形式 / そのストア |
| AUR (Arch User Repository) | Arch Linux のコミュニティパッケージリポジトリ |
| Homebrew tap | Homebrew のサードパーティ formula リポジトリ |
| Winget / Scoop | Windows パッケージマネージャ |
| SBOM (Software Bill of Materials) | ソフトウェア部品表 |
