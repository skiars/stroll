use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};

use crate::{git::Mode, tabs::TabSession};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceView {
    #[default]
    Editor,
    Docs,
    Git,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EditorPreviewMode {
    #[default]
    Source,
    Split,
    Full,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct WorktreeSession {
    pub view: Option<WorkspaceView>,
    pub tabs: TabSession,
    pub files: BTreeMap<PathBuf, FilePosition>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub file_wraps: BTreeMap<PathBuf, bool>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub editor_previews: BTreeMap<PathBuf, EditorPreviewMode>,
    pub source_history: SourceHistory,
    pub document: Option<PathBuf>,
    pub documents: BTreeMap<PathBuf, DocumentPosition>,
    // None indicates that the document list has not been opened.
    pub open_documents: Option<Vec<OpenDocument>>,
    pub git: Option<GitSession>,
    pub panels: PanelPreferences,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub editor_tree_expanded: BTreeSet<PathBuf>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub docs_tree_expanded: BTreeSet<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalSession>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_draft: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub worktree_lsp_trusted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct TerminalSession {
    pub open: bool,
    pub docs_open: bool,
    pub active: Option<u32>,
    pub tabs: Vec<TerminalTab>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct TerminalTab {
    pub number: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub scrollback: Arc<str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct PanelPreferences {
    pub editor_tree: bool,
    pub docs_tree: bool,
    pub git_history: bool,
    pub git_tree: bool,
    pub editor_tree_width: f32,
    pub docs_tree_width: f32,
    pub doc_outline_width: f32,
    pub doc_outline_visible: Option<bool>,
    pub git_history_width: f32,
    pub git_tree_width: f32,
}

impl Default for PanelPreferences {
    fn default() -> Self {
        Self {
            editor_tree: true,
            docs_tree: true,
            git_history: true,
            git_tree: true,
            editor_tree_width: 240.,
            docs_tree_width: 240.,
            doc_outline_width: 195.,
            doc_outline_visible: None,
            git_history_width: 300.,
            git_tree_width: 280.,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct OpenDocument {
    pub path: PathBuf,
    pub title: Option<String>,
}

impl WorktreeSession {
    pub fn select_document(&mut self, path: &Path) {
        self.remember_document(path);
        self.document = Some(path.to_owned());
    }

    pub(crate) fn remember_document(&mut self, path: &Path) -> &mut OpenDocument {
        let documents = self.open_documents.get_or_insert_with(|| {
            self.document
                .iter()
                .cloned()
                .map(|path| OpenDocument { path, title: None })
                .collect()
        });
        let index = documents.iter().position(|document| document.path == path);
        let index = index.unwrap_or_else(|| {
            documents.push(OpenDocument {
                path: path.to_owned(),
                title: None,
            });
            documents.len() - 1
        });
        &mut documents[index]
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SourceHistory {
    pub entries: VecDeque<SourceLocation>,
    pub index: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceLocation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub position: Option<FilePosition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<DocumentLocation>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct DocumentLocation {
    pub position: Option<DocumentPosition>,
    pub fragment: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<DocumentLinePosition>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct DocumentLinePosition {
    pub source_offset: usize,
    pub viewport_inset: f32,
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
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub worktree_collapsed_files: Arc<BTreeSet<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GitSelection {
    Commit(String, Option<String>),
    #[cfg(feature = "desktop")]
    #[serde(skip)]
    CommitRange {
        base: Option<String>,
        newest: String,
        focused: String,
        count: usize,
    },
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
            Self::CommitRange { newest, .. } => Some(newest),
            Self::Compare { target, .. } => target.as_deref(),
            Self::Worktree(_) => None,
        }
    }

    pub(crate) fn focused_commit(&self) -> Option<&str> {
        match self {
            Self::CommitRange { focused, .. } => Some(focused),
            _ => self.target_commit(),
        }
    }

    pub(crate) fn without_commit_range(self) -> Self {
        match self {
            Self::CommitRange { focused, .. } => Self::Commit(focused, None),
            Self::Compare {
                base,
                target,
                previous,
            } => Self::Compare {
                base,
                target,
                previous: Box::new(previous.without_commit_range()),
            },
            selection => selection,
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

#[cfg(feature = "desktop")]
#[derive(Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct StandaloneSession {
    pub view: Option<WorkspaceView>,
    pub panels: PanelPreferences,
    pub file_position: Option<FilePosition>,
    pub document_position: Option<DocumentPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_wrap: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor_preview: Option<EditorPreviewMode>,
}
