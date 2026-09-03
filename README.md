# Deixis

Deixis is a planned Model Context Protocol (MCP) server that gives coding agents
direct, typed access to Language Server Protocol (LSP) operations. The name
comes from the linguistic act of pointing to a referent whose meaning depends on
context—much like resolving a symbol from its source position.

The project deliberately has a narrow scope. It will manage language servers and
expose their semantic capabilities; it will not grow a second filesystem, shell,
memory, indexing, or agent framework around them.

> [!NOTE]
> The project is at the bootstrap stage. The binary currently completes an MCP
> handshake over stdio and advertises no tools. It does not start or communicate
> with a language server yet.

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

To run the current handshake-only server:

```console
cargo run
```

The process reserves stdout for MCP messages. Set `RUST_LOG` to control logs,
which are always written to stderr.

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
