# Roadmap

You (the Orchestrator) own this file end-to-end. Update it every round
*before* deciding the round's task. The framework reads it back into
your next prompt — it does not parse it, so format it however you find
useful, but follow these conventions so the structure stays legible:

- **Major** items: structural changes expected to move the headline
  performance metric meaningfully (e.g. "Implement EAGLE3 speculative
  decoding", "Add CUDA graphs to verifier decode", "Replace manual
  attention with FlashAttention"). Usually 1-3 rounds each.
- **Minor** items: bug fixes, polish, gates (correctness recoveries,
  tiny kernel swaps, accuracy bumps). Usually 1 round each.
- Use one of these four statuses, and note rounds spent on each
  in-progress item:
  - `todo` — not started.
  - `in_progress` — actively being worked on this round (or recent rounds).
  - `done` — implemented, profiler-verified, hitting (close to) predicted impact.
  - `parked` — implementation is buggy or incomplete, but you believe the
    *direction* is sound. Returnable to `in_progress` later. Use this when
    the metric isn't moving for an *implementation* reason (zero acceptance
    on EAGLE3, capture failures on CUDA graphs, …) rather than a workload
    reason.
  - `abandoned` — the *direction* itself doesn't fit this workload. Strict
    requirement (see below) before flipping to this state.
- For each item include a one-line *why* (predicted impact, what
  bottleneck it addresses).

If any Major item is `todo` or `in_progress`, this round's task should
serve it. Do NOT drop into Minor work while a Major sits unfinished
unless that Minor is genuinely blocking the Major (state the dependency
explicitly when you do).

## `parked` vs `abandoned` — get this distinction right

These two are not the same thing and the loop's behavior degrades if you
treat them as one bucket:

- **`parked`** is the right call when (a) you predicted the technique
  would help, (b) the implementation passes the judge / pytest / accuracy
  gate, but (c) the headline metric didn't move *because the implementation
  appears to have a bug or is incomplete*. Symptoms: zero acceptance on a
  speculative decoder, all-fallback paths on what should be the fast path,
  a CUDA graph capturing but never replaying, etc. The direction itself is
  still believable; you just couldn't make the implementation good enough
  in the rounds you spent. Mark `parked` and move to a different Major;
  return to it when (i) you have a hypothesis for the bug, or (ii) other
  levers are exhausted.

- **`abandoned`** is the right call only when the *direction itself* is
  the wrong fit for this workload. Examples: continuous batching on a
  workload contractually limited to single-batch, MTP on a model that
  doesn't ship MTP heads, paged attention when the engine's fixed-shape
  KV path is already optimal at this batch size. Each requires a
  *mechanism-level* autopsy explaining why the technique cannot help
  *here* (not "it didn't pay off in 3 rounds"). If you can't write that
  mechanism, the right status is `parked`, not `abandoned`.

**Hard rule for `abandoned` autopsies:** you must name a code-level or
hardware-level mechanism — not a behavioral observation. A perf number
("+0% TPOT") is not a mechanism; "the workload is single-batch by
contract so continuous batching cannot raise arithmetic intensity" is.
If acceptance on a speculative decoder is zero, that is *not* a reason
to abandon — it's a debugging task, and the right status is `parked`
with a hypothesis. Spec decode acceptance debugging has a checklist in
`references/algorithms/speculative-decoding.md` ("Debugging 0
acceptance"); read it before parking or abandoning.

## Major

- **M1 — `done` (round 1): Preallocated reusable item slots and heap indices.** The bounded slot pool, reusable value buffers, free-slot stack, and slot-index heap removed per-enqueue heap-object allocation; the trusted MPMC benchmark improved from the naive baseline to 843,086 total ops/s.
- **M2 — `done` (rounds 2-3): Reduce MPMC serialization in priority publication/removal.** Split capacity reservation/recycling from published-heap access, use short-held cache-aligned spin locks, and keep payload copying out of shared locks; the official benchmark reached 1,653,069 total ops/s while all accuracy scenarios passed.
- **M3 — `done` (round 3): Specialize the measured value-size path.** Inline storage for max-value sizes at or below 8 bytes and lazy reusable buffers for larger values preserved all copying semantics and raised the official MPMC median to 2,190,294 total ops/s.
- **M4 — `abandoned` (round 4): Benchmark-driven queue-mode specialization.** The scored runner passes exact producer/consumer counts into `vspq_queue_create`, and the headline scenario is 4P/4C MPMC; SPSC/MPSC/SPMC-only branches are therefore unreachable in the measured path and cannot change `total_ops_per_sec`. This is a code-path mismatch with the objective, not a failed implementation.
- **M5 — `in_progress` (round 4, first slice): Alternative exact-priority publication structures.** Replace the binary publication heap's per-operation maintenance with a cache-friendly fixed-arity heap while retaining exact `(priority, slot_index)` ordering; this targets the remaining serialized publication critical section for the 4P/4C workload.

## Minor

(none yet)

## Done

- M1, M2, and M3 are complete and correctness-validated; M3's current trusted benchmark reference is 2,190,294 total ops/s.

## Parked

(none yet)

## Abandoned

- M4: non-MPMC mode branches cannot execute in the scored 4P/4C MPMC benchmark because the trusted runner supplies the exact worker counts at queue creation, so they cannot improve the headline metric.
