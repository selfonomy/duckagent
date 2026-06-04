---
title: Benchmark
description: Context-policy benchmark results for DuckAgent's long-running agent loop.
draft: false
---

DuckAgent includes an offline context-policy benchmark under `benchmark/`. It exists to answer one practical question:

> How should a direct local agent keep enough evidence for long work without paying to resend every raw tool result forever?

The benchmark simulates long-running Agent Loops, tool output, prompt-cache reuse, model limits, pricing profiles, and expected recovery reads when compressed evidence needs to be revisited. It does not call an LLM and it does not mutate real session files.

## Current runtime policy

The current runtime uses the **guarded recoverable context** policy.

| Runtime behavior | Value |
| --- | --- |
| Active policy in source | `ContextProjectionPolicy::guarded_mid` |
| Benchmark family | `recoverable_decay_guarded_mid` |
| Prompt principle | [Cache-Friendly Prompt](/benchmark/cache-friendly-prompt/) |
| Active loop pressure | `80%` below a `200K` prompt window, `85%` at `200K+` |
| Active exact evidence budget | `18K` tokens |
| Completed-loop exact evidence budget | `2K` tokens |
| Tool preview budget | `220` tokens |
| Recovery model | Keep path, offset, limit, cursor, process id, and query handles so exact details can be fetched again. |

In generated benchmark reports and benchmark source this policy appears as `duckagent_recoverable_decay_guarded_mid`.

## Why it matters

The benchmarked policy keeps the active Agent Loop rich while it is still safe to do so. Once prompt pressure appears, it projects tool output into a smaller model-visible shape:

- `read_file` keeps exact evidence while budget remains, then falls back to path and range handles.
- process output keeps status, cursor, and preview fields so logs can be resumed.
- completed loops become recoverable summaries instead of raw tool transcripts.
- the session and prompt design remain append-only and cache-friendly; projection happens before model send.

That gives the model a small prompt, stable recovery handles, and enough recent evidence to avoid the worst read-compact-read loop.

## Reports

| Report | Use it for |
| --- | --- |
| `benchmark/results/guarded-mid-vs-balanced-combined/report.md` | Best current summary for the selected guarded-mid policy versus balanced and guarded-late variants. |
| `benchmark/results/matrix/report.md` | Wider policy and model matrix across long-run workloads. |
| `benchmark/results/recoverable-sweep/report.md` | Budget sweep around recoverable-decay policy variants. |
| `benchmark/context_policy_benchmark.py` | Simulator and policy definitions. |

Start with [Current Context Policy](/benchmark/context-projection/) if you want to understand the runtime behavior, read [Cache-Friendly Prompt](/benchmark/cache-friendly-prompt/) for the prompt-cache principle, then read [Results](/benchmark/results/) for the numbers worth quoting.
