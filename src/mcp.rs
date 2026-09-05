use std::{error::Error, sync::Arc};

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
    language_server: Option<Arc<LazyLanguageServer>>,
}

impl DeixisServer {
    pub fn new(startup: StartupState) -> Self {
        let language_server = startup
            .config()
            .and_then(|config| config.servers().first())
            .cloned()
            .map(|config| {
                Arc::new(LazyLanguageServer::new(
                    config,
                    startup.project().clone(),
                ))
            });
        Self {
            startup,
            language_server,
        }
    }

    pub fn project(&self) -> &Project {
        self.startup.project()
    }

    pub fn config(&self) -> Option<&Config> {
        self.startup.config()
    }

    pub async fn shutdown_language_server(&self) -> Result<(), Box<dyn Error>> {
        if let Some(language_server) = &self.language_server {
            language_server.shutdown().await?;
        }
        Ok(())
    }
}

impl ServerHandler for DeixisServer {
    fn get_info(&self) -> ServerInfo {
        let _project = self.project();
        let capabilities = if self.language_server.is_some() {
            ServerCapabilities::builder().enable_tools().build()
        } else {
            ServerCapabilities::default()
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
        let tools = if self.language_server.is_some() {
            vec![server_status_tool(), hover_tool()]
        } else {
            Vec::new()
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
        let Some(language_server) = &self.language_server else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "no language server is configured",
            )])
            .into());
        };

        let start = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("start"))
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let status = if start {
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
        let Some(language_server) = &self.language_server else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "no language server is configured",
            )])
            .into());
        };

        let hover = match language_server
            .hover(&arguments.path, &arguments.language_id, arguments.position)
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
    language_id: String,
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
        if self.language_id.is_empty() {
            return Err(McpError::invalid_params(
                "invalid hover arguments: `languageId` must not be empty",
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
    cleanup.shutdown_language_server().await?;

    Ok(())
}

fn server_status_tool() -> Tool {
    Tool::new(
        SERVER_STATUS_TOOL,
        "Return status for the first configured language server, optionally starting it.",
        object_schema(json!({
            "type": "object",
            "properties": {
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
                "languageId": {
                    "type": "string",
                    "minLength": 1,
                    "description": "LSP language identifier used to synchronize the document."
                },
                "position": position_schema(),
            },
            "required": ["path", "languageId", "position"],
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
