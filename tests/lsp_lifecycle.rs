use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use deixis::{
    cli::CliOptions,
    lsp::{LazyLanguageServer, LspError},
    project::StartupState,
};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};

mod support;

#[tokio::test]
async fn starts_lazily_initializes_and_shuts_down() -> Result<(), Box<dyn Error>>
{
    let manager = configured_manager("normal", 1_000, 1_000)?;

    assert!(!manager.status().await.started());

    let echo: EchoResponse = manager
        .request("mock/echo", json!({ "message": "hello" }))
        .await?;
    assert_eq!(echo.echo, json!({ "message": "hello" }));
    assert!(echo.initialized);

    let status = manager.status().await;
    assert!(status.started());
    assert_eq!(status.configured_name(), "mock-lsp");
    assert_eq!(status.server_name(), Some("deixis-mock-lsp"));
    assert_eq!(status.server_version(), Some("0.1.0"));
    assert_eq!(status.position_encoding(), Some("utf-8"));
    assert_eq!(status.text_document_sync(), Some(&json!(1)));
    assert!(status.capabilities().get("hoverProvider").is_some());

    let initialized: InitializedResponse =
        manager.request("mock/initialized", JsonValue::Null).await?;
    assert!(initialized.initialized);

    let outcome = manager.shutdown().await?;
    assert!(outcome.started());
    assert!(outcome.shutdown_response_received());
    assert!(!outcome.forced());
    Ok(())
}

#[tokio::test]
async fn request_timeout_cancels_only_the_pending_request()
-> Result<(), Box<dyn Error>> {
    let manager = configured_manager("normal", 100, 1_000)?;

    let error = manager
        .request::<JsonValue>("mock/delay", json!({ "delay_ms": 500 }))
        .await
        .unwrap_err();
    assert!(matches!(error, LspError::RequestTimeout { .. }));

    let cancellation: CancellationResponse =
        manager.request("mock/cancelled", JsonValue::Null).await?;
    assert!(cancellation.cancelled);

    let echo: EchoResponse = manager
        .request("mock/echo", json!({ "message": "still alive" }))
        .await?;
    assert_eq!(echo.echo, json!({ "message": "still alive" }));

    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn handles_common_server_to_client_messages() -> Result<(), Box<dyn Error>>
{
    let manager = configured_manager("normal", 1_000, 1_000)?;

    let probe: ProbeResponse =
        manager.request("mock/probeClient", JsonValue::Null).await?;

    assert_eq!(probe.configuration_items, 2);
    assert_eq!(probe.workspace_folders, 1);
    assert!(probe.registered);
    assert!(!probe.apply_edit_applied);
    assert_eq!(probe.unknown_error_code, -32601);

    assert_eq!(manager.diagnostics().await.len(), 1);
    assert_eq!(manager.dynamic_registrations().await.len(), 1);

    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn force_kills_an_unresponsive_server_on_shutdown()
-> Result<(), Box<dyn Error>> {
    let manager = configured_manager("ignore-shutdown", 1_000, 100)?;

    let _: EchoResponse = manager
        .request("mock/echo", json!({ "message": "started" }))
        .await?;

    let outcome = manager.shutdown().await?;

    assert!(outcome.started());
    assert!(!outcome.shutdown_response_received());
    assert!(outcome.forced());
    Ok(())
}

fn configured_manager(
    mode: &str,
    request_timeout_ms: u64,
    shutdown_timeout_ms: u64,
) -> Result<LazyLanguageServer, Box<dyn Error>> {
    let root = support::unique_dir(mode)?;
    let config_path =
        write_config(&root, mode, request_timeout_ms, shutdown_timeout_ms)?;
    let options = CliOptions::new(Some(config_path), Some(root.clone()));
    let startup = StartupState::from_options_in(options, &root)?;
    let config = startup
        .config()
        .expect("test config should be loaded")
        .servers()[0]
        .clone();

    Ok(LazyLanguageServer::new(config, startup.project().clone()))
}

fn write_config(
    root: &Path,
    mode: &str,
    request_timeout_ms: u64,
    shutdown_timeout_ms: u64,
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
args = ["--mode", "{}"]
language_ids = ["rust"]

[servers.timeouts]
request_ms = {}
shutdown_ms = {}

[servers.initialization_options]
answer = 42
"#,
            support::toml_string(&server),
            mode,
            request_timeout_ms,
            shutdown_timeout_ms,
        ),
    )?;
    Ok(config_path)
}

#[derive(Debug, Deserialize)]
struct EchoResponse {
    echo: JsonValue,
    initialized: bool,
}

#[derive(Debug, Deserialize)]
struct InitializedResponse {
    initialized: bool,
}

#[derive(Debug, Deserialize)]
struct CancellationResponse {
    cancelled: bool,
}

#[derive(Debug, Deserialize)]
struct ProbeResponse {
    configuration_items: usize,
    workspace_folders: usize,
    registered: bool,
    apply_edit_applied: bool,
    unknown_error_code: i64,
}
