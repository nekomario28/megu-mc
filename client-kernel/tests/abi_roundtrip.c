/* SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later */
#include "mc_spine_kernel.h"

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
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

static void palette_wire_decode(void) {
    mc_spine_kernel_palette *section = NULL, *decoded = NULL;
    mc_spine_kernel_bytes wire, again;
    uint32_t cells[MC_SPINE_KERNEL_SECTION_CELLS], copy[MC_SPINE_KERNEL_SECTION_CELLS];
    uint8_t bits = 99;
    uint8_t padded[64];
    size_t consumed = 777;
    uint32_t i;
    static const uint32_t widths[] = {2, 17, 129, 257};
    size_t w;

    for (w = 0; w < sizeof(widths) / sizeof(widths[0]); w++) {
        OK(mc_spine_kernel_palette_new_single(0, &section));
        for (i = 0; i < MC_SPINE_KERNEL_SECTION_CELLS; i++)
            OK(mc_spine_kernel_palette_set(section, i, (i * 7) % widths[w] * 3));
        OK(mc_spine_kernel_palette_write_block_states_wire(section, &wire));
        OK(mc_spine_kernel_palette_decode_block_states_wire(wire.data, (size_t)wire.len, &consumed,
                                                            &decoded));
        assert(consumed == wire.len);
        OK(mc_spine_kernel_palette_bits(decoded, &bits));
        assert(bits == (w == 0 ? 4 : w == 1 ? 5 : w == 2 ? 8 : 15));
        OK(mc_spine_kernel_palette_unpack_into(section, cells, MC_SPINE_KERNEL_SECTION_CELLS));
        OK(mc_spine_kernel_palette_unpack_into(decoded, copy, MC_SPINE_KERNEL_SECTION_CELLS));
        assert(memcmp(cells, copy, sizeof(cells)) == 0);
        OK(mc_spine_kernel_palette_write_block_states_wire(decoded, &again));
        assert(again.len == wire.len && memcmp(again.data, wire.data, (size_t)wire.len) == 0);
        OK(mc_spine_kernel_bytes_free(again));
        /* Trailing bytes are not consumed. */
        if (wire.len + 2 <= sizeof(padded)) {
            memcpy(padded, wire.data, (size_t)wire.len);
            padded[wire.len] = 0xaa;
            padded[wire.len + 1] = 0xbb;
            OK(mc_spine_kernel_palette_free(decoded));
            OK(mc_spine_kernel_palette_decode_block_states_wire(padded, (size_t)wire.len + 2,
                                                                &consumed, &decoded));
            assert(consumed == wire.len);
        }
        OK(mc_spine_kernel_palette_free(decoded));
        OK(mc_spine_kernel_palette_free(section));
        OK(mc_spine_kernel_bytes_free(wire));
    }

    {
        const uint8_t single[] = {0, 0xd0, 0x01, 0};
        const uint8_t bad_state[] = {0, 0xbc, 0xd0, 0x01, 0};
        const uint8_t bad_bits[] = {3, 1, 0, 0};
        const uint8_t truncated[] = {4, 2, 0};
        const uint8_t empty[] = {0};

        decoded = NULL;
        consumed = 777;
        assert(mc_spine_kernel_palette_decode_block_states_wire(NULL, 0, &consumed, &decoded)
               == MC_SPINE_KERNEL_NULL);
        assert(mc_spine_kernel_palette_decode_block_states_wire(single, sizeof(single), NULL,
                                                                &decoded) == MC_SPINE_KERNEL_NULL);
        assert(mc_spine_kernel_palette_decode_block_states_wire(single, sizeof(single), &consumed,
                                                                NULL) == MC_SPINE_KERNEL_NULL);
        assert(mc_spine_kernel_palette_decode_block_states_wire(single, SIZE_MAX, &consumed,
                                                                &decoded)
               == MC_SPINE_KERNEL_OUT_OF_RANGE);
        assert(mc_spine_kernel_palette_decode_block_states_wire(empty, 0, &consumed, &decoded)
               == MC_SPINE_KERNEL_INVALID_WIRE);
        assert(mc_spine_kernel_palette_decode_block_states_wire(bad_state, sizeof(bad_state),
                                                                &consumed, &decoded)
               == MC_SPINE_KERNEL_INVALID_STATE);
        assert(mc_spine_kernel_palette_decode_block_states_wire(bad_bits, sizeof(bad_bits),
                                                                &consumed, &decoded)
               == MC_SPINE_KERNEL_INVALID_WIRE);
        assert(mc_spine_kernel_palette_decode_block_states_wire(truncated, sizeof(truncated),
                                                                &consumed, &decoded)
               == MC_SPINE_KERNEL_INVALID_WIRE);
        assert(decoded == NULL && consumed == 777);
        OK(mc_spine_kernel_palette_decode_block_states_wire(single, sizeof(single), &consumed,
                                                            &decoded));
        assert(consumed == sizeof(single));
        OK(mc_spine_kernel_palette_uniform_state(decoded, &bits, copy));
        assert(bits == 1 && copy[0] == 208);
        OK(mc_spine_kernel_palette_free(decoded));
    }
}

static void fill_light(mc_spine_kernel_light_block *cells, size_t count,
                       mc_spine_kernel_light_block fact) {
    size_t i;
    for (i = 0; i < count; i++) cells[i] = fact;
}

static uint8_t nibble(const uint8_t *section, unsigned x, unsigned y, unsigned z) {
    unsigned cell = y << 8 | z << 4 | x;
    return (uint8_t)((section[cell / 2] >> (cell % 2 * 4)) & 0xf);
}

static void light_roundtrip(void) {
    enum { HEIGHT = 32, CELLS = HEIGHT * 256 };
    static mc_spine_kernel_light_block cells[CELLS];
    static uint8_t section[MC_SPINE_KERNEL_LIGHT_SECTION_BYTES];
    const mc_spine_kernel_light_block air = {0, 0, {0, 0, 0, 0, 0, 0}};
    const mc_spine_kernel_light_block torch = {0, 14, {0, 0, 0, 0, 0, 0}};
    mc_spine_kernel_light_block bad = air;
    mc_spine_kernel_light_edit edit;
    mc_spine_kernel_light_engine *engine = NULL;
    mc_spine_kernel_light_update *update = NULL;
    mc_spine_kernel_light_update_summary summary;
    mc_spine_kernel_light_chunk_info chunk;
    mc_spine_kernel_light_section changed;
    size_t i;

    assert(mc_spine_kernel_light_free(NULL) == MC_SPINE_KERNEL_NULL);
    assert(mc_spine_kernel_light_new(0, HEIGHT, 1, NULL) == MC_SPINE_KERNEL_NULL);
    assert(mc_spine_kernel_light_new(1, HEIGHT, 1, &engine) == MC_SPINE_KERNEL_INVALID_ARGUMENT);
    assert(mc_spine_kernel_light_new(0, 0, 1, &engine) == MC_SPINE_KERNEL_INVALID_ARGUMENT);
    assert(mc_spine_kernel_light_new(0, HEIGHT, 2, &engine) == MC_SPINE_KERNEL_INVALID_ARGUMENT);
    assert(engine == NULL);
    OK(mc_spine_kernel_light_new(0, HEIGHT, 1, &engine));

    fill_light(cells, CELLS, air);
    assert(mc_spine_kernel_light_load_chunk(engine, 0, 0, cells, CELLS - 1)
           == MC_SPINE_KERNEL_OUT_OF_RANGE);
    bad.opacity = 16;
    cells[5] = bad;
    assert(mc_spine_kernel_light_load_chunk(engine, 0, 0, cells, CELLS)
           == MC_SPINE_KERNEL_INVALID_ARGUMENT);
    cells[5] = air;
    assert(mc_spine_kernel_light_get_section(engine, 0, 0, MC_SPINE_KERNEL_LIGHT_SKY, 1, section,
                                             sizeof(section)) == MC_SPINE_KERNEL_OUT_OF_RANGE);
    OK(mc_spine_kernel_light_load_chunk(engine, 0, 0, cells, CELLS));
    OK(mc_spine_kernel_light_load_chunk(engine, 1, 0, cells, CELLS));

    memset(section, 0, sizeof(section));
    OK(mc_spine_kernel_light_get_section(engine, 0, 0, MC_SPINE_KERNEL_LIGHT_SKY, 1, section,
                                         sizeof(section)));
    for (i = 0; i < sizeof(section); i++) assert(section[i] == 0xff);
    assert(mc_spine_kernel_light_get_section(engine, 0, 0, 2, 1, section, sizeof(section))
           == MC_SPINE_KERNEL_INVALID_ARGUMENT);
    assert(mc_spine_kernel_light_get_section(engine, 0, 0, MC_SPINE_KERNEL_LIGHT_BLOCK, 4, section,
                                             sizeof(section)) == MC_SPINE_KERNEL_OUT_OF_RANGE);
    assert(mc_spine_kernel_light_get_section(engine, 0, 0, MC_SPINE_KERNEL_LIGHT_BLOCK, 1, section,
                                             sizeof(section) - 1) == MC_SPINE_KERNEL_BUFFER_TOO_SMALL);

    edit.x = 15;
    edit.y = 4;
    edit.z = 4;
    edit.block = torch;
    assert(mc_spine_kernel_light_apply_edit(engine, &edit, 0, &update) == MC_SPINE_KERNEL_INVALID_ARGUMENT);
    edit.y = HEIGHT;
    assert(mc_spine_kernel_light_apply_edit(engine, &edit, 1, &update) == MC_SPINE_KERNEL_OUT_OF_RANGE);
    edit.y = 4;
    edit.x = 100;
    assert(mc_spine_kernel_light_apply_edit(engine, &edit, 1, &update) == MC_SPINE_KERNEL_OUT_OF_RANGE);
    edit.x = 15;
    edit.block.opacity = 16;
    assert(mc_spine_kernel_light_apply_edit(engine, &edit, 1, &update)
           == MC_SPINE_KERNEL_INVALID_ARGUMENT);
    assert(update == NULL);
    edit.block = torch;
    OK(mc_spine_kernel_light_apply_edit(engine, &edit, 1, &update));
    OK(mc_spine_kernel_light_update_info(update, &summary));
    assert(summary.chunk_count == 2 && summary.invalidated_cells > 0);
    OK(mc_spine_kernel_light_update_chunk(update, 0, &chunk));
    assert(chunk.chunk_x == 0 && chunk.chunk_z == 0 && chunk.block_mask == 6
           && chunk.block_count == 2 && chunk.sky_mask == 0 && chunk.empty_block_mask == 0);
    OK(mc_spine_kernel_light_update_section(update, 0, MC_SPINE_KERNEL_LIGHT_BLOCK, 0, &changed));
    assert(changed.section_index == 1 && nibble(changed.data, 15, 4, 4) == 14
           && nibble(changed.data, 12, 4, 4) == 11);
    OK(mc_spine_kernel_light_get_section(engine, 1, 0, MC_SPINE_KERNEL_LIGHT_BLOCK, 1, section,
                                         sizeof(section)));
    assert(nibble(section, 0, 4, 4) == 13);
    assert(mc_spine_kernel_light_update_chunk(update, 2, &chunk) == MC_SPINE_KERNEL_OUT_OF_RANGE);
    assert(mc_spine_kernel_light_update_section(update, 0, MC_SPINE_KERNEL_LIGHT_SKY, 0, &changed)
           == MC_SPINE_KERNEL_OUT_OF_RANGE);
    assert(mc_spine_kernel_light_update_section(update, 0, 7, 0, &changed)
           == MC_SPINE_KERNEL_INVALID_ARGUMENT);
    OK(mc_spine_kernel_light_update_free(update));

    edit.block = air;
    OK(mc_spine_kernel_light_apply_edit(engine, &edit, 1, &update));
    OK(mc_spine_kernel_light_update_chunk(update, 0, &chunk));
    assert(chunk.block_mask == 0 && chunk.empty_block_mask == 6 && chunk.block_count == 0);
    OK(mc_spine_kernel_light_update_free(update));
    assert(mc_spine_kernel_light_update_free(NULL) == MC_SPINE_KERNEL_NULL);
    OK(mc_spine_kernel_light_free(engine));
}

static uint8_t *read_asset(const char *dir, const char *name, size_t *len) {
    char path[512];
    FILE *file;
    uint8_t *data;
    long size;

    assert(snprintf(path, sizeof(path), "%s/%s", dir, name) > 0);
    file = fopen(path, "rb");
    assert(file != NULL);
    assert(fseek(file, 0, SEEK_END) == 0);
    size = ftell(file);
    assert(size > 0 && fseek(file, 0, SEEK_SET) == 0);
    data = (uint8_t *)malloc((size_t)size);
    assert(data != NULL && fread(data, 1, (size_t)size, file) == (size_t)size);
    fclose(file);
    *len = (size_t)size;
    return data;
}

static void registry_roundtrip(const char *assets) {
    size_t states_len, blocks_len, i;
    uint8_t *states_json = read_asset(assets, "block_states_1211.json", &states_len);
    uint8_t *blocks_json = read_asset(assets, "blocks_1211.json", &blocks_len);
    mc_spine_kernel_registry *registry = NULL;
    mc_spine_kernel_block_facts facts;
    mc_spine_kernel_text text;
    uint32_t count = 0, state = 999;

    assert(mc_spine_kernel_registry_free(NULL) == MC_SPINE_KERNEL_NULL);
    assert(mc_spine_kernel_registry_open_json(NULL, 0, blocks_json, blocks_len, &registry)
           == MC_SPINE_KERNEL_NULL);
    assert(mc_spine_kernel_registry_open_json(states_json, states_len, blocks_json, blocks_len, NULL)
           == MC_SPINE_KERNEL_NULL);
    assert(mc_spine_kernel_registry_open_json(UTF8("{}"), blocks_json, blocks_len, &registry)
           == MC_SPINE_KERNEL_INVALID_JSON);
    assert(mc_spine_kernel_registry_open_json(UTF8("not json"), UTF8("[]"), &registry)
           == MC_SPINE_KERNEL_INVALID_JSON);
    assert(registry == NULL);
    /* A truncated asset fails closed instead of exposing a partial table. */
    assert(mc_spine_kernel_registry_open_json(states_json, states_len / 2, blocks_json, blocks_len,
                                              &registry) == MC_SPINE_KERNEL_INVALID_JSON);
    OK(mc_spine_kernel_registry_open_json(states_json, states_len, blocks_json, blocks_len,
                                          &registry));
    free(states_json);
    free(blocks_json);

    OK(mc_spine_kernel_registry_state_count(registry, &count));
    assert(count == MC_SPINE_KERNEL_STATE_COUNT);
    OK(mc_spine_kernel_registry_default_state(registry, UTF8("minecraft:stone"), &state));
    assert(state == 1);
    OK(mc_spine_kernel_registry_state_name(registry, state, &text));
    assert(text.len == 15 && memcmp(text.data, "minecraft:stone", 15) == 0);
    OK(mc_spine_kernel_registry_state_facts(registry, state, &facts));
    assert(facts.opacity == 15 && facts.is_solid_render == 1 && facts.can_occlude == 1
           && facts.blocks_motion == 1 && facts.is_air == 0 && facts.luminance == 0
           && facts.sturdy_faces_mask == 63 && facts.fluid_kind == MC_SPINE_KERNEL_REGISTRY_FLUID_NONE);
    for (i = 0; i < 6; i++) assert(facts.face_occlusion[i] == 0xf);

    OK(mc_spine_kernel_registry_default_state(registry, UTF8("minecraft:lava"), &state));
    assert(state == 96);
    OK(mc_spine_kernel_registry_state_facts(registry, state + 8, &facts));
    assert(facts.luminance == 15 && facts.is_liquid == 1 && facts.fluid_level == 8
           && facts.fluid_kind == MC_SPINE_KERNEL_REGISTRY_FLUID_LAVA && facts.is_waterlogged == 0);
    OK(mc_spine_kernel_registry_state_facts(registry, 11164, &facts));
    assert(facts.is_waterlogged == 1 && facts.fluid_kind == MC_SPINE_KERNEL_REGISTRY_FLUID_WATER);
    assert(facts.face_occlusion[4] == 0xf && facts.face_occlusion[5] == 0);
    OK(mc_spine_kernel_registry_state_facts(registry, 0, &facts));
    assert(facts.is_air == 1 && facts.opacity == 0);

    state = 999;
    assert(mc_spine_kernel_registry_default_state(registry, UTF8("stone"), &state)
           == MC_SPINE_KERNEL_NOT_FOUND);
    assert(mc_spine_kernel_registry_default_state(registry, UTF8("\xff"), &state)
           == MC_SPINE_KERNEL_INVALID_UTF8);
    assert(mc_spine_kernel_registry_state_facts(registry, MC_SPINE_KERNEL_STATE_COUNT, &facts)
           == MC_SPINE_KERNEL_INVALID_STATE);
    assert(mc_spine_kernel_registry_state_name(registry, MC_SPINE_KERNEL_STATE_COUNT, &text)
           == MC_SPINE_KERNEL_INVALID_STATE);
    assert(mc_spine_kernel_registry_state_count(NULL, &count) == MC_SPINE_KERNEL_NULL);
    assert(state == 999);
    OK(mc_spine_kernel_registry_free(registry));
}

int main(int argc, char **argv) {
    assert(mc_spine_kernel_abi_version() == MC_SPINE_KERNEL_ABI_VERSION);
    palette_roundtrip();
    palette_wire_decode();
    light_roundtrip();
    registry_roundtrip(argc > 1 ? argv[1] : "assets");
    nbt_roundtrip();
    dispatcher_roundtrip();
    physics_roundtrip();
    puts("client-kernel C roundtrip passed");
    return 0;
}
