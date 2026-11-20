use super::{review::ActionBlocks, *};
use crate::workspace::{
    DiffSource, OpenFile, WorkspaceView, code_theme::HunkPalette, editor_git::hunk_toolbar_row,
    inlay_hints::InlayHints, line_alignment::LineAlignment,
};
use gpui_component::{
    diff::{DiffEditorRow, DiffSide},
    input::{
        DefinitionProvider, Editor, HUNK_TOOLBAR_HEIGHT, HoverProvider, InlayHint, InputEditsEvent,
        Rope, RopeExt, TextDecoration, TextDecorationCollection, hovered_hunk_top,
    },
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    ops::Range,
    path::Path,
};

const FILE_HEADER_HEIGHT: Pixels = px(28.);

#[derive(Clone, PartialEq, Action)]
#[action(namespace = git, no_json, no_register)]
struct OpenDiffRowInEditor(DiffLinePosition);

#[derive(Clone)]
struct Line {
    position: DiffLinePosition,
    source: Option<PathBuf>,
    bytes: Range<usize>,
}

#[derive(Clone)]
pub(super) struct SnapshotEditor {
    editor: Entity<EditorState>,
    worktree_path: Option<PathBuf>,
    alignment: Rc<RefCell<Option<SnapshotAlignment>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct WorktreeVersion {
    editor: gpui::EntityId,
    revision: u64,
    document_revision: u64,
    disk: [u8; 32],
}

impl WorktreeVersion {
    fn of(file: &OpenFile, cx: &App) -> Option<Self> {
        let editor = file.editor.read(cx);
        (!file.missing && !file.document.conflict && !editor.is_composing()).then(|| Self {
            editor: file.editor.entity_id(),
            revision: file.revision,
            document_revision: editor.document_revision(),
            disk: file.document.draft_baseline(),
        })
    }
}

struct SnapshotAlignment {
    version: WorktreeVersion,
    lines: Option<Rc<LineAlignment>>,
}

impl SnapshotEditor {
    pub(super) fn new(editor: Entity<EditorState>, worktree_path: Option<PathBuf>) -> Self {
        Self {
            editor,
            worktree_path,
            alignment: Rc::default(),
        }
    }

    fn aligned_lines(&self, file: &OpenFile, cx: &App) -> Option<Rc<LineAlignment>> {
        let version = WorktreeVersion::of(file, cx)?;
        self.alignment
            .borrow()
            .as_ref()
            .filter(|cached| cached.version == version)?
            .lines
            .clone()
    }
}

pub(super) type SnapshotEditors = HashMap<(SharedString, DiffSide), SnapshotEditor>;

#[derive(Default)]
struct Projection {
    lines: Vec<Option<Line>>,
    text: Rope,
    snapshots: Rc<SnapshotEditors>,
    edited: bool,
    row_mapping: Option<Rc<RowMapping>>,
}

// The diff model still addresses the last published patch while the editor
// already lays out the edited text. Track the surviving rows in both spaces.
#[derive(Clone)]
struct RowMapping(Vec<(usize, usize)>);

impl RowMapping {
    fn projected(&self, row: usize) -> Option<usize> {
        let index = self.0.binary_search_by_key(&row, |&(row, _)| row).ok()?;
        Some(self.0[index].1)
    }

    fn patch_start(&self, projected: usize) -> usize {
        self.0
            .partition_point(|&(_, row)| row <= projected)
            .checked_sub(1)
            .map_or(0, |index| self.0[index].0)
    }

    fn patch_end(&self, projected: usize, len: usize) -> usize {
        let index = self.0.partition_point(|&(_, row)| row < projected);
        self.0.get(index).map_or(len, |&(row, _)| row)
    }
}

fn projected_inlay_hints(
    panel: &GitPanel,
    projection: &Projection,
    text: &Rope,
    ranges: &[Range<usize>],
    cx: &App,
) -> Task<Result<Vec<InlayHint>>> {
    let Some(workspace) = panel.workspace.upgrade() else {
        return Task::ready(Ok(Vec::new()));
    };
    let owner = workspace.read(cx);
    if &projection.text != text {
        return Task::ready(Ok(Vec::new()));
    }
    let mut sources: HashMap<&Path, BTreeMap<usize, Vec<usize>>> = HashMap::new();
    let mut verified_snapshots = HashMap::new();
    for view_row in ranges.iter().flat_map(|range| range.clone()) {
        let Some(line) = projection.lines.get(view_row).and_then(Option::as_ref) else {
            continue;
        };
        let Some(path) = projection.hint_path(panel, &line.position) else {
            continue;
        };
        let Some(file) = owner.files.get(path).filter(|file| !file.missing) else {
            continue;
        };
        let original = line.position.line() - 1;
        let snapshot = projection.snapshot(&line.position);
        let row =
            if let Some(snapshot) = snapshot.filter(|snapshot| snapshot.worktree_path.is_some()) {
                let Some(row) = snapshot
                    .aligned_lines(file, cx)
                    .and_then(|lines| lines.current_row(original))
                else {
                    continue;
                };
                row
            } else {
                let anchored = line.source.as_deref() == Some(path)
                    && line.bytes.start == file.committed.line_start_offset(original);
                if !anchored
                    && !*verified_snapshots
                        .entry((line.position.path(), line.position.side()))
                        .or_insert_with(|| {
                            snapshot.is_some_and(|snapshot| {
                                snapshot.editor.read(cx).text() == &file.committed
                            })
                        })
                {
                    continue;
                }
                original
            };
        if row >= file.committed.lines_len()
            || file.committed.slice_line(row) != text.slice_line(view_row)
        {
            continue;
        }
        sources
            .entry(path)
            .or_default()
            .entry(row)
            .or_default()
            .push(text.line_start_offset(view_row));
    }
    let aligned = panel.review.as_ref().is_some_and(|review| {
        super::review::aligns_historical_snapshots(&review.detail.comparison)
    });
    let mut requests = Vec::new();
    for (path, rows) in sources {
        let file = &owner.files[path];
        let Some(server) = &file.language_server else {
            continue;
        };
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for &row in rows.keys() {
            if let Some(last) = ranges.last_mut().filter(|last| last.end == row) {
                last.end += 1;
            } else {
                ranges.push(row..row + 1);
            }
        }
        let text = file.committed.clone();
        let version = aligned.then(|| WorktreeVersion::of(file, cx)).flatten();
        let document = server.document.clone();
        let task = crate::lsp::editor_inlay_hints(&document, &text, &ranges, cx);
        requests.push((path.to_owned(), document, text, version, rows, task));
    }
    let workspace = workspace.downgrade();
    cx.spawn(async move |cx| {
        let mut results = Vec::new();
        for (path, document, text, version, rows, request) in requests {
            results.push((path, document, text, version, rows, request.await?));
        }
        Ok(workspace
            .read_with(cx, |owner, cx| {
                let mut projected = Vec::new();
                for (path, document, text, version, rows, hints) in results {
                    if !owner.files.get(&path).is_some_and(|file| {
                        !file.missing
                            && match version {
                                Some(version) => WorktreeVersion::of(file, cx) == Some(version),
                                None => file.committed == text,
                            }
                            && file
                                .language_server
                                .as_ref()
                                .is_some_and(|server| Arc::ptr_eq(&server.document, &document))
                    }) {
                        continue;
                    }
                    for hint in hints {
                        let point = text.offset_to_point(hint.offset);
                        if let Some(starts) = rows.get(&point.row) {
                            projected.extend(starts.iter().map(|start| InlayHint {
                                offset: start + point.column,
                                label: hint.label.clone(),
                            }));
                        }
                    }
                }
                projected
            })
            .unwrap_or_default())
    })
}

impl Projection {
    fn snapshot(&self, position: &DiffLinePosition) -> Option<&SnapshotEditor> {
        self.snapshots
            .get(&(position.path().clone(), position.side()))
    }

    fn hint_path<'a>(
        &'a self,
        panel: &'a GitPanel,
        position: &DiffLinePosition,
    ) -> Option<&'a Path> {
        self.snapshot(position)
            .and_then(|snapshot| snapshot.worktree_path.as_deref())
            .or_else(|| panel.worktree_hint_path(position).map(Path::new))
    }

    fn source_range(&self, range: &Range<usize>) -> Option<(PathBuf, Range<usize>)> {
        let start = self.text.offset_to_point(range.start);
        let end = self.text.offset_to_point(range.end);
        let first = self.lines.get(start.row)?.as_ref()?;
        let path = first.source.as_ref()?;
        let mut previous = first;
        for row in start.row + 1..=end.row {
            let line = self.lines.get(row)?;
            let line = line.as_ref()?;
            if line.source.as_ref() != Some(path)
                || previous.bytes.end.checked_add(1) != Some(line.bytes.start)
                || line.position.line() != previous.position.line() + 1
            {
                return None;
            }
            previous = line;
        }
        Some((
            path.clone(),
            (first.bytes.start + start.column).min(first.bytes.end)
                ..(previous.bytes.start + end.column).min(previous.bytes.end),
        ))
    }

    fn position(&self, offset: usize) -> Option<(DiffLinePosition, usize)> {
        let point = self.text.offset_to_point(offset.min(self.text.len()));
        Some((
            self.lines.get(point.row)?.as_ref()?.position.clone(),
            point.column,
        ))
    }

    fn offset(&self, position: &DiffLinePosition, column: usize) -> Option<usize> {
        let line = self
            .lines
            .iter()
            .position(|line| line.as_ref().is_some_and(|line| &line.position == position))?;
        Some(self.text.line_start_offset(line) + column.min(self.text.slice_line(line).len()))
    }
}

impl Projection {
    fn language_source(
        &self,
        offset: usize,
        workspace: &WeakEntity<Workspace>,
        cx: &App,
    ) -> Option<(Entity<EditorState>, usize)> {
        let point = self.text.offset_to_point(offset);
        let line = self.lines.get(point.row)?.as_ref()?;
        if let Some(path) = &line.source {
            let editor = workspace
                .read_with(cx, |workspace, _| {
                    workspace.files.get(path).map(|file| file.editor.clone())
                })
                .ok()??;
            let text = editor.read(cx).text();
            if line.bytes.end > text.len()
                || !text.is_char_boundary(line.bytes.start)
                || !text.is_char_boundary(line.bytes.end)
                || text.slice(line.bytes.clone()) != self.text.slice_line(point.row)
            {
                return None;
            }
            return Some((editor, line.bytes.start + point.column));
        }
        self.snapshot_source(point.row, point.column, cx)
    }

    fn snapshot_source(
        &self,
        view_row: usize,
        column: usize,
        cx: &App,
    ) -> Option<(Entity<EditorState>, usize)> {
        let line = self.lines.get(view_row)?.as_ref()?;
        let editor = &self.snapshot(&line.position)?.editor;
        let text = editor.read(cx).text();
        let row = line.position.line().checked_sub(1)?;
        if row >= text.lines_len()
            || !text
                .slice_line(row)
                .chars()
                .eq(self.text.slice_line(view_row).chars())
        {
            return None;
        }
        let offset = text.line_start_offset(row) + column;
        Some((editor.clone(), offset))
    }

    fn map_range(
        &self,
        offset: usize,
        source: &Rope,
        range: lsp_types::Range,
        alignment: Option<&LineAlignment>,
    ) -> Option<lsp_types::Range> {
        let (position, _) = self.position(offset)?;
        let source_rows = range.start.line as usize..=range.end.line as usize;
        if source_rows.is_empty() || *source_rows.end() >= source.lines_len() {
            return None;
        }
        let original_rows = match alignment {
            Some(alignment) => alignment.original_rows(source_rows)?,
            None => source_rows,
        };
        let map = |point: lsp_types::Position, row| {
            let column = source
                .position_to_offset(&point)
                .checked_sub(source.line_start_offset(point.line as usize))?;
            let position = DiffLinePosition::new(position.path().clone(), position.side(), row + 1);
            self.offset(&position, column)
        };
        let start = map(range.start, *original_rows.start())?;
        let end = map(range.end, *original_rows.end())?;
        if alignment.is_some() {
            let start_row = self.text.offset_to_point(start).row;
            let end_row = self.text.offset_to_point(end).row;
            let lines = self.lines.get(start_row..=end_row)?;
            if lines.len() != *original_rows.end() - *original_rows.start() + 1
                || !lines.iter().zip(original_rows).all(|(line, row)| {
                    line.as_ref().is_some_and(|line| {
                        line.position.path() == position.path()
                            && line.position.side() == position.side()
                            && line.position.line() == row + 1
                    })
                })
            {
                return None;
            }
        }
        Some(lsp_types::Range::new(
            self.text.offset_to_position(start),
            self.text.offset_to_position(end),
        ))
    }
}

struct LanguageSource {
    editor: Entity<EditorState>,
    offset: usize,
    alignment: Option<Rc<LineAlignment>>,
}

impl LanguageSource {
    fn matches(&self, current: &Self) -> bool {
        self.editor == current.editor
            && self.offset == current.offset
            && match (&self.alignment, &current.alignment) {
                (Some(before), Some(after)) => Rc::ptr_eq(before, after),
                (None, None) => true,
                _ => false,
            }
    }
}

#[derive(Clone)]
struct ProjectedLanguageFeatures {
    projection: Rc<RefCell<Projection>>,
    workspace: WeakEntity<Workspace>,
}

impl ProjectedLanguageFeatures {
    fn source(&self, offset: usize, cx: &App) -> Option<LanguageSource> {
        let projection = self.projection.borrow();
        let (editor, source_offset) = projection.language_source(offset, &self.workspace, cx)?;
        if let Some((position, column)) = projection.position(offset)
            && let Some(snapshot) = projection.snapshot(&position)
            && let Some(path) = &snapshot.worktree_path
            && let Some(source) = self
                .workspace
                .read_with(cx, |workspace, cx| {
                    let file = workspace.files.get(path)?;
                    file.language_server.as_ref()?;
                    let alignment = snapshot.aligned_lines(file, cx)?;
                    let row = alignment.current_row(position.line().checked_sub(1)?)?;
                    Some(LanguageSource {
                        editor: file.editor.clone(),
                        offset: file.committed.line_start_offset(row) + column,
                        alignment: Some(alignment),
                    })
                })
                .ok()
                .flatten()
        {
            return Some(source);
        }
        Some(LanguageSource {
            editor,
            offset: source_offset,
            alignment: None,
        })
    }
}

impl HoverProvider for ProjectedLanguageFeatures {
    fn hover(
        &self,
        view_text: &Rope,
        offset: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<lsp_types::Hover>>> {
        let Some(source) = self.source(offset, cx) else {
            return Task::ready(Ok(None));
        };
        let editor = source.editor.read(cx);
        let Some(provider) = editor.lsp().hover_provider.clone() else {
            return Task::ready(Ok(None));
        };
        let text = editor.text().clone();
        let revision = editor.document_revision();
        let request = provider.hover(&text, source.offset, window, cx);
        let features = self.clone();
        let view_text = view_text.clone();
        cx.spawn(async move |cx| {
            let response = request.await?;
            Ok(cx.update(|cx| {
                if features.projection.borrow().text != view_text
                    || source.editor.read(cx).document_revision() != revision
                    || !source
                        .editor
                        .read(cx)
                        .lsp()
                        .hover_provider
                        .as_ref()
                        .is_some_and(|current| Rc::ptr_eq(current, &provider))
                    || !features
                        .source(offset, cx)
                        .is_some_and(|current| source.matches(&current))
                {
                    return None;
                }
                response.map(|mut hover| {
                    hover.range = hover.range.and_then(|range| {
                        features.projection.borrow().map_range(
                            offset,
                            &text,
                            range,
                            source.alignment.as_deref(),
                        )
                    });
                    hover
                })
            }))
        })
    }

    fn enrich_hover(
        &self,
        hover: &lsp_types::Hover,
        offset: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Option<gpui_component::input::HoverDocumentation>> {
        let Some(source) = self.source(offset, cx) else {
            return Task::ready(None);
        };
        let provider = source.editor.read(cx).lsp().hover_provider.clone();
        provider.map_or_else(
            || Task::ready(None),
            |provider| provider.enrich_hover(hover, source.offset, window, cx),
        )
    }
}

impl DefinitionProvider for ProjectedLanguageFeatures {
    fn definitions(
        &self,
        view_text: &Rope,
        offset: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<lsp_types::LocationLink>>> {
        let Some(source) = self.source(offset, cx) else {
            return Task::ready(Ok(Vec::new()));
        };
        let editor = source.editor.read(cx);
        let Some(provider) = editor.lsp().definition_provider.clone() else {
            return Task::ready(Ok(Vec::new()));
        };
        let text = editor.text().clone();
        let revision = editor.document_revision();
        let request = provider.definitions(&text, source.offset, window, cx);
        let features = self.clone();
        let view_text = view_text.clone();
        cx.spawn(async move |cx| {
            let mut links = request.await?;
            Ok(cx.update(|cx| {
                if features.projection.borrow().text != view_text
                    || source.editor.read(cx).document_revision() != revision
                    || !source
                        .editor
                        .read(cx)
                        .lsp()
                        .definition_provider
                        .as_ref()
                        .is_some_and(|current| Rc::ptr_eq(current, &provider))
                    || !features
                        .source(offset, cx)
                        .is_some_and(|current| source.matches(&current))
                {
                    return Vec::new();
                }
                let projection = features.projection.borrow();
                for link in &mut links {
                    link.origin_selection_range = link.origin_selection_range.and_then(|range| {
                        projection.map_range(offset, &text, range, source.alignment.as_deref())
                    });
                }
                links
            }))
        })
    }
}

pub(super) fn snapshot_editor(
    text: Rope,
    language: &'static str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<EditorState> {
    cx.new(|cx| {
        let mut editor = EditorState::new(window, cx)
            .language(language)
            .folding(false)
            .default_file_text(text);
        editor.lsp_mut().hover_provider = Some(Rc::new(crate::symbols::LocalSymbols(language)));
        editor.ensure_highlighter_factory(gpui_component::highlighter::input_highlighter_factory());
        editor.prepare_language(window, cx);
        editor
    })
}

struct Column {
    editor: Entity<EditorState>,
    projection: Rc<RefCell<Projection>>,
    highlights: TextDecorationCollection,
    presentation: RefCell<Rc<Vec<(Option<usize>, Option<Hsla>)>>>,
    inlay_hints: InlayHints,
}

impl Column {
    fn new(window: &mut Window, cx: &mut Context<GitPanel>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx)
                .progressive_wrap()
                .folding(false);
            // `render` paints the file headers and the review summary in an
            // overlay canvas on top of the editor, so the editor's own
            // scrollbar would otherwise be clipped wherever a header row
            // crosses it. Defer the scrollbar past the overlay, but keep it
            // below popups and menus.
            editor.set_scrollbar_renderer(Some(Rc::new(|context, _window, _cx| {
                let axis = context.axis();
                let scrollbar = if axis.is_both() {
                    Scrollbar::new(&context)
                } else if axis.is_vertical() {
                    Scrollbar::vertical(&context)
                } else {
                    Scrollbar::horizontal(&context)
                };
                // Diff content stays readable: its scrollbar does not fade out.
                deferred(scrollbar.mode(ScrollbarMode::Always)).into_any_element()
            })));
            editor
        });
        let highlights = editor.update(cx, |editor, cx| {
            editor.create_decorations_collection(Vec::new(), cx)
        });
        let inlay_hints = InlayHints::new(&editor, cx);
        Self {
            editor,
            highlights,
            presentation: RefCell::default(),
            projection: Rc::default(),
            inlay_hints,
        }
    }

    fn install(
        &self,
        old_rows: Range<usize>,
        lines: Vec<Option<Line>>,
        text: String,
        presentation: Vec<(Option<usize>, Option<Hsla>)>,
        snapshots: Rc<SnapshotEditors>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let mut projection = self.projection.borrow_mut();
        let replace_all = old_rows == (0..projection.lines.len());
        let range = projection.text.line_start_offset(old_rows.start)
            ..projection.text.line_start_offset(old_rows.end);
        let editor = self.editor.read(cx);
        let selection = editor.selected_range();
        let reversed = editor.is_selection_reversed();
        let scroll = editor.scroll_offset();
        let start = projection.position(selection.start);
        let end = projection.position(selection.end);
        let changed = !projection
            .text
            .slice(range.clone())
            .chars()
            .eq(text.chars());
        // A text replacement remaps hints in untouched rows itself. Without
        // a replacement, keep them only while the projected origins match.
        let same_origins = changed
            || (old_rows.len() == lines.len()
                && projection.lines[old_rows.clone()]
                    .iter()
                    .zip(&lines)
                    .all(|(old, new)| match (old, new) {
                        (None, None) => true,
                        (Some(old), Some(new)) => {
                            old.position == new.position
                                && old.source == new.source
                                && projection
                                    .snapshot(&old.position)
                                    .map(|old| old.editor.entity_id())
                                    == snapshots
                                        .get(&(new.position.path().clone(), new.position.side()))
                                        .map(|new| new.editor.entity_id())
                        }
                        _ => false,
                    }));

        projection.lines.splice(old_rows.clone(), lines);
        if changed {
            projection.text.replace(range.clone(), &text);
        }
        projection.snapshots = snapshots;
        projection.edited = false;
        projection.row_mapping = None;

        let mut rows = self.presentation.borrow_mut();
        // Local edits shift the projection before the diff refreshes its presentation.
        if replace_all {
            *rows = Rc::new(presentation);
        } else {
            Rc::make_mut(&mut rows).splice(old_rows, presentation);
        }
        let rows = rows.clone();
        let map = |offset, anchor: Option<(DiffLinePosition, usize)>| {
            anchor
                .and_then(|(pos, col)| projection.offset(&pos, col))
                .unwrap_or(offset)
        };
        let start = map(selection.start, start);
        let end = map(selection.end, end);
        drop(projection);
        self.editor.update(cx, |editor, cx| {
            if changed {
                editor.replace_projected_text(range, text, window, cx);
                editor.set_selected_range_with_direction(
                    start.min(end)..start.max(end),
                    reversed,
                    cx,
                );
                editor.set_scroll_offset(scroll, cx);
            }
            editor.set_projected_rows(rows, cx);
        });
        same_origins
    }
}

pub(super) struct ReviewEditor {
    original: Column,
    modified: Column,
    rows: Rc<Vec<DiffEditorRow>>,
    row_revision: Option<usize>,
    headers: Rc<Vec<usize>>,
    hunk_neighbors: Rc<HashMap<usize, [Option<usize>; 2]>>,
    blocks: Rc<Vec<Option<(SharedString, ActionBlocks, usize)>>>,
    actions_valid: bool,
    dirty_sources: HashSet<PathBuf>,
    split: bool,
    last_scroll: Rc<std::cell::Cell<(Pixels, Pixels)>>,
    scroll_from_left: Rc<std::cell::Cell<bool>>,
    viewport: Rc<std::cell::Cell<(usize, usize, usize, Pixels)>>,
    navigation_row: usize,
    navigation_source: Option<DiffLinePosition>,
    restore_viewport: bool,
    tail_rows: Rc<Cell<Option<usize>>>,
    wrap_rows: Rc<RefCell<Rc<Vec<usize>>>>,
    wrap_revision: Rc<Cell<Option<(usize, usize)>>>,
    block_layout: Option<(Pixels, Pixels, Pixels, f32, Option<usize>)>,
    notices: Rc<HashMap<usize, SharedString>>,
    highlight_revision: usize,
    theme: Option<gpui_component::ThemeMode>,
    context_click: Rc<RefCell<Option<DiffLinePosition>>>,
    alignment_task: Option<Task<()>>,
}

impl ReviewEditor {
    pub(super) fn has_pending_projection(&self, panel: &GitPanel) -> bool {
        self.modified.projection.borrow().edited && !panel.live_tasks.is_empty()
    }

    pub(super) fn new(window: &mut Window, cx: &mut Context<GitPanel>) -> Self {
        Self {
            original: Column::new(window, cx),
            modified: Column::new(window, cx),
            rows: Rc::default(),
            row_revision: None,
            headers: Rc::default(),
            hunk_neighbors: Rc::default(),
            blocks: Rc::default(),
            actions_valid: false,
            dirty_sources: HashSet::new(),
            split: false,
            last_scroll: Rc::default(),
            scroll_from_left: Rc::default(),
            viewport: Rc::default(),
            navigation_row: 0,
            navigation_source: None,
            restore_viewport: false,
            tail_rows: Rc::default(),
            wrap_rows: Rc::default(),
            wrap_revision: Rc::default(),
            block_layout: None,
            notices: Rc::default(),
            highlight_revision: 0,
            theme: None,
            context_click: Rc::default(),
            alignment_task: None,
        }
    }

    pub(super) fn bind(
        &self,
        cx: &mut Context<GitPanel>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
    ) -> Subscription {
        let model = self.modified.projection.clone();
        let weak = workspace.clone();
        let panel = cx.weak_entity();
        for column in [&self.original, &self.modified] {
            let inlay_panel = panel.clone();
            let inlay_projection = column.projection.clone();
            column.inlay_hints.set_provider(
                Some(Rc::new(move |text, rows, cx| {
                    inlay_panel
                        .read_with(cx, |panel, cx| {
                            projected_inlay_hints(panel, &inlay_projection.borrow(), text, rows, cx)
                        })
                        .unwrap_or_else(|_| Task::ready(Ok(Vec::new())))
                })),
                cx,
            );
            let projection = column.projection.clone();
            let workspace = workspace.clone();
            column.editor.update(cx, |editor, _| {
                let provider = Rc::new(ProjectedLanguageFeatures {
                    projection: projection.clone(),
                    workspace: workspace.clone(),
                });
                let source = provider.clone();
                editor.set_source_editor(move |offset, cx| {
                    source
                        .projection
                        .borrow()
                        .language_source(offset, &source.workspace, cx)
                });
                let features = editor.lsp_mut();
                features.hover_provider = Some(provider.clone());
                features.definition_provider = Some(provider);
                features.show_document = Some(Rc::new(move |params, window, cx| {
                    let workspace = workspace.clone();
                    let params = params.clone();
                    window.defer(cx, move |window, cx| {
                        _ = workspace.update(cx, |workspace, cx| {
                            workspace.open_definition(params, window, cx)
                        });
                    });
                    true
                }));
            });
        }
        self.modified.editor.update(cx, |editor, _| {
            editor.set_history_handler(move |undo, window, cx| {
                let panel = panel.clone();
                window.defer(cx, move |window, cx| {
                    _ = panel.update(cx, |panel, cx| {
                        panel.replay_review_history(undo, window, cx)
                    });
                });
            });
            let model = model.clone();
            editor.set_edit_guard(move |ranges, cx| {
                weak.read_with(cx, |workspace, _| {
                    ranges.iter().all(|range| {
                        let projection = model.borrow();
                        let Some((path, source)) = projection.source_range(range) else {
                            return false;
                        };
                        workspace.files.get(&path).is_some_and(|file| {
                            !file.document.conflict
                                && !file.document.mixed_newlines
                                && !workspace.git_edit.contains(&path)
                                && source.end <= file.committed.len()
                                && file.committed.is_char_boundary(source.start)
                                && file.committed.is_char_boundary(source.end)
                                && file
                                    .committed
                                    .slice(source)
                                    .chars()
                                    .eq(projection.text.slice(range.clone()).chars())
                        })
                    })
                })
                .unwrap_or(false)
            });
        });
        cx.subscribe_in(
            &self.modified.editor,
            window,
            move |panel, _, event: &InputEditsEvent, window, cx| {
                let edits = {
                    let mut projection = model.borrow_mut();
                    let mut edits: HashMap<PathBuf, Vec<(Range<usize>, String)>> = HashMap::new();
                    for (edit, text) in event.edits() {
                        let range = edit.start_byte..edit.old_end_byte;
                        let Some((path, source)) = projection.source_range(&range) else {
                            return;
                        };
                        edits.entry(path).or_default().push((
                            source,
                            text.slice(edit.start_byte..edit.new_end_byte).to_string(),
                        ));
                        projection.apply_edit(edit, text);
                    }
                    edits
                };
                if edits.is_empty() {
                    return;
                }
                // Workspace owns the file editors and their histories. A projected
                // edit enters those same editors as their normal text transaction.
                _ = workspace.update(cx, |workspace, cx| {
                    for (path, edits) in edits {
                        if let Some(file) = workspace.files.get(&path) {
                            file.editor.update(cx, |source, cx| {
                                source.apply_editor_edits(&edits, false, window, cx)
                            });
                        }
                    }
                });
                App::notify(cx, panel.review_pane.entity_id());
            },
        )
    }

    pub(super) fn clear_hover(&self, cx: &mut App) {
        for column in [&self.original, &self.modified] {
            column
                .editor
                .update(cx, |editor, cx| editor.clear_hover_state(cx));
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.row_revision = None;
        self.reset_viewport();
    }

    pub(super) fn invalidate_live_source(&mut self, path: PathBuf) {
        self.invalidate_source(path);
        self.reset_viewport();
    }

    fn reset_viewport(&mut self) {
        self.navigation_source = None;
        self.viewport.set((usize::MAX, 0, 0, px(0.)));
    }

    pub(super) fn invalidate_source(&mut self, path: PathBuf) {
        self.dirty_sources.insert(path);
    }

    pub(super) fn invalidate_actions(&mut self) {
        self.actions_valid = false;
    }

    pub(super) fn sync_snapshots(&self, review: &Review, cx: &mut App) {
        let snapshots: Rc<SnapshotEditors> = Rc::new(
            review
                .sources
                .values()
                .flatten()
                .map(|(key, editor)| (key.clone(), editor.clone()))
                .collect(),
        );
        for column in [&self.original, &self.modified] {
            column.projection.borrow_mut().snapshots = snapshots.clone();
            column.inlay_hints.refresh(cx);
        }
    }

    pub(super) fn restore_viewport(&mut self) {
        self.restore_viewport = true;
    }

    pub(super) fn change_anchor(&self, diff: &DiffState, cx: &App) -> Option<DiffViewportAnchor> {
        let anchor = diff.viewport_anchor()?;
        let Some(source) = self
            .navigation_source
            .as_ref()
            .filter(|source| source.path().as_str() == anchor.path())
        else {
            return Some(anchor);
        };
        let column = if self.split && source.side() == DiffSide::Original {
            &self.original
        } else {
            &self.modified
        };
        let editor = column.editor.read(cx);
        let viewport = editor.input_bounds();
        let projection = column.projection.borrow();
        let bounds = projection.offset(source, 0).and_then(|offset| {
            editor.projected_line_bounds(projection.text.offset_to_point(offset).row)
        });
        // Near the end, a line jump can be clamped with its target well below the header.
        // Keep that line visible at the same position when its staged and unstaged hunks regroup.
        Some(match bounds {
            Some(bounds)
                if bounds.intersects(&viewport)
                    && bounds.top()
                        > viewport.top()
                            + FILE_HEADER_HEIGHT
                            + editor.line_height().unwrap_or_default() =>
            {
                DiffViewportAnchor::new(
                    source.path().clone(),
                    Some(source.clone()),
                    viewport.top() - bounds.top(),
                    anchor.horizontal(),
                )
            }
            _ => anchor,
        })
    }

    #[cfg(test)]
    pub(super) fn original_state(&self) -> &Entity<EditorState> {
        &self.original.editor
    }

    pub(super) fn close_other_search(&self, active: &Entity<EditorState>, cx: &mut App) {
        for column in [&self.original.editor, &self.modified.editor] {
            if column != active && column.read(cx).search_session().open {
                column.update(cx, |editor, cx| editor.close_search(cx));
            }
        }
    }

    pub(super) fn focused_column(&self, window: &Window, cx: &App) -> &Entity<EditorState> {
        let original = self.original.editor.read(cx);
        if self.split
            && (original.focus_handle(cx).contains_focused(window, cx)
                || Editor::search_has_focus(&self.original.editor, window, cx))
        {
            &self.original.editor
        } else {
            &self.modified.editor
        }
    }

    fn row_at_point(&self, point: Point<Pixels>, cx: &App) -> Option<DiffLinePosition> {
        let left = self.split
            && self
                .original
                .editor
                .read(cx)
                .input_bounds()
                .contains(&point);
        let column = if left { &self.original } else { &self.modified };
        let editor = column.editor.read(cx);
        if !editor.input_bounds().contains(&point) {
            return None;
        }
        let row = editor.visible_row_range()?.find(|&row| {
            editor
                .projected_line_bounds(row)
                .is_some_and(|bounds| point.y >= bounds.top() && point.y < bounds.bottom())
        })?;
        let position = |column: &Column| {
            Some(
                column
                    .projection
                    .borrow()
                    .lines
                    .get(row)?
                    .as_ref()?
                    .position
                    .clone(),
            )
        };
        if !self.split {
            return position(column);
        }
        // Context on the left has the same text at the modified-side position.
        if !left
            || matches!(
                self.rows.get(row),
                Some(DiffEditorRow::Code { changed: false, .. })
            )
        {
            position(&self.modified).or_else(|| position(&self.original))
        } else {
            position(&self.original).or_else(|| position(&self.modified))
        }
    }

    pub(super) fn focus(&self, window: &mut Window, cx: &mut App) {
        self.modified
            .editor
            .update(cx, |editor, cx| editor.focus(window, cx));
    }

    pub(super) fn prepare(
        panel: &mut GitPanel,
        summary_height: Pixels,
        width: Pixels,
        window: &mut Window,
        cx: &mut Context<GitPanel>,
    ) {
        panel
            .diff
            .update(cx, |diff, cx| diff.prepare_editor_highlights(cx));
        if let Some(workspace) = panel.workspace.upgrade() {
            let editors = panel
                .diff
                .read(cx)
                .visible_paths()
                .filter_map(|path| {
                    panel.working_path(&DiffLinePosition::new(path.clone(), DiffSide::Modified, 1))
                })
                .filter_map(|path| {
                    workspace
                        .read(cx)
                        .files
                        .get(&path)
                        .map(|file| file.editor.clone())
                })
                .collect::<Vec<_>>();
            for editor in editors {
                editor.update(cx, |editor, cx| editor.prepare_language(window, cx));
            }
        }
        let diff = panel.diff.read(cx);
        let highlight_revision = diff.editor_highlight_revision();
        let split = diff.mode() == DiffMode::Split;
        let revision = diff.editor_row_revision();
        let (target, offset) = diff.editor_viewport_position();
        // Install only when the patch projection changed. Normal cursor and
        // scroll notifications never replace the editor document.
        let theme = cx.theme().mode;
        let hunk = HunkPalette::for_mode(theme);
        let changed = (panel.editor.row_revision != Some(revision)
            || panel.editor.theme != Some(theme)
            || panel.editor.split != split
            || !panel.editor.dirty_sources.is_empty())
            && !panel.editor.has_pending_projection(panel);
        if changed {
            if panel.editor.modified.editor.read(cx).is_composing() {
                return;
            }
            let rows = panel.diff.read(cx).editor_rows().collect::<Vec<_>>();
            let muted = cx.theme().muted.opacity(0.30);
            let surface = Palette::for_mode(cx.theme().mode).soft;
            let workspace = panel.workspace.upgrade();
            let source = |position: &DiffLinePosition, cx: &App| {
                let path = panel.working_path(position)?;
                let file = workspace.as_ref()?.read(cx).files.get(&path)?;
                Some((path, file.committed.clone()))
            };
            let snapshots = Rc::new(panel.review.as_ref().map_or_else(HashMap::new, |review| {
                review
                    .sources
                    .values()
                    .flat_map(|source| {
                        source
                            .iter()
                            .map(|(key, editor)| (key.clone(), editor.clone()))
                    })
                    .collect()
            }));
            let old = &panel.editor.rows;
            let dirty = &panel.editor.dirty_sources;
            let matches = |(a, b): (&DiffEditorRow, &DiffEditorRow)| {
                a == b
                    && match a {
                        DiffEditorRow::Code {
                            modified: Some((position, _)),
                            ..
                        } if !dirty.is_empty() => !dirty.contains(std::path::Path::new(
                            position.path().trim_start_matches('\0'),
                        )),
                        _ => true,
                    }
            };
            let reuse = panel.editor.row_revision.is_some()
                && panel.editor.theme == Some(theme)
                && panel.editor.split == split
                && panel
                    .editor
                    .modified
                    .projection
                    .borrow()
                    .row_mapping
                    .is_none()
                && panel.editor.modified.projection.borrow().lines.len() == old.len()
                && (!split || panel.editor.original.projection.borrow().lines.len() == old.len());
            let (prefix, suffix) = if reuse {
                let prefix = old
                    .iter()
                    .zip(&rows)
                    .take_while(|&(a, b)| matches((a, b)))
                    .count();
                let suffix = old[prefix..]
                    .iter()
                    .rev()
                    .zip(rows[prefix..].iter().rev())
                    .take_while(|&(a, b)| matches((a, b)))
                    .count();
                (prefix, suffix)
            } else {
                (0, 0)
            };
            let updated = &rows[prefix..rows.len() - suffix];
            for (column, left) in [
                (&panel.editor.modified, false),
                (&panel.editor.original, true),
            ] {
                if left && !split {
                    column.inlay_hints.invalidate(cx);
                    continue;
                }
                let mut text = String::new();
                let mut lines = Vec::with_capacity(updated.len());
                let mut presentation = Vec::with_capacity(updated.len());
                for row in updated {
                    let (code, color) = match row {
                        DiffEditorRow::Code {
                            original,
                            modified,
                            changed,
                        } => {
                            if left {
                                (
                                    original.as_ref(),
                                    if original.is_none() {
                                        Some(muted)
                                    } else {
                                        changed.then_some(hunk.removed_row)
                                    },
                                )
                            } else if split {
                                (
                                    modified.as_ref(),
                                    if modified.is_none() {
                                        Some(muted)
                                    } else {
                                        changed.then_some(hunk.added_row)
                                    },
                                )
                            } else {
                                (
                                    modified.as_ref().or(original.as_ref()),
                                    changed.then_some(if modified.is_some() {
                                        hunk.added_row
                                    } else {
                                        hunk.removed_row
                                    }),
                                )
                            }
                        }
                        _ => (None, Some(surface)),
                    };
                    if let Some((position, line)) = code {
                        let row = position.line().saturating_sub(1);
                        let source = (!left)
                            .then(|| source(position, cx))
                            .flatten()
                            .filter(|(_, rope)| rope.slice_line(row).chars().eq(line.chars()));
                        let bytes = source
                            .as_ref()
                            .map(|(_, rope)| rope.line_start_offset(row)..rope.line_end_offset(row))
                            .unwrap_or_default();
                        lines.push(Some(Line {
                            position: position.clone(),
                            source: source.map(|(path, _)| path),
                            bytes,
                        }));
                        text.push_str(line);
                        presentation.push((Some(position.line()), color));
                    } else {
                        lines.push(None);
                        presentation.push((None, color));
                    }
                    text.push('\n');
                }
                let old_len = column.projection.borrow().lines.len();
                let same_origins = column.install(
                    prefix..old_len - suffix,
                    lines,
                    text,
                    presentation,
                    snapshots.clone(),
                    window,
                    cx,
                );
                if same_origins {
                    column.inlay_hints.refresh(cx);
                } else {
                    column.inlay_hints.invalidate(cx);
                }
            }
            panel.editor.dirty_sources.clear();
            panel.editor.headers = Rc::new(
                rows.iter()
                    .enumerate()
                    .filter_map(|(row, item)| matches!(item, DiffEditorRow::File(_)).then_some(row))
                    .collect(),
            );
            panel.editor.hunk_neighbors = Rc::new(hunk_neighbors(&rows));
            panel.editor.rows = Rc::new(rows);
            panel.editor.row_revision = Some(revision);
            panel.editor.theme = Some(theme);
            panel.editor.split = split;
        }
        if changed || !panel.editor.actions_valid {
            let rows = &panel.editor.rows;
            let mut blocks = Vec::with_capacity(rows.len());
            let mut block = None;
            for (row, item) in rows.iter().enumerate() {
                if let DiffEditorRow::Code {
                    original,
                    modified,
                    changed: true,
                } = item
                {
                    let position = modified
                        .as_ref()
                        .or(original.as_ref())
                        .map(|(position, _)| position);
                    let action = position.and_then(|position| {
                        let number = |line: &(DiffLinePosition, SharedString)| line.0.line();
                        Some((
                            position.path(),
                            panel.action_blocks(
                                position.path(),
                                original.as_ref().map(number),
                                modified.as_ref().map(number),
                            )?,
                        ))
                    });
                    block = action.map(|(path, action)| {
                        let first = block
                            .as_ref()
                            .filter(|(previous, target, _)| previous == path && *target == action)
                            .map_or(row, |(_, _, first)| *first);
                        (path.clone(), action, first)
                    });
                } else {
                    block = None;
                }
                blocks.push(block.clone());
            }
            panel.editor.blocks = Rc::new(blocks);
            panel.editor.actions_valid = true;
        }
        let block_layout = (
            width,
            summary_height,
            window.rem_size(),
            window.scale_factor(),
            panel.active_file(cx).filter(|&ix| {
                panel
                    .review
                    .as_ref()
                    .is_some_and(|review| review.patch_display[ix].truncated.is_none())
            }),
        );
        let row_mapping = panel
            .editor
            .modified
            .projection
            .borrow()
            .row_mapping
            .clone();
        if changed || panel.editor.block_layout != Some(block_layout) || row_mapping.is_some() {
            let mut heights = Vec::new();
            let mut notices = HashMap::new();
            for (row, entry) in panel.editor.rows.iter().enumerate() {
                let height = match entry {
                    DiffEditorRow::Preamble => summary_height,
                    DiffEditorRow::File(_) => FILE_HEADER_HEIGHT,
                    DiffEditorRow::Notice(path) => {
                        let Some(label) = panel.review_notice(path, cx) else {
                            continue;
                        };
                        let height =
                            render_notice(path, label.clone(), Palette::for_mode(theme), cx)
                                .layout_as_root(
                                    size(width.max(px(1.)).into(), AvailableSpace::MinContent),
                                    window,
                                    cx,
                                )
                                .height;
                        notices.insert(row, label);
                        height
                    }
                    _ => continue,
                };
                heights.push((row, height));
            }
            let heights = Rc::new(heights);
            for (column, mapping) in [
                (&panel.editor.original, None),
                (&panel.editor.modified, row_mapping.as_deref()),
            ] {
                let heights = mapping.map_or_else(
                    || heights.clone(),
                    |mapping| {
                        Rc::new(
                            heights
                                .iter()
                                .filter_map(|&(row, height)| {
                                    Some((mapping.projected(row)?, height))
                                })
                                .collect(),
                        )
                    },
                );
                column.editor.update(cx, |editor, cx| {
                    editor.set_projected_block_heights(heights, cx)
                });
            }
            panel.editor.notices = Rc::new(notices);
            panel.editor.block_layout = Some(block_layout);
        }
        if changed || panel.editor.highlight_revision != highlight_revision {
            for column in [&panel.editor.original, &panel.editor.modified] {
                let projection = column.projection.borrow();
                let diff = panel.diff.read(cx);
                let highlights = projection
                    .lines
                    .iter()
                    .enumerate()
                    .filter_map(|(row, line)| {
                        Some((
                            projection.text.line_start_offset(row),
                            &line.as_ref()?.position,
                        ))
                    })
                    .flat_map(|(offset, position)| {
                        let color = match position.side() {
                            DiffSide::Original => hunk.removed_inline,
                            DiffSide::Modified => hunk.added_inline,
                        };
                        diff.editor_highlights(position, color).into_iter().map(
                            move |(range, style)| {
                                TextDecoration::new(
                                    (offset + range.start)..(offset + range.end),
                                    style,
                                )
                            },
                        )
                    })
                    .collect();
                column.highlights.set(highlights, cx);
            }
            panel.editor.highlight_revision = highlight_revision;
        }
        if (!split || !panel.wrap) && !panel.editor.wrap_rows.borrow().is_empty() {
            let heights = Rc::new(Vec::new());
            panel.editor.wrap_rows.replace(heights.clone());
            panel.editor.wrap_revision.set(None);
            for column in [&panel.editor.original, &panel.editor.modified] {
                column.editor.update(cx, |editor, cx| {
                    editor.set_projected_min_rows(heights.clone(), cx)
                });
            }
        }
        for column in [&panel.editor.original, &panel.editor.modified] {
            column.editor.update(cx, |editor, cx| {
                editor.set_soft_wrap(panel.wrap, window, cx)
            });
        }
        let file_row = panel.pending_file_reveal.as_deref().and_then(|path| {
            panel.editor.headers.iter().copied().find(|&row| {
                matches!(&panel.editor.rows[row], DiffEditorRow::File(header) if header.as_str() == path)
            })
        });
        if row_mapping.is_none()
            && (file_row.is_some()
                || panel.editor.restore_viewport
                || panel.editor.navigation_row != target)
        {
            let preserve_offset = std::mem::take(&mut panel.editor.restore_viewport);
            let target = file_row.unwrap_or(target);
            panel.editor.navigation_row = target;
            if !preserve_offset {
                panel.editor.navigation_source = match panel.editor.rows.get(target) {
                    Some(DiffEditorRow::Code {
                        modified, original, ..
                    }) => modified
                        .as_ref()
                        .or(original.as_ref())
                        .map(|(position, _)| position.clone()),
                    _ => None,
                };
            }
            let inset = if preserve_offset && file_row.is_none() {
                -offset
            } else if matches!(
                panel.editor.rows.get(target),
                Some(DiffEditorRow::Code { .. })
            ) {
                FILE_HEADER_HEIGHT
            } else {
                px(0.)
            };
            for column in [&panel.editor.original, &panel.editor.modified] {
                column.editor.update(cx, |editor, cx| {
                    editor.scroll_to_projected_line(target, inset, cx)
                });
            }
        }
    }

    pub(super) fn render(
        &self,
        panel: &GitPanel,
        font_size: Pixels,
        line_height: f32,
        font: Option<SharedString>,
        colors: Palette,
        hunk_actions: bool,
        header: impl Fn(&str, &mut App) -> AnyElement + 'static,
        summary: impl Fn(&mut GitPanel, &mut Context<GitPanel>) -> Option<AnyElement> + 'static,
        window: &mut Window,
        cx: &mut Context<GitPanel>,
    ) -> AnyElement {
        let focused = self.focused_column(window, cx);
        let fallback = if self.split && self.original.editor.read(cx).search_session().open {
            &self.original.editor
        } else {
            &self.modified.editor
        };
        let search_state = if focused.read(cx).search_session().open {
            focused
        } else {
            fallback
        };
        let search = Editor::search_panel(search_state, window, cx);
        let weak = cx.weak_entity();
        let focus = self.modified.editor.clone();
        let right = self.modified.editor.clone();
        let left = self.original.editor.clone();
        let rows = self.rows.clone();
        let row_mapping = self.modified.projection.borrow().row_mapping.clone();
        let hunk_neighbors = self.hunk_neighbors.clone();
        let revision = self.row_revision.unwrap_or_default();
        let navigation = panel.diff.read(cx).editor_viewport_position();
        let pending_file = panel.pending_file_reveal.clone();
        let headers = self.headers.clone();
        let blocks = self.blocks.clone();
        let mixed = panel.review.as_ref().is_some_and(|review| {
            super::review::is_all_worktree_changes(&review.detail.comparison)
        });
        let split = self.split;
        let last_scroll = self.last_scroll.clone();
        let scroll_from_left = self.scroll_from_left.clone();
        let viewport = self.viewport.clone();
        let tail_rows = self.tail_rows.clone();
        let wrap_rows = self.wrap_rows.clone();
        let wrap_revision = self.wrap_revision.clone();
        let wraps = panel.wrap;
        let notices = self.notices.clone();
        let hover = weak.clone();
        let wheel_view = weak.clone();
        let hunk_font = font.clone();
        let context_click = self.context_click.clone();
        let menu_panel = weak.clone();
        let editor = |state, readonly| {
            let panel = menu_panel.clone();
            let context_click = self.context_click.clone();
            Editor::new(state)
                .external_search_panel()
                .with_size(gpui_component::Size::XSmall)
                .text_size(font_size)
                .line_height(relative(line_height))
                .when_some(font.clone(), |editor, font| editor.font_family(font))
                .appearance(false)
                .bordered(false)
                .readonly(readonly)
                .size_full()
                .extend_context_menu(move |menu, _, cx| {
                    let Some(position) = context_click.borrow_mut().take() else {
                        return menu;
                    };
                    let can_open = panel
                        .read_with(cx, |panel, _| {
                            panel.openable_review_file(position.path()).is_some()
                        })
                        .unwrap_or(false);
                    menu.separator().menu_with_icon_disabled(
                        t!("git.open_in_editor"),
                        IconName::FileInput,
                        !can_open,
                        Box::new(OpenDiffRowInEditor(position)),
                    )
                })
        };
        let controls = canvas(
            move |bounds, window, cx| {
                let mut padding_changed = false;
                let wrap_key = (
                    left.read(cx).projected_wrap_revision(),
                    right.read(cx).projected_wrap_revision(),
                );
                if split && wraps && wrap_revision.replace(Some(wrap_key)) != Some(wrap_key) {
                    let combined = left
                        .read(cx)
                        .projected_wrap_rows_snapshot()
                        .zip(right.read(cx).projected_wrap_rows_snapshot())
                        .map(|(a, b)| a.max(b))
                        .collect::<Vec<_>>();
                    if **wrap_rows.borrow() != combined {
                        padding_changed = true;
                        let combined = Rc::new(combined);
                        wrap_rows.replace(combined.clone());
                        let (left, right) = (left.clone(), right.clone());
                        window.defer(cx, move |_, cx| {
                            for column in [left, right] {
                                column.update(cx, |editor, cx| {
                                    editor.set_projected_min_rows(combined.clone(), cx)
                                });
                            }
                        });
                    }
                }
                let a = left.read(cx).scroll_offset().y;
                let b = right.read(cx).scroll_offset().y;
                let (old_a, old_b) = last_scroll.get();
                if split && a != b {
                    // The follower can be applying the preceding frame's
                    // offset while the source advances on the same frame.
                    match (a != old_a, b != old_b) {
                        (true, false) => scroll_from_left.set(true),
                        (false, true) => scroll_from_left.set(false),
                        _ => {}
                    }
                    // The columns use different row heights until the new
                    // padding lands. Let each preserve its source anchor first.
                    if !padding_changed {
                        let (target, y) = if scroll_from_left.get() {
                            (right.clone(), a)
                        } else {
                            (left.clone(), b)
                        };
                        window.defer(cx, move |_, cx| {
                            target.update(cx, |editor, cx| editor.sync_projected_scroll_y(y, cx));
                        });
                    }
                }
                last_scroll.set((a, b));
                let right_state = right.read(cx);
                let visible = right_state.visible_row_range().unwrap_or(0..2);
                let top = right_state.projected_top_line();
                let start = row_mapping.as_ref().map_or(top, |map| map.patch_start(top)).min(rows.len());
                let end = row_mapping.as_ref().map_or(visible.end, |map| map.patch_end(visible.end, rows.len()))
                    .max(start.saturating_add(1)).min(rows.len());
                let bounds_at = |row, cx: &App| {
                    let projected = row_mapping.as_ref().map_or(Some(row), |map| map.projected(row))?;
                    right.read(cx).projected_line_bounds(projected)
                };
                // The left column still shows the published patch. Keep
                // displaced controls over the edited column until it catches up.
                let controls_left = |row| {
                    if split && row_mapping.as_ref().is_some_and(|map| map.projected(row) != Some(row)) {
                        bounds.center().x
                    } else {
                        bounds.left()
                    }
                };
                let measured_offset = bounds_at(start, cx)
                    .map(|line| right_state.input_bounds().top() - line.top());
                let offset = measured_offset.unwrap_or_default().max(px(0.));
                // Keep the editor's normal clearance when it already reaches the
                // final header. For a short tail, add only what header alignment needs.
                let extra_rows = headers.last().and_then(|&row| {
                    let line_height = right_state.line_height().filter(|h| *h > px(0.))?;
                    let header = bounds_at(row, cx)?;
                    let end = bounds_at(rows.len(), cx)?;
                    let height = right_state.input_bounds().size.height;
                    let needed = height - (end.bottom() - header.top());
                    (needed > height.half()).then(|| (needed / line_height).ceil() as usize)
                });
                if tail_rows.replace(extra_rows) != extra_rows {
                    let (left, right) = (left.clone(), right.clone());
                    let weak = weak.clone();
                    window.defer(cx, move |window, cx| {
                        for column in [left, right] {
                            column.update(cx, |editor, cx| {
                                editor.set_scroll_beyond_last_line(extra_rows, window, cx);
                            });
                        }
                        _ = weak.update(cx, |panel, cx| {
                            if panel.pending_file_reveal.is_some() {
                                App::notify(cx, panel.review_pane.entity_id());
                            }
                        });
                    });
                }
                let report = (revision, start, end, offset);
                let aligned = pending_file.as_deref().is_some_and(|path| {
                    matches!(rows.get(start), Some(DiffEditorRow::File(header)) if header.as_str() == path)
                        && measured_offset.is_some_and(|offset| offset.abs() < px(0.5))
                });
                if viewport.replace(report) != report || aligned {
                    let weak = weak.clone();
                    let pending_file = pending_file.clone();
                    window.defer(cx, move |_, cx| {
                        _ = weak.update(cx, |panel, cx| {
                            let diff = panel.diff.read(cx);
                            if diff.editor_row_revision() != revision
                                || diff.editor_viewport_position() != navigation
                                || panel.pending_file_reveal != pending_file
                            {
                                return;
                            }
                            panel.editor.navigation_row = start;
                            panel.diff.update(cx, |diff, cx| {
                                diff.set_editor_viewport_offset(start..end, offset, cx)
                            });
                            if aligned {
                                panel.pending_file_reveal = None;
                            }
                            if !panel.single_file
                                && panel.pending_file_reveal.is_none()
                                && let Some(review) = &mut panel.review
                            {
                                review.active =
                                    panel.diff.read(cx).top_file().and_then(|path| {
                                        review.by_path.get(path.as_str()).copied()
                                    });
                            }
                            App::notify(cx, panel.tree_pane.entity_id());
                        });
                    });
                }
                let mouse = window.mouse_position();
                let mut elements = Vec::new();
                let mut active_hunk = None;
                let previous = |indices: &[usize]| {
                    indices
                        .partition_point(|&row| row < start)
                        .checked_sub(1)
                        .map(|ix| indices[ix])
                };
                let mut file_header = previous(&headers).and_then(|row| {
                    if let DiffEditorRow::File(path) = &rows[row] {
                        Some((row, path))
                    } else {
                        None
                    }
                });
                // A file header is pinned as soon as its own top crosses the
                // viewport top, even while the row is still partially visible.
                // Waiting for the whole row to scroll out (`row < start`)
                // makes the sticky header pop in only after the in-flow header
                // has already disappeared.
                let mut header_stuck = file_header.is_some();
                let mut next_header_top = None;
                for (row, entry) in rows.iter().enumerate().take(end).skip(start) {
                    match entry {
                        DiffEditorRow::File(path) => {
                            if row <= start {
                                file_header = Some((row, path));
                                header_stuck = row < start;
                            } else if next_header_top.is_none() {
                                next_header_top = bounds_at(row, cx).map(|line| line.top());
                            }
                        }
                        _ => {}
                    }
                    let Some(line) = bounds_at(row, cx) else {
                        continue;
                    };
                    if matches!(entry, DiffEditorRow::File(_)) && row <= start {
                        header_stuck = line.top() < bounds.top();
                    }
                    if !bounds.intersects(&line) {
                        continue;
                    }
                    if let Some((path, hunk, origin)) = &blocks[row] {
                        if bounds.contains(&mouse)
                            && line.top() <= mouse.y
                            && mouse.y < line.bottom()
                        {
                            active_hunk = Some((path, *hunk, *origin, line.bottom()));
                        }
                        if mixed {
                            let marker = div()
                                .w(px(4.))
                                .h_full()
                                .when(hunk.staged.is_some(), |marker| {
                                    marker.border_1().border_color(colors.accent)
                                })
                                .when(hunk.unstaged.is_some(), |marker| {
                                    marker.bg(if hunk.staged.is_some() {
                                        colors.accent.opacity(0.35)
                                    } else {
                                        colors.accent
                                    })
                                })
                                .into_any_element();
                            elements.push((marker, controls_left(row), line.top(), line.size.height));
                        }
                    }
                    let chrome = match entry {
                        DiffEditorRow::Preamble => hover
                            .update(cx, |panel, cx| summary(panel, cx))
                            .ok()
                            .flatten(),
                        DiffEditorRow::File(path) => (!(row <= start && header_stuck))
                            .then(|| render_header(path, &header, colors, hover.clone(), cx)),
                        DiffEditorRow::Notice(path) => notices
                            .get(&row)
                            .map(|label| render_notice(path, label.clone(), colors, cx)),
                        DiffEditorRow::Hunk(_, _, label) => Some(
                            h_flex()
                                .w_full()
                                .h_full()
                                .px_4()
                                .gap_1()
                                .text_size(GIT_TEXT_SIZE)
                                .when_some(hunk_font.clone(), |row, font| row.font_family(font))
                                .text_color(colors.muted)
                                .child(div().flex_1().min_w_0().truncate().child(label.clone()))
                                .children(
                                    [
                                        ("git-previous-hunk", IconName::ArrowUp, "git.previous_hunk"),
                                        ("git-next-hunk", IconName::ArrowDown, "git.next_hunk"),
                                    ]
                                    .into_iter()
                                    .zip(hunk_neighbors.get(&row).copied().unwrap_or_default())
                                    .map(|((id, icon, label), target)| {
                                        let view = hover.clone();
                                        compact_icon_button((id, row), icon, t!(label))
                                            .debug_selector(|| format!("{id}:{row}"))
                                            .min_w(px(24.))
                                            .h_full()
                                            .flex_shrink_0()
                                            .disabled(target.is_none() || row_mapping.is_some())
                                            .on_click(move |_, _, cx| {
                                                if let Some(target) = target {
                                                    _ = view.update(cx, |panel, cx| {
                                                        if panel.editor.row_revision != Some(revision) {
                                                            return;
                                                        }
                                                        panel.pending_file_reveal = None;
                                                        panel.editor.navigation_source = None;
                                                        let inset = FILE_HEADER_HEIGHT + panel.editor.modified.editor.read(cx).line_height().unwrap_or_default();
                                                        for column in [&panel.editor.original, &panel.editor.modified] {
                                                            column.editor.update(cx, |editor, cx| {
                                                                editor.scroll_to_projected_line(target, inset, cx);
                                                            });
                                                        }
                                                        cx.notify();
                                                    });
                                                }
                                            })
                                    }),
                                )
                                .into_any_element(),
                        ),
                        DiffEditorRow::Fold(path, pairs) => {
                            let (path, pairs, view) = (path.clone(), pairs.clone(), hover.clone());
                            Some(
                                quiet_button(("git-editor-fold", row))
                                    .xsmall()
                                    .label(t!("git.more_context"))
                                    .on_click(move |_, _, cx| {
                                        _ = view.update(cx, |panel, cx| {
                                            panel.diff.update(cx, |diff, cx| {
                                                diff.expand_editor_fold(
                                                    path.as_str(),
                                                    pairs.clone(),
                                                    cx,
                                                )
                                            });
                                        });
                                    })
                                    .into_any_element(),
                            )
                        }
                        _ => None,
                    };
                    if let Some(chrome) = chrome {
                        elements.push((chrome, controls_left(row), line.top(), line.size.height));
                    }
                }
                if let Some((row, path)) = file_header
                    && header_stuck
                {
                    let top = next_header_top.map_or(bounds.top(), |next| {
                        bounds.top().min(next - FILE_HEADER_HEIGHT)
                    });
                    elements.push((
                        render_header(path, &header, colors, hover.clone(), cx),
                        controls_left(row),
                        top,
                        FILE_HEADER_HEIGHT,
                    ));
                }
                if hunk_actions && row_mapping.is_none()
                    && let Some((path, hunk, row, bottom)) = active_hunk
                    && let Some(line) = bounds_at(row, cx)
                {
                    let clip_top = if header_stuck {
                        next_header_top.map_or(bounds.top() + FILE_HEADER_HEIGHT, |next| {
                            next.min(bounds.top() + FILE_HEADER_HEIGHT)
                        })
                    } else {
                        bounds.top()
                    };
                    let clip =
                        Bounds::from_corners(point(bounds.left(), clip_top), bounds.bottom_right());
                    let block = Bounds::from_corners(
                        point(bounds.left(), line.top()),
                        point(bounds.right(), bottom),
                    );
                    if let Some(top) = hovered_hunk_top(block, clip, mouse)
                        && let Some(toolbar) = hover
                            .update(cx, |panel, cx| panel.hunk_actions(path.as_str(), hunk, cx))
                            .ok()
                            .flatten()
                    {
                        elements.push((hunk_toolbar_row(Some(toolbar)), bounds.left(), top, HUNK_TOOLBAR_HEIGHT));
                    }
                }
                elements
                    .into_iter()
                    .map(|(child, x, y, height)| {
                        let width = bounds.right() - x;
                        let mut element = div()
                            .w(width)
                            .h(height)
                            .child(child)
                            .into_any_element();
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            element.prepaint_as_root(
                                point(x, y),
                                size(width.into(), height.into()),
                                window,
                                cx,
                            );
                        });
                        element
                    })
                    .collect::<Vec<_>>()
            },
            |bounds, elements, window, cx| {
                window.on_mouse_event(move |event: &ScrollEvent, phase, _, cx| {
                    if phase.capture()
                        && bounds.contains(&event.position)
                        && event.delta.pixel_delta(px(1.)).y != px(0.)
                    {
                        _ = wheel_view.update(cx, |panel, cx| {
                            if panel.pending_file_reveal.take().is_none() {
                                return;
                            }
                            let (revision, start, end, offset) = panel.editor.viewport.get();
                            if panel.diff.read(cx).editor_row_revision() == revision {
                                panel.editor.navigation_row = start;
                                panel.diff.update(cx, |diff, cx| {
                                    diff.set_editor_viewport_offset(start..end, offset, cx)
                                });
                            }
                        });
                    }
                });
                window.on_mouse_event(|_: &MouseMoveEvent, phase, window, _| {
                    if phase.capture() {
                        window.refresh();
                    }
                });
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    for mut element in elements {
                        element.paint(window, cx);
                    }
                });
            },
        )
        .absolute()
        .inset_0()
        .size_full();
        let locator_open = panel
            .workspace
            .read_with(cx, |workspace, _| workspace.locator.open)
            .unwrap_or(false);
        v_flex()
            .flex_1()
            .min_h_0()
            .children(search)
            .child(
                crate::workspace::editor_scroll_viewport(&focus, colors.bg, !locator_open)
                    .id("git-diff-editor")
                    .debug_selector(|| "git-diff-editor".into())
                    .capture_any_mouse_down({
                        let panel = menu_panel.clone();
                        move |event, _, cx| {
                            if event.button == MouseButton::Right {
                                // Keep the clicked source row even if right-click moves the caret.
                                let position = panel
                                    .read_with(cx, |panel, cx| {
                                        panel.editor.row_at_point(event.position, cx)
                                    })
                                    .ok()
                                    .flatten();
                                context_click.replace(position);
                            }
                        }
                    })
                    .on_action(
                        cx.listener(|panel, action: &OpenDiffRowInEditor, window, cx| {
                            panel.open_diff_row_in_editor(&action.0, window, cx);
                        }),
                    )
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .size_full()
                            .when(split, |row| {
                                row.child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .h_full()
                                        .border_r_1()
                                        .border_color(colors.border)
                                        .child(editor(&self.original.editor, true)),
                                )
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .child(editor(&focus, false)),
                            ),
                    )
                    .child(controls),
            )
            .into_any_element()
    }
}

fn hunk_neighbors(rows: &[DiffEditorRow]) -> HashMap<usize, [Option<usize>; 2]> {
    let mut neighbors = HashMap::<_, [Option<usize>; 2]>::new();
    let mut previous = None;
    for (row, entry) in rows.iter().enumerate() {
        let DiffEditorRow::Hunk(path, _, _) = entry else {
            continue;
        };
        let path = path.as_str();
        if let Some((previous_row, previous_path)) = previous
            && previous_path == path
        {
            neighbors.entry(previous_row).or_default()[1] = Some(row);
            neighbors.entry(row).or_default()[0] = Some(previous_row);
        }
        previous = Some((row, path));
    }
    neighbors
}

fn deletion_target(document: &DiffFile, line: usize) -> Option<usize> {
    let [(old_start, old_count), (new_start, new_count)] = document
        .hunk_locations()
        .filter_map(|(header, _)| git::hunk_ranges(&header))
        .find(|ranges| {
            let (start, count) = ranges[0];
            (start..start + count).contains(&line)
        })?;
    if new_count == 0 {
        return Some(new_start + 1);
    }
    let mut cursor = new_start;
    for (side, source_line, _) in document.patch_source_lines() {
        match side {
            DiffSide::Original if source_line == line => return Some(cursor),
            DiffSide::Modified if (new_start..new_start + new_count).contains(&source_line) => {
                cursor = source_line + 1;
            }
            _ => {}
        }
    }
    // An original-side context row can outlive its projected counterpart.
    Some(new_start + (line - old_start).min(new_count.min(old_count) - 1))
}

impl Projection {
    fn apply_edit(&mut self, edit: &gpui_component::input::InputEdit, text: &Rope) {
        let range = edit.start_byte..edit.old_end_byte;
        let Some((path, bytes)) = self.source_range(&range) else {
            return;
        };
        let start = edit.start_position.row;
        let end = edit.old_end_position.row;
        let line_delta = edit.new_end_position.row as isize - end as isize;
        let delta = (edit.new_end_byte - edit.start_byte) as isize - range.len() as isize;
        let Some(line) = self.lines[start].clone() else {
            return;
        };
        let first = line.position.line();
        let view = line.position.path().clone();
        let mut source_start = line.bytes.start;
        let source_end = self.lines[end]
            .as_ref()
            .unwrap()
            .bytes
            .end
            .saturating_add_signed(delta);
        let replacement = (start..=edit.new_end_position.row)
            .map(|row| {
                let end = (source_start + text.line_len(row)).min(source_end);
                let result = Line {
                    position: DiffLinePosition::new(
                        view.clone(),
                        DiffSide::Modified,
                        first + row - start,
                    ),
                    source: Some(path.clone()),
                    bytes: source_start..end,
                };
                source_start = end.saturating_add(1);
                Some(result)
            })
            .collect::<Vec<_>>();
        self.edited = true;
        if line_delta != 0 || start != end {
            let mapping = self.row_mapping.get_or_insert_with(|| {
                Rc::new(RowMapping(
                    (0..=self.lines.len()).map(|row| (row, row)).collect(),
                ))
            });
            Rc::make_mut(mapping).0.retain_mut(|(_, row)| {
                if *row > end {
                    *row = row.saturating_add_signed(line_delta);
                } else if *row > start {
                    return false;
                }
                true
            });
        }
        self.lines.splice(start..=end, replacement);
        for line in self
            .lines
            .iter_mut()
            .skip(edit.new_end_position.row + 1)
            .flatten()
        {
            if line.source.as_ref() != Some(&path) || line.bytes.start < bytes.end {
                continue;
            }
            line.bytes.start = line.bytes.start.saturating_add_signed(delta);
            line.bytes.end = line.bytes.end.saturating_add_signed(delta);
            line.position = DiffLinePosition::new(
                line.position.path().clone(),
                DiffSide::Modified,
                line.position.line().saturating_add_signed(line_delta),
            );
        }
        self.text = text.clone();
    }
}

fn render_notice(path: &str, label: SharedString, colors: Palette, cx: &App) -> AnyElement {
    div()
        .debug_selector(|| format!("git-editor-notice:{path}"))
        .w_full()
        .min_w_0()
        .px_4()
        .py_3()
        .text_size(px(text_base_size(cx)))
        .line_height(relative(1.5))
        .text_color(colors.muted)
        .whitespace_normal()
        .bg(colors.soft)
        .child(label)
        .into_any_element()
}

fn render_header(
    path: &SharedString,
    render: &impl Fn(&str, &mut App) -> AnyElement,
    colors: Palette,
    panel: WeakEntity<GitPanel>,
    cx: &mut App,
) -> AnyElement {
    let name = path.clone();
    h_flex()
        .id((ElementId::from("git-editor-file"), path.clone()))
        .debug_selector(|| format!("git-editor-file-header:{path}"))
        .w_full()
        .h_full()
        .px_2()
        .bg(colors.elevated)
        .child(
            compact_icon_button(
                "git-editor-disclosure",
                IconName::ChevronDown,
                t!("git.more_context"),
            )
            .size(px(20.))
            .icon_size(px(15.))
            .debug_selector(|| "git-editor-disclosure".into())
            .on_click(move |_, _, cx| {
                _ = panel.update(cx, |panel, cx| {
                    panel
                        .diff
                        .update(cx, |diff, cx| diff.toggle_editor_file(name.as_str(), cx));
                });
            }),
        )
        .child(render(path.as_str(), cx))
        .into_any_element()
}

impl GitPanel {
    pub(in crate::workspace) fn schedule_snapshot_alignment(&self, cx: &mut Context<Self>) {
        if !self.review.as_ref().is_some_and(|review| {
            super::review::aligns_historical_snapshots(&review.detail.comparison)
        }) {
            return;
        }
        let panel = cx.weak_entity();
        cx.defer(move |cx| {
            _ = panel.update(cx, |panel, cx| panel.align_visible_snapshots(cx));
        });
    }

    fn align_visible_snapshots(&mut self, cx: &mut Context<Self>) {
        if self.editor.alignment_task.is_some() || self.detail_loading || self.snapshot.is_some() {
            return;
        }
        let Some(review) = &self.review else { return };
        if !super::review::aligns_historical_snapshots(&review.detail.comparison) {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let owner = workspace.read(cx);
        if owner.view != WorkspaceView::Git {
            return;
        }
        let next = self.diff.read(cx).visible_paths().find_map(|path| {
            let snapshots = review.sources.get(review.by_path.get(path.as_str())?)?;
            snapshots.values().find_map(|snapshot| {
                let path = snapshot.worktree_path.as_ref()?;
                let file = owner.files.get(path)?;
                file.language_server.as_ref()?;
                let version = WorktreeVersion::of(file, cx)?;
                if snapshot
                    .alignment
                    .borrow()
                    .as_ref()
                    .is_some_and(|cached| cached.version == version)
                {
                    return None;
                }
                let saved = file.saved.clone();
                let committed = file.committed.clone();
                let current = file.editor.read(cx).text().clone();
                let original = snapshot.editor.read(cx).text().clone();
                Some((
                    snapshot.clone(),
                    path.clone(),
                    version,
                    original,
                    saved,
                    current,
                    committed,
                ))
            })
        });
        let Some((snapshot, path, version, original, saved, current, committed)) = next else {
            return;
        };
        // One background comparison at a time. After it finishes, take the
        // next file from the current viewport instead of queuing work while scrolling.
        let alignment = cx.background_spawn(async move {
            (saved == committed && saved == current)
                .then(|| LineAlignment::between(&original, &saved))
                .flatten()
        });
        self.editor.alignment_task = Some(cx.spawn(async move |panel, cx| {
            let lines = alignment.await.map(Rc::new);
            _ = panel.update(cx, |panel, cx| {
                panel.editor.alignment_task = None;
                snapshot
                    .alignment
                    .replace(Some(SnapshotAlignment { version, lines }));
                panel.invalidate_source_inlay_hints(&path, cx);
                panel.align_visible_snapshots(cx);
            });
        }));
    }

    pub(in crate::workspace) fn set_inlay_hints_enabled(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor.modified.inlay_hints.enabled() == enabled {
            return;
        }
        for column in [&self.editor.original, &self.editor.modified] {
            column.inlay_hints.set_enabled(enabled, cx);
        }
        if enabled {
            self.preload_working_buffers(window, cx);
        }
    }

    pub(in crate::workspace) fn invalidate_source_inlay_hints(&self, path: &Path, cx: &mut App) {
        self.reset_source_inlay_hints(path, None, cx);
    }

    pub(in crate::workspace) fn refresh_edited_source_inlay_hints(
        &self,
        path: &Path,
        text: &Rope,
        cx: &mut App,
    ) {
        self.reset_source_inlay_hints(path, Some(text), cx);
    }

    fn reset_source_inlay_hints(&self, path: &Path, edited_text: Option<&Rope>, cx: &mut App) {
        for column in [&self.editor.original, &self.editor.modified] {
            let hints = &column.inlay_hints;
            let mut should_clear = edited_text.is_none();
            if hints.covers(|ranges| {
                let projection = column.projection.borrow();
                let mut tracks_source = false;
                for range in ranges {
                    let end = range.end.min(projection.lines.len());
                    for view_row in range.start..end {
                        let Some(line) = projection.lines[view_row].as_ref() else {
                            continue;
                        };
                        if projection.hint_path(self, &line.position) != Some(path) {
                            continue;
                        }
                        tracks_source = true;
                        if edited_text.is_some_and(|text| {
                            let source_row = line.position.line() - 1;
                            line.source.as_deref() != Some(path)
                                || source_row >= text.lines_len()
                                || text.slice_line(source_row)
                                    != projection.text.slice_line(view_row)
                        }) {
                            should_clear = true;
                        }
                    }
                }
                tracks_source
            }) {
                if should_clear {
                    hints.invalidate(cx);
                } else {
                    hints.refresh(cx);
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn review_source_bounds(
        &self,
        position: &DiffLinePosition,
        cx: &App,
    ) -> Option<Bounds<Pixels>> {
        let column = if self.editor.split && position.side() == DiffSide::Original {
            &self.editor.original
        } else {
            &self.editor.modified
        };
        let projection = column.projection.borrow();
        let offset = projection.offset(position, 0)?;
        column
            .editor
            .read(cx)
            .projected_line_bounds(projection.text.offset_to_point(offset).row)
    }

    pub(super) fn diff_source(&self, position: &DiffLinePosition, cx: &App) -> Option<DiffSource> {
        let review = self.review.as_ref()?;
        let ix = *review.by_path.get(position.path().as_str())?;
        let document = &review.documents[ix];
        let deletion = (position.side() == DiffSide::Original
            && document.modified_path().is_some())
        .then(|| deletion_target(document, position.line()))
        .flatten();
        let target = deletion.map_or_else(
            || position.clone(),
            |line| DiffLinePosition::new(position.path().clone(), DiffSide::Modified, line),
        );
        let snapshot = if self.working_path(&target).is_some() {
            None
        } else if let Some(source) = review
            .sources
            .get(&ix)
            .and_then(|sources| sources.get(&(target.path().clone(), target.side())))
        {
            Some(Task::ready(Some(source.editor.read(cx).text().clone())))
        } else {
            // The tree can request navigation before the review has loaded its language assets.
            let backend = self.backend.clone();
            let detail = review.detail.clone();
            let layer = self.document_layer();
            let side = if target.side() == DiffSide::Modified {
                Side::New
            } else {
                Side::Old
            };
            Some(cx.background_spawn(async move {
                let file = &detail.files[ix];
                let snapshot = if let Some(layer) = layer {
                    backend.layer_snapshot(&file.path, layer, side)
                } else {
                    backend.snapshot(&detail, file, side)
                };
                match snapshot.ok()? {
                    git::Snapshot::Text {
                        text,
                        truncated: false,
                    } => Some(Rope::from(
                        text.trim_start_matches('\u{feff}')
                            .replace("\r\n", "\n")
                            .as_str(),
                    )),
                    _ => None,
                }
            }))
        };
        Some(DiffSource {
            row: target.line() - 1,
            snapshot,
        })
    }

    fn working_path(&self, position: &DiffLinePosition) -> Option<PathBuf> {
        let (review, ix, file) = self.worktree_source_file(position)?;
        (super::review::has_live_target(&review.detail.comparison)
            && (!super::review::is_custom_worktree(&review.detail.comparison)
                || review.versions.contains_key(&ix))
            && (self.document_layer() != Some(git::Layer::Staged)
                || file
                    .worktree
                    .as_ref()
                    .is_some_and(|status| !status.unstaged && !status.untracked)))
        .then(|| PathBuf::from(&file.path))
    }

    fn worktree_hint_path(&self, position: &DiffLinePosition) -> Option<&str> {
        let (review, _, file) = self.worktree_source_file(position)?;
        if !super::review::has_live_target(&review.detail.comparison) {
            return None;
        }
        (self.document_layer() != Some(git::Layer::Staged)).then_some(&file.path)
    }

    fn worktree_source_file(
        &self,
        position: &DiffLinePosition,
    ) -> Option<(&Review, usize, &git::ChangedFile)> {
        if position.side() != DiffSide::Modified {
            return None;
        }
        let review = self.review.as_ref()?;
        let ix = *review.by_path.get(position.path().as_str())?;
        let file = &review.detail.files[ix];
        let needs_version = self.backend.supports_git_edit()
            && matches!(
                review.detail.comparison,
                Comparison::Worktree { base: None, .. }
            );
        (review.openable[ix]
            && (!needs_version || review.versions.contains_key(&ix))
            && !super::review::change_stat(&review.stats, file).binary
            && !review.documents[ix].is_binary()
            && !file.submodule()
            && !file.conflict()
            && matches!(review.patches[ix], PatchState::Ready)
            && review.patch_display[ix].truncated == Some(false))
        .then_some((review, ix, file))
    }

    pub(in crate::workspace) fn editing_path(&self, window: &Window, cx: &App) -> Option<PathBuf> {
        let editor = self.editor.modified.editor.read(cx);
        if self.snapshot.is_some() || !editor.focus_handle(cx).contains_focused(window, cx) {
            return None;
        }
        let offset = editor.cursor();
        self.editor
            .modified
            .projection
            .borrow()
            .source_range(&(offset..offset))
            .map(|(path, _)| path)
    }

    fn replay_review_history(&mut self, undo: bool, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.editing_path(window, cx);
        if let Some(path) = path {
            _ = self.workspace.update(cx, |workspace, cx| {
                if workspace.git_edit.contains(&path) {
                    return;
                }
                if let Some(file) = workspace.files.get(&path) {
                    file.editor.update(cx, |editor, cx| {
                        editor.replay_text_history(undo, window, cx)
                    });
                }
            });
        } else {
            self.replay_worktree_history(undo, window, cx);
        }
    }

    pub(super) fn preload_working_buffers(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = &self.review else { return };
        let paths = self
            .diff
            .read(cx)
            .visible_paths()
            .flat_map(|path| {
                let position = DiffLinePosition::new(path.clone(), DiffSide::Modified, 1);
                let working = self.working_path(&position).or_else(|| {
                    self.editor
                        .modified
                        .inlay_hints
                        .enabled()
                        .then(|| self.worktree_hint_path(&position))
                        .flatten()
                        .map(PathBuf::from)
                });
                let snapshots = review
                    .by_path
                    .get(path.as_str())
                    .and_then(|ix| review.sources.get(ix))
                    .into_iter()
                    .flat_map(|sources| sources.values())
                    .filter_map(|snapshot| snapshot.worktree_path.clone());
                working.into_iter().chain(snapshots)
            })
            .collect::<HashSet<_>>();
        let workspace = self.workspace.clone();
        let panel = cx.weak_entity();
        window.defer(cx, move |window, cx| {
            _ = workspace.update(cx, |workspace, cx| {
                for path in paths {
                    if let Some(file) = workspace.files.get(&path) {
                        file.editor
                            .update(cx, |editor, cx| editor.prepare_language(window, cx));
                        continue;
                    }
                    let panel = panel.clone();
                    let source = path.clone();
                    workspace.ensure_review_buffer(
                        path,
                        window,
                        cx,
                        move |workspace, result, window, cx| {
                            if result.is_err() {
                                return;
                            }
                            let file = &workspace.files[&source];
                            file.editor
                                .update(cx, |editor, cx| editor.prepare_language(window, cx));
                            let text = file.committed.clone();
                            let dirty = text != file.saved;
                            window.defer(cx, move |window, cx| {
                                _ = panel.update(cx, |panel, cx| {
                                    panel.editor.invalidate_source(source.clone());
                                    if dirty {
                                        panel.buffer_changed(&source, text, false, window, cx);
                                    }
                                    panel.align_visible_snapshots(cx);
                                    App::notify(cx, panel.review_pane.entity_id());
                                });
                            });
                        },
                    );
                }
            });
            _ = panel.update(cx, |panel, cx| panel.align_visible_snapshots(cx));
        });
    }
}
