use super::*;
fn previous_snapshot_changed(workspace: &Workspace, repo: &Repository, tree: &Repository) -> bool {
    let current = workspace.repository_snapshot.as_deref().unwrap_or(repo);
    current.children != tree.children || current.branch != tree.branch
}

use crate::{
    git::watch::{DEBOUNCE, RepoChanges, RepoWatcher},
    remote::Request,
    repository::files::{DocumentBatch, DocumentScan},
};
use gpui_component::input::RopeExt as _;
use similar::{DiffTag, TextDiff};
use std::ops::Range;

static NEXT_DOCUMENT_SCAN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) struct ReloadEdits {
    before: Rope,
    edits: Vec<(Range<usize>, String)>,
    changes: Vec<(Range<usize>, usize)>,
}

impl ReloadEdits {
    pub(crate) fn new(before: Rope, after: &str) -> Self {
        let original = before.to_string();
        let diff = TextDiff::configure()
            .timeout(Duration::from_millis(25))
            .diff_lines(original.as_str(), after);
        let mut edits = Vec::new();
        let mut offset = 0;
        for op in diff.ops() {
            let old = &diff.old_slices()[op.old_range()];
            if op.tag() != DiffTag::Equal {
                let new = &diff.new_slices()[op.new_range()];
                let (range, text) = editor_git::restore_text_edit(&old.concat(), &new.concat());
                edits.push((offset + range.start..offset + range.end, text));
            }
            offset += old.iter().map(|line| line.len()).sum::<usize>();
        }
        let changes = language_actions::language_edit_changes(&before, &edits);
        Self {
            before,
            edits,
            changes,
        }
    }

    pub(crate) fn map_offset(&self, after: &Rope, offset: usize, after_insert: bool) -> usize {
        language_actions::map_language_edit_offset(
            &self.before,
            after,
            &self.changes,
            offset,
            after_insert,
        )
    }

    pub(crate) fn map_anchor(&self, after: &Rope, offset: usize) -> (usize, bool) {
        let deleted = self
            .changes
            .iter()
            .any(|(range, length)| *length == 0 && range.contains(&offset));
        (self.map_offset(after, offset, true), deleted)
    }

    fn apply(&self, editor: &mut EditorState, window: &mut Window, cx: &mut Context<EditorState>) {
        let viewport = editor.viewport_anchor();
        let scroll = editor.scroll_offset();
        editor.edit_ranges_mapping_selection_ranges(
            &self.edits,
            None,
            |after, selection| {
                let start = self.map_offset(after, selection.start, true);
                let end = if selection.is_empty() {
                    start
                } else {
                    self.map_offset(after, selection.end, false).max(start)
                };
                start..end
            },
            window,
            cx,
        );
        if let Some((position, inset)) = viewport {
            let offset = self.map_offset(
                editor.text(),
                self.before.position_to_offset(&position),
                true,
            );
            editor.restore_viewport_anchor(
                editor.text().offset_to_position(offset),
                inset,
                scroll.x,
                cx,
            );
        } else {
            editor.set_scroll_offset(scroll, cx);
        }
    }
}

impl OpenFile {
    pub(super) fn apply_synced_document(
        &mut self,
        disk: Document,
        reload: Option<ReloadEdits>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.saved = Rope::from(disk.saved_text.as_str());
        self.committed = if reload.is_some() {
            self.saved.clone()
        } else {
            self.editor.read(cx).text().clone()
        };
        self.document = disk;
        self.unconfirmed_save = None;
        self.external = None;
        self.missing = false;
        self.dirty = self.committed != self.saved;
        self.pending_draft = true;
        self.revision += 1;
        if let Some(reload) = reload {
            self.editor
                .update(cx, |editor, cx| reload.apply(editor, window, cx));
        }
        if let Some(server) = &self.language_server {
            server.document.changed(self.committed.clone());
            server.document.saved(self.saved.clone());
        }
    }
}

impl Workspace {
    pub(super) fn reset_workspace_tree(&mut self, cx: &mut Context<Self>) {
        self.clear_editor_commit_markers(cx);
        self.tree_status_task = Task::ready(());
        self.doc_sync = Task::ready(());
        self.doc_refresh_after_open = None;
        self.repository_snapshot = None;
        self.tree_load_pending.clear();
        self.tree_reveal = None;
        self.tree_selected = None;
        self.tree_keyboard_path.clear();
        self.tree_clipboard = None;
        self.docs_tree = Rc::default();
        self.docs_tree_reveal = None;
        self.docs_tree_seeded = false;
        self.docs_tree_complete = false;
        self.docs_scan_pending = false;
        self.repository_watch_pending = false;
        self.docs_tree_task = Task::ready(());
        self.docs_tree_loading = false;
        self.docs_tree_dirty = true;
        self.workspace_refresh = Task::ready(());
        self.workspace_refresh_pending = Default::default();
        self.workspace_refreshing = false;
        self.tree_status = Rc::default();
        self.tree_edit = None;
        self.tree_busy = false;
    }

    pub(super) fn load_expanded_tree_directories(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.tree_snapshot() else {
            return;
        };
        let directories: Vec<_> = self
            .expanded
            .iter()
            .filter(|path| {
                self.tree_directory_is_expanded(path) && !repo.children.contains_key(*path)
            })
            .cloned()
            .collect();
        self.tree_load_pending.extend(directories);
        self.queue_workspace_refresh(Default::default(), window, cx);
    }

    fn tree_directory_is_expanded(&self, path: &Path) -> bool {
        path.ancestors()
            .take_while(|ancestor| !ancestor.as_os_str().is_empty())
            .all(|ancestor| self.expanded.contains(ancestor))
    }

    fn tree_snapshot(&self) -> Option<&Repository> {
        self.repository_snapshot
            .as_deref()
            .or(self.repository.as_deref())
    }

    pub(super) fn reveal_file_in_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.tabs.active().filter(|path| !path.is_absolute());
        if self.tree_selected.as_deref() != active {
            self.tree_selected = active.map(Path::to_owned);
            self.tree_reveal = self.tree_selected.clone();
        }
        self.resume_tree_reveal(window, cx);
    }

    pub(super) fn resume_tree_reveal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == WorkspaceView::Docs {
            self.resume_document_tree_reveal(cx);
        } else {
            self.finish_tree_reveal(&BTreeSet::new(), window, cx);
        }
    }

    fn finish_tree_reveal(
        &mut self,
        attempted: &BTreeSet<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view != WorkspaceView::Editor
            || !self.session.panels.editor_tree
            || self.tree_edit.is_some()
        {
            return;
        }
        let Some(path) = self
            .tree_reveal
            .clone()
            .filter(|path| self.tabs.active() == Some(path))
        else {
            return;
        };
        let Some(repo) = self.tree_snapshot() else {
            return;
        };
        let mut chain: Vec<_> = path.ancestors().collect();
        chain.reverse();
        let mut missing = BTreeSet::new();
        for pair in chain.windows(2) {
            let directory = pair[1] != path;
            let listed = repo.children.get(pair[0]).is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| entry.path == pair[1] && entry.directory == directory)
            });
            if !listed {
                if attempted.contains(pair[0]) || !repo.lazy_tree {
                    self.tree_reveal = None;
                    return;
                }
                missing.insert(pair[0].to_owned());
            }
        }
        if !missing.is_empty() {
            self.tree_load_pending.extend(missing);
            self.queue_workspace_refresh(Default::default(), window, cx);
            return;
        }
        self.tree_reveal = None;
        self.reveal_tree_path(&path, cx);
    }

    pub(super) fn resume_document_tree_reveal(&mut self, cx: &mut Context<Self>) {
        if self.view != WorkspaceView::Docs || !self.session.panels.docs_tree {
            return;
        }
        let Some(path) = self.docs_tree_reveal.as_ref() else {
            return;
        };
        if self.session.document.as_ref() != Some(path) {
            self.docs_tree_reveal = None;
            return;
        }
        let listed = path
            .parent()
            .and_then(|parent| self.docs_tree.children.get(parent))
            .is_some_and(|entries| entries.iter().any(|entry| &entry.path == path));
        if listed {
            let path = self.docs_tree_reveal.take().unwrap();
            self.reveal_tree_path(&path, cx);
        } else if !self.docs_tree_loading && !self.docs_tree_dirty {
            self.docs_tree_reveal = None;
        }
    }

    pub(super) fn load_docs_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view != WorkspaceView::Docs || !self.docs_tree_dirty || self.docs_tree_loading {
            return;
        }
        let Some(repo) = self.repository.clone() else {
            return;
        };
        self.docs_tree_loading = true;
        if self.docs_tree_seeded {
            self.docs_scan_pending = true;
            cx.notify();
            return;
        }
        let source = repo.clone();
        let task = cx.background_spawn(async move {
            if source.supports_document_batches() {
                source.fast_document_paths().map(DocumentTree::from_paths)
            } else {
                Ok(DocumentTree::default())
            }
        });
        self.docs_tree_task = cx.spawn_in(window, async move |this, cx| {
            let tree = task.await.unwrap_or_default();
            _ = this.update_in(cx, |this, window, cx| {
                if this
                    .repository
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &repo))
                {
                    this.publish_docs_tree(tree, false, window, cx);
                    this.docs_scan_pending = true;
                }
            });
        });
    }

    pub(super) fn scan_docs_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        self.docs_tree_dirty = false;
        let source = repo.clone();
        let incremental = !self.docs_tree_complete;
        let mut shown: BTreeSet<_> = self
            .docs_tree
            .children
            .values()
            .flatten()
            .filter(|entry| !entry.directory)
            .map(|entry| entry.path.clone())
            .collect();
        let work = cx.background_spawn(async move { source.supports_document_batches() });
        self.docs_tree_task = cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                if !work.await {
                    let source = repo.clone();
                    let tree = cx
                        .background_spawn(async move {
                            match source.document_paths()? {
                                Some(paths) => Ok(DocumentTree::from_paths(paths)),
                                None => source
                                    .complete_tree()
                                    .map(|tree| DocumentTree::from_repo(&tree)),
                            }
                        })
                        .await?;
                    _ = this.update_in(cx, |this, window, cx| {
                        if this
                            .repository
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, &repo))
                        {
                            this.publish_docs_tree(tree, true, window, cx);
                        }
                    });
                    return Ok(());
                }

                let generation =
                    NEXT_DOCUMENT_SCAN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut cursor = DocumentScan::default();
                let mut paths = Vec::new();
                loop {
                    let source = repo.clone();
                    let (next, batch) = cx
                        .background_spawn(async move {
                            let batch: Result<DocumentBatch> = if let Some(ssh) = &source.ssh {
                                ssh.call(Request::DocumentBatch(generation))
                            } else {
                                cursor.next(&source.root)
                            };
                            (cursor, batch)
                        })
                        .await;
                    cursor = next;
                    let batch = batch?;
                    paths.extend(batch.paths.iter().cloned());
                    let done = batch.done;
                    let mut additions = 0;
                    if incremental && !done {
                        for path in batch.paths {
                            additions += usize::from(shown.insert(path));
                        }
                    }
                    if done || additions > 0 {
                        let listing = if done {
                            std::mem::take(&mut paths)
                        } else {
                            shown.iter().cloned().collect()
                        };
                        let tree = cx
                            .background_spawn(async move { DocumentTree::from_paths(listing) })
                            .await;
                        let published = this
                            .update_in(cx, |this, window, cx| {
                                if this
                                    .repository
                                    .as_ref()
                                    .is_none_or(|current| !Arc::ptr_eq(current, &repo))
                                {
                                    return false;
                                }
                                this.publish_docs_tree(tree, done, window, cx);
                                true
                            })
                            .unwrap_or(false);
                        if !published {
                            break;
                        }
                    }
                    if done {
                        break;
                    }
                }
                Ok(())
            }
            .await;
            _ = this.update_in(cx, |this, window, cx| {
                if this
                    .repository
                    .as_ref()
                    .is_none_or(|current| !Arc::ptr_eq(current, &repo))
                {
                    return;
                }
                this.docs_tree_loading = false;
                if let Err(error) = result
                    && this.view == WorkspaceView::Docs
                {
                    this.show_error(error, window, cx);
                }
                this.load_docs_tree(window, cx);
                this.resume_document_tree_reveal(cx);
                cx.notify();
            });
        });
    }

    fn publish_docs_tree(
        &mut self,
        tree: DocumentTree,
        complete: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.docs_tree = Rc::new(tree);
        self.docs_tree_seeded = true;
        self.docs_tree_complete = complete;
        if self.view == WorkspaceView::Docs {
            self.rebuild_refreshed_tree(&self.rows.clone(), cx);
            self.resume_document_tree_reveal(cx);
            if self.doc.is_none()
                && self.doc_loading.is_none()
                && self.doc_error.is_none()
                && let Some(path) = self.initial_document()
            {
                self.open_doc(path, None, window, cx);
            }
        }
        cx.notify();
    }

    fn rebuild_refreshed_tree(&mut self, previous: &Rc<Vec<TreeRow>>, cx: &App) {
        self.rebuild_tree();
        self.tree_motion.get_mut().refresh(
            previous,
            &self.rows,
            |row| row.entry.path.clone(),
            self.tree_edit.is_none().then_some(&self.tree_scroll),
            cx,
        );
    }

    pub(super) fn rescan_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(repo) = self.repository.clone() {
            self.apply_repository_changes(
                &repo,
                RepoChanges {
                    rescan: true,
                    refs: true,
                    ..Default::default()
                },
                window,
                cx,
            );
        } else if self.standalone.is_some() {
            self.sync_document_paths(self.files.keys().cloned().collect(), window, cx);
            self.refresh_selected_document(window, cx);
        }
    }

    pub(super) fn refresh_tree_paths(
        &mut self,
        directories: impl IntoIterator<Item = PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(repo) = self.repository.clone() {
            self.apply_repository_changes(
                &repo,
                RepoChanges {
                    directories: directories.into_iter().collect(),
                    structure: true,
                    ..Default::default()
                },
                window,
                cx,
            );
        }
    }

    fn queue_workspace_refresh(
        &mut self,
        changes: RepoChanges,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_refresh_pending.merge(changes);
        if self.workspace_refreshing
            || (self.workspace_refresh_pending.is_empty() && self.tree_load_pending.is_empty())
        {
            return;
        }
        let Some(repo) = self.repository.clone() else {
            return;
        };
        self.workspace_refreshing = true;
        let changes = std::mem::take(&mut self.workspace_refresh_pending);
        let snapshot = self
            .repository_snapshot
            .clone()
            .unwrap_or_else(|| repo.clone());
        let source = repo.clone();
        let renames: Vec<_> = changes
            .renames
            .iter()
            .enumerate()
            .map(|(index, (from, to))| {
                (
                    from.clone(),
                    final_rename_destination(to, &changes.renames[index + 1..]).clone(),
                )
            })
            .filter(|(from, to)| from != to)
            .collect();
        let status_needed = !changes.is_empty();
        let refresh_all = changes.rescan
            || changes.index
            || changes.refs
            || (changes.structure && changes.directories.is_empty());
        let mut directories = std::mem::take(&mut self.tree_load_pending);
        let mut invalidate = BTreeSet::new();
        let visible = |path: &Path| self.tree_directory_is_expanded(path);
        let changed: BTreeSet<_> = if refresh_all {
            snapshot
                .children
                .keys()
                .cloned()
                .chain(self.expanded.iter().filter(|path| visible(path)).cloned())
                .collect()
        } else {
            changes.directories
        };
        for path in changed {
            if visible(&path) {
                directories.insert(path);
            } else if snapshot.children.contains_key(&path) {
                invalidate.insert(path);
            }
        }
        if status_needed && (refresh_all || !directories.is_empty()) {
            directories.extend(
                self.expanded
                    .iter()
                    .filter(|path| visible(path) && !snapshot.children.contains_key(*path))
                    .cloned(),
            );
        }
        // Read an expanded subtree at its destination before publishing the rename.
        for (from, to) in &renames {
            let (from, to) = (Path::new(from), Path::new(to));
            let expanded = self.expanded.contains(from);
            if expanded || self.tabs.paths().iter().any(|path| path.starts_with(from)) {
                directories.extend(
                    [from.parent(), to.parent()]
                        .into_iter()
                        .flatten()
                        .map(Path::to_owned),
                );
            }
            if expanded && to.parent().is_some_and(visible) {
                directories.extend(self.expanded.iter().filter_map(|path| {
                    let suffix = path.strip_prefix(from).ok()?;
                    path.ancestors()
                        .take_while(|ancestor| ancestor.starts_with(from))
                        .all(|ancestor| self.expanded.contains(ancestor))
                        .then(|| to.join(suffix))
                }));
            }
        }
        let status_task = status_needed.then(|| {
            cx.background_spawn(async move {
                source
                    .git_backend()
                    .worktree_status()
                    .ok()
                    .map(super::file_tree::TreeStatus::new)
            })
        });
        let attempted = directories.clone();
        let work = cx.background_spawn(async move {
            if directories.is_empty() && invalidate.is_empty() && !changes.refs {
                return Ok(None);
            }
            snapshot
                .refresh_directories(directories, invalidate, changes.refs)
                .map(Some)
        });
        let tree_repo = repo.clone();
        self.workspace_refresh = cx.spawn_in(window, async move |this, cx| {
            let tree = work.await;
            let repo = tree_repo;
            _ = this.update_in(cx, |this, window, cx| {
                if this
                    .repository
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &repo))
                {
                    this.workspace_refreshing = false;
                    this.tree_load_pending
                        .retain(|path| !attempted.contains(path));
                    let (tree, error) = match tree {
                        Ok(tree) => (tree, None),
                        Err(error) => (None, Some(error)),
                    };
                    if let Some(tree) = tree {
                        let previous = this
                            .repository_snapshot
                            .clone()
                            .unwrap_or_else(|| repo.clone());
                        let contains = |repo: &Repository, path: &Path| {
                            path.parent()
                                .and_then(|parent| repo.children.get(parent))
                                .is_some_and(|entries| {
                                    entries.iter().any(|entry| entry.path == path)
                                })
                        };
                        let (moves, deferred): (Vec<_>, Vec<_>) = renames
                            .into_iter()
                            .filter(|(from, to)| {
                                let (from, to) = (Path::new(from), Path::new(to));
                                (contains(&previous, from)
                                    || this.tabs.paths().iter().any(|path| path.starts_with(from))
                                    || this.expanded.contains(from))
                                    && !contains(&tree, from)
                                    && contains(&tree, to)
                                    && !this.tabs.paths().iter().any(|path| path.starts_with(to))
                            })
                            .partition(|(from, _)| {
                                !this.tree_busy
                                    && !this
                                        .saving
                                        .iter()
                                        .chain(this.git_edit.iter())
                                        .any(|path| path.starts_with(from))
                            });
                        if !deferred.is_empty() {
                            let repo = repo.clone();
                            cx.spawn_in(window, async move |this, cx| {
                                cx.background_executor().timer(DEBOUNCE).await;
                                _ = this.update_in(cx, |this, window, cx| {
                                    if this
                                        .repository
                                        .as_ref()
                                        .is_some_and(|current| Arc::ptr_eq(current, &repo))
                                    {
                                        let directories = deferred
                                            .iter()
                                            .flat_map(|(from, to)| [from, to])
                                            .filter_map(|path| {
                                                Path::new(path).parent().map(Path::to_owned)
                                            })
                                            .collect();
                                        this.queue_workspace_refresh(
                                            RepoChanges {
                                                directories,
                                                renames: deferred,
                                                structure: true,
                                                ..Default::default()
                                            },
                                            window,
                                            cx,
                                        );
                                    }
                                });
                            })
                            .detach();
                        }
                        let previous_rows = this.rows.clone();
                        for (from, to) in &moves {
                            let (from, to) = (Path::new(from), Path::new(to));
                            let affected = this.files_for_external_rename(from);
                            this.relocate_tree_path(from, to, affected, window, cx);
                            let paths = this
                                .files
                                .keys()
                                .filter(|path| path.starts_with(to))
                                .cloned()
                                .collect();
                            this.sync_document_paths(paths, window, cx);
                        }
                        // A collapsed ancestor can evict a directory listing without
                        // deleting the directory. Keep its expansion until a parent
                        // listing confirms the directory is gone.
                        this.expanded.retain(|path| {
                            path.ancestors().all(|ancestor| {
                                ancestor
                                    .parent()
                                    .and_then(|parent| tree.children.get(parent))
                                    .is_none_or(|entries| {
                                        entries
                                            .iter()
                                            .any(|entry| entry.directory && entry.path == ancestor)
                                    })
                            })
                        });
                        if previous_snapshot_changed(this, &repo, &tree) {
                            if this.view == WorkspaceView::Editor
                                && this.expanded.iter().any(|path| {
                                    !previous.children.contains_key(path)
                                        && tree.children.contains_key(path)
                                        && !moves.iter().any(|(_, to)| path.starts_with(to))
                                })
                            {
                                this.tree_motion.get_mut().start();
                            }
                            this.repository_snapshot = Some(Arc::new(tree));
                            if this.view == WorkspaceView::Editor {
                                this.rebuild_refreshed_tree(&previous_rows, cx);
                                if this
                                    .tree_edit
                                    .as_ref()
                                    .is_some_and(|edit| edit.row(&this.rows).is_none())
                                    && !this.tree_busy
                                {
                                    this.finish_tree_edit(window, cx);
                                }
                            }
                            cx.notify();
                        }
                    }
                    this.workspace_refreshing = false;
                    if let Some(error) = error {
                        this.tree_reveal = None;
                        if repo.ssh.as_ref().is_some_and(|ssh| ssh.failure().is_some()) {
                            this.reconnect_ssh(window, cx);
                        } else {
                            this.show_error(error, window, cx);
                        }
                    } else {
                        this.finish_tree_reveal(&attempted, window, cx);
                    }
                    this.queue_workspace_refresh(Default::default(), window, cx);
                }
            });
        });
        if let Some(task) = status_task {
            self.tree_status_task = cx.spawn_in(window, async move |this, cx| {
                if let Some(status) = task.await {
                    _ = this.update_in(cx, |this, _, cx| {
                        if this
                            .repository
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, &repo))
                            && *this.tree_status != status
                        {
                            this.tree_status = Rc::new(status);
                            cx.notify();
                        }
                    });
                }
            });
        }
    }

    pub(super) fn watch_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.repository_watch = Task::ready(());
        self.repository_watch_active = false;
        let repo = self.repository.clone();
        let standalone = self.standalone.clone();
        if repo.is_none() && standalone.is_none() {
            return;
        }
        let opening = repo.clone();
        let task = cx.background_spawn(async move {
            let watch = if let Some(repo) = opening {
                if let Some(ssh) = &repo.ssh {
                    return ssh.watch_repository().map(|events| (None, events));
                }
                RepoWatcher::open(&repo.root, &repo.common_dir)?
            } else {
                RepoWatcher::open_file(&standalone.unwrap())?
            };
            let events = watch.events.clone();
            Ok((Some(watch), events))
        });
        self.repository_watch = cx.spawn_in(window, async move |this, cx| {
            let ssh = repo.as_ref().and_then(|repo| repo.ssh.as_ref());
            if let Ok((_watch, events)) = task.await {
                _ = this.update_in(cx, |this, window, cx| {
                    this.repository_watch_active = true;
                    this.rescan_workspace(window, cx);
                    if this.view == WorkspaceView::Editor {
                        this.watch_editor_git(window, cx);
                    }
                });
                while events.wake.recv().await.is_ok() {
                    if ssh.is_none() {
                        cx.background_executor().timer(DEBOUNCE).await;
                    }
                    let mut changes = events.take();
                    if !changes.unpaired_renames.is_empty() && ssh.is_none() {
                        let paths = this.update_in(cx, |this, _, _| {
                            this.files
                                .values()
                                .filter_map(|file| {
                                    let absolute = if let Some(repo) = &repo {
                                        repo.root.join(&file.path)
                                    } else {
                                        file.path.clone()
                                    };
                                    Some((absolute, file.document.identity?))
                                })
                                .collect::<Vec<_>>()
                        });
                        let Ok(paths) = paths else {
                            return;
                        };
                        let candidates = std::mem::take(&mut changes.unpaired_renames);
                        let moves = cx
                            .background_spawn(async move {
                                crate::git::watch::match_unpaired_renames(candidates, paths)
                            })
                            .await;
                        if let Some(repo) = &repo {
                            changes
                                .renames
                                .extend(moves.into_iter().filter_map(|(from, to)| {
                                    let from = from.strip_prefix(&repo.root).ok()?.to_str()?;
                                    let to = to.strip_prefix(&repo.root).ok()?.to_str()?;
                                    Some((from.to_owned(), to.to_owned()))
                                }));
                        } else {
                            changes.local_renames.extend(moves);
                        }
                    }
                    if let Some(repo) = repo.as_ref().filter(|repo| repo.ssh.is_none())
                        && !changes.paths.is_empty()
                    {
                        let root = repo.root.clone();
                        changes = cx
                            .background_spawn(async move {
                                changes.classify_ignored_paths(&root);
                                changes
                            })
                            .await;
                    }
                    if ssh.is_some_and(|ssh| !ssh.supports_workspace_files())
                        && !changes.paths.is_empty()
                    {
                        changes.structure = true;
                    }
                    if events.wake.is_closed() {
                        changes.rescan = true;
                    }
                    if !changes.is_empty()
                        && this
                            .update_in(cx, |this, window, cx| {
                                if let Some(repo) = &repo {
                                    this.apply_repository_changes(repo, changes, window, cx);
                                } else if changes.rescan
                                    || !changes.local_renames.is_empty()
                                    || changes.local_paths.iter().any(|changed| {
                                        this.standalone
                                            .iter()
                                            .chain(this.files.keys())
                                            .any(|file| file.starts_with(changed))
                                    })
                                {
                                    this.apply_standalone_changes(
                                        changes.local_renames,
                                        window,
                                        cx,
                                    );
                                }
                            })
                            .is_err()
                    {
                        return;
                    }
                }
                _ = this.update_in(cx, |this, window, cx| {
                    this.repository_watch_active = false;
                    this.rescan_workspace(window, cx);
                    if this.view == WorkspaceView::Editor {
                        this.watch_editor_git(window, cx);
                    }
                });
            }
            loop {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                if ssh.is_some_and(|ssh| ssh.failure().is_some()) {
                    break;
                }
                if this
                    .update_in(cx, |this, window, cx| {
                        if window.is_window_active() {
                            this.rescan_workspace(window, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    pub(super) fn apply_standalone_changes(
        &mut self,
        renames: Vec<(PathBuf, PathBuf)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(anchor) = self.standalone.clone() else {
            return;
        };
        let mut paths = self.files.keys().cloned().collect::<BTreeSet<_>>();
        paths.insert(anchor.clone());
        let candidates = paths
            .into_iter()
            .filter_map(|path| {
                let target = final_rename_destination(&path, &renames);
                (target != &path).then(|| (path.clone(), target.to_owned()))
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            self.rescan_workspace(window, cx);
            return;
        }
        let check = cx.background_spawn(async move {
            candidates
                .into_iter()
                .filter(|(from, to)| {
                    fs::symlink_metadata(from)
                        .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
                        && crate::repository::canonical_local_file(to).is_ok_and(|path| path == *to)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let moves = check.await;
            let retry = this.update_in(cx, |this, window, cx| {
                if this.standalone.as_ref() != Some(&anchor) {
                    return Vec::new();
                }
                let mut retry = Vec::new();
                for (from, to) in moves {
                    if this.tabs.index(&to).is_some() || this.files.contains_key(&to) {
                        continue;
                    }
                    if this.saving.contains(&from) {
                        retry.push((from, to));
                    } else {
                        let affected = this.files_for_external_rename(&from);
                        this.relocate_tree_path(&from, &to, affected, window, cx);
                    }
                }
                this.rescan_workspace(window, cx);
                retry
            });
            if let Ok(retry) = retry
                && !retry.is_empty()
            {
                cx.background_executor().timer(DEBOUNCE).await;
                _ = this.update_in(cx, |this, window, cx| {
                    this.apply_standalone_changes(retry, window, cx);
                });
            }
        })
        .detach();
    }

    pub(super) fn apply_repository_changes(
        &mut self,
        repo: &Arc<Repository>,
        mut changes: RepoChanges,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .repository
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, repo))
        {
            return;
        }
        if changes.rescan
            || changes
                .paths
                .contains(crate::language::worktree::CONFIG_PATH)
        {
            self.refresh_worktree_language(window, cx);
        }
        let git_rescan = changes.rescan;
        changes.rescan |= changes.ignored_paths.iter().any(|path| {
            Path::new(path)
                .file_name()
                .is_some_and(|name| name == ".gitignore")
        });
        if changes.index || changes.structure || changes.refs || changes.rescan {
            self.invalidate_locator_paths();
        }
        let changed = |path: &Path| {
            !path.is_absolute()
                && (changes.rescan
                    || changes.refs
                    || changes
                        .paths
                        .iter()
                        .any(|changed| path.starts_with(changed)))
        };
        if !changes.paths.is_empty() || changes.refs || changes.rescan {
            let paths = self
                .files
                .keys()
                .filter(|path| changed(path))
                .cloned()
                .collect();
            self.sync_document_paths(paths, window, cx);
        }
        if changes.rescan || changes.refs || changes.structure {
            self.docs_tree_dirty = true;
            self.load_docs_tree(window, cx);
        }
        if changes.refs || git_rescan {
            self.load_commit_markers(false, window, cx);
        }
        self.queue_workspace_refresh(changes.clone(), window, cx);
        if self.session.document.as_deref().is_some_and(changed) {
            self.refresh_selected_document(window, cx);
        }
        let images: Vec<_> = self
            .images
            .keys()
            .filter(|path| changed(path))
            .cloned()
            .collect();
        for path in images {
            self.load_image_tab(path, window, cx);
        }
        let active = self.tabs.active();
        let active_changed = active.is_some_and(|path| {
            changes.index
                || changes.refs
                || changes.rescan
                || changes
                    .paths
                    .iter()
                    .any(|changed| path.starts_with(changed))
        });
        if self.view == WorkspaceView::Editor && active_changed {
            self.watch_editor_git(window, cx);
        }
        if let Some(panel) = self.git.as_ref() {
            changes.rescan =
                git_rescan || (!changes.directories.is_empty() && changes.paths.is_empty());
            changes
                .paths
                .retain(|path| !changes.ignored_paths.contains(path));
            changes.ignored_paths.clear();
            changes.directories.clear();
            changes.renames.clear();
            changes.structure = false;
            if !changes.is_empty() {
                panel.update(cx, |panel, cx| {
                    panel.repository_changed(changes, window, cx)
                });
            }
        }
    }
    pub(super) fn refresh_selected_document(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.view != WorkspaceView::Docs {
            return;
        }
        let Some(path) = self.session.document.clone() else {
            return;
        };
        if let Some(open) = &self.doc_loading {
            if open.path == path {
                self.doc_refresh_after_open = Some(path);
            }
            return;
        }
        if self
            .doc
            .as_ref()
            .is_some_and(|reader| reader.read(cx).path != path)
        {
            return;
        }
        let previous = self
            .doc
            .as_ref()
            .map(|reader| reader.read(cx).source_text());
        let origin = self.repository.clone();
        let repo = origin.clone();
        let local_text = self
            .local_document_text(&path)
            .filter(|_| self.standalone.is_none());
        let root = repo.as_ref().map_or_else(
            || path.parent().unwrap_or(Path::new("")).to_owned(),
            |repo| repo.root.clone(),
        );
        let current = path.clone();
        enum DocumentReload {
            Missing,
            Unchanged,
            Updated(Box<docs::PreparedDocument>, Rope, Option<ReloadEdits>),
        }
        let load = cx.background_spawn(async move {
            let source = if let Some(local_text) = local_text {
                Ok(Some(local_text.to_string()))
            } else {
                docs::read_for_reload(repo.as_deref(), &current)
            };
            source.and_then(|source| {
                let Some(source) = source else {
                    return Ok(DocumentReload::Missing);
                };
                if previous.as_ref().is_some_and(|old| old.as_ref() == source) {
                    return Ok(DocumentReload::Unchanged);
                }
                let after = Rope::from(source.as_str());
                let edits = previous.map(|old| ReloadEdits::new(Rope::from(old.as_ref()), &source));
                let remote = repo
                    .as_ref()
                    .filter(|_| !current.is_absolute())
                    .and_then(|repo| repo.remote_resources());
                let document = docs::PreparedDocument::from_source(source, remote)?;
                Ok(DocumentReload::Updated(Box::new(document), after, edits))
            })
        });
        // Replacing the waiter prevents older requests from publishing after a new write.
        self.doc_sync = cx.spawn_in(window, async move |this, cx| {
            let can_publish = |this: &Self| {
                this.view == WorkspaceView::Docs
                    && match (&this.repository, &origin) {
                        (Some(current), Some(origin)) => Arc::ptr_eq(current, origin),
                        (None, None) => true,
                        _ => false,
                    }
                    && this.session.document.as_ref() == Some(&path)
                    && this.doc_loading.is_none()
            };
            let result = load.await;
            let Ok(Some((reader, pending, after, edits))) =
                this.update_in(cx, |this, window, cx| {
                    if !can_publish(this) {
                        return None;
                    }
                    let (document, after, edits) = match result {
                        Ok(DocumentReload::Updated(document, after, edits)) => {
                            (*document, after, edits)
                        }
                        Ok(DocumentReload::Unchanged) => {
                            if this.doc_error.take().is_some() {
                                cx.notify();
                            }
                            return None;
                        }
                        Ok(DocumentReload::Missing) => {
                            this.doc_error = Some(t!("workspace.tree_file_missing").into_owned());
                            cx.notify();
                            return None;
                        }
                        Err(error) => {
                            this.doc_error = Some(format!("{error:#}"));
                            cx.notify();
                            return None;
                        }
                    };
                    let workspace = cx.weak_entity();
                    let reader = cx.new(|cx| {
                        DocReader::new(root, path.clone(), document, None, workspace, window, cx)
                    });
                    let pending =
                        reader.update(cx, |reader, cx| reader.wait_for_document_parse(cx));
                    Some((reader, pending, after, edits))
                })
            else {
                return;
            };
            if let Some((_subscription, parsed)) = pending
                && parsed.await.is_err()
            {
                return;
            }
            _ = this.update_in(cx, |this, window, cx| {
                if !can_publish(this) {
                    return;
                }
                let typography = this.config.current().docs;
                reader.update(cx, |reader, cx| {
                    reader.set_typography(typography, cx);
                    reader.set_outline_layout(
                        this.outline_width,
                        this.outline_visibility,
                        this.standalone.is_some(),
                    );
                    if let (Some(previous), Some(edits)) = (&this.doc, &edits) {
                        reader.inherit_document(
                            previous,
                            |offset| edits.map_anchor(&after, offset),
                            window,
                            cx,
                        );
                    } else {
                        reader.restore_position(this.session.documents.get(&path).copied());
                    }
                });
                this.update_document_title(&path, reader.read(cx).document_title());
                cx.observe(&reader, |_, _, cx| cx.notify()).detach();
                this.doc = Some(reader);
                this.doc_error = None;
                cx.notify();
            });
        });
    }

    pub(super) fn sync_document_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = paths
            .into_iter()
            .filter_map(|path| {
                let file = self.files.get_mut(&path)?;
                let before = file.editor.read(cx).text().clone();
                let clean = before == file.saved;
                file.sync_revision += 1;
                Some((
                    path,
                    file.editor.entity_id(),
                    file.document.draft_baseline(),
                    file.document.edit_baseline(),
                    file.sync_revision,
                    before,
                    clean,
                ))
            })
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return;
        }
        let origin = self.repository.clone();
        let repo = origin.clone();
        let read = cx.background_spawn(async move {
            paths
                .into_iter()
                .map(
                    |(path, editor, baseline, edit_baseline, revision, before, clean)| {
                        let disk = Document::read_for_sync(repo.as_deref(), &path);
                        let recovered = (!clean
                            && baseline != edit_baseline
                            && Document::matches_recovered_baseline(
                                edit_baseline,
                                &before.to_string(),
                            ))
                        .then(|| before.clone());
                        let reload = disk
                            .as_ref()
                            .ok()
                            .and_then(Option::as_ref)
                            .filter(|disk| {
                                (clean || recovered.is_some()) && before != disk.saved_text.as_str()
                            })
                            .map(|disk| ReloadEdits::new(before, &disk.saved_text));
                        (path, editor, baseline, revision, disk, reload, recovered)
                    },
                )
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let documents = read.await;
            let same_repository = |workspace: &Self| match (&workspace.repository, &origin) {
                (Some(current), Some(origin)) => Arc::ptr_eq(current, origin),
                (None, None) => true,
                _ => false,
            };
            let retry = this.update_in(cx, |this, window, cx| {
                if !same_repository(this) {
                    return Vec::new();
                }
                let mut changed = false;
                let mut retry = Vec::new();
                for (path, editor_id, baseline, revision, disk, reload, recovered) in documents {
                    let Some(file) = this.files.get_mut(&path).filter(|file| {
                        file.editor.entity_id() == editor_id && file.sync_revision == revision
                    }) else {
                        continue;
                    };
                    let editor = file.editor.read(cx);
                    if this.git_edit.contains(&path)
                        || this.saving.contains(&path)
                        || file.document.draft_baseline() != baseline
                        || editor.is_composing()
                    {
                        retry.push((path, editor_id, revision));
                        continue;
                    }
                    let disk = match disk {
                        Ok(Some(disk)) => disk,
                        Ok(None) => {
                            if !file.missing {
                                changed = true;
                                file.missing = true;
                                file.external = None;
                                file.pending_draft = true;
                                file.revision += 1;
                                if let Some(panel) = &this.git {
                                    panel.update(cx, |panel, cx| {
                                        panel.repository_source_changed(path, cx)
                                    });
                                }
                            }
                            continue;
                        }
                        Err(_) => continue,
                    };
                    let was_missing = file.missing;
                    file.document.identity = disk.identity;
                    if disk.draft_baseline() == baseline
                        && !was_missing
                        && !file.document.conflict
                        && file.unconfirmed_save.is_none()
                    {
                        continue;
                    }
                    let clean =
                        editor.text() == &file.saved || recovered.as_ref() == Some(editor.text());
                    let matches_disk = editor.text() == disk.saved_text.as_str();
                    let disk_matches_baseline =
                        disk.draft_baseline() == file.document.edit_baseline();
                    let confirmed = file
                        .unconfirmed_save
                        .as_ref()
                        .is_some_and(|text| text == disk.saved_text.as_str());
                    if !clean && !matches_disk && !disk_matches_baseline && !confirmed {
                        let new_conflict = !file.document.conflict;
                        file.missing = false;
                        file.unconfirmed_save = None;
                        file.document.conflict = true;
                        let disk_baseline = disk.draft_baseline();
                        if file
                            .external
                            .as_ref()
                            .is_none_or(|(baseline, _)| *baseline != disk_baseline)
                        {
                            file.external =
                                Some((disk_baseline, Rope::from(disk.saved_text.as_str())));
                        }
                        if !new_conflict && !was_missing {
                            continue;
                        }
                    } else {
                        let reload = if clean && !matches_disk && !confirmed {
                            let Some(reload) =
                                reload.filter(|reload| &reload.before == editor.text())
                            else {
                                retry.push((path, editor_id, revision));
                                continue;
                            };
                            Some(reload)
                        } else {
                            None
                        };
                        file.apply_synced_document(disk, reload, window, cx);
                    }
                    changed = true;
                    if let Some(panel) = &this.git {
                        panel.update(cx, |panel, cx| panel.repository_source_changed(path, cx));
                    }
                }
                if changed {
                    this.queue_drafts(window, cx);
                    if this.view == WorkspaceView::Editor {
                        if this.active_file().is_some_and(|file| {
                            file.document.conflict || this.file_editor_locked(file)
                        }) {
                            this.language_format = Task::ready(());
                        }
                        this.watch_editor_git(window, cx);
                    }
                    cx.notify();
                }
                retry
            });
            let Ok(retry) = retry else { return };
            if !retry.is_empty() {
                cx.background_executor().timer(DEBOUNCE).await;
                _ = this.update_in(cx, |this, window, cx| {
                    if same_repository(this) {
                        let paths = retry
                            .into_iter()
                            .filter_map(|(path, editor_id, revision)| {
                                let file = this.files.get(&path)?;
                                (file.editor.entity_id() == editor_id
                                    && file.sync_revision == revision)
                                    .then_some(path)
                            })
                            .collect();
                        this.sync_document_paths(paths, window, cx);
                    }
                });
            }
        })
        .detach();
    }
}

fn final_rename_destination<'a, P: PartialEq>(mut path: &'a P, renames: &'a [(P, P)]) -> &'a P {
    for (from, to) in renames {
        if path == from {
            path = to;
        }
    }
    path
}
