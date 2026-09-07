# Deixis Design

This document records the architecture and the reasons behind it. `TODO.md`
contains the remaining implementation sequence.

## Status

The repository currently contains the project infrastructure, strict
language-server configuration parsing, immutable project-root startup handling,
a portable mock language-server fixture, a valid MCP stdio server, and an
internal lazy lifecycle manager for each configured language server. It can
parse explicit or user-owned startup configuration, negotiate an MCP session
over stdio, report its name and version, route a project file to a server, start
that server on demand, handle common server-to-client messages, continue after
malformed server output, and shut down all started servers with a bounded
forced-kill fallback. A configured session exposes the read-only
`deixis_server_status` lifecycle probe, capability-gated navigation tools, a
`diagnostics` tool, and a guarded symbol-rename workflow. The semantic tools
synchronize project-contained documents and translate negotiated position
encodings. Hover returns structured markup; navigation normalizes LSP location
variants and retains configured-server provenance. Diagnostics prefers pull
reports when advertised and otherwise exposes cached push reports with explicit
freshness. Workspace-symbol search fans out concurrently across capable
servers, then merges results in stable lexical server-name order with explicit
provenance. Document symbols preserve server-provided hierarchies and normalize
legacy flat responses to a common node shape. Rename separates validation and
preview from an explicitly authorized, one-shot application. Every tool
includes a concise text fallback.
The status above and the user-facing capability list in `README.md` distinguish
shipped behavior from future design. `TODO.md` contains only open work; Git
history and `CHANGELOG.md` record completed development.

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
deixis [--root <project>] [--config <config.toml>] [--allow-mutation]
```

`--root` defaults to the current directory. The selected path is canonicalized
once and cannot change during the session. An explicit `--config` takes
precedence over the platform user configuration directory. On Linux, that is
`$XDG_CONFIG_HOME/deixis/config.toml`, falling back to
`~/.config/deixis/config.toml`; macOS and Windows use their native user
configuration directories when `XDG_CONFIG_HOME` is unset. Deixis never
discovers configuration in the project tree, so cloned content cannot silently
authorize an executable command. Language-server definitions remain
declarative TOML. Mutation is disabled by default. `--allow-mutation` is an
explicit, process-lifetime opt-in that adds mutating tools to the advertised and
callable MCP surface.

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
startup_ms = 30000
request_ms = 30000
shutdown_ms = 5000

[servers.rust.limits]
outbound_queue_capacity = 64
max_concurrent_requests = 16
max_response_bytes = 16777216

[servers.rust.restart]
max_restarts = 3
window_ms = 60000
```

Bounds apply independently to each configured server. Initialization and
ordinary requests default to 30 seconds, while graceful shutdown defaults to 5
seconds. Each server admits at most 16 concurrent requests, buffers at most 64
outbound messages, and accepts an LSP message body no larger than 16 MiB. Every
value is configurable and must be greater than zero. A server may start at most
three replacement processes within a sliding 60-second window by default.

Extension and glob values are the LSP language IDs used for document
synchronization. Within one server, a matching glob takes precedence over an
extension, and the longest matching extension wins. Matching routes in several
servers are an error unless the caller supplies an explicit server name. This
keeps routing independent of TOML table order. By default, workspace-symbol
requests fan out concurrently to capable attached servers without starting
others. An explicit server selects that server alone and may start it. Merged
results use stable lexical server-name order and retain the originating server
name.

Commands run directly, without a shell. They inherit the Deixis environment plus
explicit overrides. Configuration never causes package installation, network
access, or command interpolation.

## MCP surface

Without selected configuration, the current server advertises no tools. With
configuration, it advertises twelve read-only tools: `deixis_server_status`,
`hover`, `definition`, `declaration`, `type_definition`, `implementation`,
`references`, `diagnostics`, `document_symbols`, `workspace_symbols`,
`prepare_rename`, and `preview_rename`. Starting Deixis with
`--allow-mutation` advertises and enables `apply_rename` as a thirteenth tool.
The probe accepts an optional server name and `start` flag.
Without a name, it returns every configured server in stable lexical order with
a compact `not started`, `running`, or `attached` state. `Attached` means that
Deixis has synchronized at least one document with the current server process.
With a name, the probe returns the detailed lifecycle, readiness, negotiated
encoding, and capabilities for that server. Starting a server requires its
explicit name.
The position-based semantic tools take a root-contained path, a zero-based
UTF-8 position, and an optional server override. They resolve the path before
routing, infer the language ID, synchronize the document, verify the
corresponding server capability, and translate the request through the
negotiated position encoding.
Hover returns the LSP contents and optional range as structured JSON. The four
navigation tools accept `Location`, `Location[]`, `LocationLink[]`, or `null`,
and normalize every result to the `LocationLink` superset: configured server
name, target URI, target range, target selection range, target position
encoding, and an optional origin selection range. The references tool
additionally requires an explicit `includeDeclaration` boolean, sends it in the
LSP reference context, and normalizes `Location[]` or `null` to the configured
server name, URI, range, and range position encoding. Each semantic tool also
returns readable text content. No tool forwards arbitrary JSON-RPC.

The `diagnostics` tool takes a root-contained path and optional server override.
After synchronizing the document, it requests `textDocument/diagnostic` when
the server advertises pull diagnostics. Full pull reports are cached by result
ID so an unchanged response can reuse the prior items. Otherwise, the tool
returns the newest cached `textDocument/publishDiagnostics` report for the
document URI. A push report whose version matches the synchronized document is
`current`; a mismatched or versionless report is `stale`, and a missing report
is `unavailable`. Older and versionless notifications do not replace a newer
versioned report. Current diagnostic ranges are converted to UTF-8; stale ranges
retain the negotiated server encoding because the old source text is not
available for sound conversion. Reports preserve diagnostic extension fields
and identify both their source and position encoding.

The `document_symbols` tool takes a root-contained path and optional server
override. Deixis advertises hierarchical document-symbol, symbol-kind, tag, and
label support. Recursive `DocumentSymbol` responses retain their child trees.
Deprecated flat `SymbolInformation` responses become root nodes in the same
shape, using the location range as both the symbol range and selection range;
the flat `containerName` remains metadata rather than being treated as a stable
parent identity. Each node identifies its configured server, URI, and range
encoding. Ranges in readable project files are translated to UTF-8, while
other locations retain the negotiated server encoding.

The `workspace_symbols` tool requires a query string, including an empty string
when the caller wants all symbols. Without a server name, it concurrently
queries the attached servers and does not start any others. With a server name,
it queries that server alone, starting it if necessary. Incapable selected
servers are skipped, and responses are merged in stable lexical server-name
order. If none of the selected servers supports the method, the tool returns an
`unsupported_capability` error. Each symbol includes the configured server name
and its LSP name, kind, location, tags, container, deprecation marker, data, and
extension fields when present. Location ranges in readable project files are
translated to UTF-8; other locations retain the server's negotiated encoding.
Deixis advertises workspace-symbol kind and tag support, but not lazy resolve
support, so returned locations must include a range.

### Workspace-edit and rename safety contract

Rename is split across three tools. The mutating third step is available only
when the process starts with `--allow-mutation`. `prepare_rename` calls
`textDocument/prepareRename` when the selected server advertises prepare
support. `preview_rename` calls `textDocument/rename`, normalizes the returned
`WorkspaceEdit`, and returns structured per-file edits together with a unified
diff. Neither tool changes the filesystem. `apply_rename` accepts only the
opaque preview ID produced by `preview_rename`; possession of that ID is the
explicit authorization to apply that exact set of edits. An ID is valid for ten
minutes, may be used once, and is consumed by the first apply attempt whether
that attempt succeeds or fails. The process retains at most 16 previews and 64
MiB of source and replacement text, evicting the oldest previews as needed.

The accepted `WorkspaceEdit` subset is intentionally narrow. Deixis supports
the `changes` map and text-document edits in `documentChanges`; when both are
present, `documentChanges` is authoritative. Every target must be an existing,
readable UTF-8 file whose canonical path remains inside the immutable project
root. Each canonical file may appear only once. Change annotations, annotated
text edits, and resource operations such as create, rename, and delete are
rejected. LSP ranges are converted from the negotiated server encoding to the
public UTF-8 coordinate system, checked against the request-time document
snapshots, sorted, and rejected if they overlap. If an edited document changes
in the document store while the rename request is in flight, normalization
returns an `edit_conflict` instead of interpreting the response against a newer
buffer. A supplied document version must match the request-time synchronized
document version; a version on an unsynchronized document is invalid.

The preview retains the exact contents read for every target. Application
compares every canonical path and its complete contents with that snapshot
before staging and again before commit. Any mismatch produces an
`edit_conflict` without intentional writes to a project file. Deixis stages
each replacement as a new sibling file whose permissions are restricted to the
target's permissions before any replacement content is written, then flushes
its contents. It removes the current and prior stages after any staging error,
and reports cleanup failures. Only after every replacement is staged, the
second validation succeeds, and cancellation is checked again does it move
each original to a unique sibling backup and install the staged replacement.
The commit itself is not cancelable. A failure during commit triggers
reverse-order rollback of every file touched by the transaction. An incomplete
rollback receives the distinct
`edit_rollback_failed` code. Semantic tool calls take a shared workspace lock,
while apply holds it exclusively, so Deixis does not synchronize documents
against its own partially committed transaction.

After a successful commit, Deixis resynchronizes each affected document already
open in the responsible language server before releasing the workspace lock.
If refresh fails, it invalidates that server process so a later operation
cannot reuse an authoritative stale buffer.

This transaction protects against ordinary failures while the process remains
alive. It does not provide a durable journal, directory synchronization, or
recovery after process termination, operating-system failure, or power loss.
Concurrent programs can also change files outside Deixis's lock; the two exact
content checks are the correctness backstop before commit, but they cannot
eliminate filesystem time-of-check/time-of-use races.

Deixis advertises LSP workspace-edit support for text-only transactional
failure handling and rename prepare support. It still rejects unsolicited
server-to-client `workspace/applyEdit` requests: those requests bypass the MCP
preview ID and therefore lack authorization.

MCP hosts already namespace tools by server, so tool names do not repeat an
`lsp_` prefix. Every handler checks the downstream server capability before
sending a request. Unsupported operations fail clearly instead of sending a
request and interpreting a server error as an empty result.

Tool arguments and results follow LSP structures where practical. Locations,
ranges, symbol kinds, hover markup, diagnostic severities, and related
information remain structured JSON. Each successful result also supplies a small
textual representation for MCP clients that do not consume structured content.
These fallbacks have deterministic, tool-specific formats rather than serialized
JSON. Status is one sentence; hover uses the rendered markup body; navigation
and references use one location per line; diagnostics summarize availability and
count; and symbols use one line per symbol, with indentation for document-symbol
children. Extension fields and other details that do not belong in the concise
text remain available in the structured result and do not perturb its rendering.

Deixis advertises `window.workDoneProgress` and the rust-analyzer
`experimental.serverStatusNotification` extension. It tracks active work-done
tokens and rust-analyzer's health and quiescence values. The lifecycle probe
exposes the resulting `readiness` state and its source. When hover, navigation,
references, document symbols, or current diagnostics return no semantic result,
their structured output also includes `readiness` and a derived
`resultStability`: `transient` while observed work is active, `stable` after
observed work has completed, and `indeterminate` when the server has emitted no
usable readiness signal or reports degraded health. Initialization by itself
leaves readiness `unknown`.

Deixis acknowledges `workspace/diagnostic/refresh` when a server sends it, but
does not advertise proactive refresh support. Diagnostics remain
request-driven, and the next tool call asks the server for the current report.
This defensive acknowledgement is required for compatibility with Pyright after
it dynamically registers pull diagnostics.

Public positions use zero-based `line` and `character` fields. Input positions
and ranges for readable project files use UTF-8 code-unit offsets so the same
request and result are stable regardless of the selected language server. The
document layer translates positions to and from the server's negotiated UTF-8,
UTF-16, or UTF-32 encoding and rejects missing lines, offsets past a line end,
reversed ranges, and offsets that split a code point. LF, CRLF, and CR are
logical line endings and cannot be addressed from within a position. Location
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
   client identity, client capabilities, and configured initialization options,
   then wait no longer than the configured startup timeout.
3. Record server identity, capabilities, text synchronization mode, and position
   encoding when the server reports them.
4. Send `initialized`, then allow routed requests.

Shutdown sends `shutdown`, waits for its response, sends `exit`, closes the
pipes, and waits for the child. A bounded timeout ends in forced termination so
an unresponsive language server cannot keep the MCP process alive. If the
shutdown response itself times out, Deixis still sends `exit`, waits for the
child, and force-kills it when needed.

An unexpected exit fails outstanding calls for that server; those calls are not
replayed. The next operation routed to the failed server retires and reaps that
generation before starting a replacement. Document, diagnostic, registration,
progress, and request state belong to one generation, so the replacement starts
clean and resynchronizes documents from disk on demand. A per-server sliding
window admits three replacements in 60 seconds by default. Once exhausted,
operations fail without spawning another child until an attempt ages out of the
window. A bounded tail of eight child-stderr lines accompanies server-exit and
restart-limit errors and appears in restart logs with the configured server
name.

### Server-to-client messages

The client side handles the common requests language servers initiate:

- `workspace/configuration` returns values derived from the resolved, immutable
  configuration, using dotted section paths when the server supplies them;
- workspace-folder queries return the single project root;
- dynamic capability registration and unregistration maintain in-memory state;
- log-message, show-message, and show-message request traffic is represented in
  tracing output; and
- `workspace/applyEdit` is rejected because server-initiated edits lack an
  authorized MCP rename preview.

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
than returning an unqualified empty list. A current but empty report is likewise
qualified by server readiness, which distinguishes a startup-time empty from a
stable no-diagnostic result when the server publishes readiness signals.

## Concurrency and failure handling

Tokio owns MCP I/O, child processes, timers, and LSP I/O. Requests to different
servers may run concurrently. Within one server, outgoing requests are
correlated by ID, while incoming notifications remain ordered.

Every downstream request has a timeout and a cancellation path. Its deadline
includes time spent waiting for a per-server concurrency slot. MCP cancellation
forwards `$/cancelRequest` when the language server supports the request in
flight. Dropped MCP connections trigger orderly child shutdown. Bounded
per-server channels prevent outbound backlogs from creating unbounded memory
growth; a full queue fails immediately. The LSP reader checks `Content-Length`
before allocating or parsing a body and stops a transport that exceeds its
configured response-size limit.

Errors are translated at the MCP boundary with enough context to act on them:
tool name, server name, method, project-relative path when applicable, and the
underlying LSP or process error. Protocol data and source contents are not
written to normal info logs.

Tool execution failures set `isError` and return the same concise message as
text plus a structured envelope under `structuredContent.error`. Every envelope
contains a stable `code`, `message`, and `tool`; it includes `server`, `method`,
and `path` when those values are known. Timeouts add `timeoutMs`. Downstream
JSON-RPC failures add an `lspError` object containing the server's numeric code,
message, and optional data. The public codes are `invalid_path`,
`invalid_position`, `unsupported_capability`, `request_timeout`, `server_busy`,
`server_exited`, `lsp_error`, `server_start_failed`, `lsp_protocol_error`,
`document_error`, `request_canceled`, `server_error`, `routing_error`,
`no_server_configured`, `unknown_server`, `invalid_workspace_edit`,
`invalid_preview`, `edit_conflict`, `edit_application_failed`, and
`edit_rollback_failed`. Malformed tool arguments remain MCP `invalid_params`
protocol errors because tool execution has not begun.

When a language server closes stdout, the reader resolves every pending request
immediately as a server-exit failure. Those calls do not wait for their
individual request deadlines. A later request may replace that server within
its restart budget; it never retries a request whose outcome became uncertain.

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
parameterless `shutdown`, send parameterless `exit` after its response, wait for
the child, and use Tokio's platform-specific kill primitive after the configured
timeout. A server that exits cleanly while handling `shutdown` is accepted even
if it omits the response. The GitHub Actions test matrix runs `cargo test
--all-targets --locked` on Ubuntu, macOS, and Windows; formatting, Clippy, and
rustdoc checks run on Ubuntu.

## Testing strategy

Protocol and process behavior is tested from the outside wherever possible. The
permanent test layers are:

- a real-binary MCP handshake and shutdown test;
- an in-repository mock language-server process for deterministic lifecycle,
  routing, diagnostics, cancellation, and malformed-message tests;
- focused unit tests for configuration, path containment, position conversion,
  document versions, and result normalization; and
- opt-in compatibility tests against real TypeScript Language Server, Pyright,
  gopls, clangd, and Deno processes supplied by a pinned Nix shell.

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
definitions into source text, and requiring an inspectable, one-shot-authorized
preview before applying rename edits.

The [official Rust MCP SDK] owns MCP framing and version negotiation. Deixis
will not implement that protocol independently.

[Serena]: https://github.com/oraios/serena
[johnhnguyen97/lsp-mcp]: https://github.com/johnhnguyen97/lsp-mcp
[Tritlo/lsp-mcp]: https://github.com/Tritlo/lsp-mcp
[isaacphi/mcp-language-server]: https://github.com/isaacphi/mcp-language-server
[official Rust MCP SDK]: https://github.com/modelcontextprotocol/rust-sdk

## Deferred work

Code actions and formatting remain deferred; each would require its own public
tool workflow even if it reused the rename transaction machinery. Streamable
HTTP, MCP resources and prompts, server installation, a built-in language
catalog, multiple project roots, and long-running MCP tasks are also deferred
until demonstrated use requires them.
