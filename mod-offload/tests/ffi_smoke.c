/* SPDX-License-Identifier: LGPL-3.0-or-later */
#include "megu_mod_offload.h"

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

static void contraption_smoke(void) {
    mso_contraption *body = NULL;
    const mso_block_diff blocks[] = {{{0, 0, 0}, 0, 1}, {{3, 0, 0}, 7, 1}};
    assert(mso_contraption_create(-2, 3, -4, 42, blocks, 2, NULL) == MSO_INVALID_ARGUMENT);
    assert(mso_contraption_create(-2, 3, -4, 42, NULL, 1, &body) == MSO_INVALID_ARGUMENT);
    assert(mso_contraption_create(-2, 3, -4, 42, blocks, SIZE_MAX, &body) == MSO_INVALID_ARGUMENT);
    assert(body == NULL);
    assert(mso_contraption_create(-2, 3, -4, 42, blocks, 2, &body) == MSO_OK && body);

    mso_actor actor = {.bounds = {{1.25, 0, 0}, {1.75, 1, 1}}};
    mso_actor_collision contact;
    assert(mso_contraption_collide_actor(body, actor, &contact) == MSO_OK && !contact.intersects);
    const mso_block_diff invalid[] = {{{0, 0, 0}, 0, 0}, {{1, 0, 0}, 1, 2}};
    assert(mso_contraption_apply_diffs(body, 4, invalid, 2) == MSO_INVALID_ARGUMENT);
    actor.bounds = (mso_aabb){{0.25, 0.25, 0.25}, {0.75, 0.75, 0.75}};
    assert(mso_contraption_collide_actor(body, actor, &contact) == MSO_OK && contact.intersects);
    const mso_block_diff edits[] = {
        {{3, 0, 0}, 7, 0}, {{0, 0, 0}, 0, 1}, {{-2, 0, 0}, UINT32_MAX, 1}
    };
    assert(mso_contraption_apply_diffs(body, 4, edits, 3) == MSO_OK);
    actor.bounds = (mso_aabb){{3.25, 0.25, 0.25}, {3.75, 0.75, 0.75}};
    assert(mso_contraption_collide_actor(body, actor, &contact) == MSO_OK && !contact.intersects);

    const mso_contraption_pose pose = {{10, 2, -4}, 90, 0, {1, 2, 3}, {4, 5, 6}};
    assert(mso_contraption_set_pose(NULL, 5, pose) == MSO_INVALID_ARGUMENT);
    assert(mso_contraption_set_pose(body, 3, pose) == MSO_TIME_WENT_BACKWARDS);
    assert(mso_contraption_set_pose(body, 5, pose) == MSO_OK);
    actor = (mso_actor){.bounds = {{10.25, 2.25, -4.75}, {10.75, 2.75, -4.25}},
                        .previous_anchor = {0.5, 0.5, 0.5}, .carried = 1};
    assert(mso_contraption_collide_actor(body, actor, &contact) == MSO_OK && contact.intersects);
    assert(contact.delta[0] == 10 && contact.delta[1] == 2 && contact.delta[2] == -5);
    actor.bounds.max[0] = 0;
    assert(mso_contraption_collide_actor(body, actor, &contact) == MSO_INVALID_ARGUMENT);
    assert(contact.intersects && contact.delta[0] == 10);

    uint8_t buffer[155];
    memset(buffer, 0xa5, sizeof buffer);
    mso_batch_report batch;
    assert(mso_contraption_export_delta(body, buffer, SIZE_MAX, 2, &batch) == MSO_INVALID_ARGUMENT);
    assert(mso_contraption_export_delta(body, buffer, 81, 2, &batch) == MSO_BUFFER_TOO_SMALL);
    assert(batch.required_bytes == 82 && batch.records == 0 && batch.sequence == 0);
    for (size_t i = 0; i < sizeof buffer; ++i) {
        assert(buffer[i] == 0xa5);
    }
    assert(mso_contraption_export_delta(body, buffer, sizeof buffer, 2, &batch) == MSO_OK);
    assert(batch.bytes == 154 && batch.records == 2 && batch.sequence == 1 && buffer[154] == 0xa5);
    assert(memcmp(buffer, "S5\x01\x01", 4) == 0);
    assert(read32(buffer + 4) == 2 && read32(buffer + 8) == 1 && read32(buffer + 12) == 0);
    assert(read32(buffer + 16) == (uint32_t)-2 && read32(buffer + 20) == 3);
    assert(read32(buffer + 24) == (uint32_t)-4 && read32(buffer + 28) == 0);
    assert(read32(buffer + 32) == 4 && read32(buffer + 40) == 42);
    assert(read32(buffer + 44) == (34u << 16 | MSO_S5_CONTRAPTION_BLOCK_DIFFS));
    assert(read32(buffer + 48) == (uint32_t)-2 && buffer[60] == 1 && read32(buffer + 61) == UINT32_MAX);
    assert(read32(buffer + 65) == 3 && buffer[77] == 0 && read32(buffer + 78) == 0);
    assert(read32(buffer + 82) == 5 && read32(buffer + 90) == 42);
    assert(read32(buffer + 94) == (56u << 16 | MSO_S5_CONTRAPTION_POSE));
    assert(read32(buffer + 122) == 0x42b40000 && read32(buffer + 150) == 0x40c00000);
    assert(mso_contraption_export_delta(body, NULL, 0, 2, &batch) == MSO_OK && batch.bytes == 0);
    assert(mso_contraption_free(body) == MSO_OK && mso_contraption_free(NULL) == MSO_OK);
}

int main(void) {
    contraption_smoke();
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
