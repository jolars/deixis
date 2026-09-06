use std::{collections::BTreeMap, error::Error, sync::Arc};

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestMethod, CallToolRequestParams, CallToolResponse,
        CallToolResult, ContentBlock, Implementation, JsonObject,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities,
        ServerInfo, Tool, ToolAnnotations,
    },
    service::RequestContext,
    transport::stdio,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tracing::info;

use crate::{
    config::{Config, ConfigRouteError},
    lsp::{LazyLanguageServer, LspError, ServerSnapshot},
    positions::Position,
    project::{Project, ProjectPathError, StartupState},
};

const SERVER_STATUS_TOOL: &str = "deixis_server_status";
const HOVER_TOOL: &str = "hover";
const DEFINITION_TOOL: &str = "definition";

#[derive(Clone)]
pub struct DeixisServer {
    startup: StartupState,
    language_servers: BTreeMap<String, Arc<LazyLanguageServer>>,
}

impl DeixisServer {
    pub fn new(startup: StartupState) -> Self {
        let language_servers = startup
            .config()
            .map(|config| {
                config
                    .servers()
                    .iter()
                    .cloned()
                    .map(|config| {
                        let name = config.name().to_owned();
                        let server = Arc::new(LazyLanguageServer::new(
                            config,
                            startup.project().clone(),
                        ));
                        (name, server)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            startup,
            language_servers,
        }
    }

    pub fn project(&self) -> &Project {
        self.startup.project()
    }

    pub fn config(&self) -> Option<&Config> {
        self.startup.config()
    }

    pub async fn shutdown_language_servers(
        &self,
    ) -> Result<(), Box<dyn Error>> {
        let mut first_error = None;
        for language_server in self.language_servers.values() {
            if let Err(error) = language_server.shutdown().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            return Err(Box::new(error));
        }
        Ok(())
    }
}

impl ServerHandler for DeixisServer {
    fn get_info(&self) -> ServerInfo {
        let _project = self.project();
        let capabilities = if self.language_servers.is_empty() {
            ServerCapabilities::default()
        } else {
            ServerCapabilities::builder().enable_tools().build()
        };

        ServerInfo::new(capabilities).with_server_info(Implementation::new(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = if self.language_servers.is_empty() {
            Vec::new()
        } else {
            vec![server_status_tool(), hover_tool(), definition_tool()]
        };
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        match request.name.as_ref() {
            SERVER_STATUS_TOOL => self.call_server_status(&request).await,
            HOVER_TOOL => self.call_hover(request).await,
            DEFINITION_TOOL => self.call_definition(request).await,
            _ => Err(McpError::method_not_found::<CallToolRequestMethod>()),
        }
    }
}

impl DeixisServer {
    async fn call_server_status(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<CallToolResponse, McpError> {
        let arguments = serde_json::from_value::<ServerStatusArguments>(
            JsonValue::Object(request.arguments.clone().unwrap_or_default()),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid server status arguments: {error}"),
                None,
            )
        })?;
        arguments.validate()?;
        let language_server_name = arguments.server.as_deref().or_else(|| {
            self.language_servers.keys().next().map(String::as_str)
        });
        let language_server = language_server_name
            .and_then(|name| self.language_servers.get(name));
        let Some(language_server) = language_server else {
            let error = arguments.server.as_deref().map_or_else(
                || {
                    ToolError::new(
                        "no_server_configured",
                        SERVER_STATUS_TOOL,
                        "no language server is configured",
                    )
                },
                |name| {
                    ToolError::new(
                        "unknown_server",
                        SERVER_STATUS_TOOL,
                        format!("server `{name}` is not configured"),
                    )
                    .with_server(name)
                },
            );
            return Ok(error_result(error));
        };

        let status = if arguments.start {
            match language_server.ensure_started().await {
                Ok(status) => status,
                Err(error) => {
                    return Ok(error_result(ToolError::from_lsp(
                        ToolContext {
                            tool: SERVER_STATUS_TOOL,
                            server: language_server_name,
                            method: Some("initialize"),
                            path: None,
                        },
                        &error,
                    )));
                }
            }
        } else {
            language_server.status().await
        };

        Ok(CallToolResult::structured(status_json(&status)).into())
    }

    async fn call_hover(
        &self,
        request: CallToolRequestParams,
    ) -> Result<CallToolResponse, McpError> {
        let arguments = request.arguments.unwrap_or_default();
        let arguments = serde_json::from_value::<HoverArguments>(
            JsonValue::Object(arguments),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid hover arguments: {error}"),
                None,
            )
        })?;
        arguments.validate()?;
        let Some(config) = self.config() else {
            return Ok(error_result(
                ToolError::new(
                    "no_server_configured",
                    HOVER_TOOL,
                    "no language server is configured",
                )
                .with_method("textDocument/hover")
                .with_path(&arguments.path),
            ));
        };
        let file = match self.project().resolve_file(&arguments.path) {
            Ok(file) => file,
            Err(error) => {
                return Ok(error_result(ToolError::from_path(
                    HOVER_TOOL,
                    "textDocument/hover",
                    &arguments.path,
                    arguments.server.as_deref(),
                    &error,
                )));
            }
        };
        let route =
            match config.route(file.relative(), arguments.server.as_deref()) {
                Ok(route) => route,
                Err(error) => {
                    return Ok(error_result(ToolError::from_route(
                        HOVER_TOOL,
                        "textDocument/hover",
                        &arguments.path,
                        arguments.server.as_deref(),
                        &error,
                    )));
                }
            };
        let language_server = self
            .language_servers
            .get(route.server().name())
            .expect("every validated server should have a lifecycle manager");

        let hover = match language_server
            .hover(file.absolute(), route.language_id(), arguments.position)
            .await
        {
            Ok(hover) => hover,
            Err(error) => {
                return Ok(error_result(ToolError::from_lsp(
                    ToolContext {
                        tool: HOVER_TOOL,
                        server: Some(route.server().name()),
                        method: Some("textDocument/hover"),
                        path: Some(&arguments.path),
                    },
                    &error,
                )));
            }
        };

        let (structured, text) = match hover {
            Some(hover) => {
                let text = hover.text();
                let text = if text.is_empty() {
                    "Hover information is empty.".to_owned()
                } else {
                    text
                };
                let structured =
                    serde_json::to_value(hover).map_err(|error| {
                        McpError::internal_error(
                            format!("failed to encode hover response: {error}"),
                            None,
                        )
                    })?;
                (structured, text)
            }
            None => (
                json!({ "contents": null }),
                "No hover information.".to_owned(),
            ),
        };
        let mut result = CallToolResult::structured(structured);
        result.content = vec![ContentBlock::text(text)];

        Ok(result.into())
    }

    async fn call_definition(
        &self,
        request: CallToolRequestParams,
    ) -> Result<CallToolResponse, McpError> {
        let arguments = request.arguments.unwrap_or_default();
        let arguments = serde_json::from_value::<DefinitionArguments>(
            JsonValue::Object(arguments),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid definition arguments: {error}"),
                None,
            )
        })?;
        arguments.validate()?;
        let Some(config) = self.config() else {
            return Ok(error_result(
                ToolError::new(
                    "no_server_configured",
                    DEFINITION_TOOL,
                    "no language server is configured",
                )
                .with_method("textDocument/definition")
                .with_path(&arguments.path),
            ));
        };
        let file = match self.project().resolve_file(&arguments.path) {
            Ok(file) => file,
            Err(error) => {
                return Ok(error_result(ToolError::from_path(
                    DEFINITION_TOOL,
                    "textDocument/definition",
                    &arguments.path,
                    arguments.server.as_deref(),
                    &error,
                )));
            }
        };
        let route =
            match config.route(file.relative(), arguments.server.as_deref()) {
                Ok(route) => route,
                Err(error) => {
                    return Ok(error_result(ToolError::from_route(
                        DEFINITION_TOOL,
                        "textDocument/definition",
                        &arguments.path,
                        arguments.server.as_deref(),
                        &error,
                    )));
                }
            };
        let language_server = self
            .language_servers
            .get(route.server().name())
            .expect("every validated server should have a lifecycle manager");

        let definitions = match language_server
            .definition(
                file.absolute(),
                route.language_id(),
                arguments.position,
            )
            .await
        {
            Ok(definitions) => definitions,
            Err(error) => {
                return Ok(error_result(ToolError::from_lsp(
                    ToolContext {
                        tool: DEFINITION_TOOL,
                        server: Some(route.server().name()),
                        method: Some("textDocument/definition"),
                        path: Some(&arguments.path),
                    },
                    &error,
                )));
            }
        };
        let text = if definitions.is_empty() {
            "No definitions.".to_owned()
        } else {
            definitions
                .iter()
                .map(|definition| definition.text())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let locations = serde_json::to_value(definitions).map_err(|error| {
            McpError::internal_error(
                format!("failed to encode definition response: {error}"),
                None,
            )
        })?;
        let structured = json!({ "locations": locations });
        let mut result = CallToolResult::structured(structured);
        result.content = vec![ContentBlock::text(text)];

        Ok(result.into())
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolError {
    code: &'static str,
    message: String,
    tool: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lsp_error: Option<LspResponseError>,
}

#[derive(Debug, Serialize)]
struct LspResponseError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<JsonValue>,
}

#[derive(Debug, Clone, Copy)]
struct ToolContext<'a> {
    tool: &'static str,
    server: Option<&'a str>,
    method: Option<&'a str>,
    path: Option<&'a str>,
}

impl ToolError {
    fn new(
        code: &'static str,
        tool: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            tool,
            server: None,
            method: None,
            path: None,
            timeout_ms: None,
            lsp_error: None,
        }
    }

    fn with_server(mut self, server: &str) -> Self {
        self.server = Some(server.to_owned());
        self
    }

    fn with_method(mut self, method: &str) -> Self {
        self.method = Some(method.to_owned());
        self
    }

    fn with_path(mut self, path: &str) -> Self {
        self.path = Some(path.to_owned());
        self
    }

    fn from_path(
        tool: &'static str,
        method: &'static str,
        path: &str,
        server: Option<&str>,
        error: &ProjectPathError,
    ) -> Self {
        let mut result = Self::new("invalid_path", tool, error.to_string())
            .with_method(method)
            .with_path(path);
        if let Some(server) = server {
            result = result.with_server(server);
        }
        result
    }

    fn from_route(
        tool: &'static str,
        method: &'static str,
        path: &str,
        server: Option<&str>,
        error: &ConfigRouteError,
    ) -> Self {
        let mut result = Self::new("routing_error", tool, error.to_string())
            .with_method(method)
            .with_path(path);
        if let Some(server) = server {
            result = result.with_server(server);
        }
        result
    }

    fn from_lsp(context: ToolContext<'_>, error: &LspError) -> Self {
        let mut result =
            Self::new(lsp_error_code(error), context.tool, error.to_string());
        if let Some(server) = context.server {
            result = result.with_server(server);
        }
        if let Some(method) = context.method {
            result = result.with_method(method);
        }
        if let Some(path) = context.path {
            result = result.with_path(path);
        }

        match error {
            LspError::Spawn { server, .. }
            | LspError::MissingPipe { server, .. }
            | LspError::UnsupportedDocumentSynchronization { server, .. }
            | LspError::UnsupportedPositionEncoding { server, .. }
            | LspError::PositionConversion { server, .. }
            | LspError::DocumentSynchronizationClosed { server, .. }
            | LspError::DocumentLanguageChanged { server, .. }
            | LspError::DocumentVersionOverflow { server, .. }
            | LspError::TransportClosed { server }
            | LspError::Shutdown { server, .. } => {
                result.server = Some(server.clone());
            }
            LspError::UnsupportedCapability { server, method } => {
                result.server = Some(server.clone());
                result.method = Some((*method).to_owned());
            }
            LspError::ServerExited { server, method }
            | LspError::RequestCanceled { server, method } => {
                result.server = Some(server.clone());
                result.method = Some(method.clone());
            }
            LspError::RequestTimeout {
                server,
                method,
                timeout,
            } => {
                result.server = Some(server.clone());
                result.method = Some(method.clone());
                result.timeout_ms = Some(
                    u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                );
            }
            LspError::ResponseError {
                server,
                method,
                code,
                message,
                data,
            } => {
                result.server = Some(server.clone());
                result.method = Some(method.clone());
                result.lsp_error = Some(LspResponseError {
                    code: *code,
                    message: message.clone(),
                    data: data.clone(),
                });
            }
            LspError::EncodeMessage(_)
            | LspError::DecodeResult(_)
            | LspError::DocumentPath(_)
            | LspError::ReadDocument { .. } => {}
        }

        result
    }
}

fn lsp_error_code(error: &LspError) -> &'static str {
    match error {
        LspError::DocumentPath(_) => "invalid_path",
        LspError::PositionConversion { .. } => "invalid_position",
        LspError::UnsupportedDocumentSynchronization { .. }
        | LspError::UnsupportedPositionEncoding { .. }
        | LspError::UnsupportedCapability { .. } => "unsupported_capability",
        LspError::RequestTimeout { .. } => "request_timeout",
        LspError::TransportClosed { .. } | LspError::ServerExited { .. } => {
            "server_exited"
        }
        LspError::ResponseError { .. } => "lsp_error",
        LspError::Spawn { .. } | LspError::MissingPipe { .. } => {
            "server_start_failed"
        }
        LspError::EncodeMessage(_) | LspError::DecodeResult(_) => {
            "lsp_protocol_error"
        }
        LspError::ReadDocument { .. }
        | LspError::DocumentSynchronizationClosed { .. }
        | LspError::DocumentLanguageChanged { .. }
        | LspError::DocumentVersionOverflow { .. } => "document_error",
        LspError::RequestCanceled { .. } => "request_canceled",
        LspError::Shutdown { .. } => "server_error",
    }
}

fn error_result(error: ToolError) -> CallToolResponse {
    let message = error.message.clone();
    let mut result = CallToolResult::structured_error(json!({
        "error": error,
    }));
    result.content = vec![ContentBlock::text(message)];
    result.into()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HoverArguments {
    path: String,
    #[serde(default)]
    server: Option<String>,
    position: Position,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DefinitionArguments {
    path: String,
    #[serde(default)]
    server: Option<String>,
    position: Position,
}

impl DefinitionArguments {
    fn validate(&self) -> Result<(), McpError> {
        if self.path.is_empty() {
            return Err(McpError::invalid_params(
                "invalid definition arguments: `path` must not be empty",
                None,
            ));
        }
        if self
            .server
            .as_ref()
            .is_some_and(|server| server.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                "invalid definition arguments: `server` must not be empty",
                None,
            ));
        }
        Ok(())
    }
}

impl HoverArguments {
    fn validate(&self) -> Result<(), McpError> {
        if self.path.is_empty() {
            return Err(McpError::invalid_params(
                "invalid hover arguments: `path` must not be empty",
                None,
            ));
        }
        if self
            .server
            .as_ref()
            .is_some_and(|server| server.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                "invalid hover arguments: `server` must not be empty",
                None,
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ServerStatusArguments {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    start: bool,
}

impl ServerStatusArguments {
    fn validate(&self) -> Result<(), McpError> {
        if self
            .server
            .as_ref()
            .is_some_and(|server| server.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                "invalid server status arguments: `server` must not be empty",
                None,
            ));
        }
        Ok(())
    }
}

pub async fn serve_stdio(startup: StartupState) -> Result<(), Box<dyn Error>> {
    let server = DeixisServer::new(startup);
    let cleanup = server.clone();
    let service = server.serve(stdio()).await?;
    let reason = service.waiting().await?;
    info!(?reason, "stopping deixis");
    cleanup.shutdown_language_servers().await?;

    Ok(())
}

fn server_status_tool() -> Tool {
    Tool::new(
        SERVER_STATUS_TOOL,
        "Return status for a configured language server, optionally starting it.",
        object_schema(json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Configured server name. Defaults to the first name in stable lexical order."
                },
                "start": {
                    "type": "boolean",
                    "description": "Start the configured language server before returning status."
                }
            },
            "additionalProperties": false
        })),
    )
    .with_raw_output_schema(result_output_schema(status_output_schema()))
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(false)
            .open_world(false),
    )
}

fn hover_tool() -> Tool {
    Tool::new(
        HOVER_TOOL,
        "Return hover information for a UTF-8 position in a project file.",
        object_schema(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Project-relative or root-contained absolute file path."
                },
                "server": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Configured server name used to resolve an otherwise ambiguous route."
                },
                "position": position_schema(),
            },
            "required": ["path", "position"],
            "additionalProperties": false
        })),
    )
    .with_raw_output_schema(result_output_schema(json!({
        "type": "object",
        "properties": {
            "contents": {
                "description": "Structured hover markup, legacy marked strings, or null when no hover is available.",
                "anyOf": [
                    {
                        "type": "object",
                        "properties": {
                            "kind": { "enum": ["plaintext", "markdown"] },
                            "value": { "type": "string" }
                        },
                        "required": ["kind", "value"],
                        "additionalProperties": false
                    },
                    { "type": "string" },
                    {
                        "type": "object",
                        "properties": {
                            "language": { "type": "string" },
                            "value": { "type": "string" }
                        },
                        "required": ["language", "value"],
                        "additionalProperties": false
                    },
                    {
                        "type": "array",
                        "items": {
                            "anyOf": [
                                { "type": "string" },
                                {
                                    "type": "object",
                                    "properties": {
                                        "language": { "type": "string" },
                                        "value": { "type": "string" }
                                    },
                                    "required": ["language", "value"],
                                    "additionalProperties": false
                                }
                            ]
                        }
                    },
                    { "type": "null" }
                ]
            },
            "range": {
                "type": "object",
                "properties": {
                    "start": position_schema(),
                    "end": position_schema()
                },
                "required": ["start", "end"],
                "additionalProperties": false
            }
        },
        "required": ["contents"],
        "additionalProperties": false
    })))
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    )
}

fn definition_tool() -> Tool {
    Tool::new(
        DEFINITION_TOOL,
        "Return definitions for a UTF-8 position in a project file.",
        object_schema(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Project-relative or root-contained absolute file path."
                },
                "server": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Configured server name used to resolve an otherwise ambiguous route."
                },
                "position": position_schema(),
            },
            "required": ["path", "position"],
            "additionalProperties": false
        })),
    )
    .with_raw_output_schema(result_output_schema(json!({
        "type": "object",
        "properties": {
            "locations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "server": {
                            "type": "string",
                            "description": "Configured name of the language server that returned this definition."
                        },
                        "uri": { "type": "string" },
                        "targetRange": range_schema(),
                        "targetSelectionRange": range_schema(),
                        "targetPositionEncoding": {
                            "enum": ["utf-8", "utf-16", "utf-32"],
                            "description": "Encoding used by character offsets in the target ranges. UTF-8 is used whenever the target is a readable project file."
                        },
                        "originSelectionRange": range_schema(),
                    },
                    "required": [
                        "server",
                        "uri",
                        "targetRange",
                        "targetSelectionRange",
                        "targetPositionEncoding"
                    ],
                    "additionalProperties": false
                }
            }
        },
        "required": ["locations"],
        "additionalProperties": false
    })))
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    )
}

fn position_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "line": {
                "type": "integer",
                "minimum": 0,
                "maximum": u32::MAX
            },
            "character": {
                "type": "integer",
                "minimum": 0,
                "maximum": u32::MAX
            }
        },
        "required": ["line", "character"],
        "additionalProperties": false
    })
}

fn range_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "start": position_schema(),
            "end": position_schema()
        },
        "required": ["start", "end"],
        "additionalProperties": false
    })
}

fn status_output_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "configuredName": { "type": "string" },
            "started": { "type": "boolean" },
            "serverName": { "type": ["string", "null"] },
            "serverVersion": { "type": ["string", "null"] },
            "positionEncoding": {
                "enum": ["utf-8", "utf-16", "utf-32", null]
            },
            "textDocumentSync": {},
            "capabilities": { "type": "object" }
        },
        "required": [
            "configuredName",
            "started",
            "serverName",
            "serverVersion",
            "positionEncoding",
            "textDocumentSync",
            "capabilities"
        ],
        "additionalProperties": false
    })
}

fn result_output_schema(success: JsonValue) -> Arc<JsonObject> {
    Arc::new(object_schema(json!({
        "oneOf": [success, error_output_schema()]
    })))
}

fn error_output_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "error": {
                "type": "object",
                "properties": {
                    "code": {
                        "enum": [
                            "invalid_path",
                            "invalid_position",
                            "unsupported_capability",
                            "request_timeout",
                            "server_exited",
                            "lsp_error",
                            "server_start_failed",
                            "lsp_protocol_error",
                            "document_error",
                            "request_canceled",
                            "server_error",
                            "routing_error",
                            "no_server_configured",
                            "unknown_server"
                        ]
                    },
                    "message": { "type": "string" },
                    "tool": { "type": "string" },
                    "server": { "type": "string" },
                    "method": { "type": "string" },
                    "path": { "type": "string" },
                    "timeoutMs": {
                        "type": "integer",
                        "minimum": 0
                    },
                    "lspError": {
                        "type": "object",
                        "properties": {
                            "code": { "type": "integer" },
                            "message": { "type": "string" },
                            "data": {}
                        },
                        "required": ["code", "message"],
                        "additionalProperties": false
                    }
                },
                "required": ["code", "message", "tool"],
                "additionalProperties": false
            }
        },
        "required": ["error"],
        "additionalProperties": false
    })
}

fn status_json(status: &ServerSnapshot) -> JsonValue {
    json!({
        "configuredName": status.configured_name(),
        "started": status.started(),
        "serverName": status.server_name(),
        "serverVersion": status.server_version(),
        "positionEncoding": status
            .position_encoding()
            .map(|encoding| encoding.as_str()),
        "textDocumentSync": status.text_document_sync(),
        "capabilities": status.capabilities(),
    })
}

fn object_schema(schema: JsonValue) -> JsonObject {
    schema
        .as_object()
        .expect("tool input schema must be a JSON object")
        .clone()
}
