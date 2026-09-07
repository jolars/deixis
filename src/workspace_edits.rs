use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    error::Error,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use similar::TextDiff;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::{
    documents::SynchronizedDocument,
    positions::{Position, PositionConverter, PositionEncoding, Range},
    project::Project,
};

const MAX_PENDING_PREVIEWS: usize = 16;
const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;
const PREVIEW_LIFETIME: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkspaceEdit {
    #[serde(default)]
    changes: Option<BTreeMap<String, Vec<RawTextEdit>>>,
    #[serde(default)]
    document_changes: Option<Vec<JsonValue>>,
    #[serde(default)]
    change_annotations: Option<BTreeMap<String, JsonValue>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawTextDocumentEdit {
    text_document: OptionalVersionedTextDocumentIdentifier,
    edits: Vec<RawTextEdit>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OptionalVersionedTextDocumentIdentifier {
    uri: String,
    version: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawTextEdit {
    range: Range,
    new_text: String,
    #[serde(default)]
    annotation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameEdit {
    pub range: Range,
    pub old_text: String,
    pub new_text: String,
    #[serde(skip)]
    start: usize,
    #[serde(skip)]
    end: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameFilePreview {
    pub path: String,
    pub uri: String,
    pub document_version: Option<i32>,
    pub position_encoding: PositionEncoding,
    pub edits: Vec<RenameEdit>,
    #[serde(skip)]
    absolute_path: PathBuf,
    #[serde(skip)]
    before: String,
}

impl RenameFilePreview {
    fn after(&self) -> String {
        apply_edits(&self.before, &self.edits)
    }

    fn retained_bytes(&self) -> usize {
        self.before.len()
            + self
                .edits
                .iter()
                .map(|edit| edit.old_text.len() + edit.new_text.len())
                .sum::<usize>()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenamePreview {
    pub preview_id: String,
    pub server: String,
    pub new_name: String,
    pub file_count: usize,
    pub edit_count: usize,
    pub expires_in_seconds: u64,
    pub files: Vec<RenameFilePreview>,
    #[serde(skip)]
    created_at: Instant,
}

impl RenamePreview {
    pub fn text(&self) -> String {
        if self.files.is_empty() {
            return "Rename produced no changes.".to_owned();
        }

        let mut output = format!(
            "Rename preview {} contains {} edit(s) in {} file(s).\n",
            self.preview_id, self.edit_count, self.file_count
        );
        for file in &self.files {
            let after = file.after();
            let diff = TextDiff::from_lines(&file.before, &after);
            output.push_str(
                &diff
                    .unified_diff()
                    .context_radius(3)
                    .header(
                        &format!("a/{}", file.path),
                        &format!("b/{}", file.path),
                    )
                    .to_string(),
            );
        }
        output.push_str(&format!(
            "When Deixis is started with `--allow-mutation`, apply this exact preview with `apply_rename` and previewId `{}`.",
            self.preview_id
        ));
        output
    }

    fn retained_bytes(&self) -> usize {
        self.files
            .iter()
            .map(RenameFilePreview::retained_bytes)
            .sum()
    }
}

#[derive(Debug, Default)]
pub(crate) struct PreviewStore {
    previews: VecDeque<RenamePreview>,
    retained_bytes: usize,
}

impl PreviewStore {
    pub(crate) fn insert(
        &mut self,
        server: &str,
        new_name: &str,
        files: Vec<RenameFilePreview>,
    ) -> Result<RenamePreview, WorkspaceEditError> {
        self.remove_expired();
        let file_count = files.len();
        let edit_count = files.iter().map(|file| file.edits.len()).sum();
        let preview = RenamePreview {
            preview_id: Uuid::new_v4().to_string(),
            server: server.to_owned(),
            new_name: new_name.to_owned(),
            file_count,
            edit_count,
            expires_in_seconds: PREVIEW_LIFETIME.as_secs(),
            files,
            created_at: Instant::now(),
        };
        let retained_bytes = preview.retained_bytes();
        if retained_bytes > MAX_PENDING_BYTES {
            return Err(WorkspaceEditError::InvalidEdit(format!(
                "rename preview retains {retained_bytes} bytes, exceeding the {MAX_PENDING_BYTES}-byte limit"
            )));
        }

        while self.previews.len() >= MAX_PENDING_PREVIEWS
            || self.retained_bytes.saturating_add(retained_bytes)
                > MAX_PENDING_BYTES
        {
            let evicted = self.previews.pop_front().expect(
                "a preview should exist while the store exceeds a limit",
            );
            self.retained_bytes =
                self.retained_bytes.saturating_sub(evicted.retained_bytes());
        }
        self.retained_bytes += retained_bytes;
        self.previews.push_back(preview.clone());
        Ok(preview)
    }

    pub(crate) fn take(
        &mut self,
        preview_id: &str,
    ) -> Result<RenamePreview, PreviewIdError> {
        self.remove_expired();
        let Some(index) = self
            .previews
            .iter()
            .position(|preview| preview.preview_id == preview_id)
        else {
            return Err(PreviewIdError {
                preview_id: preview_id.to_owned(),
            });
        };
        let preview = self
            .previews
            .remove(index)
            .expect("the located preview should still exist");
        self.retained_bytes =
            self.retained_bytes.saturating_sub(preview.retained_bytes());
        Ok(preview)
    }

    fn remove_expired(&mut self) {
        let now = Instant::now();
        while self.previews.front().is_some_and(|preview| {
            now.duration_since(preview.created_at) >= PREVIEW_LIFETIME
        }) {
            let expired = self
                .previews
                .pop_front()
                .expect("the expired preview should exist");
            self.retained_bytes =
                self.retained_bytes.saturating_sub(expired.retained_bytes());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreviewIdError {
    preview_id: String,
}

impl fmt::Display for PreviewIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "rename preview `{}` is unknown, expired, evicted, or already consumed",
            self.preview_id
        )
    }
}

impl Error for PreviewIdError {}

#[cfg(test)]
async fn normalize_workspace_edit(
    project: &Project,
    edit: WorkspaceEdit,
    encoding: PositionEncoding,
    tracked_documents: &BTreeMap<PathBuf, SynchronizedDocument>,
) -> Result<Vec<RenameFilePreview>, WorkspaceEditError> {
    normalize_workspace_edit_at_snapshot(
        project,
        edit,
        encoding,
        tracked_documents,
        tracked_documents,
    )
    .await
}

pub(crate) async fn normalize_workspace_edit_at_snapshot(
    project: &Project,
    edit: WorkspaceEdit,
    encoding: PositionEncoding,
    request_documents: &BTreeMap<PathBuf, SynchronizedDocument>,
    current_documents: &BTreeMap<PathBuf, SynchronizedDocument>,
) -> Result<Vec<RenameFilePreview>, WorkspaceEditError> {
    if edit
        .change_annotations
        .as_ref()
        .is_some_and(|annotations| !annotations.is_empty())
    {
        return Err(WorkspaceEditError::InvalidEdit(
            "change annotations are not supported".to_owned(),
        ));
    }

    let documents = if let Some(document_changes) = edit.document_changes {
        parse_document_changes(document_changes)?
    } else {
        edit.changes
            .unwrap_or_default()
            .into_iter()
            .map(|(uri, edits)| RawDocumentEdits {
                uri,
                version: None,
                edits,
            })
            .collect()
    };

    let mut canonical_paths = BTreeSet::new();
    let mut files = Vec::with_capacity(documents.len());
    for document in documents
        .into_iter()
        .filter(|document| !document.edits.is_empty())
    {
        let uri = Url::parse(&document.uri).map_err(|error| {
            WorkspaceEditError::InvalidEdit(format!(
                "workspace edit URI `{}` is invalid: {error}",
                document.uri
            ))
        })?;
        if uri.scheme() != "file" {
            return Err(WorkspaceEditError::InvalidEdit(format!(
                "workspace edit URI `{}` is not a file URI",
                document.uri
            )));
        }
        let path = uri.to_file_path().map_err(|()| {
            WorkspaceEditError::InvalidEdit(format!(
                "workspace edit URI `{}` cannot be converted to a file path",
                document.uri
            ))
        })?;
        let file = project.resolve_file(&path).map_err(|error| {
            WorkspaceEditError::InvalidEdit(error.to_string())
        })?;
        if !canonical_paths.insert(file.absolute().to_path_buf()) {
            return Err(WorkspaceEditError::InvalidEdit(format!(
                "workspace edit addresses `{}` more than once",
                file.relative().display()
            )));
        }

        let request_document = request_documents.get(file.absolute());
        if request_document != current_documents.get(file.absolute()) {
            return Err(WorkspaceEditError::Conflict {
                paths: vec![portable_path(file.relative())],
                cleanup_failures: Vec::new(),
            });
        }
        let before = tokio::fs::read_to_string(file.absolute()).await.map_err(
            |source| WorkspaceEditError::Read {
                path: file.relative().to_path_buf(),
                source,
            },
        )?;
        if let Some(tracked) = request_document {
            if tracked.text() != before {
                return Err(WorkspaceEditError::Conflict {
                    paths: vec![portable_path(file.relative())],
                    cleanup_failures: Vec::new(),
                });
            }
            if document
                .version
                .is_some_and(|version| version != tracked.version())
            {
                return Err(WorkspaceEditError::InvalidEdit(format!(
                    "workspace edit version for `{}` does not match synchronized version {}",
                    file.relative().display(),
                    tracked.version()
                )));
            }
        } else if document.version.is_some() {
            return Err(WorkspaceEditError::InvalidEdit(format!(
                "workspace edit supplies a version for unsynchronized document `{}`",
                file.relative().display()
            )));
        }

        let mut edits = document
            .edits
            .into_iter()
            .map(|edit| normalize_text_edit(edit, &before, encoding))
            .collect::<Result<Vec<_>, _>>()?;
        edits.sort_by_key(|edit| (edit.start, edit.end));
        reject_overlaps(&edits, file.relative())?;
        files.push(RenameFilePreview {
            path: portable_path(file.relative()),
            uri: document.uri,
            document_version: document.version,
            position_encoding: PositionEncoding::Utf8,
            edits,
            absolute_path: file.absolute().to_path_buf(),
            before,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

#[derive(Debug)]
struct RawDocumentEdits {
    uri: String,
    version: Option<i32>,
    edits: Vec<RawTextEdit>,
}

fn parse_document_changes(
    changes: Vec<JsonValue>,
) -> Result<Vec<RawDocumentEdits>, WorkspaceEditError> {
    changes
        .into_iter()
        .map(|change| {
            if change.get("kind").is_some() {
                return Err(WorkspaceEditError::InvalidEdit(
                    "resource operations are not supported".to_owned(),
                ));
            }
            let edit: RawTextDocumentEdit = serde_json::from_value(change)
                .map_err(|error| {
                    WorkspaceEditError::InvalidEdit(format!(
                        "invalid text document edit: {error}"
                    ))
                })?;
            Ok(RawDocumentEdits {
                uri: edit.text_document.uri,
                version: edit.text_document.version,
                edits: edit.edits,
            })
        })
        .collect()
}

fn normalize_text_edit(
    edit: RawTextEdit,
    text: &str,
    encoding: PositionEncoding,
) -> Result<RenameEdit, WorkspaceEditError> {
    if edit.annotation_id.is_some() {
        return Err(WorkspaceEditError::InvalidEdit(
            "annotated text edits are not supported".to_owned(),
        ));
    }
    let range = PositionConverter::new(text)
        .from_lsp_range(edit.range, encoding)
        .map_err(|error| WorkspaceEditError::InvalidEdit(error.to_string()))?;
    let start = position_offset(text, range.start)?;
    let end = position_offset(text, range.end)?;
    Ok(RenameEdit {
        range,
        old_text: text[start..end].to_owned(),
        new_text: edit.new_text,
        start,
        end,
    })
}

fn reject_overlaps(
    edits: &[RenameEdit],
    path: &Path,
) -> Result<(), WorkspaceEditError> {
    for pair in edits.windows(2) {
        let left = &pair[0];
        let right = &pair[1];
        if right.start < left.end || right.start == left.start {
            return Err(WorkspaceEditError::InvalidEdit(format!(
                "workspace edits overlap in `{}`",
                path.display()
            )));
        }
    }
    Ok(())
}

fn position_offset(
    text: &str,
    position: Position,
) -> Result<usize, WorkspaceEditError> {
    let wanted_line = usize::try_from(position.line).map_err(|_| {
        WorkspaceEditError::InvalidEdit("line coordinate overflow".to_owned())
    })?;
    let wanted_character =
        usize::try_from(position.character).map_err(|_| {
            WorkspaceEditError::InvalidEdit(
                "character coordinate overflow".to_owned(),
            )
        })?;
    let bytes = text.as_bytes();
    let mut line = 0;
    let mut line_start = 0;
    let mut cursor = 0;
    while cursor < bytes.len() && line < wanted_line {
        match bytes[cursor] {
            b'\r' if bytes.get(cursor + 1) == Some(&b'\n') => {
                cursor += 2;
                line += 1;
                line_start = cursor;
            }
            b'\r' | b'\n' => {
                cursor += 1;
                line += 1;
                line_start = cursor;
            }
            _ => cursor += 1,
        }
    }
    if line != wanted_line {
        return Err(WorkspaceEditError::InvalidEdit(format!(
            "line {} is outside the document",
            position.line
        )));
    }
    let offset = line_start.saturating_add(wanted_character);
    if offset > text.len() || !text.is_char_boundary(offset) {
        return Err(WorkspaceEditError::InvalidEdit(format!(
            "position {}:{} is not a UTF-8 boundary",
            position.line, position.character
        )));
    }
    let line_end = text[line_start..]
        .find(['\r', '\n'])
        .map_or(text.len(), |relative| line_start + relative);
    if offset > line_end {
        return Err(WorkspaceEditError::InvalidEdit(format!(
            "position {}:{} exceeds its line",
            position.line, position.character
        )));
    }
    Ok(offset)
}

fn apply_edits(text: &str, edits: &[RenameEdit]) -> String {
    let added = edits
        .iter()
        .map(|edit| edit.new_text.len().saturating_sub(edit.old_text.len()))
        .sum();
    let mut output = String::with_capacity(text.len().saturating_add(added));
    let mut cursor = 0;
    for edit in edits {
        output.push_str(&text[cursor..edit.start]);
        output.push_str(&edit.new_text);
        cursor = edit.end;
    }
    output.push_str(&text[cursor..]);
    output
}

fn portable_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyRenameOutcome {
    pub applied: bool,
    pub preview_id: String,
    pub server: String,
    pub file_count: usize,
    pub edit_count: usize,
    pub paths: Vec<String>,
    pub warnings: Vec<String>,
}

impl ApplyRenameOutcome {
    pub fn text(&self) -> String {
        let mut text = format!(
            "Applied rename preview {}: {} edit(s) in {} file(s).",
            self.preview_id, self.edit_count, self.file_count
        );
        if !self.warnings.is_empty() {
            text.push_str(" Warnings: ");
            text.push_str(&self.warnings.join("; "));
        }
        text
    }
}

pub(crate) fn apply_rename_preview(
    preview: RenamePreview,
    cancellation: &CancellationToken,
) -> Result<ApplyRenameOutcome, WorkspaceEditError> {
    apply_rename_preview_with(
        preview,
        cancellation,
        || {},
        |_, file| commit_file(file),
    )
}

fn apply_rename_preview_with(
    preview: RenamePreview,
    cancellation: &CancellationToken,
    after_final_validation: impl FnOnce(),
    mut commit: impl FnMut(usize, &mut StagedFile) -> io::Result<()>,
) -> Result<ApplyRenameOutcome, WorkspaceEditError> {
    if cancellation.is_cancelled() {
        return Err(WorkspaceEditError::Canceled {
            cleanup_failures: Vec::new(),
        });
    }
    let transaction_id = Uuid::new_v4();
    let mut staged = Vec::with_capacity(preview.files.len());

    validate_unchanged(&preview.files)?;
    for (index, file) in preview.files.iter().enumerate() {
        if cancellation.is_cancelled() {
            return Err(WorkspaceEditError::Canceled {
                cleanup_failures: cleanup_staged(&staged),
            });
        }
        let stage_path = sibling_transaction_path(
            &file.absolute_path,
            transaction_id,
            index,
            "new",
        )?;
        let backup_path = sibling_transaction_path(
            &file.absolute_path,
            transaction_id,
            index,
            "bak",
        )?;
        if let Err(failure) = stage_file(file, &stage_path) {
            let mut cleanup_failures = failure.cleanup_failures;
            cleanup_failures.extend(cleanup_staged(&staged));
            return Err(WorkspaceEditError::Stage {
                path: file.path.clone(),
                source: failure.source,
                cleanup_failures,
            });
        }
        staged.push(StagedFile {
            target: file.absolute_path.clone(),
            display_path: file.path.clone(),
            stage: stage_path,
            backup: backup_path,
            original_moved: false,
            replacement_installed: false,
        });
    }

    if cancellation.is_cancelled() {
        return Err(WorkspaceEditError::Canceled {
            cleanup_failures: cleanup_staged(&staged),
        });
    }
    if let Err(error) = validate_unchanged(&preview.files) {
        return Err(error.with_cleanup_failures(cleanup_staged(&staged)));
    }
    after_final_validation();
    if cancellation.is_cancelled() {
        return Err(WorkspaceEditError::Canceled {
            cleanup_failures: cleanup_staged(&staged),
        });
    }

    for index in 0..staged.len() {
        let path = staged[index].display_path.clone();
        let commit_result = commit(index, &mut staged[index]);
        if let Err(source) = commit_result {
            let rollback_failures = rollback(&mut staged);
            return Err(WorkspaceEditError::Application {
                path,
                source,
                rollback_failures,
            });
        }
    }

    let mut warnings = Vec::new();
    for file in &staged {
        if let Err(error) = fs::remove_file(&file.backup) {
            warnings.push(format!(
                "failed to remove backup `{}`: {error}",
                file.backup.display()
            ));
        }
    }

    Ok(ApplyRenameOutcome {
        applied: true,
        preview_id: preview.preview_id,
        server: preview.server,
        file_count: preview.file_count,
        edit_count: preview.edit_count,
        paths: preview.files.into_iter().map(|file| file.path).collect(),
        warnings,
    })
}

fn validate_unchanged(
    files: &[RenameFilePreview],
) -> Result<(), WorkspaceEditError> {
    let mut conflicts = Vec::new();
    for file in files {
        match fs::canonicalize(&file.absolute_path) {
            Ok(path) if path == file.absolute_path => {
                match fs::read_to_string(&path) {
                    Ok(current) if current == file.before => {}
                    Ok(_) => conflicts.push(file.path.clone()),
                    Err(_) => conflicts.push(file.path.clone()),
                }
            }
            Ok(_) | Err(_) => conflicts.push(file.path.clone()),
        }
    }
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(WorkspaceEditError::Conflict {
            paths: conflicts,
            cleanup_failures: Vec::new(),
        })
    }
}

fn sibling_transaction_path(
    target: &Path,
    transaction_id: Uuid,
    index: usize,
    suffix: &str,
) -> Result<PathBuf, WorkspaceEditError> {
    let parent = target.parent().ok_or_else(|| {
        WorkspaceEditError::InvalidEdit(format!(
            "edit target `{}` has no parent directory",
            target.display()
        ))
    })?;
    Ok(parent.join(format!(".deixis-{transaction_id}-{index}.{suffix}")))
}

#[derive(Debug)]
struct StageFileFailure {
    source: io::Error,
    cleanup_failures: Vec<String>,
}

fn stage_file(
    file: &RenameFilePreview,
    stage: &Path,
) -> Result<(), StageFileFailure> {
    stage_file_with(file, stage, |output| {
        output.write_all(file.after().as_bytes())?;
        output.sync_all()
    })
}

fn stage_file_with(
    file: &RenameFilePreview,
    stage: &Path,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> Result<(), StageFileFailure> {
    let permissions = fs::metadata(&file.absolute_path)
        .map_err(|source| StageFileFailure {
            source,
            cleanup_failures: Vec::new(),
        })?
        .permissions();
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        options.mode(permissions.mode());
    }
    let mut output =
        options.open(stage).map_err(|source| StageFileFailure {
            source,
            cleanup_failures: Vec::new(),
        })?;
    let result = fs::set_permissions(stage, permissions)
        .and_then(|()| write(&mut output));
    if let Err(source) = result {
        return Err(StageFileFailure {
            source,
            cleanup_failures: cleanup_paths([stage]),
        });
    }
    Ok(())
}

#[derive(Debug)]
struct StagedFile {
    target: PathBuf,
    display_path: String,
    stage: PathBuf,
    backup: PathBuf,
    original_moved: bool,
    replacement_installed: bool,
}

fn commit_file(file: &mut StagedFile) -> io::Result<()> {
    fs::rename(&file.target, &file.backup)?;
    file.original_moved = true;
    fs::rename(&file.stage, &file.target)?;
    file.replacement_installed = true;
    Ok(())
}

fn rollback(files: &mut [StagedFile]) -> Vec<String> {
    let mut failures = Vec::new();
    for file in files.iter_mut().rev() {
        if file.replacement_installed
            && let Err(error) = fs::remove_file(&file.target)
        {
            failures.push(format!(
                "failed to remove replacement `{}`: {error}",
                file.target.display()
            ));
            continue;
        }
        if file.original_moved
            && let Err(error) = fs::rename(&file.backup, &file.target)
        {
            failures.push(format!(
                "failed to restore `{}` from `{}`: {error}",
                file.target.display(),
                file.backup.display()
            ));
        }
        if !file.replacement_installed {
            remove_staged_path(&file.stage, &mut failures);
        }
    }
    failures
}

fn cleanup_staged(files: &[StagedFile]) -> Vec<String> {
    cleanup_paths(files.iter().map(|file| file.stage.as_path()))
}

fn cleanup_paths<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Vec<String> {
    let mut failures = Vec::new();
    for path in paths {
        remove_staged_path(path, &mut failures);
    }
    failures
}

fn remove_staged_path(path: &Path, failures: &mut Vec<String>) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => failures.push(format!(
            "failed to remove staged file `{}`: {error}",
            path.display()
        )),
    }
}

fn write_cleanup_failures(
    formatter: &mut fmt::Formatter<'_>,
    failures: &[String],
) -> fmt::Result {
    if !failures.is_empty() {
        write!(
            formatter,
            "; staged-file cleanup was incomplete: {}",
            failures.join("; ")
        )?;
    }
    Ok(())
}

impl WorkspaceEditError {
    fn with_cleanup_failures(mut self, mut failures: Vec<String>) -> Self {
        match &mut self {
            Self::Conflict {
                cleanup_failures, ..
            }
            | Self::Stage {
                cleanup_failures, ..
            }
            | Self::Canceled { cleanup_failures } => {
                cleanup_failures.append(&mut failures);
            }
            Self::InvalidEdit(_)
            | Self::Read { .. }
            | Self::Application { .. } => {}
        }
        self
    }
}

#[derive(Debug)]
pub enum WorkspaceEditError {
    InvalidEdit(String),
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Conflict {
        paths: Vec<String>,
        cleanup_failures: Vec<String>,
    },
    Stage {
        path: String,
        source: io::Error,
        cleanup_failures: Vec<String>,
    },
    Application {
        path: String,
        source: io::Error,
        rollback_failures: Vec<String>,
    },
    Canceled {
        cleanup_failures: Vec<String>,
    },
}

impl WorkspaceEditError {
    pub(crate) fn rollback_failed(&self) -> bool {
        matches!(
            self,
            Self::Application {
                rollback_failures,
                ..
            } if !rollback_failures.is_empty()
        )
    }
}

impl fmt::Display for WorkspaceEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEdit(message) => formatter.write_str(message),
            Self::Read { path, source } => write!(
                formatter,
                "failed to read edit target `{}`: {source}",
                path.display()
            ),
            Self::Conflict {
                paths,
                cleanup_failures,
            } => {
                write!(
                    formatter,
                    "rename preview conflicts with current contents of {}",
                    paths
                        .iter()
                        .map(|path| format!("`{path}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )?;
                write_cleanup_failures(formatter, cleanup_failures)
            }
            Self::Stage {
                path,
                source,
                cleanup_failures,
            } => {
                write!(formatter, "failed to stage `{path}`: {source}")?;
                write_cleanup_failures(formatter, cleanup_failures)
            }
            Self::Application {
                path,
                source,
                rollback_failures,
            } => {
                write!(formatter, "failed to apply `{path}`: {source}")?;
                if rollback_failures.is_empty() {
                    formatter.write_str("; all prior changes were rolled back")
                } else {
                    write!(
                        formatter,
                        "; rollback was incomplete: {}",
                        rollback_failures.join("; ")
                    )
                }
            }
            Self::Canceled { cleanup_failures } => {
                formatter.write_str(
                    "rename application was canceled before commit",
                )?;
                write_cleanup_failures(formatter, cleanup_failures)
            }
        }
    }
}

impl Error for WorkspaceEditError {}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        env, fs,
        io::{self, Write},
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::json;
    use tokio_util::sync::CancellationToken;
    use url::Url;

    use super::{
        PreviewStore, StagedFile, WorkspaceEdit, WorkspaceEditError,
        apply_rename_preview, apply_rename_preview_with, cleanup_staged,
        commit_file, normalize_workspace_edit, stage_file_with,
    };
    use crate::{cli::CliOptions, workspace_edits::MAX_PENDING_PREVIEWS};
    use crate::{
        documents::DocumentStore,
        positions::PositionEncoding,
        project::{Project, StartupState},
    };

    #[tokio::test]
    async fn normalizes_versioned_utf16_edits_and_renders_a_diff() {
        let root = unique_dir("normalize");
        let path = root.join("main.rs");
        fs::write(&path, "let crab = \"🦀\";\nprintln!(\"{crab}\");\n")
            .unwrap();
        let project = project(&root);
        let uri = Url::from_file_path(fs::canonicalize(&path).unwrap())
            .unwrap()
            .to_string();
        let edit: WorkspaceEdit = serde_json::from_value(json!({
            "documentChanges": [{
                "textDocument": { "uri": uri, "version": null },
                "edits": [{
                    "range": {
                        "start": { "line": 1, "character": 11 },
                        "end": { "line": 1, "character": 15 }
                    },
                    "newText": "ferris"
                }]
            }]
        }))
        .unwrap();

        let files = normalize_workspace_edit(
            &project,
            edit,
            PositionEncoding::Utf16,
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(files[0].edits[0].old_text, "crab");
        assert_eq!(files[0].edits[0].range.start.character, 11);

        let mut store = PreviewStore::default();
        let preview = store.insert("rust", "ferris", files).unwrap();
        let text = preview.text();
        assert!(text.contains("--- a/main.rs"), "{text}");
        assert!(text.contains("+println!(\"{ferris}\");"), "{text}");
    }

    #[tokio::test]
    async fn rejects_resource_operations_and_overlapping_edits() {
        let root = unique_dir("invalid");
        let path = root.join("main.rs");
        fs::write(&path, "abcd\n").unwrap();
        let project = project(&root);
        let uri = Url::from_file_path(fs::canonicalize(&path).unwrap())
            .unwrap()
            .to_string();
        let resource: WorkspaceEdit = serde_json::from_value(json!({
            "documentChanges": [{ "kind": "delete", "uri": uri }]
        }))
        .unwrap();
        assert!(matches!(
            normalize_workspace_edit(
                &project,
                resource,
                PositionEncoding::Utf8,
                &BTreeMap::new(),
            )
            .await,
            Err(WorkspaceEditError::InvalidEdit(message))
                if message.contains("resource operations")
        ));

        let overlap: WorkspaceEdit = serde_json::from_value(json!({
            "changes": { uri: [
                {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 2 }
                    },
                    "newText": "x"
                },
                {
                    "range": {
                        "start": { "line": 0, "character": 1 },
                        "end": { "line": 0, "character": 3 }
                    },
                    "newText": "y"
                }
            ] }
        }))
        .unwrap();
        assert!(matches!(
            normalize_workspace_edit(
                &project,
                overlap,
                PositionEncoding::Utf8,
                &BTreeMap::new(),
            )
            .await,
            Err(WorkspaceEditError::InvalidEdit(message))
                if message.contains("overlap")
        ));
    }

    #[tokio::test]
    async fn drops_documents_with_no_text_edits() {
        let root = unique_dir("empty-edits");
        let path = root.join("main.rs");
        fs::write(&path, "let old = 1;\n").unwrap();
        let project = project(&root);
        let uri = Url::from_file_path(fs::canonicalize(&path).unwrap())
            .unwrap()
            .to_string();
        let edit: WorkspaceEdit = serde_json::from_value(json!({
            "changes": { uri: [] }
        }))
        .unwrap();

        let files = normalize_workspace_edit(
            &project,
            edit,
            PositionEncoding::Utf8,
            &BTreeMap::new(),
        )
        .await
        .unwrap();

        assert!(files.is_empty());
    }

    #[test]
    fn removes_a_partially_written_current_stage_after_failure() {
        let root = unique_dir("partial-stage");
        let target = root.join("main.rs");
        let stage = root.join(".deixis-partial.new");
        fs::write(&target, "old\n").unwrap();
        let file = rename_file(&target, "old\n", "new\n");

        let error = stage_file_with(&file, &stage, |output| {
            output.write_all(b"partial")?;
            Err(io::Error::other("injected write failure"))
        })
        .unwrap_err();

        assert_eq!(error.source.to_string(), "injected write failure");
        assert!(error.cleanup_failures.is_empty());
        assert!(!stage.exists());
    }

    #[cfg(unix)]
    #[test]
    fn creates_a_stage_with_the_targets_permissions_before_writing() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_dir("stage-permissions");
        let target = root.join("private.rs");
        let stage = root.join(".deixis-private.new");
        fs::write(&target, "old\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))
            .unwrap();
        let file = rename_file(&target, "old\n", "new\n");

        stage_file_with(&file, &stage, |output| {
            let mode = fs::metadata(&stage)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            output.write_all(b"new\n")
        })
        .unwrap();
    }

    #[test]
    fn reports_failures_to_remove_staged_files() {
        let root = unique_dir("cleanup-failure");
        let stage = root.join(".deixis-stage.new");
        fs::create_dir(&stage).unwrap();
        let staged = vec![StagedFile {
            target: root.join("main.rs"),
            display_path: "main.rs".to_owned(),
            stage: stage.clone(),
            backup: root.join(".deixis-stage.bak"),
            original_moved: false,
            replacement_installed: false,
        }];

        let failures = cleanup_staged(&staged);

        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains(&stage.display().to_string()));
        let error = WorkspaceEditError::Canceled {
            cleanup_failures: failures,
        };
        assert!(
            error
                .to_string()
                .contains("staged-file cleanup was incomplete")
        );
    }

    #[tokio::test]
    async fn rejects_external_non_utf8_and_stale_versioned_targets() {
        let root = unique_dir("target-validation");
        let path = root.join("main.rs");
        fs::write(&path, "old\n").unwrap();
        let project = project(&root);
        let absolute = fs::canonicalize(&path).unwrap();
        let uri = Url::from_file_path(&absolute).unwrap().to_string();

        let external_root = unique_dir("external-target");
        let external = external_root.join("outside.rs");
        fs::write(&external, "old\n").unwrap();
        let external_edit = workspace_edit(&[(&external, 0, 3)]);
        assert!(matches!(
            normalize_workspace_edit(
                &project,
                external_edit,
                PositionEncoding::Utf8,
                &BTreeMap::new(),
            )
            .await,
            Err(WorkspaceEditError::InvalidEdit(message))
                if message.contains("outside project root")
        ));

        let mut documents = DocumentStore::default();
        documents
            .synchronize(
                &absolute,
                Path::new("main.rs"),
                uri.clone(),
                "rust",
                "old\n".to_owned(),
                true,
            )
            .unwrap();
        let wrong_version: WorkspaceEdit = serde_json::from_value(json!({
            "documentChanges": [{
                "textDocument": { "uri": uri, "version": 2 },
                "edits": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 3 }
                    },
                    "newText": "new"
                }]
            }]
        }))
        .unwrap();
        assert!(matches!(
            normalize_workspace_edit(
                &project,
                wrong_version,
                PositionEncoding::Utf8,
                &documents.snapshots(),
            )
            .await,
            Err(WorkspaceEditError::InvalidEdit(message))
                if message.contains("does not match synchronized version")
        ));

        fs::write(&path, "changed\n").unwrap();
        let stale = workspace_edit(&[(&path, 0, 3)]);
        assert!(matches!(
            normalize_workspace_edit(
                &project,
                stale,
                PositionEncoding::Utf8,
                &documents.snapshots(),
            )
            .await,
            Err(WorkspaceEditError::Conflict { .. })
        ));

        let binary = root.join("binary.rs");
        fs::write(&binary, [0xff, 0xfe]).unwrap();
        let binary_edit = workspace_edit(&[(&binary, 0, 0)]);
        assert!(matches!(
            normalize_workspace_edit(
                &project,
                binary_edit,
                PositionEncoding::Utf8,
                &BTreeMap::new(),
            )
            .await,
            Err(WorkspaceEditError::Read { .. })
        ));
    }

    #[tokio::test]
    async fn applies_a_multi_file_preview_and_detects_conflicts() {
        let root = unique_dir("apply");
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        fs::write(&first, "fn old() {}\n").unwrap();
        fs::write(&second, "old();\n").unwrap();
        let project = project(&root);
        let edit = workspace_edit(&[(&first, 3, 6), (&second, 0, 3)]);
        let files = normalize_workspace_edit(
            &project,
            edit,
            PositionEncoding::Utf8,
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        let mut store = PreviewStore::default();
        let preview = store.insert("rust", "new", files).unwrap();
        let outcome = apply_rename_preview(
            store.take(&preview.preview_id).unwrap(),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(outcome.file_count, 2);
        assert_eq!(fs::read_to_string(&first).unwrap(), "fn new() {}\n");
        assert_eq!(fs::read_to_string(&second).unwrap(), "new();\n");

        let edit = workspace_edit(&[(&first, 3, 6), (&second, 0, 3)]);
        let files = normalize_workspace_edit(
            &project,
            edit,
            PositionEncoding::Utf8,
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        let preview = store.insert("rust", "next", files).unwrap();
        fs::write(&second, "changed();\n").unwrap();
        assert!(matches!(
            apply_rename_preview(
                store.take(&preview.preview_id).unwrap(),
                &CancellationToken::new(),
            ),
            Err(WorkspaceEditError::Conflict { .. })
        ));
        assert_eq!(fs::read_to_string(&first).unwrap(), "fn new() {}\n");
    }

    #[tokio::test]
    async fn rolls_back_files_after_a_commit_failure() {
        let root = unique_dir("rollback");
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        fs::write(&first, "fn old() {}\n").unwrap();
        fs::write(&second, "old();\n").unwrap();
        let project = project(&root);
        let edit = workspace_edit(&[(&first, 3, 6), (&second, 0, 3)]);
        let files = normalize_workspace_edit(
            &project,
            edit,
            PositionEncoding::Utf8,
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        let mut store = PreviewStore::default();
        let preview = store.insert("rust", "new", files).unwrap();

        let error = apply_rename_preview_with(
            store.take(&preview.preview_id).unwrap(),
            &CancellationToken::new(),
            || {},
            |index, file| {
                if index == 1 {
                    fs::rename(&file.target, &file.backup)?;
                    file.original_moved = true;
                    return Err(io::Error::other("injected commit failure"));
                }
                commit_file(file)
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            WorkspaceEditError::Application {
                rollback_failures,
                ..
            } if rollback_failures.is_empty()
        ));
        assert_eq!(fs::read_to_string(&first).unwrap(), "fn old() {}\n");
        assert_eq!(fs::read_to_string(&second).unwrap(), "old();\n");
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".deixis-")
        }));
    }

    #[tokio::test]
    async fn honors_cancellation_after_final_validation() {
        let root = unique_dir("cancel-after-validation");
        let target = root.join("main.rs");
        fs::write(&target, "old\n").unwrap();
        let project = project(&root);
        let files = normalize_workspace_edit(
            &project,
            workspace_edit(&[(&target, 0, 3)]),
            PositionEncoding::Utf8,
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        let mut store = PreviewStore::default();
        let preview = store.insert("rust", "new", files).unwrap();
        let cancellation = CancellationToken::new();

        let error = apply_rename_preview_with(
            store.take(&preview.preview_id).unwrap(),
            &cancellation,
            || cancellation.cancel(),
            |_, file| commit_file(file),
        )
        .unwrap_err();

        assert!(matches!(error, WorkspaceEditError::Canceled { .. }));
        assert_eq!(fs::read_to_string(&target).unwrap(), "old\n");
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".deixis-")
        }));
    }

    #[test]
    fn evicts_old_previews_and_consumes_ids_once() {
        let mut store = PreviewStore::default();
        let mut first_id = String::new();
        for index in 0..=MAX_PENDING_PREVIEWS {
            let preview = store
                .insert("rust", &format!("name{index}"), Vec::new())
                .unwrap();
            if index == 0 {
                first_id = preview.preview_id;
            }
        }
        assert!(store.take(&first_id).is_err());
        let preview = store.insert("rust", "last", Vec::new()).unwrap();
        assert!(store.take(&preview.preview_id).is_ok());
        assert!(store.take(&preview.preview_id).is_err());
    }

    fn workspace_edit(files: &[(&Path, u32, u32)]) -> WorkspaceEdit {
        let changes = files
            .iter()
            .map(|(path, start, end)| {
                (
                    Url::from_file_path(fs::canonicalize(path).unwrap())
                        .unwrap()
                        .to_string(),
                    json!([{
                        "range": {
                            "start": { "line": 0, "character": start },
                            "end": { "line": 0, "character": end }
                        },
                        "newText": "new"
                    }]),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        serde_json::from_value(json!({ "changes": changes })).unwrap()
    }

    fn rename_file(
        target: &Path,
        before: &str,
        after: &str,
    ) -> super::RenameFilePreview {
        super::RenameFilePreview {
            path: target.file_name().unwrap().to_string_lossy().into_owned(),
            uri: Url::from_file_path(target).unwrap().to_string(),
            document_version: None,
            position_encoding: PositionEncoding::Utf8,
            edits: vec![super::RenameEdit {
                range: crate::positions::Range::new(
                    crate::positions::Position::new(0, 0),
                    crate::positions::Position::new(
                        0,
                        u32::try_from(before.trim_end().len()).unwrap(),
                    ),
                ),
                old_text: before.trim_end().to_owned(),
                new_text: after.trim_end().to_owned(),
                start: 0,
                end: before.trim_end().len(),
            }],
            absolute_path: target.to_path_buf(),
            before: before.to_owned(),
        }
    }

    fn project(root: &Path) -> Project {
        StartupState::from_options_in(
            CliOptions::new(None, Some(root.to_path_buf())),
            root,
        )
        .unwrap()
        .project()
        .clone()
    }

    fn unique_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "deixis-workspace-edits-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
