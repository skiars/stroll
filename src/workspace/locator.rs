use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::Result;
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme, Disableable as _, MODAL_POPUP_PRIORITY, PopupFrame, PopupMotion, PopupMotionKind,
    Selectable as _, Sizable as _, StyledExt as _, WindowExt as _,
    button::Button,
    checkbox::Checkbox,
    h_flex,
    input::{
        Editor, EditorState, Input, InputEvent, InputState, RangeDecoration,
        RangeDecorationCollection, RangeDecorationStyle, Rope, RopeExt,
    },
    scroll::ScrollbarViewport as _,
    v_flex,
};
use gpui_kit_assets::IconName;
use lsp_types::{DocumentSymbol, Location as SymbolLocation, Position, SymbolInformation};
use rust_i18n::t;
use serde_json::Value;

use crate::{
    docs,
    locator::{self, Results, TextHit, TextQuery},
    lsp, paths,
    remote::Request,
    repository::{Document, Repository},
};

use super::{
    FileOpen, Palette, Workspace, WorkspaceView, file_name, icon, icon_button, status_button,
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Files,
    Text,
    DocumentSymbols,
    WorkspaceSymbols,
    References,
}

impl Mode {
    const ALL: [Self; 4] = [
        Self::Files,
        Self::Text,
        Self::DocumentSymbols,
        Self::WorkspaceSymbols,
    ];

    fn label(self) -> SharedString {
        match self {
            Self::Files => t!("locator.files"),
            Self::Text => t!("locator.text"),
            Self::DocumentSymbols => t!("locator.document_symbols"),
            Self::WorkspaceSymbols => t!("locator.workspace_symbols"),
            Self::References => t!("lsp.find_references"),
        }
        .into_owned()
        .into()
    }

    fn placeholder(self) -> SharedString {
        match self {
            Self::Files => t!("locator.files_prompt"),
            Self::Text => t!("locator.text_prompt"),
            Self::DocumentSymbols => t!("locator.document_prompt"),
            Self::WorkspaceSymbols => t!("locator.workspace_prompt"),
            Self::References => return SharedString::default(),
        }
        .into_owned()
        .into()
    }

    fn has_scope(self) -> bool {
        matches!(self, Self::Files | Self::Text)
    }
}

struct Tab {
    query: TextQuery,
    selected: usize,
    scroll: ListState,
}

impl Default for Tab {
    fn default() -> Self {
        Self {
            query: TextQuery::default(),
            selected: 0,
            scroll: ListState::new(0, ListAlignment::Top, px(75.)),
        }
    }
}

#[derive(Clone, Copy)]
enum ResultRow {
    Heading(usize),
    Entry(usize),
}

#[derive(Default)]
struct Session {
    tabs: [Tab; 5],
    recent: Vec<PathBuf>,
    paths: Option<(std::time::Instant, Arc<Vec<PathBuf>>)>,
    scope_open: bool,
}

#[derive(Clone)]
struct Symbol {
    name: String,
    container: String,
    kind: u64,
    depth: usize,
    position: Option<lsp_types::Range>,
    resolve: Option<Value>,
}

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    hit: Option<Arc<TextHit>>,
    symbol: Option<Symbol>,
    reference: Option<Reference>,
}

#[derive(Clone)]
struct Reference {
    range: lsp_types::Range,
    text: Option<Rope>,
}

#[derive(Clone)]
struct ReferenceSearch {
    document: Arc<lsp::Document>,
    text: Rope,
    offset: usize,
    include_declaration: bool,
}

impl Entry {
    fn file(path: PathBuf) -> Self {
        Self {
            path,
            hit: None,
            symbol: None,
            reference: None,
        }
    }

    fn line(&self) -> Option<u32> {
        self.hit
            .as_ref()
            .map(|hit| hit.line)
            .or_else(|| {
                self.reference
                    .as_ref()
                    .map(|reference| reference.range.start.line)
            })
            .or_else(|| self.symbol.as_ref()?.position.map(|range| range.start.line))
    }

    fn range(&self, text: &Rope) -> Option<Range<usize>> {
        if let Some(hit) = &self.hit {
            return (hit.range.end <= text.len()).then(|| hit.range.clone());
        }
        let range = if let Some(reference) = &self.reference {
            reference.range
        } else {
            self.symbol.as_ref()?.position?
        };
        let start = lsp::editor_position(text, range.start)?;
        let end = lsp::editor_position(text, range.end)?;
        Some(text.position_to_offset(&start)..text.position_to_offset(&end))
    }
}

struct Preview {
    entry: Entry,
    text: Rope,
    range: Option<Range<usize>>,
    stale: bool,
    editor: Entity<EditorState>,
    _decoration: RangeDecorationCollection,
}

struct CancelSearch(Arc<AtomicBool>);

impl Drop for CancelSearch {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

struct LocatorPopup(WeakEntity<Workspace>);

#[derive(Clone, Copy)]
struct LocatorDrag;

impl Render for LocatorDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl Render for LocatorPopup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.0
            .update(cx, |workspace, cx| {
                let colors = Palette::for_mode(cx.theme().mode);
                workspace
                    .locator_popup(window, cx, colors)
                    .into_any_element()
            })
            .unwrap_or_else(|_| Empty.into_any_element())
    }
}

struct LocatorInputs {
    popup: Entity<LocatorPopup>,
    query: Entity<InputState>,
    include: Entity<InputState>,
    exclude: Entity<InputState>,
    references_focus: FocusHandle,
    _events: [Subscription; 3],
}

pub(super) struct Locator {
    inputs: Option<LocatorInputs>,
    pub(super) open: bool,
    motion: Option<PopupMotion>,
    position: Option<Point<Pixels>>,
    drag_anchor: Option<Point<Pixels>>,
    return_focus: Option<FocusHandle>,
    mode: Mode,
    sessions: HashMap<Vec<u8>, Session>,
    source: Option<PathBuf>,
    references: Option<ReferenceSearch>,
    items: Vec<Entry>,
    rows: Vec<ResultRow>,
    entry_rows: Vec<usize>,
    busy: bool,
    error: Option<String>,
    truncated: bool,
    skipped: usize,
    preview: Option<Preview>,
    open_after_preview: bool,
    preview_busy: bool,
    preview_error: Option<String>,
    search_task: Task<()>,
    search_cancel: Option<CancelSearch>,
    preview_task: Task<()>,
}

impl LocatorInputs {
    fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let owner = cx.weak_entity();
        let popup = cx.new(|_| LocatorPopup(owner));
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(Mode::Files.placeholder()));
        let include = cx
            .new(|cx| InputState::new(window, cx).placeholder(t!("locator.include").into_owned()));
        let exclude = cx
            .new(|cx| InputState::new(window, cx).placeholder(t!("locator.exclude").into_owned()));
        let inputs = [&query, &include, &exclude].map(|input| {
            cx.subscribe_in(input, window, |this, _, event, window, cx| {
                if !this.locator.open {
                    return;
                }
                match event {
                    InputEvent::Change => this.locator_changed(window, cx),
                    InputEvent::PressEnter { .. }
                        if this
                            .locator
                            .inputs()
                            .query
                            .read(cx)
                            .focus_handle(cx)
                            .is_focused(window) =>
                    {
                        this.locator_accept(window, cx)
                    }
                    _ => {}
                }
            })
        });
        Self {
            popup,
            query,
            include,
            exclude,
            references_focus: cx.focus_handle(),
            _events: inputs,
        }
    }
}

impl Locator {
    fn inputs(&self) -> &LocatorInputs {
        self.inputs.as_ref().expect("open locator inputs")
    }

    fn popup(&self) -> &Entity<LocatorPopup> {
        &self.inputs().popup
    }

    pub(super) fn new() -> Self {
        Self {
            inputs: None,
            open: false,
            motion: None,
            position: None,
            drag_anchor: None,
            return_focus: None,
            mode: Mode::Files,
            sessions: HashMap::new(),
            source: None,
            references: None,
            items: Vec::new(),
            rows: Vec::new(),
            entry_rows: Vec::new(),
            busy: false,
            error: None,
            truncated: false,
            skipped: 0,
            preview: None,
            open_after_preview: false,
            preview_busy: false,
            preview_error: None,
            search_task: Task::ready(()),
            search_cancel: None,
            preview_task: Task::ready(()),
        }
    }

    fn reset_preview(&mut self) {
        self.preview_task = Task::ready(());
        self.preview = None;
        self.preview_error = None;
        self.preview_busy = false;
        self.open_after_preview = false;
    }

    fn line_command(&self, tab: &Tab) -> bool {
        self.mode == Mode::Files && tab.query.query.starts_with(':')
    }
}

impl Workspace {
    fn notify_locator(&self, cx: &mut Context<Self>) {
        App::notify(cx, self.locator.popup().entity_id());
        cx.notify();
    }

    pub(super) fn locator_visit(&mut self, path: &Path) {
        if !path.is_absolute()
            && let Some(repo) = &self.repository
        {
            let recent = &mut self
                .locator
                .sessions
                .entry(repo.storage_key().to_vec())
                .or_default()
                .recent;
            recent.retain(|item| item != path);
            recent.insert(0, path.to_owned());
            recent.truncate(60);
        }
    }

    fn locator_session(&self) -> &Session {
        let key = self
            .repository
            .as_ref()
            .map_or(&[][..], |repo| repo.storage_key());
        &self.locator.sessions[key]
    }

    fn locator_session_mut(&mut self) -> &mut Session {
        let key = self
            .repository
            .as_ref()
            .map_or(&[][..], |repo| repo.storage_key());
        self.locator.sessions.get_mut(key).unwrap()
    }

    fn locator_tab(&self) -> &Tab {
        &self.locator_session().tabs[self.locator.mode as usize]
    }

    pub(super) fn open_locator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_locator(Mode::Files, window, cx) {
            self.locator_mode(Mode::Files, window, cx);
        }
    }

    pub(super) fn find_references(
        &mut self,
        _: &super::FindReferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.active_file() else {
            return;
        };
        let editor = file.editor.read(cx);
        if !editor.focus_handle(cx).is_focused(window) || file.missing {
            return;
        }
        let Some(document) = file.language_server.as_ref().map(|server| &server.document) else {
            return;
        };
        if !document.supports_references() {
            return;
        }
        let search = ReferenceSearch {
            document: document.clone(),
            text: editor.text().clone(),
            offset: editor.cursor(),
            include_declaration: false,
        };
        if self.show_locator(Mode::References, window, cx) {
            self.locator_session_mut().tabs[Mode::References as usize] = Tab::default();
            self.locator.references = Some(search);
            self.locator.inputs().references_focus.focus(window, cx);
            self.locator_search(window, cx);
        }
    }

    fn show_locator(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if (self.repository.is_none() && mode != Mode::References)
            || self.locator.open
            || self.repository_overlay_open()
            || self.confirming_close
            || window.has_active_dialog(cx)
            || self
                .active_file()
                .is_some_and(|file| file.editor.read(cx).is_composing())
            || self.locator_composing(window, cx)
        {
            return false;
        }
        self.locator
            .sessions
            .entry(
                self.repository
                    .as_ref()
                    .map_or_else(Vec::new, |repo| repo.storage_key().to_vec()),
            )
            .or_default();
        if self.locator.motion.is_some() || mode == Mode::References {
            self.locator_set_items(Vec::new());
        }
        self.locator.mode = mode;
        self.locator.references = None;
        self.locator.position = None;
        self.locator.return_focus = window.focused(cx);
        self.locator
            .inputs
            .get_or_insert_with(|| LocatorInputs::new(window, cx));
        let now = cx.background_executor().now();
        self.locator
            .motion
            .get_or_insert_with(|| PopupMotion::new(PopupMotionKind::Popover, now))
            .retarget(true, now);
        self.locator.open = true;
        self.repository_ui.menu_open = false;
        self.open_files_list = false;
        self.lsp_panel_open = false;
        self.locator.source = match self.view {
            WorkspaceView::Editor => self.tabs.active().map(Path::to_owned),
            WorkspaceView::Docs => self.doc.as_ref().map(|doc| doc.read(cx).path.clone()),
            WorkspaceView::Git => None,
        };
        if let Some(file) = self.active_file() {
            file.editor
                .update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
        }
        true
    }

    pub(super) fn close_locator(
        &mut self,
        restore: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.locator.open {
            return;
        }
        self.locator.open = false;
        self.locator.drag_anchor = None;
        self.locator.search_cancel = None;
        self.locator.search_task = Task::ready(());
        self.locator.preview_task = Task::ready(());
        if let Some(motion) = &mut self.locator.motion {
            motion.retarget(false, cx.background_executor().now());
        }
        let previous = self.locator.return_focus.take();
        if restore {
            previous
                .unwrap_or_else(|| self.focus.clone())
                .focus(window, cx);
        }
        self.notify_locator(cx);
    }

    pub(super) fn dismiss_locator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_locator(false, window, cx);
        if self.locator.motion.is_some() {
            self.finish_locator_close();
            cx.notify();
        }
    }

    fn finish_locator_close(&mut self) {
        self.locator.motion = None;
        self.locator.position = None;
        self.locator.references = None;
        self.locator.reset_preview();
        self.locator_set_items(Vec::new());
    }

    pub(super) fn locator_overlay(
        &mut self,
        window: &Window,
        cx: &App,
    ) -> Option<impl IntoElement + use<>> {
        let motion = self.locator.motion.as_ref()?;
        let (opacity, running) = motion.sample(cx.background_executor().now(), cx.reduce_motion());
        let open = self.locator.open;
        if !open && opacity <= 0. {
            self.finish_locator_close();
            return None;
        }
        if running {
            window.request_animation_frame();
        }
        Some(
            deferred(PopupFrame::new(
                div().absolute().inset_0().opacity(opacity).child(
                    self.locator
                        .popup()
                        .clone()
                        .cached(div().size_full().style().clone()),
                ),
                !open,
            ))
            .with_priority(MODAL_POPUP_PRIORITY),
        )
    }

    fn locator_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        self.locator.items.clear();
        self.locator.rows.clear();
        self.locator.entry_rows.clear();
        self.locator.mode = mode;
        let query = self.locator_tab().query.clone();
        self.locator.inputs().query.update(cx, |input, cx| {
            input.set_placeholder(mode.placeholder(), window, cx);
            input.set_value(query.query, window, cx);
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.locator.inputs().include.update(cx, |input, cx| {
            input.set_value(query.scope.include, window, cx)
        });
        self.locator.inputs().exclude.update(cx, |input, cx| {
            input.set_value(query.scope.exclude, window, cx)
        });
        self.locator_search(window, cx);
    }

    fn locator_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.locator.mode == Mode::References {
            return;
        }
        let mode = self.locator.mode as usize;
        let query = self.locator.inputs().query.read(cx).value().to_string();
        let include = self.locator.inputs().include.read(cx).value().to_string();
        let exclude = self.locator.inputs().exclude.read(cx).value().to_string();
        let tab = &mut self.locator_session_mut().tabs[mode];
        if tab.query.query == query
            && tab.query.scope.include == include
            && tab.query.scope.exclude == exclude
        {
            return;
        }
        tab.query.query = query;
        tab.query.scope.include = include;
        tab.query.scope.exclude = exclude;
        self.locator_search(window, cx);
    }

    fn locator_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.locator.search_task = Task::ready(());
        self.locator.search_cancel = None;
        self.locator.reset_preview();
        self.locator.error = None;
        self.locator.truncated = false;
        self.locator.skipped = 0;
        self.locator.busy = false;
        let tab = self.locator_tab();
        let mode = self.locator.mode;
        let query = tab.query.clone();
        if self.locator.line_command(tab)
            || (matches!(mode, Mode::Text | Mode::WorkspaceSymbols) && query.query.is_empty())
        {
            self.locator_set_items(Vec::new());
            self.notify_locator(cx);
            return;
        }
        let repo = self.repository.clone();
        let references = self.locator.references.clone().map(|search| {
            let targets = self.reference_targets(cx);
            (search, targets)
        });
        let recent = self.locator_session().recent.clone();
        let paths = self
            .locator_session()
            .paths
            .clone()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(3));
        let mut fresh_paths = None;
        let source = self.locator.source.clone();
        let lsp = source
            .as_ref()
            .and_then(|path| self.files.get(path))
            .and_then(|file| {
                Some((
                    file.language_server.as_ref()?.document.clone(),
                    file.editor.read(cx).text().clone(),
                ))
            });
        let cancelled = Arc::new(AtomicBool::new(false));
        self.locator.search_cancel = Some(CancelSearch(cancelled.clone()));
        self.locator.busy = true;
        self.locator.search_task = cx.spawn_in(window, async move |this, cx| {
            if mode != Mode::References {
                cx.background_executor()
                    .timer(Duration::from_millis(180))
                    .await;
            }
            let result = if let Some((search, targets)) = references {
                async {
                    let targets = targets?;
                    let locations = search
                        .document
                        .references(search.text, search.offset, search.include_declaration)
                        .await?;
                    let local_source = source.as_ref().is_some_and(|path| path.is_absolute());
                    Ok(reference_entries(
                        repo.as_deref(),
                        local_source,
                        locations,
                        targets,
                    ))
                }
                .await
            } else if let Some(repo) = repo {
                if mode == Mode::Files {
                    async {
                        let paths = match paths {
                            Some((_, paths)) => paths,
                            None => {
                                let paths = Arc::new(
                                    cx.background_spawn(
                                        async move { locator::listed_paths(&repo) },
                                    )
                                    .await?,
                                );
                                fresh_paths = Some((std::time::Instant::now(), paths.clone()));
                                paths
                            }
                        };
                        cx.background_spawn(async move {
                            locator::search_files(&paths, &query.query, &query.scope, &recent)
                                .map(|result| map_results(result, Entry::file))
                        })
                        .await
                    }
                    .await
                } else if mode == Mode::Text {
                    cx.background_spawn(async move {
                        let hits = if let Some(ssh) = &repo.ssh {
                            anyhow::ensure!(ssh.supports_locator(), t!("locator.upgrade_helper"));
                            ssh.call(Request::Locator(query))?
                        } else {
                            locator::search_text(&repo, &query, || {
                                cancelled.load(Ordering::Relaxed)
                            })?
                        };
                        Ok(map_results(hits, |hit| {
                            let mut entry = Entry::file(hit.path.clone());
                            entry.hit = Some(Arc::new(hit));
                            entry
                        }))
                    })
                    .await
                } else {
                    async {
                        let source =
                            source.ok_or_else(|| anyhow::anyhow!(t!("locator.needs_file")))?;
                        let (document, text) =
                            lsp.ok_or_else(|| anyhow::anyhow!(t!("locator.unavailable")))?;
                        let value = document
                            .locator_symbols(
                                text,
                                (mode == Mode::WorkspaceSymbols).then(|| query.query.clone()),
                            )
                            .await?;
                        symbols(&repo, source, mode, &query.query, value)
                    }
                    .await
                }
            } else {
                Err(anyhow::anyhow!(t!("locator.unavailable")))
            };
            _ = this.update_in(cx, |this, window, cx| {
                let result = if mode == Mode::References && !this.reference_source_is_current(cx) {
                    Err(anyhow::anyhow!(t!("lsp.references_source_changed")))
                } else {
                    result
                };
                this.locator.busy = false;
                if fresh_paths.is_some() {
                    this.locator_session_mut().paths = fresh_paths;
                }
                let mut selected = this.locator_tab().selected;
                let previous = this.locator.items.get(selected);
                match result {
                    Ok(result) => {
                        if let Some(old) = previous {
                            selected = result
                                .items
                                .iter()
                                .position(|item| {
                                    item.path == old.path
                                        && item.line() == old.line()
                                        && item.hit.as_ref().map(|hit| hit.range.start)
                                            == old.hit.as_ref().map(|hit| hit.range.start)
                                        && item.symbol.as_ref().map(|symbol| &symbol.name)
                                            == old.symbol.as_ref().map(|symbol| &symbol.name)
                                        && item.reference.as_ref().map(|reference| reference.range)
                                            == old
                                                .reference
                                                .as_ref()
                                                .map(|reference| reference.range)
                                })
                                .unwrap_or(selected);
                        }
                        this.locator_set_items(result.items);
                        this.locator.truncated = result.truncated;
                        this.locator.skipped = result.skipped;
                    }
                    Err(error) => {
                        this.locator_set_items(Vec::new());
                        this.locator.error = Some(error.to_string());
                    }
                }
                this.locator_select(selected, false, window, cx);
            });
        });
        self.notify_locator(cx);
    }

    fn reference_source_is_current(&self, cx: &App) -> bool {
        self.locator.references.as_ref().is_some_and(|search| {
            self.locator
                .source
                .as_ref()
                .and_then(|path| self.files.get(path))
                .is_some_and(|file| {
                    !file.missing
                        && file
                            .language_server
                            .as_ref()
                            .is_some_and(|server| Arc::ptr_eq(&server.document, &search.document))
                        && file.editor.read(cx).text() == &search.text
                })
        })
    }

    fn reference_targets(&self, cx: &App) -> Result<HashMap<PathBuf, Rope>> {
        anyhow::ensure!(
            self.reference_source_is_current(cx),
            t!("lsp.references_source_changed")
        );
        let source = self.locator.source.as_ref().unwrap();
        let project = &self.files[source].server_project.as_ref().unwrap().key;
        Ok(self
            .files
            .values()
            .filter(|file| !file.path.is_absolute() || source.is_absolute())
            .map(|file| {
                let document = file.language_server.as_ref().filter(|_| {
                    file.server_project
                        .as_ref()
                        .is_some_and(|file| &file.key == project)
                });
                let text = if let Some(server) = document {
                    let text = file.editor.read(cx).text().clone();
                    if &file.path != source {
                        server.document.changed(text.clone());
                    }
                    text
                } else {
                    file.saved.clone()
                };
                (file.path.clone(), text)
            })
            .collect())
    }

    fn locator_set_items(&mut self, items: Vec<Entry>) {
        let locator = &mut self.locator;
        locator.rows.clear();
        locator.entry_rows.clear();
        for (index, entry) in items.iter().enumerate() {
            if locator.mode == Mode::Text && (index == 0 || items[index - 1].path != entry.path) {
                locator.rows.push(ResultRow::Heading(index));
            }
            locator.entry_rows.push(locator.rows.len());
            locator.rows.push(ResultRow::Entry(index));
        }
        locator.items = items;

        let scroll = &self.locator_tab().scroll;
        let anchor = scroll.logical_scroll_top();
        scroll.splice_with_height_hints(
            0..scroll.item_count(),
            self.locator.rows.iter().map(|row| match row {
                ResultRow::Heading(_) => px(36.),
                ResultRow::Entry(_) => px(51.),
            }),
        );
        let count = self.locator.rows.len();
        if count > 0 {
            scroll.scroll_to(ListOffset {
                item_ix: anchor.item_ix.min(count - 1),
                offset_in_item: anchor.offset_in_item,
            });
        }
    }

    fn locator_select(
        &mut self,
        index: usize,
        show: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mode = self.locator.mode;
        let index = index.min(self.locator.items.len().saturating_sub(1));
        let row = self.locator.entry_rows.get(index).copied();
        let tab = &mut self.locator_session_mut().tabs[mode as usize];
        tab.selected = index;
        if let Some(row) = row.filter(|_| show) {
            tab.scroll.scroll_to_reveal_item(row);
        }
        self.locator_preview(window, cx);
    }

    fn locator_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.locator.reset_preview();
        let Some(mut entry) = self.locator.items.get(self.locator_tab().selected).cloned() else {
            self.notify_locator(cx);
            return;
        };
        if self.locator.busy {
            return;
        }
        let repo = self.repository.clone();
        let store = self.store.clone();
        let live_text = |workspace: &Workspace, path: &Path, cx: &App| {
            workspace
                .files
                .get(path)
                .map(|file| (file.editor.read(cx).text().clone(), file.is_dirty()))
        };
        let mut live = live_text(self, &entry.path, cx);
        let source_lsp = self
            .locator
            .source
            .as_ref()
            .and_then(|path| self.files.get(path))
            .and_then(|file| {
                Some((
                    file.language_server.as_ref()?.document.clone(),
                    file.editor.read(cx).text().clone(),
                ))
            });
        self.locator.preview_busy = true;
        self.locator.preview_task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                if let Some(symbol) = &mut entry.symbol
                    && let Some(resolve) = symbol.resolve.take()
                {
                    let (document, text) =
                        source_lsp.ok_or_else(|| anyhow::anyhow!(t!("locator.unavailable")))?;
                    let value = document.resolve_locator_symbol(text, resolve).await?;
                    let location: SymbolLocation =
                        serde_json::from_value(value["location"].clone())?;
                    let path = repo
                        .as_ref()
                        .and_then(|repo| repo.source_path(location.uri.as_str()))
                        .ok_or_else(|| anyhow::anyhow!(t!("workspace.external_definition")))?;
                    live = this
                        .update_in(cx, |this, _, cx| live_text(this, &path, cx))
                        .ok()
                        .flatten();
                    entry.path = path;
                    symbol.position = Some(location.range);
                }
                anyhow::ensure!(
                    !entry.path.as_os_str().is_empty(),
                    t!("workspace.external_definition")
                );
                cx.background_spawn(async move {
                    let (text, draft, saved) = match live {
                        Some((text, draft)) => (text, draft, None),
                        None => {
                            let document = if let Some(repo) = &repo {
                                Document::open(repo, &entry.path, &store)?
                            } else {
                                Document::open_local(&entry.path, &store)?
                            };
                            let saved = (document.restored && entry.reference.is_some())
                                .then(|| Rope::from(document.saved_text.as_str()));
                            (Rope::from(document.text.as_str()), document.restored, saved)
                        }
                    };
                    let stale = entry.hit.as_ref().is_some_and(|hit| {
                        locator::fingerprint(&text.to_string()) != hit.fingerprint
                    }) || entry.reference.as_ref().is_some_and(|reference| {
                        reference
                            .text
                            .as_ref()
                            .or(saved.as_ref())
                            .is_some_and(|expected| expected != &text)
                    });
                    anyhow::ensure!(!stale || draft, t!("locator.stale"));
                    let range = if stale { None } else { entry.range(&text) };
                    if entry.line().is_some() && !stale {
                        anyhow::ensure!(range.is_some(), t!("locator.stale"));
                    }
                    Ok((entry, text, range, stale))
                })
                .await
            }
            .await;
            _ = this.update_in(cx, |this, window, cx| {
                this.locator.preview_busy = false;
                match result {
                    Ok((entry, text, range, stale)) => {
                        let language = if text.len() <= 300_000 {
                            this.source_language(&entry.path, &text)
                        } else {
                            "text".into()
                        };
                        let editor = cx.new(|cx| {
                            EditorState::new(window, cx)
                                .language(language)
                                .line_number(true)
                                .folding(false)
                                .searchable(false)
                                .soft_wrap(false)
                                .default_file_text(text.clone())
                        });
                        let accent = cx.theme().accent;
                        let decoration = editor.update(cx, |editor, cx| {
                            editor.create_range_decorations_collection(Vec::new(), cx)
                        });
                        if let Some(range) = &range {
                            decoration.set(
                                vec![
                                    RangeDecoration::new(range.clone())
                                        .with_style(RangeDecorationStyle::Fill)
                                        .with_color(accent.alpha(0.22)),
                                ],
                                cx,
                            );
                            if let Some(line) = entry.line() {
                                editor.update(cx, |editor, cx| {
                                    editor.set_scroll_offset(
                                        point(
                                            px(0.),
                                            px(-((line.saturating_sub(5)) as f32) * 22.1),
                                        ),
                                        cx,
                                    );
                                });
                            }
                        }
                        this.locator.preview = Some(Preview {
                            entry,
                            text,
                            range,
                            stale,
                            editor,
                            _decoration: decoration,
                        });
                    }
                    Err(error) => this.locator.preview_error = Some(error.to_string()),
                }
                if std::mem::take(&mut this.locator.open_after_preview) {
                    this.locator_accept(window, cx);
                }
                this.notify_locator(cx);
            });
        });
        self.notify_locator(cx);
    }

    fn locator_accept(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.locator.mode == Mode::References && !self.reference_source_is_current(cx) {
            self.locator.error = Some(t!("lsp.references_source_changed").into_owned());
            self.notify_locator(cx);
            return;
        }
        let query = self.locator_tab().query.query.clone();
        if self.locator.line_command(self.locator_tab()) {
            self.locator_goto_line(&query, window, cx);
            return;
        }
        if self.locator.busy {
            return;
        }
        if self.locator.preview_busy {
            self.locator.open_after_preview = true;
            return;
        }
        let Some(preview) = &self.locator.preview else {
            return;
        };
        if preview.stale {
            self.locator.error = Some(t!("locator.stale").into_owned());
            self.notify_locator(cx);
            return;
        }
        let path = preview.entry.path.clone();
        let position = preview
            .range
            .clone()
            .map(|range| (range, locator::fingerprint(&preview.text.to_string())));
        self.locator_open_target(path, position, window, cx);
    }

    fn locator_goto_line(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, position)) = self.locator.source.clone().zip(parse_position(query)) else {
            self.locator_line_error(cx);
            return;
        };
        if let Some(file) = self
            .files
            .get(&path)
            .filter(|_| self.view == WorkspaceView::Editor)
        {
            let text = file.editor.read(cx).text();
            if let Some(target) = position_range(text, position) {
                self.locator_open_target(path, Some(target), window, cx);
            } else {
                self.locator_line_error(cx);
            }
        } else if self.view == WorkspaceView::Docs && !self.locator.preview_busy {
            let repo = self.repository.as_ref().unwrap().clone();
            self.locator.preview_busy = true;
            self.locator.preview_task = cx.spawn_in(window, async move |this, cx| {
                let (path, target) = cx
                    .background_spawn(async move {
                        let target = Document::read(&repo, &path).ok().and_then(|doc| {
                            position_range(&Rope::from(doc.text.as_str()), position)
                        });
                        (path, target)
                    })
                    .await;
                _ = this.update_in(cx, |this, window, cx| {
                    this.locator.preview_busy = false;
                    if let Some(target) = target {
                        this.locator_open_target(path, Some(target), window, cx);
                    } else {
                        this.locator_line_error(cx);
                    }
                });
            });
        }
    }

    fn locator_line_error(&mut self, cx: &mut Context<Self>) {
        self.locator.error = Some(t!("locator.line_invalid").into_owned());
        self.notify_locator(cx);
    }

    fn locator_open_target(
        &mut self,
        path: PathBuf,
        position: Option<(Range<usize>, [u8; 32])>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view != WorkspaceView::Docs
            && let Some((_, fingerprint)) = &position
            && self.files.get(&path).is_some_and(|file| {
                locator::fingerprint(&file.editor.read(cx).text().to_string()) != *fingerprint
            })
        {
            self.locator.error = Some(t!("locator.stale").into_owned());
            self.notify_locator(cx);
            return;
        }
        self.close_locator(false, window, cx);
        self.focus.focus(window, cx);
        if self.view == WorkspaceView::Docs && docs::is_markdown(&path) {
            self.open_doc_at(path, None, position, None, window, cx);
        } else {
            self.switch_view(WorkspaceView::Editor, window, cx);
            self.request_file(path, FileOpen::Locate(position), window, cx);
        }
    }
}

fn map_results<A, B>(result: Results<A>, map: impl FnMut(A) -> B) -> Results<B> {
    Results {
        items: result.items.into_iter().map(map).collect(),
        truncated: result.truncated,
        skipped: result.skipped,
    }
}

fn reference_entries(
    repo: Option<&Repository>,
    local_source: bool,
    locations: Vec<SymbolLocation>,
    targets: HashMap<PathBuf, Rope>,
) -> Results<Entry> {
    let mut seen = HashSet::new();
    let mut skipped = 0;
    let mut items = Vec::new();
    for location in locations {
        let Some(path) = Workspace::language_source_path(repo, location.uri.as_str(), local_source)
        else {
            skipped += 1;
            continue;
        };
        let range = location.range;
        if range.start > range.end || !seen.insert((path.clone(), range)) {
            continue;
        }
        let reference = Reference {
            range,
            text: targets.get(&path).cloned(),
        };
        items.push(Entry {
            reference: Some(reference),
            ..Entry::file(path)
        });
    }
    items.sort_by(|a, b| {
        let a = (
            &a.path,
            a.reference.as_ref().map(|reference| reference.range.start),
        );
        let b = (
            &b.path,
            b.reference.as_ref().map(|reference| reference.range.start),
        );
        a.cmp(&b)
    });
    let truncated = items.len() > 500;
    items.truncate(500);
    Results {
        items,
        truncated,
        skipped,
    }
}

fn position_range(text: &Rope, position: Position) -> Option<(Range<usize>, [u8; 32])> {
    let position = lsp::editor_position(text, position)?;
    let line = text.slice_line(position.line as usize);
    let max_column = line.chars().count()
        - usize::from(line.get_char(line.len_chars().saturating_sub(1)) == Ok('\n'));
    if position.character as usize > max_column {
        return None;
    }
    let start = text.position_to_offset(&position);
    Some((start..start, locator::fingerprint(&text.to_string())))
}

fn parse_position(query: &str) -> Option<Position> {
    let (line, column) = query
        .strip_prefix(':')?
        .split_once(':')
        .unwrap_or((&query[1..], "1"));
    if ![line, column]
        .into_iter()
        .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    Some(Position::new(
        line.parse::<u32>().ok()?.checked_sub(1)?,
        column.parse::<u32>().ok()?.checked_sub(1)?,
    ))
}

fn symbols(
    repo: &Repository,
    path: PathBuf,
    mode: Mode,
    query: &str,
    value: Value,
) -> Result<Results<Entry>> {
    fn nested(
        items: &mut Vec<Entry>,
        path: &Path,
        symbol: DocumentSymbol,
        container: &str,
        depth: usize,
    ) {
        let name = symbol.name;
        items.push(Entry {
            symbol: Some(Symbol {
                name: name.clone(),
                container: container.to_owned(),
                kind: serde_json::to_value(symbol.kind)
                    .unwrap()
                    .as_u64()
                    .unwrap_or_default(),
                depth,
                position: Some(symbol.selection_range),
                resolve: None,
            }),
            ..Entry::file(path.to_owned())
        });
        let children = symbol.children.unwrap_or_default();
        for child in children {
            nested(items, path, child, &name, depth + 1);
        }
    }
    let mut items = Vec::new();
    for value in value.as_array().into_iter().flatten() {
        if mode == Mode::DocumentSymbols && value.get("selectionRange").is_some() {
            nested(
                &mut items,
                &path,
                serde_json::from_value(value.clone())?,
                "",
                0,
            );
            continue;
        }
        let location = &value["location"];
        let uri = location["uri"].as_str().unwrap_or_default();
        let path = repo.source_path(uri).unwrap_or_default();
        let position = serde_json::from_value(location["range"].clone()).ok();
        let resolve =
            (mode == Mode::WorkspaceSymbols && position.is_none() && !path.as_os_str().is_empty())
                .then(|| value.clone());
        let symbol = if let Ok(flat) = serde_json::from_value::<SymbolInformation>(value.clone()) {
            Symbol {
                name: flat.name,
                container: flat.container_name.unwrap_or_default(),
                kind: value["kind"].as_u64().unwrap_or_default(),
                depth: 0,
                position,
                resolve,
            }
        } else {
            let Some(name) = value["name"].as_str() else {
                continue;
            };
            Symbol {
                name: name.to_owned(),
                container: value["containerName"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                kind: value["kind"].as_u64().unwrap_or_default(),
                depth: 0,
                position,
                resolve,
            }
        };
        items.push(Entry {
            symbol: Some(symbol),
            ..Entry::file(path)
        });
    }
    if mode == Mode::DocumentSymbols {
        let query = query.to_lowercase();
        items.retain(|item| {
            item.symbol.as_ref().is_some_and(|symbol| {
                format!("{}.{}", symbol.container, symbol.name)
                    .to_lowercase()
                    .contains(&query)
            })
        });
        items.sort_by_key(Entry::line);
    }
    let truncated = items.len() > 500;
    items.truncate(500);
    Ok(Results {
        items,
        truncated,
        skipped: 0,
    })
}

impl Workspace {
    fn locator_popup(
        &self,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let locator = &self.locator;
        let mode = locator.mode;
        let references = mode == Mode::References;
        let tab = self.locator_tab();
        let line_only = locator.line_command(tab);
        let viewport = window.viewport_size();
        let narrow = viewport.width < px(700.);
        let stacked_tabs = viewport.width < px(600.);
        let top = px(92.) + (viewport.height * 0.07).min(px(56.));
        let height = (viewport.height * 0.70)
            .min(px(600.))
            .min((viewport.height - top - px(12.)).max(px(120.)));
        let width =
            (viewport.width - px(if stacked_tabs { 16. } else { 24. })).clamp(px(0.), px(960.));
        let limit = point(
            (viewport.width - width).max(px(0.)),
            (viewport.height - height).max(px(0.)),
        );
        let clamp_position = move |position: Point<Pixels>| {
            point(
                position.x.clamp(px(0.), limit.x),
                position.y.clamp(px(0.), limit.y),
            )
        };
        let origin = clamp_position(locator.position.unwrap_or_else(|| point(limit.x / 2., top)));
        let status = if let Some(error) = &locator.error {
            error.clone()
        } else if locator.busy {
            t!("locator.searching").into_owned()
        } else if line_only {
            locator
                .source
                .as_ref()
                .map(|path| paths::display(path))
                .unwrap_or_else(|| t!("locator.needs_file").into_owned())
        } else if locator.items.is_empty()
            && tab.query.query.is_empty()
            && !matches!(mode, Mode::DocumentSymbols | Mode::References)
        {
            t!("locator.query_required").into_owned()
        } else if locator.items.is_empty() {
            t!("locator.empty").into_owned()
        } else {
            locator.items.len().to_string()
        };
        let tab_button = |value: Mode| {
            div()
                .id(("locator-tab", value as usize))
                .role(Role::Tab)
                .aria_label(value.label())
                .aria_selected(value == mode)
                .tab_index(0)
                .tab_stop(value == mode)
                .h(px(36.))
                .px(px(9.))
                .flex()
                .items_center()
                .cursor_pointer()
                .border_b(px(2.))
                .border_color(if value == mode {
                    colors.accent
                } else {
                    transparent_black()
                })
                .text_color(if value == mode {
                    colors.accent
                } else {
                    colors.muted
                })
                .hover(|style| style.bg(colors.hover))
                .child(value.label())
                .on_click(
                    cx.listener(move |this, _, window, cx| this.locator_mode(value, window, cx)),
                )
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    let delta = match event.keystroke.key.as_str() {
                        "left" => 3,
                        "right" => 1,
                        "enter" | "space" => 0,
                        _ => return,
                    };
                    window.prevent_default();
                    cx.stop_propagation();
                    this.locator_mode(Mode::ALL[(value as usize + delta) % 4], window, cx);
                }))
        };
        let tabs = if stacked_tabs {
            v_flex()
                .flex_1()
                .child(
                    h_flex()
                        .child(tab_button(Mode::Files).flex_1())
                        .child(tab_button(Mode::Text).flex_1()),
                )
                .child(
                    h_flex()
                        .child(tab_button(Mode::DocumentSymbols).flex_1())
                        .child(tab_button(Mode::WorkspaceSymbols).flex_1()),
                )
                .into_any_element()
        } else {
            h_flex()
                .id("locator-tabs")
                .overflow_x_scroll()
                .min_w_0()
                .children(Mode::ALL.map(tab_button))
                .into_any_element()
        };
        let workspace = cx.weak_entity();
        let header = h_flex()
            .id("locator-header")
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(colors.border)
            .when(references, |header| {
                header.child(div().px_2().font_semibold().child(mode.label()))
            })
            .when(!references, |header| header.role(Role::TabList).child(tabs))
            .child(
                div()
                    .id("locator-drag-handle")
                    .flex_1()
                    .min_w(px(24.))
                    .h(px(36.))
                    .cursor(CursorStyle::OpenHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, _| {
                            this.locator.drag_anchor = Some(event.position - origin);
                        }),
                    )
                    .on_drag(LocatorDrag, move |drag, _, window, cx| {
                        let _ = workspace.update(cx, |this, cx| {
                            if let Some(anchor) = this.locator.drag_anchor {
                                this.locator.position =
                                    Some(clamp_position(window.mouse_position() - anchor));
                                this.notify_locator(cx);
                            }
                        });
                        cx.new(|_| *drag)
                    })
                    .on_drag_move(cx.listener(
                        move |this, event: &DragMoveEvent<LocatorDrag>, _, cx| {
                            if let Some(anchor) = this.locator.drag_anchor {
                                let position = clamp_position(event.event.position - anchor);
                                if this.locator.position != Some(position) {
                                    this.locator.position = Some(position);
                                    this.notify_locator(cx);
                                }
                            }
                        },
                    )),
            )
            .child(
                icon_button(
                    "locator-refresh",
                    IconName::RefreshCw,
                    t!("locator.refresh").into_owned(),
                )
                .with_size(px(16.))
                .disabled(locator.busy)
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.locator.mode != Mode::References {
                        this.locator_session_mut().paths = None;
                    }
                    this.locator_search(window, cx);
                })),
            )
            .child(
                icon_button(
                    "locator-close",
                    IconName::X,
                    t!("common.close").into_owned(),
                )
                .with_size(px(16.))
                .on_click(cx.listener(|this, _, window, cx| this.close_locator(true, window, cx))),
            );
        let popup = v_flex()
            .id("locator")
            .key_context("Locator")
            .role(Role::Dialog)
            .aria_label(if references {
                mode.label()
            } else {
                t!("locator.title").into_owned().into()
            })
            .when(references, |popup| {
                popup.track_focus(&locator.inputs().references_focus)
            })
            .tab_group()
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.close_locator(true, window, cx);
                cx.stop_propagation();
            }))
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::Escape, window, cx| {
                    if !this.locator_composing(window, cx) {
                        this.close_locator(true, window, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::MoveDown, window, cx| {
                    if this.locator_preview_focused(window, cx)
                        || this.locator_composing(window, cx)
                    {
                        return;
                    }
                    let index = this.locator_tab().selected.saturating_add(1);
                    this.locator_select(index, true, window, cx);
                    cx.stop_propagation();
                }),
            )
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::MoveUp, window, cx| {
                    if this.locator_preview_focused(window, cx)
                        || this.locator_composing(window, cx)
                    {
                        return;
                    }
                    let index = this.locator_tab().selected.saturating_sub(1);
                    this.locator_select(index, true, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.locator_composing(window, cx) {
                    return;
                }
                let index = this.locator_tab().selected;
                match event.keystroke.key.as_str() {
                    "escape" => this.close_locator(true, window, cx),
                    "enter"
                        if this.locator.mode == Mode::References
                            && this.locator.inputs().references_focus.is_focused(window) =>
                    {
                        this.locator_accept(window, cx)
                    }
                    "up" => this.locator_select(index.saturating_sub(1), true, window, cx),
                    "down" => this.locator_select(index.saturating_add(1), true, window, cx),
                    _ => return,
                }
                window.prevent_default();
                cx.stop_propagation();
            }))
            .w(width)
            .when(!line_only, |popup| popup.h(height))
            .max_h(height)
            .rounded(px(12.))
            .overflow_hidden()
            .border_1()
            .border_color(colors.surface.alpha(0.55))
            .shadow_lg()
            .text_size(px(13.))
            .child(header)
            .when(!references, |popup| {
                popup.child(
                    div().mx_3().mt(px(6.)).mb_1().child(
                        Input::new(&locator.inputs().query)
                            .w_full()
                            .focus_bordered(false)
                            .prefix(icon(IconName::Search, colors.muted, 14.)),
                    ),
                )
            })
            .child(self.locator_options(cx, colors, height))
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .text_size(px(12.))
                    .text_color(colors.muted)
                    .child(status)
                    .when(locator.truncated, |row| {
                        row.child(format!(" · {}", t!("locator.limit")))
                    })
                    .when(locator.skipped > 0, |row| {
                        let skipped = if references {
                            t!("lsp.references_outside")
                        } else {
                            t!("locator.skipped")
                        };
                        row.child(format!(" · {} ({})", skipped, locator.skipped))
                    }),
            )
            .when(!line_only, |popup| {
                popup.child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .when(narrow, |body| body.flex_col())
                        .child(self.locator_results(cx, colors, narrow, width))
                        .child(self.locator_preview_view(cx, colors, narrow)),
                )
            })
            .child(
                h_flex()
                    .border_t_1()
                    .border_color(colors.border)
                    .px_3()
                    .py_2()
                    .text_size(px(11.))
                    .text_color(colors.muted)
                    .child(div().flex_1().child(t!("locator.hint").into_owned()))
                    .when(line_only, |footer| {
                        footer.child(self.locator_jump_button("locator-line-open", false, cx))
                    }),
            );
        div()
            .id("locator-backdrop")
            .size_full()
            .block_mouse_except_scroll()
            .child(
                gpui_component::popup_material(popup, cx)
                    .absolute()
                    .left(origin.x)
                    .top(origin.y),
            )
    }

    fn locator_options(
        &self,
        cx: &Context<Self>,
        colors: Palette,
        height: Pixels,
    ) -> impl IntoElement {
        let mode = self.locator.mode;
        let query = &self.locator_tab().query;
        let scope_open = self.locator_session().scope_open;
        let options = [
            (query.case_sensitive, "Aa", "locator.case_sensitive"),
            (query.whole_word, "Ab", "locator.whole_word"),
            (query.regex, ".*", "locator.regex"),
        ];
        v_flex()
            .id("locator-options")
            .max_h((height * 0.35).min(px(180.)))
            .min_h_0()
            .overflow_y_scroll()
            .text_size(px(12.))
            .when_some(self.locator.references.as_ref(), |area, search| {
                area.child(
                    h_flex().px_3().pt_2().child(
                        Checkbox::new("locator-include-declaration")
                            .small()
                            .label(t!("lsp.include_declaration").into_owned())
                            .checked(search.include_declaration)
                            .on_change(cx.listener(|this, checked: &bool, window, cx| {
                                if let Some(search) = &mut this.locator.references {
                                    search.include_declaration = *checked;
                                    this.locator_search(window, cx);
                                }
                            })),
                    ),
                )
            })
            .when(mode == Mode::Text, |area| {
                area.child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .children(options.into_iter().enumerate().map(
                            |(index, (enabled, label, title))| {
                                status_button(title)
                                    .label(label)
                                    .tooltip(t!(title).into_owned())
                                    .selected(enabled)
                                    .when(enabled, |button| {
                                        button
                                            .text_color(colors.accent)
                                            .bg(colors.accent.alpha(0.15))
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        let query = &mut this.locator_session_mut().tabs
                                            [Mode::Text as usize]
                                            .query;
                                        match index {
                                            0 => query.case_sensitive = !query.case_sensitive,
                                            1 => query.whole_word = !query.whole_word,
                                            _ => query.regex = !query.regex,
                                        }
                                        this.locator_search(window, cx);
                                    }))
                            },
                        ))
                        .when(
                            self.view == WorkspaceView::Editor && self.locator.source.is_some(),
                            |row| {
                                row.child(
                                    status_button("locator-find-here")
                                        .label(t!("locator.find_here").into_owned())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.locator_find_here(window, cx)
                                        })),
                                )
                            },
                        ),
                )
            })
            .when(mode.has_scope(), |area| {
                area.child(
                    gpui_component::collapsible::Collapsible::new()
                        .motion_id("locator-scope-reveal")
                        .open(scope_open)
                        .px_3()
                        .py_1()
                        .child(
                            div()
                                .id("locator-scope")
                                .tab_index(0)
                                .role(Role::Button)
                                .aria_expanded(scope_open)
                                .cursor_pointer()
                                .rounded(px(5.))
                                .px_1()
                                .py(px(3.))
                                .text_color(colors.muted)
                                .hover(|style| style.bg(colors.hover))
                                .child(
                                    h_flex()
                                        .gap(px(6.))
                                        .child(icon(
                                            if scope_open {
                                                IconName::ChevronDown
                                            } else {
                                                IconName::ChevronRight
                                            },
                                            colors.muted,
                                            14.,
                                        ))
                                        .child(t!("locator.scope").into_owned()),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let scope = &mut this.locator_session_mut().scope_open;
                                    *scope = !*scope;
                                    if !*scope
                                        && [
                                            &this.locator.inputs().include,
                                            &this.locator.inputs().exclude,
                                        ]
                                        .into_iter()
                                        .any(|input| input.focus_handle(cx).is_focused(window))
                                    {
                                        this.locator
                                            .inputs()
                                            .query
                                            .update(cx, |input, cx| input.focus(window, cx));
                                    }
                                    this.notify_locator(cx);
                                })),
                        )
                        .content(
                            v_flex()
                                .w_full()
                                .pl_6()
                                .gap(px(3.))
                                .py(px(3.))
                                .child(
                                    Input::new(&self.locator.inputs().include)
                                        .disabled(!scope_open)
                                        .tab_index(if scope_open { 0 } else { -1 })
                                        .w_full(),
                                )
                                .child(
                                    Input::new(&self.locator.inputs().exclude)
                                        .disabled(!scope_open)
                                        .tab_index(if scope_open { 0 } else { -1 })
                                        .w_full(),
                                )
                                .child(
                                    div()
                                        .text_color(colors.muted)
                                        .text_size(px(11.))
                                        .child(t!("locator.rules").into_owned()),
                                ),
                        ),
                )
            })
            .when(!mode.has_scope(), |area| {
                let path = self
                    .locator
                    .source
                    .as_ref()
                    .map(|path| paths::display(path))
                    .unwrap_or_else(|| t!("locator.needs_file").into_owned());
                let project = self
                    .locator
                    .source
                    .as_ref()
                    .and_then(|path| self.files.get(path))
                    .and_then(|file| file.server_project.as_ref())
                    .map(|project| project.key.project_path());
                let scope = project.map_or_else(
                    || t!("locator.symbol_scope").into_owned(),
                    |root| t!("locator.project", root = paths::display(root)).into_owned(),
                );
                area.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_color(colors.muted)
                        .truncate()
                        .child(format!("{path} · {scope}")),
                )
            })
    }

    fn locator_results(
        &self,
        cx: &Context<Self>,
        colors: Palette,
        narrow: bool,
        width: Pixels,
    ) -> impl IntoElement {
        let scroll = self.locator_tab().scroll.clone();
        let owner = cx.weak_entity();
        let list_state = if self.locator.rows.is_empty() {
            ListState::new(0, ListAlignment::Top, px(0.))
        } else {
            scroll
        };
        let results = list(list_state.clone(), move |row, _, cx| {
            owner
                .update(cx, |this, cx| this.locator_result_row(row, colors, cx))
                .unwrap_or_else(|_| div().into_any_element())
        })
        .size_full()
        .px_2()
        .pb_2();

        v_flex()
            .id("locator-results")
            .partial_repaint()
            .role(Role::ListBox)
            .min_h_0()
            .min_w_0()
            .when(narrow, |list| list.flex_1())
            .when(!narrow, |list| list.w(width * 0.43).flex_shrink_0())
            .child(results)
            .vertical_popup_scrollbar(&list_state)
    }

    fn locator_result_row(&self, row: usize, colors: Palette, cx: &Context<Self>) -> AnyElement {
        let Some(row) = self.locator.rows.get(row).copied() else {
            return div().into_any_element();
        };
        let index = match row {
            ResultRow::Heading(index) => {
                return div()
                    .pb(px(3.))
                    .child(
                        div()
                            .bg(colors.soft)
                            .py_2()
                            .px(px(14.))
                            .text_size(px(12.))
                            .child(display_path(&self.locator.items[index].path)),
                    )
                    .into_any_element();
            }
            ResultRow::Entry(index) => index,
        };
        let entry = &self.locator.items[index];
        let selected = index == self.locator_tab().selected;
        let subtitle = if let Some(symbol) = &entry.symbol {
            let key = format!("locator.kind_{}", symbol.kind);
            format!(
                "{} · {} · {}",
                symbol.container,
                t!(&key),
                path_with_line(entry)
            )
        } else {
            path_with_line(entry)
        };
        let title = if let Some(hit) = &entry.hit {
            let accent = colors.accent;
            let mut snippet = StyledText::new(format!(
                "{}{}",
                hit.snippet,
                if hit.clipped { "…" } else { "" }
            ));
            if !hit.mark.is_empty() {
                snippet = snippet.with_highlights([(
                    hit.mark.clone(),
                    HighlightStyle {
                        background_color: Some(accent.alpha(0.22)),
                        underline: Some(UnderlineStyle {
                            color: Some(accent),
                            thickness: px(1.),
                            wavy: false,
                            pattern: Default::default(),
                        }),
                        ..Default::default()
                    },
                )]);
            }
            h_flex()
                .items_start()
                .font_family(cx.theme().mono_font_family.clone())
                .child(
                    div()
                        .flex_shrink_0()
                        .pr_1()
                        .text_color(colors.muted)
                        .child(format!("{}:", hit.line + 1)),
                )
                .child(div().flex_1().min_w_0().child(snippet))
                .into_any_element()
        } else {
            div()
                .truncate()
                .child(
                    entry
                        .symbol
                        .as_ref()
                        .map(|symbol| symbol.name.clone())
                        .unwrap_or_else(|| file_name(&entry.path).to_owned()),
                )
                .into_any_element()
        };
        div()
            .pb(px(3.))
            .child(
                h_flex()
                    .id(("locator-result", index))
                    .role(Role::ListBoxOption)
                    .aria_label(subtitle.clone())
                    .aria_selected(selected)
                    .tab_stop(selected)
                    .group("locator-result")
                    .flex_shrink_0()
                    .relative()
                    .gap_2()
                    .pl(px(10.))
                    .pr_1()
                    .py(px(5.))
                    .rounded(px(6.))
                    .hover(|style| style.bg(colors.hover))
                    .when(selected, |row| {
                        row.bg(colors.accent.alpha(0.15)).child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left_0()
                                .w(px(3.))
                                .bg(colors.accent),
                        )
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .pl(px(entry
                                .symbol
                                .as_ref()
                                .map_or(0, |symbol| symbol.depth.min(8) * 12)
                                as f32))
                            .child(
                                div()
                                    .line_height(px(20.))
                                    .when(selected, |title| title.font_semibold())
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .line_height(px(16.))
                                    .text_color(colors.muted)
                                    .truncate()
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        div()
                            .when(!selected, |icon| {
                                icon.opacity(0.)
                                    .group_hover("locator-result", |style| style.opacity(1.))
                            })
                            .child(
                                icon_button(
                                    ("locator-result-open", index),
                                    IconName::LogIn,
                                    t!("locator.open").into_owned(),
                                )
                                .with_size(px(16.))
                                .tab_stop(selected)
                                .disabled(self.locator.busy)
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.locator_open_row(index, window, cx)
                                    },
                                )),
                            ),
                    )
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if event.click_count() >= 2 {
                            this.locator_open_row(index, window, cx);
                        } else if index != this.locator_tab().selected {
                            this.locator_select(index, false, window, cx);
                        }
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "enter" {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.locator_open_row(index, window, cx);
                        }
                    })),
            )
            .into_any_element()
    }

    fn locator_preview_view(
        &self,
        cx: &Context<Self>,
        colors: Palette,
        narrow: bool,
    ) -> impl IntoElement {
        let locator = &self.locator;
        v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .when(narrow, |panel| panel.border_t_1())
            .when(!narrow, |panel| panel.border_l_1())
            .border_color(colors.border)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .text_size(px(12.))
                    .text_color(colors.muted)
                    .child(
                        div().flex_1().min_w_0().text_ellipsis_middle().child(
                            locator
                                .preview
                                .as_ref()
                                .map(|preview| {
                                    format!(
                                        "{}:{}",
                                        display_path(&preview.entry.path),
                                        preview.entry.line().unwrap_or(0) + 1
                                    )
                                })
                                .unwrap_or_else(|| t!("locator.preview").into_owned()),
                        ),
                    )
                    .child(self.locator_jump_button(
                        "locator-preview-open",
                        locator.preview.is_none(),
                        cx,
                    )),
            )
            .when_some(locator.preview.as_ref(), |panel, preview| {
                panel.child(
                    div()
                        .key_context("LocatorPreview")
                        .flex_1()
                        .min_h_0()
                        .child(
                            Editor::new(&preview.editor)
                                .h(relative(1.))
                                .preview()
                                .text_size(px(13.))
                                .line_height(relative(1.7))
                                .aria_label(t!("locator.preview").into_owned()),
                        ),
                )
            })
            .when(locator.preview.is_none(), |panel| {
                panel.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_size(px(12.))
                        .text_color(colors.muted)
                        .child(if locator.preview_busy {
                            t!("locator.preview_loading").into_owned()
                        } else {
                            locator
                                .preview_error
                                .clone()
                                .unwrap_or_else(|| t!("locator.preview_hint").into_owned())
                        }),
                )
            })
    }

    fn locator_composing(&self, window: &Window, cx: &App) -> bool {
        let Some(inputs) = &self.locator.inputs else {
            return false;
        };
        [&inputs.query, &inputs.include, &inputs.exclude]
            .iter()
            .any(|input| {
                let input = input.read(cx);
                input.focus_handle(cx).is_focused(window) && input.is_composing()
            })
    }

    fn locator_preview_focused(&self, window: &Window, cx: &App) -> bool {
        self.locator
            .preview
            .as_ref()
            .is_some_and(|preview| preview.editor.read(cx).focus_handle(cx).is_focused(window))
    }

    fn locator_jump_button(&self, id: &'static str, disabled: bool, cx: &Context<Self>) -> Button {
        icon_button(id, IconName::LogIn, t!("locator.open").into_owned())
            .with_size(px(16.))
            .disabled(disabled)
            .on_click(cx.listener(|this, _, window, cx| this.locator_accept(window, cx)))
    }

    fn locator_open_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.locator.busy || index >= self.locator.items.len() {
            return;
        }
        if index != self.locator_tab().selected {
            self.locator_select(index, false, window, cx);
        }
        if self.locator.preview_busy {
            self.locator.open_after_preview = true;
        } else {
            self.locator_accept(window, cx);
        }
    }

    fn locator_find_here(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.locator_tab().query.clone();
        let Some(file) = self.active_file().map(|file| file.editor.clone()) else {
            return;
        };
        self.close_locator(false, window, cx);
        file.update(cx, |editor, cx| {
            editor.focus(window, cx);
            editor.open_search(false, cx);
            editor.set_find_in_selection(false, cx);
            editor.set_find_query(
                query.query,
                !query.case_sensitive,
                gpui_base::input::SearchOptions {
                    whole_word: query.whole_word,
                    regex: query.regex,
                },
                cx,
            );
        });
    }
}

fn display_path(path: &Path) -> String {
    paths::display(path).replace(std::path::MAIN_SEPARATOR, "/")
}

fn path_with_line(entry: &Entry) -> String {
    let path = display_path(&entry.path);
    if let Some(reference) = &entry.reference {
        let Position { line, character } = reference.range.start;
        return format!(
            "{path}:{}:{}",
            line.saturating_add(1),
            character.saturating_add(1)
        );
    }
    entry
        .line()
        .map_or(path.clone(), |line| format!("{path}:{}", line + 1))
}

#[cfg(all(test, unix))]
mod tests;
