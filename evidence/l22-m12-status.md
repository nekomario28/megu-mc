# L22 / M12 status

Date: 2026-11-23  
Checkout: `/home/yuu/mc-spine`, branch `main`; HEAD observed at final review: `88f33dc1fe775c08721a2dcaf03d26c3de38cbd5`. The shared branch advanced during this task; validation ran against the shared working tree. Final `src/play_protocol.rs` SHA-256: `17c0b450131ef1045162e383ebdab0ad265a4aaa5cf9fd2b6bb45bdcd370bd3d`.

## Changes

- L22: a per-connection `PacketRateLimit` now covers handshake, login, online-mode/Velocity response, and configuration reads. It uses the existing 1,200-packet burst, 500-packet/s refill, and sustained-overage close policy; rejected packets are consumed without dispatch.
- M12: added borrowed-payload fanout paths and moved recipient-independent broadcast payloads (including seen-player, sound, and chunk broadcasts) onto them. Per-recipient encoding remains on the existing closure paths.
- Changes are uncommitted. The implementation change is in `src/play_protocol.rs`.

## Validation

- `cargo check --offline --locked -j2` — PASS.
- `cargo test --release --offline -j2 rate` — PASS, 44 tests.
- `cargo test --release --offline -j2 fanout` — PASS, 3 tests.
- `cargo test --release --offline -j2 broadcast` — PASS, 18 tests.
- `cargo test --release --offline -j2 login` — PASS, 7 tests.
- `git diff --check -- src/play_protocol.rs` — PASS.
- `cargo fmt -- --check` — FAIL: the workspace formatter reports extensive pre-existing formatting differences across unrelated files; no workspace-wide formatting was applied.

These checks establish compilation and focused unit/live-TCP behavior on the tested checkout. They do not establish production flood resistance or a vanilla-client play session.
