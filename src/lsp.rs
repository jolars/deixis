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

use crate::{
    config::LanguageServerConfig,
    documents::{
        DocumentStore, DocumentStoreError, DocumentUpdate, SynchronizedDocument,
    },
    positions::{PositionConverter, PositionEncoding, PositionError},
    project::{Project, ProjectFile, ProjectPathError},
};

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

    pub async fn ensure_started(&self) -> Result<ServerSnapshot, LspError> {
        let active = self.active_server().await?;
        Ok(active.status.lock().await.clone())
    }

    pub async fn synchronize_document(
        &self,
        path: impl AsRef<Path>,
        language_id: &str,
    ) -> Result<SynchronizedDocument, LspError> {
        let file = self
            .project
            .resolve_file(path)
            .map_err(LspError::DocumentPath)?;
        let active = self.active_server().await?;
        active.synchronize_document(file, language_id).await
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
            Some(active) => active
                .registrations
                .lock()
                .await
                .values()
                .cloned()
                .collect(),
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
    position_encoding: Option<PositionEncoding>,
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
    ) -> Result<Self, LspError> {
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
        let position_encoding = match capabilities.get("positionEncoding") {
            None => PositionEncoding::default(),
            Some(JsonValue::String(value)) => value.parse().map_err(|_| {
                LspError::UnsupportedPositionEncoding {
                    server: configured_name.to_owned(),
                    encoding: JsonValue::String(value.clone()),
                }
            })?,
            Some(value) => {
                return Err(LspError::UnsupportedPositionEncoding {
                    server: configured_name.to_owned(),
                    encoding: value.clone(),
                });
            }
        };

        Ok(Self {
            configured_name: configured_name.to_owned(),
            started: true,
            server_name,
            server_version,
            capabilities,
            text_document_sync,
            position_encoding: Some(position_encoding),
        })
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

    pub fn position_encoding(&self) -> Option<PositionEncoding> {
        self.position_encoding
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
    DocumentPath(ProjectPathError),
    ReadDocument {
        path: PathBuf,
        source: io::Error,
    },
    UnsupportedDocumentSynchronization {
        server: String,
        capability: Option<JsonValue>,
    },
    UnsupportedPositionEncoding {
        server: String,
        encoding: JsonValue,
    },
    PositionConversion {
        server: String,
        path: PathBuf,
        source: PositionError,
    },
    DocumentSynchronizationClosed {
        server: String,
        path: PathBuf,
    },
    DocumentLanguageChanged {
        server: String,
        path: PathBuf,
        previous: String,
        requested: String,
    },
    DocumentVersionOverflow {
        server: String,
        path: PathBuf,
    },
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
            Self::DocumentPath(source) => write!(formatter, "{source}"),
            Self::ReadDocument { path, source } => {
                write!(
                    formatter,
                    "failed to read project file `{}` as UTF-8 text: {source}",
                    path.display()
                )
            }
            Self::UnsupportedDocumentSynchronization { server, capability } => {
                write!(
                    formatter,
                    "language server `{server}` does not support full or incremental document synchronization"
                )?;
                if let Some(capability) = capability {
                    write!(formatter, ": {capability}")?;
                }
                Ok(())
            }
            Self::UnsupportedPositionEncoding { server, encoding } => write!(
                formatter,
                "language server `{server}` selected unsupported position encoding {encoding}"
            ),
            Self::PositionConversion {
                server,
                path,
                source,
            } => write!(
                formatter,
                "language server `{server}` could not convert a position in `{}`: {source}",
                path.display()
            ),
            Self::DocumentSynchronizationClosed { server, path } => {
                write!(
                    formatter,
                    "language server `{server}` is shutting down and cannot synchronize `{}`",
                    path.display()
                )
            }
            Self::DocumentLanguageChanged {
                server,
                path,
                previous,
                requested,
            } => {
                write!(
                    formatter,
                    "language server `{server}` already opened `{}` as `{previous}`, not `{requested}`",
                    path.display()
                )
            }
            Self::DocumentVersionOverflow { server, path } => {
                write!(
                    formatter,
                    "language server `{server}` exhausted document versions for `{}`",
                    path.display()
                )
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
            Self::Spawn { source, .. }
            | Self::ReadDocument { source, .. }
            | Self::Shutdown { source, .. } => Some(source),
            Self::PositionConversion { source, .. } => Some(source),
            Self::DocumentPath(source) => Some(source),
            Self::EncodeMessage(source) | Self::DecodeResult(source) => {
                Some(source)
            }
            Self::MissingPipe { .. }
            | Self::UnsupportedDocumentSynchronization { .. }
            | Self::UnsupportedPositionEncoding { .. }
            | Self::DocumentSynchronizationClosed { .. }
            | Self::DocumentLanguageChanged { .. }
            | Self::DocumentVersionOverflow { .. }
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
            ServerSnapshot::initialized(config.name(), &initialize_result)?;
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
        if let Err(error) = self.active.close_documents().await {
            let _ = self.force_stop(shutdown_timeout).await;
            return Err(error);
        }

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
        // RunningServer owns a Sender until it is dropped, so the writer may be
        // waiting for receiver EOF even after the child process has exited.
        self.writer_task.abort();
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
    registrations: Arc<Mutex<BTreeMap<String, JsonValue>>>,
    documents: Arc<Mutex<DocumentStore>>,
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
            registrations: Arc::new(Mutex::new(BTreeMap::new())),
            documents: Arc::new(Mutex::new(DocumentStore::default())),
        }
    }

    async fn synchronize_document(
        &self,
        file: ProjectFile,
        language_id: &str,
    ) -> Result<SynchronizedDocument, LspError> {
        let snapshot = self.status.lock().await.clone();
        let sync = DocumentSync::from_capability(snapshot.text_document_sync())
            .ok_or_else(|| LspError::UnsupportedDocumentSynchronization {
                server: self.name.clone(),
                capability: snapshot.text_document_sync().cloned(),
            })?;
        let uri = path_to_file_uri(file.absolute());
        let mut documents = self.documents.lock().await;
        let text = tokio::fs::read_to_string(file.absolute()).await.map_err(
            |source| LspError::ReadDocument {
                path: file.absolute().to_path_buf(),
                source,
            },
        )?;
        let update = documents
            .synchronize(
                file.absolute(),
                file.relative(),
                uri,
                language_id,
                text,
                sync.open_close,
            )
            .map_err(|error| self.document_store_error(error))?;
        let document = update.document().clone();

        match update {
            DocumentUpdate::Opened {
                document,
                notify: true,
            } => {
                self.send_notification(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": document.uri(),
                            "languageId": document.language_id(),
                            "version": document.version(),
                            "text": document.text(),
                        },
                    }),
                )
                .await?;
            }
            DocumentUpdate::Changed {
                document,
                previous_text,
            } => {
                let content_change = match sync.kind {
                    DocumentSyncKind::Full => json!({
                        "text": document.text(),
                    }),
                    DocumentSyncKind::Incremental => json!({
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": PositionConverter::new(&previous_text)
                                .end_position(
                                    snapshot
                                        .position_encoding()
                                        .unwrap_or_default(),
                                )
                                .map_err(|source| {
                                    LspError::PositionConversion {
                                        server: self.name.clone(),
                                        path: document
                                            .absolute_path()
                                            .to_path_buf(),
                                        source,
                                    }
                                })?,
                        },
                        "text": document.text(),
                    }),
                };
                self.send_notification(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {
                            "uri": document.uri(),
                            "version": document.version(),
                        },
                        "contentChanges": [content_change],
                    }),
                )
                .await?;
            }
            DocumentUpdate::Opened { notify: false, .. }
            | DocumentUpdate::Unchanged(_) => {}
        }
        drop(documents);

        Ok(document)
    }

    async fn close_documents(&self) -> Result<(), LspError> {
        let documents = self.documents.lock().await.close_all();
        for document in documents {
            self.send_notification(
                "textDocument/didClose",
                json!({
                    "textDocument": {
                        "uri": document.uri(),
                    },
                }),
            )
            .await?;
        }
        Ok(())
    }

    fn document_store_error(&self, error: DocumentStoreError) -> LspError {
        match error {
            DocumentStoreError::Closed { path } => {
                LspError::DocumentSynchronizationClosed {
                    server: self.name.clone(),
                    path,
                }
            }
            DocumentStoreError::LanguageChanged {
                path,
                previous,
                requested,
            } => LspError::DocumentLanguageChanged {
                server: self.name.clone(),
                path,
                previous,
                requested,
            },
            DocumentStoreError::VersionOverflow { path } => {
                LspError::DocumentVersionOverflow {
                    server: self.name.clone(),
                    path,
                }
            }
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

#[derive(Debug, Clone, Copy)]
struct DocumentSync {
    open_close: bool,
    kind: DocumentSyncKind,
}

impl DocumentSync {
    fn from_capability(capability: Option<&JsonValue>) -> Option<Self> {
        let capability = capability?;
        if let Some(kind) = capability.as_u64() {
            return DocumentSyncKind::from_number(kind).map(|kind| Self {
                open_close: true,
                kind,
            });
        }

        let options = capability.as_object()?;
        let kind = options
            .get("change")
            .and_then(JsonValue::as_u64)
            .and_then(DocumentSyncKind::from_number)?;
        let open_close = options
            .get("openClose")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        Some(Self { open_close, kind })
    }
}

#[derive(Debug, Clone, Copy)]
enum DocumentSyncKind {
    Full,
    Incremental,
}

impl DocumentSyncKind {
    fn from_number(value: u64) -> Option<Self> {
        match value {
            1 => Some(Self::Full),
            2 => Some(Self::Incremental),
            _ => None,
        }
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
            handle_server_request(server, active, id, method, params).await;
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
    server: &str,
    active: &ActiveServer,
    id: JsonValue,
    method: &str,
    params: JsonValue,
) {
    match method {
        "workspace/configuration" => {
            let result = configuration_response(&active.configuration, &params);
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
            let mut registrations = active.registrations.lock().await;
            for registration in capability_items(&params, "registrations") {
                if let Some(id) = capability_id(registration) {
                    registrations.insert(id.to_owned(), registration.clone());
                }
            }
            let _ = active
                .send_message(success_response(id, JsonValue::Null))
                .await;
        }
        "client/unregisterCapability" => {
            let mut registrations = active.registrations.lock().await;
            for registration in capability_items(&params, "unregisterations") {
                if let Some(id) = capability_id(registration) {
                    registrations.remove(id);
                }
            }
            let _ = active
                .send_message(success_response(id, JsonValue::Null))
                .await;
        }
        "window/showMessageRequest" => {
            trace_server_message(server, method, &params);
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
        "window/logMessage" | "window/showMessage" | "window/logTrace" => {
            trace_server_message(server, method, &params);
        }
        "$/progress" | "telemetry/event" => {
            debug!(server = %server, method, "ignored language server notification");
        }
        _ => {
            debug!(server = %server, method, "ignored language server notification");
        }
    }
}

fn configuration_response(
    configuration: &JsonValue,
    params: &JsonValue,
) -> JsonValue {
    let values = params
        .get("items")
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    item.get("section").and_then(JsonValue::as_str).map_or_else(
                        || configuration.clone(),
                        |section| configuration_section(configuration, section),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    JsonValue::Array(values)
}

fn configuration_section(
    configuration: &JsonValue,
    section: &str,
) -> JsonValue {
    let mut value = configuration;
    for part in section.split('.') {
        if part.is_empty() {
            return JsonValue::Null;
        }
        let Some(next) = value.get(part) else {
            return JsonValue::Null;
        };
        value = next;
    }
    value.clone()
}

fn capability_items<'a>(params: &'a JsonValue, field: &str) -> &'a [JsonValue] {
    params
        .get(field)
        .and_then(JsonValue::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn capability_id(value: &JsonValue) -> Option<&str> {
    value.get("id").and_then(JsonValue::as_str)
}

fn trace_server_message(server: &str, method: &str, params: &JsonValue) {
    if let Some(message) = params.get("message").and_then(JsonValue::as_str) {
        info!(server = %server, method, message, "language server message");
    } else {
        info!(server = %server, method, "language server message");
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
            "general": {
                "positionEncodings": ["utf-8", "utf-16", "utf-32"],
            },
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
