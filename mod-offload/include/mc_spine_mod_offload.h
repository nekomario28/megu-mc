/* SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later */
#ifndef MC_SPINE_MOD_OFFLOAD_H
#define MC_SPINE_MOD_OFFLOAD_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct mso_region mso_region;
typedef int32_t mso_status;

enum {
    MSO_OK = 0,
    MSO_INVALID_ARGUMENT = 1,
    MSO_NOT_FOUND = 2,
    MSO_ALREADY_EXISTS = 3,
    MSO_CONFLICT = 4,
    MSO_FULL = 5,
    MSO_TIME_WENT_BACKWARDS = 6,
    MSO_BUFFER_TOO_SMALL = 7,
    MSO_SEQUENCE_EXHAUSTED = 8,
    MSO_PANIC = 9
};

enum {
    MSO_KINETIC_UNPOWERED = 0,
    MSO_KINETIC_RUNNING = 1,
    MSO_KINETIC_OVERSTRESSED = 2,
    MSO_KINETIC_CONFLICT = 3
};
enum { MSO_ITEM = 0, MSO_FLUID = 1 };
enum { MSO_MECHANICAL = 0, MSO_FURNACE = 1 };
enum {
    MSO_S5_TRANSPORT_TRANSFER = 1,
    MSO_S5_FLUID_TANK_UPDATE = 2,
    MSO_S5_RECIPE_EVENT = 3,
    MSO_S5_FIRST_ADAPTER_KIND = 0x100
};

typedef struct {
    double capacity, load, speed;
    uint8_t driven;
} mso_node;

typedef struct {
    double capacity, load, speed;
    uint32_t component, status;
} mso_solution;

typedef struct {
    uint32_t item;
    uint8_t present;
} mso_item;

typedef struct {
    uint32_t fluid, amount;
    uint8_t present;
} mso_tank;

typedef struct {
    uint32_t kind, rpm;
    uint64_t fuel_ticks;
    uint8_t overstressed;
} mso_drive;

typedef struct {
    uint32_t kind, id, amount;
} mso_stack;

typedef struct {
    uint32_t id, base_ticks;
    const mso_stack *inputs;
    size_t input_count;
    const mso_stack *outputs;
    size_t output_count;
} mso_recipe;

typedef struct {
    size_t processed;
    uint64_t backlog_tick;
    uint8_t has_backlog, output_blocked, time_exhausted;
} mso_step_report;

typedef struct {
    size_t bytes, records, required_bytes;
    uint64_t sequence;
} mso_batch_report;

/*
 * Serialize calls on each live handle and free it once. Distinct handles are
 * independent. A null handle is accepted by free. Arrays may be null for a
 * zero count. Other pointers must refer to suitably aligned storage valid for
 * the entire call. Input memory must remain initialized and unchanged, and
 * writable outputs and the handle may not overlap any other arguments.
 * Flags accept exactly 0 or 1. Outputs are written on success, except export
 * also reports the required capacity on MSO_BUFFER_TOO_SMALL. After MSO_PANIC,
 * release the handle. Rust panics that unwind are contained at the boundary.
 */
mso_status mso_region_create(int32_t dimension, int32_t x, int32_t z, mso_region **out);
mso_status mso_region_free(mso_region *region);

/*
 * Step to an absolute tick with a step quota and a soft time budget. Pass
 * UINT64_MAX for unlimited microseconds. Zero allows no work of that budget
 * type. Retry a backlog after granting budget or draining output.
 */
mso_status mso_region_step(mso_region *region, uint64_t target, size_t step_limit,
                           uint64_t budget_us, mso_step_report *out);

/*
 * Write the S5 v1 little-endian frame directly to caller-owned storage. The
 * buffer may be uninitialized, or null with capacity zero. On a short buffer
 * nothing is written to it and the first record stays queued. Only the
 * returned byte prefix is written. Sequence is zero for an empty export.
 */
mso_status mso_region_export(mso_region *region, uint8_t *buffer, size_t capacity,
                             size_t record_limit, mso_batch_report *out);
mso_status mso_region_queue_delta(mso_region *region, uint16_t kind, uint32_t source,
                                  const uint8_t *payload, size_t length);

/* Kinetic edits solve synchronously and return before region stepping. */
mso_status mso_kinetic_add(mso_region *region, uint32_t id, mso_node node);
mso_status mso_kinetic_update(mso_region *region, uint32_t id, mso_node node);
mso_status mso_kinetic_remove(mso_region *region, uint32_t id);
mso_status mso_kinetic_link(mso_region *region, uint32_t a, uint32_t b, uint8_t connected, double ratio);
mso_status mso_kinetic_get(mso_region *region, uint32_t id, mso_solution *out);

mso_status mso_transport_add(mso_region *region, uint32_t id, uint32_t capacity, uint64_t delay);
mso_status mso_transport_link(mso_region *region, uint32_t id, uint8_t connected, uint32_t destination);
mso_status mso_transport_insert(mso_region *region, uint32_t id, uint32_t item);
mso_status mso_transport_extract(mso_region *region, uint32_t id, mso_item *out);

/* A positive head favors fill on side b. A connected edge may be updated. */
mso_status mso_fluid_add(mso_region *region, uint32_t id, uint32_t capacity, int32_t elevation);
mso_status mso_fluid_set_tank(mso_region *region, uint32_t id, uint32_t capacity, int32_t elevation);
mso_status mso_fluid_link(mso_region *region, uint32_t a, uint32_t b, uint8_t connected, int32_t head);
mso_status mso_fluid_insert(mso_region *region, uint32_t id, uint32_t fluid, uint32_t amount, uint32_t *out);
mso_status mso_fluid_extract(mso_region *region, uint32_t id, uint32_t amount, uint32_t *out);
mso_status mso_fluid_get(mso_region *region, uint32_t id, mso_tank *out);

/*
 * Recipe descriptors and stack arrays are copied on registration. Drives are
 * supplied by the host, which can query the kinetic graph for speed/stress.
 * The transfer APIs return the accepted count. Distribution splits the
 * specified resource among matching inputs in the given destination order.
 */
mso_status mso_recipe_add(mso_region *region, uint32_t id, uint32_t input_capacity,
                          uint32_t output_capacity, mso_drive drive,
                          const mso_recipe *recipes, size_t count);
mso_status mso_recipe_set_drive(mso_region *region, uint32_t id, mso_drive drive);
mso_status mso_recipe_insert(mso_region *region, uint32_t id, mso_stack stack, uint32_t *out);
mso_status mso_recipe_extract(mso_region *region, uint32_t id, mso_stack stack, uint32_t *out);
mso_status mso_recipe_distribute(mso_region *region, uint32_t id, mso_stack stack,
                                 const uint32_t *destinations, size_t count, uint32_t *out);

#ifdef __cplusplus
}
#endif

#endif
