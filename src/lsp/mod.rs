use std::{
    collections::BTreeMap,
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value as JsonValue, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdout},
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tracing::{debug, info, warn};

use crate::{config::LanguageServerConfig, project::Project};

const JSONRPC_VERSION: &str = "2.0";
const METHOD_NOT_FOUND: i64 = -32601;

pub struct LazyLanguageServer {
    config: LanguageServerConfig,
    project: Project,
    state: Mutex<Option<RunningServer>>,
}

impl LazyLanguageServer {
    pub fn new(config: LanguageServerConfig, project: Project) -> Self {
        Self {
            config,
            project,
            state: Mutex::new(None),
        }
    }

    pub async fn request<T>(
        &self,
        method: &str,
        params: JsonValue,
    ) -> Result<T, LspError>
    where
        T: DeserializeOwned,
    {
        let active = self.active_server().await?;
        let value = active
            .request_value(method, params, self.config.timeouts().request())
            .await?;
        serde_json::from_value(value).map_err(LspError::DecodeResult)
    }

    pub async fn status(&self) -> ServerSnapshot {
        let active = {
            let state = self.state.lock().await;
            state.as_ref().map(|running| running.active.clone())
        };

        match active {
            Some(active) => active.status.lock().await.clone(),
            None => ServerSnapshot::not_started(self.config.name()),
        }
    }

    pub async fn diagnostics(&self) -> Vec<JsonValue> {
        let active = {
            let state = self.state.lock().await;
            state.as_ref().map(|running| running.active.clone())
        };

        match active {
            Some(active) => active.diagnostics.lock().await.clone(),
            None => Vec::new(),
        }
    }

    pub async fn dynamic_registrations(&self) -> Vec<JsonValue> {
        let active = {
            let state = self.state.lock().await;
            state.as_ref().map(|running| running.active.clone())
        };

        match active {
            Some(active) => active.registrations.lock().await.clone(),
            None => Vec::new(),
        }
    }

    pub async fn shutdown(&self) -> Result<ShutdownOutcome, LspError> {
        let running = self.state.lock().await.take();

        match running {
            Some(running) => {
                running.shutdown(self.config.timeouts().shutdown()).await
            }
            None => Ok(ShutdownOutcome::not_started()),
        }
    }

    async fn active_server(&self) -> Result<ActiveServer, LspError> {
        let mut state = self.state.lock().await;
        if let Some(running) = state.as_ref() {
            return Ok(running.active.clone());
        }

        let running = RunningServer::spawn(&self.config, &self.project).await?;
        let active = running.active.clone();
        *state = Some(running);
        Ok(active)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerSnapshot {
    configured_name: String,
    started: bool,
    server_name: Option<String>,
    server_version: Option<String>,
    capabilities: JsonValue,
    text_document_sync: Option<JsonValue>,
    position_encoding: Option<String>,
}

impl ServerSnapshot {
    fn not_started(configured_name: &str) -> Self {
        Self {
            configured_name: configured_name.to_owned(),
            started: false,
            server_name: None,
            server_version: None,
            capabilities: JsonValue::Null,
            text_document_sync: None,
            position_encoding: None,
        }
    }

    fn initialized(
        configured_name: &str,
        initialize_result: &JsonValue,
    ) -> Self {
        let capabilities = initialize_result
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let server_info = initialize_result.get("serverInfo");
        let server_name = server_info
            .and_then(|value| value.get("name"))
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        let server_version = server_info
            .and_then(|value| value.get("version"))
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        let text_document_sync = capabilities.get("textDocumentSync").cloned();
        let position_encoding = capabilities
            .get("positionEncoding")
            .and_then(JsonValue::as_str)
            .map(str::to_owned);

        Self {
            configured_name: configured_name.to_owned(),
            started: true,
            server_name,
            server_version,
            capabilities,
            text_document_sync,
            position_encoding,
        }
    }

    pub fn configured_name(&self) -> &str {
        &self.configured_name
    }

    pub fn started(&self) -> bool {
        self.started
    }

    pub fn server_name(&self) -> Option<&str> {
        self.server_name.as_deref()
    }

    pub fn server_version(&self) -> Option<&str> {
        self.server_version.as_deref()
    }

    pub fn capabilities(&self) -> &JsonValue {
        &self.capabilities
    }

    pub fn text_document_sync(&self) -> Option<&JsonValue> {
        self.text_document_sync.as_ref()
    }

    pub fn position_encoding(&self) -> Option<&str> {
        self.position_encoding.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownOutcome {
    started: bool,
    shutdown_response_received: bool,
    forced: bool,
    exit_status: Option<ExitStatus>,
}

impl ShutdownOutcome {
    fn not_started() -> Self {
        Self {
            started: false,
            shutdown_response_received: false,
            forced: false,
            exit_status: None,
        }
    }

    fn stopped(
        shutdown_response_received: bool,
        forced: bool,
        exit_status: ExitStatus,
    ) -> Self {
        Self {
            started: true,
            shutdown_response_received,
            forced,
            exit_status: Some(exit_status),
        }
    }

    pub fn started(self) -> bool {
        self.started
    }

    pub fn shutdown_response_received(self) -> bool {
        self.shutdown_response_received
    }

    pub fn forced(self) -> bool {
        self.forced
    }

    pub fn exit_status(self) -> Option<ExitStatus> {
        self.exit_status
    }
}

#[derive(Debug)]
pub enum LspError {
    Spawn {
        server: String,
        source: io::Error,
    },
    MissingPipe {
        server: String,
        pipe: &'static str,
    },
    EncodeMessage(serde_json::Error),
    DecodeResult(serde_json::Error),
    TransportClosed {
        server: String,
    },
    RequestCanceled {
        server: String,
        method: String,
    },
    RequestTimeout {
        server: String,
        method: String,
        timeout: Duration,
    },
    ResponseError {
        server: String,
        method: String,
        code: i64,
        message: String,
        data: Option<JsonValue>,
    },
    Shutdown {
        server: String,
        source: io::Error,
    },
}

impl fmt::Display for LspError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { server, source } => {
                write!(
                    formatter,
                    "failed to start language server `{server}`: {source}"
                )
            }
            Self::MissingPipe { server, pipe } => {
                write!(
                    formatter,
                    "language server `{server}` did not expose {pipe}"
                )
            }
            Self::EncodeMessage(source) => {
                write!(formatter, "failed to encode LSP message: {source}")
            }
            Self::DecodeResult(source) => {
                write!(formatter, "failed to decode LSP response: {source}")
            }
            Self::TransportClosed { server } => {
                write!(formatter, "language server `{server}` transport closed")
            }
            Self::RequestCanceled { server, method } => {
                write!(
                    formatter,
                    "language server `{server}` request `{method}` was canceled"
                )
            }
            Self::RequestTimeout {
                server,
                method,
                timeout,
            } => {
                write!(
                    formatter,
                    "language server `{server}` request `{method}` timed out after {} ms",
                    timeout.as_millis()
                )
            }
            Self::ResponseError {
                server,
                method,
                code,
                message,
                ..
            } => {
                write!(
                    formatter,
                    "language server `{server}` request `{method}` failed with LSP error {code}: {message}"
                )
            }
            Self::Shutdown { server, source } => {
                write!(
                    formatter,
                    "failed to stop language server `{server}`: {source}"
                )
            }
        }
    }
}

impl Error for LspError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn { source, .. } | Self::Shutdown { source, .. } => {
                Some(source)
            }
            Self::EncodeMessage(source) | Self::DecodeResult(source) => {
                Some(source)
            }
            Self::MissingPipe { .. }
            | Self::TransportClosed { .. }
            | Self::RequestCanceled { .. }
            | Self::RequestTimeout { .. }
            | Self::ResponseError { .. } => None,
        }
    }
}

struct RunningServer {
    active: ActiveServer,
    child: Child,
    reader_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
}

impl RunningServer {
    async fn spawn(
        config: &LanguageServerConfig,
        project: &Project,
    ) -> Result<Self, LspError> {
        let mut command = config.to_command();
        command
            .current_dir(project.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|source| LspError::Spawn {
            server: config.name().to_owned(),
            source,
        })?;
        let stdin =
            child.stdin.take().ok_or_else(|| LspError::MissingPipe {
                server: config.name().to_owned(),
                pipe: "stdin",
            })?;
        let stdout =
            child.stdout.take().ok_or_else(|| LspError::MissingPipe {
                server: config.name().to_owned(),
                pipe: "stdout",
            })?;
        let stderr =
            child.stderr.take().ok_or_else(|| LspError::MissingPipe {
                server: config.name().to_owned(),
                pipe: "stderr",
            })?;

        let (sender, receiver) = mpsc::channel(64);
        let active = ActiveServer::new(config, project, sender);
        let writer_task = tokio::spawn(writer_loop(
            config.name().to_owned(),
            stdin,
            receiver,
        ));
        let reader_task = tokio::spawn(reader_loop(
            config.name().to_owned(),
            stdout,
            active.clone(),
        ));
        let stderr_task =
            tokio::spawn(stderr_loop(config.name().to_owned(), stderr));

        let mut running = Self {
            active,
            child,
            reader_task,
            stderr_task,
            writer_task,
        };

        if let Err(error) = running.initialize(config, project).await {
            let _ = running.force_stop(config.timeouts().shutdown()).await;
            return Err(error);
        }

        Ok(running)
    }

    async fn initialize(
        &mut self,
        config: &LanguageServerConfig,
        project: &Project,
    ) -> Result<(), LspError> {
        let initialize_result = self
            .active
            .request_value(
                "initialize",
                initialize_params(config, project),
                config.timeouts().request(),
            )
            .await?;
        let snapshot =
            ServerSnapshot::initialized(config.name(), &initialize_result);
        *self.active.status.lock().await = snapshot;
        self.active
            .send_notification("initialized", json!({}))
            .await?;
        Ok(())
    }

    async fn shutdown(
        mut self,
        shutdown_timeout: Duration,
    ) -> Result<ShutdownOutcome, LspError> {
        let shutdown_response_received = match self
            .active
            .request_value("shutdown", JsonValue::Null, shutdown_timeout)
            .await
        {
            Ok(_) => true,
            Err(LspError::RequestTimeout { .. }) => false,
            Err(error) => {
                let _ = self.force_stop(shutdown_timeout).await;
                return Err(error);
            }
        };

        let _ = self.active.send_notification("exit", JsonValue::Null).await;
        let (forced, exit_status) = self.wait_or_kill(shutdown_timeout).await?;
        *self.active.status.lock().await =
            ServerSnapshot::not_started(&self.active.name);
        self.join_io_tasks(shutdown_timeout).await;
        Ok(ShutdownOutcome::stopped(
            shutdown_response_received,
            forced,
            exit_status,
        ))
    }

    async fn force_stop(
        &mut self,
        shutdown_timeout: Duration,
    ) -> Result<ShutdownOutcome, LspError> {
        let (forced, exit_status) = self.wait_or_kill(shutdown_timeout).await?;
        self.join_io_tasks(shutdown_timeout).await;
        Ok(ShutdownOutcome::stopped(false, forced, exit_status))
    }

    async fn wait_or_kill(
        &mut self,
        shutdown_timeout: Duration,
    ) -> Result<(bool, ExitStatus), LspError> {
        match timeout(shutdown_timeout, self.child.wait()).await {
            Ok(result) => {
                result.map(|status| (false, status)).map_err(|source| {
                    LspError::Shutdown {
                        server: self.active.name.clone(),
                        source,
                    }
                })
            }
            Err(_) => {
                self.child.kill().await.map_err(|source| {
                    LspError::Shutdown {
                        server: self.active.name.clone(),
                        source,
                    }
                })?;
                self.child
                    .wait()
                    .await
                    .map(|status| (true, status))
                    .map_err(|source| LspError::Shutdown {
                        server: self.active.name.clone(),
                        source,
                    })
            }
        }
    }

    async fn join_io_tasks(&mut self, shutdown_timeout: Duration) {
        let _ = timeout(shutdown_timeout, &mut self.reader_task).await;
        let _ = timeout(shutdown_timeout, &mut self.stderr_task).await;
        let _ = timeout(shutdown_timeout, &mut self.writer_task).await;
    }
}

#[derive(Clone)]
struct ActiveServer {
    name: String,
    sender: mpsc::Sender<Vec<u8>>,
    pending: Arc<Mutex<BTreeMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    next_request_id: Arc<AtomicU64>,
    project_root: PathBuf,
    configuration: JsonValue,
    status: Arc<Mutex<ServerSnapshot>>,
    diagnostics: Arc<Mutex<Vec<JsonValue>>>,
    registrations: Arc<Mutex<Vec<JsonValue>>>,
}

impl ActiveServer {
    fn new(
        config: &LanguageServerConfig,
        project: &Project,
        sender: mpsc::Sender<Vec<u8>>,
    ) -> Self {
        Self {
            name: config.name().to_owned(),
            sender,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            next_request_id: Arc::new(AtomicU64::new(1)),
            project_root: project.root().to_path_buf(),
            configuration: config.initialization_options().clone(),
            status: Arc::new(Mutex::new(ServerSnapshot::not_started(
                config.name(),
            ))),
            diagnostics: Arc::new(Mutex::new(Vec::new())),
            registrations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn request_value(
        &self,
        method: &str,
        params: JsonValue,
        request_timeout: Duration,
    ) -> Result<JsonValue, LspError> {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);

        let message = json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(error) = self.send_message(message).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }

        match timeout(request_timeout, receiver).await {
            Ok(Ok(response)) => {
                response.into_result(&self.name, method.to_owned())
            }
            Ok(Err(_)) => Err(LspError::RequestCanceled {
                server: self.name.clone(),
                method: method.to_owned(),
            }),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                let _ = self
                    .send_notification("$/cancelRequest", json!({ "id": id }))
                    .await;
                Err(LspError::RequestTimeout {
                    server: self.name.clone(),
                    method: method.to_owned(),
                    timeout: request_timeout,
                })
            }
        }
    }

    async fn send_notification(
        &self,
        method: &str,
        params: JsonValue,
    ) -> Result<(), LspError> {
        self.send_message(json!({
            "jsonrpc": JSONRPC_VERSION,
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn send_message(&self, message: JsonValue) -> Result<(), LspError> {
        let body =
            serde_json::to_vec(&message).map_err(LspError::EncodeMessage)?;
        self.sender
            .send(body)
            .await
            .map_err(|_| LspError::TransportClosed {
                server: self.name.clone(),
            })
    }
}

#[derive(Debug)]
struct JsonRpcResponse {
    result: Option<JsonValue>,
    error: Option<ResponseError>,
}

impl JsonRpcResponse {
    fn into_result(
        self,
        server: &str,
        method: String,
    ) -> Result<JsonValue, LspError> {
        if let Some(error) = self.error {
            return Err(LspError::ResponseError {
                server: server.to_owned(),
                method,
                code: error.code,
                message: error.message,
                data: error.data,
            });
        }

        Ok(self.result.unwrap_or(JsonValue::Null))
    }
}

#[derive(Debug, Deserialize)]
struct RawResponse {
    result: Option<JsonValue>,
    error: Option<ResponseError>,
}

#[derive(Debug, Deserialize)]
struct ResponseError {
    code: i64,
    message: String,
    data: Option<JsonValue>,
}

async fn writer_loop(
    server: String,
    mut stdin: tokio::process::ChildStdin,
    mut receiver: mpsc::Receiver<Vec<u8>>,
) {
    while let Some(body) = receiver.recv().await {
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        if let Err(error) = stdin.write_all(header.as_bytes()).await {
            warn!(server = %server, %error, "failed to write LSP header");
            break;
        }
        if let Err(error) = stdin.write_all(&body).await {
            warn!(server = %server, %error, "failed to write LSP body");
            break;
        }
        if let Err(error) = stdin.flush().await {
            warn!(server = %server, %error, "failed to flush LSP message");
            break;
        }
    }
}

async fn reader_loop(
    server: String,
    stdout: ChildStdout,
    active: ActiveServer,
) {
    let mut reader = BufReader::new(stdout);

    loop {
        match read_lsp_message(&mut reader).await {
            Ok(Some(message)) => {
                handle_incoming_message(&server, &active, message).await
            }
            Ok(None) => break,
            Err(ReadError::Json(error)) => {
                warn!(
                    server = %server,
                    %error,
                    "language server emitted invalid JSON-RPC body"
                );
            }
            Err(error) => {
                warn!(
                    server = %server,
                    %error,
                    "language server stdout reader stopped"
                );
                break;
            }
        }
    }
}

async fn stderr_loop(server: String, stderr: ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                info!(
                    server = %server,
                    message = %line.trim_end(),
                    "language server stderr"
                );
            }
            Err(error) => {
                warn!(server = %server, %error, "failed to read language server stderr");
                break;
            }
        }
    }
}

async fn handle_incoming_message(
    server: &str,
    active: &ActiveServer,
    message: JsonValue,
) {
    if let Some(method) = message.get("method").and_then(JsonValue::as_str) {
        if let Some(id) = message.get("id").cloned() {
            let params =
                message.get("params").cloned().unwrap_or(JsonValue::Null);
            handle_server_request(active, id, method, params).await;
        } else {
            let params =
                message.get("params").cloned().unwrap_or(JsonValue::Null);
            handle_server_notification(server, active, method, params).await;
        }
        return;
    }

    let Some(id) = message.get("id").and_then(JsonValue::as_u64) else {
        warn!(
            server = %server,
            "language server response omitted a numeric request id"
        );
        return;
    };

    let response = match serde_json::from_value::<RawResponse>(message) {
        Ok(response) => JsonRpcResponse {
            result: response.result,
            error: response.error,
        },
        Err(error) => {
            warn!(server = %server, %error, "failed to decode LSP response");
            return;
        }
    };

    if let Some(sender) = active.pending.lock().await.remove(&id) {
        let _ = sender.send(response);
    } else {
        debug!(server = %server, request_id = id, "ignoring stale LSP response");
    }
}

async fn handle_server_request(
    active: &ActiveServer,
    id: JsonValue,
    method: &str,
    params: JsonValue,
) {
    match method {
        "workspace/configuration" => {
            let items = params
                .get("items")
                .and_then(JsonValue::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            let result = JsonValue::Array(
                (0..items).map(|_| active.configuration.clone()).collect(),
            );
            let _ = active.send_message(success_response(id, result)).await;
        }
        "workspace/workspaceFolders" => {
            let uri = path_to_file_uri(&active.project_root);
            let name = workspace_name(&active.project_root);
            let _ = active
                .send_message(success_response(
                    id,
                    json!([{ "uri": uri, "name": name }]),
                ))
                .await;
        }
        "client/registerCapability" => {
            active.registrations.lock().await.push(params);
            let _ = active
                .send_message(success_response(id, JsonValue::Null))
                .await;
        }
        "client/unregisterCapability" => {
            active.registrations.lock().await.push(params);
            let _ = active
                .send_message(success_response(id, JsonValue::Null))
                .await;
        }
        "workspace/applyEdit" => {
            let _ = active
                .send_message(success_response(
                    id,
                    json!({
                        "applied": false,
                        "failureReason": "Deixis is read-only",
                    }),
                ))
                .await;
        }
        _ => {
            let _ = active
                .send_message(error_response(
                    id,
                    METHOD_NOT_FOUND,
                    format!("unsupported server-to-client request `{method}`"),
                ))
                .await;
        }
    }
}

async fn handle_server_notification(
    server: &str,
    active: &ActiveServer,
    method: &str,
    params: JsonValue,
) {
    match method {
        "textDocument/publishDiagnostics" => {
            active.diagnostics.lock().await.push(params);
        }
        "window/logMessage" | "window/showMessage" => {
            if let Some(message) =
                params.get("message").and_then(JsonValue::as_str)
            {
                info!(server = %server, method, message, "language server message");
            }
        }
        _ => {
            debug!(server = %server, method, "ignored language server notification");
        }
    }
}

fn success_response(id: JsonValue, result: JsonValue) -> JsonValue {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    })
}

fn error_response(
    id: JsonValue,
    code: i64,
    message: impl Into<String>,
) -> JsonValue {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        },
    })
}

fn initialize_params(
    config: &LanguageServerConfig,
    project: &Project,
) -> JsonValue {
    let root_uri = path_to_file_uri(project.root());
    let workspace_name = workspace_name(project.root());

    json!({
        "processId": std::process::id(),
        "clientInfo": {
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
        },
        "rootPath": project.root().to_string_lossy(),
        "rootUri": root_uri,
        "workspaceFolders": [{
            "uri": root_uri,
            "name": workspace_name,
        }],
        "capabilities": {
            "workspace": {
                "configuration": true,
                "workspaceFolders": true,
                "didChangeConfiguration": {
                    "dynamicRegistration": true,
                },
            },
            "textDocument": {},
        },
        "initializationOptions": config.initialization_options(),
    })
}

fn path_to_file_uri(path: &Path) -> String {
    let mut path = path.to_string_lossy().replace('\\', "/");
    if !path.starts_with('/') {
        path.insert(0, '/');
    }

    format!("file://{}", percent_encode_path(&path))
}

fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::new();
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'/'
            | b':'
            | b'-'
            | b'.'
            | b'_'
            | b'~' => encoded.push(byte as char),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn workspace_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

#[derive(Debug)]
enum ReadError {
    Io(io::Error),
    MissingContentLength,
    InvalidContentLength(String),
    Json(serde_json::Error),
}

impl fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(formatter, "{source}"),
            Self::MissingContentLength => {
                formatter.write_str("missing Content-Length header")
            }
            Self::InvalidContentLength(value) => {
                write!(formatter, "invalid Content-Length header `{value}`")
            }
            Self::Json(source) => write!(formatter, "{source}"),
        }
    }
}

impl Error for ReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Json(source) => Some(source),
            Self::MissingContentLength | Self::InvalidContentLength(_) => None,
        }
    }
}

async fn read_lsp_message(
    reader: &mut BufReader<ChildStdout>,
) -> Result<Option<JsonValue>, ReadError> {
    let mut content_length = None;
    let mut line = String::new();

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).await.map_err(ReadError::Io)?;
        if bytes == 0 {
            return Ok(None);
        }

        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }

        if let Some(value) = header.strip_prefix("Content-Length:") {
            let value = value.trim();
            content_length = Some(value.parse::<usize>().map_err(|_| {
                ReadError::InvalidContentLength(value.to_owned())
            })?);
        }
    }

    let content_length =
        content_length.ok_or(ReadError::MissingContentLength)?;
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).await.map_err(ReadError::Io)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(ReadError::Json)
}
