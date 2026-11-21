/* SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later */
#ifndef MC_SPINE_KERNEL_H
#define MC_SPINE_KERNEL_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum {
    MC_SPINE_KERNEL_ABI_VERSION = 1,
    MC_SPINE_KERNEL_PROTOCOL = 767,
    MC_SPINE_KERNEL_SECTION_CELLS = 4096,
    MC_SPINE_KERNEL_STATE_COUNT = 26684
};

/* Status values are returned as int32_t. */
enum mc_spine_kernel_status {
    MC_SPINE_KERNEL_OK = 0,
    MC_SPINE_KERNEL_NULL = 1,
    MC_SPINE_KERNEL_OUT_OF_RANGE = 2,
    MC_SPINE_KERNEL_INVALID_UTF8 = 3,
    MC_SPINE_KERNEL_INVALID_STATE = 4,
    MC_SPINE_KERNEL_INVALID_NBT = 5,
    MC_SPINE_KERNEL_NBT_DEPTH = 6,
    MC_SPINE_KERNEL_BYTE_BUDGET = 7,
    MC_SPINE_KERNEL_INVALID_GZIP = 8,
    MC_SPINE_KERNEL_INVALID_ARGUMENT = 9,
    MC_SPINE_KERNEL_BUFFER_TOO_SMALL = 10,
    MC_SPINE_KERNEL_QUERY_FAILED = 11,
    MC_SPINE_KERNEL_PANIC = 12,
    MC_SPINE_KERNEL_INVALID_WIRE = 13,
    MC_SPINE_KERNEL_INVALID_JSON = 14,
    MC_SPINE_KERNEL_NOT_FOUND = 15
};

typedef struct mc_spine_kernel_palette mc_spine_kernel_palette;
typedef struct mc_spine_kernel_light_engine mc_spine_kernel_light_engine;
typedef struct mc_spine_kernel_light_update mc_spine_kernel_light_update;
typedef struct mc_spine_kernel_registry mc_spine_kernel_registry;
typedef struct mc_spine_kernel_dispatcher mc_spine_kernel_dispatcher;
typedef struct mc_spine_kernel_suggestions mc_spine_kernel_suggestions;

/* Read-only owned bytes. Pass the original pair to bytes_free once. */
typedef struct mc_spine_kernel_bytes {
    const uint8_t *data;
    uint64_t len;
} mc_spine_kernel_bytes;

/* A read-only borrowed UTF-8 span, valid until suggestions_free. */
typedef struct mc_spine_kernel_text {
    const uint8_t *data;
    uint64_t len;
} mc_spine_kernel_text;

typedef struct mc_spine_kernel_command_parse {
    uint64_t cursor;
    uint8_t executable;
} mc_spine_kernel_command_parse;

typedef struct mc_spine_kernel_suggestion_info {
    uint64_t start;
    uint64_t end;
    uint64_t count;
} mc_spine_kernel_suggestion_info;

typedef struct mc_spine_kernel_aabb {
    double min[3];
    double max[3];
} mc_spine_kernel_aabb;

enum {
    MC_SPINE_KERNEL_COLLISION_HORIZONTAL = 1 << 0,
    MC_SPINE_KERNEL_COLLISION_VERTICAL = 1 << 1,
    MC_SPINE_KERNEL_COLLISION_BELOW = 1 << 2,
    MC_SPINE_KERNEL_COLLISION_STEPPED_UP = 1 << 3
};

typedef struct mc_spine_kernel_collision {
    double delta[3];
    uint8_t flags;
} mc_spine_kernel_collision;

enum {
    MC_SPINE_KERNEL_FLUID_NONE = 0,
    MC_SPINE_KERNEL_FLUID_WATER = 1,
    MC_SPINE_KERNEL_FLUID_LAVA = 2,
    /* Input flags */
    MC_SPINE_KERNEL_FLUID_ULTRA_WARM = 1 << 0,
    MC_SPINE_KERNEL_FLUID_PUSHED = 1 << 1,
    MC_SPINE_KERNEL_FLUID_PLAYER = 1 << 2,
    /* Per-cell flags */
    MC_SPINE_KERNEL_CELL_FALLING = 1 << 0,
    MC_SPINE_KERNEL_CELL_BLOCKS_MOTION = 1 << 1,
    MC_SPINE_KERNEL_CELL_ICE = 1 << 2,
    /* Per-cell full sturdy-face mask */
    MC_SPINE_KERNEL_FACE_DOWN = 1 << 0,
    MC_SPINE_KERNEL_FACE_UP = 1 << 1,
    MC_SPINE_KERNEL_FACE_NORTH = 1 << 2,
    MC_SPINE_KERNEL_FACE_SOUTH = 1 << 3,
    MC_SPINE_KERNEL_FACE_WEST = 1 << 4,
    MC_SPINE_KERNEL_FACE_EAST = 1 << 5
};

typedef struct mc_spine_kernel_fluid_input {
    double velocity[3];
    uint8_t kind; /* WATER or LAVA */
    uint8_t flags;
} mc_spine_kernel_fluid_input;

typedef struct mc_spine_kernel_fluid_facts {
    uint8_t kind;
    uint8_t amount; /* 0 for NONE, 1..8 for WATER or LAVA */
    uint8_t flags;
    uint8_t sturdy_faces;
} mc_spine_kernel_fluid_facts;

typedef struct mc_spine_kernel_fluid_result {
    double push[3];
    double max_height;
    uint8_t in_fluid;
} mc_spine_kernel_fluid_result;

/* Return 0 and populate out, or return nonzero to fail the query.
   out starts as empty air and is writable only for the duration of the callback. */
typedef int32_t (*mc_spine_kernel_fluid_lookup)(void *context, int32_t x, int32_t y,
                                                int32_t z, mc_spine_kernel_fluid_facts *out);

enum {
    MC_SPINE_KERNEL_LIGHT_SECTION_BYTES = 2048,
    MC_SPINE_KERNEL_LIGHT_SKY = 0,
    MC_SPINE_KERNEL_LIGHT_BLOCK = 1,
    MC_SPINE_KERNEL_REGISTRY_FLUID_NONE = 0,
    MC_SPINE_KERNEL_REGISTRY_FLUID_WATER = 1,
    MC_SPINE_KERNEL_REGISTRY_FLUID_LAVA = 2
};

/* One cell's light facts. opacity and emission are 0..15. occlusion holds the
   covered half-block quadrants (low four bits) of the faces in the order west,
   east, north, south, down, up. */
typedef struct mc_spine_kernel_light_block {
    uint8_t opacity;
    uint8_t emission;
    uint8_t occlusion[6];
} mc_spine_kernel_light_block;

typedef struct mc_spine_kernel_light_edit {
    int32_t x;
    int32_t y;
    int32_t z;
    mc_spine_kernel_light_block block;
} mc_spine_kernel_light_edit;

typedef struct mc_spine_kernel_light_update_summary {
    uint64_t chunk_count;
    uint64_t invalidated_cells;
} mc_spine_kernel_light_update_summary;

/* Bit i of each mask is section index i. A set empty mask clears that section. */
typedef struct mc_spine_kernel_light_chunk_info {
    int32_t chunk_x;
    int32_t chunk_z;
    uint64_t sky_mask;
    uint64_t empty_sky_mask;
    uint64_t block_mask;
    uint64_t empty_block_mask;
    uint64_t sky_count;
    uint64_t block_count;
} mc_spine_kernel_light_chunk_info;

/* data borrows 2048 bytes, valid until light_update_free. */
typedef struct mc_spine_kernel_light_section {
    uint32_t section_index;
    const uint8_t *data;
} mc_spine_kernel_light_section;

/* Booleans are 0 or 1. sturdy_faces_mask uses the MC_SPINE_KERNEL_FACE_* bits.
   face_occlusion uses the light_block face order and feeds the light engine as is.
   fluid_level is the level property of water and lava blocks, otherwise 0.
   is_waterlogged marks water carried by a block other than water. */
typedef struct mc_spine_kernel_block_facts {
    uint8_t luminance;
    uint8_t opacity;
    uint8_t is_air;
    uint8_t is_liquid;
    uint8_t fluid_kind;
    uint8_t fluid_level;
    uint8_t is_waterlogged;
    uint8_t is_solid_render;
    uint8_t can_occlude;
    uint8_t blocks_motion;
    uint8_t sturdy_faces_mask;
    uint8_t face_occlusion[6];
} mc_spine_kernel_block_facts;

/* Check ABI_VERSION before use. All required pointers must be non-null and
   refer to live, aligned storage of the stated type and extent, even for
   empty byte inputs. Outputs must be writable and disjoint from other inputs
   and outputs. A returned error leaves outputs untouched. Reads on a live
   handle may run concurrently. Mutation and free require exclusive access.
   Foreign callbacks must return normally and must not retain borrowed pointers. */
uint32_t mc_spine_kernel_abi_version(void);
int32_t mc_spine_kernel_bytes_free(mc_spine_kernel_bytes bytes);

int32_t mc_spine_kernel_palette_new_single(uint32_t state, mc_spine_kernel_palette **out);
int32_t mc_spine_kernel_palette_free(mc_spine_kernel_palette *section);
int32_t mc_spine_kernel_palette_get(const mc_spine_kernel_palette *section, uint32_t index,
                                   uint32_t *out);
int32_t mc_spine_kernel_palette_set(mc_spine_kernel_palette *section, uint32_t index, uint32_t state);
int32_t mc_spine_kernel_palette_non_air(const mc_spine_kernel_palette *section, uint16_t *out);
int32_t mc_spine_kernel_palette_is_uniform(const mc_spine_kernel_palette *section, uint8_t *out);
/* present is 1 only for the 0-bit storage representation. Otherwise state is 0. */
int32_t mc_spine_kernel_palette_uniform_state(const mc_spine_kernel_palette *section,
                                             uint8_t *present, uint32_t *state);
int32_t mc_spine_kernel_palette_bits(const mc_spine_kernel_palette *section, uint8_t *out);
int32_t mc_spine_kernel_palette_unpack_into(const mc_spine_kernel_palette *section, uint32_t *out,
                                            size_t capacity);
int32_t mc_spine_kernel_palette_write_block_states_wire(const mc_spine_kernel_palette *section,
                                                        mc_spine_kernel_bytes *out);

/* Decodes one protocol-767 block-state container (0-bit single, 4-8-bit indirect,
   15-bit direct). out_consumed receives the bytes read, which may be fewer than len. */
int32_t mc_spine_kernel_palette_decode_block_states_wire(const uint8_t *bytes, size_t len,
                                                         size_t *out_consumed,
                                                         mc_spine_kernel_palette **out);

/* min_y and height are multiples of 16, and height / 16 + 2 is at most 64. */
int32_t mc_spine_kernel_light_new(int32_t min_y, uint32_t height, uint8_t has_skylight,
                                  mc_spine_kernel_light_engine **out);
int32_t mc_spine_kernel_light_free(mc_spine_kernel_light_engine *engine);
/* Replaces one column. count must equal height * 256, ordered y, z, x from min_y. */
int32_t mc_spine_kernel_light_load_chunk(mc_spine_kernel_light_engine *engine, int32_t chunk_x,
                                         int32_t chunk_z, const mc_spine_kernel_light_block *cells,
                                         size_t count);
/* Applies count >= 1 edits as one batch. Every edit must lie inside the window and a
   loaded column, or the whole batch is rejected unchanged. */
int32_t mc_spine_kernel_light_apply_edit(mc_spine_kernel_light_engine *engine,
                                         const mc_spine_kernel_light_edit *edits, size_t count,
                                         mc_spine_kernel_light_update **out);
/* Copies a 2048-byte nibble array, low nibble first. kind is LIGHT_SKY or LIGHT_BLOCK.
   Index 0 begins one section below min_y, and the last index is one section above. */
int32_t mc_spine_kernel_light_get_section(const mc_spine_kernel_light_engine *engine,
                                          int32_t chunk_x, int32_t chunk_z, uint8_t kind,
                                          uint32_t section_index, uint8_t *out, size_t capacity);
int32_t mc_spine_kernel_light_update_info(const mc_spine_kernel_light_update *update,
                                          mc_spine_kernel_light_update_summary *out);
int32_t mc_spine_kernel_light_update_chunk(const mc_spine_kernel_light_update *update,
                                           size_t chunk_index,
                                           mc_spine_kernel_light_chunk_info *out);
/* ordinal indexes the changed sections of that kind in ascending section order. */
int32_t mc_spine_kernel_light_update_section(const mc_spine_kernel_light_update *update,
                                             size_t chunk_index, uint8_t kind, size_t ordinal,
                                             mc_spine_kernel_light_section *out);
int32_t mc_spine_kernel_light_update_free(mc_spine_kernel_light_update *update);

/* Opens assets/block_states_1211.json and assets/blocks_1211.json for 1.21.1. The inputs
   must agree with each other and the compiled-in pinned state facts. */
int32_t mc_spine_kernel_registry_open_json(const uint8_t *states_json, size_t states_len,
                                           const uint8_t *blocks_json, size_t blocks_len,
                                           mc_spine_kernel_registry **out);
/* Opens any Minecraft version's pack from raw buffers: blocks_json is the pack's
   blocks.json (per-block names and state-id ranges), facts_bin its
   block_state_facts.bin (six bytes per state, so the state count is
   facts_len / 6 and must agree with blocks_json exactly), and states_json its
   optional block_states.json per-state table — NULL to skip it. Without the
   states table, per-state cross-checks and exact occlusion shapes are
   unavailable and every occluding state is treated as a full cube. */
int32_t mc_spine_kernel_registry_open_parts(const uint8_t *blocks_json, size_t blocks_len,
                                            const uint8_t *states_json, size_t states_len,
                                            const uint8_t *facts_bin, size_t facts_len,
                                            mc_spine_kernel_registry **out);
/* Opens a version pack directory. The version-neutral filenames (blocks.json,
   block_state_facts.bin, optional block_states.json) win, and the bundled
   *_1211.* spellings are the fallback. path is a UTF-8 directory path;
   a missing required file reports NOT_FOUND. */
int32_t mc_spine_kernel_registry_open_dir(const uint8_t *path, size_t path_len,
                                          mc_spine_kernel_registry **out);
int32_t mc_spine_kernel_registry_free(mc_spine_kernel_registry *registry);
int32_t mc_spine_kernel_registry_state_count(const mc_spine_kernel_registry *registry,
                                             uint32_t *out);
/* The namespaced block name, borrowed until registry_free. */
int32_t mc_spine_kernel_registry_state_name(const mc_spine_kernel_registry *registry,
                                            uint32_t state, mc_spine_kernel_text *out);
/* name is a namespaced block name. Unknown names return NOT_FOUND. */
int32_t mc_spine_kernel_registry_default_state(const mc_spine_kernel_registry *registry,
                                               const uint8_t *name, size_t len, uint32_t *out);
int32_t mc_spine_kernel_registry_state_facts(const mc_spine_kernel_registry *registry,
                                             uint32_t state, mc_spine_kernel_block_facts *out);

int32_t mc_spine_kernel_nbt_read_named_validate(const uint8_t *input, size_t len);
int32_t mc_spine_kernel_nbt_read_gzip_validate(const uint8_t *input, size_t len,
                                              size_t max_uncompressed);
int32_t mc_spine_kernel_nbt_roundtrip_named(const uint8_t *input, size_t len,
                                           mc_spine_kernel_bytes *out);

int32_t mc_spine_kernel_dispatcher_new(mc_spine_kernel_dispatcher **out);
int32_t mc_spine_kernel_dispatcher_free(mc_spine_kernel_dispatcher *dispatcher);
int32_t mc_spine_kernel_dispatcher_register_literal(mc_spine_kernel_dispatcher *dispatcher,
                                                    const uint8_t *literal, size_t len);
/* A syntax mismatch produces executable = 0. All positions are UTF-8 byte offsets. */
int32_t mc_spine_kernel_dispatcher_parse(const mc_spine_kernel_dispatcher *dispatcher,
                                         const uint8_t *input, size_t len,
                                         mc_spine_kernel_command_parse *out);
int32_t mc_spine_kernel_dispatcher_complete_suggestions(const mc_spine_kernel_dispatcher *dispatcher,
                                                        const uint8_t *input, size_t len,
                                                        size_t cursor,
                                                        mc_spine_kernel_suggestions **out);
int32_t mc_spine_kernel_suggestions_info(const mc_spine_kernel_suggestions *suggestions,
                                         mc_spine_kernel_suggestion_info *out);
int32_t mc_spine_kernel_suggestions_get(const mc_spine_kernel_suggestions *suggestions,
                                        size_t index, mc_spine_kernel_text *out);
int32_t mc_spine_kernel_suggestions_free(mc_spine_kernel_suggestions *suggestions);

/* Ordered world-space shape table. shapes may be NULL if count is 0.
   on_ground is 0 or 1. Positive max_up_step enables step-up. */
int32_t mc_spine_kernel_collide(const mc_spine_kernel_aabb *aabb, const double delta[3],
                                uint8_t on_ground, float max_up_step,
                                const mc_spine_kernel_aabb *shapes, size_t count,
                                mc_spine_kernel_collision *out);

/* Synchronous snapshot queries. context may be NULL if the callback permits it. */
int32_t mc_spine_kernel_fluid_impulse(const mc_spine_kernel_aabb *aabb,
                                     const mc_spine_kernel_fluid_input *input,
                                     mc_spine_kernel_fluid_lookup lookup, void *context,
                                     mc_spine_kernel_fluid_result *out);

#ifdef __cplusplus
}
#endif
#endif
