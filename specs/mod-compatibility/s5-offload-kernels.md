<!-- SPDX-License-Identifier: LGPL-3.0-or-later -->

# S5 Generic Offload Kernels Prototype

Phase E Wave 4, lane E4-2 implements the four simulation and scheduling kernels from the [shim plan](shim-plan.md#将来拡張汎用オフロードカーネル重量-mod-向けネイティブ加速s3-以降) as the standalone [mod-offload crate](../../mod-offload/Cargo.toml). The implementation is clean-room Rust with no dependencies. The crate is offered under LGPL-3.0-or-later, GPL-3.0-or-later, or AGPL-3.0-or-later. The repository includes the [LGPL](../../COPYING.LESSER), [GPL](../../LICENSE), and [AGPL](../../COPYING.AGPL) texts.

Lane E4-7 adds fluid pressure equilibrium, recipe processing, and a C ABI to the standalone crate. These kernels give Create-, AE2-, and Mekanism-class adapters primitives for their own content, channel routing, power rules, and JVM integration.

Lane E4-19 adds the contraption C ABI and tests independent scheduler instances concurrently under output backpressure.

## Kernel contracts

| Kernel | Contract | Work on idle and active paths |
| --- | --- | --- |
| [Kinetic graph](../../mod-offload/src/kinetic_graph.rs) | Undirected adjacency with signed, reciprocal speed ratios. Drive speeds must agree after ratio normalization. Each connected component sums capacity and load in adapter-supplied common stress units. An overloaded driven component stops until an edit restores capacity. Contradictory ratios or unrepresentable intermediate results report a conflict. | Edits traverse only affected connected components. Insertion of a link merges and solves its component. Edge or node removal partitions and solves the remaining pieces. A settled tick visits zero nodes. |
| [Sleeping transport](../../mod-offload/src/sleeping_transport.rs) | Input and output FIFOs carry registry item IDs. Each timed step passes one item to the linked downstream input or to the local output. A prototype link has one upstream and one downstream per machine. Empty input or full output makes the machine dormant immediately. Inventory and link changes reconcile the affected machines. | Each eligible machine has one timer. The queue advances by due tick and machine ID. Catch-up stops at the step budget and retains the first pending tick. Delays are positive. An overflowing wake tick leaves the item in its input and reports `ClockExhausted`. |
| [Contraption](../../mod-offload/src/contraption.rs) | Sparse signed local coordinates index a palette of exact block-state IDs. One validated rigid pose moves all occupied cells. The cluster AABB encloses the actual transformed unit boxes. Collision uses the cached cluster AABB followed by oriented unit-box versus AABB tests. Touching counts as contact. | Updating a pose visits the occupied cells to refresh the cluster bounds and emits one pose update. Shape replacement emits only changed cells keyed by local position. Queries that miss the cluster bounds skip the cells. The query tests a single pose using full unit blocks. |
| [Fluid network](../../mod-offload/src/fluid_network.rs) | Connected tanks conserve integer volume and carry one fluid ID. The fluid surface balances against tank elevation, capacity, and cumulative signed pump head. A positive link head raises fill on the destination side. Pressure head and elevation use the same integer units as fill. Equal-pressure rounding gives remaining units to eligible tanks in ascending ID order. A link that would mix fluid IDs or introduce a nonzero sum of pump heads around a cycle is rejected atomically. | A tank, inventory, or link edit schedules affected components only. Removing a link wakes both resulting components. Each solve settles one component. A settled network has an empty work queue. The model handles static pressure equilibrium in closed components. |
| [Recipe processor](../../mod-offload/src/recipe_processor.rs) | A recipe atomically consumes and produces multiple item/fluid stacks. Inputs accept resource IDs used by the machine's recipes. The first eligible recipe in registration order starts when all inputs and output space are available. Input and output capacities count total stack amounts on each side. Filtered distribution sends a selected resource to matching destination inputs in caller order, retaining unaccepted output. | Work accrues at the supplied RPM, with base ticks defined at 16 RPM and completion rounded up to a whole tick. Overstress, zero RPM, and exhausted fuel pause progress. A furnace runs at the reference rate for the supplied fuel ticks. Input, output extraction, and drive changes reconcile only the affected machines. A running machine has one deadline for completion or fuel exhaustion. |
| [Region scheduler](../../mod-offload/src/region_scheduler.rs) | A region is keyed by dimension and a 32×32 horizontal block area. Each region owns its kernels, logical clock, and batch queue. Fluid edits run at the current tick. Transport wakes precede recipe wakes at equal ticks. | A quota counts transfers, recipe deadlines, fluid solves, and additional tank records from a solve. The soft time limit is checked between these units. A fluid solve is atomic and the region holds its remaining tank records through backpressure. The host budgets synchronous topology validation, kinetic solves, and contraption edits separately. |

## Delta batch ABI v1

The frame follows the [S3 DirectBatchBridge](../../compat/neoforge-events/README.md#nf-4-direct-tick-batches) pattern of a bounded, little-endian frame in a reusable buffer with a record count and a nonzero sequence. S5 has its own magic and wire layout. A host adapter can collect the prototype frames for JNI delivery. Kinds at or above `0x0100` carry adapter payloads. The contraption exporter uses two kinds in that range.

| Header offset | Bytes | Field |
| --- | --- | --- |
| 0 | 4 | `S`, `5`, version `1`, direction `1` (outbound) |
| 4 | 4 | Record count, unsigned |
| 8 | 8 | Sequence, unsigned, starting at 1 independently per handle |
| 16 | 12 | Dimension, region X and region Z as signed 32-bit integers |
| 28 | 4 | Reserved, zero |

Each record contains `tick:u64`, `source:u32`, `kind:u16`, `payload_len:u16`, and exactly `payload_len` bytes. A missing ID is encoded as zero alongside a zero presence flag. ID zero is valid when the flag is one.

| Kind | Source | Payload |
| --- | --- | --- |
| `1` Transport transfer | Sending machine | Destination-present `u8`, destination `u32`, item `u32`. An absent destination indicates the sender's local output. |
| `2` Fluid tank update | Changed tank | Fluid-present `u8`, fluid `u32`, amount `u32` after the solve. |
| `3` Recipe event | Recipe machine | Recipe-present `u8`, completed recipe ID `u32`. An absent recipe indicates fuel ran out before completion. |
| `0x0101` Contraption pose | Contraption ID | Position X/Y/Z (`f64` each), yaw and pitch (`f32` each), linear velocity X/Y/Z and angular velocity X/Y/Z (`f32` each). Total 56 bytes. |
| `0x0102` Contraption block diffs | Contraption ID | One or more 17-byte entries in lexicographic local X/Y/Z order. Each entry has X/Y/Z (`i32` each), present (`u8`), and palette state ID (`u32`). Zero presence deletes the cell and encodes a zero state. The payload length divided by 17 gives the entry count. |

The caller owns ingress and extraction. The fluid records report redistribution by the solver. The recipe ID identifies the registered inputs and outputs that completed atomically.

Each region or contraption handle queues at most 4,096 records and 256 KiB of encoded data including the frame header. An export writes the FIFO prefix that fits both caller limits. A buffer that cannot fit the first record leaves the queue intact. The returned byte length marks the end of the frame. Empty exports leave the buffer and sequence untouched.

## C ABI

The [C11 header](../../mod-offload/include/megu_mod_offload.h) declares the `mso_*` functions exported by the crate's `cdylib`. Serialize calls on a live region handle. Distinct handles are independent. The host configures kinetics, transport, tanks, and recipes through the region, supplies each processor's drive, then steps to an absolute tick with a work quota and a soft microsecond budget. The step report gives the first due tick if work remains. A full queue requires exporting records before resuming.

Export encodes S5 directly into caller storage, which may be uninitialized. There is no intermediate frame buffer. Recipe definitions are copied into region ownership at registration. Invalid arguments, incompatible fluids and heads, and undersized buffers have explicit statuses. An undersized buffer reports the required capacity for the first record. Rust panics that unwind are caught at the ABI entry points.

A contraption handle starts at tick zero with a copied seed cluster, identity pose, and empty export queue. The host supplies the region key and contraption ID at creation. Receivers need the seed shape before consuming its stream. An edit array upserts or removes occupied local cells atomically. State ID zero is valid, and duplicate positions invalidate the array. The kernel rebuilds its local palette and collision bounds for a changed shape. Only effective edits enter the stream, with up to 3,855 entries per record. Pose updates enqueue one record when the pose or velocity metadata changes. Ticks may repeat and must be nondecreasing. If the queue is full, the host can export and retry the rejected update at the same tick.

The pose uses blocks for position, and applies right-handed pitch about local X followed by yaw about world Y, in degrees about the local origin. Linear and angular velocities are world-space metadata in blocks and degrees per tick. The host sets the absolute transform. A collision query tests the actor's current world-space AABB against occupied cells at that pose. A host-designated passenger also gets the rigid displacement of its previous world-space anchor between the last two accepted poses. Repeating a pose sample resets that displacement to zero. The host owns attachment and collision response.

The isolation test runs five independent scheduler instances on five threads. Four peers each complete 1,024 updates with their own inventory outputs, tick values, exact S5 records, and sequence numbers while the fifth region's 4,096-record queue stays saturated. The blocked region can drain its own FIFO and then resume the pending transfer.

## Reproduction

From the repository root, use a local temporary directory.

```sh
mkdir -p target/tmp
export TMPDIR="$PWD/target/tmp"
cargo fmt --manifest-path mod-offload/Cargo.toml -- --check &&
cargo clippy --manifest-path mod-offload/Cargo.toml --offline --locked --all-targets -- -D warnings &&
cargo test --manifest-path mod-offload/Cargo.toml --offline --locked &&
cargo build --manifest-path mod-offload/Cargo.toml --release --offline --locked --lib &&
gcc -std=c11 -Wall -Wextra -Werror -Imod-offload/include mod-offload/tests/ffi_smoke.c \
  -Lmod-offload/target/release -Wl,-rpath,"$PWD/mod-offload/target/release" \
  -lmod_offload -o "$TMPDIR/ffi_smoke" &&
"$TMPDIR/ffi_smoke" &&
g++ -fsyntax-only -Wall -Wextra -Werror -x c++ mod-offload/include/megu_mod_offload.h &&
cargo run --manifest-path mod-offload/Cargo.toml --release --offline --locked --bin offload_bench &&
bash tools/layers_check.sh && bash tools/debt_ratchet.sh &&
python3 tools/provenance_check.py && git diff --check
```

The settled benchmark uses 10,000 dormant machines (half empty and half blocked) and 2,000 settled kinetic nodes across 20 networks. Both paths assert zero transfers or dirty nodes. The mixed benchmark runs 5,000 dormant chains and 500 active chains over 250 measured two-tick cycles. Each cycle verifies 500 belt transfers, 1,000 fluid solves, 500 recipe completions, and 2,000 exported S5 records. Timings include host inventory moves, periodic active kinetic speed edits, region steps, and S5 export.

The contraption benchmark moves a sparse 256-block cluster over 2,000 measured ticks after 1,000 warm-up ticks. Each tick updates translation and yaw, computes one carried passenger delta, and checks eight actor AABBs (four hits, three gaps inside the cluster's broadphase bounds, and one broadphase miss). The timer covers the Rust kernel, including pose construction and bounds refresh. Setup, warm-up, C ABI calls, and S5 export for this workload were outside the timing. Live JVM and mod integration were outside the run.

Measured on Linux, AMD Ryzen 7 9800X3D, Rust 1.98.0. The first three rows retain the E4-7 measurements. The contraption row is from the E4-19 release run.

| Release workload | Wall per tick | Thread CPU per tick |
| --- | ---: | ---: |
| Settled full scan | 2,717.34 ns | 2,708.75 ns |
| Settled event-driven | 6.00 ns | 5.97 ns |
| Mixed active and dormant | 396.66 µs | 393.64 µs |
| Contraption pose, passenger delta, and actor collision | 11.50 µs | 10.74 µs |
