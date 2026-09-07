#!/usr/bin/env python3
"""Run paired Codex experiments with and without the Deixis MCP server."""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import hashlib
import json
import os
import platform
import random
import re
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time
from collections.abc import Iterable, Sequence
from pathlib import Path
from typing import Any, TextIO

import tomllib

DEFAULT_INSTRUCTION = (
    "Use typed LSP tools for definitions, references, implementations, symbols, "
    "hover information, and diagnostics when they are available and relevant. "
    "Otherwise, use ordinary repository tools."
)
TASK_ID_PATTERN = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


class BenchmarkError(Exception):
    """An expected benchmark configuration or execution error."""


@dataclasses.dataclass(frozen=True)
class Arm:
    name: str
    deixis_available: bool
    instructed: bool


ARMS = (
    Arm("control", deixis_available=False, instructed=False),
    Arm("deixis_available", deixis_available=True, instructed=False),
    Arm("instruction_only", deixis_available=False, instructed=True),
    Arm("deixis_instructed", deixis_available=True, instructed=True),
)
USAGE_BILLED_CREDENTIALS = (
    "AZURE_OPENAI_API_KEY",
    "CODEX_API_KEY",
    "OPENAI_API_KEY",
)
ARM_BY_NAME = {arm.name: arm for arm in ARMS}


@dataclasses.dataclass(frozen=True)
class Task:
    id: str
    repository: Path
    revision: str
    prompt: str
    evaluation_command: tuple[str, ...]
    tags: tuple[str, ...] = ()
    expected_evaluation_exit_code: int = 0
    evaluation_timeout_seconds: int | None = None
    precheck_command: tuple[str, ...] | None = None
    expected_precheck_exit_code: int = 1
    environment: tuple[tuple[str, str], ...] = ()


@dataclasses.dataclass(frozen=True)
class BenchmarkConfig:
    model: str
    reasoning_effort: str
    repetitions: int
    seed: int
    agent_timeout_seconds: int
    evaluation_timeout_seconds: int
    codex_command: str
    deixis_command: Path
    deixis_config: Path
    instruction: str
    arms: tuple[Arm, ...]
    tasks: tuple[Task, ...]
    environment: tuple[tuple[str, str], ...] = ()


@dataclasses.dataclass(frozen=True)
class Trial:
    task: Task
    arm: Arm
    repetition: int

    @property
    def id(self) -> str:
        return f"{self.task.id}--r{self.repetition:02d}--{self.arm.name}"


def _mapping(value: Any, context: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise BenchmarkError(f"{context} must be a TOML table")
    return value


def _reject_unknown(mapping: dict[str, Any], allowed: set[str], context: str) -> None:
    unknown = sorted(set(mapping) - allowed)
    if unknown:
        names = ", ".join(unknown)
        raise BenchmarkError(f"unknown {context} field(s): {names}")


def _required_string(mapping: dict[str, Any], key: str, context: str) -> str:
    value = mapping.get(key)
    if not isinstance(value, str) or not value.strip():
        raise BenchmarkError(f"{context}.{key} must be a nonempty string")
    return value


def _positive_int(mapping: dict[str, Any], key: str, default: int) -> int:
    value = mapping.get(key, default)
    if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
        raise BenchmarkError(f"{key} must be a positive integer")
    return value


def _string_tuple(value: Any, context: str) -> tuple[str, ...]:
    if not isinstance(value, list) or not value:
        raise BenchmarkError(f"{context} must be a nonempty array of strings")
    if any(not isinstance(item, str) or not item for item in value):
        raise BenchmarkError(f"{context} must contain only nonempty strings")
    return tuple(value)


def _environment(value: Any, context: str) -> tuple[tuple[str, str], ...]:
    if value is None:
        return ()
    mapping = _mapping(value, context)
    pairs: list[tuple[str, str]] = []
    for key, item in sorted(mapping.items()):
        if not isinstance(item, str):
            raise BenchmarkError(f"{context}.{key} must be a string")
        pairs.append((key, item))
    return tuple(pairs)


def _relative_path(base: Path, value: str) -> Path:
    path = Path(value).expanduser()
    if not path.is_absolute():
        path = base / path
    return path.resolve()


def load_manifest(path: Path) -> BenchmarkConfig:
    path = path.resolve()
    try:
        with path.open("rb") as manifest_file:
            data = tomllib.load(manifest_file)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise BenchmarkError(f"cannot read {path}: {error}") from error

    _reject_unknown(
        data,
        {
            "schema_version",
            "model",
            "reasoning_effort",
            "repetitions",
            "seed",
            "agent_timeout_seconds",
            "evaluation_timeout_seconds",
            "codex_command",
            "deixis_command",
            "deixis_config",
            "instruction",
            "arms",
            "environment",
            "tasks",
        },
        "manifest",
    )
    if data.get("schema_version") != 1:
        raise BenchmarkError("schema_version must be 1")

    base = path.parent
    raw_arm_names = data.get("arms", [arm.name for arm in ARMS])
    arm_names = _string_tuple(raw_arm_names, "arms")
    if len(set(arm_names)) != len(arm_names):
        raise BenchmarkError("arms must not contain duplicates")
    try:
        arms = tuple(ARM_BY_NAME[name] for name in arm_names)
    except KeyError as error:
        allowed = ", ".join(ARM_BY_NAME)
        raise BenchmarkError(
            f"unknown arm {error.args[0]!r}; expected one of {allowed}"
        ) from error

    raw_tasks = data.get("tasks")
    if not isinstance(raw_tasks, list) or not raw_tasks:
        raise BenchmarkError("manifest must contain at least one [[tasks]] table")
    tasks: list[Task] = []
    seen_ids: set[str] = set()
    for index, raw_task in enumerate(raw_tasks):
        context = f"tasks[{index}]"
        task_data = _mapping(raw_task, context)
        _reject_unknown(
            task_data,
            {
                "id",
                "repository",
                "revision",
                "prompt",
                "prompt_file",
                "evaluation_command",
                "expected_evaluation_exit_code",
                "evaluation_timeout_seconds",
                "precheck_command",
                "expected_precheck_exit_code",
                "environment",
                "tags",
            },
            context,
        )
        task_id = _required_string(task_data, "id", context)
        if not TASK_ID_PATTERN.fullmatch(task_id):
            raise BenchmarkError(f"{context}.id must match {TASK_ID_PATTERN.pattern}")
        if task_id in seen_ids:
            raise BenchmarkError(f"duplicate task id {task_id!r}")
        seen_ids.add(task_id)

        has_prompt = "prompt" in task_data
        has_prompt_file = "prompt_file" in task_data
        if has_prompt == has_prompt_file:
            raise BenchmarkError(
                f"{context} must define exactly one of prompt and prompt_file"
            )
        if has_prompt:
            prompt = _required_string(task_data, "prompt", context)
        else:
            prompt_path = _relative_path(
                base, _required_string(task_data, "prompt_file", context)
            )
            try:
                prompt = prompt_path.read_text(encoding="utf-8")
            except OSError as error:
                raise BenchmarkError(
                    f"cannot read prompt file {prompt_path}: {error}"
                ) from error
            if not prompt.strip():
                raise BenchmarkError(f"prompt file {prompt_path} is empty")

        tags_value = task_data.get("tags", [])
        if not isinstance(tags_value, list) or any(
            not isinstance(tag, str) or not tag for tag in tags_value
        ):
            raise BenchmarkError(f"{context}.tags must be an array of strings")
        expected_exit = task_data.get("expected_evaluation_exit_code", 0)
        expected_precheck = task_data.get("expected_precheck_exit_code", 1)
        if not isinstance(expected_exit, int) or isinstance(expected_exit, bool):
            raise BenchmarkError(
                f"{context}.expected_evaluation_exit_code must be an integer"
            )
        if not isinstance(expected_precheck, int) or isinstance(
            expected_precheck, bool
        ):
            raise BenchmarkError(
                f"{context}.expected_precheck_exit_code must be an integer"
            )
        evaluation_timeout = task_data.get("evaluation_timeout_seconds")
        if evaluation_timeout is not None and (
            not isinstance(evaluation_timeout, int)
            or isinstance(evaluation_timeout, bool)
            or evaluation_timeout <= 0
        ):
            raise BenchmarkError(
                f"{context}.evaluation_timeout_seconds must be a positive integer"
            )
        precheck_value = task_data.get("precheck_command")
        precheck = (
            _string_tuple(precheck_value, f"{context}.precheck_command")
            if precheck_value is not None
            else None
        )
        tasks.append(
            Task(
                id=task_id,
                repository=_relative_path(
                    base, _required_string(task_data, "repository", context)
                ),
                revision=_required_string(task_data, "revision", context),
                prompt=prompt,
                evaluation_command=_string_tuple(
                    task_data.get("evaluation_command"),
                    f"{context}.evaluation_command",
                ),
                tags=tuple(tags_value),
                expected_evaluation_exit_code=expected_exit,
                evaluation_timeout_seconds=evaluation_timeout,
                precheck_command=precheck,
                expected_precheck_exit_code=expected_precheck,
                environment=_environment(
                    task_data.get("environment"), f"{context}.environment"
                ),
            )
        )

    seed = data.get("seed", 20260907)
    if not isinstance(seed, int) or isinstance(seed, bool):
        raise BenchmarkError("seed must be an integer")
    instruction = data.get("instruction", DEFAULT_INSTRUCTION)
    if not isinstance(instruction, str) or not instruction.strip():
        raise BenchmarkError("instruction must be a nonempty string")

    return BenchmarkConfig(
        model=_required_string(data, "model", "manifest"),
        reasoning_effort=_required_string(data, "reasoning_effort", "manifest"),
        repetitions=_positive_int(data, "repetitions", 2),
        seed=seed,
        agent_timeout_seconds=_positive_int(data, "agent_timeout_seconds", 1200),
        evaluation_timeout_seconds=_positive_int(
            data, "evaluation_timeout_seconds", 1200
        ),
        codex_command=(
            _required_string(data, "codex_command", "manifest")
            if "codex_command" in data
            else "codex"
        ),
        deixis_command=_relative_path(
            base, _required_string(data, "deixis_command", "manifest")
        ),
        deixis_config=_relative_path(
            base, _required_string(data, "deixis_config", "manifest")
        ),
        instruction=instruction,
        arms=arms,
        tasks=tuple(tasks),
        environment=_environment(data.get("environment"), "environment"),
    )


def build_schedule(
    tasks: tuple[Task, ...],
    arms: tuple[Arm, ...],
    repetitions: int,
    seed: int,
) -> list[Trial]:
    rng = random.Random(seed)
    schedule: list[Trial] = []
    for repetition in range(1, repetitions + 1):
        task_order = list(tasks)
        rng.shuffle(task_order)
        for task in task_order:
            arm_order = list(arms)
            rng.shuffle(arm_order)
            schedule.extend(
                Trial(task=task, arm=arm, repetition=repetition) for arm in arm_order
            )
    return schedule


def _toml_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def _toml_array(values: Sequence[str]) -> str:
    return json.dumps(list(values), ensure_ascii=False)


def build_codex_command(config: BenchmarkConfig, arm: Arm, worktree: Path) -> list[str]:
    command = [
        config.codex_command,
        "exec",
        "--ephemeral",
        "--json",
        "--ignore-user-config",
        "--ignore-rules",
        "--strict-config",
        "--color",
        "never",
        "--sandbox",
        "workspace-write",
        "-C",
        str(worktree),
        "-m",
        config.model,
        "-c",
        f"model_reasoning_effort={_toml_string(config.reasoning_effort)}",
        "-c",
        'approval_policy="never"',
        "-c",
        'web_search="disabled"',
        "-c",
        "agents.max_threads=1",
        "-c",
        'forced_login_method="chatgpt"',
    ]
    if arm.deixis_available:
        deixis_args = (
            "--root",
            str(worktree),
            "--config",
            str(config.deixis_config),
        )
        command.extend(
            [
                "-c",
                f"mcp_servers.deixis.command={_toml_string(str(config.deixis_command))}",
                "-c",
                f"mcp_servers.deixis.args={_toml_array(deixis_args)}",
                "-c",
                "mcp_servers.deixis.required=true",
                "-c",
                "mcp_servers.deixis.tool_timeout_sec=70",
            ]
        )
    if arm.instructed:
        command.extend(
            [
                "-c",
                f"developer_instructions={_toml_string(config.instruction)}",
            ]
        )
    command.append("-")
    return command


def collect_event_metrics(records: Iterable[dict[str, Any]]) -> dict[str, Any]:
    usage: dict[str, Any] | None = None
    calls: dict[str, dict[str, Any]] = {}
    first_edit: float | None = None

    for record in records:
        elapsed = record.get("elapsed_seconds")
        event = record.get("event")
        if not isinstance(event, dict):
            continue
        event_type = event.get("type")
        if event_type == "turn.completed" and isinstance(event.get("usage"), dict):
            usage = event["usage"]
        item = event.get("item")
        if not isinstance(item, dict):
            continue
        item_type = item.get("type")
        if item_type in {"file_change", "file_edit"} and first_edit is None:
            first_edit = elapsed
        if item_type != "mcp_tool_call":
            continue
        item_id = str(item.get("id", f"unknown-{len(calls)}"))
        call = calls.setdefault(
            item_id,
            {
                "id": item_id,
                "server": item.get("server") or item.get("server_name"),
                "tool": item.get("tool") or item.get("tool_name") or item.get("name"),
                "status": None,
                "started_seconds": None,
                "completed_seconds": None,
                "elapsed_seconds": None,
            },
        )
        if event_type == "item.started":
            call["started_seconds"] = elapsed
        if event_type in {"item.completed", "item.failed"}:
            call["completed_seconds"] = elapsed
            call["status"] = item.get("status") or (
                "failed" if event_type == "item.failed" else "completed"
            )
        started = call.get("started_seconds")
        completed = call.get("completed_seconds")
        if isinstance(started, (int, float)) and isinstance(completed, (int, float)):
            call["elapsed_seconds"] = completed - started

    ordered_calls = sorted(
        calls.values(),
        key=lambda call: (
            call["started_seconds"] is None,
            call["started_seconds"] or 0,
            call["id"],
        ),
    )
    failure_count = sum(
        str(call.get("status", "")).lower() in {"error", "failed", "failure"}
        for call in ordered_calls
    )

    def usage_value(key: str) -> int | None:
        if usage is None:
            return None
        value = usage.get(key)
        return value if isinstance(value, int) else None

    input_tokens = usage_value("input_tokens")
    cached_input_tokens = usage_value("cached_input_tokens")
    uncached_input_tokens = (
        input_tokens - cached_input_tokens
        if input_tokens is not None and cached_input_tokens is not None
        else None
    )
    return {
        "input_tokens": input_tokens,
        "cached_input_tokens": cached_input_tokens,
        "uncached_input_tokens": uncached_input_tokens,
        "output_tokens": usage_value("output_tokens"),
        "reasoning_output_tokens": usage_value("reasoning_output_tokens"),
        "time_to_first_edit_seconds": first_edit,
        "mcp_call_count": len(ordered_calls),
        "mcp_failure_count": failure_count,
        "mcp_calls": ordered_calls,
    }


def _write_json(path: Path, value: Any) -> None:
    path.write_text(
        json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _run_checked(command: Sequence[str], cwd: Path | None = None) -> str:
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        detail = ""
        if isinstance(error, subprocess.CalledProcessError):
            detail = error.stderr.strip()
        suffix = f": {detail}" if detail else ""
        raise BenchmarkError(f"command failed: {list(command)!r}{suffix}") from error
    return completed.stdout.strip()


def _resolve_commit(task: Task) -> str:
    return _run_checked(
        [
            "git",
            "-C",
            str(task.repository),
            "rev-parse",
            "--verify",
            f"{task.revision}^{{commit}}",
        ]
    )


def _is_within(path: Path, parent: Path) -> bool:
    try:
        path.resolve().relative_to(parent.resolve())
    except ValueError:
        return False
    return True


def _validate_static(config: BenchmarkConfig, output: Path | None = None) -> None:
    if shutil.which(config.codex_command) is None:
        raise BenchmarkError(f"Codex command is not executable: {config.codex_command}")
    if not config.deixis_command.is_file():
        raise BenchmarkError(
            f"Deixis executable does not exist: {config.deixis_command}"
        )
    if not os.access(config.deixis_command, os.X_OK):
        raise BenchmarkError(
            f"Deixis executable is not executable: {config.deixis_command}"
        )
    if not config.deixis_config.is_file():
        raise BenchmarkError(
            f"Deixis configuration does not exist: {config.deixis_config}"
        )
    for task in config.tasks:
        if not task.repository.is_dir():
            raise BenchmarkError(
                f"task {task.id!r} repository does not exist: {task.repository}"
            )
        top_level = Path(
            _run_checked(
                ["git", "-C", str(task.repository), "rev-parse", "--show-toplevel"]
            )
        ).resolve()
        if top_level != task.repository.resolve():
            raise BenchmarkError(
                f"task {task.id!r} repository must be its Git root: {top_level}"
            )
        _resolve_commit(task)
        if output is not None and _is_within(output, task.repository):
            raise BenchmarkError(
                f"output directory must not be inside task repository {task.repository}"
            )


def _validate_codex_home(path: Path, codex_command: str) -> None:
    if not path.is_dir():
        raise BenchmarkError(f"benchmark Codex home does not exist: {path}")
    instructions = path / "AGENTS.md"
    if instructions.exists():
        raise BenchmarkError(
            f"benchmark Codex home must not contain AGENTS.md: {instructions}"
        )
    environment = os.environ.copy()
    for name in USAGE_BILLED_CREDENTIALS:
        environment.pop(name, None)
    environment["CODEX_HOME"] = str(path)
    try:
        completed = subprocess.run(
            [codex_command, "login", "status"],
            check=False,
            capture_output=True,
            text=True,
            env=environment,
        )
    except OSError as error:
        raise BenchmarkError(f"cannot inspect Codex authentication: {error}") from error
    status = f"{completed.stdout}\n{completed.stderr}".strip()
    if completed.returncode != 0 or "Logged in using ChatGPT" not in status:
        raise BenchmarkError(
            "benchmark Codex home must be authenticated with ChatGPT; "
            f"codex login status reported: {status or 'no status'}"
        )


def _check_worktree_contamination(worktree: Path) -> None:
    project_config = worktree / ".codex" / "config.toml"
    if project_config.exists():
        raise BenchmarkError(
            f"task worktree contains project Codex configuration: {project_config}"
        )
    suspicious = (
        re.compile(r"prefer.{0,100}\bdeixis\b", re.IGNORECASE | re.DOTALL),
        re.compile(r"\buse.{0,60}\bdeixis\b", re.IGNORECASE | re.DOTALL),
    )
    for instructions in worktree.rglob("AGENTS.md"):
        try:
            contents = instructions.read_text(encoding="utf-8")
        except (OSError, UnicodeError):
            continue
        if any(pattern.search(contents) for pattern in suspicious):
            raise BenchmarkError(
                f"task instructions mention preferring or using Deixis: {instructions}"
            )


def _popen_group_options() -> dict[str, Any]:
    if os.name == "posix":
        return {"start_new_session": True}
    if os.name == "nt":
        return {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP}
    return {}


def _terminate_process_tree(process: subprocess.Popen[Any]) -> None:
    if process.poll() is not None:
        return
    if os.name == "posix":
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
    else:
        try:
            process.send_signal(signal.CTRL_BREAK_EVENT)
        except (OSError, ValueError):
            process.terminate()
    try:
        process.wait(timeout=5)
        return
    except subprocess.TimeoutExpired:
        pass
    if os.name == "posix":
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            return
    else:
        subprocess.run(
            ["taskkill", "/PID", str(process.pid), "/T", "/F"],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    process.wait(timeout=5)


def _stream_events(
    stream: TextIO,
    raw_path: Path,
    timed_path: Path,
    started: float,
    records: list[dict[str, Any]],
) -> None:
    with (
        raw_path.open("w", encoding="utf-8") as raw_file,
        timed_path.open("w", encoding="utf-8") as timed_file,
    ):
        for line in stream:
            observed = time.perf_counter() - started
            raw_file.write(line)
            raw_file.flush()
            try:
                event = json.loads(line)
                record = {"elapsed_seconds": observed, "event": event}
            except json.JSONDecodeError as error:
                record = {
                    "elapsed_seconds": observed,
                    "raw": line.rstrip("\n"),
                    "parse_error": str(error),
                }
            records.append(record)
            timed_file.write(
                json.dumps(record, sort_keys=True, ensure_ascii=False) + "\n"
            )
            timed_file.flush()


def _run_codex(
    command: Sequence[str],
    prompt: str,
    cwd: Path,
    environment: dict[str, str],
    timeout_seconds: int,
    artifact_directory: Path,
) -> dict[str, Any]:
    prompt_path = artifact_directory / "prompt.txt"
    prompt_path.write_text(prompt, encoding="utf-8")
    _write_json(artifact_directory / "codex-command.json", list(command))
    records: list[dict[str, Any]] = []
    started_utc = dt.datetime.now(dt.UTC).isoformat()
    started = time.perf_counter()
    with (
        prompt_path.open("r", encoding="utf-8") as prompt_file,
        (artifact_directory / "codex.stderr").open(
            "w", encoding="utf-8"
        ) as stderr_file,
    ):
        try:
            process = subprocess.Popen(
                command,
                cwd=cwd,
                env=environment,
                stdin=prompt_file,
                stdout=subprocess.PIPE,
                stderr=stderr_file,
                text=True,
                **_popen_group_options(),
            )
        except OSError as error:
            raise BenchmarkError(f"cannot start Codex: {error}") from error
        assert process.stdout is not None
        reader = threading.Thread(
            target=_stream_events,
            args=(
                process.stdout,
                artifact_directory / "events.jsonl",
                artifact_directory / "events-timed.jsonl",
                started,
                records,
            ),
            daemon=True,
        )
        reader.start()
        timed_out = False
        try:
            process.wait(timeout=timeout_seconds)
        except subprocess.TimeoutExpired:
            timed_out = True
            _terminate_process_tree(process)
        except BaseException:
            _terminate_process_tree(process)
            reader.join(timeout=10)
            raise
        reader.join(timeout=10)
        if reader.is_alive():
            raise BenchmarkError("Codex event reader did not stop")
        process.stdout.close()
        exit_code = process.returncode
    wall_seconds = time.perf_counter() - started
    return {
        "started_utc": started_utc,
        "wall_seconds": wall_seconds,
        "exit_code": exit_code,
        "timed_out": timed_out,
        **collect_event_metrics(records),
    }


def _run_captured_command(
    command: Sequence[str],
    cwd: Path,
    environment: dict[str, str],
    timeout_seconds: int,
    stdout_path: Path,
    stderr_path: Path,
) -> dict[str, Any]:
    started = time.perf_counter()
    with (
        stdout_path.open("w", encoding="utf-8") as stdout_file,
        stderr_path.open("w", encoding="utf-8") as stderr_file,
    ):
        try:
            process = subprocess.Popen(
                command,
                cwd=cwd,
                env=environment,
                stdout=stdout_file,
                stderr=stderr_file,
                text=True,
                **_popen_group_options(),
            )
        except OSError as error:
            raise BenchmarkError(
                f"cannot start command {list(command)!r}: {error}"
            ) from error
        timed_out = False
        try:
            process.wait(timeout=timeout_seconds)
        except subprocess.TimeoutExpired:
            timed_out = True
            _terminate_process_tree(process)
        except BaseException:
            _terminate_process_tree(process)
            raise
    return {
        "command": list(command),
        "wall_seconds": time.perf_counter() - started,
        "exit_code": process.returncode,
        "timed_out": timed_out,
    }


def _capture_patch(worktree: Path, commit: str, artifact_directory: Path) -> None:
    subprocess.run(
        ["git", "-C", str(worktree), "add", "--intent-to-add", "--", "."],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    patch = _run_checked(["git", "-C", str(worktree), "diff", "--binary", commit, "--"])
    (artifact_directory / "patch.diff").write_text(
        patch + ("\n" if patch else ""), encoding="utf-8"
    )
    status = _run_checked(
        ["git", "-C", str(worktree), "status", "--short", "--untracked-files=all"]
    )
    (artifact_directory / "git-status.txt").write_text(
        status + ("\n" if status else ""), encoding="utf-8"
    )


def _task_environment(
    config: BenchmarkConfig, task: Task, codex_home: Path | None = None
) -> dict[str, str]:
    environment = os.environ.copy()
    environment.update(config.environment)
    environment.update(task.environment)
    for name in USAGE_BILLED_CREDENTIALS:
        environment.pop(name, None)
    environment["NO_COLOR"] = "1"
    if codex_home is not None:
        environment["CODEX_HOME"] = str(codex_home)
    return environment


def _run_trial(
    config: BenchmarkConfig,
    trial: Trial,
    commit: str,
    codex_home: Path,
    output: Path,
) -> dict[str, Any]:
    artifact_directory = output / "trials" / trial.id
    artifact_directory.mkdir(parents=True, exist_ok=False)
    worktree = artifact_directory / "worktree"
    _run_checked(
        [
            "git",
            "-C",
            str(trial.task.repository),
            "worktree",
            "add",
            "--detach",
            str(worktree),
            commit,
        ]
    )
    _check_worktree_contamination(worktree)

    task_environment = _task_environment(config, trial.task)
    precheck: dict[str, Any] | None = None
    if trial.task.precheck_command is not None:
        precheck = _run_captured_command(
            trial.task.precheck_command,
            worktree,
            task_environment,
            trial.task.evaluation_timeout_seconds or config.evaluation_timeout_seconds,
            artifact_directory / "precheck.stdout",
            artifact_directory / "precheck.stderr",
        )
        if (
            precheck["timed_out"]
            or precheck["exit_code"] != trial.task.expected_precheck_exit_code
        ):
            raise BenchmarkError(
                f"task {trial.task.id!r} precheck returned "
                f"{precheck['exit_code']}, expected "
                f"{trial.task.expected_precheck_exit_code}"
            )

    command = build_codex_command(config, trial.arm, worktree)
    codex = _run_codex(
        command,
        trial.task.prompt,
        worktree,
        _task_environment(config, trial.task, codex_home),
        config.agent_timeout_seconds,
        artifact_directory,
    )
    _capture_patch(worktree, commit, artifact_directory)
    evaluation = _run_captured_command(
        trial.task.evaluation_command,
        worktree,
        task_environment,
        trial.task.evaluation_timeout_seconds or config.evaluation_timeout_seconds,
        artifact_directory / "evaluation.stdout",
        artifact_directory / "evaluation.stderr",
    )
    success = (
        not codex["timed_out"]
        and codex["exit_code"] == 0
        and not evaluation["timed_out"]
        and evaluation["exit_code"] == trial.task.expected_evaluation_exit_code
    )
    result = {
        "trial_id": trial.id,
        "task_id": trial.task.id,
        "tags": list(trial.task.tags),
        "repository": str(trial.task.repository),
        "commit": commit,
        "repetition": trial.repetition,
        "arm": trial.arm.name,
        "deixis_available": trial.arm.deixis_available,
        "instructed": trial.arm.instructed,
        "success": success,
        "codex": codex,
        "precheck": precheck,
        "evaluation": evaluation,
        "expected_evaluation_exit_code": trial.task.expected_evaluation_exit_code,
        "artifact_directory": str(artifact_directory),
    }
    _write_json(artifact_directory / "result.json", result)
    return result


def _config_fingerprint(config: BenchmarkConfig) -> str:
    data = dataclasses.asdict(config)

    def normalize(value: Any) -> Any:
        if isinstance(value, Path):
            return str(value)
        if isinstance(value, dict):
            return {key: normalize(item) for key, item in value.items()}
        if isinstance(value, (list, tuple)):
            return [normalize(item) for item in value]
        return value

    encoded = json.dumps(normalize(data), sort_keys=True, ensure_ascii=False).encode()
    return hashlib.sha256(encoded).hexdigest()


def _experiment_metadata(
    config: BenchmarkConfig,
    codex_home: Path,
    commits: dict[str, str],
) -> dict[str, Any]:
    try:
        codex_version = _run_checked([config.codex_command, "--version"])
    except BenchmarkError:
        codex_version = None
    return {
        "created_utc": dt.datetime.now(dt.UTC).isoformat(),
        "config_fingerprint": _config_fingerprint(config),
        "model": config.model,
        "reasoning_effort": config.reasoning_effort,
        "seed": config.seed,
        "repetitions": config.repetitions,
        "arms": [dataclasses.asdict(arm) for arm in config.arms],
        "codex_home": str(codex_home),
        "codex_version": codex_version,
        "deixis_command": str(config.deixis_command),
        "deixis_sha256": _sha256(config.deixis_command),
        "deixis_config": str(config.deixis_config),
        "deixis_config_sha256": _sha256(config.deixis_config),
        "commits": commits,
        "platform": platform.platform(),
        "python_version": platform.python_version(),
    }


def _schedule_json(schedule: Sequence[Trial], commits: dict[str, str]) -> list[Any]:
    return [
        {
            "trial_id": trial.id,
            "task_id": trial.task.id,
            "commit": commits[trial.task.id],
            "repetition": trial.repetition,
            "arm": trial.arm.name,
        }
        for trial in schedule
    ]


def _append_jsonl(path: Path, value: Any) -> None:
    with path.open("a", encoding="utf-8") as destination:
        destination.write(json.dumps(value, sort_keys=True, ensure_ascii=False) + "\n")
        destination.flush()
        os.fsync(destination.fileno())


def run_experiment(
    config: BenchmarkConfig,
    output: Path,
    codex_home: Path,
    task_ids: set[str] | None = None,
    arm_names: set[str] | None = None,
    dry_run: bool = False,
) -> None:
    output = output.resolve()
    codex_home = codex_home.resolve()
    tasks = tuple(
        task for task in config.tasks if task_ids is None or task.id in task_ids
    )
    arms = tuple(
        arm for arm in config.arms if arm_names is None or arm.name in arm_names
    )
    if not tasks:
        raise BenchmarkError("task selection is empty")
    if not arms:
        raise BenchmarkError("arm selection is empty")
    selected = dataclasses.replace(config, tasks=tasks, arms=arms)
    _validate_static(selected, output)
    _validate_codex_home(codex_home, config.codex_command)
    commits = {task.id: _resolve_commit(task) for task in tasks}
    schedule = build_schedule(tasks, arms, config.repetitions, config.seed)

    if dry_run:
        print(json.dumps(_schedule_json(schedule, commits), indent=2))
        return

    output.mkdir(parents=True, exist_ok=False)
    metadata = _experiment_metadata(selected, codex_home, commits)
    _write_json(output / "experiment.json", metadata)
    _write_json(output / "schedule.json", _schedule_json(schedule, commits))
    results_path = output / "runs.jsonl"
    total = len(schedule)
    for index, trial in enumerate(schedule, start=1):
        print(f"[{index}/{total}] {trial.id}", flush=True)
        result = _run_trial(
            selected,
            trial,
            commits[trial.task.id],
            codex_home,
            output,
        )
        _append_jsonl(results_path, result)
        outcome = "PASS" if result["success"] else "FAIL"
        print(
            f"  {outcome}: {result['codex']['wall_seconds']:.1f}s, "
            f"{result['codex']['input_tokens']} input tokens",
            flush=True,
        )


def _read_results(path: Path) -> list[dict[str, Any]]:
    if path.is_dir():
        path = path / "runs.jsonl"
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise BenchmarkError(f"cannot read results {path}: {error}") from error
    results: list[dict[str, Any]] = []
    for line_number, line in enumerate(lines, start=1):
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise BenchmarkError(
                f"invalid JSON on {path}:{line_number}: {error}"
            ) from error
        if not isinstance(value, dict):
            raise BenchmarkError(f"result on {path}:{line_number} is not an object")
        results.append(value)
    return results


def _median(values: Iterable[float | int | None]) -> float | None:
    present = [float(value) for value in values if value is not None]
    return statistics.median(present) if present else None


def _model_tokens(result: dict[str, Any]) -> int | None:
    codex = result["codex"]
    input_tokens = codex.get("input_tokens")
    output_tokens = codex.get("output_tokens")
    if not isinstance(input_tokens, int) or not isinstance(output_tokens, int):
        return None
    return input_tokens + output_tokens


def summarize_results(results: Sequence[dict[str, Any]]) -> dict[str, Any]:
    by_arm: dict[str, list[dict[str, Any]]] = {}
    for result in results:
        by_arm.setdefault(str(result.get("arm")), []).append(result)
    rows: list[dict[str, Any]] = []
    for arm in [item.name for item in ARMS]:
        arm_results = by_arm.get(arm, [])
        if not arm_results:
            continue
        solved = sum(bool(result.get("success")) for result in arm_results)
        model_tokens = [_model_tokens(result) for result in arm_results]
        token_metrics_complete = all(value is not None for value in model_tokens)
        total_model_tokens = (
            sum(value for value in model_tokens if value is not None)
            if token_metrics_complete
            else None
        )
        rows.append(
            {
                "arm": arm,
                "runs": len(arm_results),
                "solved": solved,
                "solve_rate": solved / len(arm_results),
                "timeouts": sum(
                    bool(result["codex"].get("timed_out")) for result in arm_results
                ),
                "median_wall_seconds": _median(
                    result["codex"].get("wall_seconds") for result in arm_results
                ),
                "token_complete_runs": sum(value is not None for value in model_tokens),
                "model_tokens": total_model_tokens,
                "tokens_per_solve": (
                    total_model_tokens / solved
                    if solved and total_model_tokens is not None
                    else None
                ),
                "median_mcp_calls": _median(
                    result["codex"].get("mcp_call_count") for result in arm_results
                ),
            }
        )

    indexed = {
        (result.get("task_id"), result.get("repetition"), result.get("arm")): result
        for result in results
    }
    comparisons = []
    for name, left, right in (
        ("availability_neutral", "control", "deixis_available"),
        ("deployment", "instruction_only", "deixis_instructed"),
        ("instruction_with_deixis", "deixis_available", "deixis_instructed"),
    ):
        pairs = []
        keys = {
            (task_id, repetition) for task_id, repetition, arm in indexed if arm == left
        }
        for task_id, repetition in keys:
            left_result = indexed.get((task_id, repetition, left))
            right_result = indexed.get((task_id, repetition, right))
            if left_result is not None and right_result is not None:
                pairs.append((left_result, right_result))
        if not pairs:
            continue
        both_solved = [
            pair for pair in pairs if pair[0].get("success") and pair[1].get("success")
        ]
        wall_ratios = [
            right_result["codex"]["wall_seconds"] / left_result["codex"]["wall_seconds"]
            for left_result, right_result in both_solved
            if left_result["codex"].get("wall_seconds")
        ]
        token_ratios = []
        for left_result, right_result in both_solved:
            left_tokens = _model_tokens(left_result)
            right_tokens = _model_tokens(right_result)
            if left_tokens and right_tokens is not None:
                token_ratios.append(right_tokens / left_tokens)
        comparisons.append(
            {
                "name": name,
                "left": left,
                "right": right,
                "pairs": len(pairs),
                "solve_rate_difference": statistics.mean(
                    float(bool(right_result.get("success")))
                    - float(bool(left_result.get("success")))
                    for left_result, right_result in pairs
                ),
                "both_solved": len(both_solved),
                "median_wall_ratio_when_both_solved": (
                    statistics.median(wall_ratios) if wall_ratios else None
                ),
                "median_token_ratio_when_both_solved": (
                    statistics.median(token_ratios) if token_ratios else None
                ),
            }
        )
    return {"arms": rows, "paired_comparisons": comparisons}


def _format_optional(value: float | None, digits: int = 1) -> str:
    return "NA" if value is None else f"{value:.{digits}f}"


def print_summary(summary: dict[str, Any]) -> None:
    print(
        "arm                  runs solved  solve%  timeout  median_s  tokens/solve  mcp"
    )
    for row in summary["arms"]:
        print(
            f"{row['arm']:<21} {row['runs']:>4} {row['solved']:>6} "
            f"{100 * row['solve_rate']:>6.1f} {row['timeouts']:>8} "
            f"{_format_optional(row['median_wall_seconds']):>9} "
            f"{_format_optional(row['tokens_per_solve'], 0):>13} "
            f"{_format_optional(row['median_mcp_calls']):>4}"
        )
    if summary["paired_comparisons"]:
        print(
            "\npaired comparison                 pairs  solve pp  wall ratio  token ratio"
        )
        for comparison in summary["paired_comparisons"]:
            print(
                f"{comparison['name']:<33} {comparison['pairs']:>5} "
                f"{100 * comparison['solve_rate_difference']:>9.1f} "
                f"{_format_optional(comparison['median_wall_ratio_when_both_solved'], 2):>11} "
                f"{_format_optional(comparison['median_token_ratio_when_both_solved'], 2):>12}"
            )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    validate = subparsers.add_parser("validate", help="validate a manifest")
    validate.add_argument("manifest", type=Path)

    run = subparsers.add_parser("run", help="run an experiment")
    run.add_argument("manifest", type=Path)
    run.add_argument("--output", type=Path, required=True)
    run.add_argument("--codex-home", type=Path, required=True)
    run.add_argument("--task", action="append", dest="tasks")
    run.add_argument("--arm", action="append", choices=tuple(ARM_BY_NAME), dest="arms")
    run.add_argument("--dry-run", action="store_true")

    summarize = subparsers.add_parser("summarize", help="summarize runs.jsonl")
    summarize.add_argument("results", type=Path)
    summarize.add_argument("--json", action="store_true", dest="as_json")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    arguments = _parser().parse_args(argv)
    try:
        if arguments.subcommand == "validate":
            config = load_manifest(arguments.manifest)
            _validate_static(config)
            print(
                f"valid: {len(config.tasks)} task(s), {len(config.arms)} arm(s), "
                f"{config.repetitions} repetition(s)"
            )
        elif arguments.subcommand == "run":
            config = load_manifest(arguments.manifest)
            known_tasks = {task.id for task in config.tasks}
            requested_tasks = set(arguments.tasks) if arguments.tasks else None
            if requested_tasks is not None:
                unknown = sorted(requested_tasks - known_tasks)
                if unknown:
                    raise BenchmarkError(
                        f"unknown task selection: {', '.join(unknown)}"
                    )
            run_experiment(
                config,
                arguments.output,
                arguments.codex_home,
                task_ids=requested_tasks,
                arm_names=set(arguments.arms) if arguments.arms else None,
                dry_run=arguments.dry_run,
            )
        else:
            summary = summarize_results(_read_results(arguments.results))
            if arguments.as_json:
                print(json.dumps(summary, indent=2, sort_keys=True))
            else:
                print_summary(summary)
    except BenchmarkError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
