# Context Policy Benchmark

This directory contains an offline benchmark for duckagent context projection
strategies. It is designed to answer a specific architecture question:

> In a single direct agent, is it cheaper and safer to summarize every tool
> result immediately, or keep raw output until threshold pressure forces tool
> projection and LLM history compaction?

The benchmark does not call an LLM and does not mutate sessions. It simulates
Agent Loop workloads, prompt-cache reuse, model pricing, and the expected cost
of a later user turn that needs raw details that were not preserved. There is no
delegated execution agent in this benchmark.

## Compression Terms

This benchmark is a long-context, multi-tool code-agent stress test. It is not a
normal chat-cost estimate: plain chat should cost far less because it usually has
fewer model requests per user turn, much smaller prompts, little or no tool
output, and much lower compaction pressure. Use these reports primarily to
compare cache behavior, context compaction, tool-output projection, recovery
cost, and model pricing sensitivity.

The benchmark treats any non-raw tool/history representation as compression.
It separates compression into distinct families:

| Term | Meaning | Cache impact |
|---|---|---|
| Tool structured compression | A capability returns a model-visible `exact_excerpt`, `summary`, and/or `handle` instead of the full raw output. The raw bytes remain recoverable through a file path, process id, cursor, or query. | Does not rewrite old history because the compressed view is what enters context first. |
| LLM compaction | A current loop or old contiguous history range is rewritten into a bounded summary by a dedicated compaction request. One request can compact many old loops/snapshots into one grouped history snapshot. | Breaks the prompt prefix once when the projected history changes, then becomes stable again. |

The aggregate table therefore has separate columns:

- `tool struct comp`: count of tool results whose visible form is smaller than raw.
- `tool saved toks`: raw tool tokens minus visible tool-result tokens.
- `turns complete/requested`: completed user turns versus the generated workload size. Matrix reports keep failure diagnostics out of the main table; `results.json` still includes raw failure and limit fields.
- `base LLM reqs`: ordinary model requests in the generated Agent Loops before
  probabilistic recovery is applied.
- `expected LLM reqs`: `base LLM reqs + expected extra recovery tool
  continuations`. Use this, not base requests alone, when comparing strategies
  that may need to re-read omitted raw evidence.
- `LLM comp reqs`: extra compaction-model requests that rewrite old history.
- `LLM comp items`: old loops/snapshots/current-loop chunks included in those requests.
- `LLM comp cost`: separate input/output cost of those compaction-model requests.
- `recovery toks`: expected extra tokens for re-reading/searching raw evidence
  that was compressed out of context before the current final answer or a later
  follow-up needs it.

Recovery cost is evaluated after the pre-send projection for that user turn. If
a policy decays old exact evidence immediately before sending the request, a
follow-up dependency on that evidence is counted as a recovery read/query. This
matches the actual model-visible prompt rather than the previous turn's
pre-compaction state.

## Run

```bash
python3 benchmark/context_policy_benchmark.py
python3 benchmark/context_policy_benchmark.py --format json
```

Run with another pricing profile:

```bash
python3 benchmark/context_policy_benchmark.py \
  --pricing benchmark/pricing/deepseek-chat.json
```

Search models.dev and generate a profile from its machine-readable cost/limit
catalog:

```bash
python3 benchmark/models_dev_profile.py --search kimi-k2.6
python3 benchmark/models_dev_profile.py \
  --provider openrouter \
  --model moonshotai/kimi-k2.6
```

Use a subset of policies:

```bash
python3 benchmark/context_policy_benchmark.py \
  --policies immediate_summary,evidence_budget
```

Run long-running stress workloads:

```bash
python3 benchmark/context_policy_benchmark.py --stress-only
python3 benchmark/context_policy_benchmark.py --include-stress --stress-scale 2
python3 benchmark/context_policy_benchmark.py \
  --long-run-turns 1000 \
  --context-threshold-ratio 0.8 \
  --context-target-ratio 0.3 \
  --output-dir benchmark/results/single
python3 benchmark/model_matrix.py \
  --long-run-turns 1000 \
  --context-threshold-ratio 0.8 \
  --context-target-ratio 0.3 \
  --output-dir benchmark/results/matrix
python3 benchmark/model_matrix.py \
  --suite validation \
  --validation-scale 2 \
  --context-threshold-ratio 0.8 \
  --context-target-ratio 0.3 \
  --output-dir benchmark/results/validation
python3 benchmark/recoverable_policy_sweep.py \
  --long-run-turns 1000 \
  --validation-scale 2 \
  --context-threshold-ratio 0.8 \
  --context-target-ratio 0.3 \
  --output-dir benchmark/results/recoverable-sweep
```

When `--output-dir` is provided, the command writes:

- `report.md`: Markdown report for docs, issues, or chat.
- `report.html`: self-contained HTML table report for browser viewing.
- `results.json`: structured metrics for later analysis.

`recoverable_policy_sweep.py` is a focused tuning runner around
`duckagent_recoverable_decay_balanced`. It sweeps current-loop pressure exact
budgets, completed-history exact budgets, and threshold-decay exact budgets,
then labels each candidate as:

- `dominates`: lower total cost, no more recovery tokens, and no more extra
  recovery tools than balanced.
- `safe_cost_win`: at least 0.5% lower cost while recovery tokens and extra
  recovery tools stay within 2% of balanced.
- `cost_only`: lower total cost, but recovery or extra tools got worse.
- `loses`: no total-cost win over balanced.

Use the sweep before changing runtime defaults. It keeps long-run stress and
validation holdout rows separate so a candidate that only wins one generated
shape does not look universally better than it is.

## Policies

| Policy | Meaning |
|---|---|
| `immediate_summary` | Every tool result enters context as a compact summary plus a recovery handle from its first appearance. This maximizes prompt-prefix stability because raw tool output is never written into history, but later exact-detail questions may require a recovery read/query. |
| `loop_boundary_summary` | Tool results stay raw inside the active Agent Loop, so follow-up tool requests in the same loop can compare exact files/logs. When the loop finishes, the stored history is rewritten to tool summary+handle plus final assistant content for the next user turn. |
| `loop_boundary_budgeted` | Tool results stay raw inside the active Agent Loop while the prompt is comfortably below the threshold. Under current-loop pressure, raw outputs are reduced to a per-loop exact-evidence budget before the request is sent; completed history is still stored as summary+handle. |
| `loop_boundary_evidence_summary` | Hybrid evidence-boundary strategy. Tool results stay raw inside the active Agent Loop; under current-loop pressure, working evidence is reduced to a current-loop exact-excerpt budget while logs/search results become summary+handle. When the loop finishes, stored history keeps only a smaller exact-evidence budget plus summaries, handles, and final assistant content. |
| `duckagent_recoverable_boundary` | Candidate duckagent runtime strategy. The active Agent Loop stays raw by default. Under current-loop pressure, `read_file` keeps exact excerpts within a budget and falls back to path/offset/limit/next_offset handles; process/search results keep summary plus cursor/query handles. Completed-loop history uses the same recoverable projection with a smaller exact-evidence budget before LLM history compaction. |
| `duckagent_recoverable_decay` | Candidate duckagent runtime strategy with age decay. Current loop and completed loop handling match `duckagent_recoverable_boundary`, but when old history creates prompt pressure, older exact file evidence is further downgraded to recoverable handles before LLM history compaction. This tests whether exact evidence should live only in recent completed loops. |
| `duckagent_recoverable_decay_lean` | Lean duckagent recoverable-decay variant. The active Agent Loop still starts raw, but completed-loop history stores read_file evidence as recoverable handles without inline exact excerpts. This tests whether assistant final content plus handles are enough for most older turns. |
| `duckagent_recoverable_decay_balanced` | Balanced duckagent recoverable-decay variant. It uses a smaller current-loop pressure exact budget than the baseline and keeps only a small completed-history exact-evidence budget before old-history decay. This explores the middle ground between lean handles-only history and the fixed 6K history budget. |
| `duckagent_recoverable_decay_tight` | Tighter recoverable-decay variant derived from balanced. It keeps the same mechanics but lowers current-loop exact evidence to 15K tokens and completed-history exact evidence to 1.5K tokens. |
| `duckagent_recoverable_decay_tight_90` | Tight recoverable-decay variant with a 90% active-loop pressure guard. It tests whether delaying current-loop projection beyond balanced's 80% guard captures most late-current savings without forcing small-context models into repeated hard-limit compaction. |
| `duckagent_recoverable_decay_tight_95` | Tight recoverable-decay variant with a 95% active-loop pressure guard. It sits closer to late-current behavior while still leaving a small guard band before the hard prompt limit. |
| `duckagent_recoverable_decay_guarded_mid` | Prompt-window guarded recoverable-decay variant. Models below a 200K prompt window use balanced's 80% active-loop pressure point; larger-window models use an 85% active-loop pressure point while keeping balanced's exact-evidence budgets. |
| `duckagent_recoverable_decay_guarded_mid_naive_recovery` | Control variant for oscillation tests. Projection matches `duckagent_recoverable_decay_guarded_mid`, but missing raw/exact evidence is recovered by repeatedly re-reading the full raw file when that result would be compacted again. This models the bad read-compact-read loop. |
| `duckagent_recoverable_decay_guarded_late` | Prompt-window guarded recoverable-decay variant. Models below a 200K prompt window use balanced's 80% active-loop pressure point; larger-window models delay current-loop pressure to 90% while keeping balanced's 18K current and 2K completed-history exact-evidence budgets. |
| `duckagent_recoverable_decay_guarded_late_tight` | More aggressive guarded variant. It uses the same 200K prompt-window guard and 90% active-loop pressure point, but trims exact-evidence budgets to 15K current and 1.5K completed history. |
| `duckagent_recoverable_decay_late_current` | Balanced recoverable-decay history with a later active-loop pressure point. Completed history keeps the balanced 2K recoverable exact budget, while the current Agent Loop can remain raw until the hard prompt limit. |
| `duckagent_recoverable_decay_late_tight` | Hybrid of tight budgets and late active-loop pressure. It keeps current tools raw longer, then falls back to recoverable projection with 15K current exact evidence and 1.5K completed-history exact evidence. |
| `duckagent_summary_history_recoverable_current` | Loop-boundary-style compact history with recoverable active-loop pressure. Completed loops store summary+handle, but current-loop pressure keeps recoverable exact file evidence instead of dropping directly to summary. |
| `duckagent_recoverable_decay_adaptive` | Adaptive duckagent recoverable-decay variant. It starts from `duckagent_recoverable_decay_balanced`, but caps current-loop and completed-history exact-evidence budgets by a ratio of the model prompt limit. Large-context models keep the balanced caps, while smaller-context models carry less inline exact evidence before falling back to recoverable handles. |
| `duckagent_recoverable_decay_adaptive_guarded` | Guarded adaptive variant. It uses model-relative budget caps but adds semantic floors, so small-context models do not collapse exact evidence too aggressively before falling back to recoverable handles. |
| `duckagent_recoverable_decay_recent2` | Recent-window duckagent recoverable-decay variant. It matches `duckagent_recoverable_decay`, but protects the two most recent completed loops from old-history decay and grouped LLM compaction unless the hard model limit requires it. This tests whether keeping one extra recent exact-evidence window reduces recovery reads enough to justify the larger prompt. |
| `duckagent_recoverable_decay_soft` | Soft-decay duckagent variant. It keeps current-loop behavior from `duckagent_recoverable_decay`, but old history under pressure may retain a small exact-evidence budget instead of decaying all older read_file evidence to handles. This tests whether a tiny old-history exact cache buys enough recovery reduction. |
| `duckagent_recoverable_decay_relative` | Model-relative duckagent recoverable-decay variant. Current-loop and completed-loop exact-evidence budgets are derived from the model prompt limit instead of fixed token counts, so small-context models carry less inline evidence while large-context models behave close to the fixed-budget baseline. |
| `adaptive_first` | Every tool result is projected immediately by capability semantics: exact excerpts for working code evidence, summary+handle for logs/search/reference outputs. This spends more context than `immediate_summary` to reduce expected recovery reads. |
| `raw_snapshot` | Tool results stay raw while the prompt fits. When projected context reaches the compression threshold, the oldest completed loops are rewritten by LLM compaction. This preserves maximum evidence early, but raw history makes requests larger and compaction cost higher. |
| `pressure_summary` | Tool results enter as raw output until prompt pressure appears. Before LLM history compaction, older raw tool results are downgraded to summary+handle. This delays compression but still removes old raw evidence before rewriting larger history ranges. |
| `pressure_adaptive` | Tool results enter as raw output until prompt pressure appears. Older raw results are then re-projected with the capability policy, keeping exact excerpts for working evidence and summaries for recoverable outputs before LLM compaction is used. |
| `evidence_budget` | Tool results enter as raw output until prompt pressure appears. Older raw results are then projected with a per-loop exact-evidence budget, so only the most useful working evidence stays exact while the rest becomes summary+handle. |
| `early_windowed_prune` | Source-informed early windowed pruning: tool caps/persistence are applied before history growth, then old tool outputs are pruned and middle history is LLM-compacted around 50% context. This keeps max prompt size low but rewrites history more often, which can reduce prompt-cache hit rate. |
| `late_guarded_truncation` | Source-informed late guarded truncation: live tool results are capped when they enter context, but history is allowed to grow until a high preflight guard near 90% context. It compacts less eagerly, yet each request can carry a larger prompt. |

Every policy has mandatory LLM compaction before request send. If old per-loop
snapshots accumulate, the benchmark groups them into bounded topic/history
snapshots. If the current Agent Loop itself is too wide, current-loop tool
outputs are progressively projected to summary, handle, marker, and finally a
current-loop LLM summary. A valid strategy should therefore complete long runs
without prompt-limit failures.

LLM compaction strategy:

- The trigger is `context_threshold_ratio * prompt_limit`, for example 80%.
- The target is `context_target_ratio * prompt_limit`, for example 30%.
- The simulator first estimates the projected request tokens before sending.
- It compacts the oldest completed loops first, keeping
  `keep_recent_loops_on_snapshot` recent loops untouched unless the hard model
  limit requires more.
- Each selected loop has an estimated compact output size from
  `compaction_output_tokens_per_loop`.
- If many old per-loop snapshots accumulate, they are merged into one grouped
  topic/history snapshot.
- If current-loop tool output is too wide to send, current-loop evidence is
  progressively reduced before the request is counted.

The source-informed policies are approximate simulations derived from local
source-tree analysis:

- Hermes: `agent/context_compressor.py`, `tools/tool_result_storage.py`, and
  `tools/budget_config.py` show 50% compression threshold, 20% target tail
  budget, old tool-result pruning, per-result persistence at 100K chars, 200K
  per-turn tool budget, 1.5K preview, and pinned `read_file` behavior.
- `tool-result-truncation.ts`, `tool-result-context-guard.ts`,
  `preemptive-compaction.ts`, and `system-prompt-cache-boundary.ts` show a
  16K-char live tool-result cap, context guard near 90%, reserve-aware
  preflight compaction/truncation routing, and stable/dynamic system prompt
  cache boundaries.

The benchmark models the token-level shape of those systems. It does not run
their real summarizers, provider-specific cache-control metadata, exact
tokenizers, or session-file rewriting logic. Their neutral policy names are
intentional: these are not direct benchmarks of those projects.

## Pricing Profiles

Pricing is model-specific and changes over time, so profiles live in JSON files
under `benchmark/pricing/`.

Included profiles:

| File | Source | Input | Cached input | Output | Context/output limit | Notes |
|---|---|---:|---:|---:|---:|---|
| `default.json` / `openai-gpt-5.4.json` | https://openai.com/api/pricing/ + https://models.dev/api.json | $2.50 / MTok | $0.25 / MTok | $15.00 / MTok | 1.05M / 128K | OpenAI standard pricing checked 2026-05-10. |
| `openai-gpt-5.5.json` | https://openai.com/api/pricing/ + https://models.dev/api.json | $5.00 / MTok | $0.50 / MTok | $30.00 / MTok | 1.05M / 128K | OpenAI standard pricing checked 2026-05-10. |
| `openai-gpt-5.4-mini.json` | https://openai.com/api/pricing/ + https://models.dev/api.json | $0.75 / MTok | $0.075 / MTok | $4.50 / MTok | 400K / 128K | OpenAI standard pricing checked 2026-05-10. |
| `anthropic-claude-sonnet-4.json` | https://docs.anthropic.com/en/docs/about-claude/pricing + https://models.dev/api.json | $3.00 / MTok | $0.30 / MTok | $15.00 / MTok | 200K / 64K | This uses Anthropic cache-hit/read price. Anthropic cache writes have separate prices and are not modeled yet. |
| `kimi-k2.6.json` | https://platform.moonshot.ai/ + https://models.dev/api.json | $0.95 / MTok | $0.16 / MTok | $4.00 / MTok | 262K / 262K | Kimi platform K2.6 rates checked 2026-05-10. |
| `deepseek-v4-flash.json` | https://chat-deep.ai/pricing/ -> https://api-docs.deepseek.com + https://models.dev/api.json | $0.14 / MTok cache miss | $0.0028 / MTok cache hit | $0.28 / MTok | 1M / 384K | V4 Flash rates; verify DeepSeek official pricing before budgeting. |
| `deepseek-v4-pro-promo.json` | https://chat-deep.ai/pricing/ -> https://api-docs.deepseek.com + https://models.dev/api.json | $0.435 / MTok cache miss | $0.003625 / MTok cache hit | $0.87 / MTok | 1M / 384K | V4 Pro promotional rates reported through 2026-05-31 15:59 UTC; verify before budgeting. |
| `deepseek-v4-pro-regular.json` | https://chat-deep.ai/pricing/ -> https://api-docs.deepseek.com + https://models.dev/api.json | $1.74 / MTok cache miss | $0.0145 / MTok cache hit | $3.48 / MTok | 1M / 384K | V4 Pro regular listed rates; verify before budgeting. |
| `deepseek-chat.json` | https://api-docs.deepseek.com/quick_start/pricing-details-usd | $0.27 / MTok cache miss | $0.07 / MTok cache hit | $1.10 / MTok | 128K / 8K | Conservative official legacy profile from DeepSeek docs. |

The current simulator models OpenAI/DeepSeek-style cache hits directly. For
Anthropic, the profile uses cache-read pricing for `cached_input_per_million`
but does not yet add 5-minute or 1-hour cache-write premiums. That is good
enough for strategy shape comparison, but not final Anthropic billing.

## Model Limits

Profiles can include:

```json
{
  "limits": {
    "context_tokens": 1050000,
    "input_tokens": 922000,
    "output_tokens": 128000
  }
}
```

`input_tokens` is optional. When present, it is the prompt input cap used for
`max_context_tokens`; otherwise `context_tokens` is used. `output_tokens` is
checked against each simulated model output. Limit violations add a failure
penalty and force `answerability_score` to `0.0`, because that request would not
fit the selected model.

## Workload Model

The fixture in `workloads/default.json` models realistic user scenarios:

- `plain_chat`: no tools; compaction overhead should not help.
- `code_compare_two_files`: user first inspects `1.js`, then compares with
  `2.js`; exact code excerpts matter.
- `fix_test_loop`: inspect/edit/test loop; code evidence and test summaries
  matter in different ways.
- `long_log_debug`: huge process output; search refs and process handles should
  avoid raw log pollution.
- `large_mcp_or_web_result`: large external payload; summary plus source handle
  is usually enough.
- `incremental_workspace_audit`: a longer multi-loop session that puts pressure
  on threshold policies after several apparently-safe turns.

`--include-stress` appends deterministic generated workloads for long-running
agentic coding sessions:

- `stress_long_code_development`: 24+ user turns with repeated long file reads,
  reference searches, and test logs.
- `stress_many_tools_per_loop`: wide Agent Loops with 28 file/search tool calls
  in a single turn.
- `stress_process_log_marathon`: 48+ turns of process log debugging.
- `stress_fullstack_refactor`: many long frontend/backend files plus repeated
  test runs.

Use `--stress-scale N` to multiply loop counts. This is intentionally harsher
than the default fixture; it exposes whether a strategy survives continuous user
input and large current-loop tool transcripts.

`--include-validation` / `--validation-only` add deterministic holdout workloads
that are not used to tune the main 1000-turn stream:

- `validation_chat_mixed_work`: chat-heavy work with occasional small file
  reads.
- `validation_exact_revisit_work`: repeated exact-code revisits to older source
  reads.
- `validation_log_recovery_work`: log-heavy debugging where handles and query
  recovery should carry the work.
- `validation_wide_current_loop_work`: very wide current Agent Loops with many
  tool results before the final answer.
- `validation_recovery_oscillation_work`: huge-file revisit workload that
  exposes read-compact-read loops when recovery re-reads full raw files instead
  of using range/query handles and pinned exact excerpts.

Use these validation workloads to catch obvious overfitting. They are still
synthetic, so passing them does not prove real-world quality, but a candidate
that only wins the main long-run stream and loses badly here is suspicious.

Each loop specifies:

```json
{
  "user_tokens": 90,
  "assistant_final_tokens": 360,
  "tool_calls": [
    {
      "capability": "read_file",
      "outputs": {
        "raw": 9200,
        "exact_excerpt": 1850,
        "summary": 260,
        "handle": 80
      },
      "preserve": "working_evidence",
      "recovery_tokens": {
        "exact_evidence": 1900,
        "local_detail": 1200
      }
    }
  ],
  "dependency_probabilities": {
    "none": 0.25,
    "global_conclusion": 0.2,
    "exact_evidence": 0.4,
    "local_detail": 0.15
  },
  "raw_dependency_events": [
    {
      "label": "User asks to compare against an earlier file read.",
      "source_loop_offset": 1,
      "dependency": "exact_evidence",
      "probability": 0.7
    }
  ]
}
```

`dependency_probabilities` is the expected follow-up need produced by this loop.
`final_dependency_probabilities` is the expected evidence need for the loop's
own final answer after tool calls have returned. This captures the difference
between immediate tool summarization and keeping raw evidence inside the active
Agent Loop.
`raw_dependency_events` is the explicit current-turn need for raw evidence from
an earlier loop. For example, "use the implementation from 1.js" in the second
turn can point back to the first turn with `source_loop_offset: 1`. If that raw
evidence is no longer retained, the benchmark adds expected recovery tokens,
cost, latency, and extra tool calls.

## User Follow-Up Dependency

The benchmark explicitly models the probability that the next user turn needs
details from the previous raw output:

| Dependency | Example | Strategy requirement |
|---|---|---|
| `none` | User switches topic. | No raw needed. |
| `global_conclusion` | "Did the tests pass?" | Summary is enough. |
| `continuation_handle` | "Keep watching that process." | Needs `process_id`, cursor, or handle. |
| `local_detail` | "What was the third error line?" | Raw or searchable reference can recover it. |
| `exact_evidence` | "Use the function from 1.js as the comparison basis." | Needs exact excerpt, or a recovery read. |
| `raw_content` | "Open the full 1.js content again." | Needs the complete raw result in context, or a recovery read/query. |

This is the key part of the benchmark. A strategy that saves prompt tokens can
still lose if it creates too many future recovery tool calls.

`repeat reads` is tracked separately from ordinary recovery. It estimates extra
tool calls caused specifically by read-compact-read oscillation: the model asks
for a large raw file again, that raw result is too large and gets compacted
again, then the model asks for it again. Production recoverable strategies
should use range/query recovery plus a pinned exact excerpt, keeping this value
near zero.

`exact_evidence` and `raw_content` are intentionally separate. Exact evidence
means a code excerpt or quoted error block is enough. Raw content means the
later user input needs the complete previous file/log/tool output, so summary
or excerpt is treated as missing and the benchmark counts a recovery read.

## Cache Model

Requests are represented as stable prompt segments. The simulator compares each
request with the previous request and counts exact prefix tokens. Cached tokens
are billed only after `cache.min_tokens` and rounded down to
`cache.step_tokens`.

This intentionally rewards append-only history and penalizes policies that
rewrite earlier projection on every turn.

Pricing profiles set `context_threshold_tokens` to 14,000 by default so the
fixture exercises threshold behavior on ordinary laptop-sized benchmark runs.
Change that field per model when testing a larger or smaller target context.
For production-like runs, prefer `--context-threshold-ratio` plus
`--context-target-ratio`; for example, trigger history compaction at 80% of the
model prompt limit and compact old loops until the projection is back under 30%.
`compaction_output_tokens_per_loop` caps LLM-style old-loop summaries; otherwise
the benchmark would only replace raw outputs with per-tool summaries, which is
not enough to model "compact history down to a target ratio" in long sessions.

## Cost Model

For each model request:

```text
cost =
  uncached_input_tokens * input_price
+ cached_input_tokens * cached_input_price
+ output_tokens * output_price
```

Expected recovery cost is added when a later user dependency is not retained but
can be recovered by a reference, path, process id, cursor, or query.

LLM compaction cost is also added separately when a policy rewrites old history:

```text
llm_compaction_cost =
  compaction_input_tokens * input_price
+ compaction_output_tokens * output_price
```

The compaction request input is the visible history batch being compressed plus
`snapshot_overhead_tokens`. The compaction output is the compact history record
that replaces that whole batch. `LLM comp reqs` counts the extra model requests;
`LLM comp items` counts how many old loops/snapshots/current-loop chunks were
included in those requests. These requests do not count as ordinary agent
requests in the table.

Failure penalty is added when the required dependency is neither retained nor
recoverable.

## Interpreting Results

Use the aggregate table to compare strategy families, then inspect scenario
breakdowns. A good production policy should:

- keep `max_context_tokens` bounded,
- preserve a high cache hit ratio,
- keep `expected_extra_tool_calls` low,
- keep `answerability_score` near 1.0,
- complete the requested turns without prompt-limit failures.

Stress results can change the decision boundary. On long coding runs, immediate
summary projection usually has the lowest token cost but the highest expected
recovery traffic. Threshold strategies preserve more raw/exact detail early,
but pay more uncached input when pressure eventually rewrites the projection.

This is not the same as generic LLM content compression. The first fix is
structural: cap current-loop exact excerpts, keep handles/search refs, and decay
old exact evidence to summary+handle. LLM-written content compression should be
the next fallback for old assistant/user prose or for evidence that has no
recoverable handle.

The benchmark is not a quality judge. It is a deterministic cost/cache/risk
simulator. Later versions can add real session replay and LLM-judged probes.
