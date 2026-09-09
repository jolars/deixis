# Deixis Roadmap

This file contains open work in intended order. Shipped behavior belongs in the
[README](README.md) and [design](DESIGN.md); completed development history
belongs in the [changelog](CHANGELOG.md) and Git history.

## Read-only release

- [ ] Decide whether proactive diagnostics justify a filesystem watcher.
  Request-time content validation must remain the correctness backstop. If a
  watcher is adopted, specify overflow, rename, deletion, symlink, ignore, and
  cross-platform behavior before implementation; otherwise, record the reason
  for deferral in `DESIGN.md`.
- [x] Automate release binaries for Linux, macOS, and Windows. Publish SHA-256
  checksums and GitHub build attestations with every artifact.
- [ ] Publish the first functional read-only release and add its MCP Registry
  metadata.

Acceptance: installation and MCP-host instructions match the released
artifacts; the locked test suite passes on Linux, macOS, and Windows; each
artifact is traceable to its source; and the registry entry advertises only the
shipped stdio surface.

## Agent-facing query ergonomics

- [x] Reduce the default server-status output to attached server names and a
  count of configured servers that are not attached.
- [ ] Add optional, tightly bounded source context to root-contained navigation
  results so an agent can often assess a location without another broad file
  read.
- [x] Retry retriggerable LSP cancellations, including error `-32802` with
  `retriggerRequest`, after a bounded readiness wait. Preserve the caller's
  deadline and cancellation, and report the terminal failure clearly.
- [x] Add incoming and outgoing call hierarchy behind negotiated server
  capabilities, with the same result limits as references.
- [x] Add signature help behind negotiated server capabilities, with concise
  text and structured parameter information.
- [x] Present `document_symbols` as an explicit file-outline operation rather
  than a default navigation step in agent guidance and tool documentation.

## Evaluation

- [x] Add a randomized agent-level harness that separates Deixis availability
  from LSP-directed instructions and records success, tokens, time, tool calls,
  and patches.
- [x] Curate and validate a pinned ten-task Rust pilot from SWE-bench
  Multilingual, including isolated base/gold grading.
- [ ] Revise the Deixis treatment to target definitions, type definitions,
  implementations, references, and post-edit diagnostics. Do not require
  document-symbol calls or any Deixis call when a task has no semantic
  navigation need.
- [ ] Record per-call MCP latency, result size, item count, and failure status so
  token and time overhead can be attributed to individual operations.
- [ ] Calibrate the revised treatment on three to five navigation-heavy tasks
  involving cross-module references, traits or interfaces, re-exports, or name
  collisions before running the full pilot.
- [ ] Run the pilot and publish the first benchmark results with pinned Codex,
  Deixis, and language-server versions.
- [ ] Expand the task set beyond Rust before drawing multilingual conclusions.

## Mutating operations

- [x] Specify the `WorkspaceEdit` safety contract in `DESIGN.md`: preview,
  authorization, version and content conflict detection, root containment,
  resource operations, atomic application, rollback, and failure reporting.
- [x] Build and test the internal validation and preview path before advertising
  a mutating MCP tool.
- [x] Add prepare-rename and rename only after the edit contract is accepted.
- [ ] Add code-action discovery separately from code-action application.
- [ ] Consider formatting only if whole-document edits fit the same safety
  contract.
- [x] Keep the default runtime query-only and gate mutation behind the explicit
  `--allow-mutation` CLI flag.

Acceptance: no edit is applied without an inspectable preview and explicit
authorization; stale inputs fail without partial writes; rollback behavior is
black-box tested on Linux, macOS, and Windows; and a query-only build or runtime
mode advertises no mutating tools.

## Deferred until demand appears

- [ ] Streamable HTTP transport.
- [ ] MCP resources, prompts, and long-running tasks.
- [ ] A built-in language-server catalog or installer.
- [ ] Multiple project roots in one process.
- [ ] Non-LSP semantic backends, persistent indexing, or agent memory.

These items remain outside the active sequence. Promoting one requires a
concrete use case and a design update that preserves Deixis's narrow LSP
boundary.
