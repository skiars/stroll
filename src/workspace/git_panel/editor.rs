use super::{
    review::{ActionBlocks, HunkTarget},
    *,
};
use crate::workspace::{
    DiffSource, OpenFile, WorkspaceView,
    code_theme::HunkPalette,
    editor_git::{HunkDirection, hunk_label, hunk_toolbar_row, restore_text_edit},
    image_preview::ImageHoverDismiss,
    inlay_hints::InlayHints,
    line_alignment::LineAlignment,
};
use gpui_component::{
    diff::{DiffEditorRow, DiffSide},
    input::{
        DefinitionProvider, Editor, HUNK_TOOLBAR_HEIGHT, HoverProvider, InlayHint, InputEdit,
        InputEditsEvent, Rope, RopeExt, TextDecoration, TextDecorationCollection, hovered_hunk_top,
    },
    native_menu::NativeMenu,
};
use similar::{DiffTag, TextDiff};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    ops::Range,
    path::Path,
    sync::OnceLock,
    time::Instant,
};

const FILE_HEADER_HEIGHT: Pixels = px(32.);

struct FileHeaderPosition {
    top: Pixels,
    viewport_top: Pixels,
    next_top: Option<Pixels>,
}

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

    pub(super) fn editor(&self) -> &Entity<EditorState> {
        &self.editor
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

    fn projected_rows(&self, rows: Range<usize>) -> Option<Range<usize>> {
        let start = self.0.partition_point(|&(row, _)| row < rows.start);
        let end = self.0.partition_point(|&(row, _)| row < rows.end);
        let entries = &self.0[start..end];
        Some(entries.first()?.1..entries.last()?.1 + 1)
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

    fn source_offset(&self, path: &Path, byte: usize) -> Option<usize> {
        self.lines.iter().enumerate().find_map(|(row, line)| {
            let line = line.as_ref()?;
            (line.source.as_deref() == Some(path)
                && (line.bytes.start..=line.bytes.end).contains(&byte))
            .then(|| self.text.line_start_offset(row) + byte - line.bytes.start)
        })
    }

    fn decoration_offset(&self, near: usize) -> usize {
        let row = self.text.offset_to_point(near.min(self.text.len())).row;
        // The trailing projected row is also a decoration.
        let row = (0..=self.lines.len())
            .filter(|&row| {
                self.lines
                    .get(row)
                    .and_then(Option::as_ref)
                    .is_none_or(|line| line.source.is_none())
            })
            .min_by_key(|&candidate| row.abs_diff(candidate))
            .unwrap_or(self.lines.len());
        self.text.line_start_offset(row)
    }

    fn source_rows_match(&self, range: &Range<usize>, source: &Rope) -> bool {
        let start = self.text.offset_to_point(range.start).row;
        let end = self.text.offset_to_point(range.end).row;
        (start..=end).all(|row| {
            self.lines[row].as_ref().is_some_and(|line| {
                let source_row = line.position.line() - 1;
                source_row < source.lines_len()
                    && line.bytes.start == source.line_start_offset(source_row)
                    && self.text.slice_line(row) == source.slice_line(source_row)
            })
        })
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
    syntax_override: Option<SharedString>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<EditorState> {
    cx.new(|cx| {
        let mut editor = EditorState::new(window, cx)
            .language(syntax_override.unwrap_or_else(|| language.into()))
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
    hidden_selections: RefCell<Option<HiddenSelections>>,
}

#[derive(Clone)]
struct SelectionEndpoint {
    offset: usize,
    source: Option<(PathBuf, usize)>,
    position: Option<(DiffLinePosition, usize)>,
}

type ColumnSelections = Vec<[SelectionEndpoint; 2]>;

struct HiddenSelections {
    projected: Vec<(usize, usize)>,
    logical: ColumnSelections,
}

impl HiddenSelections {
    fn matches(&self, editor: &EditorState) -> bool {
        editor
            .selection_endpoints()
            .eq(self.projected.iter().copied())
    }
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
            hidden_selections: RefCell::default(),
        }
    }

    fn capture_selections(
        &self,
        projection: &Projection,
        editor: &EditorState,
    ) -> ColumnSelections {
        let mut hidden = self.hidden_selections.borrow_mut();
        if let Some(saved) = hidden.as_ref() {
            if saved.matches(editor) {
                return saved.logical.clone();
            }
            *hidden = None;
        }
        let endpoint = |offset| SelectionEndpoint {
            offset,
            source: projection
                .source_range(&(offset..offset))
                .map(|(path, bytes)| (path, bytes.start)),
            position: projection.position(offset),
        };
        editor
            .selection_endpoints()
            .map(|(anchor, head)| [endpoint(anchor), endpoint(head)])
            .collect()
    }

    fn restore_selections(
        &self,
        selections: ColumnSelections,
        editor: &mut EditorState,
        cx: &mut Context<EditorState>,
    ) {
        let projection = self.projection.borrow();
        let mut hidden = false;
        let mut map = |endpoint: &SelectionEndpoint| {
            if let Some((path, byte)) = &endpoint.source {
                return projection.source_offset(path, *byte).unwrap_or_else(|| {
                    hidden = true;
                    // A hidden source anchor must not become an editable
                    // position in an unrelated row.
                    projection.decoration_offset(endpoint.offset)
                });
            }
            endpoint
                .position
                .as_ref()
                .and_then(|(position, col)| projection.offset(position, *col))
                .unwrap_or(endpoint.offset.min(projection.text.len()))
        };
        let projected = selections
            .iter()
            .map(|[anchor, head]| (map(anchor), map(head)))
            .collect::<Vec<_>>();
        if !editor.selection_endpoints().eq(projected.iter().copied()) {
            editor.restore_projected_selections(&projected, cx);
        }
        *self.hidden_selections.borrow_mut() = hidden.then(|| HiddenSelections {
            projected: editor.selection_endpoints().collect(),
            logical: selections,
        });
    }

    fn history_target(&self, editor: &EditorState) -> Option<PathBuf> {
        let hidden = self.hidden_selections.borrow();
        if let Some(saved) = hidden.as_ref().filter(|saved| saved.matches(editor)) {
            return saved.logical[0][1]
                .source
                .as_ref()
                .map(|(path, _)| path.clone());
        }
        let offset = editor.cursor();
        self.projection
            .borrow()
            .source_range(&(offset..offset))
            .map(|(path, _)| path)
    }

    fn replay_source_edit(
        &self,
        path: &Path,
        edit: &InputEdit,
        before: &Rope,
        source: &Rope,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut projection = self.projection.borrow_mut();
        let mut selections = self.capture_selections(&projection, self.editor.read(cx));
        let mut endpoints = selections
            .iter_mut()
            .flatten()
            .filter_map(|endpoint| {
                let (file, byte) = endpoint.source.as_mut()?;
                (file == path).then_some(byte)
            })
            .collect::<Vec<_>>();
        let affected = edit.start_byte..edit.old_end_byte;
        let refine = endpoints.iter().any(|byte| affected.contains(&**byte));
        let changes = replay_changes(edit, before, source, refine);
        let mut selected_rows = HashSet::new();
        for byte in &mut endpoints {
            **byte = translate_source_offset(**byte, &changes);
            selected_rows.insert(source.offset_to_point(**byte).row);
        }
        let replacements = projection.replay_source_edit(path, edit, before, source, selected_rows);
        drop(projection);
        self.editor.update(cx, |editor, cx| {
            let scroll = editor.scroll_offset();
            let changed = !replacements.is_empty();
            for (range, text) in replacements {
                editor.replace_projected_text(range, text, window, cx);
            }
            self.restore_selections(selections, editor, cx);
            if changed {
                editor.set_scroll_offset(scroll, cx);
            }
        });
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
        let selections = self.capture_selections(&projection, editor);
        let scroll = editor.scroll_offset();
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
        drop(projection);
        self.editor.update(cx, |editor, cx| {
            if changed {
                editor.replace_projected_text(range, text, window, cx);
            }
            self.restore_selections(selections, editor, cx);
            if changed {
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
    hunks: Rc<Vec<ReviewHunk>>,
    revealed_hunk: Rc<Cell<Option<RevealedHunk>>>,
    blocks: Rc<Vec<Option<(SharedString, ActionBlocks, usize)>>>,
    actions_valid: bool,
    dirty_sources: HashSet<PathBuf>,
    split: bool,
    last_scroll: Rc<std::cell::Cell<(Pixels, Pixels)>>,
    scroll_from_left: Rc<std::cell::Cell<bool>>,
    viewport: Rc<std::cell::Cell<(usize, usize, usize, Pixels)>>,
    navigation_row: usize,
    navigation_source: Option<DiffLinePosition>,
    pub(super) navigating_file: Option<String>,
    pub(super) image_hover: ImageHoverDismiss,
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

struct ReviewHunk {
    path: SharedString,
    index: usize,
    header: usize,
    changes: Range<usize>,
}

#[derive(Clone, Copy)]
struct RevealedHunk {
    revision: usize,
    header: usize,
    pointer: Point<Pixels>,
}

impl ReviewHunk {
    fn in_direction(hunks: &[Self], selected: usize, direction: HunkDirection) -> Option<&Self> {
        hunks
            .get(direction.neighbor(selected)?)
            .filter(|target| target.path == hunks[selected].path)
    }

    fn projected_changes(&self, mapping: Option<&RowMapping>) -> Option<Range<usize>> {
        if let Some(mapping) = mapping {
            mapping.projected_rows(self.changes.clone()).or_else(|| {
                let header = mapping.projected(self.header)?;
                Some(header..header + 1)
            })
        } else {
            Some(self.changes.clone())
        }
    }
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
            hunks: Rc::default(),
            revealed_hunk: Rc::default(),
            blocks: Rc::default(),
            actions_valid: false,
            dirty_sources: HashSet::new(),
            split: false,
            last_scroll: Rc::default(),
            scroll_from_left: Rc::default(),
            viewport: Rc::default(),
            navigation_row: 0,
            navigation_source: None,
            navigating_file: None,
            image_hover: ImageHoverDismiss::default(),
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
                                && projection.source_rows_match(range, &file.committed)
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

    pub(super) fn clear_hidden_selections(&self) {
        self.modified.hidden_selections.borrow_mut().take();
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

    fn scroll_to_rows(
        &self,
        rows: [usize; 2],
        inset: Pixels,
        animate: bool,
        window: &Window,
        cx: &mut App,
    ) -> bool {
        let driver = self.focused_column(window, cx);
        let animate = animate
            && cx.smooth_scrolling()
            && !cx.reduce_motion()
            && driver.read(cx).line_height().is_some();
        for (column, row) in [(&self.original, rows[0]), (&self.modified, rows[1])] {
            if !animate || &column.editor == driver {
                column.editor.update(cx, |editor, cx| {
                    editor.scroll_to_projected_line(row, inset, cx);
                    if animate {
                        editor.animate_pending_scroll();
                    }
                });
            }
        }
        animate
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
            panel.editor.hunks = Rc::new(review_hunks(&rows));
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
                    DiffEditorRow::File(path) => {
                        FILE_HEADER_HEIGHT
                            + if panel.has_image_diff(path) {
                                super::review::IMAGE_DIFF_HEIGHT
                            } else {
                                px(0.)
                            }
                    }
                    DiffEditorRow::Notice(path) => {
                        let Some(label) = panel.review_notice(path, cx) else {
                            if panel.has_image_diff(path) {
                                // The thumbnail block already represents this file's notice.
                                heights.push((row, px(0.)));
                            }
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
        if panel.editor.row_revision == Some(revision)
            && let Some(jump) = panel
                .pending_hunk_navigation
                .take_if(|jump| !matches!(jump.target, HunkTarget::Load(_)))
        {
            let hunks = &panel.editor.hunks;
            let hunk = match jump.target {
                HunkTarget::Index(index) => hunks
                    .iter()
                    .find(|hunk| hunk.path == jump.path && hunk.index == index),
                HunkTarget::FileEdge(HunkDirection::Next) => {
                    hunks.iter().find(|hunk| hunk.path == jump.path)
                }
                HunkTarget::FileEdge(HunkDirection::Previous) => {
                    hunks.iter().rfind(|hunk| hunk.path == jump.path)
                }
                HunkTarget::Load(_) => None,
            };
            if let Some(hunk) = hunk
                && let Some(projected) = hunk.projected_changes(row_mapping.as_deref())
            {
                let right = &panel.editor.modified.editor;
                let inset = jump
                    .anchor
                    .inset(right.read(cx).input_bounds(), FILE_HEADER_HEIGHT)
                    .unwrap_or(FILE_HEADER_HEIGHT);
                let row = hunk.changes.start;
                panel
                    .editor
                    .revealed_hunk
                    .set(jump.anchor.pointer().map(|pointer| RevealedHunk {
                        revision,
                        header: hunk.header,
                        pointer,
                    }));
                panel.editor.navigation_row = target;
                panel.editor.navigation_source = None;
                panel.editor.restore_viewport = false;
                panel
                    .editor
                    .scroll_to_rows([row, projected.start], inset, true, window, cx);
                return;
            }
        }
        let file_row = panel.pending_file_reveal.as_deref().and_then(|path| {
            panel.editor.headers.iter().copied().find(|&row| {
                matches!(&panel.editor.rows[row], DiffEditorRow::File(header) if header.as_str() == path)
            })
        });
        if panel.editor.navigating_file.is_some()
            && panel.editor.navigating_file == panel.pending_file_reveal
        {
            return;
        }
        if row_mapping.is_none()
            && (file_row.is_some()
                || panel.editor.restore_viewport
                || panel.editor.navigation_row != target)
        {
            let explicit = panel.diff.read(cx).has_projected_navigation();
            if explicit {
                panel
                    .diff
                    .update(cx, |diff, _| diff.clear_projected_navigation());
            }
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
            let animate =
                !preserve_offset && !panel.single_file && (explicit || file_row.is_some());
            if panel
                .editor
                .scroll_to_rows([target; 2], inset, animate, window, cx)
            {
                panel.editor.navigating_file = panel.pending_file_reveal.clone();
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
        header: impl Fn(&str, f32, &mut App) -> AnyElement + 'static,
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
        let hunks = self.hunks.clone();
        let revealed_hunk = self.revealed_hunk.clone();
        let revision = self.row_revision.unwrap_or_default();
        let navigation = panel.diff.read(cx).editor_viewport_position();
        let pending_file = panel.pending_file_reveal.clone();
        let navigating_file =
            panel.editor.navigating_file.is_some() && panel.editor.navigating_file == pending_file;
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
        let image_hover = self.image_hover.clone();
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
                    NativeMenu::new()
                        .menu_with_icon_disabled(
                            t!("git.open_in_editor"),
                            IconName::FileInput,
                            !can_open,
                            Box::new(OpenDiffRowInEditor(position)),
                        )
                        .separator()
                        .append(menu)
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
                let left_navigating = left.read(cx).is_scroll_navigating();
                let right_navigating = right.read(cx).is_scroll_navigating();
                let (old_a, old_b) = last_scroll.get();
                if split && a != b {
                    // The follower can be applying the preceding frame's
                    // offset while the source advances on the same frame.
                    match (left_navigating, right_navigating) {
                        (true, false) => scroll_from_left.set(true),
                        (false, true) => scroll_from_left.set(false),
                        _ => match (a != old_a, b != old_b) {
                            (true, false) => scroll_from_left.set(true),
                            (false, true) => scroll_from_left.set(false),
                            _ => {}
                        },
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
                let navigating = left_navigating || right_navigating;
                if !navigating && (viewport.replace(report) != report || aligned || navigating_file) {
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
                            if aligned || navigating_file {
                                panel.pending_file_reveal = None;
                                panel.editor.navigating_file = None;
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
                let mut file_headers = Vec::new();
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
                        DiffEditorRow::File(path) => {
                            if !(row <= start && header_stuck) {
                                file_headers.push((
                                    render_header(
                                        path,
                                        &header,
                                        colors,
                                        hover.clone(),
                                        FileHeaderPosition {
                                            top: line.top(),
                                            viewport_top: bounds.top(),
                                            next_top: None,
                                        },
                                        cx,
                                    ),
                                    controls_left(row),
                                    line.top(),
                                    FILE_HEADER_HEIGHT,
                                ));
                            }
                            if let Ok(Some(images)) = hover.read_with(cx, |panel, _| panel.image_diff_row(path, colors)) {
                                elements.push((images, controls_left(row), line.top() + FILE_HEADER_HEIGHT, super::review::IMAGE_DIFF_HEIGHT));
                            }
                            None
                        }
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
                // Paint the header shadows above the hunk and notice rows they border.
                elements.append(&mut file_headers);
                if let Some((row, path)) = file_header
                    && header_stuck
                {
                    let top = next_header_top.map_or(bounds.top(), |next| {
                        bounds.top().min(next - FILE_HEADER_HEIGHT)
                    });
                    elements.push((
                        render_header(
                            path,
                            &header,
                            colors,
                            hover.clone(),
                            FileHeaderPosition {
                                top,
                                viewport_top: bounds.top(),
                                next_top: next_header_top,
                            },
                            cx,
                        ),
                        controls_left(row),
                        top,
                        FILE_HEADER_HEIGHT,
                    ));
                }
                // Keep the destination label visible if scroll limits keep it off the click.
                let revealed = revealed_hunk
                    .get()
                    .filter(|hunk| hunk.revision == revision && hunk.pointer == mouse);
                revealed_hunk.set(revealed);
                if bounds.contains(&mouse) {
                    let clip_top = if header_stuck {
                        next_header_top.map_or(bounds.top() + FILE_HEADER_HEIGHT, |next| {
                            next.min(bounds.top() + FILE_HEADER_HEIGHT)
                        })
                    } else {
                        bounds.top()
                    };
                    let clip =
                        Bounds::from_corners(point(bounds.left(), clip_top), bounds.bottom_right());
                    let placement = |hunk: &ReviewHunk, revealed: bool, cx: &App| {
                        let rows = hunk.projected_changes(row_mapping.as_deref())?;
                        let right_state = right.read(cx);
                        let start = right_state.projected_line_bounds(rows.start)?.top();
                        let bottom = right_state.projected_line_bounds(rows.end - 1)?.bottom();
                        if bottom <= clip.top() || start >= clip.bottom() {
                            return None;
                        }
                        let top = start.max(clip.top());
                        if revealed {
                            return Some(top.min((clip.bottom() - HUNK_TOOLBAR_HEIGHT).max(clip.top())));
                        }
                        let block = Bounds::from_corners(
                            point(bounds.left(), start),
                            point(bounds.right(), bottom.max(top + HUNK_TOOLBAR_HEIGHT)),
                        );
                        hovered_hunk_top(block, clip, mouse)
                    };
                    let revealed = revealed.and_then(|revealed| {
                        let selected = hunks.binary_search_by_key(&revealed.header, |hunk| hunk.header).ok()?;
                        Some((selected, placement(&hunks[selected], true, cx)?))
                    });
                    let hovered = revealed.or_else(|| {
                        let first = hunks.partition_point(|hunk| hunk.changes.end <= start);
                        hunks.iter().enumerate().skip(first)
                            .take_while(|(_, hunk)| hunk.changes.start < end)
                            .find_map(|(selected, hunk)| Some((selected, placement(hunk, false, cx)?)))
                    });
                    if let Some((selected, top)) = hovered {
                        let hunk = &hunks[selected];
                        let mut top = top;
                        let action = if hunk_actions && row_mapping.is_none() {
                            let first = blocks[hunk.changes.start].as_ref();
                            if revealed.is_some() {
                                first.map(|(path, blocks, _)| (path, *blocks))
                            } else if let Some((path, blocks, row, bottom)) = active_hunk
                                && hunk.changes.contains(&row)
                                && let Some(line) = bounds_at(row, cx)
                            {
                                let block = Bounds::from_corners(
                                    point(bounds.left(), line.top()),
                                    point(bounds.right(), bottom),
                                );
                                top = hovered_hunk_top(block, clip, mouse).unwrap_or(top);
                                Some((path, blocks))
                            } else if mouse.y < top + HUNK_TOOLBAR_HEIGHT {
                                first.map(|(path, blocks, _)| (path, *blocks))
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        if let Ok(Some(label)) = hover.update(cx, |panel, cx| {
                            panel.review_hunk_label(selected, action, font_size, cx)
                        }) {
                            elements.push((hunk_toolbar_row(label), bounds.left(), top, HUNK_TOOLBAR_HEIGHT));
                        }
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
                window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
                    if !phase.capture() || !bounds.contains(&event.position) {
                        return;
                    }
                    let delta = event.delta.pixel_delta(px(1.));
                    if delta != Point::default() {
                        image_hover.dismiss(window, cx);
                    }
                    if delta.y != px(0.) {
                        _ = wheel_view.update(cx, |panel, cx| {
                            panel.pending_hunk_navigation = None;
                            panel.editor.revealed_hunk.set(None);
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

fn review_hunks(rows: &[DiffEditorRow]) -> Vec<ReviewHunk> {
    // Group displayed changes by patch hunk independently of their Git actions.
    let mut hunks = Vec::<ReviewHunk>::new();
    let mut header = None;
    for (row, entry) in rows.iter().enumerate() {
        match entry {
            DiffEditorRow::Hunk(path, index, _) => header = Some((path, *index, row)),
            DiffEditorRow::File(_) => header = None,
            DiffEditorRow::Code { changed: true, .. } => {
                if let Some((path, index, header)) = header {
                    if let Some(hunk) = hunks.last_mut().filter(|hunk| hunk.header == header) {
                        hunk.changes.end = row + 1;
                    } else {
                        hunks.push(ReviewHunk {
                            path: path.clone(),
                            index,
                            header,
                            changes: row..row + 1,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    hunks
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

// A source transaction can arrive as an enclosing replacement. Preserve
// equal lines inside it so their anchors survive changes on either side.
fn replay_changes(
    edit: &InputEdit,
    before: &Rope,
    source: &Rope,
    refine: bool,
) -> Vec<(Range<usize>, usize)> {
    let range = edit.start_byte..edit.old_end_byte;
    let inserted = edit.start_byte..edit.new_end_byte;
    if !refine
        || range.is_empty()
        || inserted.is_empty()
        || (edit.start_position.row == edit.old_end_position.row
            && edit.start_position.row == edit.new_end_position.row)
    {
        return vec![(range, inserted.len())];
    }

    let old = before.slice(range.clone()).to_string();
    let new = source.slice(inserted).to_string();
    let diff = TextDiff::configure()
        .deadline(Instant::now() + Duration::from_millis(20))
        .diff_lines(&old, &new);
    let mut offset = range.start;
    let mut changes = Vec::new();
    for op in diff.ops() {
        let old = &diff.old_slices()[op.old_range()];
        let len = old.iter().map(|line| line.len()).sum::<usize>();
        if op.tag() != DiffTag::Equal {
            let new = &diff.new_slices()[op.new_range()];
            let (span, replacement) = restore_text_edit(&old.concat(), &new.concat());
            changes.push((offset + span.start..offset + span.end, replacement.len()));
        }
        offset += len;
    }
    changes
}

fn translate_source_offset(byte: usize, changes: &[(Range<usize>, usize)]) -> usize {
    let mut old_end = 0;
    let mut new_end = 0;
    for (range, inserted) in changes {
        if byte < range.start {
            break;
        }
        new_end += range.start - old_end + inserted;
        old_end = range.end;
        // Source selections have right affinity.
        if byte < old_end {
            return new_end;
        }
    }
    new_end + (byte - old_end)
}

impl Projection {
    fn remap_rows(&mut self, rows: Range<usize>, new_count: usize, keep: usize) {
        if keep == rows.len() && new_count == rows.len() {
            return;
        }
        let delta = new_count as isize - rows.len() as isize;
        let mapping = self.row_mapping.get_or_insert_with(|| {
            Rc::new(RowMapping(
                (0..=self.lines.len()).map(|row| (row, row)).collect(),
            ))
        });
        Rc::make_mut(mapping).0.retain_mut(|(_, row)| {
            if *row >= rows.end {
                *row = row.saturating_add_signed(delta);
            } else if *row >= rows.start + keep {
                return false;
            }
            true
        });
    }

    fn apply_edit(&mut self, edit: &InputEdit, text: &Rope) {
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
        self.remap_rows(start..end + 1, replacement.len(), 1);
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

    fn replay_source_edit(
        &mut self,
        path: &Path,
        edit: &InputEdit,
        before: &Rope,
        source: &Rope,
        selected_rows: HashSet<usize>,
    ) -> Vec<(Range<usize>, String)> {
        let range = self
            .source_offset(path, edit.start_byte)
            .zip(self.source_offset(path, edit.old_end_byte))
            .map(|(start, end)| start..end)
            .filter(|range| {
                self.source_range(range)
                    .is_some_and(|(mapped_path, bytes)| {
                        mapped_path == path && bytes == (edit.start_byte..edit.old_end_byte)
                    })
                    && self.source_rows_match(range, before)
            });
        if let Some(range) = range {
            let replacement = source.slice(edit.start_byte..edit.new_end_byte).to_string();
            let mut text = self.text.clone();
            text.replace(range.clone(), &replacement);
            let edit = InputEdit {
                start_byte: range.start,
                old_end_byte: range.end,
                new_end_byte: range.start + replacement.len(),
                start_position: self.text.offset_to_point(range.start),
                old_end_position: self.text.offset_to_point(range.end),
                new_end_position: text.offset_to_point(range.start + replacement.len()),
            };
            self.apply_edit(&edit, &text);
            return vec![(range, replacement)];
        }

        self.replay_across_gaps(path, edit, before, source, selected_rows)
    }

    fn replay_across_gaps(
        &mut self,
        path: &Path,
        edit: &InputEdit,
        before: &Rope,
        source: &Rope,
        mut selected_rows: HashSet<usize>,
    ) -> Vec<(Range<usize>, String)> {
        let start = edit.start_position.row;
        let old_end = edit.old_end_position.row + 1;
        let new_end = edit.new_end_position.row + 1;
        let row_delta = new_end as isize - old_end as isize;
        let mut runs: Vec<(Range<usize>, Range<usize>)> = Vec::new();
        for (view_row, line) in self.lines.iter_mut().enumerate() {
            let Some(line) = line
                .as_mut()
                .filter(|line| line.source.as_deref() == Some(path))
            else {
                continue;
            };
            let old_row = line.position.line() - 1;
            if old_row < start {
                selected_rows.remove(&old_row);
                continue;
            }
            if old_row >= before.lines_len()
                || line.bytes.start != before.line_start_offset(old_row)
                || self.text.slice_line(view_row) != before.slice_line(old_row)
            {
                line.source = None;
            } else if old_row >= old_end {
                let row = old_row.saturating_add_signed(row_delta);
                selected_rows.remove(&row);
                line.bytes = source.line_start_offset(row)..source.line_end_offset(row);
                line.position = DiffLinePosition::new(
                    line.position.path().clone(),
                    DiffSide::Modified,
                    row + 1,
                );
            } else if let Some((view, old)) = runs
                .last_mut()
                .filter(|(view, old)| view.end == view_row && old.end == old_row)
            {
                view.end += 1;
                old.end += 1;
            } else {
                runs.push((view_row..view_row + 1, old_row..old_row + 1));
            }
        }
        self.edited = true;
        if runs.is_empty() {
            return Vec::new();
        }

        // Line boundaries split the source replacement into the visible runs.
        // Equal lines keep the intervening collapsed context in its own run.
        let old = before.slice_lines(start..old_end).to_string();
        let new = source.slice_lines(start..new_end).to_string();
        let old = old.split('\n').collect::<Vec<_>>();
        let new = new.split('\n').collect::<Vec<_>>();
        let diff = TextDiff::configure()
            .deadline(Instant::now() + Duration::from_millis(20))
            .diff_slices(&old, &new);
        let mut boundaries = vec![start];
        for op in diff.ops() {
            let old_count = op.old_range().len();
            let new = op.new_range();
            boundaries.extend((1..old_count).map(|row| start + new.start + row.min(new.len())));
            if old_count != 0 {
                boundaries.push(start + new.end);
            }
        }
        *boundaries.last_mut().unwrap() = new_end;

        for (_, old) in &runs {
            let new = boundaries[old.start - start]..boundaries[old.end - start];
            selected_rows.retain(|row| !new.contains(row));
        }
        let mut edits = Vec::new();
        for (view, old) in runs.into_iter().rev() {
            let path_key = self.lines[view.start]
                .as_ref()
                .unwrap()
                .position
                .path()
                .clone();
            let mut new = boundaries[old.start - start]..boundaries[old.end - start];
            // Keep the source boundary under a selection when its entire
            // visible run disappears. This also gives redo a row to expand.
            if new.is_empty() && new.start < source.lines_len() && selected_rows.remove(&new.start)
            {
                new.end += 1;
            }
            let mut text = String::new();
            let mut lines = Vec::with_capacity(new.len());
            for row in new {
                text.push_str(&source.slice_line(row).to_string());
                text.push('\n');
                lines.push(Some(Line {
                    position: DiffLinePosition::new(path_key.clone(), DiffSide::Modified, row + 1),
                    source: Some(path.to_owned()),
                    bytes: source.line_start_offset(row)..source.line_end_offset(row),
                }));
            }
            let offset = self.text.line_start_offset(view.start);
            let end = self.text.line_start_offset(view.end);
            let old = self.text.slice(offset..end).to_string();
            if old != text {
                let (range, replacement) = restore_text_edit(&old, &text);
                let range = offset + range.start..offset + range.end;
                self.text.replace(range.clone(), &replacement);
                edits.push((range, replacement));
            }
            self.remap_rows(view.clone(), lines.len(), view.len().min(lines.len()));
            self.lines.splice(view, lines);
        }
        edits
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
    render: &impl Fn(&str, f32, &mut App) -> AnyElement,
    colors: Palette,
    panel: WeakEntity<GitPanel>,
    position: FileHeaderPosition,
    cx: &mut App,
) -> AnyElement {
    let name = path.clone();
    let dark = cx.theme().mode.is_dark();
    // Use the visible header's position so the text reaches full strength at
    // the viewport edge and fades again when the next header pushes it off.
    let distance = (position.top - position.viewport_top).abs();
    let text_progress = (1. - distance / FILE_HEADER_HEIGHT).max(0.);
    h_flex()
        .id((ElementId::from("git-editor-file"), path.clone()))
        .debug_selector(|| format!("git-editor-file-header:{path}"))
        .w_full()
        .h_full()
        .px_2()
        .bg(if dark {
            rgb(0x26272c).into()
        } else {
            colors.elevated
        })
        .child(file_header_lighting(dark, position.next_top))
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
        .child(render(path.as_str(), text_progress, cx))
        .into_any_element()
}

pub(super) fn file_header_name(
    title: String,
    colors: Palette,
    progress: f32,
    dark: bool,
) -> impl IntoElement {
    let ink = colors.ink;
    let name = div()
        .flex_1()
        .min_w_0()
        .truncate()
        .font_medium()
        .text_color(ink);
    if progress == 0. {
        return name.child(title);
    }

    let (warm_ink, glow) = if dark {
        (rgb(0xe0d7ca), rgb(0xffb45a))
    } else {
        (rgb(0x66451e), rgb(0xeea548))
    };
    let glow = Hsla::from(glow).opacity(0.26 * progress);
    let title = StyledText::new(title);
    let layout = title.layout().clone();
    name.relative()
        .text_color(ink.blend(Hsla::from(warm_ink).opacity(progress)))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    paint_file_name_glow(&layout, glow, bounds, window);
                },
            )
            .absolute()
            .inset_0(),
        )
        .child(title)
}

fn paint_file_name_glow(
    layout: &TextLayout,
    color: Hsla,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) {
    const RADIUS: Pixels = px(3.);
    // Sample the glow around the same glyphs used for the foreground, including
    // the ellipsis when the name was truncated. This reuses GPUI's glyph atlas.
    const SAMPLES: [(f32, f32, f32); 12] = [
        (-0.4, 0., 0.14),
        (0.4, 0., 0.14),
        (0., -0.4, 0.14),
        (0., 0.4, 0.14),
        (-0.58, -0.58, 0.09),
        (-0.58, 0.58, 0.09),
        (0.58, -0.58, 0.09),
        (0.58, 0.58, 0.09),
        (-1., 0., 0.02),
        (1., 0., 0.02),
        (0., -1., 0.02),
        (0., 1., 0.02),
    ];
    let samples =
        SAMPLES.map(|(x, y, weight)| (point(RADIUS * x, RADIUS * y), color.opacity(weight)));
    let line_height = layout.line_height();
    let mut origin = layout.bounds().origin;
    window.paint_layer(bounds, |window| {
        for line in layout.line_layouts() {
            let baseline =
                origin + point(px(0.), (line_height + line.ascent() - line.descent()) / 2.);
            for run in line.runs() {
                for glyph in run.glyphs.iter().filter(|glyph| !glyph.is_emoji) {
                    for (offset, color) in samples {
                        _ = window.paint_glyph(
                            baseline + glyph.position + offset,
                            run.font_id,
                            glyph.id,
                            line.font_size(),
                            color,
                        );
                    }
                }
            }
            origin.y += line_height;
        }
    });
}

fn file_header_lighting(dark: bool, next_top: Option<Pixels>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let black = Hsla::black();
            let paint_shade = |window: &mut Window, bounds, angle, from, to| {
                window.paint_quad(fill(
                    bounds,
                    linear_gradient(
                        angle,
                        linear_color_stop(black.opacity(from), 0.),
                        linear_color_stop(black.opacity(to), 1.),
                    ),
                ));
            };
            // The viewport clips the contact shadow when the header reaches the top.
            let contact = Bounds::new(
                point(bounds.left(), bounds.top() - px(3.)),
                size(bounds.size.width, px(3.)),
            );
            paint_shade(window, contact, 180., 0., if dark { 0.26 } else { 0.055 });
            let (reflection, center_x, radius) = if dark {
                (FileHeaderLight::DarkReflection, px(72.), 0.60)
            } else {
                (FileHeaderLight::LightReflection, px(92.), 0.76)
            };
            let radius = bounds.size.width * radius;
            let image_bounds = Bounds::new(
                point(bounds.left() + center_x - radius, bounds.top()),
                size(radius * 2., bounds.size.height),
            );
            _ = window.paint_image(
                bounds,
                image_bounds,
                Corners::default(),
                reflection.image(),
                0,
                false,
            );

            let origin = bounds.bottom_left();
            let shadow_height = px(if dark { 9. } else { 8. });
            let (near, soft, fade_at) = if dark {
                (0.53, 0.19, shadow_height * 0.39)
            } else {
                (0.08, 0.08 * 0.32, shadow_height * 0.4)
            };
            let effect_height = if dark { shadow_height } else { px(14.) };
            let bottom = next_top
                .unwrap_or(origin.y + effect_height)
                .min(origin.y + effect_height);
            if bottom <= origin.y {
                return;
            }
            // When the next title pushes the pinned one up, its surface stays clear.
            let mask = ContentMask {
                bounds: Bounds::new(origin, size(bounds.size.width, bottom - origin.y)),
            };
            window.with_content_mask(Some(mask), |window| {
                paint_shade(
                    window,
                    Bounds::new(origin, size(bounds.size.width, fade_at)),
                    180.,
                    near,
                    soft,
                );
                paint_shade(
                    window,
                    Bounds::new(
                        point(origin.x, origin.y + fade_at),
                        size(bounds.size.width, shadow_height - fade_at),
                    ),
                    180.,
                    soft,
                    0.,
                );
                if !dark {
                    let image_bounds = Bounds::new(origin, size(bounds.size.width, effect_height));
                    _ = window.paint_image(
                        mask.bounds,
                        image_bounds,
                        Corners::default(),
                        FileHeaderLight::LightEdge.image(),
                        0,
                        false,
                    );
                }
            });
        },
    )
    .absolute()
    .inset_0()
}

enum FileHeaderLight {
    DarkReflection,
    LightReflection,
    LightEdge,
}

impl FileHeaderLight {
    fn image(self) -> Arc<RenderImage> {
        static DARK_REFLECTION: OnceLock<Arc<RenderImage>> = OnceLock::new();
        static LIGHT_REFLECTION: OnceLock<Arc<RenderImage>> = OnceLock::new();
        static LIGHT_EDGE: OnceLock<Arc<RenderImage>> = OnceLock::new();
        // Horizontal coordinates are fractions of the cached sprite width.
        // RenderImage expects BGRA channels.
        let header_height = FILE_HEADER_HEIGHT.as_f32() as u32;
        let (cache, center, radii, height, middle_stop, core, middle) = match self {
            Self::DarkReflection => (
                &DARK_REFLECTION,
                point(0.50, 13.),
                size(0.50, 33.),
                header_height,
                0.58,
                [154., 210., 255., 255. * 0.21],
                [187., 222., 255., 255. * 0.08],
            ),
            Self::LightReflection => (
                &LIGHT_REFLECTION,
                point(0.50, 11.),
                size(0.50, 30.),
                header_height,
                0.58,
                [111., 182., 248., 255. * 0.09],
                [153., 207., 255., 255. * 0.09 * 0.45],
            ),
            Self::LightEdge => (
                &LIGHT_EDGE,
                point(0.13, 1.),
                size(0.50, 12.),
                14,
                0.47,
                [195., 232., 255., 255. * 0.24],
                [185., 227., 255., 255. * 0.24 / 3.],
            ),
        };
        cache
            .get_or_init(|| {
                let width = 512;
                let mut edge = middle;
                edge[3] = 0.;
                let pixels = image::RgbaImage::from_fn(width, height, |x, y| {
                    let x = ((x as f32 + 0.5) / width as f32 - center.x) / radii.width;
                    let y = (y as f32 + 0.5 - center.y) / radii.height;
                    let distance = x.hypot(y).min(1.);
                    let (from, to, fraction) = if distance <= middle_stop {
                        (core, middle, distance / middle_stop)
                    } else {
                        (middle, edge, (distance - middle_stop) / (1. - middle_stop))
                    };
                    image::Rgba(std::array::from_fn(|channel| {
                        (from[channel] + (to[channel] - from[channel]) * fraction).round() as u8
                    }))
                });
                Arc::new(RenderImage::new([image::Frame::new(pixels)]))
            })
            .clone()
    }
}

impl GitPanel {
    fn review_hunk_label(
        &self,
        selected: usize,
        action: Option<(&SharedString, ActionBlocks)>,
        font_size: Pixels,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let revision = self.editor.row_revision?;
        let hunks = &self.editor.hunks;
        let hunk = hunks.get(selected)?;
        let directions = [HunkDirection::Previous, HunkDirection::Next];
        let neighbors = directions.map(|direction| {
            ReviewHunk::in_direction(hunks, selected, direction).map(|target| target.index)
        });
        let available = directions.map(|direction| {
            neighbors[direction as usize].is_some()
                || self
                    .adjacent_hunk_file(hunk.path.as_str(), direction)
                    .is_some()
        });
        let controls = action.and_then(|(path, blocks)| self.hunk_actions(path, blocks, cx));
        let path = hunk.path.clone();
        let view = cx.weak_entity();
        Some(hunk_label(
            controls,
            format!("diff-{}", hunk.header),
            available,
            font_size,
            move |direction, anchor, window, cx| {
                _ = view.update(cx, |panel, cx| {
                    if panel.editor.row_revision != Some(revision)
                        || panel.diff.read(cx).editor_row_revision() != revision
                    {
                        return;
                    }
                    if let Some(target) = neighbors[direction as usize] {
                        panel.navigate_hunk_in_file(path.clone(), target, anchor, cx);
                    } else {
                        panel.navigate_hunk_across_files(&path, direction, anchor, window, cx);
                    }
                });
            },
            cx,
        ))
    }

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
    pub(super) fn review_test_source_offset(&self, offset: usize) -> Option<(PathBuf, usize)> {
        self.editor
            .modified
            .projection
            .borrow()
            .source_range(&(offset..offset))
            .map(|(path, range)| (path, range.start))
    }

    #[cfg(test)]
    pub(super) fn review_test_hidden_selections(
        &self,
        cx: &App,
    ) -> Option<Vec<[Option<(PathBuf, usize)>; 2]>> {
        let column = &self.editor.modified;
        let hidden = column.hidden_selections.borrow();
        let saved = hidden
            .as_ref()
            .filter(|saved| saved.matches(column.editor.read(cx)))?;
        Some(
            saved
                .logical
                .iter()
                .map(|[anchor, head]| [anchor.source.clone(), head.source.clone()])
                .collect(),
        )
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
        let column = &self.editor.modified;
        let editor = column.editor.read(cx);
        if self.snapshot.is_some()
            || !editor.focus_handle(cx).contains_focused(window, cx)
            || !self
                .review
                .as_ref()
                .is_some_and(|review| super::review::has_live_target(&review.detail.comparison))
        {
            return None;
        }
        column.history_target(editor)
    }

    fn replay_review_history(&mut self, undo: bool, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.editing_path(window, cx);
        if let Some(path) = path {
            let Some(source) = self
                .workspace
                .read_with(cx, |workspace, _| {
                    if workspace.git_edit.contains(&path) {
                        return None;
                    }
                    workspace.files.get(&path).map(|file| file.editor.clone())
                })
                .ok()
                .flatten()
            else {
                return;
            };
            // The source owns undo history. Mirror the emitted replacements
            // before accepting more input in the projected editor.
            let mut before = source.read(cx).text().clone();
            let replay = cx.subscribe_in(
                &source,
                window,
                move |panel, _, event: &InputEditsEvent, window, cx| {
                    for (edit, source) in event.edits() {
                        panel
                            .editor
                            .modified
                            .replay_source_edit(&path, edit, &before, source, window, cx);
                        before = source.clone();
                    }
                    App::notify(cx, panel.review_pane.entity_id());
                },
            );
            source.update(cx, |editor, cx| {
                editor.replay_text_history(undo, window, cx);
            });
            // A grouped replay can enqueue its edits from a deferred callback.
            cx.defer(move |cx| cx.defer(move |_| drop(replay)));
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
