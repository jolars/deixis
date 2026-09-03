# Contributing

## Project state

Deixis is pre-alpha. Its intended architecture is recorded in
[DESIGN.md](DESIGN.md), and its implementation order is recorded in
[TODO.md](TODO.md). Discuss changes that alter those boundaries before building
on them.

## Development environment

The repository pins Rust 1.98.0 in `rust-toolchain.toml`. The preferred
environment is devenv, which supplies `go-task`, `cargo-audit`, `cargo-deny`,
clippy, rustfmt, and the repository's pre-commit hooks.

Run the complete local gate with:

```console
task check
```

The individual commands are:

```console
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

Prefer test-driven development. Protocol changes should begin with a failing
black-box or transport-level test; lifecycle and routing code should also have
focused unit tests.

## Protocol discipline

The MCP transport owns stdin and stdout. Application logs, child-language-
server stderr, diagnostics for humans, and panic context must never be written
to stdout. Tests that launch the real binary protect this boundary.

Language servers are untrusted subprocesses. Keep timeouts, cancellation,
bounded queues, orderly shutdown, and forced termination paths explicit. Never
download or execute a language server merely because a project-local file asks
for it.

## Commits and releases

Use [Conventional Commits]. Keep subjects short, and wrap identifiers in
backticks when useful. Versionary uses the commit history to prepare release
pull requests, update `Cargo.toml` and `CHANGELOG.md`, create tags, and publish
GitHub releases.

The repository maintainer must configure:

- a `RELEASE_TOKEN` capable of writing contents and pull requests, and of
  triggering tag workflows;
- a protected GitHub `release` environment; and
- crates.io trusted publishing for the publish workflow.

The GitHub repository and those credentials do not form part of the local
bootstrap.

[Conventional Commits]: https://www.conventionalcommits.org/
