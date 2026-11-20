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
    MC_SPINE_KERNEL_PANIC = 12
};

typedef struct mc_spine_kernel_palette mc_spine_kernel_palette;
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
