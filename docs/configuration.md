# Configuration Reference

Deixis reads one strict TOML document containing named language-server
definitions. It rejects unknown fields and invalid values before starting the
MCP transport. Commands come only from this user-owned configuration; project
files never authorize executable discovery or downloads.

For a ready-to-copy polyglot configuration, see
[`examples/config.toml`](../examples/config.toml).

## Loading configuration

The command-line interface has three options:

```console
deixis [--root <project>] [--config <config.toml>] [--allow-mutation]
```

- `--root` selects the immutable project root. It defaults to the process's
  current directory.
- `--config` selects a configuration file and takes precedence over the user
  configuration.
- `--allow-mutation` advertises and enables mutating MCP tools. Without it,
  Deixis remains query-only; `apply_rename` is neither advertised nor callable.

Both paths may be relative to the current directory. They must exist and are
canonicalized before MCP serving begins. If `--config` is absent, Deixis looks
for `$XDG_CONFIG_HOME/deixis/config.toml`, then uses the platform path below
when `XDG_CONFIG_HOME` is unset:

| Platform | User configuration |
| --- | --- |
| Linux and other Unix | `~/.config/deixis/config.toml` |
| macOS | `~/Library/Application Support/deixis/config.toml` |
| Windows | `%APPDATA%\deixis\config.toml` |

If no configuration exists, Deixis starts a capability-free MCP server and
advertises no tools. It never searches the project tree for a configuration.

## Complete schema

The document root has one field:

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `servers` | table of server names | yes | At least one named server definition. |

Each `[servers.<name>]` table accepts these fields:

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | string | required | Nonempty executable path or name. |
| `args` | array of strings | `[]` | Arguments passed directly to the executable. |
| `environment` | table of strings | `{}` | Variables added to the inherited process environment. |
| `file_extensions` | table of strings | `{}` | File-name suffix to LSP language-ID routes. |
| `file_patterns` | table of strings | `{}` | Project-relative glob to LSP language-ID routes. |
| `initialization_options` | TOML value | `{}` | JSON-compatible value sent in the LSP `initialize` request. |
| `timeouts` | table | defaults below | Startup, request, and shutdown deadlines. |
| `limits` | table | defaults below | Queue, concurrency, and response-size bounds. |
| `restart` | table | defaults below | Crash-loop protection. |

`command` is required, and each server must define `file_extensions`,
`file_patterns`, or both. A server name, command, route, or language ID may not
be empty. Environment keys may not be empty or contain `=`.

### Routes

Extension keys must begin with `.`, contain at least one other character, and
contain no slash or backslash. They are file-name suffixes rather than only the
last extension, so a route such as `".d.ts"` is valid.

```toml
[servers.typescript]
command = "typescript-language-server"
args = ["--stdio"]

[servers.typescript.file_extensions]
".js" = "javascript"
".d.ts" = "typescript"
".ts" = "typescript"
```

Pattern keys use `globset` syntax and match project-relative paths:

```toml
[servers.cmake.file_patterns]
"**/CMakeLists.txt" = "cmake"
"cmake/**/*.cmake" = "cmake"
```

Within one server, matching patterns take precedence over extensions, and the
longest matching extension wins. Patterns that assign different language IDs
to the same path are ambiguous. If routes in several servers match a file, the
tool call must supply its optional `server` argument.

### Initialization options

Initialization options retain their nested TOML shape and are translated to
JSON:

```toml
[servers.rust.initialization_options]
checkOnSave = true

[servers.rust.initialization_options.cargo]
allFeatures = true
features = ["serde", "tokio"]
```

Strings, integers, finite floating-point numbers, booleans, arrays, and tables
are accepted. TOML date and time values are rejected because they have no
direct JSON representation.

### Environment and command execution

Commands run directly without a shell. Shell interpolation, pipelines, aliases,
and shell functions therefore do not apply. Use `args` for arguments and
`environment` for child-only overrides:

```toml
[servers.rust]
command = "/absolute/path/to/rust-analyzer"
args = []
file_extensions = { ".rs" = "rust" }

[servers.rust.environment]
RA_LOG = "rust_analyzer=info"
```

The child inherits the Deixis process environment in addition to these
overrides. No configuration operation installs a command or accesses the
network.

## Timeouts

Every timeout is a positive integer in milliseconds and applies independently
to each server:

```toml
[servers.rust.timeouts]
startup_ms = 30000
request_ms = 30000
shutdown_ms = 5000
```

| Field | Default | Scope |
| --- | ---: | --- |
| `startup_ms` | 30,000 | Wait for the LSP `initialize` response. |
| `request_ms` | 30,000 | Wait for a concurrency slot and the LSP response. |
| `shutdown_ms` | 5,000 | Complete graceful shutdown before forced termination. |

A language server starts lazily, so the first MCP tool call may include both
the startup and request intervals. Configure the MCP host's tool timeout to
exceed their sum. The host's MCP startup timeout covers only the Deixis process
and MCP handshake.

Timeout cancellation sends `$/cancelRequest` for an in-flight LSP request. A
shutdown timeout still permits the protocol's `shutdown` and `exit` sequence to
run as far as possible before the child is forcibly terminated.

## Resource limits

Every limit is a positive integer and applies independently to each server:

```toml
[servers.rust.limits]
outbound_queue_capacity = 64
max_concurrent_requests = 16
max_response_bytes = 16777216
```

| Field | Default | Behavior at the limit |
| --- | ---: | --- |
| `outbound_queue_capacity` | 64 | A full queue fails the operation instead of growing memory use. |
| `max_concurrent_requests` | 16 | Further requests wait for a slot within `request_ms`. |
| `max_response_bytes` | 16 MiB | A larger LSP body is a protocol error and stops that transport. |

The queue and concurrency values may not exceed Tokio's semaphore permit
limit.

## Restart policy

An unexpected server exit fails requests already in flight; Deixis never
replays a request whose outcome is uncertain. A later operation retires the
failed generation and may start a replacement:

```toml
[servers.rust.restart]
max_restarts = 3
window_ms = 60000
```

| Field | Default | Meaning |
| --- | ---: | --- |
| `max_restarts` | 3 | Replacement processes allowed in the sliding window. |
| `window_ms` | 60,000 | Sliding window used to detect a crash loop. |

Both values must be greater than zero. When the budget is exhausted, operations
fail without spawning another child until an attempt ages out of the window.
Each replacement starts with clean protocol state and resynchronizes documents
from disk on demand.

## Tested server recipes

The repository's compatibility suite exercises these configurations:

```toml
[servers.rust]
command = "rust-analyzer"
file_extensions = { ".rs" = "rust" }

[servers.typescript]
command = "typescript-language-server"
args = ["--stdio"]
file_extensions = { ".ts" = "typescript" }

[servers.pyright]
command = "pyright-langserver"
args = ["--stdio"]
file_extensions = { ".py" = "python" }

[servers.gopls]
command = "gopls"
file_extensions = { ".go" = "go" }

[servers.clangd]
command = "clangd"
file_extensions = { ".c" = "c", ".cc" = "cpp" }
```

Deno requires a subcommand and initialization option:

```toml
[servers.deno]
command = "deno"
args = ["lsp"]
file_extensions = { ".ts" = "typescript" }

[servers.deno.initialization_options]
enable = true
```

Do not configure both TypeScript Language Server and Deno for the same suffix
unless callers will select a `server` explicitly or patterns make the routes
disjoint.
