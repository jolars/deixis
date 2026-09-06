# Deixis

Deixis is a Model Context Protocol (MCP) server that gives coding agents
direct, typed access to Language Server Protocol (LSP) operations. The name
comes from the linguistic act of pointing to a referent whose meaning depends on
context—much like resolving a symbol from its source position.

The project deliberately has a narrow scope. It will manage language servers and
expose their semantic capabilities; it will not grow a second filesystem, shell,
memory, indexing, or agent framework around them.

> [!NOTE]
> The project is under active development. The binary manages explicitly
> configured language servers for one project and exposes capability-gated
> hover, definition, declaration, type-definition, implementation, and
> reference information, diagnostics, and hierarchical document symbols
> through path-based routing, plus concurrent workspace-symbol search across
> capable servers.

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

To select a different project root or an explicit language-server configuration
before serving MCP traffic:

```console
cargo run -- --root /path/to/project --config /path/to/deixis.toml
```

`--root` defaults to the current directory and is canonicalized once at startup.
`--config` takes precedence over the user configuration at
`$XDG_CONFIG_HOME/deixis/config.toml` (normally
`~/.config/deixis/config.toml`). macOS and Windows use their native user
configuration directories when `XDG_CONFIG_HOME` is unset. If neither file
exists, Deixis preserves the capability-free MCP bootstrap. Deixis never loads
configuration from the project tree.

Configuration uses named servers. File-name suffixes and project-relative glob
patterns map directly to LSP language identifiers:

```toml
[servers.rust]
command = "rust-analyzer"
args = []

[servers.rust.file_extensions]
".rs" = "rust"

[servers.rust.file_patterns]
"generated/**/*.rs" = "rust"

[servers.rust.initialization_options]
checkOnSave = true

[servers.rust.environment]
RUST_LOG = "info"

[servers.rust.timeouts]
startup_ms = 30000
request_ms = 30000
shutdown_ms = 5000

[servers.rust.limits]
outbound_queue_capacity = 64
max_concurrent_requests = 16
max_response_bytes = 16777216
```

Every bound is per language server. The defaults are:

| Setting | Default | Meaning |
| --- | ---: | --- |
| `timeouts.startup_ms` | 30,000 ms | Time allowed for the `initialize` response. |
| `timeouts.request_ms` | 30,000 ms | Total time for an LSP request, including concurrency-slot wait. |
| `timeouts.shutdown_ms` | 5,000 ms | Total graceful-shutdown wait before forced termination. |
| `limits.outbound_queue_capacity` | 64 messages | Buffered messages waiting for child stdin. |
| `limits.max_concurrent_requests` | 16 requests | In-flight requests, including requests waiting for responses. |
| `limits.max_response_bytes` | 16 MiB | Largest accepted LSP `Content-Length` body. |

All values must be greater than zero. A full outbound queue fails the operation
instead of accumulating more memory. A response over the configured limit is a
protocol error and makes that language-server transport unusable; shutdown then
terminates the child within its configured bound.

Configured commands are executed directly, never through a shell, and no
command is inferred from project contents. Each server starts only when a routed
LSP operation needs it. A file pattern takes precedence over an extension within
one server; the longest matching extension wins. A path that matches several
servers is rejected unless the caller supplies the optional `server` override.

Configured sessions advertise ten read-only tools: `deixis_server_status`,
`hover`, `definition`, `declaration`, `type_definition`, `implementation`,
`references`, `diagnostics`, `document_symbols`, and `workspace_symbols`.
Every successful call returns both structured JSON and a stable, concise text
fallback. The fallback is deliberately independent of optional LSP extension
fields, which remain intact in the structured result.
Calling the lifecycle probe with `{ "server": "rust", "start": true }`
starts that server and returns its recorded status. Without `server`, it
selects the first configured name in stable lexical order. The `hover` tool
accepts a project-contained `path`, a zero-based UTF-8 `position`, and an
optional `server`; Deixis infers the LSP language identifier, synchronizes the
document, checks the negotiated hover capability, and returns structured LSP
markup with a concise text fallback.
The four location tools accept the same arguments and normalize `Location` and
`LocationLink` responses into one location shape with the configured source
server, target URI, ranges, and target position encoding. The `references` tool
also requires an explicit `includeDeclaration` boolean and returns each
`Location` with the configured source server, URI, range, and position encoding.
The `diagnostics` tool accepts a path and optional server override. It prefers
pull diagnostics when the server advertises them and otherwise returns cached
push diagnostics. Its structured report identifies the configured server,
source mechanism, synchronized document version, reported push version or pull
result ID, range encoding, and whether the report is `current`, `stale`, or
`unavailable`. Current ranges use UTF-8; stale push ranges retain the negotiated
server encoding.

The `document_symbols` tool accepts a path and optional server override. It
preserves recursive `DocumentSymbol` children and converts every range in a
readable project file to UTF-8. Legacy flat `SymbolInformation` entries become
root nodes in the same shape, with the location range used for both `range` and
`selectionRange`; `containerName` and other symbol metadata remain available.
Every node records its originating server, URI, and position encoding.

The `workspace_symbols` tool sends its required `query` to every configured
server that advertises `workspace/symbol`. Requests run concurrently, while
their merged symbols remain grouped in stable lexical server-name order. Each
symbol names its originating server, preserves LSP metadata and extension
fields, and reports the encoding of its location range. Ranges for readable
project files are normalized to UTF-8. Servers without the capability are
skipped unless none support the method, in which case the tool returns an
`unsupported_capability` error.

Deixis advertises LSP work-done progress support and rust-analyzer's server
status notification, then reports the observed state through
`deixis_server_status.readiness`. Empty current diagnostics, null hover results,
empty navigation results, and empty document-symbol results add the same
`readiness` object plus a `resultStability` value. `transient` means the server
is still working, `stable` means all observed work has finished, and
`indeterminate` means the server has not supplied enough information. Deixis
does not treat successful initialization alone as proof that semantic results
are complete.

Tool execution failures return `isError: true`, a concise text message, and a
structured `error` object. Its stable `code`, `message`, and `tool` fields are
supplemented with the server, LSP method, project path, timeout, or downstream
JSON-RPC error when applicable. Invalid arguments remain MCP `invalid_params`
protocol errors.

Startup sends `initialize` and `initialized` within the configured startup
deadline, records reported capabilities, correlates request IDs, forwards
timeout cancellation with `$/cancelRequest`,
handles common server-to-client workspace messages, tracks dynamic registration
state, tracks work-done progress and rust-analyzer server status, fails pending
requests when a server exits, logs malformed server output without stopping the
reader loop, rejects
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
Home Manager module. A nonempty server catalog generates the user configuration
and registers Deixis in the shared `programs.mcp.servers` registry:

```nix
{
  inputs.deixis = {
    url = "github:jolars/deixis";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  # In the Home Manager module list:
  imports = [ inputs.deixis.homeManagerModules.default ];
  programs.deixis = {
    enable = true;
    servers = {
      rust = {
        command = "rust-analyzer";
        fileExtensions.".rs" = "rust";
      };

      typescript = {
        command = "vtsls";
        args = [ "--stdio" ];
        fileExtensions = {
          ".js" = "javascript";
          ".ts" = "typescript";
        };
      };
    };
  };
}
```

The module also accepts `programs.deixis.configFile` instead of `servers`.
Enabling the module without either option installs the package without
registering an inert MCP server. The generated MCP command has no fixed root;
Deixis binds itself to the MCP process's working directory.

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
