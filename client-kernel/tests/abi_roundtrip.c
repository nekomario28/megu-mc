/* SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later */
#include "mc_spine_kernel.h"

#include <assert.h>
#include <stdio.h>
#include <string.h>

#define OK(expr) assert((expr) == MC_SPINE_KERNEL_OK)
#define UTF8(s) (const uint8_t *)(s), sizeof(s) - 1

static void palette_roundtrip(void) {
    mc_spine_kernel_palette *section = NULL;
    mc_spine_kernel_bytes wire;
    const uint8_t single[] = {0, 0, 0};
    uint32_t states[MC_SPINE_KERNEL_SECTION_CELLS];
    uint32_t state = 999;
    uint16_t non_air = 999;
    uint8_t flag = 99, bits = 99;

    assert(mc_spine_kernel_palette_free(NULL) == MC_SPINE_KERNEL_NULL);
    assert(mc_spine_kernel_palette_new_single(0, NULL) == MC_SPINE_KERNEL_NULL);
    assert(mc_spine_kernel_palette_new_single(MC_SPINE_KERNEL_STATE_COUNT, &section)
           == MC_SPINE_KERNEL_INVALID_STATE);
    assert(section == NULL);
    OK(mc_spine_kernel_palette_new_single(0, &section));
    OK(mc_spine_kernel_palette_is_uniform(section, &flag));
    assert(flag == 1);
    OK(mc_spine_kernel_palette_uniform_state(section, &flag, &state));
    assert(flag == 1 && state == 0);
    OK(mc_spine_kernel_palette_write_block_states_wire(section, &wire));
    assert(wire.len == sizeof(single) && memcmp(wire.data, single, sizeof(single)) == 0);
    OK(mc_spine_kernel_bytes_free(wire));

    assert(mc_spine_kernel_palette_set(section, 4096, 0) == MC_SPINE_KERNEL_OUT_OF_RANGE);
    assert(mc_spine_kernel_palette_set(section, 0, UINT32_MAX) == MC_SPINE_KERNEL_INVALID_STATE);
    assert(mc_spine_kernel_palette_get(section, 4096, &state) == MC_SPINE_KERNEL_OUT_OF_RANGE);
    assert(state == 0);
    OK(mc_spine_kernel_palette_set(section, 16, 1));
    OK(mc_spine_kernel_palette_get(section, 16, &state));
    assert(state == 1);
    OK(mc_spine_kernel_palette_non_air(section, &non_air));
    assert(non_air == 1);
    OK(mc_spine_kernel_palette_bits(section, &bits));
    assert(bits == 4);
    OK(mc_spine_kernel_palette_uniform_state(section, &flag, &state));
    assert(flag == 0 && state == 0);
    states[0] = 333;
    assert(mc_spine_kernel_palette_unpack_into(section, states, 4095)
           == MC_SPINE_KERNEL_BUFFER_TOO_SMALL);
    assert(states[0] == 333);
    OK(mc_spine_kernel_palette_unpack_into(section, states, MC_SPINE_KERNEL_SECTION_CELLS));
    for (size_t i = 0; i < MC_SPINE_KERNEL_SECTION_CELLS; ++i) {
        assert(states[i] == (i == 16 ? 1u : 0u));
    }

    OK(mc_spine_kernel_palette_write_block_states_wire(section, &wire));
    OK(mc_spine_kernel_palette_free(section));
    /* 4 bits, palette [0, 1], 256 words. Cell 16 starts the second word. */
    const uint8_t prefix[] = {4, 2, 0, 1, 0x80, 2};
    assert(wire.len == sizeof(prefix) + 256 * 8);
    assert(memcmp(wire.data, prefix, sizeof(prefix)) == 0);
    for (size_t i = sizeof(prefix); i < wire.len; ++i) {
        assert(wire.data[i] == (i == sizeof(prefix) + 15 ? 1 : 0));
    }
    OK(mc_spine_kernel_bytes_free(wire));
}

static void nbt_roundtrip(void) {
    const uint8_t nbt[] = {10, 0, 0, 0, 0};
    const uint8_t gzip[] = {
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x03, 0xe3, 0x62,
        0x60, 0x60, 0x00, 0x00, 0x78, 0x3f, 0xf9, 0x4e, 0x04, 0x00, 0x00, 0x00
    };
    uint8_t deep[4 * 513] = {0};
    for (size_t i = 0; i < 513; ++i) {
        deep[3 * i] = 10;
    }
    mc_spine_kernel_bytes result = {NULL, 123};

    assert(mc_spine_kernel_bytes_free(result) == MC_SPINE_KERNEL_NULL);
    assert(mc_spine_kernel_nbt_read_named_validate(NULL, 0) == MC_SPINE_KERNEL_NULL);
    assert(mc_spine_kernel_nbt_read_named_validate(nbt, SIZE_MAX) == MC_SPINE_KERNEL_OUT_OF_RANGE);
    assert(mc_spine_kernel_nbt_roundtrip_named(nbt, sizeof(nbt), &result)
           == MC_SPINE_KERNEL_INVALID_NBT);
    assert(result.data == NULL && result.len == 123);
    assert(mc_spine_kernel_nbt_read_named_validate(deep, sizeof(deep)) == MC_SPINE_KERNEL_NBT_DEPTH);
    OK(mc_spine_kernel_nbt_read_named_validate(nbt, sizeof(nbt) - 1));
    OK(mc_spine_kernel_nbt_roundtrip_named(nbt, sizeof(nbt) - 1, &result));
    assert(result.len == sizeof(nbt) - 1 && memcmp(result.data, nbt, result.len) == 0);
    OK(mc_spine_kernel_bytes_free(result));
    assert(mc_spine_kernel_nbt_read_gzip_validate(gzip, sizeof(gzip), 3)
           == MC_SPINE_KERNEL_BYTE_BUDGET);
    assert(mc_spine_kernel_nbt_read_gzip_validate(gzip, sizeof(gzip) - 1, 4)
           == MC_SPINE_KERNEL_INVALID_GZIP);
    OK(mc_spine_kernel_nbt_read_gzip_validate(gzip, sizeof(gzip), 4));
}

static void dispatcher_roundtrip(void) {
    mc_spine_kernel_dispatcher *dispatcher = NULL;
    mc_spine_kernel_suggestions *suggestions = NULL;
    mc_spine_kernel_command_parse parsed = {999, 99};
    mc_spine_kernel_suggestion_info info;
    mc_spine_kernel_text item;
    const char input[] = "\xc3\xa9x";
    const char eclair[] = "\xc3\xa9" "clair";
    const char echo[] = "\xc3\x89" "cho";

    OK(mc_spine_kernel_dispatcher_new(&dispatcher));
    OK(mc_spine_kernel_dispatcher_register_literal(dispatcher, UTF8(eclair)));
    OK(mc_spine_kernel_dispatcher_register_literal(dispatcher, UTF8(echo)));
    assert(mc_spine_kernel_dispatcher_register_literal(dispatcher, UTF8("with space"))
           == MC_SPINE_KERNEL_INVALID_ARGUMENT);
    assert(mc_spine_kernel_dispatcher_parse(dispatcher, UTF8("\xff"), &parsed)
           == MC_SPINE_KERNEL_INVALID_UTF8);
    assert(parsed.cursor == 999 && parsed.executable == 99);
    OK(mc_spine_kernel_dispatcher_parse(dispatcher, UTF8(eclair), &parsed));
    assert(parsed.cursor == sizeof(eclair) - 1 && parsed.executable == 1);
    OK(mc_spine_kernel_dispatcher_parse(dispatcher, UTF8("unknown"), &parsed));
    assert(parsed.cursor == 0 && parsed.executable == 0);

    assert(mc_spine_kernel_dispatcher_complete_suggestions(dispatcher, UTF8(input), 1, &suggestions)
           == MC_SPINE_KERNEL_OUT_OF_RANGE);
    assert(suggestions == NULL);
    OK(mc_spine_kernel_dispatcher_complete_suggestions(dispatcher, UTF8(input), 2, &suggestions));
    OK(mc_spine_kernel_dispatcher_free(dispatcher));
    OK(mc_spine_kernel_suggestions_info(suggestions, &info));
    assert(info.start == 0 && info.end == 2 && info.count == 2);
    OK(mc_spine_kernel_suggestions_get(suggestions, 0, &item));
    assert(item.len == sizeof(echo) - 1 && memcmp(item.data, echo, item.len) == 0);
    OK(mc_spine_kernel_suggestions_get(suggestions, 1, &item));
    assert(item.len == sizeof(eclair) - 1 && memcmp(item.data, eclair, item.len) == 0);
    assert(mc_spine_kernel_suggestions_get(suggestions, 2, &item) == MC_SPINE_KERNEL_OUT_OF_RANGE);
    assert(item.len == sizeof(eclair) - 1);
    OK(mc_spine_kernel_suggestions_free(suggestions));
}

static int32_t fluid_lookup(void *context, int32_t x, int32_t y, int32_t z,
                            mc_spine_kernel_fluid_facts *out) {
    unsigned *calls = context;
    ++*calls;
    if (y == 0 && z == 0 && (x == 0 || x == 1)) {
        out->kind = MC_SPINE_KERNEL_FLUID_WATER;
        out->amount = x == 0 ? 8 : 7;
    }
    return 0;
}

static int32_t fluid_failure(void *context, int32_t x, int32_t y, int32_t z,
                             mc_spine_kernel_fluid_facts *out) {
    (void) x;
    (void) y;
    (void) z;
    (void) out;
    unsigned *calls = context;
    ++*calls;
    return -1;
}

static void physics_roundtrip(void) {
    const mc_spine_kernel_aabb box = {{0, 0, 0}, {1, 1, 1}};
    const mc_spine_kernel_aabb slab = {{1, 0, 0}, {2, 0.5, 1}};
    const double delta[] = {0.5, 0, 0};
    mc_spine_kernel_collision hit;
    const mc_spine_kernel_fluid_input input = {
        {0, 0, 0}, MC_SPINE_KERNEL_FLUID_WATER, MC_SPINE_KERNEL_FLUID_PUSHED
    };
    mc_spine_kernel_fluid_result flow;
    unsigned calls = 0;

    assert(mc_spine_kernel_collide(&box, delta, 1, 0.5f, NULL, 1, &hit) == MC_SPINE_KERNEL_NULL);
    OK(mc_spine_kernel_collide(&box, delta, 1, 0.5f, NULL, 0, &hit));
    assert(hit.flags == 0 && hit.delta[0] == 0.5 && hit.delta[1] == 0);
    OK(mc_spine_kernel_collide(&box, delta, 1, 0.5f, &slab, 1, &hit));
    assert(hit.flags == (MC_SPINE_KERNEL_COLLISION_VERTICAL | MC_SPINE_KERNEL_COLLISION_STEPPED_UP));
    assert(hit.delta[0] == 0.5 && hit.delta[1] == 0.5 && hit.delta[2] == 0);

    assert(mc_spine_kernel_fluid_impulse(&box, &input, NULL, NULL, &flow) == MC_SPINE_KERNEL_NULL);
    OK(mc_spine_kernel_fluid_impulse(&box, &input, fluid_lookup, &calls, &flow));
    assert(calls > 1 && flow.in_fluid == 1 && flow.max_height > 0.88 && flow.max_height < 0.89);
    assert(flow.push[0] == 0.014 && flow.push[1] == 0 && flow.push[2] == 0);
    calls = 0;
    assert(mc_spine_kernel_fluid_impulse(&box, &input, fluid_failure, &calls, &flow)
           == MC_SPINE_KERNEL_QUERY_FAILED);
    assert(calls == 1 && flow.push[0] == 0.014);
}

int main(void) {
    assert(mc_spine_kernel_abi_version() == MC_SPINE_KERNEL_ABI_VERSION);
    palette_roundtrip();
    nbt_roundtrip();
    dispatcher_roundtrip();
    physics_roundtrip();
    puts("client-kernel C roundtrip passed");
    return 0;
}
