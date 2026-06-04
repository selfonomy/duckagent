import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import context_policy_benchmark as bench
import model_matrix
import models_dev_profile
import recoverable_policy_sweep
from report_utils import render_html_report, write_report_files


ROOT = Path(__file__).resolve().parent


class ContextPolicyBenchmarkTests(unittest.TestCase):
    def load_default(self):
        pricing, cache, limits, config = bench.load_pricing(ROOT / "pricing" / "default.json")
        workloads = bench.parse_workloads(ROOT / "workloads" / "default.json")
        return pricing, cache, limits, config, workloads

    def policy(self, name: str) -> bench.PolicySpec:
        return next(policy for policy in bench.POLICIES if policy.name == name)

    def test_policy_set_is_direct_agent_only(self):
        names = [policy.name for policy in bench.POLICIES]

        self.assertIn("immediate_summary", names)
        self.assertIn("loop_boundary_summary", names)
        self.assertIn("loop_boundary_budgeted", names)
        self.assertIn("loop_boundary_evidence_summary", names)
        self.assertIn("duckagent_recoverable_boundary", names)
        self.assertIn("duckagent_recoverable_decay", names)
        self.assertIn("duckagent_recoverable_decay_lean", names)
        self.assertIn("duckagent_recoverable_decay_balanced", names)
        self.assertIn("duckagent_recoverable_decay_tight", names)
        self.assertIn("duckagent_recoverable_decay_tight_90", names)
        self.assertIn("duckagent_recoverable_decay_tight_95", names)
        self.assertIn("duckagent_recoverable_decay_guarded_mid", names)
        self.assertIn("duckagent_recoverable_decay_guarded_mid_naive_recovery", names)
        self.assertIn("duckagent_recoverable_decay_guarded_late", names)
        self.assertIn("duckagent_recoverable_decay_guarded_late_tight", names)
        self.assertIn("duckagent_recoverable_decay_late_current", names)
        self.assertIn("duckagent_recoverable_decay_late_tight", names)
        self.assertIn("duckagent_summary_history_recoverable_current", names)
        self.assertIn("duckagent_recoverable_decay_adaptive", names)
        self.assertIn("duckagent_recoverable_decay_adaptive_guarded", names)
        self.assertIn("duckagent_recoverable_decay_recent2", names)
        self.assertIn("duckagent_recoverable_decay_soft", names)
        self.assertIn("duckagent_recoverable_decay_relative", names)
        self.assertIn("raw_snapshot", names)
        self.assertIn("evidence_budget", names)
        self.assertIn("early_windowed_prune", names)
        self.assertIn("late_guarded_truncation", names)
        self.assertFalse(any("execute" in name for name in names))

    def test_model_relative_recoverable_budget_scales_with_prompt_limit(self):
        relative = self.policy("duckagent_recoverable_decay_relative")

        small_limits = bench.LimitConfig(input_tokens=128_000, output_tokens=8_192)
        large_limits = bench.LimitConfig(input_tokens=922_000, output_tokens=128_000)

        self.assertEqual(bench.policy_exact_budget(relative, "threshold", small_limits), 0)
        self.assertLess(
            bench.policy_exact_budget(relative, "history", small_limits),
            bench.policy_exact_budget(relative, "history", large_limits),
        )
        self.assertLess(
            bench.policy_exact_budget(relative, "pressure", small_limits),
            bench.policy_exact_budget(relative, "pressure", large_limits),
        )

    def test_adaptive_recoverable_budget_is_capped_by_balanced_limits(self):
        adaptive = self.policy("duckagent_recoverable_decay_adaptive")

        small_limits = bench.LimitConfig(input_tokens=128_000, output_tokens=8_192)
        large_limits = bench.LimitConfig(input_tokens=922_000, output_tokens=128_000)

        self.assertEqual(bench.policy_exact_budget(adaptive, "threshold", small_limits), 0)
        self.assertLess(bench.policy_exact_budget(adaptive, "pressure", small_limits), 18_000)
        self.assertLess(bench.policy_exact_budget(adaptive, "history", small_limits), 2_000)
        self.assertEqual(bench.policy_exact_budget(adaptive, "pressure", large_limits), 18_000)
        self.assertEqual(bench.policy_exact_budget(adaptive, "history", large_limits), 2_000)

    def test_guarded_adaptive_budget_has_floor_and_cap(self):
        guarded = self.policy("duckagent_recoverable_decay_adaptive_guarded")

        small_limits = bench.LimitConfig(input_tokens=128_000, output_tokens=8_192)
        large_limits = bench.LimitConfig(input_tokens=922_000, output_tokens=128_000)

        self.assertEqual(bench.policy_exact_budget(guarded, "threshold", small_limits), 0)
        self.assertEqual(bench.policy_exact_budget(guarded, "pressure", small_limits), 12_000)
        self.assertEqual(bench.policy_exact_budget(guarded, "history", small_limits), 1_500)
        self.assertEqual(bench.policy_exact_budget(guarded, "pressure", large_limits), 18_000)
        self.assertEqual(bench.policy_exact_budget(guarded, "history", large_limits), 2_000)

    def test_hybrid_recoverable_policy_budgets(self):
        tight = self.policy("duckagent_recoverable_decay_tight")
        tight_90 = self.policy("duckagent_recoverable_decay_tight_90")
        tight_95 = self.policy("duckagent_recoverable_decay_tight_95")
        guarded_mid = self.policy("duckagent_recoverable_decay_guarded_mid")
        guarded_late = self.policy("duckagent_recoverable_decay_guarded_late")
        guarded_late_tight = self.policy("duckagent_recoverable_decay_guarded_late_tight")
        late = self.policy("duckagent_recoverable_decay_late_current")
        summary_history = self.policy("duckagent_summary_history_recoverable_current")
        limits = bench.LimitConfig(input_tokens=128_000, output_tokens=8_192)

        self.assertEqual(bench.policy_exact_budget(tight, "pressure", limits), 15_000)
        self.assertEqual(bench.policy_exact_budget(tight, "history", limits), 1_500)
        self.assertEqual(tight.current_loop_threshold_ratio, 0.80)
        self.assertEqual(bench.policy_exact_budget(tight_90, "pressure", limits), 15_000)
        self.assertEqual(bench.policy_exact_budget(tight_90, "history", limits), 1_500)
        self.assertEqual(tight_90.current_loop_threshold_ratio, 0.90)
        self.assertEqual(bench.policy_exact_budget(tight_95, "pressure", limits), 15_000)
        self.assertEqual(bench.policy_exact_budget(tight_95, "history", limits), 1_500)
        self.assertEqual(tight_95.current_loop_threshold_ratio, 0.95)
        self.assertEqual(bench.policy_exact_budget(guarded_late, "pressure", limits), 18_000)
        self.assertEqual(bench.policy_exact_budget(guarded_late, "history", limits), 2_000)
        self.assertEqual(guarded_mid.current_loop_threshold_ratio, 0.85)
        self.assertEqual(guarded_mid.current_loop_threshold_fallback_ratio, 0.80)
        self.assertEqual(guarded_mid.current_loop_threshold_min_prompt_tokens, 200_000)
        self.assertEqual(guarded_late.current_loop_threshold_ratio, 0.90)
        self.assertEqual(guarded_late.current_loop_threshold_fallback_ratio, 0.80)
        self.assertEqual(guarded_late.current_loop_threshold_min_prompt_tokens, 200_000)
        self.assertEqual(bench.policy_exact_budget(guarded_late_tight, "pressure", limits), 15_000)
        self.assertEqual(bench.policy_exact_budget(guarded_late_tight, "history", limits), 1_500)
        self.assertEqual(late.current_loop_threshold_ratio, 1.0)
        self.assertEqual(late.history_tool_mode, "recoverable")
        self.assertEqual(summary_history.history_tool_mode, "summary")
        self.assertEqual(summary_history.pressure_tool_mode, "recoverable")

    def test_prompt_window_guard_switches_current_loop_threshold(self):
        guarded = self.policy("duckagent_recoverable_decay_guarded_late")
        _pricing, _cache, _limits, config, _workloads = self.load_default()
        small_limits = bench.LimitConfig(input_tokens=128_000, output_tokens=8_192)
        large_limits = bench.LimitConfig(input_tokens=262_144, output_tokens=32_768)

        small_default = bench.effective_context_threshold(small_limits, config, guarded)
        large_default = bench.effective_context_threshold(large_limits, config, guarded)

        self.assertEqual(
            bench.effective_current_loop_threshold(
                small_limits,
                config,
                guarded,
                small_default,
            ),
            102_400,
        )
        self.assertEqual(
            bench.effective_current_loop_threshold(
                large_limits,
                config,
                guarded,
                large_default,
            ),
            235_929,
        )

    def test_recoverable_policy_sweep_candidates_include_balanced_baseline(self):
        policies = recoverable_policy_sweep.build_candidate_policies()
        names = [policy.name for policy in policies]

        self.assertEqual(names.count("duckagent_recoverable_decay_balanced"), 1)
        self.assertIn("sweep_recoverable_decay_p18k_h1500_t0000", names)
        self.assertIn("sweep_recoverable_decay_p15k_h2000_t0500", names)
        self.assertIn("duckagent_recoverable_decay_adaptive_guarded", names)

    def test_recoverable_policy_sweep_classification(self):
        baseline = bench.SimulationMetrics(
            workload="x",
            policy="baseline",
            total_cost=10.0,
            expected_recovery_input_tokens=1000.0,
            expected_extra_tool_calls=5.0,
        )
        dominant = bench.SimulationMetrics(
            workload="x",
            policy="dominant",
            total_cost=9.0,
            expected_recovery_input_tokens=900.0,
            expected_extra_tool_calls=4.0,
        )
        cost_only = bench.SimulationMetrics(
            workload="x",
            policy="cost_only",
            total_cost=9.0,
            expected_recovery_input_tokens=2000.0,
            expected_extra_tool_calls=8.0,
        )

        self.assertEqual(
            recoverable_policy_sweep.classify_candidate(dominant, baseline),
            "dominates",
        )
        self.assertEqual(
            recoverable_policy_sweep.classify_candidate(cost_only, baseline),
            "cost_only",
        )

    def test_source_inspired_tool_projection_modes(self):
        process_tool = bench.make_tool(
            "large process output",
            "process_read",
            80_000,
            exact_excerpt_tokens=1_200,
            summary_tokens=580,
            handle_tokens=140,
            preserve="reference",
        )
        search_tool = bench.make_tool(
            "large search output",
            "rg",
            80_000,
            exact_excerpt_tokens=1_200,
            summary_tokens=580,
            handle_tokens=140,
            preserve="reference",
        )
        read_file_tool = bench.make_tool(
            "large file",
            "read_file",
            80_000,
            exact_excerpt_tokens=1_500,
            summary_tokens=320,
            handle_tokens=100,
            preserve="working_evidence",
        )

        process_tokens, process_exact = bench.projected_tool_tokens(
            process_tool,
            "hermes_tool",
            0,
            None,
        )
        search_tokens, search_exact = bench.projected_tool_tokens(
            search_tool,
            "hermes_tool",
            0,
            None,
        )
        read_tokens, read_exact = bench.projected_tool_tokens(
            read_file_tool,
            "hermes_tool",
            0,
            None,
        )
        openclaw_tokens, openclaw_exact = bench.projected_tool_tokens(
            read_file_tool,
            "openclaw_live",
            0,
            None,
        )

        self.assertEqual(process_tokens, bench.HERMES_PROCESS_OUTPUT_CAP_TOKENS)
        self.assertFalse(process_exact)
        self.assertLess(search_tokens, search_tool.raw_tokens)
        self.assertFalse(search_exact)
        self.assertEqual(read_tokens, read_file_tool.raw_tokens)
        self.assertTrue(read_exact)
        self.assertEqual(openclaw_tokens, bench.OPENCLAW_LIVE_TOOL_RESULT_TOKENS)
        self.assertFalse(openclaw_exact)

    def test_recoverable_projection_keeps_file_handle_when_exact_budget_is_exhausted(self):
        read_file_tool = bench.make_tool(
            "large file",
            "read_file",
            80_000,
            exact_excerpt_tokens=1_500,
            summary_tokens=320,
            handle_tokens=100,
            preserve="working_evidence",
        )
        process_tool = bench.make_tool(
            "large process output",
            "process_read",
            80_000,
            exact_excerpt_tokens=1_200,
            summary_tokens=580,
            handle_tokens=140,
            preserve="reference",
        )

        file_tokens, file_exact = bench.projected_tool_tokens(
            read_file_tool,
            "recoverable",
            0,
            0,
        )
        process_tokens, process_exact = bench.projected_tool_tokens(
            process_tool,
            "recoverable",
            0,
            0,
        )

        self.assertEqual(file_tokens, read_file_tool.handle_tokens)
        self.assertFalse(file_exact)
        self.assertEqual(process_tokens, process_tool.summary_tokens + process_tool.handle_tokens)
        self.assertFalse(process_exact)

    def test_cache_rounds_common_prefix_to_step(self):
        cache = bench.CacheConfig(min_tokens=1024, step_tokens=128)

        self.assertEqual(bench.billable_cached_tokens(1023, cache), 0)
        self.assertEqual(bench.billable_cached_tokens(1024, cache), 1024)
        self.assertEqual(bench.billable_cached_tokens(1151, cache), 1024)
        self.assertEqual(bench.billable_cached_tokens(1152, cache), 1152)

    def test_immediate_summary_compresses_tool_results(self):
        pricing, cache, limits, config, workloads = self.load_default()
        workload = next(item for item in workloads if item.name == "code_compare_two_files")

        result = bench.simulate_workload(
            workload,
            self.policy("immediate_summary"),
            pricing,
            cache,
            limits,
            config,
        )

        self.assertEqual(result.completed_user_turn_count, len(workload.loops))
        self.assertGreater(result.tool_structured_compression_events, 0)
        self.assertGreater(result.tool_structured_compression_saved_tokens, 0)
        self.assertEqual(result.limit_violation_count, 0)

    def test_policy_projection_preserves_more_exact_evidence_than_summary(self):
        pricing, cache, limits, config, workloads = self.load_default()
        workload = next(item for item in workloads if item.name == "code_compare_two_files")
        summary = bench.simulate_workload(
            workload,
            self.policy("immediate_summary"),
            pricing,
            cache,
            limits,
            config,
        )
        policy = bench.simulate_workload(
            workload,
            self.policy("adaptive_first"),
            pricing,
            cache,
            limits,
            config,
        )

        self.assertGreater(summary.expected_recovery_input_tokens, policy.expected_recovery_input_tokens)
        self.assertGreater(policy.tool_visible_tokens, summary.tool_visible_tokens)

    def test_loop_boundary_summary_keeps_current_loop_raw_evidence(self):
        pricing, cache, limits, config, _workloads = self.load_default()
        workload = bench.Workload(
            name="current_loop_compare",
            description="A single turn that must compare raw file evidence before final answer.",
            loops=[
                bench.BenchLoop(
                    name="compare_two_files",
                    user_tokens=120,
                    assistant_final_tokens=360,
                    tool_calls=[
                        bench.make_tool(
                            "read 1.js",
                            "read_file",
                            9_000,
                            exact_excerpt_tokens=1_200,
                            summary_tokens=240,
                            handle_tokens=80,
                            preserve="working_evidence",
                            recovery_tokens={"exact_evidence": 1_400, "local_detail": 800},
                        ),
                        bench.make_tool(
                            "read 2.js",
                            "read_file",
                            8_500,
                            exact_excerpt_tokens=1_100,
                            summary_tokens=240,
                            handle_tokens=80,
                            preserve="working_evidence",
                            recovery_tokens={"exact_evidence": 1_400, "local_detail": 800},
                        ),
                    ],
                    dependency_probabilities=bench.normalize_dependency_probabilities({"none": 1.0}),
                    final_dependency_probabilities=bench.normalize_dependency_probabilities(
                        {"exact_evidence": 1.0}
                    ),
                )
            ],
        )

        immediate = bench.simulate_workload(
            workload,
            self.policy("immediate_summary"),
            pricing,
            cache,
            limits,
            config,
        )
        boundary = bench.simulate_workload(
            workload,
            self.policy("loop_boundary_summary"),
            pricing,
            cache,
            limits,
            config,
        )

        self.assertGreater(immediate.expected_recovery_input_tokens, 0)
        self.assertEqual(boundary.expected_recovery_input_tokens, 0)
        self.assertGreater(boundary.input_tokens, immediate.input_tokens)

    def test_loop_boundary_evidence_summary_retains_some_history_evidence(self):
        pricing, cache, limits, config, _workloads = self.load_default()
        workload = bench.Workload(
            name="history_evidence_followup",
            description="A later turn asks for exact evidence from the previous loop.",
            loops=[
                bench.BenchLoop(
                    name="read_files",
                    user_tokens=120,
                    assistant_final_tokens=320,
                    tool_calls=[
                        bench.make_tool(
                            "read 1.js",
                            "read_file",
                            9_000,
                            exact_excerpt_tokens=1_200,
                            summary_tokens=240,
                            handle_tokens=80,
                            preserve="working_evidence",
                            recovery_tokens={"exact_evidence": 1_400},
                        ),
                        bench.make_tool(
                            "read 2.js",
                            "read_file",
                            8_500,
                            exact_excerpt_tokens=1_100,
                            summary_tokens=240,
                            handle_tokens=80,
                            preserve="working_evidence",
                            recovery_tokens={"exact_evidence": 1_400},
                        ),
                    ],
                    dependency_probabilities=bench.normalize_dependency_probabilities({"none": 1.0}),
                ),
                bench.BenchLoop(
                    name="followup_compare",
                    user_tokens=90,
                    assistant_final_tokens=260,
                    tool_calls=[],
                    dependency_probabilities=bench.normalize_dependency_probabilities({"none": 1.0}),
                    raw_dependency_events=[
                        bench.RawDependencyEvent(
                            source_loop_offset=1,
                            dependency="exact_evidence",
                            probability=1.0,
                        )
                    ],
                ),
            ],
        )

        summary = bench.simulate_workload(
            workload,
            self.policy("loop_boundary_summary"),
            pricing,
            cache,
            limits,
            config,
        )
        evidence_summary = bench.simulate_workload(
            workload,
            self.policy("loop_boundary_evidence_summary"),
            pricing,
            cache,
            limits,
            config,
        )

        self.assertGreater(summary.expected_recovery_input_tokens, 0)
        self.assertEqual(evidence_summary.expected_recovery_input_tokens, 0)
        self.assertGreater(evidence_summary.tool_visible_tokens, summary.tool_visible_tokens)

    def test_duckagent_recoverable_boundary_uses_less_history_than_evidence_summary(self):
        pricing, cache, limits, config, _workloads = self.load_default()
        workload = bench.long_run_workload(60)

        evidence_summary = bench.simulate_workload(
            workload,
            self.policy("loop_boundary_evidence_summary"),
            pricing,
            cache,
            limits,
            config,
        )
        recoverable = bench.simulate_workload(
            workload,
            self.policy("duckagent_recoverable_boundary"),
            pricing,
            cache,
            limits,
            config,
        )

        self.assertEqual(recoverable.completed_user_turn_count, 60)
        self.assertLess(recoverable.tool_visible_tokens, evidence_summary.tool_visible_tokens)
        self.assertGreaterEqual(
            recoverable.expected_recovery_input_tokens,
            evidence_summary.expected_recovery_input_tokens,
        )

    def test_duckagent_recoverable_decay_recent2_protects_one_extra_recent_loop(self):
        pricing, cache, _limits, config, _workloads = self.load_default()
        limits = bench.LimitConfig(input_tokens=20_000, output_tokens=8_192)
        config.context_threshold_ratio = 0.30
        config.context_target_ratio = 0.20
        read_tool = lambda name: bench.make_tool(
            name,
            "read_file",
            8_000,
            exact_excerpt_tokens=1_000,
            summary_tokens=220,
            handle_tokens=80,
            preserve="working_evidence",
            recovery_tokens={"exact_evidence": 1_200},
        )
        workload = bench.Workload(
            name="recent_window_decay",
            description="A third turn asks for exact evidence from two turns ago under pressure.",
            loops=[
                bench.BenchLoop(
                    name="read_first",
                    user_tokens=100,
                    assistant_final_tokens=260,
                    tool_calls=[read_tool("read first")],
                    dependency_probabilities=bench.normalize_dependency_probabilities({"none": 1.0}),
                ),
                bench.BenchLoop(
                    name="read_second",
                    user_tokens=100,
                    assistant_final_tokens=260,
                    tool_calls=[read_tool("read second")],
                    dependency_probabilities=bench.normalize_dependency_probabilities({"none": 1.0}),
                ),
                bench.BenchLoop(
                    name="ask_about_first",
                    user_tokens=120,
                    assistant_final_tokens=300,
                    tool_calls=[],
                    dependency_probabilities=bench.normalize_dependency_probabilities({"none": 1.0}),
                    raw_dependency_events=[
                        bench.RawDependencyEvent(
                            source_loop_offset=2,
                            dependency="exact_evidence",
                            probability=1.0,
                        )
                    ],
                ),
            ],
        )

        baseline = bench.simulate_workload(
            workload,
            self.policy("duckagent_recoverable_decay"),
            pricing,
            cache,
            limits,
            config,
        )
        recent2 = bench.simulate_workload(
            workload,
            self.policy("duckagent_recoverable_decay_recent2"),
            pricing,
            cache,
            limits,
            config,
        )

        self.assertGreater(baseline.expected_recovery_input_tokens, 0)
        self.assertEqual(recent2.expected_recovery_input_tokens, 0)
        self.assertGreater(recent2.input_tokens, baseline.input_tokens)

    def test_threshold_policy_uses_llm_compaction_before_limit(self):
        pricing, cache, _limits, config, _workloads = self.load_default()
        limits = bench.LimitConfig(context_tokens=128_000, output_tokens=8_192)
        config.context_threshold_ratio = 0.7
        config.context_target_ratio = 0.3
        workload = bench.long_run_workload(120)

        result = bench.simulate_workload(
            workload,
            self.policy("evidence_budget"),
            pricing,
            cache,
            limits,
            config,
        )

        self.assertEqual(result.completed_user_turn_count, 120)
        self.assertGreater(result.llm_compaction_events, 0)
        self.assertGreater(result.llm_compacted_items, result.llm_compaction_events)
        self.assertGreater(result.llm_compaction_cost, 0)
        self.assertEqual(result.limit_violation_count, 0)
        self.assertLessEqual(result.max_context_tokens, limits.prompt_limit_tokens)

    def test_raw_threshold_llm_keeps_tools_raw_then_compacts_history(self):
        pricing, cache, _limits, config, _workloads = self.load_default()
        limits = bench.LimitConfig(context_tokens=128_000, output_tokens=8_192)
        config.context_threshold_ratio = 0.8
        config.context_target_ratio = 0.3
        workload = bench.long_run_workload(80)

        result = bench.simulate_workload(
            workload,
            self.policy("raw_snapshot"),
            pricing,
            cache,
            limits,
            config,
        )

        self.assertEqual(result.completed_user_turn_count, 80)
        self.assertGreater(result.llm_compaction_events, 0)
        self.assertGreater(result.llm_compacted_items, result.llm_compaction_events)
        self.assertLess(result.tool_structured_compression_events, result.tool_raw_tokens)
        self.assertEqual(result.limit_violation_count, 0)

    def test_long_run_policies_complete_requested_turns(self):
        pricing, cache, _limits, config, _workloads = self.load_default()
        limits = bench.LimitConfig(context_tokens=128_000, output_tokens=8_192)
        config.context_threshold_ratio = 0.8
        config.context_target_ratio = 0.3
        workload = bench.long_run_workload(220)

        results = bench.run_benchmark(
            [workload],
            bench.POLICIES,
            pricing,
            cache,
            limits,
            config,
        )

        for result in results:
            self.assertEqual(result.completed_user_turn_count, 220, result.policy)
            self.assertEqual(result.limit_violation_count, 0, result.policy)

    def test_source_inspired_policies_complete_requested_turns(self):
        pricing, cache, _limits, config, _workloads = self.load_default()
        limits = bench.LimitConfig(context_tokens=128_000, output_tokens=8_192)
        workload = bench.long_run_workload(180)
        policies = [self.policy("early_windowed_prune"), self.policy("late_guarded_truncation")]

        results = bench.run_benchmark([workload], policies, pricing, cache, limits, config)

        for result in results:
            self.assertEqual(result.completed_user_turn_count, 180, result.policy)
            self.assertEqual(result.limit_violation_count, 0, result.policy)
            self.assertGreater(result.tool_structured_compression_events, 0, result.policy)

    def test_llm_compaction_requests_are_batched(self):
        pricing, cache, _limits, config, _workloads = self.load_default()
        limits = bench.LimitConfig(context_tokens=128_000, output_tokens=8_192)
        config.context_threshold_ratio = 0.8
        config.context_target_ratio = 0.3
        workload = bench.long_run_workload(220)

        result = bench.simulate_workload(
            workload,
            self.policy("immediate_summary"),
            pricing,
            cache,
            limits,
            config,
        )

        self.assertEqual(result.completed_user_turn_count, 220)
        self.assertLess(result.llm_compaction_events, 40)
        self.assertGreater(result.llm_compacted_items, result.llm_compaction_events)

    def test_raw_dependency_events_add_recovery_tokens_when_raw_is_missing(self):
        pricing, cache, limits, config, workloads = self.load_default()
        workload = next(item for item in workloads if item.name == "code_compare_two_files")

        result = bench.simulate_workload(
            workload,
            self.policy("immediate_summary"),
            pricing,
            cache,
            limits,
            config,
        )

        self.assertGreater(result.expected_recovery_input_tokens, 0)
        self.assertGreater(result.expected_extra_tool_calls, 0)

    def test_minimal_pinned_recovery_prevents_read_compact_read_oscillation(self):
        pricing, cache, _limits, config, _workloads = self.load_default()
        limits = bench.LimitConfig(context_tokens=128_000, output_tokens=8_192)
        workload = bench.validation_recovery_oscillation_work(scale=1)

        naive = bench.simulate_workload(
            workload,
            self.policy("duckagent_recoverable_decay_guarded_mid_naive_recovery"),
            pricing,
            cache,
            limits,
            config,
        )
        pinned = bench.simulate_workload(
            workload,
            self.policy("duckagent_recoverable_decay_guarded_mid"),
            pricing,
            cache,
            limits,
            config,
        )

        self.assertGreater(naive.expected_repeat_recovery_tool_calls, 0)
        self.assertEqual(pinned.expected_repeat_recovery_tool_calls, 0)
        self.assertGreater(
            naive.expected_recovery_input_tokens,
            pinned.expected_recovery_input_tokens,
        )

    def test_stress_workloads_cover_long_runs_and_many_tools(self):
        workloads = bench.stress_workloads(scale=1)

        self.assertGreaterEqual(sum(len(workload.loops) for workload in workloads), 90)
        self.assertGreaterEqual(
            max(len(loop.tool_calls) for workload in workloads for loop in workload.loops),
            28,
        )

    def test_validation_workloads_are_holdout_like_and_complete(self):
        pricing, cache, _limits, config, _workloads = self.load_default()
        limits = bench.LimitConfig(context_tokens=128_000, output_tokens=8_192)
        workloads = bench.validation_workloads(scale=1)

        self.assertEqual(len(workloads), 5)
        self.assertTrue(any(workload.name == "validation_recovery_oscillation_work" for workload in workloads))
        self.assertGreaterEqual(sum(len(workload.loops) for workload in workloads), 90)
        self.assertTrue(any(not loop.tool_calls for loop in workloads[0].loops))
        self.assertGreaterEqual(
            max(len(loop.tool_calls) for workload in workloads for loop in workload.loops),
            36,
        )

        results = bench.run_benchmark(
            workloads,
            [
                self.policy("duckagent_recoverable_decay_balanced"),
                self.policy("duckagent_recoverable_decay_adaptive"),
            ],
            pricing,
            cache,
            limits,
            config,
        )

        for result in results:
            self.assertEqual(result.limit_violation_count, 0, result.policy)
            self.assertEqual(
                result.completed_user_turn_count,
                result.requested_user_turn_count,
                result.policy,
            )

    def test_models_dev_profile_maps_cost_cache_and_limits(self):
        args = type(
            "Args",
            (),
            {
                "name": "example",
                "url": "https://models.dev/api.json",
                "context_threshold_ratio": 0.5,
                "context_threshold_tokens": None,
                "recovery_latency_penalty_ms": 900.0,
                "request_latency_ms": 650.0,
                "cache_min_tokens": 1024,
                "cache_step_tokens": 128,
                "cache_ttl_turns": 1000,
                "failure_penalty_cost": 0.02,
            },
        )()
        profile = models_dev_profile.build_profile(
            "provider",
            "model",
            {
                "name": "Model",
                "cost": {"input": 2.5, "output": 15.0, "cache_read": 0.25},
                "limit": {"context": 1050000, "input": 922000, "output": 128000},
            },
            args,
        )

        self.assertEqual(profile["pricing"]["cached_input_per_million"], 0.25)
        self.assertEqual(profile["limits"]["context_tokens"], 1050000)
        self.assertEqual(profile["limits"]["input_tokens"], 922000)
        self.assertEqual(profile["limits"]["output_tokens"], 128000)
        self.assertEqual(profile["simulation"]["context_threshold_tokens"], 461000)

    def test_aggregate_sorts_by_total_cost(self):
        pricing, cache, limits, config, workloads = self.load_default()
        results = bench.run_benchmark(workloads, bench.POLICIES, pricing, cache, limits, config)
        aggregate = bench.aggregate_results(results)
        costs = [metric.total_cost for metric in aggregate]

        self.assertEqual(costs, sorted(costs))

    def test_markdown_contains_direct_metrics(self):
        pricing, cache, limits, config, workloads = self.load_default()
        results = bench.run_benchmark(
            workloads[:1],
            bench.POLICIES[:2],
            pricing,
            cache,
            limits,
            config,
        )
        markdown = bench.format_markdown(results, pricing.currency)

        self.assertIn("## Aggregate", markdown)
        self.assertIn("tool struct comp", markdown)
        self.assertIn("LLM comp reqs", markdown)
        self.assertIn("LLM comp items", markdown)
        self.assertIn("plain_chat", markdown)

    def test_matrix_columns_prioritize_policy_cache_and_cost(self):
        labels = [label for _, label in model_matrix.MATRIX_COLUMNS]

        self.assertEqual(
            labels[:5],
            ["policy", "model", "turns complete/requested", "cache hit", "total cost"],
        )
        self.assertIn("base LLM reqs", labels)
        self.assertIn("expected LLM reqs", labels)
        self.assertNotIn("failed at", labels)
        self.assertNotIn("fail request", labels)
        self.assertNotIn("limit fails", labels)
        self.assertIn("LLM comp items", labels)

    def test_report_utils_write_markdown_html_and_json(self):
        markdown = "# Report\n\n| a | b |\n|---|---:|\n| x | 1 |"
        html = render_html_report("Report", markdown)
        with tempfile.TemporaryDirectory() as tmp:
            md_path, html_path, json_path = write_report_files(
                Path(tmp),
                markdown,
                html,
                {"ok": True},
            )

            self.assertTrue(md_path.exists())
            self.assertTrue(html_path.exists())
            self.assertTrue(json_path.exists())
            self.assertIn("<table>", html_path.read_text(encoding="utf-8"))
            self.assertIn('"ok": true', json_path.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
