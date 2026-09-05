# Deixis

Deixis is a planned Model Context Protocol (MCP) server that gives coding agents
direct, typed access to Language Server Protocol (LSP) operations. The name
comes from the linguistic act of pointing to a referent whose meaning depends on
context—much like resolving a symbol from its source position.

The project deliberately has a narrow scope. It will manage language servers and
expose their semantic capabilities; it will not grow a second filesystem, shell,
memory, indexing, or agent framework around them.

> [!NOTE]
> The project is still pre-tooling. The binary parses startup root and
> configuration options and completes an MCP handshake over stdio. When a
> configuration is present, it exposes one lifecycle probe tool for the first
> configured language server. Semantic MCP operations are not exposed yet.

## Direction

One `deixis` process will serve one project and may manage several explicitly
configured language servers. The first functional releases will concentrate on
read-only navigation:

- hover information;
- definitions, declarations, type definitions, and implementations;
- references;
- document and workspace symbols; and
- diagnostics.

See [DESIGN.md](DESIGN.md) for the architecture and [TODO.md](TODO.md) for the
implementation sequence.

## Development

The repository pins Rust 1.98.0. If you use devenv, entering the directory
provides the complete toolchain and installs the pre-commit hooks.

```console
devenv shell
task check
```

The equivalent Cargo commands are:

```console
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

To run the current server with the current directory as the immutable project
root:

```console
cargo run
```

To select a different project root or parse an explicit language-server
configuration before serving MCP traffic:

```console
cargo run -- --root /path/to/project --config /path/to/deixis.toml
```

`--root` defaults to the current directory and is canonicalized once at startup.
When `--config` is omitted, Deixis preserves the current capability-free MCP
bootstrap. When `--config` is present, the file must already exist and must use
the strict TOML server schema. Configured commands are executed directly, never
through a shell, and no command is inferred from project contents:

```toml
[[servers]]
name = "rust-analyzer"
command = "rust-analyzer"
args = []
language_ids = ["rust"]
file_patterns = ["**/*.rs"]

[servers.initialization_options]
checkOnSave = true

[servers.environment]
RUST_LOG = "info"

[servers.timeouts]
request_ms = 30000
shutdown_ms = 5000
```

The internal lifecycle manager starts the configured server only when an
internal LSP operation needs it. In configured sessions, Deixis advertises the
read-only `deixis_server_status` tool; calling it with `{ "start": true }`
starts the first configured server and returns its recorded lifecycle status.
This probe is the only public tool in the configured phase-1 surface; hover,
definition, references, symbols, diagnostics, and other semantic tools remain
unshipped.

Startup sends `initialize` and `initialized`, records reported capabilities,
correlates request IDs, forwards timeout cancellation with `$/cancelRequest`,
handles common server-to-client workspace messages, tracks dynamic registration
state, logs malformed server output without stopping the reader loop, rejects
`workspace/applyEdit` while read-only, and shuts down with `shutdown`, `exit`,
and a bounded forced-kill fallback.

The process reserves stdout for MCP messages. Set `RUST_LOG` to control logs,
which are always written to stderr. Child-process stderr is drained to Deixis
stderr with the configured server name attached, and startup validation failures
are written to stderr before MCP serving begins.

### Platform Notes

On Linux, macOS, and Windows, configured commands run directly rather than
through a shell. Root and configuration paths are canonicalized with the host
filesystem rules, and file URIs normalize Windows backslashes while preserving
drive-letter paths. Tests compile the portable mock language-server fixture with
the platform executable suffix, including `.exe` on Windows.

The shutdown sequence is platform-neutral at the protocol level: Deixis sends
`shutdown`, sends `exit`, waits for the child, and uses Tokio's platform
termination primitive if the configured timeout expires. The CI test matrix runs
`cargo test --all-targets --locked` on Ubuntu, macOS, and Windows.

## Nix

The flake exposes `packages.<system>.deixis`, a default package and app, and a
Home Manager module that installs Deixis and registers it in the shared
`programs.mcp.servers` registry:

```nix
{
  inputs.deixis = {
    url = "github:jolars/deixis";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  # In the Home Manager module list:
  imports = [ inputs.deixis.homeManagerModules.default ];
  programs.deixis.enable = true;
}
```

Run the packaged server directly with `nix run github:jolars/deixis`. During
local development, replace the input URL with a `path:` URL to the checkout.

## Inspirations

The project draws lessons from [Serena](https://github.com/oraios/serena),
[johnhnguyen97/lsp-mcp](https://github.com/johnhnguyen97/lsp-mcp), and
[Tritlo/lsp-mcp](https://github.com/Tritlo/lsp-mcp), as well as
[isaacphi/mcp-language-server](https://github.com/isaacphi/mcp-language-server),
while keeping a smaller and more protocol-focused boundary.

## License

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or
the [MIT License](LICENSE-MIT), at your option.
