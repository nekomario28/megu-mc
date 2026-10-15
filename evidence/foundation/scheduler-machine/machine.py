"""The life of a scheduled cell, written from the implementation.

Read from src/main.rs:
  - sched_ready()      true when there is no backoff entry, or its deadline has passed
  - backoff_ticks()    10 server ticks, doubling per consecutive visit that changed nothing
  - sched_invalidate() removes the entry for the cell and its six neighbours, and promotes the
                       queue entry so a cell already waiting is visited next tick instead of
                       keeping its old slot
  - requeue_fluid()    re-schedules with the delay taken from the same backoff map

The question this answers is liveness: can a cell get stuck in a state from which no accepting
state is reachable, i.e. is it ever visited but neither removed nor allowed to become due again?
0 errors means no such soft-lock.

It does NOT answer whether the `sched_ready` test alongside `is_warm` can fire. Reachability
only says the ladder eventually reaches a due deadline; it says nothing about the delay queue,
which clamps its wait at MAX_DELAY (4 s). Because `backoff_ticks` keeps doubling past 80 ticks,
a cell at `fails >= 5` has a real deadline beyond the queue's cap and the queue would return it
early, so `sched_ready` is false and the guard is load-bearing. See README.md and
tick_tests::a_backoff_past_the_queue_cap_waits_for_its_real_deadline.
"""

import flowgx as f

machine = f.Machine(
    states=[
        "unregistered",  # not a fluid cell
        "queued",        # waiting for its due tick
        "visiting",      # being looked at this invocation
        "backing_off",   # was visited, changed nothing, waiting out its doubled delay
        "removed",       # no longer this kind of cell; nothing more will be scheduled
    ],
    start="unregistered",
    accept=["removed"],
    transitions=[
        ("unregistered", "queued", "the block becomes a fluid"),
        # Both the queue's own due list and sched_ready gate this; either way it is visited.
        ("queued", "visiting", "due and warm"),
        # A visit that changed something re-schedules with the ladder reset.
        ("visiting", "queued", "changed something"),
        # A visit that changed nothing goes up one rung.
        ("visiting", "backing_off", "changed nothing"),
        # The deadline is reached, so sched_ready says yes again.
        ("backing_off", "queued", "deadline passed"),
        # The block stops being a fluid. Every state can reach this, which is the liveness claim.
        ("queued", "removed", "the block is no longer a fluid"),
        ("visiting", "removed", "the block is no longer a fluid"),
        ("backing_off", "removed", "the block is no longer a fluid"),
    ],
)
