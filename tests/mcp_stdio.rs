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
    model::{CallToolRequestParams, ServerCapabilities},
    transport::TokioChildProcess,
};
use serde_json::{Value as JsonValue, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
    time::timeout,
};

mod support;

#[tokio::test]
async fn negotiates_an_empty_mcp_server_over_stdio()
-> Result<(), Box<dyn Error>> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command.env("RUST_LOG", "deixis=debug");
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command.arg("--root").arg(&root);
    command.env("RUST_LOG", "deixis=debug");
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
            Some(&json!(["path", "languageId", "position"])),
            "{mode}"
        );
        assert!(hover_tool.output_schema.is_some(), "{mode}");

        let arguments = json!({
            "path": "main.rs",
            "languageId": "rust",
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
        "languageId": "rust",
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

fn write_mock_config(
    root: &std::path::Path,
) -> Result<PathBuf, Box<dyn Error>> {
    write_mock_config_for_mode(root, "normal")
}

fn write_mock_config_for_mode(
    root: &std::path::Path,
    mode: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    let server = support::mock_lsp_server()?;
    let config_path = root.join("deixis.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[[servers]]
name = "mock-lsp"
command = {}
args = ["--mode", {}]
language_ids = ["rust"]

[servers.timeouts]
request_ms = 1000
shutdown_ms = 1000
"#,
            support::toml_string(&server),
            serde_json::to_string(mode)?,
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
