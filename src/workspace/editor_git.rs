pub(super) mod blame;
pub(super) mod commits;

use super::*;
use crate::git::{self, FileAction, FileChanges, FileDiffOptions, FileEdit, Layer, Mode};
use anyhow::Context as _;
use gpui_component::input::{
    GutterDecoration, GutterDecorationCollection, GutterHunkSection, HUNK_TOOLBAR_HEIGHT,
};
use std::ops::Range;

const HUNK_TOOLBAR_RADIUS: Pixels = px(8.);
const GIT_GUTTER_WIDTH: Pixels = px(16.);

pub(super) struct EditorGit {
    marks: GutterDecorationCollection,
    changes: Option<FileChanges>,
    selected: Option<usize>,
    pending_hunk: Option<HunkLocation>,
    draft_base: Option<Arc<str>>,
    draft_marks: Task<()>,
    stale: bool,
    blame: blame::Blame,
    commits: commits::FileCommitMarkers,
}

pub(super) struct GitEditOutcome {
    operation: Option<git::Operation>,
    before: Option<Document>,
    after: Option<Result<Option<Document>>>,
}

pub(super) fn restore_text_edit(before: &str, after: &str) -> (std::ops::Range<usize>, String) {
    let prefix = before
        .chars()
        .zip(after.chars())
        .take_while(|(left, right)| left == right)
        .map(|(ch, _)| ch.len_utf8())
        .sum::<usize>();
    let suffix = before[prefix..]
        .chars()
        .rev()
        .zip(after[prefix..].chars().rev())
        .take_while(|(left, right)| left == right)
        .map(|(ch, _)| ch.len_utf8())
        .sum::<usize>();
    (
        prefix..before.len() - suffix,
        after[prefix..after.len() - suffix].to_owned(),
    )
}

impl EditorGit {
    pub fn new(editor: &Entity<EditorState>, cx: &mut App) -> Self {
        let marks = editor.update(cx, |editor, cx| {
            editor.set_interactive_gutter_width(GIT_GUTTER_WIDTH, cx);
            editor.create_gutter_decorations_collection(Vec::new(), cx)
        });
        Self {
            marks,
            changes: None,
            selected: None,
            pending_hunk: None,
            draft_base: None,
            draft_marks: Task::ready(()),
            stale: false,
            blame: blame::Blame::default(),
            commits: commits::FileCommitMarkers::new(editor, cx),
        }
    }

    pub(super) fn clear_hunk(&mut self, editor: &Entity<EditorState>, cx: &mut App) {
        self.pending_hunk = None;
        let commit = self.commits.selected.take();
        if self.selected.take().is_none() && commit.is_none() {
            return;
        }
        editor.update(cx, |editor, cx| editor.clear_gutter_hunk(cx));
    }
}

struct HunkLocation {
    layer: Layer,
    ranges: Vec<Range<usize>>,
}

impl HunkLocation {
    fn after_edit(changes: &FileChanges, selected: usize, action: FileAction) -> Option<Self> {
        let (patch, block) = editor_hunks(changes).nth(selected)?;
        let layer = match action {
            FileAction::Stage => Layer::Staged,
            FileAction::Unstage => Layer::Unstaged,
            FileAction::Restore => return None,
        };
        Some(Self {
            layer,
            ranges: worktree_ranges(changes, patch.layer, block).collect(),
        })
    }

    fn find(&self, changes: &FileChanges) -> Option<usize> {
        editor_hunks(changes)
            .enumerate()
            .filter(|(_, (patch, _))| patch.layer == self.layer)
            .filter_map(|(id, (patch, block))| {
                let score = worktree_ranges(changes, patch.layer, block)
                    .flat_map(|range| {
                        self.ranges.iter().map(move |previous| {
                            if *previous == range {
                                return 2;
                            }
                            usize::from(
                                range.start.max(previous.start) < range.end.min(previous.end)
                                    || (range.is_empty() && previous.contains(&range.start))
                                    || (previous.is_empty() && range.contains(&previous.start)),
                            )
                        })
                    })
                    .sum::<usize>();
                (score > 0).then_some((score, id))
            })
            .max_by_key(|&(score, id)| (score, std::cmp::Reverse(id)))
            .map(|(_, id)| id)
    }
}

fn editor_hunks(changes: &FileChanges) -> impl Iterator<Item = (&git::LayerPatch, &git::Block)> {
    changes
        .layers
        .iter()
        .flat_map(|patch| patch.blocks.iter().map(move |block| (patch, block)))
}

fn worktree_ranges<'a>(
    changes: &'a FileChanges,
    layer: Layer,
    block: &'a git::Block,
) -> impl Iterator<Item = Range<usize>> + 'a {
    let unstaged = changes
        .layers
        .iter()
        .find(|patch| patch.layer == Layer::Unstaged)
        .map_or(&[][..], |patch| patch.blocks.as_slice());
    block
        .changes
        .iter()
        .map(move |change| project_change(change, layer, unstaged))
}

fn project_change(change: &git::BlockRange, layer: Layer, unstaged: &[git::Block]) -> Range<usize> {
    let lines = change.start..change.start + change.added;
    if layer == Layer::Staged {
        // An insertion at the trailing boundary belongs to the next range.
        let start = worktree_line(lines.start, unstaged, true);
        let end = worktree_line(lines.end, unstaged, lines.is_empty());
        start..end
    } else {
        lines
    }
}

fn hunk_line(changes: &FileChanges, layer: Layer, block: &git::Block) -> usize {
    worktree_ranges(changes, layer, block)
        .map(|range| range.start)
        .min()
        .unwrap_or(usize::MAX)
}

/// Hunk ids in working-tree order, independent of their staged or unstaged layer.
fn hunk_order(changes: &FileChanges) -> Vec<usize> {
    let mut hunks = editor_hunks(changes)
        .enumerate()
        .map(|(id, (patch, block))| (hunk_line(changes, patch.layer, block), id))
        .collect::<Vec<_>>();
    hunks.sort_by_key(|&(line, id)| (line, id));
    hunks.into_iter().map(|(_, id)| id).collect()
}

fn line_offsets(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(ix, _)| ix + 1))
        .chain(std::iter::once(text.len()))
        .collect()
}

/// Project an index line through the unstaged hunks to a working-tree boundary.
pub(super) fn worktree_line(
    line: usize,
    unstaged: &[git::Block],
    include_insertions: bool,
) -> usize {
    let mut delta = 0;
    for block in unstaged {
        let old_start = block
            .old_start
            .saturating_sub(usize::from(block.old_count > 0));
        let new_start = block
            .new_start
            .saturating_sub(usize::from(block.new_count > 0));
        if line < old_start || (line == old_start && block.old_count == 0 && !include_insertions) {
            break;
        }
        if line < old_start + block.old_count {
            for change in &block.changes {
                let old_change = change.start.saturating_add_signed(-delta);
                if line < old_change
                    || (line == old_change && change.removed == 0 && !include_insertions)
                {
                    break;
                }
                if line < old_change + change.removed {
                    return change.start + (line - old_change).min(change.added);
                }
                delta += change.added as isize - change.removed as isize;
            }
            return line.saturating_add_signed(delta);
        }
        delta = (new_start + block.new_count) as isize - (old_start + block.old_count) as isize;
    }
    line.saturating_add_signed(delta)
}

fn marker_color(change: &git::BlockRange, cx: &App) -> Hsla {
    match (change.removed, change.added) {
        (_, 0) => cx.theme().danger,
        (0, _) => cx.theme().success,
        _ => Palette::for_mode(cx.theme().mode).accent,
    }
}

fn decoration_ranges(changes: &FileChanges, text: &str, cx: &App) -> Vec<GutterDecoration> {
    let offsets = line_offsets(text);
    let offset = |line| offsets.get(line).copied().unwrap_or(text.len());
    let mut marks = Vec::new();
    for (id, (patch, block)) in editor_hunks(changes).enumerate() {
        for (change, lines) in
            block
                .changes
                .iter()
                .zip(worktree_ranges(changes, patch.layer, block))
        {
            let range: Range<usize> = offset(lines.start)..offset(lines.end);
            marks.push(
                GutterDecoration::new(range)
                    .with_action(id)
                    .with_color(marker_color(change, cx))
                    .outlined(patch.layer == Layer::Staged),
            );
        }
    }
    marks
}

fn draft_decoration_ranges(
    changes: &FileChanges,
    unstaged: &[git::Block],
    offsets: &[usize],
    cx: &App,
) -> Vec<GutterDecoration> {
    let staged = changes
        .layers
        .iter()
        .filter(|patch| patch.layer == Layer::Staged)
        .flat_map(|patch| &patch.blocks);
    let offset = |line| {
        offsets
            .get(line)
            .copied()
            .unwrap_or(offsets[offsets.len() - 1])
    };
    staged
        .map(|block| (Layer::Staged, block))
        .chain(unstaged.iter().map(|block| (Layer::Unstaged, block)))
        .flat_map(|(layer, block)| {
            block.changes.iter().map(move |change| {
                let lines = project_change(change, layer, unstaged);
                GutterDecoration::new(offset(lines.start)..offset(lines.end))
                    // Keep the hunk marker's narrow gutter style. While dirty,
                    // toggle_editor_git ignores clicks before resolving the id.
                    .with_action(usize::MAX)
                    .with_color(marker_color(change, cx))
                    .outlined(layer == Layer::Staged)
            })
        })
        .collect()
}

impl Workspace {
    pub(super) fn refresh_editor_git_marks(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(repo), Some(file)) = (&self.repository, self.files.get_mut(path)) else {
            return;
        };
        file.git.draft_marks = Task::ready(());
        let Some(changes) = file.git.changes.as_ref().filter(|_| !file.git.stale) else {
            return;
        };
        if file.committed == file.saved {
            file.git
                .marks
                .set(decoration_ranges(changes, &file.saved.to_string(), cx), cx);
            return;
        }
        let Some(git_path) = git_path(repo, path) else {
            return;
        };
        let backend = repo.git_backend();
        let version = changes.version.clone();
        let base = file.git.draft_base.clone();
        let text = file.committed.clone();
        let revision = file.revision;
        let editor = file.editor.downgrade();
        let path = path.to_owned();
        file.git.draft_marks = cx.spawn_in(window, async move |this, cx| {
            let executor = cx.background_executor().clone();
            executor.timer(Duration::from_millis(90)).await;
            let result = executor
                .spawn(async move {
                    let base = match base {
                        Some(base) => base,
                        None => match backend.index_text(&git_path, &version)? {
                            Some(base) => base.into(),
                            None if version.status.is_some() => Arc::from(""),
                            None => return Ok(None),
                        },
                    };
                    let text = text.to_string();
                    let blocks = git::buffer_diff_blocks(&base, &text);
                    Ok::<_, anyhow::Error>(Some((base, blocks, line_offsets(&text))))
                })
                .await;
            let _ =
                this.update_in(cx, |workspace, _, cx| {
                    let Some(file) = workspace.files.get_mut(&path).filter(|file| {
                        file.editor.downgrade() == editor && file.revision == revision
                    }) else {
                        return;
                    };
                    let Ok(Some((base, unstaged, offsets))) = result else {
                        return;
                    };
                    if let Some(changes) = &file.git.changes {
                        file.git.draft_base = Some(base);
                        file.git.marks.set(
                            draft_decoration_ranges(changes, &unstaged, &offsets, cx),
                            cx,
                        );
                    }
                });
        });
    }

    pub(super) fn track_editor_edits(
        &self,
        editor: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            editor,
            window,
            |this, editor, event: &gpui_component::input::InputEditsEvent, window, cx| {
                let Some(file) = this.files.values_mut().find(|file| &file.editor == editor) else {
                    return;
                };
                let editor = editor.read(cx);
                file.newline_task = Task::ready(());
                file.newline = file
                    .server_project
                    .as_ref()
                    .and_then(|project| project.newline_server())
                    .and_then(|_| crate::lsp::Newline::capture(event, editor));
                file.git.blame.track_edits(event, editor);
                if this.view == WorkspaceView::Editor && this.tabs.active() == Some(&file.path) {
                    this.refresh_editor_blame(window, cx);
                    cx.notify();
                }
            },
        )
    }

    pub(super) fn observe_source_editor(
        &self,
        editor: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.observe_in(editor, window, |this, editor, window, cx| {
            this.track_source_caret(&editor, window, cx);
            this.refresh_caret_hints(&editor, false, window, cx);
            if this.view == WorkspaceView::Editor
                && this.active_file().is_some_and(|file| {
                    file.editor.entity_id() == editor.entity_id() && file.git.changes.is_some()
                })
            {
                this.refresh_editor_blame(window, cx);
                cx.notify();
            }
        })
    }

    pub(super) fn watch_editor_git(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.git_watch = Task::ready(());
        self.refresh_active_commit_markers(window, cx);
        let (Some(repo), Some(file)) = (self.repository.as_ref(), self.active_file()) else {
            return;
        };
        let path = file.path.clone();
        let Some(git_path) = git_path(repo, &path) else {
            return;
        };
        let backend = repo.git_backend();
        if !backend.supports_git_edit() {
            return;
        }
        let repo = repo.clone();
        let once = self.repository_watch_active;
        let cached = file
            .git
            .changes
            .as_ref()
            .map(|changes| changes.version.clone());
        if cached.is_some() {
            self.refresh_editor_git_marks(&path, window, cx);
        }
        self.refresh_editor_blame(window, cx);
        self.git_watch = cx.spawn_in(window, async move |this, cx| {
            let executor = cx.background_executor().clone();
            let mut last = cached;
            loop {
                let backend = backend.clone();
                let path_for_git = git_path.clone();
                let previous = last.clone();
                let result = executor
                    .spawn(async move {
                        let version = backend.file_version(&path_for_git)?;
                        if previous.as_ref() == Some(&version) {
                            return Ok(None);
                        }
                        backend
                            .file_changes(
                                &path_for_git,
                                FileDiffOptions {
                                    mode: Mode::All,
                                    context: 0,
                                    whitespace: false,
                                },
                            )
                            .map(Some)
                    })
                    .await;
                let new = result.ok().flatten();
                if let Some(changes) = new {
                    last = Some(changes.version.clone());
                    if this
                        .update_in(cx, |workspace, window, cx| {
                            if workspace.view != WorkspaceView::Editor
                                || !workspace
                                    .repository
                                    .as_ref()
                                    .is_some_and(|current| Arc::ptr_eq(current, &repo))
                                || workspace.tabs.active() != Some(&path)
                            {
                                return;
                            }
                            let Some(file) = workspace.files.get_mut(&path) else {
                                return;
                            };
                            let pending = file.git.pending_hunk.take();
                            file.git.stale =
                                changes.version.disk != Some(file.document.draft_baseline());
                            let next = pending
                                .filter(|_| !file.git.stale)
                                .and_then(|location| location.find(&changes));
                            if next.is_none() {
                                file.git.clear_hunk(&file.editor, cx);
                            }
                            if file.git.stale {
                                file.git.marks.clear(cx);
                            }
                            file.git.draft_base = None;
                            file.git.changes = Some(changes);
                            workspace.refresh_editor_git_marks(&path, window, cx);
                            workspace.refresh_editor_commit_markers(&path, window, cx);
                            if let Some(action) = next {
                                workspace.expand_editor_git(&path, action, cx);
                            }
                            workspace.refresh_editor_blame(window, cx);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                if once {
                    break;
                }
                executor.timer(Duration::from_millis(1500)).await;
            }
        });
    }

    pub(super) fn refresh_git_after_change(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.invalidate_editor_git(path, window, cx);
        if let Some(panel) = &self.git {
            panel.update(cx, |panel, cx| {
                panel.invalidate_source_inlay_hints(path, cx);
                panel.schedule_snapshot_alignment(cx);
                panel.refresh_changed_paths(
                    path.to_str().into_iter().map(str::to_owned),
                    window,
                    cx,
                )
            });
        }
    }

    pub(super) fn finish_stage_files(
        &mut self,
        paths: &[String],
        success: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.git_edit.clear();
        if !success {
            cx.notify();
            return;
        }
        for path in paths {
            self.clear_editor_git_changes(Path::new(path), cx);
        }
        if self.view == WorkspaceView::Editor {
            self.watch_editor_git(window, cx);
        }
        cx.notify();
    }

    fn invalidate_editor_git(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_editor_git_changes(path, cx);
        if self.view == WorkspaceView::Editor {
            self.watch_editor_git(window, cx);
        }
    }

    fn clear_editor_git_changes(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(file) = self.files.get_mut(path) {
            file.git.clear_hunk(&file.editor, cx);
            file.git.changes = None;
            file.git.draft_base = None;
            file.git.draft_marks = Task::ready(());
            file.git.blame = blame::Blame::default();
            file.git.stale = false;
            file.git.marks.clear(cx);
        }
    }

    pub(super) fn check_git_file(&self, path: &Path, action: FileAction) -> Result<()> {
        anyhow::ensure!(!self.saving.contains(path), t!("git.save_before_git"));
        if action != FileAction::Unstage {
            anyhow::ensure!(
                match self.files.get(path) {
                    Some(file) =>
                        file.committed == file.saved
                            && !file.document.conflict
                            && file.unconfirmed_save.is_none(),
                    None => !self
                        .repository
                        .as_ref()
                        .is_some_and(|repo| self.store.has_draft(repo, path)),
                },
                t!("git.save_before_git")
            );
        }
        Ok(())
    }

    pub(super) fn begin_git_edit(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        action: FileAction,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(self.git_edit.is_empty(), t!("git.git_busy"));
        let paths: HashSet<_> = paths.into_iter().collect();
        for path in &paths {
            self.check_git_file(path, action)?;
        }
        if self.tabs.active().is_some_and(|path| paths.contains(path)) {
            self.language_format = Task::ready(());
        }
        self.git_edit = paths;
        cx.notify();
        Ok(())
    }

    pub(super) fn finish_git_edit(
        &mut self,
        path: &Path,
        restored: Option<Result<Option<Document>>>,
        refresh_panel: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.git_edit.remove(path) {
            return;
        }
        match restored {
            Some(Ok(Some(document))) => {
                if let Some(file) = self.files.get_mut(path) {
                    let text = document.saved_text.clone();
                    file.saved = Rope::from(text.as_str());
                    file.committed = file.saved.clone();
                    file.document = document;
                    file.dirty = false;
                    file.revision += 1;
                    file.pending_draft = true;
                    if let Some(repo) = &self.repository
                        && let Err(error) = file.flush_draft(Some(repo), &self.store)
                    {
                        toast::error(window, cx, format!("{error:#}"));
                    }
                    file.editor.update(cx, |editor, cx| {
                        let selection = editor.selected_range();
                        let reversed = editor.is_selection_reversed();
                        let scroll = editor.scroll_offset();
                        editor.replace_projected_text(0..editor.text().len(), text, window, cx);
                        let start = selection.start.min(editor.text().len());
                        let end = selection.end.min(editor.text().len());
                        editor.set_selected_range_with_direction(start..end, reversed, cx);
                        editor.set_scroll_offset(scroll, cx);
                    });
                    if let Some(server) = &file.language_server {
                        server.document.changed(file.saved.clone());
                        server.document.saved(file.saved.clone());
                    }
                }
            }
            Some(Ok(None)) if self.files.contains_key(path) => {
                self.remove_tab(path, window, cx);
                if self.view == WorkspaceView::Editor {
                    self.restore_active_file(window, cx);
                }
            }
            Some(Err(error)) if self.files.contains_key(path) => self.show_error(error, window, cx),
            _ => {}
        }
        self.invalidate_editor_git(path, window, cx);
        if refresh_panel && let Some(panel) = &self.git {
            panel.update(cx, |panel, cx| panel.refresh_after_edit(window, cx));
        }
        cx.notify();
    }

    pub(super) fn restored_document(repo: &Repository, path: &Path) -> Result<Option<Document>> {
        if repo.exists(path) {
            Document::read(repo, path).map(Some)
        } else if let Some(error) = repo.ssh.as_ref().and_then(|ssh| ssh.failure()) {
            Err(anyhow::anyhow!("{error}"))
        } else {
            Ok(None)
        }
    }

    pub(super) fn run_git_edit(
        repo: &Repository,
        path: &Path,
        edit: git::FileEdit,
    ) -> Result<GitEditOutcome> {
        let restore = edit.action == FileAction::Restore;
        let before = if restore {
            Self::restored_document(repo, path)
                .ok()
                .flatten()
                .filter(|document| {
                    Some(document.draft_baseline()) == edit.version.disk && !document.mixed_newlines
                })
        } else {
            None
        };
        let operation = repo.git_backend().edit_file(edit)?;
        let after = restore.then(|| Self::restored_document(repo, path));
        Ok(GitEditOutcome {
            operation,
            before,
            after,
        })
    }

    pub(super) fn finish_recorded_git_edit(
        &mut self,
        repo: &Repository,
        path: &Path,
        result: GitEditOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let GitEditOutcome {
            operation,
            before,
            after,
        } = result;
        if let (Some(operation), Some(before), Some(Ok(Some(after)))) = (&operation, before, &after)
            && !after.mixed_newlines
            && before.text != after.text
        {
            if !self.files.contains_key(path) {
                self.install_file(path, FileBuffer::new(before.clone()), None, window, cx);
            }
            let group = super::git_history::restore_group(operation.id);
            if let Some(file) = self.files.get_mut(path)
                && file.document.draft_baseline() == before.draft_baseline()
                && file.committed == file.saved
                && !file.document.conflict
            {
                let change = restore_text_edit(&before.text, &after.text);
                file.document = after.clone();
                file.saved = Rope::from(after.text.as_str());
                let editor = file.editor.clone();
                editor.update(cx, |editor, cx| {
                    let scroll = editor.scroll_offset();
                    editor.edit_ranges_in_group(&[change], group, window, cx);
                    editor.set_scroll_offset(scroll, cx);
                });
                self.editor_changed(editor, window, cx);
                self.finish_git_edit(path, None, false, window, cx);
                if let Err(error) = self.flush_edit_history(cx, Some((repo, operation.id, path))) {
                    self.show_error(error, window, cx);
                }
                if let Some(server) = self
                    .files
                    .get(path)
                    .and_then(|file| file.language_server.as_ref())
                {
                    server.document.changed(Rope::from(after.text.as_str()));
                    server.document.saved(Rope::from(after.text.as_str()));
                }
                return;
            }
        }
        self.record_git_history(operation, window, cx);
        self.finish_git_edit(path, after, false, window, cx);
    }

    pub(super) fn toggle_editor_git(&mut self, path: &Path, action: usize, cx: &mut Context<Self>) {
        if self.toggle_commit_hunk(path, action, cx) {
            return;
        }
        let Some(file) = self.files.get_mut(path) else {
            return;
        };
        if file.git.selected == Some(action) {
            file.git.clear_hunk(&file.editor, cx);
            return;
        }
        if file.committed != file.saved {
            return;
        }
        file.git.pending_hunk = None;
        self.expand_editor_git(path, action, cx);
    }

    pub(super) fn close_editor_git_hunk(
        &mut self,
        _: &gpui_component::input::Escape,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.tabs.active().map(Path::to_owned) else {
            cx.propagate();
            return;
        };
        let Some(file) = self.files.get_mut(&path) else {
            cx.propagate();
            return;
        };
        if file.git.selected.is_none()
            && file.git.pending_hunk.is_none()
            && file.git.commits.selected.is_none()
        {
            cx.propagate();
            return;
        }
        file.git.clear_hunk(&file.editor, cx);
        cx.notify();
    }

    fn navigate_editor_git(
        &mut self,
        path: &Path,
        target: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = self.source_location(cx);
        let Some(file) = self.files.get_mut(path) else {
            return;
        };
        let Some(changes) = file.git.changes.as_ref() else {
            return;
        };
        let Some((patch, block)) = editor_hunks(changes).nth(target) else {
            return;
        };
        let line = hunk_line(changes, patch.layer, block);
        file.git.pending_hunk = None;
        self.expand_editor_git(path, target, cx);
        if let Some(file) = self.files.get(path) {
            file.editor.update(cx, |editor, cx| {
                use gpui_component::input::RopeExt as _;
                let caret_line = line.min(editor.text().lines_len().saturating_sub(1));
                let offset = editor.text().line_start_offset(caret_line);
                reveal_editor_range(editor, offset..offset, Some(line), window, cx);
            });
            self.record_source_navigation(source, cx);
        }
        cx.notify();
    }

    pub(super) fn refresh_expanded_editor_hunks(&mut self, cx: &mut Context<Self>) {
        self.repaint_commit_hunks(cx);
        let expanded = self
            .files
            .iter()
            .filter_map(|(path, file)| Some((path.clone(), file.git.selected?)))
            .collect::<Vec<_>>();
        for (path, action) in expanded {
            self.expand_editor_git(&path, action, cx);
        }
    }

    fn expand_editor_git(&mut self, path: &Path, action: usize, cx: &mut Context<Self>) {
        let Some(file) = self.files.get_mut(path) else {
            return;
        };
        let Some(changes) = &file.git.changes else {
            return;
        };
        let Some((patch, block)) = editor_hunks(changes).nth(action) else {
            return;
        };
        let Some(mut deleted) = patch.deleted_lines(block).map(Vec::into_iter) else {
            return;
        };
        let hunk = code_theme::HunkPalette::for_mode(cx.theme().mode);
        let sections: Vec<_> = block
            .changes
            .iter()
            .zip(worktree_ranges(changes, patch.layer, block))
            .map(|(change, lines)| GutterHunkSection {
                line: lines.start,
                deleted: deleted.by_ref().take(change.removed).collect(),
                added: lines.end.saturating_sub(lines.start),
                deleted_background: hunk.removed_row,
                added_background: hunk.added_row,
            })
            .collect();
        let Some(first) = block.changes.first() else {
            return;
        };
        let marker = marker_color(first, cx);
        let outlined = patch.layer == Layer::Staged;
        file.git.commits.selected = None;
        file.git.selected = Some(action);
        let weak = cx.weak_entity();
        let path = path.to_owned();
        file.editor.update(cx, |editor, cx| {
            editor.set_gutter_hunk(
                action,
                sections,
                marker,
                outlined,
                move |_, cx| {
                    weak.read_with(cx, |workspace, cx| {
                        workspace.editor_git_toolbar(&path, weak.clone(), cx)
                    })
                    .unwrap_or_else(|_| div().into_any_element())
                },
                cx,
            );
        });
    }

    fn editor_git_toolbar(&self, path: &Path, owner: WeakEntity<Self>, cx: &App) -> AnyElement {
        let Some(file) = self.files.get(path) else {
            return div().into_any_element();
        };
        let Some(git_path) = self
            .repository
            .as_ref()
            .and_then(|repo| git_path(repo, path))
        else {
            return div().into_any_element();
        };
        let Some((changes, selected)) = file.git.changes.as_ref().zip(file.git.selected) else {
            return div().into_any_element();
        };
        let Some((patch, block)) = editor_hunks(changes).nth(selected) else {
            return div().into_any_element();
        };
        let code_font_size = px(self.config.current().editor.font_size as f32);
        let navigation = {
            let order = hunk_order(changes);
            let position = order.iter().position(|&id| id == selected).unwrap_or(0);
            let previous = position.checked_sub(1).map(|ix| order[ix]);
            let next = order.get(position + 1).copied();
            let (nav_path, nav_owner) = (path.to_owned(), owner.clone());
            let nav_button = |id, icon, label, target: Option<usize>| {
                let nav_path = nav_path.clone();
                let nav_owner = nav_owner.clone();
                compact_icon_button((id, selected), icon, t!(label))
                    .debug_selector(move || format!("{id}:{selected}"))
                    .min_w(px(32.))
                    .h_full()
                    .flex_shrink_0()
                    .icon_size(code_font_size)
                    .rounded(px(0.))
                    .disabled(target.is_none())
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        let Some(target) = target else {
                            return;
                        };
                        _ = nav_owner.update(cx, |workspace, cx| {
                            workspace.navigate_editor_git(&nav_path, target, window, cx);
                        });
                    })
            };
            h_flex()
                .id("editor-git-hunk-navigation")
                .h_full()
                .flex_shrink_0()
                .child(
                    nav_button(
                        "editor-git-previous-hunk",
                        IconName::ArrowUp,
                        "git.previous_hunk",
                        previous,
                    )
                    .rounded_bl(HUNK_TOOLBAR_RADIUS),
                )
                .child(nav_button(
                    "editor-git-next-hunk",
                    IconName::ArrowDown,
                    "git.next_hunk",
                    next,
                ))
                .into_any_element()
        };
        let buttons = block.target.as_ref().map(|target| {
            let available = |action| {
                (
                    self.check_git_file(path, action).is_ok(),
                    file.git.pending_hunk.is_some() || !self.git_edit.is_empty(),
                )
            };
            let path = path.to_owned();
            let click_owner = owner.clone();
            let edit_version = changes.version.clone();
            let target = target.clone();
            hunk_toolbar(
                patch.layer.actions(),
                false,
                false,
                Some(navigation),
                Some(code_font_size),
                available,
                move |action, window, cx| {
                    _ = click_owner.update(cx, |workspace, cx| {
                        let edit = FileEdit {
                            path: git_path.clone(),
                            version: edit_version.clone(),
                            action,
                            block: Some(target.clone()),
                        };
                        workspace.edit_from_editor(path.clone(), edit, window, cx);
                    });
                },
                |_, _| {},
                cx,
                true,
            )
        });
        hunk_toolbar_row(buttons.map(|buttons| hanging_hunk_toolbar(buttons, cx)))
    }

    pub(super) fn restore_buffer_hunk(
        &mut self,
        path: PathBuf,
        edit: FileEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_review_buffer(path.clone(), window, cx, move |this, result, window, cx| {
            let result =
                result.and_then(|()| this.begin_git_edit([path.clone()], FileAction::Restore, cx));
            if let Err(error) = result {
                this.show_error(error, window, cx);
                return;
            }
            let repo = this.repository.clone().unwrap();
            let backend = repo.git_backend();
            let expected_disk = edit.version.disk;
            let work = cx.background_spawn(async move {
                let target = edit.block.context(t!("git.cannot_edit"))?;
                let changes = backend.file_changes(
                    &edit.path,
                    FileDiffOptions {
                        mode: Mode::Unstaged,
                        context: target.context,
                        whitespace: false,
                    },
                )?;
                anyhow::ensure!(changes.version == edit.version, t!("git.worktree_changed"));
                git::restore_replacements(&changes, &target)
            });
            cx.spawn_in(window, async move |this, cx| {
                let result = work.await;
                _ = this.update_in(cx, |this, window, cx| {
                    if !this
                        .repository
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &repo))
                        || !this.git_edit.remove(&path)
                    {
                        return;
                    }
                    let result = result.and_then(|replacements| {
                        let file = this.files.get(&path).context(t!("git.worktree_changed"))?;
                        anyhow::ensure!(
                            file.committed == file.saved && !file.document.mixed_newlines,
                            t!("git.save_before_git")
                        );
                        // The file may have changed on disk before its editor was opened.
                        anyhow::ensure!(
                            expected_disk == Some(file.document.draft_baseline()),
                            t!("git.worktree_changed")
                        );
                        let offsets = line_offsets(&file.saved.to_string());
                        let edits = replacements
                            .into_iter()
                            .map(|(range, text)| {
                                let (Some(&start), Some(&end)) =
                                    (offsets.get(range.start), offsets.get(range.end))
                                else {
                                    anyhow::bail!(t!("git.worktree_changed"));
                                };
                                Ok((start..end, text.replace("\r\n", "\n")))
                            })
                            .collect::<Result<Vec<_>>>()?;
                        file.editor
                            .update(cx, |editor, cx| editor.edit_ranges(&edits, window, cx));
                        Ok(())
                    });
                    if let Err(error) = result {
                        this.show_error(error, window, cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        });
    }

    fn edit_from_editor(
        &mut self,
        path: PathBuf,
        edit: FileEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if edit.action == FileAction::Restore {
            self.restore_buffer_hunk(path, edit, window, cx);
            return;
        }
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let origin = repo.clone();
        if let Err(error) = self.begin_git_edit([path.clone()], edit.action, cx) {
            self.show_error(error, window, cx);
            return;
        }
        let selected = self.files.get(&path).and_then(|file| file.git.selected);
        let next = self.files.get(&path).and_then(|file| {
            HunkLocation::after_edit(file.git.changes.as_ref()?, selected?, edit.action)
        });
        self.git_watch = Task::ready(());
        let target = path.clone();
        let work = cx.background_spawn(async move { Self::run_git_edit(&repo, &target, edit) });
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            _ = this.update_in(cx, |workspace, window, cx| {
                if !workspace
                    .repository
                    .as_ref()
                    .is_some_and(|repo| Arc::ptr_eq(repo, &origin))
                {
                    return;
                }
                let restored = match result {
                    Ok(result) => result.after,
                    Err(error) => {
                        workspace.show_error(error, window, cx);
                        workspace.finish_git_edit(&path, None, true, window, cx);
                        return;
                    }
                };
                if let Some(next) = next
                    && let Some(file) = workspace.files.get_mut(&path)
                    && file.git.selected == selected
                {
                    file.git.pending_hunk = Some(next);
                    file.editor.update(cx, |_, cx| cx.notify());
                    workspace.git_edit.remove(&path);
                    if workspace.view == WorkspaceView::Editor {
                        workspace.watch_editor_git(window, cx);
                    }
                    if let Some(panel) = &workspace.git {
                        panel.update(cx, |panel, cx| panel.refresh_after_edit(window, cx));
                    }
                    cx.notify();
                    return;
                }
                workspace.finish_git_edit(&path, restored, true, window, cx);
            });
        })
        .detach();
    }
}

pub(super) fn hunk_toolbar_row(toolbar: Option<AnyElement>) -> AnyElement {
    h_flex()
        .id("editor-git-hunk")
        .w_full()
        .h(HUNK_TOOLBAR_HEIGHT)
        .items_start()
        .justify_end()
        .pr_3()
        .children(toolbar)
        .into_any_element()
}

pub(super) fn hanging_hunk_toolbar(buttons: AnyElement, cx: &App) -> AnyElement {
    h_flex()
        .id("editor-git-hunk-controls")
        .debug_selector(|| "editor-git-hunk-controls".into())
        .h_full()
        .min_w_0()
        .block_mouse_except_scroll()
        .font_family(cx.theme().font_family.clone())
        .rounded_b(HUNK_TOOLBAR_RADIUS)
        .overflow_hidden()
        .bg(Palette::for_mode(cx.theme().mode).elevated)
        .shadow_md()
        .child(buttons)
        .into_any_element()
}

pub(super) fn hunk_toolbar(
    actions: &[FileAction],
    confirm: bool,
    whole_file: bool,
    leading: Option<AnyElement>,
    text_size: Option<Pixels>,
    available: impl Fn(FileAction) -> (bool, bool),
    activate: impl Fn(FileAction, &mut Window, &mut App) + 'static,
    cancel: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
    hanging: bool,
) -> AnyElement {
    let colors = Palette::for_mode(cx.theme().mode);
    let activate = Rc::new(activate);
    let has_leading = leading.is_some();
    h_flex()
        .id("git-hunk-toolbar")
        .min_w_0()
        .when(hanging, |bar| bar.h_full())
        .when(!hanging, |bar| {
            bar.gap_1().rounded(px(4.)).bg(colors.elevated)
        })
        .children(leading)
        .when(confirm, |bar| {
            bar.child(
                div()
                    .min_w_0()
                    .truncate()
                    .px_1()
                    .when_some(text_size, |label, size| label.text_size(size))
                    .when(text_size.is_none(), |label| label.text_xs())
                    .child(if whole_file {
                        t!("git.restore_file_question")
                    } else {
                        t!("git.restore_hunk_question")
                    }),
            )
        })
        .children(
            actions
                .iter()
                .copied()
                .filter(|action| !confirm || *action == FileAction::Restore)
                .map(|action| {
                    let activate = activate.clone();
                    let (ready, busy) = available(action);
                    quiet_button(("git-hunk-action", action as usize))
                        .debug_selector(|| format!("git-hunk-action:{}", action as usize))
                        .small()
                        .h_full()
                        .when(hanging, |button| {
                            button
                                .rounded(px(0.))
                                .when(
                                    !confirm && !has_leading && action != FileAction::Restore,
                                    |button| button.rounded_bl(HUNK_TOOLBAR_RADIUS),
                                )
                                .when(!confirm && action != FileAction::Stage, |button| {
                                    button.rounded_br(HUNK_TOOLBAR_RADIUS)
                                })
                        })
                        .when_some(text_size, |button, size| {
                            button.label_size(size).icon_size(size)
                        })
                        .when(text_size.is_none(), |button| button.text_xs())
                        .min_w(px(32.))
                        .flex_shrink_0()
                        .label(action.label())
                        .disabled(!ready || busy)
                        .when(ready, |button| button.text_color(cx.theme().foreground))
                        .tooltip(if !ready {
                            t!("git.save_before_git")
                        } else if action == FileAction::Restore {
                            if whole_file {
                                t!("git.restore_file")
                            } else {
                                t!("git.restore_hunk_hint")
                            }
                        } else {
                            action.label()
                        })
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            activate(action, window, cx);
                        })
                }),
        )
        .when(confirm, |bar| {
            bar.child(
                quiet_button("git-cancel-restore")
                    .small()
                    .h_full()
                    .when(hanging, |button| {
                        button.rounded(px(0.)).rounded_br(HUNK_TOOLBAR_RADIUS)
                    })
                    .when_some(text_size, |button, size| button.label_size(size))
                    .when(text_size.is_none(), |button| button.text_xs())
                    .min_w(px(32.))
                    .flex_shrink_0()
                    .label(t!("common.cancel"))
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        cancel(window, cx);
                    }),
            )
        })
        .into_any_element()
}

fn git_path(repo: &Repository, path: &Path) -> Option<String> {
    if path.is_absolute() {
        return None;
    }
    if repo.ssh.is_some() || cfg!(windows) {
        crate::remote::RepoPath::from_local(path)
            .ok()
            .map(|path| path.as_str().to_owned())
    } else {
        path.to_str().map(str::to_owned)
    }
}

#[cfg(all(test, target_os = "linux"))]
#[::core::prelude::v1::test]
fn editing_below_an_inline_hunk_preserves_the_source_viewport() -> Result<()> {
    use gpui::{TestAppContext, TestDispatcher, size};
    use gpui_component::{Root, input::Position};
    use std::process::Command;

    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repo");
    fs::create_dir(&root)?;
    let path = PathBuf::from("source.txt");
    let git = |args: &[&str]| -> Result<()> {
        let output = Command::new("git").current_dir(&root).args(args).output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    git(&["init", "-q"])?;
    let source: String = (0..100)
        .map(|row| format!("source line {row:02}\n"))
        .collect();
    let mut baseline = source.clone();
    baseline.insert_str(
        source.find("source line 10").unwrap(),
        &"deleted line\n".repeat(8),
    );
    fs::write(root.join(&path), baseline)?;
    git(&["add", "source.txt"])?;
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "initial",
    ])?;
    fs::write(root.join(&path), &source)?;

    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(gpui_component::init);
    let mut workspace = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_resize(size(px(940.), px(580.)));
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                Location {
                    root,
                    file: Some(path.clone()),
                },
                window,
                cx,
            )
        });
    });
    visual.run_until_parked();
    let (editor, before) = visual.update(|window, cx| {
        let editor = workspace.update(cx, |workspace, cx| {
            workspace.toggle_editor_git(&path, 0, cx);
            let file = &workspace.files[&path];
            assert_eq!(file.git.selected, Some(0));
            file.editor.clone()
        });
        editor.update(cx, |state, cx| {
            state.set_cursor_position(Position::new(40, 2), window, cx);
            let height = state.line_height().unwrap();
            state.set_scroll_offset(point(px(0.), -38. * height), cx);
        });
        window.draw(cx).clear(cx);
        let before = editor.read(cx).projected_line_bounds(31).unwrap().top();
        (editor, before)
    });
    visual.simulate_keystrokes("x");
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert!(workspace.read(cx).files[&path].git.selected.is_none());
        let state = editor.read(cx);
        assert!(state.value().contains("soxurce line 40"));
        let after = state.projected_line_bounds(31).unwrap().top();
        assert!((after - before).abs() < px(0.01), "{before:?} -> {after:?}");
    });
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Command;

    #[cfg(debug_assertions)]
    #[::core::prelude::v1::test]
    fn gutter_expands_in_place_and_restore_keeps_the_staged_hunk() {
        check_gutter_toolbar("disk\nkeep this line\n", 1, true);
    }

    #[cfg(debug_assertions)]
    #[::core::prelude::v1::test]
    fn added_only_hunk_toolbar_does_not_move_the_text() {
        check_gutter_toolbar("index\nadded\nkeep this line\n", 0, false);
    }

    #[::core::prelude::v1::test]
    fn hunk_toolbar_navigates_to_a_neighbor_and_escape_closes() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        let path = PathBuf::from("review.txt");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "user.email", "test@example.invalid"]);
        git(&["config", "commit.gpgsign", "false"]);
        let head = (0..80)
            .map(|line| format!("line{line}"))
            .collect::<Vec<_>>();
        fs::write(root.join(&path), head.join("\n") + "\n").unwrap();
        git(&["add", "review.txt"]);
        git(&["commit", "-qm", "initial"]);
        let mut worktree = head;
        worktree[0] = "LINE0".to_owned();
        worktree[60] = "LINE60".to_owned();
        fs::write(root.join(&path), worktree.join("\n") + "\n").unwrap();

        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let (workspace, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_config(store, config, WorkspaceView::Editor, window, cx)
        });
        visual.simulate_resize(size(px(1150.), px(760.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.clone(),
                        file: Some(path.clone()),
                    },
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        visual.update(|_, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_editor_git(&path, 0, cx);
            });
        });
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            visual.update(|_, cx| apply_theme(mode, cx));
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert_eq!(workspace.read(cx).files[&path].git.selected, Some(0));
                let hunk = code_theme::HunkPalette::for_mode(mode);
                let quads = window.painted_quads();
                for fill in [hunk.added_row, hunk.removed_row] {
                    assert!(
                        quads
                            .iter()
                            .any(|quad| quad.background == Background::from(fill)),
                        "{mode:?}: the expanded hunk must adopt the current theme"
                    );
                }
            });
        }
        let hover = visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            let viewport = workspace.read(cx).files[&path]
                .editor
                .read(cx)
                .input_bounds();
            let scale = window.scale_factor();
            let color =
                Background::from(code_theme::HunkPalette::for_mode(cx.theme().mode).removed_row);
            let row = window
                .painted_quads()
                .into_iter()
                .filter(|quad| {
                    quad.background == color
                        && px(quad.bounds.size.width.0 / scale) >= viewport.size.width
                })
                .min_by_key(|quad| quad.bounds.top())
                .expect("the expanded hunk has a deleted row");
            point(
                viewport.left() + px(30.),
                px(row.bounds.top().0 / scale) + px(2.),
            )
        });
        visual.simulate_mouse_move(hover, None, Modifiers::default());
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let previous = visual
            .debug_bounds("editor-git-previous-hunk:0")
            .expect("the previous hunk button is painted");
        let next = visual
            .debug_bounds("editor-git-next-hunk:0")
            .expect("the next hunk button is painted");
        assert_eq!(previous.size.height, HUNK_TOOLBAR_HEIGHT);
        assert_eq!(next.size.height, HUNK_TOOLBAR_HEIGHT);
        assert!(previous.size.width >= px(32.));
        assert!(next.size.width >= px(32.));

        visual.simulate_mouse_move(next.center(), None, Modifiers::default());
        visual.simulate_click(next.center(), Modifiers::default());
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            let file = &workspace.read(cx).files[&path];
            assert_eq!(file.git.selected, Some(1));
            assert!(file.git.pending_hunk.is_none());
            let editor = file.editor.read(cx);
            let viewport = editor.input_bounds();
            let line_height = editor.line_height().unwrap();
            let scale = window.scale_factor();
            let color =
                Background::from(code_theme::HunkPalette::for_mode(cx.theme().mode).removed_row);
            let target_top = window
                .painted_quads()
                .into_iter()
                .filter(|quad| {
                    quad.background == color
                        && px(quad.bounds.size.width.0 / scale) >= viewport.size.width
                })
                .map(|quad| px(quad.bounds.top().0 / scale))
                .min()
                .expect("the next hunk is expanded");
            let expected = viewport.top() + line_height * EDITOR_JUMP_CONTEXT_LINES as f32;
            assert!(
                (target_top - expected).abs() < px(1.),
                "the hunk starts {target_top:?} instead of five rows below {expected:?}"
            );
        });

        visual.update(|window, cx| {
            let editor = workspace.read(cx).files[&path].editor.clone();
            editor.update(cx, |editor, cx| editor.focus(window, cx));
        });
        visual.simulate_keystrokes("escape");
        visual.run_until_parked();
        visual.update(|_, cx| {
            let git = &workspace.read(cx).files[&path].git;
            assert!(git.selected.is_none());
            assert!(git.pending_hunk.is_none());
        });
    }

    #[cfg(debug_assertions)]
    fn check_gutter_toolbar(unstaged_text: &str, deleted_rows: usize, check_staging: bool) {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        let path = PathBuf::from("review.txt");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "user.email", "test@example.invalid"]);
        git(&["config", "commit.gpgsign", "false"]);
        fs::write(root.join(&path), "HEAD\nkeep this line\n").unwrap();
        git(&["add", "review.txt"]);
        git(&["commit", "-qm", "initial"]);
        fs::write(root.join(&path), "index\nkeep this line\n").unwrap();
        git(&["add", "review.txt"]);
        fs::write(root.join(&path), unstaged_text).unwrap();

        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let (workspace, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_config(store, config, WorkspaceView::Editor, window, cx)
        });
        visual.simulate_resize(size(px(1150.), px(760.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.clone(),
                        file: Some(path.clone()),
                    },
                    window,
                    cx,
                );
            })
        });
        visual.run_until_parked();

        let selection_start = unstaged_text.find("keep").unwrap();
        let selection = selection_start..selection_start + 4;
        let (click, line_height, caret_top) = visual.update(|window, cx| {
            let editor = workspace.read(cx).files[&path].editor.clone();
            editor.update(cx, |editor, cx| {
                editor.set_selected_range_with_direction(selection.clone(), true, cx);
            });
            window.draw(cx).clear(cx);
            let file = &workspace.read(cx).files[&path];
            assert_eq!(file.git.marks.get_ranges(cx).len(), 2);
            let scale = window.scale_factor();
            let marker_color = if deleted_rows == 0 {
                cx.theme().success
            } else {
                Palette::for_mode(cx.theme().mode).accent
            };
            let mark = window
                .painted_quads()
                .into_iter()
                .find(|quad| {
                    quad.background == Background::from(marker_color)
                        && (quad.bounds.size.width.0 / scale - 5.).abs() < 0.5
                })
                .expect("the unstaged gutter mark is painted");
            let viewport_left = editor.read(cx).input_bounds().left();
            assert!(
                px(mark.bounds.right().0 / scale) <= viewport_left,
                "the gutter mark sits on the editor's outer left edge"
            );
            let click = point(
                px(mark.bounds.left().0 / scale) + GIT_GUTTER_WIDTH - px(0.5),
                px(mark.bounds.center().y.0 / scale),
            );
            let (caret, line_height) = editor.read(cx).cursor_layout().unwrap();
            (click, line_height, caret.top())
        });
        visual.simulate_mouse_move(click, None, Modifiers::default());
        visual.simulate_click(click, Modifiers::default());
        visual.run_until_parked();
        let inline_marker = visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            let file = &workspace.read(cx).files[&path];
            assert_eq!(file.git.selected, Some(1));
            assert_eq!(file.editor.read(cx).text().to_string(), unstaged_text);
            let editor = file.editor.read(cx);
            let (caret, _) = editor.cursor_layout().unwrap();
            assert_eq!(caret.top() - caret_top, line_height * deleted_rows);
            assert_eq!(editor.selected_range(), selection);
            assert!(editor.is_selection_reversed());
            if deleted_rows == 0 {
                return click;
            }
            let scale = window.scale_factor();
            let accent = Background::from(Palette::for_mode(cx.theme().mode).accent);
            let marker = window
                .painted_quads()
                .into_iter()
                .find(|quad| {
                    quad.background == accent
                        && (quad.bounds.size.width.0 / scale - 5.).abs() < 0.5
                        && (quad.bounds.size.height.0 / scale - f32::from(line_height)).abs() < 0.5
                        && px(quad.bounds.center().y.0 / scale) < click.y + line_height.half()
                })
                .expect("the expanded gutter mark spans the deleted line");
            point(click.x, px(marker.bounds.center().y.0 / scale))
        });
        visual.simulate_mouse_move(inline_marker, None, Modifiers::default());
        visual.simulate_click(inline_marker, Modifiers::default());
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(workspace.read(cx).files[&path].git.selected.is_none());
        });
        visual.simulate_mouse_move(click, None, Modifiers::default());
        visual.simulate_click(click, Modifiers::default());
        let mut action_width = None;
        for font_size in [13.5, 20.] {
            let (first_row_top, row_height) = visual.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let mut config = workspace.config.current().clone();
                    config.editor.font_size = font_size;
                    workspace.config.save(config).unwrap();
                    cx.notify();
                });
                window.draw(cx).clear(cx);
                let scale = window.scale_factor();
                let background = if deleted_rows == 0 {
                    code_theme::HunkPalette::for_mode(cx.theme().mode).added_row
                } else {
                    code_theme::HunkPalette::for_mode(cx.theme().mode).removed_row
                };
                let editor = workspace.read(cx).files[&path].editor.read(cx);
                let viewport = editor.input_bounds();
                let quads = window.painted_quads();
                let row = quads
                    .iter()
                    .find(|quad| {
                        quad.background == Background::from(background)
                            && px(quad.bounds.left().0 / scale) <= viewport.left() + px(1.)
                            && px(quad.bounds.right().0 / scale) >= viewport.right() - px(1.)
                    })
                    .expect("the changed row fills the editor width");
                let gutter = quads.iter().any(|quad| {
                    quad.background == Background::from(background)
                        && quad.bounds.top() == row.bounds.top()
                        && quad.bounds.left() == row.bounds.left()
                        && quad.bounds.right() < row.bounds.right()
                });
                assert!(gutter, "the tint is also painted over the opaque gutter");
                let marker_color = if deleted_rows == 0 {
                    cx.theme().success
                } else {
                    Palette::for_mode(cx.theme().mode).accent
                };
                let marker = quads
                    .iter()
                    .find(|quad| {
                        quad.background == Background::from(marker_color)
                            && (quad.bounds.size.width.0 / scale - 5.).abs() < 0.5
                    })
                    .expect("the gutter mark is painted");
                assert_eq!(
                    row.bounds.left(),
                    marker.bounds.left(),
                    "the hunk fill starts at the gutter edge"
                );
                (
                    px(row.bounds.top().0 / scale),
                    editor.line_height().unwrap(),
                )
            });
            visual.simulate_mouse_move(
                point(click.x + px(30.), first_row_top + px(2.)),
                None,
                Modifiers::default(),
            );
            visual.update(|window, cx| window.draw(cx).clear(cx));
            let label = visual.debug_bounds("editor-git-hunk-controls").unwrap();
            assert!((label.top() - first_row_top).abs() < px(1.));
            assert_eq!(label.size.height, HUNK_TOOLBAR_HEIGHT);
            assert_ne!(row_height, label.size.height);
            let stage = visual.debug_bounds("git-hunk-action:0").unwrap();
            let restore = visual.debug_bounds("git-hunk-action:2").unwrap();
            if let Some(previous_width) = action_width {
                assert!(
                    stage.size.width > previous_width,
                    "the action label follows the code font size"
                );
            } else {
                action_width = Some(stage.size.width);
            }
            let previous = visual.debug_bounds("editor-git-previous-hunk:1").unwrap();
            let next = visual.debug_bounds("editor-git-next-hunk:1").unwrap();
            assert_eq!(previous.left(), label.left());
            assert_eq!(previous.right(), next.left());
            assert_eq!(next.right(), stage.left());
            assert_eq!(stage.right(), restore.left());
            assert_eq!(restore.right(), label.right());
            for button in [previous, next, stage, restore] {
                assert_eq!(button.top(), label.top());
                assert_eq!(button.bottom(), label.bottom());
            }
        }
        if deleted_rows > 0 {
            let editor = visual.update(|window, cx| {
                let editor = workspace.read(cx).files[&path].editor.clone();
                editor.update(cx, |editor, cx| {
                    editor.set_scroll_beyond_last_line(Some(100), window, cx);
                });
                window.draw(cx).clear(cx);
                editor
            });
            let hover = visual.update(|window, cx| {
                editor.update(cx, |editor, cx| {
                    let height = editor.line_height().unwrap();
                    editor.set_scroll_offset(point(px(0.), -height / 2.), cx);
                });
                window.draw(cx).clear(cx);
                let input = editor.read(cx).input_bounds();
                let scale = window.scale_factor();
                let tint = Background::from(
                    code_theme::HunkPalette::for_mode(cx.theme().mode).removed_row,
                );
                let top = window
                    .painted_quads()
                    .into_iter()
                    .filter(|quad| {
                        quad.background == tint
                            && quad.bounds.size.width.0 / scale >= input.size.width.as_f32()
                    })
                    .map(|quad| px(quad.bounds.top().0 / scale))
                    .next()
                    .expect("the partially visible deleted line has a background");
                assert!(
                    top < input.top(),
                    "the tint reaches into the same top padding as text"
                );
                point(input.left() + px(45.), top + px(2.))
            });
            visual.simulate_mouse_move(hover, None, Modifiers::default());
            visual.update(|window, cx| window.draw(cx).clear(cx));
            let label = visual.debug_bounds("editor-git-hunk-controls").unwrap();
            assert!((label.top() - (hover.y - px(2.))).abs() < px(1.));

            visual.update(|window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.set_scroll_offset(point(px(0.), px(0.)), cx);
                    editor.set_scroll_beyond_last_line(None, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            visual.simulate_mouse_move(click, None, Modifiers::default());
        }
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let restore = visual.debug_bounds("git-hunk-action:2").unwrap().center();
        visual.simulate_mouse_move(restore, None, Modifiers::default());
        visual.simulate_click(restore, Modifiers::default());
        visual.run_until_parked();
        visual.executor().advance_clock(Duration::from_millis(90));
        visual.run_until_parked();
        visual.update(|_, cx| {
            let file = &workspace.read(cx).files[&path];
            assert_eq!(
                file.editor.read(cx).text().to_string(),
                "index\nkeep this line\n"
            );
            assert!(file.is_dirty());
            assert_eq!(file.git.marks.get_ranges(cx), vec![0..6]);
        });
        assert_eq!(fs::read_to_string(root.join(&path)).unwrap(), unstaged_text);
        visual.simulate_keystrokes("ctrl-z");
        visual.update(|_, cx| {
            let file = &workspace.read(cx).files[&path];
            assert_eq!(file.editor.read(cx).text().to_string(), unstaged_text);
            assert_eq!(file.git.marks.get_ranges(cx).len(), 2);
        });
        visual.simulate_keystrokes("ctrl-shift-z");
        visual.update(|window, cx| {
            let file = &workspace.read(cx).files[&path];
            assert_eq!(file.committed.to_string(), "index\nkeep this line\n");
            let editor = file.editor.clone();
            editor.update(cx, |editor, cx| {
                editor.edit_ranges(
                    &[(0..0, "draft\n".to_owned()), (6..6, "extra\n".to_owned())],
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        visual.executor().advance_clock(Duration::from_millis(90));
        visual.run_until_parked();
        visual.update(|_, cx| {
            let file = &workspace.read(cx).files[&path];
            assert_eq!(
                file.committed.to_string(),
                "draft\nindex\nextra\nkeep this line\n"
            );
            let mut marks = file.git.marks.get_ranges(cx);
            marks.sort_by_key(|range| range.start);
            assert_eq!(marks, vec![0..6, 6..12, 12..18]);
        });
        visual.simulate_keystrokes("ctrl-z");
        visual.simulate_keystrokes("ctrl-y");
        visual.update(|_, cx| {
            assert_eq!(
                workspace.read(cx).files[&path].committed.to_string(),
                "draft\nindex\nextra\nkeep this line\n"
            );
        });
        visual.simulate_keystrokes("ctrl-z");
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.start_save(path.clone(), false, window, cx)
            });
        });
        visual.run_until_parked();
        assert_eq!(
            fs::read_to_string(root.join(&path)).unwrap(),
            "index\nkeep this line\n"
        );
        assert_eq!(git(&["show", ":review.txt"]), b"index\nkeep this line\n");
        if check_staging {
            visual.update(|_, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.toggle_editor_git(&path, 0, cx);
                });
            });
            for (button, destination, expected_index) in [
                (
                    "git-hunk-action:1",
                    Layer::Unstaged,
                    "HEAD\nkeep this line\n",
                ),
                (
                    "git-hunk-action:0",
                    Layer::Staged,
                    "index\nkeep this line\n",
                ),
            ] {
                let hover = visual.update(|window, cx| {
                    window.draw(cx).clear(cx);
                    let scale = window.scale_factor();
                    let file = &workspace.read(cx).files[&path];
                    let viewport = file.editor.read(cx).input_bounds();
                    let color = Background::from(
                        code_theme::HunkPalette::for_mode(cx.theme().mode).removed_row,
                    );
                    let row = window
                        .painted_quads()
                        .into_iter()
                        .find(|quad| {
                            quad.background == color
                                && px(quad.bounds.size.width.0 / scale) >= viewport.size.width
                        })
                        .expect("expanded hunk is painted");
                    point(
                        viewport.left() + px(30.),
                        px(row.bounds.top().0 / scale) + px(2.),
                    )
                });
                visual.simulate_mouse_move(hover, None, Modifiers::default());
                visual.update(|window, cx| window.draw(cx).clear(cx));
                let control = visual.debug_bounds(button).unwrap().center();
                visual.simulate_mouse_move(control, None, Modifiers::default());
                visual.simulate_click(control, Modifiers::default());
                visual.run_until_parked();
                visual.update(|_, cx| {
                    let git = &workspace.read(cx).files[&path].git;
                    let (patch, _) = editor_hunks(git.changes.as_ref().unwrap())
                        .nth(git.selected.expect("hunk remains expanded"))
                        .unwrap();
                    assert_eq!(patch.layer, destination);
                    assert!(git.pending_hunk.is_none());
                });
                assert_eq!(git(&["show", ":review.txt"]), expected_index.as_bytes());
            }
        }
    }
}
