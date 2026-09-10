#!/usr/bin/env python3
"""Prepare and grade the Deixis SWE-bench Multilingual Rust pilot."""

from __future__ import annotations

import argparse
import ast
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import tomllib

START_TEST_OUTPUT = ">>>>> Start Test Output"
END_TEST_OUTPUT = ">>>>> End Test Output"
TASK_ASSETS = (
    "Dockerfile",
    "eval.sh",
    "problem_statement.md",
    "task.yaml",
    "tests.json",
)
IMAGE_ID = re.compile(r"^sha256:[0-9a-f]{64}$")
CARGO_RESULT = re.compile(r"^test\s+(\S+)\s+\.\.\.\s+(\w+)$")
GIT_SHA = re.compile(r"^[0-9a-f]{40}$")


class SweBenchError(RuntimeError):
    """A benchmark fixture or evaluation failed."""


@dataclass(frozen=True)
class Suite:
    source_repository: str
    source_revision: str
    selection_seed: int
    selection_rule: str
    task_ids: tuple[str, ...]


def _require_keys(
    value: dict[str, Any], required: set[str], allowed: set[str], context: str
) -> None:
    missing = required - value.keys()
    unknown = value.keys() - allowed
    if missing:
        raise SweBenchError(f"{context} is missing keys: {', '.join(sorted(missing))}")
    if unknown:
        raise SweBenchError(f"{context} has unknown keys: {', '.join(sorted(unknown))}")


def load_suite(path: Path) -> Suite:
    try:
        with path.open("rb") as source:
            value = tomllib.load(source)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise SweBenchError(f"cannot load suite {path}: {error}") from error
    if not isinstance(value, dict):
        raise SweBenchError("suite must be a TOML table")
    keys = {
        "schema_version",
        "source_repository",
        "source_revision",
        "selection_seed",
        "selection_rule",
        "task_ids",
    }
    _require_keys(value, keys, keys, "suite")
    if value["schema_version"] != 1:
        raise SweBenchError("suite schema_version must be 1")
    if not isinstance(value["selection_seed"], int) or isinstance(
        value["selection_seed"], bool
    ):
        raise SweBenchError("suite selection_seed must be an integer")
    string_keys = ("source_repository", "source_revision", "selection_rule")
    if any(not isinstance(value[key], str) or not value[key] for key in string_keys):
        raise SweBenchError(
            "suite source and selection values must be nonempty strings"
        )
    task_ids = value["task_ids"]
    if (
        not isinstance(task_ids, list)
        or not task_ids
        or any(not isinstance(item, str) or not item for item in task_ids)
    ):
        raise SweBenchError("suite task_ids must be a nonempty string array")
    if len(task_ids) != len(set(task_ids)):
        raise SweBenchError("suite task_ids must be unique")
    if not GIT_SHA.fullmatch(value["source_revision"]):
        raise SweBenchError("suite source_revision must be a full Git commit")
    return Suite(
        source_repository=value["source_repository"].rstrip("/"),
        source_revision=value["source_revision"],
        selection_seed=value["selection_seed"],
        selection_rule=value["selection_rule"],
        task_ids=tuple(task_ids),
    )


def _task_metadata_keys() -> set[str]:
    return {
        "schema_version",
        "instance_id",
        "repo",
        "base_commit",
        "snapshot_commit",
        "image",
        "image_id",
        "log_parser",
        "eval_type",
        "FAIL_TO_PASS",
        "PASS_TO_PASS",
        "source_repository",
        "source_revision",
        "asset_sha256",
    }


def load_task_metadata(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SweBenchError(f"cannot load task metadata {path}: {error}") from error
    if not isinstance(value, dict):
        raise SweBenchError(f"task metadata {path} must be an object")
    keys = _task_metadata_keys()
    _require_keys(value, keys, keys, f"task metadata {path}")
    if value["schema_version"] != 1:
        raise SweBenchError(f"task metadata {path} has an unsupported schema")
    string_keys = keys - {
        "schema_version",
        "FAIL_TO_PASS",
        "PASS_TO_PASS",
        "asset_sha256",
    }
    if any(not isinstance(value[key], str) or not value[key] for key in string_keys):
        raise SweBenchError(f"task metadata {path} contains an invalid string")
    for key in ("base_commit", "snapshot_commit", "source_revision"):
        if not GIT_SHA.fullmatch(value[key]):
            raise SweBenchError(f"task metadata {path} has an invalid {key}")
    if not IMAGE_ID.fullmatch(value["image_id"]):
        raise SweBenchError(f"task metadata {path} has an invalid image_id")
    if value["log_parser"] != "parse_log_cargo":
        raise SweBenchError(f"task metadata {path} is not a Cargo task")
    if value["eval_type"] != "pass_and_fail":
        raise SweBenchError(f"task metadata {path} is not pass-and-fail")
    for key in ("FAIL_TO_PASS", "PASS_TO_PASS"):
        cases = value[key]
        if (
            not isinstance(cases, list)
            or not cases
            or any(not isinstance(item, str) or not item for item in cases)
        ):
            raise SweBenchError(f"task metadata {path} has invalid {key}")
    checksums = value["asset_sha256"]
    if not isinstance(checksums, dict) or set(checksums) != set(TASK_ASSETS):
        raise SweBenchError(f"task metadata {path} has invalid asset_sha256")
    if any(
        not isinstance(checksum, str) or not re.fullmatch(r"[0-9a-f]{64}", checksum)
        for checksum in checksums.values()
    ):
        raise SweBenchError(f"task metadata {path} has an invalid asset checksum")
    return value


def parse_cargo_log(log: str) -> dict[str, str]:
    statuses: dict[str, str] = {}
    for line in log.splitlines():
        match = CARGO_RESULT.match(line.strip())
        if match is None:
            continue
        name, outcome = match.groups()
        if outcome == "ok":
            statuses[name] = "PASSED"
        elif outcome == "FAILED":
            statuses[name] = "FAILED"
    return statuses


def _case_report(
    cases: Sequence[str], statuses: dict[str, str]
) -> dict[str, list[str]]:
    return {
        "passed": [case for case in cases if statuses.get(case) == "PASSED"],
        "failed": [case for case in cases if statuses.get(case) == "FAILED"],
        "missing": [case for case in cases if case not in statuses],
    }


def grade_cargo_log(
    log: str, fail_to_pass: Sequence[str], pass_to_pass: Sequence[str]
) -> dict[str, Any]:
    if START_TEST_OUTPUT not in log or END_TEST_OUTPUT not in log:
        return {
            "valid": False,
            "resolved": False,
            "reason": "official test-output markers are missing",
            "fail_to_pass": _case_report(fail_to_pass, {}),
            "pass_to_pass": _case_report(pass_to_pass, {}),
        }
    marked = log.split(START_TEST_OUTPUT, 1)[1].split(END_TEST_OUTPUT, 1)[0]
    statuses = parse_cargo_log(marked)
    if not statuses:
        statuses = parse_cargo_log(log)
    fail_report = _case_report(fail_to_pass, statuses)
    pass_report = _case_report(pass_to_pass, statuses)
    valid = bool(statuses)
    resolved = valid and not any(
        (
            fail_report["failed"],
            fail_report["missing"],
            pass_report["failed"],
            pass_report["missing"],
        )
    )
    return {
        "valid": valid,
        "resolved": resolved,
        "reason": None if valid else "Cargo emitted no recognized test results",
        "fail_to_pass": fail_report,
        "pass_to_pass": pass_report,
    }


def is_valid_base_report(report: dict[str, Any]) -> bool:
    return bool(
        report["valid"]
        and not report["fail_to_pass"]["passed"]
        and not report["fail_to_pass"]["missing"]
        and not report["pass_to_pass"]["failed"]
        and not report["pass_to_pass"]["missing"]
    )


def _git(repository: Path, arguments: Sequence[str], *, text: bool = True) -> str:
    completed = subprocess.run(
        ["git", "-C", str(repository), *arguments],
        check=False,
        capture_output=True,
        text=text,
    )
    if completed.returncode != 0:
        stderr = completed.stderr.strip() if text else "Git command failed"
        raise SweBenchError(f"git {' '.join(arguments)} failed: {stderr}")
    return completed.stdout


def candidate_patch(repository: Path, base_commit: str) -> str:
    _git(repository, ["add", "--intent-to-add", "--all"])
    return _git(
        repository,
        [
            "-c",
            "core.fileMode=false",
            "diff",
            "--binary",
            "--full-index",
            base_commit,
            "--",
        ],
    )


def _json_string(value: str | Path) -> str:
    return json.dumps(str(value), ensure_ascii=False)


def render_benchmark_manifest(
    *,
    tasks: Sequence[dict[str, Any]],
    snapshots: Path,
    assets: Path,
    adapter: Path,
    deixis_command: Path,
    deixis_config: Path,
    model: str = "gpt-5.6-sol",
    reasoning_effort: str = "medium",
    repetitions: int = 2,
    seed: int = 20260907,
) -> str:
    lines = [
        "schema_version = 1",
        f"model = {_json_string(model)}",
        f"reasoning_effort = {_json_string(reasoning_effort)}",
        f"repetitions = {repetitions}",
        f"seed = {seed}",
        "agent_timeout_seconds = 1200",
        "evaluation_timeout_seconds = 1800",
        "",
        'codex_command = "codex"',
        f"deixis_command = {_json_string(deixis_command)}",
        f"deixis_config = {_json_string(deixis_config)}",
        "",
        'arms = ["control", "deixis_available", "instruction_only", "deixis_instructed"]',
        "",
        'instruction = """',
        "When a task needs semantic code navigation and Deixis is available, try",
        "`definition`, `type_definition`, `implementation`, or `references` for",
        "the relevant question before using text search or reading broad file ranges.",
        "Use ordinary repository tools to locate an initial file or symbol position",
        "when needed. After editing code in a task with a semantic navigation need,",
        "use `diagnostics` on the changed files to check for language-server errors,",
        "and run the relevant tests. If the task has no semantic navigation need, no",
        "Deixis call is required, including diagnostics. Use `document_symbols` only",
        "when a file outline is needed; it is never a required step. If Deixis is",
        "unavailable, a call fails, or its result does not answer the question, fall",
        "back to ordinary repository tools.",
        '"""',
        "",
        "[environment]",
        'RUST_BACKTRACE = "1"',
    ]
    for task in tasks:
        task_id = task["instance_id"]
        evaluation = [
            "python3",
            str(adapter),
            "evaluate",
            "--task",
            str(assets / task_id / "task.json"),
        ]
        lines.extend(
            [
                "",
                "[[tasks]]",
                f"id = {_json_string(task_id)}",
                f"repository = {_json_string(snapshots / task_id)}",
                f"revision = {_json_string(task['snapshot_commit'])}",
                f"prompt_file = {_json_string(assets / task_id / 'problem_statement.md')}",
                f"evaluation_command = {json.dumps(evaluation, ensure_ascii=False)}",
                "expected_evaluation_exit_code = 0",
                f"tags = {json.dumps(['rust', 'swebench-multilingual', task['repo']], ensure_ascii=False)}",
            ]
        )
    return "\n".join(lines) + "\n"


def _download(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "deixis-benchmark/1"})
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.read()
    except (OSError, urllib.error.URLError) as error:
        raise SweBenchError(f"cannot download {url}: {error}") from error


def _raw_root(suite: Suite) -> str:
    prefix = "https://github.com/"
    if not suite.source_repository.startswith(prefix):
        raise SweBenchError("suite source_repository must be a GitHub HTTPS URL")
    repository = suite.source_repository.removeprefix(prefix)
    return f"https://raw.githubusercontent.com/{repository}/{suite.source_revision}"


def _yaml_scalar(value: str) -> str:
    value = value.strip()
    if value.startswith(("'", '"')):
        parsed = ast.literal_eval(value)
        if not isinstance(parsed, str):
            raise SweBenchError("task.yaml scalar is not a string")
        return parsed
    return value


def _parse_task_yaml(content: str) -> dict[str, str]:
    wanted = {
        "base_commit",
        "eval_type",
        "image",
        "instance_id",
        "log_parser",
        "repo",
        "version",
    }
    result: dict[str, str] = {}
    for line in content.splitlines():
        match = re.fullmatch(r"([A-Za-z_]+):\s*(.+)", line)
        if match is not None and match.group(1) in wanted:
            result[match.group(1)] = _yaml_scalar(match.group(2))
    missing = wanted - result.keys()
    if missing:
        raise SweBenchError(f"task.yaml is missing keys: {', '.join(sorted(missing))}")
    return result


def _run_checked(command: Sequence[str], *, stdout: Any = subprocess.PIPE) -> str:
    completed = subprocess.run(
        command,
        check=False,
        stdout=stdout,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode != 0:
        stderr = completed.stderr.strip()
        raise SweBenchError(f"{' '.join(command)} failed: {stderr}")
    return completed.stdout.strip() if stdout == subprocess.PIPE else ""


def _docker_image_id(image: str) -> str:
    _run_checked(["docker", "pull", image], stdout=None)
    image_id = _run_checked(
        ["docker", "image", "inspect", "--format", "{{.Id}}", image]
    )
    if not IMAGE_ID.fullmatch(image_id):
        raise SweBenchError(f"Docker returned an invalid image ID for {image}")
    return image_id


def _extract_snapshot(image_id: str, destination: Path, base_commit: str) -> str:
    if destination.exists():
        raise SweBenchError(f"snapshot destination already exists: {destination}")
    image_commit = _run_checked(
        [
            "docker",
            "run",
            "--rm",
            "--network",
            "none",
            image_id,
            "git",
            "-C",
            "/testbed",
            "rev-parse",
            "HEAD",
        ]
    )
    if image_commit != base_commit:
        raise SweBenchError(
            f"Docker image is at {image_commit}, expected task base {base_commit}"
        )
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(
        tempfile.mkdtemp(prefix=f".{destination.name}-", dir=destination.parent)
    )
    try:
        archive = temporary.parent / f".{destination.name}-{os.getpid()}.tar"
        with archive.open("wb") as output:
            _run_checked(
                [
                    "docker",
                    "run",
                    "--rm",
                    "--network",
                    "none",
                    image_id,
                    "bash",
                    "-c",
                    (
                        "git -C /testbed clean -qfdX && "
                        "tar --exclude=./.git -C /testbed -cf - ."
                    ),
                ],
                stdout=output,
            )
        with tarfile.open(archive) as source:
            source.extractall(temporary, filter="data")
        archive.unlink()
        _run_checked(["git", "init", "-q", "-b", "benchmark", str(temporary)])
        _git(temporary, ["add", "-f", "--all"])
        environment = os.environ.copy()
        environment.update(
            {
                "GIT_AUTHOR_NAME": "Deixis benchmark",
                "GIT_AUTHOR_EMAIL": "benchmark@example.invalid",
                "GIT_COMMITTER_NAME": "Deixis benchmark",
                "GIT_COMMITTER_EMAIL": "benchmark@example.invalid",
                "GIT_AUTHOR_DATE": "2000-01-01T00:00:00+00:00",
                "GIT_COMMITTER_DATE": "2000-01-01T00:00:00+00:00",
            }
        )
        completed = subprocess.run(
            [
                "git",
                "-C",
                str(temporary),
                "commit",
                "-q",
                "-m",
                f"benchmark base {base_commit}",
            ],
            check=False,
            capture_output=True,
            text=True,
            env=environment,
        )
        if completed.returncode != 0:
            raise SweBenchError(f"cannot commit snapshot: {completed.stderr.strip()}")
        snapshot_commit = _git(temporary, ["rev-parse", "HEAD"]).strip()
        temporary.rename(destination)
        return snapshot_commit
    finally:
        archive_path = temporary.parent / f".{destination.name}-{os.getpid()}.tar"
        if archive_path.exists():
            archive_path.unlink()
        if temporary.exists():
            shutil.rmtree(temporary)


def _write_bytes(path: Path, content: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_bytes(content)
    temporary.replace(path)


def _write_json(path: Path, value: Any) -> None:
    _write_bytes(
        path,
        (
            json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
        ).encode(),
    )


def _verify_snapshot(path: Path, expected_commit: str) -> None:
    if not path.is_dir():
        raise SweBenchError(f"snapshot is missing: {path}")
    observed = _git(path, ["rev-parse", "HEAD"]).strip()
    if observed != expected_commit:
        raise SweBenchError(
            f"snapshot {path} is at {observed}, expected {expected_commit}"
        )
    if _git(path, ["status", "--porcelain"]).strip():
        raise SweBenchError(f"snapshot {path} is dirty")
    if _git(path, ["remote"]).strip():
        raise SweBenchError(f"snapshot {path} unexpectedly has a Git remote")


def _fetch_assets(suite: Suite, task_id: str) -> dict[str, bytes]:
    root = _raw_root(suite)
    return {name: _download(f"{root}/tasks/{task_id}/{name}") for name in TASK_ASSETS}


def _prepare_task(suite: Suite, task_id: str, destination: Path) -> dict[str, Any]:
    assets_root = destination / "assets" / task_id
    snapshot = destination / "snapshots" / task_id
    metadata_path = assets_root / "task.json"
    if metadata_path.exists():
        metadata = load_task_metadata(metadata_path)
        if metadata["source_revision"] != suite.source_revision:
            raise SweBenchError(f"prepared task {task_id} uses another source revision")
        _verify_snapshot(snapshot, metadata["snapshot_commit"])
        return metadata

    assets = _fetch_assets(suite, task_id)
    task_yaml = _parse_task_yaml(assets["task.yaml"].decode())
    if task_yaml["instance_id"] != task_id:
        raise SweBenchError(f"task directory {task_id} contains another instance")
    if not GIT_SHA.fullmatch(task_yaml["base_commit"]):
        raise SweBenchError(f"task {task_id} has an invalid base commit")
    if task_yaml["log_parser"] != "parse_log_cargo":
        raise SweBenchError(f"task {task_id} does not use the Cargo parser")
    if task_yaml["eval_type"] != "pass_and_fail":
        raise SweBenchError(f"task {task_id} is not a pass-and-fail task")
    try:
        tests = json.loads(assets["tests.json"])
    except json.JSONDecodeError as error:
        raise SweBenchError(f"task {task_id} has invalid tests.json") from error
    if not isinstance(tests, dict):
        raise SweBenchError(f"task {task_id} tests.json is not an object")
    fail_to_pass = tests.get("FAIL_TO_PASS")
    pass_to_pass = tests.get("PASS_TO_PASS")
    for name, cases in (("FAIL_TO_PASS", fail_to_pass), ("PASS_TO_PASS", pass_to_pass)):
        if (
            not isinstance(cases, list)
            or not cases
            or any(not isinstance(case, str) or not case for case in cases)
        ):
            raise SweBenchError(f"task {task_id} has invalid {name} tests")

    image_id = _docker_image_id(task_yaml["image"])
    snapshot_commit = _extract_snapshot(image_id, snapshot, task_yaml["base_commit"])
    checksums = {
        name: hashlib.sha256(content).hexdigest() for name, content in assets.items()
    }
    metadata = {
        "schema_version": 1,
        "instance_id": task_id,
        "repo": task_yaml["repo"],
        "base_commit": task_yaml["base_commit"],
        "snapshot_commit": snapshot_commit,
        "image": task_yaml["image"],
        "image_id": image_id,
        "log_parser": task_yaml["log_parser"],
        "eval_type": task_yaml["eval_type"],
        "FAIL_TO_PASS": fail_to_pass,
        "PASS_TO_PASS": pass_to_pass,
        "source_repository": suite.source_repository,
        "source_revision": suite.source_revision,
        "asset_sha256": checksums,
    }
    for name, content in assets.items():
        _write_bytes(assets_root / name, content)
    (assets_root / "eval.sh").chmod(0o755)
    _write_json(metadata_path, metadata)
    _verify_snapshot(snapshot, snapshot_commit)
    return metadata


def _language_server_config(rust_analyzer: Path) -> str:
    return "\n".join(
        [
            "[servers.rust]",
            f"command = {_json_string(rust_analyzer)}",
            'file_extensions = { ".rs" = "rust" }',
            "",
            "[servers.rust.initialization_options]",
            "checkOnSave = true",
            "",
        ]
    )


def prepare_suite(args: argparse.Namespace) -> int:
    suite_path = args.suite.resolve()
    suite = load_suite(suite_path)
    destination = args.destination.resolve()
    manifest = args.manifest.resolve()
    adapter = Path(__file__).resolve()
    deixis_command = args.deixis_command.resolve()
    if not deixis_command.is_file() or not os.access(deixis_command, os.X_OK):
        raise SweBenchError(f"Deixis executable is missing: {deixis_command}")
    rust_analyzer_name = shutil.which(args.rust_analyzer)
    if rust_analyzer_name is None:
        raise SweBenchError(f"cannot find rust-analyzer command {args.rust_analyzer!r}")
    rust_analyzer = Path(rust_analyzer_name).resolve()
    destination.mkdir(parents=True, exist_ok=True)
    tasks: list[dict[str, Any]] = []
    for index, task_id in enumerate(suite.task_ids, start=1):
        print(f"[{index}/{len(suite.task_ids)}] preparing {task_id}", flush=True)
        tasks.append(_prepare_task(suite, task_id, destination))
    config = destination / "deixis-rust.toml"
    _write_bytes(config, _language_server_config(rust_analyzer).encode())
    rendered = render_benchmark_manifest(
        tasks=tasks,
        snapshots=destination / "snapshots",
        assets=destination / "assets",
        adapter=adapter,
        deixis_command=deixis_command,
        deixis_config=config,
        model=args.model,
        reasoning_effort=args.reasoning_effort,
        repetitions=args.repetitions,
        seed=suite.selection_seed,
    )
    _write_bytes(manifest, rendered.encode())
    provenance = {
        "schema_version": 1,
        "source_repository": suite.source_repository,
        "source_revision": suite.source_revision,
        "selection_seed": suite.selection_seed,
        "selection_rule": suite.selection_rule,
        "task_ids": list(suite.task_ids),
        "rust_analyzer": str(rust_analyzer),
        "rust_analyzer_version": _run_checked([str(rust_analyzer), "--version"]),
        "deixis_command": str(deixis_command),
        "deixis_sha256": hashlib.sha256(deixis_command.read_bytes()).hexdigest(),
        "manifest": str(manifest),
    }
    _write_json(destination / "suite.json", provenance)
    print(f"wrote {manifest}")
    return 0


def _mount(source: Path, target: str) -> str:
    return f"type=bind,src={source},dst={target},readonly"


def _run_container_evaluation(
    metadata_path: Path, metadata: dict[str, Any], patch_text: str
) -> tuple[str, dict[str, Any], int]:
    eval_script = metadata_path.parent / "eval.sh"
    if not eval_script.is_file():
        raise SweBenchError(f"evaluation script is missing: {eval_script}")
    expected_checksum = metadata["asset_sha256"]["eval.sh"]
    observed_checksum = hashlib.sha256(eval_script.read_bytes()).hexdigest()
    if observed_checksum != expected_checksum:
        raise SweBenchError(f"evaluation script checksum changed: {eval_script}")
    with tempfile.TemporaryDirectory(prefix="deixis-swebench-") as temporary_directory:
        patch_path = Path(temporary_directory) / "candidate.patch"
        patch_path.write_text(patch_text, encoding="utf-8")
        container_script = """
set -uo pipefail
cd /testbed
if [ -s /tmp/deixis-candidate.patch ]; then
    if ! git apply --binary /tmp/deixis-candidate.patch; then
        echo '>>>>> Patch Apply Failed'
        exit 85
    fi
fi
bash /tmp/deixis-eval.sh
""".strip()
        completed = subprocess.run(
            [
                "docker",
                "run",
                "--rm",
                "--network",
                "none",
                "--mount",
                _mount(patch_path.resolve(), "/tmp/deixis-candidate.patch"),
                "--mount",
                _mount(eval_script.resolve(), "/tmp/deixis-eval.sh"),
                metadata["image_id"],
                "bash",
                "-c",
                container_script,
            ],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
    log = completed.stdout
    report = grade_cargo_log(log, metadata["FAIL_TO_PASS"], metadata["PASS_TO_PASS"])
    if completed.returncode != 0:
        report["valid"] = False
        report["resolved"] = False
        report["reason"] = f"evaluation container exited {completed.returncode}"
    return log, report, completed.returncode


def _print_evaluation(log: str, report: dict[str, Any]) -> None:
    print(log, end="" if log.endswith("\n") or not log else "\n")
    print("DEIXIS_SWEBENCH_REPORT=" + json.dumps(report, sort_keys=True))


def evaluate_task(args: argparse.Namespace) -> int:
    metadata_path = args.task.resolve()
    metadata = load_task_metadata(metadata_path)
    patch_text = candidate_patch(Path.cwd(), metadata["snapshot_commit"])
    log, report, _ = _run_container_evaluation(metadata_path, metadata, patch_text)
    _print_evaluation(log, report)
    if not report["valid"]:
        return 2
    return 0 if report["resolved"] else 1


def verify_task(args: argparse.Namespace) -> int:
    metadata_path = args.task.resolve()
    metadata = load_task_metadata(metadata_path)
    base_log, base_report, _ = _run_container_evaluation(metadata_path, metadata, "")
    _print_evaluation(base_log, base_report)
    if not is_valid_base_report(base_report):
        print(
            "base snapshot did not reproduce the expected failing tests",
            file=sys.stderr,
        )
        return 2
    suite = Suite(
        source_repository=metadata["source_repository"],
        source_revision=metadata["source_revision"],
        selection_seed=0,
        selection_rule="verification",
        task_ids=(metadata["instance_id"],),
    )
    gold_url = f"{_raw_root(suite)}/tasks/{metadata['instance_id']}/gold.patch"
    gold_patch = _download(gold_url).decode()
    gold_log, gold_report, _ = _run_container_evaluation(
        metadata_path, metadata, gold_patch
    )
    _print_evaluation(gold_log, gold_report)
    if not gold_report["resolved"]:
        print("gold patch did not resolve the task", file=sys.stderr)
        return 2
    _write_json(
        metadata_path.parent / "verification.json",
        {"schema_version": 1, "base": base_report, "gold": gold_report},
    )
    return 0


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    prepare = subparsers.add_parser("prepare", help="materialize the pinned task suite")
    prepare.add_argument("suite", type=Path)
    prepare.add_argument("--destination", type=Path, required=True)
    prepare.add_argument("--manifest", type=Path, required=True)
    prepare.add_argument("--deixis-command", type=Path, required=True)
    prepare.add_argument("--rust-analyzer", default="rust-analyzer")
    prepare.add_argument("--model", default="gpt-5.6-sol")
    prepare.add_argument("--reasoning-effort", default="medium")
    prepare.add_argument("--repetitions", type=int, default=2)
    prepare.set_defaults(handler=prepare_suite)

    evaluate = subparsers.add_parser("evaluate", help="grade the current worktree")
    evaluate.add_argument("--task", type=Path, required=True)
    evaluate.set_defaults(handler=evaluate_task)

    verify = subparsers.add_parser("verify", help="verify base and gold outcomes")
    verify.add_argument("--task", type=Path, required=True)
    verify.set_defaults(handler=verify_task)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    try:
        args = _parser().parse_args(argv)
        return args.handler(args)
    except SweBenchError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
