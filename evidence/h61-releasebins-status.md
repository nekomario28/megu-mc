# H61 — release binary consolidation status

Date: 2026-11-23. Scope: `.github/workflows/release.yml`, `tools/` packaging,
and the small `src/bin/*.rs` entry points only. No `src/` or `client-*/` file
was touched; no manifest was touched; nothing was committed.

## Disposition per shipped binary

| Binary | Release size (bytes) | Disposition |
| --- | ---: | --- |
| `megu` | 16,891,920 | KEEP — dedicated server, core UX. Its CLI lives in `src/main.rs`, which this task forbids touching, and it has no admin/migration subcommand surface today. |
| `megu-client` | 3,742,464 | KEEP — headless client, core UX per task. |
| `megu-client-gpu` | 14,768,304 | KEEP — GPU client, core UX per task. |
| `megu-installer` | 620,352 | KEEP — installer/launcher UX per task. |
| `megu-mod-check` | 1,514,224 | KEEP as real binary — NOT consolidated. Its entire CLI (~1,614 lines) lives in `mod-adapter-core/src/bin/mod_check.rs`; folding it into `megu <subcommand>` requires `src/main.rs` dispatch (forbidden) and the root crate does not depend on `megu-server-files`, while `server-files` does not depend on `mod-adapter-core`. No allowed file can host it. |
| `megu-world-migrate` | 844,976 | KEEP as real binary — NOT consolidated. Same boundary: CLI is `state-store/src/bin/world_migrate.rs`; the root `megu` already links `megu-state-store` under `mod-suite` but has no subcommand dispatch, and `src/` is off-limits. |
| `megu-init` | 778,400 | CONSOLIDATED (host). Now contains the admin implementation in-process via `#[path = "server_init.rs"] mod admin;` and runs `init|doctor|op|deop|ban|pardon|whitelist|claims|help` directly instead of re-exec'ing `megu-admin`. Also accepts the internal `admin` prefix used by the shim. Bootstrap CLI (`--dir`, `--accept-eula`, `--force`, `--dry-run`, `--port`, …) is unchanged. |
| `megu-admin` | 365,264 | CONSOLIDATED (thin shim). `server_init.rs` is still the `megu-admin` bin target, but its `main` is now only a sibling launcher: it execs `megu-init admin <args>` with `MEGU_ADMIN_PROG_NAME=megu-admin` so help/error text still says `megu-admin`. The admin implementation is `pub fn run` inside the same file, reused by `megu-init`; the release build strips the unreferenced copy (365,264 bytes vs 778,400 for the host). Requires `megu-init` next to it — the same sibling relationship `megu-init` already had with `megu-admin`, now inverted. |

## Why not `megu <subcommand>` for the tooling four

The requested end state (`megu mod-check`, `megu world-migrate`, `megu admin`,
`megu init`) needs a dispatch table in the root binary. That table can only
live in `src/main.rs` (or a new root `[[bin]]`/`src/bin` target), all of which
are outside the allowed edit set (`src/` is explicitly forbidden and
`Cargo.toml` is not an allowed file). The three non-server-files tools also
sit in three different standalone crates with no shared CLI crate, so no
allowed `main.rs` can host them without new dependency edges.

`release.yml` was therefore left unchanged: all eight binaries are still
built and packaged, preserving the static contract in
`tests/release_workflow_test.py` and `tests/release_bundle_test.py` (which
this task does not allow editing). The consolidation that WAS safe — the
`server-files` init/admin pair — is implemented as a thin shim, exactly the
fallback the task allowed.

## Changes

- `server-files/src/bin/server_init.rs`
  - `fn main` → `pub fn run(args)` (implementation, argv[0]-excluding).
  - New `megu_admin_shim()` + `fn main`: `megu-admin` execs sibling
    `megu-init admin …` and forwards `MEGU_ADMIN_PROG_NAME=megu-admin`.
  - `prog_name()` honours `MEGU_ADMIN_PROG_NAME` (basename-safe) so shim
    help/errors keep the `megu-admin` spelling.
- `server-files/src/bin/megu_init.rs`
  - `#[path = "server_init.rs"] mod admin;` links the admin implementation
    in-process (no code duplication in source).
  - `legacy_command` calls `admin::run` directly instead of spawning
    `megu-admin`; new `admin` first-arg prefix routes the shim's calls.
  - Removed the `std::process::Command` import (no longer re-execs).

## Verification (all `--offline`, exit 0)

- `cargo build --locked --offline --manifest-path server-files/Cargo.toml --bins`
- `cargo build --locked --offline --release --manifest-path server-files/Cargo.toml --bins`
- `cargo build --locked --offline --release --manifest-path mod-adapter-core/Cargo.toml --bin megu-mod-check`
- `cargo build --locked --offline --release --manifest-path state-store/Cargo.toml --bin megu-world-migrate`
- `cargo test --locked --offline --manifest-path server-files/Cargo.toml` — all suites pass, including `megu_init_cli.rs` and `server_init_cli.rs` (admin commands now run in-process inside `megu-init`).
- Help surfaces: `megu --help`, `megu-mod-check --help`, `megu-world-migrate --help`, `megu-installer --help`, `megu-init --help`, `megu-init admin --help`, `megu-admin --help` all print their expected usage (`megu-admin --help` titles itself `megu-admin`).
- End-to-end: `megu-init --dir <tmp> --dry-run --json` → planned create actions; `megu-init init --dir <tmp> --json` → `{"action":"init","mc_version":"26.3"}`; `megu-admin doctor --dir <tmp> --json` → `{"ready":false,"exit_code":2}` (EULA not accepted, correct).

## Residual gap

The archive still contains eight binaries. Only the `megu-admin`/`megu-init`
pair was safely consolidable inside the allowed file set; the remaining
count reduction requires either `src/main.rs` subcommand dispatch plus
manifest/dependency changes, or test/manifest updates that are outside this
task's allowed paths.
