use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use deixis::{
    cli::CliOptions,
    lsp::{DefinitionLocation, LazyLanguageServer, LspError},
    positions::{Position, PositionEncoding, Range},
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
    assert_eq!(status.position_encoding(), Some(PositionEncoding::Utf8));
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
    let (manager, root) = configured_manager_with_root("normal", 1_000, 1_000)?;

    let probe: ProbeResponse =
        manager.request("mock/probeClient", JsonValue::Null).await?;

    assert_eq!(
        probe.configuration_values,
        json!([
            "alpha",
            2,
            null,
            {
                "answer": 42,
                "mock": {
                    "one": "alpha",
                    "two": 2,
                },
            },
        ])
    );
    assert_eq!(probe.workspace_folders, 1);
    assert_eq!(
        probe.workspace_folder_name,
        root.file_name()
            .expect("test root should have a file name")
            .to_string_lossy()
    );
    assert!(
        probe.workspace_folder_uri.starts_with("file://"),
        "{}",
        probe.workspace_folder_uri
    );
    assert!(
        probe
            .workspace_folder_uri
            .ends_with(&probe.workspace_folder_name),
        "{}",
        probe.workspace_folder_uri
    );
    assert!(probe.registered);
    assert!(probe.unregistered);
    assert!(!probe.apply_edit_applied);
    assert_eq!(
        probe.apply_edit_failure_reason,
        Some("Deixis is read-only".to_owned())
    );
    assert_eq!(probe.show_message_request_result, JsonValue::Null);
    assert_eq!(probe.unknown_error_code, -32601);
    assert_eq!(probe.position_encodings, ["utf-8", "utf-16", "utf-32"]);
    assert!(probe.hover_dynamic_registration);
    assert_eq!(probe.hover_content_formats, ["markdown", "plaintext"]);
    assert!(probe.definition_dynamic_registration);
    assert!(probe.definition_link_support);

    assert_eq!(manager.diagnostics().await.len(), 1);
    assert!(manager.dynamic_registrations().await.is_empty());

    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn synchronizes_full_documents_lazily_and_closes_them()
-> Result<(), Box<dyn Error>> {
    let (manager, root) =
        configured_manager_with_root("document-full", 1_000, 1_000)?;
    let source = root.join("main.rs");
    fs::write(&source, "fn answer() -> u8 { 42 }\n")?;

    assert!(!manager.status().await.started());
    let opened = manager.synchronize_document("main.rs", "rust").await?;
    assert_eq!(opened.version(), 1);
    assert!(manager.status().await.started());

    fs::write(&source, "fn answer() -> u8 { 42 }\n")?;
    manager.synchronize_document(&source, "rust").await?;

    fs::write(&source, "fn answer() -> u8 { 43 }\n")?;
    manager.synchronize_document("main.rs", "rust").await?;
    manager.synchronize_document("main.rs", "rust").await?;

    fs::write(&source, "fn answer() -> u8 { 42 }\n")?;
    manager.synchronize_document("main.rs", "rust").await?;

    let probe: DocumentEventsResponse = manager
        .request("mock/documentEvents", JsonValue::Null)
        .await?;
    assert_eq!(probe.open_documents, 1);
    assert_eq!(probe.events.len(), 3);

    let open = &probe.events[0];
    assert_eq!(open["method"], "textDocument/didOpen");
    assert_eq!(open["params"]["textDocument"]["languageId"], "rust");
    assert_eq!(open["params"]["textDocument"]["version"], 1);
    assert_eq!(
        open["params"]["textDocument"]["text"],
        "fn answer() -> u8 { 42 }\n"
    );
    assert!(
        open["params"]["textDocument"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/main.rs"))
    );

    let first_change = &probe.events[1];
    assert_eq!(first_change["method"], "textDocument/didChange");
    assert_eq!(first_change["params"]["textDocument"]["version"], 2);
    assert_eq!(
        first_change["params"]["contentChanges"],
        json!([{ "text": "fn answer() -> u8 { 43 }\n" }])
    );

    let second_change = &probe.events[2];
    assert_eq!(second_change["method"], "textDocument/didChange");
    assert_eq!(second_change["params"]["textDocument"]["version"], 3);
    assert_eq!(
        second_change["params"]["contentChanges"],
        json!([{ "text": "fn answer() -> u8 { 42 }\n" }])
    );

    fs::write(root.join("helper.rs"), "pub fn helper() {}\n")?;
    manager.synchronize_document("helper.rs", "rust").await?;

    // The mock rejects shutdown while any synchronized document remains open.
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn replaces_incrementally_synchronized_documents_as_one_range()
-> Result<(), Box<dyn Error>> {
    for (mode, expected_character) in [
        ("document-incremental-utf-8", 5),
        ("document-incremental-utf-16", 3),
        ("document-incremental-utf-32", 2),
    ] {
        let (manager, root) = configured_manager_with_root(mode, 1_000, 1_000)?;
        let source = root.join("unicode.rs");
        fs::write(&source, "first line\r\nsecond\r🦀x")?;

        manager.synchronize_document(&source, "rust").await?;
        fs::write(&source, "replacement\n")?;
        manager.synchronize_document(&source, "rust").await?;

        let probe: DocumentEventsResponse = manager
            .request("mock/documentEvents", JsonValue::Null)
            .await?;
        assert_eq!(probe.events.len(), 2, "{mode}");
        assert_eq!(
            probe.events[1]["method"], "textDocument/didChange",
            "{mode}"
        );
        assert_eq!(
            probe.events[1]["params"]["contentChanges"],
            json!([{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": {
                        "line": 2,
                        "character": expected_character,
                    },
                },
                "text": "replacement\n",
            }]),
            "{mode}"
        );

        manager.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn defaults_an_omitted_position_encoding_to_utf16()
-> Result<(), Box<dyn Error>> {
    let manager = configured_manager("position-omitted", 1_000, 1_000)?;

    let status = manager.ensure_started().await?;

    assert_eq!(status.position_encoding(), Some(PositionEncoding::Utf16));
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn rejects_unsupported_and_malformed_position_encodings()
-> Result<(), Box<dyn Error>> {
    for mode in ["position-unsupported", "position-malformed"] {
        let manager = configured_manager(mode, 50, 50)?;

        let error = manager.ensure_started().await.unwrap_err();

        assert!(
            matches!(error, LspError::UnsupportedPositionEncoding { .. }),
            "{mode}: {error}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn rejects_non_utf8_documents_without_opening_them()
-> Result<(), Box<dyn Error>> {
    let (manager, root) =
        configured_manager_with_root("document-full", 1_000, 1_000)?;
    fs::write(root.join("binary.rs"), [0xff, 0xfe])?;

    let error = manager
        .synchronize_document("binary.rs", "rust")
        .await
        .unwrap_err();
    assert!(matches!(error, LspError::ReadDocument { .. }));

    let probe: DocumentEventsResponse = manager
        .request("mock/documentEvents", JsonValue::Null)
        .await?;
    assert!(probe.events.is_empty());
    assert_eq!(probe.open_documents, 0);

    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn rejects_document_sync_when_the_server_does_not_support_it()
-> Result<(), Box<dyn Error>> {
    let (manager, root) =
        configured_manager_with_root("document-none", 1_000, 1_000)?;
    fs::write(root.join("main.rs"), "fn main() {}\n")?;

    let error = manager
        .synchronize_document("main.rs", "rust")
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        LspError::UnsupportedDocumentSynchronization { .. }
    ));

    let probe: DocumentEventsResponse = manager
        .request("mock/documentEvents", JsonValue::Null)
        .await?;
    assert!(probe.events.is_empty());

    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn rejects_hover_before_synchronizing_when_capability_is_disabled()
-> Result<(), Box<dyn Error>> {
    let (manager, root) =
        configured_manager_with_root("hover-unsupported", 1_000, 1_000)?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;

    let error = manager
        .hover("main.rs", "rust", Position::new(0, 8))
        .await
        .unwrap_err();
    assert!(matches!(error, LspError::UnsupportedCapability { .. }));

    let probe: DocumentEventsResponse = manager
        .request("mock/documentEvents", JsonValue::Null)
        .await?;
    assert!(probe.events.is_empty());
    assert_eq!(probe.open_documents, 0);

    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn normalizes_definition_responses_across_position_encodings()
-> Result<(), Box<dyn Error>> {
    for mode in [
        "definition-location-utf-8",
        "definition-locations-utf-16",
        "definition-links-utf-32",
        "definition-options",
    ] {
        let (manager, root) = configured_manager_with_root(mode, 1_000, 1_000)?;
        fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;

        let definitions = manager
            .definition("main.rs", "rust", Position::new(0, 8))
            .await?;

        assert_eq!(definitions.len(), 1, "{mode}");
        assert_eq!(
            definitions[0],
            DefinitionLocation {
                server: "mock-lsp".to_owned(),
                uri: definitions[0].uri.clone(),
                target_range: Range::new(
                    Position::new(0, 8),
                    Position::new(0, 14),
                ),
                target_selection_range: Range::new(
                    Position::new(0, 8),
                    Position::new(0, 14),
                ),
                target_position_encoding: PositionEncoding::Utf8,
                origin_selection_range: None,
            },
            "{mode}"
        );
        assert!(definitions[0].uri.ends_with("/main.rs"), "{mode}");

        manager.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn rejects_definition_before_synchronizing_when_capability_is_disabled()
-> Result<(), Box<dyn Error>> {
    let (manager, root) =
        configured_manager_with_root("definition-unsupported", 1_000, 1_000)?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;

    let error = manager
        .definition("main.rs", "rust", Position::new(0, 8))
        .await
        .unwrap_err();
    assert!(matches!(error, LspError::UnsupportedCapability { .. }));

    let probe: DocumentEventsResponse = manager
        .request("mock/documentEvents", JsonValue::Null)
        .await?;
    assert!(probe.events.is_empty());
    assert_eq!(probe.open_documents, 0);

    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn converts_project_definition_targets_and_preserves_external_targets()
-> Result<(), Box<dyn Error>> {
    let (manager, root) = configured_manager_with_root(
        "definition-project-target-utf-16",
        1_000,
        1_000,
    )?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;
    fs::write(root.join("helper.rs"), "pub 🦀answer\n")?;

    let definitions = manager
        .definition("main.rs", "rust", Position::new(0, 8))
        .await?;

    assert_eq!(definitions.len(), 1);
    assert!(definitions[0].uri.ends_with("/helper.rs"));
    assert_eq!(
        definitions[0].target_position_encoding,
        PositionEncoding::Utf8
    );
    assert_eq!(
        definitions[0].target_selection_range,
        Range::new(Position::new(0, 8), Position::new(0, 14))
    );
    assert_eq!(
        definitions[0].origin_selection_range,
        Some(Range::new(Position::new(0, 8), Position::new(0, 14),))
    );
    manager.shutdown().await?;

    let (manager, root) = configured_manager_with_root(
        "definition-external-utf-16",
        1_000,
        1_000,
    )?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;

    let definitions = manager
        .definition("main.rs", "rust", Position::new(0, 8))
        .await?;

    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].uri, "file:///deixis-external-do-not-read.rs");
    assert_eq!(
        definitions[0].target_position_encoding,
        PositionEncoding::Utf16
    );
    assert_eq!(
        definitions[0].target_selection_range,
        Range::new(Position::new(0, 6), Position::new(0, 12))
    );
    assert_eq!(
        definitions[0].origin_selection_range,
        Some(Range::new(Position::new(0, 8), Position::new(0, 14),))
    );
    manager.shutdown().await?;

    Ok(())
}

#[tokio::test]
async fn maps_a_null_definition_response_to_no_locations()
-> Result<(), Box<dyn Error>> {
    let (manager, root) =
        configured_manager_with_root("definition-null", 1_000, 1_000)?;
    fs::write(root.join("main.rs"), "let 🦀answer = 42;\n")?;

    let definitions = manager
        .definition("main.rs", "rust", Position::new(0, 8))
        .await?;

    assert!(definitions.is_empty());
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn continues_after_malformed_server_output() -> Result<(), Box<dyn Error>>
{
    let manager = configured_manager("malformed", 1_000, 1_000)?;

    let echo: EchoResponse = manager
        .request(
            "mock/malformedThenEcho",
            json!({ "message": "after malformed output" }),
        )
        .await?;

    assert_eq!(echo.echo, json!({ "message": "after malformed output" }));
    assert!(echo.initialized);

    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn clean_shutdown_does_not_wait_for_full_timeout()
-> Result<(), Box<dyn Error>> {
    let manager = configured_manager("normal", 1_000, 5_000)?;

    let _: EchoResponse = manager
        .request("mock/echo", json!({ "message": "started" }))
        .await?;

    let started = Instant::now();
    let outcome = manager.shutdown().await?;

    assert!(outcome.started());
    assert!(outcome.shutdown_response_received());
    assert!(!outcome.forced());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "clean shutdown waited for {:?}",
        started.elapsed()
    );
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
    configured_manager_with_root(mode, request_timeout_ms, shutdown_timeout_ms)
        .map(|(manager, _root)| manager)
}

fn configured_manager_with_root(
    mode: &str,
    request_timeout_ms: u64,
    shutdown_timeout_ms: u64,
) -> Result<(LazyLanguageServer, PathBuf), Box<dyn Error>> {
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

    Ok((
        LazyLanguageServer::new(config, startup.project().clone()),
        root,
    ))
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
[servers.mock-lsp]
command = {}
args = ["--mode", "{}"]
file_extensions = {{ ".rs" = "rust" }}

[servers.mock-lsp.timeouts]
request_ms = {}
shutdown_ms = {}

[servers.mock-lsp.initialization_options]
answer = 42

[servers.mock-lsp.initialization_options.mock]
one = "alpha"
two = 2
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
    configuration_values: JsonValue,
    workspace_folders: usize,
    workspace_folder_uri: String,
    workspace_folder_name: String,
    registered: bool,
    unregistered: bool,
    apply_edit_applied: bool,
    apply_edit_failure_reason: Option<String>,
    show_message_request_result: JsonValue,
    unknown_error_code: i64,
    position_encodings: Vec<String>,
    hover_dynamic_registration: bool,
    hover_content_formats: Vec<String>,
    definition_dynamic_registration: bool,
    definition_link_support: bool,
}

#[derive(Debug, Deserialize)]
struct DocumentEventsResponse {
    events: Vec<JsonValue>,
    open_documents: usize,
}
