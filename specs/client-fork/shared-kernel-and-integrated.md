# Client Fork Track CF-0 / CF-3 / CF-3b — shared kernel and integrated bridge

Target is Minecraft **1.21.1 / protocol 767 / Java 21**. This document records
the current source-level contracts and the planned extraction boundary. The
current crate is a binary (`src/main.rs`), and **none of these seven components
exposes a public Rust library or C ABI today**. A `pub` declaration inside the
private `brigadier` module is also private to downstream crates.

## CF-0 — zero-drift kernel inventory

The inventory names seven *responsibilities*. Six have named source files, and
lighting currently lives in `src/main.rs`. The source declaration is the
normative definition of each current signature and precondition. The following
list distinguishes `pub(crate)` surface from private implementation.

| Owner | Existing visibility and contract | Canonical inputs / outputs and invariant |
| --- | --- | --- |
| `src/block_registry.rs` — `block_registry` | `pub(crate)` `BReg`, `Support::{Full,Center,Rigid}`, `HeightmapKind`, `ns_name(&str)->String`, `bs_parts(&J)->Option<(String,Option<&J>)>`, `breg()->Option<&'static BReg>`, `mkprops(&[(&str,&str)])->J`, `reg_props_axis(&str)->Option<()>`. `BReg` methods include `state_of(&str,Option<&J>)->u32`, `checked_state(&str,&[(String,String)])->Option<u32>`, `name_of(u32)->&str`, `properties_of(u32)->Option<Vec<(String,String)>>`, `collision_boxes`, `is_face_sturdy`, `heightmap_counts`, `luminance_of`, `opacity_of`, `canonical_state`, `tag_members`, and the remaining source-declared `pub(crate)` predicates and transformations. | Single 1.21.1 block-state ID and physical-fact mapping. `BReg` depends on private registry data and `J` JSON representation. State ID/palette encoding, collision support, tags and light opacity must share one authoritative mapping. |
| `src/entity_collision.rs` — `entity_collision` | All top-level collision operations, `CollideResult`, `entity_collide`, `entity_collide_world`, `step_entity_move`, `mob_collide_move` and `EntityAabb` methods are private. File is `include!`d by `main.rs`. | World block-state/shape query and `EntityAabb`/velocity produce collision-resolved deltas and grounded/step flags. It depends on `block_registry::breg()` and the containing `main.rs` world state. No isolated external entry point. |
| `src/entity_fluid.rs` — `entity_fluid` | Private `EntityFluidKind`, `EntityFluidState`, `FluidPushResult`, `LivingTravelContext`, `FluidTravelOutcome` and `fluid_flow_with`, `travel_fluid_step`, `mob_fluid_travel_tick`, eye/bounding-box/fluid query helpers. Also `include!`d by `main.rs`. | Block/neighbor/flow queries and actor pose/velocity produce fluid heights, impulses, submersion and movement adjustments. Reads shared block IDs and world state. Fluid travel/collision order is part of simulation semantics. |
| `src/paletted.rs` — `paletted` | `pub(crate)` `SectionPalette::{Single(u32),Indirect{bits,entries,words},Direct{words}}`, `PalettedSection`, `SectionLightClass::{Empty,Opaque,Mixed}`, `is_air_state(u32)->bool`. `PalettedSection` provides `pack`, `single`, `get(usize)->u32`, `set`, `non_air()->u16`, `light_class`, `uniform_state`, `unpack_into(&mut [u32;4096])`, `write_block_states_wire(&mut Vec<u8>)`, `write_wire(&[i64;64],&mut Vec<u8>)` and other `pub(crate)` accessors. | Exactly 4096 blocks per section, non-straddling packed `u64` words; 0-bit uniform, 4–8-bit indirect and 15-bit direct palette. Wire data must be protocol-767 compatible. Mutation invalidates `clean_wire` if a stale local palette would change canonical bytes. |
| `src/main.rs` — `lighting` | No `src/lighting.rs` exists at this revision. Private `block_light_sections_from[_dim]`, `block_light_sections[_dim]`, `sky_light_sections[_world/_with_spill/_from]`, `pack_light_section`, `write_light_arrays`, `update_light_packet[_dim]`, `light_dirty_for[_dim]`. | Chunk coordinates, dimension, block opacity/light sources, neighbors and floor samples produce section light arrays (`[u8;2048]` per section) and encoded light update packets. Skyline and block-light edge propagation depend on live world state. |
| `src/nbt.rs` — `nbt` | `pub(crate)` `NbtString(Vec<u16>)`, `Tag` (all NBT tag variants), `read_named(&[u8])->Result<(NbtString,Tag),&'static str>`, `read_gzip(&[u8],usize)->Result<(NbtString,Tag),&'static str>`, `write_named(&NbtString,&Tag)->Result<Vec<u8>,&'static str>`. `Tag` provides `id`, `get`, `list`, `int`, `string`; `NbtString` provides `text`, `to_utf8`. | Named NBT uses Java modified UTF-8, explicit end tag and bounded depth/length parsing. Gzip decoder respects the caller's maximum uncompressed byte budget. Readers reject trailing bytes. |
| `src/brigadier.rs` — `brigadier` | Module private to `main.rs`, declarations include `pub` `StringReader`, `StringRange`, `CommandNode<S>`, `ArgumentType<S>`, `ArgumentBuilder<S>`, `CommandDispatcher<S>`, `ParseResults<S>`, `CommandContext<S>`, `Suggestions`, `SuggestionsBuilder` and typed argument implementations. These are Rust-internal `pub` items, **not exported from a `lib.rs`**. | Generic `S` source, UTF-8 text/reader cursor, command tree, parse context and result/suggestion types. Error location and completion order must remain deterministic; generic trait objects cannot cross a C ABI directly. |

These signatures are an inventory of existing access points, not approval to
change visibility. In particular, `entity_collision`, `entity_fluid` and
`lighting` are coupled to world state inside `main.rs` and require explicit
input/context interfaces before extraction.

For completeness, the **current `pub(crate)` method names on `BReg`** are
`state_of`, `block_id_of`, `name_of`, `checked_state`, `properties_of`,
`is_template_solid`, `state_count`, `can_harvest`, `luminance_of`, `opacity_of`,
`has_post_process`, `behaviour_of`, `stack_size_of`, `block_drop`,
`block_drop_second`, `tool_kind_of`, `hardness_of`, `is_air`, `blocks_motion`,
`is_solid`, `can_occlude`, `collision_boxes`, `is_solid_render`,
`is_collision_empty`, `is_collision_shape_full_block`, `is_redstone_conductor`,
`is_face_sturdy`, `side_solid`, `mineshaft_sturdy_face`,
`mineshaft_chain_anchor`, `heightmap_counts`, `is_double_plant`, `has_water`,
`supports_multiface`, `fluid_name`, `is_liquid`, `is_liquid_block`, `face_plane`,
`is_replaceable`, `fence_after_full_neighbor`, `pane_after_full_neighbor`,
`wall_after_full_neighbor`, `connection_after_neighbor`, `stair_after_neighbors`,
`needs_postprocess_shape`, `fragile_loses_support`, `with_prop`,
`distance_from_log`, `canonical_state`, `get_prop`, `has_prop`, `tag_members`,
`default_state`, and `in_tag`. The same module exposes `HeightmapKind::parse`,
`HeightmapKind::is_wg`, `B_NONE`, `B_SUGAR_CANE`, `B_FALLING`, and the
`BReg::unpaired` field. Exact signatures, conditional returns, and validity
ranges are those in `src/block_registry.rs`, which is the contract source.

The **complete `pub(crate)` method set of `PalettedSection`** is `pack`,
`single`, `get`, `set`, `non_air`, `has_hidden`, `light_class`, `is_uniform`,
`uniform_state`, `is_clean_wire`, `any_palette_entry`, `unpack_into`,
`copy_y_slice`, `palette_bits`, `approx_bytes`, `write_block_states_wire`, and
`write_wire`. The module also exposes `is_air_state(u32)->bool` and
`run_paletted_bench()->()`. These are in-crate APIs. Indexing must remain
within `[0,4096)`, palette serialization must preserve section ordering, and
an actual state mutation clears `clean_wire`.

`brigadier` has the Rust-local public type families `StringReader`/`StringRange`,
`CommandExceptionType`/`CommandSyntaxException` and exception factories,
`ArgumentType<S>` with integer/long/float/double/bool/string implementations,
`Suggestion`/`Suggestions`/`SuggestionsBuilder`, `CommandContext<S>` and its
builders, `CommandNode<S>`/`ArgumentBuilder<S>`, `ParseResults<S>`, and
`CommandDispatcher<S>`. Its `pub` type aliases (`Command<S>`,
`RedirectModifier<S>`, `Requirement<S>`, `ResultConsumer<S>`,
`SuggestionProvider<S>`) use Rust `Arc` closures or trait objects. The concrete
methods and generic bounds in `src/brigadier.rs` define the current Rust
contract. None is linkable as a C symbol or importable by another crate today.

## Future `rlib` / `cdylib` contract (CF-0 design, not implemented)

An extraction requires a separately authorized Rust module/Cargo change.
Contract version **1** would expose the seven owners through one crate with
`crate-type = ["rlib", "cdylib"]` and a single authoritative implementation per
owner. Rust callers use versioned, documented, immutable data/context types.
C callers use an explicit `extern "C"` facade with these design-level operations:

| Family | Proposed stable operation | Ownership / failure contract |
| --- | --- | --- |
| Registry | `kernel_registry_open`, `kernel_state_lookup`, `kernel_state_facts` | Opaque read-only snapshot handle, version + target state-table digest, checked IDs, explicit release. |
| Collision | `kernel_entity_collide` | Value-only AABB, displacement, world-query callback/context; deterministic order, defined error status. |
| Fluid | `kernel_entity_fluid_step` | Explicit actor, environment, tick and neighboring cells; output movement/immersion and event flags. |
| Palette | `kernel_palette_decode`, `kernel_palette_encode` | Fixed-size section extent, explicit byte length and caller-managed output buffer; canonical protocol 767 ordering. |
| Lighting | `kernel_light_update` | Explicit dimension, section/neighbor samples and opaque/emit facts; bounded result buffers. |
| NBT | `kernel_nbt_decode`, `kernel_nbt_encode` | Byte spans, bounded memory/depth, opaque tree handle with paired release. |
| Brigadier | `kernel_command_parse`, `kernel_command_suggest` | Owned UTF-8 bytes and a versioned callback table, typed opaque source/context, explicit result free. |

Across the C boundary, all FFI structs must use fixed-width integers and
`#[repr(C)]`, never pass Rust `Vec`, `String`, enum, trait object or references.
The facade reports errors as status codes, owns/pairs alloc/free, catches
panics before crossing the ABI, and checks the ABI revision plus registry digest
on creation. Determinism requires golden corpus byte equality and test vectors
for rounding, collision tie breaks, light propagation, modified UTF-8 and
command suggestions. These names specify intended capabilities, not existing
binary symbols or completed parity verification.

`tools/layers_check.sh` pins **44 modules / 111 edges** from the current
`main.rs` graph. CF-0 leaves every Rust module/import/include and
`evidence/foundation/module-layers/policy.json` unchanged. A later extraction
must verify the pinned graph on the untouched baseline, put the new kernel in a
separate crate with its own graph check, route the old binary owners through
the shared implementation, run parity gates, and request an intentional
module-layer policy migration if the binary graph changes. No duplicate
independent algorithm implementations are permitted. Updating only a layer
count to make a broken check green is not a migration.

## CF-3 — integrated JAR boundary

`python3 tools/client_launcher/build_integrated_mod.py --out-dir DIR` creates
reproducible `DIR/mc-spine-integrated.jar`. It contains `fabric.mod.json`,
`META-INF/neoforge.mods.toml`, the shared Java 21 coordinator,
`FabricIntegratedMod implements ClientModInitializer`, and
`NeoForgeIntegratedMod @Mod("mc_spine_integrated")`. Loader API stubs exist only
for offline Java compilation and are excluded from the archive. The same jar
is discoverable in Fabric and NeoForge 1.21.1 `mods/` directories. Real mod
loader launch, Minecraft client connection and in-game world lifecycle
integration require separate verification and hooks.

Activation in a mod loader is **explicit** via JVM properties
`-Dmc.spine.integrated.enabled=true`,
`-Dmc.spine.integrated.serverBin=/absolute/path/to/mc-spine`, optionally
`-Dmc.spine.integrated.gameDir=/game/directory`,
`-Dmc.spine.integrated.world=mc-spine`, and
`-Dmc.spine.integrated.seed=0`. If disabled, initialization has no process
side effect. A server binary must be installed independently, with compatible
`config_packets.bin`/`assets` available via `MC_SPINE_SERVER_HOME` or the
resolved game directory/native binary directory. No native executable, game
client classes, game asset pack or third-party JAR is embedded.
`IntegratedServerBridge.startConfigured()` starts one monitored instance and
`IntegratedServerBridge.closeConfigured()` explicitly ends that instance when
the embedding loader's world lifecycle ends. The JVM shutdown hook also closes
it when the game process exits. A loader world-close event binding and a real
in-game launch have not yet been validated.

For standalone use the JAR has `Main-Class` set to the bridge. Run
`java -Djava.io.tmpdir="$PWD/target/tmp" -XX:-UsePerfData -jar \
 DIR/mc-spine-integrated.jar --server-bin /path/to/mc-spine \
 --game-dir /path/to/game --world mc-spine --seed 0`.
The process emits `READY 127.0.0.1:<port> pid=<pid>` only after the
protocol-767 Status response matches the expected 1.21.1 server. Closing
stdin, writing to stdin, or JVM shutdown requests close. The coordinator
selects a free ephemeral port, passes `MC_SPINE_BIND_ADDR`, `MC_SPINE_PORT`,
`MC_SPINE_WORLD_DIR` and `MC_SPINE_SEED` to its child, stores the world under
`gameDir/saves/<world>`, holds an exclusive world lock, and on cleanup sends
termination with bounded wait then force-kill fallback. Invalid binary, escaped
world path, non-loopback bind, explicit non-ephemeral port, malformed seed and
failed protocol readiness abort startup and clean up the child. Ephemeral port
selection has an OS-level bind gap between releasing the reservation and the
child binding. Repeated protocol probes and child liveness narrow the gap.

Current `tools/client_launcher/integrated_bridge.py` remains the existing
native-client Quick Play path. Its Python launcher and the new Java loader
adapter can coexist. Neither currently redirects an installed unmodified
Minecraft client's integrated-world UI into mc-spine automatically.

### CF-3 to FFI migration

1. Preserve the protocol-767 socket bridge as the measurable baseline. Test
   real status packets, shutdown, seed/world isolation and existing prediction
   movement tests against the same Rust build revision.
2. Extract the seven CF-0 owners once behind the new crate's versioned Rust
   contract. Keep server authoritative and verify identical inputs produce
   identical registry/physics/palette/lighting/NBT/command results.
3. Introduce an optional JVM-native FFI adapter with a version/digest
   handshake and explicit ownership/callback rules. Run old loopback and new
   FFI paths over the same parity corpus before switching any caller.
4. Only after in-game client lifecycle and rollback behavior are demonstrated,
   substitute FFI calls for eligible local kernel operations. Continue using
   transport for world authority and multiplayer semantics where necessary.

## CF-3b — Direct `ByteBuffer` `MCSI` v1 frame bridge

Lane `E4-30` extends `tools/client_launcher/net/mcspine/integrated/IntegratedServerBridge.java`
with a deterministic, fail-closed direct-`ByteBuffer` frame codec for the
single-player / LAN-host `CF-3` integrated server exchange. It does not replace
the protocol-767 socket lifecycle bridge; it adds a framing layer that the
integrated client mod can use to relay packets and paletted section snapshots
without staging whole frames in heap stream objects.

### Wire contract (all integers little-endian)

| Offset | Bytes | Field | Constraint |
| --- | --- | --- | --- |
| 0 | 4 | magic | `0x4D435349` (`"MCSI"`) |
| 4 | 2 | version | `1` |
| 6 | 2 | kind | `1 = PROTOCOL767_PACKET`, `2 = PALETTED_SECTION_SNAPSHOT`, `3 = TICK_HEARTBEAT` |
| 8 | 4 | payload length | `0..262144` |
| 12 | 4 | CRC-32 | `java.util.zip.CRC32` of the payload bytes |
| 16 | N | payload | `N` = payload length |

The header is 16 bytes and the largest frame is 262160 bytes. Header and
payload are read/written with absolute `ByteBuffer` accesses directly in the
direct buffer; only the returned `DirectFrame.payload` is copied into a
defensively cloned `byte[]`.

### Public surface and fail-closed rules

- `writeDirectFrame(ByteBuffer, int, byte[])` writes one frame at the buffer's
  current position and advances it.
- `readDirectFrame(ByteBuffer)` decodes one frame at the current position,
  advances past it, and returns `DirectFrame(kind, payload, crc32)`.
- `exchangeFrameOverLoopback(ByteBuffer requestBuf, InputStream in,
  OutputStream out, ByteBuffer responseBuf)` writes the framed request region
  `[0, requestBuf.position())` to `out`, flushes, then reads the response header
  and payload straight into `responseBuf` (left little-endian, position reset to
  `0`, limit set to the frame size), returning its decoded `DirectFrame`.

Both buffers must be non-null, `ByteBuffer.isDirect() == true`, and
`order() == ByteOrder.LITTLE_ENDIAN`. Null payloads, unknown kinds, payloads
over 262144 bytes, insufficient capacity, bad magic, unsupported version,
truncated header/payload, a request size inconsistent with its length field,
and CRC-32 mismatch all throw `IllegalArgumentException` before a frame is
accepted; short streams throw `EOFException`. The loopback read caps the buffer
limit per field so a header read cannot swallow the payload.

### Packaging and receipts

`build_integrated_mod.py` packages every compiled class via a sorted `rglob`
over `javac` output, so the nested
`net/mcspine/integrated/IntegratedServerBridge$DirectFrame.class` is included.
Entries use the fixed ZIP time `(1980, 1, 1, 0, 0, 0)`, mode `0644`, creator
system `3`, and deterministic deflate; consecutive builds are byte-identical.
Compile-only Fabric/NeoForge stubs are rejected if they leak into the archive.

`tests/client_launcher_test.py::DirectFrameBridgeTest` compiles a generated
`DirectFrameHarness.java` against the built jar and runs three JVM cases:
`roundtrip` (18 checks, kinds 1/2/3), `failclosed` (14 checks: heap buffer,
null buffer, big-endian order, null/oversized payload, bad kind, short capacity,
bad magic/version/kind, truncated header/payload, oversized length, bad CRC-32),
and `loopback` (9 checks: a real `127.0.0.1` socket request/response exchange
plus heap-buffer rejection), plus a jar-reproducibility case.

Receipt (2026-11-20, this worktree, OpenJDK 27 with `--release 21`,
`TMPDIR=$PWD/target/tmp`):

- `python3 tests/client_launcher_test.py` → `Ran 29 tests ... OK (skipped=3)`.
- Mutation: disabling the CRC-32 comparison in a scratch copy makes the
  `failclosed` harness case fail with
  `expected IllegalArgumentException: CRC-32`.
- `bash tools/layers_check.sh`, `bash tools/debt_ratchet.sh`,
  `python3 tools/provenance_check.py`, `git diff --check` → pass.

The direct frame bridge is a transport/framing capability. It is not evidence
of in-game client lifecycle integration or of native-kernel FFI parity; those
remain the CF-3 → FFI migration gates above.

Source and generator code are first-party, with the repository's stated
`LGPL-3.0-or-later` / `GPL-3.0-or-later` / `AGPL-3.0-or-later` contribution
grant. Distributed artifact metadata declares `GPL-3.0-or-later` consistently
with the current project. Fabric/NeoForge API references are compile-only
type signatures. No loader bytecode, Mojang assets or third-party
AGPL-3.0-only implementation is bundled.
