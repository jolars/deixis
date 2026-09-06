# Deixis Design

This document records the intended architecture and the reasons behind it.
`TODO.md` turns that architecture into an implementation sequence.

## Status

The repository currently contains the project infrastructure, strict
language-server configuration parsing, immutable project-root startup handling,
a portable mock language-server fixture, a valid MCP stdio server, and an
internal lazy lifecycle manager for each configured language server. It can
parse explicit or user-owned startup configuration, negotiate an MCP session
over stdio, report its name and version, route a project file to a server, start
that server on demand, handle common server-to-client messages, continue after
malformed server output, and shut down all started servers with a bounded
forced-kill fallback. A configured session
exposes the read-only `deixis_server_status` lifecycle probe and
capability-gated `hover` and `definition` tools. Both semantic tools synchronize
project-contained documents and translate negotiated position encodings. Hover
returns structured markup; definition normalizes LSP location variants and
retains configured-server provenance. Both include a concise text fallback.
Nothing in this document should be read as already implemented unless it is
also marked complete in `TODO.md`.

## Purpose

Deixis gives coding agents direct access to language-server capabilities. It is
a bridge between two protocols, not a second editor or a general coding-agent
framework.

In linguistics, deixis is the act of identifying a referent through context. The
name reflects the server's central operation: resolve the meaning and
relationships of a program symbol from its document and position.

The primary user is an MCP client that already knows how to read files, edit
them, search text, and run commands, but lacks semantic navigation. That client
should be able to ask the same questions that an editor asks its language
servers without adopting Serena's project, memory, shell, or editing layers.

## Goals and non-goals

The project should:

- expose named, typed LSP operations as MCP tools;
- support polyglot projects through several language-server subprocesses;
- preserve LSP capability negotiation, position semantics, cancellation, and
  document synchronization;
- return structured, machine-readable results with concise textual fallbacks;
- be deterministic and explicit about configuration; and
- work on Linux, macOS, and Windows.

The project will not:

- provide general file search, file editing, shell execution, or persistent
  agent memory;
- build its own semantic index or support a non-LSP analysis backend;
- install or download language servers;
- infer executable commands from untrusted project contents;
- expose unrestricted JSON-RPC forwarding as a public MCP tool; or
- manage several unrelated project roots in one process.

## Runtime model

One `deixis` process is bound to one project root for its entire lifetime. It
may supervise several language servers for that project.

```text
MCP client
    │ MCP over stdin/stdout
    ▼
MCP adapter and typed tool handlers
    │
    ▼
Project-scoped router ───── document state and diagnostics
    │
    ├── LSP client ── stdio ── language server A
    ├── LSP client ── stdio ── language server B
    └── LSP client ── stdio ── language server C
```

This boundary keeps project identity, file routing, and child cleanup simple.
Users who need several projects run several MCP server instances, which is also
how MCP hosts naturally isolate project-scoped tools.

Both protocol layers normally use stdio, but they never share streams. The MCP
client owns the `deixis` process's stdin and stdout; each language server
receives a distinct pair of child-process pipes. All logs and child stderr go to
the `deixis` process's stderr.

Streamable HTTP may be added as an MCP transport later without changing the LSP
layer. It is not part of the first functional release.

## Startup and configuration

The invocation is:

```console
deixis [--root <project>] [--config <config.toml>]
```

`--root` defaults to the current directory. The selected path is canonicalized
once and cannot change during the session. An explicit `--config` takes
precedence over the platform user configuration directory. On Linux, that is
`$XDG_CONFIG_HOME/deixis/config.toml`, falling back to
`~/.config/deixis/config.toml`; macOS and Windows use their native user
configuration directories when `XDG_CONFIG_HOME` is unset. Deixis never
discovers configuration in the project tree, so cloned content cannot silently
authorize an executable command. Language-server definitions remain
declarative TOML.

Configuration parsing rejects unknown fields. A server entry contains the
following explicit concepts:

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
request_ms = 30000
shutdown_ms = 5000
```

Extension and glob values are the LSP language IDs used for document
synchronization. Within one server, a matching glob takes precedence over an
extension, and the longest matching extension wins. Matching routes in several
servers are an error unless the caller supplies an explicit server name. This
keeps routing independent of TOML table order. Workspace-wide operations will
fan out to all capable configured servers and merge results in stable lexical
server-name order, retaining the originating server name.

Commands run directly, without a shell. They inherit the Deixis environment plus
explicit overrides. Configuration never causes package installation, network
access, or command interpolation.

## MCP surface

Without selected configuration, the current server advertises no tools. With
configuration, it advertises the read-only `deixis_server_status` lifecycle
probe, `hover`, and `definition`. The probe accepts an optional server name and
`start` flag; without a name, it uses the first name in stable lexical order.
The semantic tools take a root-contained path, a zero-based UTF-8 position, and
an optional server override. They resolve the path before routing, infer the
language ID, synchronize the document, verify the corresponding server
capability, and translate the request through the negotiated position encoding.
Hover returns the LSP contents and optional range as structured JSON.
Definition accepts `Location`, `Location[]`, `LocationLink[]`, or `null`, and
normalizes every result to the `LocationLink` superset: configured server name,
target URI, target range, target selection range, target position encoding, and
an optional origin selection range. Each semantic tool also returns readable
text content. No tool forwards arbitrary JSON-RPC.

The semantic tool set remains read-only. It starts with `hover` and
`definition`; planned additions are:

- `declaration`;
- `type_definition` and `implementation`;
- `references`;
- `document_symbols` and `workspace_symbols`; and
- `diagnostics`.

MCP hosts already namespace tools by server, so tool names do not repeat an
`lsp_` prefix. Every handler checks the downstream server capability before
sending a request. Unsupported operations fail clearly instead of sending a
request and interpreting a server error as an empty result.

Tool arguments and results follow LSP structures where practical. Locations,
ranges, symbol kinds, hover markup, diagnostic severities, and related
information remain structured JSON. Each successful result also supplies a small
textual representation for MCP clients that do not consume structured content.

Public positions use zero-based `line` and `character` fields. Input positions
and ranges for readable project files use UTF-8 code-unit offsets so the same
request and result are stable regardless of the selected language server. The
document layer translates positions to and from the server's negotiated UTF-8,
UTF-16, or UTF-32 encoding and rejects missing lines, offsets past a line end,
reversed ranges, and offsets that split a code point. LF, CRLF, and CR are
logical line endings and cannot be addressed from within a position. Definition
targets outside the project, including virtual-document URI schemes, are never
read merely because a server returned them. Their ranges retain the negotiated
encoding, which is reported in `targetPositionEncoding`; an origin selection
range still refers to the synchronized project document and is converted to
UTF-8.

Deixis advertises UTF-8, UTF-16, and UTF-32 position support in that preference
order. An omitted server selection defaults to UTF-16 as required for backward
compatibility; a server that selects an encoding Deixis did not offer fails
initialization.

A generic `lsp_request(method, params)` tool is intentionally excluded. It would
bypass capability checks, input validation, path containment, output
normalization, and the auditability of a finite tool surface.

## Language-server client

The LSP layer owns process startup, JSON-RPC correlation, incoming requests and
notifications, capability state, cancellation, and shutdown. The first
vertical-slice spike kept `async-lsp` out of the production dependency graph and
implemented a small JSON-RPC stdio transport behind an internal boundary
instead. That choice keeps this phase focused on Deixis's required process
semantics: direct configured command execution, ordered child-stdin writes,
request ID correlation, per-request timeout and cancellation, server-to-client
requests, stderr draining, and bounded shutdown with forced termination.

`async-lsp` remains a candidate once document synchronization and the first
semantic tools need broader typed LSP coverage. The rest of the application
depends on the internal client boundary so the transport can still be replaced
without changing MCP handlers.

### Lifecycle

Servers start lazily when routing selects them. Startup proceeds as follows:

1. Spawn the configured command with piped stdin and stdout and inherited or
   piped stderr.
2. Send `initialize` with the immutable project root, one workspace folder,
   client identity, client capabilities, and configured initialization options.
3. Record server identity, capabilities, text synchronization mode, and position
   encoding when the server reports them.
4. Send `initialized`, then allow routed requests.

Shutdown sends `shutdown`, waits for its response, sends `exit`, closes the
pipes, and waits for the child. A bounded timeout ends in forced termination so
an unresponsive language server cannot keep the MCP process alive. If the
shutdown response itself times out, Deixis still sends `exit`, waits for the
child, and force-kills it when needed.

An unexpected exit fails outstanding calls for that server and records the
failure. Automatic restart will be bounded and opt-in to the relevant request;
there will be no unbounded crash loop.

### Server-to-client messages

The client side handles the common requests language servers initiate:

- `workspace/configuration` returns values derived from the resolved, immutable
  configuration, using dotted section paths when the server supplies them;
- workspace-folder queries return the single project root;
- dynamic capability registration and unregistration maintain in-memory state;
- log-message, show-message, and show-message request traffic is represented in
  tracing output; and
- `workspace/applyEdit` is rejected while the project is read-only.

Unknown requests receive the correct method-not-found response. Notifications
that are not relevant may be ignored, but they must not stall the reader loop.
Malformed JSON-RPC bodies from the server are logged with the configured server
name, and the reader loop continues so later valid responses can complete their
requests.

## Documents and filesystem consistency

LSP requests often require an open document even though MCP clients edit files
outside Deixis. The document layer therefore treats disk contents as the source
of truth and synchronizes lazily:

1. Canonicalize the requested path and verify that it is inside the project
   root. A symlink that resolves outside the root is not a valid input.
2. Read UTF-8 text and hash it before each file-scoped operation.
3. Send `textDocument/didOpen` the first time a server sees that document.
4. If the hash changed, increment the document version and send a full-content
   replacement. For an incremental-only server, represent the replacement as one
   range covering the previous document.
5. Send `textDocument/didClose` when a server or the MCP process stops.

This correctness-first approach avoids a filesystem watcher in the first release
and observes edits made by any MCP client. A watcher may later refresh
diagnostics proactively, but it must remain an optimization rather than a second
source of truth.

Input paths are contained by the project root. Language-server results may refer
to standard libraries or dependencies outside it; those locations are returned
but do not grant permission to read or edit them.

## Diagnostics

Push diagnostics are cached by server, document URI, and document version. Pull
diagnostics are requested when a server advertises the corresponding capability.
The `diagnostics` tool synchronizes the requested document first, then returns
the newest known report with its server and version provenance.

Stale reports are never silently presented as current. If a server has not yet
published diagnostics for the synchronized version, the result says so rather
than returning an unqualified empty list.

## Concurrency and failure handling

Tokio owns MCP I/O, child processes, timers, and LSP I/O. Requests to different
servers may run concurrently. Within one server, outgoing requests are
correlated by ID, while incoming notifications remain ordered.

Every downstream request has a timeout and a cancellation path. MCP cancellation
forwards `$/cancelRequest` when the language server supports the request in
flight. Dropped MCP connections trigger orderly child shutdown. Bounded channels
prevent a noisy server from creating unbounded memory growth.

Errors are translated at the MCP boundary with enough context to act on them:
tool name, server name, method, project-relative path when applicable, and the
underlying LSP or process error. Protocol data and source contents are not
written to normal info logs.

## Repository shape

The bootstrap is a single binary crate. It should remain one package until a
real distribution or dependency boundary justifies another crate. As work
begins, the binary entry point should become a thin composition layer around
private modules for configuration, MCP handlers, project routing, documents, and
the LSP client.

No stable Rust library API is promised. The public contract is the executable's
CLI, configuration schema, MCP capability declaration, and tool schemas.

## Platform behavior

Deixis uses Rust and Tokio process APIs for Linux, macOS, and Windows rather
than platform-specific shell behavior. Configured commands are passed directly
to `tokio::process::Command`, with arguments and environment overrides supplied
as structured values. The project root and configuration path are canonicalized
with the host filesystem rules before MCP serving begins.

Language-server stdin, stdout, and stderr are separate child pipes on every
platform. MCP stdout remains reserved for MCP frames, while Deixis logs and
child stderr are written to the parent process's stderr. The LSP layer writes
Content-Length-framed messages to child stdin, reads child stdout, and joins the
I/O tasks after shutdown or forced termination.

Path-to-URI conversion normalizes Windows backslashes to forward slashes,
preserves drive-letter paths, and percent-encodes bytes that are not valid URI
path characters. The test fixture compiles its mock language-server executable
with `rustc` and appends `std::env::consts::EXE_SUFFIX`, so Windows uses `.exe`
while Unix-like systems use no suffix.

The shutdown contract is the same on Linux, macOS, and Windows: send
`shutdown`, send `exit`, wait for the child, and use Tokio's platform-specific
kill primitive after the configured timeout. The GitHub Actions test matrix runs
`cargo test --all-targets --locked` on Ubuntu, macOS, and Windows; formatting,
Clippy, and rustdoc checks run on Ubuntu.

## Testing strategy

Protocol and process behavior is tested from the outside wherever possible. The
permanent test layers are:

- a real-binary MCP handshake and shutdown test;
- an in-repository mock language-server process for deterministic lifecycle,
  routing, diagnostics, cancellation, and malformed-message tests;
- focused unit tests for configuration, path containment, position conversion,
  document versions, and result normalization; and
- optional manual compatibility tests against real language servers.

CI must not download language servers or depend on network services. The mock
server is part of the workspace and behaves identically on Linux, macOS, and
Windows.

## Reference projects

[Serena] demonstrates the value of project-scoped semantic tools and broad
multi-language support. Its file operations, memory, shell, editing model, and
alternate JetBrains backend are outside this project's boundary.

[johnhnguyen97/lsp-mcp] provides a useful Rust module split between MCP tools,
an LSP client, configuration, and a multi-server manager. Its archived,
single-commit implementation is a reference rather than a base.

[Tritlo/lsp-mcp] demonstrates explicit LSP startup, document lifecycle,
diagnostic subscriptions, and code actions. Its extension, prompt, resource, and
logging-tool layers are not required here.

[isaacphi/mcp-language-server] validates a particularly simple deployment
contract: the project root, language-server command, and trailing server
arguments are fixed when the MCP process starts. It also demonstrates the value
of snapshotting tool output against several real language servers. Deixis keeps
those lessons, but differs by supporting several configured servers in one
project-scoped process, retaining structured LSP results instead of expanding
definitions into source text, and deferring its rename and file-edit tools until
a mutation safety contract exists.

The [official Rust MCP SDK] owns MCP framing and version negotiation. Deixis
will not implement that protocol independently.

[Serena]: https://github.com/oraios/serena
[johnhnguyen97/lsp-mcp]: https://github.com/johnhnguyen97/lsp-mcp
[Tritlo/lsp-mcp]: https://github.com/Tritlo/lsp-mcp
[isaacphi/mcp-language-server]: https://github.com/isaacphi/mcp-language-server
[official Rust MCP SDK]: https://github.com/modelcontextprotocol/rust-sdk

## Deferred work

Rename, code actions, formatting, and any application of workspace edits require
a separate design for preview, authorization, conflict detection, and rollback.
Streamable HTTP, MCP resources and prompts, server installation, a built-in
language catalog, multiple project roots, and long-running MCP tasks are also
deferred until demonstrated use requires them.
