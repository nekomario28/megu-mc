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
    return 0;
}
