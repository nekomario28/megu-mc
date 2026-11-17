# The population order is a priority discipline, not a sequence

Vanilla 1.21.1, pinned jar `59353fb4…`, mappings `03f89854…`, read with `tools/vanilla_src.sh`
(Vineflower 1.10.1, the build this host has). Raw notes, decompiled sources, probe captures and
the analysis scripts: `/home/yuu/mc-spine-local/audit/population-order-20261118-015835/`.

Answers one question: what decides the next chunk to populate, and can the port reproduce it as a
default order? Short version: the **rule** is readable from the jar and is the same in every
capture; the **sequence** is not a function of the request set, and it is measured to differ between
identical runs. A fixed default order can therefore only be *one member* of vanilla's order family,
not "the" vanilla order - so this lane did not replace the lazy default with a guess.

## 1. What the jar says

The work is driven by ticket levels, queued per level, and popped lowest-level-first.

| step | code | evidence |
|---|---|---|
| a request is a ticket | `forceload` -> `DistanceManager.updateChunkForced` -> `Ticket(FORCED, ChunkMap.c, pos)` with `ChunkMap.c = ChunkLevel.byStatus(FullChunkStatus.ENTITY_TICKING)` = **31** | `aqk.java:187-199`, `aqb.java:61`, `aqa.java:45-53` |
| tickets become levels | propagation is +1 per Chebyshev step | `aqk.java`, `aqa.java:16-33,45-53` |
| level -> required status | `ChunkLevel.getStatusForLevel`: level <= 33 -> `FULL`, then one status further down `ChunkPyramid.GENERATION_STEPS` per level | `aqa.java:16-33,45-53`, `dvy.java:5-16` |
| one FIFO bucket per level | `List<Long2ObjectLinkedOpenHashMap<List<Optional<T>>>>`, one map per level | `aqd.java:16-19` |
| the pop | `b.get(this.c).firstLongKey()`, then `removeFirst()`, then advance `c` past empty levels | `aqd.java:79-99` |
| a task's priority | `ChunkTaskPriorityQueueSorter.a(holder, runnable)` = `a(r, holder.r().a(), holder::j)`, i.e. `ChunkHolder.getQueueLevel` | `aqe.java:45-49`, `apz.java:198` |
| levels can move after submission | `onLevelChange` rewrites the holder's queue level and moves queued task lists between buckets | `aqe.java:71-78`, `aqd.java:29-49` |
| submission is per tick | pending tasks live in `ChunkMap.r`, submitted by `ChunkMap.g()` from `ServerChunkCache.s()` | `aqb.java:529-549`, `aqs.java:207-210`, tick `aqs.java:239-245` |
| a task decorates its window | status walk EMPTY->target; per status, x **outer** / z **inner** over the `2r+1` square, step executed inline on the calling thread | `apy.java:31-52,103-131`, `dwc.java:8-20` |

So the rule is: **lowest queue level first; within a level, the order the chunk position first
entered that level's bucket.** The priority is the propagated ticket level, not the request order
and not a position hash; `Long2ObjectLinkedOpenHashMap` keeps insertion order, and a re-inserted key
keeps its original slot (`aqd.java:46-49`).

The within-level order is the weak half, and it is weak by construction: a bucket entry is created
either by a `ChunkHolder` level change or by a *neighbour's* status request
(`aqn.java:29-53`), and an entry's value is a list of tasks appended when a task reaches a status.
Which of those happens before the next tick's `g()` depends on wall clock, and whether a neighbour
request generates inline or only creates a future depends on whether that future already existed
(`apy.java:117-131`).

## 2. What the recovered orders measure

Seven native decoration orders exist, all with the same instrumentation
(`SculkLog.recordPopOrder`: one deduplicated line per chunk, written and flushed under a static
lock, so the file order is the order the hook bodies took that lock - decoration is submitted through
one worldgen mailbox, so that is the decoration order, not a thread-interleaving artifact):

| artifact | run | seed / dim / stage | request box | n | sha256(16) |
|---|---|---|---|---|---|
| `captures/t6b/nat-nf-writes/poporder.tsv` | t6b | 4242 / nether / features | chunks (-25..-23, 19..21) | 81 | `3ec4ec662e6d727f` |
| `captures/t6b/nat-nf-str/poporder.tsv` | t6b | 4242 / nether / structures | same box | 81 | `c11148a347561863` |
| `captures/t6b/nat-4242-nf-hooked/poporder.tsv` | t6b | 4242 / nether / features | same box | 81 | `481710bdf0140a4a` |
| `…/population-order-20261118-015835/probe-captures/p1/poporder.tsv` | this lane | 4242 / nether / features | chunk (-25,19) | 49 | `71bb789cb2657ecd` |
| `…/p2/poporder.tsv` | this lane | identical to p1 | same | 49 | `baa8a81509e7dff0` |
| `…/s1/poporder.tsv` | this lane, `-XX:ActiveProcessorCount=1` | identical to p1 | same | 49 | `22aa96f0dbf0639e` |
| `…/s2/poporder.tsv` | this lane, `-XX:ActiveProcessorCount=1` | identical to p1 | same | 49 | `28036f0361914614` |

Three things hold in **all seven**:

* the decorated set is the request box **+ 3 rings** (1 chunk -> 7x7 = 49; 3x3 -> 9x9 = 81). The
  forced chunk is level 31 and the outermost decorated ring is level 34, so the step that runs
  `applyBiomeDecoration` (`FEATURES`) answers to level 34 in this build - three levels down from
  the ticket; nothing beyond it decorates, because a chunk whose own target *is* that status has a
  demand radius of 0 for it (`dwc.java:8-11`);
* the **first** decoration block is the x-then-z 3x3 at the request box's (min x, min z) corner;
* the decorated chunk's own level is *not* monotone - the sequence is a staircase of demander
  blocks with local drops of 1-2 levels, because the demander's own cell is decorated in the middle
  of its scan (level-run sequences `[1,0,1,2,3]` for n=1, 24-29 runs over `{0..3}` for n=3).

The first-block property also holds on the older, `run.log`-derived corpus: of the 25 `order.tsv`
files under `~/mc-spine-local`, **24 begin with exactly that x-then-z 3x3 at the window's min
corner** (16 of them are a 1-chunk request, where the whole 9-line file is that block). The one
exception, `scratch/current/mineshaft-wg-height-20261113/native-fixture` (overworld, seed 4242), runs
`(2,-2) (2,-1) (2,-3) ...` - a `z` sequence that is not sorted - which is the console-interleaving
hazard `tools/sculk_trace.py` documents for log-derived traces, so it is not usable as a program-order
artifact and is not evidence against the rule either. Log output proves less than `poporder.tsv`
here; the seven artifacts in the table above are the ones with a dedicated flush-ordered writer.

And one thing does **not** hold: no two runs agree.

| pair | identical | common prefix |
|---|---|---|
| p1 vs p2 - identical settings, back to back | no | 9 / 49 |
| s1 vs s2 - identical settings, one processor each | no | 9 / 49 |
| t6b `nat-nf-writes` vs `nat-4242-nf-hooked` - identical settings | no | 9 / 81 |

The common prefix is always the deterministic part (the first demander's block). Reducing the JVM
to one processor does not fix it: the order is built from the per-tick submission of pending tasks
racing the worldgen worker, not from a thread pool's queue.

The repository already carries the same phenomenon from a different angle:
`tests/data/overworld_chunk_0_0_1211.provenance.json` records that two runs of that input differed
in **4 cells** (diorite vs andesite at 14,24,0 / 15,24,0 / 14,25,0 / 15,25,0), and the corpus is
compared against one *named* run for that reason.

## 3. Two artifacts are not observations

`scratch/current/snowy-4242-20261113/order-A.txt` and `order-B.txt` (9 chunks each) were
**composed** from two observed ore overwrite relations to express "which pair order yields Native A
/ Native B"; the chunk-(3,-2) page says as much
(`order-A` / `order-B` は既知の ore 上書き関係から構成した順序). They are constraint witnesses, not
recovered schedules. The Native A / B captures behind them
(`captures/native/mc-native-ow-feat-a|b-20261111`) are pre-instrumentation runs with no request log
and no decoration trace.

## 4. Why "0 diffs against both A and B by default" is unreachable

Native A and Native B differ in **23 cells** - `minecraft:diorite` in A, `minecraft:andesite` in B,
y 42-46 - which is exactly the 23 `andesite->diorite` cells of the attribution table, i.e. the
`(3,-2)` vs `(2,-3)` pair order, one bit. A single fixed default order matches that bit for at most
one of the two, so `max(|port-A|, |port-B|) >= 23` on those cells for every possible default. The
two 0/98,304 rows the chunk page records are 0 *against the order that run used*, supplied through
`MC_SPINE_POP_ORDER`.

The same applies to the gate shape "the default order equals each recovered native order artifact":
the seven artifacts are mutually inconsistent, so no sequence can equal all of them.

## 5. What is missing, and what the next lane needs to decide

Missing: a deterministic within-level order. It is not in the jar to be read - it is an event order
that vanilla does not fix. To turn this into a default order one of these has to be chosen
explicitly, and each is a decision, not an inference:

1. **Keep parity against a recorded order** (today's route). `MC_SPINE_POP_ORDER` stays the
   acceptance input; the default stays the lazy scan and stays labelled as not-vanilla. This is what
   the existing corpus already does for chunk (0,0) seed 0.
2. **Define the default as a named member of the family** - level-major queue over the request set
   with a stated tie-break (e.g. the serialized demand scan: first request = the box's min corner,
   each demander's window in x-then-z, FIFO per level). It is implementable and testable against the
   three invariances above, but it will not equal any capture bit-for-bit, and the ore residuals of
   the 23-cell pair remain until a capture is made *at* that order (which a native run cannot be
   asked to do).
3. **Re-scope the issue**: make the request set and the order two separate inputs, and treat
   "順序も入力" as a statement about the corpus (each reference row carries the order it was captured
   under), not about a default. The port can express a box (`MC_SPINE_POP_REGION`, a lexicographic
   region scan) or a complete explicit list (`MC_SPINE_POP_ORDER`), but not a ticket set, so this
   option also needs a request-set parameter before the rule can be applied to one.

## What this page does not claim

- It does not claim the port cannot be made to match a capture: `MC_SPINE_POP_ORDER` already
  produces 0/98,304 against both A and B, each with its own order.
- It does not publish the internal accumulated-offset table of `ChunkStep`: only the ring count (3)
  and the first-block corner are measured, on seven artifacts. A hand re-implementation of
  `dwc`/`dvx` disagreed with the measured ring count and was discarded rather than published.
- It does not claim the two-processor/tick interaction is the only source of variation: four runs
  and two processor counts were measured, not the whole space.
- No suite, mutation or layers gate was run for this page: it changes no code.
