# Deixis Roadmap

Status: `[ ]` todo · `[~]` in progress · `[x]` complete

The phases are ordered. Each functional phase should begin with a failing
black-box test and end with the full repository gate passing on Linux, macOS,
and Windows.

## 0. Project bootstrap

- [x] Initialize a Rust 2024 binary crate and pin Rust 1.98.0.
- [x] Add a stdio MCP server that reports its name and version and advertises no
  capabilities.
- [x] Protect the protocol stream with stderr-only tracing and a real-binary MCP
  handshake test.
- [x] Add devenv, pre-commit hooks, Taskfile commands, rustfmt, and committed
  dependency resolution.
- [x] Add cross-platform CI, Dependabot, Versionary, and crates.io trusted
  publishing workflows.
- [x] Add project, contribution, design, roadmap, changelog, and license
  documentation.
- [x] Add a Nix flake with package, app, check, formatter, and Home Manager MCP
  integration outputs.

## 1. LSP vertical slice

- [x] Add a portable mock language-server binary used only by integration tests.
  It must exercise initialization, notifications, requests, cancellation,
  diagnostics, shutdown, malformed messages, and forced termination.
- [x] Spike `async-lsp` against the mock server. Record the result in
  `DESIGN.md`; adopt it behind a small internal client boundary or document
  why a different transport is required.
- [x] Add strict TOML configuration with named servers, explicit commands,
  arguments, environment overrides, extension and glob language routes, and
  initialization options. Reject unknown and invalid fields.
- [x] Add `--config` and `--root`; default the root to the current directory,
  canonicalize it at startup, and keep it immutable.
- [x] Start configured servers lazily and implement `initialize`,
  `initialized`, `shutdown`, `exit`, timeout, and forced-kill behavior.
- [x] Handle server stderr without touching MCP stdout and attach the server
  name to every child-process log event.
- [x] Implement server-to-client configuration, workspace-folder, dynamic
  registration, log-message, and unknown-method handling. Reject
  `workspace/applyEdit` while read-only.

Acceptance: an integration test starts `deixis`, routes through the mock LSP,
observes a typed response, cancels a pending request, and leaves no child
process running. The final phase-1 repository gate passes locally on Linux;
the configured CI matrix runs the locked test suite on Linux, macOS, and
Windows.

## 2. Documents, positions, and first tools

- [x] Add root-contained path normalization, including traversal and symlink
  escape tests. Permit external locations in results without reading them.
- [x] Add lazy `didOpen`, content-hash change detection, monotonic versions,
  full-document replacement, and `didClose` on shutdown.
- [x] Convert positions between the MCP boundary's zero-based UTF-8 units and
  negotiated LSP UTF-8, UTF-16, and UTF-32 units. Test ASCII, combining
  marks, non-BMP characters, line endings, and invalid boundaries.
- [x] Add capability-gated `hover` with structured markup and a concise text
  fallback.
- [x] Add capability-gated `definition`, normalize `Location` and
  `LocationLink`, and retain source-server provenance.
- [x] Define a consistent structured error shape for invalid paths, invalid
  positions, unsupported capabilities, timeouts, server exits, and LSP
  errors.

Acceptance: edits made outside Deixis are observed on the next request, and
hover and definition agree across every negotiated position encoding in the mock
matrix.

## 3. Read-only semantic coverage

- [x] Add `declaration`, `type_definition`, and `implementation`.
- [x] Add references with explicit declaration inclusion.
- [x] Add hierarchical document symbols and normalize flat-symbol responses.
- [x] Add workspace symbols, fan out across capable servers, and merge in stable
  configuration order.
- [x] Cache versioned push diagnostics and implement pull diagnostics when
  advertised. Mark unavailable or stale reports explicitly.
- [x] Add stable text renderers for all tools without discarding structured LSP
  fields.
- [x] Test null, empty, partial, multi-location, deprecated, and extension-rich
  LSP responses.

Acceptance: the read-only tool set works end to end against the mock server, and
each tool is absent or returns a capability error when its server does not
support the corresponding LSP method.

## 4. Polyglot routing and resilience

- [x] Manage several lazy language-server processes under one project.
- [x] Route file operations by extension and glob language maps, with an
  explicit server override for ambiguous files.
- [x] Fan out workspace operations concurrently while preserving deterministic
  output order and per-server provenance.
- [x] Bound queues, response sizes, concurrency, startup time, request time, and
  shutdown time with documented defaults.
- [x] Track language-server readiness signals and distinguish transient startup
  empties from stable no-result responses.
- [ ] Forward MCP cancellation to LSP.
- [x] Fail all outstanding requests when a child exits.
- [ ] Add bounded restart behavior with crash-loop protection and useful stderr
  context.
- [ ] Exercise concurrent requests, late responses, duplicate IDs, cancellation
  races, notification floods, and one-server failure in a multi-server
  project.

Acceptance: failure or restart of one language server does not corrupt document
state, block unrelated servers, reorder notifications, or leak a child process.

## 5. Compatibility and usability

- [x] Test the first routed tool manually against rust-analyzer.
- [ ] Test against a TypeScript server, Pyright, gopls, clangd, and one language
  server with unusual initialization requirements.
- [ ] Document installation, full MCP client configuration, strict TOML schema,
  logging, timeouts, and troubleshooting.
- [x] Load user-owned platform configuration when `--config` is absent without
  discovering executable commands from project content.
- [x] Generate that configuration from a typed Home Manager server catalog and
  register one root-agnostic MCP command.
- [ ] Evaluate a filesystem watcher for proactive diagnostics. Keep request-time
  content validation as the correctness backstop.
- [ ] Add release binaries for Linux, macOS, and Windows after the source build
  is stable; include checksums and provenance.
- [ ] Add MCP Registry metadata when a functional read-only tool set is
  released.

## 6. Mutating operations

- [ ] Design preview, authorization, conflict detection, atomic application, and
  rollback for `WorkspaceEdit` before exposing any mutating tool.
- [ ] Add prepare-rename and rename only after that design is accepted.
- [ ] Add code-action discovery separately from code-action application.
- [ ] Consider formatting only if its whole-document edit model fits the same
  safety contract.
- [ ] Keep query-only deployments able to omit all mutation capabilities.

## Deferred unless demand appears

- [ ] Streamable HTTP transport.
- [ ] MCP resources, prompts, and long-running tasks.
- [ ] A built-in language-server catalog or installer.
- [ ] Multiple project roots in one process.
- [ ] Non-LSP semantic backends, persistent indexing, or agent memory.
