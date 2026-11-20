<!-- SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later -->

# S5 Generic Offload Kernels Prototype

Phase E Wave 4, lane E4-2 implements the four simulation and scheduling kernels from the [shim plan](shim-plan.md#将来拡張汎用オフロードカーネル重量-mod-向けネイティブ加速s3-以降) as the standalone [mod-offload crate](../../mod-offload/Cargo.toml). The implementation is clean-room Rust with no dependencies. The crate is offered under LGPL-3.0-or-later, GPL-3.0-or-later, or AGPL-3.0-or-later. The repository includes the [LGPL](../../COPYING.LESSER), [GPL](../../LICENSE), and [AGPL](../../COPYING.AGPL) texts.

The prototype covers primitives that Create-, AE2-, and Mekanism-class adapters can use. Content registration, channel routing, fluid and power allocation, recipes, and live mod/runtime integration belong to separate work.

## Kernel contracts

| Kernel | Contract | Work on idle and active paths |
| --- | --- | --- |
| [Kinetic graph](../../mod-offload/src/kinetic_graph.rs) | Undirected adjacency with signed, reciprocal speed ratios. Drive speeds must agree after ratio normalization. Each connected component sums capacity and load in adapter-supplied common stress units. An overloaded driven component stops until an edit restores capacity. Contradictory ratios or unrepresentable intermediate results report a conflict. | Edits traverse only affected connected components. Insertion of a link merges and solves its component. Edge or node removal partitions and solves the remaining pieces. A settled tick visits zero nodes. |
| [Sleeping transport](../../mod-offload/src/sleeping_transport.rs) | Input and output FIFOs carry registry item IDs. Each timed step passes one item to the linked downstream input or to the local output. A prototype link has one upstream and one downstream per machine. Empty input or full output makes the machine dormant immediately. Inventory and link changes reconcile the affected machines. | Each eligible machine has one timer. The queue advances by due tick and machine ID. Catch-up stops at the step budget and retains the first pending tick. Delays are positive. An overflowing wake tick leaves the item in its input and reports `ClockExhausted`. |
| [Contraption](../../mod-offload/src/contraption.rs) | Sparse signed local coordinates index a palette of exact block-state IDs. One validated rigid pose moves all occupied cells. The cluster AABB encloses the actual transformed unit boxes. Collision uses the cached cluster AABB followed by oriented unit-box versus AABB tests. Touching counts as contact. | Updating a pose visits the occupied cells to refresh the cluster bounds and emits one pose update. Shape replacement emits only changed cells keyed by local position. Queries that miss the cluster bounds skip the cells. The query tests a single pose using full unit blocks. |
| [Region scheduler](../../mod-offload/src/region_scheduler.rs) | A region is keyed by dimension and a 32×32 horizontal block area, equivalent to 2×2 chunks. Each region owns its kernel state, logical clock and batch queue. | A step quota bounds scheduled transport work per call. An optional elapsed-time limit is checked between atomic transfers. Full output postpones the next transfer until output is drained. Kinetic graph solves and contraption edits are synchronous ingress operations that the host must budget separately. |

## Delta batch ABI v1

The frame follows the [S3 DirectBatchBridge](../../compat/neoforge-events/README.md#nf-4-direct-tick-batches) pattern of a bounded, little-endian frame in a reusable buffer with a record count and a nonzero sequence. S5 has its own magic and wire layout. A host adapter can collect the prototype frames for JNI delivery. Kinetic and contraption adapters can queue their own delta payloads using record kinds at or above `0x0100`.

| Header offset | Bytes | Field |
| --- | --- | --- |
| 0 | 4 | `S`, `5`, version `1`, direction `1` (outbound) |
| 4 | 4 | Record count, unsigned |
| 8 | 8 | Sequence, unsigned, starting at 1 independently per region |
| 16 | 12 | Dimension, region X and region Z as signed 32-bit integers |
| 28 | 4 | Reserved, zero |

Each record contains `tick:u64`, `source:u32`, `kind:u16`, `payload_len:u16`, and exactly `payload_len` bytes. Kind `1` carries a transport transfer. Its payload is a destination-present byte, a destination machine ID (`u32`, zero if absent), and an item ID (`u32`). An absent destination means the item entered the source's own output. External inserts and extracts are initiated by the caller.

A region queues at most 4,096 records and 256 KiB of encoded data including the frame header. An export writes the FIFO prefix that fits both caller limits. A buffer that cannot fit the first record leaves the queue intact. The returned byte length marks the end of the frame. Empty exports leave the buffer and sequence untouched.

## Reproduction

From the repository root, use a local temporary directory.

```sh
mkdir -p target/tmp
export TMPDIR="$PWD/target/tmp"
(cd mod-offload && cargo test --offline && cargo run --release --offline --bin offload_bench)
bash tools/layers_check.sh
bash tools/debt_ratchet.sh
python3 tools/provenance_check.py
```

The benchmark uses 10,000 dormant machines (half empty and half blocked) and 2,000 settled kinetic nodes across 20 networks. The full-scan baseline checks every machine's readiness and each kinetic node's dirty flag. Both paths assert zero transfers or dirty nodes. Setup and warm-up are outside timing. The release binary reports wall time and, where Linux thread schedstat is available, CPU time per tick. The measured workload contains no live JVM, active factory throughput, topology solve timing, or world tick overhead.
