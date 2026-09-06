use std::{
    collections::{BTreeMap, BTreeSet},
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

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value as JsonValue, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdout},
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tracing::{debug, info, warn};
use url::Url;

use crate::{
    config::LanguageServerConfig,
    documents::{
        DocumentStore, DocumentStoreError, DocumentUpdate, SynchronizedDocument,
    },
    positions::{
        Position, PositionConverter, PositionEncoding, PositionError, Range,
    },
    project::{Project, ProjectFile, ProjectPathError},
};

const JSONRPC_VERSION: &str = "2.0";
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

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
        Ok(active.snapshot().await)
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

    pub async fn hover(
        &self,
        path: impl AsRef<Path>,
        language_id: &str,
        position: Position,
    ) -> Result<Option<Hover>, LspError> {
        let file = self
            .project
            .resolve_file(path)
            .map_err(LspError::DocumentPath)?;
        let active = self.active_server().await?;
        let snapshot = active.status.lock().await.clone();
        if !active
            .supports_method(
                "textDocument/hover",
                snapshot.capabilities().get("hoverProvider"),
            )
            .await
        {
            return Err(LspError::UnsupportedCapability {
                server: self.config.name().to_owned(),
                method: "textDocument/hover",
            });
        }

        let document = active.synchronize_document(file, language_id).await?;
        let encoding = snapshot.position_encoding().unwrap_or_default();
        let lsp_position = document
            .to_lsp_position(position, encoding)
            .map_err(|source| LspError::PositionConversion {
                server: self.config.name().to_owned(),
                path: document.absolute_path().to_path_buf(),
                source,
            })?;
        let value = active
            .request_value(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": document.uri() },
                    "position": lsp_position,
                }),
                self.config.timeouts().request(),
            )
            .await?;
        let mut hover: Option<Hover> =
            serde_json::from_value(value).map_err(LspError::DecodeResult)?;

        if let Some(hover) = hover.as_mut()
            && let Some(range) = hover.range
        {
            hover.range =
                Some(document.from_lsp_range(range, encoding).map_err(
                    |source| LspError::PositionConversion {
                        server: self.config.name().to_owned(),
                        path: document.absolute_path().to_path_buf(),
                        source,
                    },
                )?);
        }

        Ok(hover)
    }

    pub async fn definition(
        &self,
        path: impl AsRef<Path>,
        language_id: &str,
        position: Position,
    ) -> Result<Vec<DefinitionLocation>, LspError> {
        self.location_request(
            path,
            language_id,
            position,
            "textDocument/definition",
            "definitionProvider",
        )
        .await
    }

    pub async fn declaration(
        &self,
        path: impl AsRef<Path>,
        language_id: &str,
        position: Position,
    ) -> Result<Vec<DefinitionLocation>, LspError> {
        self.location_request(
            path,
            language_id,
            position,
            "textDocument/declaration",
            "declarationProvider",
        )
        .await
    }

    pub async fn type_definition(
        &self,
        path: impl AsRef<Path>,
        language_id: &str,
        position: Position,
    ) -> Result<Vec<DefinitionLocation>, LspError> {
        self.location_request(
            path,
            language_id,
            position,
            "textDocument/typeDefinition",
            "typeDefinitionProvider",
        )
        .await
    }

    pub async fn implementation(
        &self,
        path: impl AsRef<Path>,
        language_id: &str,
        position: Position,
    ) -> Result<Vec<DefinitionLocation>, LspError> {
        self.location_request(
            path,
            language_id,
            position,
            "textDocument/implementation",
            "implementationProvider",
        )
        .await
    }

    pub async fn references(
        &self,
        path: impl AsRef<Path>,
        language_id: &str,
        position: Position,
        include_declaration: bool,
    ) -> Result<Vec<ReferenceLocation>, LspError> {
        let file = self
            .project
            .resolve_file(path)
            .map_err(LspError::DocumentPath)?;
        let active = self.active_server().await?;
        let snapshot = active.status.lock().await.clone();
        if !active
            .supports_method(
                "textDocument/references",
                snapshot.capabilities().get("referencesProvider"),
            )
            .await
        {
            return Err(LspError::UnsupportedCapability {
                server: self.config.name().to_owned(),
                method: "textDocument/references",
            });
        }

        let document = active.synchronize_document(file, language_id).await?;
        let encoding = snapshot.position_encoding().unwrap_or_default();
        let lsp_position = document
            .to_lsp_position(position, encoding)
            .map_err(|source| LspError::PositionConversion {
                server: self.config.name().to_owned(),
                path: document.absolute_path().to_path_buf(),
                source,
            })?;
        let value = active
            .request_value(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": document.uri() },
                    "position": lsp_position,
                    "context": {
                        "includeDeclaration": include_declaration,
                    },
                }),
                self.config.timeouts().request(),
            )
            .await?;
        let response: Option<Vec<Location>> =
            serde_json::from_value(value).map_err(LspError::DecodeResult)?;

        let mut references = Vec::new();
        for location in response.into_iter().flatten() {
            let target = self
                .normalize_location_target(
                    &document,
                    &location.uri,
                    location.range,
                    location.range,
                    encoding,
                )
                .await?;
            references.push(ReferenceLocation {
                server: self.config.name().to_owned(),
                uri: location.uri,
                range: target.range,
                position_encoding: target.position_encoding,
            });
        }

        Ok(references)
    }

    pub async fn document_diagnostics(
        &self,
        path: impl AsRef<Path>,
        language_id: &str,
    ) -> Result<DiagnosticReport, LspError> {
        let file = self
            .project
            .resolve_file(path)
            .map_err(LspError::DocumentPath)?;
        let active = self.active_server().await?;
        let snapshot = active.status.lock().await.clone();
        let pull_provider = active
            .diagnostic_provider(
                "textDocument/diagnostic",
                snapshot.capabilities().get("diagnosticProvider"),
            )
            .await;
        let document = active.synchronize_document(file, language_id).await?;
        let encoding = snapshot.position_encoding().unwrap_or_default();

        if let Some(provider) = pull_provider {
            active
                .pull_diagnostics(
                    &document,
                    encoding,
                    provider.identifier.as_deref(),
                    self.config.timeouts().request(),
                )
                .await
        } else {
            active.push_diagnostics(&document, encoding).await
        }
    }

    pub async fn workspace_symbols(
        &self,
        query: &str,
    ) -> Result<Vec<WorkspaceSymbol>, LspError> {
        const METHOD: &str = "workspace/symbol";

        let active = self.active_server().await?;
        let snapshot = active.status.lock().await.clone();
        if !active
            .supports_method(
                METHOD,
                snapshot.capabilities().get("workspaceSymbolProvider"),
            )
            .await
        {
            return Err(LspError::UnsupportedCapability {
                server: self.config.name().to_owned(),
                method: METHOD,
            });
        }

        let value = active
            .request_value(
                METHOD,
                json!({ "query": query }),
                self.config.timeouts().request(),
            )
            .await?;
        let response: Option<Vec<RawWorkspaceSymbol>> =
            serde_json::from_value(value).map_err(LspError::DecodeResult)?;
        let encoding = snapshot.position_encoding().unwrap_or_default();
        let mut symbols = Vec::new();
        for symbol in response.into_iter().flatten() {
            let target = self
                .normalize_file_target(
                    &symbol.location.uri,
                    symbol.location.range,
                    symbol.location.range,
                    encoding,
                )
                .await?;
            let mut fields = symbol.fields;
            fields.remove("server");
            fields.remove("positionEncoding");
            symbols.push(WorkspaceSymbol {
                server: self.config.name().to_owned(),
                name: symbol.name,
                kind: symbol.kind,
                location: WorkspaceSymbolLocation {
                    uri: symbol.location.uri,
                    range: target.range,
                    fields: symbol.location.fields,
                },
                position_encoding: target.position_encoding,
                fields,
            });
        }

        Ok(symbols)
    }

    async fn location_request(
        &self,
        path: impl AsRef<Path>,
        language_id: &str,
        position: Position,
        method: &'static str,
        capability: &'static str,
    ) -> Result<Vec<DefinitionLocation>, LspError> {
        let file = self
            .project
            .resolve_file(path)
            .map_err(LspError::DocumentPath)?;
        let active = self.active_server().await?;
        let snapshot = active.status.lock().await.clone();
        if !active
            .supports_method(method, snapshot.capabilities().get(capability))
            .await
        {
            return Err(LspError::UnsupportedCapability {
                server: self.config.name().to_owned(),
                method,
            });
        }

        let document = active.synchronize_document(file, language_id).await?;
        let encoding = snapshot.position_encoding().unwrap_or_default();
        let lsp_position = document
            .to_lsp_position(position, encoding)
            .map_err(|source| LspError::PositionConversion {
                server: self.config.name().to_owned(),
                path: document.absolute_path().to_path_buf(),
                source,
            })?;
        let value = active
            .request_value(
                method,
                json!({
                    "textDocument": { "uri": document.uri() },
                    "position": lsp_position,
                }),
                self.config.timeouts().request(),
            )
            .await?;
        let response: Option<LocationResponse> =
            serde_json::from_value(value).map_err(LspError::DecodeResult)?;

        let mut locations = Vec::new();
        for location in response.into_iter().flat_map(LocationResponse::items) {
            let (uri, target_range, target_selection_range, origin_range) =
                match location {
                    LocationResponseItem::Location(location) => {
                        (location.uri, location.range, location.range, None)
                    }
                    LocationResponseItem::LocationLink(link) => (
                        link.target_uri,
                        link.target_range,
                        link.target_selection_range,
                        link.origin_selection_range,
                    ),
                };
            let origin_selection_range = origin_range
                .map(|range| {
                    document.from_lsp_range(range, encoding).map_err(|source| {
                        LspError::PositionConversion {
                            server: self.config.name().to_owned(),
                            path: document.absolute_path().to_path_buf(),
                            source,
                        }
                    })
                })
                .transpose()?;
            let target = self
                .normalize_location_target(
                    &document,
                    &uri,
                    target_range,
                    target_selection_range,
                    encoding,
                )
                .await?;
            locations.push(DefinitionLocation {
                server: self.config.name().to_owned(),
                uri,
                target_range: target.range,
                target_selection_range: target.selection_range,
                target_position_encoding: target.position_encoding,
                origin_selection_range,
            });
        }

        Ok(locations)
    }

    async fn normalize_location_target(
        &self,
        document: &SynchronizedDocument,
        uri: &str,
        range: Range,
        selection_range: Range,
        encoding: PositionEncoding,
    ) -> Result<DefinitionTarget, LspError> {
        if uri == document.uri() {
            return convert_definition_target(
                document.text(),
                document.absolute_path(),
                range,
                selection_range,
                encoding,
                self.config.name(),
            );
        }

        self.normalize_file_target(uri, range, selection_range, encoding)
            .await
    }

    async fn normalize_file_target(
        &self,
        uri: &str,
        range: Range,
        selection_range: Range,
        encoding: PositionEncoding,
    ) -> Result<DefinitionTarget, LspError> {
        let Some(path) = Url::parse(uri)
            .ok()
            .filter(|uri| uri.scheme() == "file")
            .and_then(|uri| uri.to_file_path().ok())
        else {
            return Ok(DefinitionTarget::unconverted(
                range,
                selection_range,
                encoding,
            ));
        };
        if !path.starts_with(self.project.root()) {
            return Ok(DefinitionTarget::unconverted(
                range,
                selection_range,
                encoding,
            ));
        }
        let Ok(file) = self.project.resolve_file(&path) else {
            return Ok(DefinitionTarget::unconverted(
                range,
                selection_range,
                encoding,
            ));
        };
        let Ok(text) = tokio::fs::read_to_string(file.absolute()).await else {
            return Ok(DefinitionTarget::unconverted(
                range,
                selection_range,
                encoding,
            ));
        };

        convert_definition_target(
            &text,
            file.absolute(),
            range,
            selection_range,
            encoding,
            self.config.name(),
        )
    }

    pub async fn status(&self) -> ServerSnapshot {
        let active = {
            let state = self.state.lock().await;
            state.as_ref().map(|running| running.active.clone())
        };

        match active {
            Some(active) => active.snapshot().await,
            None => ServerSnapshot::not_started(self.config.name()),
        }
    }

    pub async fn diagnostics(&self) -> Vec<JsonValue> {
        let active = {
            let state = self.state.lock().await;
            state.as_ref().map(|running| running.active.clone())
        };

        match active {
            Some(active) => active.diagnostics.lock().await.push_values(),
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hover {
    pub contents: HoverContents,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionLocation {
    pub server: String,
    pub uri: String,
    pub target_range: Range,
    pub target_selection_range: Range,
    pub target_position_encoding: PositionEncoding,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_selection_range: Option<Range>,
}

impl DefinitionLocation {
    pub fn text(&self) -> String {
        let range = self.target_selection_range;
        format!(
            "{}: {}:{}:{}-{}:{} ({})",
            self.server,
            self.uri,
            range.start.line,
            range.start.character,
            range.end.line,
            range.end.character,
            self.target_position_encoding,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceLocation {
    pub server: String,
    pub uri: String,
    pub range: Range,
    pub position_encoding: PositionEncoding,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSymbol {
    pub server: String,
    pub name: String,
    pub kind: u32,
    pub location: WorkspaceSymbolLocation,
    pub position_encoding: PositionEncoding,
    #[serde(flatten)]
    fields: BTreeMap<String, JsonValue>,
}

impl WorkspaceSymbol {
    pub fn text(&self) -> String {
        let range = self.location.range;
        format!(
            "{}: {} (kind {}) at {}:{}:{}-{}:{} ({})",
            self.server,
            self.name,
            self.kind,
            self.location.uri,
            range.start.line,
            range.start.character,
            range.end.line,
            range.end.character,
            self.position_encoding,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkspaceSymbolLocation {
    pub uri: String,
    pub range: Range,
    #[serde(flatten)]
    fields: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Deserialize)]
struct RawWorkspaceSymbol {
    name: String,
    kind: u32,
    location: RawWorkspaceSymbolLocation,
    #[serde(flatten)]
    fields: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Deserialize)]
struct RawWorkspaceSymbolLocation {
    uri: String,
    range: Range,
    #[serde(flatten)]
    fields: BTreeMap<String, JsonValue>,
}

impl ReferenceLocation {
    pub fn text(&self) -> String {
        format!(
            "{}: {}:{}:{}-{}:{} ({})",
            self.server,
            self.uri,
            self.range.start.line,
            self.range.start.character,
            self.range.end.line,
            self.range.end.character,
            self.position_encoding,
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LocationResponse {
    Location(Location),
    Locations(Vec<Location>),
    LocationLinks(Vec<LocationLink>),
}

impl LocationResponse {
    fn items(self) -> Vec<LocationResponseItem> {
        match self {
            Self::Location(location) => {
                vec![LocationResponseItem::Location(location)]
            }
            Self::Locations(locations) => locations
                .into_iter()
                .map(LocationResponseItem::Location)
                .collect(),
            Self::LocationLinks(links) => links
                .into_iter()
                .map(LocationResponseItem::LocationLink)
                .collect(),
        }
    }
}

#[derive(Debug)]
enum LocationResponseItem {
    Location(Location),
    LocationLink(LocationLink),
}

#[derive(Debug, Deserialize)]
struct Location {
    uri: String,
    range: Range,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocationLink {
    #[serde(default)]
    origin_selection_range: Option<Range>,
    target_uri: String,
    target_range: Range,
    target_selection_range: Range,
}

#[derive(Debug)]
struct DefinitionTarget {
    range: Range,
    selection_range: Range,
    position_encoding: PositionEncoding,
}

impl DefinitionTarget {
    const fn unconverted(
        range: Range,
        selection_range: Range,
        position_encoding: PositionEncoding,
    ) -> Self {
        Self {
            range,
            selection_range,
            position_encoding,
        }
    }
}

fn convert_definition_target(
    text: &str,
    path: &Path,
    range: Range,
    selection_range: Range,
    encoding: PositionEncoding,
    server: &str,
) -> Result<DefinitionTarget, LspError> {
    let converter = PositionConverter::new(text);
    let range =
        converter
            .from_lsp_range(range, encoding)
            .map_err(|source| LspError::PositionConversion {
                server: server.to_owned(),
                path: path.to_path_buf(),
                source,
            })?;
    let selection_range = converter
        .from_lsp_range(selection_range, encoding)
        .map_err(|source| LspError::PositionConversion {
        server: server.to_owned(),
        path: path.to_path_buf(),
        source,
    })?;
    Ok(DefinitionTarget {
        range,
        selection_range,
        position_encoding: PositionEncoding::Utf8,
    })
}

impl Hover {
    pub fn text(&self) -> String {
        self.contents.text()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HoverContents {
    Markup(MarkupContent),
    MarkedStrings(Vec<MarkedString>),
    MarkedString(MarkedString),
}

impl HoverContents {
    fn text(&self) -> String {
        match self {
            Self::Markup(content) => content.value.trim().to_owned(),
            Self::MarkedStrings(contents) => contents
                .iter()
                .map(MarkedString::value)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
            Self::MarkedString(content) => content.value().trim().to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkupContent {
    pub kind: MarkupKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkupKind {
    Plaintext,
    Markdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MarkedString {
    String(String),
    LanguageString { language: String, value: String },
}

impl MarkedString {
    fn value(&self) -> &str {
        match self {
            Self::String(value) | Self::LanguageString { value, .. } => value,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSource {
    Pull,
    Push,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticAvailability {
    Current,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticReport {
    server: String,
    uri: String,
    source: DiagnosticSource,
    availability: DiagnosticAvailability,
    document_version: i32,
    report_version: Option<i32>,
    result_id: Option<String>,
    position_encoding: PositionEncoding,
    diagnostics: Vec<JsonValue>,
}

impl DiagnosticReport {
    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn source(&self) -> DiagnosticSource {
        self.source
    }

    pub fn availability(&self) -> DiagnosticAvailability {
        self.availability
    }

    pub fn document_version(&self) -> i32 {
        self.document_version
    }

    pub fn report_version(&self) -> Option<i32> {
        self.report_version
    }

    pub fn result_id(&self) -> Option<&str> {
        self.result_id.as_deref()
    }

    pub fn position_encoding(&self) -> PositionEncoding {
        self.position_encoding
    }

    pub fn diagnostics(&self) -> &[JsonValue] {
        &self.diagnostics
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Diagnostic {
    range: Range,
    #[serde(flatten)]
    fields: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublishDiagnosticsParams {
    uri: String,
    version: Option<i32>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum DocumentDiagnosticReport {
    Full {
        #[serde(default, rename = "resultId")]
        result_id: Option<String>,
        items: Vec<Diagnostic>,
    },
    Unchanged {
        #[serde(rename = "resultId")]
        result_id: String,
    },
}

#[derive(Debug, Clone)]
struct CachedPushDiagnostics {
    version: Option<i32>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone)]
struct CachedPullDiagnostics {
    result_id: Option<String>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Default)]
struct DiagnosticCache {
    push: BTreeMap<String, CachedPushDiagnostics>,
    pull: BTreeMap<String, CachedPullDiagnostics>,
}

#[derive(Debug)]
struct DiagnosticProvider {
    identifier: Option<String>,
}

impl DiagnosticCache {
    fn record_push(&mut self, params: PublishDiagnosticsParams) {
        let replace = self.push.get(&params.uri).is_none_or(|cached| {
            match (cached.version, params.version) {
                (Some(current), Some(incoming)) => incoming >= current,
                (Some(_), None) => false,
                (None, Some(_)) | (None, None) => true,
            }
        });
        if replace {
            self.push.insert(
                params.uri,
                CachedPushDiagnostics {
                    version: params.version,
                    diagnostics: params.diagnostics,
                },
            );
        }
    }

    fn push_values(&self) -> Vec<JsonValue> {
        self.push
            .iter()
            .map(|(uri, report)| {
                json!({
                    "uri": uri,
                    "version": report.version,
                    "diagnostics": report.diagnostics,
                })
            })
            .collect()
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
    readiness: ReadinessSnapshot,
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
            readiness: ReadinessSnapshot::not_started(),
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
            readiness: ReadinessSnapshot::unknown(),
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

    pub fn readiness(&self) -> &ReadinessSnapshot {
        &self.readiness
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ReadinessState {
    NotStarted,
    Starting,
    Busy,
    Ready,
    Degraded,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ReadinessSource {
    Lifecycle,
    WorkDoneProgress,
    ServerStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ResultStability {
    Stable,
    Transient,
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadinessSnapshot {
    state: ReadinessState,
    source: ReadinessSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    health: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    active_progress: usize,
}

impl ReadinessSnapshot {
    fn not_started() -> Self {
        Self {
            state: ReadinessState::NotStarted,
            source: ReadinessSource::Lifecycle,
            health: None,
            message: None,
            active_progress: 0,
        }
    }

    fn unknown() -> Self {
        Self {
            state: ReadinessState::Unknown,
            source: ReadinessSource::Lifecycle,
            health: None,
            message: None,
            active_progress: 0,
        }
    }

    pub fn state(&self) -> ReadinessState {
        self.state
    }

    pub fn source(&self) -> ReadinessSource {
        self.source
    }

    pub fn health(&self) -> Option<&str> {
        self.health.as_deref()
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn active_progress(&self) -> usize {
        self.active_progress
    }

    pub fn result_stability(&self) -> ResultStability {
        match self.state {
            ReadinessState::Ready => ResultStability::Stable,
            ReadinessState::Starting | ReadinessState::Busy => {
                ResultStability::Transient
            }
            ReadinessState::NotStarted
            | ReadinessState::Degraded
            | ReadinessState::Unknown => ResultStability::Indeterminate,
        }
    }
}

#[derive(Debug, Clone)]
struct ServerStatus {
    health: String,
    quiescent: bool,
    message: Option<String>,
}

#[derive(Debug, Default)]
struct ReadinessTracker {
    initialized: bool,
    progress_seen: bool,
    active_progress: BTreeSet<String>,
    server_status: Option<ServerStatus>,
}

impl ReadinessTracker {
    fn mark_initialized(&mut self) {
        self.initialized = true;
    }

    fn start_progress(&mut self, token: String) {
        self.progress_seen = true;
        self.active_progress.insert(token);
    }

    fn finish_progress(&mut self, token: &str) {
        self.progress_seen = true;
        self.active_progress.remove(token);
    }

    fn record_server_status(&mut self, status: ServerStatus) {
        self.server_status = Some(status);
    }

    fn snapshot(&self) -> ReadinessSnapshot {
        let active_progress = self.active_progress.len();
        if !self.initialized {
            return ReadinessSnapshot {
                state: ReadinessState::Starting,
                source: ReadinessSource::Lifecycle,
                health: None,
                message: None,
                active_progress,
            };
        }

        if let Some(status) = &self.server_status
            && status.health != "ok"
        {
            return ReadinessSnapshot {
                state: ReadinessState::Degraded,
                source: ReadinessSource::ServerStatus,
                health: Some(status.health.clone()),
                message: status.message.clone(),
                active_progress,
            };
        }

        if active_progress > 0 {
            return ReadinessSnapshot {
                state: ReadinessState::Busy,
                source: ReadinessSource::WorkDoneProgress,
                health: self
                    .server_status
                    .as_ref()
                    .map(|status| status.health.clone()),
                message: self
                    .server_status
                    .as_ref()
                    .and_then(|status| status.message.clone()),
                active_progress,
            };
        }

        if let Some(status) = &self.server_status {
            return ReadinessSnapshot {
                state: if status.quiescent {
                    ReadinessState::Ready
                } else {
                    ReadinessState::Busy
                },
                source: ReadinessSource::ServerStatus,
                health: Some(status.health.clone()),
                message: status.message.clone(),
                active_progress,
            };
        }

        if self.progress_seen {
            return ReadinessSnapshot {
                state: ReadinessState::Ready,
                source: ReadinessSource::WorkDoneProgress,
                health: None,
                message: None,
                active_progress,
            };
        }

        ReadinessSnapshot::unknown()
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
    InvalidDiagnosticReport {
        server: String,
        message: String,
    },
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
    UnsupportedCapability {
        server: String,
        method: &'static str,
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
    ServerExited {
        server: String,
        method: String,
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
            Self::InvalidDiagnosticReport { server, message } => write!(
                formatter,
                "language server `{server}` returned an invalid diagnostic report: {message}"
            ),
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
            Self::UnsupportedCapability { server, method } => write!(
                formatter,
                "language server `{server}` does not support `{method}`"
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
            Self::ServerExited { server, method } => write!(
                formatter,
                "language server `{server}` exited while request `{method}` was pending"
            ),
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
            | Self::InvalidDiagnosticReport { .. }
            | Self::UnsupportedDocumentSynchronization { .. }
            | Self::UnsupportedPositionEncoding { .. }
            | Self::UnsupportedCapability { .. }
            | Self::DocumentSynchronizationClosed { .. }
            | Self::DocumentLanguageChanged { .. }
            | Self::DocumentVersionOverflow { .. }
            | Self::TransportClosed { .. }
            | Self::ServerExited { .. }
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
        self.active.readiness.lock().await.mark_initialized();
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
    pending: PendingRequests,
    next_request_id: Arc<AtomicU64>,
    project_root: PathBuf,
    configuration: JsonValue,
    status: Arc<Mutex<ServerSnapshot>>,
    readiness: Arc<Mutex<ReadinessTracker>>,
    diagnostics: Arc<Mutex<DiagnosticCache>>,
    registrations: Arc<Mutex<BTreeMap<String, JsonValue>>>,
    documents: Arc<Mutex<DocumentStore>>,
}

type PendingRequests = Arc<
    Mutex<
        BTreeMap<
            u64,
            oneshot::Sender<Result<JsonRpcResponse, PendingRequestError>>,
        >,
    >,
>;

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
            readiness: Arc::new(Mutex::new(ReadinessTracker::default())),
            diagnostics: Arc::new(Mutex::new(DiagnosticCache::default())),
            registrations: Arc::new(Mutex::new(BTreeMap::new())),
            documents: Arc::new(Mutex::new(DocumentStore::default())),
        }
    }

    async fn snapshot(&self) -> ServerSnapshot {
        let mut snapshot = self.status.lock().await.clone();
        snapshot.readiness = self.readiness.lock().await.snapshot();
        snapshot
    }

    async fn supports_method(
        &self,
        method: &str,
        static_capability: Option<&JsonValue>,
    ) -> bool {
        if static_capability.is_some_and(capability_is_enabled) {
            return true;
        }

        self.registrations
            .lock()
            .await
            .values()
            .any(|registration| {
                registration.get("method").and_then(JsonValue::as_str)
                    == Some(method)
            })
    }

    async fn diagnostic_provider(
        &self,
        method: &str,
        static_capability: Option<&JsonValue>,
    ) -> Option<DiagnosticProvider> {
        if static_capability.is_some_and(capability_is_enabled) {
            return Some(DiagnosticProvider {
                identifier: static_capability
                    .and_then(|capability| capability.get("identifier"))
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned),
            });
        }

        self.registrations
            .lock()
            .await
            .values()
            .find(|registration| {
                registration.get("method").and_then(JsonValue::as_str)
                    == Some(method)
            })
            .map(|registration| DiagnosticProvider {
                identifier: registration
                    .get("registerOptions")
                    .and_then(|options| options.get("identifier"))
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned),
            })
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

    async fn pull_diagnostics(
        &self,
        document: &SynchronizedDocument,
        encoding: PositionEncoding,
        identifier: Option<&str>,
        request_timeout: Duration,
    ) -> Result<DiagnosticReport, LspError> {
        let previous_result_id = self
            .diagnostics
            .lock()
            .await
            .pull
            .get(document.uri())
            .and_then(|report| report.result_id.clone());
        let mut params = json!({
            "textDocument": { "uri": document.uri() },
        });
        if let Some(previous_result_id) = previous_result_id {
            params["previousResultId"] = JsonValue::String(previous_result_id);
        }
        if let Some(identifier) = identifier {
            params["identifier"] = JsonValue::String(identifier.to_owned());
        }

        let response = self
            .request_value("textDocument/diagnostic", params, request_timeout)
            .await?;
        let response: DocumentDiagnosticReport =
            serde_json::from_value(response).map_err(LspError::DecodeResult)?;
        let cached = match response {
            DocumentDiagnosticReport::Full { result_id, items } => {
                CachedPullDiagnostics {
                    result_id,
                    diagnostics: items,
                }
            }
            DocumentDiagnosticReport::Unchanged { result_id } => {
                let diagnostics = self.diagnostics.lock().await;
                let Some(report) = diagnostics.pull.get(document.uri()) else {
                    return Err(LspError::InvalidDiagnosticReport {
                        server: self.name.clone(),
                        message:
                            "an unchanged report has no cached predecessor"
                                .to_owned(),
                    });
                };
                CachedPullDiagnostics {
                    result_id: Some(result_id),
                    diagnostics: report.diagnostics.clone(),
                }
            }
        };
        let diagnostics = normalize_diagnostics(
            cached.diagnostics.clone(),
            document,
            encoding,
            &self.name,
        )?;
        self.diagnostics
            .lock()
            .await
            .pull
            .insert(document.uri().to_owned(), cached.clone());

        Ok(DiagnosticReport {
            server: self.name.clone(),
            uri: document.uri().to_owned(),
            source: DiagnosticSource::Pull,
            availability: DiagnosticAvailability::Current,
            document_version: document.version(),
            report_version: None,
            result_id: cached.result_id,
            position_encoding: PositionEncoding::Utf8,
            diagnostics,
        })
    }

    async fn push_diagnostics(
        &self,
        document: &SynchronizedDocument,
        encoding: PositionEncoding,
    ) -> Result<DiagnosticReport, LspError> {
        let cached = self
            .diagnostics
            .lock()
            .await
            .push
            .get(document.uri())
            .cloned();
        let Some(cached) = cached else {
            return Ok(DiagnosticReport {
                server: self.name.clone(),
                uri: document.uri().to_owned(),
                source: DiagnosticSource::Push,
                availability: DiagnosticAvailability::Unavailable,
                document_version: document.version(),
                report_version: None,
                result_id: None,
                position_encoding: PositionEncoding::Utf8,
                diagnostics: Vec::new(),
            });
        };
        let is_current = cached.version == Some(document.version());
        let (position_encoding, diagnostics) = if is_current {
            (
                PositionEncoding::Utf8,
                normalize_diagnostics(
                    cached.diagnostics,
                    document,
                    encoding,
                    &self.name,
                )?,
            )
        } else {
            (
                encoding,
                diagnostics_to_values(cached.diagnostics)
                    .map_err(LspError::DecodeResult)?,
            )
        };

        Ok(DiagnosticReport {
            server: self.name.clone(),
            uri: document.uri().to_owned(),
            source: DiagnosticSource::Push,
            availability: if is_current {
                DiagnosticAvailability::Current
            } else {
                DiagnosticAvailability::Stale
            },
            document_version: document.version(),
            report_version: cached.version,
            result_id: None,
            position_encoding,
            diagnostics,
        })
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
            Ok(Ok(Ok(response))) => {
                response.into_result(&self.name, method.to_owned())
            }
            Ok(Ok(Err(PendingRequestError::ServerExited))) => {
                Err(LspError::ServerExited {
                    server: self.name.clone(),
                    method: method.to_owned(),
                })
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

fn capability_is_enabled(capability: &JsonValue) -> bool {
    capability
        .as_bool()
        .unwrap_or_else(|| capability.is_object())
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

#[derive(Debug, Clone, Copy)]
enum PendingRequestError {
    ServerExited,
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

    let pending = std::mem::take(&mut *active.pending.lock().await);
    for sender in pending.into_values() {
        let _ = sender.send(Err(PendingRequestError::ServerExited));
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
        let _ = sender.send(Ok(response));
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
        "window/workDoneProgress/create" => {
            let Some(token) = params.get("token").and_then(progress_token)
            else {
                let _ = active
                    .send_message(error_response(
                        id,
                        INVALID_PARAMS,
                        "work-done progress request omitted a valid token",
                    ))
                    .await;
                return;
            };
            active.readiness.lock().await.start_progress(token);
            let _ = active
                .send_message(success_response(id, JsonValue::Null))
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
            match serde_json::from_value::<PublishDiagnosticsParams>(params) {
                Ok(params) => {
                    active.diagnostics.lock().await.record_push(params);
                }
                Err(error) => {
                    warn!(
                        server = %server,
                        %error,
                        "ignored malformed publish-diagnostics notification"
                    );
                }
            }
        }
        "window/logMessage" | "window/showMessage" | "window/logTrace" => {
            trace_server_message(server, method, &params);
        }
        "$/progress" => {
            record_progress(server, active, &params).await;
        }
        "experimental/serverStatus" => {
            match serde_json::from_value::<ServerStatusParams>(params) {
                Ok(params)
                    if matches!(
                        params.health.as_str(),
                        "ok" | "warning" | "error"
                    ) =>
                {
                    active.readiness.lock().await.record_server_status(
                        ServerStatus {
                            health: params.health,
                            quiescent: params.quiescent,
                            message: params.message,
                        },
                    );
                }
                Ok(params) => {
                    warn!(
                        server = %server,
                        health = %params.health,
                        "ignored server-status notification with unknown health"
                    );
                }
                Err(error) => {
                    warn!(
                        server = %server,
                        %error,
                        "ignored malformed server-status notification"
                    );
                }
            }
        }
        "telemetry/event" => {
            debug!(server = %server, method, "ignored language server notification");
        }
        _ => {
            debug!(server = %server, method, "ignored language server notification");
        }
    }
}

#[derive(Debug, Deserialize)]
struct ServerStatusParams {
    health: String,
    quiescent: bool,
    #[serde(default)]
    message: Option<String>,
}

async fn record_progress(
    server: &str,
    active: &ActiveServer,
    params: &JsonValue,
) {
    let Some(token) = params.get("token").and_then(progress_token) else {
        warn!(server = %server, "ignored progress notification without a valid token");
        return;
    };
    let Some(kind) = params
        .get("value")
        .and_then(|value| value.get("kind"))
        .and_then(JsonValue::as_str)
    else {
        warn!(server = %server, "ignored progress notification without a kind");
        return;
    };

    let mut readiness = active.readiness.lock().await;
    match kind {
        "begin" | "report" => readiness.start_progress(token),
        "end" => readiness.finish_progress(&token),
        _ => warn!(
            server = %server,
            kind,
            "ignored progress notification with an unknown kind"
        ),
    }
}

fn progress_token(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(value) => Some(format!("string:{value}")),
        JsonValue::Number(value) if value.is_i64() || value.is_u64() => {
            Some(format!("number:{value}"))
        }
        _ => None,
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
            "window": {
                "workDoneProgress": true,
            },
            "experimental": {
                "serverStatusNotification": true,
            },
            "general": {
                "positionEncodings": ["utf-8", "utf-16", "utf-32"],
            },
            "workspace": {
                "configuration": true,
                "workspaceFolders": true,
                "didChangeConfiguration": {
                    "dynamicRegistration": true,
                },
                "symbol": {
                    "dynamicRegistration": true,
                    "symbolKind": {
                        "valueSet": [
                            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13,
                            14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
                            25, 26
                        ],
                    },
                    "tagSupport": { "valueSet": [1] },
                },
            },
            "textDocument": {
                "diagnostic": {
                    "dynamicRegistration": true,
                    "relatedDocumentSupport": false,
                },
                "declaration": {
                    "dynamicRegistration": true,
                    "linkSupport": true,
                },
                "definition": {
                    "dynamicRegistration": true,
                    "linkSupport": true,
                },
                "hover": {
                    "dynamicRegistration": true,
                    "contentFormat": ["markdown", "plaintext"],
                },
                "implementation": {
                    "dynamicRegistration": true,
                    "linkSupport": true,
                },
                "references": {
                    "dynamicRegistration": true,
                },
                "publishDiagnostics": {
                    "relatedInformation": false,
                    "tagSupport": { "valueSet": [1, 2] },
                    "versionSupport": true,
                    "codeDescriptionSupport": true,
                    "dataSupport": true,
                },
                "typeDefinition": {
                    "dynamicRegistration": true,
                    "linkSupport": true,
                },
            },
        },
        "initializationOptions": config.initialization_options(),
    })
}

fn normalize_diagnostics(
    diagnostics: Vec<Diagnostic>,
    document: &SynchronizedDocument,
    encoding: PositionEncoding,
    server: &str,
) -> Result<Vec<JsonValue>, LspError> {
    diagnostics
        .into_iter()
        .map(|mut diagnostic| {
            diagnostic.range = document
                .from_lsp_range(diagnostic.range, encoding)
                .map_err(|source| LspError::PositionConversion {
                    server: server.to_owned(),
                    path: document.absolute_path().to_path_buf(),
                    source,
                })?;
            serde_json::to_value(diagnostic).map_err(LspError::EncodeMessage)
        })
        .collect()
}

fn diagnostics_to_values(
    diagnostics: Vec<Diagnostic>,
) -> Result<Vec<JsonValue>, serde_json::Error> {
    diagnostics.into_iter().map(serde_json::to_value).collect()
}

fn path_to_file_uri(path: &Path) -> String {
    Url::from_file_path(path)
        .expect("project paths should be absolute file URI paths")
        .into()
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        Hover, ReadinessSource, ReadinessState, ReadinessTracker, ServerStatus,
    };

    #[test]
    fn readiness_prefers_health_failures_over_progress() {
        let mut readiness = ReadinessTracker::default();
        readiness.mark_initialized();
        readiness.start_progress("string:index".to_owned());
        readiness.record_server_status(ServerStatus {
            health: "warning".to_owned(),
            quiescent: false,
            message: Some("workspace load failed".to_owned()),
        });

        let snapshot = readiness.snapshot();

        assert_eq!(snapshot.state(), ReadinessState::Degraded);
        assert_eq!(snapshot.source(), ReadinessSource::ServerStatus);
        assert_eq!(snapshot.health(), Some("warning"));
        assert_eq!(snapshot.message(), Some("workspace load failed"));
        assert_eq!(snapshot.active_progress(), 1);
    }

    #[test]
    fn preserves_structured_hover_markup_and_renders_its_text() {
        let value = json!({
            "contents": {
                "kind": "markdown",
                "value": "  `answer`: the ultimate value\n",
            },
        });

        let hover: Hover = serde_json::from_value(value).unwrap();

        assert_eq!(hover.text(), "`answer`: the ultimate value");
        assert_eq!(
            serde_json::to_value(hover).unwrap(),
            json!({
                "contents": {
                    "kind": "markdown",
                    "value": "  `answer`: the ultimate value\n",
                },
            })
        );
    }

    #[test]
    fn renders_legacy_marked_strings_without_json_noise() {
        let hover: Hover = serde_json::from_value(json!({
            "contents": [
                { "language": "rust", "value": "fn answer() -> u8" },
                "Returns the ultimate value.",
            ],
        }))
        .unwrap();

        assert_eq!(
            hover.text(),
            "fn answer() -> u8\n\nReturns the ultimate value."
        );
    }
}
