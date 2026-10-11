# prioqueue-mpmc-luna-xhigh-8r-20260806

VibeSys 8-round MPMC priority queue run, gpt-5.6-luna (xhigh effort), 2026-08-06.

VibeSys optimized a bounded priority queue (copying C ABI, Rust seed in
`workspace/src/lib.rs`) for the **MPMC** scenario (4 producers / 4 consumers).
Headline metric: `total_ops_per_sec` from the trusted evaluator (maximize).
Correctness (linearizability, priority order, no fabrication/duplication,
capacity) is a hard gate.

- Agent: Codex CLI, model `gpt-5.6-luna`, reasoning effort: xhigh
- Rounds: 8
- Hardware: single host, see `logs/gpu.json`

## Rounds

| Round | Commit | Correctness | total_ops_per_sec |
| --- | --- | --- | --- |
| 1 | `da43508` (tag `round-1`) | pass | 843,086 |
| 2 | `2e2e247` (tag `round-2`) | pass | 1,653,069 |
| 3 | `626274a` (tag `round-3`) | pass | 2,190,294 |
| 4 | `0e04042` (tag `round-4`) | fail | n/a |
| 5 | `66a016e` (tag `round-5`) | fail | n/a |
| 6 | `84c7b2e` (tag `round-6`) | fail | n/a |
| 7 | `106b51f` (tag `round-7`) | pass | 6,179,290 |
| 8 | `15dd6f5` (tag `round-8`) | fail | n/a |

## Layout

- `workspace/`: candidate workspace. One commit per agent step
  (`round-N-retry-M-{implementer,judge,framework-accuracy,framework-benchmark}`).
  `workspace/progress.md` and `workspace/roadmap.md` hold the agents' notes;
  `workspace/_evaluator/priority-queue/` is the trusted evaluator.
- `logs/run-*.log`: full orchestrator / implementer / judge transcripts.
- `logs/rounds.json`: per-round commit, gate result, and metric.
- `logs/usage.jsonl`: per-agent-call token usage and duration.
