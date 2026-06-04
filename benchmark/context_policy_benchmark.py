#!/usr/bin/env python3
"""Offline benchmark for direct single-agent context projection policies.

This benchmark intentionally models one direct agent. There is no parent/child
agent split and no delegation tool path. Every policy models one Agent Loop in a
single agent that can call capabilities itself.

Before every simulated model request, the projection is forced under the model
prompt limit by applying, in order:

1. current-loop tool-output projection when the current loop is too large,
2. old-history tool-output projection for threshold policies,
3. LLM history compaction for old completed loops,
4. grouped history compaction when many old snapshots accumulate,
5. current-loop compaction when one loop is too wide to send as-is.

Therefore a long-run strategy should complete all requested user turns. If it
does not, that is a benchmark bug or an impossible single-request current loop.
"""

from __future__ import annotations

import argparse
import copy
import json
import math
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable

from report_utils import render_html_report, write_report_files


DEPENDENCY_TYPES = (
    "none",
    "global_conclusion",
    "continuation_handle",
    "local_detail",
    "exact_evidence",
    "raw_content",
)

TOOL_MODES = (
    "raw",
    "summary",
    "policy",
    "budgeted",
    "recoverable",
    "hermes_tool",
    "openclaw_live",
    "handle",
    "marker",
)

RECOVERY_MODES = (
    "one_shot",
    "naive_raw_reread",
    "minimal_pinned",
)

HERMES_PROCESS_OUTPUT_CAP_TOKENS = 12_500
HERMES_PERSIST_THRESHOLD_TOKENS = 25_000
HERMES_PREVIEW_TOKENS = 375
OPENCLAW_LIVE_TOOL_RESULT_TOKENS = 4_000


@dataclass
class CacheConfig:
    min_tokens: int
    step_tokens: int
    ttl_turns: int | None = None


@dataclass
class PricingConfig:
    currency: str
    input_per_million: float
    cached_input_per_million: float
    output_per_million: float
    recovery_latency_penalty_ms: float
    request_latency_ms: float

    @property
    def input_per_token(self) -> float:
        return self.input_per_million / 1_000_000.0

    @property
    def cached_input_per_token(self) -> float:
        return self.cached_input_per_million / 1_000_000.0

    @property
    def output_per_token(self) -> float:
        return self.output_per_million / 1_000_000.0


@dataclass
class LimitConfig:
    context_tokens: int | None = None
    input_tokens: int | None = None
    output_tokens: int | None = None

    @property
    def prompt_limit_tokens(self) -> int | None:
        return self.input_tokens or self.context_tokens


@dataclass
class SimulationConfig:
    system_tokens: int
    tool_schema_tokens: int
    memory_catalog_tokens: int
    tool_call_overhead_tokens: int
    loop_compaction_overhead_tokens: int
    snapshot_overhead_tokens: int
    context_threshold_tokens: int
    large_tool_output_threshold_tokens: int
    keep_recent_loops_on_snapshot: int
    compaction_output_tokens_per_loop: int
    recovery_model_overhead_tokens: int
    failure_penalty_cost: float
    context_threshold_ratio: float | None = None
    context_target_ratio: float | None = None


@dataclass
class ToolCall:
    name: str
    capability: str
    purpose_tokens: int
    args_tokens: int
    outputs: dict[str, int]
    preserve: str
    recovery_tokens: dict[str, int]

    @property
    def raw_tokens(self) -> int:
        return self.outputs.get("raw", 0)

    @property
    def summary_tokens(self) -> int:
        return self.outputs.get("summary", min(self.raw_tokens, 256))

    @property
    def exact_excerpt_tokens(self) -> int:
        return self.outputs.get("exact_excerpt", self.summary_tokens)

    @property
    def handle_tokens(self) -> int:
        return self.outputs.get("handle", min(self.summary_tokens, 96))


@dataclass
class BenchLoop:
    name: str
    user_tokens: int
    assistant_final_tokens: int
    tool_calls: list[ToolCall]
    dependency_probabilities: dict[str, float]
    final_dependency_probabilities: dict[str, float] = field(default_factory=dict)
    raw_dependency_events: list["RawDependencyEvent"] = field(default_factory=list)


@dataclass(frozen=True)
class RawDependencyEvent:
    source_loop_offset: int
    dependency: str
    probability: float
    recovery_tokens: int | None = None
    label: str = ""


@dataclass
class Workload:
    name: str
    description: str
    loops: list[BenchLoop]


@dataclass(frozen=True)
class PolicySpec:
    name: str
    description: str
    initial_tool_mode: str
    pressure_tool_mode: str
    threshold_tool_projection: bool
    exact_budget_tokens: int | None = None
    history_tool_mode: str | None = None
    context_threshold_ratio: float | None = None
    context_target_ratio: float | None = None
    current_loop_threshold_ratio: float | None = None
    current_loop_threshold_fallback_ratio: float | None = None
    current_loop_threshold_min_prompt_tokens: int | None = None
    keep_recent_loops_on_snapshot: int | None = None
    reducible_tool_modes: tuple[str, ...] = ("raw",)
    pressure_exact_budget_tokens: int | None = None
    history_exact_budget_tokens: int | None = None
    threshold_exact_budget_tokens: int | None = None
    pressure_exact_budget_ratio: float | None = None
    history_exact_budget_ratio: float | None = None
    threshold_exact_budget_ratio: float | None = None
    pressure_exact_budget_floor_tokens: int | None = None
    history_exact_budget_floor_tokens: int | None = None
    threshold_exact_budget_floor_tokens: int | None = None
    recovery_mode: str = "one_shot"
    recovery_repeat_limit: int = 3


@dataclass
class ToolProjection:
    tool: ToolCall
    mode: str
    tokens: int
    exact_retained: bool
    compression_counted: bool = False


@dataclass
class CurrentLoopProjection:
    loop: BenchLoop
    tools: list[ToolProjection] = field(default_factory=list)
    compacted_prefix_tokens: int | None = None
    compacted_tool_count: int = 0

    @property
    def tokens(self) -> int:
        if self.compacted_prefix_tokens is None:
            total = self.loop.user_tokens
            start_index = 0
        else:
            total = self.compacted_prefix_tokens
            start_index = self.compacted_tool_count
        for projection in self.tools[start_index:]:
            total += tool_call_output_tokens(projection.tool) + projection.tokens
        return total

    @property
    def history_tokens_without_final(self) -> int:
        return self.tokens

    def to_history_state(self, index: int, config: SimulationConfig) -> "LoopState":
        tokens = self.tokens + self.loop.assistant_final_tokens
        snapshot_tokens = loop_llm_snapshot_tokens(self.loop, config)
        return LoopState(
            loop=self.loop,
            index=index,
            tools=copy.deepcopy(self.tools),
            history_tokens=tokens,
            snapshot_tokens=snapshot_tokens,
            snapshot=self.compacted_prefix_tokens is not None,
        )


@dataclass
class LoopState:
    loop: BenchLoop
    index: int
    tools: list[ToolProjection]
    history_tokens: int
    snapshot_tokens: int
    snapshot: bool = False
    grouped: bool = False


@dataclass
class RequestAccounting:
    input_tokens: int
    cached_tokens: int
    uncached_tokens: int
    output_tokens: int
    cost: float
    latency_ms: float


@dataclass
class SimulationMetrics:
    workload: str
    policy: str
    requested_user_turn_count: int = 0
    user_turn_count: int = 0
    completed_user_turn_count: int = 0
    input_tokens: int = 0
    cached_tokens: int = 0
    uncached_tokens: int = 0
    output_tokens: int = 0
    request_count: int = 0
    max_context_tokens: int = 0
    max_output_tokens: int = 0
    prompt_limit_tokens: int | None = None
    output_limit_tokens: int | None = None
    limit_violation_count: int = 0
    limit_penalty_cost: float = 0.0
    base_cost: float = 0.0
    recovery_cost: float = 0.0
    expected_recovery_input_tokens: float = 0.0
    expected_recovery_output_tokens: float = 0.0
    tool_raw_tokens: int = 0
    tool_visible_tokens: int = 0
    tool_structured_compression_events: int = 0
    llm_compaction_input_tokens: int = 0
    llm_compaction_output_tokens: int = 0
    llm_compaction_cost: float = 0.0
    llm_compaction_events: int = 0
    llm_compacted_items: int = 0
    total_cost: float = 0.0
    latency_ms: float = 0.0
    failure_turn: int | None = None
    failure_request_kind: str = ""
    expected_extra_tool_calls: float = 0.0
    expected_repeat_recovery_tool_calls: float = 0.0
    expected_failure_rate: float = 0.0
    answerability_score: float = 1.0
    policy_description: str = ""

    def add_request(
        self,
        request: RequestAccounting,
        limits: LimitConfig,
        config: SimulationConfig,
    ) -> bool:
        previous_limit_violations = self.limit_violation_count
        self.input_tokens += request.input_tokens
        self.cached_tokens += request.cached_tokens
        self.uncached_tokens += request.uncached_tokens
        self.output_tokens += request.output_tokens
        self.request_count += 1
        self.max_context_tokens = max(self.max_context_tokens, request.input_tokens)
        self.max_output_tokens = max(self.max_output_tokens, request.output_tokens)
        self.prompt_limit_tokens = limits.prompt_limit_tokens
        self.output_limit_tokens = limits.output_tokens
        prompt_limit = limits.prompt_limit_tokens
        if prompt_limit is not None and request.input_tokens > prompt_limit:
            self.limit_violation_count += 1
            self.limit_penalty_cost += config.failure_penalty_cost
        if limits.output_tokens is not None and request.output_tokens > limits.output_tokens:
            self.limit_violation_count += 1
            self.limit_penalty_cost += config.failure_penalty_cost
        self.base_cost += request.cost
        self.latency_ms += request.latency_ms
        return self.limit_violation_count == previous_limit_violations

    @property
    def cache_hit_ratio(self) -> float:
        if self.input_tokens == 0:
            return 0.0
        return self.cached_tokens / self.input_tokens

    @property
    def expected_total_tokens(self) -> float:
        return (
            self.input_tokens
            + self.output_tokens
            + self.expected_recovery_input_tokens
            + self.expected_recovery_output_tokens
            + self.llm_compaction_input_tokens
            + self.llm_compaction_output_tokens
        )

    @property
    def tool_structured_compression_saved_tokens(self) -> int:
        return max(0, self.tool_raw_tokens - self.tool_visible_tokens)


POLICIES = [
    PolicySpec(
        name="immediate_summary",
        description=(
            "Every tool result enters context as a compact summary plus a recovery handle from "
            "its first appearance. This maximizes prompt-prefix stability because raw tool output "
            "is never written into history, but later exact-detail questions may require a recovery read/query."
        ),
        initial_tool_mode="summary",
        pressure_tool_mode="summary",
        threshold_tool_projection=False,
    ),
    PolicySpec(
        name="loop_boundary_summary",
        description=(
            "Tool results stay raw inside the active Agent Loop, so follow-up tool requests in "
            "the same loop can compare exact files/logs. When the loop finishes, the stored "
            "history is rewritten to tool summary+handle plus final assistant content for the next user turn."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="summary",
        threshold_tool_projection=False,
        history_tool_mode="summary",
        current_loop_threshold_ratio=1.0,
    ),
    PolicySpec(
        name="loop_boundary_budgeted",
        description=(
            "Tool results stay raw inside the active Agent Loop while the prompt is comfortably "
            "below the threshold. Under current-loop pressure, raw outputs are reduced to a "
            "per-loop exact-evidence budget before the request is sent; completed history is still stored as summary+handle."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="budgeted",
        threshold_tool_projection=False,
        exact_budget_tokens=24_000,
        history_tool_mode="summary",
        current_loop_threshold_ratio=0.80,
    ),
    PolicySpec(
        name="loop_boundary_evidence_summary",
        description=(
            "Hybrid evidence-boundary strategy: tool results stay raw inside the active Agent "
            "Loop; if that loop approaches the prompt budget, working evidence is reduced to a "
            "current-loop exact-excerpt budget while logs/search results become summary+handle. "
            "When the loop finishes, stored history keeps only a smaller exact-evidence budget "
            "plus summaries, handles, and final assistant content."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="budgeted",
        threshold_tool_projection=False,
        exact_budget_tokens=24_000,
        pressure_exact_budget_tokens=24_000,
        history_tool_mode="budgeted",
        history_exact_budget_tokens=6_000,
        current_loop_threshold_ratio=0.80,
    ),
    PolicySpec(
        name="duckagent_recoverable_boundary",
        description=(
            "Candidate duckagent runtime strategy: the active Agent Loop stays raw by default. "
            "Under current-loop pressure, read_file keeps exact excerpts within a budget and "
            "falls back to path/offset/limit/next_offset handles; process/search results keep "
            "summary plus cursor/query handles. Completed-loop history uses the same recoverable "
            "projection with a smaller exact-evidence budget before any LLM history compaction."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=False,
        exact_budget_tokens=24_000,
        pressure_exact_budget_tokens=24_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=6_000,
        current_loop_threshold_ratio=0.80,
    ),
    PolicySpec(
        name="duckagent_recoverable_decay",
        description=(
            "Candidate duckagent runtime strategy with age decay: current loop and completed "
            "loop handling match duckagent_recoverable_boundary, but when old history creates "
            "prompt pressure, older exact file evidence is further downgraded to recoverable "
            "handles before LLM history compaction. This tests whether exact evidence should "
            "live only in recent completed loops."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        exact_budget_tokens=24_000,
        pressure_exact_budget_tokens=24_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=6_000,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=0.80,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_lean",
        description=(
            "Lean duckagent recoverable-decay variant. The active Agent Loop still starts raw, "
            "but completed-loop history stores read_file evidence as recoverable handles without "
            "inline exact excerpts. This tests whether assistant final content plus handles are "
            "enough for most older turns."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=18_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=0,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=0.80,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_balanced",
        description=(
            "Balanced duckagent recoverable-decay variant. It uses a smaller current-loop "
            "pressure exact budget than the baseline and keeps only a small completed-history "
            "exact-evidence budget before old-history decay. This explores the middle ground "
            "between lean handles-only history and the fixed 6K history budget."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=18_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=2_000,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=0.80,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_tight",
        description=(
            "Tighter recoverable-decay variant derived from balanced. It keeps the same "
            "active-loop and old-history mechanics, but lowers current-loop exact evidence "
            "to 15K tokens and completed-history exact evidence to 1.5K tokens. This tests "
            "whether balanced can be beaten by trimming inline evidence without losing the "
            "recoverable handles that keep validation stable."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=15_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=1_500,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=0.80,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_late_current",
        description=(
            "Balanced recoverable-decay history with a later active-loop pressure point. "
            "Completed history keeps the balanced recoverable 2K exact budget, while the "
            "current Agent Loop can remain raw until the hard prompt limit. This tests "
            "whether loop-boundary raw evidence helps without switching old history to pure summaries."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=18_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=2_000,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=1.0,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_late_tight",
        description=(
            "Hybrid of tight budgets and late active-loop pressure. It keeps current tools raw "
            "longer like loop-boundary strategies, then falls back to recoverable projection "
            "with 15K current exact evidence and 1.5K completed-history exact evidence."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=15_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=1_500,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=1.0,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_tight_90",
        description=(
            "Tight recoverable-decay variant with a 90% active-loop pressure guard. It is "
            "designed as a middle ground between balanced's 80% current-loop guard and "
            "late_tight's hard-limit guard, aiming to preserve more in-loop raw evidence "
            "without forcing small-context models into repeated current-loop compaction."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=15_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=1_500,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=0.90,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_tight_95",
        description=(
            "Tight recoverable-decay variant with a 95% active-loop pressure guard. This "
            "tests whether most of late_tight's savings come from delaying current-loop "
            "projection slightly below the hard prompt limit."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=15_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=1_500,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=0.95,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_guarded_late",
        description=(
            "Prompt-window guarded recoverable-decay variant. It keeps balanced's 18K current "
            "exact-evidence budget and 2K completed-history exact-evidence budget, but delays "
            "active-loop pressure to 90% only when the model prompt window is at least 200K tokens. "
            "Below that guard it falls back to balanced's 80% active-loop pressure point."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=18_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=2_000,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=0.90,
        current_loop_threshold_fallback_ratio=0.80,
        current_loop_threshold_min_prompt_tokens=200_000,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
        recovery_mode="minimal_pinned",
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_guarded_mid",
        description=(
            "Prompt-window guarded recoverable-decay variant with a milder 85% active-loop "
            "pressure point for 200K+ prompt windows. It preserves balanced's exact-evidence "
            "budgets and falls back to balanced's 80% guard below 200K, aiming to reduce the "
            "LLM-compaction churn seen by 260K-class models while keeping most large-window savings. "
            "Recovery uses minimal pinned evidence instead of repeated full-file raw reads."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=18_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=2_000,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=0.85,
        current_loop_threshold_fallback_ratio=0.80,
        current_loop_threshold_min_prompt_tokens=200_000,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
        recovery_mode="minimal_pinned",
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_guarded_mid_naive_recovery",
        description=(
            "Control variant for recovery-oscillation tests. Context projection matches "
            "guarded_mid, but missing exact/raw evidence is recovered by repeatedly re-reading "
            "the full raw file when that raw result would be compressed again. This models the "
            "bad read-compact-read loop that runtime safeguards must avoid."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=18_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=2_000,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=0.85,
        current_loop_threshold_fallback_ratio=0.80,
        current_loop_threshold_min_prompt_tokens=200_000,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
        recovery_mode="naive_raw_reread",
        recovery_repeat_limit=3,
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_guarded_late_tight",
        description=(
            "Aggressive prompt-window guarded variant. It uses the same 200K guard and 90% "
            "active-loop pressure point as guarded_late, but trims exact-evidence budgets to "
            "15K current and 1.5K completed history. This tests whether the late-current gain "
            "and tight evidence budget can combine without the 128K hard-limit churn seen in "
            "unconditional late_tight."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=15_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=1_500,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=0.90,
        current_loop_threshold_fallback_ratio=0.80,
        current_loop_threshold_min_prompt_tokens=200_000,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
        recovery_mode="minimal_pinned",
    ),
    PolicySpec(
        name="duckagent_summary_history_recoverable_current",
        description=(
            "Loop-boundary-style compact history with recoverable active-loop pressure. "
            "Completed loops store only summary+handle like loop_boundary_summary, but if "
            "the active Agent Loop approaches the prompt budget it keeps recoverable exact "
            "file evidence instead of dropping directly to summary."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=False,
        pressure_exact_budget_tokens=18_000,
        history_tool_mode="summary",
        current_loop_threshold_ratio=0.80,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_adaptive",
        description=(
            "Adaptive duckagent recoverable-decay variant. It starts from the balanced "
            "strategy, but caps current-loop and completed-history exact-evidence budgets by "
            "a ratio of the model prompt limit. Large-context models keep the balanced caps, "
            "while smaller-context models carry less inline exact evidence before falling "
            "back to recoverable handles."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=18_000,
        pressure_exact_budget_ratio=0.026,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=2_000,
        history_exact_budget_ratio=0.0065,
        threshold_exact_budget_tokens=0,
        threshold_exact_budget_ratio=0.0,
        current_loop_threshold_ratio=0.80,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_adaptive_guarded",
        description=(
            "Guarded adaptive duckagent recoverable-decay variant. It uses the adaptive "
            "model-relative budget caps, but adds semantic floors so smaller-context models "
            "do not collapse exact evidence too aggressively. This tests whether adaptive "
            "budgeting can improve cost without the recovery spike seen in pure adaptive mode."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_tokens=18_000,
        pressure_exact_budget_ratio=0.026,
        pressure_exact_budget_floor_tokens=12_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=2_000,
        history_exact_budget_ratio=0.0065,
        history_exact_budget_floor_tokens=1_500,
        threshold_exact_budget_tokens=0,
        threshold_exact_budget_ratio=0.0,
        current_loop_threshold_ratio=0.80,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_recent2",
        description=(
            "Recent-window duckagent recoverable-decay variant. It matches "
            "duckagent_recoverable_decay, but protects the two most recent completed loops "
            "from old-history decay and grouped LLM compaction unless the hard model limit "
            "requires it. This tests whether keeping one extra recent exact-evidence window "
            "reduces recovery reads enough to justify the larger prompt."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        exact_budget_tokens=24_000,
        pressure_exact_budget_tokens=24_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=6_000,
        threshold_exact_budget_tokens=0,
        current_loop_threshold_ratio=0.80,
        keep_recent_loops_on_snapshot=2,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_soft",
        description=(
            "Soft-decay duckagent variant. It keeps current-loop behavior from "
            "duckagent_recoverable_decay, but old history under pressure may retain a small "
            "exact-evidence budget instead of decaying all older read_file evidence to handles. "
            "This tests whether a tiny old-history exact cache buys enough recovery reduction."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        exact_budget_tokens=24_000,
        pressure_exact_budget_tokens=24_000,
        history_tool_mode="recoverable",
        history_exact_budget_tokens=6_000,
        threshold_exact_budget_tokens=2_000,
        current_loop_threshold_ratio=0.80,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="duckagent_recoverable_decay_relative",
        description=(
            "Model-relative duckagent recoverable-decay variant. Current-loop and completed-loop "
            "exact-evidence budgets are derived from the model prompt limit instead of fixed "
            "token counts, so small-context models carry less inline evidence while large-context "
            "models behave close to the fixed-budget baseline."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="recoverable",
        threshold_tool_projection=True,
        pressure_exact_budget_ratio=0.026,
        history_tool_mode="recoverable",
        history_exact_budget_ratio=0.0065,
        threshold_exact_budget_ratio=0.0,
        current_loop_threshold_ratio=0.80,
        reducible_tool_modes=("raw", "budgeted", "recoverable"),
    ),
    PolicySpec(
        name="adaptive_first",
        description=(
            "Every tool result is projected immediately by capability semantics: exact excerpts "
            "for working code evidence, summary+handle for logs/search/reference outputs. This "
            "spends more context than immediate_summary to reduce expected recovery reads."
        ),
        initial_tool_mode="policy",
        pressure_tool_mode="policy",
        threshold_tool_projection=False,
    ),
    PolicySpec(
        name="raw_snapshot",
        description=(
            "Tool results stay raw while the prompt fits. When projected context reaches the "
            "compression threshold, the oldest completed loops are rewritten by LLM compaction. "
            "This preserves maximum evidence early, but raw history makes requests larger and compaction cost higher."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="raw",
        threshold_tool_projection=False,
    ),
    PolicySpec(
        name="pressure_summary",
        description=(
            "Tool results enter as raw output until prompt pressure appears. Before LLM history "
            "compaction, older raw tool results are downgraded to summary+handle. This delays "
            "compression but still removes old raw evidence before rewriting larger history ranges."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="summary",
        threshold_tool_projection=True,
    ),
    PolicySpec(
        name="pressure_adaptive",
        description=(
            "Tool results enter as raw output until prompt pressure appears. Older raw results "
            "are then re-projected with the capability policy, keeping exact excerpts for working "
            "evidence and summaries for recoverable outputs before LLM compaction is used."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="policy",
        threshold_tool_projection=True,
    ),
    PolicySpec(
        name="evidence_budget",
        description=(
            "Tool results enter as raw output until prompt pressure appears. Older raw results "
            "are then projected with a per-loop exact-evidence budget, so only the most useful "
            "working evidence stays exact while the rest becomes summary+handle."
        ),
        initial_tool_mode="raw",
        pressure_tool_mode="budgeted",
        threshold_tool_projection=True,
        exact_budget_tokens=6_000,
    ),
    PolicySpec(
        name="early_windowed_prune",
        description=(
            "Source-informed early windowed pruning: tool caps/persistence are applied before "
            "history growth, then old tool outputs are pruned and middle history is LLM-compacted "
            "around 50% context. This keeps max prompt size low but rewrites history more often, "
            "which can reduce prompt-cache hit rate."
        ),
        initial_tool_mode="hermes_tool",
        pressure_tool_mode="summary",
        threshold_tool_projection=True,
        context_threshold_ratio=0.50,
        context_target_ratio=0.10,
        keep_recent_loops_on_snapshot=2,
        reducible_tool_modes=("raw", "hermes_tool"),
    ),
    PolicySpec(
        name="late_guarded_truncation",
        description=(
            "Source-informed late guarded truncation: live tool results are capped when they "
            "enter context, but history is allowed to grow until a high preflight guard near "
            "90% context. It compacts less eagerly, yet each request can carry a larger prompt."
        ),
        initial_tool_mode="openclaw_live",
        pressure_tool_mode="summary",
        threshold_tool_projection=True,
        context_threshold_ratio=0.90,
        context_target_ratio=0.50,
        keep_recent_loops_on_snapshot=2,
        reducible_tool_modes=("raw", "openclaw_live"),
    ),
]


def load_json(path: Path) -> dict[str, Any]:
    with path.open("r", encoding="utf-8") as handle:
        return json.load(handle)


def load_pricing(path: Path) -> tuple[PricingConfig, CacheConfig, LimitConfig, SimulationConfig]:
    raw = load_json(path)
    return (
        PricingConfig(**raw["pricing"]),
        CacheConfig(**raw["cache"]),
        LimitConfig(**raw.get("limits", {})),
        SimulationConfig(**raw["simulation"]),
    )


def parse_workloads(path: Path) -> list[Workload]:
    raw = load_json(path)
    workloads = []
    for workload_raw in raw["workloads"]:
        loops = []
        for loop_raw in workload_raw["loops"]:
            tools = [
                ToolCall(
                    name=tool["name"],
                    capability=tool["capability"],
                    purpose_tokens=int(tool["purpose_tokens"]),
                    args_tokens=int(tool["args_tokens"]),
                    outputs={key: int(value) for key, value in tool["outputs"].items()},
                    preserve=tool.get("preserve", "none"),
                    recovery_tokens={
                        key: int(value)
                        for key, value in tool.get("recovery_tokens", {}).items()
                    },
                )
                for tool in loop_raw.get("tool_calls", [])
            ]
            loops.append(
                BenchLoop(
                    name=loop_raw["name"],
                    user_tokens=int(loop_raw["user_tokens"]),
                    assistant_final_tokens=int(loop_raw["assistant_final_tokens"]),
                    tool_calls=tools,
                    dependency_probabilities=normalize_dependency_probabilities(
                        loop_raw.get("dependency_probabilities", {})
                    ),
                    final_dependency_probabilities=normalize_dependency_probabilities(
                        loop_raw.get("final_dependency_probabilities", {})
                    ),
                    raw_dependency_events=[
                        parse_raw_dependency_event(event)
                        for event in loop_raw.get("raw_dependency_events", [])
                    ],
                )
            )
        workloads.append(
            Workload(
                name=workload_raw["name"],
                description=workload_raw.get("description", ""),
                loops=loops,
            )
        )
    return workloads


def normalize_dependency_probabilities(raw: dict[str, float]) -> dict[str, float]:
    dependencies = {name: float(raw.get(name, 0.0)) for name in DEPENDENCY_TYPES}
    total = sum(dependencies.values())
    if total <= 0:
        dependencies["none"] = 1.0
        return dependencies
    return {name: value / total for name, value in dependencies.items()}


def parse_raw_dependency_event(raw: dict[str, Any]) -> RawDependencyEvent:
    dependency = str(raw.get("dependency", "exact_evidence"))
    if dependency not in DEPENDENCY_TYPES:
        raise ValueError(f"unknown raw dependency {dependency!r}")
    offset = int(raw.get("source_loop_offset", 1))
    if offset < 1:
        raise ValueError("raw_dependency_events.source_loop_offset must be >= 1")
    recovery_tokens = raw.get("recovery_tokens")
    return RawDependencyEvent(
        source_loop_offset=offset,
        dependency=dependency,
        probability=max(0.0, min(1.0, float(raw.get("probability", 1.0)))),
        recovery_tokens=None if recovery_tokens is None else int(recovery_tokens),
        label=str(raw.get("label", "")),
    )


def make_tool(
    name: str,
    capability: str,
    raw_tokens: int,
    *,
    exact_excerpt_tokens: int,
    summary_tokens: int,
    handle_tokens: int,
    preserve: str,
    purpose_tokens: int = 20,
    args_tokens: int = 64,
    recovery_tokens: dict[str, int] | None = None,
) -> ToolCall:
    return ToolCall(
        name=name,
        capability=capability,
        purpose_tokens=purpose_tokens,
        args_tokens=args_tokens,
        outputs={
            "raw": raw_tokens,
            "exact_excerpt": exact_excerpt_tokens,
            "summary": summary_tokens,
            "handle": handle_tokens,
        },
        preserve=preserve,
        recovery_tokens=recovery_tokens or {},
    )


def raw_dependency_events_for_offsets(
    loop_index: int, events: list[tuple[int, str, float]]
) -> list[RawDependencyEvent]:
    return [
        RawDependencyEvent(
            source_loop_offset=offset,
            dependency=dependency,
            probability=probability,
        )
        for offset, dependency, probability in events
        if loop_index >= offset and probability > 0
    ]


def stress_workloads(scale: int = 1) -> list[Workload]:
    scale = max(1, scale)
    return [
        stress_long_code_development(scale),
        stress_many_tools_per_loop(scale),
        stress_process_log_marathon(scale),
        stress_fullstack_refactor(scale),
    ]


def validation_workloads(scale: int = 1) -> list[Workload]:
    scale = max(1, scale)
    return [
        validation_chat_mixed_work(scale),
        validation_exact_revisit_work(scale),
        validation_log_recovery_work(scale),
        validation_wide_current_loop_work(scale),
        validation_recovery_oscillation_work(scale),
    ]


def long_run_workload(turns: int) -> Workload:
    turns = max(1, turns)
    scale = max(1, math.ceil(turns / 98))
    loops: list[BenchLoop] = []
    for workload in stress_workloads(scale):
        loops.extend(workload.loops)
    return Workload(
        name=f"long_run_{turns}_turns",
        description="Exactly N user turns assembled from deterministic stress patterns.",
        loops=loops[:turns],
    )


def validation_chat_mixed_work(scale: int) -> Workload:
    loops: list[BenchLoop] = []
    for loop_index in range(36 * scale):
        if loop_index % 5 in (0, 3):
            tools = [
                make_tool(
                    f"read small focused file {loop_index}",
                    "read_file",
                    2_400 + (loop_index % 4) * 700,
                    exact_excerpt_tokens=700,
                    summary_tokens=180,
                    handle_tokens=70,
                    preserve="working_evidence",
                    recovery_tokens={"exact_evidence": 800, "local_detail": 500},
                )
            ]
            dependency_probabilities = {
                "none": 0.42,
                "global_conclusion": 0.28,
                "local_detail": 0.12,
                "exact_evidence": 0.18,
                "raw_content": 0.03,
            }
        else:
            tools = []
            dependency_probabilities = {"none": 0.55, "global_conclusion": 0.45}
        loops.append(
            BenchLoop(
                name=f"chat_mixed_turn_{loop_index + 1}",
                user_tokens=80 + (loop_index % 5) * 24,
                assistant_final_tokens=460 + (loop_index % 4) * 120,
                tool_calls=tools,
                dependency_probabilities=normalize_dependency_probabilities(
                    dependency_probabilities
                ),
                raw_dependency_events=raw_dependency_events_for_offsets(
                    loop_index,
                    [
                        (2, "exact_evidence", 0.08),
                        (5, "local_detail", 0.06),
                    ],
                ),
            )
        )
    return Workload(
        name="validation_chat_mixed_work",
        description="Chat-heavy work with occasional small file reads.",
        loops=loops,
    )


def validation_exact_revisit_work(scale: int) -> Workload:
    loops: list[BenchLoop] = []
    for loop_index in range(20 * scale):
        tools = [
            make_tool(
                f"read revisited source {loop_index}-{file_index}",
                "read_file",
                5_600 + ((loop_index * 719 + file_index * 977) % 5_400),
                exact_excerpt_tokens=1_300 + file_index * 220,
                summary_tokens=240,
                handle_tokens=84,
                preserve="working_evidence",
                recovery_tokens={"exact_evidence": 1_600, "local_detail": 900},
            )
            for file_index in range(3)
        ]
        loops.append(
            BenchLoop(
                name=f"exact_revisit_turn_{loop_index + 1}",
                user_tokens=130 + (loop_index % 4) * 30,
                assistant_final_tokens=620 + (loop_index % 3) * 130,
                tool_calls=tools,
                dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.16,
                        "global_conclusion": 0.18,
                        "local_detail": 0.16,
                        "exact_evidence": 0.5,
                        "raw_content": 0.12,
                    }
                ),
                final_dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.08,
                        "global_conclusion": 0.14,
                        "local_detail": 0.18,
                        "exact_evidence": 0.52,
                        "raw_content": 0.08,
                    }
                ),
                raw_dependency_events=raw_dependency_events_for_offsets(
                    loop_index,
                    [
                        (1, "exact_evidence", 0.32),
                        (4, "exact_evidence", 0.24),
                        (7, "raw_content", 0.10),
                    ],
                ),
            )
        )
    return Workload(
        name="validation_exact_revisit_work",
        description="Exact-code revisit workload with repeated references to older files.",
        loops=loops,
    )


def validation_log_recovery_work(scale: int) -> Workload:
    loops: list[BenchLoop] = []
    for loop_index in range(30 * scale):
        tools = [
            make_tool(
                f"read log validation chunk {loop_index}",
                "process_read",
                14_000 + (loop_index % 9) * 5_500,
                exact_excerpt_tokens=1_000,
                summary_tokens=520,
                handle_tokens=150,
                preserve="reference",
                recovery_tokens={"local_detail": 2_100, "continuation_handle": 150},
            ),
            make_tool(
                f"query log validation errors {loop_index}",
                "rg",
                2_800 + (loop_index % 4) * 900,
                exact_excerpt_tokens=700,
                summary_tokens=260,
                handle_tokens=90,
                preserve="reference",
                recovery_tokens={"local_detail": 700},
            ),
        ]
        loops.append(
            BenchLoop(
                name=f"log_recovery_turn_{loop_index + 1}",
                user_tokens=90 + (loop_index % 6) * 16,
                assistant_final_tokens=420 + (loop_index % 5) * 80,
                tool_calls=tools,
                dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.16,
                        "global_conclusion": 0.28,
                        "continuation_handle": 0.18,
                        "local_detail": 0.34,
                        "raw_content": 0.05,
                    }
                ),
                final_dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.12,
                        "global_conclusion": 0.30,
                        "continuation_handle": 0.16,
                        "local_detail": 0.36,
                        "raw_content": 0.06,
                    }
                ),
                raw_dependency_events=raw_dependency_events_for_offsets(
                    loop_index,
                    [
                        (1, "local_detail", 0.28),
                        (6, "local_detail", 0.14),
                        (10, "raw_content", 0.05),
                    ],
                ),
            )
        )
    return Workload(
        name="validation_log_recovery_work",
        description="Log-heavy validation workload where handles and query recovery should dominate.",
        loops=loops,
    )


def validation_wide_current_loop_work(scale: int) -> Workload:
    loops: list[BenchLoop] = []
    for loop_index in range(8 * scale):
        tools: list[ToolCall] = []
        for tool_index in range(36):
            is_file = tool_index % 3 != 2
            tools.append(
                make_tool(
                    f"{'read validation file' if is_file else 'search validation refs'} {loop_index}-{tool_index}",
                    "read_file" if is_file else "rg",
                    3_800 + ((loop_index * 487 + tool_index * 613) % 7_800),
                    exact_excerpt_tokens=780 + (tool_index % 4) * 120,
                    summary_tokens=220 + (tool_index % 3) * 40,
                    handle_tokens=82,
                    preserve="working_evidence" if is_file else "reference",
                    purpose_tokens=18,
                    args_tokens=44,
                    recovery_tokens={"exact_evidence": 1_100, "local_detail": 760},
                )
            )
        loops.append(
            BenchLoop(
                name=f"wide_validation_turn_{loop_index + 1}",
                user_tokens=180 + (loop_index % 4) * 30,
                assistant_final_tokens=980 + (loop_index % 3) * 160,
                tool_calls=tools,
                dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.14,
                        "global_conclusion": 0.18,
                        "continuation_handle": 0.05,
                        "local_detail": 0.22,
                        "exact_evidence": 0.41,
                        "raw_content": 0.08,
                    }
                ),
                final_dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.08,
                        "global_conclusion": 0.18,
                        "local_detail": 0.20,
                        "exact_evidence": 0.48,
                        "raw_content": 0.06,
                    }
                ),
                raw_dependency_events=raw_dependency_events_for_offsets(
                    loop_index,
                    [
                        (1, "exact_evidence", 0.26),
                        (2, "local_detail", 0.18),
                        (3, "raw_content", 0.08),
                    ],
                ),
            )
        )
    return Workload(
        name="validation_wide_current_loop_work",
        description="Very wide current loops that stress pre-send current-loop projection.",
        loops=loops,
    )


def validation_recovery_oscillation_work(scale: int) -> Workload:
    loops: list[BenchLoop] = []
    for loop_index in range(10 * scale):
        loops.append(
            BenchLoop(
                name=f"large_file_revisit_{loop_index + 1}",
                user_tokens=150,
                assistant_final_tokens=420,
                tool_calls=[
                    make_tool(
                        f"read huge_module_{loop_index + 1}.rs",
                        "read_file",
                        300_000,
                        exact_excerpt_tokens=6_000,
                        summary_tokens=520,
                        handle_tokens=120,
                        preserve="working_evidence",
                        purpose_tokens=48,
                        args_tokens=58,
                        recovery_tokens={
                            "exact_evidence": 6_200,
                            "local_detail": 2_400,
                        },
                    )
                ],
                dependency_probabilities=normalize_dependency_probabilities({"none": 1.0}),
                final_dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.10,
                        "exact_evidence": 0.20,
                        "raw_content": 0.70,
                    }
                ),
            )
        )
    return Workload(
        name="validation_recovery_oscillation_work",
        description=(
            "Huge-file revisit workload that exposes read-compact-read oscillation. "
            "The active loop cannot keep full raw file output, and the final answer may still "
            "need exact/raw evidence. Robust recovery should use a range/query handle and pin "
            "minimal evidence instead of re-reading the full file repeatedly."
        ),
        loops=loops,
    )


def stress_long_code_development(scale: int) -> Workload:
    loops: list[BenchLoop] = []
    for loop_index in range(24 * scale):
        tools: list[ToolCall] = []
        for file_index in range(6):
            raw = 7_200 + ((loop_index * 997 + file_index * 1699) % 10_800)
            tools.append(
                make_tool(
                    f"read core file {loop_index}-{file_index}",
                    "read_file",
                    raw,
                    exact_excerpt_tokens=1_100 + (file_index % 3) * 260,
                    summary_tokens=260,
                    handle_tokens=88,
                    preserve="working_evidence",
                    args_tokens=56,
                    recovery_tokens={"exact_evidence": 1_800, "local_detail": 900},
                )
            )
        tools.append(
            make_tool(
                f"search references {loop_index}",
                "rg",
                4_800 + (loop_index % 5) * 1_250,
                exact_excerpt_tokens=900,
                summary_tokens=320,
                handle_tokens=96,
                preserve="reference",
                recovery_tokens={"local_detail": 700},
            )
        )
        tools.append(
            make_tool(
                f"run tests {loop_index}",
                "process_start",
                9_500 + (loop_index % 6) * 3_200,
                exact_excerpt_tokens=1_000,
                summary_tokens=520,
                handle_tokens=120,
                preserve="reference",
                recovery_tokens={"local_detail": 1_200},
            )
        )
        if loop_index % 3 == 2:
            tools.append(
                make_tool(
                    f"read failing test log {loop_index}",
                    "process_read",
                    24_000 + (loop_index % 4) * 8_000,
                    exact_excerpt_tokens=1_400,
                    summary_tokens=620,
                    handle_tokens=140,
                    preserve="reference",
                    recovery_tokens={"local_detail": 1_800},
                )
            )
        loops.append(
            BenchLoop(
                name=f"dev_turn_{loop_index + 1}",
                user_tokens=120 + (loop_index % 4) * 32,
                assistant_final_tokens=620 + (loop_index % 5) * 110,
                tool_calls=tools,
                dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.12,
                        "global_conclusion": 0.23,
                        "continuation_handle": 0.06,
                        "local_detail": 0.22,
                        "exact_evidence": 0.37,
                        "raw_content": 0.08,
                    }
                ),
                final_dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.1,
                        "global_conclusion": 0.18,
                        "local_detail": 0.17,
                        "exact_evidence": 0.5,
                        "raw_content": 0.05,
                    }
                ),
                raw_dependency_events=raw_dependency_events_for_offsets(
                    loop_index,
                    [
                        (1, "exact_evidence", 0.18),
                        (3, "local_detail", 0.12),
                        (6, "exact_evidence", 0.12),
                        (10, "raw_content", 0.08),
                    ],
                ),
            )
        )
    return Workload(
        name="stress_long_code_development",
        description="Long code development with repeated file reads and test logs.",
        loops=loops,
    )


def stress_many_tools_per_loop(scale: int) -> Workload:
    loops: list[BenchLoop] = []
    for loop_index in range(8 * scale):
        tools: list[ToolCall] = []
        for tool_index in range(28):
            is_file = tool_index % 4 != 3
            raw = 4_200 + ((loop_index * 379 + tool_index * 821) % 9_600)
            tools.append(
                make_tool(
                    f"{'read file' if is_file else 'search'} {loop_index}-{tool_index}",
                    "read_file" if is_file else "rg",
                    raw,
                    exact_excerpt_tokens=900 + (tool_index % 5) * 140,
                    summary_tokens=220 + (tool_index % 4) * 40,
                    handle_tokens=80,
                    preserve="working_evidence" if is_file else "reference",
                    purpose_tokens=18,
                    args_tokens=48,
                    recovery_tokens={"exact_evidence": 1_200, "local_detail": 850},
                )
            )
        loops.append(
            BenchLoop(
                name=f"wide_agent_loop_{loop_index + 1}",
                user_tokens=160 + (loop_index % 3) * 40,
                assistant_final_tokens=940 + (loop_index % 4) * 160,
                tool_calls=tools,
                dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.18,
                        "global_conclusion": 0.18,
                        "continuation_handle": 0.05,
                        "local_detail": 0.24,
                        "exact_evidence": 0.35,
                        "raw_content": 0.1,
                    }
                ),
                final_dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.08,
                        "global_conclusion": 0.16,
                        "local_detail": 0.18,
                        "exact_evidence": 0.5,
                        "raw_content": 0.08,
                    }
                ),
                raw_dependency_events=raw_dependency_events_for_offsets(
                    loop_index,
                    [
                        (1, "exact_evidence", 0.24),
                        (2, "local_detail", 0.16),
                        (3, "raw_content", 0.12),
                    ],
                ),
            )
        )
    return Workload(
        name="stress_many_tools_per_loop",
        description="Wide Agent Loops with many file/search tool calls.",
        loops=loops,
    )


def stress_process_log_marathon(scale: int) -> Workload:
    loops: list[BenchLoop] = []
    for loop_index in range(48 * scale):
        tools = [
            make_tool(
                f"read process log chunk {loop_index}",
                "process_read",
                22_000 + (loop_index % 8) * 7_500,
                exact_excerpt_tokens=1_200,
                summary_tokens=580,
                handle_tokens=140,
                preserve="reference",
                recovery_tokens={"local_detail": 2_400},
            ),
            make_tool(
                f"search latest error {loop_index}",
                "rg",
                3_500 + (loop_index % 6) * 900,
                exact_excerpt_tokens=760,
                summary_tokens=260,
                handle_tokens=88,
                preserve="reference",
                recovery_tokens={"local_detail": 650},
            ),
        ]
        if loop_index % 4 == 0:
            tools.append(
                make_tool(
                    f"read config file {loop_index}",
                    "read_file",
                    8_500 + (loop_index % 5) * 1_800,
                    exact_excerpt_tokens=1_150,
                    summary_tokens=280,
                    handle_tokens=88,
                    preserve="working_evidence",
                    recovery_tokens={"exact_evidence": 1_300, "local_detail": 800},
                )
            )
        loops.append(
            BenchLoop(
                name=f"log_turn_{loop_index + 1}",
                user_tokens=90 + (loop_index % 5) * 18,
                assistant_final_tokens=420 + (loop_index % 6) * 70,
                tool_calls=tools,
                dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.16,
                        "global_conclusion": 0.27,
                        "continuation_handle": 0.22,
                        "local_detail": 0.3,
                        "exact_evidence": 0.0,
                        "raw_content": 0.07,
                    }
                ),
                final_dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.12,
                        "global_conclusion": 0.26,
                        "continuation_handle": 0.12,
                        "local_detail": 0.42,
                        "raw_content": 0.08,
                    }
                ),
                raw_dependency_events=raw_dependency_events_for_offsets(
                    loop_index,
                    [
                        (1, "local_detail", 0.26),
                        (8, "local_detail", 0.1),
                        (12, "raw_content", 0.06),
                    ],
                ),
            )
        )
    return Workload(
        name="stress_process_log_marathon",
        description="Long process-log debugging with huge output chunks.",
        loops=loops,
    )


def stress_fullstack_refactor(scale: int) -> Workload:
    loops: list[BenchLoop] = []
    for loop_index in range(18 * scale):
        tools: list[ToolCall] = []
        for file_index in range(10):
            raw = 6_400 + ((loop_index * 541 + file_index * 1171) % 12_500)
            tools.append(
                make_tool(
                    f"read fullstack file {loop_index}-{file_index}",
                    "read_file",
                    raw,
                    exact_excerpt_tokens=1_050 + (file_index % 4) * 220,
                    summary_tokens=250,
                    handle_tokens=92,
                    preserve="working_evidence",
                    recovery_tokens={"exact_evidence": 1_600, "local_detail": 900},
                )
            )
        tools.extend(
            [
                make_tool(
                    f"run frontend tests {loop_index}",
                    "process_start",
                    12_000 + (loop_index % 5) * 4_000,
                    exact_excerpt_tokens=1_050,
                    summary_tokens=520,
                    handle_tokens=120,
                    preserve="reference",
                    recovery_tokens={"local_detail": 1_500},
                ),
                make_tool(
                    f"run backend tests {loop_index}",
                    "process_start",
                    15_000 + (loop_index % 7) * 3_600,
                    exact_excerpt_tokens=1_150,
                    summary_tokens=560,
                    handle_tokens=120,
                    preserve="reference",
                    recovery_tokens={"local_detail": 1_600},
                ),
            ]
        )
        loops.append(
            BenchLoop(
                name=f"refactor_turn_{loop_index + 1}",
                user_tokens=150 + (loop_index % 4) * 35,
                assistant_final_tokens=820 + (loop_index % 5) * 130,
                tool_calls=tools,
                dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.1,
                        "global_conclusion": 0.22,
                        "continuation_handle": 0.08,
                        "local_detail": 0.2,
                        "exact_evidence": 0.4,
                        "raw_content": 0.09,
                    }
                ),
                final_dependency_probabilities=normalize_dependency_probabilities(
                    {
                        "none": 0.08,
                        "global_conclusion": 0.14,
                        "local_detail": 0.16,
                        "exact_evidence": 0.56,
                        "raw_content": 0.06,
                    }
                ),
                raw_dependency_events=raw_dependency_events_for_offsets(
                    loop_index,
                    [
                        (1, "exact_evidence", 0.22),
                        (4, "local_detail", 0.12),
                        (7, "exact_evidence", 0.14),
                        (9, "raw_content", 0.08),
                    ],
                ),
            )
        )
    return Workload(
        name="stress_fullstack_refactor",
        description="Large refactor with many long files plus repeated test runs.",
        loops=loops,
    )


def tool_call_output_tokens(tool: ToolCall) -> int:
    return tool.purpose_tokens + tool.args_tokens + 28


def stable_token_count(config: SimulationConfig) -> int:
    return config.system_tokens + config.tool_schema_tokens + config.memory_catalog_tokens


def tool_summary_handle_tokens(tool: ToolCall) -> int:
    return min(tool.raw_tokens, tool.summary_tokens + tool.handle_tokens)


def tool_policy_tokens(tool: ToolCall) -> int:
    if tool.preserve == "working_evidence":
        return min(tool.raw_tokens, tool.exact_excerpt_tokens + tool.handle_tokens)
    if tool.preserve == "reference" or tool.capability.startswith("process_"):
        return tool_summary_handle_tokens(tool)
    return min(tool.raw_tokens, tool.summary_tokens)


def tool_hermes_tokens(tool: ToolCall) -> tuple[int, bool]:
    if tool.capability.startswith("process_"):
        return min(tool.raw_tokens, HERMES_PROCESS_OUTPUT_CAP_TOKENS), False
    if tool.capability == "read_file":
        return tool.raw_tokens, tool.preserve == "working_evidence"
    if tool.raw_tokens > HERMES_PERSIST_THRESHOLD_TOKENS:
        return min(tool.raw_tokens, HERMES_PREVIEW_TOKENS + tool.handle_tokens), False
    return tool.raw_tokens, tool.preserve == "working_evidence"


def tool_openclaw_live_tokens(tool: ToolCall) -> tuple[int, bool]:
    tokens = min(tool.raw_tokens, OPENCLAW_LIVE_TOOL_RESULT_TOKENS)
    exact = tool.preserve == "working_evidence" and tokens >= tool.raw_tokens
    return tokens, exact


def tool_recoverable_tokens(
    tool: ToolCall,
    used_exact_tokens: int,
    exact_budget_tokens: int | None,
) -> tuple[int, bool]:
    if tool.capability == "read_file" and tool.preserve == "working_evidence":
        exact_tokens = min(tool.raw_tokens, tool.exact_excerpt_tokens + tool.handle_tokens)
        if exact_budget_tokens is None or used_exact_tokens + exact_tokens <= exact_budget_tokens:
            return exact_tokens, True
        return min(tool.raw_tokens, tool.handle_tokens), False
    if tool.capability.startswith("process_") or tool.preserve == "reference":
        return tool_summary_handle_tokens(tool), False
    return min(tool.raw_tokens, tool.summary_tokens + tool.handle_tokens), False


def projected_tool_tokens(
    tool: ToolCall,
    mode: str,
    used_exact_tokens: int,
    exact_budget_tokens: int | None,
) -> tuple[int, bool]:
    if mode not in TOOL_MODES:
        raise ValueError(f"unknown tool projection mode: {mode}")
    if mode == "raw":
        return tool.raw_tokens, tool.preserve == "working_evidence"
    if mode == "handle":
        return min(tool.raw_tokens, tool.handle_tokens), False
    if mode == "marker":
        return min(tool.raw_tokens, 8), False
    if mode == "summary":
        return tool_summary_handle_tokens(tool), False
    if mode == "policy":
        exact = tool.preserve == "working_evidence"
        return tool_policy_tokens(tool), exact
    if mode == "recoverable":
        return tool_recoverable_tokens(tool, used_exact_tokens, exact_budget_tokens)
    if mode == "hermes_tool":
        return tool_hermes_tokens(tool)
    if mode == "openclaw_live":
        return tool_openclaw_live_tokens(tool)
    if tool.preserve != "working_evidence":
        return tool_summary_handle_tokens(tool), False
    exact_tokens = min(tool.raw_tokens, tool.exact_excerpt_tokens + tool.handle_tokens)
    if exact_budget_tokens is None or used_exact_tokens + exact_tokens <= exact_budget_tokens:
        return exact_tokens, True
    return tool_summary_handle_tokens(tool), False


def project_tool_list(
    tools: list[ToolCall],
    mode: str,
    exact_budget_tokens: int | None,
) -> list[ToolProjection]:
    projections: list[ToolProjection] = []
    used_exact_tokens = 0
    for tool in tools:
        tokens, exact = projected_tool_tokens(
            tool,
            mode,
            used_exact_tokens,
            exact_budget_tokens,
        )
        if exact:
            used_exact_tokens += tokens
        projections.append(ToolProjection(tool=tool, mode=mode, tokens=tokens, exact_retained=exact))
    return projections


def policy_exact_budget(
    policy: PolicySpec,
    phase: str,
    limits: LimitConfig | None = None,
) -> int | None:
    if phase == "pressure" and policy.pressure_exact_budget_tokens is not None:
        return exact_budget_with_optional_ratio(
            policy.pressure_exact_budget_tokens,
            policy.pressure_exact_budget_ratio,
            policy.pressure_exact_budget_floor_tokens,
            limits,
        )
    if phase == "history" and policy.history_exact_budget_tokens is not None:
        return exact_budget_with_optional_ratio(
            policy.history_exact_budget_tokens,
            policy.history_exact_budget_ratio,
            policy.history_exact_budget_floor_tokens,
            limits,
        )
    if phase == "threshold" and policy.threshold_exact_budget_tokens is not None:
        return exact_budget_with_optional_ratio(
            policy.threshold_exact_budget_tokens,
            policy.threshold_exact_budget_ratio,
            policy.threshold_exact_budget_floor_tokens,
            limits,
        )
    if phase == "pressure" and policy.pressure_exact_budget_ratio is not None:
        return exact_budget_from_ratio(limits, policy.pressure_exact_budget_ratio)
    if phase == "history" and policy.history_exact_budget_ratio is not None:
        return exact_budget_from_ratio(limits, policy.history_exact_budget_ratio)
    if phase == "threshold" and policy.threshold_exact_budget_ratio is not None:
        return exact_budget_from_ratio(limits, policy.threshold_exact_budget_ratio)
    return policy.exact_budget_tokens


def exact_budget_from_ratio(limits: LimitConfig | None, ratio_value: float) -> int | None:
    if limits is None or limits.prompt_limit_tokens is None:
        return None
    return max(0, int(limits.prompt_limit_tokens * ratio_value))


def exact_budget_with_optional_ratio(
    absolute_tokens: int,
    ratio_value: float | None,
    floor_tokens: int | None,
    limits: LimitConfig | None,
) -> int:
    ratio_tokens = (
        None if ratio_value is None else exact_budget_from_ratio(limits, ratio_value)
    )
    if ratio_tokens is None:
        return absolute_tokens
    bounded_ratio = ratio_tokens
    if floor_tokens is not None:
        bounded_ratio = max(floor_tokens, bounded_ratio)
    return min(absolute_tokens, bounded_ratio)


def loop_history_tokens(loop: BenchLoop, tools: list[ToolProjection]) -> int:
    total = loop.user_tokens + loop.assistant_final_tokens
    for projection in tools:
        total += tool_call_output_tokens(projection.tool) + projection.tokens
    return total


def loop_llm_snapshot_tokens(loop: BenchLoop, config: SimulationConfig) -> int:
    summary_body = loop.assistant_final_tokens
    for tool in loop.tool_calls:
        summary_body += min(tool.raw_tokens, tool.summary_tokens + tool.handle_tokens)
    return loop.user_tokens + config.loop_compaction_overhead_tokens + min(
        summary_body,
        config.compaction_output_tokens_per_loop,
    )


def effective_context_threshold(
    limits: LimitConfig,
    config: SimulationConfig,
    policy: PolicySpec | None = None,
) -> int:
    prompt_limit = limits.prompt_limit_tokens
    ratio_override = policy.context_threshold_ratio if policy else None
    ratio = ratio_override if ratio_override is not None else config.context_threshold_ratio
    if ratio is not None and prompt_limit is not None:
        return max(1, int(prompt_limit * ratio))
    return config.context_threshold_tokens


def effective_context_target(
    limits: LimitConfig,
    config: SimulationConfig,
    policy: PolicySpec | None = None,
) -> int:
    prompt_limit = limits.prompt_limit_tokens
    ratio_override = policy.context_target_ratio if policy else None
    ratio = ratio_override if ratio_override is not None else config.context_target_ratio
    if ratio is not None and prompt_limit is not None:
        return max(1, int(prompt_limit * ratio))
    return config.context_threshold_tokens


def effective_current_loop_threshold(
    limits: LimitConfig,
    config: SimulationConfig,
    policy: PolicySpec,
    default_threshold: int,
) -> int:
    prompt_limit = limits.prompt_limit_tokens
    if policy.current_loop_threshold_ratio is not None and prompt_limit is not None:
        ratio = policy.current_loop_threshold_ratio
        if (
            policy.current_loop_threshold_min_prompt_tokens is not None
            and prompt_limit < policy.current_loop_threshold_min_prompt_tokens
            and policy.current_loop_threshold_fallback_ratio is not None
        ):
            ratio = policy.current_loop_threshold_fallback_ratio
        return max(1, int(prompt_limit * ratio))
    return default_threshold


def billable_cached_tokens(common_tokens: int, cache: CacheConfig) -> int:
    if common_tokens < cache.min_tokens:
        return 0
    extra = common_tokens - cache.min_tokens
    return cache.min_tokens + (extra // cache.step_tokens) * cache.step_tokens


def account_fast_request(
    input_tokens: int,
    output_tokens: int,
    previous_input_tokens: int | None,
    common_prefix_cap_tokens: int | None,
    stable_tokens: int,
    pricing: PricingConfig,
    cache: CacheConfig,
) -> tuple[RequestAccounting, int]:
    common_tokens = 0 if previous_input_tokens is None else previous_input_tokens
    if common_prefix_cap_tokens is not None:
        common_tokens = min(common_tokens, max(0, common_prefix_cap_tokens))
    cached_tokens = min(billable_cached_tokens(common_tokens, cache), input_tokens)
    uncached_tokens = input_tokens - cached_tokens
    cost = (
        cached_tokens * pricing.cached_input_per_token
        + uncached_tokens * pricing.input_per_token
        + output_tokens * pricing.output_per_token
    )
    latency_ms = pricing.request_latency_ms + uncached_tokens * 0.02 + output_tokens * 0.03
    return (
        RequestAccounting(
            input_tokens=input_tokens,
            cached_tokens=cached_tokens,
            uncached_tokens=uncached_tokens,
            output_tokens=output_tokens,
            cost=cost,
            latency_ms=latency_ms,
        ),
        input_tokens,
    )


def cap_common_prefix(
    current_cap: int | None,
    candidate_cap: int | None,
) -> int | None:
    if candidate_cap is None:
        return current_cap
    if current_cap is None:
        return candidate_cap
    return min(current_cap, candidate_cap)


def record_tool_projection(metrics: SimulationMetrics, projection: ToolProjection) -> None:
    metrics.tool_raw_tokens += projection.tool.raw_tokens
    metrics.tool_visible_tokens += projection.tokens
    if projection.tokens < projection.tool.raw_tokens:
        projection.compression_counted = True
        metrics.tool_structured_compression_events += 1


def update_tool_projection(
    metrics: SimulationMetrics,
    projection: ToolProjection,
    mode: str,
    used_exact_tokens: int,
    exact_budget_tokens: int | None,
) -> int:
    old_tokens = projection.tokens
    new_tokens, exact = projected_tool_tokens(
        projection.tool,
        mode,
        used_exact_tokens,
        exact_budget_tokens,
    )
    if exact:
        used_exact_tokens += new_tokens
    if new_tokens >= old_tokens:
        return used_exact_tokens
    metrics.tool_visible_tokens += new_tokens - old_tokens
    if not projection.compression_counted and new_tokens < projection.tool.raw_tokens:
        projection.compression_counted = True
        metrics.tool_structured_compression_events += 1
    projection.mode = mode
    projection.tokens = new_tokens
    projection.exact_retained = exact
    return used_exact_tokens


def add_llm_compaction_accounting(
    metrics: SimulationMetrics,
    input_tokens: int,
    output_tokens: int,
    pricing: PricingConfig,
    *,
    compacted_items: int = 1,
) -> None:
    if input_tokens <= 0 and output_tokens <= 0:
        return
    input_tokens = max(0, input_tokens)
    output_tokens = max(0, output_tokens)
    metrics.llm_compaction_events += 1
    metrics.llm_compacted_items += max(1, compacted_items)
    metrics.llm_compaction_input_tokens += input_tokens
    metrics.llm_compaction_output_tokens += output_tokens
    metrics.llm_compaction_cost += (
        input_tokens * pricing.input_per_token
        + output_tokens * pricing.output_per_token
    )
    metrics.latency_ms += pricing.request_latency_ms + input_tokens * 0.02 + output_tokens * 0.03


def compact_current_loop(
    current: CurrentLoopProjection,
    metrics: SimulationMetrics,
    pricing: PricingConfig,
    config: SimulationConfig,
) -> int:
    before = current.tokens
    if before <= current.loop.user_tokens:
        return 0
    output_tokens = current.loop.user_tokens + config.loop_compaction_overhead_tokens + min(
        max(0, before - current.loop.user_tokens),
        config.compaction_output_tokens_per_loop,
    )
    current.compacted_prefix_tokens = output_tokens
    current.compacted_tool_count = len(current.tools)
    add_llm_compaction_accounting(
        metrics,
        config.snapshot_overhead_tokens + before,
        output_tokens,
        pricing,
    )
    return max(0, before - current.tokens)


def compress_current_loop_if_needed(
    current: CurrentLoopProjection,
    policy: PolicySpec,
    stable_tokens: int,
    history_tokens: int,
    threshold: int,
    prompt_limit: int | None,
    limits: LimitConfig,
    metrics: SimulationMetrics,
) -> bool:
    if current.compacted_prefix_tokens is not None and current.compacted_tool_count == len(current.tools):
        return False
    projected = stable_tokens + history_tokens + current.tokens
    limit = prompt_limit or threshold
    if projected <= min(threshold, limit):
        return False
    changed = False
    used_exact_tokens = 0
    for projection in current.tools:
        if projection.mode in policy.reducible_tool_modes:
            used_exact_tokens = update_tool_projection(
                metrics,
                projection,
                policy.pressure_tool_mode,
                used_exact_tokens,
                policy_exact_budget(policy, "pressure", limits),
            )
            changed = True
    projected = stable_tokens + history_tokens + current.tokens
    if projected <= limit:
        return changed
    used_exact_tokens = 0
    for projection in current.tools:
        used_exact_tokens = update_tool_projection(
            metrics,
            projection,
            "summary",
            used_exact_tokens,
            None,
        )
        changed = True
    projected = stable_tokens + history_tokens + current.tokens
    if projected <= limit:
        return changed
    used_exact_tokens = 0
    for projection in current.tools:
        used_exact_tokens = update_tool_projection(
            metrics,
            projection,
            "handle",
            used_exact_tokens,
            None,
        )
        changed = True
    projected = stable_tokens + history_tokens + current.tokens
    if projected <= limit:
        return changed
    used_exact_tokens = 0
    for projection in current.tools:
        used_exact_tokens = update_tool_projection(
            metrics,
            projection,
            "marker",
            used_exact_tokens,
            None,
        )
        changed = True
    return changed


def transform_state_tools(
    state: LoopState,
    policy: PolicySpec,
    metrics: SimulationMetrics,
    limits: LimitConfig,
) -> bool:
    if state.snapshot:
        return False
    if all(projection.mode not in policy.reducible_tool_modes for projection in state.tools):
        return False
    before = state.history_tokens
    used_exact_tokens = 0
    for projection in state.tools:
        if projection.mode in policy.reducible_tool_modes:
            used_exact_tokens = update_tool_projection(
                metrics,
                projection,
                policy.pressure_tool_mode,
                used_exact_tokens,
                policy_exact_budget(policy, "threshold", limits),
            )
        elif projection.exact_retained:
            used_exact_tokens += projection.tokens
    state.history_tokens = loop_history_tokens(state.loop, state.tools)
    return state.history_tokens < before


def snapshot_state(
    state: LoopState,
    metrics: SimulationMetrics,
    pricing: PricingConfig,
    config: SimulationConfig,
) -> int:
    if state.snapshot:
        return 0
    before = state.history_tokens
    state.snapshot = True
    state.history_tokens = state.snapshot_tokens
    add_llm_compaction_accounting(
        metrics,
        config.snapshot_overhead_tokens + before,
        state.snapshot_tokens,
        pricing,
    )
    return max(0, before - state.history_tokens)


def grouped_history_snapshot_tokens(before: int, config: SimulationConfig) -> int:
    return config.snapshot_overhead_tokens + min(
        max(config.compaction_output_tokens_per_loop, before // 64),
        config.compaction_output_tokens_per_loop * 8,
    )


def compact_history_states_batch(
    states: list[LoopState],
    metrics: SimulationMetrics,
    pricing: PricingConfig,
    config: SimulationConfig,
) -> int:
    candidates = [state for state in states if state.history_tokens > 0]
    if not candidates:
        return 0
    before = sum(state.history_tokens for state in candidates)
    output_tokens = grouped_history_snapshot_tokens(before, config)
    if output_tokens >= before:
        return 0

    first = candidates[0]
    first.history_tokens = output_tokens
    first.snapshot_tokens = output_tokens
    first.snapshot = True
    first.grouped = True
    for state in candidates[1:]:
        state.history_tokens = 0
        state.snapshot_tokens = 0
        state.snapshot = True
        state.grouped = True
    add_llm_compaction_accounting(
        metrics,
        config.snapshot_overhead_tokens + before,
        output_tokens,
        pricing,
        compacted_items=len(candidates),
    )
    return before - output_tokens


def select_history_batch_for_target(
    states: list[LoopState],
    history_tokens: int,
    stable_tokens: int,
    current_tokens: int,
    target: int,
    hard_limit: int,
    config: SimulationConfig,
) -> list[LoopState]:
    selected: list[LoopState] = []
    selected_tokens = 0
    projected = stable_tokens + history_tokens + current_tokens
    for state in states:
        if projected <= target and projected <= hard_limit:
            break
        if state.history_tokens <= 0:
            continue
        selected.append(state)
        selected_tokens += state.history_tokens
        estimated_output = grouped_history_snapshot_tokens(selected_tokens, config)
        projected = stable_tokens + history_tokens - max(
            0,
            selected_tokens - estimated_output,
        ) + current_tokens
    return selected


def group_snapshot_states(
    states: list[LoopState],
    metrics: SimulationMetrics,
    pricing: PricingConfig,
    config: SimulationConfig,
    keep_recent: int,
) -> int:
    cutoff = max(0, len(states) - keep_recent)
    candidates = [state for state in states[:cutoff] if state.history_tokens > 0]
    if len(candidates) < 2:
        return 0
    before = sum(state.history_tokens for state in candidates)
    output_tokens = grouped_history_snapshot_tokens(before, config)
    if output_tokens >= before:
        return 0
    first = candidates[0]
    first.history_tokens = output_tokens
    first.snapshot_tokens = output_tokens
    first.snapshot = True
    first.grouped = True
    for state in candidates[1:]:
        state.history_tokens = 0
        state.snapshot_tokens = 0
        state.snapshot = True
        state.grouped = True
    add_llm_compaction_accounting(
        metrics,
        config.snapshot_overhead_tokens + before,
        output_tokens,
        pricing,
        compacted_items=len(candidates),
    )
    return max(0, before - output_tokens)


def prepare_for_request(
    states: list[LoopState],
    current: CurrentLoopProjection,
    policy: PolicySpec,
    pricing: PricingConfig,
    limits: LimitConfig,
    config: SimulationConfig,
    metrics: SimulationMetrics,
) -> tuple[int, bool]:
    stable_tokens = stable_token_count(config)
    history_tokens = sum(state.history_tokens for state in states)
    threshold = effective_context_threshold(limits, config, policy)
    target = min(effective_context_target(limits, config, policy), threshold)
    current_loop_threshold = effective_current_loop_threshold(
        limits,
        config,
        policy,
        threshold,
    )
    prompt_limit = limits.prompt_limit_tokens
    keep_recent = (
        policy.keep_recent_loops_on_snapshot
        if policy.keep_recent_loops_on_snapshot is not None
        else config.keep_recent_loops_on_snapshot
    )
    changed = compress_current_loop_if_needed(
        current,
        policy,
        stable_tokens,
        history_tokens,
        current_loop_threshold,
        prompt_limit,
        limits,
        metrics,
    )
    projected = stable_tokens + history_tokens + current.tokens
    hard_limit = prompt_limit or threshold
    if projected <= min(threshold, hard_limit):
        return history_tokens, changed

    cutoff = max(0, len(states) - keep_recent)
    if policy.threshold_tool_projection:
        for state in states[:cutoff]:
            if transform_state_tools(state, policy, metrics, limits):
                changed = True
                history_tokens = sum(item.history_tokens for item in states)
                projected = stable_tokens + history_tokens + current.tokens
                if projected <= target:
                    break

    history_tokens = sum(state.history_tokens for state in states)
    projected = stable_tokens + history_tokens + current.tokens
    if projected <= min(target, hard_limit):
        return history_tokens, changed

    # LLM history compaction is a mandatory fallback for every policy. A real
    # compaction request consumes a contiguous old-history batch and returns one
    # compact snapshot, so request accounting is batched instead of per loop.
    batch = select_history_batch_for_target(
        states[:cutoff],
        history_tokens,
        stable_tokens,
        current.tokens,
        target,
        hard_limit,
        config,
    )
    reduced = compact_history_states_batch(batch, metrics, pricing, config)
    if reduced:
        changed = True
        history_tokens -= reduced
        projected = stable_tokens + history_tokens + current.tokens

    # If recent loops plus current loop are still too large, compact recent
    # completed loops too. Current loop tool outputs have already been minimized.
    if projected > hard_limit:
        batch = select_history_batch_for_target(
            states[cutoff:],
            history_tokens,
            stable_tokens,
            current.tokens,
            hard_limit,
            hard_limit,
            config,
        )
        reduced = compact_history_states_batch(batch, metrics, pricing, config)
        if reduced:
            changed = True
            history_tokens -= reduced
            projected = stable_tokens + history_tokens + current.tokens

    while projected > target and len(states) > keep_recent + 1:
        reduced = group_snapshot_states(
            states,
            metrics,
            pricing,
            config,
            keep_recent,
        )
        if not reduced:
            break
        changed = True
        history_tokens -= reduced
        projected = stable_tokens + history_tokens + current.tokens

    if projected > hard_limit:
        reduced = compact_current_loop(current, metrics, pricing, config)
        if reduced:
            changed = True
            projected = stable_tokens + history_tokens + current.tokens

    if projected > hard_limit and current.compacted_prefix_tokens is not None:
        # Last-resort deterministic projection for an impossible-width current
        # loop. It models dropping inline evidence to a bounded handle summary.
        before = current.tokens
        current.compacted_prefix_tokens = current.loop.user_tokens + config.loop_compaction_overhead_tokens
        current.compacted_tool_count = len(current.tools)
        changed = True
        add_llm_compaction_accounting(
            metrics,
            config.snapshot_overhead_tokens + before,
            current.compacted_prefix_tokens,
            pricing,
        )

    return history_tokens, changed


def select_initial_mode_for_tool(
    tool: ToolCall,
    current: CurrentLoopProjection,
    policy: PolicySpec,
    stable_tokens: int,
    history_tokens: int,
    threshold: int,
    prompt_limit: int | None,
) -> str:
    if policy.initial_tool_mode != "raw":
        return policy.initial_tool_mode
    raw_current = current.tokens + tool_call_output_tokens(tool) + tool.raw_tokens
    projected = stable_tokens + history_tokens + raw_current
    hard_limit = prompt_limit or threshold
    if projected <= min(threshold, hard_limit):
        return "raw"
    return policy.pressure_tool_mode


def current_loop_to_history_state(
    current: CurrentLoopProjection,
    index: int,
    config: SimulationConfig,
    policy: PolicySpec,
    metrics: SimulationMetrics,
    limits: LimitConfig,
) -> tuple[LoopState, bool]:
    state = current.to_history_state(index, config)
    if not policy.history_tool_mode or state.snapshot:
        return state, False
    before = state.history_tokens
    used_exact_tokens = 0
    for projection in state.tools:
        used_exact_tokens = update_tool_projection(
            metrics,
            projection,
            policy.history_tool_mode,
            used_exact_tokens,
            policy_exact_budget(policy, "history", limits),
        )
    state.history_tokens = loop_history_tokens(state.loop, state.tools)
    return state, state.history_tokens != before


def add_recovery_accounting(
    metrics: SimulationMetrics,
    probability: float,
    recovery_input_tokens: int,
    pricing: PricingConfig,
    config: SimulationConfig,
    *,
    failed: bool = False,
) -> None:
    if probability <= 0:
        return
    if failed:
        metrics.recovery_cost += probability * config.failure_penalty_cost
        metrics.expected_failure_rate += probability
        return
    recovery_output_tokens = 64
    metrics.recovery_cost += probability * (
        recovery_input_tokens * pricing.input_per_token
        + recovery_output_tokens * pricing.output_per_token
    )
    metrics.expected_recovery_input_tokens += probability * recovery_input_tokens
    metrics.expected_recovery_output_tokens += probability * recovery_output_tokens
    metrics.expected_extra_tool_calls += probability
    metrics.latency_ms += probability * pricing.recovery_latency_penalty_ms


def add_raw_dependency_event_costs(
    loop_index: int,
    loop: BenchLoop,
    states: list[LoopState],
    policy: PolicySpec,
    pricing: PricingConfig,
    limits: LimitConfig,
    config: SimulationConfig,
    metrics: SimulationMetrics,
) -> None:
    total_state_count = len(states)
    for event in loop.raw_dependency_events:
        source_index = loop_index - event.source_loop_offset
        if source_index < 0 or source_index >= len(states):
            continue
        add_dependency_cost_for_state(
            states[source_index],
            event.dependency,
            event.probability,
            policy,
            pricing,
            limits,
            config,
            total_state_count,
            recovery_tokens_override=event.recovery_tokens,
            metrics=metrics,
        )


def add_current_loop_dependency_costs(
    loop_index: int,
    current: CurrentLoopProjection,
    policy: PolicySpec,
    pricing: PricingConfig,
    limits: LimitConfig,
    config: SimulationConfig,
    metrics: SimulationMetrics,
) -> None:
    if not current.loop.tool_calls:
        return
    state = LoopState(
        loop=current.loop,
        index=loop_index,
        tools=current.tools,
        history_tokens=current.tokens,
        snapshot=current.compacted_prefix_tokens is not None,
        snapshot_tokens=loop_llm_snapshot_tokens(current.loop, config),
    )
    for dependency, probability in current.loop.final_dependency_probabilities.items():
        if dependency == "none" or probability <= 0:
            continue
        add_dependency_cost_for_state(
            state,
            dependency,
            probability,
            policy,
            pricing,
            limits,
            config,
            total_state_count=1,
            recovery_tokens_override=None,
            metrics=metrics,
        )


def add_expected_dependency_cost(
    state: LoopState,
    policy: PolicySpec,
    pricing: PricingConfig,
    limits: LimitConfig,
    config: SimulationConfig,
    metrics: SimulationMetrics,
    total_state_count: int,
) -> None:
    for dependency, probability in state.loop.dependency_probabilities.items():
        if dependency == "none" or probability <= 0:
            continue
        add_dependency_cost_for_state(
            state,
            dependency,
            probability,
            policy,
            pricing,
            limits,
            config,
            total_state_count,
            recovery_tokens_override=None,
            metrics=metrics,
        )


def add_dependency_cost_for_state(
    state: LoopState,
    dependency: str,
    probability: float,
    policy: PolicySpec,
    pricing: PricingConfig,
    limits: LimitConfig,
    config: SimulationConfig,
    total_state_count: int,
    *,
    recovery_tokens_override: int | None,
    metrics: SimulationMetrics,
) -> None:
    if dependency == "global_conclusion":
        return
    if dependency == "continuation_handle":
        if any(projection.tool.handle_tokens > 0 for projection in state.tools):
            return
        add_recovery_accounting(metrics, probability, 0, pricing, config, failed=True)
        return
    if dependency == "exact_evidence":
        retained = exact_evidence_retained_probability(state, total_state_count)
        probability *= max(0.0, 1.0 - retained)
        if probability <= 0:
            return
    elif dependency == "raw_content":
        retained = raw_content_retained_probability(state)
        probability *= max(0.0, 1.0 - retained)
        if probability <= 0:
            return
    elif dependency == "local_detail" and not state.snapshot:
        if any(projection.mode == "raw" for projection in state.tools):
            return
    recovery_tokens = (
        recovery_tokens_override
        if recovery_tokens_override is not None
        else recovery_tokens_for_dependency(state.loop, dependency)
    )
    if policy.recovery_mode == "minimal_pinned":
        recovery_tokens = minimal_pinned_recovery_tokens_for_dependency(
            state.loop,
            dependency,
            recovery_tokens,
        )
    elif policy.recovery_mode == "naive_raw_reread":
        raw_tokens = raw_reread_tokens_for_dependency(state.loop, dependency)
        if raw_tokens > 0:
            attempts = recovery_attempt_count_for_raw_reread(
                raw_tokens,
                policy,
                limits,
                config,
            )
            if attempts > 1:
                metrics.expected_repeat_recovery_tool_calls += probability * (attempts - 1)
            add_recovery_accounting(
                metrics,
                probability * attempts,
                config.recovery_model_overhead_tokens + raw_tokens,
                pricing,
                config,
            )
            return
    if recovery_tokens <= 0:
        add_recovery_accounting(metrics, probability, 0, pricing, config, failed=True)
        return
    add_recovery_accounting(
        metrics,
        probability,
        config.recovery_model_overhead_tokens + recovery_tokens,
        pricing,
        config,
    )


def exact_evidence_retained_probability(state: LoopState, total_state_count: int) -> float:
    if state.snapshot:
        return 0.0
    working = [
        projection
        for projection in state.tools
        if projection.tool.preserve == "working_evidence"
    ]
    if not working:
        return 0.0
    retained = sum(1 for projection in working if projection.exact_retained)
    return retained / len(working)


def raw_content_retained_probability(state: LoopState) -> float:
    if state.snapshot or not state.tools:
        return 0.0
    retained = sum(1 for projection in state.tools if projection.tokens >= projection.tool.raw_tokens)
    return retained / len(state.tools)


def recovery_tokens_for_dependency(loop: BenchLoop, dependency: str) -> int:
    if dependency == "raw_content":
        return max((tool.raw_tokens for tool in loop.tool_calls), default=0)
    return max((tool.recovery_tokens.get(dependency, 0) for tool in loop.tool_calls), default=0)


def raw_reread_tokens_for_dependency(loop: BenchLoop, dependency: str) -> int:
    if dependency not in {"raw_content", "exact_evidence", "local_detail"}:
        return 0
    return max(
        (
            tool.raw_tokens
            for tool in loop.tool_calls
            if tool.capability == "read_file" and tool.raw_tokens > tool.handle_tokens
        ),
        default=0,
    )


def recovery_attempt_count_for_raw_reread(
    raw_tokens: int,
    policy: PolicySpec,
    limits: LimitConfig,
    config: SimulationConfig,
) -> int:
    default_threshold = effective_context_threshold(limits, config, policy)
    current_threshold = effective_current_loop_threshold(
        limits,
        config,
        policy,
        default_threshold,
    )
    projected = stable_token_count(config) + raw_tokens
    if projected <= current_threshold:
        return 1
    return max(1, policy.recovery_repeat_limit)


def minimal_pinned_recovery_tokens_for_dependency(
    loop: BenchLoop,
    dependency: str,
    fallback_tokens: int,
) -> int:
    if dependency != "raw_content":
        return fallback_tokens
    candidates = []
    for tool in loop.tool_calls:
        if tool.capability != "read_file":
            continue
        exact_tokens = max(
            tool.recovery_tokens.get("exact_evidence", 0),
            tool.exact_excerpt_tokens,
        )
        if exact_tokens > 0:
            candidates.append(min(tool.raw_tokens, exact_tokens + tool.handle_tokens))
    return max(candidates, default=fallback_tokens)


def finalize_metrics(metrics: SimulationMetrics) -> SimulationMetrics:
    metrics.total_cost = (
        metrics.base_cost
        + metrics.recovery_cost
        + metrics.llm_compaction_cost
        + metrics.limit_penalty_cost
    )
    metrics.answerability_score = (
        0.0
        if metrics.limit_violation_count
        else max(0.0, 1.0 - metrics.expected_failure_rate)
    )
    return metrics


def mark_request_failure(
    metrics: SimulationMetrics,
    loop_index: int,
    request_kind: str,
) -> SimulationMetrics:
    metrics.failure_turn = loop_index + 1
    metrics.failure_request_kind = request_kind
    return finalize_metrics(metrics)


def simulate_workload(
    workload: Workload,
    policy: PolicySpec,
    pricing: PricingConfig,
    cache: CacheConfig,
    limits: LimitConfig,
    config: SimulationConfig,
) -> SimulationMetrics:
    metrics = SimulationMetrics(
        workload=workload.name,
        policy=policy.name,
        requested_user_turn_count=len(workload.loops),
        policy_description=policy.description,
    )
    states: list[LoopState] = []
    previous_input_tokens: int | None = None
    common_prefix_cap_tokens: int | None = None
    stable_tokens = stable_token_count(config)

    for loop_index, loop in enumerate(workload.loops):
        metrics.user_turn_count += 1

        current = CurrentLoopProjection(loop=loop)
        history_tokens, changed = prepare_for_request(
            states,
            current,
            policy,
            pricing,
            limits,
            config,
            metrics,
        )
        common_prefix_cap_tokens = cap_common_prefix(
            common_prefix_cap_tokens,
            stable_tokens if changed else None,
        )
        add_raw_dependency_event_costs(
            loop_index,
            loop,
            states,
            policy,
            pricing,
            limits,
            config,
            metrics,
        )

        if not loop.tool_calls:
            input_tokens = stable_tokens + history_tokens + current.tokens
            request, previous_input_tokens = account_fast_request(
                input_tokens,
                loop.assistant_final_tokens,
                previous_input_tokens,
                common_prefix_cap_tokens,
                stable_tokens,
                pricing,
                cache,
            )
            common_prefix_cap_tokens = None
            if not metrics.add_request(request, limits, config):
                return mark_request_failure(metrics, loop_index, "agent_final_no_tool")
            state, boundary_changed = current_loop_to_history_state(
                current,
                loop_index,
                config,
                policy,
                metrics,
                limits,
            )
            states.append(state)
            if boundary_changed:
                common_prefix_cap_tokens = cap_common_prefix(
                    common_prefix_cap_tokens,
                    stable_tokens + history_tokens + loop.user_tokens,
                )
            add_expected_dependency_cost(
                state,
                policy,
                pricing,
                limits,
                config,
                metrics,
                len(states),
            )
            metrics.completed_user_turn_count += 1
            continue

        for tool_index, tool in enumerate(loop.tool_calls):
            history_tokens, changed = prepare_for_request(
                states,
                current,
                policy,
                pricing,
                limits,
                config,
                metrics,
            )
            common_prefix_cap_tokens = cap_common_prefix(
                common_prefix_cap_tokens,
                stable_tokens if changed else None,
            )
            input_tokens = stable_tokens + history_tokens + current.tokens
            request, previous_input_tokens = account_fast_request(
                input_tokens,
                tool_call_output_tokens(tool),
                previous_input_tokens,
                common_prefix_cap_tokens,
                stable_tokens,
                pricing,
                cache,
            )
            common_prefix_cap_tokens = None
            if not metrics.add_request(request, limits, config):
                return mark_request_failure(
                    metrics,
                    loop_index,
                    f"agent_tool_request:{tool_index + 1}",
                )

            mode = select_initial_mode_for_tool(
                tool,
                current,
                policy,
                stable_tokens,
                history_tokens,
                effective_current_loop_threshold(
                    limits,
                    config,
                    policy,
                    effective_context_threshold(limits, config, policy),
                ),
                limits.prompt_limit_tokens,
            )
            tokens, exact = projected_tool_tokens(
                tool,
                mode,
                sum(item.tokens for item in current.tools if item.exact_retained),
                policy_exact_budget(
                    policy,
                    "pressure" if mode == policy.pressure_tool_mode else "history",
                    limits,
                ),
            )
            projection = ToolProjection(tool=tool, mode=mode, tokens=tokens, exact_retained=exact)
            record_tool_projection(metrics, projection)
            current.tools.append(projection)

        history_tokens, changed = prepare_for_request(
            states,
            current,
            policy,
            pricing,
            limits,
            config,
            metrics,
        )
        common_prefix_cap_tokens = cap_common_prefix(
            common_prefix_cap_tokens,
            stable_tokens if changed else None,
        )
        add_current_loop_dependency_costs(
            loop_index,
            current,
            policy,
            pricing,
            limits,
            config,
            metrics,
        )
        input_tokens = stable_tokens + history_tokens + current.tokens
        request, previous_input_tokens = account_fast_request(
            input_tokens,
            loop.assistant_final_tokens,
            previous_input_tokens,
            common_prefix_cap_tokens,
            stable_tokens,
            pricing,
            cache,
        )
        common_prefix_cap_tokens = None
        if not metrics.add_request(request, limits, config):
            return mark_request_failure(metrics, loop_index, "agent_final_after_tools")

        state, boundary_changed = current_loop_to_history_state(
            current,
            loop_index,
            config,
            policy,
            metrics,
            limits,
        )
        states.append(state)
        if boundary_changed:
            common_prefix_cap_tokens = cap_common_prefix(
                common_prefix_cap_tokens,
                stable_tokens + history_tokens + loop.user_tokens,
            )
        add_expected_dependency_cost(
            state,
            policy,
            pricing,
            limits,
            config,
            metrics,
            len(states),
        )
        metrics.completed_user_turn_count += 1

    return finalize_metrics(metrics)


def simulate_workload_fast(
    workload: Workload,
    policy: PolicySpec,
    pricing: PricingConfig,
    cache: CacheConfig,
    limits: LimitConfig,
    config: SimulationConfig,
) -> SimulationMetrics:
    return simulate_workload(workload, policy, pricing, cache, limits, config)


def run_benchmark(
    workloads: list[Workload],
    policies: Iterable[PolicySpec],
    pricing: PricingConfig,
    cache: CacheConfig,
    limits: LimitConfig,
    config: SimulationConfig,
) -> list[SimulationMetrics]:
    results = []
    for workload in workloads:
        for policy in policies:
            results.append(
                simulate_workload(
                    copy.deepcopy(workload),
                    policy,
                    pricing,
                    cache,
                    limits,
                    config,
                )
            )
    return results


def aggregate_results(results: list[SimulationMetrics]) -> list[SimulationMetrics]:
    by_policy: dict[str, SimulationMetrics] = {}
    for result in results:
        aggregate = by_policy.get(result.policy)
        if aggregate is None:
            aggregate = SimulationMetrics(
                workload="ALL",
                policy=result.policy,
                policy_description=result.policy_description,
            )
            by_policy[result.policy] = aggregate
        aggregate.requested_user_turn_count += result.requested_user_turn_count
        aggregate.user_turn_count += result.user_turn_count
        aggregate.completed_user_turn_count += result.completed_user_turn_count
        aggregate.input_tokens += result.input_tokens
        aggregate.cached_tokens += result.cached_tokens
        aggregate.uncached_tokens += result.uncached_tokens
        aggregate.output_tokens += result.output_tokens
        aggregate.request_count += result.request_count
        aggregate.max_context_tokens = max(aggregate.max_context_tokens, result.max_context_tokens)
        aggregate.max_output_tokens = max(aggregate.max_output_tokens, result.max_output_tokens)
        aggregate.prompt_limit_tokens = result.prompt_limit_tokens
        aggregate.output_limit_tokens = result.output_limit_tokens
        aggregate.limit_violation_count += result.limit_violation_count
        aggregate.limit_penalty_cost += result.limit_penalty_cost
        aggregate.base_cost += result.base_cost
        aggregate.recovery_cost += result.recovery_cost
        aggregate.expected_recovery_input_tokens += result.expected_recovery_input_tokens
        aggregate.expected_recovery_output_tokens += result.expected_recovery_output_tokens
        aggregate.tool_raw_tokens += result.tool_raw_tokens
        aggregate.tool_visible_tokens += result.tool_visible_tokens
        aggregate.tool_structured_compression_events += result.tool_structured_compression_events
        aggregate.llm_compaction_input_tokens += result.llm_compaction_input_tokens
        aggregate.llm_compaction_output_tokens += result.llm_compaction_output_tokens
        aggregate.llm_compaction_cost += result.llm_compaction_cost
        aggregate.llm_compaction_events += result.llm_compaction_events
        aggregate.llm_compacted_items += result.llm_compacted_items
        aggregate.total_cost += result.total_cost
        aggregate.latency_ms += result.latency_ms
        if aggregate.failure_turn is None and result.failure_turn is not None:
            aggregate.failure_turn = result.failure_turn
            aggregate.failure_request_kind = result.failure_request_kind
        aggregate.expected_extra_tool_calls += result.expected_extra_tool_calls
        aggregate.expected_repeat_recovery_tool_calls += result.expected_repeat_recovery_tool_calls
        aggregate.expected_failure_rate += result.expected_failure_rate
    workload_count = len({result.workload for result in results}) or 1
    for aggregate in by_policy.values():
        aggregate.answerability_score = max(
            0.0,
            1.0 - aggregate.expected_failure_rate / workload_count,
        )
        if aggregate.limit_violation_count:
            aggregate.answerability_score = 0.0
    return sorted(by_policy.values(), key=lambda item: (item.total_cost, -item.answerability_score))


def ratio(numerator: int, denominator: int | None) -> float | None:
    if not denominator:
        return None
    return round(numerator / denominator, 4)


def format_ratio(value: float | None) -> str:
    if value is None:
        return "n/a"
    return f"{value:.1%}"


def metrics_to_dict(metric: SimulationMetrics) -> dict[str, Any]:
    return {
        "workload": metric.workload,
        "policy": metric.policy,
        "requested_user_turn_count": metric.requested_user_turn_count,
        "user_turn_count": metric.user_turn_count,
        "completed_user_turn_count": metric.completed_user_turn_count,
        "input_tokens": metric.input_tokens,
        "cached_tokens": metric.cached_tokens,
        "uncached_tokens": metric.uncached_tokens,
        "output_tokens": metric.output_tokens,
        "expected_recovery_input_tokens": round(metric.expected_recovery_input_tokens, 2),
        "expected_recovery_output_tokens": round(metric.expected_recovery_output_tokens, 2),
        "expected_total_tokens": round(metric.expected_total_tokens, 2),
        "cache_hit_ratio": round(metric.cache_hit_ratio, 4),
        "request_count": metric.request_count,
        "max_context_tokens": metric.max_context_tokens,
        "max_output_tokens": metric.max_output_tokens,
        "prompt_limit_tokens": metric.prompt_limit_tokens,
        "output_limit_tokens": metric.output_limit_tokens,
        "context_utilization": ratio(metric.max_context_tokens, metric.prompt_limit_tokens),
        "output_utilization": ratio(metric.max_output_tokens, metric.output_limit_tokens),
        "limit_violation_count": metric.limit_violation_count,
        "limit_penalty_cost": round(metric.limit_penalty_cost, 6),
        "base_cost": round(metric.base_cost, 6),
        "recovery_cost": round(metric.recovery_cost, 6),
        "tool_raw_tokens": metric.tool_raw_tokens,
        "tool_visible_tokens": metric.tool_visible_tokens,
        "tool_structured_compression_events": metric.tool_structured_compression_events,
        "tool_structured_compression_saved_tokens": metric.tool_structured_compression_saved_tokens,
        "llm_compaction_events": metric.llm_compaction_events,
        "llm_compaction_requests": metric.llm_compaction_events,
        "llm_compacted_items": metric.llm_compacted_items,
        "llm_compaction_input_tokens": metric.llm_compaction_input_tokens,
        "llm_compaction_output_tokens": metric.llm_compaction_output_tokens,
        "llm_compaction_cost": round(metric.llm_compaction_cost, 6),
        "total_cost": round(metric.total_cost, 6),
        "latency_ms": round(metric.latency_ms, 2),
        "failure_turn": metric.failure_turn,
        "failure_request_kind": metric.failure_request_kind,
        "expected_extra_tool_calls": round(metric.expected_extra_tool_calls, 4),
        "expected_repeat_recovery_tool_calls": round(
            metric.expected_repeat_recovery_tool_calls,
            4,
        ),
        "expected_failure_rate": round(metric.expected_failure_rate, 4),
        "answerability_score": round(metric.answerability_score, 4),
    }


def format_markdown(results: list[SimulationMetrics], currency: str) -> str:
    aggregate = aggregate_results(results)
    lines = [
        "# Context Policy Benchmark",
        "",
        "## Aggregate",
        "",
        "| policy | total cost | turns complete/requested | failed at | agent reqs | expected toks | cache hit | input toks | cached toks | max ctx | ctx util | tool struct comp | tool saved toks | LLM comp reqs | LLM comp items | LLM comp cost | limit fails | recovery cost | recovery toks | extra tools | repeat reads | answerability |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for metric in aggregate:
        recovery_tokens = metric.expected_recovery_input_tokens + metric.expected_recovery_output_tokens
        lines.append(
            "| {policy} | {currency} {total:.6f} | {completed}/{requested} | {failed_at} | {requests} | {expected_tokens:.0f} | {cache:.1%} | {input_tokens} | {cached_tokens} | {max_context} | {context_util} | {tool_compressions} | {tool_saved} | {llm_compactions} | {llm_compacted_items} | {currency} {llm_compaction_cost:.6f} | {limit_fails} | {currency} {recovery:.6f} | {recovery_tokens:.0f} | {extra:.2f} | {repeat:.2f} | {score:.2f} |".format(
                policy=metric.policy,
                currency=currency,
                total=metric.total_cost,
                completed=metric.completed_user_turn_count,
                requested=metric.requested_user_turn_count,
                failed_at=metric.failure_turn or "",
                requests=metric.request_count,
                expected_tokens=metric.expected_total_tokens,
                cache=metric.cache_hit_ratio,
                input_tokens=metric.input_tokens,
                cached_tokens=metric.cached_tokens,
                max_context=metric.max_context_tokens,
                context_util=format_ratio(ratio(metric.max_context_tokens, metric.prompt_limit_tokens)),
                tool_compressions=metric.tool_structured_compression_events,
                tool_saved=metric.tool_structured_compression_saved_tokens,
                llm_compactions=metric.llm_compaction_events,
                llm_compacted_items=metric.llm_compacted_items,
                llm_compaction_cost=metric.llm_compaction_cost,
                limit_fails=metric.limit_violation_count,
                recovery=metric.recovery_cost,
                recovery_tokens=recovery_tokens,
                extra=metric.expected_extra_tool_calls,
                repeat=metric.expected_repeat_recovery_tool_calls,
                score=metric.answerability_score,
            )
        )
    lines.extend(["", "## Scenario Breakdown", ""])
    for workload in sorted({result.workload for result in results}):
        lines.extend(
            [
                f"### {workload}",
                "",
                "| policy | total cost | turns complete/requested | failed at | agent reqs | expected toks | cache hit | max ctx | ctx util | tool struct comp | tool saved toks | LLM comp reqs | LLM comp items | LLM comp cost | limit fails | recovery cost | recovery toks | extra tools | repeat reads |",
                "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
            ]
        )
        for metric in sorted(
            [result for result in results if result.workload == workload],
            key=lambda item: (item.total_cost, -item.answerability_score),
        ):
            recovery_tokens = metric.expected_recovery_input_tokens + metric.expected_recovery_output_tokens
            lines.append(
                "| {policy} | {currency} {total:.6f} | {completed}/{requested} | {failed_at} | {requests} | {expected_tokens:.0f} | {cache:.1%} | {max_context} | {context_util} | {tool_compressions} | {tool_saved} | {llm_compactions} | {llm_compacted_items} | {currency} {llm_compaction_cost:.6f} | {limit_fails} | {currency} {recovery:.6f} | {recovery_tokens:.0f} | {extra:.2f} | {repeat:.2f} |".format(
                    policy=metric.policy,
                    currency=currency,
                    total=metric.total_cost,
                    completed=metric.completed_user_turn_count,
                    requested=metric.requested_user_turn_count,
                    failed_at=metric.failure_turn or "",
                    requests=metric.request_count,
                    expected_tokens=metric.expected_total_tokens,
                    cache=metric.cache_hit_ratio,
                    max_context=metric.max_context_tokens,
                    context_util=format_ratio(ratio(metric.max_context_tokens, metric.prompt_limit_tokens)),
                    tool_compressions=metric.tool_structured_compression_events,
                    tool_saved=metric.tool_structured_compression_saved_tokens,
                    llm_compactions=metric.llm_compaction_events,
                    llm_compacted_items=metric.llm_compacted_items,
                    llm_compaction_cost=metric.llm_compaction_cost,
                    limit_fails=metric.limit_violation_count,
                    recovery=metric.recovery_cost,
                    recovery_tokens=recovery_tokens,
                    extra=metric.expected_extra_tool_calls,
                    repeat=metric.expected_repeat_recovery_tool_calls,
                )
            )
        lines.append("")
    return "\n".join(lines)


def select_policies(names: str | None) -> list[PolicySpec]:
    if not names:
        return POLICIES
    wanted = {name.strip() for name in names.split(",") if name.strip()}
    selected = [policy for policy in POLICIES if policy.name in wanted]
    missing = wanted - {policy.name for policy in selected}
    if missing:
        known = ", ".join(policy.name for policy in POLICIES)
        raise SystemExit(f"unknown policies: {', '.join(sorted(missing))}; known: {known}")
    return selected


def default_path(*parts: str) -> Path:
    return Path(__file__).resolve().parent.joinpath(*parts)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--workload",
        type=Path,
        default=default_path("workloads", "default.json"),
        help="Workload JSON file.",
    )
    parser.add_argument(
        "--pricing",
        type=Path,
        default=default_path("pricing", "default.json"),
        help="Pricing/cache JSON file.",
    )
    parser.add_argument(
        "--policies",
        help="Comma-separated policy names. Defaults to every built-in policy.",
    )
    parser.add_argument(
        "--include-stress",
        action="store_true",
        help="Append deterministic long-running stress workloads.",
    )
    parser.add_argument(
        "--include-validation",
        action="store_true",
        help="Append deterministic holdout validation workloads.",
    )
    parser.add_argument(
        "--stress-only",
        action="store_true",
        help="Run only deterministic stress workloads.",
    )
    parser.add_argument(
        "--validation-only",
        action="store_true",
        help="Run only deterministic holdout validation workloads.",
    )
    parser.add_argument(
        "--stress-scale",
        type=int,
        default=1,
        help="Scale stress workload loop counts. Default: 1.",
    )
    parser.add_argument(
        "--validation-scale",
        type=int,
        default=1,
        help="Scale validation workload loop counts. Default: 1.",
    )
    parser.add_argument(
        "--long-run-turns",
        type=int,
        help="Run one deterministic long-run workload with exactly this many user turns.",
    )
    parser.add_argument(
        "--context-threshold-ratio",
        type=float,
        help="Override compaction trigger as a ratio of the model prompt limit.",
    )
    parser.add_argument(
        "--context-target-ratio",
        type=float,
        help="Override post-compaction target as a ratio of the model prompt limit.",
    )
    parser.add_argument(
        "--context-threshold-tokens",
        type=int,
        help="Override compaction trigger as an absolute token count.",
    )
    parser.add_argument(
        "--format",
        choices=("markdown", "json"),
        default="markdown",
        help="Output format.",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        help="Write report.md, report.html, and results.json to this directory.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    pricing, cache, limits, config = load_pricing(args.pricing)
    if args.context_threshold_ratio is not None:
        config.context_threshold_ratio = args.context_threshold_ratio
    if args.context_target_ratio is not None:
        config.context_target_ratio = args.context_target_ratio
    if args.context_threshold_tokens is not None:
        config.context_threshold_tokens = args.context_threshold_tokens
        config.context_threshold_ratio = None
        if args.context_target_ratio is None:
            config.context_target_ratio = None
    if args.long_run_turns is not None:
        workloads = [long_run_workload(args.long_run_turns)]
    else:
        workloads = (
            []
            if args.stress_only or args.validation_only
            else parse_workloads(args.workload)
        )
    if args.long_run_turns is None and (args.include_stress or args.stress_only):
        workloads.extend(stress_workloads(args.stress_scale))
    if args.long_run_turns is None and (
        args.include_validation or args.validation_only
    ):
        workloads.extend(validation_workloads(args.validation_scale))
    policies = select_policies(args.policies)
    results = run_benchmark(workloads, policies, pricing, cache, limits, config)
    payload = {
        "metadata": {
            "workload": str(args.workload),
            "include_stress": args.include_stress,
            "include_validation": args.include_validation,
            "stress_only": args.stress_only,
            "validation_only": args.validation_only,
            "stress_scale": args.stress_scale,
            "validation_scale": args.validation_scale,
            "long_run_turns": args.long_run_turns,
            "pricing": str(args.pricing),
            "context_threshold_ratio": config.context_threshold_ratio,
            "context_target_ratio": config.context_target_ratio,
            "context_threshold_tokens": config.context_threshold_tokens,
            "policies": [policy.name for policy in policies],
        },
        "aggregate": [metrics_to_dict(metric) for metric in aggregate_results(results)],
        "scenarios": [metrics_to_dict(metric) for metric in results],
    }
    if args.format == "json":
        rendered = json.dumps(payload, ensure_ascii=False, indent=2)
        print(rendered)
    else:
        rendered = format_markdown(results, pricing.currency)
        print(rendered)
    if args.output_dir:
        markdown = format_markdown(results, pricing.currency)
        html = render_html_report("Context Policy Benchmark", markdown)
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
