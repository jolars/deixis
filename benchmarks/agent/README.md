# Agent benchmark

This harness measures the end-to-end effect of exposing Deixis to Codex. It
runs a blocked, randomized 2 x 2 experiment in fresh Git worktrees and records
task success, model tokens, wall time, MCP calls, the resulting patch, and the
complete Codex event stream.

The four experimental arms separate tool availability from instructions:

| Arm | Deixis available | Conditional LSP instruction |
| --- | --- | --- |
| `control` | No | No |
| `deixis_available` | Yes | No |
| `instruction_only` | No | Yes |
| `deixis_instructed` | Yes | Yes |

The task prompt never changes across arms. The instruction says to use typed
LSP tools when they are available and relevant, and to use ordinary repository
tools otherwise. This makes the following paired comparisons meaningful:

- `deixis_available` versus `control`: unprompted availability and discovery;
- `deixis_instructed` versus `instruction_only`: the configured deployment;
- `deixis_instructed` versus `deixis_available`: instruction-driven adoption.

Deixis runs without `--allow-mutation`. Codex therefore uses the same built-in
editing mechanism in every arm, while Deixis contributes semantic navigation
only.

## Requirements

- Python 3.11 or newer;
- Git;
- Docker, for the pinned SWE-bench evaluator images;
- Codex CLI;
- a pinned Deixis executable; and
- every language server named by the Deixis configuration.

Build the executable that the experiment should measure:

```console
cargo build --release --locked
```

Create a dedicated Codex home outside this repository and copy the existing
ChatGPT authentication into it. It must not contain an `AGENTS.md`; otherwise,
personal instructions could affect the experiment. Do not commit its
authentication files.

```console
install -d -m 700 ../codex-benchmark-home
install -m 600 ~/.codex/auth.json ../codex-benchmark-home/auth.json
env -u OPENAI_API_KEY -u AZURE_OPENAI_API_KEY -u CODEX_API_KEY \
  CODEX_HOME=../codex-benchmark-home codex login status
```

The status command must report `Logged in using ChatGPT`. The harness removes
usage-billed API-key environment variables, forces `forced_login_method` to
`chatgpt`, and checks the login status before it creates an output directory.
It fails closed rather than falling back to API-key billing. Codex still
connects to the Codex service under the ChatGPT subscription; this is not an
offline model benchmark. See the [OpenAI authentication documentation] for the
distinction between ChatGPT and API-key authentication.

The runner uses `--ignore-user-config`, `--ignore-rules`, `--ephemeral`, a
workspace-write sandbox, no approvals, no web search, and one agent thread. It
also rejects a task worktree containing `.codex/config.toml` or instructions
that tell the agent to use Deixis.

These controls follow the current [Codex non-interactive mode documentation]
and [Codex configuration reference]. The former defines the JSONL event and
usage records; the latter defines the per-run MCP and developer-instruction
overrides used for the factorial arms.

[Codex non-interactive mode documentation]: https://learn.chatgpt.com/docs/non-interactive-mode
[Codex configuration reference]: https://learn.chatgpt.com/docs/config-file/config-reference
[OpenAI authentication documentation]: https://learn.chatgpt.com/docs/auth

## Define tasks

Copy [benchmark.example.toml](benchmark.example.toml) to an untracked manifest
and edit it. Relative paths are resolved from the manifest's directory.

Each task names a local Git repository, a revision, an arm-invariant prompt,
and an evaluation command. Commands are argument arrays and are executed
directly, without a shell. Use a wrapper script when an evaluator needs shell
syntax.

A good task replays a real historical fix:

1. Check out the parent of the fixing commit.
2. Use the original issue as the prompt.
3. Evaluate with held-out tests introduced by the fix.
4. Confirm that the optional precheck fails at the starting revision.

Keep the benchmark manifest and held-out tests outside the task repository.
Include both semantic-navigation tasks and localized control tasks; use several
repositories and languages. Tiny fixtures generally measure language-server
startup rather than useful navigation.

The optional `precheck_command` must return `expected_precheck_exit_code`
before Codex starts. The evaluation command must return
`expected_evaluation_exit_code` after Codex exits. Per-task environment values
are inherited by the precheck, Codex, and evaluator. Keep secrets out of the
manifest.

## Prepare the Rust pilot

The checked-in [Rust pilot suite](swebench-rust-pilot.toml) selects ten real
Rust issues from seven repositories in SWE-bench Multilingual. The source
revision, selection seed, selection rule, task IDs, Docker image IDs, task-asset
checksums, Deixis binary hash, and `rust-analyzer` version are recorded. A task
that cannot reproduce its base and gold outcomes without network access is
rejected deterministically.

Build Deixis, then materialize the source snapshots and evaluator assets
outside this repository:

```console
cargo build --release --locked
python3 benchmarks/agent/swebench.py prepare \
  benchmarks/agent/swebench-rust-pilot.toml \
  --destination ../deixis-benchmark-data \
  --manifest benchmarks/agent/benchmark.toml \
  --deixis-command target/release/deixis
```

Preparation pulls the official task images, confirms that each image is at the
declared base commit, removes the original Git history and remotes, and creates
a fresh single-commit snapshot. This prevents the agent from recovering the
future fixing commit from Git. The generated manifest is intentionally ignored
because it contains machine-specific absolute paths.

Verify every fixture before spending model tokens:

```console
for task in ../deixis-benchmark-data/assets/*/task.json; do
  python3 benchmarks/agent/swebench.py verify --task "$task" || exit
done
```

Verification runs the untouched base and the official gold patch in the pinned
image with networking disabled. The gold patch is fetched from the pinned task
source only for this check and is never stored in the agent-visible snapshot.
The normal evaluator applies only the candidate worktree patch, then grades the
declared fail-to-pass and pass-to-pass tests.

## Run an experiment

Validate the manifest and inspect the randomized schedule first:

```console
python3 benchmarks/agent/harness.py validate \
  benchmarks/agent/benchmark.toml

python3 benchmarks/agent/harness.py run \
  benchmarks/agent/benchmark.toml \
  --codex-home ../codex-benchmark-home \
  --output ../deixis-benchmark-dry-run \
  --dry-run
```

The real output directory must not already exist and must not be inside any
task repository:

```console
python3 benchmarks/agent/harness.py run \
  benchmarks/agent/benchmark.toml \
  --codex-home ../codex-benchmark-home \
  --output ../deixis-benchmark-001
```

Use repeatable `--task TASK_ID` and `--arm ARM` options for a smaller pilot.
For example:

```console
python3 benchmarks/agent/harness.py run \
  benchmarks/agent/benchmark.toml \
  --codex-home ../codex-benchmark-home \
  --output ../deixis-pilot-001 \
  --task rust-cross-file \
  --arm control \
  --arm deixis_instructed
```

Run arms sequentially. The harness randomizes task and arm order within each
repetition, which reduces—but does not eliminate—time-varying service load and
machine-cache effects.

## Artifacts and summaries

The output contains:

```text
experiment.json
schedule.json
runs.jsonl
trials/<task>--r<repetition>--<arm>/
  codex-command.json
  codex.stderr
  evaluation.stderr
  evaluation.stdout
  events-timed.jsonl
  events.jsonl
  git-status.txt
  patch.diff
  prompt.txt
  result.json
  worktree/
```

`events.jsonl` preserves Codex's raw JSONL stream. `events-timed.jsonl` adds the
runner's monotonic observation time to every event. `result.json` extracts
`input_tokens`, `cached_input_tokens`, `uncached_input_tokens`,
`output_tokens`, and `reasoning_output_tokens`; it does not add reasoning tokens
to output tokens. It also records total agent wall time, time to first edit,
and MCP call durations when the CLI emits matching start and completion events.

Summarize an experiment with:

```console
python3 benchmarks/agent/harness.py summarize ../deixis-benchmark-001
```

Pass `--json` for a machine-readable summary. The text report includes solve
rate, timeouts, median wall time, raw input-plus-output tokens per solve, median
MCP calls, paired solve-rate differences, and paired wall-time and token ratios
when both arms solve the task. Use `runs.jsonl` for task-clustered bootstrap
intervals or a mixed-effects analysis.

Worktrees are retained because the patch and result should remain auditable.
Remove them with `git worktree remove` from each source repository when the
experiment is no longer needed; use `git worktree prune` to clean stale
administrative entries.

## Interpreting results

Treat task success under fixed time limits as the primary outcome. Token and
time comparisons among successful runs alone are susceptible to selection
bias, so also report total tokens and total agent time per solved task. Classify
service outages and process-launch failures as infrastructure failures rather
than task failures, and rerun those cells in a new experiment directory.

For a pilot, use 8-12 tasks and two repetitions. A stronger result needs more
tasks, multiple independent repetitions, pinned tool versions, and a recorded
task-selection rule.
