# Deixis

Deixis is a Model Context Protocol (MCP) server that gives coding agents typed
access to Language Server Protocol (LSP) operations. It manages explicitly
configured language servers for one project and exposes their semantic
capabilities without adding another filesystem, shell, editor, index, or memory
layer.

> [!WARNING]
> Deixis is pre-alpha. Its semantic tools and guarded rename workflow work, but
> no stability guarantees are available yet.

## Capabilities

A configured Deixis session exposes twelve read-only MCP tools by default.
Starting it with `--allow-mutation` adds `apply_rename` as a thirteenth tool.

  | Tool                   | Purpose                                                     |
  | ---------------------- | ----------------------------------------------------------- |
  | `deixis_server_status` | Summarize configured servers or inspect one in detail.      |
  | `hover`                | Return hover markup at a zero-based UTF-8 position.         |
  | `definition`           | Find definitions.                                           |
  | `declaration`          | Find declarations.                                          |
  | `type_definition`      | Find type definitions.                                      |
  | `implementation`       | Find implementations.                                       |
  | `references`           | Find references, with explicit declaration inclusion.       |
  | `diagnostics`          | Request pull diagnostics or return cached push diagnostics. |
  | `document_symbols`     | Return a normalized hierarchy of symbols in a file.         |
  | `workspace_symbols`    | Search attached servers, or one explicitly named server.    |
  | `prepare_rename`       | Check whether a symbol can be renamed at a position.         |
  | `preview_rename`       | Validate edits and return a diff plus a one-shot preview ID. |
  | `apply_rename`         | Apply one exact preview (requires `--allow-mutation`).        |

Deixis negotiates UTF-8, UTF-16, and UTF-32 positions, synchronizes documents
from disk before file-scoped requests, gates every operation on the language
server's advertised capabilities, and preserves source-server provenance in
results. Several language servers may serve one immutable project root.

Calling `deixis_server_status` without arguments lists every configured server
as `not started`, `running`, or `attached`. A server is attached after Deixis
has synchronized at least one document with its current process. Pass `server`
for its detailed lifecycle and capability snapshot; `start: true` also requires
an explicit server name.

See [DESIGN.md](DESIGN.md) for the protocol and architecture and
[TODO.md](TODO.md) for planned work.

## Installation

Language servers are separate programs; install the ones you configure and make
them visible in the environment of the MCP host.

### Prebuilt binaries

The [latest GitHub release] provides archives for x86-64 and ARM64 Linux, Intel
and Apple silicon macOS, and x86-64 Windows. Linux releases include both glibc
and static musl builds.

Install the appropriate release automatically on Linux or macOS:

```console
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/jolars/deixis/releases/latest/download/deixis-installer.sh | sh
```

Or from PowerShell on Windows:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/jolars/deixis/releases/latest/download/deixis-installer.ps1 | iex"
```

The installers place `deixis` in Cargo's binary directory. Each release also
includes SHA-256 checksums and GitHub build attestations. Verify a downloaded
archive with:

```console
gh attestation verify deixis-aarch64-apple-darwin.tar.xz --repo jolars/deixis
```

[latest GitHub release]: https://github.com/jolars/deixis/releases/latest

### Nix

Install the default flake package:

```console
nix profile install github:jolars/deixis
```

Or run it without installing:

```console
nix run github:jolars/deixis -- --root /path/to/project --config /path/to/config.toml
```

### Cargo

Rust 1.98.0 or newer is required:

```console
cargo install deixis --locked
```

The crates.io package belongs to the MCP Registry identity
`mcp-name: io.github.jolars/deixis`.

To build a checkout instead:

```console
git clone https://github.com/jolars/deixis.git
cd deixis
cargo build --release --locked
```

The resulting binary is `target/release/deixis` (`deixis.exe` on Windows).

## Configure language servers

Start with the tested [example configuration](examples/config.toml), or define a
single server:

```toml
[servers.rust]
command = "rust-analyzer"
file_extensions = { ".rs" = "rust" }
```

Pass the file explicitly with `--config`, or install it as the user
configuration:

  | Platform             | Default path when `XDG_CONFIG_HOME` is unset       |
  | -------------------- | -------------------------------------------------- |
  | Linux and other Unix | `~/.config/deixis/config.toml`                     |
  | macOS                | `~/Library/Application Support/deixis/config.toml` |
  | Windows              | `%APPDATA%\deixis\config.toml`                     |

`$XDG_CONFIG_HOME/deixis/config.toml` takes precedence on every platform when
that variable is set. An explicit `--config` takes precedence over the user
configuration. Deixis never discovers configuration in the project tree.

The configuration is strict: unknown fields, empty commands, invalid routes, and
zero-valued bounds stop startup with an error. The [configuration
reference](docs/configuration.md) documents every field, default, routing rule,
and process limit.

## Connect an MCP client

Deixis is a local stdio server. The MCP host must launch the binary directly; do
not wrap it in a shell command. Set the project either with `--root` or by
starting Deixis in the project directory. The root defaults to the current
working directory and is canonicalized once at startup.

### Codex

Add Deixis from the command line:

```console
codex mcp add deixis --env RUST_LOG=deixis=info -- /absolute/path/to/deixis --root /absolute/path/to/project --config /absolute/path/to/config.toml
```

Or add a project-scoped `.codex/config.toml`:

```toml
[mcp_servers.deixis]
command = "/absolute/path/to/deixis"
args = [
  "--root",
  "/absolute/path/to/project",
  "--config",
  "/absolute/path/to/config.toml",
]
tool_timeout_sec = 70

[mcp_servers.deixis.env]
RUST_LOG = "deixis=info"
```

The 70-second host tool timeout accommodates the default 30-second LSP startup
and request bounds when the first tool call starts a server lazily. Codex's
`startup_timeout_sec` applies to the Deixis MCP handshake, not to a downstream
language server. See the current [Codex MCP documentation] for all host-side
options.

[Codex MCP documentation]: https://learn.chatgpt.com/docs/extend/mcp

### JSON-based MCP hosts

For a host that uses an `mcpServers` JSON object, use the equivalent stdio
entry. Consult the host's documentation for its configuration file location.

```json
{
  "mcpServers": {
    "deixis": {
      "command": "/absolute/path/to/deixis",
      "args": [
        "--root",
        "/absolute/path/to/project",
        "--config",
        "/absolute/path/to/config.toml"
      ],
      "env": {
        "RUST_LOG": "deixis=info"
      }
    }
  }
}
```

Use absolute paths when the host does not inherit your interactive shell's
`PATH`. The configured language-server commands must also resolve in the host's
environment.

### Home Manager

The flake provides `homeManagerModules.default`. A nonempty typed server catalog
installs Deixis, writes its user configuration, and registers one root-agnostic
command in `programs.mcp.servers`:

Add the flake input:

```nix
inputs.deixis = {
  url = "github:jolars/deixis";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Then import and configure the Home Manager module:

```nix
{ inputs, ... }:

{
  imports = [ inputs.deixis.homeManagerModules.default ];

  programs.deixis = {
    enable = true;
    servers = {
      rust = {
        command = "rust-analyzer";
        fileExtensions.".rs" = "rust";
      };
      typescript = {
        command = "typescript-language-server";
        args = [ "--stdio" ];
        fileExtensions.".ts" = "typescript";
      };
    };
  };
}
```

The generated command has no fixed root, so each MCP process binds to its
working directory. Set `programs.deixis.configFile` instead of `servers` to
install an existing TOML file. The two options are mutually exclusive.

## Operation

Language servers start only when selected by a tool call. File-scoped tools
accept a project-relative or root-contained absolute `path` and an optional
configured `server` name. Position-based tools also accept:

```json
{ "line": 12, "character": 8 }
```

Both values are zero-based; `character` is a UTF-8 byte offset. If several
servers match a file, supply `server` or make the configuration routes unique.
Without a `server`, `workspace_symbols` fans out to capable attached servers
without starting others and merges results in stable server-name order. Supply
`server` to query that server alone, starting it if necessary.

Successful calls return structured JSON and a concise text fallback. Tool
failures return `isError: true` with a stable structured error code. Null or
empty semantic results may also report `readiness` and `resultStability`; a
`transient` result means the language server has signaled that it is still
working.

Symbol rename is deliberately a two-step mutation. By default,
`prepare_rename` and `preview_rename` are available for read-only inspection,
but `apply_rename` is neither advertised nor callable. Add
`--allow-mutation` to the Deixis command when configuring the MCP host to opt
into application. Then call `preview_rename` with the file, UTF-8 position, and
`newName`; inspect its structured per-file edits and unified diff; and pass its
opaque `previewId` to `apply_rename`. Previewing does not modify files. The ID
authorizes only that exact preview, expires after ten minutes, and is consumed
by the first apply attempt—including a failed attempt. A second apply requires
a new preview.

Rename accepts only text edits to existing UTF-8 files contained by the
immutable project root. It rejects file creation, deletion, rename operations,
change annotations, external paths, overlapping edits, and stale file contents.
Application stages every replacement before committing any file and attempts
to restore all originals if a commit fails. This is an in-process transaction,
not a power-loss guarantee; Deixis does not promise recovery after a process or
machine crash. Server-initiated `workspace/applyEdit` requests remain rejected
because they do not carry explicit preview authorization.

## Logging

Deixis reserves stdout for MCP frames. Its logs and all child-process stderr go
to stderr. Logging defaults to `deixis=info`; set `RUST_LOG` in the MCP host's
environment to change the filter:

```text
RUST_LOG=deixis=debug
```

Server names are attached to child-process and LSP log events. Normal info logs
do not include source contents or protocol bodies. See
[Troubleshooting](docs/troubleshooting.md) for startup, routing, timeout,
diagnostic, and shutdown failures.

## Development

The repository pins Rust 1.98.0. Entering the devenv shell supplies the complete
toolchain and installs the pre-commit hooks:

```console
devenv shell
task check
```

The opt-in compatibility suite exercises TypeScript Language Server, Pyright,
gopls, clangd, and Deno using versions pinned by `flake.lock`:

```console
task compatibility
```

Without Nix, install those five servers and run:

```console
cargo test --test real_language_servers -- --ignored --test-threads=1
```

Executable paths may be overridden with `DEIXIS_TYPESCRIPT_LANGUAGE_SERVER`,
`DEIXIS_PYRIGHT_LANGSERVER`, `DEIXIS_GOPLS`, `DEIXIS_CLANGD`, and `DEIXIS_DENO`.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the complete development gate.

## License

Licensed under either the [Apache License, Version 2.0](LICENSE-APACHE) or the
[MIT License](LICENSE-MIT), at your option.
