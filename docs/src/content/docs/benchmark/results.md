---
title: Results
description: Benchmark highlights for DuckAgent's guarded recoverable context policy.
draft: false
---

The most useful current report is:

```text
benchmark/results/guarded-mid-vs-balanced-combined/report.md
```

It runs the guarded-mid policy against balanced and guarded-late variants on a combined suite:

- `1000` long-run user turns;
- validation scale `2`;
- `1188/1188` completed simulated turns for every listed guarded-mid row;
- threshold `0.8`, target `0.3`;
- seven model pricing profiles.

## Guarded-mid versus balanced

| Model profile | Guarded-mid result | Versus balanced |
| --- | --- | --- |
| `openai-gpt-5.4` | `USD 1199.144484`, `96.6%` cache hit, `1188/1188` turns | `22.6%` lower simulated cost and `23.7%` fewer expected tokens. |
| `openai-gpt-5.5` | `USD 2398.288967`, `96.6%` cache hit, `1188/1188` turns | `22.6%` lower simulated cost and `23.7%` fewer expected tokens. |
| `openai-gpt-5.4-mini` | `USD 195.798758`, `87.5%` cache hit, `1188/1188` turns | `8.1%` lower simulated cost and `30.8%` fewer expected tokens. |
| `kimi-k2.6` | `USD 302.727538`, `87.3%` cache hit, `1188/1188` turns | `14.5%` lower simulated cost and `30.6%` fewer expected tokens. |
| `deepseek-chat` | `USD 74.952810`, `91.3%` cache hit, `1188/1188` turns | Same as balanced because the `128K` prompt profile falls back to the `80%` guard. |
| `deepseek-v4-flash` | `USD 29.190571`, `97.0%` cache hit, `1188/1188` turns | `16.9%` lower simulated cost and `21.3%` fewer expected tokens. |
| `deepseek-v4-pro-promo` | `USD 71.593043`, `97.0%` cache hit, `1188/1188` turns | `15.6%` lower simulated cost and `21.3%` fewer expected tokens. |

Across the guarded-mid rows, the benchmark reports roughly **108M raw tool tokens saved** by structured tool projection while keeping recovery handles available.

## User-facing takeaway

DuckAgent does not blindly summarize every tool result as soon as it appears. It keeps the active loop raw until prompt pressure appears, then switches to recoverable summaries with exact-evidence budgets and handles. In the current combined report, that is the best-looking default because it keeps all simulated turns complete while reducing long-context cost on large-window and mid-window models.

## Caveats

These numbers come from an offline simulator. They are useful for policy selection and regression checks, not exact billing promises. Pricing profiles live under `benchmark/pricing/` and should be reviewed whenever provider prices or model limits change.
