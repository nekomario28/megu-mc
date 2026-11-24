# H19 mesh residency status

Status: safe queued-upload comparison optimization implemented; packed-mesh
residency eviction remains unimplemented.

## Implemented

`WorldState` computes and stores a `u64` digest alongside each newly packed
section mesh. `PendingSectionUpload` carries that digest, and `matches()`
compares it before performing the full packed-byte comparison on a digest hit
and comparing the translucent order. Hash collisions therefore cannot
suppress a required upload.

## Why packed-mesh eviction was not added

- `WorldState::release_uploaded_section_cpu_copies` currently releases both
  the packed vertex copy and opaque source quads after upload. The GPU becomes
  the only complete geometry copy. Those sections cannot be repacked from the
  remaining translucent-only mesh cache data.
- Preserving opaque quads would make repacking possible, but would retain the
  larger source geometry alongside the GPU copy and defeat the current
  post-upload CPU release contract. Evicting only packed bytes after changing
  that contract does not establish a safe memory-working-set improvement.
- The current client path has no on-demand section re-fetch/re-generation API
  for a still-loaded section. The ordinary mesh rebuild path consumes dirty
  cache entries produced by world updates; it does not request missing
  geometry from the server.
- Renderer reconciliation interprets missing packed data as section removal.
  A correct eviction implementation therefore also needs a durable alive but
  evicted state, rehydration scheduling, and an upload/reconcile transition
  that preserves live GPU geometry until regenerated data is available.
  Those semantics are not supplied by the existing release/update contract.

Adding a placeholder residency flag without a complete low-memory source and
rehydration lifecycle would risk either incomplete geometry or silently
retaining GPU sections as if they were removed. No eviction, rehydration
counters, or render-identity claims are made.

## Validation

- Repository revision at inspection: `812412fd26921b34a6140022bae5c81b349a9e48`.
- `cargo test --release --offline -j2 -p megu-client -p megu-client-gpu` was
  rejected because this checkout is not a Cargo workspace.
- `cargo test --release --offline -j2 --manifest-path client-app/Cargo.toml`:
  compiled; 81 passed, 1 unrelated integrated-server runtime-root test failed
  (`find_runtime_root` selected `<repo>` instead of its temporary
  runtime directory). The isolated test reproduced the same failure.
- `cargo test --release --offline -j2 --manifest-path client-gpu/Cargo.toml`:
  blocked at compilation by the pre-existing unresolved import
  `megu_client::session::CHANNEL_MOD_REGISTRY_SYNC` in `client-gpu/src/live_session.rs`.
  An earlier compile attempt also caught and fixed the digest helper's
  unsupported `bytemuck::cast_slice` use; the final attempt no longer reports
  that error.
- No real-GPU render test was run.
