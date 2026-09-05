use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynchronizedDocument {
    absolute_path: PathBuf,
    relative_path: PathBuf,
    uri: String,
    language_id: String,
    version: i32,
    text: String,
}

impl SynchronizedDocument {
    pub fn absolute_path(&self) -> &Path {
        &self.absolute_path
    }

    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn language_id(&self) -> &str {
        &self.language_id
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Default)]
pub(crate) struct DocumentStore {
    documents: BTreeMap<PathBuf, TrackedDocument>,
    closed: bool,
}

impl DocumentStore {
    pub(crate) fn synchronize(
        &mut self,
        absolute_path: &Path,
        relative_path: &Path,
        uri: String,
        language_id: &str,
        text: String,
        close_on_shutdown: bool,
    ) -> Result<DocumentUpdate, DocumentStoreError> {
        if self.closed {
            return Err(DocumentStoreError::Closed {
                path: absolute_path.to_path_buf(),
            });
        }

        let hash = content_hash(&text);
        let Some(tracked) = self.documents.get_mut(absolute_path) else {
            let document = SynchronizedDocument {
                absolute_path: absolute_path.to_path_buf(),
                relative_path: relative_path.to_path_buf(),
                uri,
                language_id: language_id.to_owned(),
                version: 1,
                text,
            };
            self.documents.insert(
                absolute_path.to_path_buf(),
                TrackedDocument {
                    document: document.clone(),
                    hash,
                    close_on_shutdown,
                },
            );
            return Ok(DocumentUpdate::Opened {
                document,
                notify: close_on_shutdown,
            });
        };

        if tracked.document.language_id != language_id {
            return Err(DocumentStoreError::LanguageChanged {
                path: absolute_path.to_path_buf(),
                previous: tracked.document.language_id.clone(),
                requested: language_id.to_owned(),
            });
        }

        if tracked.hash == hash && tracked.document.text == text {
            return Ok(DocumentUpdate::Unchanged(tracked.document.clone()));
        }

        let version =
            tracked.document.version.checked_add(1).ok_or_else(|| {
                DocumentStoreError::VersionOverflow {
                    path: absolute_path.to_path_buf(),
                }
            })?;
        let previous_text = std::mem::replace(&mut tracked.document.text, text);
        tracked.document.version = version;
        tracked.hash = hash;

        Ok(DocumentUpdate::Changed {
            document: tracked.document.clone(),
            previous_text,
        })
    }

    pub(crate) fn close_all(&mut self) -> Vec<SynchronizedDocument> {
        self.closed = true;
        std::mem::take(&mut self.documents)
            .into_values()
            .filter_map(|tracked| {
                tracked.close_on_shutdown.then_some(tracked.document)
            })
            .collect()
    }
}

#[derive(Debug)]
struct TrackedDocument {
    document: SynchronizedDocument,
    hash: u64,
    close_on_shutdown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DocumentUpdate {
    Opened {
        document: SynchronizedDocument,
        notify: bool,
    },
    Changed {
        document: SynchronizedDocument,
        previous_text: String,
    },
    Unchanged(SynchronizedDocument),
}

impl DocumentUpdate {
    pub(crate) fn document(&self) -> &SynchronizedDocument {
        match self {
            Self::Opened { document, .. }
            | Self::Changed { document, .. }
            | Self::Unchanged(document) => document,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DocumentStoreError {
    Closed {
        path: PathBuf,
    },
    LanguageChanged {
        path: PathBuf,
        previous: String,
        requested: String,
    },
    VersionOverflow {
        path: PathBuf,
    },
}

fn content_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{DocumentStore, DocumentStoreError, DocumentUpdate};

    #[test]
    fn opens_once_and_advances_versions_only_for_content_changes() {
        let mut store = DocumentStore::default();
        let absolute = Path::new("/project/main.rs");
        let relative = Path::new("main.rs");

        let opened = store
            .synchronize(
                absolute,
                relative,
                "file:///project/main.rs".to_owned(),
                "rust",
                "first".to_owned(),
                true,
            )
            .unwrap();
        assert!(matches!(
            opened,
            DocumentUpdate::Opened { notify: true, .. }
        ));
        assert_eq!(opened.document().version(), 1);

        let unchanged = store
            .synchronize(
                absolute,
                relative,
                "file:///project/main.rs".to_owned(),
                "rust",
                "first".to_owned(),
                true,
            )
            .unwrap();
        assert!(matches!(unchanged, DocumentUpdate::Unchanged(_)));
        assert_eq!(unchanged.document().version(), 1);

        let changed = store
            .synchronize(
                absolute,
                relative,
                "file:///project/main.rs".to_owned(),
                "rust",
                "second".to_owned(),
                true,
            )
            .unwrap();
        assert!(matches!(changed, DocumentUpdate::Changed { .. }));
        assert_eq!(changed.document().version(), 2);

        let reverted = store
            .synchronize(
                absolute,
                relative,
                "file:///project/main.rs".to_owned(),
                "rust",
                "first".to_owned(),
                true,
            )
            .unwrap();
        assert_eq!(reverted.document().version(), 3);
    }

    #[test]
    fn rejects_language_changes_for_an_open_document() {
        let mut store = DocumentStore::default();
        let absolute = Path::new("/project/main");
        let relative = Path::new("main");
        store
            .synchronize(
                absolute,
                relative,
                "file:///project/main".to_owned(),
                "rust",
                String::new(),
                true,
            )
            .unwrap();

        let error = store
            .synchronize(
                absolute,
                relative,
                "file:///project/main".to_owned(),
                "python",
                String::new(),
                true,
            )
            .unwrap_err();
        assert!(matches!(error, DocumentStoreError::LanguageChanged { .. }));
    }

    #[test]
    fn closes_only_documents_opened_by_notification() {
        let mut store = DocumentStore::default();
        for (name, close_on_shutdown) in
            [("open.rs", true), ("tracked.rs", false)]
        {
            let absolute = format!("/project/{name}");
            store
                .synchronize(
                    Path::new(&absolute),
                    Path::new(name),
                    format!("file://{absolute}"),
                    "rust",
                    String::new(),
                    close_on_shutdown,
                )
                .unwrap();
        }

        let documents = store.close_all();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].relative_path(), Path::new("open.rs"));
        assert!(store.close_all().is_empty());
    }

    #[test]
    fn cannot_reopen_documents_after_shutdown_begins() {
        let mut store = DocumentStore::default();
        assert!(store.close_all().is_empty());

        let error = store
            .synchronize(
                Path::new("/project/main.rs"),
                Path::new("main.rs"),
                "file:///project/main.rs".to_owned(),
                "rust",
                String::new(),
                true,
            )
            .unwrap_err();
        assert!(matches!(error, DocumentStoreError::Closed { .. }));
    }
}
