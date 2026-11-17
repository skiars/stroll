use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};

use crate::{git::Mode, tabs::TabSession};

pub(crate) const VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceView {
    #[default]
    Editor,
    Docs,
    Git,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct WorktreeSession {
    pub view: Option<WorkspaceView>,
    pub tabs: TabSession,
    pub files: BTreeMap<PathBuf, FilePosition>,
    pub document: Option<PathBuf>,
    pub documents: BTreeMap<PathBuf, DocumentPosition>,
    // None represents a session saved before the first visit to the document list.
    pub open_documents: Option<Vec<OpenDocument>>,
    pub git: Option<GitSession>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_draft: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct OpenDocument {
    pub path: PathBuf,
    pub title: Option<String>,
}

impl WorktreeSession {
    pub fn select_document(&mut self, path: &Path) {
        let documents = self.open_documents.get_or_insert_with(|| {
            self.document
                .iter()
                .cloned()
                .map(|path| OpenDocument { path, title: None })
                .collect()
        });
        if !documents.iter().any(|document| document.path == path) {
            documents.push(OpenDocument {
                path: path.to_owned(),
                title: None,
            });
        }
        self.document = Some(path.to_owned());
    }

    pub fn close_document(&mut self, path: &Path) -> Option<usize> {
        let documents = self.open_documents.as_mut()?;
        let index = documents
            .iter()
            .position(|document| document.path == path)?;
        documents.remove(index);
        if self.document.as_deref() == Some(path) {
            self.document = documents
                .get(index)
                .or_else(|| documents.last())
                .map(|document| document.path.clone());
        }
        Some(index)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct FilePosition {
    pub selection_start: (u32, u32),
    pub selection_end: (u32, u32),
    pub reversed: bool,
    pub top: (u32, u32),
    pub inset: f32,
    pub horizontal: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct DocumentPosition {
    pub source_offset: Option<usize>,
    pub block: usize,
    pub inset: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct GitSession {
    pub reference: String,
    pub query: String,
    pub selection: Option<GitSelection>,
    pub history: Option<HistoryPosition>,
    pub diff: Option<DiffPosition>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub worktree_directories: Arc<BTreeSet<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GitSelection {
    Commit(String, Option<String>),
    Worktree(Mode),
    Compare {
        base: Option<String>,
        target: Option<String>,
        previous: Box<GitSelection>,
    },
}

#[cfg(feature = "desktop")]
impl GitSelection {
    pub(crate) fn target_commit(&self) -> Option<&str> {
        match self {
            Self::Commit(oid, _) => Some(oid),
            Self::Compare { target, .. } => target.as_deref(),
            Self::Worktree(_) => None,
        }
    }

    pub(crate) fn includes_worktree(&self) -> bool {
        self.target_commit().is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct HistoryPosition {
    // None identifies the worktree row at the top of the commit list.
    pub oid: Option<String>,
    pub inset: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct DiffPosition {
    pub path: String,
    pub line: Option<(bool, usize)>,
    pub inset: f32,
    pub horizontal: f32,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct SessionFile {
    pub version: u32,
    pub session: WorktreeSession,
}
