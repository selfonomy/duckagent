---
title: Current Context Policy
description: How DuckAgent maps the runtime context projection code to the benchmarked guarded recoverable policy.
draft: false
---

DuckAgent projects session history before every main model request. This keeps the prompt cache-friendly: the stable system prompt stays fixed, while dynamic context and recoverable history are appended or projected after that stable prefix.

The runtime entry is:

```text
src/agent.rs -> projected_main_model_messages()
src/context_projection.rs -> project_main_history()
src/context_projection.rs -> ContextProjectionPolicy::guarded_mid(...)
```

This maps to the benchmark strategy named **recoverable decay guarded mid**.

## Policy shape

| Part | Runtime behavior |
| --- | --- |
| Current Agent Loop | Stays raw while the projected prompt is below the active-loop pressure threshold. |
| Pressure threshold | `80%` of prompt window for models below `200K`; `85%` for `200K+` prompt windows. |
| Current-loop projection | When pressure appears, tool results are compacted into `[TOOL RESULT SUMMARY]` blocks. |
| Completed-loop projection | Completed tool loops are projected into user messages plus one `[COMPLETED AGENT LOOP SUMMARY]` before model send. |
| Exact evidence budget | `18K` tokens for the active loop, `2K` tokens for completed loops. |
| Preview budget | `220` tokens for generic tool previews. |
| Recovery handles | File path, requested offset/limit, next offset, process id, cursor, mode, and query are preserved when available. |

## Tool result projection

`read_file` gets the richest treatment. If the file content fits inside the remaining exact-evidence budget, the projected prompt keeps exact lines. If it does not fit, the prompt keeps a compact preview plus recovery instructions:

```text
content_recovery: call read_file with the same path and a narrower offset/limit for exact lines.
```

Process output keeps process identifiers, status, cursor, truncation state, a preview, and a recovery rule:

```text
output_recovery: call process_read with process_id plus cursor/mode/search query for exact log chunks.
```

Other tools keep a short preview. The goal is not to hide evidence. The goal is to keep the model prompt small and cache-friendly while making exact recovery cheap and explicit.

## Benchmark match

| Evidence | Match |
| --- | --- |
| Source constructor | `ContextProjectionPolicy::guarded_mid` |
| Source threshold constants | `SMALL_CONTEXT_ACTIVE_PRESSURE = 0.80`, `LARGE_CONTEXT_ACTIVE_PRESSURE = 0.85`, `LARGE_CONTEXT_THRESHOLD = 200_000` |
| Source budgets | `ACTIVE_EXACT_EVIDENCE_BUDGET_TOKENS = 18_000`, `COMPLETED_EXACT_EVIDENCE_BUDGET_TOKENS = 2_000` |
| Benchmark source policy | `duckagent_recoverable_decay_guarded_mid` |
| Generated report label | `duckagent_recoverable_decay_guarded_mid` |

The generated report label is the user-facing benchmark name. The thresholds and budgets align with the runtime implementation.

## What the benchmark does not prove

The benchmark is an offline simulator. It compares policy shapes and model-pricing sensitivity, but it is not a live bill and it does not run a provider tokenizer or a real summarizer for every request. Use it to compare directionally meaningful tradeoffs:

- prompt-cache behavior;
- projected prompt size;
- recovery tokens and extra recovery tools;
- tool-output compression savings;
- model limit pressure across pricing profiles.
