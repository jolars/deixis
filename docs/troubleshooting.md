# Troubleshooting

Most Deixis failures occur at one of three boundaries: the MCP host starting
Deixis, Deixis routing a file, or a configured language server handling an LSP
request. Structured tool errors identify the boundary with a stable `code` and,
when known, the tool, server, method, path, timeout, and downstream LSP error.

## Collect logs first

Set `RUST_LOG` on the Deixis MCP server entry, then restart the MCP host:

```text
RUST_LOG=deixis=debug
```

Logs go to stderr; stdout is reserved for MCP protocol frames. Each
language-server event includes its configured `server` name, and the last eight
lines of child stderr are retained for server-exit and restart-limit errors.
Avoid a wrapper that prints banners or diagnostics to stdout.

## Deixis does not connect

Check the MCP host entry before the language-server configuration:

1. Use an absolute path for `command` if the host has a restricted `PATH`.
2. Confirm that `--root` and `--config` point to existing paths. Deixis
   canonicalizes both before opening the MCP transport.
3. Pass arguments as an array rather than one shell-style string.
4. Inspect the host's MCP logs for Deixis stderr.

Deixis has no HTTP listener; it is a stdio MCP server. Its process should remain
running while the host keeps the MCP session open.

## The connection has no tools

An unconfigured Deixis process deliberately advertises no tools. This usually
means that `--config` was omitted and no user configuration was found. Check
the resolved platform path in the [configuration reference](configuration.md),
or pass an absolute `--config` path.

The configuration must contain at least one server. A parse or validation error
stops Deixis before the MCP session starts and is printed to stderr.

## A language server does not start

A `server_start_failed` error usually means that `command` is not resolvable in
the MCP host's environment. A command that works in an interactive shell may
depend on shell initialization, an alias, a development-environment hook, or a
different `PATH`.

- Prefer an absolute executable path while diagnosing.
- Put command arguments such as `--stdio` in `args`.
- Put required environment variables in the server's `environment` table.
- Read the captured child stderr attached to the error or Deixis logs.

Deixis executes the command directly and never invokes a shell. It also never
installs a missing language server.

## A file does not route

A `routing_error` identifies one of four configuration problems:

- No route matches the file. Add a suffix to `file_extensions` or a
  project-relative glob to `file_patterns`.
- Several servers match the file. Supply the tool's optional `server` argument,
  or make the routes disjoint.
- Several patterns in one server assign different language IDs to the file.
  Make those patterns agree or stop them from overlapping.
- The supplied server name is not configured or has no route for the file.

Patterns take precedence over extensions within one server. Among extension
routes, the longest matching suffix wins.

## A path is rejected

An `invalid_path` error means the input does not resolve to a regular file
inside the immutable project root. Deixis rejects `..` traversal and symlinks
that resolve outside the root. Results from a language server may refer to
external dependencies, but those result locations do not become valid input
paths.

Use a project-relative path or a root-contained absolute path. If every valid
path is rejected, verify the host's working directory or pass `--root`
explicitly.

## A position is rejected

MCP positions are zero-based UTF-8 positions. `line` counts lines, and
`character` counts bytes from the start of that line—not Unicode scalar values
or UTF-16 code units. The offset must land on a character boundary and may not
cross the line ending.

An editor or client that reports UTF-16 columns must convert them before calling
a Deixis tool. Deixis handles conversion from its UTF-8 boundary to the
encoding negotiated with the language server.

## A capability is unsupported

An `unsupported_capability` error means the selected language server did not
advertise the required LSP method, synchronization mode, or position encoding.
Deixis will not send a request and guess from the response. Check
`deixis_server_status`, server initialization logs, and the language server's
own documentation.

For `workspace_symbols`, incapable servers are skipped. The tool returns an
unsupported-capability error only when no configured server supports the
operation.

## Requests time out or the server is busy

`request_timeout` includes time spent waiting for a per-server concurrency
slot. `server_busy` means the bounded outbound queue was already full. Either
condition can reflect a language server that is overloaded, still indexing, or
stuck.

1. Inspect the named server with `deixis_server_status` and check any readiness
   information on an empty result.
2. Reduce concurrent calls before increasing bounds.
3. Increase `request_ms` only when normal server work genuinely takes longer.
4. Keep the MCP host's tool timeout above `startup_ms + request_ms`, because the
   first call may start a language server lazily.

On timeout or MCP cancellation, Deixis removes the pending request and sends
`$/cancelRequest` downstream. A late response is ignored.

## Empty results appear during startup

Some language servers answer before background indexing finishes. Empty hover,
navigation, document-symbol, or current-diagnostic results may include:

- `readiness`, describing the latest progress or server-status signal; and
- `resultStability`, which is `transient`, `stable`, or `indeterminate`.

Retry a `transient` result after the server becomes ready. `indeterminate`
means that the server has not provided enough readiness information; it does
not prove that the empty result is final.

## Diagnostics are stale or unavailable

The `diagnostics` tool synchronizes the file from disk first. It requests pull
diagnostics when supported and otherwise returns the latest cached push report.

- `current` matches the synchronized document version.
- `stale` is an older or versionless push report.
- `unavailable` means no report has arrived.

Save the file, allow the server to finish analysis, and call the tool again.
Deixis acknowledges diagnostic-refresh requests, but it does not yet watch the
filesystem or proactively run diagnostics.

## A server exits or repeatedly restarts

`server_exited` fails the affected in-flight request immediately. The next
operation may start a clean replacement, but the failed request is never
replayed. Once `max_restarts` replacements occur within `window_ms`, Deixis
stops spawning that server until the sliding window advances.

Read the bounded stderr context in the error. If the process was intentionally
stopped or its configuration changed, restart the Deixis MCP session to reset
all language-server generations.

## Shutdown is forced

Deixis sends LSP `shutdown`, then `exit`, and waits up to `shutdown_ms`. It
forcibly terminates a child that does not exit within the bound. Occasional
forced shutdown points to a language-server bug or an unrealistically short
deadline; repeated forced shutdown should be investigated in the child logs.

## Structured error codes

| Code | Boundary |
| --- | --- |
| `invalid_path` | The input file is missing, not regular, or outside the root. |
| `invalid_position` | The UTF-8 position is outside the document or not a boundary. |
| `routing_error` | No unique configured route selected a server. |
| `unknown_server` | The status probe named an unconfigured server. |
| `no_server_configured` | The status probe has no server to inspect. |
| `unsupported_capability` | The server did not negotiate a required LSP feature. |
| `request_timeout` | An LSP startup or request deadline expired. |
| `server_busy` | The bounded outbound queue was full. |
| `server_start_failed` | The child could not be spawned or connected to pipes. |
| `server_exited` | The transport closed, the child exited, or restart protection engaged. |
| `lsp_error` | The server returned a JSON-RPC error; inspect `lspError`. |
| `lsp_protocol_error` | The response was malformed, invalid, or too large. |
| `document_error` | Reading or synchronizing the document failed. |
| `request_canceled` | The MCP client canceled the operation. |
| `server_error` | Another lifecycle or shutdown error occurred. |

Malformed tool arguments are MCP `invalid_params` protocol errors rather than
tool errors because execution has not begun.
