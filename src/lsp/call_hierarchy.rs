use super::{
    BTreeMap, CancellationToken, Deserialize, Instant, JsonValue,
    LazyLanguageServer, LspError, Path, Position, PositionConverter,
    PositionEncoding, Range, Serialize, SynchronizedDocument, Url, json,
};

const PREPARE_METHOD: &str = "textDocument/prepareCallHierarchy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallHierarchyDirection {
    Incoming,
    Outgoing,
}

impl CallHierarchyDirection {
    pub(crate) fn method(self) -> &'static str {
        match self {
            Self::Incoming => "callHierarchy/incomingCalls",
            Self::Outgoing => "callHierarchy/outgoingCalls",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyItem {
    pub server: String,
    pub name: String,
    pub kind: u32,
    pub uri: String,
    pub range: Range,
    pub selection_range: Range,
    pub position_encoding: PositionEncoding,
    #[serde(flatten)]
    fields: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyCall {
    pub from: CallHierarchyItem,
    pub to: CallHierarchyItem,
    /// Call sites in `from.uri`, using `from.position_encoding`.
    pub from_ranges: Vec<Range>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawItem {
    name: String,
    kind: u32,
    uri: String,
    range: Range,
    selection_range: Range,
    #[serde(flatten)]
    fields: BTreeMap<String, JsonValue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIncomingCall {
    from: RawItem,
    from_ranges: Vec<Range>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawOutgoingCall {
    to: RawItem,
    from_ranges: Vec<Range>,
}

impl LazyLanguageServer {
    pub async fn call_hierarchy(
        &self,
        path: impl AsRef<Path>,
        language_id: &str,
        position: Position,
        direction: CallHierarchyDirection,
    ) -> Result<Vec<CallHierarchyCall>, LspError> {
        self.call_hierarchy_with_cancellation(
            path,
            language_id,
            position,
            direction,
            &CancellationToken::new(),
        )
        .await
    }

    pub(crate) async fn call_hierarchy_with_cancellation(
        &self,
        path: impl AsRef<Path>,
        language_id: &str,
        position: Position,
        direction: CallHierarchyDirection,
        cancellation: &CancellationToken,
    ) -> Result<Vec<CallHierarchyCall>, LspError> {
        let file = self
            .project
            .resolve_file(path)
            .map_err(LspError::DocumentPath)?;
        let active = self.active_server_with_cancellation(cancellation).await?;
        let snapshot = active.status.lock().await.clone();
        let capability = snapshot.capabilities().get("callHierarchyProvider");
        if !active.supports_method(PREPARE_METHOD, capability).await {
            return Err(LspError::UnsupportedCapability {
                server: self.config.name().to_owned(),
                method: PREPARE_METHOD,
            });
        }
        let document = active.synchronize_document(file, language_id).await?;
        let encoding = snapshot.position_encoding().unwrap_or_default();
        let position =
            document
                .to_lsp_position(position, encoding)
                .map_err(|source| LspError::PositionConversion {
                    server: self.config.name().to_owned(),
                    path: document.absolute_path().to_path_buf(),
                    source,
                })?;
        let request_timeout = self.config.timeouts().request();
        // Preparation, all expansions, and retries share the query's deadline.
        let deadline = Instant::now() + request_timeout;
        let value = active.request_value_until(
            PREPARE_METHOD,
            Some(json!({ "textDocument": { "uri": document.uri() }, "position": position })),
            deadline, request_timeout, cancellation,
        ).await?;
        let items: Option<Vec<JsonValue>> =
            serde_json::from_value(value).map_err(LspError::DecodeResult)?;
        let mut calls = Vec::new();
        for value in items.into_iter().flatten() {
            let prepared: RawItem = serde_json::from_value(value.clone())
                .map_err(LspError::DecodeResult)?;
            // Both expansion methods use the prepare capability, including dynamic registration.
            if !active.supports_method(PREPARE_METHOD, capability).await {
                return Err(LspError::UnsupportedCapability {
                    server: self.config.name().to_owned(),
                    method: direction.method(),
                });
            }
            let response = active
                .request_value_until(
                    direction.method(),
                    Some(json!({ "item": value })),
                    deadline,
                    request_timeout,
                    cancellation,
                )
                .await?;
            let edges: Vec<(RawItem, RawItem, Vec<Range>)> = match direction {
                CallHierarchyDirection::Incoming => {
                    let response: Option<Vec<RawIncomingCall>> =
                        serde_json::from_value(response)
                            .map_err(LspError::DecodeResult)?;
                    response
                        .into_iter()
                        .flatten()
                        .map(|call| {
                            (call.from, prepared.clone(), call.from_ranges)
                        })
                        .collect()
                }
                CallHierarchyDirection::Outgoing => {
                    let response: Option<Vec<RawOutgoingCall>> =
                        serde_json::from_value(response)
                            .map_err(LspError::DecodeResult)?;
                    response
                        .into_iter()
                        .flatten()
                        .map(|call| {
                            (prepared.clone(), call.to, call.from_ranges)
                        })
                        .collect()
                }
            };
            for (from, to, ranges) in edges {
                active.check_request_deadline(
                    direction.method(),
                    deadline,
                    request_timeout,
                    cancellation,
                )?;
                let (from, from_ranges) = self
                    .normalize_call_item(&document, from, ranges, encoding)
                    .await?;
                let (to, _) = self
                    .normalize_call_item(&document, to, Vec::new(), encoding)
                    .await?;
                calls.push(CallHierarchyCall {
                    from,
                    to,
                    from_ranges,
                });
            }
        }
        active.check_request_deadline(
            direction.method(),
            deadline,
            request_timeout,
            cancellation,
        )?;
        Ok(calls)
    }

    async fn normalize_call_item(
        &self,
        document: &SynchronizedDocument,
        mut item: RawItem,
        mut ranges: Vec<Range>,
        encoding: PositionEncoding,
    ) -> Result<(CallHierarchyItem, Vec<Range>), LspError> {
        let source = if item.uri == document.uri() {
            Some((
                document.absolute_path().to_path_buf(),
                document.text().to_owned(),
            ))
        } else if let Some(file) = Url::parse(&item.uri)
            .ok()
            .and_then(|uri| uri.to_file_path().ok())
            .and_then(|path| self.project.resolve_file(path).ok())
        {
            tokio::fs::read_to_string(file.absolute())
                .await
                .ok()
                .map(|text| (file.absolute().to_path_buf(), text))
        } else {
            None
        };
        let position_encoding = if let Some((path, text)) = source {
            // The caller and its call sites must be converted against the same content.
            let converter = PositionConverter::new(&text);
            let convert = |range| {
                converter.from_lsp_range(range, encoding).map_err(|source| {
                    LspError::PositionConversion {
                        server: self.config.name().to_owned(),
                        path: path.clone(),
                        source,
                    }
                })
            };
            item.range = convert(item.range)?;
            item.selection_range = convert(item.selection_range)?;
            ranges =
                ranges.into_iter().map(convert).collect::<Result<_, _>>()?;
            PositionEncoding::Utf8
        } else {
            encoding
        };
        item.fields.remove("server");
        item.fields.remove("positionEncoding");
        Ok((
            CallHierarchyItem {
                server: self.config.name().to_owned(),
                name: item.name,
                kind: item.kind,
                uri: item.uri,
                range: item.range,
                selection_range: item.selection_range,
                position_encoding,
                fields: item.fields,
            },
            ranges,
        ))
    }
}
