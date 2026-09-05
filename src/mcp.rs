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
use serde_json::{Value as JsonValue, json};
use tracing::info;

use crate::{
    config::Config,
    lsp::{LazyLanguageServer, ServerSnapshot},
    project::{Project, StartupState},
};

const SERVER_STATUS_TOOL: &str = "deixis_server_status";

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
            vec![server_status_tool()]
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
        if request.name.as_ref() != SERVER_STATUS_TOOL {
            return Err(McpError::method_not_found::<CallToolRequestMethod>());
        }

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

fn status_json(status: &ServerSnapshot) -> JsonValue {
    json!({
        "configuredName": status.configured_name(),
        "started": status.started(),
        "serverName": status.server_name(),
        "serverVersion": status.server_version(),
        "positionEncoding": status.position_encoding(),
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
