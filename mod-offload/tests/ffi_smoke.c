/* SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later */
#include "mc_spine_mod_offload.h"

#include <assert.h>
#include <stdio.h>
#include <string.h>

static uint32_t read32(const uint8_t *p) {
    return (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
}

static void record(const uint8_t *p, uint32_t tick, uint32_t source, uint16_t kind,
                   uint16_t length, uint8_t present, uint32_t id) {
    assert(read32(p) == tick && read32(p + 4) == 0);
    assert(read32(p + 8) == source);
    assert(read32(p + 12) == ((uint32_t)length << 16 | kind));
    assert(p[16] == present && read32(p + 17) == id);
}

int main(void) {
    mso_region *region = NULL;
    assert(mso_region_create(-1, 2, -3, &region) == MSO_OK && region != NULL);
    assert(mso_region_create(0, 0, 0, NULL) == MSO_INVALID_ARGUMENT);

    const mso_node node = {10.0, 2.0, 16.0, 1};
    assert(mso_kinetic_add(region, 1, node) == MSO_OK);
    assert(mso_kinetic_add(region, 2, (mso_node){0.0, 2.0, 0.0, 0}) == MSO_OK);
    assert(mso_kinetic_link(region, 1, 2, 1, -2.0) == MSO_OK);
    mso_solution solution;
    assert(mso_kinetic_get(region, 2, &solution) == MSO_OK);
    assert(solution.status == MSO_KINETIC_RUNNING && solution.speed == -32.0);

    assert(mso_transport_add(region, 10, 1, 1) == MSO_OK);
    assert(mso_transport_insert(region, 10, 77) == MSO_OK);

    assert(mso_fluid_add(region, 20, 100, 0) == MSO_OK);
    assert(mso_fluid_add(region, 21, 100, 0) == MSO_OK);
    assert(mso_fluid_link(region, 20, 21, 1, 8) == MSO_OK);
    uint32_t moved;
    assert(mso_fluid_insert(region, 20, 0, 24, &moved) == MSO_OK && moved == 24);
    assert(mso_fluid_insert(region, 21, 3, 1, &moved) == MSO_CONFLICT);

    const mso_stack in[] = {{MSO_ITEM, 77, 1}, {MSO_FLUID, 0, 1}};
    const mso_stack out[] = {{MSO_ITEM, 88, 1}, {MSO_FLUID, 3, 1}};
    const mso_recipe recipe = {0, 2, in, 2, out, 2};
    const mso_drive drive = {MSO_MECHANICAL, (uint32_t)-solution.speed, 0, 0};
    assert(mso_recipe_add(region, 30, 2, 2, drive, &recipe, 1) == MSO_OK);
    assert(mso_recipe_insert(region, 30, in[0], &moved) == MSO_OK && moved == 1);
    assert(mso_recipe_insert(region, 30, in[1], &moved) == MSO_OK && moved == 1);

    mso_step_report step;
    assert(mso_region_step(region, 1, 4, 0, &step) == MSO_OK);
    assert(step.processed == 0 && step.time_exhausted == 1);
    assert(step.has_backlog == 1 && step.backlog_tick == 0);
    assert(mso_region_step(region, 1, 3, UINT64_MAX, &step) == MSO_OK);
    assert(step.processed == 3 && step.has_backlog == 1 && step.backlog_tick == 1);
    assert(mso_region_step(region, 1, 1, UINT64_MAX, &step) == MSO_OK);
    assert(step.processed == 1 && step.has_backlog == 0);

    uint8_t buffer[129];
    memset(buffer, 0xa5, sizeof buffer);
    mso_batch_report batch;
    assert(mso_region_export(region, buffer, 56, 4, &batch) == MSO_BUFFER_TOO_SMALL);
    assert(batch.required_bytes == 57 && batch.records == 0 && batch.sequence == 0);
    for (size_t i = 0; i < sizeof buffer; ++i) {
        assert(buffer[i] == 0xa5);
    }

    assert(mso_region_export(region, buffer, sizeof buffer, 4, &batch) == MSO_OK);
    assert(batch.bytes == 128 && batch.records == 4 && batch.sequence == 1);
    assert(memcmp(buffer, "S5\x01\x01", 4) == 0);
    assert(read32(buffer + 4) == 4 && read32(buffer + 8) == 1 && read32(buffer + 12) == 0);
    assert(read32(buffer + 16) == (uint32_t)-1 && read32(buffer + 20) == 2);
    assert(read32(buffer + 24) == (uint32_t)-3 && read32(buffer + 28) == 0);
    record(buffer + 32, 0, 20, MSO_S5_FLUID_TANK_UPDATE, 9, 1, 0);
    assert(read32(buffer + 53) == 8);
    record(buffer + 57, 0, 21, MSO_S5_FLUID_TANK_UPDATE, 9, 1, 0);
    assert(read32(buffer + 78) == 16);
    record(buffer + 82, 1, 10, MSO_S5_TRANSPORT_TRANSFER, 9, 0, 0);
    assert(read32(buffer + 103) == 77);
    record(buffer + 107, 1, 30, MSO_S5_RECIPE_EVENT, 5, 1, 0);
    assert(buffer[128] == 0xa5);
    assert(mso_region_export(region, NULL, 0, 4, &batch) == MSO_OK);
    assert(batch.bytes == 0 && batch.records == 0 && batch.sequence == 0);

    mso_item item;
    assert(mso_transport_extract(region, 10, &item) == MSO_OK && item.present && item.item == 77);
    assert(mso_recipe_extract(region, 30, out[0], &moved) == MSO_OK && moved == 1);
    assert(mso_recipe_extract(region, 30, out[1], &moved) == MSO_OK && moved == 1);
    assert(mso_region_free(region) == MSO_OK && mso_region_free(NULL) == MSO_OK);
    puts("C ABI smoke: PASS");
    return 0;
}
