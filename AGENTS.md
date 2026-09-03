# AGENTS.md

This file is the operational guide for agents working in this repository.
Architecture and rationale live in `DESIGN.md`; planned work and ordering live
in `TODO.md`.

## Project priorities

Deixis is a portable Rust MCP server that exposes typed LSP operations. Keep the
boundary narrow: language-server lifecycle, document synchronization,
capability-aware routing, and translation between MCP and LSP.

- Do not add general file search, file editing, shell execution, memory, project
  indexing, language-server downloads, or a non-LSP semantic backend.
- One process serves one immutable project root and may own several language
  servers.
- Server commands are explicit user configuration. Project content must never
  silently authorize executable discovery or downloads.
- Keep Linux, macOS, and Windows behavior covered.

## Protocol invariants

- Stdout contains MCP protocol frames only. Send all logs and child-process
  diagnostics to stderr.
- Advertise only capabilities that work. A placeholder tool is not a working
  capability.
- Preserve LSP notification order. Correlate concurrent requests by ID, and
  propagate cancellation where the downstream server supports it.
- Check negotiated server capabilities before dispatching an LSP operation.
- Shut down each child with `shutdown` followed by `exit`; use a bounded wait
  before forced termination.

## Development

Prefer test-driven development. Run focused tests while working, followed by:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-targets --locked`
- `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`

`task check` runs the same gate. The pinned toolchain and project utilities are
available through devenv.

Use `src/foo.rs` plus `src/foo/bar.rs`; do not introduce `mod.rs`. Keep the
binary entry point thin as modules appear. Add dependencies only for an active
roadmap item, not for speculative future use.

## Change synchronization

- Update `DESIGN.md` when an architectural constraint or public protocol
  convention changes.
- Update `TODO.md` when work is completed, reordered, added, or intentionally
  deferred.
- Update `README.md` when installation, invocation, or shipped capabilities
  change.
- Add a black-box MCP test when server metadata, capabilities, transport, or
  shutdown behavior changes.
