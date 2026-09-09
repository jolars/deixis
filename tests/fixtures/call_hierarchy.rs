use super::*;

fn start(mode: &str, other: bool) -> i64 {
    match (mode.rsplit('-').next(), other) {
        (Some("16"), false) => 6,
        (Some("32"), false) => 5,
        (Some("16"), true) => 2,
        (Some("32"), true) => 1,
        (_, false) => 8,
        (_, true) => 4,
    }
}

fn item(uri: &str, name: &str, start: i64) -> Json {
    json_object([
        ("name", Json::String(name.to_owned())),
        ("kind", Json::Number(12)),
        ("uri", Json::String(uri.to_owned())),
        ("range", mock_range(0, start + 6)),
        ("selectionRange", mock_range(start, start + 6)),
        ("tags", Json::Array(vec![Json::Number(1)])),
        ("detail", Json::String("fn answer()".to_owned())),
        (
            "data",
            json_object([(
                "opaque",
                Json::Array(vec![
                    Json::Number(1),
                    json_object([("token", Json::String("🦀".to_owned()))]),
                ]),
            )]),
        ),
        ("server", Json::String("spoof".to_owned())),
        ("positionEncoding", Json::String("spoof".to_owned())),
    ])
}

pub(super) fn handle_request<R: BufRead>(
    mode: &str,
    output: &Arc<Mutex<io::Stdout>>,
    input: &mut R,
    state: &Arc<Mutex<MockState>>,
    id: Json,
    method: &str,
    params: Json,
) -> Result<(), Box<dyn Error>> {
    if matches!(mode, "call-hierarchy-unsupported" | "call-hierarchy-absent") {
        return write_message(
            output,
            error_response(
                id,
                -32601,
                "call hierarchy bypassed capability gate".to_owned(),
            ),
        );
    }
    let attempt = {
        let mut state = state.lock().unwrap();
        let attempt = state
            .hierarchy_attempts
            .entry(method.to_owned())
            .or_default();
        *attempt += 1;
        *attempt
    };
    if mode == "call-hierarchy-retry" && attempt == 1 {
        return write_message(
            output,
            error_response(id, -32802, "retry hierarchy".to_owned()),
        );
    }
    let prepare = method == "textDocument/prepareCallHierarchy";
    if (prepare && mode.ends_with("prepare-timeout"))
        || (!prepare && mode.ends_with("calls-timeout"))
    {
        return Ok(());
    }
    if mode == "call-hierarchy-deadline" {
        thread::sleep(Duration::from_millis(300));
    }
    if (prepare && mode == "call-hierarchy-cancel-prepare")
        || (!prepare && mode == "call-hierarchy-cancel-calls")
    {
        return write_message(
            output,
            notification(
                "experimental/serverStatus",
                json_object([
                    ("health", Json::String("ok".to_owned())),
                    ("quiescent", Json::Bool(false)),
                    ("message", Json::String("hierarchy pending".to_owned())),
                ]),
            ),
        );
    }
    let source_uri = state
        .lock()
        .unwrap()
        .open_documents
        .iter()
        .next()
        .cloned()
        .unwrap_or_default();
    if method == "textDocument/prepareCallHierarchy" {
        let state = state.lock().unwrap();
        let initialize = state.initialize_params.as_ref().unwrap();
        assert_eq!(
            initialize
                .get("capabilities")
                .unwrap()
                .get("textDocument")
                .unwrap()
                .get("callHierarchy")
                .unwrap()
                .get("dynamicRegistration")
                .and_then(Json::as_bool),
            Some(true)
        );
        if mode == "call-hierarchy-sync" && attempt == 2 {
            assert_eq!(
                state.document_texts.get(&source_uri).unwrap(),
                "var 🦀answer = 42;\n"
            );
        }
        drop(state);
        let position = params.get("position").unwrap();
        assert_eq!(position.get("line").and_then(Json::as_i64), Some(0));
        assert_eq!(
            position.get("character").and_then(Json::as_i64),
            Some(start(mode, false))
        );
        assert_eq!(
            params
                .get("textDocument")
                .unwrap()
                .get("uri")
                .and_then(Json::as_str),
            Some(source_uri.as_str())
        );
        if mode == "call-hierarchy-dynamic-unregister" {
            register(output, input, false)?;
        }
        let result = match mode {
            "call-hierarchy-prepare-null" => Json::Null,
            "call-hierarchy-prepare-empty" => Json::Array(vec![]),
            "call-hierarchy-malformed-prepare" => {
                Json::Array(vec![json_object([])])
            }
            _ => Json::Array(
                (0..if mode == "call-hierarchy-budgets" {
                    2
                } else {
                    1
                })
                    .map(|index| {
                        item(
                            &source_uri,
                            &format!("selected{index}"),
                            start(mode, false),
                        )
                    })
                    .collect(),
            ),
        };
        return write_message(output, response(id, result));
    }
    let prepared = params.get("item").unwrap();
    let name = prepared.get("name").and_then(Json::as_str).unwrap();
    // Exact equality catches lost opaque data and premature position normalization.
    assert_eq!(
        canonical(prepared.clone()),
        canonical(item(&source_uri, name, start(mode, false)))
    );
    if mode == "call-hierarchy-error" {
        return write_message(
            output,
            error_response(id, -32603, "call hierarchy failed".to_owned()),
        );
    }
    let other_uri = if mode == "call-hierarchy-outside-utf-16" {
        format!("{}.outside.rs", source_uri.rsplit_once('/').unwrap().0)
    } else {
        format!("{}/other.rs", source_uri.rsplit_once('/').unwrap().0)
    };
    let incoming = method == "callHierarchy/incomingCalls";
    let result = match mode {
        "call-hierarchy-calls-null" => Json::Null,
        "call-hierarchy-calls-empty" => Json::Array(vec![]),
        "call-hierarchy-malformed-calls" => Json::Array(vec![json_object([])]),
        _ => Json::Array(
            (0..if mode == "call-hierarchy-budgets" {
                205
            } else if mode == "call-hierarchy-oversized" {
                3
            } else {
                2
            })
                .map(|index| {
                    let (uri, target_start) =
                        if index == 0 && mode != "call-hierarchy-budgets" {
                            (other_uri.clone(), start(mode, true))
                        } else {
                            ("mock:///external".to_owned(), 13)
                        };
                    let caller_start = if incoming {
                        target_start
                    } else {
                        start(mode, false)
                    };
                    let mut target =
                        item(&uri, &format!("call{index}"), target_start);
                    if mode == "call-hierarchy-oversized" && index == 0 {
                        let Json::Object(fields) = &mut target else {
                            unreachable!()
                        };
                        fields.push((
                            "extension".to_owned(),
                            Json::String("\"🦀".repeat(20000)),
                        ));
                    }
                    let ranges = if mode == "call-hierarchy-oversized"
                        && index == 1
                    {
                        vec![mock_range(caller_start, caller_start + 2); 10000]
                    } else {
                        vec![
                            mock_range(caller_start, caller_start + 6),
                            mock_range(caller_start, caller_start + 2),
                        ]
                    };
                    json_object([
                        (if incoming { "from" } else { "to" }, target),
                        ("fromRanges", Json::Array(ranges)),
                    ])
                })
                .collect(),
        ),
    };
    write_message(output, response(id, result))
}

fn canonical(value: Json) -> Json {
    match value {
        Json::Object(fields) => {
            let mut fields: Vec<_> = fields
                .into_iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            Json::Object(fields)
        }
        Json::Array(values) => {
            Json::Array(values.into_iter().map(canonical).collect())
        }
        value => value,
    }
}

pub(super) fn register<R: BufRead>(
    output: &Arc<Mutex<io::Stdout>>,
    input: &mut R,
    enabled: bool,
) -> Result<(), Box<dyn Error>> {
    let (id, method, field) = if enabled {
        (90, "client/registerCapability", "registrations")
    } else {
        (91, "client/unregisterCapability", "unregisterations")
    };
    write_message(
        output,
        request(
            id,
            method,
            json_object([(
                field,
                Json::Array(vec![json_object([
                    ("id", Json::String("hierarchy".to_owned())),
                    (
                        "method",
                        Json::String(
                            "textDocument/prepareCallHierarchy".to_owned(),
                        ),
                    ),
                ])]),
            )]),
        ),
    )?;
    let response = read_response(input, id)?;
    assert!(response.get("error").is_none());
    Ok(())
}
