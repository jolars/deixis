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
use tokio::{
    sync::{Mutex, RwLock},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

mod output;

use crate::{
    config::{Config, ConfigRouteError},
    lsp::{
        DiagnosticAvailability, LazyLanguageServer, LspError,
        ReadinessSnapshot, ReadinessSource, ReadinessState, ResultStability,
        ServerSnapshot,
    },
    positions::Position,
    project::{Project, ProjectPathError, StartupState},
    workspace_edits::{
        PreviewIdError, PreviewStore, WorkspaceEditError, apply_rename_preview,
    },
};

const SERVER_STATUS_TOOL: &str = "deixis_server_status";
const HOVER_TOOL: &str = "hover";
const SIGNATURE_HELP_TOOL: &str = "signature_help";
const DECLARATION_TOOL: &str = "declaration";
const DEFINITION_TOOL: &str = "definition";
const TYPE_DEFINITION_TOOL: &str = "type_definition";
const IMPLEMENTATION_TOOL: &str = "implementation";
const REFERENCES_TOOL: &str = "references";
const DIAGNOSTICS_TOOL: &str = "diagnostics";
const DOCUMENT_SYMBOLS_TOOL: &str = "document_symbols";
const WORKSPACE_SYMBOLS_TOOL: &str = "workspace_symbols";
const PREPARE_RENAME_TOOL: &str = "prepare_rename";
const PREVIEW_RENAME_TOOL: &str = "preview_rename";
const APPLY_RENAME_TOOL: &str = "apply_rename";

#[derive(Clone, Copy)]
enum LocationOperation {
    Declaration,
    Definition,
    TypeDefinition,
    Implementation,
}

#[derive(Clone, Copy)]
struct LocationToolSpec {
    operation: LocationOperation,
    tool: &'static str,
    method: &'static str,
    plural: &'static str,
}

const DECLARATION_SPEC: LocationToolSpec = LocationToolSpec {
    operation: LocationOperation::Declaration,
    tool: DECLARATION_TOOL,
    method: "textDocument/declaration",
    plural: "declarations",
};
const DEFINITION_SPEC: LocationToolSpec = LocationToolSpec {
    operation: LocationOperation::Definition,
    tool: DEFINITION_TOOL,
    method: "textDocument/definition",
    plural: "definitions",
};
const TYPE_DEFINITION_SPEC: LocationToolSpec = LocationToolSpec {
    operation: LocationOperation::TypeDefinition,
    tool: TYPE_DEFINITION_TOOL,
    method: "textDocument/typeDefinition",
    plural: "type definitions",
};
const IMPLEMENTATION_SPEC: LocationToolSpec = LocationToolSpec {
    operation: LocationOperation::Implementation,
    tool: IMPLEMENTATION_TOOL,
    method: "textDocument/implementation",
    plural: "implementations",
};

#[derive(Clone)]
pub struct DeixisServer {
    startup: StartupState,
    language_servers: BTreeMap<String, Arc<LazyLanguageServer>>,
    previews: Arc<Mutex<PreviewStore>>,
    workspace_gate: Arc<RwLock<()>>,
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
            previews: Arc::new(Mutex::new(PreviewStore::default())),
            workspace_gate: Arc::new(RwLock::new(())),
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

        let info = ServerInfo::new(capabilities).with_server_info(
            Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ),
        );
        if self.language_servers.is_empty() {
            info
        } else {
            info.with_instructions(concat!(
                "Use definition, type_definition, implementation, and references ",
                "for targeted symbol navigation. Use document_symbols only when ",
                "a file outline is needed; it is not a default navigation step ",
                "or a prerequisite for other queries."
            ))
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = if self.language_servers.is_empty() {
            Vec::new()
        } else {
            let mut tools = vec![
                server_status_tool(),
                hover_tool(),
                signature_help_tool(),
                location_tool(DECLARATION_SPEC),
                location_tool(DEFINITION_SPEC),
                location_tool(TYPE_DEFINITION_SPEC),
                location_tool(IMPLEMENTATION_SPEC),
                references_tool(),
                diagnostics_tool(),
                document_symbols_tool(),
                workspace_symbols_tool(),
                prepare_rename_tool(),
                preview_rename_tool(),
            ];
            if self.startup.allow_mutation() {
                tools.push(apply_rename_tool());
            }
            tools
        };
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        match request.name.as_ref() {
            SERVER_STATUS_TOOL => {
                self.call_server_status(&request, &context.ct).await
            }
            HOVER_TOOL => self.call_hover(request, &context.ct).await,
            SIGNATURE_HELP_TOOL => {
                self.call_signature_help(request, &context.ct).await
            }
            DECLARATION_TOOL => {
                self.call_location(request, DECLARATION_SPEC, &context.ct)
                    .await
            }
            DEFINITION_TOOL => {
                self.call_location(request, DEFINITION_SPEC, &context.ct)
                    .await
            }
            TYPE_DEFINITION_TOOL => {
                self.call_location(request, TYPE_DEFINITION_SPEC, &context.ct)
                    .await
            }
            IMPLEMENTATION_TOOL => {
                self.call_location(request, IMPLEMENTATION_SPEC, &context.ct)
                    .await
            }
            REFERENCES_TOOL => self.call_references(request, &context.ct).await,
            DIAGNOSTICS_TOOL => {
                self.call_diagnostics(request, &context.ct).await
            }
            DOCUMENT_SYMBOLS_TOOL => {
                self.call_document_symbols(request, &context.ct).await
            }
            WORKSPACE_SYMBOLS_TOOL => {
                self.call_workspace_symbols(request, &context.ct).await
            }
            PREPARE_RENAME_TOOL => {
                self.call_prepare_rename(request, &context.ct).await
            }
            PREVIEW_RENAME_TOOL => {
                self.call_preview_rename(request, &context.ct).await
            }
            APPLY_RENAME_TOOL if self.startup.allow_mutation() => {
                self.call_apply_rename(request, &context.ct).await
            }
            _ => Err(McpError::method_not_found::<CallToolRequestMethod>()),
        }
    }
}

impl DeixisServer {
    async fn call_server_status(
        &self,
        request: &CallToolRequestParams,
        cancellation: &CancellationToken,
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
        if arguments.server.is_none() {
            if self.language_servers.is_empty() {
                return Ok(error_result(ToolError::new(
                    "no_server_configured",
                    SERVER_STATUS_TOOL,
                    "no language server is configured",
                )));
            }

            let mut statuses = Vec::with_capacity(self.language_servers.len());
            for language_server in self.language_servers.values() {
                statuses.push(language_server.status().await);
            }
            return Ok(success_result(
                status_overview_json(&statuses),
                status_overview_text(&statuses),
            ));
        }

        let language_server_name = arguments.server.as_deref();
        let language_server = language_server_name
            .and_then(|name| self.language_servers.get(name));
        let Some(language_server) = language_server else {
            let name = arguments
                .server
                .as_deref()
                .expect("the overview case returned above");
            return Ok(error_result(
                ToolError::new(
                    "unknown_server",
                    SERVER_STATUS_TOOL,
                    format!("server `{name}` is not configured"),
                )
                .with_server(name),
            ));
        };

        let status = if arguments.start {
            match language_server
                .ensure_started_with_cancellation(cancellation)
                .await
            {
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

        Ok(success_result(status_json(&status), status_text(&status)))
    }

    async fn call_hover(
        &self,
        request: CallToolRequestParams,
        cancellation: &CancellationToken,
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
        let _workspace = self.workspace_gate.read().await;
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
            .hover_with_cancellation(
                file.absolute(),
                route.language_id(),
                arguments.position,
                cancellation,
            )
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
            None => {
                let readiness =
                    language_server.status().await.readiness().clone();
                let text = empty_result_text("hover information", &readiness);
                let mut structured = json!({ "contents": null });
                attach_empty_result_context(&mut structured, &readiness);
                (structured, text)
            }
        };
        Ok(success_result(structured, text))
    }

    async fn call_signature_help(
        &self,
        request: CallToolRequestParams,
        cancellation: &CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        const METHOD: &str = "textDocument/signatureHelp";

        let arguments = request.arguments.unwrap_or_default();
        let arguments = serde_json::from_value::<LocationArguments>(
            JsonValue::Object(arguments),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid signature help arguments: {error}"),
                None,
            )
        })?;
        arguments.validate(SIGNATURE_HELP_TOOL)?;
        let _workspace = self.workspace_gate.read().await;
        let Some(config) = self.config() else {
            return Ok(error_result(
                ToolError::new(
                    "no_server_configured",
                    SIGNATURE_HELP_TOOL,
                    "no language server is configured",
                )
                .with_method(METHOD)
                .with_path(&arguments.path),
            ));
        };
        let file = match self.project().resolve_file(&arguments.path) {
            Ok(file) => file,
            Err(error) => {
                return Ok(error_result(ToolError::from_path(
                    SIGNATURE_HELP_TOOL,
                    METHOD,
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
                        SIGNATURE_HELP_TOOL,
                        METHOD,
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

        let signature_help = match language_server
            .signature_help_with_cancellation(
                file.absolute(),
                route.language_id(),
                arguments.position,
                cancellation,
            )
            .await
        {
            Ok(signature_help) => signature_help,
            Err(error) => {
                return Ok(error_result(ToolError::from_lsp(
                    ToolContext {
                        tool: SIGNATURE_HELP_TOOL,
                        server: Some(route.server().name()),
                        method: Some(METHOD),
                        path: Some(&arguments.path),
                    },
                    &error,
                )));
            }
        };

        let (structured, text) = match signature_help {
            Some(signature_help) => {
                let text = signature_help.text();
                let text = if text.is_empty() {
                    "Signature help is empty.".to_owned()
                } else {
                    text
                };
                let structured = serde_json::to_value(signature_help)
                    .map_err(|error| {
                        McpError::internal_error(
                            format!(
                                "failed to encode signature help response: {error}"
                            ),
                            None,
                        )
                    })?;
                (structured, text)
            }
            None => {
                let readiness =
                    language_server.status().await.readiness().clone();
                let text = empty_result_text("signature help", &readiness);
                let mut structured = json!({
                    "server": route.server().name(),
                    "signatures": [],
                });
                attach_empty_result_context(&mut structured, &readiness);
                (structured, text)
            }
        };
        Ok(success_result(structured, text))
    }

    async fn call_location(
        &self,
        request: CallToolRequestParams,
        spec: LocationToolSpec,
        cancellation: &CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        let arguments = request.arguments.unwrap_or_default();
        let arguments = serde_json::from_value::<LocationArguments>(
            JsonValue::Object(arguments),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid {} arguments: {error}", spec.tool),
                None,
            )
        })?;
        arguments.validate(spec.tool)?;
        let _workspace = self.workspace_gate.read().await;
        let Some(config) = self.config() else {
            return Ok(error_result(
                ToolError::new(
                    "no_server_configured",
                    spec.tool,
                    "no language server is configured",
                )
                .with_method(spec.method)
                .with_path(&arguments.path),
            ));
        };
        let file = match self.project().resolve_file(&arguments.path) {
            Ok(file) => file,
            Err(error) => {
                return Ok(error_result(ToolError::from_path(
                    spec.tool,
                    spec.method,
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
                        spec.tool,
                        spec.method,
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

        let locations = match spec.operation {
            LocationOperation::Declaration => {
                language_server
                    .declaration_with_cancellation(
                        file.absolute(),
                        route.language_id(),
                        arguments.position,
                        cancellation,
                    )
                    .await
            }
            LocationOperation::Definition => {
                language_server
                    .definition_with_cancellation(
                        file.absolute(),
                        route.language_id(),
                        arguments.position,
                        cancellation,
                    )
                    .await
            }
            LocationOperation::TypeDefinition => {
                language_server
                    .type_definition_with_cancellation(
                        file.absolute(),
                        route.language_id(),
                        arguments.position,
                        cancellation,
                    )
                    .await
            }
            LocationOperation::Implementation => {
                language_server
                    .implementation_with_cancellation(
                        file.absolute(),
                        route.language_id(),
                        arguments.position,
                        cancellation,
                    )
                    .await
            }
        };
        let locations = match locations {
            Ok(locations) => locations,
            Err(error) => {
                return Ok(error_result(ToolError::from_lsp(
                    ToolContext {
                        tool: spec.tool,
                        server: Some(route.server().name()),
                        method: Some(spec.method),
                        path: Some(&arguments.path),
                    },
                    &error,
                )));
            }
        };
        let readiness = if locations.is_empty() {
            Some(language_server.status().await.readiness().clone())
        } else {
            None
        };
        let text = if let Some(readiness) = &readiness {
            empty_result_text(spec.plural, readiness)
        } else {
            locations
                .iter()
                .map(|location| location.text())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let locations = serde_json::to_value(locations).map_err(|error| {
            McpError::internal_error(
                format!("failed to encode {} response: {error}", spec.tool),
                None,
            )
        })?;
        let mut structured = json!({ "locations": locations });
        if let Some(readiness) = &readiness {
            attach_empty_result_context(&mut structured, readiness);
        }
        Ok(success_result(structured, text))
    }

    async fn call_references(
        &self,
        request: CallToolRequestParams,
        cancellation: &CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        const METHOD: &str = "textDocument/references";

        let arguments = request.arguments.unwrap_or_default();
        let arguments = serde_json::from_value::<ReferencesArguments>(
            JsonValue::Object(arguments),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid references arguments: {error}"),
                None,
            )
        })?;
        arguments.validate()?;
        let _workspace = self.workspace_gate.read().await;
        let Some(config) = self.config() else {
            return Ok(error_result(
                ToolError::new(
                    "no_server_configured",
                    REFERENCES_TOOL,
                    "no language server is configured",
                )
                .with_method(METHOD)
                .with_path(&arguments.path),
            ));
        };
        let file = match self.project().resolve_file(&arguments.path) {
            Ok(file) => file,
            Err(error) => {
                return Ok(error_result(ToolError::from_path(
                    REFERENCES_TOOL,
                    METHOD,
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
                        REFERENCES_TOOL,
                        METHOD,
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

        let references = match language_server
            .references_with_cancellation(
                file.absolute(),
                route.language_id(),
                arguments.position,
                arguments.include_declaration,
                cancellation,
            )
            .await
        {
            Ok(references) => references,
            Err(error) => {
                return Ok(error_result(ToolError::from_lsp(
                    ToolContext {
                        tool: REFERENCES_TOOL,
                        server: Some(route.server().name()),
                        method: Some(METHOD),
                        path: Some(&arguments.path),
                    },
                    &error,
                )));
            }
        };
        let readiness = if references.is_empty() {
            Some(language_server.status().await.readiness().clone())
        } else {
            None
        };
        paged_result(
            references,
            "references",
            "locations",
            arguments.limit,
            arguments.offset,
            false,
            readiness.as_ref(),
        )
    }

    async fn call_diagnostics(
        &self,
        request: CallToolRequestParams,
        cancellation: &CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        const METHOD: &str = "textDocument/diagnostic";

        let arguments = request.arguments.unwrap_or_default();
        let arguments = serde_json::from_value::<DiagnosticsArguments>(
            JsonValue::Object(arguments),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid diagnostics arguments: {error}"),
                None,
            )
        })?;
        arguments.validate()?;
        let _workspace = self.workspace_gate.read().await;
        let Some(config) = self.config() else {
            return Ok(error_result(
                ToolError::new(
                    "no_server_configured",
                    DIAGNOSTICS_TOOL,
                    "no language server is configured",
                )
                .with_method(METHOD)
                .with_path(&arguments.path),
            ));
        };
        let file = match self.project().resolve_file(&arguments.path) {
            Ok(file) => file,
            Err(error) => {
                return Ok(error_result(ToolError::from_path(
                    DIAGNOSTICS_TOOL,
                    METHOD,
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
                        DIAGNOSTICS_TOOL,
                        METHOD,
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
        let report = match language_server
            .document_diagnostics_with_cancellation(
                file.absolute(),
                route.language_id(),
                cancellation,
            )
            .await
        {
            Ok(report) => report,
            Err(error) => {
                return Ok(error_result(ToolError::from_lsp(
                    ToolContext {
                        tool: DIAGNOSTICS_TOOL,
                        server: Some(route.server().name()),
                        method: Some(METHOD),
                        path: Some(&arguments.path),
                    },
                    &error,
                )));
            }
        };
        let count = report.diagnostics().len();
        let readiness = if report.availability()
            == DiagnosticAvailability::Current
            && count == 0
        {
            Some(language_server.status().await.readiness().clone())
        } else {
            None
        };
        let text = match report.availability() {
            DiagnosticAvailability::Unavailable => {
                format!("Diagnostics are unavailable for {}.", arguments.path)
            }
            DiagnosticAvailability::Stale => format!(
                "{} stale diagnostic{} for {} (report version {}, document version {}).",
                count,
                if count == 1 { "" } else { "s" },
                arguments.path,
                report.report_version().map_or_else(
                    || "unknown".to_owned(),
                    |version| version.to_string()
                ),
                report.document_version(),
            ),
            DiagnosticAvailability::Current if count == 0 => match readiness
                .as_ref()
                .map(ReadinessSnapshot::result_stability)
                .expect("current empty diagnostics should include readiness")
            {
                ResultStability::Stable => {
                    format!("No diagnostics for {}.", arguments.path)
                }
                ResultStability::Transient => format!(
                    "Diagnostics for {} may be incomplete; the language server is still working.",
                    arguments.path
                ),
                ResultStability::Indeterminate => format!(
                    "The language server returned no diagnostics for {}, but the result's stability is indeterminate.",
                    arguments.path
                ),
            },
            DiagnosticAvailability::Current => format!(
                "{} diagnostic{} for {}.",
                count,
                if count == 1 { "" } else { "s" },
                arguments.path,
            ),
        };
        let mut structured = serde_json::to_value(report).map_err(|error| {
            McpError::internal_error(
                format!("failed to encode diagnostics response: {error}"),
                None,
            )
        })?;
        if let Some(readiness) = &readiness {
            attach_empty_result_context(&mut structured, readiness);
        }
        Ok(success_result(structured, text))
    }

    async fn call_document_symbols(
        &self,
        request: CallToolRequestParams,
        cancellation: &CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        const METHOD: &str = "textDocument/documentSymbol";

        let arguments = serde_json::from_value::<DocumentSymbolsArguments>(
            JsonValue::Object(request.arguments.unwrap_or_default()),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid document symbols arguments: {error}"),
                None,
            )
        })?;
        arguments.validate()?;
        let _workspace = self.workspace_gate.read().await;
        let Some(config) = self.config() else {
            return Ok(error_result(
                ToolError::new(
                    "no_server_configured",
                    DOCUMENT_SYMBOLS_TOOL,
                    "no language server is configured",
                )
                .with_method(METHOD)
                .with_path(&arguments.path),
            ));
        };
        let file = match self.project().resolve_file(&arguments.path) {
            Ok(file) => file,
            Err(error) => {
                return Ok(error_result(ToolError::from_path(
                    DOCUMENT_SYMBOLS_TOOL,
                    METHOD,
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
                        DOCUMENT_SYMBOLS_TOOL,
                        METHOD,
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

        let symbols = match language_server
            .document_symbols_with_cancellation(
                file.absolute(),
                route.language_id(),
                cancellation,
            )
            .await
        {
            Ok(symbols) => symbols,
            Err(error) => {
                return Ok(error_result(ToolError::from_lsp(
                    ToolContext {
                        tool: DOCUMENT_SYMBOLS_TOOL,
                        server: Some(route.server().name()),
                        method: Some(METHOD),
                        path: Some(&arguments.path),
                    },
                    &error,
                )));
            }
        };
        let readiness = if symbols.is_empty() {
            Some(language_server.status().await.readiness().clone())
        } else {
            None
        };
        paged_result(
            symbols,
            "document symbols",
            "symbols",
            arguments.limit,
            arguments.offset,
            true,
            readiness.as_ref(),
        )
    }

    async fn call_workspace_symbols(
        &self,
        request: CallToolRequestParams,
        cancellation: &CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        const METHOD: &str = "workspace/symbol";

        let arguments = serde_json::from_value::<WorkspaceSymbolsArguments>(
            JsonValue::Object(request.arguments.unwrap_or_default()),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid workspace symbols arguments: {error}"),
                None,
            )
        })?;
        arguments.validate()?;
        let _workspace = self.workspace_gate.read().await;
        if self.language_servers.is_empty() {
            return Ok(error_result(
                ToolError::new(
                    "no_server_configured",
                    WORKSPACE_SYMBOLS_TOOL,
                    "no language server is configured",
                )
                .with_method(METHOD),
            ));
        }

        let mut selected_servers = BTreeMap::new();
        if let Some(name) = arguments.server.as_deref() {
            let Some(language_server) = self.language_servers.get(name) else {
                return Ok(error_result(
                    ToolError::new(
                        "unknown_server",
                        WORKSPACE_SYMBOLS_TOOL,
                        format!("server `{name}` is not configured"),
                    )
                    .with_server(name)
                    .with_method(METHOD),
                ));
            };
            selected_servers
                .insert(name.to_owned(), Arc::clone(language_server));
        } else {
            for (name, language_server) in &self.language_servers {
                if language_server.status().await.attached() {
                    selected_servers
                        .insert(name.clone(), Arc::clone(language_server));
                }
            }
        }

        if selected_servers.is_empty() {
            return Ok(success_result(
                json!({
                    "symbols": [],
                    "pagination": output::paginate(Vec::new(), arguments.limit, arguments.offset, false).metadata,
                }),
                "No attached language servers.",
            ));
        }

        let mut tasks = JoinSet::new();
        for (name, language_server) in selected_servers {
            let query = arguments.query.clone();
            let cancellation = cancellation.clone();
            tasks.spawn(async move {
                let result = language_server
                    .workspace_symbols_with_cancellation(&query, &cancellation)
                    .await;
                (name, result)
            });
        }

        let mut outcomes = BTreeMap::new();
        while let Some(outcome) = tasks.join_next().await {
            let (name, result) = outcome.map_err(|error| {
                McpError::internal_error(
                    format!("workspace symbol task failed: {error}"),
                    None,
                )
            })?;
            outcomes.insert(name, result);
        }

        let mut symbols = Vec::new();
        let mut first_unsupported = None;
        let mut capable_servers = 0_usize;
        for (name, outcome) in outcomes {
            match outcome {
                Ok(mut server_symbols) => {
                    capable_servers += 1;
                    symbols.append(&mut server_symbols);
                }
                Err(error @ LspError::UnsupportedCapability { .. }) => {
                    if first_unsupported.is_none() {
                        first_unsupported = Some(error);
                    }
                }
                Err(error) => {
                    return Ok(error_result(ToolError::from_lsp(
                        ToolContext {
                            tool: WORKSPACE_SYMBOLS_TOOL,
                            server: Some(&name),
                            method: Some(METHOD),
                            path: None,
                        },
                        &error,
                    )));
                }
            }
        }

        if capable_servers == 0 {
            let error = first_unsupported.expect(
                "every selected server should have returned an outcome",
            );
            return Ok(error_result(ToolError::from_lsp(
                ToolContext {
                    tool: WORKSPACE_SYMBOLS_TOOL,
                    server: None,
                    method: Some(METHOD),
                    path: None,
                },
                &error,
            )));
        }

        paged_result(
            symbols,
            "workspace symbols",
            "symbols",
            arguments.limit,
            arguments.offset,
            false,
            None,
        )
    }

    async fn call_prepare_rename(
        &self,
        request: CallToolRequestParams,
        cancellation: &CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        const METHOD: &str = "textDocument/prepareRename";

        let arguments = serde_json::from_value::<RenameArguments>(
            JsonValue::Object(request.arguments.unwrap_or_default()),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid prepare rename arguments: {error}"),
                None,
            )
        })?;
        arguments.validate(PREPARE_RENAME_TOOL)?;
        let _workspace = self.workspace_gate.read().await;
        let Some(config) = self.config() else {
            return Ok(error_result(
                ToolError::new(
                    "no_server_configured",
                    PREPARE_RENAME_TOOL,
                    "no language server is configured",
                )
                .with_method(METHOD)
                .with_path(&arguments.path),
            ));
        };
        let file = match self.project().resolve_file(&arguments.path) {
            Ok(file) => file,
            Err(error) => {
                return Ok(error_result(ToolError::from_path(
                    PREPARE_RENAME_TOOL,
                    METHOD,
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
                        PREPARE_RENAME_TOOL,
                        METHOD,
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
        let prepared = match language_server
            .prepare_rename_with_cancellation(
                file.absolute(),
                route.language_id(),
                arguments.position,
                cancellation,
            )
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                return Ok(error_result(ToolError::from_lsp(
                    ToolContext {
                        tool: PREPARE_RENAME_TOOL,
                        server: Some(route.server().name()),
                        method: Some(METHOD),
                        path: Some(&arguments.path),
                    },
                    &error,
                )));
            }
        };
        let text = prepared.text();
        let mut structured =
            serde_json::to_value(&prepared).map_err(|error| {
                McpError::internal_error(
                    format!(
                        "failed to encode prepare rename response: {error}"
                    ),
                    None,
                )
            })?;
        if !prepared.valid {
            let readiness = language_server.status().await.readiness().clone();
            attach_empty_result_context(&mut structured, &readiness);
        }
        Ok(success_result(structured, text))
    }

    async fn call_preview_rename(
        &self,
        request: CallToolRequestParams,
        cancellation: &CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        const METHOD: &str = "textDocument/rename";

        let arguments = serde_json::from_value::<PreviewRenameArguments>(
            JsonValue::Object(request.arguments.unwrap_or_default()),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid preview rename arguments: {error}"),
                None,
            )
        })?;
        arguments.validate()?;
        let _workspace = self.workspace_gate.read().await;
        let Some(config) = self.config() else {
            return Ok(error_result(
                ToolError::new(
                    "no_server_configured",
                    PREVIEW_RENAME_TOOL,
                    "no language server is configured",
                )
                .with_method(METHOD)
                .with_path(&arguments.path),
            ));
        };
        let file = match self.project().resolve_file(&arguments.path) {
            Ok(file) => file,
            Err(error) => {
                return Ok(error_result(ToolError::from_path(
                    PREVIEW_RENAME_TOOL,
                    METHOD,
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
                        PREVIEW_RENAME_TOOL,
                        METHOD,
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
        let files = match language_server
            .rename_with_cancellation(
                file.absolute(),
                route.language_id(),
                arguments.position,
                &arguments.new_name,
                cancellation,
            )
            .await
        {
            Ok(files) => files,
            Err(error) => {
                return Ok(error_result(ToolError::from_lsp(
                    ToolContext {
                        tool: PREVIEW_RENAME_TOOL,
                        server: Some(route.server().name()),
                        method: Some(METHOD),
                        path: Some(&arguments.path),
                    },
                    &error,
                )));
            }
        };
        if files.is_empty() {
            let readiness = language_server.status().await.readiness().clone();
            let mut structured = json!({
                "previewId": null,
                "server": route.server().name(),
                "newName": arguments.new_name,
                "fileCount": 0,
                "editCount": 0,
                "expiresInSeconds": null,
                "files": [],
            });
            attach_empty_result_context(&mut structured, &readiness);
            return Ok(success_result(
                structured,
                "Rename produced no changes.",
            ));
        }

        let preview = match self.previews.lock().await.insert(
            route.server().name(),
            &arguments.new_name,
            files,
        ) {
            Ok(preview) => preview,
            Err(error) => {
                return Ok(error_result(ToolError::from_workspace_edit(
                    PREVIEW_RENAME_TOOL,
                    Some(route.server().name()),
                    Some(METHOD),
                    Some(&arguments.path),
                    &error,
                )));
            }
        };
        let text = preview.text();
        let structured = serde_json::to_value(preview).map_err(|error| {
            McpError::internal_error(
                format!("failed to encode rename preview: {error}"),
                None,
            )
        })?;
        Ok(success_result(structured, text))
    }

    async fn call_apply_rename(
        &self,
        request: CallToolRequestParams,
        cancellation: &CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        let arguments = serde_json::from_value::<ApplyRenameArguments>(
            JsonValue::Object(request.arguments.unwrap_or_default()),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid apply rename arguments: {error}"),
                None,
            )
        })?;
        arguments.validate()?;
        let _workspace = self.workspace_gate.write().await;
        let preview =
            match self.previews.lock().await.take(&arguments.preview_id) {
                Ok(preview) => preview,
                Err(error) => {
                    return Ok(error_result(ToolError::from_preview_id(
                        &error,
                    )));
                }
            };
        let cancellation = cancellation.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            apply_rename_preview(preview, &cancellation)
        })
        .await
        .map_err(|error| {
            McpError::internal_error(
                format!("rename application task failed: {error}"),
                None,
            )
        })?;
        let mut outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                return Ok(error_result(ToolError::from_workspace_edit(
                    APPLY_RENAME_TOOL,
                    None,
                    None,
                    None,
                    &error,
                )));
            }
        };
        let language_server = self
            .language_servers
            .get(&outcome.server)
            .expect("rename previews retain a configured server name");
        if let Err(error) = language_server
            .resynchronize_after_workspace_edit(&outcome.paths)
            .await
        {
            let invalidation = language_server
                .invalidate_after_workspace_edit()
                .await
                .err()
                .map(|invalidation| {
                    format!("; invalidation failed: {invalidation}")
                })
                .unwrap_or_default();
            outcome.warnings.push(format!(
                "failed to refresh language-server documents after applying the rename: {error}; the server was invalidated{invalidation}"
            ));
        }
        let text = outcome.text();
        let structured = serde_json::to_value(outcome).map_err(|error| {
            McpError::internal_error(
                format!("failed to encode rename application: {error}"),
                None,
            )
        })?;
        Ok(success_result(structured, text))
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
            | LspError::InvalidDiagnosticReport { server, .. }
            | LspError::InvalidWorkspaceEdit { server, .. }
            | LspError::UnsupportedDocumentSynchronization { server, .. }
            | LspError::UnsupportedPositionEncoding { server, .. }
            | LspError::PositionConversion { server, .. }
            | LspError::DocumentSynchronizationClosed { server, .. }
            | LspError::DocumentLanguageChanged { server, .. }
            | LspError::DocumentVersionOverflow { server, .. }
            | LspError::TransportClosed { server }
            | LspError::OutboundQueueFull { server, .. }
            | LspError::RestartLimitReached { server, .. }
            | LspError::Shutdown { server, .. } => {
                result.server = Some(server.clone());
            }
            LspError::UnsupportedCapability { server, method } => {
                result.server = Some(server.clone());
                result.method = Some((*method).to_owned());
            }
            LspError::ServerExited { server, method, .. }
            | LspError::ResponseTooLarge { server, method, .. }
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

    fn from_preview_id(error: &PreviewIdError) -> Self {
        Self::new("invalid_preview", APPLY_RENAME_TOOL, error.to_string())
    }

    fn from_workspace_edit(
        tool: &'static str,
        server: Option<&str>,
        method: Option<&str>,
        path: Option<&str>,
        error: &WorkspaceEditError,
    ) -> Self {
        let code = match error {
            WorkspaceEditError::InvalidEdit(_)
            | WorkspaceEditError::Read { .. } => "invalid_workspace_edit",
            WorkspaceEditError::Conflict { .. } => "edit_conflict",
            WorkspaceEditError::Stage { .. } => "edit_application_failed",
            WorkspaceEditError::Application { .. }
                if error.rollback_failed() =>
            {
                "edit_rollback_failed"
            }
            WorkspaceEditError::Application { .. } => "edit_application_failed",
            WorkspaceEditError::Canceled { .. } => "request_canceled",
        };
        let mut result = Self::new(code, tool, error.to_string());
        if let Some(server) = server {
            result = result.with_server(server);
        }
        if let Some(method) = method {
            result = result.with_method(method);
        }
        if let Some(path) = path {
            result = result.with_path(path);
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
        LspError::TransportClosed { .. }
        | LspError::ServerExited { .. }
        | LspError::RestartLimitReached { .. } => "server_exited",
        LspError::OutboundQueueFull { .. } => "server_busy",
        LspError::ResponseError { .. } => "lsp_error",
        LspError::Spawn { .. } | LspError::MissingPipe { .. } => {
            "server_start_failed"
        }
        LspError::EncodeMessage(_)
        | LspError::DecodeResult(_)
        | LspError::InvalidDiagnosticReport { .. }
        | LspError::ResponseTooLarge { .. } => "lsp_protocol_error",
        LspError::InvalidWorkspaceEdit {
            source: WorkspaceEditError::Conflict { .. },
            ..
        } => "edit_conflict",
        LspError::InvalidWorkspaceEdit { .. } => "invalid_workspace_edit",
        LspError::ReadDocument { .. }
        | LspError::DocumentSynchronizationClosed { .. }
        | LspError::DocumentLanguageChanged { .. }
        | LspError::DocumentVersionOverflow { .. } => "document_error",
        LspError::RequestCanceled { .. } => "request_canceled",
        LspError::Shutdown { .. } => "server_error",
    }
}

fn paged_result(
    items: impl Serialize,
    subject: &str,
    field: &str,
    limit: u32,
    offset: u64,
    hierarchical: bool,
    readiness: Option<&ReadinessSnapshot>,
) -> Result<CallToolResponse, McpError> {
    let JsonValue::Array(items) =
        serde_json::to_value(items).map_err(|error| {
            McpError::internal_error(
                format!("failed to encode {subject}: {error}"),
                None,
            )
        })?
    else {
        unreachable!("paginated tools return arrays");
    };
    let page = output::paginate(items, limit, offset, hierarchical);
    let text = if page.metadata["total"] == 0 {
        readiness.map_or_else(
            || format!("No {subject}."),
            |readiness| empty_result_text(subject, readiness),
        )
    } else {
        page.text(subject)
    };
    let mut structured =
        json!({ field: page.items, "pagination": page.metadata });
    if let Some(readiness) = readiness {
        attach_empty_result_context(&mut structured, readiness);
    }
    Ok(success_result(structured, text))
}

fn success_result(
    structured: JsonValue,
    text: impl Into<String>,
) -> CallToolResponse {
    let mut result = CallToolResult::structured(structured);
    result.content = vec![ContentBlock::text(text)];
    result.into()
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
struct LocationArguments {
    path: String,
    #[serde(default)]
    server: Option<String>,
    position: Position,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReferencesArguments {
    path: String,
    #[serde(default)]
    server: Option<String>,
    position: Position,
    include_declaration: bool,
    #[serde(default = "output::default_limit")]
    limit: u32,
    #[serde(default)]
    offset: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiagnosticsArguments {
    path: String,
    #[serde(default)]
    server: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DocumentSymbolsArguments {
    path: String,
    #[serde(default)]
    server: Option<String>,
    #[serde(default = "output::default_limit")]
    limit: u32,
    #[serde(default)]
    offset: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceSymbolsArguments {
    query: String,
    #[serde(default)]
    server: Option<String>,
    #[serde(default = "output::default_limit")]
    limit: u32,
    #[serde(default)]
    offset: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RenameArguments {
    path: String,
    #[serde(default)]
    server: Option<String>,
    position: Position,
}

impl RenameArguments {
    fn validate(&self, tool: &str) -> Result<(), McpError> {
        if self.path.is_empty() {
            return Err(McpError::invalid_params(
                format!("invalid {tool} arguments: `path` must not be empty"),
                None,
            ));
        }
        if self
            .server
            .as_ref()
            .is_some_and(|server| server.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                format!("invalid {tool} arguments: `server` must not be empty"),
                None,
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PreviewRenameArguments {
    path: String,
    #[serde(default)]
    server: Option<String>,
    position: Position,
    new_name: String,
}

impl PreviewRenameArguments {
    fn validate(&self) -> Result<(), McpError> {
        RenameArguments {
            path: self.path.clone(),
            server: self.server.clone(),
            position: self.position,
        }
        .validate(PREVIEW_RENAME_TOOL)?;
        if self.new_name.is_empty() {
            return Err(McpError::invalid_params(
                "invalid preview rename arguments: `newName` must not be empty",
                None,
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApplyRenameArguments {
    preview_id: String,
}

impl ApplyRenameArguments {
    fn validate(&self) -> Result<(), McpError> {
        if self.preview_id.trim().is_empty() {
            return Err(McpError::invalid_params(
                "invalid apply rename arguments: `previewId` must not be empty",
                None,
            ));
        }
        Ok(())
    }
}

impl WorkspaceSymbolsArguments {
    fn validate(&self) -> Result<(), McpError> {
        if self
            .server
            .as_ref()
            .is_some_and(|server| server.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                "invalid workspace symbols arguments: `server` must not be empty",
                None,
            ));
        }
        output::validate_limit(self.limit)
    }
}

impl DocumentSymbolsArguments {
    fn validate(&self) -> Result<(), McpError> {
        if self.path.is_empty() {
            return Err(McpError::invalid_params(
                "invalid document symbols arguments: `path` must not be empty",
                None,
            ));
        }
        if self
            .server
            .as_ref()
            .is_some_and(|server| server.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                "invalid document symbols arguments: `server` must not be empty",
                None,
            ));
        }
        output::validate_limit(self.limit)
    }
}

impl DiagnosticsArguments {
    fn validate(&self) -> Result<(), McpError> {
        if self.path.is_empty() {
            return Err(McpError::invalid_params(
                "invalid diagnostics arguments: `path` must not be empty",
                None,
            ));
        }
        if self
            .server
            .as_ref()
            .is_some_and(|server| server.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                "invalid diagnostics arguments: `server` must not be empty",
                None,
            ));
        }
        Ok(())
    }
}

impl ReferencesArguments {
    fn validate(&self) -> Result<(), McpError> {
        if self.path.is_empty() {
            return Err(McpError::invalid_params(
                "invalid references arguments: `path` must not be empty",
                None,
            ));
        }
        if self
            .server
            .as_ref()
            .is_some_and(|server| server.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                "invalid references arguments: `server` must not be empty",
                None,
            ));
        }
        output::validate_limit(self.limit)
    }
}

impl LocationArguments {
    fn validate(&self, tool: &str) -> Result<(), McpError> {
        if self.path.is_empty() {
            return Err(McpError::invalid_params(
                format!("invalid {tool} arguments: `path` must not be empty"),
                None,
            ));
        }
        if self
            .server
            .as_ref()
            .is_some_and(|server| server.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                format!("invalid {tool} arguments: `server` must not be empty"),
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

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
enum ServerStatusOverviewState {
    NotStarted,
    Running,
    Attached,
}

impl ServerStatusOverviewState {
    fn text(self) -> &'static str {
        match self {
            Self::NotStarted => "not started",
            Self::Running => "running",
            Self::Attached => "attached",
        }
    }
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
        if self.start && self.server.is_none() {
            return Err(McpError::invalid_params(
                "invalid server status arguments: `server` is required when `start` is true",
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
        "Summarize all configured language servers, or return detailed status for one.",
        object_schema(json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Configured server name. Omit it for a compact overview of every configured server."
                },
                "start": {
                    "type": "boolean",
                    "description": "Start the named language server before returning status. Requires `server`."
                }
            },
            "additionalProperties": false
        })),
    )
    .with_raw_output_schema(result_output_schema(json!({
        "oneOf": [status_overview_output_schema(), status_output_schema()]
    })))
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
            },
            "readiness": readiness_schema(),
            "resultStability": result_stability_schema()
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

fn signature_help_tool() -> Tool {
    Tool::new(
        SIGNATURE_HELP_TOOL,
        "Return call signatures and structured parameter information for a UTF-8 position in a project file.",
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
            "server": {
                "type": "string",
                "description": "Configured name of the language server that returned this signature help."
            },
            "signatures": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string" },
                        "documentation": signature_documentation_schema(),
                        "parameters": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {
                                        "oneOf": [
                                            { "type": "string" },
                                            {
                                                "type": "array",
                                                "items": {
                                                    "type": "integer",
                                                    "minimum": 0,
                                                    "maximum": u32::MAX
                                                },
                                                "minItems": 2,
                                                "maxItems": 2
                                            }
                                        ]
                                    },
                                    "documentation": signature_documentation_schema()
                                },
                                "required": ["label"],
                                "additionalProperties": true
                            }
                        },
                        "activeParameter": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": u32::MAX
                        }
                    },
                    "required": ["label"],
                    "additionalProperties": true
                }
            },
            "activeSignature": {
                "type": "integer",
                "minimum": 0,
                "maximum": u32::MAX
            },
            "activeParameter": {
                "type": "integer",
                "minimum": 0,
                "maximum": u32::MAX
            },
            "readiness": readiness_schema(),
            "resultStability": result_stability_schema()
        },
        "required": ["server", "signatures"],
        "additionalProperties": true
    })))
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    )
}

fn location_tool(spec: LocationToolSpec) -> Tool {
    Tool::new(
        spec.tool,
        format!(
            "Return {} for a UTF-8 position in a project file.",
            spec.plural
        ),
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
                            "description": "Configured name of the language server that returned this location."
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
            },
            "readiness": readiness_schema(),
            "resultStability": result_stability_schema()
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

fn references_tool() -> Tool {
    Tool::new(
        REFERENCES_TOOL,
        "Return a bounded page of references for a UTF-8 position in a project file. Use pagination.nextOffset to continue; text summarizes the structured locations.",
        object_schema(json!({
            "type": "object",
            "properties": {
                "limit": output::limit_schema(),
                "offset": output::offset_schema(),
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
                "includeDeclaration": {
                    "type": "boolean",
                    "description": "Whether the declaration itself is included among the references."
                }
            },
            "required": ["path", "position", "includeDeclaration"],
            "additionalProperties": false
        })),
    )
    .with_raw_output_schema(result_output_schema(json!({
        "type": "object",
        "properties": {
            "pagination": output::pagination_schema(),
            "locations": {
                "type": "array",
                "maxItems": output::MAX_LIMIT,
                "items": {
                    "type": "object",
                    "properties": {
                        "server": {
                            "type": "string",
                            "description": "Configured name of the language server that returned this location."
                        },
                        "uri": { "type": "string" },
                        "range": range_schema(),
                        "positionEncoding": {
                            "enum": ["utf-8", "utf-16", "utf-32"],
                            "description": "Encoding used by character offsets in the range. UTF-8 is used whenever the target is a readable project file."
                        }
                    },
                    "required": ["server", "uri", "range", "positionEncoding"],
                    "additionalProperties": false
                }
            },
            "readiness": readiness_schema(),
            "resultStability": result_stability_schema()
        },
        "required": ["locations", "pagination"],
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

fn diagnostics_tool() -> Tool {
    Tool::new(
        DIAGNOSTICS_TOOL,
        "Return pull or cached push diagnostics for a project file.",
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
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })),
    )
    .with_raw_output_schema(result_output_schema(json!({
        "type": "object",
        "properties": {
            "server": {
                "type": "string",
                "description": "Configured name of the language server that produced this report."
            },
            "uri": { "type": "string" },
            "source": { "enum": ["pull", "push"] },
            "availability": {
                "enum": ["current", "stale", "unavailable"]
            },
            "documentVersion": { "type": "integer" },
            "reportVersion": { "type": ["integer", "null"] },
            "resultId": { "type": ["string", "null"] },
            "positionEncoding": {
                "enum": ["utf-8", "utf-16", "utf-32"],
                "description": "Encoding used by character offsets in diagnostic ranges. Current reports use UTF-8; stale push reports retain the server encoding."
            },
            "diagnostics": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "range": range_schema()
                    },
                    "required": ["range"],
                    "additionalProperties": true
                }
            },
            "readiness": readiness_schema(),
            "resultStability": result_stability_schema()
        },
        "required": [
            "server",
            "uri",
            "source",
            "availability",
            "documentVersion",
            "reportVersion",
            "resultId",
            "positionEncoding",
            "diagnostics"
        ],
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

fn document_symbols_tool() -> Tool {
    Tool::new(
        DOCUMENT_SYMBOLS_TOOL,
        "Inspect a file outline only when the file's symbol structure is needed. This is not a default navigation step or a prerequisite for other queries; use definition, type_definition, implementation, or references directly for targeted symbol navigation. Return a bounded page of symbols in preorder, retaining hierarchy within the page. Every nested symbol counts toward the limit. Use pagination.nextOffset to continue and index/parentIndex to join pages.",
        object_schema(json!({
            "type": "object",
            "properties": {
                "limit": output::limit_schema(),
                "offset": output::offset_schema(),
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Project-relative or root-contained absolute file path."
                },
                "server": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Configured server name used to resolve an otherwise ambiguous route."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })),
    )
    .with_raw_output_schema(document_symbols_output_schema())
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    )
}

fn document_symbols_output_schema() -> Arc<JsonObject> {
    Arc::new(object_schema(json!({
        "$defs": {
            "documentSymbol": {
                "type": "object",
                "properties": {
                    "server": {
                        "type": "string",
                        "description": "Configured name of the language server that returned this symbol."
                    },
                    "uri": { "type": "string" },
                    "index": { "type": "integer", "minimum": 0, "description": "Preorder index in this query response." },
                    "parentIndex": { "type": ["integer", "null"], "minimum": 0, "description": "Parent's preorder index, or null for a document root. A parent outside this page is not repeated." },
                    "childCount": { "type": "integer", "minimum": 0, "description": "Full number of direct children, including those outside this page." },
                    "name": { "type": "string" },
                    "detail": { "type": "string" },
                    "kind": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 26
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "integer" }
                    },
                    "deprecated": { "type": "boolean" },
                    "containerName": { "type": "string" },
                    "range": range_schema(),
                    "selectionRange": range_schema(),
                    "positionEncoding": {
                        "enum": ["utf-8", "utf-16", "utf-32"],
                        "description": "Encoding used by character offsets in the symbol ranges. UTF-8 is used whenever the symbol belongs to a readable project file."
                    },
                    "children": {
                        "type": "array",
                        "maxItems": output::MAX_LIMIT,
                        "items": { "$ref": "#/$defs/documentSymbol" }
                    },
                    "data": {}
                },
                "required": [
                    "server",
                    "uri",
                    "name",
                    "kind",
                    "range",
                    "selectionRange",
                    "positionEncoding",
                    "children",
                    "index",
                    "parentIndex",
                    "childCount"
                ],
                "additionalProperties": true
            }
        },
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "pagination": output::pagination_schema(),
                    "symbols": {
                        "type": "array",
                        "maxItems": output::MAX_LIMIT,
                        "items": { "$ref": "#/$defs/documentSymbol" }
                    },
                    "readiness": readiness_schema(),
                    "resultStability": result_stability_schema()
                },
                "required": ["symbols", "pagination"],
                "additionalProperties": false
            },
            error_output_schema()
        ]
    })))
}

fn workspace_symbols_tool() -> Tool {
    Tool::new(
        WORKSPACE_SYMBOLS_TOOL,
        "Return a bounded page of project-wide symbols from attached language servers, or from one named server. Use pagination.nextOffset to continue; text summarizes the structured symbols.",
        object_schema(json!({
            "type": "object",
            "properties": {
                "limit": output::limit_schema(),
                "offset": output::offset_schema(),
                "query": {
                    "type": "string",
                    "description": "Query passed to each selected language server. An empty string requests all symbols."
                },
                "server": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Configured server to query, starting it if necessary. Omit it to query attached servers without starting others."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })),
    )
    .with_raw_output_schema(result_output_schema(json!({
        "type": "object",
        "properties": {
            "pagination": output::pagination_schema(),
            "symbols": {
                "type": "array",
                "maxItems": output::MAX_LIMIT,
                "items": {
                    "type": "object",
                    "properties": {
                        "server": {
                            "type": "string",
                            "description": "Configured name of the language server that returned this symbol."
                        },
                        "name": { "type": "string" },
                        "kind": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 26
                        },
                        "tags": {
                            "type": "array",
                            "items": { "type": "integer" }
                        },
                        "deprecated": { "type": "boolean" },
                        "containerName": { "type": "string" },
                        "location": {
                            "type": "object",
                            "properties": {
                                "uri": { "type": "string" },
                                "range": range_schema()
                            },
                            "required": ["uri", "range"],
                            "additionalProperties": true
                        },
                        "positionEncoding": {
                            "enum": ["utf-8", "utf-16", "utf-32"],
                            "description": "Encoding used by character offsets in the location range. UTF-8 is used whenever the location is a readable project file."
                        },
                        "data": {}
                    },
                    "required": [
                        "server",
                        "name",
                        "kind",
                        "location",
                        "positionEncoding"
                    ],
                    "additionalProperties": true
                }
            }
        },
        "required": ["symbols", "pagination"],
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

fn prepare_rename_tool() -> Tool {
    Tool::new(
        PREPARE_RENAME_TOOL,
        "Test whether a symbol can be renamed at a UTF-8 position.",
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
                "position": position_schema()
            },
            "required": ["path", "position"],
            "additionalProperties": false
        })),
    )
    .with_raw_output_schema(result_output_schema(json!({
        "type": "object",
        "properties": {
            "server": { "type": "string" },
            "uri": { "type": "string" },
            "valid": { "type": "boolean" },
            "range": range_schema(),
            "placeholder": { "type": "string" },
            "defaultBehavior": { "type": "boolean" },
            "readiness": readiness_schema(),
            "resultStability": result_stability_schema()
        },
        "required": ["server", "uri", "valid"],
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

fn preview_rename_tool() -> Tool {
    Tool::new(
        PREVIEW_RENAME_TOOL,
        "Compute and validate a symbol rename without changing files. Inspect the returned edits and diff before calling `apply_rename`, which requires the `--allow-mutation` CLI flag.",
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
                "newName": {
                    "type": "string",
                    "minLength": 1,
                    "description": "New symbol name validated by the language server."
                }
            },
            "required": ["path", "position", "newName"],
            "additionalProperties": false
        })),
    )
    .with_raw_output_schema(result_output_schema(json!({
        "type": "object",
        "properties": {
            "previewId": { "type": ["string", "null"] },
            "server": { "type": "string" },
            "newName": { "type": "string" },
            "fileCount": { "type": "integer", "minimum": 0 },
            "editCount": { "type": "integer", "minimum": 0 },
            "expiresInSeconds": {
                "type": ["integer", "null"],
                "minimum": 0
            },
            "files": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "uri": { "type": "string" },
                        "documentVersion": {
                            "type": ["integer", "null"]
                        },
                        "positionEncoding": { "const": "utf-8" },
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "range": range_schema(),
                                    "oldText": { "type": "string" },
                                    "newText": { "type": "string" }
                                },
                                "required": ["range", "oldText", "newText"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": [
                        "path",
                        "uri",
                        "documentVersion",
                        "positionEncoding",
                        "edits"
                    ],
                    "additionalProperties": false
                }
            },
            "readiness": readiness_schema(),
            "resultStability": result_stability_schema()
        },
        "required": [
            "previewId",
            "server",
            "newName",
            "fileCount",
            "editCount",
            "expiresInSeconds",
            "files"
        ],
        "additionalProperties": false
    })))
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(false)
            .open_world(false),
    )
}

fn apply_rename_tool() -> Tool {
    Tool::new(
        APPLY_RENAME_TOOL,
        "Apply one exact, previously inspected rename preview. The preview is consumed on this attempt.",
        object_schema(json!({
            "type": "object",
            "properties": {
                "previewId": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Opaque identifier returned by `preview_rename`."
                }
            },
            "required": ["previewId"],
            "additionalProperties": false
        })),
    )
    .with_raw_output_schema(result_output_schema(json!({
        "type": "object",
        "properties": {
            "applied": { "const": true },
            "previewId": { "type": "string" },
            "server": { "type": "string" },
            "fileCount": { "type": "integer", "minimum": 1 },
            "editCount": { "type": "integer", "minimum": 1 },
            "paths": {
                "type": "array",
                "items": { "type": "string" }
            },
            "warnings": {
                "type": "array",
                "items": { "type": "string" }
            }
        },
        "required": [
            "applied",
            "previewId",
            "server",
            "fileCount",
            "editCount",
            "paths",
            "warnings"
        ],
        "additionalProperties": false
    })))
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            .idempotent(false)
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

fn signature_documentation_schema() -> JsonValue {
    json!({
        "oneOf": [
            { "type": "string" },
            {
                "type": "object",
                "properties": {
                    "kind": { "enum": ["plaintext", "markdown"] },
                    "value": { "type": "string" }
                },
                "required": ["kind", "value"],
                "additionalProperties": false
            }
        ]
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
            "capabilities": { "type": ["object", "null"] },
            "readiness": readiness_schema()
        },
        "required": [
            "configuredName",
            "started",
            "serverName",
            "serverVersion",
            "positionEncoding",
            "textDocumentSync",
            "capabilities",
            "readiness"
        ],
        "additionalProperties": false
    })
}

fn status_overview_output_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "servers": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "configuredName": { "type": "string" },
                        "state": {
                            "enum": [
                                "notStarted",
                                "running",
                                "attached"
                            ]
                        }
                    },
                    "required": ["configuredName", "state"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["servers"],
        "additionalProperties": false
    })
}

fn result_output_schema(success: JsonValue) -> Arc<JsonObject> {
    Arc::new(object_schema(json!({
        "oneOf": [success, error_output_schema()]
    })))
}

fn readiness_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "state": {
                "enum": [
                    "notStarted",
                    "starting",
                    "busy",
                    "ready",
                    "degraded",
                    "unknown"
                ]
            },
            "source": {
                "enum": ["lifecycle", "workDoneProgress", "serverStatus"]
            },
            "health": { "enum": ["ok", "warning", "error"] },
            "message": { "type": "string" },
            "activeProgress": { "type": "integer", "minimum": 0 }
        },
        "required": ["state", "source", "activeProgress"],
        "additionalProperties": false
    })
}

fn result_stability_schema() -> JsonValue {
    json!({ "enum": ["stable", "transient", "indeterminate"] })
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
                            "server_busy",
                            "server_exited",
                            "lsp_error",
                            "server_start_failed",
                            "lsp_protocol_error",
                            "document_error",
                            "request_canceled",
                            "server_error",
                            "routing_error",
                            "no_server_configured",
                            "unknown_server",
                            "invalid_workspace_edit",
                            "invalid_preview",
                            "edit_conflict",
                            "edit_application_failed",
                            "edit_rollback_failed"
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
        "readiness": status.readiness(),
    })
}

fn status_overview_json(statuses: &[ServerSnapshot]) -> JsonValue {
    json!({
        "servers": statuses
            .iter()
            .map(|status| {
                json!({
                    "configuredName": status.configured_name(),
                    "state": status_overview_state(status),
                })
            })
            .collect::<Vec<_>>(),
    })
}

fn status_overview_text(statuses: &[ServerSnapshot]) -> String {
    statuses
        .iter()
        .map(|status| {
            format!(
                "{}: {}",
                status.configured_name(),
                status_overview_state(status).text()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn status_overview_state(status: &ServerSnapshot) -> ServerStatusOverviewState {
    if !status.started() {
        return ServerStatusOverviewState::NotStarted;
    }
    if status.attached() {
        return ServerStatusOverviewState::Attached;
    }
    ServerStatusOverviewState::Running
}

fn status_text(status: &ServerSnapshot) -> String {
    let state = if status.started() {
        match (status.server_name(), status.server_version()) {
            (Some(name), Some(version)) => {
                format!("started as {name} {version}")
            }
            (Some(name), None) => format!("started as {name}"),
            (None, _) => "started".to_owned(),
        }
    } else {
        "not started".to_owned()
    };
    let encoding = status
        .position_encoding()
        .map_or_else(String::new, |encoding| {
            format!("; position encoding {encoding}")
        });
    let readiness = status.readiness();
    let readiness_state = match readiness.state() {
        ReadinessState::NotStarted => "not started",
        ReadinessState::Starting => "starting",
        ReadinessState::Busy => "busy",
        ReadinessState::Ready => "ready",
        ReadinessState::Degraded => "degraded",
        ReadinessState::Unknown => "unknown",
    };
    let readiness_source = match readiness.source() {
        ReadinessSource::Lifecycle => "lifecycle",
        ReadinessSource::WorkDoneProgress => "work-done progress",
        ReadinessSource::ServerStatus => "server status",
    };

    format!(
        "{}: {state}{encoding}; readiness {readiness_state} ({readiness_source}).",
        status.configured_name()
    )
}

fn attach_empty_result_context(
    structured: &mut JsonValue,
    readiness: &ReadinessSnapshot,
) {
    let object = structured
        .as_object_mut()
        .expect("successful tool output should be a JSON object");
    object.insert("readiness".to_owned(), json!(readiness));
    object.insert(
        "resultStability".to_owned(),
        json!(readiness.result_stability()),
    );
}

fn empty_result_text(subject: &str, readiness: &ReadinessSnapshot) -> String {
    match readiness.result_stability() {
        ResultStability::Stable => format!("No {subject}."),
        ResultStability::Transient => format!(
            "No stable {subject} yet; the language server is still working."
        ),
        ResultStability::Indeterminate => format!(
            "The language server returned no {subject}, but the result's stability is indeterminate."
        ),
    }
}

fn object_schema(schema: JsonValue) -> JsonObject {
    schema
        .as_object()
        .expect("tool input schema must be a JSON object")
        .clone()
}
