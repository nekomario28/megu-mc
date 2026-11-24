/* SPDX-License-Identifier: LGPL-3.0-or-later */
#ifndef MEGU_CLIENT_MESH_H
#define MEGU_CLIENT_MESH_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum {
    MEGU_MESH_ABI_VERSION = 1,
    MEGU_MESH_PROTOCOL = 767,
    MEGU_MESH_EDGE = 16,
    MEGU_MESH_SECTION_CELLS = 4096,
    MEGU_MESH_FACE_CELLS = 256,
    MEGU_MESH_LIGHT_BYTES = 2048,
    MEGU_MESH_EDGE_CORNER_CELLS = 200,
};

enum {
    MEGU_MESH_OK = 0,
    MEGU_MESH_INVALID_ARGUMENT = 1,
    MEGU_MESH_BUFFER_TOO_SMALL = 2,
    MEGU_MESH_MISSING_AIR = 0,
    MEGU_MESH_MISSING_OPAQUE = 1,
};

enum {
    MEGU_MESH_DOWN = 0, MEGU_MESH_UP = 1,
    MEGU_MESH_NORTH = 2, MEGU_MESH_SOUTH = 3,
    MEGU_MESH_WEST = 4, MEGU_MESH_EAST = 5,
};

enum {
    MEGU_BLOCK_OPAQUE = 1 << 0,
    MEGU_BLOCK_CUTOUT = 1 << 1,
    MEGU_BLOCK_TRANSLUCENT = 1 << 2,
    MEGU_BLOCK_WATERLOGGED = 1 << 3,
    MEGU_BLOCK_TINTED = 1 << 4,
    MEGU_BLOCK_FLUID_TINTED = 1 << 5,
    MEGU_BLOCK_SAME_STATE_CULL = 1 << 6,
    MEGU_BLOCK_ALL_FACES = 0x3f,
};

enum {
    MEGU_MESH_NO_FLUID = 0, MEGU_MESH_WATER = 1, MEGU_MESH_LAVA = 2,
    MEGU_LAYER_OPAQUE = 0, MEGU_LAYER_CUTOUT = 1,
    MEGU_LAYER_TRANSLUCENT = 2, MEGU_LAYER_FLUID = 3,
    MEGU_QUAD_TINTED = 1 << 0, MEGU_QUAD_FALLING = 1 << 1,
};

typedef struct megu_mesh_block {
    uint32_t state_id;
    uint32_t tint;
    uint32_t fluid_tint;
    uint8_t flags;
    uint8_t occlusion; /* 1 << direction for each fully occluding opaque face */
    uint8_t fluid_kind;
    uint8_t fluid_level;
} megu_mesh_block;

typedef struct megu_mesh_sample {
    megu_mesh_block block;
    uint8_t light; /* block low nibble, sky high nibble */
    uint8_t reserved[3]; /* initialize to zero */
} megu_mesh_sample;

typedef struct megu_mesh_quad {
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
} megu_mesh_quad;

typedef struct megu_mesh_input {
    uint32_t abi_version;
    uint32_t missing;
    /* Required. Index y*256 + z*16 + x. Each light array carries two
       successive cells per byte, low nibble for even indices. */
    const megu_mesh_block *blocks; /* MEGU_MESH_SECTION_CELLS */
    const uint8_t *block_light; /* MEGU_MESH_LIGHT_BYTES */
    const uint8_t *sky_light; /* MEGU_MESH_LIGHT_BYTES */
    /* Optional MEGU_MESH_FACE_CELLS per direction. Index z*16+x for
       DOWN/UP, y*16+x for NORTH/SOUTH and z*16+y for WEST/EAST. */
    const megu_mesh_sample *borders[6];
    /* Optional MEGU_MESH_EDGE_CORNER_CELLS. Edges first, grouped by
       free axis X/Y/Z. Within each group use (side_bits * 16 + free_coordinate).
       Corners start at 192. Pack the signs of fixed axes in X/Y/Z order,
       least significant first, with -1 = 0 and 16 = 1. */
    const megu_mesh_sample *edges_and_corners;
} megu_mesh_input;

uint32_t megu_mesh_abi_version(void);

/* All non-null buffers must be aligned and valid for their stated lengths
   until return. Output and count must be writable and disjoint from all inputs
   and each other. No pointers are retained. Call with NULL output and zero
   capacity to query *count. If capacity is too small, *count is the required
   number of quads and output remains untouched. On invalid arguments, a valid
   count pointer receives zero. Buffers and counts are in native byte order. */
int32_t megu_mesh_section(const megu_mesh_input *input,
                             megu_mesh_quad *output, size_t capacity, size_t *count);

enum {
    MEGU_MESH_MAX_COLUMN_SECTIONS = 128, /* tall Mod dimensions past
      the vanilla 24-section window, e.g. 96 sections at Y -512..1024 */
    MEGU_MESH_COLUMN_UNIFORM_AIR = 1,
    MEGU_MESH_COLUMN_SEALED_SOLID = 2,
};

typedef struct megu_mesh_column_section {
    uint32_t flags; /* zero for expanded cells, MEGU_MESH_COLUMN_UNIFORM_AIR
                       for air, MEGU_MESH_COLUMN_SEALED_SOLID for a caller
                       guarantee that every cell is a full-occlusion opaque
                       non-fluid cube so the interior never emits */
    const megu_mesh_block *blocks; /* 4096 cells for flags 0 and
                                          MEGU_MESH_COLUMN_SEALED_SOLID,
                                          NULL for uniform air */
    const uint8_t *block_light; /* required, including for uniform air */
    const uint8_t *sky_light;   /* required, including for uniform air */
} megu_mesh_column_section;

typedef struct megu_mesh_column_input {
    uint32_t abi_version; /* MEGU_MESH_ABI_VERSION */
    uint32_t missing;     /* MEGU_MESH_MISSING_AIR or MEGU_MESH_MISSING_OPAQUE */
    const megu_mesh_column_section *sections; /* bottom to top, with no gaps */
    size_t section_count; /* 1..MEGU_MESH_MAX_COLUMN_SECTIONS */
    /* North, South, West, East. Each optional buffer contains section_count
       consecutive 256-sample planes in the same order as sections. Within
       each plane use the same index as megu_mesh_input.borders. */
    const megu_mesh_sample *borders[4];
} megu_mesh_column_input;

/* The three client_mesh functions use the v1 buffer, count and status
   conventions above. Only explicitly optional pointers may be NULL.
   Column quads are grouped bottom to top with v1 order within each section.
   Positions use column-local XYZ: Y=0 is the bottom of sections[0].
   Interior Y planes and the available X/Y and Y/Z edges are stitched.
   The missing policy supplies the column ends and unavailable halo cells. */
int32_t megu_client_mesh_column(const megu_mesh_column_input *input,
                                    megu_mesh_quad *output, size_t capacity, size_t *count);

/* 16-byte GPU vertex, four native-endian uint32_t words. Inclusive bit ranges
   start from the least significant bit. Input corner order is preserved.
   position:   X 0..4, Z 5..9, Y*1024 rounded 10..29, zero 30..31.
   state_id:   full block registry ID, also the source state for fluids.
   The 20-bit Y*1024 field caps column-local Y below 1024 blocks (the first 64
   sections); taller columns mesh fully, but quads above the ceiling cannot be
   packed and callers must split them before a vertex-format upload.
   attributes: direction 0..2, corner AO 3..4, block light 5..8, sky light
               9..12, layer 13..14, fluid kind 15..16, diagonal 17,
               quad flags 18..19, U width 20..24, V height 25..29,
               corner index 30..31.
   tint:       original quad tint.
   Width and height are 1..16 blocks (1 for fluids). Corner indices 0, 1, 2,
   3 correspond to tiled UVs (0,0), (width,0), (width,height), (0,height).
   Use the quad diagonal to select the index pattern from megu_mesh_quad. */
typedef struct megu_mesh_packed_vertex {
    uint32_t position;
    uint32_t state_id;
    uint32_t attributes;
    uint32_t tint;
} megu_mesh_packed_vertex;

/* Count and capacity are in vertices. Quads may come from section or column
   meshing. quads must be non-NULL even if quad_count is zero. Output can
   point directly into mapped GPU upload memory with the required alignment. */
int32_t megu_client_mesh_pack_vertices(const megu_mesh_quad *quads, size_t quad_count,
                                           megu_mesh_packed_vertex *output,
                                           size_t capacity, size_t *count);

/* Returns indices into quads, sorted by decreasing squared camera distance
   to the centroid, with lower input indices first on ties. Includes the
   translucent block layer and water fluid quads. Camera XYZ must be finite
   and use the same origin as quads. Count and capacity are in uint32_t quad
   indices. quads must be non-NULL even if quad_count is zero. */
int32_t megu_client_mesh_sort_translucent(const megu_mesh_quad *quads, size_t quad_count,
                                              float camera_x, float camera_y, float camera_z,
                                              uint32_t *output, size_t capacity, size_t *count);

/* Direction buckets (additive ABI v1 extension). Bucket indices 0..5 are the
   face directions MEGU_MESH_DOWN..MEGU_MESH_EAST; index 6 is the
   unassigned bucket and always draws. Drawing the buckets whose side faces
   the camera plus the unassigned bucket skips the three back-facing ranges;
   a camera inside the section bounds draws all seven. Ranges are in quads and
   scale by four for packed-vertex buffers. */
enum {
    MEGU_MESH_DIR_BUCKETS = 7,
    MEGU_MESH_DIR_UNASSIGNED = 6,
};

typedef struct megu_mesh_dir_buckets {
    uint32_t offset[MEGU_MESH_DIR_BUCKETS]; /* first quad of each bucket */
    uint32_t count[MEGU_MESH_DIR_BUCKETS];  /* quads per bucket */
} megu_mesh_dir_buckets;

/* Regroups quads into the seven direction buckets, stable within each bucket,
   and reports the contiguous ranges. Uses the v1 buffer, count and status
   conventions: NULL output with zero capacity queries *count and still fills
   *buckets when non-NULL. buckets may be NULL when the ranges are not needed.
   Mesher output always uses buckets 0..5, leaving unassigned empty. */
int32_t megu_client_mesh_bucket_directions(const megu_mesh_quad *quads, size_t quad_count,
                                               megu_mesh_quad *output, size_t capacity,
                                               megu_mesh_dir_buckets *buckets, size_t *count);

/* Horizon LOD column summaries (additive ABI v1 extension). Summarizes the
   sections of one chunk column into edge*edge cells, each holding the
   highest non-air block found in its footprint: state_id is the global
   palette id at top_y (0 when empty), tint is the block tint for tinted
   tops, the fluid tint for fluid tops and 0 otherwise, top_y is the
   column-local height (-1 when empty) and light packs block in the low
   nibble and sky in the high nibble at top_y. Cells are written in
   x-fastest, z-major order, matching section_index's X/Z layout. The
   column input reuses megu_mesh_column_input; cardinal border planes
   are accepted but not needed for summaries. */
enum {
    MEGU_MESH_LOD_COARSE = 4,  /* one cell per 4x4-block footprint */
    MEGU_MESH_LOD_FINE = 16,   /* one cell per block column */
};

typedef struct megu_lod_cell {
    uint32_t state_id;
    uint32_t tint;
    int16_t top_y;
    uint8_t light;
    uint8_t pad;
} megu_lod_cell;

/* resolution must be MEGU_MESH_LOD_COARSE or MEGU_MESH_LOD_FINE.
   count receives resolution*resolution (16 or 256). Uses the v1 buffer,
   count and status conventions: NULL output with zero capacity queries
   *count; a too-small buffer reports the required count and leaves output
   untouched. */
int32_t megu_client_mesh_summarize_column(const megu_mesh_column_input *input,
                                              uint32_t resolution,
                                              megu_lod_cell *output,
                                              size_t capacity, size_t *count);

#ifdef __cplusplus
}
#endif
#endif
