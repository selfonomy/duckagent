#!/usr/bin/env python3
"""Generate benchmark pricing profiles from https://models.dev/api.json."""

from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.request
from datetime import date
from typing import Any


MODELS_DEV_URL = "https://models.dev/api.json"


def fetch_models(url: str) -> dict[str, Any]:
    request = urllib.request.Request(
        url,
        headers={"User-Agent": "duckagent-context-benchmark/1.0"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def iter_models(data: dict[str, Any]):
    for provider_id, provider in data.items():
        for model_id, model in provider.get("models", {}).items():
            yield provider_id, model_id, model


def compact_json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True)


def search_models(data: dict[str, Any], query: str) -> list[tuple[str, str, dict[str, Any]]]:
    normalized = query.lower()
    matches = []
    for provider_id, model_id, model in iter_models(data):
        haystack = " ".join(
            str(model.get(key, "")) for key in ("id", "name", "family")
        ).lower()
        if normalized in provider_id.lower() or normalized in haystack:
            matches.append((provider_id, model_id, model))
    return matches


def print_search_results(matches: list[tuple[str, str, dict[str, Any]]]) -> None:
    print("provider\tmodel\tname\tcost\tlimit")
    for provider_id, model_id, model in matches:
        print(
            "\t".join(
                [
                    provider_id,
                    model_id,
                    str(model.get("name", "")),
                    compact_json(model.get("cost", {})),
                    compact_json(model.get("limit", {})),
                ]
            )
        )


def safe_profile_name(provider_id: str, model_id: str) -> str:
    raw = f"{provider_id}-{model_id}".lower()
    return re.sub(r"[^a-z0-9._-]+", "-", raw).strip("-")


def default_simulation(context_threshold_tokens: int, failure_penalty_cost: float) -> dict[str, Any]:
    return {
        "system_tokens": 1800,
        "tool_schema_tokens": 2400,
        "memory_catalog_tokens": 420,
        "tool_call_overhead_tokens": 28,
        "loop_compaction_overhead_tokens": 96,
        "snapshot_overhead_tokens": 256,
        "context_threshold_tokens": context_threshold_tokens,
        "large_tool_output_threshold_tokens": 16000,
        "keep_recent_loops_on_snapshot": 1,
        "compaction_output_tokens_per_loop": 180,
        "recovery_model_overhead_tokens": 1400,
        "failure_penalty_cost": failure_penalty_cost,
    }


def threshold_from_limit(
    limit: dict[str, Any], ratio: float, explicit_threshold: int | None
) -> int:
    if explicit_threshold is not None:
        return explicit_threshold
    prompt_limit = limit.get("input") or limit.get("context")
    if isinstance(prompt_limit, int) and prompt_limit > 0:
        return max(1024, int(prompt_limit * ratio))
    return 14000


def build_profile(
    provider_id: str,
    model_id: str,
    model: dict[str, Any],
    args: argparse.Namespace,
) -> dict[str, Any]:
    cost = model.get("cost", {})
    limit = model.get("limit", {})
    if "input" not in cost or "output" not in cost:
        raise SystemExit(f"Model {provider_id}/{model_id} is missing cost.input or cost.output")

    cache_read = cost.get("cache_read", cost["input"])
    notes = "Generated from models.dev. cache_read is used when present; cache_write and tiered long-context pricing are not modeled."
    if "cache_read" not in cost:
        notes += " This model entry has no cache_read field, so cached input equals normal input."

    context_threshold = threshold_from_limit(
        limit,
        args.context_threshold_ratio,
        args.context_threshold_tokens,
    )
    limits = {}
    if "context" in limit:
        limits["context_tokens"] = limit["context"]
    if "input" in limit:
        limits["input_tokens"] = limit["input"]
    if "output" in limit:
        limits["output_tokens"] = limit["output"]

    return {
        "metadata": {
            "name": args.name or safe_profile_name(provider_id, model_id),
            "source": args.url,
            "checked_at": date.today().isoformat(),
            "models_dev_provider": provider_id,
            "models_dev_model": model_id,
            "model_name": model.get("name", model_id),
            "notes": notes,
        },
        "pricing": {
            "currency": "USD",
            "input_per_million": cost["input"],
            "cached_input_per_million": cache_read,
            "output_per_million": cost["output"],
            "recovery_latency_penalty_ms": args.recovery_latency_penalty_ms,
            "request_latency_ms": args.request_latency_ms,
        },
        "cache": {
            "min_tokens": args.cache_min_tokens,
            "step_tokens": args.cache_step_tokens,
            "ttl_turns": args.cache_ttl_turns,
        },
        "limits": limits,
        "simulation": default_simulation(context_threshold, args.failure_penalty_cost),
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", default=MODELS_DEV_URL)
    parser.add_argument("--search", help="Search provider/model/name/family and print matches.")
    parser.add_argument("--provider", help="models.dev provider id.")
    parser.add_argument("--model", help="models.dev model id.")
    parser.add_argument("--name", help="Override generated profile metadata name.")
    parser.add_argument("--context-threshold-ratio", type=float, default=0.75)
    parser.add_argument("--context-threshold-tokens", type=int)
    parser.add_argument("--cache-min-tokens", type=int, default=1024)
    parser.add_argument("--cache-step-tokens", type=int, default=128)
    parser.add_argument("--cache-ttl-turns", type=int, default=1000)
    parser.add_argument("--request-latency-ms", type=float, default=650.0)
    parser.add_argument("--recovery-latency-penalty-ms", type=float, default=900.0)
    parser.add_argument("--failure-penalty-cost", type=float, default=0.02)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    data = fetch_models(args.url)
    if args.search:
        print_search_results(search_models(data, args.search))
        return
    if not args.provider or not args.model:
        raise SystemExit("Use --search QUERY or provide both --provider and --model.")
    provider = data.get(args.provider)
    if not provider:
        raise SystemExit(f"Unknown provider: {args.provider}")
    model = provider.get("models", {}).get(args.model)
    if not model:
        raise SystemExit(f"Unknown model for provider {args.provider}: {args.model}")
    json.dump(
        build_profile(args.provider, args.model, model, args),
        sys.stdout,
        ensure_ascii=False,
        indent=2,
    )
    print()


if __name__ == "__main__":
    main()
