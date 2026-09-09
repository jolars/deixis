use super::*;

async fn client_for(
    mode: &str,
    request_ms: u64,
) -> Result<
    (PathBuf, rmcp::service::RunningService<rmcp::RoleClient, ()>),
    Box<dyn Error>,
> {
    let root = unique_dir(mode)?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
    fs::write(root.join("other.rs"), "🦀answer = 42;\n")?;
    let config =
        write_mock_config_for_mode_with_timeout(&root, mode, request_ms)?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command.arg("--root").arg(&root).arg("--config").arg(config);
    let client = timeout(
        Duration::from_secs(10),
        ().serve(TokioChildProcess::new(command)?),
    )
    .await??;
    Ok((root, client))
}

fn arguments() -> JsonValue {
    json!({ "path": "main.rs", "position": { "line": 0, "character": 8 } })
}

#[tokio::test]
async fn call_hierarchy_preserves_items_and_converts_caller_ranges()
-> Result<(), Box<dyn Error>> {
    for encoding in ["utf-8", "utf-16", "utf-32"] {
        let (root, client) =
            client_for(&format!("call-hierarchy-{encoding}"), 1000).await?;
        let tools = client.list_tools(None).await?;
        for tool in ["incoming_calls", "outgoing_calls"] {
            let schema = tools
                .tools
                .iter()
                .find(|entry| entry.name == tool)
                .expect("call hierarchy tool");
            assert_eq!(
                schema.input_schema["required"],
                json!(["path", "position"])
            );
            assert_eq!(
                schema.input_schema["properties"]["limit"]["default"],
                100
            );
            assert_eq!(
                schema.input_schema["properties"]["limit"]["maximum"],
                500
            );
            assert_eq!(
                schema.annotations.as_ref().unwrap().read_only_hint,
                Some(true)
            );
            assert!(schema.output_schema.is_some());
            let result =
                client
                    .call_tool(CallToolRequestParams::new(tool).with_arguments(
                        arguments().as_object().unwrap().clone(),
                    ))
                    .await?;
            assert_eq!(result.is_error, Some(false), "{result:?}");
            let structured = result.structured_content.unwrap();
            assert_eq!(structured["pagination"]["total"], 2);
            let calls = structured["calls"].as_array().unwrap();
            let (selected, other) = if tool == "incoming_calls" {
                ("to", "from")
            } else {
                ("from", "to")
            };
            assert_eq!(calls[0][selected]["name"], "selected0");
            assert_eq!(calls[0][selected]["range"]["end"]["character"], 14);
            assert_eq!(
                calls[0][selected]["selectionRange"]["start"]["character"],
                8
            );
            assert_eq!(
                calls[0][selected]["uri"],
                url::Url::from_file_path(root.join("main.rs"))
                    .unwrap()
                    .as_str()
            );
            assert_eq!(
                calls[0][other]["selectionRange"]["start"]["character"],
                4
            );
            for item in [&calls[0][selected], &calls[0][other]] {
                assert_eq!(item["server"], "mock-lsp");
                assert_eq!(item["positionEncoding"], "utf-8");
                assert_eq!(item["tags"], json!([1]));
                assert_eq!(
                    item["data"]["opaque"],
                    json!([1, { "token": "🦀" }])
                );
                assert_eq!(item["detail"], "fn answer()");
            }
            assert_eq!(calls[1][other]["uri"], "mock:///external");
            assert_eq!(calls[1][other]["positionEncoding"], encoding);
            assert_eq!(
                calls[1][other]["selectionRange"]["start"]["character"],
                13
            );
            let starts = if tool == "incoming_calls" {
                [4, 13]
            } else {
                [8, 8]
            };
            for (call, start) in calls.iter().zip(starts) {
                assert_eq!(call["fromRanges"][0]["start"]["character"], start);
                assert_eq!(
                    call["fromRanges"][1]["end"]["character"],
                    start + 2
                );
            }
        }
        timeout(Duration::from_secs(10), client.cancel()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn call_hierarchy_gates_capabilities_and_handles_empty_or_invalid_responses()
-> Result<(), Box<dyn Error>> {
    for (mode, error_code, method) in [
        (
            "unsupported",
            Some("unsupported_capability"),
            "textDocument/prepareCallHierarchy",
        ),
        (
            "absent",
            Some("unsupported_capability"),
            "textDocument/prepareCallHierarchy",
        ),
        ("prepare-null", None, ""),
        ("prepare-empty", None, ""),
        ("calls-null", None, ""),
        ("calls-empty", None, ""),
        ("malformed-prepare", Some("lsp_protocol_error"), ""),
        ("malformed-calls", Some("lsp_protocol_error"), ""),
        ("error", Some("lsp_error"), ""),
    ] {
        let (_, client) =
            client_for(&format!("call-hierarchy-{mode}"), 1000).await?;
        for tool in ["incoming_calls", "outgoing_calls"] {
            let result =
                client
                    .call_tool(CallToolRequestParams::new(tool).with_arguments(
                        arguments().as_object().unwrap().clone(),
                    ))
                    .await?;
            let structured = result.structured_content.unwrap();
            if let Some(code) = error_code {
                assert_eq!(result.is_error, Some(true));
                assert_eq!(
                    structured["error"]["code"], code,
                    "{mode}: {structured}"
                );
                assert_eq!(structured["error"]["tool"], tool);
                if !method.is_empty() {
                    assert_eq!(structured["error"]["method"], method);
                }
            } else {
                assert_eq!(
                    result.is_error,
                    Some(false),
                    "{mode}: {structured}"
                );
                assert_eq!(structured["calls"], json!([]));
                assert_eq!(structured["pagination"]["total"], 0);
                assert!(structured["readiness"].is_object());
                assert!(structured["resultStability"].is_string());
            }
        }
        timeout(Duration::from_secs(10), client.cancel()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn call_hierarchy_uses_reference_budgets_across_prepared_items()
-> Result<(), Box<dyn Error>> {
    let (_, client) = client_for("call-hierarchy-budgets", 1000).await?;
    for tool in ["incoming_calls", "outgoing_calls"] {
        for invalid in [
            json!({"limit": 0}),
            json!({"limit": 501}),
            json!({"limit": -1}),
            json!({"limit": 1.5}),
            json!({"offset": -1}),
            json!({"offset": null}),
            json!({"unknown": 1}),
            json!({"path": ""}),
            json!({"server": " "}),
        ] {
            let mut args = arguments().as_object().unwrap().clone();
            args.extend(invalid.as_object().unwrap().clone());
            let error = client
                .call_tool(
                    CallToolRequestParams::new(tool).with_arguments(args),
                )
                .await
                .expect_err("invalid arguments");
            assert!(error.to_string().contains("32602"), "{error}");
        }
        let mut offset = 0;
        let mut byte_limited = false;
        let mut received = Vec::new();
        loop {
            let mut args = arguments();
            args["offset"] = json!(offset);
            let result = client
                .call_tool(
                    CallToolRequestParams::new(tool)
                        .with_arguments(args.as_object().unwrap().clone()),
                )
                .await?;
            assert_eq!(result.is_error, Some(false), "{result:?}");
            assert!(result.content[0].as_text().unwrap().text.len() < 300);
            let structured = result.structured_content.unwrap();
            let page = &structured["pagination"];
            assert_eq!(page["total"], 410);
            assert_eq!(page["maxBytes"], 65536);
            assert!(page["returned"].as_u64().unwrap() <= 100);
            assert!(serde_json::to_vec(&structured["calls"])?.len() <= 65536);
            let selected = if tool == "incoming_calls" {
                "to"
            } else {
                "from"
            };
            let other = if tool == "incoming_calls" {
                "from"
            } else {
                "to"
            };
            received.extend(
                structured["calls"].as_array().unwrap().iter().map(|call| {
                    (
                        call[selected]["name"].clone(),
                        call[other]["name"].clone(),
                    )
                }),
            );
            let Some(next) = page["nextOffset"].as_u64() else {
                break;
            };
            assert!(next > offset);
            byte_limited |= page["returned"].as_u64().unwrap() < 100;
            offset = next;
        }
        let expected: Vec<_> = (0..2)
            .flat_map(|selected| {
                (0..205).map(move |index| {
                    (
                        json!(format!("selected{selected}")),
                        json!(format!("call{index}")),
                    )
                })
            })
            .collect();
        assert_eq!(received, expected);
        assert!(byte_limited, "the fixture must exercise the byte budget");
        for (offset, limit, returned) in [(409, 1, 1), (u64::MAX, 500, 0)] {
            let mut args = arguments();
            args["offset"] = json!(offset);
            args["limit"] = json!(limit);
            let result = client
                .call_tool(
                    CallToolRequestParams::new(tool)
                        .with_arguments(args.as_object().unwrap().clone()),
                )
                .await?;
            let structured = result.structured_content.unwrap();
            assert_eq!(structured["pagination"]["returned"], returned);
            assert_eq!(structured["pagination"]["truncated"], false);
            assert!(structured.get("readiness").is_none());
        }
    }
    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn call_hierarchy_omits_oversized_items_and_call_site_arrays()
-> Result<(), Box<dyn Error>> {
    let (_, client) = client_for("call-hierarchy-oversized", 3000).await?;
    for tool in ["incoming_calls", "outgoing_calls"] {
        let mut args = arguments();
        args["limit"] = json!(2);
        let result = client
            .call_tool(
                CallToolRequestParams::new(tool)
                    .with_arguments(args.as_object().unwrap().clone()),
            )
            .await?;
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["calls"], json!([]));
        assert_eq!(structured["pagination"]["total"], 3);
        assert_eq!(structured["pagination"]["omitted"], json!([0, 1]));
        assert_eq!(structured["pagination"]["nextOffset"], 2);
        assert_eq!(structured["pagination"]["truncated"], true);
        args["offset"] = json!(2);
        let result = client
            .call_tool(
                CallToolRequestParams::new(tool)
                    .with_arguments(args.as_object().unwrap().clone()),
            )
            .await?;
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["pagination"]["returned"], 1);
        assert_eq!(structured["pagination"]["truncated"], false);
    }
    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn call_hierarchy_accepts_options_and_dynamic_registration_and_checks_unregistration()
-> Result<(), Box<dyn Error>> {
    for mode in ["options", "dynamic", "dynamic-unregister", "retry"] {
        for tool in ["incoming_calls", "outgoing_calls"] {
            let (_, client) =
                client_for(&format!("call-hierarchy-{mode}"), 3000).await?;
            if mode.starts_with("dynamic") {
                let hover = client
                    .call_tool(
                        CallToolRequestParams::new("hover").with_arguments(
                            arguments().as_object().unwrap().clone(),
                        ),
                    )
                    .await?;
                assert_eq!(hover.is_error, Some(false), "{hover:?}");
            }
            let result =
                client
                    .call_tool(CallToolRequestParams::new(tool).with_arguments(
                        arguments().as_object().unwrap().clone(),
                    ))
                    .await?;
            let structured = result.structured_content.unwrap();
            if mode == "dynamic-unregister" {
                assert_eq!(result.is_error, Some(true));
                assert_eq!(
                    structured["error"]["code"],
                    "unsupported_capability"
                );
                assert_eq!(
                    structured["error"]["method"],
                    if tool == "incoming_calls" {
                        "callHierarchy/incomingCalls"
                    } else {
                        "callHierarchy/outgoingCalls"
                    }
                );
            } else {
                assert_eq!(
                    result.is_error,
                    Some(false),
                    "{mode}: {structured}"
                );
                assert_eq!(structured["calls"].as_array().unwrap().len(), 2);
            }
            timeout(Duration::from_secs(10), client.cancel()).await??;
        }
    }
    Ok(())
}

#[tokio::test]
async fn call_hierarchy_shares_one_deadline_between_prepare_and_expansion()
-> Result<(), Box<dyn Error>> {
    for mode in ["prepare-timeout", "calls-timeout", "deadline"] {
        for tool in ["incoming_calls", "outgoing_calls"] {
            let (_, client) =
                client_for(&format!("call-hierarchy-{mode}"), 500).await?;
            let result =
                client
                    .call_tool(CallToolRequestParams::new(tool).with_arguments(
                        arguments().as_object().unwrap().clone(),
                    ))
                    .await?;
            assert_eq!(result.is_error, Some(true), "{mode}: {result:?}");
            let structured = result.structured_content.unwrap();
            assert_eq!(structured["error"]["code"], "request_timeout");
            assert_eq!(structured["error"]["timeoutMs"], 500);
            assert_eq!(
                structured["error"]["method"],
                if mode == "prepare-timeout" {
                    "textDocument/prepareCallHierarchy"
                } else if tool == "incoming_calls" {
                    "callHierarchy/incomingCalls"
                } else {
                    "callHierarchy/outgoingCalls"
                }
            );
            timeout(Duration::from_secs(10), client.cancel()).await??;
        }
    }
    Ok(())
}

#[tokio::test]
async fn call_hierarchy_propagates_caller_cancellation_at_both_stages()
-> Result<(), Box<dyn Error>> {
    for mode in ["cancel-prepare", "cancel-calls"] {
        for tool in ["incoming_calls", "outgoing_calls"] {
            let (_, client) =
                client_for(&format!("call-hierarchy-{mode}"), 10000).await?;
            let handle = client
                .send_cancellable_request(
                    ClientRequest::CallToolRequest(Request::new(
                        CallToolRequestParams::new(tool).with_arguments(
                            arguments().as_object().unwrap().clone(),
                        ),
                    )),
                    PeerRequestOptions::no_options(),
                )
                .await?;
            let mut handle = Some(handle);
            for message in ["hierarchy pending", "cancellation received"] {
                timeout(Duration::from_secs(5), async {
                    loop {
                        let result = client.call_tool(CallToolRequestParams::new("deixis_server_status").with_arguments(json!({ "server": "mock-lsp" }).as_object().unwrap().clone())).await?;
                        if result.structured_content.unwrap()["readiness"]["message"] == message { break; }
                        sleep(Duration::from_millis(10)).await;
                    }
                    Ok::<_, Box<dyn Error>>(())
                }).await??;
                if message == "hierarchy pending" {
                    handle
                        .take()
                        .unwrap()
                        .cancel(Some("test cancellation".to_owned()))
                        .await?;
                }
            }
            timeout(Duration::from_secs(10), client.cancel()).await??;
        }
    }
    Ok(())
}

#[tokio::test]
async fn call_hierarchy_keeps_unreadable_and_outside_targets_in_server_encoding()
-> Result<(), Box<dyn Error>> {
    for outside in [false, true] {
        let mode = if outside {
            "call-hierarchy-outside-utf-16"
        } else {
            "call-hierarchy-utf-16"
        };
        let (root, client) = client_for(mode, 1000).await?;
        fs::remove_file(root.join("other.rs"))?;
        let outside_path = root.with_extension("outside.rs");
        fs::write(&outside_path, "🦀answer = 42;\n")?;
        for tool in ["incoming_calls", "outgoing_calls"] {
            let result =
                client
                    .call_tool(CallToolRequestParams::new(tool).with_arguments(
                        arguments().as_object().unwrap().clone(),
                    ))
                    .await?;
            assert_eq!(result.is_error, Some(false), "{result:?}");
            let structured = result.structured_content.unwrap();
            let other = if tool == "incoming_calls" {
                "from"
            } else {
                "to"
            };
            assert_eq!(
                structured["calls"][0][other]["positionEncoding"],
                "utf-16"
            );
            assert_eq!(
                structured["calls"][0][other]["selectionRange"]["start"]["character"],
                2
            );
            if tool == "incoming_calls" {
                assert_eq!(
                    structured["calls"][0]["fromRanges"][0]["start"]["character"],
                    2
                );
            }
        }
        timeout(Duration::from_secs(10), client.cancel()).await??;
        fs::remove_file(outside_path)?;
    }
    Ok(())
}

#[tokio::test]
async fn call_hierarchy_checks_routes_and_resynchronizes_edited_documents()
-> Result<(), Box<dyn Error>> {
    for tool in ["incoming_calls", "outgoing_calls"] {
        let (root, client) = client_for("call-hierarchy-sync", 1000).await?;
        let outside = root.with_extension("outside.rs");
        fs::write(&outside, "fn outside() {}\n")?;
        for (change, code) in [
            (json!({ "path": outside }), "invalid_path"),
            (json!({ "server": "missing" }), "routing_error"),
            (json!({ "path": "missing.rs" }), "invalid_path"),
        ] {
            let mut args = arguments().as_object().unwrap().clone();
            args.extend(change.as_object().unwrap().clone());
            let result = client
                .call_tool(
                    CallToolRequestParams::new(tool).with_arguments(args),
                )
                .await?;
            assert_eq!(
                result.structured_content.unwrap()["error"]["code"],
                code
            );
        }
        let status = client
            .call_tool(
                CallToolRequestParams::new("deixis_server_status")
                    .with_arguments(
                        json!({ "server": "mock-lsp" })
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
            )
            .await?;
        assert_eq!(status.structured_content.unwrap()["started"], false);
        for source in ["let 🦀answer = 42;\n", "var 🦀answer = 42;\n"] {
            fs::write(root.join("main.rs"), source)?;
            let result =
                client
                    .call_tool(CallToolRequestParams::new(tool).with_arguments(
                        arguments().as_object().unwrap().clone(),
                    ))
                    .await?;
            assert_eq!(result.is_error, Some(false), "{result:?}");
        }
        let mut args = arguments();
        args["position"]["character"] = json!(6);
        let result = client
            .call_tool(
                CallToolRequestParams::new(tool)
                    .with_arguments(args.as_object().unwrap().clone()),
            )
            .await?;
        assert_eq!(
            result.structured_content.unwrap()["error"]["code"],
            "invalid_position"
        );
        timeout(Duration::from_secs(10), client.cancel()).await??;
        fs::remove_file(outside)?;
    }
    Ok(())
}
