#!/usr/bin/env python3
"""Sweep recoverable-decay policy budgets around the balanced baseline."""

from __future__ import annotations

import argparse
import dataclasses
import json
from pathlib import Path
from typing import Any

import context_policy_benchmark as bench
from report_utils import render_html_report, write_report_files


DEFAULT_PRICING = [
    "openai-gpt-5.4-mini.json",
    "kimi-k2.6.json",
    "deepseek-chat.json",
    "deepseek-v4-flash.json",
]

BASELINE_POLICY_NAME = "duckagent_recoverable_decay_balanced"
DEFAULT_PRESSURE_BUDGETS = [15_000, 18_000, 21_000]
DEFAULT_HISTORY_BUDGETS = [0, 1_000, 1_500, 2_000, 2_500]
DEFAULT_THRESHOLD_BUDGETS = [0, 500]


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
        help="Comma-separated pricing JSON filenames.",
    )
    parser.add_argument(
        "--long-run-turns",
        type=int,
        default=1000,
        help="Number of user turns in the generated long-run workload.",
    )
    parser.add_argument(
        "--validation-scale",
        type=int,
        default=2,
        help="Scale validation workload loop counts.",
    )
    parser.add_argument(
        "--context-threshold-ratio",
        type=float,
        default=0.8,
        help="Prompt-utilization ratio that triggers pre-send compression.",
    )
    parser.add_argument(
        "--context-target-ratio",
        type=float,
        default=0.3,
        help="Post-compaction target ratio.",
    )
    parser.add_argument(
        "--max-rows",
        type=int,
        default=80,
        help="Maximum rows to render in the report table.",
    )
    parser.add_argument(
        "--pressure-budgets",
        help="Comma-separated current-loop exact budgets, e.g. 15k,18k,21k.",
    )
    parser.add_argument(
        "--history-budgets",
        help="Comma-separated completed-history exact budgets, e.g. 0,1000,1500,2000.",
    )
    parser.add_argument(
        "--threshold-budgets",
        help="Comma-separated threshold-decay exact budgets, e.g. 0,500.",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        help="Write report.md, report.html, and results.json to this directory.",
    )
    return parser.parse_args()


def base_policy() -> bench.PolicySpec:
    return next(policy for policy in bench.POLICIES if policy.name == BASELINE_POLICY_NAME)


def parse_budget_list(raw: str | None, default: list[int]) -> list[int]:
    if not raw:
        return default
    values = []
    for item in raw.split(","):
        value = item.strip().lower()
        if not value:
            continue
        multiplier = 1
        if value.endswith("k"):
            value = value[:-1]
            multiplier = 1000
        values.append(int(float(value) * multiplier))
    return sorted(set(values))


def build_candidate_policies(
    *,
    pressure_budgets: list[int] | None = None,
    history_budgets: list[int] | None = None,
    threshold_budgets: list[int] | None = None,
) -> list[bench.PolicySpec]:
    """Return deterministic budget candidates centered on the balanced baseline."""

    baseline = base_policy()
    candidates: list[bench.PolicySpec] = [baseline]

    pressure_budgets = pressure_budgets or DEFAULT_PRESSURE_BUDGETS
    history_budgets = history_budgets or DEFAULT_HISTORY_BUDGETS
    threshold_budgets = threshold_budgets or DEFAULT_THRESHOLD_BUDGETS

    seen = {baseline.name}
    for pressure in pressure_budgets:
        for history in history_budgets:
            for threshold in threshold_budgets:
                if pressure == 18_000 and history == 2_000 and threshold == 0:
                    continue
                name = (
                    "sweep_recoverable_decay"
                    f"_p{pressure // 1000:02d}k"
                    f"_h{history:04d}"
                    f"_t{threshold:04d}"
                )
                if name in seen:
                    continue
                seen.add(name)
                candidates.append(
                    dataclasses.replace(
                        baseline,
                        name=name,
                        description=(
                            "Recoverable-decay budget sweep candidate derived from "
                            f"{BASELINE_POLICY_NAME}: current-loop pressure exact budget "
                            f"{pressure} tokens, completed-history exact budget {history} tokens, "
                            f"threshold decay exact budget {threshold} tokens."
                        ),
                        pressure_exact_budget_tokens=pressure,
                        history_exact_budget_tokens=history,
                        threshold_exact_budget_tokens=threshold,
                        pressure_exact_budget_ratio=None,
                        history_exact_budget_ratio=None,
                        threshold_exact_budget_ratio=None,
                        pressure_exact_budget_floor_tokens=None,
                        history_exact_budget_floor_tokens=None,
                        threshold_exact_budget_floor_tokens=None,
                    )
                )

    guarded = next(
        policy
        for policy in bench.POLICIES
        if policy.name == "duckagent_recoverable_decay_adaptive_guarded"
    )
    candidates.append(guarded)
    return candidates


def pricing_name(path: Path) -> str:
    raw = bench.load_json(path)
    return str(raw.get("metadata", {}).get("name") or path.stem)


def recovery_tokens(metric: bench.SimulationMetrics) -> float:
    return metric.expected_recovery_input_tokens + metric.expected_recovery_output_tokens


def metrics_by_policy(
    workload: bench.Workload,
    policies: list[bench.PolicySpec],
    pricing_path: Path,
    threshold_ratio: float,
    target_ratio: float,
) -> dict[str, bench.SimulationMetrics]:
    pricing, cache, limits, config = bench.load_pricing(pricing_path)
    config.context_threshold_ratio = threshold_ratio
    config.context_target_ratio = target_ratio
    results = bench.run_benchmark([workload], policies, pricing, cache, limits, config)
    return {metric.policy: metric for metric in bench.aggregate_results(results)}


def classify_candidate(
    candidate: bench.SimulationMetrics,
    baseline: bench.SimulationMetrics,
) -> str:
    cost_better = candidate.total_cost < baseline.total_cost
    recovery_better = recovery_tokens(candidate) <= recovery_tokens(baseline)
    tools_better = candidate.expected_extra_tool_calls <= baseline.expected_extra_tool_calls
    if cost_better and recovery_better and tools_better:
        return "dominates"
    if (
        candidate.total_cost <= baseline.total_cost * 0.995
        and recovery_tokens(candidate) <= recovery_tokens(baseline) * 1.02
        and candidate.expected_extra_tool_calls <= baseline.expected_extra_tool_calls * 1.02
    ):
        return "safe_cost_win"
    if cost_better:
        return "cost_only"
    return "loses"


def row_for_candidate(
    suite: str,
    model: str,
    metric: bench.SimulationMetrics,
    baseline: bench.SimulationMetrics,
    currency: str,
) -> dict[str, str]:
    recovery = recovery_tokens(metric)
    baseline_recovery = recovery_tokens(baseline)
    cost_delta = metric.total_cost - baseline.total_cost
    recovery_delta = recovery - baseline_recovery
    extra_delta = metric.expected_extra_tool_calls - baseline.expected_extra_tool_calls
    return {
        "suite": suite,
        "model": model,
        "policy": metric.policy,
        "class": classify_candidate(metric, baseline),
        "cost": f"{currency} {metric.total_cost:.6f}",
        "cost_delta": f"{cost_delta:+.6f}",
        "recovery_toks": f"{recovery:.0f}",
        "recovery_delta": f"{recovery_delta:+.0f}",
        "extra_tools": f"{metric.expected_extra_tool_calls:.2f}",
        "extra_delta": f"{extra_delta:+.2f}",
        "cache_hit": f"{metric.cache_hit_ratio:.1%}",
        "llm_comp_reqs": str(metric.llm_compaction_events),
        "max_ctx": str(metric.max_context_tokens),
    }


def row_sort_key(row: dict[str, str]) -> tuple[int, float, float, str]:
    class_rank = {
        "dominates": 0,
        "safe_cost_win": 1,
        "cost_only": 2,
        "loses": 3,
    }.get(row["class"], 9)
    return (
        class_rank,
        float(row["cost_delta"]),
        float(row["recovery_delta"]),
        row["policy"],
    )


def suite_workloads(args: argparse.Namespace) -> dict[str, bench.Workload]:
    return {
        "long_run": bench.long_run_workload(args.long_run_turns),
        "validation": bench.Workload(
            name=f"validation_scale_{args.validation_scale}",
            description="Combined holdout validation workloads.",
            loops=[
                loop
                for workload in bench.validation_workloads(args.validation_scale)
                for loop in workload.loops
            ],
        ),
    }


SWEEP_COLUMNS = [
    ("suite", "suite"),
    ("model", "model"),
    ("policy", "policy"),
    ("class", "class"),
    ("cost", "total cost"),
    ("cost_delta", "cost delta vs balanced"),
    ("recovery_toks", "recovery toks"),
    ("recovery_delta", "recovery delta"),
    ("extra_tools", "extra tools"),
    ("extra_delta", "extra tools delta"),
    ("cache_hit", "cache hit"),
    ("llm_comp_reqs", "LLM comp reqs"),
    ("max_ctx", "max ctx"),
]


def format_markdown(
    rows: list[dict[str, str]],
    metadata: dict[str, Any],
) -> str:
    lines = [
        "# Recoverable Policy Sweep",
        "",
        "## Run",
        "",
        f"- baseline: {BASELINE_POLICY_NAME}",
        f"- candidate policies: {metadata['candidate_policy_count']}",
        f"- long-run turns: {metadata['long_run_turns']}",
        f"- validation scale: {metadata['validation_scale']}",
        f"- threshold: {metadata['context_threshold_ratio']}",
        f"- target: {metadata['context_target_ratio']}",
        f"- pricing profiles: {', '.join(metadata['pricing_files'])}",
        "",
        "## Interpretation",
        "",
        "- `dominates` means lower total cost, no more recovery tokens, and no more extra recovery tools than balanced.",
        "- `safe_cost_win` means at least 0.5% lower total cost while recovery tokens and extra recovery tools stay within 2% of balanced.",
        "- `cost_only` means lower total cost, but recovery or extra tools got worse. Treat this as a possible quality regression, not a real win.",
        "- `loses` means the candidate did not beat balanced on total cost.",
        "",
        "## Top Rows",
        "",
        "| " + " | ".join(label for _, label in SWEEP_COLUMNS) + " |",
        "|"
        + "|".join("---" if key in {"suite", "model", "policy", "class"} else "---:" for key, _ in SWEEP_COLUMNS)
        + "|",
    ]
    for row in rows:
        lines.append("| " + " | ".join(row[key] for key, _ in SWEEP_COLUMNS) + " |")
    lines.extend(
        [
            "",
            "## Notes",
            "",
            "- This sweep is intentionally narrow: it only changes recoverable-decay exact-evidence budgets around the balanced baseline.",
            "- The report includes long-run and validation suites separately so a policy that wins only on generated stress traffic does not look better than it is.",
            "- A lower dollar value is not enough. For context policies, recovery tokens and extra recovery tool calls are the proxy for whether the model had to re-open evidence it lost.",
        ]
    )
    return "\n".join(lines)


def run_sweep(args: argparse.Namespace) -> tuple[list[dict[str, str]], dict[str, Any], list[dict[str, Any]]]:
    pricing_files = (
        [item.strip() for item in args.pricing.split(",") if item.strip()]
        if args.pricing
        else DEFAULT_PRICING
    )
    policies = build_candidate_policies(
        pressure_budgets=parse_budget_list(args.pressure_budgets, DEFAULT_PRESSURE_BUDGETS),
        history_budgets=parse_budget_list(args.history_budgets, DEFAULT_HISTORY_BUDGETS),
        threshold_budgets=parse_budget_list(args.threshold_budgets, DEFAULT_THRESHOLD_BUDGETS),
    )
    workloads = suite_workloads(args)
    rows: list[dict[str, str]] = []
    raw_results: list[dict[str, Any]] = []

    for filename in pricing_files:
        pricing_path = args.pricing_dir / filename
        pricing, _, _, _ = bench.load_pricing(pricing_path)
        model = pricing_name(pricing_path)
        for suite_name, workload in workloads.items():
            metrics = metrics_by_policy(
                workload,
                policies,
                pricing_path,
                args.context_threshold_ratio,
                args.context_target_ratio,
            )
            baseline = metrics[BASELINE_POLICY_NAME]
            for metric in metrics.values():
                row = row_for_candidate(suite_name, model, metric, baseline, pricing.currency)
                rows.append(row)
                raw = bench.metrics_to_dict(metric)
                raw.update(
                    {
                        "suite": suite_name,
                        "model": model,
                        "pricing_file": filename,
                        "class": row["class"],
                        "cost_delta_vs_balanced": round(metric.total_cost - baseline.total_cost, 6),
                        "recovery_delta_vs_balanced": round(
                            recovery_tokens(metric) - recovery_tokens(baseline),
                            2,
                        ),
                        "extra_tools_delta_vs_balanced": round(
                            metric.expected_extra_tool_calls
                            - baseline.expected_extra_tool_calls,
                            4,
                        ),
                    }
                )
                raw_results.append(raw)

    rows.sort(key=row_sort_key)
    metadata: dict[str, Any] = {
        "candidate_policy_count": len(policies),
        "long_run_turns": args.long_run_turns,
        "validation_scale": args.validation_scale,
        "context_threshold_ratio": args.context_threshold_ratio,
        "context_target_ratio": args.context_target_ratio,
        "pricing_files": pricing_files,
    }
    return rows[: max(1, args.max_rows)], metadata, raw_results


def main() -> None:
    args = parse_args()
    rows, metadata, raw_results = run_sweep(args)
    markdown = format_markdown(rows, metadata)
    print(markdown)
    if args.output_dir:
        payload = {
            "metadata": metadata,
            "rows": rows,
            "results": raw_results,
        }
        html = render_html_report("Recoverable Policy Sweep", markdown)
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
