/* SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later */
#ifndef MC_SPINE_CLIENT_MESH_H
#define MC_SPINE_CLIENT_MESH_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum {
    MC_SPINE_MESH_ABI_VERSION = 1,
    MC_SPINE_MESH_PROTOCOL = 767,
    MC_SPINE_MESH_EDGE = 16,
    MC_SPINE_MESH_SECTION_CELLS = 4096,
    MC_SPINE_MESH_FACE_CELLS = 256,
    MC_SPINE_MESH_LIGHT_BYTES = 2048,
    MC_SPINE_MESH_EDGE_CORNER_CELLS = 200,
};

enum {
    MC_SPINE_MESH_OK = 0,
    MC_SPINE_MESH_INVALID_ARGUMENT = 1,
    MC_SPINE_MESH_BUFFER_TOO_SMALL = 2,
    MC_SPINE_MESH_MISSING_AIR = 0,
    MC_SPINE_MESH_MISSING_OPAQUE = 1,
};

enum {
    MC_SPINE_MESH_DOWN = 0, MC_SPINE_MESH_UP = 1,
    MC_SPINE_MESH_NORTH = 2, MC_SPINE_MESH_SOUTH = 3,
    MC_SPINE_MESH_WEST = 4, MC_SPINE_MESH_EAST = 5,
};

enum {
    MC_SPINE_BLOCK_OPAQUE = 1 << 0,
    MC_SPINE_BLOCK_CUTOUT = 1 << 1,
    MC_SPINE_BLOCK_TRANSLUCENT = 1 << 2,
    MC_SPINE_BLOCK_WATERLOGGED = 1 << 3,
    MC_SPINE_BLOCK_TINTED = 1 << 4,
    MC_SPINE_BLOCK_FLUID_TINTED = 1 << 5,
    MC_SPINE_BLOCK_SAME_STATE_CULL = 1 << 6,
    MC_SPINE_BLOCK_ALL_FACES = 0x3f,
};

enum {
    MC_SPINE_MESH_NO_FLUID = 0, MC_SPINE_MESH_WATER = 1, MC_SPINE_MESH_LAVA = 2,
    MC_SPINE_LAYER_OPAQUE = 0, MC_SPINE_LAYER_CUTOUT = 1,
    MC_SPINE_LAYER_TRANSLUCENT = 2, MC_SPINE_LAYER_FLUID = 3,
    MC_SPINE_QUAD_TINTED = 1 << 0, MC_SPINE_QUAD_FALLING = 1 << 1,
};

typedef struct mc_spine_mesh_block {
    uint32_t state_id;
    uint32_t tint;
    uint32_t fluid_tint;
    uint8_t flags;
    uint8_t occlusion; /* 1 << direction for each fully occluding opaque face */
    uint8_t fluid_kind;
    uint8_t fluid_level;
} mc_spine_mesh_block;

typedef struct mc_spine_mesh_sample {
    mc_spine_mesh_block block;
    uint8_t light; /* block low nibble, sky high nibble */
    uint8_t reserved[3]; /* initialize to zero */
} mc_spine_mesh_sample;

typedef struct mc_spine_mesh_quad {
    float positions[4][3]; /* section-local XYZ, outward counterclockwise */
    uint32_t state_id;
    uint32_t tint;
    float flow[2]; /* downhill slope in world X/Z */
    uint8_t ao[4]; /* 0 darkest, 3 brightest */
    uint8_t block_light[4];
    uint8_t sky_light[4];
    uint8_t face_light; /* block low nibble, sky high nibble */
    uint8_t direction;
    uint8_t layer;
    uint8_t diagonal; /* 0: 012 023; 1: 013 123 */
    uint8_t fluid_kind;
    uint8_t flags;
    uint8_t reserved;
} mc_spine_mesh_quad;

typedef struct mc_spine_mesh_input {
    uint32_t abi_version;
    uint32_t missing;
    /* Required. Index y*256 + z*16 + x. Each light array carries two
       successive cells per byte, low nibble for even indices. */
    const mc_spine_mesh_block *blocks; /* MC_SPINE_MESH_SECTION_CELLS */
    const uint8_t *block_light; /* MC_SPINE_MESH_LIGHT_BYTES */
    const uint8_t *sky_light; /* MC_SPINE_MESH_LIGHT_BYTES */
    /* Optional MC_SPINE_MESH_FACE_CELLS per direction. Index z*16+x for
       DOWN/UP, y*16+x for NORTH/SOUTH and z*16+y for WEST/EAST. */
    const mc_spine_mesh_sample *borders[6];
    /* Optional MC_SPINE_MESH_EDGE_CORNER_CELLS. Edges first, grouped by
       free axis X/Y/Z. Within each group use (side_bits * 16 + free_coordinate).
       Corners start at 192. Pack the signs of fixed axes in X/Y/Z order,
       least significant first, with -1 = 0 and 16 = 1. */
    const mc_spine_mesh_sample *edges_and_corners;
} mc_spine_mesh_input;

uint32_t mc_spine_mesh_abi_version(void);

/* All non-null buffers must be aligned and valid for their stated lengths
   until return. Output and count must be writable and disjoint from all inputs
   and each other. No pointers are retained. Call with NULL output and zero
   capacity to query *count. If capacity is too small, *count is the required
   number of quads and output remains untouched. On invalid arguments, a valid
   count pointer receives zero. Buffers and counts are in native byte order. */
int32_t mc_spine_mesh_section(const mc_spine_mesh_input *input,
                             mc_spine_mesh_quad *output, size_t capacity, size_t *count);

#ifdef __cplusplus
}
#endif
#endif
