# CF-0 shared kernel C ABI v1

`client-kernel/` builds `megu_kernel` as a Rust `rlib` and a C-linkable
`cdylib`. It has no Cargo dependencies or additional native libraries. Its
source build uses the megu checkout and the repository-pinned Rust
toolchain. The first-party license is
`LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later`.
See [LICENSE](../../LICENSE), [COPYING.LESSER](../../COPYING.LESSER),
[COPYING.AGPL](../../COPYING.AGPL), and [NOTICE](../../NOTICE) for the
Brigadier MIT attribution.

The C11 and C++ declarations in
[`megu_kernel.h`](../../client-kernel/include/megu_kernel.h)
define ABI v1. Check that `megu_kernel_abi_version()` equals
`MEGU_KERNEL_ABI_VERSION` before calling other symbols. The ABI uses
C-layout records, fixed-width record fields, native pointers and `size_t`
for buffer extents. Scalars use native byte order. Protocol and NBT byte
spans carry their respective wire encodings.

The crate imports `src/brigadier.rs` directly. Its build script projects the
server's paletted section, named NBT codec, AABB sweep, fluid calculations
and the `src/json.rs` parser used by the block-state registry from `src/` into
Cargo's build directory. It reads `write_varint` from `src/play_protocol.rs`
for the wire encoder and decoder, and includes `src/lighting.rs` verbatim, so
the light engine runs the server's algorithm with no duplicated copy.
Collision consumes ordered shape boxes and fluid consumes per-cell facts at
the registry boundary. A standard-library
[DEFLATE](https://www.rfc-editor.org/rfc/rfc1951) and
[gzip](https://www.rfc-editor.org/rfc/rfc1952) decoder feeds the shared NBT
parser. This lane exposes kernel operations independently of the server
world. Migration of server callers belongs to the broader
[shared kernel design](shared-kernel-and-integrated.md).

## Entry points

Names below use the `megu_kernel_` prefix. Palette wire decode, lighting
and the block-state registry are additive ABI v1 symbols.

| Family | Entry points | Contract |
| --- | --- | --- |
| Palette | `palette_new_single`, `palette_free`, `palette_get`, `palette_set` | One 4,096-cell section. Indices are `y*256 + z*16 + x`. ABI v1 uses state IDs `0..26684` (upper bound exclusive) from vanilla 1.21.1, protocol 767. An invalid index or ID leaves the section intact. |
| Palette queries | `palette_non_air`, `palette_is_uniform`, `palette_uniform_state`, `palette_bits`, `palette_unpack_into` | The non-air count excludes air, void air and cave air. `is_uniform` checks cell equality. The O(1) `uniform_state` sets `present=1` only for the 0-bit representation and writes state 0 when absent. An edited section can return to uniform contents while retaining packed storage. `palette_bits` describes that storage. `unpack_into` writes the first 4,096 elements of a caller buffer with capacity of at least 4,096 `uint32_t` values. |
| Palette wire | `palette_write_block_states_wire` | Returns an owned protocol-767 block-state container. It chooses the canonical 0-bit single, 4–8-bit indirect, or 15-bit direct encoding from the current cells. Packed words are big-endian, cells occupy successive low-bit slots within a word, and cells never straddle a word boundary. The caller supplies the separate count and biome container when assembling a whole section. |
| Palette wire decode | `palette_decode_block_states_wire` | Decodes one protocol-767 block-state container in the 0-bit single, 4–8-bit indirect, or 15-bit direct form into a new palette handle. `out_consumed` receives the bytes read, which may be fewer than `len`, so the caller can continue with the biome container. A malformed container returns `INVALID_WIRE`. |
| Lighting | `light_new`, `light_free`, `light_load_chunk`, `light_apply_edit`, `light_get_section` | An incremental sky and block light engine over a vertical window. `min_y` and `height` are multiples of 16, and `height / 16 + 2` is at most 64. `has_skylight` is 0 or 1. Each cell is a `light_block` with opacity and emission `0..15` and the covered half-block quadrants (low four bits) of the faces in the order west, east, north, south, down, up. `light_load_chunk` replaces one column and requires `count == height * 256` cells ordered y, z, x from `min_y`. `light_apply_edit` applies `count >= 1` edits as one batch and returns an update handle. Every edit must lie inside the window and a loaded column, or the whole batch is rejected unchanged. `light_get_section` copies a 2,048-byte nibble array, low nibble first, for `LIGHT_SKY` or `LIGHT_BLOCK`. Section index 0 is one section below `min_y`, and the last index is one section above the window. |
| Light updates | `light_update_info`, `light_update_chunk`, `light_update_section`, `light_update_free` | `light_update_info` reports the changed chunk count and invalidated cell count. `light_update_chunk` reports the chunk coordinates, sky and block masks, empty masks and changed-section counts for one chunk index. Bit `i` of each mask is section index `i`, and a set empty-mask bit clears that section. `light_update_section` returns a section index and a borrowed 2,048-byte array. `ordinal` indexes the changed sections of that kind in ascending section order. |
| Block-state registry | `registry_open_json`, `registry_free`, `registry_state_count`, `registry_state_name`, `registry_default_state`, `registry_state_facts` | `registry_open_json` opens `assets/block_states_1211.json` and `assets/blocks_1211.json` for 1.21.1. The inputs must agree with each other and with the compiled-in pinned state facts, otherwise it returns `INVALID_JSON`. `registry_state_name` returns the namespaced block name of a state as a borrowed UTF-8 span. `registry_default_state` maps a namespaced block name to its default state and returns `NOT_FOUND` for unknown names. `registry_state_facts` fills a `block_facts` record. |
| Block facts | `block_facts` record | Booleans are 0 or 1. `luminance` and `opacity` are light facts. `fluid_kind` uses the `REGISTRY_FLUID_*` values. `fluid_level` is the `level` property of water and lava blocks, otherwise 0. `is_waterlogged` marks water carried by any block other than `water`. `sturdy_faces_mask` uses the `FACE_*` bits. `face_occlusion` uses the `light_block` face order, derives from the solid-render, non-occluding and collision-box octants, and feeds the light engine as is. |
| NBT | `nbt_read_named_validate`, `nbt_read_gzip_validate`, `nbt_roundtrip_named` | Check one complete named NBT value, including Java modified UTF-8 and a 512-container depth limit. Gzip accepts concatenated members, checks headers and trailers, and applies the caller's byte budget to the combined decompressed payload. The roundtrip returns owned canonical bytes with Java UTF-16 compound key ordering, normalized NaNs and normalized empty lists. |
| Dispatcher | `dispatcher_new`, `dispatcher_free`, `dispatcher_register_literal`, `dispatcher_parse`, `dispatcher_complete_suggestions` | Register executable root literals. The dispatcher reads command text verbatim, so callers remove a leading protocol slash when present. Literals must be nonempty and free of whitespace and control characters. Parsing reports the consumed UTF-8 byte cursor and a 0/1 executable flag. Valid UTF-8 with a syntax mismatch returns success with `executable=0`. Completion uses the supplied UTF-8 byte cursor, which must be in range and on a character boundary. |
| Suggestions | `suggestions_info`, `suggestions_get`, `suggestions_free` | The result owns its texts independently of the dispatcher. `suggestions_info` returns the half-open replacement range in UTF-8 bytes and the item count. An empty result has range `0..0`. Items follow Brigadier's case-insensitive order, with registration order used for ties. `suggestions_get` returns a borrowed UTF-8 span by zero-based index. |
| Collision | `collide` | Sweep an XYZ world-space AABB by the supplied delta against an ordered table of world-space shapes. Pass the same shape order as the server for identical results. The server resolves Y first, then Z before X when `abs(x) < abs(z)`. Step-up uses the server's candidate-height and clearance rules when the caller supplies a positive `max_up_step`. The result reports clipped delta and horizontal, vertical, below and stepped-up flags. |
| Fluid | `fluid_impulse` | A synchronous callback provides each cell's fluid kind, amount, falling flag, blocks-motion flag, ice flag and full sturdy-face mask. Provide a consistent snapshot for the query and its neighbors. The server's deflated-AABB sampling and flow math produce immersion, maximum fluid height above the deflated bottom and the velocity impulse. Kind, velocity, player status, push eligibility and ultra-warm status are query inputs. Use the header's face directions and flags. |

## Ownership and errors

- Release each palette, light engine, light update, registry, dispatcher and
  suggestions handle once with its paired `*_free`. Concurrent reads of live
  handles are allowed. Mutation, including `light_load_chunk` and
  `light_apply_edit`, and release require exclusive access.
- Call `bytes_free` exactly once with the original pointer and length from a
  successful palette wire or NBT roundtrip call. These read-only bytes have
  an independent lifetime. Borrowed spans use their handle's lifetime:
  suggestion texts until `suggestions_free`, light section data until
  `light_update_free`, and registry state names until `registry_free`.
- Provide live, aligned, correctly sized storage and disjoint writable
  outputs. Required pointers must be non-null even for an empty input.
  A zero collision shape count permits a null shape pointer. Fluid bounds must leave room for neighboring coordinates in signed
  32-bit space and fit the server's signed 32-bit cell counter. A fluid callback
  may accept a null context. Callback inputs are synchronous and the
  callback's output slot is writable only while it runs. Callbacks must return
  normally across the C boundary.
- A failing call leaves output storage untouched. The ABI checks nulls,
  alignment, span extents, and value ranges. The caller guarantees pointer
  lifetime, provenance, and allocation size. Freeing a stale pointer or
  passing an invented non-null handle violates this contract.

| Status | Value | Meaning |
| --- | ---: | --- |
| `OK` | 0 | Operation completed. |
| `NULL` | 1 | Required pointer or callback is null. |
| `OUT_OF_RANGE` | 2 | Index, UTF-8 cursor, span extent, or geometry arithmetic outside the supported range. |
| `INVALID_UTF8` | 3 | Command or literal bytes are invalid UTF-8. |
| `INVALID_STATE` | 4 | Block state ID is outside the pinned registry. |
| `INVALID_NBT` | 5 | Invalid named NBT, including malformed modified UTF-8 or trailing decoded bytes. |
| `NBT_DEPTH` | 6 | NBT nesting exceeds the shared codec's limit. |
| `BYTE_BUDGET` | 7 | Gzip output would exceed the caller's uncompressed byte budget. |
| `INVALID_GZIP` | 8 | Invalid gzip framing, DEFLATE stream or member checksum or size. |
| `INVALID_ARGUMENT` | 9 | Misaligned pointer, invalid literal, non-finite or unordered geometry, invalid flag or malformed fluid facts. |
| `BUFFER_TOO_SMALL` | 10 | Output capacity is below the required extent. |
| `QUERY_FAILED` | 11 | Fluid callback returned nonzero. |
| `PANIC` | 12 | A Rust panic unwound inside the library and was caught at the C boundary. |
| `INVALID_WIRE` | 13 | Malformed protocol-767 block-state container. |
| `INVALID_JSON` | 14 | Registry JSON is malformed or disagrees with the other input or the pinned state facts. |
| `NOT_FOUND` | 15 | Unknown namespaced block name. |

## Verification

Run from the repository root on Linux. The C test links against the release
`cdylib` built by the preceding command.

```sh
mkdir -p target/tmp && export TMPDIR="$PWD/target/tmp"

TMPDIR="$PWD/target/tmp" cargo test --manifest-path client-kernel/Cargo.toml --offline --locked
TMPDIR="$PWD/target/tmp" cargo build --manifest-path client-kernel/Cargo.toml --release --offline --locked

kernel_lib="$PWD/client-kernel/target/release"
gcc -std=c11 -Wall -Wextra -Werror -pedantic \
  -I client-kernel/include client-kernel/tests/abi_roundtrip.c \
  -L "$kernel_lib" -Wl,-rpath,"$kernel_lib" -lmegu_kernel \
  -o "$TMPDIR/kernel-abi-roundtrip"
"$TMPDIR/kernel-abi-roundtrip"
g++ -std=c++11 -Wall -Wextra -Werror -pedantic -fsyntax-only -x c++ \
  client-kernel/include/megu_kernel.h

TMPDIR="$PWD/target/tmp" cargo fmt --manifest-path client-kernel/Cargo.toml -- --check
cargo clippy --manifest-path client-kernel/Cargo.toml --offline --locked --all-targets -- -D warnings
bash tools/layers_check.sh && bash tools/debt_ratchet.sh && python3 tools/provenance_check.py && git diff --check

rm -rf target/tmp client-kernel/target
```
