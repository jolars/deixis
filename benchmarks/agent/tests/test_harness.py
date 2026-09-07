import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from harness import (
    ARMS,
    BenchmarkConfig,
    BenchmarkError,
    Task,
    _run_codex,
    _run_trial,
    _task_environment,
    _validate_codex_home,
    build_codex_command,
    build_schedule,
    collect_event_metrics,
    load_manifest,
    summarize_results,
)


class ManifestTests(unittest.TestCase):
    def test_loads_paths_relative_to_the_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            repository = root / "repository"
            repository.mkdir()
            (root / "deixis").touch()
            (root / "deixis.toml").touch()
            manifest = root / "benchmark.toml"
            manifest.write_text(
                """
schema_version = 1
model = "test-model"
reasoning_effort = "medium"
repetitions = 2
seed = 17
agent_timeout_seconds = 900
evaluation_timeout_seconds = 300
deixis_command = "deixis"
deixis_config = "deixis.toml"

[[tasks]]
id = "example"
repository = "repository"
revision = "HEAD"
prompt = "Fix the bug."
evaluation_command = ["cargo", "test"]
tags = ["rust", "semantic"]
""".strip()
                + "\n",
                encoding="utf-8",
            )

            config = load_manifest(manifest)

            self.assertEqual(config.model, "test-model")
            self.assertEqual(config.repetitions, 2)
            self.assertEqual(config.deixis_command, root / "deixis")
            self.assertEqual(config.deixis_config, root / "deixis.toml")
            self.assertEqual(config.tasks[0].repository, repository)
            self.assertEqual(config.tasks[0].tags, ("rust", "semantic"))


class ScheduleTests(unittest.TestCase):
    def test_schedule_is_balanced_and_reproducible(self) -> None:
        tasks = (
            Task("one", Path("/repo/one"), "HEAD", "One", ("test",)),
            Task("two", Path("/repo/two"), "HEAD", "Two", ("test",)),
        )

        first = build_schedule(tasks, tuple(ARMS), repetitions=3, seed=42)
        second = build_schedule(tasks, tuple(ARMS), repetitions=3, seed=42)

        self.assertEqual(first, second)
        self.assertEqual(len(first), 2 * 3 * len(ARMS))
        for task in tasks:
            for repetition in range(1, 4):
                observed = {
                    trial.arm.name
                    for trial in first
                    if trial.task.id == task.id and trial.repetition == repetition
                }
                self.assertEqual(observed, {arm.name for arm in ARMS})


class CommandTests(unittest.TestCase):
    def setUp(self) -> None:
        self.config = BenchmarkConfig(
            model="test-model",
            reasoning_effort="high",
            repetitions=1,
            seed=1,
            agent_timeout_seconds=900,
            evaluation_timeout_seconds=300,
            codex_command="codex",
            deixis_command=Path("/opt/deixis"),
            deixis_config=Path("/opt/deixis.toml"),
            instruction="Use typed LSP tools when available.",
            arms=tuple(ARMS),
            tasks=(),
        )

    def test_control_has_no_mcp_or_treatment_instruction(self) -> None:
        command = build_codex_command(self.config, ARMS[0], Path("/work/tree"))
        rendered = "\n".join(command)

        self.assertIn("--ignore-user-config", command)
        self.assertIn("--ephemeral", command)
        self.assertNotIn("mcp_servers.deixis", rendered)
        self.assertNotIn("developer_instructions", rendered)

    def test_instructed_deixis_arm_configures_both_factors(self) -> None:
        command = build_codex_command(self.config, ARMS[3], Path("/work/tree"))
        rendered = "\n".join(command)

        self.assertIn('mcp_servers.deixis.command="/opt/deixis"', rendered)
        self.assertIn(
            'mcp_servers.deixis.args=["--root", "/work/tree", '
            '"--config", "/opt/deixis.toml"]',
            rendered,
        )
        self.assertIn("mcp_servers.deixis.required=true", rendered)
        self.assertIn(
            'developer_instructions="Use typed LSP tools when available."',
            rendered,
        )

    def test_command_forces_chatgpt_authentication(self) -> None:
        command = build_codex_command(self.config, ARMS[0], Path("/work/tree"))

        self.assertIn('forced_login_method="chatgpt"', command)

    def test_environment_removes_usage_billed_api_credentials(self) -> None:
        task = Task("one", Path("/repo"), "HEAD", "Fix it", ("test",))

        with patch.dict(
            os.environ,
            {
                "OPENAI_API_KEY": "do-not-use",
                "AZURE_OPENAI_API_KEY": "do-not-use",
                "CODEX_API_KEY": "do-not-use",
            },
        ):
            environment = _task_environment(self.config, task, Path("/codex-home"))

        self.assertNotIn("OPENAI_API_KEY", environment)
        self.assertNotIn("AZURE_OPENAI_API_KEY", environment)
        self.assertNotIn("CODEX_API_KEY", environment)
        self.assertEqual(environment["CODEX_HOME"], "/codex-home")

    def test_codex_home_requires_chatgpt_authentication(self) -> None:
        with (
            tempfile.TemporaryDirectory() as temporary_directory,
            patch("harness.subprocess.run") as run,
        ):
            run.return_value = subprocess.CompletedProcess(
                args=["codex", "login", "status"],
                returncode=0,
                stdout="Logged in using an API key\n",
                stderr="",
            )

            with self.assertRaisesRegex(BenchmarkError, "ChatGPT"):
                _validate_codex_home(Path(temporary_directory), "codex")

    def test_authentication_check_hides_api_credentials(self) -> None:
        with (
            tempfile.TemporaryDirectory() as temporary_directory,
            patch.dict(
                os.environ,
                {
                    "OPENAI_API_KEY": "do-not-use",
                    "CODEX_API_KEY": "do-not-use",
                },
            ),
            patch("harness.subprocess.run") as run,
        ):
            run.return_value = subprocess.CompletedProcess(
                args=["codex", "login", "status"],
                returncode=0,
                stdout="Logged in using ChatGPT\n",
                stderr="",
            )

            _validate_codex_home(Path(temporary_directory), "codex")

        environment = run.call_args.kwargs["env"]
        self.assertNotIn("OPENAI_API_KEY", environment)
        self.assertNotIn("CODEX_API_KEY", environment)
        self.assertEqual(environment["CODEX_HOME"], temporary_directory)


class EventMetricTests(unittest.TestCase):
    def test_extracts_usage_and_elapsed_tool_metrics(self) -> None:
        events = [
            {
                "elapsed_seconds": 1.0,
                "event": {
                    "type": "item.started",
                    "item": {
                        "id": "call-1",
                        "type": "mcp_tool_call",
                        "server": "deixis",
                        "tool": "definition",
                    },
                },
            },
            {
                "elapsed_seconds": 1.75,
                "event": {
                    "type": "item.completed",
                    "item": {
                        "id": "call-1",
                        "type": "mcp_tool_call",
                        "server": "deixis",
                        "tool": "definition",
                        "status": "completed",
                    },
                },
            },
            {
                "elapsed_seconds": 2.5,
                "event": {
                    "type": "item.completed",
                    "item": {"id": "edit-1", "type": "file_change"},
                },
            },
            {
                "elapsed_seconds": 3.0,
                "event": {
                    "type": "turn.completed",
                    "usage": {
                        "input_tokens": 100,
                        "cached_input_tokens": 40,
                        "output_tokens": 20,
                        "reasoning_output_tokens": 5,
                    },
                },
            },
        ]

        metrics = collect_event_metrics(events)

        self.assertEqual(metrics["input_tokens"], 100)
        self.assertEqual(metrics["uncached_input_tokens"], 60)
        self.assertEqual(metrics["output_tokens"], 20)
        self.assertEqual(metrics["reasoning_output_tokens"], 5)
        self.assertEqual(metrics["mcp_call_count"], 1)
        self.assertEqual(metrics["mcp_failure_count"], 0)
        self.assertAlmostEqual(metrics["time_to_first_edit_seconds"], 2.5)
        self.assertEqual(
            metrics["mcp_calls"],
            [
                {
                    "id": "call-1",
                    "server": "deixis",
                    "tool": "definition",
                    "status": "completed",
                    "started_seconds": 1.0,
                    "completed_seconds": 1.75,
                    "elapsed_seconds": 0.75,
                }
            ],
        )

    def test_missing_usage_is_explicit(self) -> None:
        metrics = collect_event_metrics([])

        self.assertIsNone(metrics["input_tokens"])
        self.assertIsNone(metrics["uncached_input_tokens"])
        self.assertEqual(metrics["mcp_calls"], [])

    def test_codex_capture_timestamps_events_and_preserves_raw_jsonl(self) -> None:
        script = """
import json
import sys
sys.stdin.read()
print(json.dumps({"type": "turn.started"}), flush=True)
print(json.dumps({
    "type": "turn.completed",
    "usage": {
        "input_tokens": 12,
        "cached_input_tokens": 2,
        "output_tokens": 3,
        "reasoning_output_tokens": 1,
    },
}), flush=True)
"""
        with tempfile.TemporaryDirectory() as temporary_directory:
            artifacts = Path(temporary_directory)

            result = _run_codex(
                [sys.executable, "-c", script],
                "test prompt",
                artifacts,
                dict(os.environ),
                10,
                artifacts,
            )

            self.assertEqual(result["exit_code"], 0)
            self.assertFalse(result["timed_out"])
            self.assertEqual(result["input_tokens"], 12)
            self.assertEqual(result["uncached_input_tokens"], 10)
            raw_events = (artifacts / "events.jsonl").read_text(encoding="utf-8")
            timed_events = (artifacts / "events-timed.jsonl").read_text(
                encoding="utf-8"
            )
            self.assertIn('"type": "turn.completed"', raw_events)
            self.assertIn('"elapsed_seconds"', timed_events)


class SummaryTests(unittest.TestCase):
    def test_summarizes_arms_and_paired_effects(self) -> None:
        def result(arm: str, success: bool, wall: float, tokens: int):
            return {
                "task_id": "task",
                "repetition": 1,
                "arm": arm,
                "success": success,
                "codex": {
                    "wall_seconds": wall,
                    "timed_out": False,
                    "input_tokens": tokens,
                    "output_tokens": 10,
                    "mcp_call_count": 1 if "deixis" in arm else 0,
                },
            }

        summary = summarize_results(
            [
                result("control", False, 20.0, 90),
                result("deixis_available", True, 10.0, 40),
                result("instruction_only", False, 20.0, 90),
                result("deixis_instructed", True, 10.0, 40),
            ]
        )

        rows = {row["arm"]: row for row in summary["arms"]}
        self.assertEqual(rows["control"]["solve_rate"], 0.0)
        self.assertEqual(rows["deixis_available"]["solve_rate"], 1.0)
        comparisons = {
            comparison["name"]: comparison
            for comparison in summary["paired_comparisons"]
        }
        self.assertEqual(
            comparisons["availability_neutral"]["solve_rate_difference"],
            1.0,
        )
        self.assertEqual(
            comparisons["instruction_with_deixis"][
                "median_token_ratio_when_both_solved"
            ],
            1.0,
        )


class TrialTests(unittest.TestCase):
    def test_trial_uses_detached_worktree_and_captures_patch(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            repository = root / "repository"
            repository.mkdir()
            subprocess.run(["git", "init", "--quiet"], cwd=repository, check=True)
            subprocess.run(
                ["git", "config", "user.name", "Benchmark Test"],
                cwd=repository,
                check=True,
            )
            subprocess.run(
                ["git", "config", "user.email", "benchmark@example.invalid"],
                cwd=repository,
                check=True,
            )
            (repository / "value.txt").write_text("old\n", encoding="utf-8")
            subprocess.run(["git", "add", "value.txt"], cwd=repository, check=True)
            subprocess.run(
                ["git", "commit", "--quiet", "-m", "fixture"],
                cwd=repository,
                check=True,
            )
            commit = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=repository,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            task = Task(
                id="fixture",
                repository=repository,
                revision=commit,
                prompt="Change old to new.",
                evaluation_command=(
                    sys.executable,
                    "-c",
                    (
                        "from pathlib import Path; "
                        "raise SystemExit(Path('value.txt').read_text() != 'new\\n')"
                    ),
                ),
            )
            config = BenchmarkConfig(
                model="test-model",
                reasoning_effort="medium",
                repetitions=1,
                seed=1,
                agent_timeout_seconds=10,
                evaluation_timeout_seconds=10,
                codex_command="codex",
                deixis_command=root / "deixis",
                deixis_config=root / "deixis.toml",
                instruction="Use typed LSP tools when available.",
                arms=(ARMS[0],),
                tasks=(task,),
            )
            fake_codex = """
import json
import sys
from pathlib import Path
sys.stdin.read()
Path("value.txt").write_text("new\\n")
print(json.dumps({
    "type": "turn.completed",
    "usage": {
        "input_tokens": 12,
        "cached_input_tokens": 2,
        "output_tokens": 3,
        "reasoning_output_tokens": 1,
    },
}), flush=True)
"""
            output = root / "output"
            output.mkdir()
            codex_home = root / "codex-home"
            codex_home.mkdir()

            with patch(
                "harness.build_codex_command",
                return_value=[sys.executable, "-c", fake_codex],
            ):
                result = _run_trial(
                    config,
                    build_schedule((task,), (ARMS[0],), 1, 1)[0],
                    commit,
                    codex_home,
                    output,
                )

            self.assertTrue(result["success"])
            self.assertEqual(result["codex"]["input_tokens"], 12)
            patch_text = (Path(result["artifact_directory"]) / "patch.diff").read_text(
                encoding="utf-8"
            )
            self.assertIn("+new", patch_text)
            self.assertTrue((Path(result["artifact_directory"]) / "worktree").is_dir())


if __name__ == "__main__":
    unittest.main()
