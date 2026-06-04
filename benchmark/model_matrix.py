#!/usr/bin/env python3
"""Run context policy benchmark across multiple pricing profiles."""

from __future__ import annotations

import argparse
from pathlib import Path

import context_policy_benchmark as bench
from report_utils import render_html_report, write_report_files


DEFAULT_PRICING = [
    "openai-gpt-5.4.json",
    "openai-gpt-5.5.json",
    "openai-gpt-5.4-mini.json",
    "kimi-k2.6.json",
    "deepseek-chat.json",
    "deepseek-v4-flash.json",
    "deepseek-v4-pro-promo.json",
]


DEFAULT_POLICIES = (
    "immediate_summary",
    "loop_boundary_summary",
    "loop_boundary_budgeted",
    "loop_boundary_evidence_summary",
    "duckagent_recoverable_boundary",
    "duckagent_recoverable_decay",
    "duckagent_recoverable_decay_lean",
    "duckagent_recoverable_decay_balanced",
    "duckagent_recoverable_decay_tight",
    "duckagent_recoverable_decay_tight_90",
    "duckagent_recoverable_decay_tight_95",
    "duckagent_recoverable_decay_guarded_mid",
    "duckagent_recoverable_decay_guarded_mid_naive_recovery",
    "duckagent_recoverable_decay_guarded_late",
    "duckagent_recoverable_decay_guarded_late_tight",
    "duckagent_recoverable_decay_late_current",
    "duckagent_recoverable_decay_late_tight",
    "duckagent_summary_history_recoverable_current",
    "duckagent_recoverable_decay_adaptive",
    "duckagent_recoverable_decay_adaptive_guarded",
    "duckagent_recoverable_decay_recent2",
    "duckagent_recoverable_decay_soft",
    "duckagent_recoverable_decay_relative",
    "adaptive_first",
    "raw_snapshot",
    "pressure_summary",
    "pressure_adaptive",
    "evidence_budget",
    "early_windowed_prune",
    "late_guarded_truncation",
)


def pricing_name(path: Path) -> str:
    raw = bench.load_json(path)
    return str(raw.get("metadata", {}).get("name") or path.stem)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--pricing-dir",
        type=Path,
        default=Path(__file__).resolve().parent / "pricing",
        help="Directory containing benchmark pricing profiles.",
    )
    parser.add_argument(
        "--pricing",
        help="Comma-separated pricing JSON filenames. Defaults to common profiles.",
    )
    parser.add_argument(
        "--policies",
        default=",".join(DEFAULT_POLICIES),
        help="Comma-separated policy names.",
    )
    parser.add_argument(
        "--long-run-turns",
        type=int,
        default=1000,
        help="Number of user turns in the generated long-run workload.",
    )
    parser.add_argument(
        "--suite",
        choices=("long_run", "validation", "combined"),
        default="long_run",
        help="Workload suite to run. validation is a deterministic holdout set.",
    )
    parser.add_argument(
        "--validation-scale",
        type=int,
        default=1,
        help="Scale validation workload loop counts when --suite uses validation.",
    )
    parser.add_argument(
        "--context-threshold-ratio",
        type=float,
        default=0.8,
        help="Snapshot threshold ratio of the model prompt limit.",
    )
    parser.add_argument(
        "--context-target-ratio",
        type=float,
        default=0.3,
        help="Post-compaction target ratio of the model prompt limit.",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        help="Write report.md, report.html, and results.json to this directory.",
    )
    return parser.parse_args()


def row_for_metric(path: Path, metric: bench.SimulationMetrics, currency: str) -> dict[str, str]:
    recovery_tokens = metric.expected_recovery_input_tokens + metric.expected_recovery_output_tokens
    context_util = bench.format_ratio(
        bench.ratio(metric.max_context_tokens, metric.prompt_limit_tokens)
    )
    reqs_per_turn = (
        metric.request_count / metric.completed_user_turn_count
        if metric.completed_user_turn_count
        else 0.0
    )
    expected_request_count = metric.request_count + metric.expected_extra_tool_calls
    expected_reqs_per_turn = (
        expected_request_count / metric.completed_user_turn_count
        if metric.completed_user_turn_count
        else 0.0
    )
    return {
        "policy": metric.policy,
        "model": pricing_name(path),
        "turns": f"{metric.completed_user_turn_count}/{metric.requested_user_turn_count}",
        "cache_hit": f"{metric.cache_hit_ratio:.1%}",
        "cost": f"{currency} {metric.total_cost:.6f}",
        "recovery_toks": f"{recovery_tokens:.0f}",
        "extra_tools": f"{metric.expected_extra_tool_calls:.2f}",
        "repeat_reads": f"{metric.expected_repeat_recovery_tool_calls:.2f}",
        "base_reqs": str(metric.request_count),
        "base_reqs_per_turn": f"{reqs_per_turn:.2f}",
        "expected_reqs": f"{expected_request_count:.2f}",
        "expected_reqs_per_turn": f"{expected_reqs_per_turn:.2f}",
        "expected_toks": f"{metric.expected_total_tokens:.0f}",
        "max_ctx": str(metric.max_context_tokens),
        "ctx_util": context_util,
        "tool_struct_comp": str(metric.tool_structured_compression_events),
        "tool_saved_toks": str(metric.tool_structured_compression_saved_tokens),
        "llm_comp_reqs": str(metric.llm_compaction_events),
        "llm_comp_items": str(metric.llm_compacted_items),
        "llm_comp_cost": f"{currency} {metric.llm_compaction_cost:.6f}",
    }


MATRIX_COLUMNS = [
    ("policy", "policy"),
    ("model", "model"),
    ("turns", "turns complete/requested"),
    ("cache_hit", "cache hit"),
    ("cost", "total cost"),
    ("recovery_toks", "recovery toks"),
    ("extra_tools", "extra tools"),
    ("repeat_reads", "repeat reads"),
    ("base_reqs", "base LLM reqs"),
    ("base_reqs_per_turn", "base reqs/turn"),
    ("expected_reqs", "expected LLM reqs"),
    ("expected_reqs_per_turn", "expected reqs/turn"),
    ("expected_toks", "expected toks"),
    ("max_ctx", "max ctx"),
    ("ctx_util", "ctx util"),
    ("tool_struct_comp", "tool struct comp"),
    ("tool_saved_toks", "tool saved toks"),
    ("llm_comp_reqs", "LLM comp reqs"),
    ("llm_comp_items", "LLM comp items"),
    ("llm_comp_cost", "LLM comp cost"),
]


RUN_FIELD_DEFINITIONS = [
    (
        "user turns",
        "Number of simulated user inputs. One user turn starts one Agent Loop; that loop can send multiple ordinary agent LLM requests while it calls tools, then ends when the assistant returns final content.",
    ),
    (
        "threshold",
        "Prompt-utilization ratio that triggers pre-send compression. With 0.8, the simulator starts compacting before a request would exceed 80% of the model prompt limit.",
    ),
    (
        "target",
        "Post-compression prompt-utilization target. With 0.3, history compaction tries to bring the projected prompt back near 30% of the model prompt limit.",
    ),
    (
        "policies",
        "Direct single-agent context projection strategies compared in the table. Delegated MainAgent/ExecuteAgent baselines are intentionally excluded.",
    ),
    (
        "suite",
        "Workload suite used by the matrix. `long_run` is the main 1000-turn stress stream; `validation` is a separate holdout suite with chat-heavy, exact-revisit, log-recovery, and wide-current-loop patterns; `combined` runs both.",
    ),
    (
        "pricing profiles",
        "Pricing and limit JSON files used for each model. Costs are only as accurate as those local profiles; provider-specific long-context surcharges must be encoded there to be counted.",
    ),
]


MATRIX_FIELD_DEFINITIONS = [
    ("policy", "Context projection strategy being simulated."),
    ("model", "Pricing and limit profile used for cost and context calculations."),
    (
        "turns complete/requested",
        "Completed user turns divided by requested user turns. A completed turn means the Agent Loop reached final assistant content without a prompt/output-limit failure.",
    ),
    (
        "cache hit",
        "Share of input tokens billed as cached input by the prompt-cache approximation. It is not a success score; cached input can still dominate cost at very large prompt sizes.",
    ),
    (
        "total cost",
        "Base agent-request cost plus expected recovery cost plus separate LLM compaction cost plus any limit penalty.",
    ),
    (
        "recovery toks",
        "Expected extra input+output tokens from re-reading/querying/process evidence after compressed context omitted details the current final answer or a later user turn needs.",
    ),
    (
        "extra tools",
        "Expected extra recovery tool calls caused by omitted raw details in the current loop or later follow-ups. This is probabilistic, not an observed integer.",
    ),
    (
        "repeat reads",
        "Expected extra recovery tool calls caused specifically by read-compact-read oscillation after a missing raw/exact dependency triggers repeated full-file recovery. Robust range/query recovery should keep this near zero.",
    ),
    (
        "base LLM reqs",
        "Ordinary model requests inside the generated Agent Loops before probabilistic recovery is applied. This excludes separate LLM compaction requests.",
    ),
    (
        "base reqs/turn",
        "base LLM reqs divided by completed user turns.",
    ),
    (
        "expected LLM reqs",
        "base LLM reqs plus expected extra continuation requests caused by recovery tool calls. This is probabilistic, so it can be fractional.",
    ),
    (
        "expected reqs/turn",
        "expected LLM reqs divided by completed user turns. Use this when comparing strategies that drop raw evidence and may need to re-read it.",
    ),
    (
        "expected toks",
        "Input tokens + output tokens + expected recovery tokens + LLM compaction input/output tokens.",
    ),
    ("max ctx", "Maximum projected prompt tokens sent in any ordinary agent request."),
    ("ctx util", "max ctx divided by the model prompt limit."),
    (
        "tool struct comp",
        "Count of tool results whose model-visible form is smaller than raw output, such as summary, excerpt, handle, or marker.",
    ),
    (
        "tool saved toks",
        "Raw tool output tokens minus model-visible tool-result tokens. Larger means more raw evidence was replaced by structured tool projection.",
    ),
    (
        "LLM comp reqs",
        "Separate compaction-model requests. One request can compact many old loops or snapshots.",
    ),
    (
        "LLM comp items",
        "Old loops, old snapshots, or current-loop chunks included inside those LLM compaction requests.",
    ),
    (
        "LLM comp cost",
        "Separate input/output cost of the LLM compaction requests.",
    ),
]


def definition_table(rows: list[tuple[str, str]], first_label: str = "field") -> list[str]:
    lines = [
        f"| {first_label} | meaning |",
        "|---|---|",
    ]
    lines.extend(f"| {field} | {meaning} |" for field, meaning in rows)
    return lines


def policy_definition_rows(policy_names: list[str]) -> list[tuple[str, str]]:
    selected = bench.select_policies(",".join(policy_names))
    return [(policy.name, policy.description) for policy in selected]


def format_matrix_markdown(rows: list[dict[str, str]], metadata: dict[str, object]) -> str:
    lines = [
        "# Context Policy Model Matrix",
        "",
        "## Run",
        "",
        f"- user turns: {metadata['long_run_turns']}",
        f"- threshold: {metadata['context_threshold_ratio']}",
        f"- target: {metadata['context_target_ratio']}",
        f"- suite: {metadata['suite']}",
        f"- validation scale: {metadata['validation_scale']}",
        f"- policies: {', '.join(metadata['policies'])}",
        f"- pricing profiles: {', '.join(metadata['pricing_files'])}",
        "",
        "### Run Field Definitions",
        "",
        *definition_table(RUN_FIELD_DEFINITIONS),
        "",
        "### Policy Definitions",
        "",
        *definition_table(
            policy_definition_rows(list(metadata["policies"])),
            first_label="policy",
        ),
        "",
        "## Results",
        "",
        "| " + " | ".join(label for _, label in MATRIX_COLUMNS) + " |",
        "|"
        + "|".join(
            "---" if key in ("model", "policy", "fail_request") else "---:"
            for key, _ in MATRIX_COLUMNS
        )
        + "|",
    ]
    for row in rows:
        lines.append("| " + " | ".join(row[key] for key, _ in MATRIX_COLUMNS) + " |")
    lines.extend(
        [
            "",
            "## Column Definitions",
            "",
            *definition_table(MATRIX_FIELD_DEFINITIONS, first_label="column"),
            "",
            "## Notes",
            "",
            "- `user turns: 1000` does not mean 1000 model requests. It means 1000 simulated user inputs / Agent Loops. Each Agent Loop may send multiple ordinary agent LLM requests because every tool call usually requires another request before the final assistant content.",
            "- This is a long-context, multi-tool code-agent stress benchmark, not a normal chat-cost estimate. Plain chat has fewer agent requests per user turn, much smaller prompts, little or no tool output, and should cost far less.",
            "- The main purpose of this report is to compare prompt-cache behavior, context compaction pressure, tool-output projection, recovery cost, and model pricing sensitivity across policies.",
            "- Recovery cost is evaluated after the pre-send projection for that user turn. If a policy decays old exact evidence immediately before the request is sent, a follow-up dependency on that evidence is counted as a recovery read/query.",
            "- The validation suite is a holdout simulation, not real-world proof. It helps detect obvious overfitting to the generated long-run stream by changing the mix of chat, exact-code revisits, log recovery, and very wide current loops.",
            "- High `cache hit` does not guarantee low cost. If each request carries hundreds of thousands of cached tokens, cached input can still be the dominant cost.",
            "- Diagnostic failure fields are omitted from the main table because every policy is expected to compact before request send; full raw metrics remain in `results.json`.",
            "- `raw_snapshot` keeps tool outputs raw until threshold pressure, then LLM-compacts the oldest completed history first.",
            "- `early_windowed_prune` and `late_guarded_truncation` are source-informed approximations, not direct benchmarks of the source projects they were compared against. The simulator models token-level context projection and compaction pressure; it does not execute real summarizers, provider cache-control metadata, or exact tokenizer-specific truncation.",
            "- LLM compaction is greedy by projected token delta: compact oldest completed loops before recent loops, then group old snapshots when many small snapshots accumulate.",
            "- Every listed policy is a direct single-agent policy; delegated agent baselines are intentionally excluded.",
        ]
    )
    return "\n".join(lines)


def run_matrix(args: argparse.Namespace) -> tuple[list[dict[str, str]], list[dict[str, object]], dict[str, object]]:
    pricing_files = (
        [item.strip() for item in args.pricing.split(",") if item.strip()]
        if args.pricing
        else DEFAULT_PRICING
    )
    policies = bench.select_policies(args.policies)
    workloads = matrix_workloads(args)
    rows: list[dict[str, str]] = []
    raw_results: list[dict[str, object]] = []
    for filename in pricing_files:
        path = args.pricing_dir / filename
        pricing, cache, limits, config = bench.load_pricing(path)
        config.context_threshold_ratio = args.context_threshold_ratio
        config.context_target_ratio = args.context_target_ratio
        results = bench.run_benchmark(workloads, policies, pricing, cache, limits, config)
        for metric in bench.aggregate_results(results):
            rows.append(row_for_metric(path, metric, pricing.currency))
            raw = bench.metrics_to_dict(metric)
            raw["model"] = pricing_name(path)
            raw["pricing_file"] = filename
            raw_results.append(raw)
    metadata: dict[str, object] = {
        "long_run_turns": args.long_run_turns,
        "suite": args.suite,
        "validation_scale": args.validation_scale,
        "context_threshold_ratio": args.context_threshold_ratio,
        "context_target_ratio": args.context_target_ratio,
        "policies": [policy.name for policy in policies],
        "pricing_files": pricing_files,
    }
    return rows, raw_results, metadata


def matrix_workloads(args: argparse.Namespace) -> list[bench.Workload]:
    workloads: list[bench.Workload] = []
    if args.suite in ("long_run", "combined"):
        workloads.append(bench.long_run_workload(args.long_run_turns))
    if args.suite in ("validation", "combined"):
        workloads.extend(bench.validation_workloads(args.validation_scale))
    return workloads


def main() -> None:
    args = parse_args()
    rows, raw_results, metadata = run_matrix(args)
    markdown = format_matrix_markdown(rows, metadata)
    print(markdown)
    if args.output_dir:
        payload = {
            "metadata": metadata,
            "rows": rows,
            "results": raw_results,
        }
        html = render_html_report("Context Policy Model Matrix", markdown)
        markdown_path, html_path, json_path = write_report_files(
            args.output_dir,
            markdown,
            html,
            payload,
        )
        print()
        print(f"Wrote markdown: {markdown_path}")
        print(f"Wrote html: {html_path}")
        print(f"Wrote json: {json_path}")


if __name__ == "__main__":
    main()
