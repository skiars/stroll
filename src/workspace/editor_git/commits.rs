use super::*;
use crate::workspace::line_alignment::LineAlignment;
use gpui_component::input::RopeExt as _;

const COMMIT_ACTION_START: usize = usize::MAX / 2 + 1;

#[derive(Clone)]
pub(in crate::workspace) struct CommitRange {
    pub base: Option<String>,
    pub target: String,
    pub oldest: String,
}

impl CommitRange {
    fn tooltip(&self) -> SharedString {
        let mut commits = git::short(&self.oldest).to_owned();
        if self.target != self.oldest {
            commits.push('…');
            commits.push_str(git::short(&self.target));
        }
        t!("git.editor_commit_marker", commits = commits).into()
    }
}

#[derive(Default)]
pub(in crate::workspace) struct CommitMarkers {
    range: Option<Arc<CommitRange>>,
    marks: Option<Arc<CommitMarks>>,
    task: Option<Task<()>>,
    refresh_pending: bool,
}

impl CommitMarkers {
    pub(in crate::workspace) fn active(&self) -> bool {
        self.range.is_some()
    }
}

struct CommitMarks {
    detail: Arc<git::Changes>,
    files: HashMap<String, usize>,
    head: String,
    tooltip: SharedString,
}

impl CommitMarks {
    fn load(
        backend: &git::Backend,
        range: &CommitRange,
        previous: Option<Arc<Self>>,
    ) -> Result<Option<Arc<Self>>> {
        let Some(head) = backend.checked_out_head()? else {
            return Ok(None);
        };
        if let Some(previous) = &previous
            && previous.head == head
        {
            return Ok(Some(previous.clone()));
        }
        if !backend.contains_commit(&head, &range.target)? {
            return Ok(None);
        }
        backend.check_revision_comparison()?;
        let detail = match previous {
            Some(previous) => previous.detail.clone(),
            None => Arc::new(
                backend
                    .review_files(git::Comparison::Versions {
                        base: range.base.clone(),
                        target: Some(range.target.clone()),
                    })?
                    .0,
            ),
        };
        let mut files = detail
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| file.code != 'D' && !file.submodule())
            .map(|(index, file)| (file.path.clone(), index))
            .collect::<HashMap<_, _>>();
        if !files.is_empty() && head != range.target {
            let (later, _) = backend.review_files(git::Comparison::Versions {
                base: Some(range.target.clone()),
                target: Some(head.clone()),
            })?;
            let mut renamed = Vec::new();
            for file in later.files {
                match file.code {
                    'R' => {
                        if let Some(index) = file.old_path.and_then(|path| files.remove(&path)) {
                            renamed.push((file.path, index));
                        }
                    }
                    'D' | 'T' => {
                        files.remove(&file.path);
                    }
                    _ => {}
                }
            }
            files.extend(renamed);
        }
        Ok(Some(Arc::new(Self {
            detail,
            files,
            head,
            tooltip: range.tooltip(),
        })))
    }
}

struct CommitPatch {
    target: Rope,
    line_count: usize,
    hunks: Vec<(git::Block, Vec<SharedString>)>,
}

impl CommitPatch {
    fn load(
        backend: &git::Backend,
        marks: &CommitMarks,
        index: usize,
    ) -> Result<Option<Arc<Self>>> {
        let file = &marks.detail.files[index];
        let patch = backend.review_patch_text(&marks.detail, file, 0, false)?;
        let hunks = patch.read_only_hunks().collect::<Vec<_>>();
        if hunks.is_empty() {
            return Ok(None);
        }
        let git::Snapshot::Text {
            text,
            truncated: false,
        } = backend.snapshot(&marks.detail, file, git::Side::New)?
        else {
            return Ok(None);
        };
        let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
        let line_count = text.split_terminator('\n').count();
        Ok(Some(Arc::new(Self {
            target: Rope::from(text.as_str()),
            line_count,
            hunks,
        })))
    }

    fn project(&self, current: &Rope) -> Vec<Vec<CommitSection>> {
        let Some(alignment) = LineAlignment::between(&self.target, current) else {
            return Vec::new();
        };
        let current_lines = current.lines_len()
            - usize::from(current.len() == 0 || current.byte(current.len() - 1) == b'\n');
        self.hunks
            .iter()
            .filter_map(|(block, deleted)| {
                let mut sections = Vec::new();
                let mut deleted = deleted.iter();
                for change in &block.changes {
                    let deleted = deleted
                        .by_ref()
                        .take(change.removed)
                        .cloned()
                        .collect::<Vec<_>>();
                    if change.added == 0 {
                        if let Some(line) = deletion_boundary(
                            &alignment,
                            change.start,
                            self.line_count,
                            current_lines,
                        ) {
                            sections.push(CommitSection {
                                lines: line..line,
                                deleted,
                            });
                        }
                        continue;
                    }
                    let mut runs: Vec<Range<usize>> = Vec::new();
                    for row in (change.start..change.start + change.added).filter_map(|row| {
                        alignment
                            .current_row(row)
                            .filter(|&line| line < current_lines)
                    }) {
                        if let Some(run) = runs.last_mut().filter(|run| run.end == row) {
                            run.end += 1;
                        } else {
                            runs.push(row..row + 1);
                        }
                    }
                    if let [lines] = runs.as_slice()
                        && lines.len() == change.added
                    {
                        sections.push(CommitSection {
                            lines: lines.clone(),
                            deleted,
                        });
                    } else {
                        sections.extend(runs.into_iter().map(|lines| CommitSection {
                            lines,
                            deleted: Vec::new(),
                        }));
                    }
                }
                (!sections.is_empty()).then_some(sections)
            })
            .collect()
    }
}

fn deletion_boundary(
    alignment: &LineAlignment,
    boundary: usize,
    target_lines: usize,
    current_lines: usize,
) -> Option<usize> {
    let before = match boundary.checked_sub(1) {
        Some(line) => alignment.current_row(line)? + 1,
        None => 0,
    };
    let after = if boundary == target_lines {
        current_lines
    } else {
        alignment.current_row(boundary)?
    };
    (before == after).then_some(after)
}

struct CommitSection {
    lines: Range<usize>,
    deleted: Vec<SharedString>,
}

pub(super) struct FileCommitMarkers {
    marks: GutterDecorationCollection,
    scope: Option<(Arc<CommitMarks>, usize)>,
    patch: Option<Arc<CommitPatch>>,
    revision: Option<u64>,
    task: Option<Task<()>>,
    hunks: Vec<Vec<CommitSection>>,
    pub(super) selected: Option<usize>,
}

impl FileCommitMarkers {
    pub(super) fn new(editor: &Entity<EditorState>, cx: &mut App) -> Self {
        Self {
            marks: editor.update(cx, |editor, cx| {
                editor.create_gutter_decorations_collection(Vec::new(), cx)
            }),
            scope: None,
            patch: None,
            revision: None,
            task: None,
            hunks: Vec::new(),
            selected: None,
        }
    }

    fn clear(&mut self, editor: &Entity<EditorState>, cx: &mut App) {
        if self.selected.take().is_some() {
            editor.update(cx, |editor, cx| editor.clear_gutter_hunk(cx));
        }
        self.task = None;
        self.scope = None;
        self.patch = None;
        self.revision = None;
        self.hunks.clear();
        self.marks.clear(cx);
    }

    fn paint(&mut self, text: &Rope, mode: ThemeMode, cx: &mut App) {
        let Some((scope, _)) = &self.scope else {
            return;
        };
        let marks = self
            .hunks
            .iter()
            .enumerate()
            .flat_map(|(index, sections)| {
                sections.iter().map(move |section| {
                    let offsets = text.line_start_offset(section.lines.start)
                        ..text.line_start_offset(section.lines.end);
                    GutterDecoration::new(offsets)
                        .with_action(COMMIT_ACTION_START + index)
                        .with_color(commit_marker_color(mode))
                        .fallback(true)
                        .with_tooltip(scope.tooltip.clone())
                        .without_overview_marker()
                })
            })
            .collect();
        self.marks.set(marks, cx);
    }
}

fn commit_marker_color(mode: ThemeMode) -> Hsla {
    match mode {
        ThemeMode::Dark => rgb(0xb493eb),
        ThemeMode::Light => rgb(0x7956b4),
    }
    .into()
}

impl Workspace {
    pub(in crate::workspace) fn mark_editor_commits(
        &mut self,
        range: CommitRange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_markers.range = Some(Arc::new(range));
        self.load_commit_markers(true, window, cx);
    }

    pub(in crate::workspace) fn clear_editor_commit_markers(&mut self, cx: &mut Context<Self>) {
        self.commit_markers = CommitMarkers::default();
        for file in self.files.values_mut() {
            file.git.commits.clear(&file.editor, cx);
        }
        cx.notify();
    }

    pub(in crate::workspace) fn load_commit_markers(
        &mut self,
        requested: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(range), Some(repo)) = (self.commit_markers.range.clone(), &self.repository)
        else {
            return;
        };
        if !requested && self.commit_markers.task.is_some() {
            self.commit_markers.refresh_pending = true;
            return;
        }
        self.commit_markers.refresh_pending = false;
        let backend = repo.git_backend();
        let previous = (!requested)
            .then(|| self.commit_markers.marks.clone())
            .flatten();
        let load_range = range.clone();
        let task =
            cx.background_spawn(async move { CommitMarks::load(&backend, &load_range, previous) });
        self.commit_markers.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, |workspace, window, cx| {
                if !workspace
                    .commit_markers
                    .range
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &range))
                {
                    return;
                }
                workspace.commit_markers.task = None;
                let refresh_pending = std::mem::take(&mut workspace.commit_markers.refresh_pending);
                match result {
                    Ok(Some(marks)) => {
                        if !workspace
                            .commit_markers
                            .marks
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, &marks))
                        {
                            for file in workspace.files.values_mut() {
                                file.git.commits.clear(&file.editor, cx);
                            }
                            workspace.commit_markers.marks = Some(marks);
                            if workspace.view == WorkspaceView::Editor {
                                workspace.refresh_active_commit_markers(window, cx);
                            }
                        }
                    }
                    Ok(None) => {
                        workspace.clear_editor_commit_markers(cx);
                        if requested {
                            workspace.show_error(
                                anyhow::anyhow!(t!("git.editor_commit_not_ancestor")),
                                window,
                                cx,
                            );
                        }
                    }
                    Err(error) => {
                        workspace.clear_editor_commit_markers(cx);
                        if requested {
                            workspace.show_error(error, window, cx);
                        }
                    }
                }
                if refresh_pending {
                    workspace.load_commit_markers(false, window, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn refresh_active_commit_markers(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self.tabs.active().map(Path::to_owned) {
            self.refresh_editor_commit_markers(&path, window, cx);
        }
    }

    pub(in crate::workspace) fn refresh_editor_commit_markers(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(scope), Some(repo), Some(file)) = (
            self.commit_markers.marks.clone(),
            self.repository.as_ref(),
            self.files.get_mut(path),
        ) else {
            return;
        };
        let marks = &mut file.git.commits;
        let index = git_path(repo, path)
            .filter(|_| !file.missing && !file.document.conflict)
            .and_then(|path| {
                let head_path = match &file.git.changes {
                    Some(changes) => changes.version.blame_path(&path)?,
                    None => &path,
                };
                scope.files.get(head_path).copied()
            });
        let Some(index) = index else {
            marks.clear(&file.editor, cx);
            return;
        };
        let same_scope = marks
            .scope
            .as_ref()
            .is_some_and(|(previous, previous_index)| {
                *previous_index == index && Arc::ptr_eq(previous, &scope)
            });
        if !same_scope {
            marks.clear(&file.editor, cx);
            marks.scope = Some((scope.clone(), index));
        }
        let revision = file.revision;
        if marks.revision == Some(revision) {
            return;
        }
        marks.revision = Some(revision);
        let patch = marks.patch.clone();
        let backend = repo.git_backend();
        let editor = file.editor.downgrade();
        let text = file.committed.clone();
        let path = path.to_owned();
        let load_scope = scope.clone();
        let task = cx.background_spawn(async move {
            let patch = match patch {
                Some(patch) => Some(patch),
                None => CommitPatch::load(&backend, &load_scope, index)?,
            };
            let hunks = patch
                .as_ref()
                .map_or_else(Vec::new, |patch| patch.project(&text));
            Ok::<_, anyhow::Error>((patch, hunks))
        });
        marks.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ =
                this.update_in(cx, |workspace, _, cx| {
                    let Some(file) = workspace.files.get_mut(&path).filter(|file| {
                        file.editor.downgrade() == editor && file.revision == revision
                    }) else {
                        return;
                    };
                    let marks = &mut file.git.commits;
                    if !marks
                        .scope
                        .as_ref()
                        .is_some_and(|(current, current_index)| {
                            *current_index == index && Arc::ptr_eq(current, &scope)
                        })
                    {
                        return;
                    }
                    marks.task = None;
                    let (patch, hunks) = result.unwrap_or_default();
                    marks.patch = patch;
                    marks.hunks = hunks;
                    marks.paint(&file.committed, cx.theme().mode, cx);
                });
        }));
    }

    pub(super) fn toggle_commit_hunk(
        &mut self,
        path: &Path,
        action: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        if !(COMMIT_ACTION_START..usize::MAX).contains(&action) {
            return false;
        }
        let Some(file) = self.files.get_mut(path) else {
            return true;
        };
        let index = action - COMMIT_ACTION_START;
        if file.git.commits.selected == Some(index) {
            file.git.clear_hunk(&file.editor, cx);
        } else {
            Self::expand_commit_hunk(file, index, cx);
        }
        true
    }

    pub(super) fn repaint_commit_hunks(&mut self, cx: &mut Context<Self>) {
        for file in self.files.values_mut() {
            if file.git.commits.revision != Some(file.revision) || file.git.commits.task.is_some() {
                continue;
            }
            file.git.commits.paint(&file.committed, cx.theme().mode, cx);
            if let Some(index) = file.git.commits.selected {
                Self::expand_commit_hunk(file, index, cx);
            }
        }
    }

    fn expand_commit_hunk(file: &mut OpenFile, index: usize, cx: &mut App) {
        let marks = &mut file.git.commits;
        if marks.task.is_some() || marks.revision != Some(file.revision) {
            return;
        }
        let Some(sections) = marks.hunks.get(index) else {
            return;
        };
        let mode = cx.theme().mode;
        let palette = code_theme::HunkPalette::for_mode(mode);
        let sections = sections
            .iter()
            .map(|section| GutterHunkSection {
                line: section.lines.start,
                deleted: section.deleted.clone(),
                added: section.lines.len(),
                deleted_background: palette.removed_row,
                added_background: palette.added_row,
            })
            .collect();
        file.git.selected = None;
        file.git.pending_hunk = None;
        marks.selected = Some(index);
        file.editor.update(cx, |editor, cx| {
            editor.set_gutter_hunk(
                COMMIT_ACTION_START + index,
                sections,
                commit_marker_color(mode),
                false,
                |_, _| div().into_any_element(),
                cx,
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn commit_markers_follow_shifted_lines_and_drop_ambiguous_deletions() {
        let original = "above\nold line\nbelow\nfirst anchor\ndeleted line\nsecond anchor\n";
        let target = "above\nnew line\nbelow\nfirst anchor\nsecond anchor\n";
        let hunks = git::buffer_diff_blocks(original, target)
            .into_iter()
            .zip([vec!["old line".into()], vec!["deleted line".into()]])
            .collect();
        let patch = CommitPatch {
            target: Rope::from(target),
            line_count: target.lines().count(),
            hunks,
        };
        let shifted = patch.project(&Rope::from(format!("draft prefix\n{target}").as_str()));
        assert_eq!(shifted.len(), 2);
        assert_eq!(shifted[0][0].lines, 2..3);
        assert_eq!(shifted[1][0].lines, 5..5);
        assert_eq!(shifted[1][0].deleted, [SharedString::from("deleted line")]);

        let edited = Rope::from(
            "above\nrewritten line\nbelow\nfirst anchor\ndraft insertion\nsecond anchor\n",
        );
        assert!(patch.project(&edited).is_empty());
    }
}
