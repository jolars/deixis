use std::{
    error::Error,
    fs,
    path::PathBuf,
    process::{self, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rmcp::{
    ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, ClientRequest, Request,
        ServerCapabilities,
    },
    service::PeerRequestOptions,
    transport::TokioChildProcess,
};
use serde_json::{Value as JsonValue, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
    time::{sleep, timeout},
};

mod support;

#[tokio::test]
async fn negotiates_an_empty_mcp_server_over_stdio()
-> Result<(), Box<dyn Error>> {
    let config_home = unique_dir("empty-user-config")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .env("RUST_LOG", "deixis=debug")
        .env("XDG_CONFIG_HOME", config_home);
    let transport = TokioChildProcess::new(command)?;

    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let peer = client
        .peer_info()
        .expect("server metadata should be retained");
    let implementation = peer
        .server_info
        .as_ref()
        .expect("an initialized server should identify itself");

    assert_eq!(implementation.name, "deixis");
    assert_eq!(implementation.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(peer.capabilities, ServerCapabilities::default());

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn accepts_an_explicit_root_without_config() -> Result<(), Box<dyn Error>>
{
    let root = unique_dir("explicit-root")?;
    let config_home = unique_dir("explicit-root-empty-config")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command.arg("--root").arg(&root);
    command
        .env("RUST_LOG", "deixis=debug")
        .env("XDG_CONFIG_HOME", config_home);
    let transport = TokioChildProcess::new(command)?;

    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let peer = client
        .peer_info()
        .expect("server metadata should be retained");

    assert_eq!(peer.capabilities, ServerCapabilities::default());

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn reports_missing_config_on_stderr_without_stdout()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("missing-config-root")?;
    let missing_config = root.join("missing.toml");
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command.arg("--config").arg(&missing_config);

    let output = timeout(Duration::from_secs(10), command.output()).await??;

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to resolve config"));
    assert!(stderr.contains("missing.toml"));
    Ok(())
}

#[tokio::test]
async fn reports_invalid_config_on_stderr_without_stdout()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("invalid-config-root")?;
    let config_path = root.join("deixis.toml");
    fs::write(&config_path, "servers = true")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command.arg("--config").arg(&config_path);

    let output = timeout(Duration::from_secs(10), command.output()).await??;

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to parse config TOML"));
    Ok(())
}

#[tokio::test]
async fn configured_child_diagnostics_stay_on_stderr_with_server_name()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("child-stderr-root")?;
    let config_path = write_mock_config(&root)?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path)
        .env("RUST_LOG", "deixis=info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().expect("deixis stdin should be piped");
    let stdout = child.stdout.take().expect("deixis stdout should be piped");
    let mut stderr =
        child.stderr.take().expect("deixis stderr should be piped");
    let stderr_task = tokio::spawn(async move {
        let mut output = String::new();
        stderr.read_to_string(&mut output).await.map(|_| output)
    });
    let mut stdout = BufReader::new(stdout);

    write_json_line(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "deixis-test",
                    "version": "0.1.0"
                }
            }
        }),
    )
    .await?;
    let initialize = read_stdout_json_line(&mut stdout).await?;
    assert_eq!(initialize["id"], 1);
    assert!(initialize["result"]["capabilities"]["tools"].is_object());

    write_json_line(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )
    .await?;
    write_json_line(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "deixis_server_status",
                "arguments": {
                    "start": true
                }
            }
        }),
    )
    .await?;

    let tool_response = read_stdout_json_line(&mut stdout).await?;
    assert_eq!(tool_response["id"], 2);
    assert_eq!(
        tool_response["result"]["structuredContent"]["configuredName"],
        "mock-lsp"
    );
    assert_eq!(
        tool_response["result"]["structuredContent"]["serverName"],
        "deixis-mock-lsp"
    );
    assert_eq!(
        tool_response["result"]["structuredContent"]["started"],
        true
    );
    assert_eq!(
        tool_response["result"]["structuredContent"]["positionEncoding"],
        "utf-8"
    );
    assert_eq!(
        tool_response["result"]["structuredContent"]["readiness"],
        json!({
            "state": "unknown",
            "source": "lifecycle",
            "activeProgress": 0,
        })
    );

    drop(stdin);
    let remaining_stdout = read_remaining_stdout(stdout).await?;
    assert!(
        remaining_stdout.is_empty(),
        "unexpected extra MCP stdout messages: {remaining_stdout:?}"
    );
    let status = timeout(Duration::from_secs(10), child.wait()).await??;
    assert!(status.success());

    let stderr = timeout(Duration::from_secs(10), stderr_task).await???;
    assert!(stderr.contains("server=mock-lsp"), "{stderr}");
    assert!(
        stderr.contains("mock-lsp started in normal mode"),
        "{stderr}"
    );
    assert!(stderr.contains("mock-lsp initializing"), "{stderr}");
    Ok(())
}

#[tokio::test]
async fn discovers_the_user_config_without_fixing_the_project_root()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("xdg-project-root")?;
    let config_home = unique_dir("xdg-config-home")?;
    let config_dir = config_home.join("deixis");
    fs::create_dir(&config_dir)?;
    write_mock_config_to(&config_dir.join("config.toml"), "normal")?;

    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .current_dir(&root)
        .env("XDG_CONFIG_HOME", &config_home);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let tools =
        timeout(Duration::from_secs(10), client.list_tools(None)).await??;

    assert!(tools.tools.iter().any(|tool| tool.name == "hover"));
    for tool in &tools.tools {
        let output_schema = tool.output_schema.as_ref().unwrap_or_else(|| {
            panic!("{} should declare an output schema", tool.name)
        });
        let error_codes = output_schema
            .get("oneOf")
            .and_then(JsonValue::as_array)
            .and_then(|variants| {
                variants.iter().find_map(|variant| {
                    variant.pointer("/properties/error/properties/code/enum")
                })
            })
            .and_then(JsonValue::as_array)
            .unwrap_or_else(|| {
                panic!("{} should advertise the shared error schema", tool.name)
            });
        for code in [
            "invalid_path",
            "invalid_position",
            "unsupported_capability",
            "request_timeout",
            "server_exited",
            "lsp_error",
        ] {
            assert!(
                error_codes.contains(&json!(code)),
                "{}: {code}",
                tool.name
            );
        }
    }

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn every_tool_has_a_stable_text_fallback_and_structured_output()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("stable-text-renderers")?;
    fs::write(root.join("main.rs"), "let answer = 42; xx\n")?;
    let config_path =
        write_mock_config_for_mode(&root, "diagnostics-pull-renderers")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let uri = url::Url::from_file_path(root.join("main.rs"))
        .unwrap()
        .to_string();

    let stopped_status = timeout(
        Duration::from_secs(10),
        client.call_tool(CallToolRequestParams::new("deixis_server_status")),
    )
    .await??;
    assert_eq!(
        stopped_status.content[0].as_text().unwrap().text,
        "mock-lsp: not started; readiness not started (lifecycle)."
    );
    assert_eq!(
        stopped_status.structured_content.as_ref().unwrap()["capabilities"],
        JsonValue::Null
    );

    let status_arguments =
        json!({ "start": true }).as_object().unwrap().clone();
    let status = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("deixis_server_status")
                .with_arguments(status_arguments),
        ),
    )
    .await??;
    assert_eq!(
        status.content[0].as_text().unwrap().text,
        "mock-lsp: started as deixis-mock-lsp 0.1.0; position encoding utf-8; readiness unknown (lifecycle)."
    );
    assert_eq!(
        status.structured_content.as_ref().unwrap()["capabilities"]["hoverProvider"],
        true
    );

    let position_arguments = json!({
        "path": "main.rs",
        "position": { "line": 0, "character": 8 },
    })
    .as_object()
    .unwrap()
    .clone();
    let references_arguments = json!({
        "path": "main.rs",
        "position": { "line": 0, "character": 8 },
        "includeDeclaration": true,
    })
    .as_object()
    .unwrap()
    .clone();
    let file_arguments =
        json!({ "path": "main.rs" }).as_object().unwrap().clone();
    let workspace_arguments =
        json!({ "query": "answer" }).as_object().unwrap().clone();
    let location_text = format!("mock-lsp: {uri}:0:8-0:14 (utf-8)");
    let cases = [
        (
            "hover",
            position_arguments.clone(),
            "`answer`: the ultimate value".to_owned(),
        ),
        (
            "declaration",
            position_arguments.clone(),
            location_text.clone(),
        ),
        (
            "definition",
            position_arguments.clone(),
            location_text.clone(),
        ),
        (
            "type_definition",
            position_arguments.clone(),
            location_text.clone(),
        ),
        ("implementation", position_arguments, location_text.clone()),
        (
            "references",
            references_arguments,
            format!("mock-lsp: {uri}:0:0-0:3 (utf-8)\n{location_text}"),
        ),
        (
            "diagnostics",
            file_arguments.clone(),
            "1 diagnostic for main.rs.".to_owned(),
        ),
        (
            "document_symbols",
            file_arguments,
            format!(
                "mock-lsp: binding (kind 13) at {uri}:0:6-0:12 (utf-8)\n  mock-lsp: answer (kind 13) at {uri}:0:6-0:12 (utf-8)"
            ),
        ),
        (
            "workspace_symbols",
            workspace_arguments,
            format!("mock-lsp: mockSymbol (kind 12) at {uri}:0:0-0:3 (utf-8)"),
        ),
    ];

    for (tool, arguments, expected_text) in cases {
        let result = timeout(
            Duration::from_secs(10),
            client.call_tool(
                CallToolRequestParams::new(tool).with_arguments(arguments),
            ),
        )
        .await??;

        assert_eq!(result.is_error, Some(false), "{tool}");
        assert_eq!(
            result.content[0].as_text().unwrap().text,
            expected_text,
            "{tool}"
        );
        let structured =
            result.structured_content.as_ref().unwrap_or_else(|| {
                panic!("{tool} should retain structured output")
            });
        assert!(structured.is_object(), "{tool}");

        match tool {
            "hover" => assert_eq!(structured["contents"]["kind"], "markdown"),
            "diagnostics" => {
                assert_eq!(structured["diagnostics"][0]["severity"], 1);
                assert_eq!(
                    structured["diagnostics"][0]["data"]["extension"],
                    true
                );
            }
            "document_symbols" => assert_eq!(
                structured["symbols"][0]["x-mock-extension"],
                "parent"
            ),
            "workspace_symbols" => {
                assert_eq!(structured["symbols"][0]["deprecated"], true);
                assert_eq!(structured["symbols"][0]["x-mock-extension"], true);
            }
            _ => assert_eq!(structured["locations"][0]["server"], "mock-lsp"),
        }
    }

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn read_only_tools_accept_null_and_empty_lsp_results()
-> Result<(), Box<dyn Error>> {
    for mode in ["semantic-responses-null", "semantic-responses-empty"] {
        let root = unique_dir(mode)?;
        fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
        let config_path = write_mock_config_for_mode(&root, mode)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
        command
            .arg("--root")
            .arg(&root)
            .arg("--config")
            .arg(&config_path);
        let transport = TokioChildProcess::new(command)?;
        let client =
            timeout(Duration::from_secs(10), ().serve(transport)).await??;

        let position_arguments = json!({
            "path": "main.rs",
            "position": { "line": 0, "character": 8 },
        });
        let cases = [
            ("hover", position_arguments.clone(), "contents"),
            ("declaration", position_arguments.clone(), "locations"),
            ("definition", position_arguments.clone(), "locations"),
            ("type_definition", position_arguments.clone(), "locations"),
            ("implementation", position_arguments.clone(), "locations"),
            (
                "references",
                json!({
                    "path": "main.rs",
                    "position": { "line": 0, "character": 8 },
                    "includeDeclaration": true,
                }),
                "locations",
            ),
            ("document_symbols", json!({ "path": "main.rs" }), "symbols"),
            ("workspace_symbols", json!({ "query": "" }), "symbols"),
        ];

        for (tool, arguments, result_field) in cases {
            let result = timeout(
                Duration::from_secs(10),
                client.call_tool(
                    CallToolRequestParams::new(tool).with_arguments(
                        arguments
                            .as_object()
                            .expect("tool arguments should be an object")
                            .clone(),
                    ),
                ),
            )
            .await??;

            assert_eq!(result.is_error, Some(false), "{mode}: {tool}");
            let structured = result
                .structured_content
                .as_ref()
                .expect("successful results should be structured");
            if tool == "hover" && mode.ends_with("null") {
                assert_eq!(structured[result_field], JsonValue::Null);
            } else {
                assert_eq!(
                    structured[result_field],
                    json!([]),
                    "{mode}: {tool}"
                );
            }

            if tool == "workspace_symbols"
                || (tool == "hover" && mode.ends_with("empty"))
            {
                assert!(
                    structured.get("readiness").is_none(),
                    "{mode}: {tool}"
                );
                assert!(
                    structured.get("resultStability").is_none(),
                    "{mode}: {tool}"
                );
            } else {
                assert_eq!(structured["readiness"]["state"], "unknown");
                assert_eq!(structured["resultStability"], "indeterminate");
            }
        }

        timeout(Duration::from_secs(10), client.cancel()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn workspace_symbols_fan_out_concurrently_in_stable_server_order()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("workspace-symbol-fanout")?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
    let barrier = root.join("workspace-symbol-barrier");
    let config_path = write_workspace_symbol_config(&root, &barrier)?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;

    let tools =
        timeout(Duration::from_secs(10), client.list_tools(None)).await??;
    let tool = tools
        .tools
        .iter()
        .find(|tool| tool.name == "workspace_symbols")
        .expect("configured servers should expose workspace symbols");
    assert_eq!(tool.input_schema.get("required"), Some(&json!(["query"])));
    assert!(tool.output_schema.is_some());
    assert_eq!(
        tool.annotations
            .as_ref()
            .and_then(|annotations| annotations.read_only_hint),
        Some(true)
    );

    let arguments = json!({ "query": "answer" }).as_object().unwrap().clone();
    let result = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("workspace_symbols")
                .with_arguments(arguments),
        ),
    )
    .await??;

    assert_eq!(result.is_error, Some(false));
    let structured = result.structured_content.as_ref().unwrap();
    let symbols = structured["symbols"].as_array().unwrap();
    assert_eq!(symbols.len(), 2);
    assert_eq!(symbols[0]["server"], "alpha");
    assert_eq!(symbols[0]["name"], "alphaSymbol");
    assert_eq!(symbols[0]["kind"], 12);
    assert_eq!(symbols[0]["tags"], json!([1]));
    assert_eq!(symbols[0]["deprecated"], true);
    assert_eq!(symbols[0]["containerName"], "mock crate");
    assert_eq!(
        symbols[0]["location"]["range"],
        json!({
            "start": { "line": 0, "character": 8 },
            "end": { "line": 0, "character": 14 },
        })
    );
    assert_eq!(symbols[0]["positionEncoding"], "utf-8");
    assert_eq!(symbols[0]["location"]["x-location-extension"], "nested");
    assert_eq!(symbols[0]["data"]["query"], "answer");
    assert_eq!(symbols[0]["x-mock-extension"], true);
    assert_eq!(symbols[1]["server"], "zeta");
    assert_eq!(symbols[1]["name"], "zetaSymbol");
    assert!(barrier.exists(), "the release server should have run");
    let text = result.content[0].as_text().unwrap().text.as_str();
    assert!(
        text.lines()
            .next()
            .unwrap()
            .starts_with("alpha: alphaSymbol")
    );

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn one_server_failure_does_not_block_or_corrupt_another_server()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("workspace-symbol-failure-isolation")?;
    fs::write(root.join("main.py"), "let 🦀answer = 42;\n")?;
    let completed = root.join("healthy-server-completed");
    let config_path = write_failure_isolation_config(&root, &completed)?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;

    let arguments = json!({ "query": "answer" }).as_object().unwrap().clone();
    let failed_fanout = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("workspace_symbols")
                .with_arguments(arguments),
        ),
    )
    .await??;
    assert_eq!(failed_fanout.is_error, Some(true));
    assert_eq!(
        failed_fanout
            .structured_content
            .as_ref()
            .and_then(|value| value.pointer("/error/code")),
        Some(&json!("server_exited"))
    );
    assert_eq!(
        failed_fanout
            .structured_content
            .as_ref()
            .and_then(|value| value.pointer("/error/server")),
        Some(&json!("alpha"))
    );
    assert!(
        completed.exists(),
        "the healthy server should have completed"
    );

    let hover_arguments = json!({
        "path": "main.py",
        "position": { "line": 0, "character": 8 },
    })
    .as_object()
    .unwrap()
    .clone();
    let hover = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("hover").with_arguments(hover_arguments),
        ),
    )
    .await??;
    assert_eq!(hover.is_error, Some(false));
    assert_eq!(
        hover
            .structured_content
            .as_ref()
            .and_then(|value| value.pointer("/contents/value")),
        Some(&json!("`answer`: the ultimate value"))
    );

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn workspace_symbols_reports_when_no_server_supports_the_method()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("workspace-symbol-unsupported")?;
    let config_path =
        write_mock_config_for_mode(&root, "workspace-symbol-unsupported")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let arguments = json!({ "query": "answer" }).as_object().unwrap().clone();

    let result = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("workspace_symbols")
                .with_arguments(arguments),
        ),
    )
    .await??;

    assert_eq!(result.is_error, Some(true));
    let error = &result.structured_content.as_ref().unwrap()["error"];
    assert_eq!(error["code"], "unsupported_capability");
    assert_eq!(error["tool"], "workspace_symbols");
    assert_eq!(error["server"], "mock-lsp");
    assert_eq!(error["method"], "workspace/symbol");

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn workspace_symbols_skip_servers_without_the_capability()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("workspace-symbol-mixed-capabilities")?;
    fs::write(root.join("main.rs"), "fn answer() {}\n")?;
    let config_path = write_mixed_workspace_symbol_config(&root)?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let arguments = json!({ "query": "" }).as_object().unwrap().clone();

    let result = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("workspace_symbols")
                .with_arguments(arguments),
        ),
    )
    .await??;

    assert_eq!(result.is_error, Some(false));
    let symbols = result.structured_content.as_ref().unwrap()["symbols"]
        .as_array()
        .unwrap();
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["server"], "zeta");
    assert_eq!(symbols[0]["name"], "zetaSymbol");
    assert_eq!(symbols[0]["data"]["query"], "");

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn document_symbols_preserve_hierarchy_and_normalize_flat_responses()
-> Result<(), Box<dyn Error>> {
    for mode in [
        "document-symbols-hierarchical-utf-16",
        "document-symbols-flat-utf-16",
    ] {
        let root = unique_dir(mode)?;
        fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
        let config_path = write_mock_config_for_mode(&root, mode)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
        command
            .arg("--root")
            .arg(&root)
            .arg("--config")
            .arg(&config_path);
        let transport = TokioChildProcess::new(command)?;
        let client =
            timeout(Duration::from_secs(10), ().serve(transport)).await??;

        let tools =
            timeout(Duration::from_secs(10), client.list_tools(None)).await??;
        let tool = tools
            .tools
            .iter()
            .find(|tool| tool.name == "document_symbols")
            .expect("configured servers should expose document symbols");
        assert_eq!(tool.input_schema.get("required"), Some(&json!(["path"])));
        assert!(tool.output_schema.is_some());
        assert_eq!(
            tool.annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint),
            Some(true)
        );

        let arguments =
            json!({ "path": "main.rs" }).as_object().unwrap().clone();
        let result = timeout(
            Duration::from_secs(10),
            client.call_tool(
                CallToolRequestParams::new("document_symbols")
                    .with_arguments(arguments),
            ),
        )
        .await??;

        assert_eq!(result.is_error, Some(false), "{mode}");
        let symbols = result.structured_content.as_ref().unwrap()["symbols"]
            .as_array()
            .unwrap();
        assert_eq!(symbols.len(), 1, "{mode}");
        assert_eq!(symbols[0]["server"], "mock-lsp", "{mode}");
        assert_eq!(symbols[0]["positionEncoding"], "utf-8", "{mode}");
        assert!(
            symbols[0]["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("/main.rs")),
            "{mode}"
        );

        let text = result.content[0].as_text().unwrap().text.as_str();
        if mode.contains("hierarchical") {
            let children = symbols[0]["children"].as_array().unwrap();
            assert_eq!(symbols[0]["name"], "binding");
            assert_eq!(children.len(), 1);
            assert_eq!(children[0]["name"], "answer");
            assert_eq!(children[0]["server"], "mock-lsp");
            assert_eq!(children[0]["positionEncoding"], "utf-8");
            assert!(text.lines().nth(1).is_some_and(|line| {
                line.starts_with("  mock-lsp: answer")
            }));
        } else {
            assert_eq!(symbols[0]["name"], "answer");
            assert_eq!(symbols[0]["containerName"], "binding");
            assert_eq!(symbols[0]["children"], json!([]));
            assert_eq!(text.lines().count(), 1);
        }

        timeout(Duration::from_secs(10), client.cancel()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn document_symbols_report_an_unsupported_server_capability()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("document-symbols-unsupported")?;
    fs::write(root.join("main.rs"), "let answer = 42;\n")?;
    let config_path =
        write_mock_config_for_mode(&root, "document-symbols-unsupported")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let arguments = json!({ "path": "main.rs" }).as_object().unwrap().clone();

    let result = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("document_symbols")
                .with_arguments(arguments),
        ),
    )
    .await??;

    assert_eq!(result.is_error, Some(true));
    let error = &result.structured_content.as_ref().unwrap()["error"];
    assert_eq!(error["code"], "unsupported_capability");
    assert_eq!(error["tool"], "document_symbols");
    assert_eq!(error["server"], "mock-lsp");
    assert_eq!(error["method"], "textDocument/documentSymbol");

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn hover_returns_structured_markup_across_position_encodings()
-> Result<(), Box<dyn Error>> {
    for mode in [
        "hover-utf-8",
        "hover-utf-16",
        "hover-utf-32",
        "hover-options",
    ] {
        let root = unique_dir(mode)?;
        fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
        let config_path = write_mock_config_for_mode(&root, mode)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
        command
            .arg("--root")
            .arg(&root)
            .arg("--config")
            .arg(&config_path);
        let transport = TokioChildProcess::new(command)?;
        let client =
            timeout(Duration::from_secs(10), ().serve(transport)).await??;

        let tools =
            timeout(Duration::from_secs(10), client.list_tools(None)).await??;
        let hover_tool = tools
            .tools
            .iter()
            .find(|tool| tool.name == "hover")
            .expect("configured servers should expose hover");
        assert_eq!(
            hover_tool.input_schema.get("required"),
            Some(&json!(["path", "position"])),
            "{mode}"
        );
        assert!(hover_tool.output_schema.is_some(), "{mode}");

        let arguments = json!({
            "path": "main.rs",
            "position": {
                "line": 0,
                "character": 8,
            },
        })
        .as_object()
        .expect("hover arguments should be an object")
        .clone();
        let result = timeout(
            Duration::from_secs(10),
            client.call_tool(
                CallToolRequestParams::new("hover").with_arguments(arguments),
            ),
        )
        .await??;

        assert_eq!(result.is_error, Some(false), "{mode}");
        assert_eq!(
            result.structured_content,
            Some(json!({
                "contents": {
                    "kind": "markdown",
                    "value": "`answer`: the ultimate value",
                },
                "range": {
                    "start": { "line": 0, "character": 8 },
                    "end": { "line": 0, "character": 14 },
                },
            })),
            "{mode}"
        );
        assert_eq!(
            result
                .content
                .first()
                .and_then(|content| content.as_text())
                .map(|content| content.text.as_str()),
            Some("`answer`: the ultimate value"),
            "{mode}"
        );

        timeout(Duration::from_secs(10), client.cancel()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn forwards_mcp_cancellation_during_an_lsp_response_race()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("hover-cancellation-race")?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
    let config_path = write_mock_config_for_mode_with_timeout(
        &root,
        "hover-cancellation-race",
        10_000,
    )?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;

    let start_arguments = json!({ "start": true }).as_object().unwrap().clone();
    timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("deixis_server_status")
                .with_arguments(start_arguments),
        ),
    )
    .await??;

    let hover_arguments = json!({
        "path": "main.rs",
        "position": { "line": 0, "character": 8 },
    })
    .as_object()
    .unwrap()
    .clone();
    let handle = client
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(Request::new(
                CallToolRequestParams::new("hover")
                    .with_arguments(hover_arguments.clone()),
            )),
            PeerRequestOptions::no_options(),
        )
        .await?;

    timeout(Duration::from_secs(10), async {
        loop {
            let status = client
                .call_tool(CallToolRequestParams::new("deixis_server_status"))
                .await?;
            if status
                .structured_content
                .as_ref()
                .and_then(|value| value.pointer("/readiness/message"))
                == Some(&json!("hover pending"))
            {
                return Ok::<(), rmcp::ServiceError>(());
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;

    handle.cancel(Some("test cancellation".to_owned())).await?;

    timeout(Duration::from_secs(10), async {
        loop {
            let status = client
                .call_tool(CallToolRequestParams::new("deixis_server_status"))
                .await?;
            if status
                .structured_content
                .as_ref()
                .and_then(|value| value.pointer("/readiness/message"))
                == Some(&json!("cancellation received"))
            {
                return Ok::<(), rmcp::ServiceError>(());
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;

    let hover = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("hover").with_arguments(hover_arguments),
        ),
    )
    .await??;
    assert_eq!(hover.is_error, Some(false));

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn diagnostics_prefers_pull_reports_and_exposes_freshness()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("diagnostics-pull-utf-16")?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
    let config_path =
        write_mock_config_for_mode(&root, "diagnostics-pull-utf-16")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;

    let tools =
        timeout(Duration::from_secs(10), client.list_tools(None)).await??;
    let diagnostics_tool = tools
        .tools
        .iter()
        .find(|tool| tool.name == "diagnostics")
        .expect("configured servers should expose diagnostics");
    assert_eq!(
        diagnostics_tool.input_schema.get("required"),
        Some(&json!(["path"]))
    );
    assert!(diagnostics_tool.output_schema.is_some());
    assert_eq!(
        diagnostics_tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.read_only_hint),
        Some(true)
    );

    let arguments = json!({ "path": "main.rs" }).as_object().unwrap().clone();
    let result = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("diagnostics").with_arguments(arguments),
        ),
    )
    .await??;

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        result.structured_content,
        Some(json!({
            "server": "mock-lsp",
            "uri": url::Url::from_file_path(root.join("main.rs"))
                .unwrap()
                .to_string(),
            "source": "pull",
            "availability": "current",
            "documentVersion": 1,
            "reportVersion": null,
            "resultId": "pull-1",
            "positionEncoding": "utf-8",
            "diagnostics": [{
                "range": {
                    "start": { "line": 0, "character": 8 },
                    "end": { "line": 0, "character": 14 },
                },
                "severity": 1,
                "message": "mock diagnostic",
                "data": { "extension": true },
            }],
        }))
    );
    assert!(
        result.content[0]
            .as_text()
            .is_some_and(|content| content.text.contains("1 diagnostic"))
    );

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn diagnostics_marks_missing_and_outdated_push_reports()
-> Result<(), Box<dyn Error>> {
    for (mode, availability, report_version, diagnostic_count) in [
        ("diagnostics-push-unavailable", "unavailable", None, 0),
        ("diagnostics-push-stale", "stale", Some(0), 1),
    ] {
        let root = unique_dir(mode)?;
        fs::write(root.join("main.rs"), "let answer = 42;\n")?;
        let config_path = write_mock_config_for_mode(&root, mode)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
        command
            .arg("--root")
            .arg(&root)
            .arg("--config")
            .arg(&config_path);
        let transport = TokioChildProcess::new(command)?;
        let client =
            timeout(Duration::from_secs(10), ().serve(transport)).await??;
        let arguments =
            json!({ "path": "main.rs" }).as_object().unwrap().clone();

        let result = timeout(
            Duration::from_secs(10),
            client.call_tool(
                CallToolRequestParams::new("diagnostics")
                    .with_arguments(arguments),
            ),
        )
        .await??;

        assert_eq!(result.is_error, Some(false), "{mode}");
        let report = result
            .structured_content
            .as_ref()
            .expect("diagnostics should be structured");
        assert_eq!(report["source"], "push", "{mode}");
        assert_eq!(report["availability"], availability, "{mode}");
        assert_eq!(report["documentVersion"], 1, "{mode}");
        assert_eq!(report["reportVersion"], json!(report_version), "{mode}");
        assert_eq!(
            report["diagnostics"].as_array().map(Vec::len),
            Some(diagnostic_count),
            "{mode}"
        );

        timeout(Duration::from_secs(10), client.cancel()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn diagnostics_distinguishes_transient_and_stable_empty_reports()
-> Result<(), Box<dyn Error>> {
    for (mode, source) in [
        ("diagnostics-pull-readiness-progress", "workDoneProgress"),
        ("diagnostics-pull-readiness-server-status", "serverStatus"),
    ] {
        let root = unique_dir(mode)?;
        fs::write(root.join("main.rs"), "let answer = 42;\n")?;
        let config_path = write_mock_config_for_mode(&root, mode)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
        command
            .arg("--root")
            .arg(&root)
            .arg("--config")
            .arg(&config_path);
        let transport = TokioChildProcess::new(command)?;
        let client =
            timeout(Duration::from_secs(10), ().serve(transport)).await??;
        let arguments =
            json!({ "path": "main.rs" }).as_object().unwrap().clone();

        let transient = timeout(
            Duration::from_secs(10),
            client.call_tool(
                CallToolRequestParams::new("diagnostics")
                    .with_arguments(arguments.clone()),
            ),
        )
        .await??;
        let transient_report = transient
            .structured_content
            .as_ref()
            .expect("diagnostics should be structured");
        assert!(
            transient_report["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(transient_report["readiness"]["state"], "busy", "{mode}");
        assert_eq!(transient_report["readiness"]["source"], source, "{mode}");
        assert_eq!(transient_report["resultStability"], "transient", "{mode}");
        assert!(
            transient.content[0]
                .as_text()
                .is_some_and(|content| content.text.contains("still working")),
            "{mode}"
        );

        let stable = timeout(
            Duration::from_secs(10),
            client.call_tool(
                CallToolRequestParams::new("diagnostics")
                    .with_arguments(arguments),
            ),
        )
        .await??;
        let stable_report = stable
            .structured_content
            .as_ref()
            .expect("diagnostics should be structured");
        assert!(stable_report["diagnostics"].as_array().unwrap().is_empty());
        assert_eq!(stable_report["readiness"]["state"], "ready", "{mode}");
        assert_eq!(stable_report["readiness"]["source"], source, "{mode}");
        assert_eq!(stable_report["resultStability"], "stable", "{mode}");
        assert!(
            stable.content[0]
                .as_text()
                .is_some_and(|content| content.text.contains("No diagnostics")),
            "{mode}"
        );

        timeout(Duration::from_secs(10), client.cancel()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn hover_rejects_a_server_without_the_capability()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("hover-unsupported")?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
    let config_path = write_mock_config_for_mode(&root, "hover-unsupported")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let arguments = json!({
        "path": "main.rs",
        "position": { "line": 0, "character": 8 },
    })
    .as_object()
    .expect("hover arguments should be an object")
    .clone();

    let result = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("hover").with_arguments(arguments),
        ),
    )
    .await??;

    assert_eq!(result.is_error, Some(true));
    let message = result
        .content
        .first()
        .and_then(|content| content.as_text())
        .expect("capability errors should have a text fallback");
    assert!(
        message
            .text
            .contains("does not support `textDocument/hover`")
    );

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn hover_failures_have_a_consistent_structured_shape()
-> Result<(), Box<dyn Error>> {
    let invalid_path =
        call_hover_error("normal", "missing.rs", None, 0, 8, 1_000).await?;
    assert_tool_error(
        &invalid_path,
        "invalid_path",
        None,
        Some("textDocument/hover"),
        "missing.rs",
    );

    let invalid_position = call_hover_error(
        "normal",
        "main.rs",
        Some("let answer = 42;\n"),
        0,
        100,
        1_000,
    )
    .await?;
    assert_tool_error(
        &invalid_position,
        "invalid_position",
        Some("mock-lsp"),
        Some("textDocument/hover"),
        "main.rs",
    );

    let unsupported = call_hover_error(
        "hover-unsupported",
        "main.rs",
        Some("let answer = 42;\n"),
        0,
        4,
        1_000,
    )
    .await?;
    assert_tool_error(
        &unsupported,
        "unsupported_capability",
        Some("mock-lsp"),
        Some("textDocument/hover"),
        "main.rs",
    );

    let request_timeout = call_hover_error(
        "hover-timeout",
        "main.rs",
        Some("let answer = 42;\n"),
        0,
        4,
        50,
    )
    .await?;
    assert_tool_error(
        &request_timeout,
        "request_timeout",
        Some("mock-lsp"),
        Some("textDocument/hover"),
        "main.rs",
    );
    assert_eq!(
        request_timeout
            .structured_content
            .as_ref()
            .and_then(|value| value.pointer("/error/timeoutMs")),
        Some(&json!(50))
    );

    let server_exit = call_hover_error(
        "hover-exit",
        "main.rs",
        Some("let answer = 42;\n"),
        0,
        4,
        1_000,
    )
    .await?;
    assert_tool_error(
        &server_exit,
        "server_exited",
        Some("mock-lsp"),
        Some("textDocument/hover"),
        "main.rs",
    );

    let response_too_large = call_hover_error_with_response_limit(
        "hover-large-response",
        "main.rs",
        Some("let 🦀answer = 42;\n"),
        0,
        8,
        1_000,
        4_096,
    )
    .await?;
    assert_tool_error(
        &response_too_large,
        "lsp_protocol_error",
        Some("mock-lsp"),
        Some("textDocument/hover"),
        "main.rs",
    );

    let lsp_error = call_hover_error(
        "hover-error",
        "main.rs",
        Some("let answer = 42;\n"),
        0,
        4,
        1_000,
    )
    .await?;
    assert_tool_error(
        &lsp_error,
        "lsp_error",
        Some("mock-lsp"),
        Some("textDocument/hover"),
        "main.rs",
    );
    assert_eq!(
        lsp_error
            .structured_content
            .as_ref()
            .and_then(|value| value.pointer("/error/lspError")),
        Some(&json!({
            "code": -32042,
            "message": "mock hover failed",
            "data": { "retry": false },
        }))
    );

    Ok(())
}

#[tokio::test]
async fn definition_normalizes_locations_and_retains_server_provenance()
-> Result<(), Box<dyn Error>> {
    for (mode, expected_locations) in [
        ("definition-location-utf-8", 1),
        ("definition-locations-utf-16", 1),
        ("definition-links-utf-32", 1),
        ("definition-options", 1),
        ("definition-multi-location-utf-16", 2),
    ] {
        let root = unique_dir(mode)?;
        fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
        let config_path = write_mock_config_for_mode(&root, mode)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
        command
            .arg("--root")
            .arg(&root)
            .arg("--config")
            .arg(&config_path);
        let transport = TokioChildProcess::new(command)?;
        let client =
            timeout(Duration::from_secs(10), ().serve(transport)).await??;

        let tools =
            timeout(Duration::from_secs(10), client.list_tools(None)).await??;
        let definition_tool = tools
            .tools
            .iter()
            .find(|tool| tool.name == "definition")
            .expect("configured servers should expose definition");
        assert_eq!(
            definition_tool.input_schema.get("required"),
            Some(&json!(["path", "position"])),
            "{mode}"
        );
        assert!(definition_tool.output_schema.is_some(), "{mode}");

        let arguments = json!({
            "path": "main.rs",
            "position": { "line": 0, "character": 8 },
        })
        .as_object()
        .expect("definition arguments should be an object")
        .clone();
        let result = timeout(
            Duration::from_secs(10),
            client.call_tool(
                CallToolRequestParams::new("definition")
                    .with_arguments(arguments),
            ),
        )
        .await??;

        assert_eq!(result.is_error, Some(false), "{mode}");
        let locations = result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("locations"))
            .and_then(JsonValue::as_array)
            .expect("definition should return a locations array");
        assert_eq!(locations.len(), expected_locations, "{mode}");
        assert_eq!(locations[0]["server"], "mock-lsp", "{mode}");
        assert!(
            locations[0]["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("/main.rs")),
            "{mode}"
        );
        assert_eq!(
            locations[0]["targetRange"],
            json!({
                "start": { "line": 0, "character": 8 },
                "end": { "line": 0, "character": 14 },
            }),
            "{mode}"
        );
        assert_eq!(
            locations[0]["targetSelectionRange"], locations[0]["targetRange"],
            "{mode}"
        );
        assert_eq!(locations[0]["targetPositionEncoding"], "utf-8", "{mode}");
        assert!(locations[0].get("originSelectionRange").is_none(), "{mode}");
        if expected_locations == 2 {
            assert_eq!(
                locations[1]["targetSelectionRange"],
                json!({
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 3 },
                }),
                "{mode}"
            );
        }
        assert!(
            result
                .content
                .first()
                .and_then(|content| content.as_text())
                .is_some_and(|content| content.text.contains("mock-lsp")),
            "{mode}"
        );

        timeout(Duration::from_secs(10), client.cancel()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn definition_rejects_a_server_without_the_capability()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("definition-unsupported")?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
    let config_path =
        write_mock_config_for_mode(&root, "definition-unsupported")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let arguments = json!({
        "path": "main.rs",
        "position": { "line": 0, "character": 8 },
    })
    .as_object()
    .expect("definition arguments should be an object")
    .clone();

    let result = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("definition").with_arguments(arguments),
        ),
    )
    .await??;

    assert_eq!(result.is_error, Some(true));
    assert!(
        result
            .content
            .first()
            .and_then(|content| content.as_text())
            .is_some_and(|content| content
                .text
                .contains("does not support `textDocument/definition`"))
    );

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn exposes_declaration_type_definition_and_implementation_end_to_end()
-> Result<(), Box<dyn Error>> {
    for (tool, mode) in [
        ("declaration", "declaration-location-utf-8"),
        ("type_definition", "type-definition-locations-utf-16"),
        ("implementation", "implementation-links-utf-32"),
    ] {
        let root = unique_dir(mode)?;
        fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
        let config_path = write_mock_config_for_mode(&root, mode)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
        command
            .arg("--root")
            .arg(&root)
            .arg("--config")
            .arg(&config_path);
        let transport = TokioChildProcess::new(command)?;
        let client =
            timeout(Duration::from_secs(10), ().serve(transport)).await??;

        let tools =
            timeout(Duration::from_secs(10), client.list_tools(None)).await??;
        let semantic_tool = tools
            .tools
            .iter()
            .find(|candidate| candidate.name == tool)
            .unwrap_or_else(|| {
                panic!("configured servers should expose {tool}")
            });
        assert_eq!(
            semantic_tool.input_schema.get("required"),
            Some(&json!(["path", "position"])),
            "{tool}"
        );
        assert!(semantic_tool.output_schema.is_some(), "{tool}");
        assert_eq!(
            semantic_tool
                .annotations
                .as_ref()
                .and_then(|a| a.read_only_hint),
            Some(true)
        );

        let arguments = json!({
            "path": "main.rs",
            "position": { "line": 0, "character": 8 },
        })
        .as_object()
        .expect("semantic arguments should be an object")
        .clone();
        let result = timeout(
            Duration::from_secs(10),
            client.call_tool(
                CallToolRequestParams::new(tool).with_arguments(arguments),
            ),
        )
        .await??;

        assert_eq!(result.is_error, Some(false), "{tool}");
        let locations = result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("locations"))
            .and_then(JsonValue::as_array)
            .unwrap_or_else(|| panic!("{tool} should return locations"));
        assert_eq!(locations.len(), 1, "{tool}");
        assert_eq!(locations[0]["server"], "mock-lsp", "{tool}");
        assert!(
            locations[0]["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("/main.rs")),
            "{tool}"
        );
        assert_eq!(
            locations[0]["targetSelectionRange"],
            json!({
                "start": { "line": 0, "character": 8 },
                "end": { "line": 0, "character": 14 },
            }),
            "{tool}"
        );
        assert_eq!(locations[0]["targetPositionEncoding"], "utf-8", "{tool}");

        timeout(Duration::from_secs(10), client.cancel()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn new_location_tools_report_their_unsupported_capabilities()
-> Result<(), Box<dyn Error>> {
    for (tool, mode, method) in [
        (
            "declaration",
            "declaration-unsupported",
            "textDocument/declaration",
        ),
        (
            "type_definition",
            "type-definition-unsupported",
            "textDocument/typeDefinition",
        ),
        (
            "implementation",
            "implementation-unsupported",
            "textDocument/implementation",
        ),
    ] {
        let root = unique_dir(mode)?;
        fs::write(root.join("main.rs"), "let answer = 42;\n")?;
        let config_path = write_mock_config_for_mode(&root, mode)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
        command
            .arg("--root")
            .arg(&root)
            .arg("--config")
            .arg(&config_path);
        let transport = TokioChildProcess::new(command)?;
        let client =
            timeout(Duration::from_secs(10), ().serve(transport)).await??;
        let arguments = json!({
            "path": "main.rs",
            "position": { "line": 0, "character": 4 },
        })
        .as_object()
        .expect("semantic arguments should be an object")
        .clone();

        let result = timeout(
            Duration::from_secs(10),
            client.call_tool(
                CallToolRequestParams::new(tool).with_arguments(arguments),
            ),
        )
        .await??;

        assert_eq!(result.is_error, Some(true), "{tool}");
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .and_then(|value| value.pointer("/error/code")),
            Some(&json!("unsupported_capability")),
            "{tool}"
        );
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .and_then(|value| value.pointer("/error/method")),
            Some(&json!(method)),
            "{tool}"
        );
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .and_then(|value| value.pointer("/error/tool")),
            Some(&json!(tool)),
            "{tool}"
        );

        timeout(Duration::from_secs(10), client.cancel()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn exposes_references_with_explicit_declaration_inclusion()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("references-locations-utf-16")?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
    let config_path =
        write_mock_config_for_mode(&root, "references-locations-utf-16")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;

    let tools =
        timeout(Duration::from_secs(10), client.list_tools(None)).await??;
    let references_tool = tools
        .tools
        .iter()
        .find(|candidate| candidate.name == "references")
        .expect("configured servers should expose references");
    assert_eq!(
        references_tool.input_schema.get("required"),
        Some(&json!(["path", "position", "includeDeclaration"]))
    );
    assert_eq!(
        references_tool
            .input_schema
            .get("properties")
            .and_then(JsonValue::as_object)
            .and_then(|properties| properties.get("includeDeclaration"))
            .and_then(|include_declaration| include_declaration.get("type")),
        Some(&json!("boolean"))
    );
    assert!(references_tool.output_schema.is_some());
    assert_eq!(
        references_tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.read_only_hint),
        Some(true)
    );

    for (include_declaration, expected_len) in [(false, 1), (true, 2)] {
        let arguments = json!({
            "path": "main.rs",
            "position": { "line": 0, "character": 8 },
            "includeDeclaration": include_declaration,
        })
        .as_object()
        .expect("references arguments should be an object")
        .clone();
        let result = timeout(
            Duration::from_secs(10),
            client.call_tool(
                CallToolRequestParams::new("references")
                    .with_arguments(arguments),
            ),
        )
        .await??;

        assert_eq!(result.is_error, Some(false));
        let locations = result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("locations"))
            .and_then(JsonValue::as_array)
            .expect("references should return locations");
        assert_eq!(locations.len(), expected_len);
        assert_eq!(
            locations.last().expect("a reference")["server"],
            "mock-lsp"
        );
        assert_eq!(
            locations.last().expect("a reference")["range"],
            json!({
                "start": { "line": 0, "character": 8 },
                "end": { "line": 0, "character": 14 },
            })
        );
        assert_eq!(
            locations.last().expect("a reference")["positionEncoding"],
            "utf-8"
        );
    }

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn references_reports_an_unsupported_capability()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("references-unsupported")?;
    fs::write(root.join("main.rs"), "let answer = 42;\n")?;
    let config_path =
        write_mock_config_for_mode(&root, "references-unsupported")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let arguments = json!({
        "path": "main.rs",
        "position": { "line": 0, "character": 4 },
        "includeDeclaration": true,
    })
    .as_object()
    .expect("references arguments should be an object")
    .clone();

    let result = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("references").with_arguments(arguments),
        ),
    )
    .await??;

    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .and_then(|value| value.pointer("/error/code")),
        Some(&json!("unsupported_capability"))
    );
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .and_then(|value| value.pointer("/error/method")),
        Some(&json!("textDocument/references"))
    );
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .and_then(|value| value.pointer("/error/tool")),
        Some(&json!("references"))
    );

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

#[tokio::test]
async fn routes_hover_to_the_server_selected_by_the_file_extension()
-> Result<(), Box<dyn Error>> {
    let root = unique_dir("multi-server-routing")?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
    let server = support::mock_lsp_server()?;
    let config_path = root.join("deixis.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[servers.python]
command = {}
args = ["--mode", "hover-unsupported"]
file_extensions = {{ ".py" = "python" }}

[servers.rust]
command = {}
args = ["--mode", "hover-utf-8"]
file_extensions = {{ ".rs" = "rust" }}
"#,
            support::toml_string(&server),
            support::toml_string(&server),
        ),
    )?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let arguments = json!({
        "path": "main.rs",
        "position": { "line": 0, "character": 8 },
    })
    .as_object()
    .unwrap()
    .clone();

    let result = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("hover").with_arguments(arguments),
        ),
    )
    .await??;

    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        result.structured_content.as_ref().and_then(|value| value
            .get("contents")
            .and_then(|contents| contents.get("value"))
            .and_then(JsonValue::as_str)),
        Some("`answer`: the ultimate value")
    );

    let status_arguments = json!({
        "server": "python",
        "start": true,
    })
    .as_object()
    .unwrap()
    .clone();
    let status = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("deixis_server_status")
                .with_arguments(status_arguments),
        ),
    )
    .await??;
    assert_eq!(
        status
            .structured_content
            .as_ref()
            .and_then(|value| value.get("configuredName"))
            .and_then(JsonValue::as_str),
        Some("python")
    );

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}

fn unique_dir(name: &str) -> Result<PathBuf, std::io::Error> {
    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "deixis-{name}-{}-{nanos}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

async fn call_hover_error(
    mode: &str,
    path: &str,
    source: Option<&str>,
    line: u32,
    character: u32,
    request_timeout_ms: u64,
) -> Result<CallToolResult, Box<dyn Error>> {
    call_hover_error_with_response_limit(
        mode,
        path,
        source,
        line,
        character,
        request_timeout_ms,
        16 * 1024 * 1024,
    )
    .await
}

async fn call_hover_error_with_response_limit(
    mode: &str,
    path: &str,
    source: Option<&str>,
    line: u32,
    character: u32,
    request_timeout_ms: u64,
    max_response_bytes: usize,
) -> Result<CallToolResult, Box<dyn Error>> {
    let root = unique_dir(mode)?;
    if let Some(source) = source {
        fs::write(root.join(path), source)?;
    }
    let config_path = write_mock_config_for_mode_with_bounds(
        &root,
        mode,
        request_timeout_ms,
        max_response_bytes,
    )?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command
        .arg("--root")
        .arg(&root)
        .arg("--config")
        .arg(&config_path);
    let transport = TokioChildProcess::new(command)?;
    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let arguments = json!({
        "path": path,
        "position": { "line": line, "character": character },
    })
    .as_object()
    .unwrap()
    .clone();

    let result = timeout(
        Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("hover").with_arguments(arguments),
        ),
    )
    .await??;

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(result)
}

fn assert_tool_error(
    result: &CallToolResult,
    code: &str,
    server: Option<&str>,
    method: Option<&str>,
    path: &str,
) {
    assert_eq!(result.is_error, Some(true));
    let error = result
        .structured_content
        .as_ref()
        .and_then(|value| value.get("error"))
        .expect("tool errors should contain a structured error envelope");
    assert_eq!(error.get("code").and_then(JsonValue::as_str), Some(code));
    assert_eq!(error.get("tool").and_then(JsonValue::as_str), Some("hover"));
    assert_eq!(error.get("server").and_then(JsonValue::as_str), server);
    assert_eq!(error.get("method").and_then(JsonValue::as_str), method);
    assert_eq!(error.get("path").and_then(JsonValue::as_str), Some(path));
    let message = error
        .get("message")
        .and_then(JsonValue::as_str)
        .expect("tool errors should contain a readable message");
    assert_eq!(
        result
            .content
            .first()
            .and_then(|content| content.as_text())
            .map(|content| content.text.as_str()),
        Some(message)
    );
}

fn write_mock_config(
    root: &std::path::Path,
) -> Result<PathBuf, Box<dyn Error>> {
    write_mock_config_for_mode(root, "normal")
}

fn write_mock_config_for_mode(
    root: &std::path::Path,
    mode: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    write_mock_config_for_mode_with_timeout(root, mode, 1_000)
}

fn write_mock_config_for_mode_with_timeout(
    root: &std::path::Path,
    mode: &str,
    request_timeout_ms: u64,
) -> Result<PathBuf, Box<dyn Error>> {
    write_mock_config_for_mode_with_bounds(
        root,
        mode,
        request_timeout_ms,
        16 * 1024 * 1024,
    )
}

fn write_mock_config_for_mode_with_bounds(
    root: &std::path::Path,
    mode: &str,
    request_timeout_ms: u64,
    max_response_bytes: usize,
) -> Result<PathBuf, Box<dyn Error>> {
    let config_path = root.join("deixis.toml");
    write_mock_config_to_with_bounds(
        &config_path,
        mode,
        request_timeout_ms,
        max_response_bytes,
    )?;
    Ok(config_path)
}

fn write_mock_config_to(
    config_path: &std::path::Path,
    mode: &str,
) -> Result<(), Box<dyn Error>> {
    write_mock_config_to_with_timeout(config_path, mode, 1_000)
}

fn write_mock_config_to_with_timeout(
    config_path: &std::path::Path,
    mode: &str,
    request_timeout_ms: u64,
) -> Result<(), Box<dyn Error>> {
    write_mock_config_to_with_bounds(
        config_path,
        mode,
        request_timeout_ms,
        16 * 1024 * 1024,
    )
}

fn write_mock_config_to_with_bounds(
    config_path: &std::path::Path,
    mode: &str,
    request_timeout_ms: u64,
    max_response_bytes: usize,
) -> Result<(), Box<dyn Error>> {
    let server = support::mock_lsp_server()?;
    fs::write(
        config_path,
        format!(
            r#"
[servers.mock-lsp]
command = {}
args = ["--mode", {}]
file_extensions = {{ ".rs" = "rust" }}

[servers.mock-lsp.timeouts]
request_ms = {request_timeout_ms}
shutdown_ms = 1000

[servers.mock-lsp.limits]
max_response_bytes = {max_response_bytes}
"#,
            support::toml_string(&server),
            serde_json::to_string(mode)?,
        ),
    )?;
    Ok(())
}

fn write_workspace_symbol_config(
    root: &std::path::Path,
    barrier: &std::path::Path,
) -> Result<PathBuf, Box<dyn Error>> {
    let config_path = root.join("deixis.toml");
    let server = support::mock_lsp_server()?;
    fs::write(
        &config_path,
        format!(
            r#"
[servers.zeta]
command = {}
args = ["--mode", "workspace-symbol-release"]
file_extensions = {{ ".py" = "python" }}

[servers.zeta.environment]
DEIXIS_MOCK_WORKSPACE_ROLE = "release"
DEIXIS_MOCK_WORKSPACE_BARRIER = {}
DEIXIS_MOCK_WORKSPACE_NAME = "zetaSymbol"

[servers.zeta.timeouts]
request_ms = 1000
shutdown_ms = 1000

[servers.alpha]
command = {}
args = ["--mode", "workspace-symbol-wait-utf-16"]
file_extensions = {{ ".rs" = "rust" }}

[servers.alpha.environment]
DEIXIS_MOCK_WORKSPACE_ROLE = "wait"
DEIXIS_MOCK_WORKSPACE_BARRIER = {}
DEIXIS_MOCK_WORKSPACE_NAME = "alphaSymbol"

[servers.alpha.timeouts]
request_ms = 1000
shutdown_ms = 1000
"#,
            support::toml_string(&server),
            support::toml_string(barrier),
            support::toml_string(&server),
            support::toml_string(barrier),
        ),
    )?;
    Ok(config_path)
}

fn write_mixed_workspace_symbol_config(
    root: &std::path::Path,
) -> Result<PathBuf, Box<dyn Error>> {
    let config_path = root.join("deixis.toml");
    let server = support::mock_lsp_server()?;
    fs::write(
        &config_path,
        format!(
            r#"
[servers.alpha]
command = {}
args = ["--mode", "workspace-symbol-unsupported"]
file_extensions = {{ ".rs" = "rust" }}

[servers.zeta]
command = {}
args = ["--mode", "normal"]
file_extensions = {{ ".py" = "python" }}

[servers.zeta.environment]
DEIXIS_MOCK_WORKSPACE_NAME = "zetaSymbol"
"#,
            support::toml_string(&server),
            support::toml_string(&server),
        ),
    )?;
    Ok(config_path)
}

fn write_failure_isolation_config(
    root: &std::path::Path,
    completed: &std::path::Path,
) -> Result<PathBuf, Box<dyn Error>> {
    let config_path = root.join("deixis.toml");
    let server = support::mock_lsp_server()?;
    fs::write(
        &config_path,
        format!(
            r#"
[servers.alpha]
command = {}
args = ["--mode", "workspace-symbol-exit"]
file_extensions = {{ ".rs" = "rust" }}

[servers.zeta]
command = {}
args = ["--mode", "hover-utf-8"]
file_extensions = {{ ".py" = "python" }}

[servers.zeta.environment]
DEIXIS_MOCK_WORKSPACE_ROLE = "release"
DEIXIS_MOCK_WORKSPACE_BARRIER = {}
DEIXIS_MOCK_WORKSPACE_NAME = "zetaSymbol"
"#,
            support::toml_string(&server),
            support::toml_string(&server),
            support::toml_string(completed),
        ),
    )?;
    Ok(config_path)
}

async fn write_json_line(
    stdin: &mut ChildStdin,
    value: JsonValue,
) -> Result<(), Box<dyn Error>> {
    stdin.write_all(value.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_stdout_json_line(
    stdout: &mut BufReader<ChildStdout>,
) -> Result<JsonValue, Box<dyn Error>> {
    let mut line = String::new();
    let bytes =
        timeout(Duration::from_secs(10), stdout.read_line(&mut line)).await??;
    if bytes == 0 {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "deixis stdout closed before a JSON-RPC response",
        )));
    }

    parse_stdout_json_line(&line)
}

async fn read_remaining_stdout(
    mut stdout: BufReader<ChildStdout>,
) -> Result<Vec<JsonValue>, Box<dyn Error>> {
    let mut messages = Vec::new();
    loop {
        let mut line = String::new();
        let bytes =
            timeout(Duration::from_secs(10), stdout.read_line(&mut line))
                .await??;
        if bytes == 0 {
            return Ok(messages);
        }
        messages.push(parse_stdout_json_line(&line)?);
    }
}

fn parse_stdout_json_line(line: &str) -> Result<JsonValue, Box<dyn Error>> {
    serde_json::from_str(line.trim_end_matches(['\r', '\n'])).map_err(|error| {
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("deixis stdout line was not JSON: {line:?}: {error}"),
        )) as Box<dyn Error>
    })
}
