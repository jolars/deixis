use std::{
    collections::BTreeSet,
    env,
    error::Error,
    fmt,
    io::{self, BufRead, Write},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

const JSONRPC_VERSION: &str = "2.0";

fn main() -> Result<(), Box<dyn Error>> {
    let mode = parse_mode();
    eprintln!("mock-lsp started in {mode} mode");

    let output = Arc::new(Mutex::new(io::stdout()));
    let mut input = io::stdin().lock();
    let state = Arc::new(Mutex::new(MockState::default()));

    loop {
        let Some(message) = read_message(&mut input)? else {
            return Ok(());
        };

        if let Some(method) = message.get("method").and_then(Json::as_str) {
            if let Some(id) = message.get("id").cloned() {
                handle_request(
                    &mode,
                    &output,
                    &mut input,
                    state.clone(),
                    id,
                    method,
                    message.get("params").cloned().unwrap_or(Json::Null),
                )?;
            } else {
                handle_notification(
                    &mode,
                    &state,
                    method,
                    message.get("params").cloned().unwrap_or(Json::Null),
                );
                if method == "exit" && mode != "ignore-shutdown" {
                    return Ok(());
                }
            }
        }
    }
}

fn parse_mode() -> String {
    let mut args = env::args().skip(1);
    let mut mode = String::from("normal");
    while let Some(arg) = args.next() {
        if arg == "--mode" {
            mode = args.next().unwrap_or(mode);
        }
    }
    mode
}

fn handle_request<R: BufRead>(
    mode: &str,
    output: &Arc<Mutex<io::Stdout>>,
    input: &mut R,
    state: Arc<Mutex<MockState>>,
    id: Json,
    method: &str,
    params: Json,
) -> Result<(), Box<dyn Error>> {
    match method {
        "initialize" => {
            eprintln!("mock-lsp initializing");
            state.lock().unwrap().initialize_params = Some(params);
            let text_document_sync = if mode.starts_with("document-incremental") {
                json_object([
                    ("openClose", Json::Bool(true)),
                    ("change", Json::Number(2)),
                ])
            } else if mode == "document-none" {
                Json::Number(0)
            } else {
                Json::Number(1)
            };
            let position_encoding = match mode {
                "position-omitted" => None,
                "position-unsupported" => {
                    Some(Json::String("utf-7".to_owned()))
                }
                "position-malformed" => Some(Json::Number(8)),
                "document-incremental-utf-16" => {
                    Some(Json::String("utf-16".to_owned()))
                }
                "document-incremental-utf-32" => {
                    Some(Json::String("utf-32".to_owned()))
                }
                "hover-utf-16"
                | "definition-locations-utf-16"
                | "definition-project-target-utf-16"
                | "definition-external-utf-16"
                | "type-definition-locations-utf-16" => {
                    Some(Json::String("utf-16".to_owned()))
                }
                "hover-utf-32"
                | "definition-links-utf-32"
                | "implementation-links-utf-32" => {
                    Some(Json::String("utf-32".to_owned()))
                }
                _ => Some(Json::String("utf-8".to_owned())),
            };
            let hover_provider = match mode {
                "hover-unsupported" => Json::Bool(false),
                "hover-options" => json_object([(
                    "workDoneProgress",
                    Json::Bool(true),
                )]),
                _ => Json::Bool(true),
            };
            let definition_provider = match mode {
                "definition-unsupported" => Json::Bool(false),
                "definition-options" => json_object([(
                    "workDoneProgress",
                    Json::Bool(true),
                )]),
                _ => Json::Bool(true),
            };
            let declaration_provider = match mode {
                "declaration-unsupported" => Json::Bool(false),
                "declaration-options" => json_object([(
                    "workDoneProgress",
                    Json::Bool(true),
                )]),
                _ => Json::Bool(true),
            };
            let type_definition_provider = match mode {
                "type-definition-unsupported" => Json::Bool(false),
                "type-definition-options" => json_object([(
                    "workDoneProgress",
                    Json::Bool(true),
                )]),
                _ => Json::Bool(true),
            };
            let implementation_provider = match mode {
                "implementation-unsupported" => Json::Bool(false),
                "implementation-options" => json_object([(
                    "workDoneProgress",
                    Json::Bool(true),
                )]),
                _ => Json::Bool(true),
            };
            let mut capabilities = vec![
                ("declarationProvider".to_owned(), declaration_provider),
                ("definitionProvider".to_owned(), definition_provider),
                ("hoverProvider".to_owned(), hover_provider),
                (
                    "implementationProvider".to_owned(),
                    implementation_provider,
                ),
                ("textDocumentSync".to_owned(), text_document_sync),
                (
                    "typeDefinitionProvider".to_owned(),
                    type_definition_provider,
                ),
            ];
            if let Some(position_encoding) = position_encoding {
                capabilities.push((
                    "positionEncoding".to_owned(),
                    position_encoding,
                ));
            }
            write_message(
                output,
                response(
                    id,
                    json_object([
                        (
                            "capabilities",
                            Json::Object(capabilities),
                        ),
                        (
                            "serverInfo",
                            json_object([
                                (
                                    "name",
                                    Json::String("deixis-mock-lsp".to_owned()),
                                ),
                                ("version", Json::String("0.1.0".to_owned())),
                            ]),
                        ),
                    ]),
                ),
            )?;
        }
        "mock/echo" => {
            let initialized = state.lock().unwrap().initialized;
            write_message(
                output,
                response(
                    id,
                    json_object([
                        ("echo", params),
                        ("initialized", Json::Bool(initialized)),
                    ]),
                ),
            )?;
        }
        "mock/malformedThenEcho" => {
            eprintln!("mock-lsp emitting malformed output");
            write_raw_message(output, b"{\"jsonrpc\":\"2.0\",\"id\":999,")?;
            let initialized = state.lock().unwrap().initialized;
            write_message(
                output,
                response(
                    id,
                    json_object([
                        ("echo", params),
                        ("initialized", Json::Bool(initialized)),
                    ]),
                ),
            )?;
        }
        "mock/initialized" => {
            let initialized = state.lock().unwrap().initialized;
            write_message(
                output,
                response(
                    id,
                    json_object([("initialized", Json::Bool(initialized))]),
                ),
            )?;
        }
        "mock/delay" => {
            let delay_ms = params
                .get("delay_ms")
                .and_then(Json::as_u64)
                .unwrap_or(250);
            let output = Arc::clone(output);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(delay_ms));
                let _ = write_message(
                    &output,
                    response(id, json_object([("delayed", Json::Bool(true))])),
                );
            });
        }
        "mock/cancelled" => {
            let cancelled = !state.lock().unwrap().cancellations.is_empty();
            write_message(
                output,
                response(id, json_object([("cancelled", Json::Bool(cancelled))])),
            )?;
        }
        "mock/probeClient" => {
            let report = probe_client(output, input, state)?;
            write_message(output, response(id, report))?;
        }
        "mock/documentEvents" => {
            let state = state.lock().unwrap();
            let report = json_object([
                ("events", Json::Array(state.document_events.clone())),
                (
                    "open_documents",
                    Json::Number(state.open_documents.len() as i64),
                ),
            ]);
            drop(state);
            write_message(output, response(id, report))?;
        }
        "textDocument/hover" => {
            if mode == "hover-timeout" {
                return Ok(());
            }
            if mode == "hover-exit" {
                return Err(io::Error::other(
                    "mock language server exited during hover",
                )
                .into());
            }
            if mode == "hover-error" {
                write_message(
                    output,
                    error_response_with_data(
                        id,
                        -32042,
                        "mock hover failed".to_owned(),
                        json_object([("retry", Json::Bool(false))]),
                    ),
                )?;
                return Ok(());
            }
            if mode == "hover-unsupported" {
                write_message(
                    output,
                    error_response(
                        id,
                        -32601,
                        "hover request bypassed capability gate".to_owned(),
                    ),
                )?;
                return Ok(());
            }

            let expected_character = match mode {
                "hover-utf-16" => 6,
                "hover-utf-32" => 5,
                _ => 8,
            };
            let position = params.get("position");
            let line = position
                .and_then(|position| position.get("line"))
                .and_then(Json::as_i64);
            let character = position
                .and_then(|position| position.get("character"))
                .and_then(Json::as_i64);
            if line != Some(0) || character != Some(expected_character) {
                write_message(
                    output,
                    error_response(
                        id,
                        -32602,
                        format!(
                            "expected hover position 0:{expected_character}, got {line:?}:{character:?}"
                        ),
                    ),
                )?;
                return Ok(());
            }

            write_message(
                output,
                response(
                    id,
                    json_object([
                        (
                            "contents",
                            json_object([
                                ("kind", Json::String("markdown".to_owned())),
                                (
                                    "value",
                                    Json::String(
                                        "`answer`: the ultimate value".to_owned(),
                                    ),
                                ),
                            ]),
                        ),
                        (
                            "range",
                            json_object([
                                (
                                    "start",
                                    json_object([
                                        ("line", Json::Number(0)),
                                        (
                                            "character",
                                            Json::Number(expected_character),
                                        ),
                                    ]),
                                ),
                                (
                                    "end",
                                    json_object([
                                        ("line", Json::Number(0)),
                                        (
                                            "character",
                                            Json::Number(expected_character + 6),
                                        ),
                                    ]),
                                ),
                            ]),
                        ),
                    ]),
                ),
            )?;
        }
        "textDocument/declaration"
        | "textDocument/definition"
        | "textDocument/implementation"
        | "textDocument/typeDefinition" => {
            let operation = match method {
                "textDocument/declaration" => "declaration",
                "textDocument/definition" => "definition",
                "textDocument/implementation" => "implementation",
                "textDocument/typeDefinition" => "type-definition",
                _ => unreachable!(),
            };
            if mode == format!("{operation}-unsupported") {
                write_message(
                    output,
                    error_response(
                        id,
                        -32601,
                        format!("{operation} request bypassed capability gate"),
                    ),
                )?;
                return Ok(());
            }

            let expected_character = if mode.ends_with("utf-16") {
                6
            } else if mode.ends_with("utf-32") {
                5
            } else {
                8
            };
            let position = params.get("position");
            let line = position
                .and_then(|position| position.get("line"))
                .and_then(Json::as_i64);
            let character = position
                .and_then(|position| position.get("character"))
                .and_then(Json::as_i64);
            if line != Some(0) || character != Some(expected_character) {
                write_message(
                    output,
                    error_response(
                        id,
                        -32602,
                        format!(
                            "expected {operation} position 0:{expected_character}, got {line:?}:{character:?}"
                        ),
                    ),
                )?;
                return Ok(());
            }

            let source_uri = params
                .get("textDocument")
                .and_then(|document| document.get("uri"))
                .and_then(Json::as_str)
                .unwrap_or_default();
            let target_uri = match mode {
                "definition-project-target-utf-16" => source_uri
                    .strip_suffix("main.rs")
                    .map(|prefix| format!("{prefix}helper.rs"))
                    .unwrap_or_else(|| source_uri.to_owned()),
                "definition-external-utf-16" => {
                    "file:///deixis-external-do-not-read.rs".to_owned()
                }
                _ => source_uri.to_owned(),
            };
            let selection_range = json_object([
                (
                    "start",
                    json_object([
                        ("line", Json::Number(0)),
                        ("character", Json::Number(expected_character)),
                    ]),
                ),
                (
                    "end",
                    json_object([
                        ("line", Json::Number(0)),
                        ("character", Json::Number(expected_character + 6)),
                    ]),
                ),
            ]);
            let location = json_object([
                ("uri", Json::String(target_uri.clone())),
                ("range", selection_range.clone()),
            ]);
            let result = match mode {
                mode if mode.contains("-locations-") => {
                    Json::Array(vec![location])
                }
                mode if mode.contains("-links-") => Json::Array(vec![json_object([
                    ("targetUri", Json::String(target_uri)),
                    ("targetRange", selection_range.clone()),
                    ("targetSelectionRange", selection_range),
                ])]),
                "definition-project-target-utf-16"
                | "definition-external-utf-16" => {
                    Json::Array(vec![json_object([
                        (
                            "originSelectionRange",
                            selection_range.clone(),
                        ),
                        ("targetUri", Json::String(target_uri)),
                        ("targetRange", selection_range.clone()),
                        ("targetSelectionRange", selection_range),
                    ])])
                }
                mode if mode.ends_with("-null") => Json::Null,
                _ => location,
            };
            write_message(output, response(id, result))?;
        }
        "shutdown" => {
            if mode == "ignore-shutdown" {
                loop {
                    thread::sleep(Duration::from_secs(60));
                }
            }
            let mut state = state.lock().unwrap();
            if !state.open_documents.is_empty() {
                drop(state);
                write_message(
                    output,
                    error_response(
                        id,
                        -32000,
                        "documents remained open during shutdown".to_owned(),
                    ),
                )?;
                return Ok(());
            }
            state.shutdown_requested = true;
            drop(state);
            write_message(output, response(id, Json::Null))?;
        }
        _ => {
            write_message(
                output,
                error_response(id, -32601, format!("unknown method `{method}`")),
            )?;
        }
    }

    Ok(())
}

fn handle_notification(
    mode: &str,
    state: &Arc<Mutex<MockState>>,
    method: &str,
    params: Json,
) {
    match method {
        "initialized" => {
            state.lock().unwrap().initialized = true;
        }
        "$/cancelRequest" => {
            if let Some(id) = params.get("id").cloned() {
                state.lock().unwrap().cancellations.insert(id.to_string());
            }
        }
        "textDocument/didOpen" => {
            let mut state = state.lock().unwrap();
            if let Some(uri) = params
                .get("textDocument")
                .and_then(|document| document.get("uri"))
                .and_then(Json::as_str)
            {
                state.open_documents.insert(uri.to_owned());
            }
            state.document_events.push(notification(method, params));
        }
        "textDocument/didChange" => {
            state
                .lock()
                .unwrap()
                .document_events
                .push(notification(method, params));
        }
        "textDocument/didClose" => {
            let mut state = state.lock().unwrap();
            if let Some(uri) = params
                .get("textDocument")
                .and_then(|document| document.get("uri"))
                .and_then(Json::as_str)
            {
                state.open_documents.remove(uri);
            }
            state.document_events.push(notification(method, params));
        }
        "exit" if mode == "ignore-shutdown" => loop {
            thread::sleep(Duration::from_secs(60));
        },
        _ => {}
    }
}

fn probe_client<R: BufRead>(
    output: &Arc<Mutex<io::Stdout>>,
    input: &mut R,
    state: Arc<Mutex<MockState>>,
) -> Result<Json, Box<dyn Error>> {
    write_message(
        output,
        request(
            10,
            "workspace/configuration",
            json_object([(
                "items",
                Json::Array(vec![
                    json_object([(
                        "section",
                        Json::String("mock.one".to_owned()),
                    )]),
                    json_object([(
                        "section",
                        Json::String("mock.two".to_owned()),
                    )]),
                    json_object([(
                        "section",
                        Json::String("mock.missing".to_owned()),
                    )]),
                    json_object([]),
                ]),
            )]),
        ),
    )?;
    let configuration = read_response(input, 10)?;

    write_message(
        output,
        request(11, "workspace/workspaceFolders", Json::Null),
    )?;
    let workspace_folders = read_response(input, 11)?;

    write_message(
        output,
        request(
            12,
            "client/registerCapability",
            json_object([(
                "registrations",
                Json::Array(vec![json_object([
                    ("id", Json::String("mock-registration".to_owned())),
                    (
                        "method",
                        Json::String("textDocument/hover".to_owned()),
                    ),
                ])]),
            )]),
        ),
    )?;
    let registration = read_response(input, 12)?;

    write_message(
        output,
        request(
            13,
            "client/unregisterCapability",
            json_object([(
                "unregisterations",
                Json::Array(vec![json_object([
                    ("id", Json::String("mock-registration".to_owned())),
                    (
                        "method",
                        Json::String("textDocument/hover".to_owned()),
                    ),
                ])]),
            )]),
        ),
    )?;
    let unregistration = read_response(input, 13)?;

    write_message(
        output,
        request(14, "workspace/applyEdit", json_object([])),
    )?;
    let apply_edit = read_response(input, 14)?;

    write_message(
        output,
        request(
            15,
            "window/showMessageRequest",
            json_object([
                ("type", Json::Number(3)),
                (
                    "message",
                    Json::String("choose an action from mock".to_owned()),
                ),
                (
                    "actions",
                    Json::Array(vec![json_object([(
                        "title",
                        Json::String("Ignore".to_owned()),
                    )])]),
                ),
            ]),
        ),
    )?;
    let show_message = read_response(input, 15)?;

    write_message(output, request(16, "mock/unknownClientRequest", Json::Null))?;
    let unknown = read_response(input, 16)?;

    write_message(
        output,
        notification(
            "window/logMessage",
            json_object([("message", Json::String("hello from mock".to_owned()))]),
        ),
    )?;
    write_message(
        output,
        notification(
            "window/showMessage",
            json_object([("message", Json::String("show from mock".to_owned()))]),
        ),
    )?;
    write_message(
        output,
        notification(
            "window/logTrace",
            json_object([("message", Json::String("trace from mock".to_owned()))]),
        ),
    )?;
    write_message(
        output,
        notification(
            "textDocument/publishDiagnostics",
            json_object([
                ("uri", Json::String("file:///mock.rs".to_owned())),
                ("diagnostics", Json::Array(Vec::new())),
            ]),
        ),
    )?;

    let (
        position_encodings,
        hover_dynamic_registration,
        hover_content_formats,
        definition_dynamic_registration,
        definition_link_support,
        declaration_dynamic_registration,
        declaration_link_support,
        type_definition_dynamic_registration,
        type_definition_link_support,
        implementation_dynamic_registration,
        implementation_link_support,
    ) = {
        let mut state = state.lock().unwrap();
        state.client_probe_complete = true;
        let capabilities = state
            .initialize_params
            .as_ref()
            .and_then(|params| params.get("capabilities"))
            .cloned()
            .unwrap_or(Json::Null);
        let position_encodings = capabilities
            .get("general")
            .and_then(|general| general.get("positionEncodings"))
            .cloned()
            .unwrap_or(Json::Null);
        let hover = capabilities
            .get("textDocument")
            .and_then(|text_document| text_document.get("hover"));
        let dynamic_registration = hover
            .and_then(|hover| hover.get("dynamicRegistration"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let content_formats = hover
            .and_then(|hover| hover.get("contentFormat"))
            .cloned()
            .unwrap_or(Json::Null);
        let definition = capabilities
            .get("textDocument")
            .and_then(|text_document| text_document.get("definition"));
        let definition_dynamic_registration = definition
            .and_then(|definition| definition.get("dynamicRegistration"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let definition_link_support = definition
            .and_then(|definition| definition.get("linkSupport"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let declaration = capabilities
            .get("textDocument")
            .and_then(|text_document| text_document.get("declaration"));
        let declaration_dynamic_registration = declaration
            .and_then(|declaration| declaration.get("dynamicRegistration"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let declaration_link_support = declaration
            .and_then(|declaration| declaration.get("linkSupport"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let type_definition = capabilities
            .get("textDocument")
            .and_then(|text_document| text_document.get("typeDefinition"));
        let type_definition_dynamic_registration = type_definition
            .and_then(|type_definition| {
                type_definition.get("dynamicRegistration")
            })
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let type_definition_link_support = type_definition
            .and_then(|type_definition| type_definition.get("linkSupport"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let implementation = capabilities
            .get("textDocument")
            .and_then(|text_document| text_document.get("implementation"));
        let implementation_dynamic_registration = implementation
            .and_then(|implementation| {
                implementation.get("dynamicRegistration")
            })
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let implementation_link_support = implementation
            .and_then(|implementation| implementation.get("linkSupport"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        (
            position_encodings,
            dynamic_registration,
            content_formats,
            definition_dynamic_registration,
            definition_link_support,
            declaration_dynamic_registration,
            declaration_link_support,
            type_definition_dynamic_registration,
            type_definition_link_support,
            implementation_dynamic_registration,
            implementation_link_support,
        )
    };
    Ok(json_object([
        (
            "configuration_values",
            configuration
                .get("result")
                .cloned()
                .unwrap_or(Json::Null),
        ),
        (
            "workspace_folders",
            Json::Number(
                workspace_folders
                    .get("result")
                    .and_then(Json::as_array)
                    .map(Vec::len)
                    .unwrap_or(0) as i64,
            ),
        ),
        (
            "workspace_folder_uri",
            workspace_folders
                .get("result")
                .and_then(Json::as_array)
                .and_then(|folders| folders.first())
                .and_then(|folder| folder.get("uri"))
                .cloned()
                .unwrap_or(Json::Null),
        ),
        (
            "workspace_folder_name",
            workspace_folders
                .get("result")
                .and_then(Json::as_array)
                .and_then(|folders| folders.first())
                .and_then(|folder| folder.get("name"))
                .cloned()
                .unwrap_or(Json::Null),
        ),
        (
            "registered",
            Json::Bool(registration.get("result") == Some(&Json::Null)),
        ),
        (
            "unregistered",
            Json::Bool(unregistration.get("result") == Some(&Json::Null)),
        ),
        (
            "apply_edit_applied",
            Json::Bool(
                apply_edit
                    .get("result")
                    .and_then(|value| value.get("applied"))
                    .and_then(Json::as_bool)
                    .unwrap_or(true),
            ),
        ),
        (
            "apply_edit_failure_reason",
            apply_edit
                .get("result")
                .and_then(|value| value.get("failureReason"))
                .cloned()
                .unwrap_or(Json::Null),
        ),
        (
            "show_message_request_result",
            show_message.get("result").cloned().unwrap_or(Json::Null),
        ),
        (
            "unknown_error_code",
            Json::Number(
                unknown
                    .get("error")
                    .and_then(|value| value.get("code"))
                    .and_then(Json::as_i64)
                    .unwrap_or(0),
            ),
        ),
        ("position_encodings", position_encodings),
        (
            "hover_dynamic_registration",
            Json::Bool(hover_dynamic_registration),
        ),
        ("hover_content_formats", hover_content_formats),
        (
            "definition_dynamic_registration",
            Json::Bool(definition_dynamic_registration),
        ),
        (
            "definition_link_support",
            Json::Bool(definition_link_support),
        ),
        (
            "declaration_dynamic_registration",
            Json::Bool(declaration_dynamic_registration),
        ),
        (
            "declaration_link_support",
            Json::Bool(declaration_link_support),
        ),
        (
            "type_definition_dynamic_registration",
            Json::Bool(type_definition_dynamic_registration),
        ),
        (
            "type_definition_link_support",
            Json::Bool(type_definition_link_support),
        ),
        (
            "implementation_dynamic_registration",
            Json::Bool(implementation_dynamic_registration),
        ),
        (
            "implementation_link_support",
            Json::Bool(implementation_link_support),
        ),
    ]))
}

fn read_response<R: BufRead>(
    input: &mut R,
    expected_id: i64,
) -> Result<Json, Box<dyn Error>> {
    loop {
        let message =
            read_message(input)?.ok_or_else(|| ProtocolError("unexpected EOF"))?;
        if message.get("id").and_then(Json::as_i64) == Some(expected_id)
            && message.get("method").is_none()
        {
            return Ok(message);
        }
        if message.get("method").and_then(Json::as_str)
            == Some("$/cancelRequest")
        {
            continue;
        }
    }
}

fn request(id: i64, method: &str, params: Json) -> Json {
    json_object([
        ("jsonrpc", Json::String(JSONRPC_VERSION.to_owned())),
        ("id", Json::Number(id)),
        ("method", Json::String(method.to_owned())),
        ("params", params),
    ])
}

fn response(id: Json, result: Json) -> Json {
    json_object([
        ("jsonrpc", Json::String(JSONRPC_VERSION.to_owned())),
        ("id", id),
        ("result", result),
    ])
}

fn error_response(id: Json, code: i64, message: String) -> Json {
    json_object([
        ("jsonrpc", Json::String(JSONRPC_VERSION.to_owned())),
        ("id", id),
        (
            "error",
            json_object([
                ("code", Json::Number(code)),
                ("message", Json::String(message)),
            ]),
        ),
    ])
}

fn error_response_with_data(
    id: Json,
    code: i64,
    message: String,
    data: Json,
) -> Json {
    json_object([
        ("jsonrpc", Json::String(JSONRPC_VERSION.to_owned())),
        ("id", id),
        (
            "error",
            json_object([
                ("code", Json::Number(code)),
                ("message", Json::String(message)),
                ("data", data),
            ]),
        ),
    ])
}

fn notification(method: &str, params: Json) -> Json {
    json_object([
        ("jsonrpc", Json::String(JSONRPC_VERSION.to_owned())),
        ("method", Json::String(method.to_owned())),
        ("params", params),
    ])
}

fn json_object<const N: usize>(entries: [(&str, Json); N]) -> Json {
    Json::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn write_message(
    output: &Arc<Mutex<io::Stdout>>,
    message: Json,
) -> Result<(), Box<dyn Error>> {
    let body = message.to_string();
    write_raw_message(output, body.as_bytes())
}

fn write_raw_message(
    output: &Arc<Mutex<io::Stdout>>,
    body: &[u8],
) -> Result<(), Box<dyn Error>> {
    let mut output = output.lock().unwrap();
    write!(
        output,
        "Content-Length: {}\r\n\r\n{}",
        body.len(),
        String::from_utf8_lossy(body),
    )?;
    output.flush()?;
    Ok(())
}

fn read_message<R: BufRead>(input: &mut R) -> Result<Option<Json>, Box<dyn Error>> {
    let mut content_length = None;
    let mut line = String::new();

    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse::<usize>()?);
        }
    }

    let mut body = vec![0; content_length.ok_or(ProtocolError("missing length"))?];
    input.read_exact(&mut body)?;
    Json::parse(std::str::from_utf8(&body)?).map(Some).map_err(Into::into)
}

#[derive(Debug, Default)]
struct MockState {
    initialized: bool,
    shutdown_requested: bool,
    client_probe_complete: bool,
    initialize_params: Option<Json>,
    cancellations: BTreeSet<String>,
    document_events: Vec<Json>,
    open_documents: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Json {
    Null,
    Bool(bool),
    Number(i64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        let Json::Object(entries) = self else {
            return None;
        };
        entries
            .iter()
            .find(|(entry_key, _)| entry_key == key)
            .map(|(_, value)| value)
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }

    fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(value) => (*value).try_into().ok(),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&Vec<Json>> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    fn parse(input: &str) -> Result<Self, JsonParseError> {
        Parser::new(input).parse()
    }
}

impl fmt::Display for Json {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => formatter.write_str("null"),
            Self::Bool(true) => formatter.write_str("true"),
            Self::Bool(false) => formatter.write_str("false"),
            Self::Number(value) => write!(formatter, "{value}"),
            Self::String(value) => write_json_string(formatter, value),
            Self::Array(values) => {
                formatter.write_str("[")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(",")?;
                    }
                    write!(formatter, "{value}")?;
                }
                formatter.write_str("]")
            }
            Self::Object(entries) => {
                formatter.write_str("{")?;
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(",")?;
                    }
                    write_json_string(formatter, key)?;
                    formatter.write_str(":")?;
                    write!(formatter, "{value}")?;
                }
                formatter.write_str("}")
            }
        }
    }
}

fn write_json_string(
    formatter: &mut fmt::Formatter<'_>,
    value: &str,
) -> fmt::Result {
    formatter.write_str("\"")?;
    for character in value.chars() {
        match character {
            '"' => formatter.write_str("\\\"")?,
            '\\' => formatter.write_str("\\\\")?,
            '\n' => formatter.write_str("\\n")?,
            '\r' => formatter.write_str("\\r")?,
            '\t' => formatter.write_str("\\t")?,
            c if c.is_control() => write!(formatter, "\\u{:04x}", c as u32)?,
            c => write!(formatter, "{c}")?,
        }
    }
    formatter.write_str("\"")
}

struct Parser<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            position: 0,
        }
    }

    fn parse(mut self) -> Result<Json, JsonParseError> {
        let value = self.parse_value()?;
        self.skip_ws();
        if self.position != self.input.len() {
            return Err(self.error("trailing data"));
        }
        Ok(value)
    }

    fn parse_value(&mut self) -> Result<Json, JsonParseError> {
        self.skip_ws();
        match self.peek() {
            Some(b'n') => self.expect_literal(b"null", Json::Null),
            Some(b't') => self.expect_literal(b"true", Json::Bool(true)),
            Some(b'f') => self.expect_literal(b"false", Json::Bool(false)),
            Some(b'"') => self.parse_string().map(Json::String),
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_object(),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            _ => Err(self.error("expected JSON value")),
        }
    }

    fn parse_object(&mut self) -> Result<Json, JsonParseError> {
        self.consume(b'{')?;
        let mut entries = Vec::new();
        loop {
            self.skip_ws();
            if self.take_if(b'}') {
                break;
            }
            let key = self.parse_string()?;
            self.skip_ws();
            self.consume(b':')?;
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            if self.take_if(b'}') {
                break;
            }
            self.consume(b',')?;
        }
        Ok(Json::Object(entries))
    }

    fn parse_array(&mut self) -> Result<Json, JsonParseError> {
        self.consume(b'[')?;
        let mut values = Vec::new();
        loop {
            self.skip_ws();
            if self.take_if(b']') {
                break;
            }
            values.push(self.parse_value()?);
            self.skip_ws();
            if self.take_if(b']') {
                break;
            }
            self.consume(b',')?;
        }
        Ok(Json::Array(values))
    }

    fn parse_string(&mut self) -> Result<String, JsonParseError> {
        self.consume(b'"')?;
        let mut value = String::new();
        while let Some(byte) = self.next() {
            match byte {
                b'"' => return Ok(value),
                b'\\' => {
                    let escaped = self
                        .next()
                        .ok_or_else(|| self.error("unterminated escape"))?;
                    match escaped {
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'/' => value.push('/'),
                        b'b' => value.push('\u{0008}'),
                        b'f' => value.push('\u{000c}'),
                        b'n' => value.push('\n'),
                        b'r' => value.push('\r'),
                        b't' => value.push('\t'),
                        b'u' => value.push(self.parse_unicode_escape()?),
                        _ => return Err(self.error("invalid escape")),
                    }
                }
                other => value.push(other as char),
            }
        }
        Err(self.error("unterminated string"))
    }

    fn parse_unicode_escape(&mut self) -> Result<char, JsonParseError> {
        let mut value = 0;
        for _ in 0..4 {
            value *= 16;
            value += match self.next() {
                Some(byte @ b'0'..=b'9') => u32::from(byte - b'0'),
                Some(byte @ b'a'..=b'f') => u32::from(byte - b'a' + 10),
                Some(byte @ b'A'..=b'F') => u32::from(byte - b'A' + 10),
                _ => return Err(self.error("invalid unicode escape")),
            };
        }
        char::from_u32(value).ok_or_else(|| self.error("invalid unicode scalar"))
    }

    fn parse_number(&mut self) -> Result<Json, JsonParseError> {
        let start = self.position;
        if self.take_if(b'-') {
            if self.peek().is_none() {
                return Err(self.error("invalid number"));
            }
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        let value = std::str::from_utf8(&self.input[start..self.position])
            .map_err(|_| self.error("invalid number"))?
            .parse::<i64>()
            .map_err(|_| self.error("invalid number"))?;
        Ok(Json::Number(value))
    }

    fn expect_literal(
        &mut self,
        literal: &[u8],
        value: Json,
    ) -> Result<Json, JsonParseError> {
        if self.input.get(self.position..self.position + literal.len())
            == Some(literal)
        {
            self.position += literal.len();
            Ok(value)
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn consume(&mut self, expected: u8) -> Result<(), JsonParseError> {
        if self.take_if(expected) {
            Ok(())
        } else {
            Err(self.error("unexpected character"))
        }
    }

    fn take_if(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.position += 1;
        Some(byte)
    }

    fn error(&self, message: &'static str) -> JsonParseError {
        JsonParseError {
            message,
            position: self.position,
        }
    }
}

#[derive(Debug)]
struct JsonParseError {
    message: &'static str,
    position: usize,
}

impl fmt::Display for JsonParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at byte {}", self.message, self.position)
    }
}

impl Error for JsonParseError {}

#[derive(Debug)]
struct ProtocolError(&'static str);

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for ProtocolError {}
