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
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use tracing::info;

use crate::{
    config::Config,
    lsp::{LazyLanguageServer, ServerSnapshot},
    positions::Position,
    project::{Project, StartupState},
};

const SERVER_STATUS_TOOL: &str = "deixis_server_status";
const HOVER_TOOL: &str = "hover";

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
            vec![server_status_tool(), hover_tool()]
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
        let language_server = match arguments.server.as_deref() {
            Some(name) => self.language_servers.get(name),
            None => self.language_servers.values().next(),
        };
        let Some(language_server) = language_server else {
            let message = arguments.server.map_or_else(
                || "no language server is configured".to_owned(),
                |name| format!("server `{name}` is not configured"),
            );
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                message,
            )])
            .into());
        };

        let status = if arguments.start {
            match language_server.ensure_started().await {
                Ok(status) => status,
                Err(error) => {
                    return Ok(CallToolResult::error(vec![
                        ContentBlock::text(error.to_string()),
                    ])
                    .into());
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
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "no language server is configured",
            )])
            .into());
        };
        let file = match self.project().resolve_file(&arguments.path) {
            Ok(file) => file,
            Err(error) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    error.to_string(),
                )])
                .into());
            }
        };
        let route =
            match config.route(file.relative(), arguments.server.as_deref()) {
                Ok(route) => route,
                Err(error) => {
                    return Ok(CallToolResult::error(vec![
                        ContentBlock::text(error.to_string()),
                    ])
                    .into());
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
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    error.to_string(),
                )])
                .into());
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
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HoverArguments {
    path: String,
    #[serde(default)]
    server: Option<String>,
    position: Position,
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
    .with_raw_output_schema(Arc::new(object_schema(json!({
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
    }))))
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
