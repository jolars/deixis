use super::*;

pub(super) const INCOMING_CALLS_TOOL: &str = "incoming_calls";
pub(super) const OUTGOING_CALLS_TOOL: &str = "outgoing_calls";

fn tool_identity(
    direction: CallHierarchyDirection,
) -> (&'static str, &'static str) {
    match direction {
        CallHierarchyDirection::Incoming => {
            (INCOMING_CALLS_TOOL, "incoming calls")
        }
        CallHierarchyDirection::Outgoing => {
            (OUTGOING_CALLS_TOOL, "outgoing calls")
        }
    }
}

impl DeixisServer {
    pub(super) async fn call_call_hierarchy(
        &self,
        request: CallToolRequestParams,
        direction: CallHierarchyDirection,
        cancellation: &CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        let method = direction.method();
        let (tool, subject) = tool_identity(direction);

        let arguments = request.arguments.unwrap_or_default();
        let arguments = serde_json::from_value::<CallHierarchyArguments>(
            JsonValue::Object(arguments),
        )
        .map_err(|error| {
            McpError::invalid_params(
                format!("invalid {tool} arguments: {error}"),
                None,
            )
        })?;
        arguments.validate(tool)?;
        let _workspace = self.workspace_gate.read().await;
        let Some(config) = self.config() else {
            return Ok(error_result(
                ToolError::new(
                    "no_server_configured",
                    tool,
                    "no language server is configured",
                )
                .with_method(method)
                .with_path(&arguments.path),
            ));
        };
        let file = match self.project().resolve_file(&arguments.path) {
            Ok(file) => file,
            Err(error) => {
                return Ok(error_result(ToolError::from_path(
                    tool,
                    method,
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
                        tool,
                        method,
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

        let calls = match language_server
            .call_hierarchy_with_cancellation(
                file.absolute(),
                route.language_id(),
                arguments.position,
                direction,
                cancellation,
            )
            .await
        {
            Ok(calls) => calls,
            Err(error) => {
                return Ok(error_result(ToolError::from_lsp(
                    ToolContext {
                        tool,
                        server: Some(route.server().name()),
                        method: Some(method),
                        path: Some(&arguments.path),
                    },
                    &error,
                )));
            }
        };
        let readiness = if calls.is_empty() {
            Some(language_server.status().await.readiness().clone())
        } else {
            None
        };
        paged_result(
            calls,
            subject,
            "calls",
            arguments.limit,
            arguments.offset,
            false,
            readiness.as_ref(),
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CallHierarchyArguments {
    path: String,
    server: Option<String>,
    position: Position,
    #[serde(default = "output::default_limit")]
    limit: u32,
    #[serde(default)]
    offset: u64,
}

impl CallHierarchyArguments {
    fn validate(&self, tool: &str) -> Result<(), McpError> {
        if self.path.is_empty()
            || self
                .server
                .as_ref()
                .is_some_and(|server| server.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                format!(
                    "invalid {tool} arguments: path and server must not be empty"
                ),
                None,
            ));
        }
        output::validate_limit(self.limit)
    }
}

pub(super) fn call_hierarchy_tool(direction: CallHierarchyDirection) -> Tool {
    let (tool, subject) = tool_identity(direction);
    Tool::new(
        tool,
        format!("Return a bounded page of {subject} for a UTF-8 position in a project file. Prepare call hierarchy internally and return caller/callee pairs with call-site ranges. Use pagination.nextOffset to continue with the same query arguments."),
        object_schema(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "minLength": 1, "description": "Project-relative or root-contained absolute file path." },
                "server": { "type": "string", "minLength": 1, "description": "Configured server name used to resolve an otherwise ambiguous route." },
                "position": position_schema(),
                "limit": output::limit_schema(),
                "offset": output::offset_schema()
            },
            "required": ["path", "position"],
            "additionalProperties": false
        })),
    )
    .with_raw_output_schema(result_output_schema(json!({
        "type": "object",
        "properties": {
            "calls": {
                "type": "array", "maxItems": output::MAX_LIMIT,
                "items": {
                    "type": "object",
                    "properties": {
                        "from": item_schema(), "to": item_schema(),
                        "fromRanges": {
                            "type": "array", "items": range_schema(),
                            "description": "Call sites in from.uri, using from.positionEncoding."
                        }
                    },
                    "required": ["from", "to", "fromRanges"],
                    "additionalProperties": false
                }
            },
            "pagination": output::pagination_schema(),
            "readiness": readiness_schema(),
            "resultStability": result_stability_schema()
        },
        "required": ["calls", "pagination"],
        "additionalProperties": false
    })))
    .with_annotations(ToolAnnotations::new().read_only(true).destructive(false).idempotent(true).open_world(false))
}

fn item_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "server": { "type": "string", "description": "Configured name of the language server that returned this symbol." },
            "name": { "type": "string" },
            "kind": { "type": "integer", "minimum": 0 },
            "uri": { "type": "string" },
            "range": range_schema(), "selectionRange": range_schema(),
            "positionEncoding": { "enum": ["utf-8", "utf-16", "utf-32"], "description": "UTF-8 for readable project files; the server encoding otherwise." },
            "tags": { "type": "array", "items": { "type": "integer" } },
            "detail": { "type": "string" },
            "data": {}
        },
        "required": ["server", "name", "kind", "uri", "range", "selectionRange", "positionEncoding"],
        "additionalProperties": true
    })
}
