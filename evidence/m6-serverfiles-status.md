# M6 wiring audit — server-files/ + state-store/ scope

Date: 2026-11-23. Scope: the six tolerated-unwired modules under
`server-files/` (4) and `state-store/` (2) from
`tools/wiring_audit_unwired.json`. Uncommitted working-tree change.

## Decisions per module

### Wired (option a)

- **`server-files/src/claims_wire.rs`** → wired into `claims.rs`.
  `ClaimsWorld` gained `pending_wire_batches()`, `drain_wire_batches()` and
  `dimension_wire_batches(dimension, max_per_batch)`, thin facades over
  `claims_wire::build_delta_batches` / `build_dimension_sync_batches` —
  exactly the "how the sync feed reaches client overlays" role the codec's
  module doc already claims. Audit edge: `claims.rs -> claims_wire.rs`.
- **`server-files/src/mods_dir.rs`** → wired into `directory.rs`.
  `ServerDirectory::scan_mods(mc_version)` delegates to
  `mods_dir::scan_mods_folder` on `<root>/mods` — the facade that owns the
  server directory now owns its mod discovery too.
- **`server-files/src/pack_loader.rs`** → wired into `directory.rs`.
  `ServerDirectory::world_datapacks_dir()` resolves
  `<level-name>/datapacks` through the existing `validated_level_path`
  fail-closed check, and `load_world_datapack(name)` runs one pack
  directory through `pack_loader::load_validated_pack_dir`. `name` must be
  a single `Component::Normal` path component, so traversal fails before
  any I/O; `PackLoadError::Io` absorbs the `io::Result` from the dir
  resolution via its existing `From<io::Error>`.

### Kept tolerated (option b) — alive but audit-invisible or genuinely staged

- **`server-files/src/installer.rs`** — not dead: all three install lanes
  are driven by `server-files/src/bin/megu_installer.rs`, which
  `wiring_audit.py::source_paths` excludes (`bin` path parts). No lib
  consumer exists or is needed; the tolerated reason was updated to say so.
- **`state-store/src/audit_log.rs`** — consumed by
  `state-store/src/bin/world_migrate.rs` (`recover_audit_log`, rollback /
  restore CLI paths), again a bin the audit excludes. The live-server
  block-mutation tap is still pending; reason updated.
- **`state-store/src/world_interop.rs`** — genuinely staged: the sharded
  store is not yet the world backend (H10 cutover), so no production caller
  exists yet. Kept tolerated; **not** proposed for deletion — it is the
  designated interop surface for that cutover and has dedicated tests
  (`state-store/tests/world_interop_tests.rs`).

### Deletion proposals (option c)

None. No scoped module is superseded.

## Verification

- `cargo test` in `server-files/`: all suites pass (lib + integration +
  doc-tests, exit 0).
- `cargo test` in `state-store/`: all suites pass (exit 0). No state-store
  source was modified.
- `python3 tools/wiring_audit.py --strict --tolerated-unwired
  tools/wiring_audit_unwired.json`:
  - `claims_wire.rs`, `mods_dir.rs`, `pack_loader.rs` now report `wired`
    (edges: `claims.rs -> claims_wire.rs`, `directory.rs -> mods_dir.rs`,
    `directory.rs -> pack_loader.rs`); their tolerated entries were removed.
  - `installer.rs`, `audit_log.rs`, `world_interop.rs` remain `unwired` and
    remain tolerated; `unwired_stale_tolerated` is empty.
  - **Out-of-scope note:** the strict run still exits 1 on four
    `UNWIRED-NEW` modules outside this task's scope —
    `mod-adapter-core/src/live_registry_bridge.rs`,
    `client-app/src/mod_assets_meta.rs`, `client-app/src/server_list.rs`,
    `client-ui/src/death_screen.rs`. These belong to other M6 slices; this
    change neither caused nor touches them.

## Files changed

- `server-files/src/claims.rs` — wire-batch facade methods + import.
- `server-files/src/directory.rs` — `scan_mods`, `world_datapacks_dir`,
  `load_world_datapack` + imports.
- `tools/wiring_audit_unwired.json` — dropped 3 wired entries, clarified 2
  tolerated reasons (13 → 10 entries).
