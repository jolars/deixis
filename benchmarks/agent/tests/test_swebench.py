import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import tomllib

MODULE_PATH = Path(__file__).resolve().parents[1] / "swebench.py"
SPEC = importlib.util.spec_from_file_location("deixis_swebench", MODULE_PATH)
assert SPEC is not None
assert SPEC.loader is not None
swebench = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = swebench
SPEC.loader.exec_module(swebench)


class CargoGradingTests(unittest.TestCase):
    def test_grades_a_resolved_cargo_run(self) -> None:
        log = """
>>>>> Start Test Output
test regression_case ... ok
test existing_case ... ok
>>>>> End Test Output
"""

        report = swebench.grade_cargo_log(
            log,
            fail_to_pass=("regression_case",),
            pass_to_pass=("existing_case",),
        )

        self.assertTrue(report["valid"])
        self.assertTrue(report["resolved"])
        self.assertEqual(report["fail_to_pass"]["passed"], ["regression_case"])
        self.assertEqual(report["pass_to_pass"]["passed"], ["existing_case"])

    def test_missing_or_failed_cases_do_not_resolve(self) -> None:
        log = """
>>>>> Start Test Output
test regression_case ... FAILED
test other_case ... ok
>>>>> End Test Output
"""

        report = swebench.grade_cargo_log(
            log,
            fail_to_pass=("regression_case",),
            pass_to_pass=("existing_case",),
        )

        self.assertTrue(report["valid"])
        self.assertFalse(report["resolved"])
        self.assertEqual(report["fail_to_pass"]["failed"], ["regression_case"])
        self.assertEqual(report["pass_to_pass"]["missing"], ["existing_case"])

    def test_rejects_output_without_official_markers(self) -> None:
        report = swebench.grade_cargo_log(
            "test regression_case ... ok\n",
            fail_to_pass=("regression_case",),
            pass_to_pass=(),
        )

        self.assertFalse(report["valid"])
        self.assertFalse(report["resolved"])

    def test_recognizes_a_valid_unsolved_base(self) -> None:
        log = """
>>>>> Start Test Output
test regression_case ... FAILED
test existing_case ... ok
>>>>> End Test Output
"""

        report = swebench.grade_cargo_log(
            log,
            fail_to_pass=("regression_case",),
            pass_to_pass=("existing_case",),
        )

        self.assertTrue(swebench.is_valid_base_report(report))

    def test_base_requires_the_regression_test_to_run(self) -> None:
        log = """
>>>>> Start Test Output
test existing_case ... ok
>>>>> End Test Output
"""
        report = swebench.grade_cargo_log(
            log,
            fail_to_pass=("regression_case",),
            pass_to_pass=("existing_case",),
        )

        self.assertFalse(swebench.is_valid_base_report(report))


class PatchTests(unittest.TestCase):
    def test_candidate_patch_includes_commits_and_untracked_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            repository = Path(temporary_directory)
            subprocess.run(
                ["git", "init", "-q", "-b", "benchmark"],
                cwd=repository,
                check=True,
            )
            subprocess.run(
                ["git", "config", "user.name", "Benchmark"],
                cwd=repository,
                check=True,
            )
            subprocess.run(
                ["git", "config", "user.email", "benchmark@example.invalid"],
                cwd=repository,
                check=True,
            )
            (repository / "tracked.txt").write_text("before\n", encoding="utf-8")
            subprocess.run(["git", "add", "."], cwd=repository, check=True)
            subprocess.run(
                ["git", "commit", "-q", "-m", "base"],
                cwd=repository,
                check=True,
            )
            base = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=repository,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            (repository / "tracked.txt").write_text("after\n", encoding="utf-8")
            subprocess.run(["git", "add", "tracked.txt"], cwd=repository, check=True)
            subprocess.run(
                ["git", "commit", "-q", "-m", "agent commit"],
                cwd=repository,
                check=True,
            )
            (repository / "new.txt").write_text("new\n", encoding="utf-8")

            patch = swebench.candidate_patch(repository, base)

            self.assertIn("tracked.txt", patch)
            self.assertIn("new.txt", patch)
            self.assertIn("+after", patch)
            self.assertIn("+new", patch)


class SnapshotTests(unittest.TestCase):
    def test_rejects_an_image_at_the_wrong_base_commit(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            destination = Path(temporary_directory) / "snapshot"
            with (
                patch.object(swebench, "_run_checked", return_value="b" * 40),
                self.assertRaisesRegex(swebench.SweBenchError, "expected task base"),
            ):
                swebench._extract_snapshot("sha256:" + "c" * 64, destination, "a" * 40)

            self.assertFalse(destination.exists())


class ManifestTests(unittest.TestCase):
    def test_renders_a_manifest_accepted_by_tomllib(self) -> None:
        task = {
            "instance_id": "owner__repo-1",
            "repo": "owner/repo",
            "snapshot_commit": "abc123",
        }
        rendered = swebench.render_benchmark_manifest(
            tasks=[task],
            snapshots=Path("/data/snapshots"),
            assets=Path("/data/assets"),
            adapter=Path("/src/swebench.py"),
            deixis_command=Path("/src/target/release/deixis"),
            deixis_config=Path("/data/deixis-rust.toml"),
        )

        parsed = tomllib.loads(rendered)

        self.assertEqual(parsed["schema_version"], 1)
        self.assertEqual(len(parsed["tasks"]), 1)
        self.assertEqual(parsed["tasks"][0]["revision"], "abc123")
        self.assertEqual(
            parsed["tasks"][0]["evaluation_command"],
            [
                "python3",
                "/src/swebench.py",
                "evaluate",
                "--task",
                "/data/assets/owner__repo-1/task.json",
            ],
        )


class MetadataTests(unittest.TestCase):
    def test_load_task_metadata_rejects_an_unpinned_image(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            path = Path(temporary_directory) / "task.json"
            path.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "instance_id": "owner__repo-1",
                        "repo": "owner/repo",
                        "base_commit": "a" * 40,
                        "snapshot_commit": "b" * 40,
                        "image": "example:latest",
                        "image_id": "not-a-digest",
                        "log_parser": "parse_log_cargo",
                        "eval_type": "pass_and_fail",
                        "FAIL_TO_PASS": ["regression"],
                        "PASS_TO_PASS": ["existing"],
                        "source_repository": "https://github.com/example/tasks",
                        "source_revision": "c" * 40,
                        "asset_sha256": {
                            name: "d" * 64 for name in swebench.TASK_ASSETS
                        },
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(swebench.SweBenchError, "image_id"):
                swebench.load_task_metadata(path)


if __name__ == "__main__":
    unittest.main()
