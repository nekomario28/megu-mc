/* SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later */
#include "mc_spine_client_mesh.h"

#include <assert.h>
#include <stdio.h>

_Static_assert(sizeof(mc_spine_mesh_block) == 16, "block size");
_Static_assert(offsetof(mc_spine_mesh_block, fluid_tint) == 8, "fluid tint offset");
_Static_assert(offsetof(mc_spine_mesh_block, flags) == 12, "block flags offset");
_Static_assert(sizeof(mc_spine_mesh_sample) == 20, "sample size");
_Static_assert(offsetof(mc_spine_mesh_sample, light) == 16, "sample light offset");
_Static_assert(sizeof(mc_spine_mesh_quad) == 84, "quad size");
_Static_assert(offsetof(mc_spine_mesh_quad, state_id) == 48, "state offset");
_Static_assert(offsetof(mc_spine_mesh_quad, flow) == 56, "flow offset");
_Static_assert(offsetof(mc_spine_mesh_quad, ao) == 64, "AO offset");
_Static_assert(offsetof(mc_spine_mesh_quad, block_light) == 68, "block light offset");
_Static_assert(offsetof(mc_spine_mesh_quad, sky_light) == 72, "sky light offset");
_Static_assert(offsetof(mc_spine_mesh_quad, face_light) == 76, "face light offset");
_Static_assert(offsetof(mc_spine_mesh_quad, diagonal) == 79, "diagonal offset");
_Static_assert(offsetof(mc_spine_mesh_quad, flags) == 81, "quad flags offset");
_Static_assert(offsetof(mc_spine_mesh_input, blocks) == 8, "block pointer offset");
_Static_assert(offsetof(mc_spine_mesh_input, borders) == 8 + 3 * sizeof(void *), "borders offset");
_Static_assert(offsetof(mc_spine_mesh_input, edges_and_corners) == 8 + 9 * sizeof(void *), "halo offset");
_Static_assert(sizeof(mc_spine_mesh_input) == 8 + 10 * sizeof(void *), "input size");
_Static_assert(sizeof(mc_spine_mesh_packed_vertex) == 16, "packed GPU stride");
_Static_assert(offsetof(mc_spine_mesh_packed_vertex, position) == 0, "packed position offset");
_Static_assert(offsetof(mc_spine_mesh_packed_vertex, state_id) == 4, "packed state offset");
_Static_assert(offsetof(mc_spine_mesh_packed_vertex, attributes) == 8, "packed attribute offset");
_Static_assert(offsetof(mc_spine_mesh_packed_vertex, tint) == 12, "packed tint offset");

static void column_roundtrip(void) {
    static mc_spine_mesh_block blocks[2][MC_SPINE_MESH_SECTION_CELLS];
    static uint8_t light[MC_SPINE_MESH_LIGHT_BYTES];
    static mc_spine_mesh_sample east[2][MC_SPINE_MESH_FACE_CELLS];
    const mc_spine_mesh_block glass = {
        .state_id = 519, .flags = MC_SPINE_BLOCK_TRANSLUCENT | MC_SPINE_BLOCK_SAME_STATE_CULL,
    };
    blocks[0][15 * 256 + 8 * 16 + 15] = glass;
    blocks[1][8 * 16 + 15] = glass;
    east[1][8 * 16].block = (mc_spine_mesh_block){
        .state_id = 1, .flags = MC_SPINE_BLOCK_OPAQUE, .occlusion = MC_SPINE_BLOCK_ALL_FACES,
    };
    mc_spine_mesh_column_section sections[2] = {
        {.blocks = blocks[0], .block_light = light, .sky_light = light},
        {.blocks = blocks[1], .block_light = light, .sky_light = light},
    };
    const mc_spine_mesh_column_input input = {
        .abi_version = MC_SPINE_MESH_ABI_VERSION, .missing = MC_SPINE_MESH_MISSING_AIR,
        .sections = sections, .section_count = 2, .borders = {[3] = east[0]},
    };
    size_t count = 99;
    assert(mc_spine_client_mesh_column(NULL, NULL, 0, &count) == MC_SPINE_MESH_INVALID_ARGUMENT);
    assert(count == 0);
    sections[1].blocks = NULL;
    assert(mc_spine_client_mesh_column(&input, NULL, 0, &count) == MC_SPINE_MESH_INVALID_ARGUMENT);
    assert(count == 0);
    sections[1].blocks = blocks[1];
    assert(mc_spine_client_mesh_column(&input, NULL, 0, &count) == MC_SPINE_MESH_OK);
    assert(count == 9);

    mc_spine_mesh_quad quads[9] = {{.state_id = 54321}};
    assert(mc_spine_client_mesh_column(&input, quads, 8, &count) == MC_SPINE_MESH_BUFFER_TOO_SMALL);
    assert(count == 9 && quads[0].state_id == 54321);
    assert(mc_spine_client_mesh_column(&input, quads, 9, &count) == MC_SPINE_MESH_OK);
    assert(count == 9);
    assert(quads[0].direction == MC_SPINE_MESH_DOWN && quads[0].positions[0][1] == 15);
    assert(quads[4].direction == MC_SPINE_MESH_EAST);
    assert(quads[5].direction == MC_SPINE_MESH_UP && quads[5].positions[0][1] == 17);

    mc_spine_mesh_packed_vertex vertices[36] = {{.state_id = 54321}};
    assert(mc_spine_client_mesh_pack_vertices(NULL, 0, NULL, 0, &count)
           == MC_SPINE_MESH_INVALID_ARGUMENT && count == 0);
    assert(mc_spine_client_mesh_pack_vertices(quads, 9, NULL, 0, &count) == MC_SPINE_MESH_OK);
    assert(count == 36);
    quads[8].direction = UINT8_MAX;
    assert(mc_spine_client_mesh_pack_vertices(quads, 9, vertices, 36, &count)
           == MC_SPINE_MESH_INVALID_ARGUMENT);
    assert(count == 0 && vertices[0].state_id == 54321);
    quads[8].direction = MC_SPINE_MESH_WEST;
    assert(mc_spine_client_mesh_pack_vertices(quads, 9, vertices, 36, &count) == MC_SPINE_MESH_OK);
    assert(count == 36);
    assert(vertices[0].position == (15u | (8u << 5) | (15u * 1024u << 10)));
    assert(vertices[0].state_id == 519);
    assert(vertices[2].attributes >> 30 == 2);
    assert((vertices[0].attributes >> 3 & 3) == quads[0].ao[0]);
    assert((vertices[0].attributes >> 5 & 15) == quads[0].block_light[0]);
    assert((vertices[0].attributes >> 9 & 15) == quads[0].sky_light[0]);
    assert((vertices[0].attributes >> 13 & 3) == MC_SPINE_LAYER_TRANSLUCENT);
    assert((vertices[0].attributes >> 20 & 31) == 1 && (vertices[0].attributes >> 25 & 31) == 1);

    uint32_t indices[9];
    assert(mc_spine_client_mesh_sort_translucent(NULL, 0, 0, 0, 0, NULL, 0, &count)
           == MC_SPINE_MESH_INVALID_ARGUMENT && count == 0);
    assert(mc_spine_client_mesh_sort_translucent(quads, 9, 15, 100, 8, indices, 9, &count)
           == MC_SPINE_MESH_OK);
    assert(count == 9 && indices[0] == 0 && indices[8] == 5);

    mc_spine_mesh_dir_buckets buckets = {{0}, {0}};
    mc_spine_mesh_quad grouped[9];
    assert(mc_spine_client_mesh_bucket_directions(NULL, 0, NULL, 0, &buckets, &count)
           == MC_SPINE_MESH_INVALID_ARGUMENT && count == 0);
    assert(mc_spine_client_mesh_bucket_directions(quads, 9, NULL, 0, &buckets, &count)
           == MC_SPINE_MESH_OK);
    assert(count == 9 && buckets.count[MC_SPINE_MESH_DIR_UNASSIGNED] == 0);
    size_t total = 0;
    for (size_t b = 0; b < MC_SPINE_MESH_DIR_BUCKETS; ++b) {
        assert(buckets.offset[b] == total);
        total += buckets.count[b];
    }
    assert(total == 9);
    assert(mc_spine_client_mesh_bucket_directions(quads, 9, grouped, 8, &buckets, &count)
           == MC_SPINE_MESH_BUFFER_TOO_SMALL && count == 9);
    assert(mc_spine_client_mesh_bucket_directions(quads, 9, grouped, 9, &buckets, &count)
           == MC_SPINE_MESH_OK);
    for (size_t b = 0; b < MC_SPINE_MESH_DIR_BUCKETS; ++b) {
        for (uint32_t i = 0; i < buckets.count[b]; ++i) {
            assert(grouped[buckets.offset[b] + i].direction == b);
        }
    }
    puts("C ABI column roundtrip: 9 quads, 36 packed vertices, 9 sorted indices, direction buckets");
}

int main(void) {
    static mc_spine_mesh_block blocks[MC_SPINE_MESH_SECTION_CELLS];
    static uint8_t block_light[MC_SPINE_MESH_LIGHT_BYTES];
    static uint8_t sky_light[MC_SPINE_MESH_LIGHT_BYTES];
    static mc_spine_mesh_sample east[MC_SPINE_MESH_FACE_CELLS];
    const size_t index = 4 * 256 + 7 * 16 + 15;
    blocks[index] = (mc_spine_mesh_block){
        .state_id = 26683, .flags = MC_SPINE_BLOCK_OPAQUE | MC_SPINE_BLOCK_TINTED,
        .occlusion = MC_SPINE_BLOCK_ALL_FACES, .tint = 0x113355,
    };
    block_light[index / 2] = 0xb0;
    sky_light[(index + 256) / 2] = 0xd0;
    east[7 * 16 + 4].block = (mc_spine_mesh_block){
        .state_id = 1, .flags = MC_SPINE_BLOCK_OPAQUE, .occlusion = MC_SPINE_BLOCK_ALL_FACES,
    };

    const mc_spine_mesh_input input = {
        .abi_version = MC_SPINE_MESH_ABI_VERSION, .missing = MC_SPINE_MESH_MISSING_AIR,
        .blocks = blocks, .block_light = block_light, .sky_light = sky_light,
        .borders = {[MC_SPINE_MESH_EAST] = east},
    };
    assert(mc_spine_mesh_abi_version() == MC_SPINE_MESH_ABI_VERSION);
    size_t count = 0;
    assert(mc_spine_mesh_section(&input, NULL, 0, &count) == MC_SPINE_MESH_OK);
    assert(count == 5);
    mc_spine_mesh_quad quads[5] = {{.state_id = 54321}};
    assert(mc_spine_mesh_section(&input, quads, 4, &count) == MC_SPINE_MESH_BUFFER_TOO_SMALL);
    assert(count == 5 && quads[0].state_id == 54321);
    assert(mc_spine_mesh_section(&input, quads, 5, &count) == MC_SPINE_MESH_OK);
    for (size_t i = 0; i < count; ++i) {
        assert(quads[i].state_id == 26683 && quads[i].direction != MC_SPINE_MESH_EAST);
        assert(quads[i].layer == MC_SPINE_LAYER_OPAQUE);
        assert(quads[i].tint == 0x113355 && quads[i].flags == MC_SPINE_QUAD_TINTED);
    }
    assert(quads[1].direction == MC_SPINE_MESH_UP);
    assert(quads[1].face_light == 0xdb);
    assert(quads[1].positions[0][0] == 15 && quads[1].positions[0][1] == 5
           && quads[1].positions[0][2] == 7);
    puts("C ABI v1 roundtrip: 5 quads, border culling and block/sky nibbles verified");
    column_roundtrip();
    return 0;
}
