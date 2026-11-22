<!-- Thanks for contributing to Megu (megu). Keep PRs small and focused. -->

## Checklist

- [ ] I have read [CONTRIBUTING.md](../CONTRIBUTING.md) and agree to the DCO.
- [ ] I signed off every commit (`git commit -s`) — see [DCO](https://developercertificate.org/).
- [ ] This change respects the clean-room / provenance policy: no Mojang-derived assets, decompiled code, or `*_1211*` generated files are included.
- [ ] `python3 tools/provenance_check.py` passes.
- [ ] `bash tests/verify.sh` passes on this branch.
- [ ] For Rust changes: `cargo clippy --manifest-path <crate>/Cargo.toml --offline --locked --all-targets -- -D warnings` is clean (0 warnings).
- [ ] No new wall-clock reads (`Instant::now` / `SystemTime::now`) in production code — determinism is a release gate.
- [ ] Commit messages explain *why*, not just *what*.

## What changed and why

<!-- 2–5 lines: motivation, approach, and any trade-offs. -->

## Test evidence

<!-- Paste the relevant gate output or link the CI run. -->
