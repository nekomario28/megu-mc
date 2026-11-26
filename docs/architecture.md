# Architecture

`megu` is the canonical server package and executable. Its standalone
crates and dependency edges are indexed in
[architecture-crates.md](architecture-crates.md), and the guiding design
principles (Rust-core / Go-shell split, typed domain boundaries, single-source
generation, unified transactions, and the Tier A/B de-hardcoding blueprint)
live in [DESIGN.md](../DESIGN.md). The capabilities below refer to modules
present in this tree.

## Crate capabilities

- **`mod-adapter-core`** — Zero-JVM JAR and bytecode Mixin inspection
  ([`class_mixin_scanner`](../mod-adapter-core/src/class_mixin_scanner.rs)),
  including safe `@Inject` Tier 2 event lifting; generic culinary recipes and
  compatible addon discovery
  ([`culinary_adapter`](../mod-adapter-core/src/culinary_adapter.rs));
  progression barriers, multi-part bosses, `CGD1` chunk-generation and
  `EAD1` entity-AI delegates, saturation/inventory QoL support, and
  region-blending hooks
  ([`world_extensions`](../mod-adapter-core/src/world_extensions.rs));
  universal NeoForge/Fabric/Quilt/Jar-in-Jar dependency resolution & validated
  Server Mod Sync ([`mod_sync`](../mod-adapter-core/src/mod_sync.rs)), live
  mod JAR loot-table overlay + NeoForge/Forge Global Loot Modifier evaluation
  ([`loot_modifier_engine`](../mod-adapter-core/src/loot_modifier_engine.rs)),
  vanilla-to-mod overwrite provenance diff tracking
  ([`provenance_diff`](../mod-adapter-core/src/provenance_diff.rs)), and
  generic creature behavior primitives
  ([`creature_primitives`](../mod-adapter-core/src/creature_primitives.rs)).
  [`bytecode_subset_vm`](../mod-adapter-core/src/bytecode_subset_vm.rs) verifies
  and executes an allowlisted, fuel-bounded `.class` method subset without a
  JVM; [`ecosystem_compat`](../mod-adapter-core/src/ecosystem_compat.rs),
  [`mine_cut_all`](../mod-adapter-core/src/mine_cut_all.rs), and
  [`custom_media`](../mod-adapter-core/src/custom_media.rs) contain bounded
  compatibility, opt-in bulk-action, and custom-media logic. Invasive Java
  hooks require separate Tier 3 delegation and validation.
- **`client-port`** — Vulkan mesh shaders, compute-driven multi-draw
  indirect, and `ClassicLowSpec` for legacy Intel HD/UHD devices with
  16 KiB uniform-buffer batches. The GPU pipeline includes Create-style
  kinetic instances and block-entity instancing for chests, item frames,
  and signs ([`gpu_pipeline`](../client-port/src/gpu_pipeline.rs)).
  [`model_baker`](../client-port/src/model_baker.rs),
  [`atlas`](../client-port/src/atlas.rs), and
  [`hiz_culler`](../client-port/src/hiz_culler.rs) implement model baking,
  texture packing, and hierarchical occlusion culling.
- **`client-kernel`** — Client registry and version data; entity snapshot
  interpolation in [`entity_interp`](../client-kernel/src/entity_interp.rs).
- **`client-app`** — WGSL shader definitions and validation
  ([`shaders`](../client-app/src/shaders.rs)), RenderGraph
  ([`render_graph`](../client-app/src/render_graph.rs)), streaming frame budgets
  ([`frame_pacer`](../client-app/src/frame_pacer.rs)), integrated server
  harness ([`integrated`](../client-app/src/integrated.rs)), headless
  [software rasterizer](../client-app/src/render.rs), and bounded audio/QoL
  helpers ([`audio_qol`](../client-app/src/audio_qol.rs)).
- **`client-ui`** — Pure-Rust UI draw lists, layout, screen state, trademark-neutral
  built-in modules (`mod_manager`, `recipe_overlay`, `recipe_browser`), and a
  headless RGBA rasterizer ([`src`](../client-ui/src/lib.rs)) consumed live
  by `client-gpu`.
- **`client-gpu`** — Isolated windowed `winit` + `wgpu` native GPU client
  ([`window`](../client-gpu/src/window.rs),
  [`renderer`](../client-gpu/src/renderer.rs),
  [`live_session`](../client-gpu/src/live_session.rs)) with zero-window-close
  in-process soft reload, transactional mod profile staging & rollback
  ([`mod_profile_tx`](../client-gpu/src/mod_profile_tx.rs)), and opt-in
  on-demand local Japanese resource-pack generation via transient `llama-server`
  ([`mod_translator`](../client-gpu/src/mod_translator.rs)).
- **`core`** — Shared pure-Rust domain primitives, canonical `1.21.1`/`26.3`
  version constants, compact debug HUD metrics, and redacted deterministic
  crash fingerprints ([`crash_report`](../core/src/crash_report.rs)).
- **`create-adapter`** — Rail topology, contraption kinematics, and 6-DOF
  aeronautics sub-world operations
  ([`rail_network`](../create-adapter/src/rail_network.rs),
  [`aeronautics`](../create-adapter/src/aeronautics.rs)).
- **`proto`** — Canonical shared wire codecs including `SW6D`/`SHLM`
  ([`subworld_wire`](../proto/src/subworld_wire.rs)), `FRL2` friend invite
  relay ([`friend_relay`](../proto/src/friend_relay.rs)), and `VCR1` proximity
  voice relay ([`voice_relay`](../proto/src/voice_relay.rs)).
  `ServerSession` and the version router provide protocol 767/777
  session/codec paths ([`session`](../proto/src/session.rs),
  [`router`](../proto/src/router.rs)).
- **`state-store`** — `AUD1` audit write-ahead log and rollback
  ([`audit_log`](../state-store/src/audit_log.rs)); spiral chunk
  pregeneration and ticket-aware LRU unload
  ([`pregen`](../state-store/src/pregen.rs)); Anvil region and `level.dat`
  inspection/migration ([`anvil_inspect`](../state-store/src/anvil_inspect.rs)).
- **`server-files`** — `SharedChunkWireCache` with `Arc<[u8]>` packet
  storage, `DirectionalChunkQueue`, and the `SBC1` startup binary cache
  ([`perf_cache`](../server-files/src/perf_cache.rs)).
- **`mod-offload`** — Rust-side contracts for bounded delegate work. The
  optional [`mini-JRE builder`](../tools/build_mini_jre.py) requires an
  operator-installed JDK 21+.

## Operations and reporting

The server exposes level-4 [`/megu policy`](../src/teams_and_commands.rs)
`list|get|set|reset` for map fair-play and opt-in bulk actions. Five map
controls include `player_radar_scope=party_only`; current policy state is
in-memory. [`website/functions/api/report.js`](../website/functions/api/report.js)
validates and redacts a compact crash/bug/idea report, then relays it to
GitHub Issues using a secret and KV-backed crash deduplication.
[`website/functions/api/latest.js`](../website/functions/api/latest.js)
reads the latest public GitHub release. Neither endpoint proves an
end-to-end third-party modpack run.

## Clean-room public export

[`export_public_branch.sh`](../tools/export_public_branch.sh) strips
generated Minecraft data from the public tree and can run strict
provenance checks (`--verify`). Production code still embeds generated
block-state facts via `include_bytes!` in
[`client-kernel/src/registry.rs`](../client-kernel/src/registry.rs).
The stripped tree must regenerate those inputs from an operator-supplied
official server JAR before a release build. The export and build
procedure is in [release-checklist.md](release-checklist.md).
