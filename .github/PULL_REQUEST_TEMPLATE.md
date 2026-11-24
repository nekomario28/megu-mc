<!-- Thanks for contributing to Megu (megu-mc)! / ご貢献ありがとうございます！ -->

## Summary / 変更概要

<!-- 2–5 lines: what changed and why. / 何をなぜ変更したかを簡潔に記入してください。 -->

## Checklist / チェックリスト

- [ ] I read [CONTRIBUTING.md](../CONTRIBUTING.md) and signed off all commits with `git commit -s` ([DCO 1.1](https://developercertificate.org/)). / DCO 署名（`git commit -s`）済み
- [ ] Clean-room & provenance compliance: no Mojang-derived data, decompiled code, or incompatible (PolyForm / ARR) code is included. / クリーンルーム原則・来歴遵守
- [ ] `python3 tools/run_fast_gates.py` (includes `python3 tools/provenance_check.py --strict-zero-gl`) and `bash tools/debt_ratchet.sh` pass.
- [ ] Affected crate tests and `cargo clippy --all-targets --locked -- -D warnings` pass with 0 warnings.

## Verification / 検証結果

<!-- Commands run and their results. / 実行したテストコマンドと結果を記入してください。 -->
