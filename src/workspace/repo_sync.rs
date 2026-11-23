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

static NEXT_DOCUMENT_SCAN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Workspace {
    pub(super) fn reset_workspace_tree(&mut self, cx: &mut Context<Self>) {
        self.clear_editor_commit_markers(cx);
        self.tree_status_task = Task::ready(());
        self.doc_sync = Task::ready(());
        self.repository_snapshot = None;
        self.tree_load_pending.clear();
        self.tree_reveal = None;
        self.tree_selected = None;
        self.tree_keyboard_path.clear();
        self.tree_clipboard = None;
        self.docs_tree = Rc::default();
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
        self.finish_tree_reveal(&BTreeSet::new(), window, cx);
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
        if chain[1..chain.len() - 1]
            .iter()
            .any(|parent| !self.expanded.contains(*parent))
        {
            self.expanded.extend(
                chain[1..chain.len() - 1]
                    .iter()
                    .map(|path| path.to_path_buf()),
            );
            self.rebuild_tree();
        }
        if let Some(index) = self.rows.iter().position(|row| row.entry.path == path) {
            self.scroll_tree_row_into_view(index);
            cx.notify();
        }
    }

    pub(super) fn scroll_tree_row_into_view(&self, index: usize) {
        let inset = if self.config.current().appearance.file_tree_sticky_scroll {
            let viewport = self.tree_scroll.0.borrow().base_handle.bounds();
            tree_sticky_scroll::row_limit(viewport.size.height).min(self.rows[index].depth)
        } else {
            0
        };
        self.tree_scroll
            .scroll_to_item_with_offset(index, ScrollStrategy::Nearest, inset);
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
            let scroll = self.tree_scroll.0.borrow().base_handle.clone();
            let offset = scroll.offset();
            let top = ((-offset.y / tree_motion::ROW_HEIGHT) as usize).min(self.rows.len());
            let anchor = (offset.y < px(0.))
                .then(|| self.rows.get(top))
                .flatten()
                .map(|row| row.entry.path.clone());
            self.rebuild_tree();
            if let Some(path) = anchor
                && let Some(index) = self.rows.iter().position(|row| row.entry.path == path)
            {
                scroll.set_offset(point(
                    offset.x,
                    offset.y + tree_motion::ROW_HEIGHT * (top as f32 - index as f32),
                ));
            }
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
        } else if let Some(path) = self.standalone.clone() {
            self.sync_document_paths(vec![path], window, cx);
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
        let renames = changes.renames.clone();
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
        // Check both parents of a move involving an open tab, even if the tree is collapsed.
        for (from, to) in &renames {
            if self.tabs.paths().iter().any(|path| path.starts_with(from)) {
                for path in [from, to] {
                    if let Some(parent) = Path::new(path).parent() {
                        directories.insert(parent.to_owned());
                    }
                }
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
                        let moves: Vec<_> = renames
                            .into_iter()
                            .filter(|(from, to)| {
                                let (from, to) = (Path::new(from), Path::new(to));
                                !this.tree_busy
                                    && !this
                                        .saving
                                        .iter()
                                        .chain(this.git_edit.iter())
                                        .any(|path| path.starts_with(from))
                                    && (contains(&previous, from)
                                        || this
                                            .tabs
                                            .paths()
                                            .iter()
                                            .any(|path| path.starts_with(from))
                                        || this.expanded.contains(from))
                                    && !contains(&tree, from)
                                    && contains(&tree, to)
                                    && !this.tabs.paths().iter().any(|path| path.starts_with(to))
                            })
                            .collect();
                        for (from, to) in moves {
                            let (from, to) = (Path::new(&from), Path::new(&to));
                            let affected = this.files_for_external_rename(from);
                            this.relocate_tree_path(&repo, from, to, affected, window, cx);
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
                                })
                            {
                                this.tree_motion.get_mut().start();
                            }
                            this.repository_snapshot = Some(Arc::new(tree));
                            this.rebuild_tree();
                            if this
                                .tree_edit
                                .as_ref()
                                .is_some_and(|edit| edit.row(&this.rows).is_none())
                                && !this.tree_busy
                            {
                                this.finish_tree_edit(window, cx);
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
                                } else {
                                    this.rescan_workspace(window, cx);
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

    fn apply_repository_changes(
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
        if self.view == WorkspaceView::Docs
            && let Some(path) = self.session.document.clone().filter(|path| changed(path))
        {
            self.sync_document_reader(repo, path, window, cx);
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
    fn sync_document_reader(
        &mut self,
        repo: &Arc<Repository>,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reader = self.doc.clone();
        let current = reader
            .as_ref()
            .map(|reader| reader.read(cx).source_text().to_string());
        let repo = repo.clone();
        let origin = repo.clone();
        let source = path.clone();
        let read = cx.background_spawn(async move {
            match repo.bytes_if_present(&source) {
                Ok(Some(bytes)) => crate::repository::decode_markdown(bytes).map(Some),
                Ok(None) => Ok(None),
                Err(error) => Err(error),
            }
        });
        self.doc_sync = cx.spawn_in(window, async move |this, cx| {
            let Ok(source) = read.await else { return };
            if current == source && reader.is_some() {
                return;
            }
            _ = this.update_in(cx, |this, window, cx| {
                if this
                    .repository
                    .as_ref()
                    .is_none_or(|repo| !Arc::ptr_eq(repo, &origin))
                    || this.view != WorkspaceView::Docs
                    || this.session.document.as_deref() != Some(&path)
                    || this.doc != reader
                {
                    return;
                }
                this.capture_document_position(cx);
                this.doc = None;
                if source.is_some() {
                    let (fragment, position) = this
                        .doc_loading
                        .take()
                        .map(|pending| (pending.fragment, pending.position))
                        .unwrap_or_default();
                    this.open_doc_at(path, fragment, position, None, window, cx);
                } else if this.doc_loading.is_none() {
                    this.doc_error = Some(t!("workspace.tree_file_missing").into_owned());
                    cx.notify();
                }
            });
        });
    }

    fn sync_document_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = paths
            .into_iter()
            .filter_map(|path| {
                let file = self.files.get_mut(&path)?;
                file.sync_revision += 1;
                Some((
                    path,
                    file.editor.entity_id(),
                    file.document.draft_baseline(),
                    file.sync_revision,
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
                .map(|(path, editor, baseline, revision)| {
                    let disk = Document::read_for_sync(repo.as_deref(), &path);
                    (path, editor, baseline, revision, disk)
                })
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
                for (path, editor_id, baseline, revision, disk) in documents {
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
                    changed |= was_missing;
                    file.missing = false;
                    // A recovered conflict already loaded the newer disk baseline.
                    // A matching file event must not unlock the original draft.
                    if disk.draft_baseline() == baseline
                        && !(file.document.restored && file.document.conflict)
                    {
                        if was_missing || file.document.conflict {
                            file.document.conflict = false;
                            changed = true;
                            if let Some(panel) = &this.git {
                                panel.update(cx, |panel, cx| {
                                    panel.repository_source_changed(path, cx)
                                });
                            }
                        }
                        continue;
                    }
                    if editor.text() != &file.saved && editor.text() != disk.saved_text.as_str() {
                        if !file.document.conflict {
                            if file.committed != *editor.text() {
                                file.committed = editor.text().clone();
                                file.pending_draft = true;
                                file.revision += 1;
                            }
                            if file.flush_draft(origin.as_deref(), &this.store).is_ok() {
                                file.document.conflict = true;
                                file.document.restored = false;
                                changed = true;
                                if let Some(panel) = &this.git {
                                    panel.update(cx, |panel, cx| {
                                        panel.repository_source_changed(path, cx)
                                    });
                                }
                            }
                        }
                        continue;
                    }
                    changed = true;
                    let text = disk.saved_text.clone();
                    let update_editor = editor.text() != text.as_str();
                    let persist = file.is_dirty();
                    file.saved = Rope::from(text.as_str());
                    file.committed = file.saved.clone();
                    if update_editor {
                        file.editor.update(cx, |editor, cx| {
                            let selection = editor.selected_range();
                            let reversed = editor.is_selection_reversed();
                            let scroll = editor.scroll_offset();
                            editor.set_value(text, window, cx);
                            let start = selection.start.min(editor.text().len());
                            let end = selection.end.min(editor.text().len());
                            editor.set_selected_range_with_direction(start..end, reversed, cx);
                            editor.set_scroll_offset(scroll, cx);
                        });
                    }
                    file.document = disk;
                    file.dirty = false;
                    file.pending_draft = persist;
                    file.revision += 1;
                    if let Some(server) = &file.language_server {
                        server.document.changed(file.saved.clone());
                        server.document.saved(file.saved.clone());
                    }
                    if let Some(panel) = &this.git {
                        panel.update(cx, |panel, cx| panel.repository_source_changed(path, cx));
                    }
                }
                if changed {
                    this.queue_drafts(window, cx);
                    if this.view == WorkspaceView::Editor {
                        if this
                            .active_file()
                            .is_some_and(|file| this.file_editor_locked(file))
                        {
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
